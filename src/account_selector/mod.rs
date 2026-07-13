use std::cmp::Ordering;

use chrono::{DateTime, Utc};

use crate::types::{AuthData, StoredAccount, UsageInfo, UsageWindowData, UsageWindowKind};

mod deadline_aware;
mod demand_aware_hysteresis;
mod reset_weighted_minimax;
mod shadow_price;

pub use deadline_aware::DeadlineAwarePolicy;
pub use demand_aware_hysteresis::DemandAwareHysteresisPolicy;
pub use reset_weighted_minimax::ResetWeightedMinimaxPolicy;
pub use shadow_price::ShadowPricePolicy;

pub const DEFAULT_MIN_SAFE_HEADROOM: f64 = 5.0;
pub const DEFAULT_WEEKLY_TO_FIVE_HOUR_RATIO: f64 = 5.0;

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
    DemandAwareHysteresis,
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
    pub five_hour_headroom: Option<f64>,
    pub weekly_headroom: Option<f64>,
    pub five_hour_headroom_units: Option<f64>,
    pub weekly_headroom_units: Option<f64>,
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
pub struct SelectionContext<'a> {
    pub now: i64,
    current_account_id: Option<&'a str>,
}

impl<'a> SelectionContext<'a> {
    pub fn now() -> Self {
        Self {
            now: Utc::now().timestamp(),
            current_account_id: None,
        }
    }

    pub fn with_current_account_id<'b>(
        self,
        current_account_id: Option<&'b str>,
    ) -> SelectionContext<'b> {
        SelectionContext {
            now: self.now,
            current_account_id,
        }
    }

    pub fn at(now: i64) -> Self {
        Self {
            now,
            current_account_id: None,
        }
    }
}

struct EvaluatedCandidate<'a> {
    account: &'a StoredAccount,
    five_hour: Option<EvaluatedWindow>,
    weekly: Option<EvaluatedWindow>,
    active_windows: ActiveUsageWindows,
    metrics: UsageSelectionMetrics,
    order: usize,
}

impl EvaluatedCandidate<'_> {
    fn window(&self, window: UsageWindow) -> Option<EvaluatedWindow> {
        match window {
            UsageWindow::FiveHour => self.five_hour,
            UsageWindow::Weekly => self.weekly,
        }
    }
}

struct EvaluatedUsage {
    five_hour: Option<EvaluatedWindow>,
    weekly: Option<EvaluatedWindow>,
    active_windows: ActiveUsageWindows,
    metrics: UsageSelectionMetrics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveUsageWindows {
    FiveHour,
    Weekly,
    Both,
}

impl ActiveUsageWindows {
    fn from_presence(has_five_hour: bool, has_weekly: bool) -> Option<Self> {
        match (has_five_hour, has_weekly) {
            (true, false) => Some(Self::FiveHour),
            (false, true) => Some(Self::Weekly),
            (true, true) => Some(Self::Both),
            (false, false) => None,
        }
    }

    fn intersection(self, other: Self) -> Option<Self> {
        Self::from_presence(
            self.has_five_hour() && other.has_five_hour(),
            self.has_weekly() && other.has_weekly(),
        )
    }

    fn has_five_hour(self) -> bool {
        matches!(self, Self::FiveHour | Self::Both)
    }

    fn has_weekly(self) -> bool {
        matches!(self, Self::Weekly | Self::Both)
    }
}

#[derive(Debug, Clone, Copy)]
struct ValidatedWindow {
    data: UsageWindowData,
    used_percent: f64,
}

struct ValidatedUsage {
    five_hour: Option<ValidatedWindow>,
    weekly: Option<ValidatedWindow>,
    available_windows: ActiveUsageWindows,
}

#[derive(Debug, Clone, Copy)]
struct EvaluatedWindow {
    data: UsageWindowData,
    used_percent: f64,
    headroom: f64,
    headroom_units: f64,
}

pub trait AccountSelectionPolicy {
    fn select_account_at<'a>(
        &mut self,
        candidates: &[AccountUsageCandidate<'a>],
        context: SelectionContext<'_>,
    ) -> Option<AccountSelection<'a>>;
}

pub fn select_account<'a>(
    candidates: &[AccountUsageCandidate<'a>],
    config: SelectionConfig,
) -> Option<AccountSelection<'a>> {
    select_account_with_context(candidates, config, SelectionContext::now())
}

pub fn select_account_with_context<'a>(
    candidates: &[AccountUsageCandidate<'a>],
    config: SelectionConfig,
    context: SelectionContext<'_>,
) -> Option<AccountSelection<'a>> {
    // Keep the runtime default on the proven policy until simulations show a
    // clear user-unavailable-time improvement from a replacement policy.
    match config.policy {
        SelectionPolicyKind::ShadowPrice => {
            ShadowPricePolicy::new(config).select_account_at(candidates, context)
        }
        SelectionPolicyKind::ResetWeightedMinimax => {
            ResetWeightedMinimaxPolicy::new(config).select_account_at(candidates, context)
        }
        SelectionPolicyKind::DemandAwareHysteresis => {
            DemandAwareHysteresisPolicy::new(config).select_account_at(candidates, context)
        }
        SelectionPolicyKind::DeadlineAware => {
            DeadlineAwarePolicy::new(config).select_account_at(candidates, context)
        }
    }
}

pub fn usage_selection_metrics(
    usage: &UsageInfo,
    config: SelectionConfig,
) -> Option<UsageSelectionMetrics> {
    let validated = validate_usage(usage)?;
    let active_windows = validated.available_windows;
    evaluate_usage(
        validated,
        active_windows,
        normalized_min_safe_headroom(config.min_safe_headroom),
        normalized_weekly_to_five_hour_ratio(config.weekly_to_five_hour_ratio),
    )
    .map(|evaluated| evaluated.metrics)
}

fn evaluated_candidates<'a>(
    candidates: &[AccountUsageCandidate<'a>],
    config: SelectionConfig,
) -> Vec<EvaluatedCandidate<'a>> {
    let min_safe_headroom = normalized_min_safe_headroom(config.min_safe_headroom);
    let weekly_to_five_hour_ratio =
        normalized_weekly_to_five_hour_ratio(config.weekly_to_five_hour_ratio);

    let validated = candidates
        .iter()
        .enumerate()
        .filter_map(|(order, candidate)| {
            validate_candidate(candidate.account, candidate.usage)
                .map(|usage| (order, candidate.account, usage))
        })
        .collect::<Vec<_>>();

    let mut available_windows = validated
        .iter()
        .map(|(_, _, usage)| usage.available_windows);
    let Some(mut active_windows) = available_windows.next() else {
        return Vec::new();
    };
    for available in available_windows {
        let Some(intersection) = active_windows.intersection(available) else {
            return Vec::new();
        };
        active_windows = intersection;
    }

    validated
        .into_iter()
        .filter_map(|(order, account, usage)| {
            evaluate_usage(
                usage,
                active_windows,
                min_safe_headroom,
                weekly_to_five_hour_ratio,
            )
            .map(|evaluated| EvaluatedCandidate {
                account,
                five_hour: evaluated.five_hour,
                weekly: evaluated.weekly,
                active_windows: evaluated.active_windows,
                metrics: evaluated.metrics,
                order,
            })
        })
        .collect()
}

fn validate_candidate(account: &StoredAccount, usage: &UsageInfo) -> Option<ValidatedUsage> {
    if !account.auto_switch_enabled() {
        return None;
    }
    if !matches!(account.auth_data, AuthData::ChatGPT { .. }) {
        return None;
    }
    validate_usage(usage)
}

fn validate_usage(usage: &UsageInfo) -> Option<ValidatedUsage> {
    if usage.error.is_some() || usage.rate_limit_reached_type.is_some() {
        return None;
    }

    let mut five_hour = None;
    let mut weekly = None;
    for window in usage.windows() {
        let used_percent = window.used_percent?;
        if !used_percent.is_finite() || used_percent >= 100.0 {
            return None;
        }

        let validated = ValidatedWindow {
            data: window,
            used_percent,
        };
        let target = match window.kind() {
            UsageWindowKind::FiveHour => &mut five_hour,
            UsageWindowKind::Weekly => &mut weekly,
            _ => return None,
        };
        if target.replace(validated).is_some() {
            return None;
        }
    }

    Some(ValidatedUsage {
        available_windows: ActiveUsageWindows::from_presence(
            five_hour.is_some(),
            weekly.is_some(),
        )?,
        five_hour,
        weekly,
    })
}

fn evaluate_usage(
    usage: ValidatedUsage,
    active_windows: ActiveUsageWindows,
    min_safe_headroom: f64,
    weekly_to_five_hour_ratio: f64,
) -> Option<EvaluatedUsage> {
    let five_hour = if active_windows.has_five_hour() {
        Some(evaluate_window(usage.five_hour?, 1.0))
    } else {
        None
    };
    let weekly = if active_windows.has_weekly() {
        Some(evaluate_window(usage.weekly?, weekly_to_five_hour_ratio))
    } else {
        None
    };

    let (bottleneck, bottleneck_window) = match (five_hour, weekly) {
        (Some(five_hour), None) => (UsageWindow::FiveHour, five_hour),
        (None, Some(weekly)) => (UsageWindow::Weekly, weekly),
        (Some(five_hour), Some(weekly)) if five_hour.headroom_units <= weekly.headroom_units => {
            (UsageWindow::FiveHour, five_hour)
        }
        (Some(_), Some(weekly)) => (UsageWindow::Weekly, weekly),
        (None, None) => return None,
    };
    let bottleneck_headroom = bottleneck_window.headroom_units;

    Some(EvaluatedUsage {
        five_hour,
        weekly,
        active_windows,
        metrics: UsageSelectionMetrics {
            five_hour_headroom: five_hour.map(|window| window.headroom),
            weekly_headroom: weekly.map(|window| window.headroom),
            five_hour_headroom_units: five_hour.map(|window| window.headroom_units),
            weekly_headroom_units: weekly.map(|window| window.headroom_units),
            bottleneck,
            bottleneck_headroom,
            bottleneck_resets_at: bottleneck_window.data.resets_at,
            safe_for_reset_priority: bottleneck_headroom >= min_safe_headroom,
        },
    })
}

fn evaluate_window(window: ValidatedWindow, capacity_weight: f64) -> EvaluatedWindow {
    let headroom = headroom_from_used_percent(window.used_percent);
    EvaluatedWindow {
        data: window.data,
        used_percent: window.used_percent,
        headroom,
        headroom_units: headroom * capacity_weight,
    }
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
    context: SelectionContext<'_>,
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
