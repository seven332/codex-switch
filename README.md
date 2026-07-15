# codex-switch

Multi-account runtime switching for Codex.

codex-switch keeps several ChatGPT Codex accounts on one machine and runs Codex through a small
local proxy. When the current account reaches a usage limit, a managed session can hot-load another
account with usable quota so the next turn continues without editing `auth.json` or restarting the
TUI.

![codex-switch usage overview](docs/assets/usage-overview.svg)

![codex-switch runtime auto-switch warning](docs/assets/runtime-auto-switch.svg)

## Quick start

Install from crates.io:

```sh
cargo install codex-switch
```

Add accounts, inspect their quota, and start a managed Codex session:

```sh
codex-switch login personal
codex-switch login work
codex-switch usage --all
codex-switch run -- resume
```

Use `codex-switch run` anywhere you would normally start an interactive Codex session. Arguments
for Codex must appear after `--`; for example, `codex-switch run -- resume --last` starts
`codex resume --last` with runtime account switching enabled.

## Highlights

- Store and manage multiple Codex accounts locally.
- Login with browser OAuth or device authorization.
- Import and export Codex-compatible `auth.json` files.
- Switch accounts safely or exclude selected accounts from automatic use.
- Run Codex with quota-aware, in-session switching for ChatGPT OAuth accounts.
- Report returned usage windows, credits, reset credits, and overall pool forecasts.
- Diagnose installation, auth, process, permission, and runtime-log problems locally.
- Keep credentials local under `~/.codex-switch` with private Unix file permissions.

## Common commands

| Task | Command |
| --- | --- |
| Add an account | `codex-switch login <name>` |
| List accounts | `codex-switch list` |
| Check all usage | `codex-switch usage --all` |
| Start managed Codex | `codex-switch run -- [CODEX_ARGS]...` |
| Switch explicitly | `codex-switch switch <name-or-id>` |
| Run a one-shot quota check | `codex-switch auto-switch` |
| Diagnose the installation | `codex-switch doctor` |

Run `codex-switch --help` to list every command, or
`codex-switch <command> --help` for the authoritative options for one command.

## How runtime switching works

`run` starts `codex app-server`, launches the remote Codex TUI through a local WebSocket proxy, and
monitors usage signals. Usage-limit errors and hard-limit notifications trigger recovery attempts;
best-effort checks also run in the background. A usable replacement can be applied to the running
app-server without restarting the TUI.

Runtime switching requires ChatGPT OAuth accounts. API key accounts can still be stored and used
for explicit switches, but they are not usage-checkable and cannot be hot-loaded into a managed
session. See [Runtime auto-switching](docs/runtime.md) for triggers, limitations, and diagnostics.

## Documentation

- [Account management](docs/accounts.md): login, import, export, switch, enable/disable, storage,
  and credential safety.
- [Runtime auto-switching](docs/runtime.md): one-shot and managed switching, triggers, and runtime
  behavior.
- [Usage reporting](docs/usage.md): usage windows, pool forecasts, earned resets, and JSON output.
- [Maintenance and diagnostics](docs/maintenance.md): doctor, runtime logs, and updates.
- [Development and releases](docs/development.md): local validation and the release workflow.

## Storage and security

Stored accounts, including their credentials, live in `~/.codex-switch/accounts.json`. Switching
writes Codex auth data to `$CODEX_HOME/auth.json` when `CODEX_HOME` is set, or `~/.codex/auth.json`
otherwise. Do not print, share, or commit these files. See
[Account management](docs/accounts.md#storage-and-security) for details.

## License

[MIT](LICENSE)
