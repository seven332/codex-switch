# Maintenance and diagnostics

Run `codex-switch <command> --help` for the authoritative option list for a command.

## Diagnose an installation

```sh
codex-switch doctor
codex-switch doctor --codex-bin /path/to/codex
codex-switch doctor --json
```

`doctor` checks the local codex-switch installation, Codex CLI, account store, current Codex
`auth.json`, sensitive-file permissions, process detection, and runtime logs. Use it when debugging
`codex-switch run` startup hangs, unexpected account matching, unsafe permissions, or missing Codex
remote-TUI support.

The default checks are local-only and read-only. They do not call ChatGPT usage APIs, refresh
tokens, write account or Codex auth files, consume reset credits, or switch accounts.

## Inspect runtime logs

```sh
codex-switch logs path
codex-switch logs latest
codex-switch logs tail
codex-switch logs tail --follow
codex-switch logs tail --lines 200
codex-switch logs prune
codex-switch logs prune --days 7 --dry-run
```

Logs are stored under `~/.codex-switch/logs/`. `tail` prints the latest log and defaults to 100
existing lines. `prune` defaults to retaining seven recent days; use `--dry-run` to inspect its
selection before deletion.

Cleanup only removes files that match the codex-switch runtime-log naming pattern and does not
delete the current process log.

## Update codex-switch

```sh
codex-switch update --check
codex-switch update
codex-switch update --version 0.2.1
```

For release binaries, the updater verifies the GitHub release asset SHA-256 digest before
replacing the executable. It does not run `sudo`; if the executable is not writable, rerun the
installation with suitable permissions.

For crates.io installations tracked by Cargo, update installs the exact crates.io version into
the same Cargo install root, so `cargo` must be available. Installations from a local path or Git
source are not rewritten; reinstall those manually.
