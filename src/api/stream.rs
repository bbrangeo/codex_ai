//! Decodes Codex responses and delivers them as JSON or SSE to the client.

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::future::ready;
use std::pin::pin;

use axum::http::header;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use codex_ai::chat::ChatStream;
use eventsource_stream::{EventStreamError, Eventsource as _};
use futures_util::{Stream, StreamExt as _};
use serde_json::{Value, json};

use super::{ApiError, UpstreamResponse};
use crate::telemetry::Trace;

// Keep the original payload for native Responses clients, including fields we don't interpret.
enum ResponseEvent {
    Update(Value),
    Finished(Value),
    Failed(Value),
}

const MISSING_FINAL_RESPONSE: &str = "Codex stream ended without a final response";

fn events(
    upstream: UpstreamResponse,
    trace: Trace,
) -> impl Stream<Item = Result<ResponseEvent, ApiError>> {
    async_stream::try_stream! {
        let UpstreamResponse { response, permit: _permit } = upstream;
        let mut upstream = pin!(decode_response(response, trace));
        let mut items = BTreeMap::new();
        let mut pending_compactions = BTreeSet::new();
        while let Some(event) = upstream.next().await {
            let mut event = event?;
            let kind = event["type"].as_str().unwrap_or_default();
            match kind {
                "response.completed" | "response.incomplete" => {
                    if !pending_compactions.is_empty() {
                        Err(ApiError::gateway("Codex response ended with an unfinished compaction"))?;
                    }
                    let response = event.get_mut("response").filter(|response| response.is_object())
                        .ok_or_else(|| ApiError::gateway("missing final Codex response"))?;
                    // Codex may only send completed items in their own SSE events.
                    if response["output"].as_array().is_none_or(Vec::is_empty) && !items.is_empty() {
                        response["output"] = Value::Array(items.into_values().collect());
                    }
                    trace.event(format_args!("codex.stream.end event={}", event["type"].as_str().unwrap_or_default()));
                    yield ResponseEvent::Finished(event);
                    return;
                }
                kind if terminal_failure(kind) => {
                    trace.event(format_args!("codex.stream.end event={kind}"));
                    yield ResponseEvent::Failed(event);
                    return;
                }
                "response.output_item.added" if event["item"]["type"] == "compaction" => {
                    let index = event["output_index"].as_u64()
                        .ok_or_else(|| ApiError::gateway("missing output_index for Codex compaction"))?;
                    pending_compactions.insert(index);
                }
                "response.output_item.done" => {
                    if let Some(index) = event["output_index"].as_u64()
                        && let Some(item) = event.get("item").filter(|item| item.is_object())
                    {
                        if item["type"] == "compaction" {
                            pending_compactions.remove(&index);
                        }
                        items.insert(index, item.clone());
                    }
                }
                _ => {}
            }
            yield ResponseEvent::Update(event);
        }
        Err(ApiError::gateway(MISSING_FINAL_RESPONSE))?;
    }
}

fn decode_response(
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
}

pub(super) fn terminal_failure(kind: &str) -> bool {
    matches!(kind, "error" | "response.error" | "response.failed")
}

pub(super) async fn collect_response(
    upstream: UpstreamResponse,
    trace: Trace,
) -> Result<Value, ApiError> {
    let mut events = pin!(events(upstream, trace));
    while let Some(event) = events.next().await {
        match event? {
            ResponseEvent::Update(_) => {}
            ResponseEvent::Finished(mut event) => return Ok(event["response"].take()),
            ResponseEvent::Failed(event) => return Err(ApiError::stream_failure(&event)),
        }
    }
    Err(ApiError::gateway(MISSING_FINAL_RESPONSE))
}

pub(super) enum Output {
    Responses,
    Chat(ChatStream),
}

pub(super) fn stream_response(
    upstream: UpstreamResponse,
    mut output: Output,
    trace: Trace,
) -> Response {
    let stream = async_stream::stream! {
        let mut events = pin!(events(upstream, trace));
        let error = loop {
            let event = match events.next().await {
                Some(Ok(event)) => event,
                Some(Err(error)) => break error,
                None => return,
            };
            let terminal = !matches!(event, ResponseEvent::Update(_));
            let event = match event {
                ResponseEvent::Failed(event) if matches!(output, Output::Chat(_)) => {
                    break ApiError::stream_failure(&event);
                }
                ResponseEvent::Update(event) | ResponseEvent::Finished(event) | ResponseEvent::Failed(event) => event,
            };
            match &mut output {
                Output::Responses => {
                    let kind = event["type"].as_str().unwrap_or_default();
                    yield Ok::<_, Infallible>(Event::default().event(kind).data(event.to_string()));
                }
                Output::Chat(chat) => {
                    for chunk in chat.chunks(&event) {
                        yield Ok(Event::default().data(chunk.to_string()));
                    }
                    if terminal {
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
