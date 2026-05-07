use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize, de};
use tokio::time::sleep;

use chrono::Utc;

use crate::types::{NewChatGptAccount, StoredAccount, parse_chatgpt_id_token_claims};

const DEFAULT_ISSUER: &str = "https://auth.openai.com";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const DEVICE_AUTH_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const DEFAULT_DEVICE_AUTH_POLL_INTERVAL_SECS: u64 = 5;

#[derive(Debug, Clone)]
struct DeviceCode {
    verification_url: String,
    user_code: String,
    device_auth_id: String,
    interval: u64,
}

#[derive(Debug, Deserialize)]
struct UserCodeResponse {
    device_auth_id: String,
    #[serde(alias = "user_code", alias = "usercode")]
    user_code: String,
    #[serde(default, deserialize_with = "deserialize_interval")]
    interval: u64,
}

#[derive(Debug, Serialize)]
struct UserCodeRequest {
    client_id: String,
}

#[derive(Debug, Serialize)]
struct TokenPollRequest {
    device_auth_id: String,
    user_code: String,
}

#[derive(Debug, Deserialize)]
struct CodeSuccessResponse {
    authorization_code: String,
    code_verifier: String,
    #[allow(dead_code)]
    code_challenge: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    id_token: String,
    access_token: String,
    refresh_token: String,
}

pub async fn login(account_name: String) -> Result<StoredAccount> {
    let client = reqwest::Client::new();
    let issuer = DEFAULT_ISSUER.trim_end_matches('/');
    let device_code = request_device_code(&client, issuer).await?;

    print_device_code_prompt(&device_code);

    let code = poll_for_authorization_code(&client, issuer, &device_code).await?;
    let tokens = exchange_code_for_tokens(&client, issuer, &code).await?;
    let claims = parse_chatgpt_id_token_claims(&tokens.id_token);

    Ok(StoredAccount::new_chatgpt(NewChatGptAccount {
        name: account_name,
        email: claims.email,
        plan_type: claims.plan_type,
        chatgpt_user_id: claims.user_id,
        chatgpt_account_is_fedramp: claims.account_is_fedramp,
        token_last_refresh_at: Utc::now(),
        subscription_expires_at: claims.subscription_expires_at,
        id_token: tokens.id_token,
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        account_id: claims.account_id,
    }))
}

async fn request_device_code(client: &reqwest::Client, issuer: &str) -> Result<DeviceCode> {
    let api_base_url = format!("{issuer}/api/accounts");
    let response = client
        .post(format!("{api_base_url}/deviceauth/usercode"))
        .header("Content-Type", "application/json")
        .json(&UserCodeRequest {
            client_id: CLIENT_ID.to_string(),
        })
        .send()
        .await
        .context("Failed to request device code")?;

    if !response.status().is_success() {
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            anyhow::bail!(
                "device code login is not enabled for this Codex server. Use a newer Codex-compatible auth server."
            );
        }

        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("device code request failed with status {status}: {body}");
    }

    let user_code = response
        .json::<UserCodeResponse>()
        .await
        .context("Failed to parse device code response")?;

    Ok(DeviceCode {
        verification_url: format!("{issuer}/codex/device"),
        user_code: user_code.user_code,
        device_auth_id: user_code.device_auth_id,
        interval: normalize_poll_interval(user_code.interval),
    })
}

async fn poll_for_authorization_code(
    client: &reqwest::Client,
    issuer: &str,
    device_code: &DeviceCode,
) -> Result<CodeSuccessResponse> {
    let url = format!("{issuer}/api/accounts/deviceauth/token");
    let start = Instant::now();

    loop {
        let response = client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&TokenPollRequest {
                device_auth_id: device_code.device_auth_id.clone(),
                user_code: device_code.user_code.clone(),
            })
            .send()
            .await
            .context("Failed to poll device authorization status")?;

        let status = response.status();
        if status.is_success() {
            return response
                .json::<CodeSuccessResponse>()
                .await
                .context("Failed to parse device authorization response");
        }

        if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::NOT_FOUND {
            if start.elapsed() >= DEVICE_AUTH_TIMEOUT {
                anyhow::bail!("device auth timed out after 15 minutes");
            }

            let remaining = DEVICE_AUTH_TIMEOUT.saturating_sub(start.elapsed());
            let interval = Duration::from_secs(device_code.interval).min(remaining);
            sleep(interval).await;
            continue;
        }

        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("device auth failed with status {status}: {body}");
    }
}

async fn exchange_code_for_tokens(
    client: &reqwest::Client,
    issuer: &str,
    code: &CodeSuccessResponse,
) -> Result<TokenResponse> {
    let redirect_uri = format!("{issuer}/deviceauth/callback");
    let body = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
        urlencoding::encode(&code.authorization_code),
        urlencoding::encode(&redirect_uri),
        urlencoding::encode(CLIENT_ID),
        urlencoding::encode(&code.code_verifier),
    );

    let response = client
        .post(format!("{issuer}/oauth/token"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .context("Failed to exchange device authorization code for tokens")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("token endpoint returned status {status}: {body}");
    }

    response
        .json::<TokenResponse>()
        .await
        .context("Failed to parse token response")
}

fn deserialize_interval<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Interval {
        String(String),
        Number(u64),
    }

    match Option::<Interval>::deserialize(deserializer)? {
        Some(Interval::String(value)) => value
            .trim()
            .parse::<u64>()
            .map(normalize_poll_interval)
            .map_err(de::Error::custom),
        Some(Interval::Number(value)) => Ok(normalize_poll_interval(value)),
        None => Ok(0),
    }
}

fn normalize_poll_interval(seconds: u64) -> u64 {
    if seconds == 0 {
        DEFAULT_DEVICE_AUTH_POLL_INTERVAL_SECS
    } else {
        seconds
    }
}

fn print_device_code_prompt(device_code: &DeviceCode) {
    println!("ChatGPT device authorization");
    println!();
    println!("1. Open this link in your browser:");
    println!("   {}", device_code.verification_url);
    println!();
    println!("2. Enter this one-time code, expires in 15 minutes:");
    println!("   {}", device_code.user_code);
    println!();
    println!("Waiting for authorization...");
}

#[cfg(test)]
mod tests {
    use super::normalize_poll_interval;

    #[test]
    fn normalize_poll_interval_uses_default_for_zero() {
        assert_eq!(normalize_poll_interval(0), 5);
    }

    #[test]
    fn normalize_poll_interval_preserves_server_value() {
        assert_eq!(normalize_poll_interval(2), 2);
    }
}
