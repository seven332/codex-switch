use std::env::VarError;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;
use uuid::Uuid;

use crate::store;

const RUNTIME_LOG_ENV: &str = "CODEX_SWITCH_LOG";
const RUN_LOG_CREATE_ATTEMPTS: usize = 16;
const MAX_LOG_COMPONENT_LEN: usize = 64;

pub(crate) struct RuntimeLogGuard {
    stderr_enabled: Arc<AtomicBool>,
    log_path: PathBuf,
    _file_guard: WorkerGuard,
}

impl RuntimeLogGuard {
    pub(crate) fn path(&self) -> &Path {
        &self.log_path
    }

    pub(crate) fn disable_stderr(&self) {
        self.stderr_enabled.store(false, Ordering::Release);
    }

    pub(crate) fn enable_stderr(&self) {
        self.stderr_enabled.store(true, Ordering::Release);
    }
}

pub(crate) fn init_runtime_tracing() -> Result<RuntimeLogGuard> {
    let (log_file, log_path) = create_run_log_file()?;
    let (non_blocking, file_guard) = tracing_appender::non_blocking(log_file);
    let stderr_enabled = Arc::new(AtomicBool::new(true));
    let filter_spec = runtime_tracing_filter_spec(std::env::var(RUNTIME_LOG_ENV));
    let file_filter = runtime_tracing_filter(&filter_spec);
    let stderr_filter = runtime_tracing_filter(&filter_spec);

    let file_layer = fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_filter(file_filter);
    let stderr_layer = fmt::layer()
        .with_writer(ConditionalStderr::new(stderr_enabled.clone()))
        .with_ansi(false)
        .with_filter(stderr_filter);

    tracing_subscriber::registry()
        .with(file_layer)
        .with(stderr_layer)
        .try_init()
        .context("Failed to initialize runtime logging")?;

    Ok(RuntimeLogGuard {
        stderr_enabled,
        log_path,
        _file_guard: file_guard,
    })
}

fn runtime_tracing_filter(filter_spec: &str) -> EnvFilter {
    EnvFilter::try_new(filter_spec).unwrap_or_else(|_| EnvFilter::new("codex_switch=info"))
}

pub(crate) fn runtime_tracing_filter_spec(value: Result<String, VarError>) -> String {
    let Ok(value) = value else {
        return "codex_switch=info".to_string();
    };
    let value = value.trim();
    if value.is_empty() {
        return "codex_switch=info".to_string();
    }

    if is_plain_tracing_level(value) {
        return format!("codex_switch={}", value.to_ascii_lowercase());
    }

    value.to_string()
}

fn is_plain_tracing_level(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "trace" | "debug" | "info" | "warn" | "error" | "off"
    )
}

fn create_run_log_file() -> Result<(File, PathBuf)> {
    let log_dir = runtime_log_dir()?;
    create_private_log_dir(&log_dir)?;

    for _ in 0..RUN_LOG_CREATE_ATTEMPTS {
        let path = log_dir.join(run_log_file_name(
            Utc::now(),
            &host_component(),
            std::process::id(),
            Uuid::new_v4(),
        ));
        match create_private_log_file(&path) {
            Ok(file) => return Ok((file, path)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("Failed to create runtime log file: {}", path.display())
                });
            }
        }
    }

    anyhow::bail!(
        "Failed to create a unique runtime log file in {}",
        log_dir.display()
    )
}

fn runtime_log_dir() -> Result<PathBuf> {
    Ok(store::config_dir()?.join("logs"))
}

fn create_private_log_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(path).with_context(|| {
            format!("Failed to create runtime log directory: {}", path.display())
        })?;
        set_private_dir_permissions(path)
    }

    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
            .with_context(|| format!("Failed to create runtime log directory: {}", path.display()))
    }
}

fn create_private_log_file(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        OpenOptions::new()
            .append(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    }

    #[cfg(not(unix))]
    {
        OpenOptions::new().append(true).create_new(true).open(path)
    }
}

#[cfg(unix)]
fn set_private_dir_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).with_context(|| {
        format!(
            "Failed to set runtime log directory permissions: {}",
            path.display()
        )
    })
}

#[cfg(not(unix))]
fn set_private_dir_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

fn run_log_file_name(now: DateTime<Utc>, host: &str, pid: u32, unique: Uuid) -> String {
    format!(
        "codex-switch-run-{}-{}-{}-{}.log",
        now.format("%Y%m%d-%H%M%S"),
        sanitize_log_component(host),
        pid,
        unique.simple()
    )
}

fn host_component() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            fs::read_to_string("/etc/hostname")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| "unknown-host".to_string())
}

fn sanitize_log_component(value: &str) -> String {
    let mut sanitized = String::new();
    for ch in value.chars() {
        if sanitized.len() >= MAX_LOG_COMPONENT_LEN {
            break;
        }
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            sanitized.push(ch);
        } else if !sanitized.ends_with('-') {
            sanitized.push('-');
        }
    }

    let sanitized = sanitized.trim_matches('-');
    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized.to_string()
    }
}

#[derive(Clone)]
struct ConditionalStderr {
    enabled: Arc<AtomicBool>,
}

impl ConditionalStderr {
    fn new(enabled: Arc<AtomicBool>) -> Self {
        Self { enabled }
    }
}

impl<'writer> MakeWriter<'writer> for ConditionalStderr {
    type Writer = ConditionalStderrWriter;

    fn make_writer(&'writer self) -> Self::Writer {
        ConditionalStderrWriter {
            enabled: self.enabled.clone(),
            stderr: io::stderr(),
        }
    }
}

struct ConditionalStderrWriter {
    enabled: Arc<AtomicBool>,
    stderr: io::Stderr,
}

impl Write for ConditionalStderrWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.enabled.load(Ordering::Acquire) {
            self.stderr.write(buf)
        } else {
            Ok(buf.len())
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.enabled.load(Ordering::Acquire) {
            self.stderr.flush()
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_log_component_sanitizes_path_separators_and_control_characters() {
        assert_eq!(sanitize_log_component("../host\nname"), "..-host-name");
    }

    #[test]
    fn runtime_log_component_falls_back_when_empty_after_sanitizing() {
        assert_eq!(sanitize_log_component("\n\t"), "unknown");
    }

    #[test]
    fn runtime_log_file_name_contains_host_pid_and_uuid() {
        let timestamp = DateTime::parse_from_rfc3339("2026-05-19T12:34:56Z")
            .expect("timestamp should parse")
            .with_timezone(&Utc);
        let uuid =
            Uuid::parse_str("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").expect("uuid should parse");

        assert_eq!(
            run_log_file_name(timestamp, "dev/container", 123, uuid),
            "codex-switch-run-20260519-123456-dev-container-123-aaaaaaaabbbbccccddddeeeeeeeeeeee.log"
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_log_file_is_create_new_and_user_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("codex-switch-log-test-{}", Uuid::new_v4()));
        fs::create_dir(&dir).expect("test log dir should be created");
        let path = dir.join("run.log");
        let file = create_private_log_file(&path).expect("log file should be created");
        drop(file);

        let mode = fs::metadata(&path)
            .expect("log file metadata should be readable")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            create_private_log_file(&path)
                .expect_err("existing log file should not be replaced")
                .kind(),
            io::ErrorKind::AlreadyExists
        );

        fs::remove_dir_all(&dir).expect("test log dir should be removed");
    }

    #[cfg(unix)]
    #[test]
    fn private_log_dir_is_user_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("codex-switch-log-test-{}", Uuid::new_v4()));
        create_private_log_dir(&dir).expect("test log dir should be created");

        let mode = fs::metadata(&dir)
            .expect("log dir metadata should be readable")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);

        fs::remove_dir_all(&dir).expect("test log dir should be removed");
    }

    #[test]
    fn default_runtime_log_filter_is_codex_switch_info() {
        assert_eq!(
            runtime_tracing_filter_spec(Err(std::env::VarError::NotPresent)),
            "codex_switch=info"
        );
        assert_eq!(
            runtime_tracing_filter_spec(Ok(String::new())),
            "codex_switch=info"
        );
    }

    #[test]
    fn plain_runtime_log_level_maps_to_codex_switch_target() {
        assert_eq!(
            runtime_tracing_filter_spec(Ok("debug".to_string())),
            "codex_switch=debug"
        );
        assert_eq!(
            runtime_tracing_filter_spec(Ok("WARN".to_string())),
            "codex_switch=warn"
        );
    }

    #[test]
    fn full_runtime_log_filter_is_preserved() {
        assert_eq!(
            runtime_tracing_filter_spec(Ok("codex_switch=debug,tokio=warn".to_string())),
            "codex_switch=debug,tokio=warn"
        );
    }
}
