use std::collections::BTreeMap;
use std::convert::Infallible;
use std::future::ready;
use std::pin::pin;
use std::sync::Arc;

use anyhow::{Context as _, Result, ensure};
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, MatchedPath, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use eventsource_stream::{EventStreamError, Eventsource as _};
use futures_util::{Stream, StreamExt as _};
use reqwest::{Client, RequestBuilder};
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq as _;

use crate::auth::{Credentials, ORIGINATOR, Session, SessionError};
use crate::chat::{self, ChatStream};
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

pub(super) fn router(codex: Codex, key: HeaderValue, log_level: LogLevel) -> Router {
    Router::new()
        .route("/v1/models", get(models))
        .route("/v1/responses", post(responses))
        .route("/v1/chat/completions", post(chat_completions))
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
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Response, ApiError> {
    let (mut body, options) = chat::into_responses(body?.0).map_err(ApiError::invalid)?;
    normalize_request(&mut body)?;
    let upstream = codex.start_response(&body, options.stream, trace).await?;
    if options.stream {
        Ok(stream_response(
            upstream,
            Output::Chat(ChatStream::new(options)),
            trace,
        ))
    } else {
        let response = collect_response(upstream, trace).await?;
        Ok(Json(chat::completion(&response, &options.model)).into_response())
    }
}

fn normalize_request(body: &mut Value) -> Result<bool, ApiError> {
    let body = body
        .as_object_mut()
        .ok_or_else(|| ApiError::invalid("expected a JSON object"))?;
    if !body
        .get("model")
        .and_then(Value::as_str)
        .is_some_and(|model| !model.trim().is_empty())
    {
        return Err(ApiError::invalid("model must be a non-empty string"));
    }
    if ["previous_response_id", "conversation"]
        .into_iter()
        .any(|key| body.get(key).is_some_and(|value| !value.is_null()))
        || ["store", "background"].into_iter().any(|key| {
            body.get(key)
                .is_some_and(|value| !value.is_null() && value != false)
        })
    {
        return Err(ApiError::invalid(
            "stored responses, server-side conversations, and background mode are not supported",
        ));
    }
    let stream = match body.get("stream") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(stream)) => *stream,
        _ => return Err(ApiError::invalid("stream must be a boolean")),
    };
    match body.get("instructions") {
        None | Some(Value::String(_)) => {}
        Some(Value::Null) => {
            body.remove("instructions");
        }
        _ => return Err(ApiError::invalid("instructions must be a string")),
    }
    let input = match body.remove("input") {
        Some(Value::String(text)) => vec![json!({"role": "user", "content": text})],
        Some(Value::Array(input)) => input,
        _ => return Err(ApiError::invalid("input must be a string or an array")),
    };
    body.insert("input".into(), Value::Array(normalize_input(input)));
    body.remove("conversation");
    body.remove("previous_response_id");
    body.remove("background");
    body.insert("store".into(), Value::Bool(false));
    body.insert("stream".into(), Value::Bool(true));
    Ok(stream)
}

fn normalize_input(input: Vec<Value>) -> Vec<Value> {
    let mut normalized = Vec::with_capacity(input.len());
    let mut initial_system = Vec::new();
    for mut item in input {
        if matches!(item["type"].as_str(), None | Some("message"))
            && let Some(role @ ("system" | "developer" | "user" | "assistant")) =
                item["role"].as_str()
        {
            let system = role == "system";
            let assistant = role == "assistant";
            if system
                && normalized.is_empty()
                && let Some(text) = system_text(&item["content"])
            {
                if !text.trim().is_empty() {
                    initial_system.push(text);
                }
                continue;
            }
            if system {
                item["role"] = json!("developer");
            }
            item["type"] = json!("message");
            if item["content"].is_string() {
                let text = item["content"].take();
                let part = if assistant {
                    json!({"type": "output_text", "text": text, "annotations": []})
                } else {
                    json!({"type": "input_text", "text": text})
                };
                item["content"] = Value::Array(vec![part]);
            }
        }
        normalized.push(item);
    }
    // Match Zed: keep the reusable system prompt in the cacheable prefix,
    // before any replayed compaction item, without moving later updates.
    if !initial_system.is_empty() {
        normalized.insert(
            0,
            json!({
                "type": "message", "role": "developer",
                "content": [{"type": "input_text", "text": initial_system.join("\n\n")}]
            }),
        );
    }
    normalized
}

fn system_text(content: &Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        return Some(text.to_owned());
    }
    let parts: Option<Vec<_>> = content
        .as_array()?
        .iter()
        .map(|part| {
            if part["type"] == "input_text" {
                part["text"].as_str()
            } else {
                None
            }
        })
        .collect();
    Some(parts?.join("\n\n"))
}

fn events(
    upstream: reqwest::Response,
    trace: Trace,
) -> impl Stream<Item = Result<Value, ApiError>> {
    async_stream::try_stream! {
        let status = upstream.status();
        let content_type = upstream.headers().get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map_or_else(|| "missing".into(), |mime| mime.trim().to_ascii_lowercase());
        let format_error = || ApiError::gateway(format!(
            "Codex returned neither SSE nor Responses JSON (HTTP {status}, Content-Type: {content_type})"
        ));
        if matches!(content_type.as_str(), "text/html" | "application/xhtml+xml") {
            trace.event(format_args!("codex.body format=html"));
            Err(format_error())?;
        } else if content_type == "application/json" || content_type.ends_with("+json") {
            trace.event(format_args!("codex.body format=json"));
            let response = upstream.json::<Value>().await.map_err(|error| {
                if error.is_timeout() { error.into() } else { format_error() }
            })?;
            if !response["error"].is_null() || response["status"] == "failed" || !response["detail"].is_null() {
                Err(ApiError::stream_failure(&response))?;
            }
            if !response["output"].is_array() {
                Err(format_error())?;
            }
            for event in json_response_events(response)? {
                yield event;
            }
        } else {
            // Like Zed, decode SSE from the body rather than requiring a particular media type.
            trace.event(format_args!("codex.body format=sse declared={}", content_type == "text/event-stream"));
            let mut stream = pin!(sse_events(upstream));
            let mut seen = false;
            while let Some(event) = stream.next().await {
                seen = true;
                yield event?;
            }
            if !seen && content_type != "text/event-stream" {
                Err(format_error())?;
            }
        }
    }
}

fn json_response_events(response: Value) -> Result<Vec<Value>, ApiError> {
    let terminal = match response["status"].as_str() {
        None | Some("completed") => "response.completed",
        Some("incomplete") => "response.incomplete",
        _ => {
            return Err(ApiError::gateway(
                "Codex JSON response is not in a terminal state",
            ));
        }
    };
    let mut created = response.clone();
    created["status"] = json!("in_progress");
    created["usage"] = Value::Null;
    created["output"] = json!([]);
    let mut events = vec![json!({"type": "response.created", "response": created})];
    for (output_index, item) in response["output"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let mut added = item.clone();
        if item["type"] == "message" {
            added["content"] = json!([]);
            added["status"] = json!("in_progress");
        }
        events.push(json!({"type": "response.output_item.added", "output_index": output_index, "item": added}));
        if item["type"] == "message" {
            for (content_index, part) in
                item["content"].as_array().into_iter().flatten().enumerate()
            {
                let (field, kind) = match part["type"].as_str() {
                    Some("output_text") => ("text", "response.output_text.delta"),
                    Some("refusal") => ("refusal", "response.refusal.delta"),
                    _ => continue,
                };
                let Some(delta) = part[field].as_str() else {
                    continue;
                };
                let mut added = part.clone();
                added[field] = json!("");
                events.push(json!({"type": "response.content_part.added", "item_id": item["id"], "output_index": output_index, "content_index": content_index, "part": added}));
                events.push(json!({"type": kind, "item_id": item["id"], "output_index": output_index, "content_index": content_index, "delta": delta}));
                events.push(json!({"type": "response.content_part.done", "item_id": item["id"], "output_index": output_index, "content_index": content_index, "part": part}));
            }
        }
        events.push(json!({"type": "response.output_item.done", "output_index": output_index, "item": item}));
    }
    events.push(json!({"type": terminal, "response": response}));
    for (sequence, event) in events.iter_mut().enumerate() {
        event["sequence_number"] = json!(sequence);
    }
    Ok(events)
}

fn sse_events(upstream: reqwest::Response) -> impl Stream<Item = Result<Value, ApiError>> {
    let mut items = BTreeMap::new();
    upstream
        .bytes_stream()
        .eventsource()
        // A terminator closes the stream, but only a Responses final event is success.
        .take_while(|event| ready(!matches!(event, Ok(event) if event.data == "[DONE]")))
        .filter_map(|event| async move {
            let event = match event {
                Ok(event) => event,
                Err(EventStreamError::Transport(error)) => return Some(Err(error.into())),
                Err(_) => {
                    return Some(Err(ApiError::gateway(
                        "Codex SSE stream was interrupted or invalid",
                    )));
                }
            };
            if event.data.is_empty() {
                return None;
            }
            let decoded = serde_json::from_str::<Value>(&event.data)
                .ok()
                .filter(|value| {
                    value["type"].as_str().is_some_and(|kind| {
                        !kind.is_empty()
                            && kind
                                .bytes()
                                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
                    })
                })
                .ok_or_else(|| ApiError::gateway("invalid Codex SSE event"));
            Some(decoded)
        })
        .map(move |event| {
            let mut event = event?;
            let kind = event["type"].as_str().unwrap_or_default();
            if terminal_success(kind) {
                let response = &mut event["response"];
                if !response.is_object() {
                    return Err(ApiError::gateway("missing final Codex response"));
                }
                // Codex may only send completed items in their own SSE events.
                if response["output"].as_array().is_none_or(Vec::is_empty) && !items.is_empty() {
                    response["output"] =
                        Value::Array(std::mem::take(&mut items).into_values().collect());
                }
            } else if kind == "response.output_item.done"
                && let Some(index) = event["output_index"].as_u64()
                && event["item"].is_object()
            {
                items.insert(index, event["item"].clone());
            }
            Ok(event)
        })
}

fn terminal_success(kind: &str) -> bool {
    matches!(kind, "response.completed" | "response.incomplete")
}

fn terminal_failure(kind: &str) -> bool {
    matches!(kind, "error" | "response.error" | "response.failed")
}

async fn collect_response(upstream: reqwest::Response, trace: Trace) -> Result<Value, ApiError> {
    let mut events = pin!(events(upstream, trace));
    while let Some(event) = events.next().await {
        let mut event = event?;
        let kind = event["type"].as_str().unwrap_or_default();
        if terminal_success(kind) {
            trace.event(format_args!("codex.stream.end event={kind}"));
            return Ok(event["response"].take());
        }
        if terminal_failure(kind) {
            trace.event(format_args!("codex.stream.end event={kind}"));
            return Err(ApiError::stream_failure(&event));
        }
    }
    Err(ApiError::gateway(
        "Codex stream ended without a final response",
    ))
}

enum Output {
    Responses,
    Chat(ChatStream),
}

fn stream_response(upstream: reqwest::Response, mut output: Output, trace: Trace) -> Response {
    let stream = async_stream::stream! {
        let mut events = pin!(events(upstream, trace));
        let error = loop {
            let event = match events.next().await {
                Some(Ok(event)) => event,
                Some(Err(error)) => break error,
                None => break ApiError::gateway("Codex stream ended without a final response"),
            };
            let kind = event["type"].as_str().unwrap_or_default();
            let terminal = terminal_success(kind) || terminal_failure(kind);
            if terminal {
                trace.event(format_args!("codex.stream.end event={kind}"));
            }
            match &mut output {
                Output::Responses => yield Ok::<_, Infallible>(Event::default().event(kind).data(event.to_string())),
                Output::Chat(chat) => {
                    if terminal_failure(kind) {
                        break ApiError::stream_failure(&event);
                    }
                    for chunk in chat.chunks(&event) {
                        yield Ok(Event::default().data(chunk.to_string()));
                    }
                    if terminal_success(kind) {
                        yield Ok(Event::default().data("[DONE]"));
                    }
                }
            }
            if terminal {
                return;
            }
        };
        trace.event(format_args!("codex.stream.error code={}", error.code));
        let mut frame = Event::default();
        let data = match output {
            Output::Responses => {
                frame = frame.event("error");
                json!({"type": "error", "message": error.message, "code": error.payload_code(), "param": null})
            }
            Output::Chat(_) => error.payload(),
        };
        yield Ok(frame.data(data.to_string()));
    };
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
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

    fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    #[test]
    fn masks_model_fields_that_are_not_short_gpt_ids() {
        assert_eq!(model_for_debug(&json!("GPT-6.2-Sol")), "GPT-6.2-Sol");
        for value in [
            json!("some-secret"),
            json!("gpt-foo\nsecret"),
            json!("gpt-very-long-private-credential-not-a-model"),
            Value::Null,
        ] {
            assert_eq!(model_for_debug(&value), "<masked>");
        }
    }

    #[test]
    fn normalizes_system_instructions_for_codex() {
        let compaction = json!({"type": "compaction", "encrypted_content": "opaque"});
        let reasoning =
            json!({"type": "reasoning", "id": "rs_test", "encrypted_content": "opaque"});
        let call = json!({"type": "function_call", "call_id": "call_test", "name": "example", "arguments": "{}"});
        let result =
            json!({"type": "function_call_output", "call_id": "call_test", "output": "42"});
        let mut request = json!({"model": "mock-model", "instructions": "Base", "input": [
            {"role": "system", "content": "Initial"},
            {"role": "system", "content": [{"type": "input_text", "text": "More"}]},
            compaction, {"role": "user", "content": "Question"},
            {"role": "system", "content": "Updated"}, reasoning, call, result
        ]});
        assert!(matches!(normalize_request(&mut request), Ok(false)));
        assert_eq!(request["instructions"], "Base");
        assert_eq!(
            request["input"],
            json!([
                {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "Initial\n\nMore"}]},
                compaction,
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Question"}]},
                {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "Updated"}]},
                reasoning, call, result
            ])
        );
        assert_eq!(request["store"], false);
        assert_eq!(request["stream"], true);

        let mut request = json!({"model": "mock-model", "input": "Hello"});
        assert!(normalize_request(&mut request).is_ok());
        assert!(request.get("instructions").is_none());
    }

    async fn start(app: Router) -> Result<(String, JoinHandle<std::io::Result<()>>)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let task = tokio::spawn(async move { axum::serve(listener, app).await });
        Ok((url, task))
    }

    #[tokio::test]
    async fn preserves_provider_error_codes_in_json_and_chat_streams() -> Result<()> {
        let event = json!({"type": "response.failed", "response": {"error": {
            "code": "usage_limit_reached", "message": "Quota exhausted"
        }}});
        let mock = Router::new()
            .route("/stream", get(move || async move {
                ([(header::CONTENT_TYPE, "text/event-stream")], format!("data: {event}\n\n"))
            }))
            .route("/rejected", get(|| async {
                (StatusCode::TOO_MANY_REQUESTS, Json(json!({"error": {
                    "type": "rate_limit_error", "code": "rate_limit_exceeded", "message": "Slow down"
                }})))
            }));
        let (base, task) = start(mock).await?;
        let client = Client::builder().no_proxy().build()?;
        let request = || client.get(format!("{base}/stream")).send();
        let error = collect_response(request().await?, Trace::default())
            .await
            .err()
            .context("expected a quota error")?;
        assert_eq!(error.status, StatusCode::PAYMENT_REQUIRED);
        assert_eq!(error.payload()["error"]["code"], "usage_limit_reached");

        let (_, options) = chat::into_responses(json!({
            "model": "mock-model", "messages": [{"role": "user", "content": "Test"}], "stream": true
        }))
        .map_err(anyhow::Error::msg)?;
        let response = stream_response(
            request().await?,
            Output::Chat(ChatStream::new(options)),
            Trace::default(),
        );
        let body = axum::body::to_bytes(response.into_body(), 4096).await?;
        let body = std::str::from_utf8(&body)?;
        assert!(body.contains("\"code\":\"usage_limit_reached\""));
        assert!(!body.contains("[DONE]"));

        let error =
            ApiError::from_upstream(client.get(format!("{base}/rejected")).send().await?).await;
        assert_eq!(error.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(error.payload()["error"]["code"], "rate_limit_exceeded");
        task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn distinguishes_sse_terminator_without_final_response_from_timeout() -> Result<()> {
        let mock = Router::new().route(
            "/{case}",
            get(
                |axum::extract::Path(case): axum::extract::Path<String>| async move {
                    let frames = if case == "done" {
                        "data: {\"type\":\"response.created\"}\n\ndata: [DONE]\n\n"
                    } else {
                        "data: {\"type\":\"response.created\"}\n\n"
                    };
                    let stream =
                        futures_util::stream::once(async move { Ok::<_, Infallible>(frames) })
                            .chain(futures_util::stream::pending());
                    (
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        axum::body::Body::from_stream(stream),
                    )
                },
            ),
        );
        let (base, task) = start(mock).await?;
        let client = Client::builder()
            .no_proxy()
            .read_timeout(std::time::Duration::from_secs(1))
            .build()?;
        let request = |case| client.get(format!("{base}/{case}")).send();
        let error = collect_response(request("done").await?, Trace::default())
            .await
            .err()
            .context("[DONE] alone must not succeed")?;
        assert_eq!(error.status, StatusCode::BAD_GATEWAY);
        assert_eq!(error.code, "upstream_error");

        let error = collect_response(request("timeout").await?, Trace::default())
            .await
            .err()
            .context("expected an upstream read timeout")?;
        assert_eq!(error.status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(error.code, "upstream_timeout");
        task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn serves_openai_formats_with_local_auth_and_codex_upstream() -> Result<()> {
        const KEY: &str = "only-for-the-local-mocked-api-test";
        let mock = Router::new()
            .route("/models", get(|headers: HeaderMap| async move {
                assert_eq!(headers[header::AUTHORIZATION], "Bearer test-upstream-token");
                Json(json!({"models": [
                    {"slug": "hidden", "visibility": "hide", "priority": 0},
                    {"slug": "mock-model", "visibility": "list", "priority": 1}
                ]}))
            }))
            .route("/responses", post(|headers: HeaderMap, Json(body): Json<Value>| async move {
                assert_eq!(headers[header::AUTHORIZATION], "Bearer test-upstream-token");
                assert_eq!(headers["chatgpt-account-id"], "test-account");
                assert_eq!(body["store"], false);
                assert_eq!(body["stream"], true);
                assert_eq!(body["input"][0]["content"][0]["text"], "Hello");
                let item = json!({"type": "message", "role": "assistant", "content": [
                    {"type": "output_text", "text": "Hi", "annotations": []}
                ]});
                let summary = json!({"id": "resp_test", "model": body["model"], "created_at": 1, "object": "response", "status": "completed", "output": [item], "usage": {"input_tokens": 2, "output_tokens": 1, "total_tokens": 3}});
                if body["model"] == "mock-json" {
                    return Json(summary).into_response();
                }
                if body["model"] == "mock-html" {
                    return ([(header::CONTENT_TYPE, "text/html")], "<html>do not forward</html>").into_response();
                }
                let events = [
                    json!({"type": "response.created", "response": {"id": "resp_test", "model": "mock-model", "created_at": 1}}),
                    json!({"type": "response.output_text.delta", "delta": "Hi"}),
                    json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
                    json!({"type": "response.completed", "response": {"id": "resp_test", "object": "response", "output": [], "usage": {"input_tokens": 2, "output_tokens": 1, "total_tokens": 3}}}),
                ];
                let stream: String = events.iter().map(|event| format!("data: {event}\n\n")).collect();
                let content_type = if body["model"] == "mock-plain" { "text/plain" } else { "text/event-stream" };
                ([(header::CONTENT_TYPE, content_type)], stream).into_response()
            }));
        let (upstream, upstream_task) = start(mock).await?;
        let directory = tempfile::tempdir()?;
        let client = Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(5))
            .build()?;
        let session = crate::auth::test_session(directory.path().join("private"), client.clone())?;
        let mut codex = Codex::new(client.clone(), session);
        codex.base_url = upstream;
        let (local, local_task) = start(router(codex, local_key(KEY)?, LogLevel::Off)).await?;

        let models_url = format!("{local}/v1/models");
        let rejected = client.get(&models_url).send().await?;
        assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
        let models: Value = client
            .get(&models_url)
            .bearer_auth(KEY)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        assert_eq!(models["data"][0]["id"], "mock-model");
        assert_eq!(models["data"].as_array().map(Vec::len), Some(1));
        let url = format!("{local}/v1/responses");
        let request = |stream| {
            client
                .post(&url)
                .bearer_auth(KEY)
                .json(&json!({"model":"mock-model", "input":"Hello", "stream":stream}))
        };
        let response: Value = request(false)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        assert_eq!(response["output"][0]["content"][0]["text"], "Hi");
        let events = request(true)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        assert!(events.contains("event: response.output_text.delta"));
        assert!(events.contains("event: response.completed"));
        let completed = events
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str::<Value>(data).ok())
            .find(|event| event["type"] == "response.completed")
            .context("expected a final event")?;
        assert_eq!(
            completed["response"]["output"][0]["content"][0]["text"],
            "Hi"
        );

        let url = format!("{local}/v1/chat/completions");
        let request = |model, stream| {
            client.post(&url).bearer_auth(KEY).json(&json!({
                "model": model, "messages": [{"role": "user", "content": "Hello"}], "stream": stream
            }))
        };
        for model in ["mock-model", "mock-plain", "mock-json"] {
            let response = request(model, false).send().await?;
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "upstream format for {model}"
            );
            let chat: Value = response.json().await?;
            assert_eq!(chat["choices"][0]["message"]["content"], "Hi");
            let chunks = request(model, true)
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?;
            assert!(
                chunks.contains("\"content\":\"Hi\""),
                "stream should contain text for {model}"
            );
            assert!(chunks.contains("data: [DONE]"));
        }
        let response = request("mock-html", false).send().await?;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let error: Value = response.json().await?;
        let message = error["error"]["message"]
            .as_str()
            .context("expected an error message")?;
        assert!(message.contains("text/html"));
        assert!(!message.contains("do not forward"));
        local_task.abort();
        upstream_task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn translates_chat_tool_calls_over_http() -> Result<()> {
        const KEY: &str = "only-for-the-local-mocked-api-test";
        const NAME: &str = "local_time";
        const CALL_ID: &str = "call_local_time";
        const ITEM_ID: &str = "fc_local_time";
        const ARGUMENT_DELTAS: [&str; 2] = [r#"{"city":"#, r#""Paris"}"#];
        const TOOL_OUTPUT: &str = "14:30";
        const ANSWER: &str = "It's 2:30 p.m. in Paris.";
        let arguments = ARGUMENT_DELTAS.concat();
        let parameters = json!({
            "type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]
        });
        let chat_tools = json!([{
            "type": "function", "function": {"name": NAME, "parameters": parameters}
        }]);
        let upstream_tools = json!([{
            "type": "function", "name": NAME, "parameters": parameters, "strict": false
        }]);
        let mock = Router::new().route(
            "/responses",
            post(move |Json(body): Json<Value>| {
                let upstream_tools = upstream_tools.clone();
                async move {
                    assert_eq!(body["tools"], upstream_tools);
                    assert_eq!(body["input"][0]["role"], "user");
                    let mut events = vec![json!({"type": "response.created", "response": {
                        "id": "resp_tools", "model": body["model"], "created_at": 1
                    }})];
                    let item = if body["input"].as_array().map(Vec::len) == Some(1) {
                        let mut item = json!({"type": "function_call", "id": ITEM_ID,
                            "call_id": CALL_ID, "name": NAME, "arguments": ""});
                        events.push(json!({
                            "type": "response.output_item.added", "output_index": 0, "item": item
                        }));
                        for delta in ARGUMENT_DELTAS {
                            events.push(json!({"type": "response.function_call_arguments.delta",
                                "output_index": 0, "item_id": ITEM_ID, "delta": delta}));
                        }
                        item["arguments"] = json!(ARGUMENT_DELTAS.concat());
                        item
                    } else {
                        let call = &body["input"][1];
                        assert_eq!(call["type"], "function_call");
                        assert_eq!(call["call_id"], CALL_ID);
                        assert_eq!(call["name"], NAME);
                        assert_eq!(call["arguments"], ARGUMENT_DELTAS.concat());
                        let output = &body["input"][2];
                        assert_eq!(output["type"], "function_call_output");
                        assert_eq!(output["call_id"], CALL_ID);
                        assert_eq!(output["output"], TOOL_OUTPUT);
                        json!({"type": "message", "role": "assistant", "content": [
                            {"type": "output_text", "text": ANSWER, "annotations": []}
                        ]})
                    };
                    events.push(json!({
                        "type": "response.output_item.done", "output_index": 0, "item": item
                    }));
                    events.push(json!({"type": "response.completed", "response": {
                        "id": "resp_tools", "model": body["model"], "created_at": 1,
                        "object": "response", "status": "completed", "output": [item]
                    }}));
                    let stream: String = events
                        .iter()
                        .map(|event| format!("data: {event}\n\n"))
                        .collect();
                    ([(header::CONTENT_TYPE, "text/event-stream")], stream)
                }
            }),
        );
        let (upstream, upstream_task) = start(mock).await?;
        let directory = tempfile::tempdir()?;
        let client = Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(5))
            .build()?;
        let session = crate::auth::test_session(directory.path().join("private"), client.clone())?;
        let mut codex = Codex::new(client.clone(), session);
        codex.base_url = upstream;
        let (local, local_task) = start(router(codex, local_key(KEY)?, LogLevel::Debug)).await?;
        let url = format!("{local}/v1/chat/completions");
        let user = json!({"role": "user", "content": "What time is it in Paris?"});
        let request = |messages: Value, stream| {
            client.post(&url).bearer_auth(KEY).json(&json!({
                "model": "mock-model", "messages": messages, "tools": chat_tools, "stream": stream
            }))
        };

        let stream = request(json!([user]), true)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let mut data: Vec<_> = stream
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .collect();
        assert_eq!(data.pop(), Some("[DONE]"));
        let chunks: Vec<Value> = data
            .into_iter()
            .map(serde_json::from_str)
            .collect::<std::result::Result<_, _>>()?;
        let (last, preceding) = chunks.split_last().context("expected a final chunk")?;
        assert_eq!(last["choices"][0]["index"], 0);
        assert_eq!(last["choices"][0]["finish_reason"], "tool_calls");
        let mut calls = Vec::new();
        for chunk in preceding {
            let choice = &chunk["choices"][0];
            assert_eq!(choice["index"], 0);
            assert!(choice["finish_reason"].is_null());
            if let Some(tool_calls) = choice["delta"]["tool_calls"].as_array() {
                calls.extend(tool_calls);
            }
        }
        assert_eq!(calls.len(), 1 + ARGUMENT_DELTAS.len());
        assert_eq!(calls[0]["id"], CALL_ID);
        assert_eq!(calls[0]["type"], "function");
        assert_eq!(calls[0]["function"]["name"], NAME);
        assert!(calls.iter().all(|call| call["index"] == 0));
        for (call, delta) in calls[1..].iter().zip(ARGUMENT_DELTAS) {
            assert_eq!(call["function"]["arguments"], delta);
        }

        let chat: Value = request(json!([user]), false)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let choice = &chat["choices"][0];
        assert_eq!(choice["finish_reason"], "tool_calls");
        let message = &choice["message"];
        assert_eq!(message["role"], "assistant");
        assert_eq!(
            message["tool_calls"],
            json!([{"id": CALL_ID, "type": "function",
                "function": {"name": NAME, "arguments": arguments}}])
        );

        let follow_up: Value = request(
            json!([user, message, {
                "role": "tool", "tool_call_id": CALL_ID, "content": TOOL_OUTPUT
            }]),
            false,
        )
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
        assert_eq!(follow_up["choices"][0]["message"]["content"], ANSWER);
        assert_eq!(follow_up["choices"][0]["finish_reason"], "stop");
        local_task.abort();
        upstream_task.abort();
        Ok(())
    }
}
