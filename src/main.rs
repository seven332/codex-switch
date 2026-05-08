mod auth_json;
mod auto_switch;
mod cli;
mod oauth;
mod process;
mod runtime;
mod store;
mod switcher;
mod token;
mod types;
mod usage;

use std::collections::HashMap;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeZone, Utc};
use clap::Parser;

use cli::{Cli, Command};
use types::{AccountsStore, StoredAccount, UsageInfo};

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
            print_accounts(&store);
        }
        Command::Login { name, device_auth } => {
            store::ensure_name_available(&name)?;
            process::ensure_can_switch()?;
            let flow = if device_auth {
                oauth::LoginFlow::DeviceAuth
            } else {
                oauth::LoginFlow::Browser
            };
            let account = oauth::login(name, flow).await?;
            if let Some(existing) = store::find_duplicate_account(&account)? {
                anyhow::bail!(
                    "account is already stored as {} ({})",
                    existing.name,
                    store::short_id(&existing.id)
                );
            }
            process::ensure_can_switch()?;
            let stored = store::add_account(account)?;
            let active = switcher::switch_to_account(&stored.id).await?;
            println!(
                "Logged in and switched to {} ({})",
                active.name,
                store::short_id(&active.id)
            );
        }
        Command::Import { name, file } => {
            let file = match file {
                Some(file) => file,
                None => auth_json::codex_auth_file()?,
            };
            let account = auth_json::import_from_auth_json(&file, name)?;

            if let Some(existing) = store::find_duplicate_account(&account)? {
                anyhow::bail!(
                    "auth.json is already imported as {} ({})",
                    existing.name,
                    store::short_id(&existing.id)
                );
            }

            store::ensure_name_available(&account.name)?;
            let stored = store::add_account(account)?;
            println!(
                "Imported {} ({}) from {}",
                stored.name,
                store::short_id(&stored.id),
                file.display()
            );
        }
        Command::Switch { account } => {
            let active = switcher::switch_to_account(&account).await?;
            println!(
                "Switched to {} ({})",
                active.name,
                store::short_id(&active.id)
            );
        }
        Command::AutoSwitch { threshold } => {
            let result = auto_switch::auto_switch(threshold).await?;
            print_auto_switch_result(result);
        }
        Command::Run {
            threshold,
            codex_bin,
            codex_args,
        } => {
            let status = runtime::run_codex(threshold, codex_bin, codex_args).await?;
            if !status.success() {
                std::process::exit(status.code().unwrap_or(1));
            }
        }
        Command::Usage { all, account } => {
            if all {
                print_all_usage().await?;
            } else {
                let account = match account {
                    Some(selector) => store::get_account_by_selector(&selector)?,
                    None => store::get_active_account()?.context(
                        "No active account. Pass a name-or-id, or run codex-switch switch <name-or-id>.",
                    )?,
                };
                let info = usage::get_account_usage(&account).await?;
                print_usage(&account, &info);
            }
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

fn print_auto_switch_result(result: auto_switch::AutoSwitchResult) {
    match result {
        auto_switch::AutoSwitchResult::ActiveKept { account, reason } => {
            println!(
                "Active account is usable: {} ({}) - {}",
                account.name,
                store::short_id(&account.id),
                reason
            );
        }
        auto_switch::AutoSwitchResult::ActiveUnsupported { account, reason } => {
            println!(
                "Active account was not switched: {} ({}) - {}",
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

async fn print_all_usage() -> Result<()> {
    let store = store::load_accounts()?;
    if store.accounts.is_empty() {
        println!("No accounts stored.");
        return Ok(());
    }

    let results = usage::get_all_account_usage(&store.accounts).await;
    let by_id: HashMap<String, UsageInfo> = results
        .into_iter()
        .map(|info| (info.account_id.clone(), info))
        .collect();

    for (index, account) in store.accounts.iter().enumerate() {
        if index > 0 {
            println!();
        }
        if let Some(info) = by_id.get(&account.id) {
            print_usage(account, info);
        }
    }

    Ok(())
}

fn print_accounts(store: &AccountsStore) {
    if store.accounts.is_empty() {
        println!("No accounts stored.");
        return;
    }

    println!(
        "{:<3} {:<8} {:<22} {:<32} {:<10} {:<10} LAST USED",
        "", "ID", "NAME", "EMAIL", "PLAN", "AUTH"
    );
    for account in &store.accounts {
        let active = if store.active_account_id.as_deref() == Some(&account.id) {
            "*"
        } else {
            ""
        };
        println!(
            "{:<3} {:<8} {:<22} {:<32} {:<10} {:<10} {}",
            active,
            store::short_id(&account.id),
            account.name,
            account.email.as_deref().unwrap_or("-"),
            account.plan_type.as_deref().unwrap_or("-"),
            account.auth_mode,
            format_datetime_option(account.last_used_at.as_ref())
        );
    }
}

fn print_usage(account: &StoredAccount, info: &UsageInfo) {
    println!("{} ({})", account.name, store::short_id(&account.id));

    if matches!(info.error.as_deref(), Some("usage unsupported")) {
        println!("usage: unsupported");
        return;
    }

    if let Some(error) = &info.error {
        println!("usage error: {error}");
        return;
    }

    println!("plan: {}", info.plan_type.as_deref().unwrap_or("-"));
    print_limit_window(
        "5-hour",
        info.primary_used_percent,
        info.primary_window_minutes,
        info.primary_resets_at,
    );
    print_limit_window(
        "weekly",
        info.secondary_used_percent,
        info.secondary_window_minutes,
        info.secondary_resets_at,
    );
    print_credits(info);
    if let Some(kind) = &info.rate_limit_reached_type {
        println!("rate limit reached: {kind}");
    }
    print_additional_limits(info);
}

fn print_limit_window(
    label: &str,
    used_percent: Option<f64>,
    window_minutes: Option<i64>,
    resets_at: Option<i64>,
) {
    let percent = used_percent
        .map(|value| format!("{value:.1}% used"))
        .unwrap_or_else(|| "-".to_string());
    let reset = resets_at
        .map(format_unix_timestamp)
        .unwrap_or_else(|| "-".to_string());

    match window_minutes {
        Some(minutes) => println!("{label}: {percent}, window {minutes}m, resets {reset}"),
        None => println!("{label}: {percent}, resets {reset}"),
    }
}

fn print_credits(info: &UsageInfo) {
    if info.has_credits.is_none()
        && info.unlimited_credits.is_none()
        && info.credits_balance.is_none()
    {
        return;
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

    println!("credits: {credits}");
}

fn print_additional_limits(info: &UsageInfo) {
    for limit in &info.additional_limits {
        let label = limit
            .limit_name
            .as_deref()
            .or(limit.limit_id.as_deref())
            .unwrap_or("additional");
        println!("additional {label}:");
        print_limit_window(
            "  5-hour",
            limit.primary_used_percent,
            limit.primary_window_minutes,
            limit.primary_resets_at,
        );
        print_limit_window(
            "  weekly",
            limit.secondary_used_percent,
            limit.secondary_window_minutes,
            limit.secondary_resets_at,
        );
    }
}

fn format_datetime_option(value: Option<&DateTime<Utc>>) -> String {
    value
        .map(|dt| {
            dt.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S %:z")
                .to_string()
        })
        .unwrap_or_else(|| "-".to_string())
}

fn format_unix_timestamp(timestamp: i64) -> String {
    Utc.timestamp_opt(timestamp, 0)
        .single()
        .map(|dt| {
            dt.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S %:z")
                .to_string()
        })
        .unwrap_or_else(|| timestamp.to_string())
}
