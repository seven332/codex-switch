use anyhow::Result;

use crate::account_selector::{self, AccountUsageCandidate, SelectionConfig, SelectionContext};
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

#[derive(Debug)]
pub enum AutoSwitchPlan {
    CurrentKept {
        account: Box<StoredAccount>,
        reason: String,
    },
    CurrentUnsupported {
        account: Box<StoredAccount>,
        reason: String,
    },
    Switch {
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
    let plan = plan_auto_switch(true).await?;
    commit_auto_switch_plan(plan, false).await
}

pub async fn auto_switch_allow_running() -> Result<AutoSwitchResult> {
    let plan = plan_auto_switch(true).await?;
    commit_auto_switch_plan(plan, true).await
}

pub async fn plan_auto_switch_for_runtime() -> Result<AutoSwitchPlan> {
    plan_auto_switch(false).await
}

pub async fn commit_auto_switch_plan(
    plan: AutoSwitchPlan,
    allow_running: bool,
) -> Result<AutoSwitchResult> {
    match plan {
        AutoSwitchPlan::CurrentKept { account, reason } => {
            Ok(AutoSwitchResult::CurrentKept { account, reason })
        }
        AutoSwitchPlan::CurrentUnsupported { account, reason } => {
            Ok(AutoSwitchResult::CurrentUnsupported { account, reason })
        }
        AutoSwitchPlan::Switch { from, to, reason } => {
            let switched = if allow_running {
                switcher::switch_to_account_unchecked(&to.id).await?
            } else {
                switcher::switch_to_account(&to.id).await?
            };
            Ok(AutoSwitchResult::Switched {
                from,
                to: Box::new(switched),
                reason,
            })
        }
    }
}

pub fn usage_requires_switch(info: &UsageInfo) -> bool {
    matches!(assess_usage(info), UsageDecision::Unavailable(_))
}

async fn plan_auto_switch(write_current_auth_on_refresh: bool) -> Result<AutoSwitchPlan> {
    let store = store::load_accounts()?;
    if store.accounts.is_empty() {
        anyhow::bail!("No accounts stored.");
    }

    let current = auth_json::current_stored_account_best_effort(&store);
    let current_account_id = current.as_ref().map(|account| account.id.clone());
    let current_id = current_account_id.as_deref();
    let current_index =
        current_id.and_then(|id| store.accounts.iter().position(|account| account.id == id));
    let mut evaluations = empty_evaluation_slots(store.accounts.len());
    let mut current_decision = None;
    let mut skipped = Vec::new();

    if let Some(index) = current_index {
        let account = &store.accounts[index];
        let info = match get_account_usage_for_plan(account, write_current_auth_on_refresh).await {
            Ok(info) => info,
            Err(err) => {
                anyhow::bail!("Failed to get usage for {}: {err}", account.name);
            }
        };

        match apply_current_usage_result(account, index, info, &mut evaluations)? {
            CurrentUsageOutcome::Terminal(result) => return Ok(result),
            CurrentUsageOutcome::Continue(decision) => current_decision = Some(decision),
        }
    }

    let replacement_results = collect_replacement_account_usage_for_plan(
        &store.accounts,
        current_id,
        write_current_auth_on_refresh,
    )
    .await;
    apply_replacement_usage_results(
        &store.accounts,
        replacement_results,
        &mut evaluations,
        &mut skipped,
    );
    let evaluations = ordered_evaluations(evaluations);

    if let Some(selection) = select_usable_account_by_policy(&evaluations, current_id) {
        let selected_account = selection.account;
        if Some(selected_account.id.as_str()) == current_id {
            let reason = match current_decision {
                Some(UsageDecision::Usable(reason)) => reason,
                _ => "usage policy kept current account".to_string(),
            };
            return Ok(AutoSwitchPlan::CurrentKept {
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
        return Ok(AutoSwitchPlan::Switch {
            from: current.map(Box::new),
            to: Box::new(selected_account.clone()),
            reason: switch_reason,
        });
    }

    if let (Some(account), Some(UsageDecision::Usable(reason))) =
        (&current, current_decision.clone())
    {
        return Ok(AutoSwitchPlan::CurrentKept {
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

async fn get_account_usage_for_plan(
    account: &StoredAccount,
    write_current_auth_on_refresh: bool,
) -> Result<UsageInfo> {
    if write_current_auth_on_refresh {
        usage::get_account_usage(account).await
    } else {
        usage::get_account_usage_without_auth_write(account).await
    }
}

async fn collect_replacement_account_usage_for_plan(
    accounts: &[StoredAccount],
    current_id: Option<&str>,
    write_current_auth_on_refresh: bool,
) -> Vec<usage::AccountUsageFetch> {
    if write_current_auth_on_refresh {
        usage::collect_replacement_account_usage(accounts, current_id).await
    } else {
        usage::collect_replacement_account_usage_without_auth_write(accounts, current_id).await
    }
}

#[derive(Debug)]
enum CurrentUsageOutcome {
    Terminal(AutoSwitchPlan),
    Continue(UsageDecision),
}

fn apply_current_usage_result(
    account: &StoredAccount,
    index: usize,
    info: UsageInfo,
    evaluations: &mut [Option<AccountUsageEvaluation>],
) -> Result<CurrentUsageOutcome> {
    let decision = assess_usage(&info);
    match &decision {
        UsageDecision::Unsupported(reason) => {
            return Ok(CurrentUsageOutcome::Terminal(
                AutoSwitchPlan::CurrentUnsupported {
                    account: Box::new(account.clone()),
                    reason: reason.clone(),
                },
            ));
        }
        UsageDecision::Error(reason) => {
            anyhow::bail!(
                "Could not evaluate current account {}: {reason}",
                account.name
            );
        }
        UsageDecision::Usable(_) | UsageDecision::Unavailable(_) => {}
    }

    let Some(slot) = evaluations.get_mut(index) else {
        anyhow::bail!("Current account index is out of range");
    };
    *slot = Some(AccountUsageEvaluation {
        account: account.clone(),
        usage: info,
        decision: decision.clone(),
    });
    Ok(CurrentUsageOutcome::Continue(decision))
}

fn empty_evaluation_slots(len: usize) -> Vec<Option<AccountUsageEvaluation>> {
    (0..len).map(|_| None).collect()
}

fn ordered_evaluations(
    evaluations: Vec<Option<AccountUsageEvaluation>>,
) -> Vec<AccountUsageEvaluation> {
    evaluations.into_iter().flatten().collect()
}

fn apply_replacement_usage_results(
    accounts: &[StoredAccount],
    results: Vec<usage::AccountUsageFetch>,
    evaluations: &mut [Option<AccountUsageEvaluation>],
    skipped: &mut Vec<String>,
) {
    for result in results {
        let Some(account) = accounts.get(result.index) else {
            skipped.push(format!("{}: account not found", result.account_id));
            continue;
        };

        let info = match result.result {
            Ok(info) => info,
            Err(err) => {
                skipped.push(format!("{}: {err}", account.name));
                continue;
            }
        };

        let decision = assess_usage(&info);
        if let UsageDecision::Unavailable(reason)
        | UsageDecision::Unsupported(reason)
        | UsageDecision::Error(reason) = &decision
        {
            skipped.push(format!("{}: {reason}", account.name));
        }

        if let Some(slot) = evaluations.get_mut(result.index) {
            *slot = Some(AccountUsageEvaluation {
                account: account.clone(),
                usage: info,
                decision,
            });
        } else {
            skipped.push(format!("{}: account not found", account.name));
        }
    }
}

fn select_usable_account_by_policy<'a>(
    evaluations: &'a [AccountUsageEvaluation],
    current_id: Option<&str>,
) -> Option<account_selector::AccountSelection<'a>> {
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

    match current_id {
        Some(_) => account_selector::select_account_with_context(
            &candidates,
            SelectionConfig::default(),
            SelectionContext::now().with_current_account_id(current_id),
        ),
        None => account_selector::select_account(&candidates, SelectionConfig::default()),
    }
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
        AccountUsageEvaluation, AutoSwitchPlan, CurrentUsageOutcome, UsageDecision,
        apply_current_usage_result, apply_replacement_usage_results, assess_usage,
        empty_evaluation_slots, ordered_evaluations, select_usable_account_by_policy,
    };
    use crate::types::{AuthData, AuthMode, StoredAccount, UsageInfo};
    use crate::usage::AccountUsageFetch;

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

        let selected = select_usable_account_by_policy(&evaluations, Some("active"))
            .expect("policy should select a usable account");

        assert_eq!(selected.account.id, "soon-reset");
    }

    #[test]
    fn policy_selection_ignores_missing_current_account_context() {
        let first = chatgpt_account("first");
        let second = chatgpt_account("second");
        let evaluations = vec![
            AccountUsageEvaluation {
                account: first,
                usage: usage_info_with_limits("first", 50.0, 50.0, 500, 1_000),
                decision: UsageDecision::Usable("usage is available".to_string()),
            },
            AccountUsageEvaluation {
                account: second,
                usage: usage_info_with_limits("second", 50.0, 50.0, 500, 1_000),
                decision: UsageDecision::Usable("usage is available".to_string()),
            },
        ];

        let selected = select_usable_account_by_policy(&evaluations, Some("missing"))
            .expect("policy should select a usable account");

        assert_eq!(selected.account.id, "first");
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

        let selected = select_usable_account_by_policy(&evaluations, Some("active"))
            .expect("policy should select the remaining usable account");

        assert_eq!(selected.account.id, "active");
    }

    #[test]
    fn current_unsupported_usage_short_circuits_without_recording_candidate() {
        let current = chatgpt_account("current");
        let mut evaluations = empty_evaluation_slots(1);

        let outcome = apply_current_usage_result(
            &current,
            0,
            UsageInfo::unsupported(current.id.clone()),
            &mut evaluations,
        )
        .expect("current unsupported usage should be handled");

        match outcome {
            CurrentUsageOutcome::Terminal(AutoSwitchPlan::CurrentUnsupported {
                account,
                reason,
            }) => {
                assert_eq!(account.id, "current");
                assert_eq!(reason, "usage unsupported");
            }
            _ => panic!("current unsupported usage should short-circuit"),
        }
        assert!(evaluations[0].is_none());
    }

    #[test]
    fn current_usage_error_is_fatal() {
        let current = chatgpt_account("current");
        let mut evaluations = empty_evaluation_slots(1);

        let err = apply_current_usage_result(
            &current,
            0,
            UsageInfo::error(current.id.clone(), "parse failed".to_string()),
            &mut evaluations,
        )
        .expect_err("current usage error should be fatal");

        assert_eq!(
            err.to_string(),
            "Could not evaluate current account current: parse failed"
        );
        assert!(evaluations[0].is_none());
    }

    #[test]
    fn current_usable_usage_is_recorded_for_policy() {
        let current = chatgpt_account("current");
        let mut evaluations = empty_evaluation_slots(1);

        let outcome = apply_current_usage_result(
            &current,
            0,
            usage_info_with_limits("current", 20.0, 20.0, 500, 1_000),
            &mut evaluations,
        )
        .expect("current usable usage should continue");

        match outcome {
            CurrentUsageOutcome::Continue(UsageDecision::Usable(reason)) => {
                assert_eq!(reason, "usage is available");
            }
            _ => panic!("current usable usage should continue"),
        }
        let evaluation = evaluations[0]
            .as_ref()
            .expect("current evaluation should be stored");
        assert_eq!(evaluation.account.id, "current");
        assert!(matches!(evaluation.decision, UsageDecision::Usable(_)));
    }

    #[test]
    fn replacement_usage_results_preserve_account_order() {
        let first = chatgpt_account("first");
        let second = chatgpt_account("second");
        let accounts = vec![first, second];
        let mut evaluations = empty_evaluation_slots(accounts.len());
        let mut skipped = Vec::new();
        let results = vec![
            usage_fetch(
                1,
                "second",
                Ok(usage_info_with_limits("second", 20.0, 20.0, 500, 1_000)),
            ),
            usage_fetch(
                0,
                "first",
                Ok(usage_info_with_limits("first", 20.0, 20.0, 500, 1_000)),
            ),
        ];

        apply_replacement_usage_results(&accounts, results, &mut evaluations, &mut skipped);

        assert!(skipped.is_empty());
        assert_eq!(
            ordered_evaluations(evaluations)
                .iter()
                .map(|evaluation| evaluation.account.id.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
    }

    #[test]
    fn replacement_fetch_errors_are_skipped_without_hiding_usable_candidates() {
        let failed = chatgpt_account("failed");
        let usable = chatgpt_account("usable");
        let accounts = vec![failed, usable];
        let mut evaluations = empty_evaluation_slots(accounts.len());
        let mut skipped = Vec::new();
        let results = vec![
            usage_fetch(0, "failed", Err("network failed".to_string())),
            usage_fetch(
                1,
                "usable",
                Ok(usage_info_with_limits("usable", 20.0, 20.0, 500, 1_000)),
            ),
        ];

        apply_replacement_usage_results(&accounts, results, &mut evaluations, &mut skipped);
        let evaluations = ordered_evaluations(evaluations);
        let selected = select_usable_account_by_policy(&evaluations, None)
            .expect("policy should select the successful replacement");

        assert_eq!(skipped, vec!["failed: network failed"]);
        assert_eq!(selected.account.id, "usable");
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

    fn usage_fetch(
        index: usize,
        account_id: &str,
        result: std::result::Result<UsageInfo, String>,
    ) -> AccountUsageFetch {
        AccountUsageFetch {
            index,
            account_id: account_id.to_string(),
            result,
        }
    }
}
