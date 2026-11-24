# codex_ai

A local Rust gateway that lets OpenAI clients use a ChatGPT account with Codex access. It draws on Zed's [`openai_subscribed.rs`](https://github.com/zed-industries/zed/blob/7279c76ef71d45504d7cfea00ee054f852ca0fbf/crates/openai_subscribed/src/openai_subscribed.rs), [`responses.rs`](https://github.com/zed-industries/zed/blob/7279c76ef71d45504d7cfea00ee054f852ca0fbf/crates/open_ai/src/responses.rs), and [`completion.rs`](https://github.com/zed-industries/zed/blob/7279c76ef71d45504d7cfea00ee054f852ca0fbf/crates/open_ai/src/completion.rs).

To install and manage the service with `brew services`, see the [Homebrew guide](homebrew/README.md).

## Getting started (macOS and Linux, Rust 1.89+)

```sh
cargo run --locked -- login
```

Open the displayed URL in a browser **on the same machine** and authorize the sign-in. The OAuth callback uses port `1455`, or `1457` if the first port is in use. Then run:

```sh
export CODEX_API_KEY="$(openssl rand -hex 32)"
cargo run --locked -- serve
```

Configure your OpenAI client with the **base URL `http://127.0.0.1:8787/v1`** and the **local `CODEX_API_KEY`**. This key is neither an OpenAI API key nor an OAuth token. To list the models available to your account, run this from a shell with the same key:

```sh
curl -fsS http://127.0.0.1:8787/v1/models \
  -H "Authorization: Bearer $CODEX_API_KEY"
```

To generate text, replace `MODEL_ID` with an `id` returned by `/v1/models`:

```sh
curl -fsS http://127.0.0.1:8787/v1/responses \
  -H "Authorization: Bearer $CODEX_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"MODEL_ID","input":"Say hello."}'
```

## Supported API endpoints

| Endpoint | Behavior |
| --- | --- |
| `GET /v1/models` | Lists models visible in your Codex account's catalog. |
| `POST /v1/responses` | Generates responses as JSON or SSE (`stream: true`). Native Responses items and tools are forwarded to Codex, subject to backend support. |
| `POST /v1/chat/completions` | Returns text, `function` tool calls, and usage as JSON or SSE. For images and other tool types, use Responses. |

The gateway does not retain conversation state: it sends `store: false`. Include the relevant history in `input` or `messages` on each turn. The gateway does not support `previous_response_id`, `background` mode, or embeddings, and it does not implement the full public OpenAI API. Available models, parameters, and quotas depend on your Codex access. The ChatGPT backend used here is not the public API at `api.openai.com` and may change.

### Testing a Chat Completions tool call

```sh
curl -fsS http://127.0.0.1:8787/v1/chat/completions \
  -H "Authorization: Bearer $CODEX_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"MODEL_ID","messages":[{"role":"user","content":"What time is it in Paris?"}],"tools":[{"type":"function","function":{"name":"get_local_time","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}}],"tool_choice":{"type":"function","function":{"name":"get_local_time"}}}'
```

Replace `MODEL_ID` with an `id` from `/v1/models`. The client receives `message.tool_calls`, runs the function, and sends the history back with the `assistant` message followed by a `tool` message whose `tool_call_id` matches the call's `id`. The gateway does not run the function. Legacy Chat `function_call` calls and `custom` tools are not supported on this endpoint.

If a remote client uses `stream: true` (like Cursor Agent), the tunnel must support SSE. Quick Tunnels at `trycloudflare.com` do not support SSE.

## Debug mode

Start the server with `CODEX_DEBUG=1` to trace requests in the terminal:

```sh
CODEX_DEBUG=1 ./target/debug/codex_ai serve
```

Diagnostic events are written to `stderr`. Events for the same request share a `request` ID, and `elapsed_ms` measures time since the request started. They report the route, HTTP status, recognized GPT model, tool count, OAuth cache status, and whether the Codex stream finished or failed. In SSE mode, `http.response` marks when headers are sent, not when generation finishes. Look for `codex.stream.end` or `codex.stream.error` afterward.

No telemetry is sent externally. Debug mode does not log full URLs, headers, keys, prompts, responses, or tool arguments. Remove `CODEX_DEBUG=1` and restart the server to disable it. If you use `brew services`, add `CODEX_DEBUG=1` to the [service environment file](homebrew/README.md) and restart the service. Diagnostic output will appear in the usual service log.

## Cache and security

- Access and refresh tokens are stored in an unencrypted local `tokens.json` file (`0600` permissions, in a private directory with `0700` permissions). The cache location is chosen in this order: `CODEX_CACHE_DIR` (an absolute path), `$XDG_CACHE_HOME/codex_ai`, then `~/.cache/codex_ai`. Do not put the cache directory in a repository or shared folder.
- The server reuses the access token, refreshes it as it nears expiry or after a `401` from Codex, and replaces the cache atomically. Only one process can use the cache at a time. If the session is revoked, stop the server and run `login` again.
- `CODEX_LISTEN` lets you change the port or loopback address (for example, `127.0.0.1:8080`). Do not expose the server to the network: anyone with the local key can use your account's quota.
- To clear the local cache, stop the server and run `cargo run --locked -- logout`. This does not revoke the session with OpenAI.

`cargo test --locked` checks the cache and HTTP formats against a mock backend. No ChatGPT account is required.
