# Installing with Homebrew

The `codex-ai` formula builds the Rust project from a local archive. Run the following from the repository root:

```sh
tap_dir="$(brew --repository local/codex-ai)"
mkdir -p "$tap_dir/Formula"
cp homebrew/Formula/codex-ai.rb "$tap_dir/Formula/"
tar -czf "$tap_dir/codex-ai-0.1.0.tar.gz" Cargo.toml Cargo.lock src
brew trust --formula local/codex-ai/codex-ai
brew install local/codex-ai/codex-ai
```

`brew trust` is needed on Homebrew versions that require explicit approval for third-party formulas. To rebuild after local changes, copy the formula again, recreate the archive, then run `brew fetch --force --build-from-source local/codex-ai/codex-ai` and `brew reinstall local/codex-ai/codex-ai`.

## Configuring and running the service

1. Run `codex-ai-service --help` to create `~/.config/cosex_ai/env` if it does not exist (`0600` permissions).
2. Generate a local key with `openssl rand -hex 32`, then set `CODEX_API_KEY` in that file. This is not an OpenAI API key. `CODEX_LISTEN` is optional: the server listens on `127.0.0.1:8787` by default.
3. Run `codex-ai-service login`, then open the displayed URL to sign in to your ChatGPT account. If you need to sign in again later, stop the service first: it holds the login cache lock.
4. Start the service with `brew services start codex-ai`, without `sudo`.

By default, the API is available at `http://127.0.0.1:8787/v1`. In your client, use `CODEX_API_KEY` as the bearer token. Logs are written to `$(brew --prefix)/var/log/codex-ai.log`.

The launcher reads the `env` file each time it starts without overwriting it. Use one assignment per line (`NAME=value` or `NAME='value with spaces'`). The file is sourced as shell code: only put trusted content in it and keep it out of version control. After editing it, run `brew services restart codex-ai`. To stop the service, run `brew services stop codex-ai`. To run the server in the foreground, use `codex-ai-service serve`.

To publish the formula, replace its `file://` URL with the URL of a release archive and add the archive's `sha256`. Then place the formula in the `Formula/` directory of a published Homebrew tap.
