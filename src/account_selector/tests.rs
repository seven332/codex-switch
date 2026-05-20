use std::{
    cmp::Ordering,
    collections::{HashMap, VecDeque},
    fmt,
    sync::OnceLock,
};

use chrono::{TimeZone, Utc};

use super::{
    AccountSelection, AccountSelectionPolicy, AccountUsageCandidate, DeadlineAwarePolicy,
    DemandAwareHysteresisPolicy, ResetWeightedMinimaxPolicy, SelectionConfig, SelectionContext,
    SelectionPolicyKind, ShadowPricePolicy, UsageWindow, compare_headroom_desc, compare_last_used,
    compare_optional_reset, evaluated_candidates, select_account, select_account_with_context,
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
}

impl AccountSelectionPolicy for DrainFirstPolicy {
    fn select_account_at<'a>(
        &mut self,
        candidates: &[AccountUsageCandidate<'a>],
        _context: SelectionContext<'_>,
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
        _context: SelectionContext<'_>,
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
        _context: SelectionContext<'_>,
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

#[derive(Debug, Clone, Copy, Default)]
struct CurrentFirstPolicy {
    config: SelectionConfig,
}

impl AccountSelectionPolicy for CurrentFirstPolicy {
    fn select_account_at<'a>(
        &mut self,
        candidates: &[AccountUsageCandidate<'a>],
        context: SelectionContext<'_>,
    ) -> Option<AccountSelection<'a>> {
        let evaluated = evaluated_candidates(candidates, self.config);
        let selected = context
            .current_account_id
            .and_then(|account_id| {
                evaluated
                    .iter()
                    .find(|candidate| candidate.account.id == account_id)
            })
            .or_else(|| evaluated.first())?;

        Some(AccountSelection {
            account: selected.account,
            metrics: selected.metrics,
        })
    }
}

const DEADLINE_AWARE_POLICY_NAME: &str = "deadline-aware";
const DRAIN_FIRST_POLICY_NAME: &str = "drain-first";
const MAX_HEADROOM_POLICY_NAME: &str = "max-headroom";
const RESET_FIRST_POLICY_NAME: &str = "reset-first";
const SHADOW_PRICE_POLICY_NAME: &str = "shadow-price";
const RESET_WEIGHTED_MINIMAX_POLICY_NAME: &str = "reset-weighted-minimax";
const DEMAND_AWARE_HYSTERESIS_POLICY_NAME: &str = "demand-aware-hysteresis";
const SIMULATED_POLICY_NAMES: &[&str] = &[
    DEADLINE_AWARE_POLICY_NAME,
    DRAIN_FIRST_POLICY_NAME,
    MAX_HEADROOM_POLICY_NAME,
    RESET_FIRST_POLICY_NAME,
    SHADOW_PRICE_POLICY_NAME,
    RESET_WEIGHTED_MINIMAX_POLICY_NAME,
    DEMAND_AWARE_HYSTERESIS_POLICY_NAME,
];
const POLICY_COUNT: usize = SIMULATED_POLICY_NAMES.len();
const DEFAULT_POLICY_NAME: &str = DEADLINE_AWARE_POLICY_NAME;
const RUNTIME_REPLACEMENT_CANDIDATES: &[SelectionPolicyKind] = &[
    SelectionPolicyKind::ShadowPrice,
    SelectionPolicyKind::ResetWeightedMinimax,
    SelectionPolicyKind::DemandAwareHysteresis,
];
const REPLACEMENT_MIN_UNAVAILABLE_REDUCTION_MINUTES: u32 = 30;
const REPLACEMENT_MIN_UNAVAILABLE_REDUCTION_RATIO: f64 = 0.05;
const REPLACEMENT_MAX_SWITCH_INCREASE_RATIO: f64 = 1.25;
const REPLACEMENT_MAX_SWITCH_INCREASE_ABSOLUTE: u32 = 25;
const REPLACEMENT_FAILED_CREDITS_RELATIVE_TOLERANCE: f64 = 0.01;
const REPLACEMENT_FAILED_CREDITS_ABSOLUTE_TOLERANCE: f64 = 0.000_001;

#[derive(Debug, Clone, Copy)]
struct ReplacementGate {
    min_unavailable_reduction_minutes: u32,
    min_unavailable_reduction_ratio: f64,
    max_switch_increase_ratio: f64,
    max_switch_increase_absolute: u32,
    failed_credits_relative_tolerance: f64,
    failed_credits_absolute_tolerance: f64,
}

impl Default for ReplacementGate {
    fn default() -> Self {
        Self {
            min_unavailable_reduction_minutes: REPLACEMENT_MIN_UNAVAILABLE_REDUCTION_MINUTES,
            min_unavailable_reduction_ratio: REPLACEMENT_MIN_UNAVAILABLE_REDUCTION_RATIO,
            max_switch_increase_ratio: REPLACEMENT_MAX_SWITCH_INCREASE_RATIO,
            max_switch_increase_absolute: REPLACEMENT_MAX_SWITCH_INCREASE_ABSOLUTE,
            failed_credits_relative_tolerance: REPLACEMENT_FAILED_CREDITS_RELATIVE_TOLERANCE,
            failed_credits_absolute_tolerance: REPLACEMENT_FAILED_CREDITS_ABSOLUTE_TOLERANCE,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum ReplacementGateDecision {
    Pass,
    Fail(ReplacementGateFailure),
}

impl ReplacementGateDecision {
    fn is_pass(&self) -> bool {
        matches!(self, Self::Pass)
    }
}

#[derive(Debug, Clone, PartialEq)]
struct ReplacementGateFailure {
    kind: ReplacementGateFailureKind,
    reason: String,
}

impl ReplacementGateFailure {
    fn new(kind: ReplacementGateFailureKind, reason: String) -> Self {
        Self { kind, reason }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplacementGateFailureKind {
    PreventableFailuresPresent,
    MaxContiguousUnavailableRegressed,
    FailedCreditsExceeded,
    SwitchesExceeded,
    UnavailableRegressed,
    UnavailableReductionTooSmall,
    ScenarioRegressed,
}

#[derive(Debug, Clone, Copy)]
enum ReplacementGateSummaryScope {
    Aggregate,
    Scenario,
}

impl ReplacementGateSummaryScope {
    fn label(self) -> &'static str {
        match self {
            Self::Aggregate => "aggregate",
            Self::Scenario => "scenario",
        }
    }
}

impl ReplacementGate {
    fn evaluate_candidate(
        &self,
        evaluation: &PolicyEvaluation,
        candidate_name: &str,
    ) -> ReplacementGateDecision {
        let default_aggregate = find_policy_stats(&evaluation.aggregate_stats, DEFAULT_POLICY_NAME);
        let candidate_aggregate = find_policy_stats(&evaluation.aggregate_stats, candidate_name);
        if let Some(failure) =
            self.guardrail_failure(&default_aggregate.stats, &candidate_aggregate.stats)
        {
            return ReplacementGateDecision::Fail(failure);
        }

        for scenario in &evaluation.scenarios {
            let default_stats = find_policy_stats(&scenario.policy_stats, DEFAULT_POLICY_NAME);
            let candidate_stats = find_policy_stats(&scenario.policy_stats, candidate_name);
            if let ReplacementGateDecision::Fail(failure) =
                self.evaluate_scenario(&default_stats.stats, &candidate_stats.stats)
            {
                return ReplacementGateDecision::Fail(ReplacementGateFailure::new(
                    ReplacementGateFailureKind::ScenarioRegressed,
                    format!("{}: {}", scenario.scenario_name, failure.reason),
                ));
            }
        }

        let aggregate_decision = self
            .evaluate_unavailable_improvement(&default_aggregate.stats, &candidate_aggregate.stats);
        if !aggregate_decision.is_pass() {
            return aggregate_decision;
        }

        ReplacementGateDecision::Pass
    }

    fn evaluate_aggregate(
        &self,
        default: &SimStats,
        candidate: &SimStats,
    ) -> ReplacementGateDecision {
        if let Some(failure) = self.guardrail_failure(default, candidate) {
            return ReplacementGateDecision::Fail(failure);
        }

        self.evaluate_unavailable_improvement(default, candidate)
    }

    fn evaluate_unavailable_improvement(
        &self,
        default: &SimStats,
        candidate: &SimStats,
    ) -> ReplacementGateDecision {
        let default_unavailable = default.user_unavailable_minutes();
        let candidate_unavailable = candidate.user_unavailable_minutes();
        if candidate_unavailable > default_unavailable {
            return ReplacementGateDecision::Fail(ReplacementGateFailure::new(
                ReplacementGateFailureKind::UnavailableRegressed,
                format!(
                    "unavailable regressed from {default_unavailable} to {candidate_unavailable}"
                ),
            ));
        }

        let actual_reduction = default_unavailable.saturating_sub(candidate_unavailable);
        let required_reduction = self.min_unavailable_reduction(default_unavailable);
        if actual_reduction < required_reduction {
            return ReplacementGateDecision::Fail(ReplacementGateFailure::new(
                ReplacementGateFailureKind::UnavailableReductionTooSmall,
                format!(
                    "unavailable reduction {actual_reduction} is below required {required_reduction}"
                ),
            ));
        }

        ReplacementGateDecision::Pass
    }

    fn evaluate_scenario(
        &self,
        default: &SimStats,
        candidate: &SimStats,
    ) -> ReplacementGateDecision {
        if let Some(failure) = self.guardrail_failure(default, candidate) {
            return ReplacementGateDecision::Fail(failure);
        }

        if candidate.user_unavailable_minutes() > default.user_unavailable_minutes() {
            return ReplacementGateDecision::Fail(ReplacementGateFailure::new(
                ReplacementGateFailureKind::UnavailableRegressed,
                format!(
                    "unavailable regressed from {} to {}",
                    default.user_unavailable_minutes(),
                    candidate.user_unavailable_minutes()
                ),
            ));
        }

        ReplacementGateDecision::Pass
    }

    fn guardrail_failure(
        &self,
        default: &SimStats,
        candidate: &SimStats,
    ) -> Option<ReplacementGateFailure> {
        if candidate.preventable_failures > 0 {
            return Some(ReplacementGateFailure::new(
                ReplacementGateFailureKind::PreventableFailuresPresent,
                format!(
                    "preventable failures must stay zero, got {}",
                    candidate.preventable_failures
                ),
            ));
        }

        if candidate.max_contiguous_unavailable_minutes > default.max_contiguous_unavailable_minutes
        {
            return Some(ReplacementGateFailure::new(
                ReplacementGateFailureKind::MaxContiguousUnavailableRegressed,
                format!(
                    "max contiguous unavailable regressed from {} to {}",
                    default.max_contiguous_unavailable_minutes,
                    candidate.max_contiguous_unavailable_minutes
                ),
            ));
        }

        let failed_credits_budget = self.failed_credits_budget(default.failed_credits);
        if candidate.failed_credits > failed_credits_budget {
            return Some(ReplacementGateFailure::new(
                ReplacementGateFailureKind::FailedCreditsExceeded,
                format!(
                    "failed credits {:.1} exceed budget {:.1}",
                    candidate.failed_credits, failed_credits_budget
                ),
            ));
        }

        let switch_budget = self.switch_budget(default.account_switches);
        if candidate.account_switches > switch_budget {
            return Some(ReplacementGateFailure::new(
                ReplacementGateFailureKind::SwitchesExceeded,
                format!(
                    "switches {} exceed budget {}",
                    candidate.account_switches, switch_budget
                ),
            ));
        }

        None
    }

    fn min_unavailable_reduction(&self, default_unavailable: u32) -> u32 {
        let ratio_reduction =
            (f64::from(default_unavailable) * self.min_unavailable_reduction_ratio).ceil() as u32;
        self.min_unavailable_reduction_minutes.max(ratio_reduction)
    }

    fn switch_budget(&self, default_switches: u32) -> u32 {
        (f64::from(default_switches) * self.max_switch_increase_ratio).ceil() as u32
            + self.max_switch_increase_absolute
    }

    fn failed_credits_budget(&self, default_failed: f64) -> f64 {
        default_failed * (1.0 + self.failed_credits_relative_tolerance)
            + self.failed_credits_absolute_tolerance
    }

    fn compare_passing_candidates(left: &PolicyStats, right: &PolicyStats) -> Ordering {
        compare_policy_outcome(&left.stats, &right.stats)
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
    assert_eq!(selection.metrics.weekly_headroom_units, 50.0);
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
fn selection_context_carries_current_account_id_to_policy() {
    let current = chatgpt_account("current", None);
    let other = chatgpt_account("other", None);
    let current_info = usage_info("current", 50.0, 50.0, 100, 200);
    let other_info = usage_info("other", 50.0, 50.0, 100, 200);
    let candidates = [
        candidate(&other, &other_info),
        candidate(&current, &current_info),
    ];
    let context = SelectionContext::at(0).with_current_account_id(Some("current"));

    let selection = CurrentFirstPolicy::default()
        .select_account_at(&candidates, context)
        .expect("current account should be selected");

    assert_eq!(selection.account.id, "current");
}

#[test]
fn selection_context_current_account_preserves_time() {
    let context = SelectionContext::at(12_345).with_current_account_id(Some("current"));

    assert_eq!(context.now, 12_345);
    assert_eq!(context.current_account_id, Some("current"));
}

#[test]
fn select_account_with_context_treats_explicit_none_as_no_current_account() {
    let first = chatgpt_account("first", None);
    let second = chatgpt_account("second", None);
    let first_info = usage_info("first", 20.0, 20.0, 500, 1_000);
    let second_info = usage_info("second", 50.0, 50.0, 500, 1_000);
    let candidates = [
        candidate(&first, &first_info),
        candidate(&second, &second_info),
    ];
    let context = SelectionContext::at(0);

    let implicit_none_selection =
        select_account_with_context(&candidates, SelectionConfig::default(), context)
            .expect("usable account should be selected");
    let explicit_none_selection = select_account_with_context(
        &candidates,
        SelectionConfig::default(),
        context.with_current_account_id(None),
    )
    .expect("usable account should be selected");

    assert_eq!(
        explicit_none_selection.account.id,
        implicit_none_selection.account.id
    );
}

#[test]
fn deadline_aware_ignores_current_account_context_for_now() {
    let current = chatgpt_account("current", None);
    let soon_reset = chatgpt_account("soon-reset", None);
    let current_info = usage_info("current", 20.0, 20.0, 500, 1_000);
    let soon_reset_info = usage_info("soon-reset", 95.0, 20.0, 10, 1_000);
    let candidates = [
        candidate(&current, &current_info),
        candidate(&soon_reset, &soon_reset_info),
    ];
    let context = SelectionContext::at(0).with_current_account_id(Some("current"));

    let selection = select_account_with_context(&candidates, SelectionConfig::default(), context)
        .expect("usable account should be selected");

    assert_eq!(selection.account.id, "soon-reset");
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
fn demand_aware_hysteresis_keeps_healthy_active_for_tiny_improvement() {
    let active = chatgpt_account("active", None);
    let candidate_account = chatgpt_account("candidate", None);
    let active_info = usage_info("active", 40.0, 20.0, 305, 604_800);
    let candidate_info = usage_info("candidate", 40.0, 20.0, 300, 604_800);
    let candidates = [
        candidate(&candidate_account, &candidate_info),
        candidate(&active, &active_info),
    ];
    let context = SelectionContext::at(0).with_current_account_id(Some("active"));

    let selection = DemandAwareHysteresisPolicy::default()
        .select_account_at(&candidates, context)
        .expect("usable account should be selected");

    assert_eq!(selection.account.id, "active");
}

#[test]
fn demand_aware_hysteresis_replaces_unavailable_active() {
    let active = chatgpt_account("active", None);
    let replacement = chatgpt_account("replacement", None);
    let active_info = usage_info("active", 100.0, 20.0, 10, 604_800);
    let replacement_info = usage_info("replacement", 50.0, 20.0, 300, 604_800);
    let candidates = [
        candidate(&active, &active_info),
        candidate(&replacement, &replacement_info),
    ];
    let context = SelectionContext::at(0).with_current_account_id(Some("active"));

    let selection = DemandAwareHysteresisPolicy::default()
        .select_account_at(&candidates, context)
        .expect("replacement account should be selected");

    assert_eq!(selection.account.id, "replacement");
}

#[test]
fn demand_aware_hysteresis_replaces_active_for_materially_better_candidate() {
    let active = chatgpt_account("active", None);
    let candidate_account = chatgpt_account("candidate", None);
    let active_info = usage_info("active", 90.0, 20.0, 18_000, 604_800);
    let candidate_info = usage_info("candidate", 50.0, 20.0, 300, 604_800);
    let candidates = [
        candidate(&active, &active_info),
        candidate(&candidate_account, &candidate_info),
    ];
    let context = SelectionContext::at(0).with_current_account_id(Some("active"));

    let selection = DemandAwareHysteresisPolicy::default()
        .select_account_at(&candidates, context)
        .expect("materially better account should be selected");

    assert_eq!(selection.account.id, "candidate");
}

#[test]
fn demand_aware_hysteresis_protects_near_term_demand_margin() {
    let soon = chatgpt_account("soon", None);
    let safer = chatgpt_account("safer", None);
    let soon_info = usage_info("soon", 96.0, 20.0, 60, 604_800);
    let safer_info = usage_info("safer", 80.0, 20.0, 18_000, 604_800);
    let candidates = [candidate(&soon, &soon_info), candidate(&safer, &safer_info)];

    let selection = DemandAwareHysteresisPolicy::default()
        .select_account_at(&candidates, SelectionContext::at(0))
        .expect("usable account should be selected");

    assert_eq!(selection.account.id, "safer");
}

#[test]
fn selection_config_can_select_demand_aware_hysteresis_policy() {
    let active = chatgpt_account("active", None);
    let candidate_account = chatgpt_account("candidate", None);
    let active_info = usage_info("active", 40.0, 20.0, 305, 604_800);
    let candidate_info = usage_info("candidate", 40.0, 20.0, 300, 604_800);
    let candidates = [
        candidate(&candidate_account, &candidate_info),
        candidate(&active, &active_info),
    ];
    let config = SelectionConfig {
        policy: SelectionPolicyKind::DemandAwareHysteresis,
        ..SelectionConfig::default()
    };
    let context = SelectionContext::at(0).with_current_account_id(Some("active"));

    let selection = select_account_with_context(&candidates, config, context)
        .expect("usable account should be selected");

    assert_eq!(selection.account.id, "active");
}

#[test]
fn simulator_evaluates_policies_across_realistic_usage_scenarios() {
    let evaluation = realistic_usage_evaluation();
    let diagnostics = PolicyEvaluationDiagnostics(evaluation);
    let aggregate_stats = &evaluation.aggregate_stats;
    let drain_first = find_policy_stats(aggregate_stats, DRAIN_FIRST_POLICY_NAME);
    let deadline_aware = find_policy_stats(aggregate_stats, DEADLINE_AWARE_POLICY_NAME);
    let shadow_price = find_policy_stats(aggregate_stats, SHADOW_PRICE_POLICY_NAME);
    let reset_weighted_minimax =
        find_policy_stats(aggregate_stats, RESET_WEIGHTED_MINIMAX_POLICY_NAME);
    let aggregate_demand_credits = aggregate_stats[0].stats.total_demand_credits();

    for scenario in &evaluation.scenarios {
        assert_policy_stats_are_well_formed(scenario, &diagnostics);
        assert_availability_policy_invariants(scenario, &diagnostics);
    }

    for stats in aggregate_stats {
        assert!(
            (stats.stats.total_demand_credits() - aggregate_demand_credits).abs() < 0.000_001,
            "all policies should see the same total demand:\n{diagnostics}",
        );
    }

    assert!(
        aggregate_stats
            .iter()
            .any(|stats| stats.stats.user_unavailable_minutes() > 0),
        "realistic suite should include quota pressure that can exhaust every account:\n{diagnostics}",
    );
    assert!(
        drain_first.stats.preventable_failures > 0,
        "drain-first should expose cases where the selected account cannot serve while another account can:\n{diagnostics}",
    );
    assert_eq!(
        deadline_aware.stats.preventable_failures, 0,
        "deadline-aware should not miss a serviceable account:\n{diagnostics}",
    );
    assert_eq!(
        shadow_price.stats.preventable_failures, 0,
        "shadow-price should not miss a serviceable account:\n{diagnostics}",
    );
    assert_eq!(
        reset_weighted_minimax.stats.preventable_failures, 0,
        "reset-weighted-minimax should not miss a serviceable account:\n{diagnostics}",
    );
    for candidate in RUNTIME_REPLACEMENT_CANDIDATES {
        find_policy_stats(aggregate_stats, policy_name_for_kind(*candidate));
    }
}

#[test]
fn default_policy_replacement_requires_clear_availability_win() {
    let evaluation = realistic_usage_evaluation();
    let diagnostics = PolicyEvaluationDiagnostics(evaluation);

    assert_eq!(
        policy_name_for_kind(SelectionPolicyKind::default()),
        DEFAULT_POLICY_NAME
    );
    assert!(
        clear_default_replacement_candidate(evaluation).is_none(),
        "a non-default policy now clears the replacement gate; either update the default policy or tighten the gate:\n{diagnostics}",
    );
}

#[test]
fn runtime_replacement_candidates_are_explicit_simulated_non_default_policies() {
    let mut candidate_names = Vec::new();
    for candidate in RUNTIME_REPLACEMENT_CANDIDATES {
        let policy_name = policy_name_for_kind(*candidate);
        assert_ne!(
            policy_name, DEFAULT_POLICY_NAME,
            "default policy should not be a replacement candidate",
        );
        assert!(
            SIMULATED_POLICY_NAMES.contains(&policy_name),
            "{policy_name} must be included in simulated policy names",
        );
        assert!(
            !candidate_names.contains(&policy_name),
            "{policy_name} is duplicated in runtime replacement candidates",
        );
        candidate_names.push(policy_name);
    }
}

#[test]
fn default_replacement_ignores_non_runtime_policy_even_when_it_passes_gate() {
    let evaluation = PolicyEvaluation {
        aggregate_stats: [
            PolicyStats {
                policy_name: DEFAULT_POLICY_NAME,
                stats: gate_stats(1_000, 80, 500.0, 100, 0),
            },
            PolicyStats {
                policy_name: DRAIN_FIRST_POLICY_NAME,
                stats: gate_stats(900, 70, 500.0, 120, 0),
            },
            filler_policy_stats(MAX_HEADROOM_POLICY_NAME),
            filler_policy_stats(RESET_FIRST_POLICY_NAME),
            PolicyStats {
                policy_name: SHADOW_PRICE_POLICY_NAME,
                stats: gate_stats(1_000, 80, 500.0, 100, 0),
            },
            PolicyStats {
                policy_name: RESET_WEIGHTED_MINIMAX_POLICY_NAME,
                stats: gate_stats(1_000, 80, 500.0, 100, 0),
            },
            PolicyStats {
                policy_name: DEMAND_AWARE_HYSTERESIS_POLICY_NAME,
                stats: gate_stats(1_000, 80, 500.0, 100, 0),
            },
        ],
        scenarios: vec![ScenarioPolicyStats {
            scenario_name: "non-runtime-policy-wins",
            policy_stats: [
                PolicyStats {
                    policy_name: DEFAULT_POLICY_NAME,
                    stats: gate_stats(100, 20, 50.0, 10, 0),
                },
                PolicyStats {
                    policy_name: DRAIN_FIRST_POLICY_NAME,
                    stats: gate_stats(90, 20, 50.0, 10, 0),
                },
                filler_policy_stats(MAX_HEADROOM_POLICY_NAME),
                filler_policy_stats(RESET_FIRST_POLICY_NAME),
                PolicyStats {
                    policy_name: SHADOW_PRICE_POLICY_NAME,
                    stats: gate_stats(100, 20, 50.0, 10, 0),
                },
                PolicyStats {
                    policy_name: RESET_WEIGHTED_MINIMAX_POLICY_NAME,
                    stats: gate_stats(100, 20, 50.0, 10, 0),
                },
                PolicyStats {
                    policy_name: DEMAND_AWARE_HYSTERESIS_POLICY_NAME,
                    stats: gate_stats(100, 20, 50.0, 10, 0),
                },
            ],
        }],
    };

    let default_stats = find_policy_stats(&evaluation.aggregate_stats, DEFAULT_POLICY_NAME);
    let drain_first = find_policy_stats(&evaluation.aggregate_stats, DRAIN_FIRST_POLICY_NAME);
    assert!(
        ReplacementGate::default()
            .evaluate_aggregate(&default_stats.stats, &drain_first.stats)
            .is_pass()
    );
    assert_eq!(clear_default_replacement_candidate(&evaluation), None);
}

#[test]
fn replacement_gate_rejects_unavailable_improvement_with_preventable_failures() {
    let gate = ReplacementGate::default();
    let default = gate_stats(1_000, 80, 500.0, 100, 0);
    let candidate = gate_stats(900, 70, 500.0, 120, 1);

    assert_gate_failure_kind(
        gate.evaluate_aggregate(&default, &candidate),
        ReplacementGateFailureKind::PreventableFailuresPresent,
    );
}

#[test]
fn replacement_gate_rejects_unavailable_improvement_with_excessive_switches() {
    let gate = ReplacementGate::default();
    let default = gate_stats(1_000, 80, 500.0, 100, 0);
    let candidate = gate_stats(900, 70, 500.0, gate.switch_budget(100) + 1, 0);

    assert_gate_failure_kind(
        gate.evaluate_aggregate(&default, &candidate),
        ReplacementGateFailureKind::SwitchesExceeded,
    );
}

#[test]
fn replacement_gate_rejects_unavailable_improvement_with_longer_outage() {
    let gate = ReplacementGate::default();
    let default = gate_stats(1_000, 80, 500.0, 100, 0);
    let candidate = gate_stats(900, 81, 500.0, 120, 0);

    assert_gate_failure_kind(
        gate.evaluate_aggregate(&default, &candidate),
        ReplacementGateFailureKind::MaxContiguousUnavailableRegressed,
    );
}

#[test]
fn replacement_gate_rejects_unavailable_improvement_with_excessive_failed_credits() {
    let gate = ReplacementGate::default();
    let default = gate_stats(1_000, 80, 500.0, 100, 0);
    let candidate = gate_stats(900, 70, gate.failed_credits_budget(500.0) + 0.1, 120, 0);

    assert_gate_failure_kind(
        gate.evaluate_aggregate(&default, &candidate),
        ReplacementGateFailureKind::FailedCreditsExceeded,
    );
}

#[test]
fn replacement_gate_accepts_clear_improvement_with_guardrails() {
    let gate = ReplacementGate::default();
    let default = gate_stats(1_000, 80, 500.0, 100, 0);
    let candidate = gate_stats(900, 70, 500.0, 120, 0);

    assert_eq!(
        gate.evaluate_aggregate(&default, &candidate),
        ReplacementGateDecision::Pass
    );
}

#[test]
fn replacement_gate_allows_scenario_without_clear_improvement() {
    let gate = ReplacementGate::default();
    let default = gate_stats(1_000, 80, 500.0, 100, 0);
    let candidate = gate_stats(1_000, 70, 500.0, 120, 0);

    assert_eq!(
        gate.evaluate_scenario(&default, &candidate),
        ReplacementGateDecision::Pass
    );
    assert_gate_failure_kind(
        gate.evaluate_aggregate(&default, &candidate),
        ReplacementGateFailureKind::UnavailableReductionTooSmall,
    );
}

#[test]
fn replacement_gate_reports_aggregate_unavailable_regression() {
    let gate = ReplacementGate::default();
    let default = gate_stats(1_000, 80, 500.0, 100, 0);
    let candidate = gate_stats(1_001, 70, 500.0, 120, 0);

    assert_gate_failure_kind(
        gate.evaluate_aggregate(&default, &candidate),
        ReplacementGateFailureKind::UnavailableRegressed,
    );
}

#[test]
fn replacement_gate_summary_distinguishes_aggregate_and_scenario_pass() {
    let gate = ReplacementGate::default();
    let default = PolicyStats {
        policy_name: DEFAULT_POLICY_NAME,
        stats: gate_stats(1_000, 80, 500.0, 100, 0),
    };
    let aggregate_candidate = PolicyStats {
        policy_name: SHADOW_PRICE_POLICY_NAME,
        stats: gate_stats(900, 70, 500.0, 120, 0),
    };
    let scenario_candidate = PolicyStats {
        policy_name: SHADOW_PRICE_POLICY_NAME,
        stats: gate_stats(1_000, 70, 500.0, 120, 0),
    };

    assert_eq!(
        replacement_gate_summary(
            Some(&default),
            &aggregate_candidate,
            gate,
            ReplacementGateSummaryScope::Aggregate,
        ),
        "replacement_gate=pass scope=aggregate reason=unavailable reduction 100 meets required 50, guardrails passed"
    );
    assert_eq!(
        replacement_gate_summary(
            Some(&default),
            &scenario_candidate,
            gate,
            ReplacementGateSummaryScope::Scenario,
        ),
        "replacement_gate=pass scope=scenario reason=scenario guardrails passed"
    );
}

#[test]
fn replacement_gate_summary_includes_failure_scope() {
    let gate = ReplacementGate::default();
    let default = PolicyStats {
        policy_name: DEFAULT_POLICY_NAME,
        stats: gate_stats(1_000, 80, 500.0, 100, 0),
    };
    let candidate = PolicyStats {
        policy_name: SHADOW_PRICE_POLICY_NAME,
        stats: gate_stats(1_001, 80, 500.0, 100, 0),
    };

    assert_eq!(
        replacement_gate_summary(
            Some(&default),
            &candidate,
            gate,
            ReplacementGateSummaryScope::Aggregate,
        ),
        "replacement_gate=fail scope=aggregate reason=unavailable regressed from 1000 to 1001"
    );
    assert_eq!(
        replacement_gate_summary(
            Some(&default),
            &candidate,
            gate,
            ReplacementGateSummaryScope::Scenario,
        ),
        "replacement_gate=fail scope=scenario reason=unavailable regressed from 1000 to 1001"
    );
}

#[test]
fn replacement_gate_summary_includes_scope_for_non_decision_statuses() {
    let gate = ReplacementGate::default();
    let default = PolicyStats {
        policy_name: DEFAULT_POLICY_NAME,
        stats: gate_stats(1_000, 80, 500.0, 100, 0),
    };
    let baseline = PolicyStats {
        policy_name: DRAIN_FIRST_POLICY_NAME,
        stats: gate_stats(900, 70, 500.0, 120, 0),
    };

    assert_eq!(
        replacement_gate_summary(
            Some(&default),
            &default,
            gate,
            ReplacementGateSummaryScope::Aggregate,
        ),
        "replacement_gate=default scope=aggregate reason=current default policy"
    );
    assert_eq!(
        replacement_gate_summary(
            Some(&default),
            &baseline,
            gate,
            ReplacementGateSummaryScope::Scenario,
        ),
        "replacement_gate=not-candidate scope=scenario reason=not in runtime replacement candidates"
    );
    assert_eq!(
        replacement_gate_summary(
            None,
            &PolicyStats {
                policy_name: SHADOW_PRICE_POLICY_NAME,
                stats: gate_stats(900, 70, 500.0, 120, 0),
            },
            gate,
            ReplacementGateSummaryScope::Aggregate,
        ),
        "replacement_gate=unknown scope=aggregate reason=missing default policy"
    );
}

#[test]
fn replacement_gate_rejects_candidate_when_any_scenario_regresses() {
    let evaluation = PolicyEvaluation {
        aggregate_stats: [
            PolicyStats {
                policy_name: DEFAULT_POLICY_NAME,
                stats: gate_stats(1_000, 80, 500.0, 100, 0),
            },
            PolicyStats {
                policy_name: SHADOW_PRICE_POLICY_NAME,
                stats: gate_stats(900, 70, 500.0, 120, 0),
            },
            filler_policy_stats(DRAIN_FIRST_POLICY_NAME),
            filler_policy_stats(MAX_HEADROOM_POLICY_NAME),
            filler_policy_stats(RESET_FIRST_POLICY_NAME),
            filler_policy_stats(RESET_WEIGHTED_MINIMAX_POLICY_NAME),
            filler_policy_stats(DEMAND_AWARE_HYSTERESIS_POLICY_NAME),
        ],
        scenarios: vec![ScenarioPolicyStats {
            scenario_name: "regressed-scenario",
            policy_stats: [
                PolicyStats {
                    policy_name: DEFAULT_POLICY_NAME,
                    stats: gate_stats(100, 20, 50.0, 10, 0),
                },
                PolicyStats {
                    policy_name: SHADOW_PRICE_POLICY_NAME,
                    stats: gate_stats(101, 20, 50.0, 10, 0),
                },
                filler_policy_stats(DRAIN_FIRST_POLICY_NAME),
                filler_policy_stats(MAX_HEADROOM_POLICY_NAME),
                filler_policy_stats(RESET_FIRST_POLICY_NAME),
                filler_policy_stats(RESET_WEIGHTED_MINIMAX_POLICY_NAME),
                filler_policy_stats(DEMAND_AWARE_HYSTERESIS_POLICY_NAME),
            ],
        }],
    };

    match ReplacementGate::default().evaluate_candidate(&evaluation, SHADOW_PRICE_POLICY_NAME) {
        ReplacementGateDecision::Fail(failure) => {
            assert_eq!(failure.kind, ReplacementGateFailureKind::ScenarioRegressed);
            assert_eq!(
                failure.reason,
                "regressed-scenario: unavailable regressed from 100 to 101"
            );
        }
        ReplacementGateDecision::Pass => panic!("replacement gate should fail on scenario regress"),
    }
}

#[test]
fn replacement_gate_tie_breaks_between_passing_candidates() {
    let gate = ReplacementGate::default();
    let default = gate_stats(1_000, 80, 500.0, 100, 0);
    let better = PolicyStats {
        policy_name: "better",
        stats: gate_stats(880, 75, 500.0, 120, 0),
    };
    let worse = PolicyStats {
        policy_name: "worse",
        stats: gate_stats(900, 70, 500.0, 120, 0),
    };
    assert!(gate.evaluate_aggregate(&default, &better.stats).is_pass());
    assert!(gate.evaluate_aggregate(&default, &worse.stats).is_pass());

    let selected = [worse, better]
        .into_iter()
        .min_by(ReplacementGate::compare_passing_candidates)
        .expect("passing candidate list should not be empty");

    assert_eq!(selected.policy_name, "better");
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
fn initial_usage_reset_times_ignore_empty_stagger_placeholders() {
    let mut account = SimAccount::new("account-1", 1, 2);
    account.seed_initial_usage(100.0, 300.0, 240, 4_000);
    let usage = account.usage_info(0);

    assert_eq!(usage.primary_resets_at, Some(240 * 60));
    assert_eq!(usage.secondary_resets_at, Some(4_000 * 60));
}

#[test]
fn consumed_usage_reset_times_ignore_empty_stagger_placeholders() {
    let mut account = SimAccount::new("account-1", 1, 2);
    account.consume(0, 100.0);
    let usage = account.usage_info(0);

    assert_eq!(usage.primary_resets_at, Some(FIVE_HOUR_WINDOW_MINUTES * 60));
    assert_eq!(usage.secondary_resets_at, Some(WEEKLY_WINDOW_MINUTES * 60));
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
        (
            "demand-aware-hysteresis",
            simulate_policy_trace(&mut DemandAwareHysteresisPolicy::default(), &trace, limits),
        ),
    ];

    for (policy_name, outcome) in policy_outcomes {
        assert!(
            oracle.is_at_least_as_good_as(outcome),
            "{policy_name} exceeded the offline oracle: {outcome:?} vs {oracle:?}",
        );
    }
}

const FIVE_HOUR_WINDOW_MINUTES: i64 = 300;
const WEEKLY_WINDOW_MINUTES: i64 = 10_080;
const SIMULATION_WEEKS: i64 = 2;
const SIMULATION_MINUTES: i64 = WEEKLY_WINDOW_MINUTES * SIMULATION_WEEKS;
const PRO_200_FIVE_HOUR_LIMIT_CREDITS: f64 = 1_250.0;
const PRO_200_WEEKLY_LIMIT_CREDITS: f64 = 5_000.0;

#[derive(Debug, Clone, Copy)]
struct SimScenario {
    name: &'static str,
    account_count: usize,
    demand: SimDemand,
    initial_usage: &'static [InitialUsageSeed],
}

impl SimScenario {
    fn new(
        name: &'static str,
        account_count: usize,
        demand: SimDemand,
        initial_usage: &'static [InitialUsageSeed],
    ) -> Self {
        Self {
            name,
            account_count,
            demand,
            initial_usage,
        }
    }

    fn demand_at(self, minute: i64) -> Option<f64> {
        if !is_coding_minute(minute) {
            return None;
        }

        let credits_per_minute = match self.demand {
            SimDemand::WorkdayProfile(profile) => profile.credits_at(minute),
            SimDemand::DeadlineRamp {
                normal_profile,
                deadline_profile,
                deadline_start_day,
            } => {
                if minute / 1_440 >= deadline_start_day {
                    deadline_profile.credits_at(minute)
                } else {
                    normal_profile.credits_at(minute)
                }
            }
            SimDemand::InterruptDriven {
                profile,
                interrupt_credits_per_minute,
                interrupt_every_minutes,
                interrupt_duration_minutes,
            } => {
                if is_interrupt_minute(minute, interrupt_every_minutes, interrupt_duration_minutes)
                {
                    interrupt_credits_per_minute
                } else {
                    profile.credits_at(minute)
                }
            }
        };

        Some(credits_per_minute * weekday_load_multiplier(minute))
    }
}

#[derive(Debug, Clone, Copy)]
enum SimDemand {
    WorkdayProfile(WorkloadProfile),
    DeadlineRamp {
        normal_profile: WorkloadProfile,
        deadline_profile: WorkloadProfile,
        deadline_start_day: i64,
    },
    InterruptDriven {
        profile: WorkloadProfile,
        interrupt_credits_per_minute: f64,
        interrupt_every_minutes: i64,
        interrupt_duration_minutes: i64,
    },
}

#[derive(Debug, Clone, Copy)]
struct WorkloadProfile {
    baseline_credits_per_minute: f64,
    focus_credits_per_minute: f64,
    review_credits_per_minute: f64,
    burst_credits_per_minute: f64,
}

impl WorkloadProfile {
    fn credits_at(self, minute: i64) -> f64 {
        let minute_of_day = minute % 1_440;

        if is_prompt_burst_minute(minute) {
            return self.burst_credits_per_minute;
        }

        if (615..705).contains(&minute_of_day) || (810..930).contains(&minute_of_day) {
            self.focus_credits_per_minute
        } else if (570..615).contains(&minute_of_day) || (960..1_020).contains(&minute_of_day) {
            self.review_credits_per_minute
        } else {
            self.baseline_credits_per_minute
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct InitialUsageSeed {
    account_index: usize,
    five_hour_credits: f64,
    weekly_credits: f64,
    five_hour_resets_at_minute: i64,
    weekly_resets_at_minute: i64,
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

    fn merge(&mut self, other: Self) {
        self.served_credits += other.served_credits;
        self.failed_credits += other.failed_credits;
        self.preventable_failures += other.preventable_failures;
        self.unavailable_minutes += other.unavailable_minutes;
        self.max_contiguous_unavailable_minutes = self
            .max_contiguous_unavailable_minutes
            .max(other.max_contiguous_unavailable_minutes);
        self.account_switches += other.account_switches;
        self.min_five_hour_remaining = self
            .min_five_hour_remaining
            .min(other.min_five_hour_remaining);
        self.min_weekly_remaining = self.min_weekly_remaining.min(other.min_weekly_remaining);
    }
}

#[derive(Debug, Clone, Copy)]
struct PolicyStats {
    policy_name: &'static str,
    stats: SimStats,
}

#[derive(Debug)]
struct ScenarioPolicyStats {
    scenario_name: &'static str,
    policy_stats: [PolicyStats; POLICY_COUNT],
}

#[derive(Debug)]
struct PolicyEvaluation {
    scenarios: Vec<ScenarioPolicyStats>,
    aggregate_stats: [PolicyStats; POLICY_COUNT],
}

struct PolicyEvaluationDiagnostics<'a>(&'a PolicyEvaluation);

impl fmt::Display for PolicyEvaluationDiagnostics<'_> {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_policy_stats_table(
            output,
            "aggregate",
            &self.0.aggregate_stats,
            ReplacementGateSummaryScope::Aggregate,
        )?;
        for scenario in &self.0.scenarios {
            write_policy_stats_table(
                output,
                scenario.scenario_name,
                &scenario.policy_stats,
                ReplacementGateSummaryScope::Scenario,
            )?;
        }
        Ok(())
    }
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
    let mut last_account_index: Option<usize> = None;
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

const NO_INITIAL_USAGE: &[InitialUsageSeed] = &[];

const CARRYOVER_FROM_PREVIOUS_SESSIONS_2: &[InitialUsageSeed] = &[
    InitialUsageSeed {
        account_index: 0,
        five_hour_credits: 450.0,
        weekly_credits: 2_700.0,
        five_hour_resets_at_minute: 180,
        weekly_resets_at_minute: 3 * 1_440,
    },
    InitialUsageSeed {
        account_index: 1,
        five_hour_credits: 120.0,
        weekly_credits: 1_100.0,
        five_hour_resets_at_minute: 260,
        weekly_resets_at_minute: 5 * 1_440,
    },
];

const CARRYOVER_FROM_PREVIOUS_SESSIONS_3: &[InitialUsageSeed] = &[
    InitialUsageSeed {
        account_index: 0,
        five_hour_credits: 450.0,
        weekly_credits: 2_700.0,
        five_hour_resets_at_minute: 180,
        weekly_resets_at_minute: 3 * 1_440,
    },
    InitialUsageSeed {
        account_index: 1,
        five_hour_credits: 120.0,
        weekly_credits: 1_100.0,
        five_hour_resets_at_minute: 260,
        weekly_resets_at_minute: 5 * 1_440,
    },
    InitialUsageSeed {
        account_index: 2,
        five_hour_credits: 300.0,
        weekly_credits: 1_900.0,
        five_hour_resets_at_minute: 90,
        weekly_resets_at_minute: 4 * 1_440,
    },
];

const FIVE_HOUR_CARRYOVER_MIXED: &[InitialUsageSeed] = &[
    InitialUsageSeed {
        account_index: 0,
        five_hour_credits: 950.0,
        weekly_credits: 1_700.0,
        five_hour_resets_at_minute: 45,
        weekly_resets_at_minute: 6 * 1_440,
    },
    InitialUsageSeed {
        account_index: 1,
        five_hour_credits: 175.0,
        weekly_credits: 3_100.0,
        five_hour_resets_at_minute: 230,
        weekly_resets_at_minute: 4 * 1_440,
    },
];

const WEEKLY_NEAR_RESET_MIXED: &[InitialUsageSeed] = &[
    InitialUsageSeed {
        account_index: 0,
        five_hour_credits: 25.0,
        weekly_credits: 4_600.0,
        five_hour_resets_at_minute: 200,
        weekly_resets_at_minute: 2 * 1_440,
    },
    InitialUsageSeed {
        account_index: 1,
        five_hour_credits: 75.0,
        weekly_credits: 300.0,
        five_hour_resets_at_minute: 150,
        weekly_resets_at_minute: 5 * 1_440,
    },
];

const ALL_ACCOUNTS_NEAR_SOFT_THRESHOLD: &[InitialUsageSeed] = &[
    InitialUsageSeed {
        account_index: 0,
        five_hour_credits: 1_185.0,
        weekly_credits: 3_650.0,
        five_hour_resets_at_minute: 35,
        weekly_resets_at_minute: 3 * 1_440,
    },
    InitialUsageSeed {
        account_index: 1,
        five_hour_credits: 1_210.0,
        weekly_credits: 3_300.0,
        five_hour_resets_at_minute: 165,
        weekly_resets_at_minute: 5 * 1_440,
    },
    InitialUsageSeed {
        account_index: 2,
        five_hour_credits: 1_150.0,
        weekly_credits: 3_900.0,
        five_hour_resets_at_minute: 250,
        weekly_resets_at_minute: 6 * 1_440,
    },
];

const WEEKLY_BOTTLENECK_WITH_FIVE_HOUR_HEADROOM: &[InitialUsageSeed] = &[
    InitialUsageSeed {
        account_index: 0,
        five_hour_credits: 90.0,
        weekly_credits: 4_650.0,
        five_hour_resets_at_minute: 240,
        weekly_resets_at_minute: 90,
    },
    InitialUsageSeed {
        account_index: 1,
        five_hour_credits: 720.0,
        weekly_credits: 900.0,
        five_hour_resets_at_minute: 150,
        weekly_resets_at_minute: 5 * 1_440,
    },
];

const THREE_ACCOUNT_WEEKLY_PRESSURE: &[InitialUsageSeed] = &[
    InitialUsageSeed {
        account_index: 0,
        five_hour_credits: 320.0,
        weekly_credits: 4_500.0,
        five_hour_resets_at_minute: 200,
        weekly_resets_at_minute: 2 * 1_440,
    },
    InitialUsageSeed {
        account_index: 1,
        five_hour_credits: 350.0,
        weekly_credits: 4_300.0,
        five_hour_resets_at_minute: 280,
        weekly_resets_at_minute: 3 * 1_440,
    },
    InitialUsageSeed {
        account_index: 2,
        five_hour_credits: 100.0,
        weekly_credits: 800.0,
        five_hour_resets_at_minute: 80,
        weekly_resets_at_minute: 6 * 1_440,
    },
];

fn realistic_usage_scenarios() -> [SimScenario; 12] {
    [
        SimScenario::new(
            "regular-feature-work",
            2,
            SimDemand::WorkdayProfile(WorkloadProfile {
                baseline_credits_per_minute: 0.9,
                focus_credits_per_minute: 2.4,
                review_credits_per_minute: 1.5,
                burst_credits_per_minute: 5.0,
            }),
            NO_INITIAL_USAGE,
        ),
        SimScenario::new(
            "large-refactor-with-turn-spikes",
            2,
            SimDemand::WorkdayProfile(WorkloadProfile {
                baseline_credits_per_minute: 2.6,
                focus_credits_per_minute: 7.8,
                review_credits_per_minute: 4.0,
                burst_credits_per_minute: 13.0,
            }),
            NO_INITIAL_USAGE,
        ),
        SimScenario::new(
            "release-week-ramp",
            2,
            SimDemand::DeadlineRamp {
                normal_profile: WorkloadProfile {
                    baseline_credits_per_minute: 0.9,
                    focus_credits_per_minute: 2.8,
                    review_credits_per_minute: 1.8,
                    burst_credits_per_minute: 5.5,
                },
                deadline_profile: WorkloadProfile {
                    baseline_credits_per_minute: 4.0,
                    focus_credits_per_minute: 12.0,
                    review_credits_per_minute: 6.0,
                    burst_credits_per_minute: 18.0,
                },
                deadline_start_day: 7,
            },
            NO_INITIAL_USAGE,
        ),
        SimScenario::new(
            "carryover-from-previous-codex-sessions",
            2,
            SimDemand::WorkdayProfile(WorkloadProfile {
                baseline_credits_per_minute: 1.3,
                focus_credits_per_minute: 4.1,
                review_credits_per_minute: 2.2,
                burst_credits_per_minute: 7.5,
            }),
            CARRYOVER_FROM_PREVIOUS_SESSIONS_2,
        ),
        SimScenario::new(
            "mixed-five-hour-carryover",
            2,
            SimDemand::WorkdayProfile(WorkloadProfile {
                baseline_credits_per_minute: 1.4,
                focus_credits_per_minute: 4.6,
                review_credits_per_minute: 2.5,
                burst_credits_per_minute: 8.0,
            }),
            FIVE_HOUR_CARRYOVER_MIXED,
        ),
        SimScenario::new(
            "weekly-near-reset-carryover",
            2,
            SimDemand::WorkdayProfile(WorkloadProfile {
                baseline_credits_per_minute: 1.2,
                focus_credits_per_minute: 3.5,
                review_credits_per_minute: 2.1,
                burst_credits_per_minute: 6.5,
            }),
            WEEKLY_NEAR_RESET_MIXED,
        ),
        SimScenario::new(
            "support-interruptions-during-feature-work",
            3,
            SimDemand::InterruptDriven {
                profile: WorkloadProfile {
                    baseline_credits_per_minute: 1.0,
                    focus_credits_per_minute: 3.4,
                    review_credits_per_minute: 1.8,
                    burst_credits_per_minute: 6.5,
                },
                interrupt_credits_per_minute: 8.0,
                interrupt_every_minutes: 180,
                interrupt_duration_minutes: 18,
            },
            NO_INITIAL_USAGE,
        ),
        SimScenario::new(
            "three-account-heavy-project-week",
            3,
            SimDemand::DeadlineRamp {
                normal_profile: WorkloadProfile {
                    baseline_credits_per_minute: 2.0,
                    focus_credits_per_minute: 6.0,
                    review_credits_per_minute: 3.2,
                    burst_credits_per_minute: 11.0,
                },
                deadline_profile: WorkloadProfile {
                    baseline_credits_per_minute: 5.0,
                    focus_credits_per_minute: 14.0,
                    review_credits_per_minute: 7.0,
                    burst_credits_per_minute: 20.0,
                },
                deadline_start_day: 7,
            },
            CARRYOVER_FROM_PREVIOUS_SESSIONS_3,
        ),
        SimScenario::new(
            "sustained-boundary-load",
            2,
            SimDemand::WorkdayProfile(WorkloadProfile {
                baseline_credits_per_minute: 1.8,
                focus_credits_per_minute: 5.4,
                review_credits_per_minute: 3.0,
                burst_credits_per_minute: 9.5,
            }),
            NO_INITIAL_USAGE,
        ),
        SimScenario::new(
            "soft-threshold-recovery",
            3,
            SimDemand::WorkdayProfile(WorkloadProfile {
                baseline_credits_per_minute: 0.8,
                focus_credits_per_minute: 2.6,
                review_credits_per_minute: 1.4,
                burst_credits_per_minute: 5.0,
            }),
            ALL_ACCOUNTS_NEAR_SOFT_THRESHOLD,
        ),
        SimScenario::new(
            "weekly-bottleneck-with-five-hour-headroom",
            2,
            SimDemand::WorkdayProfile(WorkloadProfile {
                baseline_credits_per_minute: 1.1,
                focus_credits_per_minute: 3.8,
                review_credits_per_minute: 2.0,
                burst_credits_per_minute: 7.0,
            }),
            WEEKLY_BOTTLENECK_WITH_FIVE_HOUR_HEADROOM,
        ),
        SimScenario::new(
            "three-account-weekly-pressure",
            3,
            SimDemand::WorkdayProfile(WorkloadProfile {
                baseline_credits_per_minute: 1.4,
                focus_credits_per_minute: 4.4,
                review_credits_per_minute: 2.4,
                burst_credits_per_minute: 8.0,
            }),
            THREE_ACCOUNT_WEEKLY_PRESSURE,
        ),
    ]
}

fn policy_stats_for_scenario(scenario: SimScenario) -> [PolicyStats; POLICY_COUNT] {
    validate_sim_scenario(scenario);

    let mut deadline_aware = DeadlineAwarePolicy::default();
    let mut drain_first = DrainFirstPolicy::new(SelectionConfig::default());
    let mut max_headroom = MaxHeadroomPolicy::new(SelectionConfig::default());
    let mut reset_first = ResetFirstPolicy::new(SelectionConfig::default());
    let mut shadow_price = ShadowPricePolicy::default();
    let mut reset_weighted_minimax = ResetWeightedMinimaxPolicy::default();
    let mut demand_aware_hysteresis = DemandAwareHysteresisPolicy::default();

    let policy_stats = [
        PolicyStats {
            policy_name: DEADLINE_AWARE_POLICY_NAME,
            stats: simulate_policy_scenario(&mut deadline_aware, scenario),
        },
        PolicyStats {
            policy_name: DRAIN_FIRST_POLICY_NAME,
            stats: simulate_policy_scenario(&mut drain_first, scenario),
        },
        PolicyStats {
            policy_name: MAX_HEADROOM_POLICY_NAME,
            stats: simulate_policy_scenario(&mut max_headroom, scenario),
        },
        PolicyStats {
            policy_name: RESET_FIRST_POLICY_NAME,
            stats: simulate_policy_scenario(&mut reset_first, scenario),
        },
        PolicyStats {
            policy_name: SHADOW_PRICE_POLICY_NAME,
            stats: simulate_policy_scenario(&mut shadow_price, scenario),
        },
        PolicyStats {
            policy_name: RESET_WEIGHTED_MINIMAX_POLICY_NAME,
            stats: simulate_policy_scenario(&mut reset_weighted_minimax, scenario),
        },
        PolicyStats {
            policy_name: DEMAND_AWARE_HYSTERESIS_POLICY_NAME,
            stats: simulate_policy_scenario(&mut demand_aware_hysteresis, scenario),
        },
    ];
    assert_eq!(
        policy_stats.map(|stats| stats.policy_name).as_slice(),
        SIMULATED_POLICY_NAMES,
    );
    policy_stats
}

fn validate_sim_scenario(scenario: SimScenario) {
    assert!(
        !scenario.name.is_empty(),
        "simulated usage scenarios must have a name",
    );
    assert!(
        scenario.account_count > 0,
        "{} must include at least one account",
        scenario.name,
    );
    let mut seeded_accounts = vec![false; scenario.account_count];
    for seed in scenario.initial_usage {
        validate_initial_usage_seed(seed, scenario.account_count);
        assert!(
            !seeded_accounts[seed.account_index],
            "{} has duplicate initial usage for account {}",
            scenario.name, seed.account_index,
        );
        seeded_accounts[seed.account_index] = true;
    }

    match scenario.demand {
        SimDemand::WorkdayProfile(profile) => {
            validate_workload_profile(scenario.name, "workday", profile);
        }
        SimDemand::DeadlineRamp {
            normal_profile,
            deadline_profile,
            deadline_start_day,
        } => {
            validate_workload_profile(scenario.name, "normal", normal_profile);
            validate_workload_profile(scenario.name, "deadline", deadline_profile);
            assert!(
                (0..SIMULATION_WEEKS * 7).contains(&deadline_start_day),
                "{} deadline start day must fall inside the simulation",
                scenario.name,
            );
        }
        SimDemand::InterruptDriven {
            profile,
            interrupt_credits_per_minute,
            interrupt_every_minutes,
            interrupt_duration_minutes,
        } => {
            validate_workload_profile(scenario.name, "interrupt-base", profile);
            validate_credit_rate(
                scenario.name,
                "interrupt",
                "interrupt_credits_per_minute",
                interrupt_credits_per_minute,
            );
            assert!(
                interrupt_every_minutes > 0,
                "{} interrupt interval must be positive",
                scenario.name,
            );
            assert!(
                (0..=interrupt_every_minutes).contains(&interrupt_duration_minutes),
                "{} interrupt duration must stay within the interrupt interval",
                scenario.name,
            );
        }
    }
}

fn validate_workload_profile(scenario_name: &str, profile_name: &str, profile: WorkloadProfile) {
    validate_credit_rate(
        scenario_name,
        profile_name,
        "baseline_credits_per_minute",
        profile.baseline_credits_per_minute,
    );
    validate_credit_rate(
        scenario_name,
        profile_name,
        "focus_credits_per_minute",
        profile.focus_credits_per_minute,
    );
    validate_credit_rate(
        scenario_name,
        profile_name,
        "review_credits_per_minute",
        profile.review_credits_per_minute,
    );
    validate_credit_rate(
        scenario_name,
        profile_name,
        "burst_credits_per_minute",
        profile.burst_credits_per_minute,
    );
}

fn validate_credit_rate(
    scenario_name: &str,
    profile_name: &str,
    field_name: &str,
    credits_per_minute: f64,
) {
    assert!(
        credits_per_minute.is_finite() && credits_per_minute >= 0.0,
        "{scenario_name} {profile_name} {field_name} must be finite and non-negative",
    );
}

fn merge_policy_stats(
    aggregate_stats: &mut Option<[PolicyStats; POLICY_COUNT]>,
    scenario_stats: [PolicyStats; POLICY_COUNT],
) {
    let Some(aggregate_stats) = aggregate_stats else {
        *aggregate_stats = Some(scenario_stats);
        return;
    };

    for (aggregate, scenario) in aggregate_stats.iter_mut().zip(scenario_stats) {
        assert_eq!(aggregate.policy_name, scenario.policy_name);
        aggregate.stats.merge(scenario.stats);
    }
}

fn realistic_usage_evaluation() -> &'static PolicyEvaluation {
    static EVALUATION: OnceLock<PolicyEvaluation> = OnceLock::new();
    EVALUATION.get_or_init(build_realistic_usage_evaluation)
}

fn build_realistic_usage_evaluation() -> PolicyEvaluation {
    let mut scenarios = Vec::new();
    let mut aggregate_stats = None;

    for scenario in realistic_usage_scenarios() {
        let policy_stats = policy_stats_for_scenario(scenario);
        merge_policy_stats(&mut aggregate_stats, policy_stats);
        scenarios.push(ScenarioPolicyStats {
            scenario_name: scenario.name,
            policy_stats,
        });
    }

    PolicyEvaluation {
        scenarios,
        aggregate_stats: aggregate_stats.expect("realistic scenario suite should not be empty"),
    }
}

fn find_policy_stats<'a>(policy_stats: &'a [PolicyStats], policy_name: &str) -> &'a PolicyStats {
    policy_stats
        .iter()
        .find(|stats| stats.policy_name == policy_name)
        .unwrap_or_else(|| panic!("{policy_name} stats should be present"))
}

fn assert_policy_stats_are_well_formed(
    scenario: &ScenarioPolicyStats,
    diagnostics: &PolicyEvaluationDiagnostics<'_>,
) {
    let demand_credits = scenario.policy_stats[0].stats.total_demand_credits();
    for stats in &scenario.policy_stats {
        assert!(
            stats.stats.total_demand_credits() > 0.0,
            "{} should generate demand for {}:\n{diagnostics}",
            scenario.scenario_name,
            stats.policy_name
        );
        assert!(
            (stats.stats.total_demand_credits() - demand_credits).abs() < 0.000_001,
            "all policies should see the same demand in {}:\n{diagnostics}",
            scenario.scenario_name
        );
        assert!(
            stats.stats.min_five_hour_remaining.is_finite(),
            "{} should track 5-hour remaining for {}:\n{diagnostics}",
            scenario.scenario_name,
            stats.policy_name
        );
        assert!(
            stats.stats.min_weekly_remaining.is_finite(),
            "{} should track weekly remaining for {}:\n{diagnostics}",
            scenario.scenario_name,
            stats.policy_name
        );
    }
}

fn assert_availability_policy_invariants(
    scenario: &ScenarioPolicyStats,
    diagnostics: &PolicyEvaluationDiagnostics<'_>,
) {
    let deadline_aware = find_policy_stats(&scenario.policy_stats, DEADLINE_AWARE_POLICY_NAME);
    let shadow_price = find_policy_stats(&scenario.policy_stats, SHADOW_PRICE_POLICY_NAME);
    let reset_weighted_minimax =
        find_policy_stats(&scenario.policy_stats, RESET_WEIGHTED_MINIMAX_POLICY_NAME);
    assert_eq!(
        deadline_aware.stats.preventable_failures, 0,
        "deadline-aware should not miss a serviceable account in {}:\n{diagnostics}",
        scenario.scenario_name
    );
    assert_eq!(
        shadow_price.stats.preventable_failures, 0,
        "shadow-price should not miss a serviceable account in {}:\n{diagnostics}",
        scenario.scenario_name
    );
    assert_eq!(
        reset_weighted_minimax.stats.preventable_failures, 0,
        "reset-weighted-minimax should not miss a serviceable account in {}:\n{diagnostics}",
        scenario.scenario_name
    );
}

fn clear_default_replacement_candidate(evaluation: &PolicyEvaluation) -> Option<&'static str> {
    let gate = ReplacementGate::default();
    evaluation
        .aggregate_stats
        .iter()
        .filter(|stats| is_runtime_replacement_candidate(stats.policy_name))
        .copied()
        .filter(|stats| {
            gate.evaluate_candidate(evaluation, stats.policy_name)
                .is_pass()
        })
        .min_by(ReplacementGate::compare_passing_candidates)
        .map(|stats| stats.policy_name)
}

fn compare_policy_outcome(left: &SimStats, right: &SimStats) -> Ordering {
    left.user_unavailable_minutes()
        .cmp(&right.user_unavailable_minutes())
        .then_with(|| {
            left.max_contiguous_unavailable_minutes
                .cmp(&right.max_contiguous_unavailable_minutes)
        })
        .then_with(|| left.failed_credits.total_cmp(&right.failed_credits))
        .then_with(|| left.preventable_failures.cmp(&right.preventable_failures))
        .then_with(|| left.account_switches.cmp(&right.account_switches))
}

fn write_policy_stats_table(
    output: &mut fmt::Formatter<'_>,
    label: &str,
    policy_stats: &[PolicyStats],
    gate_scope: ReplacementGateSummaryScope,
) -> fmt::Result {
    let gate = ReplacementGate::default();
    let default_stats = policy_stats
        .iter()
        .find(|stats| stats.policy_name == DEFAULT_POLICY_NAME);
    writeln!(output, "{label}:")?;
    for stats in policy_stats {
        let gate_summary = replacement_gate_summary(default_stats, stats, gate, gate_scope);
        writeln!(
            output,
            "  {}: unavailable={} max_unavailable={} failed={:.1} switches={} preventable={} min_5h={:.1} min_weekly={:.1} {}",
            stats.policy_name,
            stats.stats.user_unavailable_minutes(),
            stats.stats.max_contiguous_unavailable_minutes,
            stats.stats.failed_credits,
            stats.stats.account_switches,
            stats.stats.preventable_failures,
            stats.stats.min_five_hour_remaining,
            stats.stats.min_weekly_remaining,
            gate_summary,
        )?;
    }
    Ok(())
}

fn replacement_gate_summary(
    default_stats: Option<&PolicyStats>,
    stats: &PolicyStats,
    gate: ReplacementGate,
    gate_scope: ReplacementGateSummaryScope,
) -> String {
    if stats.policy_name == DEFAULT_POLICY_NAME {
        return format!(
            "replacement_gate=default scope={} reason=current default policy",
            gate_scope.label()
        );
    }
    if !is_runtime_replacement_candidate(stats.policy_name) {
        return format!(
            "replacement_gate=not-candidate scope={} reason=not in runtime replacement candidates",
            gate_scope.label()
        );
    }

    let Some(default_stats) = default_stats else {
        return format!(
            "replacement_gate=unknown scope={} reason=missing default policy",
            gate_scope.label()
        );
    };

    let decision = match gate_scope {
        ReplacementGateSummaryScope::Aggregate => {
            gate.evaluate_aggregate(&default_stats.stats, &stats.stats)
        }
        ReplacementGateSummaryScope::Scenario => {
            gate.evaluate_scenario(&default_stats.stats, &stats.stats)
        }
    };

    match (decision, gate_scope) {
        (ReplacementGateDecision::Pass, ReplacementGateSummaryScope::Aggregate) => {
            let default_unavailable = default_stats.stats.user_unavailable_minutes();
            let reduction =
                default_unavailable.saturating_sub(stats.stats.user_unavailable_minutes());
            let required = gate.min_unavailable_reduction(default_unavailable);
            format!(
                "replacement_gate=pass scope=aggregate reason=unavailable reduction {reduction} meets required {required}, guardrails passed"
            )
        }
        (ReplacementGateDecision::Pass, ReplacementGateSummaryScope::Scenario) => {
            "replacement_gate=pass scope=scenario reason=scenario guardrails passed".to_string()
        }
        (ReplacementGateDecision::Fail(failure), _) => {
            format!(
                "replacement_gate=fail scope={} reason={}",
                gate_scope.label(),
                failure.reason
            )
        }
    }
}

fn policy_name_for_kind(kind: SelectionPolicyKind) -> &'static str {
    match kind {
        SelectionPolicyKind::DeadlineAware => DEADLINE_AWARE_POLICY_NAME,
        SelectionPolicyKind::ShadowPrice => SHADOW_PRICE_POLICY_NAME,
        SelectionPolicyKind::ResetWeightedMinimax => RESET_WEIGHTED_MINIMAX_POLICY_NAME,
        SelectionPolicyKind::DemandAwareHysteresis => DEMAND_AWARE_HYSTERESIS_POLICY_NAME,
    }
}

fn is_runtime_replacement_candidate(policy_name: &str) -> bool {
    RUNTIME_REPLACEMENT_CANDIDATES
        .iter()
        .any(|candidate| policy_name_for_kind(*candidate) == policy_name)
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
    let accounts: Vec<StoredAccount> = (0..limits.account_count)
        .map(|index| chatgpt_account(&format!("trace-account-{index}"), None))
        .collect::<Vec<_>>();
    let mut states = vec![TraceAccountState::default(); limits.account_count];
    let mut outcome = TraceOutcome::default();
    let mut last_account_index: Option<usize> = None;

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
        let current_account_id = last_account_index
            .and_then(|index| accounts.get(index))
            .map(|account| account.id.as_str());
        let selected_index = policy
            .select_account_at(
                &candidates,
                SelectionContext::at(i64::from(demand.minute) * 60)
                    .with_current_account_id(current_account_id),
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
            last_account_index = Some(index);
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
        limits.weekly_limit >= limits.five_hour_limit,
        "trace weekly limit must be at least the 5-hour limit",
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
        limits.weekly_window >= limits.five_hour_window,
        "trace weekly window must be at least the 5-hour window",
    );
    assert!(
        trace
            .windows(2)
            .all(|pair| pair[0].minute <= pair[1].minute),
        "trace demand must be sorted by minute",
    );
}

fn apply_initial_usage(accounts: &mut [SimAccount], initial_usage: &[InitialUsageSeed]) {
    let account_count = accounts.len();
    for seed in initial_usage {
        validate_initial_usage_seed(seed, account_count);
        let account = &mut accounts[seed.account_index];
        account.seed_initial_usage(
            seed.five_hour_credits,
            seed.weekly_credits,
            seed.five_hour_resets_at_minute,
            seed.weekly_resets_at_minute,
        );
    }
}

fn validate_initial_usage_seed(seed: &InitialUsageSeed, account_count: usize) {
    assert!(
        seed.account_index < account_count,
        "initial usage seed references account {} but scenario has {account_count} accounts",
        seed.account_index
    );
    assert!(
        seed.five_hour_credits >= 0.0 && seed.five_hour_credits.is_finite(),
        "initial 5-hour credits must be finite and non-negative",
    );
    assert!(
        seed.five_hour_credits <= PRO_200_FIVE_HOUR_LIMIT_CREDITS,
        "initial 5-hour credits must not exceed the simulated account limit",
    );
    assert!(
        seed.weekly_credits >= 0.0 && seed.weekly_credits.is_finite(),
        "initial weekly credits must be finite and non-negative",
    );
    assert!(
        seed.weekly_credits <= PRO_200_WEEKLY_LIMIT_CREDITS,
        "initial weekly credits must not exceed the simulated account limit",
    );
    assert!(
        seed.weekly_credits + f64::EPSILON >= seed.five_hour_credits,
        "initial weekly credits must include the simulated 5-hour credits",
    );
    assert!(
        (0..=FIVE_HOUR_WINDOW_MINUTES).contains(&seed.five_hour_resets_at_minute),
        "initial 5-hour reset must stay within the 5-hour window",
    );
    if seed.five_hour_credits > 0.0 {
        assert!(
            seed.five_hour_resets_at_minute > 0,
            "initial 5-hour credits must reset after the simulation starts",
        );
    }
    assert!(
        (0..=WEEKLY_WINDOW_MINUTES).contains(&seed.weekly_resets_at_minute),
        "initial weekly reset must stay within the weekly window",
    );
    let older_weekly_credits = seed.weekly_credits - seed.five_hour_credits;
    if older_weekly_credits > f64::EPSILON {
        assert!(
            seed.weekly_resets_at_minute > 0,
            "initial weekly-only credits must reset after the simulation starts",
        );
        assert!(
            seed.weekly_resets_at_minute <= WEEKLY_WINDOW_MINUTES - FIVE_HOUR_WINDOW_MINUTES,
            "initial weekly-only credits must already be outside the 5-hour window",
        );
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

    let selected_index = select_sim_account(policy, accounts, demand.minute, *last_account_index);
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
    current_account_index: Option<usize>,
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
    let current_account_id = current_account_index
        .and_then(|index| accounts.get(index))
        .map(|account| account.account.id.as_str());

    policy
        .select_account_at(
            &candidates,
            SelectionContext::at(minute * 60).with_current_account_id(current_account_id),
        )
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

fn is_coding_minute(minute: i64) -> bool {
    if !is_work_minute(minute) {
        return false;
    }

    let minute_of_day = minute % 1_440;
    let standup_or_break = (600..615).contains(&minute_of_day)
        || (960..970).contains(&minute_of_day)
        || (minute_of_day % 60 >= 52);
    !standup_or_break
}

fn is_prompt_burst_minute(minute: i64) -> bool {
    let minute_of_day = minute % 1_440;
    let focus_block_minute = if (615..705).contains(&minute_of_day) {
        Some(minute_of_day - 615)
    } else if (810..930).contains(&minute_of_day) {
        Some(minute_of_day - 810)
    } else {
        None
    };

    focus_block_minute.is_some_and(|block_minute| block_minute % 45 < 6)
}

fn is_interrupt_minute(
    minute: i64,
    interrupt_every_minutes: i64,
    interrupt_duration_minutes: i64,
) -> bool {
    if interrupt_every_minutes <= 0 || interrupt_duration_minutes <= 0 {
        return false;
    }

    let minute_of_day = minute % 1_440;
    let workday_start = 570;
    if minute_of_day < workday_start {
        return false;
    }

    (minute_of_day - workday_start) % interrupt_every_minutes < interrupt_duration_minutes
}

fn weekday_load_multiplier(minute: i64) -> f64 {
    match (minute / 1_440) % 7 {
        0 => 0.9,
        1 => 1.05,
        2 => 1.1,
        3 => 1.0,
        4 => 0.75,
        _ => 0.0,
    }
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
        self.remove_empty_stagger_placeholders();

        if five_hour_credits > 0.0 {
            let five_hour_event = seed_usage_event(
                five_hour_resets_at_minute,
                FIVE_HOUR_WINDOW_MINUTES,
                five_hour_credits,
            );
            self.five_hour_events.push_back(five_hour_event);
            self.weekly_events.push_back(five_hour_event);
            sort_events(&mut self.five_hour_events);
        }
        let older_weekly_credits = weekly_credits - five_hour_credits;
        if older_weekly_credits > 0.0 {
            self.weekly_events.push_back(seed_usage_event(
                weekly_resets_at_minute,
                WEEKLY_WINDOW_MINUTES,
                older_weekly_credits,
            ));
        }
        sort_events(&mut self.weekly_events);
    }

    fn expire(&mut self, minute: i64) {
        expire_events(&mut self.five_hour_events, minute, FIVE_HOUR_WINDOW_MINUTES);
        expire_events(&mut self.weekly_events, minute, WEEKLY_WINDOW_MINUTES);
    }

    fn consume(&mut self, minute: i64, credits: f64) {
        self.remove_empty_stagger_placeholders();

        let event = UsageEvent {
            at_minute: minute,
            credits,
        };
        self.five_hour_events.push_back(event);
        self.weekly_events.push_back(event);
    }

    fn remove_empty_stagger_placeholders(&mut self) {
        self.five_hour_events.retain(|event| event.credits > 0.0);
        self.weekly_events.retain(|event| event.credits > 0.0);
    }

    fn can_serve(&self, credits: f64) -> bool {
        self.five_hour_remaining() + f64::EPSILON >= credits
            && self.weekly_remaining() + f64::EPSILON >= credits
    }

    fn usage_info(&self, minute: i64) -> UsageInfo {
        let five_hour_used = self.five_hour_used();
        let weekly_used = self.weekly_used();
        assert!(
            weekly_used + f64::EPSILON >= five_hour_used,
            "simulated weekly usage must include 5-hour usage",
        );
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

fn gate_stats(
    unavailable_minutes: u32,
    max_contiguous_unavailable_minutes: u32,
    failed_credits: f64,
    account_switches: u32,
    preventable_failures: u32,
) -> SimStats {
    SimStats {
        served_credits: 10_000.0,
        failed_credits,
        preventable_failures,
        unavailable_minutes,
        max_contiguous_unavailable_minutes,
        account_switches,
        min_five_hour_remaining: 0.0,
        min_weekly_remaining: 0.0,
    }
}

fn filler_policy_stats(policy_name: &'static str) -> PolicyStats {
    PolicyStats {
        policy_name,
        stats: gate_stats(0, 0, 0.0, 0, 0),
    }
}

fn assert_gate_failure_kind(
    decision: ReplacementGateDecision,
    expected: ReplacementGateFailureKind,
) {
    match decision {
        ReplacementGateDecision::Fail(failure) => assert_eq!(failure.kind, expected),
        ReplacementGateDecision::Pass => panic!("replacement gate should fail with {expected:?}"),
    }
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
            id_token: "id-token".into(),
            access_token: "access-token".into(),
            refresh_token: "refresh-token".into(),
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
            key: "api-key".into(),
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
