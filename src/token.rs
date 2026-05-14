use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine;
use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::Mutex;
use tokio::time::{Duration, sleep};
use uuid::Uuid;

use crate::auth_json;
use crate::codex_http;
use crate::redaction::redact_known_secrets;
use crate::store;
use crate::types::{AuthData, RedactedString, StoredAccount, parse_chatgpt_id_token_claims};

const REFRESH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR: &str = "CODEX_REFRESH_TOKEN_URL_OVERRIDE";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const TOKEN_REFRESH_INTERVAL_DAYS: i64 = 8;
const ACCESS_TOKEN_REFRESH_SKEW: chrono::Duration = chrono::Duration::minutes(5);
const TOKEN_REFRESH_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const TOKEN_REFRESH_LOCK_RETRY_DELAY: Duration = Duration::from_millis(250);
const TOKEN_REFRESH_LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(2 * 60);

static TOKEN_REFRESH_LOCK: Mutex<()> = Mutex::const_new(());

#[derive(Debug, Serialize)]
struct RefreshTokenRequest {
    client_id: &'static str,
    grant_type: &'static str,
    refresh_token: RedactedString,
}

#[derive(Debug, serde::Deserialize)]
struct RefreshTokenResponse {
    #[serde(default)]
    id_token: Option<RedactedString>,
    #[serde(default)]
    access_token: Option<RedactedString>,
    #[serde(default)]
    refresh_token: Option<RedactedString>,
}

struct TokenRefreshFileLock {
    path: PathBuf,
    lock_id: String,
}

impl Drop for TokenRefreshFileLock {
    fn drop(&mut self) {
        release_token_refresh_file_lock(&self.path, &self.lock_id);
    }
}

pub async fn ensure_chatgpt_tokens_fresh(account: &StoredAccount) -> Result<StoredAccount> {
    match &account.auth_data {
        AuthData::ApiKey { .. } => Ok(account.clone()),
        AuthData::ChatGPT { access_token, .. } => {
            if auth_expired_or_needs_refresh(account, access_token.expose_secret()) {
                refresh_chatgpt_tokens(account).await
            } else {
                Ok(account.clone())
            }
        }
    }
}

pub async fn refresh_chatgpt_tokens(account: &StoredAccount) -> Result<StoredAccount> {
    if matches!(account.auth_data, AuthData::ApiKey { .. }) {
        return Ok(account.clone());
    }

    let _guard = TOKEN_REFRESH_LOCK.lock().await;
    let _file_lock = acquire_token_refresh_file_lock().await?;
    let latest_account = latest_account_for_refresh(account)?;

    if chatgpt_auth_changed(account, &latest_account)
        && !chatgpt_account_needs_refresh(&latest_account)
    {
        return Ok(latest_account);
    }

    refresh_chatgpt_tokens_locked(&latest_account).await
}

async fn refresh_chatgpt_tokens_locked(account: &StoredAccount) -> Result<StoredAccount> {
    let (current_refresh_token, current_account_id) = match &account.auth_data {
        AuthData::ApiKey { .. } => return Ok(account.clone()),
        AuthData::ChatGPT {
            refresh_token,
            account_id,
            ..
        } => (refresh_token.clone(), account_id.clone()),
    };

    if current_refresh_token.expose_secret().trim().is_empty() {
        anyhow::bail!("Missing refresh token for account {}", account.name);
    }

    let refreshed =
        refresh_tokens_with_refresh_token(current_refresh_token.expose_secret()).await?;
    let claims = refreshed
        .id_token
        .as_ref()
        .map(|id_token| parse_chatgpt_id_token_claims(id_token.expose_secret()));
    let next_account_id = claims
        .as_ref()
        .and_then(|claims| claims.account_id.clone())
        .or(current_account_id);
    let token_last_refresh_at = Utc::now();

    let accounts_store = store::load_accounts()?;
    let is_current = auth_json::current_stored_account_best_effort(&accounts_store)
        .is_some_and(|current_account| current_account.id == account.id);
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

    if is_current {
        auth_json::write_account_auth(&updated)?;
    }

    Ok(updated)
}

fn latest_account_for_refresh(account: &StoredAccount) -> Result<StoredAccount> {
    let store = store::load_accounts()?;
    Ok(store
        .accounts
        .into_iter()
        .find(|stored| stored.id == account.id)
        .unwrap_or_else(|| account.clone()))
}

fn chatgpt_auth_changed(left: &StoredAccount, right: &StoredAccount) -> bool {
    match (&left.auth_data, &right.auth_data) {
        (
            AuthData::ChatGPT {
                id_token: left_id_token,
                access_token: left_access_token,
                refresh_token: left_refresh_token,
                account_id: left_account_id,
            },
            AuthData::ChatGPT {
                id_token: right_id_token,
                access_token: right_access_token,
                refresh_token: right_refresh_token,
                account_id: right_account_id,
            },
        ) => {
            left_id_token != right_id_token
                || left_access_token != right_access_token
                || left_refresh_token != right_refresh_token
                || left_account_id != right_account_id
                || left.token_last_refresh_at != right.token_last_refresh_at
        }
        _ => false,
    }
}

fn chatgpt_account_needs_refresh(account: &StoredAccount) -> bool {
    match &account.auth_data {
        AuthData::ApiKey { .. } => false,
        AuthData::ChatGPT { access_token, .. } => {
            auth_expired_or_needs_refresh(account, access_token.expose_secret())
        }
    }
}

async fn acquire_token_refresh_file_lock() -> Result<TokenRefreshFileLock> {
    let path = token_refresh_lock_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }

    let lock_id = Uuid::new_v4().to_string();
    let deadline = tokio::time::Instant::now() + TOKEN_REFRESH_LOCK_WAIT_TIMEOUT;
    loop {
        match create_token_refresh_lock_file(&path, &lock_id) {
            Ok(()) => {
                return Ok(TokenRefreshFileLock { path, lock_id });
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                if tokio::time::Instant::now() >= deadline {
                    anyhow::bail!(
                        "Timed out waiting for token refresh lock: {}. Another codex-switch process may be refreshing tokens, or a stale lock file may need to be removed manually.",
                        path.display()
                    );
                }
                sleep(TOKEN_REFRESH_LOCK_RETRY_DELAY).await;
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("Failed to create token refresh lock: {}", path.display())
                });
            }
        }
    }
}

fn token_refresh_lock_path() -> Result<PathBuf> {
    Ok(store::config_dir()?
        .join("runtime")
        .join("token-refresh.lock"))
}

fn create_token_refresh_lock_file(path: &Path, lock_id: &str) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        write_token_refresh_lock_metadata(path, lock_id, &mut file)
    }

    #[cfg(not(unix))]
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        write_token_refresh_lock_metadata(path, lock_id, &mut file)
    }
}

fn write_token_refresh_lock_metadata(
    path: &Path,
    lock_id: &str,
    file: &mut fs::File,
) -> io::Result<()> {
    let result = (|| {
        writeln!(
            file,
            "lock_id={}\npid={}\ncreated_at={}",
            lock_id,
            std::process::id(),
            Utc::now().to_rfc3339()
        )?;
        file.sync_all()
    })();

    if result.is_err() {
        let _ = fs::remove_file(path);
    }

    result
}

fn release_token_refresh_file_lock(path: &Path, lock_id: &str) {
    if fs::read_to_string(path)
        .ok()
        .is_some_and(|content| token_refresh_lock_belongs_to_owner(&content, lock_id))
    {
        let _ = fs::remove_file(path);
    }
}

fn token_refresh_lock_belongs_to_owner(content: &str, lock_id: &str) -> bool {
    content
        .lines()
        .any(|line| line.strip_prefix("lock_id=") == Some(lock_id))
}

fn auth_expired_or_needs_refresh(account: &StoredAccount, access_token: &str) -> bool {
    if let Some(expires_at) = parse_jwt_expiration(access_token) {
        return expires_at <= Utc::now() + ACCESS_TOKEN_REFRESH_SKEW;
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
    let client = reqwest::Client::builder()
        .timeout(TOKEN_REFRESH_REQUEST_TIMEOUT)
        .build()
        .context("Failed to build token refresh HTTP client")?;
    let endpoint = refresh_token_endpoint();
    let headers = codex_http::codex_default_headers()?;
    let body = RefreshTokenRequest {
        client_id: CLIENT_ID,
        grant_type: "refresh_token",
        refresh_token: RedactedString::new(refresh_token),
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
        let body =
            redact_known_secrets(response.text().await.unwrap_or_default(), &[refresh_token]);
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
    use std::path::PathBuf;

    use super::{
        REFRESH_TOKEN_URL, TokenRefreshFileLock, auth_expired_or_needs_refresh,
        create_token_refresh_lock_file, parse_jwt_expiration, refresh_token_endpoint_from_env,
        token_refresh_lock_belongs_to_owner,
    };
    use crate::types::{AuthData, AuthMode, StoredAccount};
    use base64::Engine;
    use chrono::{Duration, Utc};
    use uuid::Uuid;

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
        let token = test_jwt_with_exp((Utc::now() + Duration::minutes(10)).timestamp());
        let account = test_chatgpt_account(token.clone());

        assert!(!auth_expired_or_needs_refresh(&account, &token));
    }

    #[test]
    fn auth_refreshes_when_access_token_is_near_expiry() {
        let token = test_jwt_with_exp((Utc::now() + Duration::minutes(4)).timestamp());
        let account = test_chatgpt_account(token.clone());

        assert!(auth_expired_or_needs_refresh(&account, &token));
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

    #[test]
    fn stale_guard_does_not_remove_replaced_token_refresh_lock() {
        let (dir, lock_path) = temp_lock_path("replaced-token-refresh-lock");
        let lock_a_id = Uuid::new_v4().to_string();
        create_token_refresh_lock_file(&lock_path, &lock_a_id)
            .expect("first lock should be created");
        let lock_a = TokenRefreshFileLock {
            path: lock_path.clone(),
            lock_id: lock_a_id,
        };
        std::fs::remove_file(&lock_path).expect("test should remove first lock file");

        let lock_b_id = Uuid::new_v4().to_string();
        create_token_refresh_lock_file(&lock_path, &lock_b_id)
            .expect("second lock should be created");
        let lock_b = TokenRefreshFileLock {
            path: lock_path.clone(),
            lock_id: lock_b_id.clone(),
        };
        drop(lock_a);

        let lock_content = std::fs::read_to_string(&lock_path).expect("second lock should remain");
        assert!(token_refresh_lock_belongs_to_owner(
            &lock_content,
            &lock_b_id
        ));

        drop(lock_b);
        assert!(!lock_path.exists());

        std::fs::remove_dir_all(dir).expect("temp lock dir should be removed");
    }

    #[test]
    fn refresh_debug_output_redacts_secret_values() {
        let request_refresh_token = "request-refresh-token-codex-switch-test-secret";
        let id_token = "response-id-token-codex-switch-test-secret";
        let access_token = "response-access-token-codex-switch-test-secret";
        let response_refresh_token = "response-refresh-token-codex-switch-test-secret";
        let request = super::RefreshTokenRequest {
            client_id: super::CLIENT_ID,
            grant_type: "refresh_token",
            refresh_token: request_refresh_token.into(),
        };
        let response = super::RefreshTokenResponse {
            id_token: Some(id_token.into()),
            access_token: Some(access_token.into()),
            refresh_token: Some(response_refresh_token.into()),
        };
        let debug = format!("{request:?} {response:?}");

        for secret in [
            request_refresh_token,
            id_token,
            access_token,
            response_refresh_token,
        ] {
            assert!(!debug.contains(secret), "debug output leaked {secret}");
        }
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn refresh_json_preserves_secret_values() {
        let request_refresh_token = "request-refresh-token-json-compat-secret";
        let id_token = "response-id-token-json-compat-secret";
        let access_token = "response-access-token-json-compat-secret";
        let response_refresh_token = "response-refresh-token-json-compat-secret";
        let request = super::RefreshTokenRequest {
            client_id: super::CLIENT_ID,
            grant_type: "refresh_token",
            refresh_token: request_refresh_token.into(),
        };

        let request_json =
            serde_json::to_string(&request).expect("serialize refresh token request");
        assert!(request_json.contains(request_refresh_token));

        let response: super::RefreshTokenResponse = serde_json::from_str(&format!(
            r#"{{"id_token":"{id_token}","access_token":"{access_token}","refresh_token":"{response_refresh_token}"}}"#
        ))
        .expect("deserialize refresh token response");
        assert_eq!(
            response
                .id_token
                .as_ref()
                .map(|value| value.expose_secret()),
            Some(id_token)
        );
        assert_eq!(
            response
                .access_token
                .as_ref()
                .map(|value| value.expose_secret()),
            Some(access_token)
        );
        assert_eq!(
            response
                .refresh_token
                .as_ref()
                .map(|value| value.expose_secret()),
            Some(response_refresh_token)
        );
    }

    fn test_jwt_with_exp(exp: i64) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#);
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
        format!("{header}.{payload}.")
    }

    fn temp_lock_path(name: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("codex-switch-{name}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp lock dir should be created");
        let lock_path = dir.join("token-refresh.lock");
        (dir, lock_path)
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
                id_token: "id-token".into(),
                access_token: access_token.into(),
                refresh_token: "refresh-token".into(),
                account_id: None,
            },
            created_at: Utc::now(),
            last_used_at: None,
        }
    }
}
