//! Prepares native Responses requests for the stateless Codex backend.

use std::collections::HashMap;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};

use crate::InvalidRequest;

const MAX_CALL_ID_LEN: usize = 64;

/// Normalizes messages, forces upstream streaming, and returns the client’s streaming preference.
///
/// # Errors
/// Returns an error for malformed input, unsupported server-side state, or colliding call IDs.
pub fn normalize_request(body: &mut Value) -> Result<bool, InvalidRequest> {
    let (body, stream) = normalize_stateless_request(body)?;
    body.insert("store".into(), Value::Bool(false));
    body.insert("stream".into(), Value::Bool(true));
    Ok(stream)
}

/// Normalizes the history for native, non-streaming Codex compaction.
///
/// # Errors
/// Returns an error for malformed input, server-side state, or requested streaming.
pub fn normalize_compact_request(body: &mut Value) -> Result<(), InvalidRequest> {
    let (body, stream) = normalize_stateless_request(body)?;
    if stream {
        return Err(InvalidRequest(
            "compaction does not support streaming".into(),
        ));
    }
    body.remove("store");
    body.remove("stream");
    Ok(())
}

fn normalize_stateless_request(
    body: &mut Value,
) -> Result<(&mut Map<String, Value>, bool), InvalidRequest> {
    let body = body
        .as_object_mut()
        .ok_or_else(|| InvalidRequest("expected a JSON object".into()))?;
    if !body
        .get("model")
        .and_then(Value::as_str)
        .is_some_and(|model| !model.trim().is_empty())
    {
        return Err(InvalidRequest("model must be a non-empty string".into()));
    }
    if ["previous_response_id", "conversation"]
        .into_iter()
        .any(|key| body.get(key).is_some_and(|value| !value.is_null()))
        || ["store", "background"].into_iter().any(|key| {
            body.get(key)
                .is_some_and(|value| !value.is_null() && value != false)
        })
    {
        return Err(InvalidRequest(
            "stored responses, server-side conversations, and background mode are not supported"
                .into(),
        ));
    }
    let stream = match body.get("stream") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(stream)) => *stream,
        _ => return Err(InvalidRequest("stream must be a boolean".into())),
    };
    match body.get("instructions") {
        None | Some(Value::String(_)) => {}
        Some(Value::Null) => {
            body.remove("instructions");
        }
        _ => return Err(InvalidRequest("instructions must be a string".into())),
    }
    let mut input = match body.remove("input") {
        Some(Value::String(text)) => vec![json!({"role": "user", "content": text})],
        Some(Value::Array(input)) => input,
        _ => return Err(InvalidRequest("input must be a string or an array".into())),
    };
    normalize_call_ids(&mut input)?;
    body.insert("input".into(), Value::Array(normalize_input(input)));
    body.remove("conversation");
    body.remove("previous_response_id");
    body.remove("background");
    // Unlike the public Responses API, the Codex backend rejects this field.
    body.remove("safety_identifier");
    Ok((body, stream))
}

fn normalize_call_ids(input: &mut [Value]) -> Result<(), InvalidRequest> {
    if !input.iter().any(|item| {
        item["call_id"]
            .as_str()
            .is_some_and(|id| id.len() > MAX_CALL_ID_LEN)
    }) {
        return Ok(());
    }
    let mut original_by_id = HashMap::new();
    for item in input {
        let Some(Value::String(id)) = item.get_mut("call_id") else {
            continue;
        };
        // The same alias must be used in every replay, for both the call and its result.
        let replacement = (id.len() > MAX_CALL_ID_LEN).then(|| {
            format!(
                "call_{}",
                URL_SAFE_NO_PAD.encode(Sha256::digest(id.as_bytes()))
            )
        });
        let normalized = replacement.as_ref().unwrap_or(id);
        if let Some(original) = original_by_id.get(normalized) {
            if original != id {
                return Err(InvalidRequest("call IDs collide after shortening".into()));
            }
        } else {
            original_by_id.insert(normalized.clone(), id.clone());
        }
        if let Some(replacement) = replacement {
            *id = replacement;
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortens_long_call_ids_consistently_without_truncating() {
        let prefix = "x".repeat(MAX_CALL_ID_LEN);
        let first = format!("{prefix}a");
        let second = format!("{prefix}b");
        let original = json!({"model": "mock-model", "input": [
            {"type": "function_call", "id": "fc_123", "call_id": first},
            {"type": "function_call_output", "call_id": first},
            {"type": "function_call", "call_id": second},
            {"type": "function_call", "call_id": prefix}
        ]});
        let mut request = original.clone();
        normalize_request(&mut request).expect("history should normalize");
        let input = &request["input"];
        assert_eq!(input[0]["id"], "fc_123");
        assert_eq!(input[0]["call_id"], input[1]["call_id"]);
        assert_ne!(input[0]["call_id"], input[2]["call_id"]);
        assert_eq!(input[3]["call_id"], prefix);

        let mut replay = original.clone();
        normalize_request(&mut replay).expect("replay should normalize");
        assert_eq!(replay["input"], *input);

        let mut collision = original;
        collision["input"][3]["call_id"] = input[0]["call_id"].clone();
        let error = normalize_request(&mut collision).expect_err("IDs cannot be merged");
        assert_eq!(error.to_string(), "call IDs collide after shortening");
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
}
