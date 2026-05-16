use std::cmp::Ordering;

use super::{
    AccountSelection, AccountSelectionPolicy, AccountUsageCandidate, EvaluatedCandidate,
    SelectionConfig, SelectionContext, SelectionPolicyKind, compare_bool_desc,
    compare_headroom_desc, compare_last_used, compare_optional_reset, evaluated_candidates,
};

#[derive(Debug, Clone, Copy, Default)]
pub struct DeadlineAwarePolicy {
    pub config: SelectionConfig,
}

impl DeadlineAwarePolicy {
    pub fn new(config: SelectionConfig) -> Self {
        Self {
            config: SelectionConfig {
                policy: SelectionPolicyKind::DeadlineAware,
                ..config
            },
        }
    }
}

impl AccountSelectionPolicy for DeadlineAwarePolicy {
    fn select_account_at<'a>(
        &mut self,
        candidates: &[AccountUsageCandidate<'a>],
        _context: SelectionContext<'_>,
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
