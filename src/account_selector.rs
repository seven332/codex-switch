use std::cmp::Ordering;

use chrono::{DateTime, Utc};

use crate::types::{AuthData, StoredAccount, UsageInfo};

pub const DEFAULT_MIN_SAFE_HEADROOM: f64 = 5.0;
pub const DEFAULT_WEEKLY_TO_FIVE_HOUR_RATIO: f64 = 4.0;

#[derive(Debug, Clone, Copy)]
pub struct SelectionConfig {
    pub min_safe_headroom: f64,
    pub weekly_to_five_hour_ratio: f64,
}

impl Default for SelectionConfig {
    fn default() -> Self {
        Self {
            min_safe_headroom: DEFAULT_MIN_SAFE_HEADROOM,
            weekly_to_five_hour_ratio: DEFAULT_WEEKLY_TO_FIVE_HOUR_RATIO,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AccountUsageCandidate<'a> {
    pub account: &'a StoredAccount,
    pub usage: &'a UsageInfo,
}

#[derive(Debug)]
pub struct AccountSelection<'a> {
    pub account: &'a StoredAccount,
    pub metrics: UsageSelectionMetrics,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UsageSelectionMetrics {
    pub five_hour_headroom: f64,
    pub weekly_headroom: f64,
    pub five_hour_headroom_units: f64,
    pub weekly_headroom_units: f64,
    pub bottleneck: UsageWindow,
    pub bottleneck_headroom: f64,
    pub bottleneck_resets_at: Option<i64>,
    pub safe_for_reset_priority: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageWindow {
    FiveHour,
    Weekly,
}

struct EvaluatedCandidate<'a> {
    account: &'a StoredAccount,
    metrics: UsageSelectionMetrics,
    order: usize,
}

pub trait AccountSelectionPolicy {
    fn select_account<'a>(
        &mut self,
        candidates: &[AccountUsageCandidate<'a>],
    ) -> Option<AccountSelection<'a>>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DeadlineAwarePolicy {
    pub config: SelectionConfig,
}

impl DeadlineAwarePolicy {
    pub fn new(config: SelectionConfig) -> Self {
        Self { config }
    }
}

impl AccountSelectionPolicy for DeadlineAwarePolicy {
    fn select_account<'a>(
        &mut self,
        candidates: &[AccountUsageCandidate<'a>],
    ) -> Option<AccountSelection<'a>> {
        evaluated_candidates(candidates, self.config)
            .into_iter()
            .min_by(compare_candidates)
            .map(|candidate| AccountSelection {
                account: candidate.account,
                metrics: candidate.metrics,
            })
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub struct DrainFirstPolicy {
    pub config: SelectionConfig,
    current_account_id: Option<String>,
}

#[cfg(test)]
impl DrainFirstPolicy {
    pub fn new(config: SelectionConfig) -> Self {
        Self {
            config,
            current_account_id: None,
        }
    }

    pub fn current_account_id(&self) -> Option<&str> {
        self.current_account_id.as_deref()
    }
}

#[cfg(test)]
impl AccountSelectionPolicy for DrainFirstPolicy {
    fn select_account<'a>(
        &mut self,
        candidates: &[AccountUsageCandidate<'a>],
    ) -> Option<AccountSelection<'a>> {
        let evaluated = evaluated_candidates(candidates, self.config);
        let selected = self
            .current_account_id
            .as_deref()
            .and_then(|account_id| {
                evaluated
                    .iter()
                    .find(|candidate| candidate.account.id == account_id)
            })
            .or_else(|| evaluated.first())?;

        self.current_account_id = Some(selected.account.id.clone());
        Some(AccountSelection {
            account: selected.account,
            metrics: selected.metrics,
        })
    }
}

pub fn select_account<'a>(
    candidates: &[AccountUsageCandidate<'a>],
    config: SelectionConfig,
) -> Option<AccountSelection<'a>> {
    DeadlineAwarePolicy::new(config).select_account(candidates)
}

pub fn usage_selection_metrics(
    usage: &UsageInfo,
    config: SelectionConfig,
) -> Option<UsageSelectionMetrics> {
    evaluate_usage(
        usage,
        normalized_min_safe_headroom(config.min_safe_headroom),
        normalized_weekly_to_five_hour_ratio(config.weekly_to_five_hour_ratio),
    )
}

fn evaluated_candidates<'a>(
    candidates: &[AccountUsageCandidate<'a>],
    config: SelectionConfig,
) -> Vec<EvaluatedCandidate<'a>> {
    let min_safe_headroom = normalized_min_safe_headroom(config.min_safe_headroom);
    let weekly_to_five_hour_ratio =
        normalized_weekly_to_five_hour_ratio(config.weekly_to_five_hour_ratio);

    candidates
        .iter()
        .enumerate()
        .filter_map(|(order, candidate)| {
            evaluate_candidate(
                candidate.account,
                candidate.usage,
                min_safe_headroom,
                weekly_to_five_hour_ratio,
            )
            .map(|metrics| EvaluatedCandidate {
                account: candidate.account,
                metrics,
                order,
            })
        })
        .collect()
}

fn evaluate_candidate(
    account: &StoredAccount,
    usage: &UsageInfo,
    min_safe_headroom: f64,
    weekly_to_five_hour_ratio: f64,
) -> Option<UsageSelectionMetrics> {
    if !matches!(account.auth_data, AuthData::ChatGPT { .. }) {
        return None;
    }
    evaluate_usage(usage, min_safe_headroom, weekly_to_five_hour_ratio)
}

fn evaluate_usage(
    usage: &UsageInfo,
    min_safe_headroom: f64,
    weekly_to_five_hour_ratio: f64,
) -> Option<UsageSelectionMetrics> {
    if usage.error.is_some() || usage.rate_limit_reached_type.is_some() {
        return None;
    }

    let five_hour_used = usage.primary_used_percent?;
    let weekly_used = usage.secondary_used_percent?;
    if !five_hour_used.is_finite() || !weekly_used.is_finite() {
        return None;
    }
    if five_hour_used >= 100.0 || weekly_used >= 100.0 {
        return None;
    }

    let five_hour_headroom = headroom_from_used_percent(five_hour_used);
    let weekly_headroom = headroom_from_used_percent(weekly_used);
    let five_hour_headroom_units = five_hour_headroom;
    let weekly_headroom_units = weekly_headroom * weekly_to_five_hour_ratio;
    let (bottleneck, bottleneck_headroom, bottleneck_resets_at) =
        if five_hour_headroom_units <= weekly_headroom_units {
            (
                UsageWindow::FiveHour,
                five_hour_headroom_units,
                usage.primary_resets_at,
            )
        } else {
            (
                UsageWindow::Weekly,
                weekly_headroom_units,
                usage.secondary_resets_at,
            )
        };

    Some(UsageSelectionMetrics {
        five_hour_headroom,
        weekly_headroom,
        five_hour_headroom_units,
        weekly_headroom_units,
        bottleneck,
        bottleneck_headroom,
        bottleneck_resets_at,
        safe_for_reset_priority: bottleneck_headroom >= min_safe_headroom,
    })
}

fn normalized_min_safe_headroom(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 100.0)
    } else {
        DEFAULT_MIN_SAFE_HEADROOM
    }
}

fn normalized_weekly_to_five_hour_ratio(value: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        DEFAULT_WEEKLY_TO_FIVE_HOUR_RATIO
    }
}

fn headroom_from_used_percent(used_percent: f64) -> f64 {
    (100.0 - used_percent).clamp(0.0, 100.0)
}

fn compare_candidates(left: &EvaluatedCandidate<'_>, right: &EvaluatedCandidate<'_>) -> Ordering {
    compare_bool_desc(
        left.metrics.safe_for_reset_priority,
        right.metrics.safe_for_reset_priority,
    )
    .then_with(|| {
        if left.metrics.safe_for_reset_priority {
            compare_optional_reset(
                left.metrics.bottleneck_resets_at,
                right.metrics.bottleneck_resets_at,
            )
            .then_with(|| compare_headroom_desc(left, right))
        } else {
            compare_headroom_desc(left, right).then_with(|| {
                compare_optional_reset(
                    left.metrics.bottleneck_resets_at,
                    right.metrics.bottleneck_resets_at,
                )
            })
        }
    })
    .then_with(|| compare_last_used(left.account.last_used_at, right.account.last_used_at))
    .then_with(|| left.order.cmp(&right.order))
}

fn compare_bool_desc(left: bool, right: bool) -> Ordering {
    right.cmp(&left)
}

fn compare_headroom_desc(
    left: &EvaluatedCandidate<'_>,
    right: &EvaluatedCandidate<'_>,
) -> Ordering {
    right
        .metrics
        .bottleneck_headroom
        .total_cmp(&left.metrics.bottleneck_headroom)
}

fn compare_optional_reset(left: Option<i64>, right: Option<i64>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn compare_last_used(left: Option<DateTime<Utc>>, right: Option<DateTime<Utc>>) -> Ordering {
    match (left, right) {
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(left), Some(right)) => left.cmp(&right),
        (None, None) => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use chrono::{TimeZone, Utc};

    use super::{
        AccountSelectionPolicy, AccountUsageCandidate, DeadlineAwarePolicy, DrainFirstPolicy,
        SelectionConfig, UsageWindow, select_account,
    };
    use crate::types::{AuthData, AuthMode, StoredAccount, UsageInfo};

    #[test]
    fn excludes_hard_unavailable_accounts() {
        let api_key = api_key_account("api-key");
        let usage_error = chatgpt_account("usage-error", None);
        let rate_limited = chatgpt_account("rate-limited", None);
        let five_hour_exhausted = chatgpt_account("five-hour-exhausted", None);
        let weekly_exhausted = chatgpt_account("weekly-exhausted", None);
        let usable = chatgpt_account("usable", None);
        let usage_error_info = UsageInfo {
            error: Some("failed".to_string()),
            ..usage_info("usage-error", 10.0, 10.0, 100, 200)
        };
        let rate_limited_info = UsageInfo {
            rate_limit_reached_type: Some("rate_limit_reached".to_string()),
            ..usage_info("rate-limited", 10.0, 10.0, 100, 200)
        };
        let five_hour_exhausted_info = usage_info("five-hour-exhausted", 100.0, 10.0, 100, 200);
        let weekly_exhausted_info = usage_info("weekly-exhausted", 10.0, 100.0, 100, 200);
        let usable_info = usage_info("usable", 30.0, 30.0, 100, 200);
        let api_key_info = usage_info("api-key", 0.0, 0.0, 100, 200);
        let candidates = [
            candidate(&api_key, &api_key_info),
            candidate(&usage_error, &usage_error_info),
            candidate(&rate_limited, &rate_limited_info),
            candidate(&five_hour_exhausted, &five_hour_exhausted_info),
            candidate(&weekly_exhausted, &weekly_exhausted_info),
            candidate(&usable, &usable_info),
        ];

        let selection = select_account(&candidates, SelectionConfig::default())
            .expect("usable account should be selected");

        assert_eq!(selection.account.id, "usable");
    }

    #[test]
    fn weekly_exhaustion_blocks_selection_even_with_high_five_hour_headroom() {
        let weekly_exhausted = chatgpt_account("weekly-exhausted", None);
        let usable = chatgpt_account("usable", None);
        let weekly_exhausted_info = usage_info("weekly-exhausted", 20.0, 100.0, 100, 200);
        let usable_info = usage_info("usable", 70.0, 70.0, 100, 200);
        let candidates = [
            candidate(&weekly_exhausted, &weekly_exhausted_info),
            candidate(&usable, &usable_info),
        ];

        let selection = select_account(&candidates, SelectionConfig::default())
            .expect("usable account should be selected");

        assert_eq!(selection.account.id, "usable");
        assert_eq!(selection.metrics.bottleneck, UsageWindow::FiveHour);
    }

    #[test]
    fn weekly_near_exhaustion_lowers_bottleneck_score() {
        let weekly_bottleneck = chatgpt_account("weekly-bottleneck", None);
        let balanced = chatgpt_account("balanced", None);
        let weekly_bottleneck_info = usage_info("weekly-bottleneck", 20.0, 98.0, 100, 200);
        let balanced_info = usage_info("balanced", 70.0, 70.0, 100, 200);
        let candidates = [
            candidate(&weekly_bottleneck, &weekly_bottleneck_info),
            candidate(&balanced, &balanced_info),
        ];

        let selection = select_account(&candidates, SelectionConfig::default())
            .expect("usable account should be selected");

        assert_eq!(selection.account.id, "balanced");
        assert_eq!(selection.metrics.bottleneck_headroom, 30.0);
    }

    #[test]
    fn weekly_headroom_is_scaled_to_five_hour_units() {
        let account = chatgpt_account("account", None);
        let info = usage_info("account", 70.0, 90.0, 100, 200);
        let candidates = [candidate(&account, &info)];

        let selection = select_account(&candidates, SelectionConfig::default())
            .expect("usable account should be selected");

        assert_eq!(selection.metrics.five_hour_headroom, 30.0);
        assert_eq!(selection.metrics.weekly_headroom, 10.0);
        assert_eq!(selection.metrics.five_hour_headroom_units, 30.0);
        assert_eq!(selection.metrics.weekly_headroom_units, 40.0);
        assert_eq!(selection.metrics.bottleneck, UsageWindow::FiveHour);
        assert_eq!(selection.metrics.bottleneck_headroom, 30.0);
    }

    #[test]
    fn all_accounts_over_soft_threshold_still_selects_least_risky_account() {
        let first = chatgpt_account("first", None);
        let second = chatgpt_account("second", None);
        let first_info = usage_info("first", 96.0, 96.0, 100, 200);
        let second_info = usage_info("second", 97.0, 97.0, 100, 200);
        let candidates = [
            candidate(&first, &first_info),
            candidate(&second, &second_info),
        ];

        let selection = select_account(&candidates, SelectionConfig::default())
            .expect("risky account should still be selected");

        assert_eq!(selection.account.id, "first");
        assert_eq!(selection.metrics.bottleneck_headroom, 4.0);
        assert!(!selection.metrics.safe_for_reset_priority);
    }

    #[test]
    fn soon_resetting_account_does_not_win_below_safe_headroom() {
        let soon = chatgpt_account("soon", None);
        let safer = chatgpt_account("safer", None);
        let soon_info = usage_info("soon", 98.0, 20.0, 10, 1_000);
        let safer_info = usage_info("safer", 80.0, 20.0, 500, 1_000);
        let candidates = [candidate(&soon, &soon_info), candidate(&safer, &safer_info)];

        let selection = select_account(&candidates, SelectionConfig::default())
            .expect("usable account should be selected");

        assert_eq!(selection.account.id, "safer");
    }

    #[test]
    fn soon_resetting_account_wins_at_safe_headroom() {
        let soon = chatgpt_account("soon", None);
        let later = chatgpt_account("later", None);
        let soon_info = usage_info("soon", 95.0, 20.0, 10, 1_000);
        let later_info = usage_info("later", 80.0, 20.0, 500, 1_000);
        let candidates = [candidate(&soon, &soon_info), candidate(&later, &later_info)];

        let selection = select_account(&candidates, SelectionConfig::default())
            .expect("usable account should be selected");

        assert_eq!(selection.account.id, "soon");
        assert_eq!(selection.metrics.bottleneck_resets_at, Some(10));
    }

    #[test]
    fn account_specific_reset_time_breaks_safe_headroom_ties() {
        let earlier = chatgpt_account("earlier", None);
        let later = chatgpt_account("later", None);
        let earlier_info = usage_info("earlier", 80.0, 20.0, 10, 1_000);
        let later_info = usage_info("later", 80.0, 20.0, 500, 1_000);
        let candidates = [
            candidate(&later, &later_info),
            candidate(&earlier, &earlier_info),
        ];

        let selection = select_account(&candidates, SelectionConfig::default())
            .expect("usable account should be selected");

        assert_eq!(selection.account.id, "earlier");
        assert_eq!(selection.metrics.bottleneck_resets_at, Some(10));
    }

    #[test]
    fn older_last_used_breaks_ties() {
        let older = chatgpt_account(
            "older",
            Some(Utc.with_ymd_and_hms(2026, 5, 10, 12, 0, 0).unwrap()),
        );
        let newer = chatgpt_account(
            "newer",
            Some(Utc.with_ymd_and_hms(2026, 5, 11, 12, 0, 0).unwrap()),
        );
        let older_info = usage_info("older", 50.0, 50.0, 100, 200);
        let newer_info = usage_info("newer", 50.0, 50.0, 100, 200);
        let candidates = [
            candidate(&newer, &newer_info),
            candidate(&older, &older_info),
        ];

        let selection = select_account(&candidates, SelectionConfig::default())
            .expect("usable account should be selected");

        assert_eq!(selection.account.id, "older");
    }

    #[test]
    fn stable_order_breaks_remaining_ties() {
        let first = chatgpt_account("first", None);
        let second = chatgpt_account("second", None);
        let first_info = usage_info("first", 50.0, 50.0, 100, 200);
        let second_info = usage_info("second", 50.0, 50.0, 100, 200);
        let candidates = [
            candidate(&first, &first_info),
            candidate(&second, &second_info),
        ];

        let selection = select_account(&candidates, SelectionConfig::default())
            .expect("usable account should be selected");

        assert_eq!(selection.account.id, "first");
    }

    #[test]
    fn simulator_finds_deadline_aware_policy_sustainable_burn_near_weekly_capacity() {
        let max_burn = max_sustainable_burn_for_policy(2, DeadlineAwarePolicy::default);
        let theoretical_weekly_max =
            2.0 * PRO_200_WEEKLY_LIMIT_CREDITS / f64::from(ACTIVE_MINUTES_PER_ROLLING_WEEK);

        assert!(
            max_burn >= 4.35,
            "selector should sustain near the 2-account weekly limit, got {max_burn:.3}"
        );
        assert!(
            max_burn <= theoretical_weekly_max + 0.01,
            "selector exceeded theoretical weekly capacity: {max_burn:.3} > {theoretical_weekly_max:.3}"
        );

        let just_over_the_limit = theoretical_weekly_max + 0.1;
        let stats =
            simulate_policy_work_week(&mut DeadlineAwarePolicy::default(), 2, just_over_the_limit);
        assert!(stats.failed_credits > 0.0);
        assert!(stats.unavailable_minutes > 0);
    }

    #[test]
    fn simulator_compares_deadline_aware_against_drain_first_baseline() {
        let deadline_aware_max = max_sustainable_burn_for_policy(2, DeadlineAwarePolicy::default);
        let drain_first_max =
            max_sustainable_burn_for_policy(
                2,
                || DrainFirstPolicy::new(SelectionConfig::default()),
            );
        let theoretical_weekly_max =
            2.0 * PRO_200_WEEKLY_LIMIT_CREDITS / f64::from(ACTIVE_MINUTES_PER_ROLLING_WEEK);

        assert!(deadline_aware_max <= theoretical_weekly_max + 0.01);
        assert!(drain_first_max <= theoretical_weekly_max + 0.01);
        assert!(
            deadline_aware_max + 0.01 >= drain_first_max,
            "deadline-aware max burn {deadline_aware_max:.3} should not be worse than drain-first {drain_first_max:.3}"
        );

        let mut deadline_aware = DeadlineAwarePolicy::default();
        let mut drain_first = DrainFirstPolicy::new(SelectionConfig::default());
        let deadline_aware_stats = simulate_policy_work_week(&mut deadline_aware, 2, 4.0);
        let drain_first_stats = simulate_policy_work_week(&mut drain_first, 2, 4.0);

        assert_eq!(deadline_aware_stats.unavailable_minutes, 0);
        assert_eq!(drain_first_stats.unavailable_minutes, 0);
        assert!(drain_first.current_account_id().is_some());
    }

    #[test]
    fn deadline_aware_reduces_unavailable_time_for_constant_burn_near_capacity() {
        let mut saw_improvement = false;

        for credits_per_minute in [4.1, 4.2, 4.3, 4.35, 4.4] {
            let mut deadline_aware = DeadlineAwarePolicy::default();
            let mut drain_first = DrainFirstPolicy::new(SelectionConfig::default());
            let deadline_aware_stats =
                simulate_policy_work_week(&mut deadline_aware, 2, credits_per_minute);
            let drain_first_stats =
                simulate_policy_work_week(&mut drain_first, 2, credits_per_minute);

            assert!(
                deadline_aware_stats.user_unavailable_minutes()
                    <= drain_first_stats.user_unavailable_minutes(),
                "deadline-aware should not be worse at {credits_per_minute:.2} credits/min: {deadline_aware_stats:?} vs {drain_first_stats:?}"
            );

            saw_improvement |= deadline_aware_stats.user_unavailable_minutes()
                < drain_first_stats.user_unavailable_minutes();
        }

        assert!(
            saw_improvement,
            "deadline-aware should improve at least one steady burn rate near capacity"
        );
    }

    #[test]
    fn simulator_staggers_initial_reset_times_across_five_hour_and_weekly_windows() {
        let first = SimAccount::new("account-0", 0, 2);
        let second = SimAccount::new("account-1", 1, 2);
        let first_usage = first.usage_info(0);
        let second_usage = second.usage_info(0);

        assert_eq!(first_usage.primary_resets_at, Some(0));
        assert_eq!(
            second_usage.primary_resets_at,
            Some(FIVE_HOUR_WINDOW_MINUTES / 2)
        );
        assert_eq!(first_usage.secondary_resets_at, Some(0));
        assert_eq!(
            second_usage.secondary_resets_at,
            Some(WEEKLY_WINDOW_MINUTES / 2)
        );
    }

    const FIVE_HOUR_WINDOW_MINUTES: i64 = 300;
    const WEEKLY_WINDOW_MINUTES: i64 = 10_080;
    const SIMULATION_WEEKS: i64 = 2;
    const SIMULATION_MINUTES: i64 = WEEKLY_WINDOW_MINUTES * SIMULATION_WEEKS;
    const PRO_200_FIVE_HOUR_LIMIT_CREDITS: f64 = 1_250.0;
    const PRO_200_WEEKLY_LIMIT_CREDITS: f64 = 5_000.0;
    const ACTIVE_MINUTES_PER_ROLLING_WEEK: i32 = 2_250;

    #[derive(Debug, Clone, Copy)]
    struct UsageEvent {
        at_minute: i64,
        credits: f64,
    }

    #[derive(Debug, Clone, Copy)]
    struct Demand {
        minute: i64,
        credits: f64,
    }

    struct SimAccount {
        account: StoredAccount,
        five_hour_events: VecDeque<UsageEvent>,
        weekly_events: VecDeque<UsageEvent>,
    }

    #[derive(Debug)]
    struct SimStats {
        served_credits: f64,
        failed_credits: f64,
        preventable_failures: u32,
        unavailable_minutes: u32,
        max_contiguous_unavailable_minutes: u32,
        account_switches: u32,
        min_five_hour_remaining: f64,
        min_weekly_remaining: f64,
    }

    impl SimStats {
        fn user_unavailable_minutes(&self) -> u32 {
            self.unavailable_minutes
        }
    }

    fn max_sustainable_burn_for_policy<P, F>(account_count: usize, mut policy_factory: F) -> f64
    where
        P: AccountSelectionPolicy,
        F: FnMut() -> P,
    {
        let mut low = 0.0;
        let mut high = 10.0;

        for _ in 0..24 {
            let mid = (low + high) / 2.0;
            let mut policy = policy_factory();
            let stats = simulate_policy_work_week(&mut policy, account_count, mid);
            if stats.user_unavailable_minutes() == 0 {
                low = mid;
            } else {
                high = mid;
            }
        }

        low
    }

    fn simulate_policy_work_week<P: AccountSelectionPolicy>(
        policy: &mut P,
        account_count: usize,
        credits_per_minute: f64,
    ) -> SimStats {
        let mut accounts = (0..account_count)
            .map(|index| SimAccount::new(&format!("account-{index}"), index, account_count))
            .collect::<Vec<_>>();
        let mut stats = SimStats {
            served_credits: 0.0,
            failed_credits: 0.0,
            preventable_failures: 0,
            unavailable_minutes: 0,
            max_contiguous_unavailable_minutes: 0,
            account_switches: 0,
            min_five_hour_remaining: f64::INFINITY,
            min_weekly_remaining: f64::INFINITY,
        };
        let mut last_account_index = None;
        let mut contiguous_unavailable_minutes = 0;

        for minute in 0..SIMULATION_MINUTES {
            for account in &mut accounts {
                account.expire(minute);
            }

            if !is_work_minute(minute) {
                continue;
            }

            serve_demand(
                policy,
                &mut accounts,
                Demand {
                    minute,
                    credits: credits_per_minute,
                },
                &mut stats,
                &mut last_account_index,
                &mut contiguous_unavailable_minutes,
            );
        }

        stats
    }

    fn serve_demand<P: AccountSelectionPolicy>(
        policy: &mut P,
        accounts: &mut [SimAccount],
        demand: Demand,
        stats: &mut SimStats,
        last_account_index: &mut Option<usize>,
        contiguous_unavailable_minutes: &mut u32,
    ) {
        let any_account_can_serve = accounts
            .iter()
            .any(|account| account.can_serve(demand.credits));
        if any_account_can_serve {
            *contiguous_unavailable_minutes = 0;
        } else {
            stats.unavailable_minutes += 1;
            *contiguous_unavailable_minutes += 1;
            stats.max_contiguous_unavailable_minutes = stats
                .max_contiguous_unavailable_minutes
                .max(*contiguous_unavailable_minutes);
        }

        let selected_index = select_sim_account(policy, accounts, demand.minute);
        let serving_index = selected_index
            .filter(|index| accounts[*index].can_serve(demand.credits))
            .or_else(|| {
                let fallback_index = accounts
                    .iter()
                    .position(|account| account.can_serve(demand.credits));
                if fallback_index.is_some() && selected_index != fallback_index {
                    stats.preventable_failures += 1;
                }
                fallback_index
            });

        if let Some(index) = serving_index {
            accounts[index].consume(demand.minute, demand.credits);
            stats.served_credits += demand.credits;
            if last_account_index.is_some_and(|previous| previous != index) {
                stats.account_switches += 1;
            }
            *last_account_index = Some(index);
        } else {
            stats.failed_credits += demand.credits;
        }

        for account in accounts {
            stats.min_five_hour_remaining = stats
                .min_five_hour_remaining
                .min(account.five_hour_remaining());
            stats.min_weekly_remaining = stats.min_weekly_remaining.min(account.weekly_remaining());
        }
    }

    fn select_sim_account<P: AccountSelectionPolicy>(
        policy: &mut P,
        accounts: &[SimAccount],
        minute: i64,
    ) -> Option<usize> {
        let usages = accounts
            .iter()
            .map(|account| account.usage_info(minute))
            .collect::<Vec<_>>();
        let candidates = accounts
            .iter()
            .zip(usages.iter())
            .map(|(account, usage)| candidate(&account.account, usage))
            .collect::<Vec<_>>();

        policy.select_account(&candidates).and_then(|selection| {
            accounts
                .iter()
                .position(|account| account.account.id == selection.account.id)
        })
    }

    fn is_work_minute(minute: i64) -> bool {
        let day_of_week = (minute / 1_440) % 7;
        if day_of_week >= 5 {
            return false;
        }

        let minute_of_day = minute % 1_440;
        (570..720).contains(&minute_of_day) || (810..1_110).contains(&minute_of_day)
    }

    impl SimAccount {
        fn new(id: &str, index: usize, account_count: usize) -> Self {
            let five_hour_seed =
                staggered_seed_event(index, account_count, FIVE_HOUR_WINDOW_MINUTES);
            let weekly_seed = staggered_seed_event(index, account_count, WEEKLY_WINDOW_MINUTES);

            Self {
                account: chatgpt_account(id, None),
                five_hour_events: VecDeque::from([five_hour_seed]),
                weekly_events: VecDeque::from([weekly_seed]),
            }
        }

        fn expire(&mut self, minute: i64) {
            expire_events(&mut self.five_hour_events, minute, FIVE_HOUR_WINDOW_MINUTES);
            expire_events(&mut self.weekly_events, minute, WEEKLY_WINDOW_MINUTES);
        }

        fn consume(&mut self, minute: i64, credits: f64) {
            let event = UsageEvent {
                at_minute: minute,
                credits,
            };
            self.five_hour_events.push_back(event);
            self.weekly_events.push_back(event);
        }

        fn can_serve(&self, credits: f64) -> bool {
            self.five_hour_remaining() + f64::EPSILON >= credits
                && self.weekly_remaining() + f64::EPSILON >= credits
        }

        fn usage_info(&self, minute: i64) -> UsageInfo {
            let five_hour_used = self.five_hour_used();
            let weekly_used = self.weekly_used();
            usage_info(
                &self.account.id,
                used_percent(five_hour_used, PRO_200_FIVE_HOUR_LIMIT_CREDITS),
                used_percent(weekly_used, PRO_200_WEEKLY_LIMIT_CREDITS),
                reset_at(&self.five_hour_events, minute, FIVE_HOUR_WINDOW_MINUTES),
                reset_at(&self.weekly_events, minute, WEEKLY_WINDOW_MINUTES),
            )
        }

        fn five_hour_used(&self) -> f64 {
            total_credits(&self.five_hour_events)
        }

        fn weekly_used(&self) -> f64 {
            total_credits(&self.weekly_events)
        }

        fn five_hour_remaining(&self) -> f64 {
            (PRO_200_FIVE_HOUR_LIMIT_CREDITS - self.five_hour_used()).max(0.0)
        }

        fn weekly_remaining(&self) -> f64 {
            (PRO_200_WEEKLY_LIMIT_CREDITS - self.weekly_used()).max(0.0)
        }
    }

    fn expire_events(events: &mut VecDeque<UsageEvent>, minute: i64, window_minutes: i64) {
        while events
            .front()
            .is_some_and(|event| event.at_minute + window_minutes <= minute)
        {
            events.pop_front();
        }
    }

    fn staggered_seed_event(index: usize, account_count: usize, window_minutes: i64) -> UsageEvent {
        let offset = staggered_reset_offset(index, account_count, window_minutes);
        UsageEvent {
            at_minute: offset - window_minutes,
            credits: 0.0,
        }
    }

    fn staggered_reset_offset(index: usize, account_count: usize, window_minutes: i64) -> i64 {
        if account_count == 0 {
            return 0;
        }

        window_minutes * i64::try_from(index).expect("account index should fit in i64")
            / i64::try_from(account_count).expect("account count should fit in i64")
    }

    fn reset_at(events: &VecDeque<UsageEvent>, minute: i64, window_minutes: i64) -> i64 {
        events
            .front()
            .map_or(minute, |event| event.at_minute + window_minutes)
    }

    fn total_credits(events: &VecDeque<UsageEvent>) -> f64 {
        events.iter().map(|event| event.credits).sum()
    }

    fn used_percent(used: f64, limit: f64) -> f64 {
        (used / limit * 100.0).clamp(0.0, 100.0)
    }

    fn candidate<'a>(
        account: &'a StoredAccount,
        usage: &'a UsageInfo,
    ) -> AccountUsageCandidate<'a> {
        AccountUsageCandidate { account, usage }
    }

    fn chatgpt_account(id: &str, last_used_at: Option<chrono::DateTime<Utc>>) -> StoredAccount {
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
                id_token: "id-token".to_string(),
                access_token: "access-token".to_string(),
                refresh_token: "refresh-token".to_string(),
                account_id: Some(id.to_string()),
            },
            created_at: Utc::now(),
            last_used_at,
        }
    }

    fn api_key_account(id: &str) -> StoredAccount {
        StoredAccount {
            id: id.to_string(),
            name: id.to_string(),
            email: None,
            plan_type: None,
            chatgpt_user_id: None,
            chatgpt_account_is_fedramp: false,
            token_last_refresh_at: None,
            subscription_expires_at: None,
            auth_mode: AuthMode::ApiKey,
            auth_data: AuthData::ApiKey {
                key: "api-key".to_string(),
            },
            created_at: Utc::now(),
            last_used_at: None,
        }
    }

    fn usage_info(
        account_id: &str,
        five_hour_used_percent: f64,
        weekly_used_percent: f64,
        five_hour_resets_at: i64,
        weekly_resets_at: i64,
    ) -> UsageInfo {
        UsageInfo {
            account_id: account_id.to_string(),
            limit_id: Some("codex".to_string()),
            limit_name: None,
            plan_type: Some("pro".to_string()),
            primary_used_percent: Some(five_hour_used_percent),
            primary_window_minutes: Some(300),
            primary_resets_at: Some(five_hour_resets_at),
            secondary_used_percent: Some(weekly_used_percent),
            secondary_window_minutes: Some(10_080),
            secondary_resets_at: Some(weekly_resets_at),
            has_credits: None,
            unlimited_credits: None,
            credits_balance: None,
            rate_limit_reached_type: None,
            additional_limits: Vec::new(),
            error: None,
        }
    }
}
