use std::future::Future;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use futures_util::stream::{self, StreamExt};
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::Serialize;

use crate::codex_http;
use crate::store;
use crate::token;
use crate::types::{
    AdditionalRateLimitDetails, AuthData, ConsumeRateLimitResetCreditResponse, CreditStatusDetails,
    RateLimitResetCreditsDetails, RateLimitStatusDetails, RateLimitStatusPayload,
    RateLimitWindowSnapshot, StoredAccount, UsageInfo, UsageLimitInfo,
};

const CHATGPT_BACKEND_API: &str = "https://chatgpt.com/backend-api";
const USAGE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const RATE_LIMIT_RESET_DETAILS_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONCURRENT_USAGE_REQUESTS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResetCreditDetailsMode {
    SummaryOnly,
    IncludeExpiration,
}

struct ChatGptUsageResponses {
    usage: reqwest::Response,
    reset_credit_details: Option<Result<RateLimitResetCreditsDetails>>,
}

#[derive(Debug, Clone)]
pub struct AccountUsageFetch {
    pub index: usize,
    pub account_id: String,
    pub result: std::result::Result<UsageInfo, String>,
}

impl AccountUsageFetch {
    fn into_usage_info(self) -> UsageInfo {
        match self.result {
            Ok(info) => info,
            Err(err) => UsageInfo::error(self.account_id, err),
        }
    }
}

pub async fn get_account_usage(account: &StoredAccount) -> Result<UsageInfo> {
    get_account_usage_inner(account, true, ResetCreditDetailsMode::SummaryOnly).await
}

pub async fn get_account_usage_without_auth_write(account: &StoredAccount) -> Result<UsageInfo> {
    get_account_usage_inner(account, false, ResetCreditDetailsMode::SummaryOnly).await
}

pub async fn get_account_usage_for_display(account: &StoredAccount) -> Result<UsageInfo> {
    get_account_usage_inner(account, true, ResetCreditDetailsMode::IncludeExpiration).await
}

pub async fn consume_rate_limit_reset_credit(
    account: &StoredAccount,
    redeem_request_id: &str,
) -> Result<ConsumeRateLimitResetCreditResponse> {
    match &account.auth_data {
        AuthData::ApiKey { .. } => anyhow::bail!("Rate-limit resets require ChatGPT OAuth"),
        AuthData::ChatGPT { .. } => {
            if redeem_request_id.trim().is_empty() {
                anyhow::bail!("redeem request id must not be empty");
            }
            let client = usage_http_client()?;
            consume_rate_limit_reset_credit_with_chatgpt_auth(account, &client, redeem_request_id)
                .await
        }
    }
}

async fn get_account_usage_inner(
    account: &StoredAccount,
    write_current_auth_on_refresh: bool,
    reset_credit_details_mode: ResetCreditDetailsMode,
) -> Result<UsageInfo> {
    match &account.auth_data {
        AuthData::ApiKey { .. } => Ok(UsageInfo::unsupported(account.id.clone())),
        AuthData::ChatGPT { .. } => {
            let client = usage_http_client()?;
            get_usage_with_chatgpt_auth(
                account,
                &client,
                write_current_auth_on_refresh,
                reset_credit_details_mode,
            )
            .await
        }
    }
}

pub async fn get_all_account_usage_for_display(accounts: &[StoredAccount]) -> Vec<UsageInfo> {
    collect_account_usage_for_display(accounts)
        .await
        .into_iter()
        .map(AccountUsageFetch::into_usage_info)
        .collect()
}

pub async fn collect_replacement_account_usage(
    accounts: &[StoredAccount],
    current_account_id: Option<&str>,
) -> Vec<AccountUsageFetch> {
    collect_replacement_account_usage_inner(accounts, current_account_id, true).await
}

pub async fn collect_replacement_account_usage_without_auth_write(
    accounts: &[StoredAccount],
    current_account_id: Option<&str>,
) -> Vec<AccountUsageFetch> {
    collect_replacement_account_usage_inner(accounts, current_account_id, false).await
}

async fn collect_replacement_account_usage_inner(
    accounts: &[StoredAccount],
    current_account_id: Option<&str>,
    write_current_auth_on_refresh: bool,
) -> Vec<AccountUsageFetch> {
    let indexed_accounts = accounts
        .iter()
        .enumerate()
        .filter(|(_, account)| Some(account.id.as_str()) != current_account_id)
        .filter(|(_, account)| account.auto_switch_enabled())
        .collect::<Vec<_>>();

    collect_indexed_account_usage(
        indexed_accounts,
        write_current_auth_on_refresh,
        ResetCreditDetailsMode::SummaryOnly,
    )
    .await
}

async fn collect_account_usage_for_display(accounts: &[StoredAccount]) -> Vec<AccountUsageFetch> {
    let indexed_accounts = accounts.iter().enumerate().collect::<Vec<_>>();

    collect_indexed_account_usage(
        indexed_accounts,
        true,
        ResetCreditDetailsMode::IncludeExpiration,
    )
    .await
}

async fn collect_indexed_account_usage(
    indexed_accounts: Vec<(usize, &StoredAccount)>,
    write_current_auth_on_refresh: bool,
    reset_credit_details_mode: ResetCreditDetailsMode,
) -> Vec<AccountUsageFetch> {
    let mut results = Vec::with_capacity(indexed_accounts.len());
    let mut oauth_accounts = Vec::new();

    for (index, account) in indexed_accounts {
        match &account.auth_data {
            AuthData::ApiKey { .. } => results.push(AccountUsageFetch {
                index,
                account_id: account.id.clone(),
                result: Ok(UsageInfo::unsupported(account.id.clone())),
            }),
            AuthData::ChatGPT { .. } => oauth_accounts.push((index, account)),
        }
    }

    if oauth_accounts.is_empty() {
        return results;
    }

    let client =
        match usage_http_client() {
            Ok(client) => client,
            Err(err) => {
                results.extend(oauth_accounts.into_iter().map(|(index, account)| {
                    fetch_result_for_client_build_error(index, account, &err)
                }));
                results.sort_by_key(|result| result.index);
                return results;
            }
        };

    let owned_oauth_accounts = oauth_accounts
        .into_iter()
        .map(|(index, account)| (index, account.clone()))
        .collect::<Vec<_>>();
    results.extend(
        collect_oauth_account_usage(
            owned_oauth_accounts,
            client,
            write_current_auth_on_refresh,
            reset_credit_details_mode,
        )
        .await,
    );
    results.sort_by_key(|result| result.index);
    results
}

async fn collect_oauth_account_usage(
    indexed_accounts: Vec<(usize, StoredAccount)>,
    client: reqwest::Client,
    write_current_auth_on_refresh: bool,
    reset_credit_details_mode: ResetCreditDetailsMode,
) -> Vec<AccountUsageFetch> {
    collect_indexed_account_usage_with(
        indexed_accounts,
        MAX_CONCURRENT_USAGE_REQUESTS,
        move |account| {
            let client = client.clone();
            async move {
                get_account_usage_with_client(
                    &account,
                    &client,
                    write_current_auth_on_refresh,
                    reset_credit_details_mode,
                )
                .await
                .map_err(|err| err.to_string())
            }
        },
    )
    .await
}

async fn collect_indexed_account_usage_with<F, Fut>(
    indexed_accounts: Vec<(usize, StoredAccount)>,
    max_concurrent: usize,
    fetch: F,
) -> Vec<AccountUsageFetch>
where
    F: Fn(StoredAccount) -> Fut + Clone,
    Fut: Future<Output = std::result::Result<UsageInfo, String>>,
{
    let concurrency = max_concurrent.max(1);
    let mut results = stream::iter(indexed_accounts.into_iter().map(|(index, account)| {
        let account_id = account.id.clone();
        let fetch = fetch.clone();
        async move {
            let result = fetch(account).await;
            AccountUsageFetch {
                index,
                account_id,
                result,
            }
        }
    }))
    .buffer_unordered(concurrency)
    .collect::<Vec<_>>()
    .await;

    results.sort_by_key(|result| result.index);
    results
}

async fn get_account_usage_with_client(
    account: &StoredAccount,
    client: &reqwest::Client,
    write_current_auth_on_refresh: bool,
    reset_credit_details_mode: ResetCreditDetailsMode,
) -> Result<UsageInfo> {
    match &account.auth_data {
        AuthData::ApiKey { .. } => Ok(UsageInfo::unsupported(account.id.clone())),
        AuthData::ChatGPT { .. } => {
            get_usage_with_chatgpt_auth(
                account,
                client,
                write_current_auth_on_refresh,
                reset_credit_details_mode,
            )
            .await
        }
    }
}

fn usage_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(USAGE_REQUEST_TIMEOUT)
        .build()
        .context("Failed to build usage HTTP client")
}

async fn get_usage_with_chatgpt_auth(
    account: &StoredAccount,
    client: &reqwest::Client,
    write_current_auth_on_refresh: bool,
    reset_credit_details_mode: ResetCreditDetailsMode,
) -> Result<UsageInfo> {
    let fresh_account = if write_current_auth_on_refresh {
        token::ensure_chatgpt_tokens_fresh(account).await?
    } else {
        token::ensure_chatgpt_tokens_fresh_without_auth_write(account).await?
    };
    let (access_token, account_id, account_is_fedramp) = extract_chatgpt_auth(&fresh_account)?;

    let responses = send_chatgpt_usage_requests(
        client,
        access_token,
        account_id,
        account_is_fedramp,
        reset_credit_details_mode,
    )
    .await?;
    if responses.usage.status() == StatusCode::UNAUTHORIZED {
        let refreshed_account = if write_current_auth_on_refresh {
            token::refresh_chatgpt_tokens(&fresh_account).await?
        } else {
            token::refresh_chatgpt_tokens_without_auth_write(&fresh_account).await?
        };
        let (retry_token, retry_account_id, retry_is_fedramp) =
            extract_chatgpt_auth(&refreshed_account)?;
        let retry_responses = send_chatgpt_usage_requests(
            client,
            retry_token,
            retry_account_id,
            retry_is_fedramp,
            reset_credit_details_mode,
        )
        .await?;
        return parse_usage_response_and_sync_metadata(&refreshed_account.id, retry_responses)
            .await;
    }

    parse_usage_response_and_sync_metadata(&fresh_account.id, responses).await
}

fn extract_chatgpt_auth(account: &StoredAccount) -> Result<(&str, Option<&str>, bool)> {
    match &account.auth_data {
        AuthData::ChatGPT {
            access_token,
            account_id,
            ..
        } => Ok((
            access_token.expose_secret(),
            account_id.as_deref(),
            account.chatgpt_account_is_fedramp,
        )),
        AuthData::ApiKey { .. } => anyhow::bail!("Account is not using ChatGPT OAuth"),
    }
}

async fn send_chatgpt_usage_request(
    client: &reqwest::Client,
    access_token: &str,
    chatgpt_account_id: Option<&str>,
    chatgpt_account_is_fedramp: bool,
) -> Result<reqwest::Response> {
    let headers =
        build_chatgpt_headers(access_token, chatgpt_account_id, chatgpt_account_is_fedramp)?;

    client
        .get(format!("{CHATGPT_BACKEND_API}/wham/usage"))
        .headers(headers)
        .send()
        .await
        .context("Failed to send usage request")
}

async fn send_chatgpt_usage_requests(
    client: &reqwest::Client,
    access_token: &str,
    chatgpt_account_id: Option<&str>,
    chatgpt_account_is_fedramp: bool,
    reset_credit_details_mode: ResetCreditDetailsMode,
) -> Result<ChatGptUsageResponses> {
    match reset_credit_details_mode {
        ResetCreditDetailsMode::SummaryOnly => Ok(ChatGptUsageResponses {
            usage: send_chatgpt_usage_request(
                client,
                access_token,
                chatgpt_account_id,
                chatgpt_account_is_fedramp,
            )
            .await?,
            reset_credit_details: None,
        }),
        ResetCreditDetailsMode::IncludeExpiration => {
            let (usage, reset_credit_details) = tokio::join!(
                send_chatgpt_usage_request(
                    client,
                    access_token,
                    chatgpt_account_id,
                    chatgpt_account_is_fedramp,
                ),
                fetch_rate_limit_reset_credit_details(
                    client,
                    access_token,
                    chatgpt_account_id,
                    chatgpt_account_is_fedramp,
                ),
            );
            Ok(ChatGptUsageResponses {
                usage: usage?,
                reset_credit_details: Some(reset_credit_details),
            })
        }
    }
}

async fn fetch_rate_limit_reset_credit_details(
    client: &reqwest::Client,
    access_token: &str,
    chatgpt_account_id: Option<&str>,
    chatgpt_account_is_fedramp: bool,
) -> Result<RateLimitResetCreditsDetails> {
    tokio::time::timeout(RATE_LIMIT_RESET_DETAILS_REQUEST_TIMEOUT, async {
        let response = send_chatgpt_rate_limit_reset_credit_details_request(
            client,
            access_token,
            chatgpt_account_id,
            chatgpt_account_is_fedramp,
        )
        .await?;
        parse_rate_limit_reset_credit_details_response(response).await
    })
    .await
    .context("Rate-limit reset credit details request timed out")?
}

async fn send_chatgpt_rate_limit_reset_credit_details_request(
    client: &reqwest::Client,
    access_token: &str,
    chatgpt_account_id: Option<&str>,
    chatgpt_account_is_fedramp: bool,
) -> Result<reqwest::Response> {
    let headers =
        build_chatgpt_headers(access_token, chatgpt_account_id, chatgpt_account_is_fedramp)?;

    client
        .get(format!(
            "{CHATGPT_BACKEND_API}/wham/rate-limit-reset-credits"
        ))
        .headers(headers)
        .send()
        .await
        .context("Failed to send rate-limit reset credit details request")
}

#[derive(Debug, Serialize)]
struct ConsumeRateLimitResetCreditRequest<'a> {
    redeem_request_id: &'a str,
}

async fn consume_rate_limit_reset_credit_with_chatgpt_auth(
    account: &StoredAccount,
    client: &reqwest::Client,
    redeem_request_id: &str,
) -> Result<ConsumeRateLimitResetCreditResponse> {
    let fresh_account = token::ensure_chatgpt_tokens_fresh(account).await?;
    let (access_token, account_id, account_is_fedramp) = extract_chatgpt_auth(&fresh_account)?;

    let response = send_chatgpt_rate_limit_reset_request(
        client,
        access_token,
        account_id,
        account_is_fedramp,
        redeem_request_id,
    )
    .await?;
    if response.status() == StatusCode::UNAUTHORIZED {
        let refreshed_account = token::refresh_chatgpt_tokens(&fresh_account).await?;
        let (retry_token, retry_account_id, retry_is_fedramp) =
            extract_chatgpt_auth(&refreshed_account)?;
        let retry_response = send_chatgpt_rate_limit_reset_request(
            client,
            retry_token,
            retry_account_id,
            retry_is_fedramp,
            redeem_request_id,
        )
        .await?;
        return parse_rate_limit_reset_consume_response(retry_response).await;
    }

    parse_rate_limit_reset_consume_response(response).await
}

async fn send_chatgpt_rate_limit_reset_request(
    client: &reqwest::Client,
    access_token: &str,
    chatgpt_account_id: Option<&str>,
    chatgpt_account_is_fedramp: bool,
    redeem_request_id: &str,
) -> Result<reqwest::Response> {
    let headers =
        build_chatgpt_headers(access_token, chatgpt_account_id, chatgpt_account_is_fedramp)?;

    client
        .post(format!(
            "{CHATGPT_BACKEND_API}/wham/rate-limit-reset-credits/consume"
        ))
        .headers(headers)
        .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
        .json(&ConsumeRateLimitResetCreditRequest { redeem_request_id })
        .send()
        .await
        .context("Failed to send rate-limit reset request")
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

async fn parse_rate_limit_reset_credit_details_response(
    response: reqwest::Response,
) -> Result<RateLimitResetCreditsDetails> {
    let status = response.status();
    if !status.is_success() {
        let _body = response.text().await.unwrap_or_default();
        anyhow::bail!("Rate-limit reset credit details API error: {status}");
    }

    let body = response
        .text()
        .await
        .context("Failed to read rate-limit reset credit details response body")?;
    parse_rate_limit_reset_credit_details_body(&body)
}

fn parse_rate_limit_reset_credit_details_body(body: &str) -> Result<RateLimitResetCreditsDetails> {
    serde_json::from_str(body).context("Failed to parse rate-limit reset credit details response")
}

async fn parse_rate_limit_reset_consume_response(
    response: reqwest::Response,
) -> Result<ConsumeRateLimitResetCreditResponse> {
    let status = response.status();
    if !status.is_success() {
        let _body = response.text().await.unwrap_or_default();
        anyhow::bail!("API error: {status}");
    }

    response
        .json::<ConsumeRateLimitResetCreditResponse>()
        .await
        .context("Failed to parse rate-limit reset response")
}

fn fetch_result_for_client_build_error(
    index: usize,
    account: &StoredAccount,
    err: &anyhow::Error,
) -> AccountUsageFetch {
    let result = match &account.auth_data {
        AuthData::ApiKey { .. } => Ok(UsageInfo::unsupported(account.id.clone())),
        AuthData::ChatGPT { .. } => Err(err.to_string()),
    };
    AccountUsageFetch {
        index,
        account_id: account.id.clone(),
        result,
    }
}

async fn parse_usage_response_and_sync_metadata(
    account_id: &str,
    responses: ChatGptUsageResponses,
) -> Result<UsageInfo> {
    let info = parse_usage_response(account_id, responses.usage).await?;
    let info = apply_rate_limit_reset_credit_details(
        info,
        responses.reset_credit_details,
        Utc::now().timestamp(),
    );
    Ok(sync_usage_metadata_best_effort_with(
        info,
        |account_id, plan_type| {
            store::update_account_usage_metadata(account_id, plan_type).map(|_| ())
        },
    ))
}

fn apply_rate_limit_reset_credit_details(
    mut info: UsageInfo,
    details: Option<Result<RateLimitResetCreditsDetails>>,
    now: i64,
) -> UsageInfo {
    let Some(details) = details else {
        return info;
    };
    let details = match details {
        Ok(details) => details,
        Err(err) => {
            tracing::warn!(
                target: "codex_switch",
                "failed to fetch rate-limit reset credit details; continuing with count-only usage: {err:#}"
            );
            return info;
        }
    };

    if info.error.is_none()
        && info
            .rate_limit_reset_credits_available
            .is_some_and(|available| available > 0)
    {
        info.rate_limit_reset_credits_next_expires_at =
            next_rate_limit_reset_credit_expiration(&details, now);
    }
    info
}

fn next_rate_limit_reset_credit_expiration(
    details: &RateLimitResetCreditsDetails,
    now: i64,
) -> Option<i64> {
    details
        .credits
        .iter()
        .filter(|credit| credit.status == "available")
        .filter_map(|credit| credit.expires_at.as_deref())
        .filter_map(|expires_at| DateTime::parse_from_rfc3339(expires_at).ok())
        .map(|expires_at| expires_at.timestamp())
        .filter(|expires_at| *expires_at > now)
        .min()
}

fn sync_usage_metadata_best_effort_with(
    info: UsageInfo,
    sync: impl FnOnce(&str, Option<String>) -> Result<()>,
) -> UsageInfo {
    if info.error.is_none() {
        let result = sync(&info.account_id, info.plan_type.clone());
        if let Err(err) = result {
            tracing::warn!(
                target: "codex_switch",
                "failed to save account usage metadata; continuing with fetched usage: {err:#}"
            );
        }
    }
    info
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
    let rate_limit_reset_credits_available = payload
        .rate_limit_reset_credits
        .as_ref()
        .map(|summary| summary.available_count);

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
        rate_limit_reset_credits_available,
        rate_limit_reset_credits_next_expires_at: None,
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
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    use super::{
        ConsumeRateLimitResetCreditRequest, apply_rate_limit_reset_credit_details,
        collect_indexed_account_usage_with, collect_replacement_account_usage,
        consume_rate_limit_reset_credit, convert_payload_to_usage_info,
        next_rate_limit_reset_credit_expiration, parse_rate_limit_reset_credit_details_body,
        sync_usage_metadata_best_effort_with,
    };
    use crate::types::{
        RateLimitResetCreditsDetails, RateLimitStatusPayload, StoredAccount, UsageInfo,
        UsageWindowSlot,
    };
    use chrono::{DateTime, Utc};
    use tokio::time::sleep;

    #[test]
    fn consume_rate_limit_reset_request_uses_backend_payload_field() {
        assert_eq!(
            serde_json::to_value(ConsumeRateLimitResetCreditRequest {
                redeem_request_id: "redeem-123",
            })
            .expect("serialize request"),
            serde_json::json!({ "redeem_request_id": "redeem-123" })
        );
    }

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
            "rate_limit_reset_credits": {
                "available_count": 2
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
        assert_eq!(info.rate_limit_reset_credits_available, Some(2));
    }

    #[test]
    fn rate_limit_reset_credit_details_parse_upstream_fields() {
        let details = parse_rate_limit_reset_credit_details_body(
            r#"{
                "credits": [{
                    "id": "credit-1",
                    "reset_type": "codex_rate_limits",
                    "status": "available",
                    "granted_at": "2027-01-15T08:00:00Z",
                    "expires_at": "2027-01-22T08:00:00Z",
                    "title": "Full reset",
                    "description": "Reset weekly usage"
                }, {
                    "id": "credit-2",
                    "reset_type": "codex_rate_limits",
                    "status": "redeemed",
                    "granted_at": "2027-01-15T08:00:00Z",
                    "expires_at": null,
                    "title": null,
                    "description": null
                }],
                "available_count": 1
            }"#,
        )
        .expect("details should parse");

        assert_eq!(details.available_count, 1);
        assert_eq!(details.credits.len(), 2);
        assert_eq!(details.credits[0].status, "available");
        assert_eq!(
            details.credits[0].expires_at.as_deref(),
            Some("2027-01-22T08:00:00Z")
        );
        assert_eq!(details.credits[1].expires_at, None);
    }

    #[test]
    fn reset_credit_expiration_selects_earliest_valid_available_future_timestamp() {
        let now = 1_800_000_000;
        let early = now + 60 * 60;
        let late = now + 2 * 60 * 60;
        let details = reset_credit_details(serde_json::json!([
            { "status": "available", "expires_at": rfc3339(now - 1) },
            { "status": "available", "expires_at": rfc3339(now) },
            { "status": "redeeming", "expires_at": rfc3339(now + 1) },
            { "status": "available", "expires_at": null },
            { "status": "available", "expires_at": "not-a-timestamp" },
            { "status": "available", "expires_at": rfc3339(late) },
            { "status": "available", "expires_at": rfc3339(early) }
        ]));

        assert_eq!(
            next_rate_limit_reset_credit_expiration(&details, now),
            Some(early)
        );
    }

    #[test]
    fn reset_credit_details_enrich_positive_count_without_replacing_it() {
        let now = 1_800_000_000;
        let expires_at = now + 60 * 60;
        let mut info = test_usage_info("account-id");
        info.rate_limit_reset_credits_available = Some(2);
        let details = reset_credit_details(serde_json::json!([
            { "status": "available", "expires_at": rfc3339(expires_at) }
        ]));

        let info = apply_rate_limit_reset_credit_details(info, Some(Ok(details)), now);

        assert_eq!(info.rate_limit_reset_credits_available, Some(2));
        assert_eq!(
            info.rate_limit_reset_credits_next_expires_at,
            Some(expires_at)
        );
    }

    #[test]
    fn reset_credit_details_do_not_enrich_absent_or_non_positive_base_count() {
        let now = 1_800_000_000;
        for available_count in [None, Some(0), Some(-1)] {
            let details = reset_credit_details(serde_json::json!([
                { "status": "available", "expires_at": rfc3339(now + 60 * 60) }
            ]));
            let mut info = test_usage_info("account-id");
            info.rate_limit_reset_credits_available = available_count;

            let info = apply_rate_limit_reset_credit_details(info, Some(Ok(details)), now);

            assert_eq!(info.rate_limit_reset_credits_available, available_count);
            assert_eq!(info.rate_limit_reset_credits_next_expires_at, None);
        }
    }

    #[test]
    fn reset_credit_detail_failure_keeps_successful_count_only_usage() {
        let mut info = test_usage_info("account-id");
        info.rate_limit_reset_credits_available = Some(3);

        let info = apply_rate_limit_reset_credit_details(
            info,
            Some(Err(anyhow::anyhow!("details unavailable"))),
            1_800_000_000,
        );

        assert_eq!(info.rate_limit_reset_credits_available, Some(3));
        assert_eq!(info.rate_limit_reset_credits_next_expires_at, None);
        assert!(info.error.is_none());
    }

    #[test]
    fn usage_metadata_sync_failure_keeps_fetched_usage() {
        let info = test_usage_info("account-id");

        let info = sync_usage_metadata_best_effort_with(info, |account_id, plan_type| {
            assert_eq!(account_id, "account-id");
            assert_eq!(plan_type.as_deref(), Some("pro"));
            anyhow::bail!("accounts lock timed out")
        });

        assert_eq!(info.account_id, "account-id");
        assert_eq!(info.plan_type.as_deref(), Some("pro"));
        assert!(info.error.is_none());
    }

    #[test]
    fn usage_payload_classifies_swapped_windows_by_duration() {
        let payload: RateLimitStatusPayload = serde_json::from_value(serde_json::json!({
            "plan_type": "plus",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                    "used_percent": 20,
                    "limit_window_seconds": 604800,
                    "reset_after_seconds": 300,
                    "reset_at": 1_800_500_000
                },
                "secondary_window": {
                    "used_percent": 10,
                    "limit_window_seconds": 18000,
                    "reset_after_seconds": 300,
                    "reset_at": 1_800_000_000
                }
            }
        }))
        .expect("payload should parse");

        let info = convert_payload_to_usage_info("account-id", payload);
        let five_hour = info.five_hour_window().expect("5-hour window");
        let weekly = info.weekly_window().expect("weekly window");

        assert_eq!(five_hour.slot, UsageWindowSlot::Secondary);
        assert_eq!(five_hour.used_percent, Some(10.0));
        assert_eq!(weekly.slot, UsageWindowSlot::Primary);
        assert_eq!(weekly.used_percent, Some(20.0));
    }

    #[test]
    fn usage_payload_classifies_a_single_secondary_window() {
        let payload: RateLimitStatusPayload = serde_json::from_value(serde_json::json!({
            "plan_type": "plus",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": null,
                "secondary_window": {
                    "used_percent": 10,
                    "limit_window_seconds": 18000,
                    "reset_after_seconds": 300,
                    "reset_at": 1_800_000_000
                }
            }
        }))
        .expect("payload should parse");

        let info = convert_payload_to_usage_info("account-id", payload);

        assert_eq!(info.windows().count(), 1);
        assert_eq!(
            info.five_hour_window().expect("5-hour window").slot,
            UsageWindowSlot::Secondary
        );
        assert!(info.weekly_window().is_none());
    }

    #[test]
    fn usage_payload_classifies_a_single_primary_window() {
        let payload: RateLimitStatusPayload = serde_json::from_value(serde_json::json!({
            "plan_type": "plus",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                    "used_percent": 20,
                    "limit_window_seconds": 604800,
                    "reset_after_seconds": 300,
                    "reset_at": 1_800_500_000
                },
                "secondary_window": null
            }
        }))
        .expect("payload should parse");

        let info = convert_payload_to_usage_info("account-id", payload);

        assert_eq!(info.windows().count(), 1);
        assert_eq!(
            info.weekly_window().expect("weekly window").slot,
            UsageWindowSlot::Primary
        );
        assert!(info.five_hour_window().is_none());
    }

    #[tokio::test]
    async fn indexed_usage_collection_preserves_original_order() {
        let indexed_accounts = vec![(0, test_account("slow")), (1, test_account("fast"))];

        let results =
            collect_indexed_account_usage_with(indexed_accounts, 2, |account| async move {
                if account.id == "slow" {
                    sleep(Duration::from_millis(20)).await;
                }
                Ok(test_usage_info(&account.id))
            })
            .await;

        assert_eq!(
            results
                .iter()
                .map(|result| result.index)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(
            results
                .iter()
                .map(|result| result.account_id.as_str())
                .collect::<Vec<_>>(),
            vec!["slow", "fast"]
        );
    }

    #[tokio::test]
    async fn indexed_usage_collection_respects_concurrency_limit() {
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let indexed_accounts = (0..5)
            .map(|index| (index, test_account(&format!("account-{index}"))))
            .collect::<Vec<_>>();

        let results = collect_indexed_account_usage_with(indexed_accounts, 2, {
            let active = Arc::clone(&active);
            let max_active = Arc::clone(&max_active);
            move |account| {
                let active = Arc::clone(&active);
                let max_active = Arc::clone(&max_active);
                async move {
                    let in_flight = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(in_flight, Ordering::SeqCst);
                    sleep(Duration::from_millis(10)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(test_usage_info(&account.id))
                }
            }
        })
        .await;

        assert_eq!(results.len(), 5);
        assert_eq!(max_active.load(Ordering::SeqCst), 2);
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn replacement_usage_collection_excludes_current_account() {
        let accounts = vec![
            test_account("first"),
            test_account("current"),
            test_account("third"),
        ];

        let results = collect_replacement_account_usage(&accounts, Some("current")).await;

        assert_eq!(
            results
                .iter()
                .map(|result| (result.index, result.account_id.as_str()))
                .collect::<Vec<_>>(),
            vec![(0, "first"), (2, "third")]
        );
        assert!(results.iter().all(|result| result.result.is_ok()));
    }

    #[tokio::test]
    async fn replacement_usage_collection_excludes_disabled_accounts() {
        let mut disabled = test_account("disabled");
        disabled.auto_switch_disabled = true;
        let accounts = vec![test_account("first"), disabled, test_account("third")];

        let results = collect_replacement_account_usage(&accounts, None).await;

        assert_eq!(
            results
                .iter()
                .map(|result| (result.index, result.account_id.as_str()))
                .collect::<Vec<_>>(),
            vec![(0, "first"), (2, "third")]
        );
        assert!(results.iter().all(|result| result.result.is_ok()));
    }

    #[tokio::test]
    async fn consume_rate_limit_reset_credit_rejects_api_key_account() {
        let err = consume_rate_limit_reset_credit(&test_account("api-key"), "redeem-123")
            .await
            .expect_err("API key accounts should not support resets");

        assert!(err.to_string().contains("ChatGPT OAuth"));
    }

    fn test_account(id: &str) -> StoredAccount {
        let mut account = StoredAccount::new_api_key(id.to_string(), "sk-test".to_string());
        account.id = id.to_string();
        account
    }

    fn reset_credit_details(credits: serde_json::Value) -> RateLimitResetCreditsDetails {
        serde_json::from_value(serde_json::json!({
            "credits": credits,
            "available_count": 1
        }))
        .expect("reset credit details")
    }

    fn rfc3339(timestamp: i64) -> String {
        DateTime::<Utc>::from_timestamp(timestamp, 0)
            .expect("valid timestamp")
            .to_rfc3339()
    }

    fn test_usage_info(account_id: &str) -> UsageInfo {
        UsageInfo {
            account_id: account_id.to_string(),
            limit_id: Some("codex".to_string()),
            limit_name: None,
            plan_type: Some("pro".to_string()),
            primary_used_percent: Some(0.0),
            primary_window_minutes: Some(300),
            primary_resets_at: Some(1_800_000_000),
            secondary_used_percent: Some(0.0),
            secondary_window_minutes: Some(10_080),
            secondary_resets_at: Some(1_800_000_000),
            has_credits: None,
            unlimited_credits: None,
            credits_balance: None,
            rate_limit_reset_credits_available: None,
            rate_limit_reset_credits_next_expires_at: None,
            rate_limit_reached_type: None,
            additional_limits: Vec::new(),
            error: None,
        }
    }
}
