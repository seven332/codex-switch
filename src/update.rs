use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::StatusCode;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

const GITHUB_RELEASES_API: &str = "https://api.github.com/repos/seven332/codex-switch/releases";
const UPDATE_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_UPDATE_ASSET_SIZE: u64 = 64 * 1024 * 1024;

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
    let executable_path = if options.check {
        None
    } else {
        Some(current_executable_update_target()?)
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

    let executable_path = executable_path.context("Update target was not resolved")?;
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
    }
    validate_asset_size(name, binary.len())?;
    Ok(binary)
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

fn current_executable_update_target() -> Result<PathBuf> {
    let executable_path =
        std::env::current_exe().context("Failed to resolve current executable path")?;
    if is_cargo_install_path(&executable_path) {
        anyhow::bail!(
            "Current executable appears to be installed by Cargo at {}. Use `cargo install codex-switch --locked --force` instead.",
            executable_path.display()
        );
    }
    if is_cargo_build_artifact_path(&executable_path) {
        anyhow::bail!(
            "Current executable appears to be a Cargo build artifact at {}. Install a release binary before using `codex-switch update`.",
            executable_path.display()
        );
    }
    Ok(executable_path)
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

fn is_cargo_install_path(path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    cargo_bin_dir().as_deref() == Some(parent)
}

fn is_cargo_build_artifact_path(path: &Path) -> bool {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    path.starts_with(manifest_dir.join("target"))
}

fn cargo_bin_dir() -> Option<PathBuf> {
    if let Some(cargo_home) = std::env::var_os("CARGO_HOME") {
        return Some(PathBuf::from(cargo_home).join("bin"));
    }
    dirs::home_dir().map(|home| home.join(".cargo").join("bin"))
}

fn install_failure_hint(path: &Path) -> String {
    if is_cargo_install_path(path) {
        "Use `cargo install codex-switch --locked --force` instead.".to_string()
    } else if is_cargo_build_artifact_path(path) {
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
    use std::fs;
    use std::path::PathBuf;

    use uuid::Uuid;

    use super::{
        GitHubRelease, GitHubReleaseAsset, UpdateOutcome, install_binary_at,
        is_cargo_build_artifact_path, normalize_release_tag, parse_asset_sha256_digest,
        parse_release_version, platform_asset_name, release_asset, sha256_hex,
        update_check_outcome, update_temp_path, validate_asset_content_length, validate_asset_size,
        verify_asset_digest,
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
        assert!(!is_cargo_build_artifact_path(
            PathBuf::from("/usr/local/bin/codex-switch").as_path()
        ));
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

    fn test_asset(name: &str, digest: Option<&str>) -> GitHubReleaseAsset {
        GitHubReleaseAsset {
            name: name.to_string(),
            browser_download_url: "https://example.invalid/bin".to_string(),
            digest: digest.map(ToString::to_string),
        }
    }
}
