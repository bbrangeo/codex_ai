# codex_ai

`codex_ai` is a local gateway for using a ChatGPT account with Codex access from an OpenAI-compatible client. Point a script, SDK, or app that supports a custom OpenAI base URL at `http://127.0.0.1:8787/v1`. It can then list your account's Codex models and request responses without a separate OpenAI API key.

**The gateway runs on your machine; the models do not.** Prompts, any conversation history you include, and any tool results you send are forwarded to the ChatGPT Codex backend. You need an internet connection and a ChatGPT account with Codex access. Model availability and usage limits come from that account.

## How it works

1. Run `login` and sign in to ChatGPT in your browser. The gateway caches the resulting OAuth tokens locally and uses them to authenticate with Codex.
2. Run `serve` with a `CODEX_API_KEY` you generate yourself. The server listens only on a loopback address, by default `127.0.0.1:8787`. The local key authenticates your client to the gateway; it is not sent to Codex. Your client does not need access to the OAuth tokens.
3. Your client makes an HTTP request to the gateway. The gateway forwards Responses requests to Codex and translates Chat Completions requests into Responses requests. It sends back either a JSON response or a stream of server-sent events (SSE), depending on what the client requested.

This is not a local model runner, a replacement for the full OpenAI API, or a coding agent that edits files or executes your client's tools. See [compatibility and limits](#compatibility-and-limits) before pointing an existing app at the gateway.

## Getting started with Homebrew (macOS)

This repository also hosts the [Homebrew formula](Formula/codex-ai.rb). Give Homebrew the repository URL when adding the tap:

```sh
brew tap bbrangeo/codex-ai https://github.com/bbrangeo/codex_ai.git
brew trust --formula bbrangeo/codex-ai/codex-ai
brew install bbrangeo/codex-ai/codex-ai
brew test bbrangeo/codex-ai/codex-ai
```

`brew trust` approves only this formula. Skip that line if your Homebrew version does not have the command. `brew test` checks the launcher and its environment file in a temporary directory, without a ChatGPT account.

1. Run `codex-ai-service --help` to create `~/.config/cosex_ai/env` with `0600` permissions.
2. Generate a local key with `openssl rand -hex 32` and set `CODEX_API_KEY` in that file. You can also set `CODEX_LISTEN` (default: `127.0.0.1:8787`) or `CODEX_DEBUG=1`.
3. Stop any existing `codex_ai serve` process, then run `codex-ai-service login`. Open the displayed URL in a browser **on the same machine** and complete sign-in.
4. Run `brew services start bbrangeo/codex-ai/codex-ai` **without `sudo`** to start the service and have it restart at login. To run in the foreground instead, use `codex-ai-service serve`.

See the [Homebrew guide](homebrew/README.md) for switching from the previous GitHub tap, upgrades, and logs.

## Getting started with Cargo (macOS, Linux, and Windows; Rust 1.89+)

```sh
cargo run --locked -- login
```

Open the displayed URL in a browser **on the same machine** and authorize the sign-in. The OAuth callback uses port `1455`, or `1457` if the first port is in use. Then start the server. On macOS and Linux:

```sh
export CODEX_API_KEY="$(openssl rand -hex 32)"
cargo run --locked -- serve
```

On Windows, set the local key in PowerShell instead:

```powershell
$key = New-Object byte[] 32
$rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
$rng.GetBytes($key)
$rng.Dispose()
$env:CODEX_API_KEY = [Convert]::ToBase64String($key)
cargo run --locked -- serve
```

## Calling the API

Configure your OpenAI client with the **base URL `http://127.0.0.1:8787/v1`** and the **local `CODEX_API_KEY`**. This key is neither an OpenAI API key nor an OAuth token.

The `curl` examples require the same key in the shell that runs them. If you installed via Homebrew and trust the contents of your environment file, you can load it in a POSIX shell:

```sh
set -a
. "$HOME/.config/cosex_ai/env"
set +a
```

To list the models available to your account:

```sh
curl -fsS http://127.0.0.1:8787/v1/models \
  -H "Authorization: Bearer $CODEX_API_KEY"
```

To generate text with the Responses API, replace `MODEL_ID` with an `id` returned by `/v1/models`:

```sh
curl -fsS http://127.0.0.1:8787/v1/responses \
  -H "Authorization: Bearer $CODEX_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"MODEL_ID","input":"Say hello."}'
```

If your client uses Chat Completions instead, send the same prompt in a `messages` array:

```sh
curl -fsS http://127.0.0.1:8787/v1/chat/completions \
  -H "Authorization: Bearer $CODEX_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"MODEL_ID","messages":[{"role":"user","content":"Say hello."}]}'
```

Both generation endpoints return JSON by default. To receive incremental events instead, add `"stream": true` to the request and use `curl -N` to see events as they arrive.

## Supported API endpoints

| Endpoint | Behavior |
| --- | --- |
| `GET /v1/models` | Lists models visible in your Codex account's catalog. |
| `POST /v1/responses` | Generates responses as JSON or SSE (`stream: true`). Native Responses items and tools are forwarded to Codex, subject to backend support. |
| `POST /v1/chat/completions` | Returns text, `function` tool calls, and usage as JSON or SSE. For images and other tool types, use Responses. |

## Compatibility and limits

- **The client owns the conversation.** The gateway does not keep conversation state and sends `store: false` to Codex. Include the relevant history in `input` (Responses) or `messages` (Chat Completions) on each turn. Stored responses, `previous_response_id`, server-side conversations, and background mode are not supported.
- **The client runs its own functions.** The gateway returns function calls and translates the results your client sends back; it does not execute those functions. For multi-turn tool workflows, prefer Responses and replay any opaque reasoning items alongside tool calls and results.
- **Compatibility is limited to the endpoints above.** Embeddings are not supported, and not every OpenAI parameter is accepted. Supported options can vary by model. Chat Completions is limited to text and `function` tools; use Responses for features such as image input or other tool types where Codex supports them.
- **The upstream is ChatGPT Codex, not `api.openai.com`.** The backend may change independently of the public OpenAI API.

Leading `system` messages are sent to Codex as one leading `developer` message so their text can be part of the cached prefix. An explicit Responses `instructions` field is forwarded separately.

### Testing a Chat Completions tool call

```sh
curl -fsS http://127.0.0.1:8787/v1/chat/completions \
  -H "Authorization: Bearer $CODEX_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"MODEL_ID","messages":[{"role":"user","content":"What time is it in Paris?"}],"tools":[{"type":"function","function":{"name":"get_local_time","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}}],"tool_choice":{"type":"function","function":{"name":"get_local_time"}}}'
```

Replace `MODEL_ID` with an `id` from `/v1/models`. The client receives `message.tool_calls`, runs the function, and sends the history back with the `assistant` message followed by a `tool` message whose `tool_call_id` matches the call's `id`. The gateway does not run the function. Legacy Chat `function_call` calls and `custom` tools are not supported on this endpoint.

If a remote client uses `stream: true` (like Cursor Agent), the tunnel must support SSE. Quick Tunnels at `trycloudflare.com` do not support SSE.

## Request logs

```sh
codex_ai serve --logs info
codex_ai serve --logs debug
```

`info` logs one line per HTTP response with the method, route, status, and elapsed time. `debug` adds request start, recognized GPT model, tool count, OAuth cache status, and Codex stream diagnostics. Logs go to `stderr`, and events for the same request share a `request` ID. In SSE mode, `http.response` marks when headers are sent, not when generation finishes. In `debug` mode, look for `codex.stream.end` or `codex.stream.error` afterward.

Request logging is off unless you pass `--logs` or set `CODEX_DEBUG=1`. An explicit `--logs` overrides that variable. If you use `brew services`, add `CODEX_DEBUG=1` to the [service environment file](homebrew/README.md) and restart the service. The foreground launcher also accepts `codex-ai-service serve --logs info` or `--logs debug`.

No telemetry is sent externally. Request logs do not include full URLs, headers, keys, prompts, responses, or tool arguments.

## Cache and security

- On macOS and Linux, access and refresh tokens are stored unencrypted in `tokens.json` (`0600`) inside a private cache directory (`0700`). The directory is chosen in this order: `CODEX_CACHE_DIR` (an absolute path), `$XDG_CACHE_HOME/codex_ai`, then `~/.cache/codex_ai`. Do not put the cache directory in a repository or shared folder.
- On Windows, the unencrypted `tokens.json` file is stored in `%LOCALAPPDATA%\codex_ai` and relies on the inherited permissions of the local user profile. `CODEX_CACHE_DIR` is not supported on Windows. Do not grant other users access to the cache.
- The server reuses the access token, refreshes it as it nears expiry or after a `401` from Codex, and replaces the cache atomically. Only one process can use the cache at a time. If the session is revoked, stop the server and run `login` again.
- `CODEX_LISTEN` lets you change the port or loopback address (for example, `127.0.0.1:8080`). Do not expose the server to the network: anyone with the local key can use your account's quota.
- To clear the local cache, stop the server and run `codex-ai-service logout` (Homebrew) or `cargo run --locked -- logout` (Cargo). This does not revoke the session with OpenAI.

`cargo test --locked` checks the cache and HTTP formats against a mock backend. No ChatGPT account is required.

## Publishing a release

Start the workflow from **Actions → Build macOS and Windows → Run workflow**. Choose `build` (the default) to compile and test both platforms without publishing, or `release` to also publish after the tests pass. For a release, update the version in `Cargo.toml`, refresh `Cargo.lock` with `cargo update --workspace`, and push the changes to `master` first.

```sh
gh workflow run build.yml --ref master -f mode=build
gh workflow run build.yml --ref master -f mode=release
```

Only `release` runs on the default branch can publish. If the matching `v<version>` tag does not exist, the workflow tags the tested commit and publishes the macOS Apple Silicon and Windows x86-64 archives to [GitHub Releases](https://github.com/bbrangeo/codex_ai/releases). It then publishes the crate to crates.io if the tag points to that tested commit and the crate version is not already published. Existing tags and releases are left unchanged.

To enable crates.io publishing for the first time:

1. Use a new version rather than moving `v0.1.0`, which predates the publish metadata and the [MIT license](LICENSE).
2. After the new version passes CI and is tagged, publish from that tagged commit with `cargo publish --locked --registry crates-io`, using a crates.io API token locally. Until this first upload, CI skips crates.io publication with a warning.
3. In the crate's Settings → Trusted Publishing, add GitHub owner `bbrangeo`, repository `codex_ai`, and workflow filename `build.yml`. Leave the environment field blank. Future versions use a short-lived token; no GitHub secret is needed.
