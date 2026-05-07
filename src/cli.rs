use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "codex-switch")]
#[command(about = "Local Codex account switcher")]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List stored accounts.
    List,
    /// Log in with ChatGPT/OpenAI device authorization and switch to the new account.
    Login {
        /// Account display name.
        name: String,
    },
    /// Import an existing Codex CLI auth.json.
    Import {
        /// Account display name.
        name: String,
        /// Path to auth.json. Defaults to the current Codex auth.json.
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Switch Codex to a stored account.
    Switch {
        /// Account name, full ID, or unique ID prefix.
        account: String,
    },
    /// Show usage for one account, the active account, or all accounts.
    Usage {
        /// Query every stored account.
        #[arg(long, conflicts_with = "account")]
        all: bool,
        /// Account name, full ID, or unique ID prefix. Defaults to active account.
        account: Option<String>,
    },
    /// Delete a stored account.
    Delete {
        /// Account name, full ID, or unique ID prefix.
        account: String,
    },
    /// Rename a stored account.
    Rename {
        /// Account name, full ID, or unique ID prefix.
        account: String,
        /// New account display name.
        new_name: String,
    },
}
