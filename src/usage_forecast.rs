use std::collections::HashMap;

use crate::account_selector::{
    self, AccountSelectionDecision, AccountUsageCandidate, SelectionConfig, SelectionContext,
};
use crate::runtime::{AUTO_SWITCH_MAINTENANCE_MAX_INTERVAL, AUTO_SWITCH_MAINTENANCE_MIN_INTERVAL};
use crate::types::{AuthData, StoredAccount, UsageInfo, UsageWindowKind};

const FORECAST_HORIZON_SECONDS: i64 = 14 * 24 * 60 * 60;
const MAX_SIMULATION_STEPS: usize = 10_000;
const LIMIT_PERCENT: f64 = 100.0;
const MIN_FIVE_HOUR_RATE_SAMPLE_MINUTES: f64 = 15.0;
const MIN_WEEKLY_RATE_SAMPLE_MINUTES: f64 = 6.0 * 60.0;
const POLICY_REEVALUATION_SECONDS: i64 = ((AUTO_SWITCH_MAINTENANCE_MIN_INTERVAL.as_secs()
    + AUTO_SWITCH_MAINTENANCE_MAX_INTERVAL.as_secs())
    / 2) as i64;

#[derive(Debug, Clone, PartialEq)]
pub struct UsageForecast {
    pub rates: Option<UsageForecastRates>,
    pub outcome: UsageForecastOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UsageForecastRates {
    pub five_hour_percent_per_hour: Option<f64>,
    pub weekly_percent_per_hour: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UsageForecastOutcome {
    NotExpected {
        horizon_seconds: i64,
    },
    Unavailable {
        at: i64,
        limited_by: ForecastLimit,
        recovery_at: Option<i64>,
    },
    NotEnoughData {
        reason: ForecastUnavailableReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForecastUnavailableReason {
    IncompleteUsageData,
    InsufficientRateSamples,
    NoComparableUsageWindows,
}

impl ForecastUnavailableReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IncompleteUsageData => "incomplete usage data",
            Self::InsufficientRateSamples => "insufficient rate samples",
            Self::NoComparableUsageWindows => "no comparable usage windows",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForecastLimit {
    FiveHour,
    Weekly,
    FiveHourAndWeekly,
    Additional,
    Credits,
    UsageLimit,
    RateLimit,
    Multiple,
    Unknown,
}

pub fn forecast_all_usage(
    accounts: &[StoredAccount],
    usage_by_id: &HashMap<String, UsageInfo>,
    current_account_id: Option<&str>,
    now: i64,
) -> UsageForecast {
    let Some(mut forecast) = build_forecast(accounts, usage_by_id, now) else {
        return UsageForecast {
            rates: None,
            outcome: UsageForecastOutcome::NotEnoughData {
                reason: ForecastUnavailableReason::IncompleteUsageData,
            },
        };
    };
    let outcome = simulate_forecast(&mut forecast, current_account_id, now);
    let rates = (!matches!(outcome, UsageForecastOutcome::NotEnoughData { .. }))
        .then_some(forecast.rates)
        .filter(UsageForecastRates::has_any);

    UsageForecast { rates, outcome }
}

struct ForecastInput<'a> {
    accounts: Vec<SimAccount<'a>>,
    rates: UsageForecastRates,
    fallback_unavailable_limit: Option<ForecastLimit>,
}

#[derive(Clone)]
struct SimAccount<'a> {
    account: &'a StoredAccount,
    five_hour: Option<SimWindow>,
    weekly: Option<SimWindow>,
    blocked_until: Option<i64>,
}

#[derive(Clone, Copy)]
struct SimWindow {
    used_percent: f64,
    window_seconds: i64,
    resets_at: i64,
    rate_sample_allowed: bool,
}

struct RateSamples {
    five_hour: Vec<RateSample>,
    weekly: Vec<RateSample>,
}

#[derive(Clone, Copy)]
struct RateSample {
    used_percent: f64,
    elapsed_minutes: f64,
}

#[derive(Clone, Copy)]
struct AdditionalLimitBlock {
    recovery_at: Option<i64>,
}

fn build_forecast<'a>(
    accounts: &'a [StoredAccount],
    usage_by_id: &HashMap<String, UsageInfo>,
    now: i64,
) -> Option<ForecastInput<'a>> {
    let mut sim_accounts = Vec::new();
    let mut samples = RateSamples {
        five_hour: Vec::new(),
        weekly: Vec::new(),
    };
    let mut fallback_unavailable_limit = None;

    for account in accounts {
        if !account.auto_switch_enabled() {
            continue;
        }

        if !matches!(account.auth_data, AuthData::ChatGPT { .. }) {
            continue;
        }

        let Some(usage) = usage_by_id.get(&account.id) else {
            continue;
        };
        if usage.error.is_some() {
            continue;
        }
        let known_limit = known_unavailable_limit(usage, now);
        if credits_depleted(usage) {
            record_fallback_limit(
                &mut fallback_unavailable_limit,
                known_limit
                    .map(|limit| combine_limits(ForecastLimit::Credits, limit))
                    .unwrap_or(ForecastLimit::Credits),
            );
            continue;
        }
        let additional_block = additional_hard_limit_block(usage, now);

        let Some((mut five_hour, mut weekly)) = make_account_windows(usage, now) else {
            if let Some(limit) = known_limit {
                record_fallback_limit(&mut fallback_unavailable_limit, limit);
            }
            continue;
        };
        let blocked_until = if let Some(block) = additional_block {
            let Some(recovery_at) = block.recovery_at else {
                record_fallback_limit(
                    &mut fallback_unavailable_limit,
                    known_limit.unwrap_or(ForecastLimit::Additional),
                );
                continue;
            };
            Some(recovery_at)
        } else {
            None
        };

        if let Some(window) = five_hour.as_ref()
            && let Some(sample) = rate_sample(window, now, MIN_FIVE_HOUR_RATE_SAMPLE_MINUTES)
        {
            samples.five_hour.push(sample);
        }
        if let Some(window) = weekly.as_ref()
            && let Some(sample) = rate_sample(window, now, MIN_WEEKLY_RATE_SAMPLE_MINUTES)
        {
            samples.weekly.push(sample);
        }

        apply_rate_limit_reached_type(usage, &mut five_hour, &mut weekly);
        sim_accounts.push(SimAccount {
            account,
            five_hour,
            weekly,
            blocked_until,
        });
    }

    if sim_accounts.is_empty() && fallback_unavailable_limit.is_none() {
        return None;
    }

    Some(ForecastInput {
        accounts: sim_accounts,
        rates: estimate_rates(&samples),
        fallback_unavailable_limit,
    })
}

fn estimate_rates(samples: &RateSamples) -> UsageForecastRates {
    UsageForecastRates {
        five_hour_percent_per_hour: estimate_rate_percent_per_hour(&samples.five_hour),
        weekly_percent_per_hour: estimate_rate_percent_per_hour(&samples.weekly),
    }
}

impl UsageForecastRates {
    fn has_any(&self) -> bool {
        self.five_hour_percent_per_hour.is_some() || self.weekly_percent_per_hour.is_some()
    }

    fn all_present_rates_non_positive(self) -> bool {
        [
            self.five_hour_percent_per_hour,
            self.weekly_percent_per_hour,
        ]
        .into_iter()
        .flatten()
        .all(|rate| rate <= 0.0)
    }
}

impl ForecastInput<'_> {
    fn has_required_rates(&self) -> bool {
        let needs_five_hour = self
            .accounts
            .iter()
            .any(|account| account.five_hour.is_some());
        let needs_weekly = self.accounts.iter().any(|account| account.weekly.is_some());

        (!needs_five_hour || self.rates.five_hour_percent_per_hour.is_some())
            && (!needs_weekly || self.rates.weekly_percent_per_hour.is_some())
    }
}

fn make_account_windows(
    usage: &UsageInfo,
    now: i64,
) -> Option<(Option<SimWindow>, Option<SimWindow>)> {
    let mut five_hour = None;
    let mut weekly = None;

    for window in usage.windows() {
        let target = match window.kind() {
            UsageWindowKind::FiveHour => &mut five_hour,
            UsageWindowKind::Weekly => &mut weekly,
            _ => return None,
        };
        let simulated = make_window(
            window.used_percent,
            window.window_minutes,
            window.resets_at,
            now,
        )?;
        if target.replace(simulated).is_some() {
            return None;
        }
    }

    (five_hour.is_some() || weekly.is_some()).then_some((five_hour, weekly))
}

fn make_window(
    used_percent: Option<f64>,
    window_minutes: Option<i64>,
    resets_at: Option<i64>,
    now: i64,
) -> Option<SimWindow> {
    let used_percent = used_percent?;
    if !used_percent.is_finite() {
        return None;
    }

    let window_seconds = window_minutes?.checked_mul(60)?;
    if window_seconds <= 0 {
        return None;
    }

    let raw_resets_at = resets_at?;
    let (used_percent, resets_at, rate_sample_allowed) = if raw_resets_at <= now {
        (
            0.0,
            normalize_reset(raw_resets_at, window_seconds, now),
            false,
        )
    } else {
        (used_percent.clamp(0.0, LIMIT_PERCENT), raw_resets_at, true)
    };
    Some(SimWindow {
        used_percent,
        window_seconds,
        resets_at,
        rate_sample_allowed,
    })
}

fn normalize_reset(resets_at: i64, window_seconds: i64, now: i64) -> i64 {
    if resets_at > now {
        return resets_at;
    }

    let elapsed = now.saturating_sub(resets_at);
    let periods = elapsed / window_seconds + 1;
    resets_at
        .checked_add(periods.saturating_mul(window_seconds))
        .unwrap_or_else(|| now.saturating_add(window_seconds))
}

fn rate_sample(window: &SimWindow, now: i64, min_elapsed_minutes: f64) -> Option<RateSample> {
    if !window.rate_sample_allowed {
        return None;
    }

    let remaining_seconds = window
        .resets_at
        .saturating_sub(now)
        .clamp(0, window.window_seconds);
    let elapsed_seconds = window.window_seconds.saturating_sub(remaining_seconds);
    if elapsed_seconds <= 0 {
        return None;
    }

    let elapsed_minutes = elapsed_seconds as f64 / 60.0;
    if elapsed_minutes < min_elapsed_minutes {
        return None;
    }

    Some(RateSample {
        used_percent: window.used_percent,
        elapsed_minutes,
    })
}

fn estimate_rate_percent_per_hour(samples: &[RateSample]) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }

    let mut positive_count = 0;
    let mut used_sum = 0.0;
    let mut elapsed_sum = 0.0;
    for sample in samples {
        if sample.used_percent <= 0.0 {
            continue;
        }
        positive_count += 1;
        used_sum += sample.used_percent;
        elapsed_sum += sample.elapsed_minutes;
    }
    if positive_count == 0 {
        return Some(0.0);
    }

    let average_elapsed = elapsed_sum / positive_count as f64;
    if average_elapsed <= 0.0 || !average_elapsed.is_finite() {
        return None;
    }

    Some((used_sum / average_elapsed) * 60.0)
}

fn apply_rate_limit_reached_type(
    usage: &UsageInfo,
    five_hour: &mut Option<SimWindow>,
    weekly: &mut Option<SimWindow>,
) {
    let Some(kind) = usage.rate_limit_reached_type.as_deref() else {
        return;
    };
    if matches!(
        kind,
        "workspace_owner_credits_depleted" | "workspace_member_credits_depleted"
    ) {
        return;
    }
    match (five_hour.as_mut(), weekly.as_mut()) {
        (Some(five_hour), None) => five_hour.used_percent = LIMIT_PERCENT,
        (None, Some(weekly)) => weekly.used_percent = LIMIT_PERCENT,
        (Some(five_hour), Some(weekly))
            if five_hour.used_percent < LIMIT_PERCENT && weekly.used_percent < LIMIT_PERCENT =>
        {
            if five_hour.used_percent > weekly.used_percent {
                five_hour.used_percent = LIMIT_PERCENT;
            } else if weekly.used_percent > five_hour.used_percent {
                weekly.used_percent = LIMIT_PERCENT;
            } else {
                five_hour.used_percent = LIMIT_PERCENT;
                weekly.used_percent = LIMIT_PERCENT;
            }
        }
        (Some(_), Some(_)) | (None, None) => {}
    }
}

fn simulate_forecast(
    forecast: &mut ForecastInput<'_>,
    current_account_id: Option<&str>,
    now: i64,
) -> UsageForecastOutcome {
    let horizon_at = now.saturating_add(FORECAST_HORIZON_SECONDS);
    let mut time = now;
    let mut next_policy_reevaluation = time.saturating_add(POLICY_REEVALUATION_SECONDS);
    apply_due_resets(&mut forecast.accounts, time);
    let mut current_index = initial_account_index(&forecast.accounts, current_account_id, time);

    if current_index.is_none() {
        return if forecast
            .accounts
            .iter()
            .any(|account| account.is_usable_at(time))
        {
            UsageForecastOutcome::NotEnoughData {
                reason: ForecastUnavailableReason::NoComparableUsageWindows,
            }
        } else {
            UsageForecastOutcome::Unavailable {
                at: time,
                limited_by: forecast_limited_by(forecast, time),
                recovery_at: recovery_time(&forecast.accounts, time),
            }
        };
    }

    if !forecast.has_required_rates() {
        return UsageForecastOutcome::NotEnoughData {
            reason: ForecastUnavailableReason::InsufficientRateSamples,
        };
    }
    let rates = forecast.rates;
    if rates.all_present_rates_non_positive() {
        return UsageForecastOutcome::NotExpected {
            horizon_seconds: FORECAST_HORIZON_SECONDS,
        };
    }

    for _ in 0..MAX_SIMULATION_STEPS {
        apply_due_resets(&mut forecast.accounts, time);

        let should_reselect = current_index.is_none_or(|index| {
            forecast
                .accounts
                .get(index)
                .is_none_or(|account| !account.is_usable_at(time))
        }) || time >= next_policy_reevaluation;

        if should_reselect {
            let current_id = current_index
                .and_then(|index| forecast.accounts.get(index))
                .map(|account| account.account.id.as_str());
            current_index = select_account_index(&forecast.accounts, current_id, time);
            if time >= next_policy_reevaluation {
                next_policy_reevaluation = time.saturating_add(POLICY_REEVALUATION_SECONDS);
            }
        }

        let Some(selected_index) = current_index else {
            return if forecast
                .accounts
                .iter()
                .any(|account| account.is_usable_at(time))
            {
                UsageForecastOutcome::NotEnoughData {
                    reason: ForecastUnavailableReason::NoComparableUsageWindows,
                }
            } else {
                UsageForecastOutcome::Unavailable {
                    at: time,
                    limited_by: forecast_limited_by(forecast, time),
                    recovery_at: recovery_time(&forecast.accounts, time),
                }
            };
        };

        if time >= horizon_at {
            return UsageForecastOutcome::NotExpected {
                horizon_seconds: FORECAST_HORIZON_SECONDS,
            };
        }

        let Some(next_time) = next_event_time(
            &forecast.accounts,
            selected_index,
            rates,
            time,
            next_policy_reevaluation,
        ) else {
            return UsageForecastOutcome::NotExpected {
                horizon_seconds: FORECAST_HORIZON_SECONDS,
            };
        };
        let next_time = next_time.min(horizon_at);
        if next_time <= time {
            return UsageForecastOutcome::NotExpected {
                horizon_seconds: FORECAST_HORIZON_SECONDS,
            };
        }

        charge_account(
            &mut forecast.accounts[selected_index],
            &rates,
            next_time - time,
        );
        time = next_time;
    }

    UsageForecastOutcome::NotExpected {
        horizon_seconds: FORECAST_HORIZON_SECONDS,
    }
}

fn initial_account_index(
    accounts: &[SimAccount<'_>],
    current_account_id: Option<&str>,
    now: i64,
) -> Option<usize> {
    current_account_id
        .and_then(|id| {
            accounts
                .iter()
                .position(|account| account.account.id == id && account.is_usable_at(now))
        })
        .or_else(|| select_account_index(accounts, current_account_id, now))
}

fn select_account_index(
    accounts: &[SimAccount<'_>],
    current_account_id: Option<&str>,
    now: i64,
) -> Option<usize> {
    let indexed_usages = accounts
        .iter()
        .enumerate()
        .filter(|(_, account)| account.is_usable_at(now))
        .map(|(index, account)| (index, account.usage_info()))
        .collect::<Vec<_>>();
    let candidates = indexed_usages
        .iter()
        .map(|(index, usage)| AccountUsageCandidate {
            account: accounts[*index].account,
            usage,
        })
        .collect::<Vec<_>>();

    let decision = account_selector::select_account_or_keep_current(
        &candidates,
        SelectionConfig::default(),
        SelectionContext::at(now).with_current_account_id(current_account_id),
    )?;
    let account = match decision {
        AccountSelectionDecision::Selected(selection) => selection.account,
        AccountSelectionDecision::KeepCurrent(account) => account,
    };
    accounts
        .iter()
        .position(|candidate| candidate.account.id == account.id)
}

fn apply_due_resets(accounts: &mut [SimAccount<'_>], now: i64) {
    for account in accounts {
        account.apply_due_resets(now);
    }
}

fn next_event_time(
    accounts: &[SimAccount<'_>],
    selected_index: usize,
    rates: UsageForecastRates,
    now: i64,
    next_policy_reevaluation: i64,
) -> Option<i64> {
    let selected = accounts.get(selected_index)?;
    [
        selected.five_hour.and_then(|window| {
            rates
                .five_hour_percent_per_hour
                .and_then(|rate| window.exhaustion_time(rate, now))
        }),
        selected.weekly.and_then(|window| {
            rates
                .weekly_percent_per_hour
                .and_then(|rate| window.exhaustion_time(rate, now))
        }),
        selected
            .five_hour
            .filter(|window| window.resets_at > now)
            .map(|window| window.resets_at),
        selected
            .weekly
            .filter(|window| window.resets_at > now)
            .map(|window| window.resets_at),
        (next_policy_reevaluation > now).then_some(next_policy_reevaluation),
    ]
    .into_iter()
    .flatten()
    .min()
}

fn charge_account(account: &mut SimAccount<'_>, rates: &UsageForecastRates, seconds: i64) {
    let hours = seconds as f64 / 3600.0;
    if let (Some(window), Some(rate)) =
        (account.five_hour.as_mut(), rates.five_hour_percent_per_hour)
    {
        window.used_percent = (window.used_percent + rate * hours).clamp(0.0, LIMIT_PERCENT);
    }
    if let (Some(window), Some(rate)) = (account.weekly.as_mut(), rates.weekly_percent_per_hour) {
        window.used_percent = (window.used_percent + rate * hours).clamp(0.0, LIMIT_PERCENT);
    }
}

fn limited_by(accounts: &[SimAccount<'_>], now: i64) -> ForecastLimit {
    let five_hour = accounts.iter().any(|account| {
        account
            .five_hour
            .is_some_and(|window| window.used_percent >= LIMIT_PERCENT)
    });
    let weekly = accounts.iter().any(|account| {
        account
            .weekly
            .is_some_and(|window| window.used_percent >= LIMIT_PERCENT)
    });
    let additional = accounts.iter().any(|account| account.is_blocked_at(now));
    match (five_hour, weekly, additional) {
        (false, false, false) => ForecastLimit::Unknown,
        (true, false, false) => ForecastLimit::FiveHour,
        (false, true, false) => ForecastLimit::Weekly,
        (true, true, false) => ForecastLimit::FiveHourAndWeekly,
        (false, false, true) => ForecastLimit::Additional,
        _ => ForecastLimit::Multiple,
    }
}

fn forecast_limited_by(forecast: &ForecastInput<'_>, now: i64) -> ForecastLimit {
    combine_limits(
        limited_by(&forecast.accounts, now),
        forecast
            .fallback_unavailable_limit
            .unwrap_or(ForecastLimit::Unknown),
    )
}

fn recovery_time(accounts: &[SimAccount<'_>], now: i64) -> Option<i64> {
    accounts
        .iter()
        .filter_map(|account| account.recovery_time(now))
        .min()
}

fn credits_depleted(info: &UsageInfo) -> bool {
    if matches!(
        info.rate_limit_reached_type.as_deref(),
        Some("workspace_owner_credits_depleted" | "workspace_member_credits_depleted")
    ) {
        return true;
    }
    if info.unlimited_credits == Some(true) {
        return false;
    }
    if info.has_credits != Some(true) {
        return false;
    }
    info.credits_balance
        .as_deref()
        .and_then(|balance| balance.trim().parse::<f64>().ok())
        .is_some_and(|balance| balance <= 0.0)
}

fn explicit_unavailable_limit(info: &UsageInfo) -> Option<ForecastLimit> {
    match info.rate_limit_reached_type.as_deref()? {
        "workspace_owner_credits_depleted" | "workspace_member_credits_depleted" => {
            Some(ForecastLimit::Credits)
        }
        "workspace_owner_usage_limit_reached" | "workspace_member_usage_limit_reached" => {
            Some(ForecastLimit::UsageLimit)
        }
        "rate_limit_reached" => Some(ForecastLimit::RateLimit),
        _ => Some(ForecastLimit::RateLimit),
    }
}

fn hard_usage_limit(info: &UsageInfo, now: i64) -> Option<ForecastLimit> {
    let five_hour_window = info.five_hour_window();
    let weekly_window = info.weekly_window();
    let five_hour = five_hour_window
        .is_some_and(|window| active_hard_usage(window.used_percent, window.resets_at, now));
    let weekly = weekly_window
        .is_some_and(|window| active_hard_usage(window.used_percent, window.resets_at, now));
    let unclassified = info.windows().any(|window| {
        active_hard_usage(window.used_percent, window.resets_at, now)
            && match window.kind() {
                UsageWindowKind::FiveHour => five_hour_window.is_none(),
                UsageWindowKind::Weekly => weekly_window.is_none(),
                _ => true,
            }
    });
    let usage_limit = match (five_hour, weekly) {
        (true, true) => Some(ForecastLimit::FiveHourAndWeekly),
        (true, false) => Some(ForecastLimit::FiveHour),
        (false, true) => Some(ForecastLimit::Weekly),
        (false, false) => unclassified.then_some(ForecastLimit::Unknown),
    };
    match (
        usage_limit,
        additional_hard_limit_block(info, now).map(|_| ForecastLimit::Additional),
    ) {
        (Some(left), Some(right)) => Some(combine_limits(left, right)),
        (Some(limit), None) | (None, Some(limit)) => Some(limit),
        (None, None) => None,
    }
}

fn active_hard_usage(used_percent: Option<f64>, resets_at: Option<i64>, now: i64) -> bool {
    used_percent.is_some_and(|used| used.is_finite() && used >= LIMIT_PERCENT)
        && resets_at.is_none_or(|resets_at| resets_at > now)
}

fn additional_hard_limit_block(info: &UsageInfo, now: i64) -> Option<AdditionalLimitBlock> {
    let mut found = false;
    let mut recovery_at = Some(now);
    for limit in &info.additional_limits {
        for window in limit.windows() {
            let used_percent = window.used_percent;
            let resets_at = window.resets_at;
            if !used_percent.is_some_and(|used| used.is_finite() && used >= LIMIT_PERCENT) {
                continue;
            }
            let Some(active_recovery) = active_hard_window_recovery(resets_at, now) else {
                continue;
            };
            found = true;
            recovery_at = match (recovery_at, active_recovery) {
                (Some(current), Some(next)) => Some(current.max(next)),
                _ => None,
            };
        }
    }
    found.then_some(AdditionalLimitBlock { recovery_at })
}

fn active_hard_window_recovery(resets_at: Option<i64>, now: i64) -> Option<Option<i64>> {
    match resets_at {
        Some(resets_at) if resets_at <= now => None,
        Some(resets_at) => Some(Some(resets_at)),
        None => Some(None),
    }
}

fn known_unavailable_limit(info: &UsageInfo, now: i64) -> Option<ForecastLimit> {
    match (
        explicit_unavailable_limit(info),
        hard_usage_limit(info, now),
    ) {
        (Some(left), Some(right)) => Some(combine_limits(left, right)),
        (Some(limit), None) | (None, Some(limit)) => Some(limit),
        (None, None) => None,
    }
}

fn record_fallback_limit(fallback: &mut Option<ForecastLimit>, limit: ForecastLimit) {
    *fallback = Some(match *fallback {
        None => limit,
        Some(existing) => combine_limits(existing, limit),
    });
}

fn combine_limits(left: ForecastLimit, right: ForecastLimit) -> ForecastLimit {
    match (left, right) {
        (ForecastLimit::Unknown, limit) | (limit, ForecastLimit::Unknown) => limit,
        (left, right) if left == right => left,
        (ForecastLimit::Multiple, _) | (_, ForecastLimit::Multiple) => ForecastLimit::Multiple,
        _ => ForecastLimit::Multiple,
    }
}

impl SimAccount<'_> {
    fn is_blocked_at(&self, now: i64) -> bool {
        self.blocked_until
            .is_some_and(|blocked_until| blocked_until > now)
    }

    fn is_usable_at(&self, now: i64) -> bool {
        !self.is_blocked_at(now)
            && (self.five_hour.is_some() || self.weekly.is_some())
            && [self.five_hour, self.weekly]
                .into_iter()
                .flatten()
                .all(|window| window.used_percent < LIMIT_PERCENT)
    }

    fn usage_info(&self) -> UsageInfo {
        let (primary, secondary) = match (self.five_hour, self.weekly) {
            (Some(five_hour), Some(weekly)) => (Some(five_hour), Some(weekly)),
            (Some(five_hour), None) => (Some(five_hour), None),
            (None, Some(weekly)) => (Some(weekly), None),
            (None, None) => (None, None),
        };
        UsageInfo {
            account_id: self.account.id.clone(),
            limit_id: Some("codex".to_string()),
            limit_name: None,
            plan_type: self.account.plan_type.clone(),
            primary_used_percent: primary.map(|window| window.used_percent),
            primary_window_minutes: primary.map(|window| window.window_seconds / 60),
            primary_resets_at: primary.map(|window| window.resets_at),
            secondary_used_percent: secondary.map(|window| window.used_percent),
            secondary_window_minutes: secondary.map(|window| window.window_seconds / 60),
            secondary_resets_at: secondary.map(|window| window.resets_at),
            has_credits: None,
            unlimited_credits: None,
            credits_balance: None,
            rate_limit_reset_credits_available: None,
            rate_limit_reached_type: None,
            additional_limits: Vec::new(),
            error: None,
        }
    }

    fn recovery_time(&self, now: i64) -> Option<i64> {
        let blocked_ready = self
            .blocked_until
            .filter(|blocked_until| *blocked_until > now);
        let usage_ready = [self.five_hour, self.weekly]
            .into_iter()
            .flatten()
            .filter(|window| window.used_percent >= LIMIT_PERCENT)
            .map(|window| window.resets_at)
            .max()
            .unwrap_or(now);
        Some(usage_ready.max(blocked_ready.unwrap_or(now)))
    }

    fn apply_due_resets(&mut self, now: i64) {
        if let Some(window) = self.five_hour.as_mut() {
            window.apply_due_reset(now);
        }
        if let Some(window) = self.weekly.as_mut() {
            window.apply_due_reset(now);
        }
        if self
            .blocked_until
            .is_some_and(|blocked_until| blocked_until <= now)
        {
            self.blocked_until = None;
        }
    }
}

impl SimWindow {
    fn apply_due_reset(&mut self, now: i64) {
        if self.resets_at > now {
            return;
        }

        self.used_percent = 0.0;
        self.resets_at = normalize_reset(self.resets_at, self.window_seconds, now);
    }

    fn exhaustion_time(&self, rate_percent_per_hour: f64, now: i64) -> Option<i64> {
        if rate_percent_per_hour <= 0.0 || !rate_percent_per_hour.is_finite() {
            return None;
        }
        if self.used_percent >= LIMIT_PERCENT {
            return Some(now);
        }

        let remaining = LIMIT_PERCENT - self.used_percent;
        let seconds = ((remaining / rate_percent_per_hour) * 3600.0).ceil() as i64;
        Some(now.saturating_add(seconds.max(1)))
    }
}

impl ForecastLimit {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FiveHour => "5-hour",
            Self::Weekly => "weekly",
            Self::FiveHourAndWeekly => "5-hour and weekly",
            Self::Additional => "additional limit",
            Self::Credits => "credits",
            Self::UsageLimit => "usage limit",
            Self::RateLimit => "rate limit",
            Self::Multiple => "multiple limits",
            Self::Unknown => "unknown",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ForecastLimit, ForecastUnavailableReason, POLICY_REEVALUATION_SECONDS,
        UsageForecastOutcome, UsageForecastRates, build_forecast, charge_account,
        forecast_all_usage, initial_account_index, next_event_time, select_account_index,
    };
    use crate::types::{AuthData, RedactedString, StoredAccount, UsageInfo, UsageLimitInfo};
    use chrono::Utc;
    use std::collections::HashMap;

    const NOW: i64 = 1_800_000_000;

    #[test]
    fn estimates_independent_rates_from_average_elapsed_time() {
        let accounts = vec![chatgpt_account("a"), chatgpt_account("b")];
        let usage_by_id = HashMap::from([
            (
                accounts[0].id.clone(),
                usage_info(&accounts[0].id, 60.0, 20.0, 60, 4_320),
            ),
            (
                accounts[1].id.clone(),
                usage_info(&accounts[1].id, 30.0, 40.0, 180, 1_440),
            ),
        ]);

        let forecast = build_forecast(&accounts, &usage_by_id, NOW).expect("forecast input");
        let rates = forecast.rates;

        assert_eq!(rates.five_hour_percent_per_hour, Some(30.0));
        assert!((rates.weekly_percent_per_hour.expect("weekly rate") - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn swapped_window_slots_preserve_forecast() {
        let accounts = vec![chatgpt_account("a")];
        let canonical = usage_info(&accounts[0].id, 30.0, 40.0, 180, 4_320);
        let canonical_forecast = forecast_all_usage(
            &accounts,
            &HashMap::from([(accounts[0].id.clone(), canonical.clone())]),
            Some(accounts[0].id.as_str()),
            NOW,
        );
        let mut swapped = canonical;
        std::mem::swap(
            &mut swapped.primary_used_percent,
            &mut swapped.secondary_used_percent,
        );
        std::mem::swap(
            &mut swapped.primary_window_minutes,
            &mut swapped.secondary_window_minutes,
        );
        std::mem::swap(
            &mut swapped.primary_resets_at,
            &mut swapped.secondary_resets_at,
        );

        let swapped_forecast = forecast_all_usage(
            &accounts,
            &HashMap::from([(accounts[0].id.clone(), swapped)]),
            Some(accounts[0].id.as_str()),
            NOW,
        );

        assert_eq!(swapped_forecast, canonical_forecast);
    }

    #[test]
    fn weekly_only_pool_produces_a_weekly_forecast() {
        let accounts = vec![chatgpt_account("a"), chatgpt_account("b")];
        let mut secondary_weekly = single_window_usage_info(&accounts[1].id, 30.0, 10_080, 8_640);
        secondary_weekly.secondary_used_percent = secondary_weekly.primary_used_percent.take();
        secondary_weekly.secondary_window_minutes = secondary_weekly.primary_window_minutes.take();
        secondary_weekly.secondary_resets_at = secondary_weekly.primary_resets_at.take();
        let usage_by_id = HashMap::from([
            (
                accounts[0].id.clone(),
                single_window_usage_info(&accounts[0].id, 20.0, 10_080, 9_000),
            ),
            (accounts[1].id.clone(), secondary_weekly),
        ]);

        let forecast = forecast_all_usage(&accounts, &usage_by_id, Some("a"), NOW);
        let rates = forecast.rates.expect("weekly forecast rates");

        assert_eq!(rates.five_hour_percent_per_hour, None);
        assert!(rates.weekly_percent_per_hour.is_some());
        assert!(!matches!(
            forecast.outcome,
            UsageForecastOutcome::NotEnoughData { .. }
        ));
    }

    #[test]
    fn five_hour_only_pool_produces_a_five_hour_forecast() {
        let accounts = vec![chatgpt_account("a"), chatgpt_account("b")];
        let usage_by_id = HashMap::from([
            (
                accounts[0].id.clone(),
                single_window_usage_info(&accounts[0].id, 20.0, 300, 240),
            ),
            (
                accounts[1].id.clone(),
                single_window_usage_info(&accounts[1].id, 30.0, 300, 180),
            ),
        ]);

        let forecast = forecast_all_usage(&accounts, &usage_by_id, Some("a"), NOW);
        let rates = forecast.rates.expect("5-hour forecast rates");

        assert!(rates.five_hour_percent_per_hour.is_some());
        assert_eq!(rates.weekly_percent_per_hour, None);
        assert!(!matches!(
            forecast.outcome,
            UsageForecastOutcome::NotEnoughData { .. }
        ));
    }

    #[test]
    fn weekly_only_account_reports_exhaustion_and_recovery() {
        let accounts = vec![chatgpt_account("weekly")];
        let usage_by_id = HashMap::from([(
            accounts[0].id.clone(),
            single_window_usage_info(&accounts[0].id, 50.0, 10_080, 9_000),
        )]);

        let forecast = forecast_all_usage(&accounts, &usage_by_id, Some("weekly"), NOW);

        assert!(matches!(
            forecast.outcome,
            UsageForecastOutcome::Unavailable {
                at,
                limited_by: ForecastLimit::Weekly,
                recovery_at: Some(recovery_at),
            } if at > NOW && at < recovery_at && recovery_at == NOW + 9_000 * 60
        ));
    }

    #[test]
    fn five_hour_only_account_reports_exhaustion_and_recovery() {
        let accounts = vec![chatgpt_account("five-hour")];
        let usage_by_id = HashMap::from([(
            accounts[0].id.clone(),
            single_window_usage_info(&accounts[0].id, 50.0, 300, 240),
        )]);

        let forecast = forecast_all_usage(&accounts, &usage_by_id, Some("five-hour"), NOW);

        assert!(matches!(
            forecast.outcome,
            UsageForecastOutcome::Unavailable {
                at,
                limited_by: ForecastLimit::FiveHour,
                recovery_at: Some(recovery_at),
            } if at == NOW + 60 * 60 && recovery_at == NOW + 240 * 60
        ));
    }

    #[test]
    fn single_window_rate_limit_marks_the_real_window_unavailable() {
        let accounts = vec![chatgpt_account("weekly")];
        let mut usage = single_window_usage_info(&accounts[0].id, 20.0, 10_080, 9_000);
        usage.rate_limit_reached_type = Some("rate_limit_reached".to_string());
        let usage_by_id = HashMap::from([(accounts[0].id.clone(), usage)]);

        let forecast = forecast_all_usage(&accounts, &usage_by_id, Some("weekly"), NOW);

        assert!(matches!(
            forecast.outcome,
            UsageForecastOutcome::Unavailable {
                at,
                limited_by: ForecastLimit::Weekly,
                recovery_at: Some(recovery_at),
            } if at == NOW && recovery_at == NOW + 9_000 * 60
        ));
    }

    #[test]
    fn synthetic_selection_usage_does_not_create_a_missing_window() {
        let accounts = vec![chatgpt_account("weekly")];
        let usage_by_id = HashMap::from([(
            accounts[0].id.clone(),
            single_window_usage_info(&accounts[0].id, 20.0, 10_080, 9_000),
        )]);
        let forecast = build_forecast(&accounts, &usage_by_id, NOW).expect("forecast input");

        let selection_usage = forecast.accounts[0].usage_info();

        assert!(selection_usage.five_hour_window().is_none());
        assert!(selection_usage.weekly_window().is_some());
    }

    #[test]
    fn mixed_single_window_pool_keeps_usable_current_account() {
        let accounts = vec![
            chatgpt_account("five-hour-current"),
            chatgpt_account("weekly-replacement"),
        ];
        let usage_by_id = HashMap::from([
            (
                accounts[0].id.clone(),
                single_window_usage_info(&accounts[0].id, 50.0, 300, 240),
            ),
            (
                accounts[1].id.clone(),
                single_window_usage_info(&accounts[1].id, 20.0, 10_080, 9_000),
            ),
        ]);

        let forecast = forecast_all_usage(&accounts, &usage_by_id, Some("five-hour-current"), NOW);

        assert!(!matches!(
            forecast.outcome,
            UsageForecastOutcome::NotEnoughData {
                reason: ForecastUnavailableReason::NoComparableUsageWindows,
            }
        ));
    }

    #[test]
    fn mixed_single_window_pool_without_current_reports_no_comparable_windows() {
        let accounts = vec![chatgpt_account("five-hour"), chatgpt_account("weekly")];
        let usage_by_id = HashMap::from([
            (
                accounts[0].id.clone(),
                single_window_usage_info(&accounts[0].id, 20.0, 300, 240),
            ),
            (
                accounts[1].id.clone(),
                single_window_usage_info(&accounts[1].id, 20.0, 10_080, 9_000),
            ),
        ]);

        let forecast = forecast_all_usage(&accounts, &usage_by_id, None, NOW);

        assert_eq!(
            forecast.outcome,
            UsageForecastOutcome::NotEnoughData {
                reason: ForecastUnavailableReason::NoComparableUsageWindows,
            }
        );
    }

    #[test]
    fn single_window_pool_reports_insufficient_rate_samples_separately() {
        let accounts = vec![chatgpt_account("weekly")];
        let usage_by_id = HashMap::from([(
            accounts[0].id.clone(),
            single_window_usage_info(&accounts[0].id, 20.0, 10_080, 9_900),
        )]);

        let forecast = forecast_all_usage(&accounts, &usage_by_id, Some("weekly"), NOW);

        assert_eq!(forecast.rates, None);
        assert_eq!(
            forecast.outcome,
            UsageForecastOutcome::NotEnoughData {
                reason: ForecastUnavailableReason::InsufficientRateSamples,
            }
        );
    }

    #[test]
    fn noncanonical_window_reports_not_enough_data() {
        let accounts = vec![chatgpt_account("a")];
        let mut info = usage_info(&accounts[0].id, 30.0, 40.0, 180, 4_320);
        info.primary_window_minutes = Some(120);
        let usage_by_id = HashMap::from([(accounts[0].id.clone(), info)]);

        let forecast =
            forecast_all_usage(&accounts, &usage_by_id, Some(accounts[0].id.as_str()), NOW);

        assert_eq!(
            forecast.outcome,
            UsageForecastOutcome::NotEnoughData {
                reason: ForecastUnavailableReason::IncompleteUsageData,
            }
        );
    }

    #[test]
    fn noncanonical_hard_limit_is_reported_as_unknown() {
        let accounts = vec![chatgpt_account("a")];
        let mut info = usage_info(&accounts[0].id, 100.0, 40.0, 180, 4_320);
        info.primary_window_minutes = Some(120);
        let usage_by_id = HashMap::from([(accounts[0].id.clone(), info)]);

        let forecast =
            forecast_all_usage(&accounts, &usage_by_id, Some(accounts[0].id.as_str()), NOW);

        assert_eq!(
            forecast.outcome,
            UsageForecastOutcome::Unavailable {
                at: NOW,
                limited_by: ForecastLimit::Unknown,
                recovery_at: None,
            }
        );
    }

    #[test]
    fn disabled_accounts_do_not_contribute_capacity_or_rate_samples() {
        let enabled = chatgpt_account("enabled");
        let mut disabled = chatgpt_account("disabled");
        disabled.auto_switch_disabled = true;
        let accounts = vec![enabled, disabled];
        let usage_by_id = HashMap::from([
            (
                accounts[0].id.clone(),
                usage_info(&accounts[0].id, 50.0, 20.0, 240, 9_000),
            ),
            (
                accounts[1].id.clone(),
                usage_info(&accounts[1].id, 90.0, 90.0, 60, 360),
            ),
        ]);

        let forecast = build_forecast(&accounts, &usage_by_id, NOW).expect("forecast input");
        let rates = forecast.rates;

        assert_eq!(forecast.accounts.len(), 1);
        assert_eq!(forecast.accounts[0].account.id, "enabled");
        assert_eq!(rates.five_hour_percent_per_hour, Some(50.0));
        assert!(
            (rates.weekly_percent_per_hour.expect("weekly rate") - 1.111_111_111_111_111_2).abs()
                < 1e-12
        );
    }

    #[test]
    fn zero_used_elapsed_windows_do_not_dilute_positive_rate_samples() {
        let accounts = vec![chatgpt_account("active"), chatgpt_account("idle")];
        let usage_by_id = HashMap::from([
            (
                accounts[0].id.clone(),
                usage_info(&accounts[0].id, 50.0, 20.0, 240, 9_000),
            ),
            (
                accounts[1].id.clone(),
                usage_info(&accounts[1].id, 0.0, 0.0, 60, 4_320),
            ),
        ]);

        let forecast = build_forecast(&accounts, &usage_by_id, NOW).expect("forecast input");
        let rates = forecast.rates;

        assert_eq!(rates.five_hour_percent_per_hour, Some(50.0));
        assert!(
            (rates.weekly_percent_per_hour.expect("weekly rate") - 1.111_111_111_111_111_2).abs()
                < 1e-12
        );
    }

    #[test]
    fn simulation_starts_with_current_account_before_policy_selection() {
        let accounts = vec![chatgpt_account("current"), chatgpt_account("other")];
        let usage_by_id = HashMap::from([
            (
                accounts[0].id.clone(),
                usage_info(&accounts[0].id, 99.0, 10.0, 240, 9_000),
            ),
            (
                accounts[1].id.clone(),
                usage_info(&accounts[1].id, 10.0, 10.0, 240, 9_000),
            ),
        ]);
        let forecast = build_forecast(&accounts, &usage_by_id, NOW).expect("forecast input");

        let index = initial_account_index(&forecast.accounts, Some("current"), NOW)
            .expect("initial account");

        assert_eq!(forecast.accounts[index].account.id, "current");
    }

    #[test]
    fn simulation_reports_mixed_limits_and_recovery() {
        let accounts = vec![chatgpt_account("a"), chatgpt_account("b")];
        let usage_by_id = HashMap::from([
            (
                accounts[0].id.clone(),
                usage_info(&accounts[0].id, 100.0, 20.0, 120, 7_200),
            ),
            (
                accounts[1].id.clone(),
                usage_info(&accounts[1].id, 20.0, 100.0, 240, 360),
            ),
        ]);

        let forecast = forecast_all_usage(&accounts, &usage_by_id, Some("b"), NOW);

        assert!(matches!(
            forecast.outcome,
            UsageForecastOutcome::Unavailable {
                at,
                limited_by: ForecastLimit::FiveHourAndWeekly,
                recovery_at: Some(recovery_at),
            } if at == NOW && recovery_at == NOW + 7_200
        ));
    }

    #[test]
    fn zero_rates_are_not_expected_to_exhaust() {
        let accounts = vec![chatgpt_account("a")];
        let usage_by_id = HashMap::from([(
            accounts[0].id.clone(),
            usage_info(&accounts[0].id, 0.0, 0.0, 240, 9_000),
        )]);

        let forecast = forecast_all_usage(&accounts, &usage_by_id, Some("a"), NOW);

        assert!(matches!(
            forecast.outcome,
            UsageForecastOutcome::NotExpected { .. }
        ));
    }

    #[test]
    fn incomplete_usage_reports_not_enough_data() {
        let accounts = vec![chatgpt_account("a")];
        let mut info = usage_info(&accounts[0].id, 10.0, 10.0, 240, 9_000);
        info.primary_resets_at = None;
        let usage_by_id = HashMap::from([(accounts[0].id.clone(), info)]);

        let forecast = forecast_all_usage(&accounts, &usage_by_id, Some("a"), NOW);

        assert_eq!(
            forecast.outcome,
            UsageForecastOutcome::NotEnoughData {
                reason: ForecastUnavailableReason::IncompleteUsageData,
            }
        );
    }

    #[test]
    fn hard_limit_accounts_are_unavailable_without_complete_windows() {
        let accounts = vec![chatgpt_account("a")];
        let mut info = usage_info(&accounts[0].id, 100.0, 20.0, 120, 9_000);
        info.secondary_resets_at = None;
        let usage_by_id = HashMap::from([(accounts[0].id.clone(), info)]);

        let forecast = forecast_all_usage(&accounts, &usage_by_id, Some("a"), NOW);

        assert!(forecast.rates.is_none());
        assert!(matches!(
            forecast.outcome,
            UsageForecastOutcome::Unavailable {
                at,
                limited_by: ForecastLimit::FiveHour,
                recovery_at: None,
            } if at == NOW
        ));
    }

    #[test]
    fn expired_hard_limit_with_incomplete_usage_reports_not_enough_data() {
        let accounts = vec![chatgpt_account("a")];
        let mut info = usage_info(&accounts[0].id, 100.0, 20.0, 120, 9_000);
        info.primary_resets_at = Some(NOW - 60);
        info.secondary_resets_at = None;
        let usage_by_id = HashMap::from([(accounts[0].id.clone(), info)]);

        let forecast = forecast_all_usage(&accounts, &usage_by_id, Some("a"), NOW);

        assert_eq!(
            forecast.outcome,
            UsageForecastOutcome::NotEnoughData {
                reason: ForecastUnavailableReason::IncompleteUsageData,
            }
        );
    }

    #[test]
    fn additional_hard_limit_accounts_are_blocked_until_reset() {
        let accounts = vec![chatgpt_account("limited"), chatgpt_account("usable")];
        let mut limited = usage_info(&accounts[0].id, 10.0, 10.0, 240, 9_000);
        limited
            .additional_limits
            .push(additional_limit(100.0, 10.0));
        let usage_by_id = HashMap::from([
            (accounts[0].id.clone(), limited),
            (
                accounts[1].id.clone(),
                usage_info(&accounts[1].id, 20.0, 20.0, 240, 9_000),
            ),
        ]);

        let forecast = build_forecast(&accounts, &usage_by_id, NOW).expect("forecast input");

        assert_eq!(forecast.accounts.len(), 2);
        assert!(forecast.accounts[0].is_blocked_at(NOW));
        assert!(!forecast.accounts[0].is_blocked_at(NOW + 120 * 60));
        let selected = initial_account_index(&forecast.accounts, Some("limited"), NOW)
            .expect("selected account");
        assert_eq!(forecast.accounts[selected].account.id, "usable");
    }

    #[test]
    fn additional_hard_limit_accounts_are_unavailable() {
        let accounts = vec![chatgpt_account("a")];
        let mut info = usage_info(&accounts[0].id, 10.0, 10.0, 240, 9_000);
        info.additional_limits.push(additional_limit(100.0, 10.0));
        let usage_by_id = HashMap::from([(accounts[0].id.clone(), info)]);

        let forecast = forecast_all_usage(&accounts, &usage_by_id, Some("a"), NOW);

        assert!(forecast.rates.is_some());
        assert!(matches!(
            forecast.outcome,
            UsageForecastOutcome::Unavailable {
                at,
                limited_by: ForecastLimit::Additional,
                recovery_at: Some(recovery_at),
            } if at == NOW && recovery_at == NOW + 120 * 60
        ));
    }

    #[test]
    fn expired_additional_hard_limit_does_not_block_capacity() {
        let accounts = vec![chatgpt_account("a")];
        let mut info = usage_info(&accounts[0].id, 10.0, 10.0, 240, 9_000);
        let mut additional = additional_limit(100.0, 10.0);
        additional.primary_resets_at = Some(NOW - 60);
        info.additional_limits.push(additional);
        let usage_by_id = HashMap::from([(accounts[0].id.clone(), info)]);

        let forecast = build_forecast(&accounts, &usage_by_id, NOW).expect("forecast input");

        assert!(!forecast.accounts[0].is_blocked_at(NOW));
        assert_eq!(forecast.fallback_unavailable_limit, None);
    }

    #[test]
    fn expired_additional_hard_limit_with_missing_window_does_not_block_capacity() {
        let accounts = vec![chatgpt_account("a")];
        let mut info = usage_info(&accounts[0].id, 10.0, 10.0, 240, 9_000);
        let mut additional = additional_limit(100.0, 10.0);
        additional.primary_window_minutes = None;
        additional.primary_resets_at = Some(NOW - 60);
        info.additional_limits.push(additional);
        let usage_by_id = HashMap::from([(accounts[0].id.clone(), info)]);

        let forecast = build_forecast(&accounts, &usage_by_id, NOW).expect("forecast input");

        assert!(!forecast.accounts[0].is_blocked_at(NOW));
        assert_eq!(forecast.fallback_unavailable_limit, None);
    }

    #[test]
    fn additional_hard_limit_with_missing_window_uses_future_reset() {
        let accounts = vec![chatgpt_account("a")];
        let mut info = usage_info(&accounts[0].id, 10.0, 10.0, 240, 9_000);
        let mut additional = additional_limit(100.0, 10.0);
        additional.primary_window_minutes = None;
        info.additional_limits.push(additional);
        let usage_by_id = HashMap::from([(accounts[0].id.clone(), info)]);

        let forecast = forecast_all_usage(&accounts, &usage_by_id, Some("a"), NOW);

        assert!(matches!(
            forecast.outcome,
            UsageForecastOutcome::Unavailable {
                at,
                limited_by: ForecastLimit::Additional,
                recovery_at: Some(recovery_at),
            } if at == NOW && recovery_at == NOW + 120 * 60
        ));
    }

    #[test]
    fn additional_hard_limit_without_reset_is_unavailable_without_recovery() {
        let accounts = vec![chatgpt_account("a")];
        let mut info = usage_info(&accounts[0].id, 10.0, 10.0, 240, 9_000);
        let mut additional = additional_limit(100.0, 10.0);
        additional.primary_resets_at = None;
        info.additional_limits.push(additional);
        let usage_by_id = HashMap::from([(accounts[0].id.clone(), info)]);

        let forecast = forecast_all_usage(&accounts, &usage_by_id, Some("a"), NOW);

        assert!(matches!(
            forecast.outcome,
            UsageForecastOutcome::Unavailable {
                at,
                limited_by: ForecastLimit::Additional,
                recovery_at: None,
            } if at == NOW
        ));
    }

    #[test]
    fn credits_depleted_accounts_do_not_contribute_capacity() {
        let accounts = vec![chatgpt_account("depleted"), chatgpt_account("usable")];
        let mut info = usage_info(&accounts[0].id, 10.0, 10.0, 240, 9_000);
        info.rate_limit_reached_type = Some("workspace_member_credits_depleted".to_string());
        let usage_by_id = HashMap::from([
            (accounts[0].id.clone(), info),
            (
                accounts[1].id.clone(),
                usage_info(&accounts[1].id, 20.0, 20.0, 240, 9_000),
            ),
        ]);

        let forecast = build_forecast(&accounts, &usage_by_id, NOW).expect("forecast input");

        assert_eq!(forecast.accounts.len(), 1);
        assert_eq!(forecast.accounts[0].account.id, "usable");
    }

    #[test]
    fn credits_depleted_accounts_are_unavailable_without_recovery() {
        let accounts = vec![chatgpt_account("a")];
        let mut info = usage_info(&accounts[0].id, 10.0, 10.0, 240, 9_000);
        info.rate_limit_reached_type = Some("workspace_member_credits_depleted".to_string());
        let usage_by_id = HashMap::from([(accounts[0].id.clone(), info)]);

        let forecast = forecast_all_usage(&accounts, &usage_by_id, Some("a"), NOW);

        assert!(forecast.rates.is_none());
        assert!(matches!(
            forecast.outcome,
            UsageForecastOutcome::Unavailable {
                at,
                limited_by: ForecastLimit::Credits,
                recovery_at: None,
            } if at == NOW
        ));
    }

    #[test]
    fn zero_credit_balance_accounts_are_unavailable_without_recovery() {
        let accounts = vec![chatgpt_account("a")];
        let mut info = usage_info(&accounts[0].id, 10.0, 10.0, 240, 9_000);
        info.has_credits = Some(true);
        info.unlimited_credits = Some(false);
        info.credits_balance = Some("0".to_string());
        let usage_by_id = HashMap::from([(accounts[0].id.clone(), info)]);

        let forecast = forecast_all_usage(&accounts, &usage_by_id, Some("a"), NOW);

        assert!(matches!(
            forecast.outcome,
            UsageForecastOutcome::Unavailable {
                at,
                limited_by: ForecastLimit::Credits,
                recovery_at: None,
            } if at == NOW
        ));
    }

    #[test]
    fn rate_limit_reached_accounts_are_unavailable_even_below_hard_percent() {
        let accounts = vec![chatgpt_account("a")];
        let mut info = usage_info(&accounts[0].id, 80.0, 20.0, 120, 9_000);
        info.rate_limit_reached_type = Some("rate_limit_reached".to_string());
        let usage_by_id = HashMap::from([(accounts[0].id.clone(), info)]);

        let forecast = forecast_all_usage(&accounts, &usage_by_id, Some("a"), NOW);

        assert!(matches!(
            forecast.outcome,
            UsageForecastOutcome::Unavailable {
                at,
                limited_by: ForecastLimit::FiveHour,
                recovery_at: Some(recovery_at),
            } if at == NOW && recovery_at == NOW + 120 * 60
        ));
    }

    #[test]
    fn rate_limit_reached_accounts_are_unavailable_even_with_zero_rate() {
        let accounts = vec![chatgpt_account("a")];
        let mut info = usage_info(&accounts[0].id, 0.0, 0.0, 120, 9_000);
        info.rate_limit_reached_type = Some("rate_limit_reached".to_string());
        let usage_by_id = HashMap::from([(accounts[0].id.clone(), info)]);

        let forecast = forecast_all_usage(&accounts, &usage_by_id, Some("a"), NOW);

        assert!(matches!(
            forecast.outcome,
            UsageForecastOutcome::Unavailable {
                at,
                limited_by: ForecastLimit::FiveHourAndWeekly,
                recovery_at: Some(recovery_at),
            } if at == NOW && recovery_at == NOW + 9_000 * 60
        ));
    }

    #[test]
    fn rate_limit_reached_accounts_are_unavailable_without_complete_windows() {
        let accounts = vec![chatgpt_account("a")];
        let mut info = usage_info(&accounts[0].id, 80.0, 20.0, 120, 9_000);
        info.rate_limit_reached_type = Some("rate_limit_reached".to_string());
        info.primary_resets_at = None;
        let usage_by_id = HashMap::from([(accounts[0].id.clone(), info)]);

        let forecast = forecast_all_usage(&accounts, &usage_by_id, Some("a"), NOW);

        assert!(forecast.rates.is_none());
        assert!(matches!(
            forecast.outcome,
            UsageForecastOutcome::Unavailable {
                at,
                limited_by: ForecastLimit::RateLimit,
                recovery_at: None,
            } if at == NOW
        ));
    }

    #[test]
    fn usage_limit_reached_accounts_are_unavailable_without_complete_windows() {
        let accounts = vec![chatgpt_account("a")];
        let mut info = usage_info(&accounts[0].id, 80.0, 20.0, 120, 9_000);
        info.rate_limit_reached_type = Some("workspace_owner_usage_limit_reached".to_string());
        info.secondary_resets_at = None;
        let usage_by_id = HashMap::from([(accounts[0].id.clone(), info)]);

        let forecast = forecast_all_usage(&accounts, &usage_by_id, Some("a"), NOW);

        assert!(forecast.rates.is_none());
        assert!(matches!(
            forecast.outcome,
            UsageForecastOutcome::Unavailable {
                at,
                limited_by: ForecastLimit::UsageLimit,
                recovery_at: None,
            } if at == NOW
        ));
    }

    #[test]
    fn mixed_explicit_unavailable_limits_are_reported_without_complete_windows() {
        let accounts = vec![chatgpt_account("a"), chatgpt_account("b")];
        let mut rate_limited = usage_info(&accounts[0].id, 80.0, 20.0, 120, 9_000);
        rate_limited.rate_limit_reached_type = Some("rate_limit_reached".to_string());
        rate_limited.primary_resets_at = None;
        let mut credits_depleted = usage_info(&accounts[1].id, 20.0, 20.0, 120, 9_000);
        credits_depleted.rate_limit_reached_type =
            Some("workspace_member_credits_depleted".to_string());
        let usage_by_id = HashMap::from([
            (accounts[0].id.clone(), rate_limited),
            (accounts[1].id.clone(), credits_depleted),
        ]);

        let forecast = forecast_all_usage(&accounts, &usage_by_id, Some("a"), NOW);

        assert!(matches!(
            forecast.outcome,
            UsageForecastOutcome::Unavailable {
                limited_by: ForecastLimit::Multiple,
                ..
            }
        ));
    }

    #[test]
    fn simulated_and_skipped_unavailable_limits_are_combined() {
        let accounts = vec![chatgpt_account("a"), chatgpt_account("b")];
        let exhausted = usage_info(&accounts[0].id, 100.0, 20.0, 120, 9_000);
        let mut credits_depleted = usage_info(&accounts[1].id, 20.0, 20.0, 120, 9_000);
        credits_depleted.has_credits = Some(true);
        credits_depleted.unlimited_credits = Some(false);
        credits_depleted.credits_balance = Some("0".to_string());
        let usage_by_id = HashMap::from([
            (accounts[0].id.clone(), exhausted),
            (accounts[1].id.clone(), credits_depleted),
        ]);

        let forecast = forecast_all_usage(&accounts, &usage_by_id, Some("a"), NOW);

        assert!(matches!(
            forecast.outcome,
            UsageForecastOutcome::Unavailable {
                limited_by: ForecastLimit::Multiple,
                recovery_at: Some(recovery_at),
                ..
            } if recovery_at == NOW + 120 * 60
        ));
    }

    #[test]
    fn unavailable_accounts_do_not_require_rate_samples() {
        let accounts = vec![chatgpt_account("a")];
        let mut info = usage_info(&accounts[0].id, 0.0, 0.0, 290, 9_900);
        info.rate_limit_reached_type = Some("rate_limit_reached".to_string());
        let usage_by_id = HashMap::from([(accounts[0].id.clone(), info)]);

        let forecast = forecast_all_usage(&accounts, &usage_by_id, Some("a"), NOW);

        assert!(forecast.rates.is_none());
        assert!(matches!(
            forecast.outcome,
            UsageForecastOutcome::Unavailable {
                at,
                limited_by: ForecastLimit::FiveHourAndWeekly,
                recovery_at: Some(recovery_at),
            } if at == NOW && recovery_at == NOW + 9_900 * 60
        ));
    }

    #[test]
    fn zero_elapsed_window_skips_rate_sample_but_keeps_capacity() {
        let accounts = vec![chatgpt_account("current"), chatgpt_account("other")];
        let usage_by_id = HashMap::from([
            (
                accounts[0].id.clone(),
                usage_info(&accounts[0].id, 0.0, 10.0, 300, 9_000),
            ),
            (
                accounts[1].id.clone(),
                usage_info(&accounts[1].id, 10.0, 10.0, 240, 9_000),
            ),
        ]);

        let forecast = build_forecast(&accounts, &usage_by_id, NOW).expect("forecast input");
        let rates = forecast.rates;

        assert_eq!(forecast.accounts.len(), 2);
        assert_eq!(rates.five_hour_percent_per_hour, Some(10.0));
    }

    #[test]
    fn short_elapsed_windows_do_not_contribute_unstable_rate_samples() {
        let accounts = vec![chatgpt_account("current"), chatgpt_account("other")];
        let usage_by_id = HashMap::from([
            (
                accounts[0].id.clone(),
                usage_info(&accounts[0].id, 80.0, 80.0, 290, 9_900),
            ),
            (
                accounts[1].id.clone(),
                usage_info(&accounts[1].id, 10.0, 10.0, 240, 9_000),
            ),
        ]);

        let forecast = build_forecast(&accounts, &usage_by_id, NOW).expect("forecast input");
        let rates = forecast.rates;

        assert_eq!(forecast.accounts.len(), 2);
        assert_eq!(rates.five_hour_percent_per_hour, Some(10.0));
        assert!(
            (rates.weekly_percent_per_hour.expect("weekly rate") - 0.555_555_555_555_555_6).abs()
                < 1e-12
        );
    }

    #[test]
    fn expired_reset_windows_keep_capacity_without_rate_sample() {
        let accounts = vec![chatgpt_account("stale"), chatgpt_account("fresh")];
        let mut stale = usage_info(&accounts[0].id, 90.0, 20.0, 240, 9_000);
        stale.primary_resets_at = Some(NOW - 60);
        let usage_by_id = HashMap::from([
            (accounts[0].id.clone(), stale),
            (
                accounts[1].id.clone(),
                usage_info(&accounts[1].id, 10.0, 20.0, 240, 9_000),
            ),
        ]);

        let forecast = build_forecast(&accounts, &usage_by_id, NOW).expect("forecast input");
        let rates = forecast.rates;

        assert_eq!(forecast.accounts.len(), 2);
        assert_eq!(
            forecast.accounts[0]
                .five_hour
                .expect("5-hour window")
                .used_percent,
            0.0
        );
        assert_eq!(rates.five_hour_percent_per_hour, Some(10.0));
    }

    #[test]
    fn next_event_time_ignores_non_selected_resets() {
        let accounts = vec![chatgpt_account("selected"), chatgpt_account("idle")];
        let usage_by_id = HashMap::from([
            (
                accounts[0].id.clone(),
                usage_info(&accounts[0].id, 10.0, 10.0, 120, 9_000),
            ),
            (
                accounts[1].id.clone(),
                usage_info(&accounts[1].id, 10.0, 10.0, 1, 9_000),
            ),
        ]);
        let forecast = build_forecast(&accounts, &usage_by_id, NOW).expect("forecast input");

        let next = next_event_time(&forecast.accounts, 0, forecast.rates, NOW, NOW + 120 * 60)
            .expect("next event");

        assert_eq!(next, NOW + 120 * 60);
    }

    #[test]
    fn next_event_time_includes_policy_reevaluation() {
        let accounts = vec![chatgpt_account("selected"), chatgpt_account("idle")];
        let usage_by_id = HashMap::from([
            (
                accounts[0].id.clone(),
                usage_info(&accounts[0].id, 10.0, 10.0, 120, 9_000),
            ),
            (
                accounts[1].id.clone(),
                usage_info(&accounts[1].id, 10.0, 10.0, 60, 9_000),
            ),
        ]);
        let forecast = build_forecast(&accounts, &usage_by_id, NOW).expect("forecast input");
        let next_policy_reevaluation = NOW + POLICY_REEVALUATION_SECONDS;

        let next = next_event_time(
            &forecast.accounts,
            0,
            UsageForecastRates {
                five_hour_percent_per_hour: Some(1.0),
                weekly_percent_per_hour: Some(1.0),
            },
            NOW,
            next_policy_reevaluation,
        )
        .expect("next event");

        assert_eq!(next, next_policy_reevaluation);
    }

    #[test]
    fn exhausted_five_hour_window_stops_account_from_weekly_charges() {
        let accounts = vec![chatgpt_account("current"), chatgpt_account("replacement")];
        let usage_by_id = HashMap::from([
            (
                accounts[0].id.clone(),
                usage_info(&accounts[0].id, 99.0, 10.0, 240, 9_000),
            ),
            (
                accounts[1].id.clone(),
                usage_info(&accounts[1].id, 10.0, 10.0, 240, 9_000),
            ),
        ]);
        let mut forecast = build_forecast(&accounts, &usage_by_id, NOW).expect("forecast input");
        let rates = forecast.rates;
        let selected = initial_account_index(&forecast.accounts, Some("current"), NOW)
            .expect("initial account");
        assert_eq!(forecast.accounts[selected].account.id, "current");

        let next = next_event_time(
            &forecast.accounts,
            selected,
            rates,
            NOW,
            NOW + POLICY_REEVALUATION_SECONDS,
        )
        .expect("next event");
        let weekly_before = forecast.accounts[selected]
            .weekly
            .expect("weekly window")
            .used_percent;

        charge_account(&mut forecast.accounts[selected], &rates, next - NOW);
        let weekly_after_exhaustion = forecast.accounts[selected]
            .weekly
            .expect("weekly window")
            .used_percent;

        assert_eq!(
            forecast.accounts[selected]
                .five_hour
                .expect("5-hour window")
                .used_percent,
            100.0
        );
        assert!(weekly_after_exhaustion > weekly_before);
        assert!(!forecast.accounts[selected].is_usable_at(next));
        let replacement = select_account_index(
            &forecast.accounts,
            Some(forecast.accounts[selected].account.id.as_str()),
            next,
        )
        .expect("replacement account");
        assert_eq!(forecast.accounts[replacement].account.id, "replacement");
        let replacement_weekly_before = forecast.accounts[replacement]
            .weekly
            .expect("weekly window")
            .used_percent;

        let replacement_next = next_event_time(
            &forecast.accounts,
            replacement,
            rates,
            next,
            next + POLICY_REEVALUATION_SECONDS,
        )
        .expect("replacement next event");
        charge_account(
            &mut forecast.accounts[replacement],
            &rates,
            replacement_next - next,
        );
        assert!(
            forecast.accounts[replacement]
                .weekly
                .expect("weekly window")
                .used_percent
                > replacement_weekly_before
        );
        assert_eq!(
            forecast.accounts[selected]
                .weekly
                .expect("weekly window")
                .used_percent,
            weekly_after_exhaustion
        );
    }

    fn chatgpt_account(id: &str) -> StoredAccount {
        StoredAccount {
            id: id.to_string(),
            name: id.to_string(),
            email: None,
            plan_type: Some("pro".to_string()),
            chatgpt_user_id: None,
            chatgpt_account_is_fedramp: false,
            token_last_refresh_at: Some(Utc::now()),
            subscription_expires_at: None,
            auth_mode: crate::types::AuthMode::ChatGPT,
            auth_data: AuthData::ChatGPT {
                id_token: RedactedString::new("id-token"),
                access_token: RedactedString::new("access-token"),
                refresh_token: RedactedString::new("refresh-token"),
                account_id: Some(id.to_string()),
            },
            auto_switch_disabled: false,
            created_at: Utc::now(),
            last_used_at: None,
        }
    }

    fn usage_info(
        account_id: &str,
        five_hour_used: f64,
        weekly_used: f64,
        five_hour_remaining_minutes: i64,
        weekly_remaining_minutes: i64,
    ) -> UsageInfo {
        UsageInfo {
            account_id: account_id.to_string(),
            limit_id: Some("codex".to_string()),
            limit_name: None,
            plan_type: Some("pro".to_string()),
            primary_used_percent: Some(five_hour_used),
            primary_window_minutes: Some(300),
            primary_resets_at: Some(NOW + five_hour_remaining_minutes * 60),
            secondary_used_percent: Some(weekly_used),
            secondary_window_minutes: Some(10_080),
            secondary_resets_at: Some(NOW + weekly_remaining_minutes * 60),
            has_credits: None,
            unlimited_credits: None,
            credits_balance: None,
            rate_limit_reset_credits_available: None,
            rate_limit_reached_type: None,
            additional_limits: Vec::new(),
            error: None,
        }
    }

    fn single_window_usage_info(
        account_id: &str,
        used_percent: f64,
        window_minutes: i64,
        remaining_minutes: i64,
    ) -> UsageInfo {
        UsageInfo {
            account_id: account_id.to_string(),
            limit_id: Some("codex".to_string()),
            limit_name: None,
            plan_type: Some("pro".to_string()),
            primary_used_percent: Some(used_percent),
            primary_window_minutes: Some(window_minutes),
            primary_resets_at: Some(NOW + remaining_minutes * 60),
            secondary_used_percent: None,
            secondary_window_minutes: None,
            secondary_resets_at: None,
            has_credits: None,
            unlimited_credits: None,
            credits_balance: None,
            rate_limit_reset_credits_available: None,
            rate_limit_reached_type: None,
            additional_limits: Vec::new(),
            error: None,
        }
    }

    fn additional_limit(five_hour_used: f64, weekly_used: f64) -> UsageLimitInfo {
        UsageLimitInfo {
            limit_id: Some("gpt-5.3-codex-spark".to_string()),
            limit_name: Some("GPT-5.3-Codex-Spark".to_string()),
            primary_used_percent: Some(five_hour_used),
            primary_window_minutes: Some(300),
            primary_resets_at: Some(NOW + 120 * 60),
            secondary_used_percent: Some(weekly_used),
            secondary_window_minutes: Some(10_080),
            secondary_resets_at: Some(NOW + 9_000 * 60),
        }
    }
}
