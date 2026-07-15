# Account management

codex-switch stores multiple Codex accounts locally and identifies them by display name, full ID,
or a unique ID prefix. Run `codex-switch <command> --help` for the authoritative option list for a
command.

## List accounts

```sh
codex-switch list
codex-switch list --json
```

The current Codex auth account is marked in the human-readable output. JSON output omits account
credentials.

## Log in

Browser OAuth is the default login flow:

```sh
codex-switch login personal
```

The callback server uses the same ports as Codex: `1455` by default, with `1457` as the fallback.
For device authorization, use:

```sh
codex-switch login personal --device-auth
```

To refresh an existing stored ChatGPT OAuth account whose credentials no longer refresh, log in
again with its existing name:

```sh
codex-switch login personal --replace
```

Replacement happens only after OAuth succeeds. It preserves the stored account ID and timestamps
and rejects an auth identity that belongs to another stored account. API key accounts cannot be
replaced with `login --replace`.

Login saves the account but does not change the current Codex `auth.json`. Use `switch` when you
want Codex to use it.

## Import an existing Codex account

```sh
codex-switch import personal
codex-switch import personal --file /path/to/auth.json
```

Without `--file`, import reads `$CODEX_HOME/auth.json` when `CODEX_HOME` is set, or
`~/.codex/auth.json` otherwise. API key and regular ChatGPT OAuth files are supported. Agent
identity auth files and externally managed `chatgptAuthTokens` files are not supported because
codex-switch cannot refresh or switch those credentials safely.

Login and import reject accounts that match an existing stored auth identity.

## Export an account

Export prints a Codex-compatible `auth.json` to stdout by default:

```sh
codex-switch export personal
```

The output contains credentials. Treat it as a secret. To write a file, use:

```sh
codex-switch export personal --file ./auth.json
codex-switch export personal --file ./auth.json --force
```

Existing files are not overwritten without `--force`. Exporting does not change the current Codex
auth file.

## Switch accounts

```sh
codex-switch switch personal
```

`switch` refuses to write Codex auth data while unmanaged Codex processes are active. Close those
sessions before switching. Sessions launched by `codex-switch run` are managed and can hot-load a
selected ChatGPT OAuth account; see [Runtime auto-switching](runtime.md).

## Include or exclude an account from automatic selection

```sh
codex-switch disable work
codex-switch enable work
```

A disabled account remains stored but is skipped by standalone `auto-switch`, managed `run`
switching, and `usage --all` forecast capacity. It still works with explicit `switch`, `usage`,
`export`, `rename`, and `delete` commands.

## Rename or delete an account

```sh
codex-switch rename personal personal-new
codex-switch delete personal-new
```

Deleting an account removes it from the codex-switch account store. It does not rewrite the
current Codex `auth.json`.

## Storage and security

Accounts are stored in `~/.codex-switch/accounts.json`. This file contains credentials and should
not be printed, shared, or committed.

Switching writes Codex auth data to:

1. `$CODEX_HOME/auth.json`, when `CODEX_HOME` is set.
2. `~/.codex/auth.json`, otherwise.

On Unix, codex-switch writes the account store and Codex auth file with `0600` permissions. Prefer
the codex-switch commands over editing `accounts.json` manually so concurrent updates use the
account-store lock.
