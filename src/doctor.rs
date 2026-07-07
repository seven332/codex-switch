use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use chrono::Utc;

use crate::auth_json;
use crate::codex_http;
use crate::process;
use crate::runtime_log;
use crate::store;
use crate::token;
use crate::types::{AccountsStore, AuthData, StoredAccount};

const CODEX_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(20);

pub(crate) struct DoctorOptions {
    pub(crate) codex_bin: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DoctorReport {
    checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    fn push(&mut self, check: DoctorCheck) {
        self.checks.push(check);
    }

    pub(crate) fn has_errors(&self) -> bool {
        self.checks
            .iter()
            .any(|check| check.severity == DoctorSeverity::Error)
    }

    pub(crate) fn format_human(&self) -> String {
        let mut output = String::new();
        for check in &self.checks {
            output.push_str(&format!(
                "[{}] {}: {}\n",
                check.severity.as_str(),
                check.category,
                check.message
            ));
            for hint in &check.hints {
                output.push_str(&format!("  hint: {hint}\n"));
            }
        }
        output
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DoctorCheck {
    severity: DoctorSeverity,
    category: &'static str,
    message: String,
    hints: Vec<String>,
}

impl DoctorCheck {
    fn ok(category: &'static str, message: impl Into<String>) -> Self {
        Self::new(DoctorSeverity::Ok, category, message)
    }

    fn warn(category: &'static str, message: impl Into<String>) -> Self {
        Self::new(DoctorSeverity::Warn, category, message)
    }

    fn error(category: &'static str, message: impl Into<String>) -> Self {
        Self::new(DoctorSeverity::Error, category, message)
    }

    fn new(severity: DoctorSeverity, category: &'static str, message: impl Into<String>) -> Self {
        Self {
            severity,
            category,
            message: message.into(),
            hints: Vec::new(),
        }
    }

    fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hints.push(hint.into());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DoctorSeverity {
    Ok,
    Warn,
    Error,
}

impl DoctorSeverity {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Copy)]
enum PermissionTarget {
    File,
    Directory,
}

pub(crate) fn run(options: DoctorOptions) -> DoctorReport {
    let mut report = DoctorReport::default();

    add_install_checks(&mut report);
    add_codex_checks(&mut report, &options.codex_bin);
    let store = add_store_checks(&mut report);
    add_auth_checks(&mut report, store.as_ref());
    add_process_checks(&mut report);
    add_log_checks(&mut report);

    report
}

fn add_install_checks(report: &mut DoctorReport) {
    match std::env::current_exe() {
        Ok(path) => report.push(DoctorCheck::ok(
            "install",
            format!(
                "codex-switch {} at {}",
                env!("CARGO_PKG_VERSION"),
                path.display()
            ),
        )),
        Err(err) => report.push(
            DoctorCheck::warn(
                "install",
                format!(
                    "codex-switch {} is running, but current executable path could not be read: {err}",
                    env!("CARGO_PKG_VERSION")
                ),
            )
            .with_hint("check PATH or reinstall codex-switch if multiple versions are installed"),
        ),
    }
}

fn add_codex_checks(report: &mut DoctorReport, codex_bin: &str) {
    let version_output =
        match run_command_with_timeout(codex_bin, &["--version"], CODEX_COMMAND_TIMEOUT) {
            Ok(CommandProbeResult::Completed(output)) => output,
            Ok(CommandProbeResult::TimedOut) => {
                report.push(
                    DoctorCheck::error(
                        "codex",
                        format!(
                            "{codex_bin} --version timed out after {}s",
                            CODEX_COMMAND_TIMEOUT.as_secs()
                        ),
                    )
                    .with_hint("verify the Codex executable path; wrappers should not block"),
                );
                return;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                report.push(
                    DoctorCheck::error("codex", format!("Codex executable not found: {codex_bin}"))
                        .with_hint("install Codex or pass --codex-bin <path>"),
                );
                return;
            }
            Err(err) => {
                report.push(
                    DoctorCheck::error(
                        "codex",
                        format!("failed to run {codex_bin} --version: {err}"),
                    )
                    .with_hint("verify the Codex executable path and permissions"),
                );
                return;
            }
        };

    if !version_output.status.success() {
        report.push(
            DoctorCheck::error(
                "codex",
                format!(
                    "{codex_bin} --version failed: {}",
                    command_output_excerpt(&version_output)
                ),
            )
            .with_hint("verify the Codex executable path and installation"),
        );
        return;
    }

    let version_text = command_output_text(&version_output);
    match codex_http::parse_codex_version_output(&version_text) {
        Some(version) => report.push(DoctorCheck::ok(
            "codex",
            format!("{codex_bin} reports Codex version {version}"),
        )),
        None => report.push(
            DoctorCheck::warn(
                "codex",
                format!("{codex_bin} --version succeeded, but the version could not be parsed"),
            )
            .with_hint("run codex --version manually to verify the installed Codex CLI"),
        ),
    }

    add_codex_remote_support_check(report, codex_bin);
}

fn add_codex_remote_support_check(report: &mut DoctorReport, codex_bin: &str) {
    let help_output = match run_command_with_timeout(codex_bin, &["--help"], CODEX_COMMAND_TIMEOUT)
    {
        Ok(CommandProbeResult::Completed(output)) => output,
        Ok(CommandProbeResult::TimedOut) => {
            report.push(
                DoctorCheck::warn(
                    "codex",
                    format!(
                        "{codex_bin} --help timed out after {}s during remote support check",
                        CODEX_COMMAND_TIMEOUT.as_secs()
                    ),
                )
                .with_hint("codex-switch run requires Codex --remote support"),
            );
            return;
        }
        Err(err) => {
            report.push(
                DoctorCheck::warn(
                    "codex",
                    format!("failed to run {codex_bin} --help for remote support check: {err}"),
                )
                .with_hint("codex-switch run requires Codex --remote support"),
            );
            return;
        }
    };

    if !help_output.status.success() {
        report.push(
            DoctorCheck::warn(
                "codex",
                format!(
                    "{codex_bin} --help failed during remote support check: {}",
                    command_output_excerpt(&help_output)
                ),
            )
            .with_hint("codex-switch run requires Codex --remote support"),
        );
        return;
    }

    let help_text = command_output_text(&help_output);
    if help_output_supports_remote(&help_text) {
        report.push(DoctorCheck::ok(
            "codex",
            "Codex help advertises --remote and --remote-auth-token-env",
        ));
    } else {
        report.push(
            DoctorCheck::error(
                "codex",
                "Codex help does not advertise the remote TUI flags required by codex-switch run",
            )
            .with_hint("upgrade Codex or pass --codex-bin <path> for a compatible Codex CLI"),
        );
    }
}

enum CommandProbeResult {
    Completed(Output),
    TimedOut,
}

fn run_command_with_timeout(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> std::io::Result<CommandProbeResult> {
    let mut child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + timeout;

    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output().map(CommandProbeResult::Completed);
        }

        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait_with_output();
            return Ok(CommandProbeResult::TimedOut);
        }

        std::thread::sleep(COMMAND_POLL_INTERVAL);
    }
}

fn add_store_checks(report: &mut DoctorReport) -> Option<AccountsStore> {
    let accounts_path = match store::accounts_file() {
        Ok(path) => path,
        Err(err) => {
            report.push(DoctorCheck::error(
                "store",
                format!("failed to resolve accounts.json path: {err:#}"),
            ));
            return None;
        }
    };

    if accounts_path.exists() {
        add_permission_check(
            report,
            "store",
            "accounts.json",
            &accounts_path,
            PermissionTarget::File,
        );
    } else {
        report.push(
            DoctorCheck::warn(
                "store",
                format!(
                    "accounts.json does not exist at {}",
                    accounts_path.display()
                ),
            )
            .with_hint("run codex-switch login <name> or codex-switch import <name>"),
        );
    }

    match store::load_accounts() {
        Ok(store) => {
            report.push(DoctorCheck::ok(
                "store",
                format!(
                    "{} loaded from {}",
                    account_store_summary(&store),
                    accounts_path.display()
                ),
            ));
            add_duplicate_account_checks(report, &store);
            Some(store)
        }
        Err(err) => {
            report.push(
                DoctorCheck::error(
                    "store",
                    format!(
                        "failed to read accounts.json at {}: {err:#}",
                        accounts_path.display()
                    ),
                )
                .with_hint("fix or restore ~/.codex-switch/accounts.json"),
            );
            None
        }
    }
}

fn add_auth_checks(report: &mut DoctorReport, store: Option<&AccountsStore>) {
    let auth_path = match auth_json::codex_auth_file() {
        Ok(path) => path,
        Err(err) => {
            report.push(DoctorCheck::error(
                "auth",
                format!("failed to resolve Codex auth.json path: {err:#}"),
            ));
            return;
        }
    };

    if auth_path.exists() {
        add_permission_check(
            report,
            "auth",
            "Codex auth.json",
            &auth_path,
            PermissionTarget::File,
        );
    }

    match auth_json::current_auth_account() {
        Ok(Some(current_auth)) => {
            report.push(DoctorCheck::ok(
                "auth",
                format!(
                    "Codex auth.json at {} uses {} auth",
                    auth_path.display(),
                    current_auth.auth_mode
                ),
            ));
            add_current_token_check(report, &current_auth);
            if let Some(store) = store {
                add_current_auth_match_check(report, store, &current_auth);
            }
        }
        Ok(None) => {
            report.push(
                DoctorCheck::warn(
                    "auth",
                    format!("Codex auth.json does not exist at {}", auth_path.display()),
                )
                .with_hint("run codex-switch switch <name-or-id> to write Codex auth.json"),
            );
        }
        Err(err) => {
            report.push(
                DoctorCheck::error(
                    "auth",
                    format!("failed to parse Codex auth.json at {}: {err:#}", auth_path.display()),
                )
                .with_hint("run codex-switch import <name> after fixing auth.json, or switch to a stored account"),
            );
        }
    }
}

fn add_current_token_check(report: &mut DoctorReport, account: &StoredAccount) {
    let AuthData::ChatGPT { access_token, .. } = &account.auth_data else {
        return;
    };

    match token::access_token_expires_at(access_token.expose_secret()) {
        Some(expires_at) if expires_at <= Utc::now() => report.push(
            DoctorCheck::warn(
                "auth",
                format!(
                    "current ChatGPT access token expired at {}",
                    expires_at.to_rfc3339()
                ),
            )
            .with_hint(
                "run codex-switch usage or codex-switch login <name> --replace if refresh fails",
            ),
        ),
        Some(expires_at) => report.push(DoctorCheck::ok(
            "auth",
            format!(
                "current ChatGPT access token expires at {}",
                expires_at.to_rfc3339()
            ),
        )),
        None => report.push(
            DoctorCheck::warn(
                "auth",
                "current ChatGPT access token expiry could not be parsed",
            )
            .with_hint("run codex-switch login <name> --replace if this account stops refreshing"),
        ),
    }
}

fn add_current_auth_match_check(
    report: &mut DoctorReport,
    store: &AccountsStore,
    current_auth: &StoredAccount,
) {
    match store::find_matching_account(store, current_auth) {
        Some(account) => report.push(DoctorCheck::ok(
            "auth",
            format!(
                "current auth.json matches stored account {} ({})",
                account.name,
                store::short_id(&account.id)
            ),
        )),
        None => report.push(
            DoctorCheck::warn("auth", "current auth.json does not match a stored account")
                .with_hint("run codex-switch import <name> or codex-switch switch <name-or-id>"),
        ),
    }
}

fn add_duplicate_account_checks(report: &mut DoctorReport, store: &AccountsStore) {
    for (left_index, left) in store.accounts.iter().enumerate() {
        for right in store.accounts.iter().skip(left_index + 1) {
            if left.name == right.name {
                report.push(
                    DoctorCheck::error(
                        "store",
                        format!(
                            "duplicate account name: {} ({}) and {} ({})",
                            left.name,
                            store::short_id(&left.id),
                            right.name,
                            store::short_id(&right.id)
                        ),
                    )
                    .with_hint("rename or delete one duplicate account"),
                );
            }

            if store::accounts_have_same_auth_identity(left, right) {
                report.push(
                    DoctorCheck::error(
                        "store",
                        format!(
                            "duplicate account auth identity: {} ({}) and {} ({})",
                            left.name,
                            store::short_id(&left.id),
                            right.name,
                            store::short_id(&right.id)
                        ),
                    )
                    .with_hint("delete or replace one duplicate account"),
                );
            }
        }
    }
}

fn add_process_checks(report: &mut DoctorReport) {
    match process::check_codex_processes() {
        Ok(info) if info.can_switch => {
            let extra = if info.background_count == 0 && info.managed_run_count == 0 {
                String::new()
            } else {
                format!(
                    " (ignored {} background, {} managed run)",
                    info.background_count, info.managed_run_count
                )
            };
            report.push(DoctorCheck::ok(
                "process",
                format!("no unmanaged Codex processes would block switching{extra}"),
            ));
        }
        Ok(info) => {
            let pids = info
                .pids
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            report.push(
                DoctorCheck::warn(
                    "process",
                    format!(
                        "{} unmanaged Codex process(es) would block codex-switch switch (pid: {pids})",
                        info.count
                    ),
                )
                .with_hint("close regular Codex sessions before running codex-switch switch"),
            );
        }
        Err(err) => report.push(
            DoctorCheck::warn(
                "process",
                format!("failed to inspect Codex processes: {err:#}"),
            )
            .with_hint("process detection may not work in this environment"),
        ),
    }
}

fn add_log_checks(report: &mut DoctorReport) {
    let log_dir = match runtime_log::runtime_log_dir_path() {
        Ok(path) => path,
        Err(err) => {
            report.push(DoctorCheck::warn(
                "logs",
                format!("failed to resolve runtime log directory: {err:#}"),
            ));
            return;
        }
    };

    if !log_dir.exists() {
        report.push(DoctorCheck::ok(
            "logs",
            format!(
                "runtime log directory has not been created at {}",
                log_dir.display()
            ),
        ));
        return;
    }

    add_permission_check(
        report,
        "logs",
        "runtime log directory",
        &log_dir,
        PermissionTarget::Directory,
    );

    match runtime_log::latest_runtime_log_path() {
        Ok(Some(path)) => {
            report.push(DoctorCheck::ok(
                "logs",
                format!("latest runtime log: {}", path.display()),
            ));
            add_permission_check(
                report,
                "logs",
                "latest runtime log",
                &path,
                PermissionTarget::File,
            );
        }
        Ok(None) => report.push(DoctorCheck::ok(
            "logs",
            format!("no runtime logs found in {}", log_dir.display()),
        )),
        Err(err) => report.push(
            DoctorCheck::warn("logs", format!("failed to inspect runtime logs: {err:#}"))
                .with_hint("check ~/.codex-switch/logs permissions"),
        ),
    }
}

fn add_permission_check(
    report: &mut DoctorReport,
    category: &'static str,
    label: &'static str,
    path: &Path,
    target: PermissionTarget,
) {
    match private_permission_check(label, path, target) {
        Ok(check) => report.push(check),
        Err(err) => report.push(DoctorCheck::warn(
            category,
            format!(
                "failed to inspect {label} permissions at {}: {err}",
                path.display()
            ),
        )),
    }
}

fn private_permission_check(
    label: &'static str,
    path: &Path,
    target: PermissionTarget,
) -> std::io::Result<DoctorCheck> {
    let metadata = fs::metadata(path)?;
    private_permission_check_from_metadata(label, path, target, &metadata)
}

#[cfg(unix)]
fn private_permission_check_from_metadata(
    label: &'static str,
    path: &Path,
    target: PermissionTarget,
    metadata: &fs::Metadata,
) -> std::io::Result<DoctorCheck> {
    use std::os::unix::fs::PermissionsExt;

    let mode = metadata.permissions().mode() & 0o777;
    let expected = match target {
        PermissionTarget::File => "0600",
        PermissionTarget::Directory => "0700",
    };
    if private_mode_is_safe(mode) {
        Ok(DoctorCheck::ok(
            "permissions",
            format!("{label} permissions are {mode:03o}"),
        ))
    } else {
        let chmod_mode = match target {
            PermissionTarget::File => "600",
            PermissionTarget::Directory => "700",
        };
        Ok(DoctorCheck::warn(
            "permissions",
            format!(
                "{label} permissions are {mode:03o}; expected {expected} or stricter at {}",
                path.display()
            ),
        )
        .with_hint(format!("run chmod {chmod_mode} {}", path.display())))
    }
}

#[cfg(not(unix))]
fn private_permission_check_from_metadata(
    label: &'static str,
    _path: &Path,
    _target: PermissionTarget,
    _metadata: &fs::Metadata,
) -> std::io::Result<DoctorCheck> {
    Ok(DoctorCheck::ok(
        "permissions",
        format!("{label} permission check is skipped on this platform"),
    ))
}

#[cfg(unix)]
fn private_mode_is_safe(mode: u32) -> bool {
    mode & 0o077 == 0
}

fn account_store_summary(store: &AccountsStore) -> String {
    let mut chatgpt = 0;
    let mut api_key = 0;
    for account in &store.accounts {
        match account.auth_data {
            AuthData::ChatGPT { .. } => chatgpt += 1,
            AuthData::ApiKey { .. } => api_key += 1,
        }
    }
    format!(
        "{} account(s): {chatgpt} ChatGPT OAuth, {api_key} API key",
        store.accounts.len()
    )
}

fn help_output_supports_remote(output: &str) -> bool {
    let has_remote = output.split_whitespace().any(|token| {
        let token =
            token.trim_matches(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | '=')));
        token == "--remote" || token.starts_with("--remote=")
    });
    has_remote && output.contains("--remote-auth-token-env")
}

fn command_output_text(output: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn command_output_excerpt(output: &Output) -> String {
    let text = command_output_text(output);
    let text = text.trim();
    if text.is_empty() {
        return format!("exit status {}", output.status);
    }
    let mut chars = text.chars();
    let excerpt = chars.by_ref().take(240).collect::<String>();
    if chars.next().is_some() {
        format!("{excerpt}...")
    } else {
        excerpt
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use uuid::Uuid;

    use super::*;
    use crate::types::{AuthMode, RedactedString};

    #[test]
    fn doctor_report_formats_hints_and_detects_errors() {
        let mut report = DoctorReport::default();
        report.push(DoctorCheck::ok("install", "ok message"));
        report.push(DoctorCheck::warn("auth", "warn message").with_hint("fix it"));
        assert!(!report.has_errors());

        report.push(DoctorCheck::error("store", "error message"));

        assert!(report.has_errors());
        assert_eq!(
            report.format_human(),
            "[ok] install: ok message\n[warn] auth: warn message\n  hint: fix it\n[error] store: error message\n"
        );
    }

    #[test]
    fn account_store_summary_counts_auth_modes() {
        let store = AccountsStore {
            accounts: vec![
                api_key_account("api", "sk-one"),
                chatgpt_account("oauth", "acct-1"),
            ],
            ..AccountsStore::default()
        };

        assert_eq!(
            account_store_summary(&store),
            "2 account(s): 1 ChatGPT OAuth, 1 API key"
        );
    }

    #[test]
    fn duplicate_account_identity_reports_without_secrets() {
        let mut report = DoctorReport::default();
        let store = AccountsStore {
            accounts: vec![
                api_key_account("first", "sk-secret-value"),
                api_key_account("second", "sk-secret-value"),
            ],
            ..AccountsStore::default()
        };

        add_duplicate_account_checks(&mut report, &store);

        let output = report.format_human();
        assert!(output.contains("duplicate account auth identity"));
        assert!(!output.contains("sk-secret-value"));
        assert!(report.has_errors());
    }

    #[test]
    fn duplicate_account_name_reports_error() {
        let mut report = DoctorReport::default();
        let store = AccountsStore {
            accounts: vec![
                api_key_account("same", "sk-one"),
                api_key_account("same", "sk-two"),
            ],
            ..AccountsStore::default()
        };

        add_duplicate_account_checks(&mut report, &store);

        assert!(report.format_human().contains("duplicate account name"));
        assert!(report.has_errors());
    }

    #[test]
    fn current_auth_match_reports_stored_account() {
        let mut report = DoctorReport::default();
        let account = api_key_account("personal", "sk-match");
        let current_auth = api_key_account("current", "sk-match");
        let store = AccountsStore {
            accounts: vec![account.clone()],
            ..AccountsStore::default()
        };

        add_current_auth_match_check(&mut report, &store, &current_auth);

        assert_eq!(
            report.format_human(),
            format!(
                "[ok] auth: current auth.json matches stored account personal ({})\n",
                store::short_id(&account.id)
            )
        );
    }

    #[test]
    fn remote_help_support_requires_both_flags() {
        assert!(help_output_supports_remote(
            "Usage: codex --remote <ADDR> --remote-auth-token-env <ENV>"
        ));
        assert!(help_output_supports_remote(
            "Usage: codex [--remote=<ADDR>] [--remote-auth-token-env <ENV>]"
        ));
        assert!(!help_output_supports_remote(
            "Usage: codex --remote-auth-token-env <ENV>"
        ));
        assert!(!help_output_supports_remote("Usage: codex --remote <ADDR>"));
        assert!(!help_output_supports_remote("Usage: codex"));
    }

    #[cfg(unix)]
    #[test]
    fn private_mode_rejects_group_or_other_permissions() {
        assert!(private_mode_is_safe(0o600));
        assert!(private_mode_is_safe(0o700));
        assert!(!private_mode_is_safe(0o640));
        assert!(!private_mode_is_safe(0o604));
    }

    #[cfg(unix)]
    #[test]
    fn permission_check_warns_for_public_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("codex-switch-doctor-{}", Uuid::new_v4()));
        fs::create_dir(&dir).expect("test dir should be created");
        let path = dir.join("accounts.json");
        fs::write(&path, "{}").expect("test file should be written");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))
            .expect("test permissions should be set");

        let check = private_permission_check("accounts.json", &path, PermissionTarget::File)
            .expect("permission check should succeed");

        assert_eq!(check.severity, DoctorSeverity::Warn);
        assert!(check.message.contains("0644") || check.message.contains("644"));
        fs::remove_dir_all(&dir).expect("test dir should be removed");
    }

    fn api_key_account(name: &str, key: &str) -> StoredAccount {
        let mut account = StoredAccount::new_api_key(name.to_string(), key.to_string());
        account.id = Uuid::new_v4().to_string();
        account.created_at = Utc::now();
        account
    }

    fn chatgpt_account(name: &str, account_id: &str) -> StoredAccount {
        let now = Utc::now();
        StoredAccount {
            id: Uuid::new_v4().to_string(),
            name: name.to_string(),
            email: Some(format!("{name}@example.com")),
            plan_type: Some("pro".to_string()),
            chatgpt_user_id: Some(format!("user-{name}")),
            chatgpt_account_is_fedramp: false,
            token_last_refresh_at: Some(now),
            subscription_expires_at: None,
            auth_mode: AuthMode::ChatGPT,
            auth_data: AuthData::ChatGPT {
                id_token: RedactedString::new("id-token"),
                access_token: RedactedString::new("access-token"),
                refresh_token: RedactedString::new("refresh-token"),
                account_id: Some(account_id.to_string()),
            },
            auto_switch_disabled: false,
            created_at: now,
            last_used_at: None,
        }
    }
}
