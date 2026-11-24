# Using Homebrew

For first-time installation, configuration, and sign-in, see [Getting started with Homebrew](../README.md#getting-started-with-homebrew-macos).

## Switch from the previous GitHub tap

If you previously added the tap from `bbrangeo/homebrew-codex-ai`, point it at this repository instead. Save any local changes to the tap first: `brew update-reset` resets that checkout. No need to uninstall `codex-ai`.

```sh
brew tap --custom-remote bbrangeo/codex-ai https://github.com/bbrangeo/codex_ai.git
brew update-reset "$(brew --repository bbrangeo/codex-ai)"
brew trust --formula bbrangeo/codex-ai/codex-ai
brew test bbrangeo/codex-ai/codex-ai
```

## Upgrade

```sh
brew update
brew upgrade bbrangeo/codex-ai/codex-ai
brew test bbrangeo/codex-ai/codex-ai
```

Restart the service after upgrading if it was running. You do not need to sign in again while your existing OAuth session is valid.

If you installed from the old `local/codex-ai` tap, stop that service if it is running and run `HOMEBREW_NO_AUTOREMOVE=1 brew uninstall local/codex-ai/codex-ai` before following the public tap installation instructions. This preserves your configuration and avoids removing unrelated dependencies.

## Manage the service

```sh
brew services info bbrangeo/codex-ai/codex-ai --json
brew services restart bbrangeo/codex-ai/codex-ai
brew services stop bbrangeo/codex-ai/codex-ai
```

The launcher reads `~/.config/cosex_ai/env` at every start without overwriting it. Use one assignment per line (`NAME=value` or `NAME='value with spaces'`). The file is sourced as shell code: keep it private and only put trusted content in it. Stop the service before running `codex-ai-service login` again: the service holds the login cache lock.

For diagnostic logs, set `CODEX_DEBUG=1` in the environment file and restart the service. Debug logs do not include keys or prompts:

```sh
tail -f "$(brew --prefix)/var/log/codex-ai.log"
```

To run in the foreground, stop the service first and run `codex-ai-service serve`.

## Publish the formula

After publishing a [GitHub release](../README.md#publishing-a-release), update the URL in [`Formula/codex-ai.rb`](../Formula/codex-ai.rb) to the new tag's source archive (`archive/refs/tags/v<VERSION>.tar.gz`). Download the archive and update `sha256` with the output of `shasum -a 256`. Push the updated formula to `master`, then verify the new version with Homebrew.
