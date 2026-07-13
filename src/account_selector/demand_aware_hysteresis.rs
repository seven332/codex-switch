use std::cmp::Ordering;

use super::{
    AccountSelection, AccountSelectionPolicy, AccountUsageCandidate, EvaluatedCandidate,
    SelectionConfig, SelectionContext, SelectionPolicyKind, UsageWindow, compare_bool_desc,
    compare_headroom_desc, compare_last_used, compare_optional_reset, evaluated_candidates,
    normalized_min_safe_headroom, reset_delay_ratio,
};

const DEMAND_AWARE_HYSTERESIS_MARGIN: f64 = 0.08;
const DEMAND_AWARE_LOW_MARGIN_WEIGHT: f64 = 10.0;
const DEMAND_AWARE_WEEKLY_BOTTLENECK_WEIGHT: f64 = 0.15;
const DEMAND_AWARE_RESET_EXPONENT: f64 = 1.25;
const DEMAND_AWARE_MIN_RESET_FACTOR: f64 = 0.1;

#[derive(Debug, Clone, Copy)]
pub struct DemandAwareHysteresisPolicy {
    pub config: SelectionConfig,
}

impl DemandAwareHysteresisPolicy {
    pub fn new(config: SelectionConfig) -> Self {
        Self {
            config: SelectionConfig {
                policy: SelectionPolicyKind::DemandAwareHysteresis,
                ..config
            },
        }
    }
}

impl Default for DemandAwareHysteresisPolicy {
    fn default() -> Self {
        Self::new(SelectionConfig::default())
    }
}

impl AccountSelectionPolicy for DemandAwareHysteresisPolicy {
    fn select_account_at<'a>(
        &mut self,
        candidates: &[AccountUsageCandidate<'a>],
        context: SelectionContext<'_>,
    ) -> Option<AccountSelection<'a>> {
        let evaluated = evaluated_candidates(candidates, self.config);
        let best = evaluated
            .iter()
            .min_by(|left, right| compare_demand_aware(left, right, self.config, context))?;
        let selected = context
            .current_account_id
            .and_then(|account_id| {
                evaluated
                    .iter()
                    .find(|candidate| candidate.account.id == account_id)
            })
            .filter(|current| should_keep_current(current, best, self.config, context))
            .unwrap_or(best);

        Some(AccountSelection {
            account: selected.account,
            metrics: selected.metrics,
        })
    }
}

fn should_keep_current(
    current: &EvaluatedCandidate<'_>,
    best: &EvaluatedCandidate<'_>,
    config: SelectionConfig,
    context: SelectionContext<'_>,
) -> bool {
    if current.account.id == best.account.id {
        return true;
    }
    if !current.metrics.safe_for_reset_priority {
        return false;
    }

    let current_score = demand_aware_score(current, config, context);
    let best_score = demand_aware_score(best, config, context);
    best_score + DEMAND_AWARE_HYSTERESIS_MARGIN >= current_score
}

fn compare_demand_aware(
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
            compare_demand_aware_score(left, right, config, context)
                .then_with(|| compare_headroom_desc(left, right))
        } else {
            compare_headroom_desc(left, right)
                .then_with(|| compare_demand_aware_score(left, right, config, context))
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

fn compare_demand_aware_score(
    left: &EvaluatedCandidate<'_>,
    right: &EvaluatedCandidate<'_>,
    config: SelectionConfig,
    context: SelectionContext<'_>,
) -> Ordering {
    demand_aware_score(left, config, context).total_cmp(&demand_aware_score(right, config, context))
}

fn demand_aware_score(
    candidate: &EvaluatedCandidate<'_>,
    config: SelectionConfig,
    context: SelectionContext<'_>,
) -> f64 {
    let bottleneck_risk = (1.0 - candidate.metrics.bottleneck_headroom / 100.0).clamp(0.0, 1.0);
    let reset_factor = demand_aware_reset_factor(
        candidate.metrics.bottleneck_resets_at,
        bottleneck_window_minutes(candidate),
        context,
    );
    let low_margin_penalty = low_margin_penalty(
        candidate.metrics.bottleneck_headroom,
        normalized_min_safe_headroom(config.min_safe_headroom),
    );
    let weekly_bottleneck_penalty = if candidate.metrics.bottleneck == UsageWindow::Weekly {
        DEMAND_AWARE_WEEKLY_BOTTLENECK_WEIGHT * reset_factor
    } else {
        0.0
    };

    bottleneck_risk * reset_factor + low_margin_penalty + weekly_bottleneck_penalty
}

fn low_margin_penalty(bottleneck_headroom: f64, min_safe_headroom: f64) -> f64 {
    if min_safe_headroom <= 0.0 || bottleneck_headroom >= min_safe_headroom {
        return 0.0;
    }

    ((min_safe_headroom - bottleneck_headroom) / min_safe_headroom).clamp(0.0, 1.0)
        * DEMAND_AWARE_LOW_MARGIN_WEIGHT
}

fn demand_aware_reset_factor(
    resets_at: Option<i64>,
    window_minutes: Option<i64>,
    context: SelectionContext<'_>,
) -> f64 {
    reset_delay_ratio(resets_at, window_minutes, context)
        .powf(DEMAND_AWARE_RESET_EXPONENT)
        .clamp(DEMAND_AWARE_MIN_RESET_FACTOR, 1.0)
}

fn bottleneck_window_minutes(candidate: &EvaluatedCandidate<'_>) -> Option<i64> {
    candidate
        .window(candidate.metrics.bottleneck)
        .and_then(|window| window.data.window_minutes)
}
