# Using Homebrew

For first-time installation, configuration, and sign-in, see [Getting started with Homebrew](../README.md#getting-started-with-homebrew-macos).

## Reinstall after changing the source

If the service is running, stop it before reinstalling. From the repository root:

```sh
tap_dir="$(brew --repository local/codex-ai)"
cp homebrew/Formula/codex-ai.rb "$tap_dir/Formula/"
tar -czf "$tap_dir/codex-ai-0.1.0.tar.gz" Cargo.toml Cargo.lock src
brew fetch --force --build-from-source local/codex-ai/codex-ai
brew reinstall --build-from-source local/codex-ai/codex-ai
brew test local/codex-ai/codex-ai
```

`brew fetch --force` refreshes the cached archive at the same URL. The `No checksum was provided` warning is expected for this local build. Start the service again if you had it running. You do not need to sign in again while your existing OAuth session is valid.

## Manage the service

```sh
brew services info local/codex-ai/codex-ai --json
brew services restart local/codex-ai/codex-ai
brew services stop local/codex-ai/codex-ai
```

The launcher reads `~/.config/cosex_ai/env` at every start without overwriting it. Use one assignment per line (`NAME=value` or `NAME='value with spaces'`). The file is sourced as shell code: keep it private and only put trusted content in it. Stop the service before running `codex-ai-service login` again: the service holds the login cache lock.

For diagnostic logs, set `CODEX_DEBUG=1` in the environment file and restart the service. Debug logs do not include keys or prompts:

```sh
tail -f "$(brew --prefix)/var/log/codex-ai.log"
```

To run in the foreground, stop the service first and run `codex-ai-service serve`.

## Publish the formula

Replace the `file://` URL with the URL of a release archive and add its `sha256`. Then place the formula in the `Formula/` directory of a published Homebrew tap.
