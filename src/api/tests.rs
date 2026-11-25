use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::http::HeaderMap;
use futures_util::StreamExt as _;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use super::*;

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

pub(super) async fn start(app: Router) -> Result<(String, JoinHandle<std::io::Result<()>>)> {
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
    }))?;
    let response = stream_response(
        request().await?,
        Output::Chat(ChatStream::new(options)),
        Trace::default(),
    );
    let body = axum::body::to_bytes(response.into_body(), 4096).await?;
    let body = std::str::from_utf8(&body)?;
    assert!(body.contains("\"code\":\"usage_limit_reached\""));
    assert!(!body.contains("[DONE]"));

    let error = ApiError::from_upstream(client.get(format!("{base}/rejected")).send().await?).await;
    assert_eq!(error.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(error.payload()["error"]["code"], "rate_limit_exceeded");
    task.abort();
    Ok(())
}

#[tokio::test]
async fn requires_a_finished_compaction_before_a_successful_response() -> Result<()> {
    let mock = Router::new().route(
        "/{case}",
        get(|axum::extract::Path(case): axum::extract::Path<String>| async move {
            let item = json!({
                "type": "compaction", "id": "cmp_test", "encrypted_content": "opaque",
                "provider_extension": {"preserve": true}
            });
            let mut events = vec![json!({
                "type": "response.output_item.added", "output_index": 0, "item": {"type": "compaction"}
            })];
            if case == "done" {
                events.push(json!({
                    "type": "response.output_item.done", "output_index": 0, "item": item
                }));
            }
            events.push(json!({"type": "response.completed", "response": {"output": []}}));
            let body: String = events.iter().map(|event| format!("data: {event}\n\n")).collect();
            ([(header::CONTENT_TYPE, "text/event-stream")], body)
        }),
    );
    let (base, task) = start(mock).await?;
    let client = Client::builder().no_proxy().build()?;
    let request = |case| client.get(format!("{base}/{case}")).send();

    let incomplete = collect_response(request("missing").await?, Trace::default()).await;
    let error = incomplete
        .err()
        .context("unfinished compaction must not succeed")?;
    assert_eq!(error.status, StatusCode::BAD_GATEWAY);
    assert!(error.message.contains("unfinished compaction"));
    let stream = stream_response(
        request("missing").await?,
        Output::Responses,
        Trace::default(),
    );
    let body = axum::body::to_bytes(stream.into_body(), 4096).await?;
    let body = std::str::from_utf8(&body)?;
    assert!(body.contains("event: error"));
    assert!(!body.contains("event: response.completed"));

    let complete = collect_response(request("done").await?, Trace::default())
        .await
        .map_err(|error| anyhow::anyhow!(error.message))?;
    assert_eq!(complete["output"][0]["encrypted_content"], "opaque");
    assert_eq!(
        complete["output"][0]["provider_extension"],
        json!({"preserve": true})
    );
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
                let stream = futures_util::stream::once(async move { Ok::<_, Infallible>(frames) })
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
    let catalog_reads = Arc::new(AtomicUsize::new(0));
    let generations = Arc::new(AtomicUsize::new(0));
    let capabilities = json!({
        "context_window": 128_000,
        "supported_reasoning_levels": [
            {"effort": "low", "description": "Low"}, {"effort": "high"}
        ],
        "default_reasoning_level": "low",
        "input_modalities": ["text", "image"],
        "service_tiers": [{"id": "fast", "name": "Fast", "description": "Faster"}]
    });
    let mut model = capabilities.clone();
    model["slug"] = json!("mock-model");
    model["visibility"] = json!("list");
    model["priority"] = json!(1);
    model["base_instructions"] = json!("private provider instructions");
    let reads = Arc::clone(&catalog_reads);
    let calls = Arc::clone(&generations);
    let mock = Router::new()
        .route("/models", get(move |headers: HeaderMap| async move {
            reads.fetch_add(1, Ordering::Relaxed);
            assert_eq!(headers[header::AUTHORIZATION], "Bearer test-upstream-token");
            Json(json!({"models": [
                {"slug": "mock-json", "visibility": "list", "priority": 2},
                {"slug": "hidden", "visibility": "hide", "priority": 0},
                model
            ]}))
        }))
        .route("/responses", post(move |headers: HeaderMap, Json(body): Json<Value>| async move {
            calls.fetch_add(1, Ordering::Relaxed);
            assert_eq!(headers[header::AUTHORIZATION], "Bearer test-upstream-token");
            if body["model"] == "mock-model" {
                if body["service_tier"] == "priority" {
                    assert_eq!(body["reasoning"]["effort"], "high");
                } else {
                    assert_eq!(body["reasoning"]["effort"], "low");
                }
                if body["prompt_cache_key"] == "expect-summary" {
                    assert_eq!(body["reasoning"]["summary"], "auto");
                }
            } else {
                assert!(body.get("reasoning").is_none());
            }
            assert_eq!(headers["chatgpt-account-id"], "test-account");
            assert_eq!(body["store"], false);
            assert_eq!(body["stream"], true);
            assert_eq!(body["input"][0]["content"][0]["text"], "Hello");
            if body.get("safety_identifier").is_some() {
                return (StatusCode::BAD_REQUEST, Json(json!({"detail": "Unsupported parameter: safety_identifier"}))).into_response();
            }
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
    let (local, local_task) = start(router(
        codex,
        local_key(KEY)?,
        LogLevel::Off,
        ChatTools::Strict,
    ))
    .await?;

    let models_url = format!("{local}/v1/models");
    let rejected = client.get(&models_url).send().await?;
    assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(catalog_reads.load(Ordering::Relaxed), 0);
    let url = format!("{local}/v1/responses");
    let request = |stream| {
        client.post(&url).bearer_auth(KEY).json(&json!({
            "model": "mock-model", "input": "Hello", "stream": stream,
            "safety_identifier": "test-user"
        }))
    };
    let response: Value = request(false)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(response["output"][0]["content"][0]["text"], "Hi");
    let models: Value = client
        .get(&models_url)
        .bearer_auth(KEY)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(
        models,
        json!({"object": "list", "data": [
            {"id": "mock-model", "object": "model", "created": 0, "owned_by": "openai", "x_codex": capabilities},
            {"id": "mock-json", "object": "model", "created": 0, "owned_by": "openai", "x_codex": {}}
        ]})
    );
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

    let chat_url = format!("{local}/v1/chat/completions");
    let before = generations.load(Ordering::Relaxed);
    for (endpoint, payload, field, allowed) in [
        (
            &url,
            json!({"input": "Hello", "reasoning": {"effort": "xhigh"}}),
            "reasoning.effort",
            "high",
        ),
        (
            &chat_url,
            json!({"messages": [{"role": "user", "content": "Hello"}], "service_tier": "ultrafast", "stream": true}),
            "service_tier",
            "priority",
        ),
        (
            &chat_url,
            json!({"messages": [{"role": "user", "content": "Hello"}], "reasoning_effort": "medium"}),
            "reasoning.effort",
            "low",
        ),
    ] {
        let mut payload = payload;
        payload["model"] = json!("mock-model");
        let response = client
            .post(endpoint)
            .bearer_auth(KEY)
            .json(&payload)
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let error: Value = response.json().await?;
        let message = error["error"]["message"]
            .as_str()
            .context("expected a validation message")?;
        for part in [field, "mock-model", allowed] {
            assert!(message.contains(part), "missing {part} in {message}");
        }
    }
    assert_eq!(generations.load(Ordering::Relaxed), before);

    for payload in [
        json!({"reasoning_effort": "high", "service_tier": "priority"}),
        json!({"reasoning": true}),
        json!({"reasoning": {"summary": "auto"}, "prompt_cache_key": "expect-summary"}),
        json!({"service_tier": "flex"}),
    ] {
        let mut payload = payload;
        payload["model"] = json!("mock-model");
        payload["messages"] = json!([{"role": "user", "content": "Hello"}]);
        client
            .post(&chat_url)
            .bearer_auth(KEY)
            .json(&payload)
            .send()
            .await?
            .error_for_status()?;
    }

    let url = chat_url;
    let rejected = client
        .post(&url)
        .bearer_auth(KEY)
        .json(&json!({
            "model": "mock-model", "messages": [{"role": "user", "content": "Hello"}],
            "tools": [{"type": "mcp"}]
        }))
        .send()
        .await?;
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    let error: Value = rejected.json().await?;
    assert_eq!(error["error"]["code"], "invalid_request");
    assert_eq!(
        error["error"]["message"],
        "only function tools are supported on Chat Completions"
    );

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
    assert_eq!(catalog_reads.load(Ordering::Relaxed), 1);
    local_task.abort();
    upstream_task.abort();
    Ok(())
}

#[test]
fn filters_native_chat_tool_choices_without_relaxing_function_validation() {
    let trace = Trace::default();
    let mut body = json!({
        "tools": [{"type": "mcp"}], "tool_choice": "required", "parallel_tool_calls": true
    });
    filter_chat_tools(&mut body, trace);
    assert_eq!(body, json!({"tools": []}));

    let mut body = json!({
        "tools": [{"type": "mcp"}, {"type": "function", "function": {"name": "lookup"}}],
        "tool_choice": {"type": "mcp"}
    });
    filter_chat_tools(&mut body, trace);
    assert_eq!(
        body,
        json!({
            "tools": [{"type": "function", "function": {"name": "lookup"}}], "tool_choice": "none"
        })
    );

    let mut body = json!({
        "model": "mock-model", "messages": [{"role": "user", "content": "Hello"}],
        "tools": [{"type": "mcp"}, {"type": "function"}, {}]
    });
    filter_chat_tools(&mut body, trace);
    assert_eq!(body["tools"], json!([{"type": "function"}, {}]));
    assert!(chat::into_responses(body).is_err());
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
    let chat_tools = json!([
        {"type": "mcp", "server_label": "mock-only"},
        {"type": "function", "function": {"name": NAME, "parameters": parameters}}
    ]);
    let upstream_tools = json!([{
        "type": "function", "name": NAME, "parameters": parameters, "strict": false
    }]);
    let mock = Router::new().route(
        "/responses",
        post(move |Json(body): Json<Value>| {
            let upstream_tools = upstream_tools.clone();
            async move {
                assert_eq!(body["tools"], upstream_tools);
                assert!(body.get("safety_identifier").is_none());
                assert_eq!(
                    body["reasoning"],
                    json!({"effort": "high", "summary": "auto"})
                );
                assert_eq!(body["input"][0]["role"], "user");
                assert_eq!(body["service_tier"], "ultrafast");
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
                    assert!(call["call_id"].as_str().is_some_and(|id| id.len() <= 64));
                    assert_eq!(call["name"], NAME);
                    assert_eq!(call["arguments"], ARGUMENT_DELTAS.concat());
                    let output = &body["input"][2];
                    assert_eq!(output["type"], "function_call_output");
                    assert_eq!(output["call_id"], call["call_id"]);
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
    let (local, local_task) = start(router(
        codex,
        local_key(KEY)?,
        LogLevel::Debug,
        ChatTools::FunctionsOnly,
    ))
    .await?;
    let fallback = client
        .get(format!("{local}/v1/models"))
        .bearer_auth(KEY)
        .send()
        .await?;
    assert_eq!(fallback.status(), StatusCode::OK);
    let fallback: Value = fallback.json().await?;
    assert_eq!(fallback["data"][0]["id"], "gpt-5.6-sol");
    assert_eq!(fallback["data"][0]["x_codex"], json!({}));
    let url = format!("{local}/v1/chat/completions");
    let user = json!({"role": "user", "content": "What time is it in Paris?"});

    let request = |messages: Value, stream| {
        client.post(&url).bearer_auth(KEY).json(&json!({
            "model": "mock-model", "messages": messages, "tools": chat_tools,
            "stream": stream, "reasoning": {"effort": "high", "summary": "auto"},
            "service_tier": "ultrafast", "user": "test-user"
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

    let replay_id = format!("call_{}", "x".repeat(80));
    let mut replay = message.clone();
    replay["modelProviderMessageId"] = json!("msg_tool_call");
    replay["tool_calls"][0]["id"] = json!(replay_id);
    let follow_up: Value = request(
        json!([user, replay, {
            "role": "tool", "tool_call_id": replay_id, "content": TOOL_OUTPUT, "name": NAME
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
