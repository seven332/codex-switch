# Usage reporting

Usage reporting is available for ChatGPT OAuth accounts. API key accounts are reported as
unsupported. Run `codex-switch usage --help` for the authoritative option list.

## Query usage

Without an account selector, usage is read for the current Codex auth account:

```sh
codex-switch usage
codex-switch usage personal
codex-switch usage --all
```

Account selectors may be a display name, full ID, or unique ID prefix. To include limits that are
hidden by default:

```sh
codex-switch usage --all --show-additional
```

The output includes only the windows returned by ChatGPT. Known 5-hour, daily, weekly, monthly,
and annual windows are identified from their reported duration rather than their primary or
secondary response position.

## Overall forecast

`usage --all` prints an overall estimate for the enabled ChatGPT OAuth account pool. It models
whichever canonical 5-hour and weekly windows ChatGPT returns; a pool with only one window type is
forecast from that window alone.

The estimate starts from the current account when it has usable data, periodically reapplies the
same account-selection and keep-current policy as auto-switch, and reports only rates that can be
estimated from the available windows. Disabled accounts do not contribute capacity or rate
samples.

## Earned rate-limit resets

When ChatGPT returns earned rate-limit reset credits, usage output includes the available count.
Each credit resets the weekly usage limit, but has its own redemption expiration separate from the
weekly window's normal reset time. Expired credits disappear and can no longer be redeemed.

When detailed credit metadata is available, usage output also shows the earliest upcoming
expiration. A deadline within one weekly period (seven days) is marked `expiring soon`:

```text
rate-limit resets: 4 available, next expires in 6d 3h (14:10 on 8 Sep), expiring soon
```

If the detail request fails or contains no usable future expiration, output falls back to the
available count. Consume one manually with:

```sh
codex-switch reset-usage personal
codex-switch reset-usage personal --yes
```

Without an account selector, `reset-usage` uses the current Codex auth account. It requires a
ChatGPT OAuth account and asks for confirmation unless `--yes` is supplied. Neither `run` nor
`auto-switch` consumes reset credits automatically.

## JSON output

`list`, `usage`, and `doctor` provide machine-readable output:

```sh
codex-switch list --json
codex-switch usage --all --json
codex-switch doctor --json
```

JSON is written to stdout, while diagnostics and command errors remain on stderr. Each document
uses `schema_version: 1` and omits API keys, ID tokens, access tokens, refresh tokens, and raw Codex
auth JSON. Usage entries include optional Unix-seconds field
`rate_limit_reset_credits_next_expires_at` when a future earned-credit expiration is known.
