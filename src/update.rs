use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

const GITHUB_RELEASES_API: &str = "https://api.github.com/repos/seven332/codex-switch/releases";
const UPDATE_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const UPDATE_PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
const UPDATE_PROGRESS_BAR_WIDTH: usize = 24;
const UPDATE_PROGRESS_FILLED: &str = "\u{2588}";
const UPDATE_PROGRESS_EMPTY: &str = "\u{2591}";
const MAX_UPDATE_ASSET_SIZE: u64 = 64 * 1024 * 1024;
const CRATES_IO_REGISTRY_SOURCE: &str = "registry+https://github.com/rust-lang/crates.io-index";

#[derive(Debug)]
pub struct UpdateOptions {
    pub check: bool,
    pub version: Option<String>,
}

#[derive(Debug)]
pub enum UpdateOutcome {
    UpToDate {
        current_version: String,
        release_version: String,
    },
    UpdateAvailable {
        current_version: String,
        release_version: String,
    },
    CurrentNewer {
        current_version: String,
        release_version: String,
    },
    Updated {
        previous_version: String,
        installed_version: String,
        executable_path: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InstallTarget {
    ReleaseBinary(PathBuf),
    CargoInstall {
        executable_path: PathBuf,
        install_root: PathBuf,
    },
    UnsupportedCargoInstall {
        executable_path: PathBuf,
        install_root: PathBuf,
        package_id: String,
    },
    InvalidCargoTracking {
        executable_path: PathBuf,
        install_root: PathBuf,
        message: String,
    },
    CargoBuildArtifact(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum UpdateInstallTarget {
    ReleaseBinary(PathBuf),
    CargoInstall {
        executable_path: PathBuf,
        install_root: PathBuf,
    },
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<GitHubReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubReleaseAsset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CargoTrackedInstall {
    package_id: String,
    source: CargoTrackedInstallSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CargoTrackedInstallSource {
    CratesIo,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ReleaseVersion {
    major: u64,
    minor: u64,
    patch: u64,
}

impl fmt::Display for ReleaseVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

pub async fn update(options: UpdateOptions) -> Result<UpdateOutcome> {
    let current_version = parse_release_version(env!("CARGO_PKG_VERSION"))
        .context("Failed to parse current package version")?;
    let requested_tag = options
        .version
        .as_deref()
        .map(normalize_release_tag)
        .transpose()?;
    let update_target = if options.check {
        None
    } else {
        Some(current_update_install_target()?)
    };
    let client = update_http_client()?;
    let release = match requested_tag.as_deref() {
        Some(tag) => fetch_release_by_tag(&client, tag).await?,
        None => fetch_latest_release(&client).await?,
    };
    let target_version = parse_release_version(&release.tag_name)
        .with_context(|| format!("Failed to parse release tag {}", release.tag_name))?;

    if options.check {
        return Ok(update_check_outcome(current_version, target_version));
    }

    if options.version.is_none() && target_version <= current_version {
        return Ok(update_check_outcome(current_version, target_version));
    }

    let update_target = update_target.context("Update target was not resolved")?;
    match update_target {
        UpdateInstallTarget::ReleaseBinary(executable_path) => {
            let asset_name = current_platform_asset_name()?;
            let asset = release_asset(&release, asset_name)?;
            let binary = download_asset(&client, &asset.browser_download_url, asset_name).await?;
            verify_asset_digest(&binary, asset)?;

            install_update_at(&executable_path, &binary)?;
            Ok(UpdateOutcome::Updated {
                previous_version: current_version.to_string(),
                installed_version: target_version.to_string(),
                executable_path,
            })
        }
        UpdateInstallTarget::CargoInstall {
            executable_path,
            install_root,
        } => {
            install_update_with_cargo(target_version, &install_root).await?;
            Ok(UpdateOutcome::Updated {
                previous_version: current_version.to_string(),
                installed_version: target_version.to_string(),
                executable_path,
            })
        }
    }
}

fn update_check_outcome(
    current_version: ReleaseVersion,
    release_version: ReleaseVersion,
) -> UpdateOutcome {
    match current_version.cmp(&release_version) {
        std::cmp::Ordering::Less => UpdateOutcome::UpdateAvailable {
            current_version: current_version.to_string(),
            release_version: release_version.to_string(),
        },
        std::cmp::Ordering::Equal => UpdateOutcome::UpToDate {
            current_version: current_version.to_string(),
            release_version: release_version.to_string(),
        },
        std::cmp::Ordering::Greater => UpdateOutcome::CurrentNewer {
            current_version: current_version.to_string(),
            release_version: release_version.to_string(),
        },
    }
}

fn update_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(UPDATE_REQUEST_TIMEOUT)
        .user_agent(format!("codex-switch/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("Failed to build update HTTP client")
}

async fn fetch_latest_release(client: &reqwest::Client) -> Result<GitHubRelease> {
    fetch_release(client, &format!("{GITHUB_RELEASES_API}/latest")).await
}

async fn fetch_release_by_tag(client: &reqwest::Client, tag: &str) -> Result<GitHubRelease> {
    fetch_release(
        client,
        &format!("{GITHUB_RELEASES_API}/tags/{}", urlencoding::encode(tag)),
    )
    .await
}

async fn fetch_release(client: &reqwest::Client, url: &str) -> Result<GitHubRelease> {
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("Failed to request GitHub release metadata: {url}"))?;
    if response.status() == StatusCode::NOT_FOUND {
        anyhow::bail!("GitHub release not found: {url}");
    }
    response
        .error_for_status()
        .with_context(|| format!("GitHub release metadata request failed: {url}"))?
        .json::<GitHubRelease>()
        .await
        .context("Failed to parse GitHub release metadata")
}

async fn download_asset(client: &reqwest::Client, url: &str, name: &str) -> Result<Vec<u8>> {
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("Failed to download release asset {name}"))?;
    let response = response
        .error_for_status()
        .with_context(|| format!("Release asset download failed for {name}"))?;
    let content_length = response.content_length();
    validate_asset_content_length(name, content_length)?;
    read_limited_asset_response(response, name, content_length).await
}

fn validate_asset_content_length(name: &str, content_length: Option<u64>) -> Result<()> {
    if let Some(content_length) = content_length
        && content_length == 0
    {
        anyhow::bail!("Release asset {name} is empty");
    }
    if let Some(content_length) = content_length
        && content_length > MAX_UPDATE_ASSET_SIZE
    {
        anyhow::bail!(
            "Release asset {name} is too large: {content_length} bytes exceeds {MAX_UPDATE_ASSET_SIZE} bytes"
        );
    }
    Ok(())
}

fn validate_asset_size(name: &str, size: usize) -> Result<()> {
    let size = u64::try_from(size).context("Downloaded release asset size does not fit in u64")?;
    if size == 0 {
        anyhow::bail!("Release asset {name} is empty");
    }
    if size > MAX_UPDATE_ASSET_SIZE {
        anyhow::bail!(
            "Release asset {name} is too large: {size} bytes exceeds {MAX_UPDATE_ASSET_SIZE} bytes"
        );
    }
    Ok(())
}

async fn read_limited_asset_response(
    mut response: reqwest::Response,
    name: &str,
    content_length: Option<u64>,
) -> Result<Vec<u8>> {
    let capacity = content_length
        .and_then(|content_length| usize::try_from(content_length).ok())
        .unwrap_or(0);
    let mut binary = Vec::with_capacity(capacity);
    let mut progress = DownloadProgress::new(name, content_length);
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("Failed to read release asset {name}"))?
    {
        let next_size = binary
            .len()
            .checked_add(chunk.len())
            .context("Downloaded release asset size overflowed usize")?;
        validate_asset_size(name, next_size)?;
        binary.extend_from_slice(&chunk);
        progress.advance(chunk.len());
    }
    validate_asset_size(name, binary.len())?;
    progress.finish();
    Ok(binary)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DownloadProgressMode {
    Terminal,
    Plain,
}

#[derive(Debug)]
struct DownloadProgress<'a> {
    name: &'a str,
    total: Option<u64>,
    downloaded: u64,
    mode: DownloadProgressMode,
    last_render: Instant,
    rendered_terminal_line: bool,
    finished: bool,
}

impl<'a> DownloadProgress<'a> {
    fn new(name: &'a str, total: Option<u64>) -> Self {
        let mode = if io::stderr().is_terminal() {
            DownloadProgressMode::Terminal
        } else {
            DownloadProgressMode::Plain
        };
        let mut progress = Self {
            name,
            total,
            downloaded: 0,
            mode,
            last_render: Instant::now(),
            rendered_terminal_line: false,
            finished: false,
        };
        progress.render(false);
        progress
    }

    fn advance(&mut self, chunk_len: usize) {
        self.downloaded = self
            .downloaded
            .saturating_add(u64::try_from(chunk_len).unwrap_or(u64::MAX));
        if self.mode == DownloadProgressMode::Terminal
            && self.last_render.elapsed() >= UPDATE_PROGRESS_INTERVAL
        {
            self.render(false);
        }
    }

    fn finish(&mut self) {
        self.render(true);
        self.finished = true;
    }

    fn render(&mut self, done: bool) {
        let line = format_download_progress(self.name, self.downloaded, self.total, done);
        match self.mode {
            DownloadProgressMode::Terminal => {
                let mut stderr = io::stderr().lock();
                let _ = write!(stderr, "\r\x1b[2K{line}");
                if done {
                    let _ = writeln!(stderr);
                }
                self.rendered_terminal_line = true;
            }
            DownloadProgressMode::Plain => {
                if done || self.downloaded == 0 {
                    let mut stderr = io::stderr().lock();
                    let _ = writeln!(stderr, "{line}");
                }
            }
        }
        self.last_render = Instant::now();
    }
}

impl Drop for DownloadProgress<'_> {
    fn drop(&mut self) {
        if self.mode == DownloadProgressMode::Terminal
            && self.rendered_terminal_line
            && !self.finished
        {
            let _ = writeln!(io::stderr());
        }
    }
}

fn format_download_progress(name: &str, downloaded: u64, total: Option<u64>, done: bool) -> String {
    let verb = if done { "Downloaded" } else { "Downloading" };
    match total {
        Some(total) if total > 0 => {
            let percent = downloaded.min(total) as f64 * 100.0 / total as f64;
            format!(
                "{verb} {name}: [{}] {percent:.1}% {} / {}",
                format_progress_bar(downloaded, total),
                format_bytes(downloaded),
                format_bytes(total)
            )
        }
        _ => format!("{verb} {name}: {}", format_bytes(downloaded)),
    }
}

fn format_progress_bar(downloaded: u64, total: u64) -> String {
    if total == 0 {
        return UPDATE_PROGRESS_EMPTY.repeat(UPDATE_PROGRESS_BAR_WIDTH);
    }

    let filled = ((u128::from(downloaded.min(total)) * UPDATE_PROGRESS_BAR_WIDTH as u128)
        / u128::from(total)) as usize;
    let empty = UPDATE_PROGRESS_BAR_WIDTH.saturating_sub(filled);
    format!(
        "{}{}",
        UPDATE_PROGRESS_FILLED.repeat(filled),
        UPDATE_PROGRESS_EMPTY.repeat(empty)
    )
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;

    if bytes < KIB {
        format!("{bytes} B")
    } else if bytes < MIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else if bytes < GIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    }
}

fn release_asset<'a>(release: &'a GitHubRelease, name: &str) -> Result<&'a GitHubReleaseAsset> {
    release
        .assets
        .iter()
        .find(|asset| asset.name == name)
        .with_context(|| format!("Release {} is missing asset {name}", release.tag_name))
}

fn current_platform_asset_name() -> Result<&'static str> {
    platform_asset_name(std::env::consts::OS, std::env::consts::ARCH).with_context(|| {
        format!(
            "Unsupported platform for codex-switch update: {}-{}",
            std::env::consts::ARCH,
            std::env::consts::OS
        )
    })
}

fn platform_asset_name(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("codex-switch-x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Some("codex-switch-aarch64-unknown-linux-musl"),
        ("macos", "aarch64") => Some("codex-switch-aarch64-apple-darwin"),
        _ => None,
    }
}

fn normalize_release_tag(version: &str) -> Result<String> {
    let version = parse_release_version(version)?;
    Ok(format!("v{version}"))
}

fn parse_release_version(value: &str) -> Result<ReleaseVersion> {
    let value = value.trim();
    let value = value.strip_prefix('v').unwrap_or(value);
    let mut parts = value.split('.');
    let major = parse_version_part(parts.next(), value)?;
    let minor = parse_version_part(parts.next(), value)?;
    let patch = parse_version_part(parts.next(), value)?;
    if parts.next().is_some() {
        anyhow::bail!("Invalid release version: {value}");
    }
    Ok(ReleaseVersion {
        major,
        minor,
        patch,
    })
}

fn parse_version_part(part: Option<&str>, full_version: &str) -> Result<u64> {
    let Some(part) = part else {
        anyhow::bail!("Invalid release version: {full_version}");
    };
    if part.is_empty() || !part.chars().all(|value| value.is_ascii_digit()) {
        anyhow::bail!("Invalid release version: {full_version}");
    }
    part.parse::<u64>()
        .with_context(|| format!("Invalid release version: {full_version}"))
}

fn verify_asset_digest(binary: &[u8], asset: &GitHubReleaseAsset) -> Result<()> {
    let expected = parse_asset_sha256_digest(asset)?;
    let actual = sha256_hex(binary);
    if expected != actual {
        anyhow::bail!("Checksum verification failed: expected {expected}, got {actual}");
    }
    Ok(())
}

fn parse_asset_sha256_digest(asset: &GitHubReleaseAsset) -> Result<String> {
    let digest = asset
        .digest
        .as_deref()
        .with_context(|| format!("Release asset {} does not expose a digest", asset.name))?;
    let Some(checksum) = digest.strip_prefix("sha256:") else {
        anyhow::bail!(
            "Release asset {} uses unsupported digest format: {digest}",
            asset.name
        );
    };
    if checksum.len() != 64 || !checksum.chars().all(|value| value.is_ascii_hexdigit()) {
        anyhow::bail!("Release asset {} has an invalid SHA-256 digest", asset.name);
    }
    Ok(checksum.to_ascii_lowercase())
}

fn sha256_hex(content: &[u8]) -> String {
    let digest = Sha256::digest(content);
    let mut result = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(result, "{byte:02x}");
    }
    result
}

fn current_update_install_target() -> Result<UpdateInstallTarget> {
    let executable_path =
        std::env::current_exe().context("Failed to resolve current executable path")?;
    update_install_target_for_executable_path(executable_path)
}

fn update_install_target_for_executable_path(
    executable_path: PathBuf,
) -> Result<UpdateInstallTarget> {
    update_install_target_for_executable_path_with_cargo_bin_dirs(
        executable_path,
        &cargo_bin_dirs(),
    )
}

fn update_install_target_for_executable_path_with_cargo_bin_dirs(
    executable_path: PathBuf,
    cargo_bin_dirs: &[PathBuf],
) -> Result<UpdateInstallTarget> {
    match install_target_for_executable_path_with_cargo_bin_dirs(executable_path, cargo_bin_dirs) {
        InstallTarget::ReleaseBinary(executable_path) => {
            Ok(UpdateInstallTarget::ReleaseBinary(executable_path))
        }
        InstallTarget::CargoInstall {
            executable_path,
            install_root,
        } => Ok(UpdateInstallTarget::CargoInstall {
            executable_path,
            install_root,
        }),
        InstallTarget::CargoBuildArtifact(executable_path) => {
            anyhow::bail!(
                "Current executable appears to be a Cargo build artifact at {}. Install codex-switch before using `codex-switch update`.",
                executable_path.display()
            );
        }
        InstallTarget::UnsupportedCargoInstall {
            executable_path,
            install_root,
            package_id,
        } => {
            anyhow::bail!(
                "{}",
                unsupported_cargo_install_message(&executable_path, &install_root, &package_id)
            );
        }
        InstallTarget::InvalidCargoTracking {
            executable_path,
            install_root,
            message,
        } => {
            anyhow::bail!(
                "Current executable appears to be in a Cargo install root at {}, but Cargo tracking metadata could not be read from {}. Refusing to overwrite it as a release binary: {message}",
                executable_path.display(),
                install_root.join(".crates2.json").display()
            );
        }
    }
}

fn install_target_for_executable_path_with_cargo_bin_dirs(
    executable_path: PathBuf,
    cargo_bin_dirs: &[PathBuf],
) -> InstallTarget {
    match cargo_install_root_for_path(&executable_path, cargo_bin_dirs) {
        CargoInstallLookup::Install(cargo_install) => {
            return match cargo_install.source {
                CargoTrackedInstallSource::CratesIo => InstallTarget::CargoInstall {
                    executable_path,
                    install_root: cargo_install.install_root,
                },
                CargoTrackedInstallSource::Other => InstallTarget::UnsupportedCargoInstall {
                    executable_path,
                    install_root: cargo_install.install_root,
                    package_id: cargo_install.package_id,
                },
            };
        }
        CargoInstallLookup::InvalidTracking(problem) => {
            return InstallTarget::InvalidCargoTracking {
                executable_path,
                install_root: problem.install_root,
                message: problem.message,
            };
        }
        CargoInstallLookup::NotCargoInstall => {}
    }
    if is_cargo_build_artifact_path(&executable_path) {
        return InstallTarget::CargoBuildArtifact(executable_path);
    }
    InstallTarget::ReleaseBinary(executable_path)
}

fn unsupported_cargo_install_message(
    executable_path: &Path,
    install_root: &Path,
    package_id: &str,
) -> String {
    let reinstall_args = cargo_reinstall_args(install_root);
    let reinstall_command = format_command(OsStr::new("cargo"), &reinstall_args);
    format!(
        "Current executable appears to be installed by Cargo from an unsupported source at {} ({package_id}). `codex-switch update` can update crates.io Cargo installs only; update this source install manually or reinstall from crates.io with `{reinstall_command}`.",
        executable_path.display()
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CargoInstallRoot {
    install_root: PathBuf,
    package_id: String,
    source: CargoTrackedInstallSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CargoTrackingProblem {
    install_root: PathBuf,
    message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CargoInstallLookup {
    NotCargoInstall,
    Install(CargoInstallRoot),
    InvalidTracking(CargoTrackingProblem),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CargoTrackingLookup {
    NoTrackingFile,
    NoMatchingInstall,
    MatchingInstall(CargoTrackedInstall),
    Invalid(CargoTrackingProblem),
}

fn cargo_install_root_for_path(path: &Path, cargo_bin_dirs: &[PathBuf]) -> CargoInstallLookup {
    if !package_executable_name_matches(path) {
        return CargoInstallLookup::NotCargoInstall;
    }
    if let Some(cargo_bin_dir) = matching_cargo_bin_dir(path, cargo_bin_dirs)
        && let Some(install_root) = cargo_bin_dir.parent()
    {
        return match cargo_tracking_file_install_for_bin(install_root, env!("CARGO_PKG_NAME")) {
            CargoTrackingLookup::MatchingInstall(tracked_install) => {
                CargoInstallLookup::Install(CargoInstallRoot {
                    install_root: install_root.to_path_buf(),
                    package_id: tracked_install.package_id,
                    source: tracked_install.source,
                })
            }
            CargoTrackingLookup::Invalid(problem) => CargoInstallLookup::InvalidTracking(problem),
            CargoTrackingLookup::NoTrackingFile | CargoTrackingLookup::NoMatchingInstall => {
                CargoInstallLookup::NotCargoInstall
            }
        };
    }
    tracked_cargo_install_root_for_path(path)
}

fn tracked_cargo_install_root_for_path(path: &Path) -> CargoInstallLookup {
    let Some(bin_dir) = path.parent() else {
        return CargoInstallLookup::NotCargoInstall;
    };
    if bin_dir.file_name() != Some(OsStr::new("bin")) {
        return CargoInstallLookup::NotCargoInstall;
    }
    let Some(install_root) = bin_dir.parent().map(Path::to_path_buf) else {
        return CargoInstallLookup::NotCargoInstall;
    };
    match cargo_tracking_file_install_for_bin(&install_root, env!("CARGO_PKG_NAME")) {
        CargoTrackingLookup::MatchingInstall(tracked_install) => {
            CargoInstallLookup::Install(CargoInstallRoot {
                install_root,
                package_id: tracked_install.package_id,
                source: tracked_install.source,
            })
        }
        CargoTrackingLookup::Invalid(problem) => CargoInstallLookup::InvalidTracking(problem),
        CargoTrackingLookup::NoTrackingFile | CargoTrackingLookup::NoMatchingInstall => {
            CargoInstallLookup::NotCargoInstall
        }
    }
}

fn cargo_tracking_problem(install_root: &Path, message: String) -> CargoTrackingProblem {
    CargoTrackingProblem {
        install_root: install_root.to_path_buf(),
        message,
    }
}

fn package_executable_name_matches(path: &Path) -> bool {
    path.file_name() == Some(OsStr::new(env!("CARGO_PKG_NAME")))
}

fn cargo_tracking_file_install_for_bin(install_root: &Path, bin_name: &str) -> CargoTrackingLookup {
    let tracking_path = install_root.join(".crates2.json");
    let content = match fs::read(&tracking_path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return CargoTrackingLookup::NoTrackingFile;
        }
        Err(error) => {
            return CargoTrackingLookup::Invalid(cargo_tracking_problem(
                install_root,
                format!("failed to read {}: {error}", tracking_path.display()),
            ));
        }
    };
    let tracking = match serde_json::from_slice::<Value>(&content) {
        Ok(tracking) => tracking,
        Err(error) => {
            return CargoTrackingLookup::Invalid(cargo_tracking_problem(
                install_root,
                format!("failed to parse {}: {error}", tracking_path.display()),
            ));
        }
    };
    let Some(installs_object) = tracking.get("installs").and_then(Value::as_object) else {
        return CargoTrackingLookup::Invalid(cargo_tracking_problem(
            install_root,
            format!("{} is missing an installs object", tracking_path.display()),
        ));
    };

    let mut installs = Vec::new();
    for (package_id, entry) in installs_object {
        if !cargo_tracking_package_id_matches(package_id, env!("CARGO_PKG_NAME")) {
            continue;
        }
        let Some(bins) = entry.get("bins").and_then(Value::as_array) else {
            return CargoTrackingLookup::Invalid(cargo_tracking_problem(
                install_root,
                format!("matching Cargo install entry {package_id} has missing or invalid bins"),
            ));
        };
        let mut matches_bin = false;
        for bin in bins {
            let Some(bin) = bin.as_str() else {
                return CargoTrackingLookup::Invalid(cargo_tracking_problem(
                    install_root,
                    format!("matching Cargo install entry {package_id} has a non-string bin"),
                ));
            };
            if bin == bin_name {
                matches_bin = true;
            }
        }
        if matches_bin {
            installs.push(CargoTrackedInstall {
                package_id: package_id.clone(),
                source: cargo_tracking_package_source(package_id),
            });
        }
    }
    installs.sort_by(|left, right| left.package_id.cmp(&right.package_id));
    installs
        .iter()
        .find(|install| install.source == CargoTrackedInstallSource::Other)
        .cloned()
        .or_else(|| installs.into_iter().next())
        .map_or(CargoTrackingLookup::NoMatchingInstall, |install| {
            CargoTrackingLookup::MatchingInstall(install)
        })
}

fn cargo_tracking_package_id_matches(package_id: &str, package_name: &str) -> bool {
    package_id
        .strip_prefix(package_name)
        .is_some_and(|rest| rest.starts_with(' '))
}

fn cargo_tracking_package_source(package_id: &str) -> CargoTrackedInstallSource {
    if cargo_tracking_package_source_value(package_id) == Some(CRATES_IO_REGISTRY_SOURCE) {
        CargoTrackedInstallSource::CratesIo
    } else {
        CargoTrackedInstallSource::Other
    }
}

fn cargo_tracking_package_source_value(package_id: &str) -> Option<&str> {
    package_id.rsplit_once(" (")?.1.strip_suffix(')')
}

async fn install_update_with_cargo(
    target_version: ReleaseVersion,
    install_root: &Path,
) -> Result<()> {
    let cargo = cargo_executable();
    let args = cargo_install_args(target_version, install_root);
    let command = format_command(&cargo, &args);
    let status = tokio::process::Command::new(&cargo)
        .args(&args)
        .status()
        .await
        .with_context(|| format!("Failed to run Cargo update command: `{command}`"))?;

    if !status.success() {
        anyhow::bail!("Cargo update command failed with status {status}: `{command}`");
    }
    Ok(())
}

fn cargo_executable() -> OsString {
    non_empty_var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"))
}

fn cargo_install_args(target_version: ReleaseVersion, install_root: &Path) -> Vec<OsString> {
    let mut args = cargo_reinstall_args(install_root);
    args.push(OsString::from("--version"));
    args.push(OsString::from(format!("={target_version}")));
    args
}

fn cargo_reinstall_args(install_root: &Path) -> Vec<OsString> {
    vec![
        OsString::from("install"),
        OsString::from("codex-switch"),
        OsString::from("--locked"),
        OsString::from("--force"),
        OsString::from("--root"),
        install_root.as_os_str().to_os_string(),
    ]
}

fn format_command(command: &OsStr, args: &[OsString]) -> String {
    let mut formatted = format_command_part(command);
    for arg in args {
        formatted.push(' ');
        formatted.push_str(&format_command_part(arg));
    }
    formatted
}

fn format_command_part(value: &OsStr) -> String {
    let value = value.to_string_lossy();
    if value.is_empty() {
        return "''".to_string();
    }
    if value.chars().all(is_unquoted_command_char) {
        return value.into_owned();
    }

    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for value_char in value.chars() {
        if value_char == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(value_char);
        }
    }
    quoted.push('\'');
    quoted
}

fn is_unquoted_command_char(value: char) -> bool {
    value.is_ascii_alphanumeric() || matches!(value, '/' | '.' | '_' | '-' | '+' | ':' | '=')
}

fn install_update_at(executable_path: &Path, binary: &[u8]) -> Result<()> {
    install_binary_at(executable_path, binary).map_err(|err| {
        anyhow::anyhow!(
            "Failed to install update to {}: {err:#}\n{}",
            executable_path.display(),
            install_failure_hint(executable_path)
        )
    })
}

fn matching_cargo_bin_dir<'a>(path: &Path, cargo_bin_dirs: &'a [PathBuf]) -> Option<&'a Path> {
    let parent = path.parent()?;
    cargo_bin_dirs
        .iter()
        .find(|cargo_bin_dir| cargo_bin_dir.as_path() == parent)
        .map(PathBuf::as_path)
}

fn is_cargo_build_artifact_path(path: &Path) -> bool {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    path.starts_with(manifest_dir.join("target"))
}

fn cargo_bin_dirs() -> Vec<PathBuf> {
    let mut bin_dirs = Vec::new();
    if let Some(install_root) = non_empty_var_os("CARGO_INSTALL_ROOT") {
        bin_dirs.push(PathBuf::from(install_root).join("bin"));
    }
    if let Some(cargo_home) = non_empty_var_os("CARGO_HOME") {
        bin_dirs.push(PathBuf::from(cargo_home).join("bin"));
    } else if let Some(home) = dirs::home_dir() {
        bin_dirs.push(home.join(".cargo").join("bin"));
    }
    bin_dirs.dedup();
    bin_dirs
}

fn non_empty_var_os(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

fn install_failure_hint(path: &Path) -> String {
    if is_cargo_build_artifact_path(path) {
        "Install a release binary before using `codex-switch update`.".to_string()
    } else {
        "The current executable path may not be writable. Re-run your install script or install the release binary with elevated permissions.".to_string()
    }
}

fn install_binary_at(path: &Path, binary: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .context("Executable path has no parent directory")?;
    let temp_path = update_temp_path(path);
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp_path).with_context(|| {
            format!("Failed to create temporary binary in {}", parent.display())
        })?;
        file.write_all(binary).with_context(|| {
            format!("Failed to write temporary binary: {}", temp_path.display())
        })?;
        file.sync_all()
            .with_context(|| format!("Failed to sync temporary binary: {}", temp_path.display()))?;
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temp_path, fs::Permissions::from_mode(0o755)).with_context(
                || {
                    format!(
                        "Failed to set temporary binary permissions: {}",
                        temp_path.display()
                    )
                },
            )?;
        }
        fs::rename(&temp_path, path).with_context(|| {
            format!(
                "Failed to replace {} with {}",
                path.display(),
                temp_path.display()
            )
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn update_temp_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|file_name| file_name.to_str())
        .unwrap_or("codex-switch");
    path.with_file_name(format!(".{file_name}.update-{}.tmp", Uuid::new_v4()))
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::path::{Path, PathBuf};

    use uuid::Uuid;

    use super::{
        GitHubRelease, GitHubReleaseAsset, InstallTarget, UpdateInstallTarget, UpdateOutcome,
        cargo_install_args, format_bytes, format_command, format_download_progress,
        format_progress_bar, install_binary_at,
        install_target_for_executable_path_with_cargo_bin_dirs, is_cargo_build_artifact_path,
        normalize_release_tag, parse_asset_sha256_digest, parse_release_version,
        platform_asset_name, release_asset, sha256_hex, unsupported_cargo_install_message,
        update_check_outcome, update_install_target_for_executable_path_with_cargo_bin_dirs,
        update_temp_path, validate_asset_content_length, validate_asset_size, verify_asset_digest,
    };

    #[test]
    fn version_parsing_accepts_plain_and_tag_versions() {
        assert_eq!(normalize_release_tag("0.1.10").unwrap(), "v0.1.10");
        assert_eq!(normalize_release_tag("v0.1.10").unwrap(), "v0.1.10");
        assert!(parse_release_version("0.1").is_err());
        assert!(parse_release_version("0.1.x").is_err());
        assert!(parse_release_version("0.1.2.3").is_err());
    }

    #[test]
    fn version_comparison_orders_patch_minor_and_major() {
        assert!(parse_release_version("0.1.10").unwrap() > parse_release_version("0.1.9").unwrap());
        assert!(parse_release_version("0.2.0").unwrap() > parse_release_version("0.1.99").unwrap());
        assert!(
            parse_release_version("1.0.0").unwrap() > parse_release_version("0.99.99").unwrap()
        );
    }

    #[test]
    fn update_check_distinguishes_newer_current_version() {
        assert!(matches!(
            update_check_outcome(
                parse_release_version("0.1.9").unwrap(),
                parse_release_version("0.1.10").unwrap()
            ),
            UpdateOutcome::UpdateAvailable { .. }
        ));
        assert!(matches!(
            update_check_outcome(
                parse_release_version("0.1.10").unwrap(),
                parse_release_version("0.1.10").unwrap()
            ),
            UpdateOutcome::UpToDate { .. }
        ));
        assert!(matches!(
            update_check_outcome(
                parse_release_version("0.1.11").unwrap(),
                parse_release_version("0.1.10").unwrap()
            ),
            UpdateOutcome::CurrentNewer { .. }
        ));
    }

    #[test]
    fn platform_asset_selection_matches_release_names() {
        assert_eq!(
            platform_asset_name("linux", "x86_64"),
            Some("codex-switch-x86_64-unknown-linux-musl")
        );
        assert_eq!(
            platform_asset_name("linux", "aarch64"),
            Some("codex-switch-aarch64-unknown-linux-musl")
        );
        assert_eq!(
            platform_asset_name("macos", "aarch64"),
            Some("codex-switch-aarch64-apple-darwin")
        );
        assert_eq!(platform_asset_name("macos", "x86_64"), None);
        assert_eq!(platform_asset_name("windows", "x86_64"), None);
    }

    #[test]
    fn release_asset_finds_exact_asset_name() {
        let release = GitHubRelease {
            tag_name: "v0.1.10".to_string(),
            assets: vec![GitHubReleaseAsset {
                name: "codex-switch-x86_64-unknown-linux-musl".to_string(),
                browser_download_url: "https://example.invalid/bin".to_string(),
                digest: Some(
                    "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                        .to_string(),
                ),
            }],
        };

        assert!(release_asset(&release, "codex-switch-x86_64-unknown-linux-musl").is_ok());
        assert!(release_asset(&release, "codex-switch-aarch64-unknown-linux-musl").is_err());
    }

    #[test]
    fn cargo_build_artifact_path_is_not_self_updateable() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("debug")
            .join("codex-switch");

        assert!(is_cargo_build_artifact_path(&path));
        assert!(matches!(
            install_target_for_executable_path_with_cargo_bin_dirs(path.clone(), &[]),
            InstallTarget::CargoBuildArtifact(_)
        ));
        assert!(update_install_target_for_executable_path_with_cargo_bin_dirs(path, &[]).is_err());
        assert!(!is_cargo_build_artifact_path(
            PathBuf::from("/usr/local/bin/codex-switch").as_path()
        ));
    }

    #[test]
    fn cargo_install_path_uses_cargo_install_target() {
        let install_root = temp_cargo_root("tracked-default-root");
        write_cargo_tracking_file(
            &install_root,
            r#"{"installs":{"codex-switch 0.1.19 (registry+https://github.com/rust-lang/crates.io-index)":{"bins":["codex-switch"]}}}"#,
        );
        let path = install_root.join("bin").join("codex-switch");

        assert_eq!(
            update_install_target_for_executable_path_with_cargo_bin_dirs(
                path.clone(),
                &[install_root.join("bin")]
            )
            .unwrap(),
            UpdateInstallTarget::CargoInstall {
                executable_path: path,
                install_root: install_root.clone(),
            }
        );
        fs::remove_dir_all(install_root).expect("test root should be removed");
    }

    #[test]
    fn cargo_install_path_without_tracking_is_release_binary_target() {
        let install_root = temp_cargo_root("untracked-default-root");
        let path = install_root.join("bin").join("codex-switch");

        assert_eq!(
            install_target_for_executable_path_with_cargo_bin_dirs(
                path.clone(),
                &[install_root.join("bin")]
            ),
            InstallTarget::ReleaseBinary(path)
        );
        fs::remove_dir_all(install_root).expect("test root should be removed");
    }

    #[test]
    fn cargo_install_path_ignores_renamed_binary() {
        let install_root = PathBuf::from("/home/user/.cargo");
        let path = install_root.join("bin").join("cs");

        assert_eq!(
            install_target_for_executable_path_with_cargo_bin_dirs(
                path.clone(),
                &[install_root.join("bin")]
            ),
            InstallTarget::ReleaseBinary(path)
        );
    }

    #[test]
    fn cargo_install_root_path_uses_matching_cargo_install_root() {
        let default_root = PathBuf::from("/home/user/.cargo");
        let custom_root = temp_cargo_root("tracked-custom-root");
        write_cargo_tracking_file(
            &custom_root,
            r#"{"installs":{"codex-switch 0.1.19 (registry+https://github.com/rust-lang/crates.io-index)":{"bins":["codex-switch"]}}}"#,
        );
        let path = custom_root.join("bin").join("codex-switch");

        assert_eq!(
            install_target_for_executable_path_with_cargo_bin_dirs(
                path.clone(),
                &[default_root.join("bin"), custom_root.join("bin")]
            ),
            InstallTarget::CargoInstall {
                executable_path: path,
                install_root: custom_root.clone(),
            }
        );
        fs::remove_dir_all(custom_root).expect("test root should be removed");
    }

    #[test]
    fn cargo_tracking_file_detects_unconfigured_install_root() {
        let install_root = temp_cargo_root("unconfigured-root");
        write_cargo_tracking_file(
            &install_root,
            r#"{"installs":{"codex-switch 0.1.19 (registry+https://github.com/rust-lang/crates.io-index)":{"bins":["codex-switch"]}}}"#,
        );
        let path = install_root.join("bin").join("codex-switch");

        assert_eq!(
            install_target_for_executable_path_with_cargo_bin_dirs(path.clone(), &[]),
            InstallTarget::CargoInstall {
                executable_path: path,
                install_root: install_root.clone(),
            }
        );
        fs::remove_dir_all(install_root).expect("test root should be removed");
    }

    #[test]
    fn cargo_tracking_file_ignores_other_package_with_same_bin() {
        let install_root = temp_cargo_root("other-package-same-bin");
        write_cargo_tracking_file(
            &install_root,
            r#"{"installs":{"codex-switcher 0.1.19 (registry+https://github.com/rust-lang/crates.io-index)":{"bins":["codex-switch"]}}}"#,
        );
        let path = install_root.join("bin").join("codex-switch");

        assert_eq!(
            install_target_for_executable_path_with_cargo_bin_dirs(path.clone(), &[]),
            InstallTarget::ReleaseBinary(path)
        );
        fs::remove_dir_all(install_root).expect("test root should be removed");
    }

    #[test]
    fn invalid_cargo_tracking_file_is_not_release_binary_target() {
        let install_root = temp_cargo_root("invalid-tracking");
        write_cargo_tracking_file(&install_root, "{");
        let path = install_root.join("bin").join("codex-switch");

        let target = install_target_for_executable_path_with_cargo_bin_dirs(
            path.clone(),
            &[install_root.join("bin")],
        );

        assert!(matches!(
            &target,
            InstallTarget::InvalidCargoTracking { .. }
        ));
        assert!(
            update_install_target_for_executable_path_with_cargo_bin_dirs(
                path,
                &[install_root.join("bin")]
            )
            .unwrap_err()
            .to_string()
            .contains("Refusing to overwrite it as a release binary")
        );
        fs::remove_dir_all(install_root).expect("test root should be removed");
    }

    #[test]
    fn cargo_tracking_file_ignores_malformed_unrelated_entries() {
        let install_root = temp_cargo_root("malformed-unrelated");
        write_cargo_tracking_file(
            &install_root,
            r#"{"installs":{"other-tool 0.1.0 (registry+https://github.com/rust-lang/crates.io-index)":{"bins":42},"codex-switch 0.1.19 (registry+https://github.com/rust-lang/crates.io-index)":{"bins":["codex-switch"]}}}"#,
        );
        let path = install_root.join("bin").join("codex-switch");

        assert_eq!(
            install_target_for_executable_path_with_cargo_bin_dirs(path.clone(), &[]),
            InstallTarget::CargoInstall {
                executable_path: path,
                install_root: install_root.clone(),
            }
        );
        fs::remove_dir_all(install_root).expect("test root should be removed");
    }

    #[test]
    fn cargo_tracking_file_rejects_malformed_matching_entry() {
        let install_root = temp_cargo_root("malformed-matching");
        write_cargo_tracking_file(
            &install_root,
            r#"{"installs":{"codex-switch 0.1.19 (registry+https://github.com/rust-lang/crates.io-index)":{"bins":42}}}"#,
        );
        let path = install_root.join("bin").join("codex-switch");

        assert!(matches!(
            install_target_for_executable_path_with_cargo_bin_dirs(path, &[]),
            InstallTarget::InvalidCargoTracking { .. }
        ));
        fs::remove_dir_all(install_root).expect("test root should be removed");
    }

    #[test]
    fn cargo_tracking_file_rejects_non_registry_source_install() {
        let install_root = temp_cargo_root("path-source");
        write_cargo_tracking_file(
            &install_root,
            r#"{"installs":{"codex-switch 0.1.19 (path+file:///workspace/codex-switch)":{"bins":["codex-switch"]}}}"#,
        );
        let path = install_root.join("bin").join("codex-switch");

        let target = install_target_for_executable_path_with_cargo_bin_dirs(path, &[]);

        assert!(matches!(
            &target,
            InstallTarget::UnsupportedCargoInstall { .. }
        ));
        assert!(
            update_install_target_for_executable_path_with_cargo_bin_dirs(
                install_root.join("bin").join("codex-switch"),
                &[]
            )
            .is_err()
        );
        fs::remove_dir_all(install_root).expect("test root should be removed");
    }

    #[test]
    fn unsupported_cargo_install_message_keeps_install_root() {
        let executable_path = PathBuf::from("/opt/codex-switch/bin/codex-switch");
        let install_root = PathBuf::from("/opt/codex-switch");
        let message = unsupported_cargo_install_message(
            &executable_path,
            &install_root,
            "codex-switch 0.1.19 (path+file:///workspace/codex-switch)",
        );

        assert!(message.contains("/opt/codex-switch/bin/codex-switch"));
        assert!(message.contains("--root /opt/codex-switch"));
    }

    #[test]
    fn cargo_tracking_file_prefers_rejecting_ambiguous_source_install() {
        let install_root = temp_cargo_root("mixed-source");
        write_cargo_tracking_file(
            &install_root,
            r#"{"installs":{"codex-switch 0.1.19 (registry+https://github.com/rust-lang/crates.io-index)":{"bins":["codex-switch"]},"codex-switch 0.1.20 (git+https://github.com/seven332/codex-switch?rev=main)":{"bins":["codex-switch"]}}}"#,
        );
        let path = install_root.join("bin").join("codex-switch");

        let target = install_target_for_executable_path_with_cargo_bin_dirs(path, &[]);

        assert!(matches!(
            &target,
            InstallTarget::UnsupportedCargoInstall { .. }
        ));
        fs::remove_dir_all(install_root).expect("test root should be removed");
    }

    #[test]
    fn release_binary_path_uses_release_binary_target() {
        let path = PathBuf::from("/opt/codex-switch");

        assert_eq!(
            install_target_for_executable_path_with_cargo_bin_dirs(path.clone(), &[]),
            InstallTarget::ReleaseBinary(path)
        );
    }

    #[test]
    fn cargo_install_args_use_exact_target_version() {
        let install_root = PathBuf::from("/home/user/.cargo");
        let args = cargo_install_args(parse_release_version("0.1.19").unwrap(), &install_root)
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            args,
            [
                "install",
                "codex-switch",
                "--locked",
                "--force",
                "--root",
                "/home/user/.cargo",
                "--version",
                "=0.1.19"
            ]
        );
    }

    #[test]
    fn command_format_quotes_ambiguous_arguments() {
        let command = OsStr::new("/opt/Cargo Home/bin/cargo");
        let args = [
            OsString::from("install"),
            OsString::from("codex-switch"),
            OsString::from("--root"),
            OsString::from("/opt/codex switch/user's root"),
        ];

        assert_eq!(
            format_command(command, &args),
            "'/opt/Cargo Home/bin/cargo' install codex-switch --root '/opt/codex switch/user'\\''s root'"
        );
    }

    #[test]
    fn asset_digest_parser_accepts_sha256_digest() {
        let digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let asset = test_asset("codex-switch", Some(&format!("sha256:{digest}")));

        assert_eq!(
            parse_asset_sha256_digest(&asset).expect("digest should parse"),
            digest
        );
    }

    #[test]
    fn asset_digest_parser_rejects_missing_or_unsupported_digest() {
        assert!(parse_asset_sha256_digest(&test_asset("codex-switch", None)).is_err());
        assert_eq!(
            parse_asset_sha256_digest(&test_asset("codex-switch", Some("sha512:abcd")))
                .unwrap_err()
                .to_string(),
            "Release asset codex-switch uses unsupported digest format: sha512:abcd"
        );
        assert!(
            parse_asset_sha256_digest(&test_asset("codex-switch", Some("sha256:abcd"))).is_err()
        );
    }

    #[test]
    fn asset_digest_verification_rejects_mismatch() {
        let binary = b"codex-switch";
        let asset = test_asset(
            "codex-switch",
            Some(&format!("sha256:{}", sha256_hex(binary))),
        );

        verify_asset_digest(binary, &asset).unwrap();
        assert!(verify_asset_digest(b"other", &asset).is_err());
    }

    #[test]
    fn asset_size_limit_rejects_oversized_assets() {
        assert!(validate_asset_content_length("codex-switch", None).is_ok());
        assert!(validate_asset_content_length("codex-switch", Some(0)).is_err());
        assert!(validate_asset_content_length("codex-switch", Some(64 * 1024 * 1024)).is_ok());
        assert!(validate_asset_content_length("codex-switch", Some(64 * 1024 * 1024 + 1)).is_err());
        assert!(validate_asset_size("codex-switch", 0).is_err());
        assert!(validate_asset_size("codex-switch", 64 * 1024 * 1024).is_ok());
        assert!(validate_asset_size("codex-switch", 64 * 1024 * 1024 + 1).is_err());
    }

    #[test]
    fn download_progress_formats_known_total() {
        assert_eq!(
            format_download_progress("codex-switch", 512, Some(1024), false),
            format!(
                "Downloading codex-switch: [{}] 50.0% 512 B / 1.0 KiB",
                format_progress_bar(512, 1024)
            )
        );
        assert_eq!(
            format_download_progress("codex-switch", 2048, Some(1024), true),
            format!(
                "Downloaded codex-switch: [{}] 100.0% 2.0 KiB / 1.0 KiB",
                format_progress_bar(2048, 1024)
            )
        );
    }

    #[test]
    fn download_progress_formats_unknown_total() {
        assert_eq!(
            format_download_progress("codex-switch", 1024, None, false),
            "Downloading codex-switch: 1.0 KiB"
        );
        assert_eq!(
            format_download_progress("codex-switch", 1536, None, true),
            "Downloaded codex-switch: 1.5 KiB"
        );
    }

    #[test]
    fn byte_format_uses_binary_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MiB");
    }

    #[test]
    fn progress_bar_clamps_at_total() {
        assert_eq!(format_progress_bar(0, 0), "\u{2591}".repeat(24));
        assert_eq!(format_progress_bar(0, 100), "\u{2591}".repeat(24));
        assert_eq!(
            format_progress_bar(50, 100),
            format!("{}{}", "\u{2588}".repeat(12), "\u{2591}".repeat(12))
        );
        assert_eq!(format_progress_bar(200, 100), "\u{2588}".repeat(24));
    }

    #[test]
    fn install_binary_replaces_file_and_sets_executable_permissions() {
        let (dir, path) = temp_install_path("replace");
        fs::write(&path, b"old").expect("test should write old binary");

        install_binary_at(&path, b"new").expect("install should replace binary");

        assert_eq!(fs::read(&path).expect("test should read binary"), b"new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode, 0o755);
        }
        fs::remove_dir_all(dir).expect("temp dir should be removed");
    }

    #[test]
    fn install_binary_preserves_target_when_rename_fails() {
        let (dir, path) = temp_install_path("rename-fails");
        fs::create_dir(&path).expect("target directory should be created");

        assert!(install_binary_at(&path, b"new").is_err());

        assert!(path.is_dir());
        let temp_prefix = format!(
            ".{}.update-",
            path.file_name()
                .and_then(|file_name| file_name.to_str())
                .unwrap()
        );
        let leaked_temp = fs::read_dir(&dir)
            .expect("temp dir should list")
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&temp_prefix)
            });
        assert!(!leaked_temp);
        fs::remove_dir_all(dir).expect("temp dir should be removed");
    }

    #[test]
    fn temp_path_stays_next_to_executable() {
        let path = PathBuf::from("/tmp/codex-switch");
        let temp = update_temp_path(&path);

        assert_eq!(temp.parent(), path.parent());
        assert!(
            temp.file_name()
                .unwrap()
                .to_string_lossy()
                .contains("update")
        );
    }

    fn temp_install_path(name: &str) -> (PathBuf, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("codex-switch-update-{name}-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("temp dir should be created");
        let path = dir.join("codex-switch");
        (dir, path)
    }

    fn temp_cargo_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("codex-switch-cargo-{name}-{}", Uuid::new_v4()));
        fs::create_dir_all(root.join("bin")).expect("test should create cargo bin dir");
        root
    }

    fn write_cargo_tracking_file(install_root: &Path, content: &str) {
        fs::write(install_root.join(".crates2.json"), content)
            .expect("test should write cargo tracking file");
    }

    fn test_asset(name: &str, digest: Option<&str>) -> GitHubReleaseAsset {
        GitHubReleaseAsset {
            name: name.to_string(),
            browser_download_url: "https://example.invalid/bin".to_string(),
            digest: digest.map(ToString::to_string),
        }
    }
}
