use std::process::Command;
use std::sync::OnceLock;

use anyhow::Result;
use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};

pub const CODEX_OAUTH_ORIGINATOR: &str = "codex_cli_rs";
pub const CODEX_APP_SERVER_DAEMON_CLIENT_NAME: &str = "codex_app_server_daemon";

const CODEX_INTERNAL_ORIGINATOR_OVERRIDE_ENV_VAR: &str = "CODEX_INTERNAL_ORIGINATOR_OVERRIDE";
const DEFAULT_CODEX_BIN: &str = "codex";

static CODEX_BIN: OnceLock<String> = OnceLock::new();
static DETECTED_CODEX_VERSION: OnceLock<Option<String>> = OnceLock::new();
static CODEX_VERSION: OnceLock<String> = OnceLock::new();
static TERMINAL_INFO: OnceLock<TerminalInfo> = OnceLock::new();

#[derive(Debug, Clone)]
struct Originator {
    value: String,
    header_value: HeaderValue,
}

pub fn set_codex_bin_for_user_agent(codex_bin: impl Into<String>) {
    let codex_bin = codex_bin.into();
    if !codex_bin.trim().is_empty() {
        let _ = CODEX_BIN.set(codex_bin);
    }
}

pub fn codex_version() -> String {
    CODEX_VERSION.get_or_init(detect_codex_version).clone()
}

pub fn detected_codex_version() -> Option<String> {
    DETECTED_CODEX_VERSION
        .get_or_init(detect_configured_codex_version)
        .clone()
}

pub fn originator_value() -> String {
    originator().value
}

pub fn codex_default_headers() -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    insert_codex_default_headers(&mut headers)?;
    Ok(headers)
}

pub fn insert_codex_default_headers(headers: &mut HeaderMap) -> Result<()> {
    let originator = originator();
    headers.insert("originator", originator.header_value);
    insert_codex_user_agent_header(headers)
}

pub fn insert_codex_user_agent_header(headers: &mut HeaderMap) -> Result<()> {
    if let Ok(user_agent) = HeaderValue::from_str(&get_codex_user_agent()) {
        headers.insert(USER_AGENT, user_agent);
    }
    Ok(())
}

fn originator() -> Originator {
    let value = std::env::var(CODEX_INTERNAL_ORIGINATOR_OVERRIDE_ENV_VAR)
        .unwrap_or_else(|_| CODEX_OAUTH_ORIGINATOR.to_string());

    match HeaderValue::from_str(&value) {
        Ok(header_value) => Originator {
            value,
            header_value,
        },
        Err(_) => Originator {
            value: CODEX_OAUTH_ORIGINATOR.to_string(),
            header_value: HeaderValue::from_static(CODEX_OAUTH_ORIGINATOR),
        },
    }
}

fn get_codex_user_agent() -> String {
    let version = codex_version();
    let os_info = os_info::get();
    let originator = originator();
    let prefix = format!(
        "{}/{} ({} {}; {}) {}",
        originator.value.as_str(),
        version,
        os_info.os_type(),
        os_info.version(),
        os_info.architecture().unwrap_or("unknown"),
        terminal_user_agent_token()
    );

    sanitize_user_agent(prefix.clone(), &prefix)
}

fn sanitize_user_agent(candidate: String, fallback: &str) -> String {
    if HeaderValue::from_str(candidate.as_str()).is_ok() {
        return candidate;
    }

    let sanitized: String = candidate
        .chars()
        .map(|ch| if matches!(ch, ' '..='~') { ch } else { '_' })
        .collect();
    if !sanitized.is_empty() && HeaderValue::from_str(sanitized.as_str()).is_ok() {
        sanitized
    } else if HeaderValue::from_str(fallback).is_ok() {
        fallback.to_string()
    } else {
        originator().value
    }
}

fn detect_codex_version() -> String {
    detected_codex_version().unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
}

fn detect_configured_codex_version() -> Option<String> {
    let codex_bin = CODEX_BIN
        .get()
        .map(String::as_str)
        .unwrap_or(DEFAULT_CODEX_BIN);

    detect_codex_version_from_bin(codex_bin)
}

fn detect_codex_version_from_bin(codex_bin: &str) -> Option<String> {
    let output = Command::new(codex_bin).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    parse_codex_version_output(&format!("{stdout}\n{stderr}"))
}

pub(crate) fn parse_codex_version_output(output: &str) -> Option<String> {
    output
        .lines()
        .filter(|line| line.to_ascii_lowercase().contains("codex"))
        .find_map(parse_version_from_line)
        .or_else(|| output.lines().find_map(parse_version_from_line))
}

fn parse_version_from_line(line: &str) -> Option<String> {
    line.split_whitespace().find_map(|token| {
        let token = token.trim_matches(|ch: char| {
            !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '+'))
        });
        let token = token.strip_prefix('v').unwrap_or(token);
        is_semver_like(token).then(|| token.to_string())
    })
}

fn is_semver_like(value: &str) -> bool {
    let without_prerelease = value.split_once('-').map(|(core, _)| core).unwrap_or(value);
    let core = without_prerelease
        .split_once('+')
        .map(|(core, _)| core)
        .unwrap_or(without_prerelease);
    let parts = core.split('.').collect::<Vec<_>>();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TerminalInfo {
    name: TerminalName,
    term_program: Option<String>,
    version: Option<String>,
    term: Option<String>,
    multiplexer: Option<Multiplexer>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalName {
    AppleTerminal,
    Ghostty,
    Iterm2,
    WarpTerminal,
    VsCode,
    WezTerm,
    Kitty,
    Alacritty,
    Konsole,
    GnomeTerminal,
    Vte,
    WindowsTerminal,
    Dumb,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Multiplexer {
    Tmux { version: Option<String> },
    Zellij {},
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct TmuxClientInfo {
    termtype: Option<String>,
    termname: Option<String>,
}

impl TerminalInfo {
    fn new(
        name: TerminalName,
        term_program: Option<String>,
        version: Option<String>,
        term: Option<String>,
        multiplexer: Option<Multiplexer>,
    ) -> Self {
        Self {
            name,
            term_program,
            version,
            term,
            multiplexer,
        }
    }

    fn from_term_program(
        name: TerminalName,
        term_program: String,
        version: Option<String>,
        multiplexer: Option<Multiplexer>,
    ) -> Self {
        Self::new(name, Some(term_program), version, None, multiplexer)
    }

    fn from_term_program_and_term(
        name: TerminalName,
        term_program: String,
        version: Option<String>,
        term: Option<String>,
        multiplexer: Option<Multiplexer>,
    ) -> Self {
        Self::new(name, Some(term_program), version, term, multiplexer)
    }

    fn from_name(
        name: TerminalName,
        version: Option<String>,
        multiplexer: Option<Multiplexer>,
    ) -> Self {
        Self::new(name, None, version, None, multiplexer)
    }

    fn from_term(term: String, multiplexer: Option<Multiplexer>) -> Self {
        let name = match term.as_str() {
            "dumb" => TerminalName::Dumb,
            "wezterm" | "wezterm-mux" => TerminalName::WezTerm,
            _ => TerminalName::Unknown,
        };
        Self::new(name, None, None, Some(term), multiplexer)
    }

    fn unknown(multiplexer: Option<Multiplexer>) -> Self {
        Self::new(TerminalName::Unknown, None, None, None, multiplexer)
    }

    fn user_agent_token(&self) -> String {
        let raw = if let Some(program) = self.term_program.as_ref() {
            match self.version.as_ref().filter(|version| !version.is_empty()) {
                Some(version) => format!("{program}/{version}"),
                None => program.clone(),
            }
        } else if let Some(term) = self.term.as_ref().filter(|value| !value.is_empty()) {
            term.clone()
        } else {
            match self.name {
                TerminalName::AppleTerminal => {
                    format_terminal_version("Apple_Terminal", &self.version)
                }
                TerminalName::Ghostty => format_terminal_version("Ghostty", &self.version),
                TerminalName::Iterm2 => format_terminal_version("iTerm.app", &self.version),
                TerminalName::WarpTerminal => {
                    format_terminal_version("WarpTerminal", &self.version)
                }
                TerminalName::VsCode => format_terminal_version("vscode", &self.version),
                TerminalName::WezTerm => format_terminal_version("WezTerm", &self.version),
                TerminalName::Kitty => "kitty".to_string(),
                TerminalName::Alacritty => "Alacritty".to_string(),
                TerminalName::Konsole => format_terminal_version("Konsole", &self.version),
                TerminalName::GnomeTerminal => "gnome-terminal".to_string(),
                TerminalName::Vte => format_terminal_version("VTE", &self.version),
                TerminalName::WindowsTerminal => "WindowsTerminal".to_string(),
                TerminalName::Dumb => "dumb".to_string(),
                TerminalName::Unknown => "unknown".to_string(),
            }
        };

        sanitize_terminal_header_value(raw)
    }
}

trait Environment {
    fn var(&self, name: &str) -> Option<String>;

    fn has(&self, name: &str) -> bool {
        self.var(name).is_some()
    }

    fn var_non_empty(&self, name: &str) -> Option<String> {
        self.var(name).and_then(none_if_whitespace)
    }

    fn has_non_empty(&self, name: &str) -> bool {
        self.var_non_empty(name).is_some()
    }

    fn tmux_client_info(&self) -> TmuxClientInfo;
}

struct ProcessEnvironment;

impl Environment for ProcessEnvironment {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn tmux_client_info(&self) -> TmuxClientInfo {
        tmux_client_info()
    }
}

fn terminal_user_agent_token() -> String {
    terminal_info().user_agent_token()
}

fn terminal_info() -> TerminalInfo {
    TERMINAL_INFO
        .get_or_init(|| detect_terminal_info_from_env(&ProcessEnvironment))
        .clone()
}

fn detect_terminal_info_from_env(env: &dyn Environment) -> TerminalInfo {
    let multiplexer = detect_multiplexer(env);

    if let Some(term_program) = env.var_non_empty("TERM_PROGRAM") {
        if is_tmux_term_program(&term_program)
            && matches!(multiplexer, Some(Multiplexer::Tmux { .. }))
            && let Some(terminal) =
                terminal_from_tmux_client_info(env.tmux_client_info(), multiplexer.clone())
        {
            return terminal;
        }

        let version = env.var_non_empty("TERM_PROGRAM_VERSION");
        let name = terminal_name_from_term_program(&term_program).unwrap_or(TerminalName::Unknown);
        return TerminalInfo::from_term_program(name, term_program, version, multiplexer);
    }

    if env.has("WEZTERM_VERSION") {
        let version = env.var_non_empty("WEZTERM_VERSION");
        return TerminalInfo::from_name(TerminalName::WezTerm, version, multiplexer);
    }

    if env.has("ITERM_SESSION_ID") || env.has("ITERM_PROFILE") || env.has("ITERM_PROFILE_NAME") {
        return TerminalInfo::from_name(TerminalName::Iterm2, None, multiplexer);
    }

    if env.has("TERM_SESSION_ID") {
        return TerminalInfo::from_name(TerminalName::AppleTerminal, None, multiplexer);
    }

    if env.has("KITTY_WINDOW_ID")
        || env
            .var("TERM")
            .map(|term| term.contains("kitty"))
            .unwrap_or(false)
    {
        return TerminalInfo::from_name(TerminalName::Kitty, None, multiplexer);
    }

    if env.has("ALACRITTY_SOCKET")
        || env
            .var("TERM")
            .map(|term| term == "alacritty")
            .unwrap_or(false)
    {
        return TerminalInfo::from_name(TerminalName::Alacritty, None, multiplexer);
    }

    if env.has("KONSOLE_VERSION") {
        let version = env.var_non_empty("KONSOLE_VERSION");
        return TerminalInfo::from_name(TerminalName::Konsole, version, multiplexer);
    }

    if env.has("GNOME_TERMINAL_SCREEN") {
        return TerminalInfo::from_name(TerminalName::GnomeTerminal, None, multiplexer);
    }

    if env.has("VTE_VERSION") {
        let version = env.var_non_empty("VTE_VERSION");
        return TerminalInfo::from_name(TerminalName::Vte, version, multiplexer);
    }

    if env.has("WT_SESSION") {
        return TerminalInfo::from_name(TerminalName::WindowsTerminal, None, multiplexer);
    }

    if let Some(term) = env.var_non_empty("TERM") {
        return TerminalInfo::from_term(term, multiplexer);
    }

    TerminalInfo::unknown(multiplexer)
}

fn detect_multiplexer(env: &dyn Environment) -> Option<Multiplexer> {
    if env.has_non_empty("TMUX") || env.has_non_empty("TMUX_PANE") {
        return Some(Multiplexer::Tmux {
            version: tmux_version_from_env(env),
        });
    }

    if env.has_non_empty("ZELLIJ")
        || env.has_non_empty("ZELLIJ_SESSION_NAME")
        || env.has_non_empty("ZELLIJ_VERSION")
    {
        return Some(Multiplexer::Zellij {});
    }

    None
}

fn is_tmux_term_program(value: &str) -> bool {
    value.eq_ignore_ascii_case("tmux")
}

fn terminal_from_tmux_client_info(
    client_info: TmuxClientInfo,
    multiplexer: Option<Multiplexer>,
) -> Option<TerminalInfo> {
    let termtype = client_info.termtype.and_then(none_if_whitespace);
    let termname = client_info.termname.and_then(none_if_whitespace);

    if let Some(termtype) = termtype.as_ref() {
        let (program, version) = split_term_program_and_version(termtype);
        let name = terminal_name_from_term_program(&program).unwrap_or(TerminalName::Unknown);
        return Some(TerminalInfo::from_term_program_and_term(
            name,
            program,
            version,
            termname,
            multiplexer,
        ));
    }

    termname
        .as_ref()
        .map(|termname| TerminalInfo::from_term(termname.to_string(), multiplexer))
}

fn tmux_version_from_env(env: &dyn Environment) -> Option<String> {
    let term_program = env.var("TERM_PROGRAM")?;
    if !is_tmux_term_program(&term_program) {
        return None;
    }

    env.var_non_empty("TERM_PROGRAM_VERSION")
}

fn split_term_program_and_version(value: &str) -> (String, Option<String>) {
    let mut parts = value.split_whitespace();
    let program = parts.next().unwrap_or_default().to_string();
    let version = parts.next().map(ToString::to_string);
    (program, version)
}

fn tmux_client_info() -> TmuxClientInfo {
    let termtype = tmux_display_message("#{client_termtype}");
    let termname = tmux_display_message("#{client_termname}");

    TmuxClientInfo { termtype, termname }
}

fn tmux_display_message(format: &str) -> Option<String> {
    let output = Command::new("tmux")
        .args(["display-message", "-p", format])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let value = String::from_utf8(output.stdout).ok()?;
    none_if_whitespace(value.trim().to_string())
}

fn sanitize_terminal_header_value(value: String) -> String {
    value.replace(|ch| !is_valid_terminal_header_value_char(ch), "_")
}

fn is_valid_terminal_header_value_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/')
}

fn terminal_name_from_term_program(value: &str) -> Option<TerminalName> {
    let normalized: String = value
        .trim()
        .chars()
        .filter(|ch| !matches!(ch, ' ' | '-' | '_' | '.'))
        .map(|ch| ch.to_ascii_lowercase())
        .collect();

    match normalized.as_str() {
        "appleterminal" => Some(TerminalName::AppleTerminal),
        "ghostty" => Some(TerminalName::Ghostty),
        "iterm" | "iterm2" | "itermapp" => Some(TerminalName::Iterm2),
        "warp" | "warpterminal" => Some(TerminalName::WarpTerminal),
        "vscode" => Some(TerminalName::VsCode),
        "wezterm" => Some(TerminalName::WezTerm),
        "kitty" => Some(TerminalName::Kitty),
        "alacritty" => Some(TerminalName::Alacritty),
        "konsole" => Some(TerminalName::Konsole),
        "gnometerminal" => Some(TerminalName::GnomeTerminal),
        "vte" => Some(TerminalName::Vte),
        "windowsterminal" => Some(TerminalName::WindowsTerminal),
        "dumb" => Some(TerminalName::Dumb),
        _ => None,
    }
}

fn format_terminal_version(name: &str, version: &Option<String>) -> String {
    match version.as_ref().filter(|value| !value.is_empty()) {
        Some(version) => format!("{name}/{version}"),
        None => name.to_string(),
    }
}

fn none_if_whitespace(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use reqwest::header::USER_AGENT;

    use super::{
        TmuxClientInfo, codex_default_headers, detect_terminal_info_from_env, is_semver_like,
        originator_value, parse_codex_version_output, sanitize_user_agent,
    };

    #[test]
    fn codex_default_headers_include_originator_and_user_agent() {
        let headers = codex_default_headers().expect("headers should build");
        let expected_originator = originator_value();

        assert_eq!(
            headers
                .get("originator")
                .and_then(|value| value.to_str().ok()),
            Some(expected_originator.as_str())
        );
        assert!(
            headers
                .get(USER_AGENT)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with(&format!("{expected_originator}/")))
        );
    }

    #[test]
    fn parse_codex_version_output_uses_codex_version_line() {
        let output = "WARNING: PATH update failed\ncodex-cli 0.129.0\n";
        assert_eq!(
            parse_codex_version_output(output),
            Some("0.129.0".to_string())
        );
    }

    #[test]
    fn parse_codex_version_output_accepts_prerelease() {
        assert_eq!(
            parse_codex_version_output("codex-cli v1.2.3-alpha.1+build.4"),
            Some("1.2.3-alpha.1+build.4".to_string())
        );
    }

    #[test]
    fn semver_like_rejects_non_versions() {
        assert!(is_semver_like("1.2.3"));
        assert!(is_semver_like("1.2.3-alpha.1"));
        assert!(!is_semver_like("1.2"));
        assert!(!is_semver_like("codex-cli"));
    }

    #[test]
    fn sanitize_user_agent_replaces_invalid_header_chars() {
        assert_eq!(
            sanitize_user_agent("codex_cli_rs/1.2.3\nbad".to_string(), "fallback"),
            "codex_cli_rs/1.2.3_bad"
        );
    }

    #[test]
    fn terminal_info_prefers_term_program() {
        let env = TestEnvironment::new([
            ("TERM_PROGRAM", "WezTerm"),
            ("TERM_PROGRAM_VERSION", "20240203"),
            ("TERM", "xterm-256color"),
        ]);

        assert_eq!(
            detect_terminal_info_from_env(&env).user_agent_token(),
            "WezTerm/20240203"
        );
    }

    #[test]
    fn terminal_info_uses_tmux_client_terminal() {
        let env = TestEnvironment::new([("TMUX", "/tmp/tmux"), ("TERM_PROGRAM", "tmux")])
            .with_tmux_client_info(TmuxClientInfo {
                termtype: Some("ghostty 1.1.3".to_string()),
                termname: Some("xterm-ghostty".to_string()),
            });

        assert_eq!(
            detect_terminal_info_from_env(&env).user_agent_token(),
            "ghostty/1.1.3"
        );
    }

    #[derive(Default)]
    struct TestEnvironment {
        vars: HashMap<String, String>,
        tmux_client_info: TmuxClientInfo,
    }

    impl TestEnvironment {
        fn new<const N: usize>(vars: [(&str, &str); N]) -> Self {
            let mut env = Self::default();
            for (key, value) in vars {
                env.vars.insert(key.to_string(), value.to_string());
            }
            env
        }

        fn with_tmux_client_info(mut self, tmux_client_info: TmuxClientInfo) -> Self {
            self.tmux_client_info = tmux_client_info;
            self
        }
    }

    impl super::Environment for TestEnvironment {
        fn var(&self, name: &str) -> Option<String> {
            self.vars.get(name).cloned()
        }

        fn tmux_client_info(&self) -> TmuxClientInfo {
            self.tmux_client_info.clone()
        }
    }
}
