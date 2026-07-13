use std::cmp::Ordering;

use super::{
    AccountSelection, AccountSelectionPolicy, AccountUsageCandidate, EvaluatedCandidate,
    SelectionConfig, SelectionContext, SelectionPolicyKind, compare_bool_desc,
    compare_headroom_desc, compare_last_used, compare_optional_reset, evaluated_candidates,
};

const COLD_USAGE_EPSILON: f64 = 0.000_001;

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
        context: SelectionContext<'_>,
    ) -> Option<AccountSelection<'a>> {
        let evaluated = evaluated_candidates(candidates, self.config);
        let deadline_aware = evaluated
            .iter()
            .min_by(|left, right| compare_candidates(left, right))?;
        let selected = select_with_cold_activation(&evaluated, deadline_aware, context);
        let account = selected.account;
        let metrics = selected.metrics;

        Some(AccountSelection { account, metrics })
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

fn select_with_cold_activation<'slice, 'data>(
    evaluated: &'slice [EvaluatedCandidate<'data>],
    deadline_aware: &'slice EvaluatedCandidate<'data>,
    context: SelectionContext<'_>,
) -> &'slice EvaluatedCandidate<'data> {
    if let Some(current_cold) = context.current_account_id.and_then(|account_id| {
        evaluated
            .iter()
            .find(|candidate| candidate.account.id == account_id && is_cold_candidate(candidate))
    }) {
        return current_cold;
    }

    if cold_activation_due(evaluated, context)
        && let Some(cold) = best_cold_activation_candidate(evaluated)
    {
        return cold;
    }

    if is_cold_candidate(deadline_aware)
        && let Some(non_cold) = best_safe_non_cold_candidate(evaluated)
    {
        return non_cold;
    }

    deadline_aware
}

fn best_cold_activation_candidate<'slice, 'data>(
    evaluated: &'slice [EvaluatedCandidate<'data>],
) -> Option<&'slice EvaluatedCandidate<'data>> {
    evaluated
        .iter()
        .filter(|candidate| {
            candidate.metrics.safe_for_reset_priority && is_cold_candidate(candidate)
        })
        .min_by(|left, right| compare_cold_activation_candidates(left, right))
}

fn best_safe_non_cold_candidate<'slice, 'data>(
    evaluated: &'slice [EvaluatedCandidate<'data>],
) -> Option<&'slice EvaluatedCandidate<'data>> {
    evaluated
        .iter()
        .filter(|candidate| {
            !is_cold_candidate(candidate) && candidate.metrics.safe_for_reset_priority
        })
        .min_by(|left, right| compare_candidates(left, right))
}

fn cold_activation_due(
    evaluated: &[EvaluatedCandidate<'_>],
    context: SelectionContext<'_>,
) -> bool {
    let Some(last_activity_at) = latest_activity_timestamp(evaluated) else {
        return true;
    };
    let Some(interval_seconds) = cold_activation_interval_seconds(evaluated) else {
        return false;
    };

    context.now.saturating_sub(last_activity_at) >= interval_seconds
}

fn cold_activation_interval_seconds(evaluated: &[EvaluatedCandidate<'_>]) -> Option<i64> {
    let account_count = i64::try_from(evaluated.len()).ok()?;
    if account_count <= 1 {
        return None;
    }

    let window_minutes = evaluated
        .iter()
        .filter_map(|candidate| candidate.five_hour.window_minutes)
        .filter(|window_minutes| *window_minutes > 0)
        .min()?;
    let window_seconds = window_minutes.checked_mul(60)?;

    Some((window_seconds / account_count).max(1))
}

fn latest_activity_timestamp(evaluated: &[EvaluatedCandidate<'_>]) -> Option<i64> {
    evaluated
        .iter()
        .filter_map(candidate_activity_timestamp)
        .max()
}

fn candidate_activity_timestamp(candidate: &EvaluatedCandidate<'_>) -> Option<i64> {
    let mut latest = (!is_cold_candidate(candidate))
        .then(|| {
            candidate
                .account
                .last_used_at
                .map(|last_used_at| last_used_at.timestamp())
        })
        .flatten();

    for timestamp in inferred_usage_timestamps(candidate) {
        latest = Some(latest.map_or(timestamp, |latest| latest.max(timestamp)));
    }

    latest
}

fn inferred_usage_timestamps(candidate: &EvaluatedCandidate<'_>) -> impl Iterator<Item = i64> {
    [
        (
            candidate.five_hour.used_percent,
            candidate.five_hour.window_minutes,
            candidate.five_hour.resets_at,
        ),
        (
            candidate.weekly.used_percent,
            candidate.weekly.window_minutes,
            candidate.weekly.resets_at,
        ),
    ]
    .into_iter()
    .filter_map(|(used_percent, window_minutes, resets_at)| {
        if !has_nonzero_usage(used_percent) {
            return None;
        }

        let window_seconds = window_minutes?.checked_mul(60)?;
        if window_seconds <= 0 {
            return None;
        }

        Some(resets_at?.saturating_sub(window_seconds))
    })
}

fn compare_cold_activation_candidates(
    left: &EvaluatedCandidate<'_>,
    right: &EvaluatedCandidate<'_>,
) -> Ordering {
    compare_last_used(left.account.last_used_at, right.account.last_used_at)
        .then_with(|| left.order.cmp(&right.order))
}

fn is_cold_candidate(candidate: &EvaluatedCandidate<'_>) -> bool {
    is_zero_usage(candidate.five_hour.used_percent) && is_zero_usage(candidate.weekly.used_percent)
}

fn is_zero_usage(used_percent: Option<f64>) -> bool {
    used_percent.is_some_and(|value| value.is_finite() && value.abs() <= COLD_USAGE_EPSILON)
}

fn has_nonzero_usage(used_percent: Option<f64>) -> bool {
    used_percent.is_some_and(|value| value.is_finite() && value > COLD_USAGE_EPSILON)
}
