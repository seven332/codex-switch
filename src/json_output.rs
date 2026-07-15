use serde::Serialize;

use crate::auto_switch;
use crate::doctor::DoctorReport;
use crate::store;
use crate::types::{AccountsStore, StoredAccount, UsageInfo, UsageLimitInfo, UsageWindowData};
use crate::usage_forecast::{UsageForecast, UsageForecastOutcome, UsageForecastRates};

const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ListJson {
    schema_version: u32,
    command: &'static str,
    accounts: Vec<AccountJson>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DoctorJson {
    schema_version: u32,
    command: &'static str,
    has_errors: bool,
    checks: Vec<DoctorCheckJson>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct UsageJson {
    schema_version: u32,
    command: &'static str,
    all: bool,
    show_additional: bool,
    generated_at: i64,
    current_account_id: Option<String>,
    accounts: Vec<UsageAccountJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    forecast: Option<ForecastJson>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct UsageJsonEntry<'a> {
    pub(crate) account: &'a StoredAccount,
    pub(crate) usage: &'a UsageInfo,
}

#[derive(Debug, Clone, Serialize)]
struct AccountJson {
    id: String,
    short_id: String,
    name: String,
    email: Option<String>,
    plan_type: Option<String>,
    auth_mode: String,
    auto_switch_disabled: bool,
    current: bool,
    chatgpt_account_is_fedramp: bool,
    created_at: i64,
    last_used_at: Option<i64>,
    token_last_refresh_at: Option<i64>,
    subscription_expires_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
struct UsageAccountJson {
    account: AccountJson,
    usage: UsageInfoJson,
}

#[derive(Debug, Clone, Serialize)]
struct UsageInfoJson {
    status: &'static str,
    supported: bool,
    plan_type: Option<String>,
    limit_id: Option<String>,
    limit_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unavailable_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rate_limit_reached_type: Option<String>,
    five_hour: UsageWindowJson,
    weekly: UsageWindowJson,
    #[serde(skip_serializing_if = "Option::is_none")]
    credits: Option<CreditsJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rate_limit_reset_credits_available: Option<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    additional_limits: Vec<AdditionalLimitJson>,
}

#[derive(Debug, Clone, Serialize)]
struct AdditionalLimitJson {
    limit_id: Option<String>,
    limit_name: Option<String>,
    five_hour: UsageWindowJson,
    weekly: UsageWindowJson,
}

#[derive(Debug, Clone, Serialize)]
struct UsageWindowJson {
    used_percent: Option<f64>,
    left_percent: Option<f64>,
    window_minutes: Option<i64>,
    resets_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
struct CreditsJson {
    has_credits: Option<bool>,
    unlimited: Option<bool>,
    balance: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ForecastJson {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    horizon_seconds: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unavailable_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limited_by: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rates: Option<ForecastRatesJson>,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct ForecastRatesJson {
    five_hour_percent_per_hour: Option<f64>,
    weekly_percent_per_hour: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorCheckJson {
    severity: &'static str,
    category: &'static str,
    message: String,
    hints: Vec<String>,
}

pub(crate) fn list_report(store: &AccountsStore, current_account_id: Option<&str>) -> ListJson {
    ListJson {
        schema_version: SCHEMA_VERSION,
        command: "list",
        accounts: store
            .accounts
            .iter()
            .map(|account| account_json(account, current_account_id))
            .collect(),
    }
}

pub(crate) fn doctor_report(report: &DoctorReport) -> DoctorJson {
    DoctorJson {
        schema_version: SCHEMA_VERSION,
        command: "doctor",
        has_errors: report.has_errors(),
        checks: report.checks().iter().map(doctor_check_json).collect(),
    }
}

pub(crate) fn usage_report(
    entries: &[UsageJsonEntry<'_>],
    current_account_id: Option<&str>,
    all: bool,
    show_additional: bool,
    generated_at: i64,
    forecast: Option<&UsageForecast>,
) -> UsageJson {
    UsageJson {
        schema_version: SCHEMA_VERSION,
        command: "usage",
        all,
        show_additional,
        generated_at,
        current_account_id: current_account_id.map(ToOwned::to_owned),
        accounts: entries
            .iter()
            .map(|entry| UsageAccountJson {
                account: account_json(entry.account, current_account_id),
                usage: usage_info_json(entry.usage, show_additional),
            })
            .collect(),
        forecast: forecast.map(forecast_json),
    }
}

fn account_json(account: &StoredAccount, current_account_id: Option<&str>) -> AccountJson {
    AccountJson {
        id: account.id.clone(),
        short_id: store::short_id(&account.id),
        name: account.name.clone(),
        email: account.email.clone(),
        plan_type: account.plan_type.clone(),
        auth_mode: account.auth_mode.to_string(),
        auto_switch_disabled: account.auto_switch_disabled,
        current: current_account_id == Some(account.id.as_str()),
        chatgpt_account_is_fedramp: account.chatgpt_account_is_fedramp,
        created_at: account.created_at.timestamp(),
        last_used_at: account.last_used_at.map(|value| value.timestamp()),
        token_last_refresh_at: account.token_last_refresh_at.map(|value| value.timestamp()),
        subscription_expires_at: account
            .subscription_expires_at
            .map(|value| value.timestamp()),
    }
}

fn doctor_check_json(check: &crate::doctor::DoctorCheck) -> DoctorCheckJson {
    DoctorCheckJson {
        severity: check.severity.as_str(),
        category: check.category,
        message: check.message.clone(),
        hints: check.hints.clone(),
    }
}

fn usage_info_json(info: &UsageInfo, show_additional: bool) -> UsageInfoJson {
    let supported = !matches!(info.error.as_deref(), Some("usage unsupported"));
    let unavailable_reason = auto_switch::usage_unavailable_reason(info);
    let status = usage_status(info, unavailable_reason.as_deref());

    UsageInfoJson {
        status,
        supported,
        plan_type: info.plan_type.clone(),
        limit_id: info.limit_id.clone(),
        limit_name: info.limit_name.clone(),
        error: info.error.clone(),
        unavailable_reason,
        rate_limit_reached_type: info.rate_limit_reached_type.clone(),
        five_hour: usage_window_data_json(info.five_hour_window()),
        weekly: usage_window_data_json(info.weekly_window()),
        credits: credits_json(info),
        rate_limit_reset_credits_available: info
            .rate_limit_reset_credits_available
            .map(|count| count.max(0)),
        additional_limits: if show_additional {
            info.additional_limits
                .iter()
                .map(additional_limit_json)
                .collect()
        } else {
            Vec::new()
        },
    }
}

fn usage_status(info: &UsageInfo, unavailable_reason: Option<&str>) -> &'static str {
    if matches!(info.error.as_deref(), Some("usage unsupported")) {
        return "unsupported";
    }
    if info.error.is_some() {
        return "error";
    }
    if unavailable_reason.is_some() {
        return "unavailable";
    }
    "ok"
}

fn usage_window_json(
    used_percent: Option<f64>,
    window_minutes: Option<i64>,
    resets_at: Option<i64>,
) -> UsageWindowJson {
    let used_percent = finite_percent(used_percent);
    UsageWindowJson {
        used_percent,
        left_percent: used_percent.map(usage_left_percent),
        window_minutes,
        resets_at,
    }
}

fn usage_window_data_json(window: Option<UsageWindowData>) -> UsageWindowJson {
    window.map_or_else(
        || usage_window_json(None, None, None),
        |window| usage_window_json(window.used_percent, window.window_minutes, window.resets_at),
    )
}

fn finite_percent(value: Option<f64>) -> Option<f64> {
    value.filter(|value| value.is_finite())
}

fn usage_left_percent(used_percent: f64) -> f64 {
    (100.0 - used_percent).clamp(0.0, 100.0)
}

fn credits_json(info: &UsageInfo) -> Option<CreditsJson> {
    if info.has_credits.is_none()
        && info.unlimited_credits.is_none()
        && info.credits_balance.is_none()
    {
        return None;
    }

    Some(CreditsJson {
        has_credits: info.has_credits,
        unlimited: info.unlimited_credits,
        balance: info.credits_balance.clone(),
    })
}

fn additional_limit_json(limit: &UsageLimitInfo) -> AdditionalLimitJson {
    AdditionalLimitJson {
        limit_id: limit.limit_id.clone(),
        limit_name: limit.limit_name.clone(),
        five_hour: usage_window_data_json(limit.five_hour_window()),
        weekly: usage_window_data_json(limit.weekly_window()),
    }
}

fn forecast_json(forecast: &UsageForecast) -> ForecastJson {
    match forecast.outcome {
        UsageForecastOutcome::NotExpected { horizon_seconds } => ForecastJson {
            status: "not_expected",
            reason: None,
            horizon_seconds: Some(horizon_seconds),
            unavailable_at: None,
            limited_by: None,
            recovery_at: None,
            rates: forecast.rates.map(forecast_rates_json),
        },
        UsageForecastOutcome::Unavailable {
            at,
            limited_by,
            recovery_at,
        } => ForecastJson {
            status: "unavailable",
            reason: None,
            horizon_seconds: None,
            unavailable_at: Some(at),
            limited_by: Some(limited_by.as_str()),
            recovery_at,
            rates: forecast.rates.map(forecast_rates_json),
        },
        UsageForecastOutcome::NotEnoughData { reason } => {
            forecast_unavailable_json(reason.as_str())
        }
    }
}

fn forecast_unavailable_json(reason: &'static str) -> ForecastJson {
    ForecastJson {
        status: "not_enough_data",
        reason: Some(reason),
        horizon_seconds: None,
        unavailable_at: None,
        limited_by: None,
        recovery_at: None,
        rates: None,
    }
}

fn forecast_rates_json(rates: UsageForecastRates) -> ForecastRatesJson {
    ForecastRatesJson {
        five_hour_percent_per_hour: finite_percent(rates.five_hour_percent_per_hour),
        weekly_percent_per_hour: finite_percent(rates.weekly_percent_per_hour),
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use serde_json::{Value, json};

    use super::*;
    use crate::doctor::{DoctorCheck, DoctorReport};
    use crate::types::{NewChatGptAccount, RedactedString};
    use crate::usage_forecast::{ForecastLimit, ForecastUnavailableReason, UsageForecastOutcome};

    #[test]
    fn list_json_omits_secret_fields_and_values() {
        let mut api =
            StoredAccount::new_api_key("api".to_string(), "sk-secret-json-output".to_string());
        api.id = "api-account-id".to_string();
        api.created_at = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let oauth = StoredAccount::new_chatgpt(NewChatGptAccount {
            name: "oauth".to_string(),
            email: Some("user@example.com".to_string()),
            plan_type: Some("pro".to_string()),
            chatgpt_user_id: Some("user-id".to_string()),
            chatgpt_account_is_fedramp: false,
            token_last_refresh_at: Utc.timestamp_opt(1_700_000_010, 0).unwrap(),
            subscription_expires_at: Some(Utc.timestamp_opt(1_800_000_000, 0).unwrap()),
            id_token: RedactedString::new("id-token-secret-json-output"),
            access_token: RedactedString::new("access-token-secret-json-output"),
            refresh_token: RedactedString::new("refresh-token-secret-json-output"),
            account_id: Some("chatgpt-account".to_string()),
        });
        let store = AccountsStore {
            version: 1,
            accounts: vec![api, oauth],
            masked_account_ids: Vec::new(),
        };

        let json = serde_json::to_string(&list_report(&store, Some("api-account-id")))
            .expect("list JSON should serialize");

        assert!(json.contains("\"schema_version\":1"));
        assert!(json.contains("\"current\":true"));
        assert!(!json.contains("auth_data"));
        assert!(!json.contains("OPENAI_API_KEY"));
        assert!(!json.contains("sk-secret-json-output"));
        assert!(!json.contains("id-token-secret-json-output"));
        assert!(!json.contains("access-token-secret-json-output"));
        assert!(!json.contains("refresh-token-secret-json-output"));

        let value: Value = serde_json::from_str(&json).expect("list JSON should parse");
        assert_eq!(value["accounts"][0]["created_at"], json!(1_700_000_000));
        assert_eq!(
            value["accounts"][1]["token_last_refresh_at"],
            json!(1_700_000_010)
        );
    }

    #[test]
    fn list_json_handles_empty_store() {
        let store = AccountsStore::default();

        let value = serde_json::to_value(list_report(&store, None))
            .expect("empty list JSON should serialize");

        assert_eq!(value["schema_version"], json!(1));
        assert_eq!(value["command"], json!("list"));
        assert_eq!(value["accounts"], json!([]));
    }

    #[test]
    fn usage_json_reports_statuses_and_hides_additional_limits_by_default() {
        let now = 1_800_000_000;
        let mut account = StoredAccount::new_api_key("work".to_string(), "sk-test".to_string());
        account.id = "account-id".to_string();
        account.auto_switch_disabled = true;
        account.created_at = Utc.timestamp_opt(now - 60, 0).unwrap();
        let info = usage_info_with_additional_limit("account-id", now);

        let report = usage_report(
            &[UsageJsonEntry {
                account: &account,
                usage: &info,
            }],
            Some("account-id"),
            false,
            false,
            now,
            None,
        );
        let value = serde_json::to_value(report).expect("usage JSON should serialize");

        assert_eq!(value["schema_version"], json!(1));
        assert_eq!(value["command"], json!("usage"));
        assert_eq!(value["accounts"][0]["account"]["current"], json!(true));
        assert_eq!(
            value["accounts"][0]["account"]["auto_switch_disabled"],
            json!(true)
        );
        assert_eq!(value["accounts"][0]["usage"]["status"], json!("ok"));
        assert_eq!(
            value["accounts"][0]["usage"]["five_hour"]["left_percent"],
            json!(90.0)
        );
        assert!(value["accounts"][0]["usage"]["additional_limits"].is_null());
    }

    #[test]
    fn usage_json_maps_canonical_windows_by_duration() {
        let now = 1_800_000_000;
        let mut info = usage_info_with_additional_limit("account-id", now);
        std::mem::swap(
            &mut info.primary_used_percent,
            &mut info.secondary_used_percent,
        );
        std::mem::swap(
            &mut info.primary_window_minutes,
            &mut info.secondary_window_minutes,
        );
        std::mem::swap(&mut info.primary_resets_at, &mut info.secondary_resets_at);

        let value = serde_json::to_value(usage_info_json(&info, false))
            .expect("usage JSON should serialize");

        assert_eq!(value["five_hour"]["used_percent"], json!(10.0));
        assert_eq!(value["five_hour"]["window_minutes"], json!(300));
        assert_eq!(value["weekly"]["used_percent"], json!(20.0));
        assert_eq!(value["weekly"]["window_minutes"], json!(10_080));
    }

    #[test]
    fn usage_json_keeps_missing_canonical_window_objects_compatible() {
        let now = 1_800_000_000;
        let mut info = usage_info_with_additional_limit("account-id", now);
        info.primary_window_minutes = Some(120);
        info.secondary_used_percent = None;
        info.secondary_window_minutes = None;
        info.secondary_resets_at = None;

        let value = serde_json::to_value(usage_info_json(&info, false))
            .expect("usage JSON should serialize");

        assert!(value["five_hour"].is_object());
        assert!(value["five_hour"]["used_percent"].is_null());
        assert!(value["weekly"].is_object());
        assert!(value["weekly"]["used_percent"].is_null());
    }

    #[test]
    fn usage_json_handles_empty_account_set() {
        let report = usage_report(&[], None, true, false, 1_800_000_000, None);
        let value = serde_json::to_value(report).expect("empty usage JSON should serialize");

        assert_eq!(value["schema_version"], json!(1));
        assert_eq!(value["command"], json!("usage"));
        assert_eq!(value["all"], json!(true));
        assert_eq!(value["generated_at"], json!(1_800_000_000));
        assert_eq!(value["current_account_id"], Value::Null);
        assert_eq!(value["accounts"], json!([]));
        assert!(value["forecast"].is_null());
    }

    #[test]
    fn usage_json_includes_additional_limits_when_requested() {
        let now = 1_800_000_000;
        let mut account = StoredAccount::new_api_key("work".to_string(), "sk-test".to_string());
        account.id = "account-id".to_string();
        let info = usage_info_with_additional_limit("account-id", now);
        let forecast = UsageForecast {
            rates: None,
            outcome: UsageForecastOutcome::NotEnoughData {
                reason: ForecastUnavailableReason::IncompleteUsageData,
            },
        };

        let report = usage_report(
            &[UsageJsonEntry {
                account: &account,
                usage: &info,
            }],
            Some("account-id"),
            true,
            true,
            now,
            Some(&forecast),
        );
        let value = serde_json::to_value(report).expect("usage JSON should serialize");

        assert_eq!(
            value["accounts"][0]["usage"]["additional_limits"][0]["limit_name"],
            json!("GPT-5.3-Codex-Spark")
        );
        assert_eq!(value["forecast"]["status"], json!("not_enough_data"));
        assert_eq!(value["forecast"]["reason"], json!("incomplete usage data"));
    }

    #[test]
    fn usage_json_reports_unsupported_and_error_accounts() {
        let account = StoredAccount::new_api_key("work".to_string(), "sk-test".to_string());
        let unsupported = UsageInfo::unsupported(account.id.clone());
        let error = UsageInfo::error(account.id.clone(), "refresh failed".to_string());

        assert_eq!(usage_info_json(&unsupported, false).status, "unsupported");
        assert_eq!(usage_info_json(&error, false).status, "error");
    }

    #[test]
    fn usage_json_reports_forecast_outcome() {
        let forecast = UsageForecast {
            rates: Some(UsageForecastRates {
                five_hour_percent_per_hour: Some(12.5),
                weekly_percent_per_hour: Some(0.8),
            }),
            outcome: UsageForecastOutcome::Unavailable {
                at: 1_800_000_000,
                limited_by: ForecastLimit::Weekly,
                recovery_at: Some(1_800_003_600),
            },
        };

        let value =
            serde_json::to_value(forecast_json(&forecast)).expect("forecast JSON should serialize");

        assert_eq!(value["status"], json!("unavailable"));
        assert_eq!(value["limited_by"], json!("weekly"));
        assert_eq!(value["recovery_at"], json!(1_800_003_600));
        assert_eq!(value["rates"]["five_hour_percent_per_hour"], json!(12.5));
    }

    #[test]
    fn usage_json_preserves_rate_keys_for_a_single_active_window() {
        let forecast = UsageForecast {
            rates: Some(UsageForecastRates {
                five_hour_percent_per_hour: None,
                weekly_percent_per_hour: Some(0.8),
            }),
            outcome: UsageForecastOutcome::NotExpected {
                horizon_seconds: 14 * 24 * 60 * 60,
            },
        };

        let value =
            serde_json::to_value(forecast_json(&forecast)).expect("forecast JSON should serialize");

        assert!(value["rates"]["five_hour_percent_per_hour"].is_null());
        assert_eq!(value["rates"]["weekly_percent_per_hour"], json!(0.8));
    }

    #[test]
    fn doctor_json_preserves_check_status_without_human_formatting() {
        let mut report = DoctorReport::default();
        report.push(DoctorCheck::ok("install", "ok"));
        report.push(DoctorCheck::warn("auth", "warn").with_hint("fix"));
        report.push(DoctorCheck::error("store", "error"));

        let value =
            serde_json::to_value(doctor_report(&report)).expect("doctor JSON should serialize");

        assert_eq!(value["schema_version"], json!(1));
        assert_eq!(value["command"], json!("doctor"));
        assert_eq!(value["has_errors"], json!(true));
        assert_eq!(value["checks"][1]["severity"], json!("warn"));
        assert_eq!(value["checks"][1]["hints"], json!(["fix"]));
    }

    #[test]
    fn non_finite_usage_percent_is_omitted() {
        let window = usage_window_json(Some(f64::NAN), Some(300), Some(1_800_000_000));
        let value: Value = serde_json::to_value(window).expect("window JSON should serialize");

        assert!(value["used_percent"].is_null());
        assert!(value["left_percent"].is_null());
    }

    fn usage_info_with_additional_limit(account_id: &str, now: i64) -> UsageInfo {
        UsageInfo {
            account_id: account_id.to_string(),
            limit_id: Some("default".to_string()),
            limit_name: Some("Default".to_string()),
            plan_type: Some("pro".to_string()),
            primary_used_percent: Some(10.0),
            primary_window_minutes: Some(300),
            primary_resets_at: Some(now + 60 * 60),
            secondary_used_percent: Some(20.0),
            secondary_window_minutes: Some(10_080),
            secondary_resets_at: Some(now + 2 * 24 * 60 * 60),
            has_credits: Some(true),
            unlimited_credits: Some(false),
            credits_balance: Some("42".to_string()),
            rate_limit_reset_credits_available: Some(2),
            rate_limit_reached_type: None,
            additional_limits: vec![UsageLimitInfo {
                limit_id: Some("gpt-5.3-codex-spark".to_string()),
                limit_name: Some("GPT-5.3-Codex-Spark".to_string()),
                primary_used_percent: Some(0.0),
                primary_window_minutes: Some(300),
                primary_resets_at: Some(now + 2 * 60 * 60),
                secondary_used_percent: Some(0.0),
                secondary_window_minutes: Some(10_080),
                secondary_resets_at: Some(now + 3 * 24 * 60 * 60),
            }],
            error: None,
        }
    }
}
