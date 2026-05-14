use anyhow::Result;

use crate::account_selector::{self, AccountUsageCandidate, SelectionConfig};
use crate::auth_json;
use crate::process;
use crate::store;
use crate::switcher;
use crate::types::{StoredAccount, UsageInfo, UsageLimitInfo};
use crate::usage;

const HARD_USAGE_LIMIT_PERCENT: f64 = 100.0;

#[derive(Debug)]
pub enum AutoSwitchResult {
    CurrentKept {
        account: Box<StoredAccount>,
        reason: String,
    },
    CurrentUnsupported {
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

pub async fn auto_switch() -> Result<AutoSwitchResult> {
    process::ensure_can_switch()?;
    auto_switch_inner(false).await
}

pub async fn auto_switch_allow_running() -> Result<AutoSwitchResult> {
    auto_switch_inner(true).await
}

pub fn usage_requires_switch(info: &UsageInfo) -> bool {
    matches!(assess_usage(info), UsageDecision::Unavailable(_))
}

async fn auto_switch_inner(allow_running: bool) -> Result<AutoSwitchResult> {
    let store = store::load_accounts()?;
    if store.accounts.is_empty() {
        anyhow::bail!("No accounts stored.");
    }

    let current = auth_json::current_stored_account_best_effort(&store);

    let current_id = current.as_ref().map(|account| account.id.as_str());
    let mut evaluations = Vec::with_capacity(store.accounts.len());
    let mut current_decision = None;
    let mut skipped = Vec::new();

    for account in &store.accounts {
        let is_current = Some(account.id.as_str()) == current_id;
        let info = match usage::get_account_usage(account).await {
            Ok(info) => info,
            Err(err) => {
                if is_current {
                    anyhow::bail!("Failed to get usage for {}: {err}", account.name);
                }
                skipped.push(format!("{}: {err}", account.name));
                continue;
            }
        };

        let decision = assess_usage(&info);
        if is_current {
            current_decision = Some(decision.clone());
            match &decision {
                UsageDecision::Unsupported(reason) => {
                    return Ok(AutoSwitchResult::CurrentUnsupported {
                        account: Box::new(account.clone()),
                        reason: reason.clone(),
                    });
                }
                UsageDecision::Error(reason) => {
                    anyhow::bail!(
                        "Could not evaluate current account {}: {reason}",
                        account.name
                    );
                }
                UsageDecision::Usable(_) | UsageDecision::Unavailable(_) => {}
            }
        } else if let UsageDecision::Unavailable(reason)
        | UsageDecision::Unsupported(reason)
        | UsageDecision::Error(reason) = &decision
        {
            skipped.push(format!("{}: {reason}", account.name));
        }

        evaluations.push(AccountUsageEvaluation {
            account: account.clone(),
            usage: info,
            decision,
        });
    }

    if let Some(selection) = select_usable_account_by_policy(&evaluations) {
        let selected_account = selection.account;
        if Some(selected_account.id.as_str()) == current_id {
            let reason = match current_decision {
                Some(UsageDecision::Usable(reason)) => reason,
                _ => "usage policy kept current account".to_string(),
            };
            return Ok(AutoSwitchResult::CurrentKept {
                account: Box::new(selected_account.clone()),
                reason,
            });
        }

        let switch_reason = match current_decision {
            Some(UsageDecision::Unavailable(reason)) => reason,
            Some(UsageDecision::Usable(_)) => format!(
                "usage policy selected an account with better quota headroom ({:.1} bottleneck score)",
                selection.metrics.bottleneck_headroom
            ),
            None => "no current account".to_string(),
            Some(UsageDecision::Unsupported(reason) | UsageDecision::Error(reason)) => reason,
        };
        let switched = if allow_running {
            switcher::switch_to_account_unchecked(&selected_account.id).await?
        } else {
            switcher::switch_to_account(&selected_account.id).await?
        };
        return Ok(AutoSwitchResult::Switched {
            from: current.map(Box::new),
            to: Box::new(switched),
            reason: switch_reason,
        });
    }

    if let (Some(account), Some(UsageDecision::Usable(reason))) =
        (&current, current_decision.clone())
    {
        return Ok(AutoSwitchResult::CurrentKept {
            account: Box::new(account.clone()),
            reason,
        });
    }

    let mut switch_reason = match current_decision {
        Some(UsageDecision::Unavailable(reason)) => reason,
        Some(UsageDecision::Usable(_)) => {
            "current account is usable, but usage policy found no selectable account".to_string()
        }
        Some(UsageDecision::Unsupported(reason) | UsageDecision::Error(reason)) => reason,
        None => "no current account".to_string(),
    };

    if let Some(detail) = skipped.first() {
        switch_reason.push_str(&format!("; no usable replacement found ({detail})"));
    } else {
        switch_reason.push_str("; no replacement account found");
    }

    anyhow::bail!("{switch_reason}");
}

fn select_usable_account_by_policy(
    evaluations: &[AccountUsageEvaluation],
) -> Option<account_selector::AccountSelection<'_>> {
    let candidates = evaluations
        .iter()
        .filter_map(|evaluation| {
            if matches!(evaluation.decision, UsageDecision::Usable(_)) {
                Some(AccountUsageCandidate {
                    account: &evaluation.account,
                    usage: &evaluation.usage,
                })
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    account_selector::select_account(&candidates, SelectionConfig::default())
}

struct AccountUsageEvaluation {
    account: StoredAccount,
    usage: UsageInfo,
    decision: UsageDecision,
}

fn assess_usage(info: &UsageInfo) -> UsageDecision {
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

    if let Some(reason) = hard_limit_reason(info) {
        return UsageDecision::Unavailable(reason);
    }

    UsageDecision::Usable("usage is available".to_string())
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

fn hard_limit_reason(info: &UsageInfo) -> Option<String> {
    [
        ("5-hour".to_string(), info.primary_used_percent),
        ("weekly".to_string(), info.secondary_used_percent),
    ]
    .into_iter()
    .chain(info.additional_limits.iter().flat_map(additional_windows))
    .find_map(|(label, value)| {
        value.and_then(|used| {
            if used >= HARD_USAGE_LIMIT_PERCENT {
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
    use chrono::Utc;

    use super::{
        AccountUsageEvaluation, UsageDecision, assess_usage, select_usable_account_by_policy,
    };
    use crate::types::{AuthData, AuthMode, StoredAccount, UsageInfo};

    #[test]
    fn usage_is_unavailable_when_credits_are_depleted() {
        let info = UsageInfo {
            has_credits: Some(true),
            unlimited_credits: Some(false),
            credits_balance: Some("0".to_string()),
            ..usage_info()
        };

        assert_eq!(
            assess_usage(&info),
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
            assess_usage(&info),
            UsageDecision::Unavailable("usage limit reached".to_string())
        );
    }

    #[test]
    fn usage_is_unavailable_when_hard_limit_is_reached() {
        let info = UsageInfo {
            primary_used_percent: Some(100.0),
            ..usage_info()
        };

        assert_eq!(
            assess_usage(&info),
            UsageDecision::Unavailable("5-hour usage is 100.0%".to_string())
        );
    }

    #[test]
    fn usage_is_usable_below_hard_limit() {
        let info = UsageInfo {
            primary_used_percent: Some(50.0),
            secondary_used_percent: Some(80.0),
            ..usage_info()
        };

        assert_eq!(
            assess_usage(&info),
            UsageDecision::Usable("usage is available".to_string())
        );
    }

    #[test]
    fn policy_selection_prefers_deadline_aware_candidate_over_active_order() {
        let active = chatgpt_account("active");
        let soon_reset = chatgpt_account("soon-reset");
        let evaluations = vec![
            AccountUsageEvaluation {
                account: active,
                usage: usage_info_with_limits("active", 20.0, 20.0, 500, 1_000),
                decision: UsageDecision::Usable("usage is available".to_string()),
            },
            AccountUsageEvaluation {
                account: soon_reset,
                usage: usage_info_with_limits("soon-reset", 95.0, 20.0, 10, 1_000),
                decision: UsageDecision::Usable("usage is available".to_string()),
            },
        ];

        let selected = select_usable_account_by_policy(&evaluations)
            .expect("policy should select a usable account");

        assert_eq!(selected.account.id, "soon-reset");
    }

    #[test]
    fn policy_selection_ignores_hard_unavailable_candidates() {
        let active = chatgpt_account("active");
        let over_limit = chatgpt_account("over-limit");
        let evaluations = vec![
            AccountUsageEvaluation {
                account: active,
                usage: usage_info_with_limits("active", 20.0, 20.0, 500, 1_000),
                decision: UsageDecision::Usable("usage is available".to_string()),
            },
            AccountUsageEvaluation {
                account: over_limit,
                usage: usage_info_with_limits("over-limit", 100.0, 20.0, 10, 1_000),
                decision: UsageDecision::Unavailable("5-hour usage is 100.0%".to_string()),
            },
        ];

        let selected = select_usable_account_by_policy(&evaluations)
            .expect("policy should select the remaining usable account");

        assert_eq!(selected.account.id, "active");
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

    fn usage_info_with_limits(
        account_id: &str,
        five_hour_used_percent: f64,
        weekly_used_percent: f64,
        five_hour_resets_at: i64,
        weekly_resets_at: i64,
    ) -> UsageInfo {
        UsageInfo {
            account_id: account_id.to_string(),
            primary_used_percent: Some(five_hour_used_percent),
            secondary_used_percent: Some(weekly_used_percent),
            primary_resets_at: Some(five_hour_resets_at),
            secondary_resets_at: Some(weekly_resets_at),
            ..usage_info()
        }
    }

    fn chatgpt_account(id: &str) -> StoredAccount {
        StoredAccount {
            id: id.to_string(),
            name: id.to_string(),
            email: None,
            plan_type: Some("pro".to_string()),
            chatgpt_user_id: None,
            chatgpt_account_is_fedramp: false,
            token_last_refresh_at: None,
            subscription_expires_at: None,
            auth_mode: AuthMode::ChatGPT,
            auth_data: AuthData::ChatGPT {
                id_token: "id-token".into(),
                access_token: "access-token".into(),
                refresh_token: "refresh-token".into(),
                account_id: Some(id.to_string()),
            },
            created_at: Utc::now(),
            last_used_at: None,
        }
    }
}
