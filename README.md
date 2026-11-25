# codex_ai

**Use ChatGPT Codex from an OpenAI-compatible client.**

Point your script, SDK, or app at `http://127.0.0.1:8787/v1` to list your
account's Codex models and generate responses without a separate OpenAI API key.

[Homebrew](#getting-started-with-homebrew-macos) · [Cargo](#getting-started-with-cargo-macos-linux-and-windows-rust-189) · [API examples](#calling-the-api) · [GitHub Releases](https://github.com/bbrangeo/codex_ai/releases) · [MIT License](LICENSE)

![Request path: your app calls codex_ai on your machine, which forwards requests to the remote ChatGPT Codex backend.](.github/codex-ai-flow.svg)

> [!IMPORTANT]
> **The gateway runs locally; the models do not.** Prompts, conversation history,
> and tool results you send are forwarded to ChatGPT Codex. You need an internet
> connection and a ChatGPT account with Codex access. Model availability and
> usage limits come from that account.

## Getting started with Homebrew (macOS)

**1. Install with Homebrew.** This repository hosts the
[formula](Formula/codex-ai.rb). Give Homebrew the repository URL when adding the tap:

```sh
brew tap bbrangeo/codex-ai https://github.com/bbrangeo/codex_ai.git
brew trust --formula bbrangeo/codex-ai/codex-ai
brew install bbrangeo/codex-ai/codex-ai
brew test bbrangeo/codex-ai/codex-ai
```

`brew trust` approves only this formula. Skip that line if your Homebrew version does not have the command. `brew test` checks the launcher and its environment file in a temporary directory, without a ChatGPT account.

**2. Configure and sign in.** Run `codex-ai-service --help` to create
`~/.config/codex_ai/env` with `0600` permissions. Generate a local key with
`openssl rand -hex 32` and set `CODEX_API_KEY` in that file. You can also set
`CODEX_LISTEN` (default: `127.0.0.1:8787`) or `CODEX_DEBUG=1`.
New files include `CODEX_CHAT_FUNCTION_TOOLS_ONLY=1` by default for
[Chat Completions compatibility with Cursor](#troubleshooting-cursor-through-cloudflare-tunnel).
Set it to `0` if you do not want the gateway to filter tools.

**Set the key in this file, not just in your terminal.** On macOS, `brew services`
starts the gateway through `launchd`, so it does not inherit an
`export CODEX_API_KEY=...` from your current shell.

Sign in:

```sh
codex-ai-service login
```

`login` opens your browser automatically and always prints the sign-in URL.
For a headless or remote machine, see [Remote sign-in over SSH](#remote-sign-in-over-ssh).

**3. Start the gateway** without `sudo`:

```sh
brew services start bbrangeo/codex-ai/codex-ai
```

Homebrew restarts the service at login. To run in the foreground **instead**, use
`codex-ai-service serve`.

Already installed? See [Homebrew maintenance](#homebrew-maintenance) for service
commands, upgrades, and switching taps.

## Getting started with Cargo (macOS, Linux, and Windows; Rust 1.89+)

From a checkout of this repository, sign in:

```sh
cargo run --locked -- login
```

Complete sign-in in the browser, then start the server. On macOS and Linux:

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

## Sign-in and session management

### Remote sign-in over SSH

Use `login --no-browser` to skip opening a browser. The URL is still printed.
From the machine where your browser runs, connect to the remote host with local
port forwarding:

```sh
ssh -L 1455:localhost:1455 user@remote-host
```

In that SSH session, run `codex_ai login --no-browser` (or
`codex-ai-service login --no-browser` for Homebrew), then open the printed URL
in your local browser. Keep the SSH session open until sign-in completes so the
browser's loopback callback reaches the remote process.

The OAuth callback uses port `1455`, falling back to `1457` if `1455` is busy
on the remote host. If the sign-in URL specifies callback port `1457`, forward
it too (add `-L 1457:localhost:1457` to the SSH command), or free port `1455`
and retry sign-in. The local ports you forward must also be free.

### Reauthenticate without stopping the server

Run `login` again using the same cache and credential store. If the running
server detects a revoked session or requires sign-in, it reloads the saved
credentials. On Unix, run `kill -HUP <serve-pid>` after sign-in to have the
running server adopt the new credentials immediately. `SIGHUP` reloads only
credentials, not configuration, and does not stop the server.

Only one `serve` process can run per cache. `SIGTERM` and Ctrl-C initiate a
graceful shutdown, allowing in-flight requests and streams to finish. To clear
local credentials, **stop the server first** and run `codex-ai-service logout`
(Homebrew) or `cargo run --locked -- logout` (Cargo) with the same credential
store settings. `logout` does not revoke the session with OpenAI.

### Check the current session

Run `codex_ai status` or `codex_ai whoami` (with Homebrew, use
`codex-ai-service status` or `codex-ai-service whoami` so the commands use the
service's settings). Both are CLI commands that print JSON with the email,
`account_id`, expiration as a Unix timestamp (seconds), and port. Missing or
unknown values are `null`; tokens are never printed. The account fields come
from the saved credentials without contacting Codex. The port is the active
port if a running server uses the same cache, or the configured port otherwise.

## Calling the API

| Client setting | Value |
| --- | --- |
| Base URL | `http://127.0.0.1:8787/v1` |
| API key | Your local `CODEX_API_KEY` (not an OpenAI API key or an OAuth token) |

The `curl` examples need that same key in the shell that runs them.

<details>
<summary>Load the key from your Homebrew environment file</summary>

If you trust the contents of the file, load it in a POSIX shell:

```sh
set -a
. "$HOME/.config/codex_ai/env"
set +a
```

</details>

### List available models

```sh
curl -fsS http://127.0.0.1:8787/v1/models \
  -H "Authorization: Bearer $CODEX_API_KEY"
```

Each model keeps the standard OpenAI fields and adds an `x_codex` object with
catalog metadata when available: `context_window` (tokens),
`supported_reasoning_levels` (objects with `effort`), `default_reasoning_level`,
`input_modalities`, and `service_tiers` (objects with `id`).

### Generate a response

Replace `MODEL_ID` with an `id` returned by `/v1/models`.

```sh
curl -fsS http://127.0.0.1:8787/v1/responses \
  -H "Authorization: Bearer $CODEX_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"MODEL_ID","input":"Say hello."}'
```

### Use Chat Completions instead

Send the same prompt in a `messages` array:

```sh
curl -fsS http://127.0.0.1:8787/v1/chat/completions \
  -H "Authorization: Bearer $CODEX_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"MODEL_ID","messages":[{"role":"user","content":"Say hello."}]}'
```

> [!TIP]
> Both generation endpoints return JSON by default. To stream events, add
> `"stream": true` to the request and use `curl -N`.

## Supported API endpoints

| Endpoint | Behavior |
| --- | --- |
| `GET /healthz` | Returns `200 OK` with no body. Requires no API key and makes no upstream call. This checks liveness, not readiness. |
| `GET /v1/models` | Lists models visible in your Codex account's catalog, or a built-in fallback list if no catalog could be loaded. |
| `GET /v1/models/{id}` | Requires the gateway API key. Returns one model from the same catalog or fallback as `GET /v1/models`, or `404` if the ID is not found. |
| `POST /v1/responses` | Generates responses as JSON or SSE (`stream: true`). Native Responses items and tools are forwarded to Codex, subject to backend support. |
| `POST /v1/responses/compact` | Compacts the supplied history with Codex. Returns JSON only. |
| `POST /v1/chat/completions` | Returns text, `function` tool calls, and usage as JSON or SSE. For images and other tool types, use Responses. |

To compact a conversation, send `model` and the current history as `input` to
`/v1/responses/compact`. Use the returned `output` as the new history for
`/v1/responses`, then append new items. Keep any opaque `compaction` items intact.
The gateway does not compact automatically or support `previous_response_id`.

## How it works

1. **Sign in.** `login` opens your browser by default and always prints the
   sign-in URL. The gateway stores the OAuth tokens locally and uses them to
   authenticate with Codex.
2. **Connect to localhost.** `serve` listens only on a loopback address. Your
   `CODEX_API_KEY` authenticates your client to the gateway; that key is not sent
   to Codex, and your client does not need access to the OAuth tokens.
3. **Send a request.** The gateway forwards Responses requests to Codex and
   translates Chat Completions into Responses requests. It returns JSON or a
   stream of server-sent events (SSE), depending on what the client requested.

## Compatibility and limits

This is not a local model runner, a replacement for the full OpenAI API, or a
coding agent that edits files or executes your client's tools.

Set `CODEX_MAX_CONCURRENCY` to a positive integer to limit simultaneous Codex
requests per gateway process (default: `4`). Extra requests wait for a slot;
streaming responses keep their slot until the stream ends or the client
disconnects. The model catalog shares this limit.

Before responding to the client, the gateway retries upstream HTTP `429` and
`5xx` at most twice, after 250 ms and 500 ms or a longer `Retry-After` delay.
If `Retry-After` calls for more than 10 seconds of waiting, the gateway returns
the error with that header instead. It never restarts an SSE stream.

- **The client owns the conversation.** The gateway does not keep conversation state and sends `store: false` to Codex. Include the relevant history in `input` (Responses) or `messages` (Chat Completions) on each turn. Stored responses, `previous_response_id`, server-side conversations, and background mode are not supported.
- **The client runs its own functions.** The gateway returns function calls and translates the results your client sends back; it does not execute those functions. For multi-turn tool workflows, prefer Responses and replay any opaque reasoning items alongside tool calls and results.
- **Compatibility is limited to the endpoints above.** Embeddings are not supported, and not every OpenAI parameter is accepted. Supported options can vary by model. Chat Completions is limited to text and `function` tools; use Responses for features such as image input or other tool types where Codex supports them.
- **The upstream is ChatGPT Codex, not `api.openai.com`.** The backend may change independently of the public OpenAI API. It does not accept `safety_identifier`, so the gateway omits it, including when derived from Chat Completions `user`.

Leading `system` messages are sent to Codex as one leading `developer` message so their text can be part of the cached prefix. An explicit Responses `instructions` field is forwarded separately. Tool call IDs over Codex's 64-character limit are shortened consistently for calls and their matching results before forwarding.

For Chat Completions, the gateway fills in a missing or empty `prompt_cache_key`
from the model and the first normalized `developer` and `user` messages. Later
turns reuse the key as long as those inputs stay the same. The gateway forwards
it as both `session-id` and `thread-id`. Set an explicit key if your client
trims the beginning of the conversation or needs to distinguish identical starts.

Both generation endpoints return `400` for a `reasoning.effort` or `service_tier`
that the model catalog does not support. Use `priority` for Fast and `ultrafast`
for Ultrafast. If no effort is specified, the gateway uses the model's catalog
default when available. The catalog is shared with `/v1/models` and cached for
five minutes. The first request after expiry triggers a single background
refresh and uses the last successful catalog, even if the refresh fails.
The initial load waits up to five seconds while the background fetch can take
up to one minute. Until a catalog is loaded, `/v1/models` can return a built-in
list without capability metadata or a guarantee of account access. If no
catalog is available, a model is absent, or the relevant metadata is missing,
the gateway leaves that model-specific validation to Codex.

Chat Completions also accepts `reasoning` as an effort string (`"high"`). An
explicit effort in `reasoning` takes precedence over `reasoning_effort`.
Boolean `true` requests the catalog default, or `medium` if there is no default
and `medium` is not known to be unsupported. Boolean `false` supplies no effort
override; it does not disable reasoning.

### Testing a Chat Completions tool call

<details>
<summary>Show the curl example</summary>

```sh
curl -fsS http://127.0.0.1:8787/v1/chat/completions \
  -H "Authorization: Bearer $CODEX_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "MODEL_ID",
    "messages": [{"role":"user","content":"What time is it in Paris?"}],
    "tools": [{
      "type": "function",
      "function": {
        "name": "get_local_time",
        "parameters": {
          "type": "object",
          "properties": {"city":{"type":"string"}},
          "required": ["city"]
        }
      }
    }],
    "tool_choice": {"type":"function","function":{"name":"get_local_time"}}
  }'
```

</details>

Replace `MODEL_ID` with an `id` from `/v1/models`. The client receives `message.tool_calls`, runs the function, and sends the history back with the `assistant` message followed by a `tool` message whose `tool_call_id` matches the call's `id`. The gateway does not run the function. Legacy Chat `function_call` calls and `custom` tools are not supported on this endpoint.

### Troubleshooting Cursor through Cloudflare Tunnel

- Set Cursor's override base URL to `https://<your-hostname>/v1` and use your
  `CODEX_API_KEY` as the API key. Keep the gateway bound to loopback and use a
  strong, dedicated key if you expose it through a tunnel.
- With the default gateway listener, set the hostname's ingress `service` to
  `http://127.0.0.1:8787` (without `/v1`). `localhost` can resolve to `::1`,
  where the default IPv4 listener is not reachable. Run
  `cloudflared tunnel ingress validate` and restart the tunnel after editing
  its configuration. A request to `/v1/models` without a key should return
  `401` with `invalid_api_key` if it reached the gateway.
- A `400` with `messages[N]: unsupported message field` comes from the gateway's
  Chat Completions parser, not the tunnel. `N` is the zero-based message index;
  the error names the field, not its value. Extra message fields set to `null`
  are ignored. Chat participant names (`name`) and Cursor's
  `modelProviderMessageId` are accepted as strings but not forwarded because
  Responses messages have no corresponding fields.
- If Cursor sends non-function tools and the gateway rejects them, check for
  `CODEX_CHAT_FUNCTION_TOOLS_ONLY=1`. New Homebrew configurations include it by
  default. For an existing configuration, add it to `~/.config/codex_ai/env`
  and [restart the service](#manage-the-homebrew-service). If you launch the
  binary directly, export the variable before starting it. The gateway drops
  non-function tools from Chat Completions requests and keeps function tools.
  If no tools remain, generation continues without tools. Requests to
  `/v1/responses` are unaffected. The gateway cannot change what Cursor enables
  or fix a request that Cursor blocks before sending it.
- If a remote client uses `stream: true` (like Cursor Agent), the tunnel must
  support SSE. Quick Tunnels at `trycloudflare.com` do not support SSE.

## Request logs

```sh
# Homebrew (foreground, with the service environment loaded)
codex-ai-service serve --logs info
codex-ai-service serve --logs debug

# Cargo (installed binary)
codex_ai serve --logs info
codex_ai serve --logs debug
```

If you export `CODEX_API_KEY` yourself, you can run the Homebrew binary directly with `codex-ai serve --logs info` (or `debug`). From a Cargo checkout, use `cargo run --locked -- serve --logs info` (or `debug`).

`info` logs one line per HTTP response with the method, route, status, and elapsed time. `debug` adds request start, recognized GPT model, tool count, OAuth cache status, and Codex stream diagnostics. For a Chat Completions request rejected before forwarding, look for `chat.validation_rejected` for the validation reason. Logs go to `stderr`, and events for the same request share a `request` ID. In SSE mode, `http.response` marks when headers are sent, not when generation finishes. In `debug` mode, look for `codex.stream.end` or `codex.stream.error` afterward.

Request logging is off unless you pass `--logs` or set `CODEX_DEBUG=1`. An explicit `--logs` overrides that variable. If you use `brew services`, add `CODEX_DEBUG=1` to the [service environment file](#manage-the-homebrew-service) and restart the service.

No telemetry is sent externally. Request logs do not include full URLs, headers, keys, prompts, responses, or tool arguments.

## Cache and security

- On macOS and Linux, access and refresh tokens are stored unencrypted in `tokens.json` (`0600`) inside a private cache directory (`0700`). The directory is chosen in this order: `CODEX_CACHE_DIR` (an absolute path), `$XDG_CACHE_HOME/codex_ai`, then `~/.cache/codex_ai`. Do not put the cache directory in a repository or shared folder.
- On Windows, the unencrypted `tokens.json` file is stored in `%LOCALAPPDATA%\codex_ai` and relies on the inherited permissions of the local user profile. `CODEX_CACHE_DIR` is not supported on Windows. Do not grant other users access to the cache.
- The server reuses the access token and refreshes it as it nears expiry or after a `401` from Codex. With file storage, it replaces the token file atomically.
- `CODEX_LISTEN` lets you change the port or loopback address (for example, `127.0.0.1:8080`). Do not expose the server to the network: anyone with the local key can use your account's quota.

### Optional OS credential storage

File storage is the default. To use the OS credential store, build with
`cargo build --locked --features keyring` (or use
`cargo run --locked --features keyring -- <command>` from a checkout) and set
`CODEX_CREDENTIAL_STORE=keyring` consistently for `login`, `serve`, `status`,
`whoami`, and `logout`. With a Homebrew build that includes this feature, set
it in `~/.config/codex_ai/env` so the launcher and service use the same store.

- **macOS:** Uses Keychain.
- **Windows:** Uses Credential Manager, which limits a credential blob to 2,560 bytes.
- **Linux:** Uses Secret Service and needs access to an available, unlocked store in the user's session.

There is no automatic fallback to file storage, and enabling `keyring` does
not import or remove an existing `tokens.json`. If the OS store is unavailable
or cannot save the credentials, explicitly choose file storage by unsetting
`CODEX_CREDENTIAL_STORE` for all commands. If you switch manually, sign in
with the new store selected and keep any existing token file private or delete
it once you have verified that the new store works.

## Code layout

The Cargo package builds two crates. The library (`src/lib.rs`) translates Chat
Completions and normalizes Responses requests without making network calls. The
binary (`src/main.rs`) handles the CLI, HTTP, and OAuth. Its SSE transport lives in
`src/api/stream.rs`. Both are packaged and released together as `codex_ai`.

`cargo test --locked` runs the protocol, cache, and HTTP tests. The HTTP tests use a mock backend, so no ChatGPT account is required.

## Homebrew maintenance

### Manage the Homebrew service

```sh
brew services info bbrangeo/codex-ai/codex-ai --json
brew services restart bbrangeo/codex-ai/codex-ai
brew services stop bbrangeo/codex-ai/codex-ai
```

The launcher reads `~/.config/codex_ai/env` at every start without overwriting it.
Use one assignment per line (`NAME=value` or `NAME='value with spaces'`). The
file is sourced as shell code: keep it private and only put trusted content in it.
You can run `codex-ai-service login` again while the service is running. See
[session management](#sign-in-and-session-management) for credential reloads
and check the local session with `codex-ai-service status` or
`codex-ai-service whoami`.

To watch the service log:

```sh
tail -f "$(brew --prefix)/var/log/codex-ai.log"
```

If the log says `CODEX_API_KEY is required`, check that `~/.config/codex_ai/env`
contains an uncommented `CODEX_API_KEY=...` assignment with your generated key,
then restart the service with the command above. If you shared the old key,
generate a new one and update your clients. The log retains old errors; after
restarting, look for a new `OpenAI API: http://127.0.0.1:8787/v1` line (or your
configured address).

To run in the foreground, stop the service first and run `codex-ai-service serve`.

### Upgrade with Homebrew

```sh
brew update
brew upgrade bbrangeo/codex-ai/codex-ai
brew test bbrangeo/codex-ai/codex-ai
```

Restart the service after upgrading if it was running. You do not need to sign in
again while your existing OAuth session is valid.

### Switch from the previous GitHub tap

If you previously added the tap from `bbrangeo/homebrew-codex-ai`, point it at
this repository instead. Save any local changes to the tap first:
`brew update-reset` resets that checkout. You do not need to uninstall `codex-ai`.

```sh
brew tap --custom-remote bbrangeo/codex-ai https://github.com/bbrangeo/codex_ai.git
brew update-reset "$(brew --repository bbrangeo/codex-ai)"
brew trust --formula bbrangeo/codex-ai/codex-ai
brew test bbrangeo/codex-ai/codex-ai
```

If you installed from the old `local/codex-ai` tap instead, stop that service if
it is running, then uninstall it before following the
[Homebrew installation steps](#getting-started-with-homebrew-macos):

```sh
HOMEBREW_NO_AUTOREMOVE=1 brew uninstall local/codex-ai/codex-ai
```

This preserves your configuration and avoids removing unrelated dependencies.

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

### Update the Homebrew formula

After publishing a GitHub release, update the URL in [`Formula/codex-ai.rb`](Formula/codex-ai.rb) to the new tag's source archive (`archive/refs/tags/v<VERSION>.tar.gz`). Download the archive and update `sha256` with the output of `shasum -a 256`. Push the updated formula to `master`, then [test the new version with Homebrew](#upgrade-with-homebrew).
