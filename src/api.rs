mod stream;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use anyhow::{Context as _, Result, ensure};
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, MatchedPath, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use codex_ai::InvalidRequest;
use codex_ai::chat::{self, ChatStream};
use codex_ai::responses::normalize_request;
use reqwest::{Client, RequestBuilder};
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq as _;

use self::stream::{Output, collect_response, stream_response, terminal_failure};
use crate::auth::{Credentials, ORIGINATOR, Session, SessionError};
use crate::telemetry::{LogLevel, Trace};

const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
// As in Zed, the catalog uses a Codex compatibility version, not the host version.
const MODEL_CATALOG_CLIENT_VERSION: &str = "0.999.0";
const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;

pub(super) struct Codex {
    client: Client,
    session: Arc<Session>,
    base_url: String,
}

impl Codex {
    pub(super) fn new(client: Client, session: Session) -> Self {
        Self {
            client,
            session: Arc::new(session),
            base_url: CODEX_BASE_URL.into(),
        }
    }

    async fn send(
        &self,
        trace: Trace,
        build: impl Fn() -> RequestBuilder,
    ) -> Result<reqwest::Response, ApiError> {
        let send = |credentials: &Credentials, attempt: u8| {
            let request = upstream_request(build(), credentials);
            async move {
                trace.event(format_args!("codex.http.send attempt={attempt}"));
                request
                    .send()
                    .await
                    .inspect(|response| {
                        trace.event(format_args!(
                            "codex.http.response attempt={attempt} status={}",
                            response.status().as_u16()
                        ));
                    })
                    .inspect_err(|error| {
                        trace.event(format_args!(
                            "codex.http.error attempt={attempt} timeout={} connect={}",
                            error.is_timeout(),
                            error.is_connect()
                        ));
                    })
            }
        };
        let credentials = self.session.credentials(None, trace).await?;
        let mut response = send(&credentials, 1).await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            drop(response);
            let credentials = self.session.credentials(Some(&credentials), trace).await?;
            response = send(&credentials, 2).await?;
        }
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(ApiError::from_upstream(response).await)
        }
    }

    async fn start_response(
        &self,
        body: &Value,
        stream: bool,
        trace: Trace,
    ) -> Result<reqwest::Response, ApiError> {
        trace.event(format_args!(
            "codex.request model={} stream={stream} tools={}",
            model_for_debug(&body["model"]),
            body["tools"].as_array().map_or(0, Vec::len)
        ));
        let url = format!("{}/responses", self.base_url);
        self.send(trace, || {
            let mut request = self
                .client
                .post(&url)
                .header(header::ACCEPT, "text/event-stream")
                .json(body);
            if let Some(key) = body["prompt_cache_key"].as_str()
                && !key.is_empty()
                && let Ok(key) = HeaderValue::from_str(key)
            {
                request = request
                    .header("session-id", key.clone())
                    .header("thread-id", key);
            }
            request
        })
        .await
    }
}

fn model_for_debug(value: &Value) -> &str {
    value
        .as_str()
        .filter(|model| {
            // Only display short GPT slugs; the local Bearer key is at least 32 characters.
            model.len() < 32
                && model
                    .get(..4)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("gpt-"))
                && model
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        })
        .unwrap_or("<masked>")
}

fn upstream_request(request: RequestBuilder, credentials: &Credentials) -> RequestBuilder {
    let mut request = request
        .bearer_auth(&credentials.access_token)
        .header("originator", ORIGINATOR)
        .header("openai-beta", "responses=experimental");
    if let Some(account) = &credentials.account_id {
        request = request.header("chatgpt-account-id", account);
    }
    request
}

pub(super) fn local_key(value: &str) -> Result<HeaderValue> {
    ensure!(
        value.len() >= 32
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-._~+/=".contains(&byte)),
        "CODEX_API_KEY must be a bearer token of at least 32 characters; use `openssl rand -hex 32`"
    );
    let mut key = HeaderValue::from_str(&format!("Bearer {value}"))
        .context("invalid authorization header for CODEX_API_KEY")?;
    key.set_sensitive(true);
    Ok(key)
}

#[derive(Clone, Copy)]
pub(super) enum ChatTools {
    Strict,
    FunctionsOnly,
}

pub(super) fn router(
    codex: Codex,
    key: HeaderValue,
    log_level: LogLevel,
    chat_tools: ChatTools,
) -> Router {
    Router::new()
        .route("/v1/models", get(models))
        .route("/v1/responses", post(responses))
        .route(
            "/v1/chat/completions",
            post(chat_completions).layer(Extension(chat_tools)),
        )
        .route_layer(middleware::from_fn_with_state(key, authorize))
        .fallback(|| async { ApiError::new(StatusCode::NOT_FOUND, "not_found", "unknown route") })
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .layer(middleware::from_fn_with_state(log_level, trace_request))
        .with_state(Arc::new(codex))
}

async fn trace_request(
    State(log_level): State<LogLevel>,
    mut request: Request,
    next: Next,
) -> Response {
    let trace = Trace::start(log_level);
    request.extensions_mut().insert(trace);
    if log_level == LogLevel::Off {
        return next.run(request).await;
    }
    let method = match *request.method() {
        Method::GET => "GET",
        Method::POST => "POST",
        Method::HEAD => "HEAD",
        Method::OPTIONS => "OPTIONS",
        _ => "OTHER",
    };
    let matched_path = request.extensions().get::<MatchedPath>().cloned();
    let route = matched_path
        .as_ref()
        .map_or("<unmatched>", MatchedPath::as_str);
    trace.event(format_args!("http.request method={method} route={route}"));
    let response = next.run(request).await;
    let error = response
        .extensions()
        .get::<ErrorCode>()
        .map_or("-", |code| code.0);
    trace.info(format_args!(
        "http.response method={method} route={route} status={} error={error}",
        response.status().as_u16()
    ));
    response
}

async fn authorize(State(key): State<HeaderValue>, request: Request, next: Next) -> Response {
    let mut auth = request.headers().get_all(header::AUTHORIZATION).iter();
    if let Some(value) = auth.next()
        && auth.next().is_none()
        && let Some((scheme, secret)) = value.to_str().ok().and_then(|value| value.split_once(' '))
        && scheme.eq_ignore_ascii_case("bearer")
        && bool::from(secret.as_bytes().ct_eq(&key.as_bytes()[7..]))
    {
        let mut response = next.run(request).await;
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return response;
    }
    (
        [(header::WWW_AUTHENTICATE, "Bearer")],
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "invalid_api_key",
            "missing or invalid local API key",
        ),
    )
        .into_response()
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
}

async fn models(
    State(codex): State<Arc<Codex>>,
    Extension(trace): Extension<Trace>,
) -> Result<Json<Value>, ApiError> {
    let url = format!("{}/models", codex.base_url);
    let response = codex
        .send(trace, || {
            codex
                .client
                .get(&url)
                .query(&[("client_version", MODEL_CATALOG_CLIENT_VERSION)])
                .header(header::ACCEPT, "application/json")
        })
        .await?;
    let mut catalog: Catalog = response.json().await?;
    catalog.models.retain(|model| model.visibility == "list");
    catalog.models.sort_by_key(|model| model.priority);
    let data: Vec<_> = catalog.models.into_iter().map(|model| {
        json!({"id": model.slug, "object": "model", "created": 0, "owned_by": "openai"})
    }).collect();
    Ok(Json(json!({"object": "list", "data": data})))
}

async fn responses(
    State(codex): State<Arc<Codex>>,
    Extension(trace): Extension<Trace>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Response, ApiError> {
    let mut body = body?.0;
    let stream = normalize_request(&mut body)?;
    let upstream = codex.start_response(&body, stream, trace).await?;
    if stream {
        Ok(stream_response(upstream, Output::Responses, trace))
    } else {
        Ok(Json(collect_response(upstream, trace).await?).into_response())
    }
}

async fn chat_completions(
    State(codex): State<Arc<Codex>>,
    Extension(trace): Extension<Trace>,
    Extension(chat_tools): Extension<ChatTools>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Response, ApiError> {
    let mut body = body?.0;
    if matches!(chat_tools, ChatTools::FunctionsOnly) {
        filter_chat_tools(&mut body, trace);
    }
    let (mut body, options) = chat::into_responses(body).inspect_err(|error| {
        trace.event(format_args!(
            "chat.validation_rejected reason={:?}",
            error.to_string()
        ));
    })?;
    normalize_request(&mut body)?;
    let upstream = codex.start_response(&body, options.stream(), trace).await?;
    if options.stream() {
        Ok(stream_response(
            upstream,
            Output::Chat(ChatStream::new(options)),
            trace,
        ))
    } else {
        let response = collect_response(upstream, trace).await?;
        Ok(Json(chat::completion(&response, options.model())).into_response())
    }
}

fn filter_chat_tools(body: &mut Value, trace: Trace) {
    let Some(body) = body.as_object_mut() else {
        return;
    };
    let selected_kind = body
        .get("tool_choice")
        .and_then(|choice| choice["type"].as_str())
        .map(str::to_owned);
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    let before = tools.len();
    let mut selected_tool_removed = false;
    tools.retain(|tool| match tool["type"].as_str() {
        // Let normal validation report malformed tools instead of silently dropping them.
        None | Some("" | "function") => true,
        Some(kind) => {
            selected_tool_removed |= selected_kind.as_deref() == Some(kind);
            false
        }
    });
    let remaining = tools.len();
    let removed = before - remaining;
    if removed == 0 {
        return;
    }
    if remaining == 0 {
        body.remove("tool_choice");
        body.remove("parallel_tool_calls");
    } else if selected_tool_removed {
        // Do not substitute a different function for the explicitly selected tool.
        body.insert("tool_choice".into(), json!("none"));
    }
    trace.event(format_args!(
        "chat.tools_filtered removed={removed} remaining={remaining}"
    ));
}

struct ApiError {
    status: StatusCode,
    code: &'static str,
    // Upstream data goes in responses, not diagnostic logs.
    provider_code: Option<String>,
    message: String,
}

fn provider_error(payload: &Value) -> &Value {
    [
        &payload["response"]["error"],
        &payload["error"],
        &payload["detail"],
    ]
    .into_iter()
    .find(|error| error.is_object())
    .unwrap_or(payload)
}

fn provider_discriminator(error: &Value) -> Option<&str> {
    ["code", "type"]
        .into_iter()
        .filter_map(|key| error[key].as_str())
        .find(|code| !code.trim().is_empty() && !terminal_failure(code))
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            provider_code: None,
            message: message.into(),
        }
    }

    fn gateway(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, "upstream_error", message)
    }

    async fn from_upstream(response: reqwest::Response) -> Self {
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED {
            return Self::new(
                StatusCode::BAD_GATEWAY,
                "codex_authentication_failed",
                "Codex rejected the session; stop the server and run `codex_ai login` again",
            );
        }
        let message = format!("Codex returned HTTP {status}");
        if !status.is_client_error() {
            return Self::gateway(message);
        }
        let mut error = Self::new(status, "codex_request_rejected", message);
        if status != StatusCode::FORBIDDEN
            && let Ok(body) = response.json::<Value>().await
        {
            let provider = provider_error(&body);
            error.provider_code = provider_discriminator(provider).map(str::to_owned);
            if let Some(message) = provider["message"]
                .as_str()
                .or_else(|| body["detail"].as_str())
            {
                error.message = message.to_owned();
            }
        }
        error
    }

    fn stream_failure(event: &Value) -> Self {
        let provider = provider_error(event);
        let code = provider_discriminator(provider);
        let status = match code {
            Some("usage_limit_reached" | "insufficient_quota") => StatusCode::PAYMENT_REQUIRED,
            Some("rate_limit_exceeded" | "rate_limit_error") => StatusCode::TOO_MANY_REQUESTS,
            Some("context_length_exceeded" | "request_too_large" | "invalid_encrypted_content") => {
                StatusCode::BAD_REQUEST
            }
            _ => StatusCode::BAD_GATEWAY,
        };
        let message = provider["message"]
            .as_str()
            .or_else(|| event["message"].as_str())
            .or_else(|| event["detail"].as_str())
            .unwrap_or("Codex generation failed");
        let mut error = Self::new(status, "upstream_error", message);
        error.provider_code = code.map(str::to_owned);
        error
    }

    fn payload_code(&self) -> &str {
        self.provider_code.as_deref().unwrap_or(self.code)
    }

    fn payload(&self) -> Value {
        let kind = match self.status {
            StatusCode::UNAUTHORIZED => "authentication_error",
            StatusCode::PAYMENT_REQUIRED => "payment_required_error",
            StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
            s if s.is_client_error() => "invalid_request_error",
            _ => "server_error",
        };
        json!({"error": {"message": self.message, "type": kind, "param": null, "code": self.payload_code()}})
    }
}

impl From<InvalidRequest> for ApiError {
    fn from(error: InvalidRequest) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            error.to_string(),
        )
    }
}

impl From<JsonRejection> for ApiError {
    fn from(error: JsonRejection) -> Self {
        Self::new(
            error.status(),
            "invalid_json",
            "invalid or oversized JSON body; Content-Type: application/json is required",
        )
    }
}

impl From<reqwest::Error> for ApiError {
    fn from(error: reqwest::Error) -> Self {
        if error.is_timeout() {
            Self::new(
                StatusCode::GATEWAY_TIMEOUT,
                "upstream_timeout",
                "Codex did not respond in time",
            )
        } else {
            Self::gateway("Codex response is unavailable or invalid")
        }
    }
}

impl From<SessionError> for ApiError {
    fn from(error: SessionError) -> Self {
        match error {
            SessionError::LoginRequired => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "codex_login_required",
                "stop the server and run `codex_ai login` again",
            ),
            SessionError::Temporary(error) => {
                eprintln!("Codex session failed: {error:#}");
                Self::gateway("temporarily unable to update the Codex session")
            }
        }
    }
}

#[derive(Clone, Copy)]
struct ErrorCode(&'static str);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            [(header::CACHE_CONTROL, "no-store")],
            Extension(ErrorCode(self.code)),
            Json(self.payload()),
        )
            .into_response()
    }
}
