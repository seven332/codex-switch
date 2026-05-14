use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;

use crate::store::{write_new_private_file, write_private_file};
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
    let content = account_auth_json_content(account)?;
    write_private_file(&auth_path, &content)?;
    Ok(auth_path)
}

pub fn account_auth_json_content(account: &StoredAccount) -> Result<String> {
    let auth_json = create_auth_json(account);
    let mut content =
        serde_json::to_string_pretty(&auth_json).context("Failed to serialize auth.json")?;
    content.push('\n');
    Ok(content)
}

pub fn export_account_auth(account: &StoredAccount, path: &Path, force: bool) -> Result<()> {
    let content = account_auth_json_content(account)?;
    if force {
        write_private_file(path, &content)
    } else {
        write_new_private_file(path, &content)
    }
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
        return Ok(StoredAccount::new_api_key(
            account_name,
            api_key.into_inner(),
        ));
    }

    if let Some(tokens) = tokens {
        let claims = parse_chatgpt_id_token_claims(tokens.id_token.expose_secret());
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use chrono::{TimeZone, Utc};
    use uuid::Uuid;

    use super::{account_auth_json_content, export_account_auth};
    use crate::types::{AuthDotJson, NewChatGptAccount, StoredAccount};

    #[test]
    fn oauth_export_uses_codex_auth_json_shape() {
        let account = chatgpt_account();
        let content =
            account_auth_json_content(&account).expect("auth.json should serialize successfully");

        assert!(content.ends_with('\n'));
        let value: serde_json::Value =
            serde_json::from_str(&content).expect("exported auth.json should be valid JSON");
        let auth: AuthDotJson =
            serde_json::from_str(&content).expect("exported auth.json should be valid JSON");
        let tokens = auth.tokens.expect("OAuth export should include tokens");

        assert!(value.get("OPENAI_API_KEY").is_none());
        assert_eq!(auth.auth_mode.as_deref(), Some("chatgpt"));
        assert_eq!(auth.openai_api_key, None);
        assert_eq!(auth.last_refresh, account.token_last_refresh_at);
        assert_eq!(tokens.id_token.expose_secret(), "id-token");
        assert_eq!(tokens.access_token.expose_secret(), "access-token");
        assert_eq!(tokens.refresh_token.expose_secret(), "refresh-token");
        assert_eq!(tokens.account_id.as_deref(), Some("account-id"));
    }

    #[test]
    fn api_key_export_uses_codex_auth_json_shape() {
        let account = StoredAccount::new_api_key("api".to_string(), "sk-test".to_string());
        let content =
            account_auth_json_content(&account).expect("auth.json should serialize successfully");

        assert!(content.ends_with('\n'));
        let value: serde_json::Value =
            serde_json::from_str(&content).expect("exported auth.json should be valid JSON");
        let auth: AuthDotJson =
            serde_json::from_str(&content).expect("exported auth.json should be valid JSON");

        assert_eq!(auth.auth_mode.as_deref(), Some("apikey"));
        assert_eq!(
            auth.openai_api_key
                .as_ref()
                .map(|value| value.expose_secret()),
            Some("sk-test")
        );
        assert_eq!(
            value.get("OPENAI_API_KEY").and_then(|value| value.as_str()),
            Some("sk-test")
        );
        assert!(auth.tokens.is_none());
        assert!(auth.last_refresh.is_none());
    }

    #[test]
    fn auth_json_deserializes_missing_or_null_api_key_as_none() {
        for content in [
            r#"{
                "auth_mode": "chatgpt",
                "tokens": {
                    "id_token": "id-token",
                    "access_token": "access-token",
                    "refresh_token": "refresh-token",
                    "account_id": "account-id"
                }
            }"#,
            r#"{
                "auth_mode": "chatgpt",
                "OPENAI_API_KEY": null,
                "tokens": {
                    "id_token": "id-token",
                    "access_token": "access-token",
                    "refresh_token": "refresh-token",
                    "account_id": "account-id"
                }
            }"#,
        ] {
            let auth: AuthDotJson =
                serde_json::from_str(content).expect("auth.json should deserialize");

            assert_eq!(auth.openai_api_key, None);
            assert!(auth.tokens.is_some());
        }
    }

    #[test]
    fn file_export_refuses_to_overwrite_without_force() {
        let path = temp_export_path("overwrite");
        fs::write(&path, "existing\n").expect("test should create existing file");

        let account = StoredAccount::new_api_key("api".to_string(), "sk-test".to_string());
        let err = export_account_auth(&account, &path, false)
            .expect_err("export should reject existing output without force");

        assert!(
            err.to_string()
                .contains("Refusing to overwrite existing file")
        );
        assert_eq!(
            fs::read_to_string(&path).expect("test should read existing file"),
            "existing\n"
        );

        fs::remove_file(path).expect("test should remove temp file");
    }

    #[test]
    fn file_export_overwrites_with_force() {
        let path = temp_export_path("force");
        fs::write(&path, "existing\n").expect("test should create existing file");

        let account = StoredAccount::new_api_key("api".to_string(), "sk-test".to_string());
        export_account_auth(&account, &path, true).expect("force export should overwrite file");

        let auth: AuthDotJson = serde_json::from_str(
            &fs::read_to_string(&path).expect("test should read exported auth.json"),
        )
        .expect("exported auth.json should be valid JSON");
        assert_eq!(
            auth.openai_api_key
                .as_ref()
                .map(|value| value.expose_secret()),
            Some("sk-test")
        );

        fs::remove_file(path).expect("test should remove temp file");
    }

    #[cfg(unix)]
    #[test]
    fn file_export_writes_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_export_path("permissions");
        let account = StoredAccount::new_api_key("api".to_string(), "sk-test".to_string());

        export_account_auth(&account, &path, false).expect("export should write auth.json");

        let mode = fs::metadata(&path)
            .expect("test should read exported auth.json metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        fs::remove_file(path).expect("test should remove temp file");
    }

    fn chatgpt_account() -> StoredAccount {
        StoredAccount::new_chatgpt(NewChatGptAccount {
            name: "oauth".to_string(),
            email: Some("user@example.com".to_string()),
            plan_type: Some("plus".to_string()),
            chatgpt_user_id: Some("user-id".to_string()),
            chatgpt_account_is_fedramp: false,
            token_last_refresh_at: Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap(),
            subscription_expires_at: None,
            id_token: "id-token".into(),
            access_token: "access-token".into(),
            refresh_token: "refresh-token".into(),
            account_id: Some("account-id".to_string()),
        })
    }

    fn temp_export_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("codex-switch-{name}-{}.json", Uuid::new_v4()))
    }
}
