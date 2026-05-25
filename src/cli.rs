use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "codex-switch")]
#[command(about = "Multi-account runtime switcher for Codex")]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List stored accounts.
    List,
    /// Log in with ChatGPT/OpenAI OAuth and save the account.
    Login {
        /// Account display name.
        name: String,
        /// Replace an existing ChatGPT OAuth account with the same name after login succeeds.
        #[arg(long)]
        replace: bool,
        /// Use device authorization instead of browser OAuth.
        #[arg(long = "device-auth")]
        device_auth: bool,
    },
    /// Import an existing Codex CLI auth.json.
    Import {
        /// Account display name.
        name: String,
        /// Path to auth.json. Defaults to the current Codex auth.json.
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Export a stored account as Codex CLI auth.json.
    Export {
        /// Account name, full ID, or unique ID prefix.
        account: String,
        /// Write auth.json to this file instead of stdout.
        #[arg(long)]
        file: Option<PathBuf>,
        /// Overwrite the output file if it already exists.
        #[arg(long, requires = "file")]
        force: bool,
    },
    /// Switch Codex to a stored account.
    Switch {
        /// Account name, full ID, or unique ID prefix.
        account: String,
    },
    /// Switch to a usable account when the current Codex auth account is out of usage.
    AutoSwitch,
    /// Run Codex with runtime account auto-switching.
    Run {
        /// Codex executable to launch.
        #[arg(long, default_value = "codex")]
        codex_bin: String,
        /// Arguments forwarded to `codex`. Must be passed after `--`.
        #[arg(value_name = "CODEX_ARGS", num_args = 0.., last = true, allow_hyphen_values = true)]
        codex_args: Vec<String>,
    },
    /// Update codex-switch from GitHub Releases.
    Update {
        /// Only check whether an update is available.
        #[arg(long)]
        check: bool,
        /// Install a specific release version, such as 0.1.10 or v0.1.10.
        #[arg(long)]
        version: Option<String>,
    },
    /// Show usage for one account, the current Codex auth account, or all accounts.
    Usage {
        /// Query every stored account.
        #[arg(long, conflicts_with = "account")]
        all: bool,
        /// Include additional usage limits.
        #[arg(long = "show-additional")]
        show_additional: bool,
        /// Account name, full ID, or unique ID prefix. Defaults to the current Codex auth account.
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

#[cfg(test)]
mod tests {
    use super::{Cli, Command};
    use clap::Parser;

    #[test]
    fn run_args_require_double_dash_separator() {
        let err = Cli::try_parse_from(["codex-switch", "run", "resume"])
            .expect_err("run arguments should require --");

        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn run_without_forwarded_args_is_allowed() {
        let cli =
            Cli::try_parse_from(["codex-switch", "run"]).expect("run without args should parse");

        let Command::Run { codex_args, .. } = cli.command else {
            panic!("expected run command");
        };

        assert!(codex_args.is_empty());
    }

    #[test]
    fn run_args_after_double_dash_are_forwarded() {
        let cli = Cli::try_parse_from([
            "codex-switch",
            "run",
            "--codex-bin",
            "/usr/local/bin/codex",
            "--",
            "resume",
            "--last",
        ])
        .expect("run arguments after -- should parse");

        let Command::Run {
            codex_bin,
            codex_args,
        } = cli.command
        else {
            panic!("expected run command");
        };

        assert_eq!(codex_bin, "/usr/local/bin/codex");
        assert_eq!(codex_args, ["resume", "--last"]);
    }

    #[test]
    fn run_args_after_double_dash_may_start_with_hyphen() {
        let cli = Cli::try_parse_from(["codex-switch", "run", "--", "--model", "gpt-5"])
            .expect("hyphen-prefixed run arguments after -- should parse");

        let Command::Run { codex_args, .. } = cli.command else {
            panic!("expected run command");
        };

        assert_eq!(codex_args, ["--model", "gpt-5"]);
    }
}
