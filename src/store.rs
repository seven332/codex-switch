use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::store_lock::acquire_accounts_file_lock;
use crate::types::{AccountsStore, AuthData, RedactedString, StoredAccount};

enum StoreUpdate<T> {
    Changed(T),
    Unchanged(T),
}

pub fn config_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not find home directory")?;
    Ok(home.join(".codex-switch"))
}

pub fn accounts_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("accounts.json"))
}

fn accounts_lock_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("accounts.lock"))
}

pub fn load_accounts() -> Result<AccountsStore> {
    let path = accounts_file()?;
    load_accounts_from_path(&path)
}

fn load_accounts_from_path(path: &Path) -> Result<AccountsStore> {
    if !path.exists() {
        return Ok(AccountsStore::default());
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read accounts file: {}", path.display()))?;
    let store = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse accounts file: {}", path.display()))?;

    Ok(store)
}

fn save_accounts_to_path(path: &Path, store: &AccountsStore) -> Result<()> {
    let mut content =
        serde_json::to_string_pretty(store).context("Failed to serialize accounts store")?;
    content.push('\n');
    write_private_file(path, &content)
}

fn mutate_accounts<T>(
    mutation: impl FnOnce(&mut AccountsStore) -> Result<StoreUpdate<T>>,
) -> Result<T> {
    let path = accounts_file()?;
    let lock_path = accounts_lock_file()?;
    mutate_accounts_at(&path, &lock_path, mutation)
}

fn mutate_accounts_at<T>(
    path: &Path,
    lock_path: &Path,
    mutation: impl FnOnce(&mut AccountsStore) -> Result<StoreUpdate<T>>,
) -> Result<T> {
    let _lock = acquire_accounts_file_lock(lock_path)?;
    let mut store = load_accounts_from_path(path)?;
    match mutation(&mut store)? {
        StoreUpdate::Changed(value) => {
            save_accounts_to_path(path, &store)?;
            Ok(value)
        }
        StoreUpdate::Unchanged(value) => Ok(value),
    }
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

pub fn ensure_chatgpt_replacement_target(name: &str) -> Result<()> {
    let store = load_accounts()?;
    chatgpt_replacement_target_index(&store, name)?;
    Ok(())
}

pub fn add_account(account: StoredAccount) -> Result<StoredAccount> {
    mutate_accounts(|store| add_account_to_store(store, account))
}

pub fn replace_chatgpt_account_by_name(
    name: &str,
    account: StoredAccount,
) -> Result<StoredAccount> {
    mutate_accounts(|store| replace_chatgpt_account_by_name_in_store(store, name, account))
}

pub fn find_matching_account<'a>(
    store: &'a AccountsStore,
    account: &StoredAccount,
) -> Option<&'a StoredAccount> {
    store
        .accounts
        .iter()
        .find(|existing| accounts_have_same_auth_identity(existing, account))
}

pub(crate) fn accounts_have_same_auth_identity(
    left: &StoredAccount,
    right: &StoredAccount,
) -> bool {
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

pub fn touch_account(account_id: &str) -> Result<()> {
    mutate_accounts(|store| touch_account_in_store(store, account_id))
}

pub fn remove_account_by_selector(selector: &str) -> Result<StoredAccount> {
    mutate_accounts(|store| remove_account_by_selector_from_store(store, selector))
}

pub fn rename_account_by_selector(selector: &str, new_name: String) -> Result<StoredAccount> {
    mutate_accounts(|store| rename_account_by_selector_in_store(store, selector, new_name))
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
    mutate_accounts(|store| update_account_chatgpt_tokens_in_store(store, account_id, update))
}

fn add_account_to_store(
    store: &mut AccountsStore,
    account: StoredAccount,
) -> Result<StoreUpdate<StoredAccount>> {
    if store
        .accounts
        .iter()
        .any(|existing| existing.name == account.name)
    {
        anyhow::bail!("An account named '{}' already exists", account.name);
    }

    if let Some(existing) = store
        .accounts
        .iter()
        .find(|existing| accounts_have_same_auth_identity(existing, &account))
    {
        anyhow::bail!(
            "account is already stored as {} ({})",
            existing.name,
            short_id(&existing.id)
        );
    }

    let stored = account.clone();
    store.accounts.push(account);
    Ok(StoreUpdate::Changed(stored))
}

fn replace_chatgpt_account_by_name_in_store(
    store: &mut AccountsStore,
    name: &str,
    account: StoredAccount,
) -> Result<StoreUpdate<StoredAccount>> {
    let index = chatgpt_replacement_target_index(store, name)?;
    ensure_chatgpt_replacement_account(&account)?;

    if let Some(existing) =
        store
            .accounts
            .iter()
            .enumerate()
            .find_map(|(existing_index, existing)| {
                (existing_index != index && accounts_have_same_auth_identity(existing, &account))
                    .then_some(existing)
            })
    {
        anyhow::bail!(
            "account is already stored as {} ({})",
            existing.name,
            short_id(&existing.id)
        );
    }

    let existing = &store.accounts[index];
    let mut replacement = account;
    replacement.id = existing.id.clone();
    replacement.name = existing.name.clone();
    replacement.created_at = existing.created_at;
    replacement.last_used_at = existing.last_used_at;

    let stored = replacement.clone();
    store.accounts[index] = replacement;
    Ok(StoreUpdate::Changed(stored))
}

fn ensure_chatgpt_replacement_account(account: &StoredAccount) -> Result<()> {
    match &account.auth_data {
        AuthData::ChatGPT { .. } => Ok(()),
        AuthData::ApiKey { .. } => {
            anyhow::bail!("Replacement login must be a ChatGPT OAuth account")
        }
    }
}

fn chatgpt_replacement_target_index(store: &AccountsStore, name: &str) -> Result<usize> {
    let (index, account) = store
        .accounts
        .iter()
        .enumerate()
        .find(|(_, existing)| existing.name == name)
        .with_context(|| format!("No account named '{name}' exists to replace"))?;

    match &account.auth_data {
        AuthData::ChatGPT { .. } => Ok(index),
        AuthData::ApiKey { .. } => {
            anyhow::bail!("Account '{name}' is an API key account and cannot be replaced by login")
        }
    }
}

fn touch_account_in_store(store: &mut AccountsStore, account_id: &str) -> Result<StoreUpdate<()>> {
    if let Some(account) = store
        .accounts
        .iter_mut()
        .find(|account| account.id == account_id)
    {
        account.last_used_at = Some(Utc::now());
        return Ok(StoreUpdate::Changed(()));
    }

    Ok(StoreUpdate::Unchanged(()))
}

fn remove_account_by_selector_from_store(
    store: &mut AccountsStore,
    selector: &str,
) -> Result<StoreUpdate<StoredAccount>> {
    let account_id = resolve_account_id(store, selector)?;
    let index = store
        .accounts
        .iter()
        .position(|account| account.id == account_id)
        .context("Account not found after resolving selector")?;
    Ok(StoreUpdate::Changed(store.accounts.remove(index)))
}

fn rename_account_by_selector_in_store(
    store: &mut AccountsStore,
    selector: &str,
    new_name: String,
) -> Result<StoreUpdate<StoredAccount>> {
    let account_id = resolve_account_id(store, selector)?;

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
    Ok(StoreUpdate::Changed(account.clone()))
}

fn update_account_chatgpt_tokens_in_store(
    store: &mut AccountsStore,
    account_id: &str,
    update: ChatGptTokenUpdate,
) -> Result<StoreUpdate<StoredAccount>> {
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
    Ok(StoreUpdate::Changed(updated))
}

pub fn update_account_usage_metadata(
    account_id: &str,
    plan_type: Option<String>,
) -> Result<Option<StoredAccount>> {
    mutate_accounts(|store| update_account_usage_metadata_in_store(store, account_id, plan_type))
}

fn update_account_usage_metadata_in_store(
    store: &mut AccountsStore,
    account_id: &str,
    plan_type: Option<String>,
) -> Result<StoreUpdate<Option<StoredAccount>>> {
    let Some(account) = store
        .accounts
        .iter_mut()
        .find(|account| account.id == account_id)
    else {
        return Ok(StoreUpdate::Unchanged(None));
    };

    if !apply_usage_metadata(account, plan_type) {
        return Ok(StoreUpdate::Unchanged(Some(account.clone())));
    }

    let updated = account.clone();
    Ok(StoreUpdate::Changed(Some(updated)))
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
    use super::*;
    use crate::types::{NewChatGptAccount, StoredAccount};

    #[test]
    fn duplicate_auth_matches_api_key() {
        let left = StoredAccount::new_api_key("left".to_string(), "sk-test".to_string());
        let right = StoredAccount::new_api_key("right".to_string(), "sk-test".to_string());

        assert!(accounts_have_same_auth_identity(&left, &right));
    }

    #[test]
    fn duplicate_auth_matches_chatgpt_account_id() {
        let left = chatgpt_account("left", Some("account-id"), "refresh-left", "id-left");
        let right = chatgpt_account("right", Some("account-id"), "refresh-right", "id-right");

        assert!(accounts_have_same_auth_identity(&left, &right));
    }

    #[test]
    fn duplicate_auth_matches_chatgpt_refresh_token() {
        let left = chatgpt_account("left", None, "refresh-token", "id-left");
        let right = chatgpt_account("right", None, "refresh-token", "id-right");

        assert!(accounts_have_same_auth_identity(&left, &right));
    }

    #[test]
    fn duplicate_auth_rejects_different_chatgpt_accounts() {
        let left = chatgpt_account("left", Some("left-account"), "left-refresh", "left-id");
        let right = chatgpt_account("right", Some("right-account"), "right-refresh", "right-id");

        assert!(!accounts_have_same_auth_identity(&left, &right));
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

    #[test]
    fn transaction_waits_for_lock_and_reloads_latest_store() {
        let (dir, path, lock_path) = temp_store_paths("delayed-usage");
        let account = chatgpt_account("account", Some("account-id"), "refresh", "id");
        let account_id = account.id.clone();
        save_accounts_to_path(&path, &store_with_accounts(vec![account]))
            .expect("initial store should save");

        let lock = acquire_accounts_file_lock(&lock_path).expect("test should acquire lock");
        let worker_path = path.clone();
        let worker_lock_path = lock_path.clone();
        let worker_account_id = account_id.clone();
        let worker = std::thread::spawn(move || {
            mutate_accounts_at(&worker_path, &worker_lock_path, |store| {
                update_account_usage_metadata_in_store(
                    store,
                    &worker_account_id,
                    Some("pro".to_string()),
                )
            })
        });

        let mut latest_store = load_accounts_from_path(&path).expect("latest store should load");
        latest_store.accounts[0].name = "renamed".to_string();
        save_accounts_to_path(&path, &latest_store).expect("latest store should save");
        drop(lock);

        worker
            .join()
            .expect("worker should not panic")
            .expect("usage update should save");

        let final_store = load_accounts_from_path(&path).expect("final store should load");
        let final_account = final_store.accounts.first().expect("account should remain");
        assert_eq!(final_account.name, "renamed");
        assert_eq!(final_account.plan_type.as_deref(), Some("pro"));

        fs::remove_dir_all(dir).expect("temp store should be removed");
    }

    #[test]
    fn transaction_reloads_latest_store_for_sequential_usage_update() {
        let (dir, path, lock_path) = temp_store_paths("sequential-usage");
        let account = chatgpt_account("account", Some("account-id"), "refresh", "id");
        let account_id = account.id.clone();
        save_accounts_to_path(&path, &store_with_accounts(vec![account]))
            .expect("initial store should save");

        mutate_accounts_at(&path, &lock_path, |store| {
            rename_account_by_selector_in_store(store, &account_id, "renamed".to_string())
        })
        .expect("rename should save");

        mutate_accounts_at(&path, &lock_path, |store| {
            update_account_usage_metadata_in_store(store, &account_id, Some("pro".to_string()))
        })
        .expect("usage update should save");

        let final_store = load_accounts_from_path(&path).expect("final store should load");
        let final_account = final_store.accounts.first().expect("account should remain");
        assert_eq!(final_account.name, "renamed");
        assert_eq!(final_account.plan_type.as_deref(), Some("pro"));

        fs::remove_dir_all(dir).expect("temp store should be removed");
    }

    #[test]
    fn deleted_account_is_not_resurrected_by_delayed_token_or_usage_update() {
        let (dir, path, lock_path) = temp_store_paths("deleted-account");
        let account = chatgpt_account("account", Some("account-id"), "refresh", "id");
        let account_id = account.id.clone();
        save_accounts_to_path(&path, &store_with_accounts(vec![account]))
            .expect("initial store should save");

        mutate_accounts_at(&path, &lock_path, |store| {
            remove_account_by_selector_from_store(store, &account_id)
        })
        .expect("delete should save");

        let token_result = mutate_accounts_at(&path, &lock_path, |store| {
            update_account_chatgpt_tokens_in_store(store, &account_id, token_update())
        });
        assert!(token_result.is_err());

        let usage_result = mutate_accounts_at(&path, &lock_path, |store| {
            update_account_usage_metadata_in_store(store, &account_id, Some("pro".to_string()))
        })
        .expect("usage metadata sync should ignore missing account");
        assert!(usage_result.is_none());

        let final_store = load_accounts_from_path(&path).expect("final store should load");
        assert!(final_store.accounts.is_empty());

        fs::remove_dir_all(dir).expect("temp store should be removed");
    }

    #[test]
    fn add_account_checks_duplicate_auth_inside_transaction() {
        let (dir, path, lock_path) = temp_store_paths("duplicate-add");
        let account = StoredAccount::new_api_key("first".to_string(), "sk-test".to_string());
        save_accounts_to_path(&path, &store_with_accounts(vec![account]))
            .expect("initial store should save");

        let duplicate = StoredAccount::new_api_key("second".to_string(), "sk-test".to_string());
        let err = mutate_accounts_at(&path, &lock_path, |store| {
            add_account_to_store(store, duplicate)
        })
        .expect_err("duplicate auth should be rejected inside the transaction");

        assert!(err.to_string().contains("account is already stored as"));
        let final_store = load_accounts_from_path(&path).expect("final store should load");
        assert_eq!(final_store.accounts.len(), 1);

        fs::remove_dir_all(dir).expect("temp store should be removed");
    }

    #[test]
    fn replace_account_preserves_local_identity_and_updates_login() {
        let mut account = chatgpt_account("account", Some("old-account"), "old-refresh", "old-id");
        account.email = Some("old@example.com".to_string());
        account.plan_type = Some("free".to_string());
        account.created_at = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .expect("timestamp should parse")
            .with_timezone(&Utc);
        account.last_used_at = Some(
            DateTime::parse_from_rfc3339("2026-01-02T00:00:00Z")
                .expect("timestamp should parse")
                .with_timezone(&Utc),
        );
        let account_id = account.id.clone();
        let created_at = account.created_at;
        let last_used_at = account.last_used_at;
        let mut store = store_with_accounts(vec![account]);

        let mut replacement =
            chatgpt_account("replacement", Some("new-account"), "new-refresh", "new-id");
        replacement.email = Some("new@example.com".to_string());
        replacement.plan_type = Some("pro".to_string());

        let stored =
            match replace_chatgpt_account_by_name_in_store(&mut store, "account", replacement)
                .expect("replace should succeed")
            {
                StoreUpdate::Changed(stored) => stored,
                StoreUpdate::Unchanged(_) => panic!("replace should save the store"),
            };

        assert_eq!(stored.id, account_id);
        assert_eq!(stored.name, "account");
        assert_eq!(stored.created_at, created_at);
        assert_eq!(stored.last_used_at, last_used_at);
        assert_eq!(stored.email.as_deref(), Some("new@example.com"));
        assert_eq!(stored.plan_type.as_deref(), Some("pro"));
        assert_eq!(store.accounts.len(), 1);

        match &stored.auth_data {
            AuthData::ChatGPT {
                account_id,
                refresh_token,
                id_token,
                ..
            } => {
                assert_eq!(account_id.as_deref(), Some("new-account"));
                assert_eq!(refresh_token.expose_secret(), "new-refresh");
                assert_eq!(id_token.expose_secret(), "new-id");
            }
            AuthData::ApiKey { .. } => panic!("replacement should remain ChatGPT auth"),
        }
    }

    #[test]
    fn replace_account_rejects_missing_name() {
        let mut store = AccountsStore::default();
        let replacement =
            chatgpt_account("replacement", Some("new-account"), "new-refresh", "new-id");

        let err = match replace_chatgpt_account_by_name_in_store(&mut store, "missing", replacement)
        {
            Ok(_) => panic!("missing replacement target should be rejected"),
            Err(err) => err,
        };

        assert!(err.to_string().contains("No account named 'missing'"));
    }

    #[test]
    fn replace_account_rejects_api_key_target() {
        let target = StoredAccount::new_api_key("target".to_string(), "sk-test".to_string());
        let mut store = store_with_accounts(vec![target]);
        let replacement =
            chatgpt_account("replacement", Some("new-account"), "new-refresh", "new-id");

        let err = match replace_chatgpt_account_by_name_in_store(&mut store, "target", replacement)
        {
            Ok(_) => panic!("API key target should be rejected"),
            Err(err) => err,
        };

        assert!(
            err.to_string()
                .contains("API key account and cannot be replaced by login")
        );
        assert_eq!(store.accounts[0].auth_mode, crate::types::AuthMode::ApiKey);
        match &store.accounts[0].auth_data {
            AuthData::ApiKey { key } => assert_eq!(key.expose_secret(), "sk-test"),
            AuthData::ChatGPT { .. } => panic!("target should remain API key auth"),
        }
    }

    #[test]
    fn replace_account_rejects_api_key_replacement() {
        let target = chatgpt_account(
            "target",
            Some("target-account"),
            "target-refresh",
            "target-id",
        );
        let mut store = store_with_accounts(vec![target]);
        let replacement =
            StoredAccount::new_api_key("replacement".to_string(), "sk-test".to_string());

        let err = match replace_chatgpt_account_by_name_in_store(&mut store, "target", replacement)
        {
            Ok(_) => panic!("API key replacement should be rejected"),
            Err(err) => err,
        };

        assert!(
            err.to_string()
                .contains("Replacement login must be a ChatGPT OAuth account")
        );
        match &store.accounts[0].auth_data {
            AuthData::ChatGPT { account_id, .. } => {
                assert_eq!(account_id.as_deref(), Some("target-account"));
            }
            AuthData::ApiKey { .. } => panic!("target should remain ChatGPT auth"),
        }
    }

    #[test]
    fn replace_account_rejects_duplicate_auth_identity_from_another_account() {
        let target = chatgpt_account(
            "target",
            Some("target-account"),
            "target-refresh",
            "target-id",
        );
        let other = chatgpt_account("other", Some("other-account"), "other-refresh", "other-id");
        let mut store = store_with_accounts(vec![target, other]);
        let replacement = chatgpt_account(
            "replacement",
            Some("other-account"),
            "new-refresh",
            "new-id",
        );

        let err = match replace_chatgpt_account_by_name_in_store(&mut store, "target", replacement)
        {
            Ok(_) => panic!("duplicate auth identity should be rejected"),
            Err(err) => err,
        };

        assert!(
            err.to_string()
                .contains("account is already stored as other")
        );
        assert_eq!(store.accounts[0].name, "target");
        match &store.accounts[0].auth_data {
            AuthData::ChatGPT { account_id, .. } => {
                assert_eq!(account_id.as_deref(), Some("target-account"));
            }
            AuthData::ApiKey { .. } => panic!("target should remain ChatGPT auth"),
        }
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

    fn token_update() -> ChatGptTokenUpdate {
        ChatGptTokenUpdate {
            id_token: Some("new-id-token".into()),
            access_token: Some("new-access-token".into()),
            refresh_token: Some("new-refresh-token".into()),
            chatgpt_account_id: Some("new-account-id".to_string()),
            email: Some("user@example.com".to_string()),
            plan_type: Some("pro".to_string()),
            chatgpt_user_id: Some("user-id".to_string()),
            chatgpt_account_is_fedramp: Some(false),
            token_last_refresh_at: Utc::now(),
            subscription_expires_at: None,
        }
    }

    fn store_with_accounts(accounts: Vec<StoredAccount>) -> AccountsStore {
        AccountsStore {
            version: 1,
            accounts,
            masked_account_ids: Vec::new(),
        }
    }

    fn temp_store_paths(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("codex-switch-store-{name}-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("temp store dir should be created");
        let accounts_path = dir.join("accounts.json");
        let lock_path = dir.join("accounts.lock");
        (dir, accounts_path, lock_path)
    }
}
