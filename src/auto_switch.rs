use anyhow::{Context, Result};

use crate::process;
use crate::store;
use crate::switcher;
use crate::types::{StoredAccount, UsageInfo, UsageLimitInfo};
use crate::usage;

#[derive(Debug)]
pub enum AutoSwitchResult {
    ActiveKept {
        account: Box<StoredAccount>,
        reason: String,
    },
    ActiveUnsupported {
        account: Box<StoredAccount>,
        reason: String,
    },
    Switched {
        from: Option<Box<StoredAccount>>,
        to: Box<StoredAccount>,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
enum UsageDecision {
    Usable(String),
    Unavailable(String),
    Unsupported(String),
    Error(String),
}

pub async fn auto_switch(threshold: f64) -> Result<AutoSwitchResult> {
    validate_threshold(threshold)?;
    process::ensure_can_switch()?;
    auto_switch_inner(threshold, false).await
}

pub async fn auto_switch_allow_running(threshold: f64) -> Result<AutoSwitchResult> {
    validate_threshold(threshold)?;
    auto_switch_inner(threshold, true).await
}

pub fn usage_requires_switch(info: &UsageInfo, threshold: f64) -> bool {
    matches!(assess_usage(info, threshold), UsageDecision::Unavailable(_))
}

async fn auto_switch_inner(threshold: f64, allow_running: bool) -> Result<AutoSwitchResult> {
    let store = store::load_accounts()?;
    if store.accounts.is_empty() {
        anyhow::bail!("No accounts stored.");
    }

    let active = store
        .active_account_id
        .as_deref()
        .and_then(|active_id| {
            store
                .accounts
                .iter()
                .find(|account| account.id == active_id)
        })
        .cloned();

    let mut switch_reason = match &active {
        Some(account) => {
            let info = usage::get_account_usage(account)
                .await
                .with_context(|| format!("Failed to get usage for {}", account.name))?;
            match assess_usage(&info, threshold) {
                UsageDecision::Usable(reason) => {
                    return Ok(AutoSwitchResult::ActiveKept {
                        account: Box::new(account.clone()),
                        reason,
                    });
                }
                UsageDecision::Unsupported(reason) => {
                    return Ok(AutoSwitchResult::ActiveUnsupported {
                        account: Box::new(account.clone()),
                        reason,
                    });
                }
                UsageDecision::Error(reason) => {
                    anyhow::bail!(
                        "Could not evaluate active account {}: {reason}",
                        account.name
                    );
                }
                UsageDecision::Unavailable(reason) => reason,
            }
        }
        None => "no active account".to_string(),
    };

    let active_id = active.as_ref().map(|account| account.id.as_str());
    let mut skipped = Vec::new();

    for account in store
        .accounts
        .iter()
        .filter(|account| Some(account.id.as_str()) != active_id)
    {
        let info = match usage::get_account_usage(account).await {
            Ok(info) => info,
            Err(err) => {
                skipped.push(format!("{}: {err}", account.name));
                continue;
            }
        };

        match assess_usage(&info, threshold) {
            UsageDecision::Usable(_) => {
                let switched = if allow_running {
                    switcher::switch_to_account_unchecked(&account.id).await?
                } else {
                    switcher::switch_to_account(&account.id).await?
                };
                return Ok(AutoSwitchResult::Switched {
                    from: active.map(Box::new),
                    to: Box::new(switched),
                    reason: switch_reason,
                });
            }
            UsageDecision::Unavailable(reason)
            | UsageDecision::Unsupported(reason)
            | UsageDecision::Error(reason) => {
                skipped.push(format!("{}: {reason}", account.name));
            }
        }
    }

    if let Some(detail) = skipped.first() {
        switch_reason.push_str(&format!("; no usable replacement found ({detail})"));
    } else {
        switch_reason.push_str("; no replacement account found");
    }

    anyhow::bail!("{switch_reason}");
}

fn validate_threshold(threshold: f64) -> Result<()> {
    if !threshold.is_finite() || !(0.0..=100.0).contains(&threshold) {
        anyhow::bail!("threshold must be between 0 and 100");
    }
    Ok(())
}

fn assess_usage(info: &UsageInfo, threshold: f64) -> UsageDecision {
    if matches!(info.error.as_deref(), Some("usage unsupported")) {
        return UsageDecision::Unsupported("usage unsupported".to_string());
    }

    if let Some(error) = &info.error {
        return UsageDecision::Error(error.clone());
    }

    if let Some(kind) = info.rate_limit_reached_type.as_deref() {
        return UsageDecision::Unavailable(rate_limit_reason(kind));
    }

    if credits_depleted(info) {
        return UsageDecision::Unavailable("credits balance is 0".to_string());
    }

    if let Some(reason) = limit_threshold_reason(info, threshold) {
        return UsageDecision::Unavailable(reason);
    }

    UsageDecision::Usable("usage is below threshold".to_string())
}

fn rate_limit_reason(kind: &str) -> String {
    match kind {
        "workspace_owner_credits_depleted" | "workspace_member_credits_depleted" => {
            "credits depleted".to_string()
        }
        "workspace_owner_usage_limit_reached" | "workspace_member_usage_limit_reached" => {
            "usage limit reached".to_string()
        }
        "rate_limit_reached" => "rate limit reached".to_string(),
        value => format!("rate limit reached: {value}"),
    }
}

fn credits_depleted(info: &UsageInfo) -> bool {
    if info.unlimited_credits == Some(true) {
        return false;
    }
    if info.has_credits != Some(true) {
        return false;
    }
    info.credits_balance
        .as_deref()
        .and_then(|balance| balance.trim().parse::<f64>().ok())
        .is_some_and(|balance| balance <= 0.0)
}

fn limit_threshold_reason(info: &UsageInfo, threshold: f64) -> Option<String> {
    [
        ("5-hour".to_string(), info.primary_used_percent),
        ("weekly".to_string(), info.secondary_used_percent),
    ]
    .into_iter()
    .chain(info.additional_limits.iter().flat_map(additional_windows))
    .find_map(|(label, value)| {
        value.and_then(|used| {
            if used >= threshold {
                Some(format!("{label} usage is {used:.1}%"))
            } else {
                None
            }
        })
    })
}

fn additional_windows(limit: &UsageLimitInfo) -> [(String, Option<f64>); 2] {
    let label = limit
        .limit_name
        .as_deref()
        .or(limit.limit_id.as_deref())
        .unwrap_or("additional")
        .to_string();
    [
        (format!("{label} 5-hour"), limit.primary_used_percent),
        (format!("{label} weekly"), limit.secondary_used_percent),
    ]
}

#[cfg(test)]
mod tests {
    use super::{UsageDecision, assess_usage};
    use crate::types::UsageInfo;

    #[test]
    fn usage_is_unavailable_when_credits_are_depleted() {
        let info = UsageInfo {
            has_credits: Some(true),
            unlimited_credits: Some(false),
            credits_balance: Some("0".to_string()),
            ..usage_info()
        };

        assert_eq!(
            assess_usage(&info, 100.0),
            UsageDecision::Unavailable("credits balance is 0".to_string())
        );
    }

    #[test]
    fn usage_is_unavailable_when_limit_reached_type_is_set() {
        let info = UsageInfo {
            rate_limit_reached_type: Some("workspace_member_usage_limit_reached".to_string()),
            ..usage_info()
        };

        assert_eq!(
            assess_usage(&info, 100.0),
            UsageDecision::Unavailable("usage limit reached".to_string())
        );
    }

    #[test]
    fn usage_is_unavailable_when_threshold_is_reached() {
        let info = UsageInfo {
            primary_used_percent: Some(95.0),
            ..usage_info()
        };

        assert_eq!(
            assess_usage(&info, 90.0),
            UsageDecision::Unavailable("5-hour usage is 95.0%".to_string())
        );
    }

    #[test]
    fn usage_is_usable_when_below_threshold() {
        let info = UsageInfo {
            primary_used_percent: Some(50.0),
            secondary_used_percent: Some(80.0),
            ..usage_info()
        };

        assert_eq!(
            assess_usage(&info, 90.0),
            UsageDecision::Usable("usage is below threshold".to_string())
        );
    }

    fn usage_info() -> UsageInfo {
        UsageInfo {
            account_id: "account-id".to_string(),
            limit_id: Some("codex".to_string()),
            limit_name: None,
            plan_type: Some("plus".to_string()),
            primary_used_percent: None,
            primary_window_minutes: None,
            primary_resets_at: None,
            secondary_used_percent: None,
            secondary_window_minutes: None,
            secondary_resets_at: None,
            has_credits: None,
            unlimited_credits: None,
            credits_balance: None,
            rate_limit_reached_type: None,
            additional_limits: Vec::new(),
            error: None,
        }
    }
}
