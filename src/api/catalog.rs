use std::sync::Arc;
use std::time::Duration;

use axum::http::header;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::time::{Instant, timeout};

use super::{ApiError, Codex};
use crate::telemetry::Trace;

// As in Zed, this is a Codex compatibility version, not the host version.
const CLIENT_VERSION: &str = "0.999.0";
const CATALOG_TTL: Duration = Duration::from_secs(300);
const CATALOG_RETRY_DELAY: Duration = Duration::from_secs(10);
const CATALOG_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const CATALOG_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

// ChatGptModel::all() in zed-industries/zed at 77228009dd267dcf8ab9a20eef5802b2a8ea4afa.
const FALLBACK_MODELS: &[&str] = &[
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
    "gpt-5.4",
    "gpt-5.4-mini",
];

#[derive(Default)]
pub(super) struct CachedCatalog {
    latest: Option<Arc<Catalog>>,
    next_refresh_at: Option<Instant>,
    refreshing: Option<watch::Receiver<Option<Arc<Catalog>>>>,
}

#[derive(Deserialize)]
struct Catalog {
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

#[derive(Default, Deserialize, Serialize)]
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

fn model_entry(slug: &str, capabilities: &Capabilities) -> Value {
    json!({
        "id": slug, "object": "model", "created": 0, "owned_by": "openai",
        "x_codex": capabilities,
    })
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
    pub(super) async fn model_listing(self: &Arc<Self>, trace: Trace) -> Value {
        let data: Vec<_> = match self.catalog(trace).await {
            Some(catalog) => catalog
                .models
                .iter()
                .filter(|model| model.visibility == "list")
                .map(|model| model_entry(&model.slug, &model.capabilities))
                .collect(),
            None => {
                trace.event(format_args!("codex.catalog.fallback"));
                let capabilities = Capabilities::default();
                FALLBACK_MODELS
                    .iter()
                    .map(|slug| model_entry(slug, &capabilities))
                    .collect()
            }
        };
        json!({"object": "list", "data": data})
    }

    async fn catalog(self: &Arc<Self>, trace: Trace) -> Option<Arc<Catalog>> {
        let mut cache = self.catalog.lock().await;
        if cache.next_refresh_at.is_some_and(|at| Instant::now() < at) {
            return cache.latest.clone();
        }

        let mut refresh = cache
            .refreshing
            .get_or_insert_with(|| {
                let (done, refresh) = watch::channel(None);
                let codex = Arc::clone(self);
                tokio::spawn(async move {
                    let latest = codex.refresh_catalog(trace).await;
                    done.send_replace(latest);
                });
                refresh
            })
            .clone();
        // Once we have a catalog or have tried to fetch one, serve without waiting.
        if cache.next_refresh_at.is_some() {
            return cache.latest.clone();
        }
        drop(cache);
        timeout(CATALOG_WAIT_TIMEOUT, refresh.changed())
            .await
            .ok()?
            .ok()?;
        refresh.borrow().clone()
    }

    async fn refresh_catalog(&self, trace: Trace) -> Option<Arc<Catalog>> {
        let result = timeout(CATALOG_REQUEST_TIMEOUT, self.fetch_catalog(trace)).await;
        let mut cache = self.catalog.lock().await;
        let delay = match result {
            Ok(Ok(catalog)) => {
                cache.latest = Some(Arc::new(catalog));
                CATALOG_TTL
            }
            Ok(Err(error)) => {
                trace.event(format_args!(
                    "codex.catalog.unavailable code={}",
                    error.code
                ));
                CATALOG_RETRY_DELAY
            }
            Err(_) => {
                trace.event(format_args!(
                    "codex.catalog.unavailable code=upstream_timeout"
                ));
                CATALOG_RETRY_DELAY
            }
        };
        cache.next_refresh_at = Some(Instant::now() + delay);
        cache.refreshing = None;
        cache.latest.clone()
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
        self: &Arc<Self>,
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
        let capabilities = catalog
            .as_deref()
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

#[cfg(test)]
mod tests {
    use anyhow::{Context as _, Result};
    use axum::http::StatusCode;
    use axum::routing::get;
    use axum::{Json, Router};
    use reqwest::Client;
    use tokio::sync::{mpsc, oneshot};

    use super::super::tests::start;
    use super::*;

    #[tokio::test]
    async fn serves_fallback_and_refreshes_stale_catalog_without_waiting() -> Result<()> {
        let (requested, mut requests) = mpsc::unbounded_channel();
        let mock = Router::new().route(
            "/models",
            get(move || async move {
                let (respond, response) = oneshot::channel::<(StatusCode, Json<Value>)>();
                requested
                    .send(respond)
                    .expect("test is listening for requests");
                response.await.expect("test supplies an upstream response")
            }),
        );
        let (upstream, task) = start(mock).await?;
        let directory = tempfile::tempdir()?;
        let client = Client::builder().no_proxy().build()?;
        let session = crate::auth::test_session(directory.path().join("private"), client.clone())?;
        let mut codex = Codex::new(client, session);
        codex.base_url = upstream;
        let codex = Arc::new(codex);
        let models = || codex.model_listing(Trace::default());
        let limit = Duration::from_secs(2);
        let unavailable = || (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})));
        let listing = |slug| {
            (
                StatusCode::OK,
                Json(json!({"models": [
                    {"slug": slug, "visibility": "list", "priority": 0, "context_window": 128_000}
                ]})),
            )
        };

        let (fallback, concurrent, reply) = tokio::join!(models(), models(), async {
            timeout(limit, requests.recv())
                .await?
                .context("expected initial catalog request")?
                .send(unavailable())
                .expect("initial fetch is waiting for the response");
            Ok::<_, anyhow::Error>(())
        });
        reply?;
        let entries = fallback["data"]
            .as_array()
            .context("expected fallback models")?;
        assert_eq!(fallback["object"], "list");
        assert_eq!(entries.len(), 6);
        assert_eq!(entries[0]["id"], "gpt-5.6-sol");
        assert!(entries.iter().all(|entry| entry["x_codex"] == json!({})));
        assert_eq!(concurrent, fallback);
        assert_eq!(models().await, fallback);
        assert!(requests.try_recv().is_err());

        let mut previous = fallback;
        for (response, expected) in [
            (listing("first-model"), "first-model"),
            (unavailable(), "first-model"),
            (listing("updated-model"), "updated-model"),
        ] {
            codex.catalog.lock().await.next_refresh_at = Some(Instant::now());
            // Hold the upstream response until both callers have received the old listing.
            let (a, b) = timeout(limit, async { tokio::join!(models(), models()) }).await?;
            assert_eq!(a, previous);
            assert_eq!(b, previous);
            let mut refresh = codex
                .catalog
                .lock()
                .await
                .refreshing
                .clone()
                .context("expected an in-flight refresh")?;
            timeout(limit, requests.recv())
                .await?
                .context("expected a refresh request")?
                .send(response)
                .expect("refresh is waiting for the response");
            timeout(limit, refresh.changed()).await??;

            previous = models().await;
            assert_eq!(previous["data"][0]["id"], expected);
            assert_eq!(previous["data"][0]["x_codex"]["context_window"], 128_000);
            assert!(requests.try_recv().is_err());
        }
        task.abort();
        Ok(())
    }
}
