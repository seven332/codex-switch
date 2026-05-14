use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::types::{AccountsStore, AuthData, RedactedString, StoredAccount};

pub fn config_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not find home directory")?;
    Ok(home.join(".codex-switch"))
}

pub fn accounts_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("accounts.json"))
}

pub fn load_accounts() -> Result<AccountsStore> {
    let path = accounts_file()?;

    if !path.exists() {
        return Ok(AccountsStore::default());
    }

    let content = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read accounts file: {}", path.display()))?;
    let store = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse accounts file: {}", path.display()))?;

    Ok(store)
}

pub fn save_accounts(store: &AccountsStore) -> Result<()> {
    let path = accounts_file()?;
    let mut content =
        serde_json::to_string_pretty(store).context("Failed to serialize accounts store")?;
    content.push('\n');
    write_private_file(&path, &content)
}

pub fn write_private_file(path: &Path, content: &str) -> Result<()> {
    write_private_file_with_mode(path, content, PrivateFileWriteMode::Replace)
}

pub fn write_new_private_file(path: &Path, content: &str) -> Result<()> {
    write_private_file_with_mode(path, content, PrivateFileWriteMode::CreateNew)
}

#[derive(Debug, Clone, Copy)]
enum PrivateFileWriteMode {
    Replace,
    CreateNew,
}

fn write_private_file_with_mode(
    path: &Path,
    content: &str,
    mode: PrivateFileWriteMode,
) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }

    let temp_path = temp_file_path(path);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options
        .open(&temp_path)
        .with_context(|| format!("Failed to open private file: {}", temp_path.display()))?;
    file.write_all(content.as_bytes())
        .with_context(|| format!("Failed to write private file: {}", temp_path.display()))?;
    file.sync_all()
        .with_context(|| format!("Failed to sync private file: {}", temp_path.display()))?;
    drop(file);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(&temp_path, fs::Permissions::from_mode(0o600)).with_context(|| {
            format!(
                "Failed to set private file permissions: {}",
                temp_path.display()
            )
        })?;
    }

    match mode {
        PrivateFileWriteMode::Replace => {
            fs::rename(&temp_path, path).with_context(|| {
                format!(
                    "Failed to replace private file {} with {}",
                    path.display(),
                    temp_path.display()
                )
            })?;
        }
        PrivateFileWriteMode::CreateNew => {
            // Link the fully written temp file into place so create-new export never overwrites.
            if let Err(err) = fs::hard_link(&temp_path, path) {
                let _ = fs::remove_file(&temp_path);
                if err.kind() == std::io::ErrorKind::AlreadyExists {
                    anyhow::bail!(
                        "Refusing to overwrite existing file: {} (pass --force to overwrite)",
                        path.display()
                    );
                }
                return Err(err)
                    .with_context(|| format!("Failed to create private file: {}", path.display()));
            }
            let _ = fs::remove_file(&temp_path);
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("Failed to set file permissions: {}", path.display()))?;
    }

    Ok(())
}

fn temp_file_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("private");
    path.with_file_name(format!(".{file_name}.{}.tmp", Uuid::new_v4()))
}

pub fn ensure_name_available(name: &str) -> Result<()> {
    let store = load_accounts()?;
    if store.accounts.iter().any(|account| account.name == name) {
        anyhow::bail!("An account named '{name}' already exists");
    }
    Ok(())
}

pub fn add_account(account: StoredAccount) -> Result<StoredAccount> {
    let mut store = load_accounts()?;

    if store
        .accounts
        .iter()
        .any(|existing| existing.name == account.name)
    {
        anyhow::bail!("An account named '{}' already exists", account.name);
    }

    let stored = account.clone();
    store.accounts.push(account);
    save_accounts(&store)?;
    Ok(stored)
}

pub fn find_duplicate_account(account: &StoredAccount) -> Result<Option<StoredAccount>> {
    let store = load_accounts()?;
    Ok(store
        .accounts
        .into_iter()
        .find(|existing| has_same_auth_identity(existing, account)))
}

fn has_same_auth_identity(left: &StoredAccount, right: &StoredAccount) -> bool {
    match (&left.auth_data, &right.auth_data) {
        (AuthData::ApiKey { key: left_key }, AuthData::ApiKey { key: right_key }) => {
            left_key == right_key
        }
        (
            AuthData::ChatGPT {
                id_token: left_id_token,
                refresh_token: left_refresh_token,
                account_id: left_account_id,
                ..
            },
            AuthData::ChatGPT {
                id_token: right_id_token,
                refresh_token: right_refresh_token,
                account_id: right_account_id,
                ..
            },
        ) => {
            same_non_empty_option(left_account_id.as_deref(), right_account_id.as_deref())
                || same_non_empty(
                    left_refresh_token.expose_secret(),
                    right_refresh_token.expose_secret(),
                )
                || same_non_empty(
                    left_id_token.expose_secret(),
                    right_id_token.expose_secret(),
                )
                || (same_non_empty_option(
                    left.chatgpt_user_id.as_deref(),
                    right.chatgpt_user_id.as_deref(),
                ) && same_non_empty_option(left.email.as_deref(), right.email.as_deref()))
        }
        _ => false,
    }
}

fn same_non_empty(left: &str, right: &str) -> bool {
    !left.is_empty() && left == right
}

fn same_non_empty_option(left: Option<&str>, right: Option<&str>) -> bool {
    matches!((left, right), (Some(left), Some(right)) if same_non_empty(left, right))
}

pub fn resolve_account_id(store: &AccountsStore, selector: &str) -> Result<String> {
    if let Some(account) = store.accounts.iter().find(|account| account.id == selector) {
        return Ok(account.id.clone());
    }

    if let Some(account) = store
        .accounts
        .iter()
        .find(|account| account.name == selector)
    {
        return Ok(account.id.clone());
    }

    let id_prefix_matches = store
        .accounts
        .iter()
        .filter(|account| account.id.starts_with(selector))
        .collect::<Vec<_>>();

    match id_prefix_matches.as_slice() {
        [account] => Ok(account.id.clone()),
        [] => anyhow::bail!("Account not found: {selector}"),
        matches => {
            let ids = matches
                .iter()
                .map(|account| format!("{} ({})", account.name, short_id(&account.id)))
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!("Account selector '{selector}' is ambiguous: {ids}");
        }
    }
}

pub fn get_account_by_selector(selector: &str) -> Result<StoredAccount> {
    let store = load_accounts()?;
    let account_id = resolve_account_id(&store, selector)?;
    store
        .accounts
        .into_iter()
        .find(|account| account.id == account_id)
        .context("Account not found after resolving selector")
}

pub fn get_active_account() -> Result<Option<StoredAccount>> {
    let store = load_accounts()?;
    let Some(active_id) = store.active_account_id else {
        return Ok(None);
    };

    Ok(store
        .accounts
        .into_iter()
        .find(|account| account.id == active_id))
}

pub fn set_active_account(account_id: &str) -> Result<()> {
    let mut store = load_accounts()?;

    if !store
        .accounts
        .iter()
        .any(|account| account.id == account_id)
    {
        anyhow::bail!("Account not found: {account_id}");
    }

    store.active_account_id = Some(account_id.to_string());
    save_accounts(&store)
}

pub fn touch_account(account_id: &str) -> Result<()> {
    let mut store = load_accounts()?;

    if let Some(account) = store
        .accounts
        .iter_mut()
        .find(|account| account.id == account_id)
    {
        account.last_used_at = Some(Utc::now());
        save_accounts(&store)?;
    }

    Ok(())
}

pub fn remove_account_by_selector(selector: &str) -> Result<StoredAccount> {
    let mut store = load_accounts()?;
    let account_id = resolve_account_id(&store, selector)?;
    let index = store
        .accounts
        .iter()
        .position(|account| account.id == account_id)
        .context("Account not found after resolving selector")?;
    let removed = store.accounts.remove(index);

    if store.active_account_id.as_deref() == Some(&removed.id) {
        store.active_account_id = None;
    }

    save_accounts(&store)?;
    Ok(removed)
}

pub fn rename_account_by_selector(selector: &str, new_name: String) -> Result<StoredAccount> {
    let mut store = load_accounts()?;
    let account_id = resolve_account_id(&store, selector)?;

    if store
        .accounts
        .iter()
        .any(|account| account.id != account_id && account.name == new_name)
    {
        anyhow::bail!("An account named '{new_name}' already exists");
    }

    let account = store
        .accounts
        .iter_mut()
        .find(|account| account.id == account_id)
        .context("Account not found after resolving selector")?;
    account.name = new_name;
    let updated = account.clone();

    save_accounts(&store)?;
    Ok(updated)
}

#[derive(Debug, Clone)]
pub struct ChatGptTokenUpdate {
    pub id_token: Option<RedactedString>,
    pub access_token: Option<RedactedString>,
    pub refresh_token: Option<RedactedString>,
    pub chatgpt_account_id: Option<String>,
    pub email: Option<String>,
    pub plan_type: Option<String>,
    pub chatgpt_user_id: Option<String>,
    pub chatgpt_account_is_fedramp: Option<bool>,
    pub token_last_refresh_at: DateTime<Utc>,
    pub subscription_expires_at: Option<DateTime<Utc>>,
}

pub fn update_account_chatgpt_tokens(
    account_id: &str,
    update: ChatGptTokenUpdate,
) -> Result<StoredAccount> {
    let mut store = load_accounts()?;
    let account = store
        .accounts
        .iter_mut()
        .find(|account| account.id == account_id)
        .context("Account not found")?;

    match &mut account.auth_data {
        AuthData::ChatGPT {
            id_token: stored_id_token,
            access_token: stored_access_token,
            refresh_token: stored_refresh_token,
            account_id: stored_account_id,
        } => {
            if let Some(id_token) = update.id_token {
                *stored_id_token = id_token;
            }
            if let Some(access_token) = update.access_token {
                *stored_access_token = access_token;
            }
            if let Some(refresh_token) = update.refresh_token {
                *stored_refresh_token = refresh_token;
            }
            if let Some(chatgpt_account_id) = update.chatgpt_account_id {
                *stored_account_id = Some(chatgpt_account_id);
            }
        }
        AuthData::ApiKey { .. } => {
            anyhow::bail!("Cannot update OAuth tokens for an API key account");
        }
    }

    if let Some(email) = update.email {
        account.email = Some(email);
    }

    if let Some(plan_type) = update.plan_type {
        account.plan_type = Some(plan_type);
    }

    if let Some(chatgpt_user_id) = update.chatgpt_user_id {
        account.chatgpt_user_id = Some(chatgpt_user_id);
    }

    if let Some(chatgpt_account_is_fedramp) = update.chatgpt_account_is_fedramp {
        account.chatgpt_account_is_fedramp = chatgpt_account_is_fedramp;
    }

    account.token_last_refresh_at = Some(update.token_last_refresh_at);

    if let Some(subscription_expires_at) = update.subscription_expires_at {
        account.subscription_expires_at = Some(subscription_expires_at);
    }

    let updated = account.clone();
    save_accounts(&store)?;
    Ok(updated)
}

pub fn update_account_usage_metadata(
    account_id: &str,
    plan_type: Option<String>,
) -> Result<Option<StoredAccount>> {
    let mut store = load_accounts()?;
    let Some(account) = store
        .accounts
        .iter_mut()
        .find(|account| account.id == account_id)
    else {
        return Ok(None);
    };

    if !apply_usage_metadata(account, plan_type) {
        return Ok(Some(account.clone()));
    }

    let updated = account.clone();
    save_accounts(&store)?;
    Ok(Some(updated))
}

fn apply_usage_metadata(account: &mut StoredAccount, plan_type: Option<String>) -> bool {
    let Some(plan_type) = plan_type
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    else {
        return false;
    };

    if account.plan_type.as_deref() == Some(plan_type.as_str()) {
        return false;
    }

    account.plan_type = Some(plan_type);
    true
}

pub fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::{apply_usage_metadata, has_same_auth_identity};
    use crate::types::{NewChatGptAccount, StoredAccount};
    use chrono::Utc;

    #[test]
    fn duplicate_auth_matches_api_key() {
        let left = StoredAccount::new_api_key("left".to_string(), "sk-test".to_string());
        let right = StoredAccount::new_api_key("right".to_string(), "sk-test".to_string());

        assert!(has_same_auth_identity(&left, &right));
    }

    #[test]
    fn duplicate_auth_matches_chatgpt_account_id() {
        let left = chatgpt_account("left", Some("account-id"), "refresh-left", "id-left");
        let right = chatgpt_account("right", Some("account-id"), "refresh-right", "id-right");

        assert!(has_same_auth_identity(&left, &right));
    }

    #[test]
    fn duplicate_auth_matches_chatgpt_refresh_token() {
        let left = chatgpt_account("left", None, "refresh-token", "id-left");
        let right = chatgpt_account("right", None, "refresh-token", "id-right");

        assert!(has_same_auth_identity(&left, &right));
    }

    #[test]
    fn duplicate_auth_rejects_different_chatgpt_accounts() {
        let left = chatgpt_account("left", Some("left-account"), "left-refresh", "left-id");
        let right = chatgpt_account("right", Some("right-account"), "right-refresh", "right-id");

        assert!(!has_same_auth_identity(&left, &right));
    }

    #[test]
    fn usage_metadata_updates_plan_type() {
        let mut account = chatgpt_account("account", Some("account-id"), "refresh", "id");
        account.plan_type = Some("free".to_string());

        assert!(apply_usage_metadata(
            &mut account,
            Some(" pro ".to_string())
        ));
        assert_eq!(account.plan_type.as_deref(), Some("pro"));
        assert!(!apply_usage_metadata(&mut account, Some("pro".to_string())));
        assert!(!apply_usage_metadata(&mut account, Some("".to_string())));
        assert_eq!(account.plan_type.as_deref(), Some("pro"));
    }

    fn chatgpt_account(
        name: &str,
        account_id: Option<&str>,
        refresh_token: &str,
        id_token: &str,
    ) -> StoredAccount {
        StoredAccount::new_chatgpt(NewChatGptAccount {
            name: name.to_string(),
            email: None,
            plan_type: None,
            chatgpt_user_id: None,
            chatgpt_account_is_fedramp: false,
            token_last_refresh_at: Utc::now(),
            subscription_expires_at: None,
            id_token: id_token.into(),
            access_token: "access-token".into(),
            refresh_token: refresh_token.into(),
            account_id: account_id.map(str::to_string),
        })
    }
}
