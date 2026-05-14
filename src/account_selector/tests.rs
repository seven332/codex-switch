use std::{
    cmp::Ordering,
    collections::{HashMap, VecDeque},
};

use chrono::{TimeZone, Utc};

use super::{
    AccountSelection, AccountSelectionPolicy, AccountUsageCandidate, DeadlineAwarePolicy,
    ResetWeightedMinimaxPolicy, SelectionConfig, SelectionContext, SelectionPolicyKind,
    ShadowPricePolicy, UsageWindow, compare_headroom_desc, compare_last_used,
    compare_optional_reset, evaluated_candidates, select_account,
};
use crate::types::{AuthData, AuthMode, StoredAccount, UsageInfo};

#[derive(Debug, Clone, Default)]
struct DrainFirstPolicy {
    config: SelectionConfig,
    current_account_id: Option<String>,
}

impl DrainFirstPolicy {
    fn new(config: SelectionConfig) -> Self {
        Self {
            config,
            current_account_id: None,
        }
    }

    fn current_account_id(&self) -> Option<&str> {
        self.current_account_id.as_deref()
    }
}

impl AccountSelectionPolicy for DrainFirstPolicy {
    fn select_account_at<'a>(
        &mut self,
        candidates: &[AccountUsageCandidate<'a>],
        _context: SelectionContext,
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

#[derive(Debug, Clone, Copy, Default)]
struct MaxHeadroomPolicy {
    config: SelectionConfig,
}

impl MaxHeadroomPolicy {
    fn new(config: SelectionConfig) -> Self {
        Self { config }
    }
}

impl AccountSelectionPolicy for MaxHeadroomPolicy {
    fn select_account_at<'a>(
        &mut self,
        candidates: &[AccountUsageCandidate<'a>],
        _context: SelectionContext,
    ) -> Option<AccountSelection<'a>> {
        evaluated_candidates(candidates, self.config)
            .into_iter()
            .min_by(|left, right| {
                compare_headroom_desc(left, right)
                    .then_with(|| {
                        compare_optional_reset(
                            left.metrics.bottleneck_resets_at,
                            right.metrics.bottleneck_resets_at,
                        )
                    })
                    .then_with(|| {
                        compare_last_used(left.account.last_used_at, right.account.last_used_at)
                    })
                    .then_with(|| left.order.cmp(&right.order))
            })
            .map(|candidate| AccountSelection {
                account: candidate.account,
                metrics: candidate.metrics,
            })
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct ResetFirstPolicy {
    config: SelectionConfig,
}

impl ResetFirstPolicy {
    fn new(config: SelectionConfig) -> Self {
        Self { config }
    }
}

impl AccountSelectionPolicy for ResetFirstPolicy {
    fn select_account_at<'a>(
        &mut self,
        candidates: &[AccountUsageCandidate<'a>],
        _context: SelectionContext,
    ) -> Option<AccountSelection<'a>> {
        evaluated_candidates(candidates, self.config)
            .into_iter()
            .min_by(|left, right| {
                compare_optional_reset(
                    left.metrics.bottleneck_resets_at,
                    right.metrics.bottleneck_resets_at,
                )
                .then_with(|| compare_headroom_desc(left, right))
                .then_with(|| {
                    compare_last_used(left.account.last_used_at, right.account.last_used_at)
                })
                .then_with(|| left.order.cmp(&right.order))
            })
            .map(|candidate| AccountSelection {
                account: candidate.account,
                metrics: candidate.metrics,
            })
    }
}

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
fn shadow_price_prefers_less_used_when_reset_times_match() {
    let lower_pressure = chatgpt_account("lower-pressure", None);
    let higher_pressure = chatgpt_account("higher-pressure", None);
    let lower_pressure_info = usage_info("lower-pressure", 20.0, 20.0, 18_000, 604_800);
    let higher_pressure_info = usage_info("higher-pressure", 60.0, 60.0, 18_000, 604_800);
    let candidates = [
        candidate(&higher_pressure, &higher_pressure_info),
        candidate(&lower_pressure, &lower_pressure_info),
    ];

    let selection = ShadowPricePolicy::default()
        .select_account_at(&candidates, SelectionContext::at(0))
        .expect("usable account should be selected");

    assert_eq!(selection.account.id, "lower-pressure");
}

#[test]
fn shadow_price_discounts_used_quota_that_resets_soon() {
    let soon = chatgpt_account("soon", None);
    let later = chatgpt_account("later", None);
    let soon_info = usage_info("soon", 75.0, 10.0, 60, 604_800);
    let later_info = usage_info("later", 40.0, 10.0, 18_000, 604_800);
    let candidates = [candidate(&later, &later_info), candidate(&soon, &soon_info)];

    let selection = ShadowPricePolicy::default()
        .select_account_at(&candidates, SelectionContext::at(0))
        .expect("usable account should be selected");

    assert_eq!(selection.account.id, "soon");
}

#[test]
fn shadow_price_prices_weekly_pressure_independently() {
    let weekly_pressure = chatgpt_account("weekly-pressure", None);
    let five_hour_pressure = chatgpt_account("five-hour-pressure", None);
    let weekly_pressure_info = usage_info("weekly-pressure", 10.0, 95.0, 18_000, 604_800);
    let five_hour_pressure_info = usage_info("five-hour-pressure", 65.0, 20.0, 18_000, 604_800);
    let candidates = [
        candidate(&weekly_pressure, &weekly_pressure_info),
        candidate(&five_hour_pressure, &five_hour_pressure_info),
    ];

    let selection = ShadowPricePolicy::default()
        .select_account_at(&candidates, SelectionContext::at(0))
        .expect("usable account should be selected");

    assert_eq!(selection.account.id, "five-hour-pressure");
}

#[test]
fn shadow_price_treats_missing_reset_as_conservative() {
    let unknown_reset = chatgpt_account("unknown-reset", None);
    let soon_reset = chatgpt_account("soon-reset", None);
    let mut unknown_reset_info = usage_info("unknown-reset", 50.0, 10.0, 60, 604_800);
    unknown_reset_info.primary_resets_at = None;
    let soon_reset_info = usage_info("soon-reset", 50.0, 10.0, 60, 604_800);
    let candidates = [
        candidate(&unknown_reset, &unknown_reset_info),
        candidate(&soon_reset, &soon_reset_info),
    ];

    let selection = ShadowPricePolicy::default()
        .select_account_at(&candidates, SelectionContext::at(0))
        .expect("usable account should be selected");

    assert_eq!(selection.account.id, "soon-reset");
}

#[test]
fn shadow_price_keeps_nonzero_pressure_for_imminent_resets() {
    let near_exhausted = chatgpt_account("near-exhausted", None);
    let safer = chatgpt_account("safer", None);
    let near_exhausted_info = usage_info("near-exhausted", 99.0, 10.0, 0, 604_800);
    let safer_info = usage_info("safer", 50.0, 10.0, 18_000, 604_800);
    let candidates = [
        candidate(&near_exhausted, &near_exhausted_info),
        candidate(&safer, &safer_info),
    ];

    let selection = ShadowPricePolicy::default()
        .select_account_at(&candidates, SelectionContext::at(0))
        .expect("usable account should be selected");

    assert_eq!(selection.account.id, "safer");
}

#[test]
fn selection_config_can_select_shadow_price_policy() {
    let soon = chatgpt_account("soon", None);
    let later = chatgpt_account("later", None);
    let now = Utc::now().timestamp();
    let soon_info = usage_info("soon", 75.0, 10.0, now + 60, now + 604_800);
    let later_info = usage_info("later", 40.0, 10.0, now + 18_000, now + 604_800);
    let candidates = [candidate(&later, &later_info), candidate(&soon, &soon_info)];
    let config = SelectionConfig {
        policy: SelectionPolicyKind::ShadowPrice,
        ..SelectionConfig::default()
    };

    let selection = select_account(&candidates, config).expect("usable account should be selected");

    assert_eq!(selection.account.id, "soon");
}

#[test]
fn reset_weighted_minimax_prefers_earlier_reset_when_risk_matches() {
    let soon = chatgpt_account("soon", None);
    let later = chatgpt_account("later", None);
    let soon_info = usage_info("soon", 50.0, 50.0, 60, 604_800);
    let later_info = usage_info("later", 50.0, 50.0, 18_000, 604_800);
    let candidates = [candidate(&later, &later_info), candidate(&soon, &soon_info)];

    let selection = ResetWeightedMinimaxPolicy::default()
        .select_account_at(&candidates, SelectionContext::at(0))
        .expect("usable account should be selected");

    assert_eq!(selection.account.id, "soon");
}

#[test]
fn reset_weighted_minimax_protects_headroom_below_safe_threshold() {
    let soon = chatgpt_account("soon", None);
    let safer = chatgpt_account("safer", None);
    let soon_info = usage_info("soon", 99.0, 10.0, 60, 604_800);
    let safer_info = usage_info("safer", 96.0, 10.0, 18_000, 604_800);
    let candidates = [candidate(&soon, &soon_info), candidate(&safer, &safer_info)];

    let selection = ResetWeightedMinimaxPolicy::default()
        .select_account_at(&candidates, SelectionContext::at(0))
        .expect("usable account should be selected");

    assert_eq!(selection.account.id, "safer");
}

#[test]
fn selection_config_can_select_reset_weighted_minimax_policy() {
    let soon = chatgpt_account("soon", None);
    let later = chatgpt_account("later", None);
    let now = Utc::now().timestamp();
    let soon_info = usage_info("soon", 50.0, 50.0, now + 60, now + 604_800);
    let later_info = usage_info("later", 50.0, 50.0, now + 18_000, now + 604_800);
    let candidates = [candidate(&later, &later_info), candidate(&soon, &soon_info)];
    let config = SelectionConfig {
        policy: SelectionPolicyKind::ResetWeightedMinimax,
        ..SelectionConfig::default()
    };

    let selection = select_account(&candidates, config).expect("usable account should be selected");

    assert_eq!(selection.account.id, "soon");
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
        max_sustainable_burn_for_policy(2, || DrainFirstPolicy::new(SelectionConfig::default()));
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
        let drain_first_stats = simulate_policy_work_week(&mut drain_first, 2, credits_per_minute);

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
fn simulator_compares_baseline_policies_across_demand_scenarios() {
    for scenario in evaluation_scenarios() {
        let policy_stats = policy_stats_for_scenario(scenario);
        let deadline_aware = policy_stats
            .iter()
            .find(|stats| stats.policy_name == "deadline-aware")
            .expect("deadline-aware stats should be present");
        let drain_first = policy_stats
            .iter()
            .find(|stats| stats.policy_name == "drain-first")
            .expect("drain-first stats should be present");
        let shadow_price = policy_stats
            .iter()
            .find(|stats| stats.policy_name == "shadow-price")
            .expect("shadow-price stats should be present");
        let reset_weighted_minimax = policy_stats
            .iter()
            .find(|stats| stats.policy_name == "reset-weighted-minimax")
            .expect("reset-weighted-minimax stats should be present");

        for stats in &policy_stats {
            assert!(
                stats.stats.total_demand_credits() > 0.0,
                "{} should generate demand for {}",
                scenario.name,
                stats.policy_name
            );
            assert!(
                stats.stats.min_five_hour_remaining.is_finite(),
                "{} should track 5-hour remaining for {}",
                scenario.name,
                stats.policy_name
            );
            assert!(
                stats.stats.min_weekly_remaining.is_finite(),
                "{} should track weekly remaining for {}",
                scenario.name,
                stats.policy_name
            );
        }

        assert!(
            deadline_aware.stats.user_unavailable_minutes()
                <= drain_first.stats.user_unavailable_minutes(),
            "deadline-aware should not increase unavailable time vs drain-first in {}: {policy_stats:?}",
            scenario.name
        );
        assert!(
            shadow_price.stats.user_unavailable_minutes()
                <= deadline_aware.stats.user_unavailable_minutes(),
            "shadow-price should not increase unavailable time vs deadline-aware in {}: {policy_stats:?}",
            scenario.name
        );
        assert!(
            reset_weighted_minimax.stats.user_unavailable_minutes()
                <= shadow_price.stats.user_unavailable_minutes(),
            "reset-weighted-minimax should not increase unavailable time vs shadow-price in {}: {policy_stats:?}",
            scenario.name
        );
    }
}

#[test]
fn simulator_includes_bursty_and_weekly_skewed_scenarios() {
    let bursty = SimScenario::new(
        "bursty-short-sessions",
        2,
        SimDemand::SessionBursts {
            base_credits_per_minute: 1.5,
            burst_credits_per_minute: 8.0,
            burst_minutes: 20,
        },
        InitialUsage::Empty,
    );
    let weekly_skewed = SimScenario::new(
        "weekly-near-exhaustion-mixed",
        2,
        SimDemand::Constant(2.0),
        InitialUsage::WeeklyNearExhaustionMixed,
    );

    let mut bursty_policy = DeadlineAwarePolicy::default();
    let bursty_stats = simulate_policy_scenario(&mut bursty_policy, bursty);
    let mut weekly_policy = DeadlineAwarePolicy::default();
    let weekly_stats = simulate_policy_scenario(&mut weekly_policy, weekly_skewed);

    assert!(bursty_stats.account_switches > 0);
    assert!(bursty_stats.served_credits > 0.0);
    assert!(weekly_stats.min_weekly_remaining < PRO_200_WEEKLY_LIMIT_CREDITS * 0.1);
    assert_eq!(weekly_stats.user_unavailable_minutes(), 0);
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
        Some((FIVE_HOUR_WINDOW_MINUTES / 2) * 60)
    );
    assert_eq!(first_usage.secondary_resets_at, Some(0));
    assert_eq!(
        second_usage.secondary_resets_at,
        Some((WEEKLY_WINDOW_MINUTES / 2) * 60)
    );
}

#[test]
fn offline_oracle_does_not_drop_serviceable_demand_for_future_capacity() {
    let limits = TraceLimits {
        account_count: 1,
        five_hour_limit: 10,
        weekly_limit: 100,
        five_hour_window: 5,
        weekly_window: 100,
    };
    let trace = [
        TraceDemand {
            minute: 0,
            credits: 7,
        },
        TraceDemand {
            minute: 1,
            credits: 4,
        },
        TraceDemand {
            minute: 6,
            credits: 7,
        },
    ];

    let outcome = offline_oracle(&trace, limits);

    assert_eq!(outcome.user_unavailable_minutes(), 1);
    assert_eq!(outcome.served_credits, 14);
    assert_eq!(outcome.failed_credits, 4);
}

#[test]
fn offline_oracle_bounds_existing_online_policies_on_small_trace() {
    let limits = TraceLimits {
        account_count: 2,
        five_hour_limit: 10,
        weekly_limit: 100,
        five_hour_window: 5,
        weekly_window: 100,
    };
    let trace = [
        TraceDemand {
            minute: 0,
            credits: 7,
        },
        TraceDemand {
            minute: 1,
            credits: 4,
        },
        TraceDemand {
            minute: 2,
            credits: 7,
        },
        TraceDemand {
            minute: 6,
            credits: 7,
        },
        TraceDemand {
            minute: 7,
            credits: 4,
        },
        TraceDemand {
            minute: 8,
            credits: 7,
        },
    ];

    let oracle = offline_oracle(&trace, limits);
    let policy_outcomes = [
        (
            "deadline-aware",
            simulate_policy_trace(&mut DeadlineAwarePolicy::default(), &trace, limits),
        ),
        (
            "drain-first",
            simulate_policy_trace(
                &mut DrainFirstPolicy::new(SelectionConfig::default()),
                &trace,
                limits,
            ),
        ),
        (
            "max-headroom",
            simulate_policy_trace(
                &mut MaxHeadroomPolicy::new(SelectionConfig::default()),
                &trace,
                limits,
            ),
        ),
        (
            "reset-first",
            simulate_policy_trace(
                &mut ResetFirstPolicy::new(SelectionConfig::default()),
                &trace,
                limits,
            ),
        ),
        (
            "shadow-price",
            simulate_policy_trace(&mut ShadowPricePolicy::default(), &trace, limits),
        ),
        (
            "reset-weighted-minimax",
            simulate_policy_trace(&mut ResetWeightedMinimaxPolicy::default(), &trace, limits),
        ),
    ];

    for (policy_name, outcome) in policy_outcomes {
        assert!(
            oracle.is_at_least_as_good_as(outcome),
            "{policy_name} exceeded the offline oracle: {outcome:?} vs {oracle:?}",
        );
    }
}

#[test]
fn generated_trace_suite_keeps_shadow_price_no_worse_than_deadline_aware() {
    let mut deadline_aware_unavailable = 0;
    let mut shadow_price_unavailable = 0;
    let mut deadline_aware_failed_credits = 0;
    let mut shadow_price_failed_credits = 0;
    let mut evaluated_traces = 0;
    let mut shadow_price_regressions = 0;

    for seed in 1_u64..=32 {
        let limits = generated_trace_limits(seed);
        let trace = generated_trace(seed);
        let deadline_aware =
            simulate_policy_trace(&mut DeadlineAwarePolicy::default(), &trace, limits);
        let shadow_price = simulate_policy_trace(&mut ShadowPricePolicy::default(), &trace, limits);

        deadline_aware_unavailable += deadline_aware.user_unavailable_minutes();
        shadow_price_unavailable += shadow_price.user_unavailable_minutes();
        deadline_aware_failed_credits += deadline_aware.failed_credits;
        shadow_price_failed_credits += shadow_price.failed_credits;
        evaluated_traces += 1;
        if shadow_price.user_unavailable_minutes() > deadline_aware.user_unavailable_minutes() {
            shadow_price_regressions += 1;
        }
    }

    assert_eq!(evaluated_traces, 32);
    assert!(
        shadow_price_regressions < evaluated_traces,
        "shadow-price should not regress every generated trace",
    );
    assert!(
        deadline_aware_unavailable > 0,
        "generated traces should exercise unavailable-time behavior",
    );
    assert!(
        shadow_price_unavailable < deadline_aware_unavailable,
        "shadow-price aggregate unavailable time should improve: {shadow_price_unavailable} vs {deadline_aware_unavailable}",
    );
    assert!(
        f64::from(shadow_price_failed_credits) <= f64::from(deadline_aware_failed_credits) * 1.01,
        "shadow-price failed credits should stay within 1% of deadline-aware: {shadow_price_failed_credits} vs {deadline_aware_failed_credits}",
    );
}

#[test]
fn generated_trace_suite_keeps_reset_weighted_minimax_best_overall() {
    let mut deadline_aware_unavailable = 0;
    let mut shadow_price_unavailable = 0;
    let mut reset_weighted_minimax_unavailable = 0;
    let mut deadline_aware_failed_credits = 0;
    let mut reset_weighted_minimax_failed_credits = 0;
    let mut evaluated_traces = 0;
    let mut reset_weighted_minimax_regressions = 0;

    for seed in 1_u64..=32 {
        let limits = generated_trace_limits(seed);
        let trace = generated_trace(seed);
        let deadline_aware =
            simulate_policy_trace(&mut DeadlineAwarePolicy::default(), &trace, limits);
        let shadow_price = simulate_policy_trace(&mut ShadowPricePolicy::default(), &trace, limits);
        let reset_weighted_minimax =
            simulate_policy_trace(&mut ResetWeightedMinimaxPolicy::default(), &trace, limits);

        deadline_aware_unavailable += deadline_aware.user_unavailable_minutes();
        shadow_price_unavailable += shadow_price.user_unavailable_minutes();
        reset_weighted_minimax_unavailable += reset_weighted_minimax.user_unavailable_minutes();
        deadline_aware_failed_credits += deadline_aware.failed_credits;
        reset_weighted_minimax_failed_credits += reset_weighted_minimax.failed_credits;
        evaluated_traces += 1;
        if reset_weighted_minimax.user_unavailable_minutes()
            > deadline_aware.user_unavailable_minutes()
        {
            reset_weighted_minimax_regressions += 1;
        }
    }

    assert_eq!(evaluated_traces, 32);
    assert!(
        reset_weighted_minimax_regressions < evaluated_traces,
        "reset-weighted-minimax should not regress every generated trace",
    );
    assert!(
        reset_weighted_minimax_unavailable < shadow_price_unavailable,
        "reset-weighted-minimax aggregate unavailable time should improve over shadow-price: {reset_weighted_minimax_unavailable} vs {shadow_price_unavailable}",
    );
    assert!(
        reset_weighted_minimax_unavailable < deadline_aware_unavailable,
        "reset-weighted-minimax aggregate unavailable time should improve over deadline-aware: {reset_weighted_minimax_unavailable} vs {deadline_aware_unavailable}",
    );
    assert!(
        reset_weighted_minimax_failed_credits <= deadline_aware_failed_credits,
        "reset-weighted-minimax failed credits should not exceed deadline-aware: {reset_weighted_minimax_failed_credits} vs {deadline_aware_failed_credits}",
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
struct SimScenario {
    name: &'static str,
    account_count: usize,
    demand: SimDemand,
    initial_usage: InitialUsage,
}

impl SimScenario {
    fn new(
        name: &'static str,
        account_count: usize,
        demand: SimDemand,
        initial_usage: InitialUsage,
    ) -> Self {
        Self {
            name,
            account_count,
            demand,
            initial_usage,
        }
    }

    fn steady(name: &'static str, account_count: usize, credits_per_minute: f64) -> Self {
        Self::new(
            name,
            account_count,
            SimDemand::Constant(credits_per_minute),
            InitialUsage::Empty,
        )
    }

    fn demand_at(self, minute: i64) -> Option<f64> {
        if !is_work_minute(minute) {
            return None;
        }

        Some(match self.demand {
            SimDemand::Constant(credits_per_minute) => credits_per_minute,
            SimDemand::SessionBursts {
                base_credits_per_minute,
                burst_credits_per_minute,
                burst_minutes,
            } => {
                if is_session_burst_minute(minute, burst_minutes) {
                    burst_credits_per_minute
                } else {
                    base_credits_per_minute
                }
            }
            SimDemand::HeavyFirstWorkday {
                normal_credits_per_minute,
                heavy_credits_per_minute,
            } => {
                if minute / 1_440 == 0 {
                    heavy_credits_per_minute
                } else {
                    normal_credits_per_minute
                }
            }
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum SimDemand {
    Constant(f64),
    SessionBursts {
        base_credits_per_minute: f64,
        burst_credits_per_minute: f64,
        burst_minutes: i64,
    },
    HeavyFirstWorkday {
        normal_credits_per_minute: f64,
        heavy_credits_per_minute: f64,
    },
}

#[derive(Debug, Clone, Copy)]
enum InitialUsage {
    Empty,
    WeeklyNearExhaustionMixed,
}

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

#[derive(Debug, Clone, Copy)]
struct TraceLimits {
    account_count: usize,
    five_hour_limit: u16,
    weekly_limit: u16,
    five_hour_window: i32,
    weekly_window: i32,
}

#[derive(Debug, Clone, Copy)]
struct TraceDemand {
    minute: i32,
    credits: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct TraceEvent {
    minute: i32,
    credits: u16,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
struct TraceAccountState {
    five_hour_events: Vec<TraceEvent>,
    weekly_events: Vec<TraceEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct OracleMemoKey {
    demand_index: usize,
    accounts: Vec<TraceAccountState>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct TraceOutcome {
    served_credits: u32,
    failed_credits: u32,
    unavailable_minutes: u32,
}

struct SimAccount {
    account: StoredAccount,
    five_hour_events: VecDeque<UsageEvent>,
    weekly_events: VecDeque<UsageEvent>,
}

#[derive(Debug, Clone, Copy)]
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

    fn total_demand_credits(&self) -> f64 {
        self.served_credits + self.failed_credits
    }
}

#[derive(Debug)]
struct PolicyStats {
    policy_name: &'static str,
    stats: SimStats,
}

struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
    fn new(seed: u64) -> Self {
        Self {
            state: seed ^ 0xe703_7ed1_a0b4_28db,
        }
    }

    fn next_u32(&mut self) -> u32 {
        self.state = self
            .state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        u32::try_from(self.state >> 32).expect("upper 32 bits should fit u32")
    }

    fn range_i32(&mut self, min: i32, max: i32) -> i32 {
        assert!(min <= max);
        let width = u32::try_from(max - min + 1).expect("range width should fit u32");
        let offset = i32::try_from(self.next_u32() % width).expect("offset should fit i32");
        min + offset
    }

    fn range_u16(&mut self, min: u16, max: u16) -> u16 {
        assert!(min <= max);
        let width = u32::from(max - min) + 1;
        let offset = u16::try_from(self.next_u32() % width).expect("offset should fit u16");
        min + offset
    }

    fn one_in(&mut self, denominator: u32) -> bool {
        assert!(denominator > 0);
        self.next_u32().is_multiple_of(denominator)
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
    simulate_policy_scenario(
        policy,
        SimScenario::steady("steady-work-week", account_count, credits_per_minute),
    )
}

fn simulate_policy_scenario<P: AccountSelectionPolicy>(
    policy: &mut P,
    scenario: SimScenario,
) -> SimStats {
    let mut accounts = (0..scenario.account_count)
        .map(|index| SimAccount::new(&format!("account-{index}"), index, scenario.account_count))
        .collect::<Vec<_>>();
    apply_initial_usage(&mut accounts, scenario.initial_usage);
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

        if let Some(credits) = scenario.demand_at(minute) {
            serve_demand(
                policy,
                &mut accounts,
                Demand { minute, credits },
                &mut stats,
                &mut last_account_index,
                &mut contiguous_unavailable_minutes,
            );
        }
    }

    stats
}

fn evaluation_scenarios() -> [SimScenario; 5] {
    [
        SimScenario::steady("light-steady", 2, 1.0),
        SimScenario::steady("near-capacity-steady", 2, 4.35),
        SimScenario::new(
            "bursty-short-sessions",
            2,
            SimDemand::SessionBursts {
                base_credits_per_minute: 1.5,
                burst_credits_per_minute: 8.0,
                burst_minutes: 20,
            },
            InitialUsage::Empty,
        ),
        SimScenario::new(
            "one-heavy-day-then-normal",
            2,
            SimDemand::HeavyFirstWorkday {
                normal_credits_per_minute: 2.0,
                heavy_credits_per_minute: 6.0,
            },
            InitialUsage::Empty,
        ),
        SimScenario::new(
            "weekly-near-exhaustion-mixed",
            2,
            SimDemand::Constant(2.0),
            InitialUsage::WeeklyNearExhaustionMixed,
        ),
    ]
}

fn policy_stats_for_scenario(scenario: SimScenario) -> [PolicyStats; 6] {
    let mut deadline_aware = DeadlineAwarePolicy::default();
    let mut drain_first = DrainFirstPolicy::new(SelectionConfig::default());
    let mut max_headroom = MaxHeadroomPolicy::new(SelectionConfig::default());
    let mut reset_first = ResetFirstPolicy::new(SelectionConfig::default());
    let mut shadow_price = ShadowPricePolicy::default();
    let mut reset_weighted_minimax = ResetWeightedMinimaxPolicy::default();

    [
        PolicyStats {
            policy_name: "deadline-aware",
            stats: simulate_policy_scenario(&mut deadline_aware, scenario),
        },
        PolicyStats {
            policy_name: "drain-first",
            stats: simulate_policy_scenario(&mut drain_first, scenario),
        },
        PolicyStats {
            policy_name: "max-headroom",
            stats: simulate_policy_scenario(&mut max_headroom, scenario),
        },
        PolicyStats {
            policy_name: "reset-first",
            stats: simulate_policy_scenario(&mut reset_first, scenario),
        },
        PolicyStats {
            policy_name: "shadow-price",
            stats: simulate_policy_scenario(&mut shadow_price, scenario),
        },
        PolicyStats {
            policy_name: "reset-weighted-minimax",
            stats: simulate_policy_scenario(&mut reset_weighted_minimax, scenario),
        },
    ]
}

fn generated_trace_limits(seed: u64) -> TraceLimits {
    let mut rng = DeterministicRng::new(seed);
    let five_hour_limit = rng.range_u16(18, 32);

    TraceLimits {
        account_count: 2 + usize::try_from(seed % 3).expect("seed modulo should fit usize"),
        five_hour_limit,
        weekly_limit: five_hour_limit * 4,
        five_hour_window: 30,
        weekly_window: 240,
    }
}

fn generated_trace(seed: u64) -> Vec<TraceDemand> {
    let mut rng = DeterministicRng::new(seed ^ 0xa076_1d64_78bd_642f);
    let mut trace = Vec::new();
    let mut minute = 0;

    while minute < 480 {
        minute += rng.range_i32(1, 3);
        if !is_generated_trace_work_minute(minute) {
            continue;
        }

        let burst = if rng.one_in(7) {
            rng.range_u16(3, 8)
        } else {
            0
        };
        let credits = rng.range_u16(1, 4) + burst;
        trace.push(TraceDemand { minute, credits });
    }

    trace
}

fn is_generated_trace_work_minute(minute: i32) -> bool {
    let slot = minute % 120;
    (10..55).contains(&slot) || (75..105).contains(&slot)
}

// Offline upper bound for small deterministic traces. This is a test oracle,
// not a runtime strategy: it knows the full future demand trace, but it still
// must serve the current demand whenever any account has capacity.
fn offline_oracle(trace: &[TraceDemand], limits: TraceLimits) -> TraceOutcome {
    validate_trace_inputs(trace, limits);
    let accounts = vec![TraceAccountState::default(); limits.account_count];
    let mut memo = HashMap::new();
    offline_oracle_from(0, accounts, trace, limits, &mut memo)
}

fn offline_oracle_from(
    demand_index: usize,
    accounts: Vec<TraceAccountState>,
    trace: &[TraceDemand],
    limits: TraceLimits,
    memo: &mut HashMap<OracleMemoKey, TraceOutcome>,
) -> TraceOutcome {
    let Some(demand) = trace.get(demand_index).copied() else {
        return TraceOutcome::default();
    };

    let accounts = accounts
        .into_iter()
        .map(|mut account| {
            account.expire(demand.minute, limits);
            account
        })
        .collect::<Vec<_>>();
    let key = OracleMemoKey {
        demand_index,
        accounts: accounts.clone(),
    };
    if let Some(outcome) = memo.get(&key) {
        return *outcome;
    }

    let serving_indices = accounts
        .iter()
        .enumerate()
        .filter_map(|(index, account)| account.can_serve(demand.credits, limits).then_some(index))
        .collect::<Vec<_>>();

    let outcome = if serving_indices.is_empty() {
        offline_oracle_from(demand_index + 1, accounts, trace, limits, memo)
            .with_unavailable(demand.credits)
    } else {
        serving_indices
            .into_iter()
            .map(|index| {
                let mut next_accounts = accounts.clone();
                next_accounts[index].consume(demand);
                offline_oracle_from(demand_index + 1, next_accounts, trace, limits, memo)
                    .with_served(demand.credits)
            })
            .max_by(TraceOutcome::compare_for_oracle)
            .expect("serving indices should not be empty")
    };

    memo.insert(key, outcome);
    outcome
}

fn simulate_policy_trace<P: AccountSelectionPolicy>(
    policy: &mut P,
    trace: &[TraceDemand],
    limits: TraceLimits,
) -> TraceOutcome {
    validate_trace_inputs(trace, limits);
    let accounts = (0..limits.account_count)
        .map(|index| chatgpt_account(&format!("trace-account-{index}"), None))
        .collect::<Vec<_>>();
    let mut states = vec![TraceAccountState::default(); limits.account_count];
    let mut outcome = TraceOutcome::default();

    for demand in trace {
        for state in &mut states {
            state.expire(demand.minute, limits);
        }

        let usages = states
            .iter()
            .zip(accounts.iter())
            .map(|(state, account)| trace_usage_info(&account.id, state, demand.minute, limits))
            .collect::<Vec<_>>();
        let candidates = accounts
            .iter()
            .zip(usages.iter())
            .map(|(account, usage)| candidate(account, usage))
            .collect::<Vec<_>>();
        let selected_index = policy
            .select_account_at(
                &candidates,
                SelectionContext::at(i64::from(demand.minute) * 60),
            )
            .and_then(|selection| {
                accounts
                    .iter()
                    .position(|account| account.id == selection.account.id)
            });
        let serving_index = selected_index
            .filter(|index| states[*index].can_serve(demand.credits, limits))
            .or_else(|| {
                states
                    .iter()
                    .position(|state| state.can_serve(demand.credits, limits))
            });

        if let Some(index) = serving_index {
            states[index].consume(*demand);
            outcome = outcome.with_served(demand.credits);
        } else {
            outcome = outcome.with_unavailable(demand.credits);
        }
    }

    outcome
}

fn validate_trace_inputs(trace: &[TraceDemand], limits: TraceLimits) {
    assert!(
        limits.account_count > 0,
        "trace fixtures must include at least one account",
    );
    assert!(
        limits.five_hour_limit > 0,
        "trace fixtures must use a positive 5-hour limit",
    );
    assert!(
        limits.weekly_limit > 0,
        "trace fixtures must use a positive weekly limit",
    );
    assert!(
        limits.five_hour_window > 0,
        "trace fixtures must use a positive 5-hour window",
    );
    assert!(
        limits.weekly_window > 0,
        "trace fixtures must use a positive weekly window",
    );
    assert!(
        trace
            .windows(2)
            .all(|pair| pair[0].minute <= pair[1].minute),
        "trace demand must be sorted by minute",
    );
}

fn apply_initial_usage(accounts: &mut [SimAccount], initial_usage: InitialUsage) {
    match initial_usage {
        InitialUsage::Empty => {}
        InitialUsage::WeeklyNearExhaustionMixed => {
            if let Some(account) = accounts.get_mut(0) {
                account.seed_initial_usage(25.0, 4_600.0, 200, 2 * 1_440);
            }
            if let Some(account) = accounts.get_mut(1) {
                account.seed_initial_usage(75.0, 300.0, 150, 5 * 1_440);
            }
        }
    }
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

    policy
        .select_account_at(&candidates, SelectionContext::at(minute * 60))
        .and_then(|selection| {
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

fn is_session_burst_minute(minute: i64, burst_minutes: i64) -> bool {
    let burst_minutes = burst_minutes.max(0);
    let minute_of_day = minute % 1_440;
    (570..570 + burst_minutes).contains(&minute_of_day)
        || (810..810 + burst_minutes).contains(&minute_of_day)
}

impl SimAccount {
    fn new(id: &str, index: usize, account_count: usize) -> Self {
        let five_hour_seed = staggered_seed_event(index, account_count, FIVE_HOUR_WINDOW_MINUTES);
        let weekly_seed = staggered_seed_event(index, account_count, WEEKLY_WINDOW_MINUTES);

        Self {
            account: chatgpt_account(id, None),
            five_hour_events: VecDeque::from([five_hour_seed]),
            weekly_events: VecDeque::from([weekly_seed]),
        }
    }

    fn seed_initial_usage(
        &mut self,
        five_hour_credits: f64,
        weekly_credits: f64,
        five_hour_resets_at_minute: i64,
        weekly_resets_at_minute: i64,
    ) {
        if five_hour_credits > 0.0 {
            self.five_hour_events.push_back(seed_usage_event(
                five_hour_resets_at_minute,
                FIVE_HOUR_WINDOW_MINUTES,
                five_hour_credits,
            ));
            sort_events(&mut self.five_hour_events);
        }
        if weekly_credits > 0.0 {
            self.weekly_events.push_back(seed_usage_event(
                weekly_resets_at_minute,
                WEEKLY_WINDOW_MINUTES,
                weekly_credits,
            ));
            sort_events(&mut self.weekly_events);
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

impl TraceAccountState {
    fn expire(&mut self, minute: i32, limits: TraceLimits) {
        self.five_hour_events
            .retain(|event| event.minute + limits.five_hour_window > minute);
        self.weekly_events
            .retain(|event| event.minute + limits.weekly_window > minute);
    }

    fn consume(&mut self, demand: TraceDemand) {
        let event = TraceEvent {
            minute: demand.minute,
            credits: demand.credits,
        };
        self.five_hour_events.push(event);
        self.weekly_events.push(event);
    }

    fn can_serve(&self, credits: u16, limits: TraceLimits) -> bool {
        self.five_hour_remaining(limits) >= u32::from(credits)
            && self.weekly_remaining(limits) >= u32::from(credits)
    }

    fn five_hour_remaining(&self, limits: TraceLimits) -> u32 {
        u32::from(limits.five_hour_limit)
            .saturating_sub(total_trace_credits(&self.five_hour_events))
    }

    fn weekly_remaining(&self, limits: TraceLimits) -> u32 {
        u32::from(limits.weekly_limit).saturating_sub(total_trace_credits(&self.weekly_events))
    }

    fn five_hour_used(&self) -> u32 {
        total_trace_credits(&self.five_hour_events)
    }

    fn weekly_used(&self) -> u32 {
        total_trace_credits(&self.weekly_events)
    }
}

impl TraceOutcome {
    fn user_unavailable_minutes(self) -> u32 {
        self.unavailable_minutes
    }

    fn with_served(mut self, credits: u16) -> Self {
        self.served_credits += u32::from(credits);
        self
    }

    fn with_unavailable(mut self, credits: u16) -> Self {
        self.unavailable_minutes += 1;
        self.failed_credits += u32::from(credits);
        self
    }

    fn is_at_least_as_good_as(self, other: Self) -> bool {
        Self::compare_for_oracle(&self, &other) != Ordering::Less
    }

    fn compare_for_oracle(left: &Self, right: &Self) -> Ordering {
        right
            .unavailable_minutes
            .cmp(&left.unavailable_minutes)
            .then_with(|| left.served_credits.cmp(&right.served_credits))
            .then_with(|| right.failed_credits.cmp(&left.failed_credits))
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

fn seed_usage_event(resets_at_minute: i64, window_minutes: i64, credits: f64) -> UsageEvent {
    UsageEvent {
        at_minute: resets_at_minute - window_minutes,
        credits,
    }
}

fn sort_events(events: &mut VecDeque<UsageEvent>) {
    let mut sorted = events.drain(..).collect::<Vec<_>>();
    sorted.sort_by_key(|event| event.at_minute);
    *events = VecDeque::from(sorted);
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
        * 60
}

fn total_credits(events: &VecDeque<UsageEvent>) -> f64 {
    events.iter().map(|event| event.credits).sum()
}

fn total_trace_credits(events: &[TraceEvent]) -> u32 {
    events.iter().map(|event| u32::from(event.credits)).sum()
}

fn trace_usage_info(
    account_id: &str,
    state: &TraceAccountState,
    minute: i32,
    limits: TraceLimits,
) -> UsageInfo {
    usage_info_with_windows(
        account_id,
        used_percent(
            f64::from(state.five_hour_used()),
            f64::from(limits.five_hour_limit),
        ),
        used_percent(
            f64::from(state.weekly_used()),
            f64::from(limits.weekly_limit),
        ),
        trace_reset_at(&state.five_hour_events, minute, limits.five_hour_window),
        trace_reset_at(&state.weekly_events, minute, limits.weekly_window),
        i64::from(limits.five_hour_window),
        i64::from(limits.weekly_window),
    )
}

fn trace_reset_at(events: &[TraceEvent], minute: i32, window_minutes: i32) -> i64 {
    events.first().map_or(i64::from(minute), |event| {
        i64::from(event.minute + window_minutes)
    }) * 60
}

fn used_percent(used: f64, limit: f64) -> f64 {
    (used / limit * 100.0).clamp(0.0, 100.0)
}

fn candidate<'a>(account: &'a StoredAccount, usage: &'a UsageInfo) -> AccountUsageCandidate<'a> {
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
    usage_info_with_windows(
        account_id,
        five_hour_used_percent,
        weekly_used_percent,
        five_hour_resets_at,
        weekly_resets_at,
        300,
        10_080,
    )
}

fn usage_info_with_windows(
    account_id: &str,
    five_hour_used_percent: f64,
    weekly_used_percent: f64,
    five_hour_resets_at: i64,
    weekly_resets_at: i64,
    five_hour_window_minutes: i64,
    weekly_window_minutes: i64,
) -> UsageInfo {
    UsageInfo {
        account_id: account_id.to_string(),
        limit_id: Some("codex".to_string()),
        limit_name: None,
        plan_type: Some("pro".to_string()),
        primary_used_percent: Some(five_hour_used_percent),
        primary_window_minutes: Some(five_hour_window_minutes),
        primary_resets_at: Some(five_hour_resets_at),
        secondary_used_percent: Some(weekly_used_percent),
        secondary_window_minutes: Some(weekly_window_minutes),
        secondary_resets_at: Some(weekly_resets_at),
        has_credits: None,
        unlimited_credits: None,
        credits_balance: None,
        rate_limit_reached_type: None,
        additional_limits: Vec::new(),
        error: None,
    }
}
