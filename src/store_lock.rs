use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use uuid::Uuid;

const ACCOUNTS_LOCK_RETRY_DELAY: Duration = Duration::from_millis(250);
const ACCOUNTS_LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct AccountsFileLock {
    path: PathBuf,
    lock_id: String,
}

impl Drop for AccountsFileLock {
    fn drop(&mut self) {
        release_accounts_file_lock(&self.path, &self.lock_id);
    }
}

pub(crate) fn acquire_accounts_file_lock(path: &Path) -> Result<AccountsFileLock> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }

    // This coordinates codex-switch writers that share the same ~/.codex-switch
    // directory, including shared dev-container mounts. It cannot coordinate
    // processes that write through different filesystem paths.
    let lock_id = Uuid::new_v4().to_string();
    let deadline = Instant::now() + ACCOUNTS_LOCK_WAIT_TIMEOUT;
    loop {
        match create_accounts_lock_file(path, &lock_id) {
            Ok(()) => {
                return Ok(AccountsFileLock {
                    path: path.to_path_buf(),
                    lock_id,
                });
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                if Instant::now() >= deadline {
                    anyhow::bail!(
                        "Timed out waiting for accounts lock: {}. Another codex-switch process may be updating accounts.json, or a stale lock file may need to be removed manually.",
                        path.display()
                    );
                }
                thread::sleep(ACCOUNTS_LOCK_RETRY_DELAY);
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("Failed to create accounts lock: {}", path.display())
                });
            }
        }
    }
}

fn create_accounts_lock_file(path: &Path, lock_id: &str) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        write_accounts_lock_metadata(path, lock_id, &mut file)
    }

    #[cfg(not(unix))]
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        write_accounts_lock_metadata(path, lock_id, &mut file)
    }
}

fn write_accounts_lock_metadata(path: &Path, lock_id: &str, file: &mut fs::File) -> io::Result<()> {
    let result = (|| {
        writeln!(
            file,
            "lock_id={}\npid={}\ncreated_at={}",
            lock_id,
            std::process::id(),
            Utc::now().to_rfc3339()
        )?;
        file.sync_all()
    })();

    if result.is_err() {
        let _ = fs::remove_file(path);
    }

    result
}

fn release_accounts_file_lock(path: &Path, lock_id: &str) {
    if fs::read_to_string(path)
        .ok()
        .is_some_and(|content| accounts_lock_belongs_to_owner(&content, lock_id))
    {
        let _ = fs::remove_file(path);
    }
}

fn accounts_lock_belongs_to_owner(content: &str, lock_id: &str) -> bool {
    content
        .lines()
        .any(|line| line.strip_prefix("lock_id=") == Some(lock_id))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use uuid::Uuid;

    use super::{
        ACCOUNTS_LOCK_WAIT_TIMEOUT, accounts_lock_belongs_to_owner, acquire_accounts_file_lock,
    };

    #[test]
    fn accounts_lock_wait_timeout_is_thirty_seconds() {
        assert_eq!(ACCOUNTS_LOCK_WAIT_TIMEOUT, Duration::from_secs(30));
    }

    #[test]
    fn stale_guard_does_not_remove_replaced_lock_file() {
        let (dir, lock_path) = temp_lock_path("replaced-lock");
        let lock_a = acquire_accounts_file_lock(&lock_path).expect("first lock should acquire");
        std::fs::remove_file(&lock_path).expect("test should remove first lock file");

        let lock_b = acquire_accounts_file_lock(&lock_path).expect("second lock should acquire");
        let lock_b_id = lock_b.lock_id.clone();
        drop(lock_a);

        let lock_content = std::fs::read_to_string(&lock_path).expect("second lock should remain");
        assert!(accounts_lock_belongs_to_owner(&lock_content, &lock_b_id));

        drop(lock_b);
        assert!(!lock_path.exists());

        std::fs::remove_dir_all(dir).expect("temp lock dir should be removed");
    }

    fn temp_lock_path(name: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("codex-switch-lock-{name}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp lock dir should be created");
        let lock_path = dir.join("accounts.lock");
        (dir, lock_path)
    }
}
