use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;

use crate::store::write_private_file;
use crate::types::{
    AuthData, AuthDotJson, NewChatGptAccount, StoredAccount, TokenData,
    parse_chatgpt_id_token_claims,
};

pub fn codex_home() -> Result<PathBuf> {
    if let Ok(codex_home) = std::env::var("CODEX_HOME")
        && !codex_home.trim().is_empty()
    {
        return Ok(PathBuf::from(codex_home));
    }

    let home = dirs::home_dir().context("Could not find home directory")?;
    Ok(home.join(".codex"))
}

pub fn codex_auth_file() -> Result<PathBuf> {
    Ok(codex_home()?.join("auth.json"))
}

pub fn write_account_auth(account: &StoredAccount) -> Result<PathBuf> {
    let auth_path = codex_auth_file()?;
    let auth_json = create_auth_json(account);
    let mut content =
        serde_json::to_string_pretty(&auth_json).context("Failed to serialize auth.json")?;
    content.push('\n');
    write_private_file(&auth_path, &content)?;
    Ok(auth_path)
}

pub fn import_from_auth_json(path: &Path, account_name: String) -> Result<StoredAccount> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read auth.json: {}", path.display()))?;
    import_from_auth_json_contents(&content, account_name)
        .with_context(|| format!("Failed to parse auth.json: {}", path.display()))
}

fn import_from_auth_json_contents(content: &str, account_name: String) -> Result<StoredAccount> {
    let auth: AuthDotJson =
        serde_json::from_str(content).context("Failed to parse auth.json contents")?;
    let AuthDotJson {
        openai_api_key,
        tokens,
        last_refresh,
        ..
    } = auth;

    if let Some(api_key) = openai_api_key {
        return Ok(StoredAccount::new_api_key(account_name, api_key));
    }

    if let Some(tokens) = tokens {
        let claims = parse_chatgpt_id_token_claims(&tokens.id_token);
        return Ok(StoredAccount::new_chatgpt(NewChatGptAccount {
            name: account_name,
            email: claims.email,
            plan_type: claims.plan_type,
            chatgpt_user_id: claims.user_id,
            chatgpt_account_is_fedramp: claims.account_is_fedramp,
            token_last_refresh_at: last_refresh.unwrap_or_else(Utc::now),
            subscription_expires_at: claims.subscription_expires_at,
            id_token: tokens.id_token,
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            account_id: claims.account_id.or(tokens.account_id),
        }));
    }

    anyhow::bail!("auth.json contains neither OPENAI_API_KEY nor tokens");
}

fn create_auth_json(account: &StoredAccount) -> AuthDotJson {
    match &account.auth_data {
        AuthData::ApiKey { key } => AuthDotJson {
            auth_mode: Some("apikey".to_string()),
            openai_api_key: Some(key.clone()),
            tokens: None,
            last_refresh: None,
        },
        AuthData::ChatGPT {
            id_token,
            access_token,
            refresh_token,
            account_id,
        } => AuthDotJson {
            auth_mode: Some("chatgpt".to_string()),
            openai_api_key: None,
            tokens: Some(TokenData {
                id_token: id_token.clone(),
                access_token: access_token.clone(),
                refresh_token: refresh_token.clone(),
                account_id: account_id.clone(),
            }),
            last_refresh: account.token_last_refresh_at,
        },
    }
}
