# Runtime auto-switching

codex-switch supports a one-shot account check and a managed Codex session that can change
ChatGPT OAuth accounts without restarting the TUI. Run `codex-switch <command> --help` for the
authoritative option list.

## One-shot auto-switch

```sh
codex-switch auto-switch
```

The command checks the current and stored ChatGPT OAuth accounts, applies the quota-aware selection
policy, and switches when that policy selects a different eligible account. This includes recovery
when the current account is out of usage. It refuses to switch while an unmanaged Codex process is
active.

Accounts that are out of credits, rate-limited, usage-limited, at 100% usage, or disabled are not
replacement candidates. A disabled current account is kept while it remains usable. API key
accounts are not usage-checkable and are skipped.

## Run a managed Codex session

```sh
codex-switch run
codex-switch run -- resume
codex-switch run -- resume --last
codex-switch run -- resume <session-id>
```

Codex arguments must appear after `--`. To use another Codex executable:

```sh
codex-switch run --codex-bin /path/to/codex -- resume
```

`run` supports Codex interactive commands that accept `--remote`: the default TUI, `resume`, and
`fork`. It checks accounts before startup, starts `codex app-server`, launches the remote TUI
through a local WebSocket proxy, and forwards the supplied Codex arguments.

If the startup check finds that the current account is usage-limited but no usable replacement
exists, `run` continues with the current Codex auth instead of blocking startup.

## In-session switching

During a managed session, codex-switch can load another ChatGPT OAuth account into the running
Codex app-server without restarting the TUI:

- Usage-limit errors trigger an immediate recovery attempt.
- `account/rateLimits/updated` notifications trigger immediate recovery for hard limits.
- A notification near the shared 5% bottleneck headroom threshold triggers a background re-check.
- Best-effort background checks run every 15–45 minutes.
- `codex-switch switch <name-or-id>` from another shell is hot-loaded by managed sessions.

Normal rate-limit updates do not block the TUI on usage API calls. An in-flight request may keep
the auth it started with, so the request that first reports a usage-limit error can fail before the
next turn uses the replacement account.

Runtime switching requires ChatGPT OAuth accounts. API key accounts are not usage-checkable and
cannot be applied to a running app-server. Explicitly switching to one still updates `auth.json`
for the next Codex process, while an existing managed session keeps its current runtime auth.

## Diagnostics

Each `run` process writes diagnostics under `~/.codex-switch/logs/`. Startup diagnostics also go to
stderr until control passes to the TUI; background diagnostics then remain in the log file so they
do not corrupt terminal rendering.

Set `CODEX_SWITCH_LOG` to a level such as `off` or `debug`, or to a full tracing filter. Managed
sessions also start best-effort cleanup for matching runtime logs older than seven days and never
delete the current process log.

See [Maintenance and diagnostics](maintenance.md) for `doctor` and `logs` commands.
