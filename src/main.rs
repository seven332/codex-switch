mod account_selector;
mod auth_json;
mod auto_switch;
mod cli;
mod codex_http;
mod oauth;
mod process;
mod redaction;
mod runtime;
mod store;
mod store_lock;
mod switcher;
mod token;
mod types;
mod update;
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
            let current_account_id = auth_json::current_stored_account_id_best_effort(&store);
            print_accounts(&store, current_account_id.as_deref());
        }
        Command::Login { name, device_auth } => {
            store::ensure_name_available(&name)?;
            let flow = if device_auth {
                oauth::LoginFlow::DeviceAuth
            } else {
                oauth::LoginFlow::Browser
            };
            let account = oauth::login(name, flow).await?;
            let stored = store::add_account(account)?;
            println!(
                "Logged in {} ({})",
                stored.name,
                store::short_id(&stored.id)
            );
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
        Command::Usage { all, account } => {
            if all {
                print_all_usage().await?;
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
                print_usage(&account, &info, is_current);
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

async fn print_all_usage() -> Result<()> {
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

    for (index, account) in store.accounts.iter().enumerate() {
        if index > 0 {
            println!();
        }
        if let Some(info) = by_id.get(&account.id) {
            let is_current = current_account_id.as_deref() == Some(account.id.as_str());
            print_usage(account, info, is_current);
        }
    }

    Ok(())
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

fn print_usage(account: &StoredAccount, info: &UsageInfo, is_current: bool) {
    println!("{}", format_usage_account_header(account, is_current));

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

fn format_usage_account_header(account: &StoredAccount, is_current: bool) -> String {
    let marker = if is_current { "* " } else { "" };
    format!(
        "{marker}{} ({})",
        account.name,
        store::short_id(&account.id)
    )
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

#[cfg(test)]
mod tests {
    use super::{format_usage_account_header, usage_account_from_store};
    use crate::store;
    use crate::types::{AccountsStore, StoredAccount};

    #[test]
    fn usage_header_marks_current_account() {
        let account = StoredAccount::new_api_key("work".to_string(), "sk-test".to_string());

        assert_eq!(
            format_usage_account_header(&account, true),
            format!("* work ({})", store::short_id(&account.id))
        );
    }

    #[test]
    fn usage_header_leaves_non_current_account_unmarked() {
        let account = StoredAccount::new_api_key("work".to_string(), "sk-test".to_string());

        assert_eq!(
            format_usage_account_header(&account, false),
            format!("work ({})", store::short_id(&account.id))
        );
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
