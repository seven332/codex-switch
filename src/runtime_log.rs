use std::env::VarError;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
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
const RUN_LOG_PREFIX: &str = "codex-switch-run-";
const RUN_LOG_SUFFIX: &str = ".log";
const RUN_LOG_TIMESTAMP_FORMAT: &str = "%Y%m%d-%H%M%S";
const RUN_LOG_TIMESTAMP_LEN: usize = 15;

pub(crate) const DEFAULT_RUNTIME_LOG_RETENTION_DAYS: u64 = 7;

pub(crate) struct RuntimeLogGuard {
    stderr_enabled: Arc<AtomicBool>,
    log_path: PathBuf,
    _file_guard: WorkerGuard,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RuntimeLogPruneSummary {
    pub(crate) scanned: usize,
    pub(crate) matched: usize,
    pub(crate) candidates: usize,
    pub(crate) removed: usize,
    pub(crate) ignored_missing: usize,
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

    spawn_runtime_log_cleanup(log_path.clone());

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

pub(crate) fn runtime_log_dir_path() -> Result<PathBuf> {
    runtime_log_dir()
}

pub(crate) fn latest_runtime_log_path() -> Result<Option<PathBuf>> {
    latest_runtime_log_path_in_dir(&runtime_log_dir()?)
}

pub(crate) fn latest_runtime_log_path_in_dir(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }

    let mut latest: Option<(String, PathBuf)> = None;
    for entry in fs::read_dir(path)
        .with_context(|| format!("Failed to read runtime log directory: {}", path.display()))?
    {
        let entry = entry
            .with_context(|| format!("Failed to read runtime log entry in {}", path.display()))?;
        let file_type = entry.file_type().with_context(|| {
            format!(
                "Failed to inspect runtime log entry: {}",
                entry.path().display()
            )
        })?;
        if !file_type.is_file() {
            continue;
        }

        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if !file_name.starts_with("codex-switch-run-") || !file_name.ends_with(".log") {
            continue;
        }

        let path = entry.path();
        match latest {
            Some((ref latest_name, _)) if file_name <= latest_name.as_str() => {}
            _ => latest = Some((file_name.to_string(), path)),
        }
    }

    Ok(latest.map(|(_, path)| path))
}

pub(crate) fn prune_runtime_logs(
    retention_days: u64,
    dry_run: bool,
) -> Result<RuntimeLogPruneSummary> {
    prune_runtime_logs_in_dir(
        &runtime_log_dir()?,
        Utc::now(),
        retention_days,
        None,
        dry_run,
    )
}

fn spawn_runtime_log_cleanup(current_log_path: PathBuf) {
    let result = std::thread::Builder::new()
        .name("codex-switch-log-cleanup".to_string())
        .spawn(move || {
            match prune_runtime_logs_in_dir(
                current_log_path.parent().unwrap_or_else(|| Path::new(".")),
                Utc::now(),
                DEFAULT_RUNTIME_LOG_RETENTION_DAYS,
                Some(&current_log_path),
                false,
            ) {
                Ok(summary) => tracing::debug!(
                    scanned = summary.scanned,
                    matched = summary.matched,
                    candidates = summary.candidates,
                    removed = summary.removed,
                    ignored_missing = summary.ignored_missing,
                    "runtime log cleanup complete"
                ),
                Err(err) => {
                    tracing::debug!(error = ?err, "runtime log cleanup failed");
                }
            }
        });

    if let Err(err) = result {
        tracing::debug!(error = ?err, "failed to start runtime log cleanup thread");
    }
}

fn prune_runtime_logs_in_dir(
    path: &Path,
    now: DateTime<Utc>,
    retention_days: u64,
    current_log_path: Option<&Path>,
    dry_run: bool,
) -> Result<RuntimeLogPruneSummary> {
    prune_runtime_logs_in_dir_with_remover(
        path,
        now,
        retention_days,
        current_log_path,
        dry_run,
        |candidate| fs::remove_file(candidate),
    )
}

fn prune_runtime_logs_in_dir_with_remover(
    path: &Path,
    now: DateTime<Utc>,
    retention_days: u64,
    current_log_path: Option<&Path>,
    dry_run: bool,
    mut remove_file: impl FnMut(&Path) -> io::Result<()>,
) -> Result<RuntimeLogPruneSummary> {
    anyhow::ensure!(
        retention_days > 0,
        "Runtime log retention days must be greater than 0"
    );

    if !path.exists() {
        return Ok(RuntimeLogPruneSummary::default());
    }

    let retention_days =
        i64::try_from(retention_days).context("Runtime log retention days are too large")?;
    let retention = Duration::try_days(retention_days)
        .context("Runtime log retention duration is too large")?;
    let cutoff = now - retention;
    let mut summary = RuntimeLogPruneSummary::default();
    let mut candidates = Vec::new();

    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(RuntimeLogPruneSummary::default());
        }
        Err(err) => {
            return Err(err).with_context(|| {
                format!("Failed to read runtime log directory: {}", path.display())
            });
        }
    };

    for entry in entries {
        let entry = entry
            .with_context(|| format!("Failed to read runtime log entry in {}", path.display()))?;
        summary.scanned += 1;

        let file_type = entry.file_type().with_context(|| {
            format!(
                "Failed to inspect runtime log entry: {}",
                entry.path().display()
            )
        })?;
        if !file_type.is_file() {
            continue;
        }

        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let Some(timestamp) = runtime_log_timestamp_from_file_name(file_name) else {
            continue;
        };
        summary.matched += 1;

        let entry_path = entry.path();
        if current_log_path == Some(entry_path.as_path()) {
            continue;
        }
        if timestamp < cutoff {
            candidates.push(entry_path);
        }
    }

    summary.candidates = candidates.len();
    if dry_run {
        return Ok(summary);
    }

    for candidate in candidates {
        match remove_file(&candidate) {
            Ok(()) => summary.removed += 1,
            Err(err) if err.kind() == io::ErrorKind::NotFound => summary.ignored_missing += 1,
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("Failed to remove runtime log: {}", candidate.display())
                });
            }
        }
    }

    Ok(summary)
}

fn runtime_log_timestamp_from_file_name(file_name: &str) -> Option<DateTime<Utc>> {
    let body = file_name
        .strip_prefix(RUN_LOG_PREFIX)?
        .strip_suffix(RUN_LOG_SUFFIX)?;
    if body.len() <= RUN_LOG_TIMESTAMP_LEN {
        return None;
    }

    let timestamp = body.get(..RUN_LOG_TIMESTAMP_LEN)?;
    let remainder = body.get(RUN_LOG_TIMESTAMP_LEN..)?;
    let remainder = remainder.strip_prefix('-')?;
    let mut parts = remainder.rsplitn(3, '-');
    let uuid = parts.next()?;
    let pid = parts.next()?;
    let host = parts.next()?;
    if host.is_empty()
        || pid.is_empty()
        || !pid.chars().all(|ch| ch.is_ascii_digit())
        || uuid.len() != 32
        || !uuid.chars().all(|ch| ch.is_ascii_hexdigit())
    {
        return None;
    }

    let parsed = NaiveDateTime::parse_from_str(timestamp, RUN_LOG_TIMESTAMP_FORMAT).ok()?;
    Some(DateTime::from_naive_utc_and_offset(parsed, Utc))
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

    #[test]
    fn latest_runtime_log_path_picks_newest_named_log() {
        let dir = std::env::temp_dir().join(format!("codex-switch-log-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("test log dir should be created");
        let older = dir.join("codex-switch-run-20260101-000000-host-1-a.log");
        let newer = dir.join("codex-switch-run-20260102-000000-host-1-b.log");
        let ignored = dir.join("other.log");
        File::create(&older).expect("older log should be created");
        File::create(&newer).expect("newer log should be created");
        File::create(&ignored).expect("ignored log should be created");

        let latest =
            latest_runtime_log_path_in_dir(&dir).expect("latest log lookup should succeed");

        assert_eq!(latest.as_deref(), Some(newer.as_path()));
        fs::remove_dir_all(&dir).expect("test log dir should be removed");
    }

    #[test]
    fn runtime_log_timestamp_parser_accepts_current_log_pattern() {
        let timestamp = runtime_log_timestamp_from_file_name(
            "codex-switch-run-20260102-030405-dev-host-123-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.log",
        )
        .expect("timestamp should parse");

        assert_eq!(timestamp.to_rfc3339(), "2026-01-02T03:04:05+00:00");
    }

    #[test]
    fn runtime_log_timestamp_parser_rejects_malformed_names() {
        assert!(
            runtime_log_timestamp_from_file_name(
                "codex-switch-run-20260102-030405-dev-host-123-short.log"
            )
            .is_none()
        );
        assert!(runtime_log_timestamp_from_file_name("other.log").is_none());
        assert!(
            runtime_log_timestamp_from_file_name(
                "codex-switch-run-20260102-030405-dev-host-abc-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.log"
            )
            .is_none()
        );
        assert!(
            runtime_log_timestamp_from_file_name(
                "codex-switch-run-é-é-é-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.log"
            )
            .is_none()
        );
    }

    #[test]
    fn prune_runtime_logs_deletes_only_matching_logs_older_than_retention() {
        let dir = temp_log_dir("prune-old");
        let old = create_test_log(&dir, "20251231-235959", "old");
        let boundary = create_test_log(&dir, "20260101-000000", "boundary");
        let recent = create_test_log(&dir, "20260107-000000", "recent");
        let ignored = dir.join("notes.log");
        File::create(&ignored).expect("ignored file should be created");
        let now = test_time("2026-01-08T00:00:00Z");

        let summary = prune_runtime_logs_in_dir(&dir, now, 7, None, false)
            .expect("runtime log pruning should succeed");

        assert_eq!(summary.scanned, 4);
        assert_eq!(summary.matched, 3);
        assert_eq!(summary.candidates, 1);
        assert_eq!(summary.removed, 1);
        assert!(!old.exists());
        assert!(boundary.exists());
        assert!(recent.exists());
        assert!(ignored.exists());
        fs::remove_dir_all(&dir).expect("test log dir should be removed");
    }

    #[test]
    fn prune_runtime_logs_keeps_current_log_even_when_old() {
        let dir = temp_log_dir("prune-current");
        let current = create_test_log(&dir, "20251231-235959", "current");
        let old = create_test_log(&dir, "20251230-235959", "old");
        let now = test_time("2026-01-08T00:00:00Z");

        let summary = prune_runtime_logs_in_dir(&dir, now, 7, Some(&current), false)
            .expect("runtime log pruning should succeed");

        assert_eq!(summary.candidates, 1);
        assert_eq!(summary.removed, 1);
        assert!(current.exists());
        assert!(!old.exists());
        fs::remove_dir_all(&dir).expect("test log dir should be removed");
    }

    #[test]
    fn prune_runtime_logs_dry_run_does_not_delete_candidates() {
        let dir = temp_log_dir("prune-dry-run");
        let old = create_test_log(&dir, "20251231-235959", "old");
        let now = test_time("2026-01-08T00:00:00Z");

        let summary = prune_runtime_logs_in_dir(&dir, now, 7, None, true)
            .expect("runtime log dry run should succeed");

        assert_eq!(summary.candidates, 1);
        assert_eq!(summary.removed, 0);
        assert!(old.exists());
        fs::remove_dir_all(&dir).expect("test log dir should be removed");
    }

    #[test]
    fn prune_runtime_logs_ignores_not_found_delete_races() {
        let dir = temp_log_dir("prune-race");
        create_test_log(&dir, "20251231-235959", "old");
        let now = test_time("2026-01-08T00:00:00Z");

        let summary = prune_runtime_logs_in_dir_with_remover(&dir, now, 7, None, false, |_| {
            Err(io::Error::new(io::ErrorKind::NotFound, "already gone"))
        })
        .expect("not found races should be ignored");

        assert_eq!(summary.candidates, 1);
        assert_eq!(summary.removed, 0);
        assert_eq!(summary.ignored_missing, 1);
        fs::remove_dir_all(&dir).expect("test log dir should be removed");
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

    fn temp_log_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("codex-switch-log-test-{name}-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("test log dir should be created");
        dir
    }

    fn create_test_log(dir: &Path, timestamp: &str, tag: &str) -> PathBuf {
        let uuid = match tag {
            "old" => "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "boundary" => "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "recent" => "cccccccccccccccccccccccccccccccc",
            "current" => "dddddddddddddddddddddddddddddddd",
            _ => "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
        };
        let path = dir.join(format!(
            "codex-switch-run-{timestamp}-test-host-123-{uuid}.log"
        ));
        File::create(&path).expect("test log should be created");
        path
    }

    fn test_time(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .expect("test timestamp should parse")
            .with_timezone(&Utc)
    }
}
