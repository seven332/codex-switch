use std::cmp::Ordering;

use super::{
    AccountSelection, AccountSelectionPolicy, AccountUsageCandidate, EvaluatedCandidate,
    SelectionConfig, SelectionContext, SelectionPolicyKind, compare_bool_desc,
    compare_headroom_desc, compare_last_used, compare_optional_reset, evaluated_candidates,
    reset_delay_ratio,
};

const RESET_WEIGHTED_MINIMAX_MIN_RESET_FACTOR: f64 = 0.1;
const RESET_WEIGHTED_MINIMAX_RESET_EXPONENT: f64 = 1.25;

#[derive(Debug, Clone, Copy)]
pub struct ResetWeightedMinimaxPolicy {
    pub config: SelectionConfig,
}

impl ResetWeightedMinimaxPolicy {
    pub fn new(config: SelectionConfig) -> Self {
        Self {
            config: SelectionConfig {
                policy: SelectionPolicyKind::ResetWeightedMinimax,
                ..config
            },
        }
    }
}

impl Default for ResetWeightedMinimaxPolicy {
    fn default() -> Self {
        Self::new(SelectionConfig::default())
    }
}

impl AccountSelectionPolicy for ResetWeightedMinimaxPolicy {
    fn select_account_at<'a>(
        &mut self,
        candidates: &[AccountUsageCandidate<'a>],
        context: SelectionContext<'_>,
    ) -> Option<AccountSelection<'a>> {
        evaluated_candidates(candidates, self.config)
            .into_iter()
            .min_by(|left, right| compare_reset_weighted_minimax(left, right, context))
            .map(|candidate| AccountSelection {
                account: candidate.account,
                metrics: candidate.metrics,
            })
    }
}

fn compare_reset_weighted_minimax(
    left: &EvaluatedCandidate<'_>,
    right: &EvaluatedCandidate<'_>,
    context: SelectionContext<'_>,
) -> Ordering {
    compare_bool_desc(
        left.metrics.safe_for_reset_priority,
        right.metrics.safe_for_reset_priority,
    )
    .then_with(|| {
        if left.metrics.safe_for_reset_priority && right.metrics.safe_for_reset_priority {
            compare_reset_weighted_minimax_score(left, right, context)
                .then_with(|| compare_headroom_desc(left, right))
        } else {
            compare_headroom_desc(left, right)
                .then_with(|| compare_reset_weighted_minimax_score(left, right, context))
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

fn compare_reset_weighted_minimax_score(
    left: &EvaluatedCandidate<'_>,
    right: &EvaluatedCandidate<'_>,
    context: SelectionContext<'_>,
) -> Ordering {
    reset_weighted_minimax_score(left, context)
        .total_cmp(&reset_weighted_minimax_score(right, context))
        .then_with(|| {
            compare_optional_reset(
                left.metrics.bottleneck_resets_at,
                right.metrics.bottleneck_resets_at,
            )
        })
}

fn reset_weighted_minimax_score(
    candidate: &EvaluatedCandidate<'_>,
    context: SelectionContext<'_>,
) -> f64 {
    let bottleneck_risk = (1.0 - candidate.metrics.bottleneck_headroom / 100.0).clamp(0.0, 1.0);
    let reset_factor = reset_weighted_minimax_reset_factor(
        candidate.metrics.bottleneck_resets_at,
        bottleneck_window_minutes(candidate),
        context,
    );
    bottleneck_risk * reset_factor
}

fn bottleneck_window_minutes(candidate: &EvaluatedCandidate<'_>) -> Option<i64> {
    candidate
        .window(candidate.metrics.bottleneck)
        .and_then(|window| window.data.window_minutes)
}

fn reset_weighted_minimax_reset_factor(
    resets_at: Option<i64>,
    window_minutes: Option<i64>,
    context: SelectionContext<'_>,
) -> f64 {
    reset_delay_ratio(resets_at, window_minutes, context)
        .powf(RESET_WEIGHTED_MINIMAX_RESET_EXPONENT)
        .clamp(RESET_WEIGHTED_MINIMAX_MIN_RESET_FACTOR, 1.0)
}
