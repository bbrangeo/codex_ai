//! Chat Completions text and function tools over Responses.

use serde_json::{Map, Value, json};

#[derive(Debug)]
pub(super) struct ChatOptions {
    pub(super) model: String,
    pub(super) stream: bool,
    pub(super) include_usage: bool,
}

/// Converts a Chat request without adding transport defaults.
///
/// # Errors
/// Returns an error for malformed input or unsupported Chat features.
pub(super) fn into_responses(body: Value) -> Result<(Value, ChatOptions), String> {
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
    let mut input = Vec::with_capacity(messages.len());
    for message in messages {
        append_input(message, &mut input)?;
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
            "tools" => {
                let Value::Array(tools) = value else {
                    return Err("tools must be an array".into());
                };
                if !tools.is_empty() {
                    request["tools"] = Value::Array(
                        tools
                            .into_iter()
                            .map(tool_definition)
                            .collect::<Result<Vec<_>, _>>()?,
                    );
                }
            }
            "tool_choice" => request["tool_choice"] = tool_choice(&value)?,
            "parallel_tool_calls" if value.is_boolean() => request[key] = value,
            "parallel_tool_calls" => return Err("parallel_tool_calls must be a boolean".into()),
            "functions" | "function_call" => {
                return Err("legacy functions are not supported; use tools and tool_choice".into());
            }
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
            "reasoning_effort" if value.is_string() => {
                request["reasoning"] = json!({"effort": value})
            }
            "reasoning_effort" => return Err("reasoning_effort must be a string".into()),
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
    if request.get("tools").is_some() && request.get("parallel_tool_calls").is_none() {
        request["parallel_tool_calls"] = Value::Bool(true);
    }
    Ok((
        request,
        ChatOptions {
            model,
            stream,
            include_usage,
        },
    ))
}

fn append_input(message: Value, input: &mut Vec<Value>) -> Result<(), &'static str> {
    let Value::Object(mut message) = message else {
        return Err("each message must be an object");
    };
    let role = message.remove("role");
    let role = match role.as_ref().and_then(Value::as_str) {
        Some(role @ ("system" | "developer" | "user" | "assistant" | "tool")) => role,
        _ => return Err("unsupported message role"),
    };
    let content = message.remove("content").unwrap_or_default();
    if role == "tool" {
        let id = message.remove("tool_call_id").unwrap_or_default();
        if nonempty(&id).is_none() || !message.is_empty() {
            return Err("tool messages require tool_call_id and content");
        }
        let output = if content.is_string() {
            content
        } else {
            Value::Array(text_parts(content, role)?)
        };
        input.push(json!({"type": "function_call_output", "call_id": id, "output": output}));
        return Ok(());
    }
    let mut calls = Vec::new();
    let mut refusal = Value::Null;
    if role == "assistant" {
        calls = match message.remove("tool_calls") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(calls)) => calls,
            _ => return Err("assistant tool_calls must be an array"),
        };
        refusal = message.remove("refusal").unwrap_or_default();
    }
    if !message.is_empty() {
        return Err("unsupported message field");
    }
    if content.is_null() && calls.is_empty() && refusal.is_null() {
        return Err("message content is required unless the assistant has tool calls or a refusal");
    }
    let mut content = if content.is_null()
        || (!calls.is_empty() && content.as_str().is_some_and(|s| s.trim().is_empty()))
    {
        Vec::new()
    } else {
        text_parts(content, role)?
    };
    if let Value::String(refusal) = refusal {
        content.push(json!({"type": "refusal", "refusal": refusal}));
    } else if !refusal.is_null() {
        return Err("assistant refusal must be a string");
    }
    if !content.is_empty() {
        input.push(json!({"type": "message", "role": role, "content": content}));
    }
    for call in calls {
        if call["type"] != "function"
            || nonempty(&call["id"]).is_none()
            || nonempty(&call["function"]["name"]).is_none()
            || !call["function"]["arguments"].is_string()
        {
            return Err("assistant tool calls require a function name, arguments string and id");
        }
        input.push(json!({"type": "function_call", "call_id": call["id"], "name": call["function"]["name"], "arguments": call["function"]["arguments"]}));
    }
    Ok(())
}

fn text_parts(content: Value, role: &str) -> Result<Vec<Value>, &'static str> {
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
                Err("only text message parts are supported")
            })
            .collect(),
        _ => Err("message content must be a string or a non-empty array of text parts"),
    }
}

fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.trim().is_empty())
}

fn tool_definition(mut tool: Value) -> Result<Value, &'static str> {
    if tool["type"] != "function" {
        return Err("only function tools are supported on Chat Completions");
    }
    let Value::Object(mut function) = tool["function"].take() else {
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

#[derive(Debug)]
struct Metadata {
    id: String,
    model: String,
    created: u64,
}

impl Metadata {
    fn new(response: &Value, model: &str) -> Self {
        let id = response["id"].as_str().unwrap_or("unknown");
        let id = if id.starts_with("chatcmpl-") {
            id.to_owned()
        } else {
            format!("chatcmpl-{}", id.strip_prefix("resp_").unwrap_or(id))
        };
        Self {
            id,
            model: response["model"].as_str().unwrap_or(model).to_owned(),
            created: response["created_at"].as_u64().unwrap_or_default(),
        }
    }

    fn envelope(&self, object: &str, choices: Value) -> Value {
        json!({
            "id": self.id, "object": object, "created": self.created,
            "model": self.model, "choices": choices,
        })
    }
}

fn finish_reason(response: &Value, has_tools: bool) -> &'static str {
    match response["incomplete_details"]["reason"].as_str() {
        Some("max_tokens" | "max_output_tokens") => "length",
        Some("content_filter") => "content_filter",
        _ if has_tools => "tool_calls",
        _ => "stop",
    }
}

struct FunctionCall<'a> {
    id: &'a str,
    name: &'a str,
    arguments: &'a str,
}

impl<'a> FunctionCall<'a> {
    fn from_item(item: &'a Value) -> Option<Self> {
        if item["type"] != "function_call" {
            return None;
        }
        Some(Self {
            id: nonempty(&item["call_id"]).or_else(|| nonempty(&item["id"]))?,
            name: nonempty(&item["name"])?,
            arguments: item["arguments"].as_str().unwrap_or_default(),
        })
    }

    fn chat_value(&self) -> Value {
        json!({"id": self.id, "type": "function", "function": {
            "name": self.name, "arguments": self.arguments
        }})
    }
}

fn usage(response: &Value) -> Option<Value> {
    let usage = response["usage"].as_object()?;
    let prompt = usage.get("input_tokens")?.as_u64()?;
    let completion = usage.get("output_tokens")?.as_u64()?;
    let total = usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| prompt.saturating_add(completion));
    let mut result = json!({
        "prompt_tokens": prompt, "completion_tokens": completion, "total_tokens": total,
    });
    for (source, target) in [
        ("input_tokens_details", "prompt_tokens_details"),
        ("output_tokens_details", "completion_tokens_details"),
    ] {
        if let Some(details) = usage.get(source).filter(|details| details.is_object()) {
            result[target] = details.clone();
        }
    }
    Some(result)
}

/// Converts the terminal Responses payload into a single Chat choice.
pub(super) fn completion(response: &Value, model: &str) -> Value {
    let mut content = None;
    let mut refusal = None;
    let mut tool_calls = Vec::new();
    for item in response["output"].as_array().into_iter().flatten() {
        if let Some(call) = FunctionCall::from_item(item) {
            tool_calls.push(call.chat_value());
        }
        if item["type"] != "message" {
            continue;
        }
        for part in item["content"].as_array().into_iter().flatten() {
            match part["type"].as_str() {
                Some("output_text") => append_text(&mut content, &part["text"]),
                Some("refusal") => append_text(&mut refusal, &part["refusal"]),
                _ => {}
            }
        }
    }
    let mut message = json!({"role": "assistant", "content": content, "refusal": refusal});
    let has_tools = !tool_calls.is_empty();
    if has_tools {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    let mut result = Metadata::new(response, model).envelope(
        "chat.completion",
        json!([{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason(response, has_tools),
        }]),
    );
    if let Some(usage) = usage(response) {
        result["usage"] = usage;
    }
    result
}

fn append_text(target: &mut Option<String>, value: &Value) {
    if let Some(text) = value.as_str() {
        target.get_or_insert_with(String::new).push_str(text);
    }
}

#[derive(Debug)]
struct StreamToolCall {
    id: String,
    item_id: Option<String>,
    output_index: Option<u64>,
    arguments: String,
}

impl StreamToolCall {
    fn append(&mut self, index: usize, delta: &str) -> Option<Value> {
        if delta.is_empty() {
            return None;
        }
        self.arguments.push_str(delta);
        Some(json!({"tool_calls": [{"index": index, "function": {"arguments": delta}}]}))
    }

    fn append_snapshot(&mut self, index: usize, arguments: &str) -> Option<Value> {
        // The final snapshot often repeats arguments already sent by earlier deltas.
        self.append(index, arguments.strip_prefix(&self.arguments)?)
    }
}

/// Tracks one choice's headers and tool calls across Responses events.
#[derive(Debug)]
pub(super) struct ChatStream {
    options: ChatOptions,
    metadata: Option<Metadata>,
    // ponytail: linear lookup across the tool calls in one choice; keep index order naturally.
    tool_calls: Vec<StreamToolCall>,
    finished: bool,
}

impl ChatStream {
    pub(super) fn new(options: ChatOptions) -> Self {
        Self {
            options,
            metadata: None,
            tool_calls: Vec::new(),
            finished: false,
        }
    }

    fn observe_tool(&mut self, item: &Value, output_index: Option<u64>) -> Option<Value> {
        let tool = FunctionCall::from_item(item)?;
        if let Some((index, call)) = self
            .tool_calls
            .iter_mut()
            .enumerate()
            .find(|(_, call)| call.id == tool.id)
        {
            return call.append_snapshot(index, tool.arguments);
        }
        // Responses output_index counts reasoning and text items too. Chat indexes only tools.
        let index = self.tool_calls.len();
        self.tool_calls.push(StreamToolCall {
            id: tool.id.to_owned(),
            item_id: nonempty(&item["id"]).map(str::to_owned),
            output_index,
            arguments: tool.arguments.to_owned(),
        });
        let mut call = tool.chat_value();
        call["index"] = json!(index);
        Some(json!({"tool_calls": [call]}))
    }

    fn tool_arguments(&mut self, event: &Value) -> Option<Value> {
        let item_id = nonempty(&event["item_id"]);
        let output_index = event["output_index"].as_u64();
        let (index, call) = self.tool_calls.iter_mut().enumerate().find(|(_, call)| {
            match (item_id, call.item_id.as_deref()) {
                (Some(item), Some(call_item)) => item == call_item,
                _ => output_index.is_some() && output_index == call.output_index,
            }
        })?;
        if event["type"] == "response.function_call_arguments.done" {
            call.append_snapshot(index, event["arguments"].as_str()?)
        } else {
            call.append(index, event["delta"].as_str()?)
        }
    }

    /// Converts one Responses event; SSE framing and errors belong to the caller.
    pub(super) fn chunks(&mut self, event: &Value) -> Vec<Value> {
        if self.finished {
            return Vec::new();
        }
        let kind = event["type"].as_str();
        let response = &event["response"];
        let mut deltas = Vec::new();
        match kind {
            Some("response.created" | "response.in_progress") => {}
            Some("response.output_text.delta" | "response.refusal.delta") => {
                let Some(text) = event["delta"].as_str() else {
                    return Vec::new();
                };
                let delta = if kind == Some("response.refusal.delta") {
                    json!({"refusal": text})
                } else {
                    json!({"content": text})
                };
                deltas.push(delta);
            }
            Some("response.output_item.added" | "response.output_item.done") => {
                let Some(delta) = self.observe_tool(&event["item"], event["output_index"].as_u64())
                else {
                    return Vec::new();
                };
                deltas.push(delta);
            }
            Some(
                "response.function_call_arguments.delta" | "response.function_call_arguments.done",
            ) => {
                let Some(delta) = self.tool_arguments(event) else {
                    return Vec::new();
                };
                deltas.push(delta);
            }
            Some("response.completed" | "response.incomplete") => {
                self.finished = true;
                deltas.extend(
                    response["output"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|item| self.observe_tool(item, None)),
                );
            }
            _ => return Vec::new(),
        }
        let mut chunks = Vec::new();
        let first = self.metadata.is_none();
        let metadata = self
            .metadata
            .get_or_insert_with(|| Metadata::new(response, &self.options.model));
        let mut push_choice = |delta: Value, finish: Option<&str>| {
            let mut chunk = metadata.envelope(
                "chat.completion.chunk",
                json!([{
                    "index": 0, "delta": delta, "finish_reason": finish,
                }]),
            );
            if self.options.include_usage {
                chunk["usage"] = Value::Null;
            }
            chunks.push(chunk);
        };
        if first {
            push_choice(json!({"role": "assistant", "content": ""}), None);
        }
        for delta in deltas {
            push_choice(delta, None);
        }
        if self.finished {
            push_choice(
                json!({}),
                Some(finish_reason(response, !self.tool_calls.is_empty())),
            );
        }
        if self.finished
            && self.options.include_usage
            && let Some(usage) = usage(response)
        {
            let mut chunk = metadata.envelope("chat.completion.chunk", json!([]));
            chunk["usage"] = usage;
            chunks.push(chunk);
        }
        chunks
    }
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
        assert!(error.contains("frequency_penalty"));
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

    #[test]
    fn joins_only_message_content_and_maps_usage() {
        let result = completion(
            &json!({
                "id": "resp_test", "created_at": 42, "output": [
                    {"type": "reasoning", "content": [{"type": "output_text", "text": "hidden"}]},
                    {"type": "message", "content": [
                        {"type": "output_text", "text": "hello"}, {"type": "output_text", "text": "!"},
                        {"type": "refusal", "refusal": "no"}
                    ]}
                ],
                "status": "incomplete", "incomplete_details": {"reason": "content_filter"},
                "usage": {"input_tokens": 8, "output_tokens": 4,
                    "input_tokens_details": {"cached_tokens": 3}, "output_tokens_details": {"reasoning_tokens": 2}}
            }),
            "codex",
        );
        assert_eq!(
            result,
            json!({
                "id": "chatcmpl-test", "created": 42, "model": "codex", "object": "chat.completion",
                "choices": [{"index": 0, "finish_reason": "content_filter",
                    "message": {"role": "assistant", "content": "hello!", "refusal": "no"}}],
                "usage": {"prompt_tokens": 8, "completion_tokens": 4, "total_tokens": 12,
                    "prompt_tokens_details": {"cached_tokens": 3}, "completion_tokens_details": {"reasoning_tokens": 2}}
            })
        );
    }

    #[test]
    fn streams_text_refusal_and_terminal_usage_with_stable_headers() {
        let mut stream = ChatStream::new(ChatOptions {
            model: "codex".into(),
            stream: true,
            include_usage: true,
        });
        let created = stream.chunks(&json!({"type": "response.created", "response": {
            "id": "resp_test", "model": "codex-version", "created_at": 42
        }}));
        assert_eq!(created[0]["choices"][0]["delta"]["role"], "assistant");
        assert!(
            stream
                .chunks(&json!({"type": "response.in_progress"}))
                .is_empty()
        );
        let text = stream.chunks(&json!({"type": "response.output_text.delta", "delta": "hi"}));
        assert_eq!(text[0]["choices"][0]["delta"]["content"], "hi");
        let refusal = stream.chunks(&json!({"type": "response.refusal.delta", "delta": "no"}));
        assert_eq!(refusal[0]["choices"][0]["delta"]["refusal"], "no");
        let terminal = json!({"type": "response.incomplete", "response": {
            "id": "resp_different", "incomplete_details": {"reason": "max_output_tokens"},
            "usage": {"input_tokens": 2, "output_tokens": 3, "total_tokens": 5}
        }});
        let end = stream.chunks(&terminal);
        assert_eq!(end.len(), 2);
        assert_eq!(end[0]["choices"][0]["finish_reason"], "length");
        assert_eq!(end[1]["choices"], json!([]));
        assert_eq!(end[1]["usage"]["total_tokens"], 5);
        for chunk in created.iter().chain(&text).chain(&refusal).chain(&end) {
            assert_eq!(chunk["id"], "chatcmpl-test");
            assert_eq!(chunk["created"], 42);
            assert_eq!(chunk["model"], "codex-version");
            if chunk["choices"] != json!([]) {
                assert_eq!(chunk.get("usage"), Some(&Value::Null));
            }
        }
        assert!(stream.chunks(&terminal).is_empty());
    }

    #[test]
    fn streams_two_interleaved_calls_without_repeating_arguments() {
        let mut stream = ChatStream::new(ChatOptions {
            model: "codex".into(),
            stream: true,
            include_usage: false,
        });
        let first = json!({"type": "function_call", "id":"fc_1", "call_id":"call_1", "name":"get_local_time", "arguments":""});
        let second = json!({"type": "function_call", "id":"fc_2", "call_id":"call_2", "name":"get_local_time", "arguments":""});
        let mut chunks = Vec::new();
        for event in [
            json!({"type":"response.created", "response":{"id":"resp_test"}}),
            json!({"type":"response.output_item.added", "output_index":2, "item":first}),
            json!({"type":"response.function_call_arguments.delta", "item_id":"fc_1", "delta":"{\"city\":"}),
            json!({"type":"response.output_item.added", "output_index":4, "item":second}),
            json!({"type":"response.function_call_arguments.delta", "output_index":4, "delta":"{\"city\":\"Rome\"}"}),
            json!({"type":"response.function_call_arguments.done", "item_id":"fc_1", "arguments":"{\"city\":\"Paris\"}"}),
            json!({"type":"response.output_item.done", "output_index":4, "item":{"type":"function_call", "call_id":"call_2", "name":"get_local_time", "arguments":"{\"city\":\"Rome\"}"}}),
            json!({"type":"response.completed", "response":{"output":[{"type":"function_call", "call_id":"call_1", "name":"get_local_time", "arguments":"{\"city\":\"Paris\"}"},{"type":"function_call", "call_id":"call_2", "name":"get_local_time", "arguments":"{\"city\":\"Rome\"}"}]}}),
        ] {
            chunks.extend(stream.chunks(&event));
        }
        let calls: Vec<_> = chunks
            .iter()
            .filter_map(|chunk| chunk["choices"][0]["delta"]["tool_calls"].get(0))
            .collect();
        assert_eq!(calls.len(), 5);
        assert_eq!(calls[0]["index"], 0);
        assert_eq!(calls[0]["id"], "call_1");
        assert_eq!(calls[2]["index"], 1);
        assert_eq!(calls[2]["id"], "call_2");
        assert_eq!(calls[3]["function"]["arguments"], "{\"city\":\"Rome\"}");
        assert_eq!(calls[4]["index"], 0);
        assert_eq!(calls[4]["function"]["arguments"], "\"Paris\"}");
        assert_eq!(
            chunks.last().expect("final chunk")["choices"][0]["finish_reason"],
            "tool_calls"
        );
    }

    #[test]
    fn completes_without_usage_when_not_requested() {
        let mut stream = ChatStream::new(ChatOptions {
            model: "codex".into(),
            stream: true,
            include_usage: false,
        });
        let chunks = stream.chunks(&json!({"type": "response.completed", "response": {
            "id": "resp_test", "usage": {"input_tokens": 2, "output_tokens": 0}
        }}));
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(chunks[1]["choices"][0]["finish_reason"], "stop");
        assert!(chunks.iter().all(|chunk| chunk.get("usage").is_none()));
    }
}
