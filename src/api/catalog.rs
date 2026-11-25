use std::sync::Arc;
use std::time::Duration;

use axum::http::{StatusCode, header};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::{Instant, timeout};

use super::{ApiError, Codex};
use crate::telemetry::Trace;

// As in Zed, this is a Codex compatibility version, not the host version.
const CLIENT_VERSION: &str = "0.999.0";
const CATALOG_TTL: Duration = Duration::from_secs(300);
const CATALOG_RETRY_DELAY: Duration = Duration::from_secs(10);
const CATALOG_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) struct CachedCatalog {
    expires_at: Instant,
    result: Result<Arc<Catalog>, ApiError>,
}

#[derive(Deserialize)]
pub(super) struct Catalog {
    models: Vec<CatalogModel>,
}

#[derive(Deserialize)]
struct CatalogModel {
    slug: String,
    visibility: String,
    priority: i32,
    #[serde(flatten)]
    capabilities: Capabilities,
}

#[derive(Deserialize, Serialize)]
struct Capabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    context_window: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    supported_reasoning_levels: Option<Vec<ReasoningLevel>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_reasoning_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_modalities: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tiers: Option<Vec<ServiceTier>>,
    #[serde(default, skip_serializing)]
    additional_speed_tiers: Vec<String>,
}

#[derive(Deserialize, Serialize)]
struct ReasoningLevel {
    effort: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct ServiceTier {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

impl Catalog {
    pub(super) fn listing(&self) -> Value {
        let data: Vec<_> = self
            .models
            .iter()
            .filter(|model| model.visibility == "list")
            .map(|model| {
                json!({
                    "id": model.slug, "object": "model", "created": 0, "owned_by": "openai",
                    "x_codex": model.capabilities,
                })
            })
            .collect();
        json!({"object": "list", "data": data})
    }
}

impl Capabilities {
    fn default_effort(&self) -> Option<&str> {
        self.default_reasoning_level
            .as_deref()
            .filter(|effort| !effort.trim().is_empty())
    }

    fn allowed_efforts(&self) -> Option<impl Iterator<Item = &str>> {
        Some(
            self.supported_reasoning_levels
                .as_ref()?
                .iter()
                .map(|level| level.effort.as_str())
                .chain(self.default_effort()),
        )
    }

    fn allowed_tiers(&self) -> Option<impl Iterator<Item = &str>> {
        let advertised = self
            .service_tiers
            .as_ref()?
            .iter()
            .map(|tier| match tier.id.as_str() {
                "fast" => "priority",
                id => id,
            });
        let legacy_priority = self
            .additional_speed_tiers
            .iter()
            .any(|tier| tier == "fast");
        // OpenAI's routing defaults and Codex's Flex tier need not be listed.
        Some(
            ["auto", "default", "flex"]
                .into_iter()
                .chain(advertised)
                .chain(legacy_priority.then_some("priority")),
        )
    }
}

impl Codex {
    pub(super) async fn catalog(&self, trace: Trace) -> Result<Arc<Catalog>, ApiError> {
        // Single in-flight refresh, with a deadline even when the upstream stalls.
        let mut cache = self.catalog.lock().await;
        if let Some(entry) = cache
            .as_ref()
            .filter(|entry| Instant::now() < entry.expires_at)
        {
            return entry.result.clone();
        }
        let result = match timeout(CATALOG_TIMEOUT, self.fetch_catalog(trace)).await {
            Ok(result) => result.map(Arc::new),
            Err(_) => Err(ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "upstream_timeout",
                "Codex model catalog did not respond in time",
            )),
        };
        let ttl = if result.is_ok() {
            CATALOG_TTL
        } else {
            CATALOG_RETRY_DELAY
        };
        *cache = Some(CachedCatalog {
            expires_at: Instant::now() + ttl,
            result: result.clone(),
        });
        result
    }

    async fn fetch_catalog(&self, trace: Trace) -> Result<Catalog, ApiError> {
        let url = format!("{}/models", self.base_url);
        let response = self
            .send(trace, || {
                self.client
                    .get(&url)
                    .query(&[("client_version", CLIENT_VERSION)])
                    .header(header::ACCEPT, "application/json")
            })
            .await?;
        let mut catalog: Catalog = response.json().await?;
        catalog.models.sort_by_key(|model| model.priority);
        Ok(catalog)
    }

    pub(super) async fn apply_model_options(
        &self,
        body: &mut Value,
        fallback_effort: Option<&str>,
        trace: Trace,
    ) -> Result<(), ApiError> {
        if !body["reasoning"].is_null() && !body["reasoning"].is_object() {
            return Err(ApiError::invalid_request("reasoning must be an object"));
        }
        let effort = optional_string(&body["reasoning"]["effort"], "reasoning.effort")?;
        let tier = optional_string(&body["service_tier"], "service_tier")?;
        let model_id = body["model"].as_str().unwrap_or_default();
        let catalog = self.catalog(trace).await;
        if let Err(error) = &catalog {
            trace.event(format_args!(
                "codex.catalog.unavailable code={}",
                error.code
            ));
        }
        let capabilities = catalog
            .as_deref()
            .ok()
            .and_then(|catalog| catalog.models.iter().find(|model| model.slug == model_id))
            .map(|model| &model.capabilities);
        if let Some(capabilities) = capabilities {
            check_option(
                model_id,
                "reasoning.effort",
                effort,
                capabilities.allowed_efforts(),
            )?;
            check_option(model_id, "service_tier", tier, capabilities.allowed_tiers())?;
        }
        if effort.is_none() {
            let default = capabilities
                .and_then(Capabilities::default_effort)
                .or_else(|| {
                    fallback_effort.filter(|effort| {
                        capabilities
                            .and_then(Capabilities::allowed_efforts)
                            .is_none_or(|mut allowed| allowed.any(|level| level == *effort))
                    })
                });
            if let Some(default) = default {
                body["reasoning"]["effort"] = json!(default);
            }
        }
        Ok(())
    }
}

fn optional_string<'a>(value: &'a Value, field: &str) -> Result<Option<&'a str>, ApiError> {
    match value {
        Value::Null => Ok(None),
        Value::String(value) if !value.trim().is_empty() => Ok(Some(value)),
        _ => Err(ApiError::invalid_request(format!(
            "{field} must be a non-empty string"
        ))),
    }
}

fn check_option<'a>(
    model: &str,
    field: &str,
    requested: Option<&str>,
    allowed: Option<impl Iterator<Item = &'a str>>,
) -> Result<(), ApiError> {
    if let (Some(requested), Some(allowed)) = (requested, allowed) {
        let mut allowed: Vec<_> = allowed.collect();
        if !allowed.contains(&requested) {
            allowed.sort_unstable();
            allowed.dedup();
            let choices = if allowed.is_empty() {
                "no values are supported".into()
            } else {
                format!("supported values: {}", allowed.join(", "))
            };
            return Err(ApiError::invalid_request(format!(
                "{field} {requested:?} is not supported by model {model:?}; {choices}"
            )));
        }
    }
    Ok(())
}
