use std::collections::HashMap;
use std::io;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Deserializer, Serialize, de};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{sleep, timeout};

use chrono::Utc;

use crate::codex_http;
use crate::redaction::redact_known_secrets;
use crate::types::{
    NewChatGptAccount, RedactedString, StoredAccount, try_parse_chatgpt_id_token_claims,
};

const DEFAULT_ISSUER: &str = "https://auth.openai.com";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const DEFAULT_BROWSER_CALLBACK_PORT: u16 = 1455;
const FALLBACK_BROWSER_CALLBACK_PORT: u16 = 1457;
const DEVICE_AUTH_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const CALLBACK_READ_TIMEOUT: Duration = Duration::from_secs(5);
const OAUTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_DEVICE_AUTH_POLL_INTERVAL_SECS: u64 = 5;
const OAUTH_SCOPE: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginFlow {
    Browser,
    DeviceAuth,
}

#[derive(Debug, Clone)]
struct PkceCodes {
    code_verifier: RedactedString,
    code_challenge: String,
}

#[derive(Debug, Clone)]
struct DeviceCode {
    verification_url: String,
    user_code: RedactedString,
    device_auth_id: RedactedString,
    interval: u64,
}

#[derive(Debug, Deserialize)]
struct UserCodeResponse {
    device_auth_id: RedactedString,
    #[serde(alias = "user_code", alias = "usercode")]
    user_code: RedactedString,
    #[serde(default, deserialize_with = "deserialize_interval")]
    interval: u64,
}

#[derive(Debug, Serialize)]
struct UserCodeRequest {
    client_id: String,
}

#[derive(Debug, Serialize)]
struct TokenPollRequest {
    device_auth_id: RedactedString,
    user_code: RedactedString,
}

#[derive(Debug, Deserialize)]
struct CodeSuccessResponse {
    authorization_code: RedactedString,
    code_verifier: RedactedString,
    #[allow(dead_code)]
    code_challenge: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    id_token: RedactedString,
    access_token: RedactedString,
    refresh_token: RedactedString,
}

pub async fn login(account_name: String, flow: LoginFlow) -> Result<StoredAccount> {
    let tokens = match flow {
        LoginFlow::Browser => browser_login().await?,
        LoginFlow::DeviceAuth => device_auth_login().await?,
    };
    stored_account_from_tokens(account_name, tokens)
}

async fn browser_login() -> Result<TokenResponse> {
    let client = oauth_http_client()?;
    let issuer = DEFAULT_ISSUER.trim_end_matches('/');
    let pkce = generate_pkce();
    let state = generate_state();
    let listener = bind_browser_callback_listener().await?;
    let port = listener
        .local_addr()
        .context("Failed to determine browser callback port")?
        .port();
    let redirect_uri = format!("http://localhost:{port}/auth/callback");
    let auth_url = build_authorize_url(issuer, &redirect_uri, &pkce, &state);

    print_browser_login_prompt(port, &auth_url);
    if let Err(err) = webbrowser::open(&auth_url) {
        println!("Could not open a browser automatically: {err}");
    }

    wait_for_browser_tokens(listener, &client, issuer, &redirect_uri, &pkce, &state).await
}

async fn device_auth_login() -> Result<TokenResponse> {
    let client = oauth_http_client()?;
    let issuer = DEFAULT_ISSUER.trim_end_matches('/');
    let device_code = request_device_code(&client, issuer).await?;

    print_device_code_prompt(&device_code);

    let code = poll_for_authorization_code(&client, issuer, &device_code).await?;
    let redirect_uri = format!("{issuer}/deviceauth/callback");
    exchange_authorization_code_for_tokens(
        &client,
        issuer,
        code.authorization_code.expose_secret(),
        &redirect_uri,
        code.code_verifier.expose_secret(),
    )
    .await
}

fn oauth_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(OAUTH_REQUEST_TIMEOUT)
        .build()
        .context("Failed to build OAuth HTTP client")
}

fn stored_account_from_tokens(
    account_name: String,
    tokens: TokenResponse,
) -> Result<StoredAccount> {
    let claims = try_parse_chatgpt_id_token_claims(tokens.id_token.expose_secret())
        .context("OAuth token response contains an invalid id_token")?;

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

        let body = redact_known_secrets(response.text().await.unwrap_or_default(), &[]);
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

        let body = redact_known_secrets(
            response.text().await.unwrap_or_default(),
            &[
                device_code.device_auth_id.expose_secret(),
                device_code.user_code.expose_secret(),
            ],
        );
        anyhow::bail!("device auth failed with status {status}: {body}");
    }
}

async fn exchange_authorization_code_for_tokens(
    client: &reqwest::Client,
    issuer: &str,
    authorization_code: &str,
    redirect_uri: &str,
    code_verifier: &str,
) -> Result<TokenResponse> {
    let body = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
        urlencoding::encode(authorization_code),
        urlencoding::encode(redirect_uri),
        urlencoding::encode(CLIENT_ID),
        urlencoding::encode(code_verifier),
    );

    let response = client
        .post(format!("{issuer}/oauth/token"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .context("Failed to exchange OAuth authorization code for tokens")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = redact_known_secrets(
            response.text().await.unwrap_or_default(),
            &[authorization_code, code_verifier],
        );
        anyhow::bail!("token endpoint returned status {status}: {body}");
    }

    response
        .json::<TokenResponse>()
        .await
        .context("Failed to parse token response")
}

fn build_authorize_url(issuer: &str, redirect_uri: &str, pkce: &PkceCodes, state: &str) -> String {
    let originator = codex_http::originator_value();
    let query = [
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", redirect_uri),
        ("scope", OAUTH_SCOPE),
        ("code_challenge", pkce.code_challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("state", state),
        ("originator", originator.as_str()),
    ]
    .into_iter()
    .map(|(key, value)| format!("{key}={}", urlencoding::encode(value)))
    .collect::<Vec<_>>()
    .join("&");

    format!("{issuer}/oauth/authorize?{query}")
}

async fn bind_browser_callback_listener() -> Result<TcpListener> {
    match TcpListener::bind(("127.0.0.1", DEFAULT_BROWSER_CALLBACK_PORT)).await {
        Ok(listener) => Ok(listener),
        Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
            println!(
                "Port {DEFAULT_BROWSER_CALLBACK_PORT} is in use; using callback port {FALLBACK_BROWSER_CALLBACK_PORT}."
            );
            TcpListener::bind(("127.0.0.1", FALLBACK_BROWSER_CALLBACK_PORT))
                .await
                .with_context(|| {
                    format!("Failed to bind fallback browser callback port {FALLBACK_BROWSER_CALLBACK_PORT}")
                })
        }
        Err(err) => Err(err).context("Failed to bind browser callback server"),
    }
}

async fn wait_for_browser_tokens(
    listener: TcpListener,
    client: &reqwest::Client,
    issuer: &str,
    redirect_uri: &str,
    pkce: &PkceCodes,
    expected_state: &str,
) -> Result<TokenResponse> {
    let start = Instant::now();

    loop {
        if start.elapsed() >= DEVICE_AUTH_TIMEOUT {
            anyhow::bail!("browser auth timed out after 15 minutes");
        }

        let remaining = DEVICE_AUTH_TIMEOUT.saturating_sub(start.elapsed());
        let (stream, _) = match timeout(remaining, listener.accept()).await {
            Ok(Ok(accepted)) => accepted,
            Ok(Err(err)) => return Err(err).context("Failed to accept browser callback"),
            Err(_) => anyhow::bail!("browser auth timed out after 15 minutes"),
        };

        if let Some(tokens) =
            handle_browser_callback(stream, client, issuer, redirect_uri, pkce, expected_state)
                .await?
        {
            return Ok(tokens);
        }
    }
}

async fn handle_browser_callback(
    mut stream: TcpStream,
    client: &reqwest::Client,
    issuer: &str,
    redirect_uri: &str,
    pkce: &PkceCodes,
    expected_state: &str,
) -> Result<Option<TokenResponse>> {
    let request = read_http_request(&mut stream).await?;
    let Some((method, target)) = parse_request_line(&request) else {
        send_text_response(&mut stream, 400, "Bad Request", "Bad Request").await?;
        anyhow::bail!("Invalid browser callback request");
    };

    if method != "GET" {
        send_text_response(&mut stream, 405, "Method Not Allowed", "Method Not Allowed").await?;
        return Ok(None);
    }

    let (path, query) = split_target(&target);
    if path != "/auth/callback" {
        send_text_response(&mut stream, 404, "Not Found", "Not Found").await?;
        return Ok(None);
    }

    let params = parse_query(query.unwrap_or_default());
    if params.get("state").map(String::as_str) != Some(expected_state) {
        send_error_page(&mut stream, "OAuth state mismatch. Sign-in was rejected.").await?;
        anyhow::bail!("OAuth state mismatch");
    }

    if let Some(error_code) = params.get("error") {
        let message = oauth_callback_error_message(
            error_code,
            params.get("error_description").map(String::as_str),
        );
        send_error_page(&mut stream, &message).await?;
        anyhow::bail!(message);
    }

    let Some(code) = params.get("code").filter(|code| !code.trim().is_empty()) else {
        send_error_page(
            &mut stream,
            "OAuth callback did not include an authorization code.",
        )
        .await?;
        anyhow::bail!("OAuth callback did not include an authorization code");
    };

    let tokens = match exchange_authorization_code_for_tokens(
        client,
        issuer,
        code,
        redirect_uri,
        pkce.code_verifier.expose_secret(),
    )
    .await
    {
        Ok(tokens) => tokens,
        Err(err) => {
            let message = format!("Token exchange failed: {err}");
            send_error_page(&mut stream, &message).await?;
            return Err(err);
        }
    };

    send_success_page(&mut stream).await?;
    Ok(Some(tokens))
}

async fn read_http_request(stream: &mut TcpStream) -> Result<String> {
    let mut request = Vec::with_capacity(2048);
    loop {
        let mut chunk = [0u8; 1024];
        let size = match timeout(CALLBACK_READ_TIMEOUT, stream.read(&mut chunk)).await {
            Ok(Ok(size)) => size,
            Ok(Err(err)) => return Err(err).context("Failed to read browser callback request"),
            Err(_) => anyhow::bail!("Timed out reading browser callback request"),
        };

        if size == 0 {
            break;
        }

        request.extend_from_slice(&chunk[..size]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if request.len() >= 16 * 1024 {
            anyhow::bail!("Browser callback request was too large");
        }
    }

    if request.is_empty() {
        anyhow::bail!("Browser callback request was empty");
    }

    Ok(String::from_utf8_lossy(&request).into_owned())
}

fn parse_request_line(request: &str) -> Option<(String, String)> {
    let line = request.lines().next()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?;
    let target = parts.next()?;
    Some((method.to_string(), target.to_string()))
}

fn split_target(target: &str) -> (&str, Option<&str>) {
    match target.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (target, None),
    }
}

fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            if key.is_empty() {
                return None;
            }
            Some((decode_query_component(key), decode_query_component(value)))
        })
        .collect()
}

fn decode_query_component(value: &str) -> String {
    urlencoding::decode(value)
        .map(|value| value.into_owned())
        .unwrap_or_else(|_| value.to_string())
}

async fn send_success_page(stream: &mut TcpStream) -> Result<()> {
    send_html_response(
        stream,
        200,
        "OK",
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>codex-switch login</title></head><body><h1>Login complete</h1><p>You can close this window and return to codex-switch.</p></body></html>",
    )
    .await
}

async fn send_error_page(stream: &mut TcpStream, message: &str) -> Result<()> {
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>codex-switch login</title></head><body><h1>Login failed</h1><p>{}</p></body></html>",
        escape_html(message)
    );
    send_html_response(stream, 400, "Bad Request", &body).await
}

async fn send_text_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: &str,
) -> Result<()> {
    send_response(stream, status, reason, "text/plain; charset=utf-8", body).await
}

async fn send_html_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: &str,
) -> Result<()> {
    send_response(stream, status, reason, "text/html; charset=utf-8", body).await
}

async fn send_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &str,
) -> Result<()> {
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .context("Failed to write browser callback response")
}

fn escape_html(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn oauth_callback_error_message(error_code: &str, error_description: Option<&str>) -> String {
    match error_description.filter(|value| !value.trim().is_empty()) {
        Some(description) => format!("{error_code}: {description}"),
        None => error_code.to_string(),
    }
}

fn generate_pkce() -> PkceCodes {
    let mut bytes = [0u8; 64];
    rand::rng().fill_bytes(&mut bytes);
    let code_verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let digest = Sha256::digest(code_verifier.as_bytes());
    let code_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);

    PkceCodes {
        code_verifier: RedactedString::new(code_verifier),
        code_challenge,
    }
}

fn generate_state() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
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
    println!("   {}", device_code.user_code.expose_secret());
    println!();
    println!("Waiting for authorization...");
}

fn print_browser_login_prompt(port: u16, auth_url: &str) {
    println!("Starting local login server on http://localhost:{port}.");
    println!("If your browser did not open, open this URL:");
    println!();
    println!("{auth_url}");
    println!();
    println!("Waiting for authorization...");
}

#[cfg(test)]
mod tests {
    use super::{build_authorize_url, normalize_poll_interval, parse_query};
    use base64::Engine;

    #[test]
    fn normalize_poll_interval_uses_default_for_zero() {
        assert_eq!(normalize_poll_interval(0), 5);
    }

    #[test]
    fn normalize_poll_interval_preserves_server_value() {
        assert_eq!(normalize_poll_interval(2), 2);
    }

    #[test]
    fn authorize_url_uses_codex_oauth_parameters() {
        let pkce = super::PkceCodes {
            code_verifier: "verifier".into(),
            code_challenge: "challenge".to_string(),
        };
        let url = build_authorize_url(
            "https://auth.openai.com",
            "http://localhost:1455/auth/callback",
            &pkce,
            "state",
        );
        let query = url.split_once('?').expect("query").1;
        let params = parse_query(query);

        assert_eq!(
            params.get("response_type").map(String::as_str),
            Some("code")
        );
        assert_eq!(
            params.get("client_id").map(String::as_str),
            Some(super::CLIENT_ID)
        );
        assert_eq!(
            params.get("redirect_uri").map(String::as_str),
            Some("http://localhost:1455/auth/callback")
        );
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(
            params.get("originator").map(String::as_str),
            Some(crate::codex_http::originator_value().as_str())
        );
        assert_eq!(params.get("state").map(String::as_str), Some("state"));
    }

    #[test]
    fn oauth_debug_output_redacts_secret_values() {
        let device_auth_id = "device-auth-id-codex-switch-test-secret";
        let user_code = "user-code-codex-switch-test-secret";
        let pkce_verifier = "pkce-verifier-codex-switch-test-secret";
        let authorization_code = "authorization-code-codex-switch-test-secret";
        let code_verifier = "code-verifier-codex-switch-test-secret";
        let id_token = "id-token-codex-switch-test-secret";
        let access_token = "access-token-codex-switch-test-secret";
        let refresh_token = "refresh-token-codex-switch-test-secret";
        let device_code = super::DeviceCode {
            verification_url: "https://auth.openai.com/codex/device".to_string(),
            user_code: user_code.into(),
            device_auth_id: device_auth_id.into(),
            interval: 5,
        };
        let user_code_response = super::UserCodeResponse {
            device_auth_id: device_auth_id.into(),
            user_code: user_code.into(),
            interval: 5,
        };
        let token_poll_request = super::TokenPollRequest {
            device_auth_id: device_auth_id.into(),
            user_code: user_code.into(),
        };
        let pkce = super::PkceCodes {
            code_verifier: pkce_verifier.into(),
            code_challenge: "challenge".to_string(),
        };
        let code = super::CodeSuccessResponse {
            authorization_code: authorization_code.into(),
            code_verifier: code_verifier.into(),
            code_challenge: "challenge".to_string(),
        };
        let tokens = super::TokenResponse {
            id_token: id_token.into(),
            access_token: access_token.into(),
            refresh_token: refresh_token.into(),
        };
        let debug = format!(
            "{device_code:?} {user_code_response:?} {token_poll_request:?} {pkce:?} {code:?} {tokens:?}"
        );

        for secret in [
            device_auth_id,
            user_code,
            pkce_verifier,
            authorization_code,
            code_verifier,
            id_token,
            access_token,
            refresh_token,
        ] {
            assert!(!debug.contains(secret), "debug output leaked {secret}");
        }
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn oauth_json_preserves_secret_values() {
        let device_auth_id = "device-auth-id-json-compat-secret";
        let user_code = "user-code-json-compat-secret";
        let request = super::TokenPollRequest {
            device_auth_id: device_auth_id.into(),
            user_code: user_code.into(),
        };

        let json = serde_json::to_string(&request).expect("serialize token poll request");

        assert!(json.contains(device_auth_id));
        assert!(json.contains(user_code));

        let response: super::UserCodeResponse = serde_json::from_str(&format!(
            r#"{{"device_auth_id":"{device_auth_id}","user_code":"{user_code}","interval":5}}"#
        ))
        .expect("deserialize user code response");
        assert_eq!(response.device_auth_id.expose_secret(), device_auth_id);
        assert_eq!(response.user_code.expose_secret(), user_code);
    }

    #[test]
    fn stored_account_from_tokens_rejects_invalid_id_token() {
        let err = super::stored_account_from_tokens(
            "oauth".to_string(),
            super::TokenResponse {
                id_token: "not-a-jwt".into(),
                access_token: "access-token".into(),
                refresh_token: "refresh-token".into(),
            },
        )
        .expect_err("invalid id_token should be rejected");

        assert!(format!("{err:#}").contains("invalid ID token"), "{err:#}");
    }

    #[test]
    fn stored_account_from_tokens_reads_claims_from_id_token() {
        let account = super::stored_account_from_tokens(
            "oauth".to_string(),
            super::TokenResponse {
                id_token: test_id_token().into(),
                access_token: "access-token".into(),
                refresh_token: "refresh-token".into(),
            },
        )
        .expect("valid id_token should create account");

        assert_eq!(account.email.as_deref(), Some("user@example.com"));
        assert_eq!(account.plan_type.as_deref(), Some("pro"));
        assert_eq!(account.chatgpt_user_id.as_deref(), Some("user-id"));
    }

    fn test_id_token() -> String {
        let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let payload = serde_json::json!({
            "email": "user@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "account-id",
                "chatgpt_plan_type": "pro",
                "chatgpt_user_id": "user-id",
                "chatgpt_account_is_fedramp": false
            }
        });
        let payload_json = serde_json::to_vec(&payload).expect("test payload should serialize");

        format!(
            "{}.{}.{}",
            encode(br#"{"alg":"none","typ":"JWT"}"#),
            encode(&payload_json),
            encode(b"sig")
        )
    }
}
