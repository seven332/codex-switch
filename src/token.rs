use anyhow::{Context, Result};
use base64::Engine;
use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::time::{Duration, sleep};

use crate::auth_json;
use crate::codex_http;
use crate::store;
use crate::types::{AuthData, StoredAccount, parse_chatgpt_id_token_claims};

const REFRESH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR: &str = "CODEX_REFRESH_TOKEN_URL_OVERRIDE";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const TOKEN_REFRESH_INTERVAL_DAYS: i64 = 8;

#[derive(Debug, Serialize)]
struct RefreshTokenRequest {
    client_id: &'static str,
    grant_type: &'static str,
    refresh_token: String,
}

#[derive(Debug, serde::Deserialize)]
struct RefreshTokenResponse {
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
}

pub async fn ensure_chatgpt_tokens_fresh(account: &StoredAccount) -> Result<StoredAccount> {
    match &account.auth_data {
        AuthData::ApiKey { .. } => Ok(account.clone()),
        AuthData::ChatGPT { access_token, .. } => {
            if auth_expired_or_needs_refresh(account, access_token) {
                refresh_chatgpt_tokens(account).await
            } else {
                Ok(account.clone())
            }
        }
    }
}

pub async fn refresh_chatgpt_tokens(account: &StoredAccount) -> Result<StoredAccount> {
    let (current_refresh_token, current_account_id) = match &account.auth_data {
        AuthData::ApiKey { .. } => return Ok(account.clone()),
        AuthData::ChatGPT {
            refresh_token,
            account_id,
            ..
        } => (refresh_token.clone(), account_id.clone()),
    };

    if current_refresh_token.trim().is_empty() {
        anyhow::bail!("Missing refresh token for account {}", account.name);
    }

    let refreshed = refresh_tokens_with_refresh_token(&current_refresh_token).await?;
    let claims = refreshed
        .id_token
        .as_deref()
        .map(parse_chatgpt_id_token_claims);
    let next_account_id = claims
        .as_ref()
        .and_then(|claims| claims.account_id.clone())
        .or(current_account_id);
    let token_last_refresh_at = Utc::now();

    let is_active = store::load_accounts()?.active_account_id.as_deref() == Some(&account.id);
    let updated = store::update_account_chatgpt_tokens(
        &account.id,
        store::ChatGptTokenUpdate {
            id_token: refreshed.id_token,
            access_token: refreshed.access_token,
            refresh_token: refreshed.refresh_token,
            chatgpt_account_id: next_account_id,
            email: claims.as_ref().and_then(|claims| claims.email.clone()),
            plan_type: claims.as_ref().and_then(|claims| claims.plan_type.clone()),
            chatgpt_user_id: claims.as_ref().and_then(|claims| claims.user_id.clone()),
            chatgpt_account_is_fedramp: claims.as_ref().map(|claims| claims.account_is_fedramp),
            token_last_refresh_at,
            subscription_expires_at: claims
                .as_ref()
                .and_then(|claims| claims.subscription_expires_at),
        },
    )?;

    if is_active {
        auth_json::write_account_auth(&updated)?;
    }

    Ok(updated)
}

fn auth_expired_or_needs_refresh(account: &StoredAccount, access_token: &str) -> bool {
    if let Some(expires_at) = parse_jwt_expiration(access_token) {
        return expires_at <= Utc::now();
    }

    match account.token_last_refresh_at {
        Some(last_refresh) => {
            last_refresh < Utc::now() - chrono::Duration::days(TOKEN_REFRESH_INTERVAL_DAYS)
        }
        None => false,
    }
}

fn parse_jwt_expiration(token: &str) -> Option<DateTime<Utc>> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }

    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .ok()?;
    let json: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    json.get("exp")
        .and_then(|value| value.as_i64())
        .and_then(|exp| DateTime::<Utc>::from_timestamp(exp, 0))
}

async fn refresh_tokens_with_refresh_token(refresh_token: &str) -> Result<RefreshTokenResponse> {
    let client = reqwest::Client::new();
    let endpoint = refresh_token_endpoint();
    let headers = codex_http::codex_default_headers()?;
    let body = RefreshTokenRequest {
        client_id: CLIENT_ID,
        grant_type: "refresh_token",
        refresh_token: refresh_token.to_string(),
    };

    let mut last_send_error = None;
    let mut response = None;

    for attempt in 1..=3u8 {
        match client
            .post(&endpoint)
            .headers(headers.clone())
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
        {
            Ok(resp) => {
                response = Some(resp);
                break;
            }
            Err(err) => {
                last_send_error = Some(err);
                if attempt < 3 {
                    sleep(Duration::from_millis(250 * u64::from(attempt))).await;
                }
            }
        }
    }

    let response = match response {
        Some(resp) => resp,
        None => {
            let err = last_send_error.context("Failed to send token refresh request")?;
            return Err(err.into());
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Token refresh failed: {status} - {body}");
    }

    response
        .json::<RefreshTokenResponse>()
        .await
        .context("Failed to parse token refresh response")
}

fn refresh_token_endpoint() -> String {
    refresh_token_endpoint_from_env(std::env::var(REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR))
}

fn refresh_token_endpoint_from_env(value: Result<String, std::env::VarError>) -> String {
    value
        .ok()
        .map(|endpoint| endpoint.trim().to_string())
        .filter(|endpoint| !endpoint.is_empty())
        .unwrap_or_else(|| REFRESH_TOKEN_URL.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        REFRESH_TOKEN_URL, auth_expired_or_needs_refresh, parse_jwt_expiration,
        refresh_token_endpoint_from_env,
    };
    use crate::types::{AuthData, AuthMode, StoredAccount};
    use base64::Engine;
    use chrono::{Duration, Utc};

    #[test]
    fn parse_jwt_expiration_reads_exp_claim() {
        let exp = Utc::now().timestamp() + 3600;
        let token = test_jwt_with_exp(exp);

        assert_eq!(
            parse_jwt_expiration(&token).map(|dt| dt.timestamp()),
            Some(exp)
        );
    }

    #[test]
    fn auth_refreshes_when_access_token_is_expired() {
        let token = test_jwt_with_exp((Utc::now() - Duration::minutes(1)).timestamp());
        let account = test_chatgpt_account(token.clone());

        assert!(auth_expired_or_needs_refresh(&account, &token));
    }

    #[test]
    fn auth_does_not_refresh_before_access_token_expiry() {
        let token = test_jwt_with_exp((Utc::now() + Duration::minutes(4)).timestamp());
        let account = test_chatgpt_account(token.clone());

        assert!(!auth_expired_or_needs_refresh(&account, &token));
    }

    #[test]
    fn refresh_token_endpoint_uses_override_when_present() {
        assert_eq!(
            refresh_token_endpoint_from_env(Ok(" https://example.com/oauth/token ".to_string())),
            "https://example.com/oauth/token"
        );
    }

    #[test]
    fn refresh_token_endpoint_uses_default_when_override_is_empty() {
        assert_eq!(
            refresh_token_endpoint_from_env(Ok(" ".to_string())),
            REFRESH_TOKEN_URL
        );
    }

    fn test_jwt_with_exp(exp: i64) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#);
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
        format!("{header}.{payload}.")
    }

    fn test_chatgpt_account(access_token: String) -> StoredAccount {
        StoredAccount {
            id: "account-id".to_string(),
            name: "test".to_string(),
            email: None,
            plan_type: None,
            chatgpt_user_id: None,
            chatgpt_account_is_fedramp: false,
            token_last_refresh_at: Some(Utc::now()),
            subscription_expires_at: None,
            auth_mode: AuthMode::ChatGPT,
            auth_data: AuthData::ChatGPT {
                id_token: "id-token".to_string(),
                access_token,
                refresh_token: "refresh-token".to_string(),
                account_id: None,
            },
            created_at: Utc::now(),
            last_used_at: None,
        }
    }
}
