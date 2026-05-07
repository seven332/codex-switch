use anyhow::{Context, Result};

use crate::auth_json;
use crate::process;
use crate::store;
use crate::token;
use crate::types::StoredAccount;

pub async fn switch_to_account(selector: &str) -> Result<StoredAccount> {
    let store_snapshot = store::load_accounts()?;
    let account_id = store::resolve_account_id(&store_snapshot, selector)?;
    let account = store_snapshot
        .accounts
        .into_iter()
        .find(|account| account.id == account_id)
        .context("Account not found after resolving selector")?;

    process::ensure_can_switch()?;
    let account = token::ensure_chatgpt_tokens_fresh(&account).await?;
    auth_json::write_account_auth(&account)?;
    store::set_active_account(&account.id)?;
    store::touch_account(&account.id)?;

    store::get_account_by_selector(&account.id)
}
