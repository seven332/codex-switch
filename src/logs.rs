use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::runtime_log;

const FOLLOW_POLL_INTERVAL: Duration = Duration::from_millis(500);

pub(crate) fn runtime_log_dir_path() -> Result<PathBuf> {
    runtime_log::runtime_log_dir_path()
}

pub(crate) fn latest_log_path() -> Result<PathBuf> {
    let log_dir = runtime_log::runtime_log_dir_path()?;
    latest_log_path_in_dir(&log_dir)
}

pub(crate) fn latest_log_path_in_dir(path: &Path) -> Result<PathBuf> {
    runtime_log::latest_runtime_log_path_in_dir(path)?
        .with_context(|| format!("No runtime logs found in {}", path.display()))
}

pub(crate) async fn tail_latest(lines: usize, follow: bool) -> Result<()> {
    let path = latest_log_path()?;
    if follow {
        tail_file_follow(&path, lines).await
    } else {
        print_tail_file(&path, lines)
    }
}

fn print_tail_file(path: &Path, lines: usize) -> Result<()> {
    let content = tail_lines_from_path(path, lines)?;
    write_stdout(content.as_bytes())
}

pub(crate) fn tail_lines_from_path(path: &Path, lines: usize) -> Result<String> {
    let file = open_log_file(path)?;
    read_last_lines_from_file(file, lines)
        .map(|(content, _)| content)
        .with_context(|| format!("Failed to read runtime log: {}", path.display()))
}

async fn tail_file_follow(path: &Path, lines: usize) -> Result<()> {
    let file = open_log_file(path)?;
    let (content, mut file) = read_last_lines_from_file(file, lines)
        .with_context(|| format!("Failed to read runtime log: {}", path.display()))?;
    write_stdout(content.as_bytes())?;
    let mut position = file
        .stream_position()
        .with_context(|| format!("Failed to inspect runtime log position: {}", path.display()))?;

    loop {
        tokio::time::sleep(FOLLOW_POLL_INTERVAL).await;

        let length = fs::metadata(path)
            .with_context(|| format!("Failed to inspect runtime log: {}", path.display()))?
            .len();
        if length < position {
            file = open_log_file(path)?;
            position = 0;
        }
        if length <= position {
            continue;
        }

        file.seek(SeekFrom::Start(position))
            .with_context(|| format!("Failed to seek runtime log: {}", path.display()))?;
        let mut appended = Vec::new();
        file.read_to_end(&mut appended)
            .with_context(|| format!("Failed to read runtime log: {}", path.display()))?;
        position += appended.len() as u64;
        write_stdout(&appended)?;
    }
}

fn open_log_file(path: &Path) -> Result<File> {
    File::open(path).with_context(|| format!("Failed to open runtime log: {}", path.display()))
}

fn read_last_lines_from_file(mut file: File, lines: usize) -> Result<(String, File)> {
    if lines == 0 {
        file.seek(SeekFrom::End(0))
            .context("Failed to seek runtime log")?;
        return Ok((String::new(), file));
    }

    let mut reader = BufReader::new(file);
    let mut ring = VecDeque::with_capacity(lines);

    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .context("Failed to read runtime log line")?;
        if read == 0 {
            break;
        }
        if ring.len() == lines {
            ring.pop_front();
        }
        ring.push_back(line);
    }

    Ok((ring.into_iter().collect(), reader.into_inner()))
}

fn write_stdout(bytes: &[u8]) -> Result<()> {
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(bytes)
        .context("Failed to write runtime log output")?;
    stdout
        .flush()
        .context("Failed to flush runtime log output")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use uuid::Uuid;

    #[test]
    fn latest_log_path_in_dir_errors_when_no_logs_exist() {
        let dir = temp_dir("empty");
        fs::create_dir_all(&dir).expect("test log dir should be created");

        let err = latest_log_path_in_dir(&dir).expect_err("missing log should error");

        assert!(
            err.to_string()
                .contains(&format!("No runtime logs found in {}", dir.display()))
        );
        fs::remove_dir_all(&dir).expect("test log dir should be removed");
    }

    #[test]
    fn latest_log_path_in_dir_uses_runtime_log_naming() {
        let dir = temp_dir("latest");
        fs::create_dir_all(&dir).expect("test log dir should be created");
        let older = dir.join("codex-switch-run-20260101-000000-host-1-a.log");
        let newer = dir.join("codex-switch-run-20260102-000000-host-1-b.log");
        let ignored = dir.join("other.log");
        File::create(&older).expect("older log should be created");
        File::create(&newer).expect("newer log should be created");
        File::create(&ignored).expect("ignored log should be created");

        let latest = latest_log_path_in_dir(&dir).expect("latest log should be found");

        assert_eq!(latest, newer);
        fs::remove_dir_all(&dir).expect("test log dir should be removed");
    }

    #[test]
    fn tail_lines_from_empty_file_is_empty() {
        let (dir, path) = temp_log("empty", "");

        assert_eq!(
            tail_lines_from_path(&path, 100).expect("tail should read"),
            ""
        );

        fs::remove_dir_all(&dir).expect("test log dir should be removed");
    }

    #[test]
    fn tail_lines_keeps_fewer_than_requested_lines() {
        let (dir, path) = temp_log("short", "one\ntwo\n");

        assert_eq!(
            tail_lines_from_path(&path, 100).expect("tail should read"),
            "one\ntwo\n"
        );

        fs::remove_dir_all(&dir).expect("test log dir should be removed");
    }

    #[test]
    fn tail_lines_keeps_exact_requested_lines() {
        let (dir, path) = temp_log("exact", "one\ntwo\n");

        assert_eq!(
            tail_lines_from_path(&path, 2).expect("tail should read"),
            "one\ntwo\n"
        );

        fs::remove_dir_all(&dir).expect("test log dir should be removed");
    }

    #[test]
    fn tail_lines_returns_only_last_requested_lines() {
        let (dir, path) = temp_log("long", "one\ntwo\nthree\n");

        assert_eq!(
            tail_lines_from_path(&path, 2).expect("tail should read"),
            "two\nthree\n"
        );

        fs::remove_dir_all(&dir).expect("test log dir should be removed");
    }

    #[test]
    fn tail_lines_zero_reads_no_initial_lines() {
        let (dir, path) = temp_log("zero", "one\ntwo\n");

        assert_eq!(
            tail_lines_from_path(&path, 0).expect("tail should read"),
            ""
        );

        fs::remove_dir_all(&dir).expect("test log dir should be removed");
    }

    #[test]
    fn tail_lines_preserves_final_line_without_newline() {
        let (dir, path) = temp_log("no-newline", "one\ntwo");

        assert_eq!(
            tail_lines_from_path(&path, 1).expect("tail should read"),
            "two"
        );

        fs::remove_dir_all(&dir).expect("test log dir should be removed");
    }

    fn temp_log(name: &str, content: &str) -> (PathBuf, PathBuf) {
        let dir = temp_dir(name);
        fs::create_dir_all(&dir).expect("test log dir should be created");
        let path = dir.join("codex-switch-run-20260101-000000-host-1-a.log");
        let mut file = File::create(&path).expect("test log should be created");
        file.write_all(content.as_bytes())
            .expect("test log should be written");
        (dir, path)
    }

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("codex-switch-logs-{name}-{}", Uuid::new_v4()))
    }
}
