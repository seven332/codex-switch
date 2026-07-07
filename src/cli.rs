use std::path::PathBuf;

use clap::{Parser, Subcommand};

pub const DEFAULT_LOG_TAIL_LINES: usize = 100;

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
    /// Exclude a stored account from automatic account selection.
    Disable {
        /// Account name, full ID, or unique ID prefix.
        account: String,
    },
    /// Re-include a stored account in automatic account selection.
    Enable {
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
    /// Update the current codex-switch installation.
    Update {
        /// Only check whether an update is available.
        #[arg(long)]
        check: bool,
        /// Install a specific version, such as 0.1.10 or v0.1.10.
        #[arg(long)]
        version: Option<String>,
    },
    /// Diagnose local codex-switch and Codex configuration.
    Doctor {
        /// Codex executable to inspect.
        #[arg(long, default_value = "codex")]
        codex_bin: String,
    },
    /// Inspect runtime diagnostic logs.
    Logs {
        #[command(subcommand)]
        command: LogsCommand,
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
    /// Consume one earned ChatGPT rate-limit reset for an account.
    ResetUsage {
        /// Account name, full ID, or unique ID prefix. Defaults to the current Codex auth account.
        account: Option<String>,
        /// Skip the interactive confirmation prompt.
        #[arg(long)]
        yes: bool,
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

#[derive(Debug, Subcommand)]
pub enum LogsCommand {
    /// Print the runtime log directory path.
    Path,
    /// Print the latest runtime log file path.
    Latest,
    /// Print or follow the latest runtime log.
    Tail {
        /// Number of existing lines to print before following.
        #[arg(long, default_value_t = DEFAULT_LOG_TAIL_LINES)]
        lines: usize,
        /// Continue printing appended log output until interrupted.
        #[arg(long)]
        follow: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command, DEFAULT_LOG_TAIL_LINES, LogsCommand};
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

    #[test]
    fn reset_usage_defaults_to_confirmation() {
        let cli = Cli::try_parse_from(["codex-switch", "reset-usage", "work"])
            .expect("reset-usage should parse");

        let Command::ResetUsage { account, yes } = cli.command else {
            panic!("expected reset-usage command");
        };

        assert_eq!(account.as_deref(), Some("work"));
        assert!(!yes);
    }

    #[test]
    fn reset_usage_supports_yes_flag_without_account() {
        let cli = Cli::try_parse_from(["codex-switch", "reset-usage", "--yes"])
            .expect("reset-usage --yes should parse");

        let Command::ResetUsage { account, yes } = cli.command else {
            panic!("expected reset-usage command");
        };

        assert_eq!(account, None);
        assert!(yes);
    }

    #[test]
    fn doctor_defaults_to_codex_binary() {
        let cli = Cli::try_parse_from(["codex-switch", "doctor"])
            .expect("doctor should parse without options");

        let Command::Doctor { codex_bin } = cli.command else {
            panic!("expected doctor command");
        };
        assert_eq!(codex_bin, "codex");
    }

    #[test]
    fn doctor_accepts_codex_bin() {
        let cli = Cli::try_parse_from(["codex-switch", "doctor", "--codex-bin", "/opt/codex"])
            .expect("doctor --codex-bin should parse");

        let Command::Doctor { codex_bin } = cli.command else {
            panic!("expected doctor command");
        };
        assert_eq!(codex_bin, "/opt/codex");
    }

    #[test]
    fn logs_path_parses() {
        let cli =
            Cli::try_parse_from(["codex-switch", "logs", "path"]).expect("logs path should parse");

        let Command::Logs {
            command: LogsCommand::Path,
        } = cli.command
        else {
            panic!("expected logs path command");
        };
    }

    #[test]
    fn logs_latest_parses() {
        let cli = Cli::try_parse_from(["codex-switch", "logs", "latest"])
            .expect("logs latest should parse");

        let Command::Logs {
            command: LogsCommand::Latest,
        } = cli.command
        else {
            panic!("expected logs latest command");
        };
    }

    #[test]
    fn logs_tail_defaults_to_standard_line_count() {
        let cli =
            Cli::try_parse_from(["codex-switch", "logs", "tail"]).expect("logs tail should parse");

        let Command::Logs {
            command: LogsCommand::Tail { lines, follow },
        } = cli.command
        else {
            panic!("expected logs tail command");
        };

        assert_eq!(lines, DEFAULT_LOG_TAIL_LINES);
        assert!(!follow);
    }

    #[test]
    fn logs_tail_accepts_lines() {
        let cli = Cli::try_parse_from(["codex-switch", "logs", "tail", "--lines", "25"])
            .expect("logs tail --lines should parse");

        let Command::Logs {
            command: LogsCommand::Tail { lines, follow },
        } = cli.command
        else {
            panic!("expected logs tail command");
        };

        assert_eq!(lines, 25);
        assert!(!follow);
    }

    #[test]
    fn logs_tail_accepts_follow_and_lines() {
        let cli =
            Cli::try_parse_from(["codex-switch", "logs", "tail", "--follow", "--lines", "25"])
                .expect("logs tail --follow --lines should parse");

        let Command::Logs {
            command: LogsCommand::Tail { lines, follow },
        } = cli.command
        else {
            panic!("expected logs tail command");
        };

        assert_eq!(lines, 25);
        assert!(follow);
    }

    #[test]
    fn disable_accepts_account_selector() {
        let cli =
            Cli::try_parse_from(["codex-switch", "disable", "work"]).expect("disable should parse");

        let Command::Disable { account } = cli.command else {
            panic!("expected disable command");
        };
        assert_eq!(account, "work");
    }

    #[test]
    fn enable_accepts_account_selector() {
        let cli =
            Cli::try_parse_from(["codex-switch", "enable", "work"]).expect("enable should parse");

        let Command::Enable { account } = cli.command else {
            panic!("expected enable command");
        };
        assert_eq!(account, "work");
    }
}
