use std::fmt;

use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountsStore {
    pub version: u32,
    pub accounts: Vec<StoredAccount>,
    pub active_account_id: Option<String>,
    #[serde(default)]
    pub masked_account_ids: Vec<String>,
}

impl Default for AccountsStore {
    fn default() -> Self {
        Self {
            version: 1,
            accounts: Vec::new(),
            active_account_id: None,
            masked_account_ids: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredAccount {
    pub id: String,
    pub name: String,
    pub email: Option<String>,
    pub plan_type: Option<String>,
    pub chatgpt_user_id: Option<String>,
    pub chatgpt_account_is_fedramp: bool,
    pub token_last_refresh_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub subscription_expires_at: Option<DateTime<Utc>>,
    pub auth_mode: AuthMode,
    pub auth_data: AuthData,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

impl StoredAccount {
    pub fn new_api_key(name: String, api_key: String) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name,
            email: None,
            plan_type: None,
            chatgpt_user_id: None,
            chatgpt_account_is_fedramp: false,
            token_last_refresh_at: None,
            subscription_expires_at: None,
            auth_mode: AuthMode::ApiKey,
            auth_data: AuthData::ApiKey { key: api_key },
            created_at: Utc::now(),
            last_used_at: None,
        }
    }

    pub fn new_chatgpt(account: NewChatGptAccount) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: account.name,
            email: account.email,
            plan_type: account.plan_type,
            chatgpt_user_id: account.chatgpt_user_id,
            chatgpt_account_is_fedramp: account.chatgpt_account_is_fedramp,
            token_last_refresh_at: Some(account.token_last_refresh_at),
            subscription_expires_at: account.subscription_expires_at,
            auth_mode: AuthMode::ChatGPT,
            auth_data: AuthData::ChatGPT {
                id_token: account.id_token,
                access_token: account.access_token,
                refresh_token: account.refresh_token,
                account_id: account.account_id,
            },
            created_at: Utc::now(),
            last_used_at: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewChatGptAccount {
    pub name: String,
    pub email: Option<String>,
    pub plan_type: Option<String>,
    pub chatgpt_user_id: Option<String>,
    pub chatgpt_account_is_fedramp: bool,
    pub token_last_refresh_at: DateTime<Utc>,
    pub subscription_expires_at: Option<DateTime<Utc>>,
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    ApiKey,
    ChatGPT,
}

impl fmt::Display for AuthMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey => f.write_str("api_key"),
            Self::ChatGPT => f.write_str("chatgpt"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthData {
    ApiKey {
        key: String,
    },
    ChatGPT {
        id_token: String,
        access_token: String,
        refresh_token: String,
        account_id: Option<String>,
    },
}

#[derive(Debug, Clone, Default)]
pub struct ChatGptIdTokenClaims {
    pub email: Option<String>,
    pub plan_type: Option<String>,
    pub user_id: Option<String>,
    pub account_id: Option<String>,
    pub account_is_fedramp: bool,
    pub subscription_expires_at: Option<DateTime<Utc>>,
}

pub fn parse_chatgpt_id_token_claims(id_token: &str) -> ChatGptIdTokenClaims {
    let parts: Vec<&str> = id_token.split('.').collect();
    if parts.len() != 3 {
        return ChatGptIdTokenClaims::default();
    }

    let payload = match base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[1]) {
        Ok(bytes) => bytes,
        Err(_) => return ChatGptIdTokenClaims::default(),
    };

    let json: serde_json::Value = match serde_json::from_slice(&payload) {
        Ok(value) => value,
        Err(_) => return ChatGptIdTokenClaims::default(),
    };

    let profile_claims = json.get("https://api.openai.com/profile");
    let auth_claims = json.get("https://api.openai.com/auth");

    ChatGptIdTokenClaims {
        email: json
            .get("email")
            .and_then(|v| v.as_str())
            .or_else(|| {
                profile_claims
                    .and_then(|profile| profile.get("email"))
                    .and_then(|v| v.as_str())
            })
            .map(String::from),
        plan_type: auth_claims
            .and_then(|auth| auth.get("chatgpt_plan_type"))
            .and_then(|v| v.as_str())
            .map(String::from),
        user_id: auth_claims
            .and_then(|auth| auth.get("chatgpt_user_id").or_else(|| auth.get("user_id")))
            .and_then(|v| v.as_str())
            .map(String::from),
        account_id: auth_claims
            .and_then(|auth| auth.get("chatgpt_account_id"))
            .and_then(|v| v.as_str())
            .map(String::from),
        account_is_fedramp: auth_claims
            .and_then(|auth| auth.get("chatgpt_account_is_fedramp"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        subscription_expires_at: auth_claims
            .and_then(|auth| auth.get("chatgpt_subscription_active_until"))
            .and_then(|v| v.as_str())
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc)),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthDotJson {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<String>,
    #[serde(rename = "OPENAI_API_KEY")]
    pub openai_api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TokenData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenData {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageInfo {
    pub account_id: String,
    pub limit_id: Option<String>,
    pub limit_name: Option<String>,
    pub plan_type: Option<String>,
    pub primary_used_percent: Option<f64>,
    pub primary_window_minutes: Option<i64>,
    pub primary_resets_at: Option<i64>,
    pub secondary_used_percent: Option<f64>,
    pub secondary_window_minutes: Option<i64>,
    pub secondary_resets_at: Option<i64>,
    pub has_credits: Option<bool>,
    pub unlimited_credits: Option<bool>,
    pub credits_balance: Option<String>,
    pub rate_limit_reached_type: Option<String>,
    #[serde(default)]
    pub additional_limits: Vec<UsageLimitInfo>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageLimitInfo {
    pub limit_id: Option<String>,
    pub limit_name: Option<String>,
    pub primary_used_percent: Option<f64>,
    pub primary_window_minutes: Option<i64>,
    pub primary_resets_at: Option<i64>,
    pub secondary_used_percent: Option<f64>,
    pub secondary_window_minutes: Option<i64>,
    pub secondary_resets_at: Option<i64>,
}

impl UsageInfo {
    pub fn error(account_id: String, error: String) -> Self {
        Self {
            account_id,
            limit_id: None,
            limit_name: None,
            plan_type: None,
            primary_used_percent: None,
            primary_window_minutes: None,
            primary_resets_at: None,
            secondary_used_percent: None,
            secondary_window_minutes: None,
            secondary_resets_at: None,
            has_credits: None,
            unlimited_credits: None,
            credits_balance: None,
            rate_limit_reached_type: None,
            additional_limits: Vec::new(),
            error: Some(error),
        }
    }

    pub fn unsupported(account_id: String) -> Self {
        Self {
            account_id,
            limit_id: None,
            limit_name: None,
            plan_type: Some("api_key".to_string()),
            primary_used_percent: None,
            primary_window_minutes: None,
            primary_resets_at: None,
            secondary_used_percent: None,
            secondary_window_minutes: None,
            secondary_resets_at: None,
            has_credits: None,
            unlimited_credits: None,
            credits_balance: None,
            rate_limit_reached_type: None,
            additional_limits: Vec::new(),
            error: Some("usage unsupported".to_string()),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitStatusPayload {
    #[serde(rename = "plan_type")]
    pub plan_type: PlanType,
    #[serde(rename = "rate_limit", default)]
    pub rate_limit: Option<Option<Box<RateLimitStatusDetails>>>,
    #[serde(rename = "credits", default)]
    pub credits: Option<Option<Box<CreditStatusDetails>>>,
    #[serde(rename = "additional_rate_limits", default)]
    pub additional_rate_limits: Option<Option<Vec<AdditionalRateLimitDetails>>>,
    #[serde(rename = "rate_limit_reached_type", default)]
    pub rate_limit_reached_type: Option<Option<RateLimitReachedType>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdditionalRateLimitDetails {
    #[serde(rename = "limit_name")]
    pub limit_name: String,
    #[serde(rename = "metered_feature")]
    pub metered_feature: String,
    #[serde(rename = "rate_limit", default)]
    pub rate_limit: Option<Option<Box<RateLimitStatusDetails>>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitReachedType {
    #[serde(rename = "type")]
    pub kind: RateLimitReachedKind,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub enum RateLimitReachedKind {
    #[serde(rename = "rate_limit_reached")]
    RateLimitReached,
    #[serde(rename = "workspace_owner_credits_depleted")]
    WorkspaceOwnerCreditsDepleted,
    #[serde(rename = "workspace_member_credits_depleted")]
    WorkspaceMemberCreditsDepleted,
    #[serde(rename = "workspace_owner_usage_limit_reached")]
    WorkspaceOwnerUsageLimitReached,
    #[serde(rename = "workspace_member_usage_limit_reached")]
    WorkspaceMemberUsageLimitReached,
    #[serde(rename = "unknown", other)]
    Unknown,
}

impl RateLimitReachedKind {
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            Self::RateLimitReached => Some("rate_limit_reached"),
            Self::WorkspaceOwnerCreditsDepleted => Some("workspace_owner_credits_depleted"),
            Self::WorkspaceMemberCreditsDepleted => Some("workspace_member_credits_depleted"),
            Self::WorkspaceOwnerUsageLimitReached => Some("workspace_owner_usage_limit_reached"),
            Self::WorkspaceMemberUsageLimitReached => Some("workspace_member_usage_limit_reached"),
            Self::Unknown => None,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitStatusDetails {
    #[serde(rename = "allowed")]
    pub allowed: bool,
    #[serde(rename = "limit_reached")]
    pub limit_reached: bool,
    #[serde(rename = "primary_window", default)]
    pub primary_window: Option<Option<Box<RateLimitWindowSnapshot>>>,
    #[serde(rename = "secondary_window", default)]
    pub secondary_window: Option<Option<Box<RateLimitWindowSnapshot>>>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitWindowSnapshot {
    #[serde(rename = "used_percent")]
    pub used_percent: i32,
    #[serde(rename = "limit_window_seconds")]
    pub limit_window_seconds: i32,
    #[serde(rename = "reset_after_seconds")]
    pub reset_after_seconds: i32,
    #[serde(rename = "reset_at")]
    pub reset_at: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreditStatusDetails {
    #[serde(rename = "has_credits")]
    pub has_credits: bool,
    #[serde(rename = "unlimited")]
    pub unlimited: bool,
    #[serde(rename = "balance", default)]
    pub balance: Option<Option<String>>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub enum PlanType {
    #[serde(rename = "guest")]
    Guest,
    #[serde(rename = "free")]
    Free,
    #[serde(rename = "go")]
    Go,
    #[serde(rename = "plus")]
    Plus,
    #[serde(rename = "pro")]
    Pro,
    #[serde(rename = "prolite")]
    ProLite,
    #[serde(rename = "free_workspace")]
    FreeWorkspace,
    #[serde(rename = "team")]
    Team,
    #[serde(rename = "self_serve_business_usage_based")]
    SelfServeBusinessUsageBased,
    #[serde(rename = "business")]
    Business,
    #[serde(rename = "enterprise_cbp_usage_based")]
    EnterpriseCbpUsageBased,
    #[serde(rename = "education")]
    Education,
    #[serde(rename = "quorum")]
    Quorum,
    #[serde(rename = "k12")]
    K12,
    #[serde(rename = "enterprise")]
    Enterprise,
    #[serde(rename = "edu")]
    Edu,
    #[serde(rename = "unknown", other)]
    Unknown,
}

impl PlanType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Guest => "guest",
            Self::Free => "free",
            Self::Go => "go",
            Self::Plus => "plus",
            Self::Pro => "pro",
            Self::ProLite => "prolite",
            Self::FreeWorkspace => "free_workspace",
            Self::Team => "team",
            Self::SelfServeBusinessUsageBased => "self_serve_business_usage_based",
            Self::Business => "business",
            Self::EnterpriseCbpUsageBased => "enterprise_cbp_usage_based",
            Self::Education => "education",
            Self::Quorum => "quorum",
            Self::K12 => "k12",
            Self::Enterprise => "enterprise",
            Self::Edu => "edu",
            Self::Unknown => "unknown",
        }
    }
}
