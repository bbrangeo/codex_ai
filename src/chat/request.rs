//! Converts Chat Completions requests to Responses requests.

use serde_json::{Map, Value, json};

use super::nonempty;
use crate::InvalidRequest;

/// Client preferences needed to turn a Responses payload back into Chat Completions.
#[derive(Debug)]
pub struct ChatOptions {
    pub(super) model: String,
    pub(super) stream: bool,
    pub(super) include_usage: bool,
    pub(super) legacy_functions: bool,
}

impl ChatOptions {
    /// Returns the model requested by the client, for responses with missing model metadata.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Returns whether the client expects server-sent events instead of JSON.
    pub fn stream(&self) -> bool {
        self.stream
    }
}

/// Converts a Chat request without adding transport defaults.
///
/// # Errors
/// Returns an error for malformed input or unsupported Chat features.
pub fn into_responses(body: Value) -> Result<(Value, ChatOptions), InvalidRequest> {
    convert(body).map_err(InvalidRequest)
}

fn convert(body: Value) -> Result<(Value, ChatOptions), String> {
    let Value::Object(mut body) = body else {
        return Err("expected a JSON object".into());
    };
    let model = match body.remove("model") {
        Some(Value::String(model)) if !model.trim().is_empty() => model,
        _ => return Err("model must be a non-empty string".into()),
    };
    let stream = match body.remove("stream") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(stream)) => stream,
        _ => return Err("stream must be a boolean".into()),
    };
    let include_usage = match body.remove("stream_options") {
        None | Some(Value::Null) => false,
        Some(Value::Object(mut options)) if stream => {
            let include_usage = options.remove("include_usage");
            if let Some((field, _)) = options.iter().find(|(_, value)| !value.is_null()) {
                return Err(format!(
                    "unsupported Chat Completions option: stream_options.{field}"
                ));
            }
            match include_usage {
                None | Some(Value::Null) => false,
                Some(Value::Bool(value)) => value,
                _ => return Err("stream_options.include_usage must be a boolean".into()),
            }
        }
        _ => return Err("stream_options requires stream=true and an object".into()),
    };
    if let Some(n) = body.remove("n")
        && !n.is_null()
        && n.as_u64() != Some(1)
    {
        return Err("only n=1 is supported".into());
    }
    let Some(Value::Array(messages)) = body.remove("messages") else {
        return Err("messages must be a non-empty array".into());
    };
    if messages.is_empty() {
        return Err("messages must be a non-empty array".into());
    }
    let has_option = |key| body.get(key).is_some_and(|value| !value.is_null());
    for (legacy, modern) in [("functions", "tools"), ("function_call", "tool_choice")] {
        if has_option(legacy) && has_option(modern) {
            return Err(format!("use either {legacy} or {modern}, not both"));
        }
    }
    let legacy_functions = has_option("functions") || has_option("function_call");
    let mut input = Vec::with_capacity(messages.len());
    let mut pending_function = None;
    for (index, message) in messages.into_iter().enumerate() {
        append_input(message, &mut input, &mut pending_function)
            .map_err(|error| format!("messages[{index}]: {error}"))?;
    }
    let mut request = json!({"model": model, "input": input});
    let mut text = Map::new();
    let max_tokens = body.remove("max_tokens").filter(|value| !value.is_null());
    let max_completion_tokens = body
        .remove("max_completion_tokens")
        .filter(|value| !value.is_null());
    if max_tokens.is_some() && max_completion_tokens.is_some() {
        return Err("use either max_tokens or max_completion_tokens, not both".into());
    }
    if let Some(limit) = max_completion_tokens.or(max_tokens) {
        if !matches!(limit.as_u64(), Some(1..)) {
            return Err("the token limit must be a positive integer".into());
        }
        request["max_output_tokens"] = limit;
    }
    let reasoning = reasoning_config(body.remove("reasoning"), body.remove("reasoning_effort"))?;
    if !reasoning.is_empty() {
        request["reasoning"] = Value::Object(reasoning);
    }
    for (key, value) in body {
        if value.is_null() {
            continue;
        }
        match key.as_str() {
            "store" if value == false => {}
            "store" => return Err("only store=false is supported".into()),
            "modalities" if value == json!(["text"]) => {}
            "modalities" | "audio" => return Err("only text output is supported".into()),
            "frequency_penalty" | "presence_penalty" if value.as_f64() == Some(0.0) => {}
            "logprobs" if value == false => {}
            "top_logprobs" if value.as_u64() == Some(0) => {}
            "logit_bias" if value.as_object().is_some_and(Map::is_empty) => {}
            "stop" if value.as_array().is_some_and(Vec::is_empty) => {}
            "tools" | "functions" => {
                let Value::Array(tools) = value else {
                    return Err(format!("{key} must be an array"));
                };
                if !tools.is_empty() {
                    let convert = if key == "functions" {
                        function_definition
                    } else {
                        tool_definition
                    };
                    request["tools"] = Value::Array(
                        tools
                            .into_iter()
                            .map(convert)
                            .collect::<Result<Vec<_>, _>>()?,
                    );
                }
            }
            "tool_choice" => request["tool_choice"] = tool_choice(&value)?,
            "function_call" => {
                request["tool_choice"] = if matches!(value.as_str(), Some("none" | "auto")) {
                    value
                } else if let Some(name) = nonempty(&value["name"]) {
                    json!({"type": "function", "name": name})
                } else {
                    return Err("function_call must be none, auto, or a named function".into());
                };
            }
            "parallel_tool_calls" if value.is_boolean() => request[key] = value,
            "parallel_tool_calls" => return Err("parallel_tool_calls must be a boolean".into()),
            "user" if value.is_string() => {
                if request.get("safety_identifier").is_none() {
                    request["safety_identifier"] = value;
                }
            }
            "user" => return Err("user must be a string".into()),
            "temperature"
            | "top_p"
            | "service_tier"
            | "metadata"
            | "safety_identifier"
            | "prompt_cache_key"
            | "prompt_cache_retention" => request[key] = value,

            "response_format" => {
                text.insert("format".into(), response_format(value)?);
            }
            "verbosity" if value.is_string() => {
                text.insert(key, value);
            }
            "verbosity" => return Err("verbosity must be a string".into()),
            _ => return Err(format!("unsupported Chat Completions option: {key}")),
        }
    }
    if !text.is_empty() {
        request["text"] = Value::Object(text);
    }
    if legacy_functions {
        if request["parallel_tool_calls"] == true {
            return Err("legacy function_call does not support parallel tool calls".into());
        }
        request["parallel_tool_calls"] = Value::Bool(false);
    } else if request.get("tools").is_some() && request.get("parallel_tool_calls").is_none() {
        request["parallel_tool_calls"] = Value::Bool(true);
    }
    Ok((
        request,
        ChatOptions {
            model,
            stream,
            include_usage,
            legacy_functions,
        },
    ))
}

fn reasoning_config(
    option: Option<Value>,
    legacy_effort: Option<Value>,
) -> Result<Map<String, Value>, String> {
    let enable_reasoning = option == Some(Value::Bool(true));
    let mut reasoning = match option {
        None | Some(Value::Null | Value::Bool(_)) => Map::new(),
        Some(Value::Object(reasoning)) => reasoning,
        Some(effort @ Value::String(_)) => Map::from_iter([("effort".into(), effort)]),
        Some(other) => {
            let kind = if other.is_array() { "array" } else { "number" };
            return Err(format!(
                "reasoning must be an object, string or boolean (got {kind})"
            ));
        }
    };
    reasoning.retain(|_, value| !value.is_null());
    for field in ["effort", "summary"] {
        if reasoning.get(field).is_some_and(|value| !value.is_string()) {
            return Err(format!("reasoning.{field} must be a string"));
        }
    }
    let fallback = match legacy_effort {
        effort @ Some(Value::String(_)) => effort,
        None | Some(Value::Null) => enable_reasoning.then(|| json!("medium")),
        _ => return Err("reasoning_effort must be a string".into()),
    };
    if let Some(effort) = fallback {
        reasoning.entry("effort").or_insert(effort);
    }
    Ok(reasoning)
}

fn append_input(
    message: Value,
    input: &mut Vec<Value>,
    pending_function: &mut Option<usize>,
) -> Result<(), String> {
    let Value::Object(mut message) = message else {
        return Err("each message must be an object".into());
    };
    let previous_function = pending_function.take();
    let role = message.remove("role");
    let role = match role.as_ref().and_then(Value::as_str) {
        Some(role @ ("system" | "developer" | "user" | "assistant" | "tool")) => role,
        Some("function") => {
            let call = previous_function
                .and_then(|index| input.get(index))
                .filter(|call| message.get("name") == call.get("name"))
                .ok_or(
                    "function messages must follow an assistant function_call with the same name",
                )?;
            message.insert("tool_call_id".into(), call["call_id"].clone());
            "tool"
        }
        _ => return Err("unsupported message role".into()),
    };
    let content = message.remove("content").unwrap_or_default();
    let tool_call_id = (role == "tool").then(|| message.remove("tool_call_id").unwrap_or_default());
    let mut calls = Vec::new();
    let mut refusal = Value::Null;
    let mut legacy_call = None;
    if role == "assistant" {
        calls = match message.remove("tool_calls") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(calls)) => calls,
            _ => return Err("assistant tool_calls must be an array".into()),
        };
        refusal = message.remove("refusal").unwrap_or_default();
        legacy_call = message
            .remove("function_call")
            .filter(|call| !call.is_null());
        if legacy_call.is_some() && !calls.is_empty() {
            return Err("assistant messages cannot mix function_call and tool_calls".into());
        }
        // A display summary from an earlier turn is not a replayable Responses reasoning item.
        if let Some(summary) = message.remove("reasoning_content")
            && !summary.is_null()
            && !summary.is_string()
        {
            return Err("reasoning_content must be a string".into());
        }
    }
    // Responses messages have no place for Chat participant names or Cursor history IDs.
    for field in ["name", "modelProviderMessageId"] {
        if let Some(value) = message.remove(field)
            && !value.is_null()
            && !value.is_string()
        {
            return Err(format!("{field} must be a string"));
        }
    }
    if let Some((field, _)) = message.iter().find(|(_, value)| !value.is_null()) {
        return Err(format!("unsupported message field: {field:?}"));
    }
    if let Some(id) = tool_call_id {
        if nonempty(&id).is_none() {
            return Err("tool messages require a non-empty tool_call_id".into());
        }
        let output = if content.is_string() {
            content
        } else {
            Value::Array(content_parts(content, role)?)
        };
        input.push(json!({"type": "function_call_output", "call_id": id, "output": output}));
        return Ok(());
    }
    let has_calls = !calls.is_empty() || legacy_call.is_some();
    if content.is_null() && !has_calls && refusal.is_null() {
        return Err(
            "message content is required unless the assistant has tool calls or a refusal".into(),
        );
    }
    let mut content = if content.is_null()
        || (has_calls && content.as_str().is_some_and(|s| s.trim().is_empty()))
    {
        Vec::new()
    } else {
        content_parts(content, role)?
    };
    if let Value::String(refusal) = refusal {
        content.push(json!({"type": "refusal", "refusal": refusal}));
    } else if !refusal.is_null() {
        return Err("assistant refusal must be a string".into());
    }
    if !content.is_empty() {
        input.push(json!({"type": "message", "role": role, "content": content}));
    }
    if let Some(function) = legacy_call {
        // Legacy Chat does not carry an ID, so derive one from the position in the history.
        *pending_function = Some(input.len());
        calls.push(json!({"type": "function", "id": format!("call_legacy_{}", input.len()), "function": function}));
    }
    for call in calls {
        if call["type"] != "function"
            || nonempty(&call["id"]).is_none()
            || nonempty(&call["function"]["name"]).is_none()
            || !call["function"]["arguments"].is_string()
        {
            return Err(
                "assistant tool calls require a function name, arguments string and id".into(),
            );
        }
        input.push(json!({"type": "function_call", "call_id": call["id"], "name": call["function"]["name"], "arguments": call["function"]["arguments"]}));
    }
    Ok(())
}

fn content_parts(content: Value, role: &str) -> Result<Vec<Value>, &'static str> {
    let part = |text: Value| {
        if role == "assistant" {
            json!({"type": "output_text", "text": text, "annotations": []})
        } else {
            json!({"type": "input_text", "text": text})
        }
    };
    match content {
        text @ Value::String(_) => Ok(vec![part(text)]),
        Value::Array(parts) if !parts.is_empty() => parts
            .into_iter()
            .map(|mut value| {
                if let Value::Object(fields) = &mut value
                    && fields.len() == 2
                    && fields.get("type").and_then(Value::as_str) == Some("text")
                    && let Some(text @ Value::String(_)) = fields.remove("text")
                {
                    return Ok(part(text));
                }
                if role == "user" && value["type"] == "image_url" {
                    return image_part(&value["image_url"]);
                }
                Err("only text parts and user image_url parts are supported")
            })
            .collect(),
        _ => Err("message content must be a string or a non-empty array of parts"),
    }
}

fn image_part(image: &Value) -> Result<Value, &'static str> {
    let url = nonempty(&image["url"]).ok_or("image_url.url must be a non-empty string")?;
    let detail = match &image["detail"] {
        Value::Null => "auto",
        value => match value.as_str() {
            Some(detail @ ("auto" | "low" | "high")) => detail,
            _ => return Err("image_url.detail must be auto, low, or high"),
        },
    };
    Ok(json!({"type": "input_image", "image_url": url, "detail": detail}))
}

fn tool_definition(mut tool: Value) -> Result<Value, &'static str> {
    if tool["type"] != "function" {
        return Err("only function tools are supported on Chat Completions");
    }
    function_definition(tool["function"].take())
}

fn function_definition(function: Value) -> Result<Value, &'static str> {
    let Value::Object(mut function) = function else {
        return Err("a function tool requires a function definition");
    };
    function.retain(|_, value| !value.is_null());
    if function.get("name").and_then(nonempty).is_none()
        || function.iter().any(|(key, value)| match key.as_str() {
            "name" => false,
            "description" => !value.is_string(),
            "parameters" => !value.is_object(),
            "strict" => !value.is_boolean(),
            _ => true,
        })
    {
        return Err("invalid function tool definition");
    }
    // Chat's default is non-strict; Responses may normalize the schema when omitted.
    function.entry("strict").or_insert(Value::Bool(false));
    function.insert("type".into(), json!("function"));
    Ok(Value::Object(function))
}

fn tool_choice(choice: &Value) -> Result<Value, &'static str> {
    if matches!(choice.as_str(), Some("none" | "auto" | "required")) {
        return Ok(choice.clone());
    }
    if choice["type"] == "function"
        && let Some(name) = nonempty(&choice["function"]["name"])
    {
        return Ok(json!({"type": "function", "name": name}));
    }
    Err("tool_choice must be none, auto, required, or a named function")
}

fn response_format(format: Value) -> Result<Value, &'static str> {
    if let Value::Object(mut fields) = format {
        match fields.remove("type").as_ref().and_then(Value::as_str) {
            Some(kind @ ("text" | "json_object")) if fields.is_empty() => {
                return Ok(json!({"type": kind}));
            }
            Some("json_schema") if fields.len() == 1 => {
                if let Some(Value::Object(mut schema)) = fields.remove("json_schema")
                    && schema.get("name").is_some_and(Value::is_string)
                    && schema.get("schema").is_some_and(Value::is_object)
                    && schema.keys().all(|key| {
                        matches!(key.as_str(), "name" | "schema" | "description" | "strict")
                    })
                {
                    schema.insert("type".into(), json!("json_schema"));
                    return Ok(Value::Object(schema));
                }
            }
            _ => {}
        }
    }
    Err("unsupported or invalid response_format")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_messages_and_options() {
        let (request, options) = into_responses(json!({
            "model": "codex", "stream": true, "stream_options": {"include_usage": true},
            "messages": [
                {"role": "system", "content": "system"},
                {"role": "developer", "content": "developer"},
                {"role": "user", "content": [{"type": "text", "text": "question"}]},
                {"role": "assistant", "content": "answer"}
            ],
            "n": 1, "store": false, "temperature": 0.5, "top_p": 0.9,
            "max_completion_tokens": 64, "reasoning_effort": "low",
            "response_format": {"type": "json_schema", "json_schema": {
                "name": "answer", "schema": {"type": "object"}, "strict": true
            }}, "verbosity": "low"
        }))
        .expect("text-only Chat request should convert");
        assert_eq!(options.model, "codex");
        assert!(options.stream && options.include_usage);
        assert_eq!(
            request,
            json!({
                "model": "codex", "input": [
                    {"type": "message", "role": "system", "content": [{"type": "input_text", "text": "system"}]},
                    {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "developer"}]},
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "question"}]},
                    {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "answer", "annotations": []}]}
                ],
                "temperature": 0.5, "top_p": 0.9, "max_output_tokens": 64,
                "reasoning": {"effort": "low"}, "text": {"verbosity": "low", "format": {
                    "type": "json_schema", "name": "answer", "schema": {"type": "object"}, "strict": true
                }}
            })
        );
    }

    #[test]
    fn prefers_explicit_reasoning_effort_and_validates_reasoning_types() {
        let mut request = json!({
            "model": "codex", "messages": [{"role": "user", "content": "hi"}],
            "reasoning_effort": "low", "reasoning": {"effort": "high", "summary": "auto"}
        });
        let (converted, _) = into_responses(request.clone()).expect("reasoning should convert");
        assert_eq!(
            converted["reasoning"],
            json!({"effort": "high", "summary": "auto"})
        );
        request["reasoning"]["effort"] = Value::Null;
        let (converted, _) = into_responses(request.clone()).expect("legacy effort is a fallback");
        assert_eq!(
            converted["reasoning"],
            json!({"effort": "low", "summary": "auto"})
        );

        for (value, expected) in [
            (json!("high"), json!({"effort": "high"})),
            (json!(true), json!({"effort": "low"})),
            (json!(false), json!({"effort": "low"})),
        ] {
            request["reasoning"] = value;
            let (converted, _) = into_responses(request.clone()).expect("shorthand should convert");
            assert_eq!(converted["reasoning"], expected);
        }
        request["reasoning_effort"] = Value::Null;
        request["reasoning"] = json!(true);
        let (converted, _) = into_responses(request.clone()).expect("true enables reasoning");
        assert_eq!(converted["reasoning"], json!({"effort": "medium"}));
        request["reasoning"] = json!(false);
        let (converted, _) =
            into_responses(request.clone()).expect("false leaves the model default");
        assert!(converted.get("reasoning").is_none());

        for (value, error) in [
            (
                json!(3),
                "reasoning must be an object, string or boolean (got number)",
            ),
            (
                json!([]),
                "reasoning must be an object, string or boolean (got array)",
            ),
            (json!({"effort": 3}), "reasoning.effort must be a string"),
            (
                json!({"summary": true}),
                "reasoning.summary must be a string",
            ),
        ] {
            request["reasoning"] = value;
            assert_eq!(
                into_responses(request.clone())
                    .expect_err("invalid reasoning")
                    .to_string(),
                error
            );
        }
    }

    #[test]
    fn accepts_neutral_chat_options_and_names_unsupported_fields() {
        let mut request = json!({
            "model": "codex", "messages": [{"role": "user", "content": "hi"}],
            "frequency_penalty": 0, "presence_penalty": 0.0, "logprobs": false,
            "top_logprobs": 0, "logit_bias": {}, "stop": [], "seed": null
        });
        let (converted, _) =
            into_responses(request.clone()).expect("neutral options should be omitted");
        for field in [
            "frequency_penalty",
            "presence_penalty",
            "logprobs",
            "top_logprobs",
            "logit_bias",
            "stop",
            "seed",
        ] {
            assert!(
                converted.get(field).is_none(),
                "unexpected forwarded field {field}"
            );
        }
        request["user"] = json!("client-user-123");
        let (converted, _) =
            into_responses(request.clone()).expect("legacy user should map to safety_identifier");
        assert_eq!(converted["safety_identifier"], "client-user-123");
        request["safety_identifier"] = json!("explicit-id");
        let (converted, _) = into_responses(request.clone())
            .expect("explicit safety identifier should take priority");
        assert_eq!(converted["safety_identifier"], "explicit-id");
        request["frequency_penalty"] = json!(0.5);
        let error = into_responses(request).expect_err("active penalty cannot be silently ignored");
        assert!(error.to_string().contains("frequency_penalty"));
    }

    #[test]
    fn ignores_null_message_fields_and_names_unsupported_fields() {
        let mut request = json!({
            "model": "codex", "messages": [
                {"role": "user", "content": "hi", "future_field": null},
                {"role": "tool", "content": "result", "tool_call_id": "call_1", "future_field": null}
            ]
        });
        let (converted, _) =
            into_responses(request.clone()).expect("null message fields should be omitted");
        assert_eq!(
            converted["input"],
            json!([
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                {"type": "function_call_output", "call_id": "call_1", "output": "result"}
            ])
        );
        for index in 0..2 {
            let mut request = request.clone();
            request["messages"][index]["future_field"] = json!("not echoed");
            let error =
                into_responses(request).expect_err("active unknown fields must be rejected");
            assert_eq!(
                error.to_string(),
                format!("messages[{index}]: unsupported message field: \"future_field\"")
            );
        }
        request["messages"][1]["tool_call_id"] = Value::Null;
        assert!(into_responses(request).is_err(), "tool_call_id is required");
    }

    #[test]
    fn omits_chat_names_and_cursor_ids_from_history() {
        let mut request = json!({
            "model": "codex", "stream": true, "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "hello"},
                {"role": "user", "content": "follow-up"}
            ]
        });
        let (expected, _) = into_responses(request.clone()).expect("plain history should convert");
        request["messages"][1]["modelProviderMessageId"] = json!("msg_previous_turn");
        request["messages"][1]["name"] = json!("cursor");
        let (converted, _) =
            into_responses(request.clone()).expect("Cursor history should convert");
        assert_eq!(converted, expected);

        for field in ["name", "modelProviderMessageId"] {
            let mut request = request.clone();
            request["messages"][1][field] = json!({});
            let error = into_responses(request).expect_err("message metadata must be a string");
            assert_eq!(
                error.to_string(),
                format!("messages[1]: {field} must be a string")
            );
        }
    }

    #[test]
    fn rejects_unmapped_requests() {
        for (field, value) in [
            ("n", json!(2)),
            (
                "tools",
                json!([{"type": "custom", "custom": {"name": "test"}}]),
            ),
            ("tool_choice", json!("unknown")),
            ("parallel_tool_calls", json!("false")),
            ("stop", json!("END")),
            ("modalities", json!(["text", "audio"])),
            ("stream_options", json!({"include_usage": true})),
            ("store", json!(true)),
            (
                "messages",
                json!([{"role": "user", "content": [{"type": "image_url", "image_url": {}}]}]),
            ),
            (
                "messages",
                json!([{"role": "assistant", "content": null, "tool_calls": [{"type":"function", "function":{"name":"test", "arguments":{}}}]}]),
            ),
            ("messages", json!([{"role": "tool", "content": "x"}])),
        ] {
            let mut request =
                json!({"model": "codex", "messages": [{"role": "user", "content": "hi"}]});
            request[field] = value;
            assert!(
                into_responses(request).is_err(),
                "expected {field} to be rejected"
            );
        }
    }

    #[test]
    fn converts_user_images_in_order() {
        let mut body = json!({"model": "codex", "messages": [{"role": "user", "content": [
            {"type": "text", "text": "Compare these"},
            {"type": "image_url", "image_url": {"url": "https://example.com/image.png", "detail": "high"}},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,YQ=="}}
        ]}]});
        let (request, _) = into_responses(body.clone()).expect("user images should convert");
        assert_eq!(
            request["input"][0]["content"],
            json!([
                {"type": "input_text", "text": "Compare these"},
                {"type": "input_image", "image_url": "https://example.com/image.png", "detail": "high"},
                {"type": "input_image", "image_url": "data:image/png;base64,YQ==", "detail": "auto"}
            ])
        );
        body["messages"][0]["content"][1]["image_url"]["detail"] = json!("invalid");
        assert!(into_responses(body).is_err());
    }

    #[test]
    fn validates_legacy_function_options_and_result_pairing() {
        let mut body = json!({"model": "codex", "messages": [{"role": "user", "content": "hi"}],
            "functions": [{"name": "lookup"}]});
        for choice in ["auto", "none"] {
            body["function_call"] = json!(choice);
            let (request, _) =
                into_responses(body.clone()).expect("legacy function choice should convert");
            assert_eq!(request["tool_choice"], choice);
            assert_eq!(request["parallel_tool_calls"], false);
        }
        body["tools"] = json!([]);
        assert!(
            into_responses(body.clone()).is_err(),
            "conflicting tool declarations"
        );
        body["tools"] = Value::Null;
        body["messages"] = json!([
            {"role": "assistant", "function_call": {"name": "lookup", "arguments": "{}"}},
            {"role": "function", "name": "different_function", "content": "result"}
        ]);
        assert!(
            into_responses(body).is_err(),
            "unmatched legacy function result"
        );
    }

    #[test]
    fn converts_named_tool_choice_and_text_tool_result() {
        let (request, _) = into_responses(json!({
            "model": "codex", "parallel_tool_calls": false,
            "tool_choice": {"type": "function", "function": {"name": "get_local_time"}},
            "tools": [{"type": "function", "function": {"name": "get_local_time", "strict": true}}],
            "messages": [
                {"role": "assistant", "content": "I will check", "refusal": null,
                    "tool_calls": [{"type":"function", "id":"call_time", "function":{"name":"get_local_time", "arguments":"{\"city\":\"Paris\"}"}}]},
                {"role": "tool", "tool_call_id":"call_time", "content": [{"type":"text", "text":"15:30"}]}
            ]
        })).expect("tool history should convert");
        assert_eq!(
            request["tool_choice"],
            json!({"type": "function", "name": "get_local_time"})
        );
        assert_eq!(
            request["tools"],
            json!([{"type": "function", "name": "get_local_time", "strict": true}])
        );
        assert_eq!(request["parallel_tool_calls"], false);
        assert_eq!(
            request["input"],
            json!([
                {"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"I will check", "annotations":[]}]},
                {"type":"function_call", "call_id":"call_time", "name":"get_local_time", "arguments":"{\"city\":\"Paris\"}"},
                {"type":"function_call_output", "call_id":"call_time", "output":[{"type":"input_text", "text":"15:30"}]}
            ])
        );
    }
}
