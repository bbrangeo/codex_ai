//! Maps Responses payloads and events to Chat Completions.

use serde_json::{Value, json};

use super::{nonempty, request::ChatOptions};

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
pub fn completion(response: &Value, model: &str) -> Value {
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
pub struct ChatStream {
    options: ChatOptions,
    metadata: Option<Metadata>,
    // ponytail: linear lookup across the tool calls in one choice; keep index order naturally.
    tool_calls: Vec<StreamToolCall>,
    finished: bool,
}

impl ChatStream {
    /// Starts an empty mapper for the client options returned by request conversion.
    pub fn new(options: ChatOptions) -> Self {
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
    pub fn chunks(&mut self, event: &Value) -> Vec<Value> {
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
