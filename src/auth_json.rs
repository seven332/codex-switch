use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;

use crate::store::{self, write_new_private_file, write_private_file};
use crate::types::{
    AccountsStore, AuthData, AuthDotJson, AuthJsonAuthMode, NewChatGptAccount, StoredAccount,
    TokenData, try_parse_chatgpt_id_token_claims,
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
    let auth_json = create_auth_json(account)?;
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

pub fn current_auth_account() -> Result<Option<StoredAccount>> {
    let path = codex_auth_file()?;
    if !path.exists() {
        return Ok(None);
    }

    let content = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read auth.json: {}", path.display()))?;
    import_from_auth_json_contents(&content, "current-auth".to_string())
        .map(Some)
        .with_context(|| format!("Failed to parse auth.json: {}", path.display()))
}

pub fn current_stored_account(store: &AccountsStore) -> Result<Option<StoredAccount>> {
    let Some(current_auth_account) = current_auth_account()? else {
        return Ok(None);
    };

    Ok(store::find_matching_account(store, &current_auth_account).cloned())
}

pub fn current_stored_account_best_effort(store: &AccountsStore) -> Option<StoredAccount> {
    current_stored_account(store).ok().flatten()
}

pub fn current_stored_account_id(store: &AccountsStore) -> Result<Option<String>> {
    Ok(current_stored_account(store)?.map(|account| account.id))
}

pub fn current_stored_account_id_best_effort(store: &AccountsStore) -> Option<String> {
    current_stored_account_best_effort(store).map(|account| account.id)
}

fn import_from_auth_json_contents(content: &str, account_name: String) -> Result<StoredAccount> {
    let auth: AuthDotJson =
        serde_json::from_str(content).context("Failed to parse auth.json contents")?;
    let AuthDotJson {
        auth_mode,
        openai_api_key,
        tokens,
        last_refresh,
        agent_identity: _,
        ..
    } = auth;
    let resolved_auth_mode = auth_mode.unwrap_or_else(|| {
        if openai_api_key.is_some() {
            AuthJsonAuthMode::ApiKey
        } else {
            AuthJsonAuthMode::Chatgpt
        }
    });

    match resolved_auth_mode {
        AuthJsonAuthMode::ApiKey => {
            let Some(api_key) = openai_api_key else {
                anyhow::bail!("API key auth.json is missing OPENAI_API_KEY");
            };
            Ok(StoredAccount::new_api_key(
                account_name,
                api_key.into_inner(),
            ))
        }
        AuthJsonAuthMode::Chatgpt => {
            let Some(tokens) = tokens else {
                anyhow::bail!("ChatGPT auth.json is missing tokens");
            };
            let claims = try_parse_chatgpt_id_token_claims(tokens.id_token.expose_secret())
                .context("ChatGPT auth.json contains an invalid id_token")?;
            Ok(StoredAccount::new_chatgpt(NewChatGptAccount {
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
                account_id: tokens.account_id.or(claims.account_id),
            }))
        }
        AuthJsonAuthMode::ChatgptAuthTokens => {
            anyhow::bail!(
                "externally managed ChatGPT auth token auth.json files are not supported by codex-switch"
            );
        }
        AuthJsonAuthMode::AgentIdentity => {
            anyhow::bail!("agent identity auth.json files are not supported by codex-switch");
        }
    }
}

fn create_auth_json(account: &StoredAccount) -> Result<AuthDotJson> {
    match &account.auth_data {
        AuthData::ApiKey { key } => Ok(AuthDotJson {
            auth_mode: Some(AuthJsonAuthMode::ApiKey),
            openai_api_key: Some(key.clone()),
            tokens: None,
            last_refresh: None,
            agent_identity: None,
        }),
        AuthData::ChatGPT {
            id_token,
            access_token,
            refresh_token,
            account_id,
        } => {
            try_parse_chatgpt_id_token_claims(id_token.expose_secret())
                .context("Stored ChatGPT account contains an invalid id_token")?;
            Ok(AuthDotJson {
                auth_mode: Some(AuthJsonAuthMode::Chatgpt),
                openai_api_key: None,
                tokens: Some(TokenData {
                    id_token: id_token.clone(),
                    access_token: access_token.clone(),
                    refresh_token: refresh_token.clone(),
                    account_id: account_id.clone(),
                }),
                last_refresh: account.token_last_refresh_at,
                agent_identity: None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use base64::Engine;
    use chrono::{TimeZone, Utc};
    use uuid::Uuid;

    use super::{account_auth_json_content, export_account_auth, import_from_auth_json_contents};
    use crate::types::{AuthData, AuthDotJson, AuthJsonAuthMode, NewChatGptAccount, StoredAccount};

    #[test]
    fn oauth_export_uses_codex_auth_json_shape() {
        let account = chatgpt_account();
        let expected_id_token = test_id_token();
        let content =
            account_auth_json_content(&account).expect("auth.json should serialize successfully");

        assert!(content.ends_with('\n'));
        let value: serde_json::Value =
            serde_json::from_str(&content).expect("exported auth.json should be valid JSON");
        let auth: AuthDotJson =
            serde_json::from_str(&content).expect("exported auth.json should be valid JSON");
        let tokens = auth.tokens.expect("OAuth export should include tokens");

        assert!(
            value
                .get("OPENAI_API_KEY")
                .is_some_and(serde_json::Value::is_null)
        );
        assert_eq!(auth.auth_mode, Some(AuthJsonAuthMode::Chatgpt));
        assert_eq!(auth.openai_api_key, None);
        assert_eq!(auth.last_refresh, account.token_last_refresh_at);
        assert_eq!(auth.agent_identity, None);
        assert_eq!(tokens.id_token.expose_secret(), expected_id_token);
        assert_eq!(tokens.access_token.expose_secret(), "access-token");
        assert_eq!(tokens.refresh_token.expose_secret(), "refresh-token");
        assert_eq!(tokens.account_id.as_deref(), Some("account-id"));
    }

    #[test]
    fn oauth_export_rejects_invalid_stored_id_token() {
        let mut account = chatgpt_account();
        let AuthData::ChatGPT { id_token, .. } = &mut account.auth_data else {
            panic!("test account should use ChatGPT auth");
        };
        *id_token = "not-a-jwt".into();

        let err = account_auth_json_content(&account)
            .expect_err("invalid stored id_token should not export");

        assert!(
            format!("{err:#}").contains("Stored ChatGPT account contains an invalid id_token"),
            "{err:#}"
        );
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

        assert_eq!(auth.auth_mode, Some(AuthJsonAuthMode::ApiKey));
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
        assert_eq!(auth.agent_identity, None);
    }

    #[test]
    fn auth_json_deserializes_missing_or_null_api_key_as_none() {
        for content in [
            chatgpt_auth_json(Some("chatgpt"), None, None),
            chatgpt_auth_json(Some("chatgpt"), Some(serde_json::Value::Null), None),
        ] {
            let auth: AuthDotJson =
                serde_json::from_str(&content).expect("auth.json should deserialize");

            assert_eq!(auth.openai_api_key, None);
            assert!(auth.tokens.is_some());
        }
    }

    #[test]
    fn import_accepts_api_key_auth_json() {
        let account = import_from_auth_json_contents(
            r#"{
                "auth_mode": "apikey",
                "OPENAI_API_KEY": "sk-test"
            }"#,
            "api".to_string(),
        )
        .expect("API key auth.json should import");

        assert_eq!(account.name, "api");
        assert_eq!(account.auth_mode, crate::types::AuthMode::ApiKey);
        let AuthData::ApiKey { key } = account.auth_data else {
            panic!("expected API key auth data");
        };
        assert_eq!(key.expose_secret(), "sk-test");
    }

    #[test]
    fn import_accepts_regular_chatgpt_auth_json() {
        let expected_id_token = test_id_token();
        let content = chatgpt_auth_json(
            Some("chatgpt"),
            Some(serde_json::Value::Null),
            Some("2026-05-13T12:00:00Z"),
        );
        let account = import_from_auth_json_contents(&content, "chatgpt".to_string())
            .expect("regular ChatGPT auth.json should import");

        assert_eq!(account.name, "chatgpt");
        assert_eq!(account.auth_mode, crate::types::AuthMode::ChatGPT);
        assert_eq!(
            account.token_last_refresh_at,
            Some(Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap())
        );
        let AuthData::ChatGPT {
            id_token,
            access_token,
            refresh_token,
            account_id,
        } = account.auth_data
        else {
            panic!("expected ChatGPT auth data");
        };
        assert_eq!(id_token.expose_secret(), expected_id_token);
        assert_eq!(access_token.expose_secret(), "access-token");
        assert_eq!(refresh_token.expose_secret(), "refresh-token");
        assert_eq!(account_id.as_deref(), Some("account-id"));
    }

    #[test]
    fn import_preserves_token_data_account_id_over_id_token_claim() {
        let mut content: serde_json::Value =
            serde_json::from_str(&chatgpt_auth_json(Some("chatgpt"), None, None))
                .expect("test auth.json should parse");
        content["tokens"]["account_id"] = serde_json::json!("token-data-account-id");

        let account = import_from_auth_json_contents(&content.to_string(), "chatgpt".to_string())
            .expect("regular ChatGPT auth.json should import");

        let AuthData::ChatGPT { account_id, .. } = account.auth_data else {
            panic!("expected ChatGPT auth data");
        };
        assert_eq!(account_id.as_deref(), Some("token-data-account-id"));
    }

    #[test]
    fn import_defaults_missing_auth_mode_with_api_key_to_api_key() {
        let account = import_from_auth_json_contents(
            r#"{
                "OPENAI_API_KEY": "sk-test"
            }"#,
            "api".to_string(),
        )
        .expect("missing auth_mode with API key should import as API key");

        assert_eq!(account.auth_mode, crate::types::AuthMode::ApiKey);
        let AuthData::ApiKey { key } = account.auth_data else {
            panic!("expected API key auth data");
        };
        assert_eq!(key.expose_secret(), "sk-test");
    }

    #[test]
    fn import_defaults_missing_auth_mode_without_api_key_to_chatgpt() {
        let account = import_from_auth_json_contents(
            &chatgpt_auth_json(None, None, None),
            "chatgpt".to_string(),
        )
        .expect("missing auth_mode without API key should import as ChatGPT");

        assert_eq!(account.auth_mode, crate::types::AuthMode::ChatGPT);
        let AuthData::ChatGPT { account_id, .. } = account.auth_data else {
            panic!("expected ChatGPT auth data");
        };
        assert_eq!(account_id.as_deref(), Some("account-id"));
    }

    #[test]
    fn import_respects_explicit_chatgpt_auth_mode_over_api_key() {
        let account = import_from_auth_json_contents(
            &chatgpt_auth_json(Some("chatgpt"), Some(serde_json::json!("sk-test")), None),
            "chatgpt".to_string(),
        )
        .expect("explicit ChatGPT auth mode should use tokens");

        assert_eq!(account.auth_mode, crate::types::AuthMode::ChatGPT);
        let AuthData::ChatGPT { refresh_token, .. } = account.auth_data else {
            panic!("expected ChatGPT auth data");
        };
        assert_eq!(refresh_token.expose_secret(), "refresh-token");
    }

    #[test]
    fn import_rejects_invalid_chatgpt_id_token() {
        let err = import_from_auth_json_contents(
            r#"{
                "auth_mode": "chatgpt",
                "OPENAI_API_KEY": null,
                "tokens": {
                    "id_token": "not-a-jwt",
                    "access_token": "access-token",
                    "refresh_token": "refresh-token",
                    "account_id": "account-id"
                }
            }"#,
            "chatgpt".to_string(),
        )
        .expect_err("invalid ChatGPT id_token should be rejected");

        assert!(format!("{err:#}").contains("invalid ID token"), "{err:#}");
    }

    #[test]
    fn import_respects_explicit_api_key_auth_mode_over_tokens() {
        let err = import_from_auth_json_contents(
            &chatgpt_auth_json(Some("apikey"), None, None),
            "api".to_string(),
        )
        .expect_err("explicit API key auth mode should require OPENAI_API_KEY");

        assert!(err.to_string().contains("missing OPENAI_API_KEY"), "{err}");
    }

    #[test]
    fn import_rejects_agent_identity_auth_json() {
        let err = import_from_auth_json_contents(
            r#"{
                "auth_mode": "agentIdentity",
                "OPENAI_API_KEY": null,
                "agent_identity": "agent-identity-jwt"
            }"#,
            "agent".to_string(),
        )
        .expect_err("agent identity auth should be rejected");

        assert!(
            err.to_string().contains("agent identity auth.json files"),
            "{err}"
        );
    }

    #[test]
    fn import_rejects_external_chatgpt_auth_tokens_auth_json() {
        let err = import_from_auth_json_contents(
            &chatgpt_auth_json(
                Some("chatgptAuthTokens"),
                Some(serde_json::Value::Null),
                None,
            ),
            "external".to_string(),
        )
        .expect_err("external ChatGPT auth tokens should be rejected");

        assert!(
            err.to_string()
                .contains("externally managed ChatGPT auth token"),
            "{err}"
        );
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
            id_token: test_id_token().into(),
            access_token: "access-token".into(),
            refresh_token: "refresh-token".into(),
            account_id: Some("account-id".to_string()),
        })
    }

    fn chatgpt_auth_json(
        auth_mode: Option<&str>,
        openai_api_key: Option<serde_json::Value>,
        last_refresh: Option<&str>,
    ) -> String {
        let mut value = serde_json::json!({
            "tokens": {
                "id_token": test_id_token(),
                "access_token": "access-token",
                "refresh_token": "refresh-token",
                "account_id": "account-id"
            }
        });
        if let Some(auth_mode) = auth_mode {
            value["auth_mode"] = serde_json::json!(auth_mode);
        }
        if let Some(openai_api_key) = openai_api_key {
            value["OPENAI_API_KEY"] = openai_api_key;
        }
        if let Some(last_refresh) = last_refresh {
            value["last_refresh"] = serde_json::json!(last_refresh);
        }
        serde_json::to_string(&value).expect("test auth.json should serialize")
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

    fn temp_export_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("codex-switch-{name}-{}.json", Uuid::new_v4()))
    }
}
