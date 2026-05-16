use std::cmp::Ordering;

use super::{
    AccountSelection, AccountSelectionPolicy, AccountUsageCandidate,
    DEFAULT_WEEKLY_TO_FIVE_HOUR_RATIO, EvaluatedCandidate, SelectionConfig, SelectionContext,
    SelectionPolicyKind, compare_bool_desc, compare_headroom_desc, compare_last_used,
    compare_optional_reset, evaluated_candidates, normalized_weekly_to_five_hour_ratio,
    reset_delay_ratio,
};

const SHADOW_PRICE_EPSILON: f64 = 0.01;
const SHADOW_PRICE_MIN_RESET_FACTOR: f64 = 0.02;
const SHADOW_PRICE_RESET_EXPONENT: f64 = 1.5;
const SHADOW_PRICE_IMMEDIATE_RISK_WEIGHT: f64 = 0.15;

#[derive(Debug, Clone, Copy)]
pub struct ShadowPricePolicy {
    pub config: SelectionConfig,
}

impl ShadowPricePolicy {
    pub fn new(config: SelectionConfig) -> Self {
        Self {
            config: SelectionConfig {
                policy: SelectionPolicyKind::ShadowPrice,
                ..config
            },
        }
    }
}

impl Default for ShadowPricePolicy {
    fn default() -> Self {
        Self::new(SelectionConfig::default())
    }
}

impl AccountSelectionPolicy for ShadowPricePolicy {
    fn select_account_at<'a>(
        &mut self,
        candidates: &[AccountUsageCandidate<'a>],
        context: SelectionContext<'_>,
    ) -> Option<AccountSelection<'a>> {
        evaluated_candidates(candidates, self.config)
            .into_iter()
            .min_by(|left, right| compare_shadow_price(left, right, self.config, context))
            .map(|candidate| AccountSelection {
                account: candidate.account,
                metrics: candidate.metrics,
            })
    }
}

fn compare_shadow_price(
    left: &EvaluatedCandidate<'_>,
    right: &EvaluatedCandidate<'_>,
    config: SelectionConfig,
    context: SelectionContext<'_>,
) -> Ordering {
    compare_bool_desc(
        left.metrics.safe_for_reset_priority,
        right.metrics.safe_for_reset_priority,
    )
    .then_with(|| {
        if left.metrics.safe_for_reset_priority && right.metrics.safe_for_reset_priority {
            compare_shadow_price_score(left, right, config, context)
                .then_with(|| compare_headroom_desc(left, right))
        } else {
            compare_headroom_desc(left, right)
                .then_with(|| compare_shadow_price_score(left, right, config, context))
        }
    })
    .then_with(|| {
        compare_optional_reset(
            left.metrics.bottleneck_resets_at,
            right.metrics.bottleneck_resets_at,
        )
    })
    .then_with(|| compare_last_used(left.account.last_used_at, right.account.last_used_at))
    .then_with(|| left.order.cmp(&right.order))
}

fn compare_shadow_price_score(
    left: &EvaluatedCandidate<'_>,
    right: &EvaluatedCandidate<'_>,
    config: SelectionConfig,
    context: SelectionContext<'_>,
) -> Ordering {
    shadow_price_score(left, config, context)
        .total_cmp(&shadow_price_score(right, config, context))
        .then_with(|| {
            compare_optional_reset(
                left.metrics.bottleneck_resets_at,
                right.metrics.bottleneck_resets_at,
            )
        })
}

fn shadow_price_score(
    candidate: &EvaluatedCandidate<'_>,
    config: SelectionConfig,
    context: SelectionContext<'_>,
) -> f64 {
    let weekly_to_five_hour_ratio =
        normalized_weekly_to_five_hour_ratio(config.weekly_to_five_hour_ratio);
    shadow_window_price(
        candidate.usage.primary_used_percent,
        candidate.usage.primary_window_minutes,
        candidate.usage.primary_resets_at,
        1.0,
        context,
    ) + shadow_window_price(
        candidate.usage.secondary_used_percent,
        candidate.usage.secondary_window_minutes,
        candidate.usage.secondary_resets_at,
        weekly_to_five_hour_ratio,
        context,
    )
}

fn shadow_window_price(
    used_percent: Option<f64>,
    window_minutes: Option<i64>,
    resets_at: Option<i64>,
    capacity_weight: f64,
    context: SelectionContext<'_>,
) -> f64 {
    let Some(used_percent) = used_percent.filter(|value| value.is_finite()) else {
        return f64::INFINITY;
    };
    let capacity_weight = if capacity_weight.is_finite() && capacity_weight > 0.0 {
        capacity_weight
    } else {
        DEFAULT_WEEKLY_TO_FIVE_HOUR_RATIO
    };
    let used = (used_percent / 100.0).clamp(0.0, 1.0);
    let headroom = (1.0 - used).clamp(0.0, 1.0);
    let remaining_units = (headroom * capacity_weight).max(SHADOW_PRICE_EPSILON);
    let pressure = used / remaining_units;
    let reset_factor = shadow_reset_factor(resets_at, window_minutes, context);
    let risk_factor = SHADOW_PRICE_IMMEDIATE_RISK_WEIGHT
        + (1.0 - SHADOW_PRICE_IMMEDIATE_RISK_WEIGHT) * reset_factor;
    pressure * risk_factor
}

fn shadow_reset_factor(
    resets_at: Option<i64>,
    window_minutes: Option<i64>,
    context: SelectionContext<'_>,
) -> f64 {
    reset_delay_ratio(resets_at, window_minutes, context)
        .powf(SHADOW_PRICE_RESET_EXPONENT)
        .clamp(SHADOW_PRICE_MIN_RESET_FACTOR, 1.0)
}
