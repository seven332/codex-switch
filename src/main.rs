mod account_selector;
mod auth_json;
mod auto_switch;
mod cli;
mod codex_http;
mod doctor;
mod oauth;
mod process;
mod redaction;
mod runtime;
mod runtime_log;
mod store;
mod store_lock;
mod switcher;
mod token;
mod types;
mod update;
mod usage;
mod usage_forecast;
mod usage_style;

use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Local, TimeZone, Utc};
use clap::Parser;
use uuid::Uuid;

use cli::{Cli, Command};
use types::{AccountsStore, AuthData, ConsumeRateLimitResetCreditCode, StoredAccount, UsageInfo};

const USAGE_BAR_WIDTH: usize = 20;
const USAGE_LABEL_WIDTH: usize = 6;
const QUOTA_BAR_FILLED: &str = "\u{2588}";
const QUOTA_BAR_EMPTY: &str = "\u{2591}";
const RESET_BAR_FILLED: &str = "\u{2588}";
const RESET_BAR_EMPTY: &str = "\u{2500}";

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::List => {
            let store = store::load_accounts()?;
            let current_account_id = auth_json::current_stored_account_id_best_effort(&store);
            print_accounts(&store, current_account_id.as_deref());
        }
        Command::Login {
            name,
            replace,
            device_auth,
        } => {
            if replace {
                store::ensure_chatgpt_replacement_target(&name)?;
            } else {
                store::ensure_name_available(&name)?;
            }
            let flow = if device_auth {
                oauth::LoginFlow::DeviceAuth
            } else {
                oauth::LoginFlow::Browser
            };
            let account = oauth::login(name.clone(), flow).await?;
            let stored = if replace {
                store::replace_chatgpt_account_by_name(&name, account)?
            } else {
                store::add_account(account)?
            };
            let action = if replace {
                "Replaced login for"
            } else {
                "Logged in"
            };
            println!("{action} {} ({})", stored.name, store::short_id(&stored.id));
        }
        Command::Import { name, file } => {
            let file = match file {
                Some(file) => file,
                None => auth_json::codex_auth_file()?,
            };
            let account = auth_json::import_from_auth_json(&file, name)?;

            let stored = store::add_account(account)?;
            println!(
                "Imported {} ({}) from {}",
                stored.name,
                store::short_id(&stored.id),
                file.display()
            );
        }
        Command::Export {
            account,
            file,
            force,
        } => {
            let account = store::get_account_by_selector(&account)?;
            match file {
                Some(file) => {
                    auth_json::export_account_auth(&account, &file, force)?;
                    println!(
                        "Exported {} ({}) to {}",
                        account.name,
                        store::short_id(&account.id),
                        file.display()
                    );
                }
                None => {
                    print!("{}", auth_json::account_auth_json_content(&account)?);
                }
            }
        }
        Command::Switch { account } => {
            let active = switcher::switch_to_account(&account).await?;
            println!(
                "Switched to {} ({})",
                active.name,
                store::short_id(&active.id)
            );
        }
        Command::AutoSwitch => {
            let result = auto_switch::auto_switch().await?;
            print_auto_switch_result(result);
        }
        Command::Run {
            codex_bin,
            codex_args,
        } => {
            let status = runtime::run_codex(codex_bin, codex_args).await?;
            if !status.success() {
                std::process::exit(status.code().unwrap_or(1));
            }
        }
        Command::Update { check, version } => {
            let outcome = update::update(update::UpdateOptions { check, version }).await?;
            print_update_outcome(outcome);
        }
        Command::Doctor { codex_bin } => {
            let report = doctor::run(doctor::DoctorOptions { codex_bin });
            print!("{}", report.format_human());
            if report.has_errors() {
                std::process::exit(1);
            }
        }
        Command::Usage {
            all,
            show_additional,
            account,
        } => {
            if all {
                print_all_usage(show_additional).await?;
            } else {
                let accounts_store = store::load_accounts()?;
                let current_account_id = match account.as_deref() {
                    Some(_) => auth_json::current_stored_account_id_best_effort(&accounts_store),
                    None => auth_json::current_stored_account_id(&accounts_store)?,
                };
                let account = usage_account_from_store(
                    &accounts_store,
                    account.as_deref(),
                    current_account_id.as_deref(),
                )?;
                let is_current = current_account_id.as_deref() == Some(account.id.as_str());
                let info = usage::get_account_usage(&account).await?;
                print_usage(
                    &account,
                    &info,
                    is_current,
                    Utc::now().timestamp(),
                    show_additional,
                );
            }
        }
        Command::ResetUsage { account, yes } => {
            reset_usage(account.as_deref(), yes).await?;
        }
        Command::Delete { account } => {
            let removed = store::remove_account_by_selector(&account)?;
            println!(
                "Deleted {} ({})",
                removed.name,
                store::short_id(&removed.id)
            );
        }
        Command::Rename { account, new_name } => {
            let updated = store::rename_account_by_selector(&account, new_name)?;
            println!(
                "Renamed account to {} ({})",
                updated.name,
                store::short_id(&updated.id)
            );
        }
    }

    Ok(())
}

async fn reset_usage(account_selector: Option<&str>, yes: bool) -> Result<()> {
    let accounts_store = store::load_accounts()?;
    let current_account_id = match account_selector {
        Some(_) => auth_json::current_stored_account_id_best_effort(&accounts_store),
        None => auth_json::current_stored_account_id(&accounts_store)?,
    };
    let account = usage_account_from_store(
        &accounts_store,
        account_selector,
        current_account_id.as_deref(),
    )?;
    let is_current = current_account_id.as_deref() == Some(account.id.as_str());

    if matches!(account.auth_data, AuthData::ApiKey { .. }) {
        anyhow::bail!("Rate-limit resets are only supported for ChatGPT OAuth accounts");
    }

    if !yes && !confirm_rate_limit_reset(&account)? {
        println!("Cancelled.");
        return Ok(());
    }

    let redeem_request_id = Uuid::new_v4().to_string();
    let response = usage::consume_rate_limit_reset_credit(&account, &redeem_request_id).await?;

    match response.code {
        ConsumeRateLimitResetCreditCode::Reset => {
            println!(
                "Usage reset for {} ({}).",
                account.name,
                store::short_id(&account.id)
            );
            print_refreshed_usage_after_reset(&account, is_current).await?;
        }
        ConsumeRateLimitResetCreditCode::AlreadyRedeemed => {
            println!(
                "Usage reset was already redeemed for {} ({}).",
                account.name,
                store::short_id(&account.id)
            );
            print_refreshed_usage_after_reset(&account, is_current).await?;
        }
        ConsumeRateLimitResetCreditCode::NothingToReset => {
            println!(
                "Your usage does not need a reset right now for {} ({}).",
                account.name,
                store::short_id(&account.id)
            );
        }
        ConsumeRateLimitResetCreditCode::NoCredit => {
            println!(
                "No rate-limit resets are available for {} ({}).",
                account.name,
                store::short_id(&account.id)
            );
        }
    }

    Ok(())
}

fn confirm_rate_limit_reset(account: &StoredAccount) -> Result<bool> {
    if !io::stdin().is_terminal() {
        anyhow::bail!(
            "Refusing to consume a rate-limit reset without confirmation. Re-run with --yes to confirm."
        );
    }

    eprint!(
        "Use one rate-limit reset for {} ({})? [y/N] ",
        account.name,
        store::short_id(&account.id)
    );
    io::stderr().flush().context("Failed to flush prompt")?;

    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("Failed to read confirmation")?;
    let answer = answer.trim();
    Ok(answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes"))
}

async fn print_refreshed_usage_after_reset(
    account: &StoredAccount,
    is_current: bool,
) -> Result<()> {
    let fresh_account = store::get_account_by_selector(&account.id)?;
    let info = usage::get_account_usage(&fresh_account).await?;
    println!();
    print_usage(
        &fresh_account,
        &info,
        is_current,
        Utc::now().timestamp(),
        false,
    );
    Ok(())
}

fn usage_account_from_store(
    accounts_store: &AccountsStore,
    selector: Option<&str>,
    current_account_id: Option<&str>,
) -> Result<StoredAccount> {
    match selector {
        Some(selector) => {
            let account_id = store::resolve_account_id(accounts_store, selector)?;
            accounts_store
                .accounts
                .iter()
                .find(|account| account.id == account_id)
                .cloned()
                .context("Account not found after resolving selector")
        }
        None => current_account_id
            .and_then(|current_account_id| {
                accounts_store
                    .accounts
                    .iter()
                    .find(|account| account.id == current_account_id)
            })
            .cloned()
            .context(
                "No stored account matches current Codex auth.json. Pass a name-or-id, or run codex-switch import <name>.",
            ),
    }
}

fn print_auto_switch_result(result: auto_switch::AutoSwitchResult) {
    match result {
        auto_switch::AutoSwitchResult::CurrentKept { account, reason } => {
            println!(
                "Current account is usable: {} ({}) - {}",
                account.name,
                store::short_id(&account.id),
                reason
            );
        }
        auto_switch::AutoSwitchResult::CurrentUnsupported { account, reason } => {
            println!(
                "Current account was not switched: {} ({}) - {}",
                account.name,
                store::short_id(&account.id),
                reason
            );
        }
        auto_switch::AutoSwitchResult::Switched { from, to, reason } => {
            if let Some(from) = from {
                println!(
                    "Switched from {} ({}) to {} ({}) - {}",
                    from.name,
                    store::short_id(&from.id),
                    to.name,
                    store::short_id(&to.id),
                    reason
                );
            } else {
                println!(
                    "Switched to {} ({}) - {}",
                    to.name,
                    store::short_id(&to.id),
                    reason
                );
            }
        }
    }
}

fn print_update_outcome(outcome: update::UpdateOutcome) {
    match outcome {
        update::UpdateOutcome::UpToDate {
            current_version,
            release_version,
        } => {
            println!(
                "codex-switch is up to date (current {current_version}, release {release_version})."
            );
        }
        update::UpdateOutcome::UpdateAvailable {
            current_version,
            release_version,
        } => {
            println!("Update available: {current_version} -> {release_version}");
        }
        update::UpdateOutcome::CurrentNewer {
            current_version,
            release_version,
        } => {
            println!(
                "Current codex-switch version {current_version} is newer than release {release_version}."
            );
        }
        update::UpdateOutcome::Updated {
            previous_version,
            installed_version,
            executable_path,
        } => {
            if previous_version == installed_version {
                println!(
                    "Installed codex-switch {installed_version} at {}",
                    executable_path.display()
                );
            } else {
                println!(
                    "Updated codex-switch {previous_version} -> {installed_version} at {}",
                    executable_path.display()
                );
            }
        }
    }
}

async fn print_all_usage(show_additional: bool) -> Result<()> {
    let store = store::load_accounts()?;
    if store.accounts.is_empty() {
        println!("No accounts stored.");
        return Ok(());
    }
    let current_account_id = auth_json::current_stored_account_id_best_effort(&store);

    let results = usage::get_all_account_usage(&store.accounts).await;
    let by_id: HashMap<String, UsageInfo> = results
        .into_iter()
        .map(|info| (info.account_id.clone(), info))
        .collect();
    let now = Utc::now().timestamp();

    for (index, account) in store.accounts.iter().enumerate() {
        if index > 0 {
            println!();
        }
        if let Some(info) = by_id.get(&account.id) {
            let is_current = current_account_id.as_deref() == Some(account.id.as_str());
            print_usage(account, info, is_current, now, show_additional);
        }
    }
    if has_forecastable_usage_account(&store) {
        if let Some(forecast) = usage_forecast::forecast_all_usage(
            &store.accounts,
            &by_id,
            current_account_id.as_deref(),
            now,
        ) {
            println!();
            print_usage_forecast(&forecast, now);
        } else {
            println!();
            print_usage_output("overall estimate:\n  unavailable: not enough complete usage data");
        }
    }

    Ok(())
}

fn has_forecastable_usage_account(store: &AccountsStore) -> bool {
    store
        .accounts
        .iter()
        .any(|account| matches!(account.auth_data, AuthData::ChatGPT { .. }))
}

fn print_accounts(store: &AccountsStore, current_account_id: Option<&str>) {
    if store.accounts.is_empty() {
        println!("No accounts stored.");
        return;
    }

    println!(
        "{:<3} {:<8} {:<22} {:<32} {:<10} {:<10} LAST USED",
        "", "ID", "NAME", "EMAIL", "PLAN", "AUTH"
    );
    for account in &store.accounts {
        let current_marker = if current_account_id == Some(account.id.as_str()) {
            "*"
        } else {
            ""
        };
        println!(
            "{:<3} {:<8} {:<22} {:<32} {:<10} {:<10} {}",
            current_marker,
            store::short_id(&account.id),
            account.name,
            account.email.as_deref().unwrap_or("-"),
            account.plan_type.as_deref().unwrap_or("-"),
            account.auth_mode,
            format_datetime_option(account.last_used_at.as_ref())
        );
    }
}

fn print_usage(
    account: &StoredAccount,
    info: &UsageInfo,
    is_current: bool,
    now: i64,
    show_additional: bool,
) {
    print_usage_output(&format_usage(
        account,
        info,
        is_current,
        now,
        show_additional,
    ));
}

fn print_usage_forecast(forecast: &usage_forecast::UsageForecast, now: i64) {
    print_usage_output(&format_usage_forecast(forecast, now));
}

fn print_usage_output(output: &str) {
    println!("{}", usage_style::style_for_stdout().style_text(output));
}

fn format_usage_forecast(forecast: &usage_forecast::UsageForecast, now: i64) -> String {
    let mut lines = vec!["overall estimate:".to_string()];

    match &forecast.outcome {
        usage_forecast::UsageForecastOutcome::NotExpected { horizon_seconds } => {
            lines.push(format!(
                "  unavailable: not expected within {} at current pace",
                format_reset_duration(*horizon_seconds as u64)
            ));
        }
        usage_forecast::UsageForecastOutcome::Unavailable {
            at,
            limited_by,
            recovery_at,
        } => {
            lines.push(format!(
                "  unavailable: {}",
                format_reset_timestamp(*at, now)
            ));
            lines.push(format!("  limited by: {}", limited_by.as_str()));
            if let Some(recovery_at) = recovery_at {
                lines.push(format!(
                    "  recovery: {}",
                    format_reset_timestamp(*recovery_at, now)
                ));
                if *recovery_at >= *at {
                    lines.push(format!(
                        "  expected outage: {}",
                        format_reset_duration(recovery_at.saturating_sub(*at) as u64)
                    ));
                }
            }
        }
    }
    if let Some(rates) = forecast.rates {
        lines.push(format_estimated_rate(rates));
    }

    lines.join("\n")
}

fn format_estimated_rate(rates: usage_forecast::UsageForecastRates) -> String {
    format!(
        "  estimated rate: 5-hour {:.1}%/h, weekly {:.1}%/h",
        rates.five_hour_percent_per_hour, rates.weekly_percent_per_hour
    )
}

fn format_usage(
    account: &StoredAccount,
    info: &UsageInfo,
    is_current: bool,
    now: i64,
    show_additional: bool,
) -> String {
    let unavailable_status = format_usage_unavailable_status(info);
    let mut lines = vec![format_usage_account_header(
        account,
        is_current,
        unavailable_status.is_some(),
    )];

    if matches!(info.error.as_deref(), Some("usage unsupported")) {
        lines.push("usage: unsupported".to_string());
        return lines.join("\n");
    }

    if let Some(error) = &info.error {
        lines.push(format!("usage error: {error}"));
        return lines.join("\n");
    }

    lines.push(format!(
        "plan: {}",
        info.plan_type.as_deref().unwrap_or("-")
    ));
    if let Some(status) = unavailable_status {
        lines.push(status);
    }
    lines.push(format_limit_window(
        "5-hour",
        info.primary_used_percent,
        info.primary_window_minutes,
        info.primary_resets_at,
        now,
    ));
    lines.push(format_limit_window(
        "weekly",
        info.secondary_used_percent,
        info.secondary_window_minutes,
        info.secondary_resets_at,
        now,
    ));
    if let Some(credits) = format_credits(info) {
        lines.push(credits);
    }
    if let Some(rate_limit_reset_credits) = format_rate_limit_reset_credits(info) {
        lines.push(rate_limit_reset_credits);
    }
    if show_additional {
        lines.extend(format_additional_limits(info, now));
    }

    lines.join("\n")
}

fn format_usage_account_header(
    account: &StoredAccount,
    is_current: bool,
    is_limited: bool,
) -> String {
    let marker = if is_current { "* " } else { "" };
    let status = if is_limited { " [UNAVAILABLE]" } else { "" };
    format!(
        "{marker}{} ({}){status}",
        account.name,
        store::short_id(&account.id)
    )
}

fn format_limit_status(kind: &str) -> String {
    let reason = match kind {
        "rate_limit_reached" => "rate limited",
        "workspace_owner_credits_depleted" | "workspace_member_credits_depleted" => {
            "credits depleted"
        }
        "workspace_owner_usage_limit_reached" | "workspace_member_usage_limit_reached" => {
            "usage limit reached"
        }
        _ => "limited",
    };

    format!("status: {reason} ({kind})")
}

fn format_usage_unavailable_status(info: &UsageInfo) -> Option<String> {
    if info.error.is_some() {
        return None;
    }

    if let Some(kind) = &info.rate_limit_reached_type {
        return Some(format_limit_status(kind));
    }

    auto_switch::usage_unavailable_reason(info).map(|reason| format!("status: {reason}"))
}

fn format_limit_window(
    label: &str,
    used_percent: Option<f64>,
    window_minutes: Option<i64>,
    resets_at: Option<i64>,
    now: i64,
) -> String {
    let left = format_usage_left_percent(used_percent);
    let quota_bar = format_usage_left_bar(used_percent);
    let reset_bar = format_reset_remaining_bar(resets_at, window_minutes, now);
    let reset = format_reset_detail(resets_at, window_minutes, now);
    let label_width = USAGE_LABEL_WIDTH.max(label.len());

    format!(
        "{:<width$} ┬ quota [{quota_bar}] {left}\n{:<width$} └ reset [{reset_bar}] {reset}",
        label,
        "",
        width = label_width
    )
}

fn format_credits(info: &UsageInfo) -> Option<String> {
    if info.has_credits.is_none()
        && info.unlimited_credits.is_none()
        && info.credits_balance.is_none()
    {
        return None;
    }

    let credits = if info.unlimited_credits == Some(true) {
        "unlimited".to_string()
    } else if let Some(balance) = &info.credits_balance {
        balance.clone()
    } else if info.has_credits == Some(false) {
        "none".to_string()
    } else {
        "available".to_string()
    };

    Some(format!("credits: {credits}"))
}

fn format_rate_limit_reset_credits(info: &UsageInfo) -> Option<String> {
    info.rate_limit_reset_credits_available
        .map(|available_count| format!("rate-limit resets: {} available", available_count.max(0)))
}

fn format_additional_limits(info: &UsageInfo, now: i64) -> Vec<String> {
    let mut lines = Vec::new();

    for limit in &info.additional_limits {
        let label = limit
            .limit_name
            .as_deref()
            .or(limit.limit_id.as_deref())
            .unwrap_or("additional");
        lines.push(format!("additional {label}:"));
        lines.push(format_limit_window(
            "  5-hour",
            limit.primary_used_percent,
            limit.primary_window_minutes,
            limit.primary_resets_at,
            now,
        ));
        lines.push(format_limit_window(
            "  weekly",
            limit.secondary_used_percent,
            limit.secondary_window_minutes,
            limit.secondary_resets_at,
            now,
        ));
    }

    lines
}

fn format_datetime_option(value: Option<&DateTime<Utc>>) -> String {
    value
        .map(format_local_datetime)
        .unwrap_or_else(|| "-".to_string())
}

fn format_local_datetime(dt: &DateTime<Utc>) -> String {
    dt.with_timezone(&Local)
        .format("%Y-%m-%d %H:%M:%S %:z")
        .to_string()
}

fn format_reset_timestamp_option(timestamp: Option<i64>, now: i64) -> String {
    timestamp
        .map(|timestamp| format_reset_timestamp(timestamp, now))
        .unwrap_or_else(|| "-".to_string())
}

fn format_reset_timestamp(timestamp: i64, now: i64) -> String {
    let Some(dt) = Utc.timestamp_opt(timestamp, 0).single() else {
        return timestamp.to_string();
    };
    let Some(short) = format_short_reset_datetime(dt, now) else {
        return timestamp.to_string();
    };
    format!("{} ({short})", format_reset_relative_time(timestamp, now))
}

fn format_short_reset_datetime(dt: DateTime<Utc>, now: i64) -> Option<String> {
    let now = Utc.timestamp_opt(now, 0).single()?.with_timezone(&Local);
    let dt = dt.with_timezone(&Local);
    let date = dt.date_naive();
    let now_date = now.date_naive();
    let day_delta = date.signed_duration_since(now_date).num_days();

    if day_delta == 0 {
        Some(format!("today {}", dt.format("%H:%M")))
    } else if day_delta == 1 {
        Some(format!("tomorrow {}", dt.format("%H:%M")))
    } else if (2..7).contains(&day_delta) {
        Some(dt.format("%a %H:%M").to_string())
    } else if dt.year() == now.year() {
        Some(dt.format("%H:%M on %-d %b").to_string())
    } else {
        Some(dt.format("%H:%M on %-d %b %Y").to_string())
    }
}

fn format_usage_left_percent(used_percent: Option<f64>) -> String {
    used_percent
        .map(|used| format!("{:.1}% left", usage_left_percent(used)))
        .unwrap_or_else(|| "-".to_string())
}

fn format_usage_left_bar(used_percent: Option<f64>) -> String {
    format_ratio_bar(
        used_percent.map(|used| usage_left_percent(used) / 100.0),
        QUOTA_BAR_FILLED,
        QUOTA_BAR_EMPTY,
    )
}

fn usage_left_percent(used_percent: f64) -> f64 {
    if !used_percent.is_finite() {
        return 0.0;
    }
    (100.0 - used_percent).clamp(0.0, 100.0)
}

fn format_reset_remaining_percent(
    resets_at: Option<i64>,
    window_minutes: Option<i64>,
    now: i64,
) -> String {
    reset_window_remaining_ratio(resets_at, window_minutes, now)
        .map(|ratio| format!("{:.0}% remaining", ratio * 100.0))
        .unwrap_or_else(|| "-".to_string())
}

fn format_reset_remaining_bar(
    resets_at: Option<i64>,
    window_minutes: Option<i64>,
    now: i64,
) -> String {
    format_ratio_bar(
        reset_window_remaining_ratio(resets_at, window_minutes, now),
        RESET_BAR_FILLED,
        RESET_BAR_EMPTY,
    )
}

fn format_reset_detail(resets_at: Option<i64>, window_minutes: Option<i64>, now: i64) -> String {
    let remaining = format_reset_remaining_percent(resets_at, window_minutes, now);
    let reset = format_reset_timestamp_option(resets_at, now);

    match (remaining.as_str(), reset.as_str()) {
        ("-", "-") => "-".to_string(),
        ("-", reset) => reset.to_string(),
        (remaining, "-") => remaining.to_string(),
        (remaining, reset) => format!("{remaining}, {reset}"),
    }
}

fn format_ratio_bar(ratio: Option<f64>, filled_char: &str, empty_char: &str) -> String {
    let Some(ratio) = ratio else {
        return empty_char.repeat(USAGE_BAR_WIDTH);
    };
    if !ratio.is_finite() {
        return empty_char.repeat(USAGE_BAR_WIDTH);
    }

    let filled = (ratio.clamp(0.0, 1.0) * USAGE_BAR_WIDTH as f64).round() as usize;
    let filled = filled.min(USAGE_BAR_WIDTH);
    let empty = USAGE_BAR_WIDTH.saturating_sub(filled);
    format!("{}{}", filled_char.repeat(filled), empty_char.repeat(empty))
}

fn reset_window_remaining_ratio(
    resets_at: Option<i64>,
    window_minutes: Option<i64>,
    now: i64,
) -> Option<f64> {
    let (Some(resets_at), Some(window_minutes)) = (resets_at, window_minutes) else {
        return None;
    };
    if window_minutes <= 0 {
        return None;
    }

    let window_seconds = window_minutes.checked_mul(60)?;
    if window_seconds <= 0 {
        return None;
    }
    Utc.timestamp_opt(resets_at, 0).single()?;

    let remaining = resets_at.saturating_sub(now).max(0);
    Some((remaining as f64 / window_seconds as f64).clamp(0.0, 1.0))
}

fn format_reset_relative_time(timestamp: i64, now: i64) -> String {
    let delta = timestamp.saturating_sub(now);
    if delta.unsigned_abs() < 60 {
        return "now".to_string();
    }

    let duration = format_reset_duration(delta.unsigned_abs());
    if delta > 0 {
        format!("in {duration}")
    } else {
        format!("overdue by {duration}")
    }
}

fn format_reset_duration(seconds: u64) -> String {
    let minutes = (seconds / 60).max(1);
    let days = minutes / 1_440;
    let hours = (minutes % 1_440) / 60;
    let minutes = minutes % 60;

    if days > 0 {
        if hours > 0 {
            format!("{days}d {hours}h")
        } else if minutes > 0 {
            format!("{days}d {minutes}m")
        } else {
            format!("{days}d")
        }
    } else if hours > 0 {
        if minutes > 0 {
            format!("{hours}h {minutes}m")
        } else {
            format!("{hours}h")
        }
    } else {
        format!("{minutes}m")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        format_limit_status, format_limit_window, format_rate_limit_reset_credits,
        format_reset_detail, format_reset_remaining_bar, format_reset_remaining_percent,
        format_reset_timestamp_option, format_short_reset_datetime, format_usage,
        format_usage_account_header, format_usage_forecast, format_usage_left_bar,
        format_usage_left_percent, has_forecastable_usage_account, usage_account_from_store,
    };
    use crate::store;
    use crate::types::{
        AccountsStore, NewChatGptAccount, RedactedString, StoredAccount, UsageInfo, UsageLimitInfo,
    };
    use crate::usage_forecast::{
        ForecastLimit, UsageForecast, UsageForecastOutcome, UsageForecastRates,
    };
    use chrono::{TimeZone, Utc};

    #[test]
    fn reset_timestamp_includes_future_relative_time() {
        let now = 1_800_000_000;
        let reset = now + 2 * 60 * 60 + 15 * 60;

        assert_eq!(
            format_reset_timestamp_option(Some(reset), now),
            expected_reset_timestamp(reset, "in 2h 15m")
        );
    }

    #[test]
    fn reset_timestamp_includes_day_relative_time() {
        let now = 1_800_000_000;
        let reset = now + 6 * 24 * 60 * 60 + 16 * 60 * 60 + 30 * 60;

        assert_eq!(
            format_reset_timestamp_option(Some(reset), now),
            expected_reset_timestamp(reset, "in 6d 16h")
        );
    }

    #[test]
    fn reset_timestamp_keeps_minutes_when_days_have_no_hours() {
        let now = 1_800_000_000;
        let reset = now + 24 * 60 * 60 + 30 * 60;

        assert_eq!(
            format_reset_timestamp_option(Some(reset), now),
            expected_reset_timestamp(reset, "in 1d 30m")
        );
    }

    #[test]
    fn reset_timestamp_uses_now_for_near_current_time() {
        let now = 1_800_000_000;
        let reset = now + 30;

        assert_eq!(
            format_reset_timestamp_option(Some(reset), now),
            expected_reset_timestamp(reset, "now")
        );
    }

    #[test]
    fn reset_timestamp_uses_minutes_at_one_minute_boundary() {
        let now = 1_800_000_000;
        let future_reset = now + 60;
        let past_reset = now - 60;

        assert_eq!(
            format_reset_timestamp_option(Some(future_reset), now),
            expected_reset_timestamp(future_reset, "in 1m")
        );
        assert_eq!(
            format_reset_timestamp_option(Some(past_reset), now),
            expected_reset_timestamp(past_reset, "overdue by 1m")
        );
    }

    #[test]
    fn reset_timestamp_includes_past_relative_time() {
        let now = 1_800_000_000;
        let reset = now - 3 * 60;

        assert_eq!(
            format_reset_timestamp_option(Some(reset), now),
            expected_reset_timestamp(reset, "overdue by 3m")
        );
    }

    #[test]
    fn reset_timestamp_preserves_missing_value() {
        assert_eq!(format_reset_timestamp_option(None, 1_800_000_000), "-");
    }

    #[test]
    fn reset_timestamp_preserves_invalid_value_without_relative_time() {
        assert_eq!(
            format_reset_timestamp_option(Some(i64::MAX), 1_800_000_000),
            i64::MAX.to_string()
        );
    }

    fn expected_reset_timestamp(timestamp: i64, relative: &str) -> String {
        format!(
            "{relative} ({})",
            format_short_reset_datetime(Utc.timestamp_opt(timestamp, 0).unwrap(), 1_800_000_000)
                .unwrap()
        )
    }

    #[test]
    fn reset_timestamp_uses_short_local_time() {
        let now = 1_800_000_000;
        let reset = now + 2 * 24 * 60 * 60 + 15 * 60;

        assert_eq!(
            format_reset_timestamp_option(Some(reset), now),
            format!(
                "in 2d 15m ({})",
                format_short_reset_datetime(Utc.timestamp_opt(reset, 0).unwrap(), now).unwrap()
            )
        );
    }

    #[test]
    fn usage_left_percent_and_bar_use_remaining_quota() {
        assert_eq!(format_usage_left_percent(Some(92.0)), "8.0% left");
        assert_eq!(
            format_usage_left_bar(Some(92.0)),
            format!("{}{}", "\u{2588}".repeat(2), "\u{2591}".repeat(18))
        );
        assert_eq!(format_usage_left_percent(Some(10.0)), "90.0% left");
        assert_eq!(
            format_usage_left_bar(Some(10.0)),
            format!("{}{}", "\u{2588}".repeat(18), "\u{2591}".repeat(2))
        );
    }

    #[test]
    fn usage_left_percent_clamps_out_of_range_values() {
        assert_eq!(format_usage_left_percent(Some(150.0)), "0.0% left");
        assert_eq!(format_usage_left_bar(Some(150.0)), "\u{2591}".repeat(20));
        assert_eq!(format_usage_left_percent(Some(-20.0)), "100.0% left");
        assert_eq!(format_usage_left_bar(Some(-20.0)), "\u{2588}".repeat(20));
        assert_eq!(format_usage_left_percent(None), "-");
        assert_eq!(format_usage_left_bar(None), "\u{2591}".repeat(20));
        assert_eq!(format_usage_left_percent(Some(f64::NAN)), "0.0% left");
        assert_eq!(format_usage_left_bar(Some(f64::NAN)), "\u{2591}".repeat(20));
    }

    #[test]
    fn reset_remaining_bar_shows_remaining_window() {
        let now = 1_800_000_000;
        let reset = now + 60 * 60;

        assert_eq!(
            format_reset_remaining_bar(Some(reset), Some(300), now),
            format!("{}{}", "\u{2588}".repeat(4), "\u{2500}".repeat(16))
        );
        assert_eq!(
            format_reset_remaining_percent(Some(reset), Some(300), now),
            "20% remaining"
        );
    }

    #[test]
    fn reset_remaining_preserves_missing_or_invalid_window() {
        let now = 1_800_000_000;

        assert_eq!(
            format_reset_remaining_bar(None, Some(300), now),
            "\u{2500}".repeat(20)
        );
        assert_eq!(format_reset_remaining_percent(None, Some(300), now), "-");
        assert_eq!(
            format_reset_remaining_bar(Some(now + 60), None, now),
            "\u{2500}".repeat(20)
        );
        assert_eq!(
            format_reset_remaining_percent(Some(now + 60), Some(0), now),
            "-"
        );
        assert_eq!(
            format_reset_remaining_bar(Some(i64::MAX), Some(300), now),
            "\u{2500}".repeat(20)
        );
        assert_eq!(
            format_reset_remaining_percent(Some(i64::MAX), Some(300), now),
            "-"
        );
    }

    #[test]
    fn reset_detail_omits_duplicate_missing_placeholders() {
        let now = 1_800_000_000;
        let reset = now + 60 * 60;

        assert_eq!(format_reset_detail(None, None, now), "-");
        assert_eq!(
            format_reset_detail(Some(reset), None, now),
            format_reset_timestamp_option(Some(reset), now)
        );
        assert_eq!(
            format_reset_detail(Some(reset), Some(300), now),
            format!(
                "20% remaining, {}",
                format_reset_timestamp_option(Some(reset), now)
            )
        );
    }

    #[test]
    fn limit_window_formats_quota_and_reset_remaining() {
        let now = 1_800_000_000;
        let reset = now + 2 * 60 * 60 + 15 * 60;

        assert_eq!(
            format_limit_window("weekly", Some(92.0), Some(10_080), Some(reset), now),
            format!(
                "weekly ┬ quota [{}] 8.0% left\n       └ reset [{}] 1% remaining, {}",
                format_usage_left_bar(Some(92.0)),
                format_reset_remaining_bar(Some(reset), Some(10_080), now),
                format_reset_timestamp_option(Some(reset), now)
            )
        );
    }

    #[test]
    fn limit_window_aligns_reset_with_indented_labels() {
        let now = 1_800_000_000;
        let reset = now + 60 * 60;

        assert_eq!(
            format_limit_window("  5-hour", Some(20.0), Some(300), Some(reset), now),
            format!(
                "  5-hour ┬ quota [{}] 80.0% left\n         └ reset [{}] 20% remaining, {}",
                format_usage_left_bar(Some(20.0)),
                format_reset_remaining_bar(Some(reset), Some(300), now),
                format_reset_timestamp_option(Some(reset), now)
            )
        );
    }

    #[test]
    fn usage_header_marks_current_account() {
        let account = StoredAccount::new_api_key("work".to_string(), "sk-test".to_string());

        assert_eq!(
            format_usage_account_header(&account, true, false),
            format!("* work ({})", store::short_id(&account.id))
        );
    }

    #[test]
    fn usage_header_leaves_non_current_account_unmarked() {
        let account = StoredAccount::new_api_key("work".to_string(), "sk-test".to_string());

        assert_eq!(
            format_usage_account_header(&account, false, false),
            format!("work ({})", store::short_id(&account.id))
        );
    }

    #[test]
    fn usage_output_marks_limited_accounts() {
        let now = 1_800_000_000;
        let account = StoredAccount::new_api_key("work".to_string(), "sk-test".to_string());
        let mut info = usage_info_with_additional_limit(&account.id, now);
        info.rate_limit_reached_type = Some("workspace_member_usage_limit_reached".to_string());

        let output = format_usage(&account, &info, true, now, false);

        assert!(output.starts_with(&format!(
            "* work ({}) [UNAVAILABLE]",
            store::short_id(&account.id)
        )));
        let plan_index = output.find("plan: pro").expect("plan line");
        let status_index = output
            .find("status: usage limit reached (workspace_member_usage_limit_reached)")
            .expect("status line");
        let quota_index = output.find("5-hour").expect("quota line");
        assert!(plan_index < status_index);
        assert!(status_index < quota_index);
    }

    #[test]
    fn usage_output_marks_depleted_credits_unavailable() {
        let now = 1_800_000_000;
        let account = StoredAccount::new_api_key("work".to_string(), "sk-test".to_string());
        let mut info = usage_info_with_additional_limit(&account.id, now);
        info.unlimited_credits = Some(false);
        info.credits_balance = Some("0".to_string());

        let output = format_usage(&account, &info, true, now, false);

        assert!(output.starts_with(&format!(
            "* work ({}) [UNAVAILABLE]",
            store::short_id(&account.id)
        )));
        assert!(output.contains("status: credits balance is 0"));
    }

    #[test]
    fn usage_output_marks_hard_limits_unavailable() {
        let now = 1_800_000_000;
        let account = StoredAccount::new_api_key("work".to_string(), "sk-test".to_string());
        let mut info = usage_info_with_additional_limit(&account.id, now);
        info.primary_used_percent = Some(100.0);

        let output = format_usage(&account, &info, true, now, false);

        assert!(output.starts_with(&format!(
            "* work ({}) [UNAVAILABLE]",
            store::short_id(&account.id)
        )));
        assert!(output.contains("status: 5-hour usage is 100.0%"));
    }

    #[test]
    fn usage_output_shows_rate_limit_reset_credits_when_present() {
        let now = 1_800_000_000;
        let account = StoredAccount::new_api_key("work".to_string(), "sk-test".to_string());
        let mut info = usage_info_with_additional_limit(&account.id, now);
        info.rate_limit_reset_credits_available = Some(2);

        let output = format_usage(&account, &info, true, now, false);

        assert!(output.contains("credits: 42\nrate-limit resets: 2 available"));
    }

    #[test]
    fn rate_limit_reset_credits_are_hidden_when_missing() {
        let info = usage_info_with_additional_limit("account-id", 1_800_000_000);

        assert_eq!(format_rate_limit_reset_credits(&info), None);
    }

    #[test]
    fn rate_limit_reset_credits_clamp_negative_values() {
        let mut info = usage_info_with_additional_limit("account-id", 1_800_000_000);
        info.rate_limit_reset_credits_available = Some(-1);

        assert_eq!(
            format_rate_limit_reset_credits(&info).as_deref(),
            Some("rate-limit resets: 0 available")
        );
    }

    #[test]
    fn usage_output_classifies_limited_status_reasons() {
        assert_eq!(
            format_limit_status("rate_limit_reached"),
            "status: rate limited (rate_limit_reached)"
        );
        assert_eq!(
            format_limit_status("workspace_owner_credits_depleted"),
            "status: credits depleted (workspace_owner_credits_depleted)"
        );
        assert_eq!(
            format_limit_status("workspace_member_usage_limit_reached"),
            "status: usage limit reached (workspace_member_usage_limit_reached)"
        );
        assert_eq!(
            format_limit_status("custom_limit"),
            "status: limited (custom_limit)"
        );
    }

    #[test]
    fn usage_forecast_output_shows_pool_estimate() {
        let now = 1_800_000_000;
        let forecast = UsageForecast {
            rates: Some(UsageForecastRates {
                five_hour_percent_per_hour: 18.0,
                weekly_percent_per_hour: 0.7,
            }),
            outcome: UsageForecastOutcome::Unavailable {
                at: now + 3 * 60 * 60,
                limited_by: ForecastLimit::FiveHourAndWeekly,
                recovery_at: Some(now + 4 * 60 * 60),
            },
        };

        let output = format_usage_forecast(&forecast, now);

        assert!(output.contains("overall estimate:"));
        assert!(output.contains("unavailable: in 3h"));
        assert!(output.contains("limited by: 5-hour and weekly"));
        assert!(output.contains("recovery: in 4h"));
        assert!(output.contains("expected outage: 1h"));
        assert!(output.contains("estimated rate: 5-hour 18.0%/h, weekly 0.7%/h"));
    }

    #[test]
    fn usage_forecast_output_shows_sustainable_estimate() {
        let forecast = UsageForecast {
            rates: Some(UsageForecastRates {
                five_hour_percent_per_hour: 4.0,
                weekly_percent_per_hour: 0.2,
            }),
            outcome: UsageForecastOutcome::NotExpected {
                horizon_seconds: 14 * 24 * 60 * 60,
            },
        };

        let output = format_usage_forecast(&forecast, 1_800_000_000);

        assert!(output.contains("unavailable: not expected within 14d at current pace"));
        assert!(output.contains("estimated rate: 5-hour 4.0%/h, weekly 0.2%/h"));
    }

    #[test]
    fn usage_forecast_output_omits_rate_when_missing() {
        let now = 1_800_000_000;
        let forecast = UsageForecast {
            rates: None,
            outcome: UsageForecastOutcome::Unavailable {
                at: now,
                limited_by: ForecastLimit::FiveHour,
                recovery_at: Some(now + 60 * 60),
            },
        };

        let output = format_usage_forecast(&forecast, now);

        assert!(output.contains("unavailable: now"));
        assert!(!output.contains("estimated rate:"));
    }

    #[test]
    fn usage_forecast_requires_chatgpt_account() {
        let api_key_account = StoredAccount::new_api_key("work".to_string(), "sk-test".to_string());
        let api_key_store = AccountsStore {
            version: 1,
            accounts: vec![api_key_account],
            masked_account_ids: Vec::new(),
        };

        assert!(!has_forecastable_usage_account(&api_key_store));

        let chatgpt_store = AccountsStore {
            version: 1,
            accounts: vec![StoredAccount::new_chatgpt(NewChatGptAccount {
                name: "work".to_string(),
                email: None,
                plan_type: Some("pro".to_string()),
                chatgpt_user_id: None,
                chatgpt_account_is_fedramp: false,
                token_last_refresh_at: Utc::now(),
                subscription_expires_at: None,
                id_token: RedactedString::new("id-token"),
                access_token: RedactedString::new("access-token"),
                refresh_token: RedactedString::new("refresh-token"),
                account_id: Some("account".to_string()),
            })],
            masked_account_ids: Vec::new(),
        };

        assert!(has_forecastable_usage_account(&chatgpt_store));
    }

    #[test]
    fn usage_output_hides_additional_limits_by_default() {
        let now = 1_800_000_000;
        let account = StoredAccount::new_api_key("work".to_string(), "sk-test".to_string());
        let info = usage_info_with_additional_limit(&account.id, now);

        let output = format_usage(&account, &info, true, now, false);

        assert!(!output.contains("additional GPT-5.3-Codex-Spark"));
        assert!(output.contains("5-hour ┬ quota"));
        assert!(output.contains("weekly ┬ quota"));
    }

    #[test]
    fn usage_output_shows_additional_limits_when_requested() {
        let now = 1_800_000_000;
        let account = StoredAccount::new_api_key("work".to_string(), "sk-test".to_string());
        let info = usage_info_with_additional_limit(&account.id, now);

        let output = format_usage(&account, &info, true, now, true);

        assert!(output.contains("additional GPT-5.3-Codex-Spark:"));
        assert!(output.contains("  5-hour ┬ quota"));
        assert!(output.contains("  weekly ┬ quota"));
    }

    #[test]
    fn usage_account_from_store_uses_current_auth_account() {
        let account = StoredAccount::new_api_key("work".to_string(), "sk-test".to_string());
        let accounts_store = AccountsStore {
            version: 1,
            accounts: vec![account.clone()],
            masked_account_ids: Vec::new(),
        };

        let selected = usage_account_from_store(&accounts_store, None, Some(&account.id))
            .expect("current account");

        assert_eq!(selected.id, account.id);
    }

    fn usage_info_with_additional_limit(account_id: &str, now: i64) -> UsageInfo {
        UsageInfo {
            account_id: account_id.to_string(),
            limit_id: None,
            limit_name: None,
            plan_type: Some("pro".to_string()),
            primary_used_percent: Some(10.0),
            primary_window_minutes: Some(300),
            primary_resets_at: Some(now + 60 * 60),
            secondary_used_percent: Some(20.0),
            secondary_window_minutes: Some(10_080),
            secondary_resets_at: Some(now + 2 * 24 * 60 * 60),
            has_credits: Some(true),
            unlimited_credits: None,
            credits_balance: Some("42".to_string()),
            rate_limit_reset_credits_available: None,
            rate_limit_reached_type: None,
            additional_limits: vec![UsageLimitInfo {
                limit_id: Some("gpt-5.3-codex-spark".to_string()),
                limit_name: Some("GPT-5.3-Codex-Spark".to_string()),
                primary_used_percent: Some(0.0),
                primary_window_minutes: Some(300),
                primary_resets_at: Some(now + 2 * 60 * 60),
                secondary_used_percent: Some(0.0),
                secondary_window_minutes: Some(10_080),
                secondary_resets_at: Some(now + 3 * 24 * 60 * 60),
            }],
            error: None,
        }
    }

    #[test]
    fn usage_account_from_store_requires_current_auth_account() {
        let account = StoredAccount::new_api_key("work".to_string(), "sk-test".to_string());
        let accounts_store = AccountsStore {
            version: 1,
            accounts: vec![account.clone()],
            masked_account_ids: Vec::new(),
        };

        let err = usage_account_from_store(&accounts_store, None, None)
            .expect_err("usage without current auth should fail");

        assert!(err.to_string().contains("current Codex auth.json"));
    }

    #[test]
    fn usage_account_from_store_resolves_explicit_selector() {
        let account = StoredAccount::new_api_key("work".to_string(), "sk-test".to_string());
        let accounts_store = AccountsStore {
            version: 1,
            accounts: vec![account.clone()],
            masked_account_ids: Vec::new(),
        };

        let selected = usage_account_from_store(&accounts_store, Some("work"), None)
            .expect("selected account");

        assert_eq!(selected.id, account.id);
    }
}
