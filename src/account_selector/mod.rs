use std::cmp::Ordering;

use chrono::{DateTime, Utc};

use crate::types::{AuthData, StoredAccount, UsageInfo};

mod deadline_aware;
mod reset_weighted_minimax;
mod shadow_price;

pub use deadline_aware::DeadlineAwarePolicy;
pub use reset_weighted_minimax::ResetWeightedMinimaxPolicy;
pub use shadow_price::ShadowPricePolicy;

pub const DEFAULT_MIN_SAFE_HEADROOM: f64 = 5.0;
pub const DEFAULT_WEEKLY_TO_FIVE_HOUR_RATIO: f64 = 4.0;

#[derive(Debug, Clone, Copy)]
pub struct SelectionConfig {
    pub min_safe_headroom: f64,
    pub weekly_to_five_hour_ratio: f64,
    pub policy: SelectionPolicyKind,
}

impl Default for SelectionConfig {
    fn default() -> Self {
        Self {
            min_safe_headroom: DEFAULT_MIN_SAFE_HEADROOM,
            weekly_to_five_hour_ratio: DEFAULT_WEEKLY_TO_FIVE_HOUR_RATIO,
            policy: SelectionPolicyKind::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SelectionPolicyKind {
    #[default]
    DeadlineAware,
    ShadowPrice,
    ResetWeightedMinimax,
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

#[derive(Debug, Clone, Copy)]
pub struct SelectionContext {
    pub now: i64,
}

impl SelectionContext {
    pub fn now() -> Self {
        Self {
            now: Utc::now().timestamp(),
        }
    }

    #[cfg(test)]
    pub fn at(now: i64) -> Self {
        Self { now }
    }
}

struct EvaluatedCandidate<'a> {
    account: &'a StoredAccount,
    usage: &'a UsageInfo,
    metrics: UsageSelectionMetrics,
    order: usize,
}

pub trait AccountSelectionPolicy {
    fn select_account_at<'a>(
        &mut self,
        candidates: &[AccountUsageCandidate<'a>],
        context: SelectionContext,
    ) -> Option<AccountSelection<'a>>;

    fn select_account<'a>(
        &mut self,
        candidates: &[AccountUsageCandidate<'a>],
    ) -> Option<AccountSelection<'a>> {
        self.select_account_at(candidates, SelectionContext::now())
    }
}

pub fn select_account<'a>(
    candidates: &[AccountUsageCandidate<'a>],
    config: SelectionConfig,
) -> Option<AccountSelection<'a>> {
    // Keep the runtime default on the proven policy until simulations show a
    // clear user-unavailable-time improvement from a replacement policy.
    match config.policy {
        SelectionPolicyKind::ShadowPrice => {
            ShadowPricePolicy::new(config).select_account(candidates)
        }
        SelectionPolicyKind::ResetWeightedMinimax => {
            ResetWeightedMinimaxPolicy::new(config).select_account(candidates)
        }
        SelectionPolicyKind::DeadlineAware => {
            DeadlineAwarePolicy::new(config).select_account(candidates)
        }
    }
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
                usage: candidate.usage,
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

fn reset_delay_ratio(
    resets_at: Option<i64>,
    window_minutes: Option<i64>,
    context: SelectionContext,
) -> f64 {
    let (Some(resets_at), Some(window_minutes)) = (resets_at, window_minutes) else {
        return 1.0;
    };
    if window_minutes <= 0 {
        return 1.0;
    }

    let Some(window_seconds) = window_minutes.checked_mul(60) else {
        return 1.0;
    };
    if window_seconds <= 0 {
        return 1.0;
    }

    let reset_delay = resets_at.saturating_sub(context.now).max(0);
    (reset_delay as f64 / window_seconds as f64).clamp(0.0, 1.0)
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
mod tests;
