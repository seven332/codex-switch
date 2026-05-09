use anyhow::{Context, Result};
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};

use crate::codex_http;
use crate::token;
use crate::types::{
    AdditionalRateLimitDetails, AuthData, CreditStatusDetails, RateLimitStatusDetails,
    RateLimitStatusPayload, RateLimitWindowSnapshot, StoredAccount, UsageInfo, UsageLimitInfo,
};

const CHATGPT_BACKEND_API: &str = "https://chatgpt.com/backend-api";

pub async fn get_account_usage(account: &StoredAccount) -> Result<UsageInfo> {
    match &account.auth_data {
        AuthData::ApiKey { .. } => Ok(UsageInfo::unsupported(account.id.clone())),
        AuthData::ChatGPT { .. } => get_usage_with_chatgpt_auth(account).await,
    }
}

pub async fn get_all_account_usage(accounts: &[StoredAccount]) -> Vec<UsageInfo> {
    let mut results = Vec::with_capacity(accounts.len());

    for account in accounts {
        let info = match get_account_usage(account).await {
            Ok(info) => info,
            Err(err) => UsageInfo::error(account.id.clone(), err.to_string()),
        };
        results.push(info);
    }

    results
}

async fn get_usage_with_chatgpt_auth(account: &StoredAccount) -> Result<UsageInfo> {
    let fresh_account = token::ensure_chatgpt_tokens_fresh(account).await?;
    let (access_token, account_id, account_is_fedramp) = extract_chatgpt_auth(&fresh_account)?;

    let response = send_chatgpt_usage_request(access_token, account_id, account_is_fedramp).await?;
    if response.status() == StatusCode::UNAUTHORIZED {
        let refreshed_account = token::refresh_chatgpt_tokens(&fresh_account).await?;
        let (retry_token, retry_account_id, retry_is_fedramp) =
            extract_chatgpt_auth(&refreshed_account)?;
        let retry_response =
            send_chatgpt_usage_request(retry_token, retry_account_id, retry_is_fedramp).await?;
        return parse_usage_response(&refreshed_account.id, retry_response).await;
    }

    parse_usage_response(&fresh_account.id, response).await
}

fn extract_chatgpt_auth(account: &StoredAccount) -> Result<(&str, Option<&str>, bool)> {
    match &account.auth_data {
        AuthData::ChatGPT {
            access_token,
            account_id,
            ..
        } => Ok((
            access_token.as_str(),
            account_id.as_deref(),
            account.chatgpt_account_is_fedramp,
        )),
        AuthData::ApiKey { .. } => anyhow::bail!("Account is not using ChatGPT OAuth"),
    }
}

async fn send_chatgpt_usage_request(
    access_token: &str,
    chatgpt_account_id: Option<&str>,
    chatgpt_account_is_fedramp: bool,
) -> Result<reqwest::Response> {
    let client = reqwest::Client::new();
    let headers =
        build_chatgpt_headers(access_token, chatgpt_account_id, chatgpt_account_is_fedramp)?;

    client
        .get(format!("{CHATGPT_BACKEND_API}/wham/usage"))
        .headers(headers)
        .send()
        .await
        .context("Failed to send usage request")
}

fn build_chatgpt_headers(
    access_token: &str,
    chatgpt_account_id: Option<&str>,
    chatgpt_account_is_fedramp: bool,
) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    codex_http::insert_codex_user_agent_header(&mut headers)?;
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {access_token}")).context("Invalid access token")?,
    );

    if let Some(account_id) = chatgpt_account_id {
        headers.insert(
            HeaderName::from_static("chatgpt-account-id"),
            HeaderValue::from_str(account_id).context("Invalid ChatGPT account ID")?,
        );
    }

    if chatgpt_account_is_fedramp {
        headers.insert(
            HeaderName::from_static("x-openai-fedramp"),
            HeaderValue::from_static("true"),
        );
    }

    Ok(headers)
}

async fn parse_usage_response(account_id: &str, response: reqwest::Response) -> Result<UsageInfo> {
    let status = response.status();

    if !status.is_success() {
        let _body = response.text().await.unwrap_or_default();
        return Ok(UsageInfo::error(
            account_id.to_string(),
            format!("API error: {status}"),
        ));
    }

    let body = response
        .text()
        .await
        .context("Failed to read usage response body")?;
    let payload: RateLimitStatusPayload =
        serde_json::from_str(&body).context("Failed to parse usage response")?;

    Ok(convert_payload_to_usage_info(account_id, payload))
}

fn convert_payload_to_usage_info(account_id: &str, payload: RateLimitStatusPayload) -> UsageInfo {
    let plan_type = Some(payload.plan_type.as_str().to_string());
    let rate_limit_reached_type = payload
        .rate_limit_reached_type
        .as_ref()
        .and_then(|details| details.as_ref())
        .and_then(|details| details.kind.as_str().map(str::to_string));
    let mut snapshots = vec![make_usage_snapshot(
        Some("codex".to_string()),
        None,
        payload.rate_limit,
    )];

    if let Some(Some(additional)) = payload.additional_rate_limits {
        snapshots.extend(additional.into_iter().map(make_additional_usage_snapshot));
    }

    let preferred_index = snapshots
        .iter()
        .position(|snapshot| snapshot.limit_id.as_deref() == Some("codex"))
        .unwrap_or(0);
    let preferred = snapshots.remove(preferred_index);
    let credits = extract_credits(payload.credits);

    UsageInfo {
        account_id: account_id.to_string(),
        limit_id: preferred.limit_id,
        limit_name: preferred.limit_name,
        plan_type,
        primary_used_percent: preferred.primary_used_percent,
        primary_window_minutes: preferred.primary_window_minutes,
        primary_resets_at: preferred.primary_resets_at,
        secondary_used_percent: preferred.secondary_used_percent,
        secondary_window_minutes: preferred.secondary_window_minutes,
        secondary_resets_at: preferred.secondary_resets_at,
        has_credits: credits.as_ref().map(|credits| credits.has_credits),
        unlimited_credits: credits.as_ref().map(|credits| credits.unlimited),
        credits_balance: credits.and_then(|credits| credits.balance.flatten()),
        rate_limit_reached_type,
        additional_limits: snapshots,
        error: None,
    }
}

fn make_additional_usage_snapshot(details: AdditionalRateLimitDetails) -> UsageLimitInfo {
    make_usage_snapshot(
        Some(details.metered_feature),
        Some(details.limit_name),
        details.rate_limit,
    )
}

fn make_usage_snapshot(
    limit_id: Option<String>,
    limit_name: Option<String>,
    rate_limit: Option<Option<Box<RateLimitStatusDetails>>>,
) -> UsageLimitInfo {
    let (primary, secondary) = extract_rate_limits(rate_limit);

    UsageLimitInfo {
        limit_id,
        limit_name,
        primary_used_percent: primary
            .as_ref()
            .map(|window| f64::from(window.used_percent)),
        primary_window_minutes: primary
            .as_ref()
            .map(|window| window.limit_window_seconds)
            .map(window_minutes_from_seconds),
        primary_resets_at: primary.as_ref().map(|window| i64::from(window.reset_at)),
        secondary_used_percent: secondary
            .as_ref()
            .map(|window| f64::from(window.used_percent)),
        secondary_window_minutes: secondary
            .as_ref()
            .map(|window| window.limit_window_seconds)
            .map(window_minutes_from_seconds),
        secondary_resets_at: secondary.as_ref().map(|window| i64::from(window.reset_at)),
    }
}

fn window_minutes_from_seconds(seconds: i32) -> i64 {
    (i64::from(seconds) + 59) / 60
}

fn extract_rate_limits(
    rate_limit: Option<Option<Box<RateLimitStatusDetails>>>,
) -> (
    Option<RateLimitWindowSnapshot>,
    Option<RateLimitWindowSnapshot>,
) {
    let Some(details) = rate_limit.flatten() else {
        return (None, None);
    };
    let details = *details;
    (
        details.primary_window.flatten().map(|window| *window),
        details.secondary_window.flatten().map(|window| *window),
    )
}

fn extract_credits(
    credits: Option<Option<Box<CreditStatusDetails>>>,
) -> Option<CreditStatusDetails> {
    credits.flatten().map(|credits| *credits)
}

#[cfg(test)]
mod tests {
    use super::convert_payload_to_usage_info;
    use crate::types::RateLimitStatusPayload;

    #[test]
    fn usage_payload_accepts_codex_rate_limit_reached_type_field() {
        let payload: RateLimitStatusPayload = serde_json::from_value(serde_json::json!({
            "plan_type": "plus",
            "rate_limit": {
                "allowed": false,
                "limit_reached": true,
                "primary_window": {
                    "used_percent": 100,
                    "limit_window_seconds": 18000,
                    "reset_after_seconds": 300,
                    "reset_at": 1_800_000_000
                },
                "secondary_window": null
            },
            "credits": {
                "has_credits": true,
                "unlimited": false,
                "balance": "12.5"
            },
            "rate_limit_reached_type": {
                "type": "workspace_member_usage_limit_reached"
            }
        }))
        .expect("payload should parse");

        let info = convert_payload_to_usage_info("account-id", payload);

        assert_eq!(
            info.rate_limit_reached_type.as_deref(),
            Some("workspace_member_usage_limit_reached")
        );
        assert_eq!(info.primary_used_percent, Some(100.0));
        assert_eq!(info.primary_window_minutes, Some(300));
        assert_eq!(info.primary_resets_at, Some(1_800_000_000));
        assert_eq!(info.credits_balance.as_deref(), Some("12.5"));
    }
}
