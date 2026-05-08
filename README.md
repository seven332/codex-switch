# codex-switch

Local CLI account switcher for Codex.

## Install

```sh
cargo install --path .
```

## Commands

```sh
codex-switch list
codex-switch login <name> [--device-auth]
codex-switch import <name> [--file <path>]
codex-switch switch <name-or-id>
codex-switch auto-switch [--threshold <percent>]
codex-switch run [--threshold <percent>] [--codex-bin <path>] -- [CODEX_ARGS]...
codex-switch usage [name-or-id]
codex-switch usage --all
codex-switch delete <name-or-id>
codex-switch rename <name-or-id> <new-name>
```

## Storage

Accounts are stored in `~/.codex-switch/accounts.json`.

Switching writes Codex auth data to:

1. `$CODEX_HOME/auth.json`, when `CODEX_HOME` is set
2. `~/.codex/auth.json`, otherwise

On Unix, both files are written with `0600` permissions.

## Switching

`switch` refuses to write Codex auth data while active Codex processes are detected. Close Codex before switching accounts.

`auto-switch` also refuses to switch while Codex is running. When Codex is not running, it checks the active ChatGPT OAuth account. If the account is out of credits, rate-limited, usage-limited, or at the configured usage threshold, it switches to the first stored ChatGPT OAuth account that is still usable. API key accounts are not usage-checkable and are skipped as replacement candidates.

`run` is the runtime auto-switching entrypoint. It checks the active account before startup, starts `codex app-server`, launches Codex as a remote TUI through a local websocket proxy, and passes arguments after `--` to `codex`. During the managed session, the proxy watches Codex app-server `account/rateLimits/updated` notifications and usage-limit errors. When the active account reaches the usage threshold or Codex reports `usageLimitExceeded`, `codex-switch` switches to the first usable ChatGPT OAuth account and sends the new auth tokens to the running Codex app-server.

```sh
codex-switch run
codex-switch run --threshold 95
codex-switch run -- resume
codex-switch run -- resume --last
codex-switch run -- resume <session-id>
```

`run` supports Codex interactive commands that accept `--remote`: the default TUI, `resume`, and `fork`. ChatGPT OAuth accounts are required for runtime switching. API key accounts are not usage-checkable and cannot be applied to the running app-server. The switch applies through Codex's app-server auth API; an in-flight request may continue with the auth it already started with, so the request that first reports a usage-limit error may still fail before the next turn uses the replacement account.

## Login

`login` uses ChatGPT/OpenAI browser OAuth by default. It starts a local callback server, opens the browser, saves the account, and switches it active.

The browser callback server uses the same ports as Codex: `1455` by default, with `1457` as the fallback.

Use `codex-switch login <name> --device-auth` for device authorization. The CLI prints a verification URL and one-time code, then waits for authorization.

## Import

`import` reads an existing Codex `auth.json`. When `--file` is omitted, it imports from the current Codex auth file: `$CODEX_HOME/auth.json` when `CODEX_HOME` is set, otherwise `~/.codex/auth.json`.

`login` and `import` reject accounts that match an already stored auth identity.

## Usage

Usage reporting is supported for ChatGPT OAuth accounts. API key accounts are listed as unsupported for usage.

## Release

The GitHub Actions release workflow builds Linux musl binaries for `aarch64` and `x86_64`. On pushes to `master`, it compares the current Cargo package version with the previous `Cargo.toml` version. When the version changes, it creates tag `v{version}`, creates a GitHub release, and uploads the raw binaries:

```text
codex-switch-aarch64-unknown-linux-musl
codex-switch-x86_64-unknown-linux-musl
```

## Development

```sh
cargo fmt --check
cargo check --locked
cargo test --locked
cargo clippy --locked -- -D warnings
```
