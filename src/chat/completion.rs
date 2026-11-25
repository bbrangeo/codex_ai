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

fn finish_reason(response: &Value, call_reason: Option<&'static str>) -> &'static str {
    match response["incomplete_details"]["reason"].as_str() {
        Some("max_tokens" | "max_output_tokens") => "length",
        Some("content_filter") => "content_filter",
        _ => call_reason.unwrap_or("stop"),
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

/// Converts the terminal Responses payload into a single Chat choice using current tool fields.
pub fn completion(response: &Value, model: &str) -> Value {
    complete(response, model, "tool_calls")
}

impl ChatOptions {
    /// Converts the terminal payload using the Chat client’s function format.
    pub fn completion(&self, response: &Value) -> Value {
        complete(response, &self.model, self.call_field())
    }

    fn call_field(&self) -> &'static str {
        if self.legacy_functions {
            "function_call"
        } else {
            "tool_calls"
        }
    }
}

const SUMMARY_SEPARATOR: &str = "\n\n";

fn summary_parts(item: &Value) -> impl Iterator<Item = (u64, &str)> {
    item["summary"]
        .as_array()
        .into_iter()
        .flatten()
        .zip(0..)
        .filter_map(|(part, index)| {
            let text = part["text"].as_str()?;
            (part["type"] == "summary_text" && !text.is_empty()).then_some((index, text))
        })
}

fn complete(response: &Value, model: &str, call_field: &'static str) -> Value {
    let mut content = None;
    let mut refusal = None;
    let mut summaries = Vec::new();
    let mut tool_calls = Vec::new();
    for item in response["output"].as_array().into_iter().flatten() {
        if item["type"] == "reasoning" {
            summaries.extend(summary_parts(item).map(|(_, text)| text));
        }
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
    if !summaries.is_empty() {
        message["reasoning_content"] = json!(summaries.join(SUMMARY_SEPARATOR));
    }
    let call_reason = (!tool_calls.is_empty()).then_some(call_field);
    if let Some(field) = call_reason {
        message[field] = if field == "function_call" {
            tool_calls[0]["function"].take()
        } else {
            Value::Array(tool_calls)
        };
    }
    let mut result = Metadata::new(response, model).envelope(
        "chat.completion",
        json!([{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason(response, call_reason),
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

#[derive(Debug, Default)]
struct StreamReasoning {
    current_item_id: Option<String>,
    parts: Vec<StreamSummaryPart>,
}

#[derive(Debug)]
struct StreamSummaryPart {
    item_id: Option<String>,
    output_index: Option<u64>,
    index: u64,
    text: String,
}

impl StreamReasoning {
    fn update(
        &mut self,
        item_id: Option<&str>,
        output_index: Option<u64>,
        index: u64,
        text: &str,
        snapshot: bool,
    ) -> Option<Value> {
        if text.is_empty() {
            return None;
        }
        let item_id = item_id.or_else(|| {
            self.current_item_id
                .as_deref()
                .filter(|_| output_index.is_none())
        });
        let part = self.parts.iter_mut().rfind(|part| {
            part.index == index
                && match (
                    item_id.zip(part.item_id.as_deref()),
                    output_index.zip(part.output_index),
                ) {
                    (Some((id, other)), _) => id == other,
                    (_, Some((index, other))) => index == other,
                    _ => part.item_id.is_none() && part.output_index.is_none(),
                }
        });
        if let Some(part) = part {
            if part.item_id.is_none() {
                part.item_id = item_id.map(str::to_owned);
            }
            part.output_index = part.output_index.or(output_index);
            let delta = if snapshot {
                text.strip_prefix(&part.text)?
            } else {
                text
            };
            if delta.is_empty() {
                return None;
            }
            part.text.push_str(delta);
            return Some(json!({"reasoning_content": delta}));
        }
        let separator = if self.parts.is_empty() {
            ""
        } else {
            SUMMARY_SEPARATOR
        };
        self.parts.push(StreamSummaryPart {
            item_id: item_id.map(str::to_owned),
            output_index,
            index,
            text: text.to_owned(),
        });
        Some(json!({"reasoning_content": format!("{separator}{text}")}))
    }
}

/// Tracks one choice's headers, reasoning summaries, and tool calls across Responses events.
#[derive(Debug)]
pub struct ChatStream {
    options: ChatOptions,
    metadata: Option<Metadata>,
    // ponytail: linear lookup across the tool calls in one choice; keep index order naturally.
    tool_calls: Vec<StreamToolCall>,
    reasoning: StreamReasoning,
    finished: bool,
}

impl ChatStream {
    /// Starts an empty mapper for the client options returned by request conversion.
    pub fn new(options: ChatOptions) -> Self {
        Self {
            options,
            metadata: None,
            tool_calls: Vec::new(),
            reasoning: StreamReasoning::default(),
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

    fn observe_output(&mut self, item: &Value, output_index: Option<u64>, deltas: &mut Vec<Value>) {
        if item["type"] == "reasoning" {
            deltas.extend(summary_parts(item).filter_map(|(index, text)| {
                self.reasoning
                    .update(nonempty(&item["id"]), output_index, index, text, true)
            }));
        } else {
            deltas.extend(self.observe_tool(item, output_index));
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
            Some("response.output_item.added") => {
                let item = &event["item"];
                if item["type"] == "reasoning" {
                    self.reasoning.current_item_id = nonempty(&item["id"]).map(str::to_owned);
                }
                deltas.extend(self.observe_tool(item, event["output_index"].as_u64()));
            }
            Some("response.output_item.done") => {
                self.observe_output(&event["item"], event["output_index"].as_u64(), &mut deltas);
            }
            Some(
                "response.reasoning_summary_text.delta"
                | "response.reasoning_summary_text.done"
                | "response.reasoning_summary_part.done",
            ) => {
                let text = match kind {
                    Some("response.reasoning_summary_text.delta") => &event["delta"],
                    Some("response.reasoning_summary_text.done") => &event["text"],
                    _ if event["part"]["type"] == "summary_text" => &event["part"]["text"],
                    _ => return Vec::new(),
                };
                if let Some(text) = text.as_str() {
                    deltas.extend(self.reasoning.update(
                        nonempty(&event["item_id"]),
                        event["output_index"].as_u64(),
                        event["summary_index"].as_u64().unwrap_or_default(),
                        text,
                        kind != Some("response.reasoning_summary_text.delta"),
                    ));
                }
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
                for (item, index) in response["output"].as_array().into_iter().flatten().zip(0..) {
                    self.observe_output(item, Some(index), &mut deltas);
                }
            }
            _ => return Vec::new(),
        }
        if deltas.is_empty()
            && !self.finished
            && !matches!(kind, Some("response.created" | "response.in_progress"))
        {
            return Vec::new();
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
        for mut delta in deltas {
            if self.options.legacy_functions
                && let Some(call) = delta
                    .get_mut("tool_calls")
                    .and_then(|calls| calls.get_mut(0))
            {
                delta = json!({"function_call": call["function"].take()});
            }
            push_choice(delta, None);
        }
        if self.finished {
            push_choice(
                json!({}),
                Some(finish_reason(
                    response,
                    (!self.tool_calls.is_empty()).then(|| self.options.call_field()),
                )),
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
                    {"type": "reasoning", "content": [{"type": "reasoning_text", "text": "hidden"}],
                        "encrypted_content": "opaque", "summary": [
                            {"type": "summary_text", "text": "First"}, {"type": "summary_text", "text": "Second"}
                        ]},
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
                    "message": {"role": "assistant", "content": "hello!", "refusal": "no",
                        "reasoning_content": "First\n\nSecond"}}],
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
            legacy_functions: false,
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
    fn streams_reasoning_summaries_without_repeating_snapshots() {
        let (_, options) = crate::chat::into_responses(json!({
            "model": "codex", "stream": true, "messages": [{"role": "user", "content": "hi"}]
        }))
        .expect("request should convert");
        let mut stream = ChatStream::new(options);
        let item = json!({"type": "reasoning", "id": "rs_1", "summary": [
            {"type": "summary_text", "text": "Checking the answer"},
            {"type": "summary_text", "text": "Confirmed"}
        ]});
        let data = |chunks: Vec<Value>| {
            chunks
                .into_iter()
                .filter_map(|chunk| {
                    chunk["choices"][0]["delta"]["reasoning_content"]
                        .as_str()
                        .map(str::to_owned)
                })
                .collect::<String>()
        };
        assert_eq!(
            data(stream.chunks(&json!({
                "type": "response.reasoning_summary_text.delta", "summary_index": 0,
                "item_id": "rs_1", "delta": "Checking"
            }))),
            "Checking"
        );
        assert!(
            stream
                .chunks(&json!({"type": "response.reasoning_text.delta", "delta": "hidden"}))
                .is_empty()
        );
        assert_eq!(
            data(
                stream.chunks(&json!({"type": "response.reasoning_summary_text.done",
                    "item_id": "rs_1", "summary_index": 0, "text": "Checking the answer"
                }))
            ),
            " the answer"
        );
        assert_eq!(
            data(stream.chunks(&json!({
                "type": "response.output_item.done", "output_index": 0, "item": item
            }))),
            "\n\nConfirmed"
        );
        assert!(
            data(
                stream
                    .chunks(&json!({"type": "response.completed", "response": {"output": [item]}}))
            )
            .is_empty()
        );
    }

    #[test]
    fn streams_two_interleaved_calls_without_repeating_arguments() {
        let mut stream = ChatStream::new(ChatOptions {
            model: "codex".into(),
            stream: true,
            include_usage: false,
            legacy_functions: false,
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
            legacy_functions: false,
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
