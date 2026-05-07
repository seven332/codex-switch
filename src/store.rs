use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::types::{AccountsStore, AuthData, StoredAccount};

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
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let temp_path = temp_file_path(path);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp_path)
            .with_context(|| format!("Failed to open private file: {}", temp_path.display()))?;
        file.write_all(content.as_bytes())
            .with_context(|| format!("Failed to write private file: {}", temp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("Failed to sync private file: {}", temp_path.display()))?;
        drop(file);

        fs::set_permissions(&temp_path, fs::Permissions::from_mode(0o600)).with_context(|| {
            format!(
                "Failed to set private file permissions: {}",
                temp_path.display()
            )
        })?;
        fs::rename(&temp_path, path).with_context(|| {
            format!(
                "Failed to replace private file {} with {}",
                path.display(),
                temp_path.display()
            )
        })?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("Failed to set file permissions: {}", path.display()))?;
    }

    #[cfg(not(unix))]
    {
        fs::write(path, content)
            .with_context(|| format!("Failed to write private file: {}", path.display()))?;
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
    pub id_token: Option<String>,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
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

pub fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}
