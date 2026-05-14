# codex-switch

Local CLI account switcher for Codex.

## Install

```sh
cargo install codex-switch
```

For local development:

```sh
cargo install --path .
```

## Commands

```sh
codex-switch list
codex-switch login <name> [--device-auth]
codex-switch import <name> [--file <path>]
codex-switch export <name-or-id> [--file <path>] [--force]
codex-switch switch <name-or-id>
codex-switch auto-switch
codex-switch run [--codex-bin <path>] -- [CODEX_ARGS]...
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

`switch` refuses to write Codex auth data while unmanaged active Codex processes are detected. Close regular Codex sessions before switching accounts. Sessions launched through `codex-switch run` are managed by the local proxy, so `switch` is allowed while those sessions are running.

`auto-switch` also refuses to switch while Codex is running. When Codex is not running, it checks stored ChatGPT OAuth accounts and switches to the account selected by the quota-aware policy. Accounts that are out of credits, rate-limited, usage-limited, or at 100% usage are not selectable. API key accounts are not usage-checkable and are skipped as replacement candidates.

`run` is the runtime auto-switching entrypoint. It checks accounts before startup, starts `codex app-server`, launches Codex as a remote TUI through a local websocket proxy, and passes arguments after `--` to `codex`. During the managed session, the proxy watches Codex app-server `account/rateLimits/updated` notifications and usage-limit errors. It also performs best-effort background auto-switch checks every 15-45 minutes. Hard usage-limit signals trigger an immediate recovery attempt. Non-hard rate-limit updates only schedule a background re-check when the current Codex auth account is near the shared 5% bottleneck headroom threshold, so normal updates do not block the TUI on usage API calls. If another shell runs `codex-switch switch <name-or-id>`, managed `run` sessions detect the current Codex auth account change and hot-load it when the selected account is ChatGPT OAuth.

```sh
codex-switch run
codex-switch run -- resume
codex-switch run -- resume --last
codex-switch run -- resume <session-id>
```

`run` supports Codex interactive commands that accept `--remote`: the default TUI, `resume`, and `fork`. ChatGPT OAuth accounts are required for runtime switching. API key accounts are not usage-checkable and cannot be applied to the running app-server. Switching to an API key account still updates `auth.json` for the next Codex start, but existing managed `run` sessions keep their current runtime auth. The runtime switch applies through Codex's app-server auth API; an in-flight request may continue with the auth it already started with, so the request that first reports a usage-limit error may still fail before the next turn uses the replacement account.

## Login

`login` uses ChatGPT/OpenAI browser OAuth by default. It starts a local callback server, opens the browser, and saves the account. It does not write Codex `auth.json`; run `codex-switch switch <name-or-id>` when you want to use the new account.

The browser callback server uses the same ports as Codex: `1455` by default, with `1457` as the fallback.

Use `codex-switch login <name> --device-auth` for device authorization. The CLI prints a verification URL and one-time code, then waits for authorization.

## Import

`import` reads an existing Codex `auth.json`. When `--file` is omitted, it imports from the current Codex auth file: `$CODEX_HOME/auth.json` when `CODEX_HOME` is set, otherwise `~/.codex/auth.json`.

`login` and `import` reject accounts that match an already stored auth identity.

## Export

`export` writes a stored account as Codex-compatible `auth.json` JSON. By default it prints to stdout:

```sh
codex-switch export my-account
```

The exported JSON contains credentials. Treat stdout and exported files as secrets.

Use `--file` to write to a file. Existing files are not overwritten unless `--force` is passed.

```sh
codex-switch export my-account --file ./auth.json
codex-switch export my-account --file ./auth.json --force
```

Exporting does not change the current Codex auth file.

## Usage

Usage reporting is supported for ChatGPT OAuth accounts. API key accounts are listed as unsupported for usage.

## Release

The GitHub Actions release workflow builds Linux musl binaries for `aarch64` and `x86_64`, plus a macOS arm64 binary. On pushes to `master`, it compares the current Cargo package version with the previous `Cargo.toml` version. When the version changes, it creates tag `v{version}`, creates a GitHub release, uploads the raw binaries, and publishes the crate to crates.io.

Publishing to crates.io requires a repository Actions secret named `CARGO_REGISTRY_TOKEN`.

```text
codex-switch-aarch64-unknown-linux-musl
codex-switch-x86_64-unknown-linux-musl
codex-switch-aarch64-apple-darwin
```

## Development

```sh
cargo fmt --check
cargo check --locked --all-targets --all-features
cargo test --locked --all-targets --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
```
