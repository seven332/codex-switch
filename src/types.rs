use std::fmt;

use base64::Engine;
use chrono::{DateTime, Utc};
use serde::de;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// String wrapper that redacts Debug output. This does not zeroize memory.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RedactedString(String);

impl RedactedString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Debug for RedactedString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl From<String> for RedactedString {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for RedactedString {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountsStore {
    pub version: u32,
    pub accounts: Vec<StoredAccount>,
    #[serde(default)]
    pub masked_account_ids: Vec<String>,
}

impl Default for AccountsStore {
    fn default() -> Self {
        Self {
            version: 1,
            accounts: Vec::new(),
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
    #[serde(default)]
    pub auto_switch_disabled: bool,
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
            auth_data: AuthData::ApiKey {
                key: RedactedString::new(api_key),
            },
            auto_switch_disabled: false,
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
            auto_switch_disabled: false,
            created_at: Utc::now(),
            last_used_at: None,
        }
    }

    pub fn auto_switch_enabled(&self) -> bool {
        !self.auto_switch_disabled
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
    pub id_token: RedactedString,
    pub access_token: RedactedString,
    pub refresh_token: RedactedString,
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
        key: RedactedString,
    },
    ChatGPT {
        id_token: RedactedString,
        access_token: RedactedString,
        refresh_token: RedactedString,
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

#[derive(Deserialize)]
struct ChatGptJwtClaims {
    #[serde(default)]
    email: Option<String>,
    #[serde(rename = "https://api.openai.com/profile", default)]
    profile: Option<ChatGptProfileClaims>,
    #[serde(rename = "https://api.openai.com/auth", default)]
    auth: Option<ChatGptAuthClaims>,
}

#[derive(Deserialize)]
struct ChatGptProfileClaims {
    #[serde(default)]
    email: Option<String>,
}

#[derive(Deserialize)]
struct ChatGptAuthClaims {
    #[serde(default)]
    chatgpt_plan_type: Option<String>,
    #[serde(default)]
    chatgpt_user_id: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    chatgpt_account_id: Option<String>,
    #[serde(default)]
    chatgpt_account_is_fedramp: bool,
    #[serde(default)]
    chatgpt_subscription_active_until: Option<serde_json::Value>,
}

#[derive(Debug)]
pub enum ChatGptIdTokenParseError {
    InvalidFormat,
    Base64(base64::DecodeError),
    Json(serde_json::Error),
}

impl fmt::Display for ChatGptIdTokenParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFormat => f.write_str("invalid ID token format"),
            Self::Base64(_) => f.write_str("invalid ID token payload encoding"),
            Self::Json(_) => f.write_str("invalid ID token payload JSON"),
        }
    }
}

impl std::error::Error for ChatGptIdTokenParseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidFormat => None,
            Self::Base64(err) => Some(err),
            Self::Json(err) => Some(err),
        }
    }
}

impl From<base64::DecodeError> for ChatGptIdTokenParseError {
    fn from(err: base64::DecodeError) -> Self {
        Self::Base64(err)
    }
}

impl From<serde_json::Error> for ChatGptIdTokenParseError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

pub fn try_parse_chatgpt_id_token_claims(
    id_token: &str,
) -> Result<ChatGptIdTokenClaims, ChatGptIdTokenParseError> {
    let mut parts = id_token.split('.');
    let (_header_b64, payload_b64, _signature_b64) =
        match (parts.next(), parts.next(), parts.next()) {
            (Some(header), Some(payload), Some(signature))
                if !header.is_empty() && !payload.is_empty() && !signature.is_empty() =>
            {
                (header, payload, signature)
            }
            _ => return Err(ChatGptIdTokenParseError::InvalidFormat),
        };

    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload_b64)?;
    let claims: ChatGptJwtClaims = serde_json::from_slice(&payload)?;

    Ok(chatgpt_id_token_claims_from_jwt_claims(claims))
}

fn chatgpt_id_token_claims_from_jwt_claims(claims: ChatGptJwtClaims) -> ChatGptIdTokenClaims {
    let email = claims
        .email
        .or_else(|| claims.profile.and_then(|profile| profile.email));

    match claims.auth {
        Some(auth) => ChatGptIdTokenClaims {
            email,
            plan_type: auth
                .chatgpt_plan_type
                .as_deref()
                .map(normalize_chatgpt_auth_plan_type),
            user_id: auth.chatgpt_user_id.or(auth.user_id),
            account_id: auth.chatgpt_account_id,
            account_is_fedramp: auth.chatgpt_account_is_fedramp,
            subscription_expires_at: auth
                .chatgpt_subscription_active_until
                .as_ref()
                .and_then(|value| value.as_str())
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.with_timezone(&Utc)),
        },
        None => ChatGptIdTokenClaims {
            email,
            ..ChatGptIdTokenClaims::default()
        },
    }
}

fn normalize_chatgpt_auth_plan_type(value: &str) -> String {
    match value {
        "free" => "free",
        "go" => "go",
        "plus" => "plus",
        "pro" => "pro",
        "prolite" => "prolite",
        "team" => "team",
        "self_serve_business_usage_based" => "self_serve_business_usage_based",
        "business" => "business",
        "enterprise_cbp_usage_based" => "enterprise_cbp_usage_based",
        "enterprise" | "hc" => "enterprise",
        "education" | "edu" => "edu",
        _ => return value.to_string(),
    }
    .to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthDotJson {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<AuthJsonAuthMode>,
    #[serde(rename = "OPENAI_API_KEY")]
    pub openai_api_key: Option<RedactedString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TokenData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_identity: Option<RedactedString>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthJsonAuthMode {
    #[serde(rename = "apikey")]
    ApiKey,
    #[serde(rename = "chatgpt")]
    Chatgpt,
    #[serde(rename = "chatgptAuthTokens")]
    ChatgptAuthTokens,
    #[serde(rename = "agentIdentity")]
    AgentIdentity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenData {
    #[serde(deserialize_with = "deserialize_id_token")]
    pub id_token: RedactedString,
    pub access_token: RedactedString,
    pub refresh_token: RedactedString,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

fn deserialize_id_token<'de, D>(deserializer: D) -> Result<RedactedString, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    try_parse_chatgpt_id_token_claims(&value).map_err(de::Error::custom)?;
    Ok(RedactedString::new(value))
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
    #[serde(default)]
    pub rate_limit_reset_credits_available: Option<i64>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageWindowSlot {
    Primary,
    Secondary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageWindowKind {
    FiveHour,
    Daily,
    Weekly,
    Monthly,
    Annual,
    Other,
}

impl UsageWindowKind {
    pub fn from_window_minutes(window_minutes: Option<i64>) -> Self {
        const FIVE_HOUR_MINUTES: i64 = 5 * 60;
        const DAILY_MINUTES: i64 = 24 * 60;
        const WEEKLY_MINUTES: i64 = 7 * DAILY_MINUTES;
        const MONTHLY_MINUTES: i64 = 30 * DAILY_MINUTES;
        const ANNUAL_MINUTES: i64 = 365 * DAILY_MINUTES;

        let Some(window_minutes) = window_minutes.filter(|minutes| *minutes >= 0) else {
            return Self::Other;
        };

        if is_approximate_window(window_minutes, FIVE_HOUR_MINUTES) {
            Self::FiveHour
        } else if is_approximate_window(window_minutes, DAILY_MINUTES) {
            Self::Daily
        } else if is_approximate_window(window_minutes, WEEKLY_MINUTES) {
            Self::Weekly
        } else if is_approximate_window(window_minutes, MONTHLY_MINUTES) {
            Self::Monthly
        } else if is_approximate_window(window_minutes, ANNUAL_MINUTES) {
            Self::Annual
        } else {
            Self::Other
        }
    }

    pub fn label(self) -> Option<&'static str> {
        match self {
            Self::FiveHour => Some("5-hour"),
            Self::Daily => Some("daily"),
            Self::Weekly => Some("weekly"),
            Self::Monthly => Some("monthly"),
            Self::Annual => Some("annual"),
            Self::Other => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UsageWindowData {
    pub slot: UsageWindowSlot,
    pub used_percent: Option<f64>,
    pub window_minutes: Option<i64>,
    pub resets_at: Option<i64>,
}

impl UsageWindowData {
    pub fn kind(self) -> UsageWindowKind {
        UsageWindowKind::from_window_minutes(self.window_minutes)
    }

    pub fn label(self) -> String {
        if let Some(label) = self.kind().label() {
            return label.to_string();
        }

        match self.window_minutes.filter(|minutes| *minutes > 0) {
            Some(minutes) if minutes % (24 * 60) == 0 => {
                format!("{}-day", minutes / (24 * 60))
            }
            Some(minutes) if minutes % 60 == 0 => format!("{}-hour", minutes / 60),
            Some(minutes) => format!("{minutes}-min"),
            None => match self.slot {
                UsageWindowSlot::Primary => "window 1".to_string(),
                UsageWindowSlot::Secondary => "window 2".to_string(),
            },
        }
    }
}

fn is_approximate_window(minutes: i64, expected_minutes: i64) -> bool {
    let tolerance = expected_minutes / 20;
    (expected_minutes - tolerance..=expected_minutes + tolerance).contains(&minutes)
}

fn usage_windows(
    primary_used_percent: Option<f64>,
    primary_window_minutes: Option<i64>,
    primary_resets_at: Option<i64>,
    secondary_used_percent: Option<f64>,
    secondary_window_minutes: Option<i64>,
    secondary_resets_at: Option<i64>,
) -> [Option<UsageWindowData>; 2] {
    [
        usage_window(
            UsageWindowSlot::Primary,
            primary_used_percent,
            primary_window_minutes,
            primary_resets_at,
        ),
        usage_window(
            UsageWindowSlot::Secondary,
            secondary_used_percent,
            secondary_window_minutes,
            secondary_resets_at,
        ),
    ]
}

fn usage_window(
    slot: UsageWindowSlot,
    used_percent: Option<f64>,
    window_minutes: Option<i64>,
    resets_at: Option<i64>,
) -> Option<UsageWindowData> {
    if used_percent.is_none() && window_minutes.is_none() && resets_at.is_none() {
        return None;
    }

    Some(UsageWindowData {
        slot,
        used_percent,
        window_minutes,
        resets_at,
    })
}

fn unique_window(
    windows: impl Iterator<Item = UsageWindowData>,
    kind: UsageWindowKind,
) -> Option<UsageWindowData> {
    let mut matching = windows.filter(|window| window.kind() == kind);
    let window = matching.next()?;
    matching.next().is_none().then_some(window)
}

impl UsageInfo {
    pub fn windows(&self) -> impl Iterator<Item = UsageWindowData> {
        usage_windows(
            self.primary_used_percent,
            self.primary_window_minutes,
            self.primary_resets_at,
            self.secondary_used_percent,
            self.secondary_window_minutes,
            self.secondary_resets_at,
        )
        .into_iter()
        .flatten()
    }

    pub fn five_hour_window(&self) -> Option<UsageWindowData> {
        unique_window(self.windows(), UsageWindowKind::FiveHour)
    }

    pub fn weekly_window(&self) -> Option<UsageWindowData> {
        unique_window(self.windows(), UsageWindowKind::Weekly)
    }

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
            rate_limit_reset_credits_available: None,
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
            rate_limit_reset_credits_available: None,
            rate_limit_reached_type: None,
            additional_limits: Vec::new(),
            error: Some("usage unsupported".to_string()),
        }
    }
}

impl UsageLimitInfo {
    pub fn windows(&self) -> impl Iterator<Item = UsageWindowData> {
        usage_windows(
            self.primary_used_percent,
            self.primary_window_minutes,
            self.primary_resets_at,
            self.secondary_used_percent,
            self.secondary_window_minutes,
            self.secondary_resets_at,
        )
        .into_iter()
        .flatten()
    }

    pub fn five_hour_window(&self) -> Option<UsageWindowData> {
        unique_window(self.windows(), UsageWindowKind::FiveHour)
    }

    pub fn weekly_window(&self) -> Option<UsageWindowData> {
        unique_window(self.windows(), UsageWindowKind::Weekly)
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
    #[serde(rename = "rate_limit_reset_credits", default)]
    pub rate_limit_reset_credits: Option<RateLimitResetCreditsSummary>,
    #[serde(rename = "additional_rate_limits", default)]
    pub additional_rate_limits: Option<Option<Vec<AdditionalRateLimitDetails>>>,
    #[serde(rename = "rate_limit_reached_type", default)]
    pub rate_limit_reached_type: Option<Option<RateLimitReachedType>>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RateLimitResetCreditsSummary {
    #[serde(rename = "available_count")]
    pub available_count: i64,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ConsumeRateLimitResetCreditResponse {
    pub code: ConsumeRateLimitResetCreditCode,
    #[serde(default)]
    pub windows_reset: i64,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConsumeRateLimitResetCreditCode {
    Reset,
    NothingToReset,
    NoCredit,
    AlreadyRedeemed,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanType {
    Guest,
    Free,
    Go,
    Plus,
    Pro,
    ProLite,
    FreeWorkspace,
    Team,
    SelfServeBusinessUsageBased,
    Business,
    EnterpriseCbpUsageBased,
    Education,
    Quorum,
    K12,
    Enterprise,
    Edu,
    Unknown(String),
}

impl<'de> Deserialize<'de> for PlanType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(Self::from_raw_value(value))
    }
}

impl PlanType {
    fn from_raw_value(value: String) -> Self {
        match value.as_str() {
            "guest" => Self::Guest,
            "free" => Self::Free,
            "go" => Self::Go,
            "plus" => Self::Plus,
            "pro" => Self::Pro,
            "prolite" => Self::ProLite,
            "free_workspace" => Self::FreeWorkspace,
            "team" => Self::Team,
            "self_serve_business_usage_based" => Self::SelfServeBusinessUsageBased,
            "business" => Self::Business,
            "enterprise_cbp_usage_based" => Self::EnterpriseCbpUsageBased,
            "education" => Self::Education,
            "quorum" => Self::Quorum,
            "k12" => Self::K12,
            "enterprise" => Self::Enterprise,
            "edu" => Self::Edu,
            _ => Self::Unknown(value),
        }
    }

    pub fn as_str(&self) -> &str {
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
            Self::Unknown(value) => value,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn redacted_string_debug_redacts_secret_but_json_preserves_value() {
        let secret = "secret-value-that-must-stay-private";
        let value = RedactedString::new(secret);

        assert_eq!(format!("{value:?}"), "<redacted>");

        let json = serde_json::to_string(&value).expect("serialize redacted string");
        assert!(json.contains(secret));

        let decoded: RedactedString =
            serde_json::from_str(&json).expect("deserialize redacted string");
        assert_eq!(decoded.expose_secret(), secret);
    }

    #[test]
    fn account_debug_redacts_api_key_and_chatgpt_tokens() {
        let api_secret = "sk-codex-switch-test-secret";
        let id_secret = "id-token-codex-switch-test-secret";
        let access_secret = "access-token-codex-switch-test-secret";
        let refresh_secret = "refresh-token-codex-switch-test-secret";

        let api_auth = AuthData::ApiKey {
            key: api_secret.into(),
        };
        let chatgpt_auth = AuthData::ChatGPT {
            id_token: id_secret.into(),
            access_token: access_secret.into(),
            refresh_token: refresh_secret.into(),
            account_id: Some("account-id".to_string()),
        };
        let api_account = StoredAccount::new_api_key("api".to_string(), api_secret.to_string());
        let chatgpt_account = StoredAccount::new_chatgpt(NewChatGptAccount {
            name: "chatgpt".to_string(),
            email: Some("user@example.com".to_string()),
            plan_type: Some("pro".to_string()),
            chatgpt_user_id: Some("user-id".to_string()),
            chatgpt_account_is_fedramp: false,
            token_last_refresh_at: Utc::now(),
            subscription_expires_at: None,
            id_token: id_secret.into(),
            access_token: access_secret.into(),
            refresh_token: refresh_secret.into(),
            account_id: Some("account-id".to_string()),
        });
        let debug = format!("{api_auth:?} {chatgpt_auth:?} {api_account:?} {chatgpt_account:?}");

        for secret in [api_secret, id_secret, access_secret, refresh_secret] {
            assert!(!debug.contains(secret), "debug output leaked {secret}");
        }
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn accounts_and_auth_json_keep_raw_secret_json_values() {
        let api_secret = "sk-json-compat-secret";
        let id_secret = test_id_token();
        let access_secret = "access-token-json-compat-secret";
        let refresh_secret = "refresh-token-json-compat-secret";
        let agent_identity_secret = "agent-identity-json-compat-secret";

        let api_account = StoredAccount::new_api_key("api".to_string(), api_secret.to_string());
        let chatgpt_account = StoredAccount::new_chatgpt(NewChatGptAccount {
            name: "chatgpt".to_string(),
            email: None,
            plan_type: Some("pro".to_string()),
            chatgpt_user_id: None,
            chatgpt_account_is_fedramp: false,
            token_last_refresh_at: Utc::now(),
            subscription_expires_at: None,
            id_token: id_secret.clone().into(),
            access_token: access_secret.into(),
            refresh_token: refresh_secret.into(),
            account_id: Some("account-id".to_string()),
        });
        let store = AccountsStore {
            version: 1,
            accounts: vec![api_account, chatgpt_account],
            masked_account_ids: Vec::new(),
        };
        let auth_json = AuthDotJson {
            auth_mode: Some(AuthJsonAuthMode::Chatgpt),
            openai_api_key: Some(api_secret.into()),
            tokens: Some(TokenData {
                id_token: id_secret.clone().into(),
                access_token: access_secret.into(),
                refresh_token: refresh_secret.into(),
                account_id: Some("account-id".to_string()),
            }),
            last_refresh: None,
            agent_identity: Some(agent_identity_secret.into()),
        };
        let auth_debug = format!(
            "{auth_json:?} {:?}",
            auth_json.tokens.as_ref().expect("tokens")
        );
        for secret in [
            api_secret,
            id_secret.as_str(),
            access_secret,
            refresh_secret,
            agent_identity_secret,
        ] {
            assert!(
                !auth_debug.contains(secret),
                "auth debug output leaked {secret}"
            );
        }
        assert!(auth_debug.contains("<redacted>"));

        let store_json = serde_json::to_string(&store).expect("serialize accounts store");
        let auth_file_json = serde_json::to_string(&auth_json).expect("serialize auth json");
        for secret in [
            api_secret,
            id_secret.as_str(),
            access_secret,
            refresh_secret,
        ] {
            assert!(store_json.contains(secret), "accounts json lost {secret}");
        }
        for secret in [
            api_secret,
            id_secret.as_str(),
            access_secret,
            refresh_secret,
            agent_identity_secret,
        ] {
            assert!(auth_file_json.contains(secret), "auth json lost {secret}");
        }

        let decoded_auth: AuthDotJson =
            serde_json::from_str(&auth_file_json).expect("deserialize auth json");
        assert_eq!(
            decoded_auth
                .openai_api_key
                .as_ref()
                .map(RedactedString::expose_secret),
            Some(api_secret)
        );
        let decoded_tokens = decoded_auth.tokens.expect("tokens");
        assert_eq!(decoded_tokens.id_token.expose_secret(), id_secret);
        assert_eq!(decoded_tokens.access_token.expose_secret(), access_secret);
        assert_eq!(decoded_tokens.refresh_token.expose_secret(), refresh_secret);
        assert_eq!(
            decoded_auth
                .agent_identity
                .as_ref()
                .map(RedactedString::expose_secret),
            Some(agent_identity_secret)
        );
    }

    #[test]
    fn accounts_store_does_not_serialize_active_account_id() {
        let store = AccountsStore::default();
        let value = serde_json::to_value(&store).expect("serialize accounts store");

        assert!(value.get("active_account_id").is_none());
    }

    #[test]
    fn accounts_store_ignores_legacy_active_account_id() {
        let store: AccountsStore = serde_json::from_value(serde_json::json!({
            "version": 1,
            "accounts": [],
            "active_account_id": "legacy-account-id",
            "masked_account_ids": []
        }))
        .expect("deserialize accounts store");

        assert!(store.accounts.is_empty());
        assert!(store.masked_account_ids.is_empty());
    }

    #[test]
    fn stored_account_defaults_auto_switch_disabled_to_false() {
        let account: StoredAccount = serde_json::from_value(serde_json::json!({
            "id": "account-id",
            "name": "api",
            "email": null,
            "plan_type": null,
            "chatgpt_user_id": null,
            "chatgpt_account_is_fedramp": false,
            "token_last_refresh_at": null,
            "subscription_expires_at": null,
            "auth_mode": "api_key",
            "auth_data": {
                "type": "api_key",
                "key": "sk-test"
            },
            "created_at": "2026-01-01T00:00:00Z",
            "last_used_at": null
        }))
        .expect("legacy account should deserialize");

        assert!(!account.auto_switch_disabled);
        assert!(account.auto_switch_enabled());
    }

    #[test]
    fn new_accounts_are_auto_switch_enabled() {
        let api_account = StoredAccount::new_api_key("api".to_string(), "sk-test".to_string());
        let chatgpt_account = StoredAccount::new_chatgpt(NewChatGptAccount {
            name: "chatgpt".to_string(),
            email: None,
            plan_type: Some("pro".to_string()),
            chatgpt_user_id: None,
            chatgpt_account_is_fedramp: false,
            token_last_refresh_at: Utc::now(),
            subscription_expires_at: None,
            id_token: test_id_token().into(),
            access_token: "access-token".into(),
            refresh_token: "refresh-token".into(),
            account_id: Some("account-id".to_string()),
        });

        assert!(api_account.auto_switch_enabled());
        assert!(chatgpt_account.auto_switch_enabled());
    }

    #[test]
    fn usage_plan_type_preserves_unknown_raw_value() {
        let plan: PlanType =
            serde_json::from_str(r#""future_plan""#).expect("plan should deserialize");

        assert_eq!(plan.as_str(), "future_plan");
    }

    #[test]
    fn usage_window_kind_uses_upstream_tolerance_boundaries() {
        assert_eq!(
            UsageWindowKind::from_window_minutes(Some(285)),
            UsageWindowKind::FiveHour
        );
        assert_eq!(
            UsageWindowKind::from_window_minutes(Some(315)),
            UsageWindowKind::FiveHour
        );
        assert_eq!(
            UsageWindowKind::from_window_minutes(Some(284)),
            UsageWindowKind::Other
        );
        assert_eq!(
            UsageWindowKind::from_window_minutes(Some(316)),
            UsageWindowKind::Other
        );
        assert_eq!(
            UsageWindowKind::from_window_minutes(Some(9_576)),
            UsageWindowKind::Weekly
        );
        assert_eq!(
            UsageWindowKind::from_window_minutes(Some(10_584)),
            UsageWindowKind::Weekly
        );
        assert_eq!(
            UsageWindowKind::from_window_minutes(None),
            UsageWindowKind::Other
        );
    }

    #[test]
    fn usage_window_labels_known_and_custom_durations() {
        let window = |slot, window_minutes| UsageWindowData {
            slot,
            used_percent: Some(10.0),
            window_minutes,
            resets_at: None,
        };

        assert_eq!(
            window(UsageWindowSlot::Primary, Some(300)).label(),
            "5-hour"
        );
        assert_eq!(
            window(UsageWindowSlot::Primary, Some(1_440)).label(),
            "daily"
        );
        assert_eq!(
            window(UsageWindowSlot::Primary, Some(10_080)).label(),
            "weekly"
        );
        assert_eq!(
            window(UsageWindowSlot::Primary, Some(43_200)).label(),
            "monthly"
        );
        assert_eq!(
            window(UsageWindowSlot::Primary, Some(525_600)).label(),
            "annual"
        );
        assert_eq!(
            window(UsageWindowSlot::Primary, Some(120)).label(),
            "2-hour"
        );
        assert_eq!(window(UsageWindowSlot::Primary, Some(90)).label(), "90-min");
        assert_eq!(window(UsageWindowSlot::Primary, None).label(), "window 1");
        assert_eq!(window(UsageWindowSlot::Secondary, None).label(), "window 2");
    }

    #[test]
    fn usage_info_finds_canonical_windows_in_either_slot() {
        let mut info = UsageInfo::error("account-id".to_string(), "unused".to_string());
        info.primary_used_percent = Some(20.0);
        info.primary_window_minutes = Some(10_080);
        info.primary_resets_at = Some(20_000);
        info.secondary_used_percent = Some(10.0);
        info.secondary_window_minutes = Some(300);
        info.secondary_resets_at = Some(10_000);

        let five_hour = info.five_hour_window().expect("5-hour window");
        let weekly = info.weekly_window().expect("weekly window");

        assert_eq!(five_hour.slot, UsageWindowSlot::Secondary);
        assert_eq!(five_hour.used_percent, Some(10.0));
        assert_eq!(weekly.slot, UsageWindowSlot::Primary);
        assert_eq!(weekly.used_percent, Some(20.0));
    }

    #[test]
    fn usage_info_omits_absent_slots_and_rejects_duplicate_canonical_windows() {
        let mut info = UsageInfo::error("account-id".to_string(), "unused".to_string());
        info.primary_used_percent = Some(10.0);
        info.primary_window_minutes = Some(300);

        assert_eq!(info.windows().count(), 1);
        assert!(info.five_hour_window().is_some());
        assert!(info.weekly_window().is_none());

        info.secondary_used_percent = Some(20.0);
        info.secondary_window_minutes = Some(300);

        assert_eq!(info.windows().count(), 2);
        assert!(info.five_hour_window().is_none());
    }

    #[test]
    fn consume_rate_limit_reset_credit_response_parses_backend_codes() {
        let response: ConsumeRateLimitResetCreditResponse = serde_json::from_value(
            serde_json::json!({ "code": "already_redeemed", "windows_reset": 0 }),
        )
        .expect("consume response");

        assert_eq!(
            response.code,
            ConsumeRateLimitResetCreditCode::AlreadyRedeemed
        );
        assert_eq!(response.windows_reset, 0);
    }

    #[test]
    fn usage_info_deserializes_without_rate_limit_reset_credits() {
        let info: UsageInfo = serde_json::from_value(serde_json::json!({
            "account_id": "account-id",
            "limit_id": null,
            "limit_name": null,
            "plan_type": "pro",
            "primary_used_percent": null,
            "primary_window_minutes": null,
            "primary_resets_at": null,
            "secondary_used_percent": null,
            "secondary_window_minutes": null,
            "secondary_resets_at": null,
            "has_credits": null,
            "unlimited_credits": null,
            "credits_balance": null,
            "rate_limit_reached_type": null,
            "additional_limits": [],
            "error": null
        }))
        .expect("usage info");

        assert_eq!(info.rate_limit_reset_credits_available, None);
    }

    #[test]
    fn id_token_plan_type_normalizes_codex_auth_aliases() {
        let enterprise = try_parse_chatgpt_id_token_claims(&test_id_token_with_plan("hc"))
            .expect("enterprise alias should parse");
        let edu = try_parse_chatgpt_id_token_claims(&test_id_token_with_plan("education"))
            .expect("education alias should parse");

        assert_eq!(enterprise.plan_type.as_deref(), Some("enterprise"));
        assert_eq!(edu.plan_type.as_deref(), Some("edu"));
    }

    #[test]
    fn id_token_plan_type_preserves_unknown_raw_value() {
        let claims = try_parse_chatgpt_id_token_claims(&test_id_token_with_plan("Pro"))
            .expect("unknown plan casing should still parse");

        assert_eq!(claims.plan_type.as_deref(), Some("Pro"));
    }

    #[test]
    fn id_token_ignores_invalid_subscription_expiry_type() {
        let payload = serde_json::json!({
            "email": "user@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "account-id",
                "chatgpt_plan_type": "pro",
                "chatgpt_user_id": "user-id",
                "chatgpt_account_is_fedramp": false,
                "chatgpt_subscription_active_until": 123
            }
        });

        let claims = try_parse_chatgpt_id_token_claims(&test_id_token_with_payload(payload))
            .expect("non-Codex subscription expiry type should not reject the token");

        assert_eq!(claims.subscription_expires_at, None);
    }

    #[test]
    fn id_token_claims_reject_invalid_claim_types() {
        let payload = serde_json::json!({
            "email": "user@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "account-id",
                "chatgpt_plan_type": "pro",
                "chatgpt_user_id": "user-id",
                "chatgpt_account_is_fedramp": "false"
            }
        });

        let err = try_parse_chatgpt_id_token_claims(&test_id_token_with_payload(payload))
            .expect_err("invalid claim type should be rejected");

        assert!(err.to_string().contains("invalid ID token payload JSON"));
    }

    fn test_id_token() -> String {
        test_id_token_with_plan("pro")
    }

    fn test_id_token_with_plan(plan_type: &str) -> String {
        test_id_token_with_payload(serde_json::json!({
            "email": "user@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "account-id",
                "chatgpt_plan_type": plan_type,
                "chatgpt_user_id": "user-id",
                "chatgpt_account_is_fedramp": false
            }
        }))
    }

    fn test_id_token_with_payload(payload: serde_json::Value) -> String {
        let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let payload_json = serde_json::to_vec(&payload).expect("test payload should serialize");

        format!(
            "{}.{}.{}",
            encode(br#"{"alg":"none","typ":"JWT"}"#),
            encode(&payload_json),
            encode(b"sig")
        )
    }
}
