use std::ffi::OsString;
use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc as std_mpsc};
use std::thread::JoinHandle;

use crossterm::QueueableCommand;
use crossterm::cursor::{MoveTo, RestorePosition, SavePosition};
use crossterm::style::Print;
use nix::errno::Errno;
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::sys::select::{FdSet, select};
use nix::sys::signal::{Signal as UnixSignal, kill, killpg};
use nix::sys::termios::{LocalFlags, tcgetattr};
use nix::sys::time::{TimeVal, TimeValLike};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use pty_process::{OwnedReadPty, OwnedWritePty, Size};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant, sleep_until, timeout};

use anyhow::{Context, Result};

const ENTER_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049h";
const LEAVE_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049l";
const BEGIN_SYNCHRONIZED_UPDATE: &[u8] = b"\x1b[?2026h";
const END_SYNCHRONIZED_UPDATE: &[u8] = b"\x1b[?2026l";
const INPUT_EVENT_CAPACITY: usize = 16;

const MARKERS: [(Marker, &[u8]); 4] = [
    (Marker::EnterAlternateScreen, ENTER_ALTERNATE_SCREEN),
    (Marker::LeaveAlternateScreen, LEAVE_ALTERNATE_SCREEN),
    (Marker::BeginSynchronizedUpdate, BEGIN_SYNCHRONIZED_UPDATE),
    (Marker::EndSynchronizedUpdate, END_SYNCHRONIZED_UPDATE),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    EnterAlternateScreen,
    LeaveAlternateScreen,
    BeginSynchronizedUpdate,
    EndSynchronizedUpdate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BadgePlacement {
    column: u16,
    width: u16,
}

#[derive(Debug)]
struct FrameCompositor {
    label: String,
    label_width: u16,
    columns: u16,
    pending: Vec<u8>,
    alternate_screen: bool,
    synchronized_update: bool,
    synchronized_update_coherent: bool,
    badge_placement: Option<BadgePlacement>,
}

impl FrameCompositor {
    fn new(version: &str, columns: u16) -> Self {
        let label = format!("codex-switch {version}");
        debug_assert!(label.is_ascii());
        let label_width = u16::try_from(label.len()).unwrap_or(u16::MAX);
        Self {
            label,
            label_width,
            columns,
            pending: Vec::with_capacity(END_SYNCHRONIZED_UPDATE.len()),
            alternate_screen: false,
            synchronized_update: false,
            synchronized_update_coherent: false,
            badge_placement: None,
        }
    }

    fn set_columns(&mut self, columns: u16) {
        if self.columns != columns && self.synchronized_update {
            self.synchronized_update_coherent = false;
        }
        self.columns = columns;
    }

    fn alternate_screen_active(&self) -> bool {
        self.alternate_screen
    }

    fn terminal_context_active(&self) -> bool {
        self.alternate_screen || self.synchronized_update
    }

    fn process(&mut self, bytes: &[u8], output: &mut Vec<u8>) -> io::Result<()> {
        for &byte in bytes {
            self.pending.push(byte);
            self.flush_non_marker_prefix(output)?;
        }
        Ok(())
    }

    fn finish(&mut self, output: &mut Vec<u8>) {
        output.append(&mut self.pending);
    }

    fn append_terminal_cleanup(&mut self, output: &mut Vec<u8>) {
        self.finish(output);
        if self.synchronized_update {
            output.extend_from_slice(END_SYNCHRONIZED_UPDATE);
        }
        if self.alternate_screen {
            output.extend_from_slice(LEAVE_ALTERNATE_SCREEN);
        }
        if self.synchronized_update || self.alternate_screen {
            output.extend_from_slice(b"\x1b[0m\x1b[?25h");
        }
        self.synchronized_update = false;
        self.synchronized_update_coherent = false;
        self.alternate_screen = false;
        self.badge_placement = None;
    }

    fn flush_non_marker_prefix(&mut self, output: &mut Vec<u8>) -> io::Result<()> {
        loop {
            if let Some(marker) = exact_marker(&self.pending) {
                let marker_bytes = std::mem::take(&mut self.pending);
                self.handle_marker(marker, &marker_bytes, output)?;
                return Ok(());
            }

            if is_marker_prefix(&self.pending) {
                return Ok(());
            }

            output.push(self.pending.remove(0));
            if self.pending.is_empty() {
                return Ok(());
            }
        }
    }

    fn handle_marker(
        &mut self,
        marker: Marker,
        marker_bytes: &[u8],
        output: &mut Vec<u8>,
    ) -> io::Result<()> {
        match marker {
            Marker::EnterAlternateScreen => {
                if self.synchronized_update {
                    self.synchronized_update_coherent = false;
                }
                self.alternate_screen = true;
                self.badge_placement = None;
            }
            Marker::LeaveAlternateScreen => {
                if self.synchronized_update {
                    self.synchronized_update_coherent = false;
                }
                self.alternate_screen = false;
                self.badge_placement = None;
            }
            Marker::BeginSynchronizedUpdate => {
                self.synchronized_update_coherent = !self.synchronized_update;
                self.synchronized_update = true;
                output.extend_from_slice(marker_bytes);
                if self.alternate_screen && self.synchronized_update_coherent {
                    self.append_stale_badge_clear(output)?;
                }
                return Ok(());
            }
            Marker::EndSynchronizedUpdate => {
                if self.alternate_screen
                    && self.synchronized_update
                    && self.synchronized_update_coherent
                {
                    self.append_badge(output)?;
                }
                self.synchronized_update = false;
                self.synchronized_update_coherent = false;
            }
        }
        output.extend_from_slice(marker_bytes);
        Ok(())
    }

    fn next_badge_placement(&self) -> Option<BadgePlacement> {
        (self.columns >= self.label_width).then(|| BadgePlacement {
            column: self.columns - self.label_width,
            width: self.label_width,
        })
    }

    fn append_stale_badge_clear(&mut self, output: &mut Vec<u8>) -> io::Result<()> {
        if self.badge_placement == self.next_badge_placement() {
            return Ok(());
        }

        if let Some(previous) = self.badge_placement
            && previous.column < self.columns
        {
            output.queue(SavePosition)?;
            let visible_width = previous.width.min(self.columns - previous.column);
            output.queue(MoveTo(previous.column, 0))?;
            output.queue(Print(" ".repeat(usize::from(visible_width))))?;
            output.queue(RestorePosition)?;
        }
        self.badge_placement = None;
        Ok(())
    }

    fn append_badge(&mut self, output: &mut Vec<u8>) -> io::Result<()> {
        let next_placement = self.next_badge_placement();
        debug_assert!(self.badge_placement.is_none() || self.badge_placement == next_placement);

        if let Some(placement) = next_placement {
            output.queue(SavePosition)?;
            output.queue(MoveTo(placement.column, 0))?;
            output.queue(Print(self.label.as_str()))?;
            output.queue(RestorePosition)?;
        }
        self.badge_placement = next_placement;
        Ok(())
    }
}

fn exact_marker(bytes: &[u8]) -> Option<Marker> {
    MARKERS
        .iter()
        .find_map(|(marker, candidate)| (*candidate == bytes).then_some(*marker))
}

fn is_marker_prefix(bytes: &[u8]) -> bool {
    MARKERS
        .iter()
        .any(|(_, candidate)| candidate.starts_with(bytes))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverlayEligibility {
    Eligible,
    Direct(&'static str),
}

#[derive(Debug, Clone, Copy)]
struct TerminalFacts<'a> {
    stdin_is_terminal: bool,
    stdout_is_terminal: bool,
    stderr_is_terminal: bool,
    raw_mode_enabled: bool,
    term: Option<&'a str>,
}

fn overlay_eligibility(args: &[OsString], facts: TerminalFacts<'_>) -> OverlayEligibility {
    if !facts.stdin_is_terminal || !facts.stdout_is_terminal || !facts.stderr_is_terminal {
        return OverlayEligibility::Direct("a standard stream is not a TTY");
    }
    if facts.raw_mode_enabled {
        return OverlayEligibility::Direct("the outer terminal is already in raw mode");
    }
    if facts.term == Some("dumb") {
        return OverlayEligibility::Direct("TERM is dumb");
    }
    if args.iter().any(|arg| arg == "--no-alt-screen") {
        return OverlayEligibility::Direct("Codex alternate screen is disabled");
    }
    OverlayEligibility::Eligible
}

fn terminal_is_noncanonical(terminal: impl AsFd) -> io::Result<bool> {
    let attributes = tcgetattr(terminal).map_err(|err| io::Error::from_raw_os_error(err as i32))?;
    Ok(!attributes.local_flags.contains(LocalFlags::ICANON))
}

#[derive(Debug)]
pub(crate) enum OverlaySpawn {
    Spawned(Box<TuiOverlaySession>),
    Direct { reason: String },
}

#[derive(Debug)]
enum InputEvent {
    Bytes(Vec<u8>),
    Eof,
    Error(String),
}

#[derive(Debug)]
enum ChildEvent {
    Stopped,
    Continued,
    Exited(ExitStatus),
    Error(String),
}

struct SessionSignals {
    resize: Signal,
    hangup: Signal,
    interrupt: Signal,
    quit: Signal,
    terminate: Signal,
}

impl SessionSignals {
    fn new() -> io::Result<Self> {
        Ok(Self {
            resize: signal(SignalKind::window_change())?,
            hangup: signal(SignalKind::hangup())?,
            interrupt: signal(SignalKind::interrupt())?,
            quit: signal(SignalKind::quit())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }
}

struct RawModeGuard {
    enabled: bool,
}

impl RawModeGuard {
    fn enable() -> io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self { enabled: true })
    }

    fn suspend(&mut self) -> io::Result<()> {
        if self.enabled {
            crossterm::terminal::disable_raw_mode()?;
            self.enabled = false;
        }
        Ok(())
    }

    fn resume(&mut self) -> io::Result<()> {
        if !self.enabled {
            crossterm::terminal::enable_raw_mode()?;
            self.enabled = true;
        }
        Ok(())
    }

    fn restore(&mut self) {
        if self.enabled {
            let _ = crossterm::terminal::disable_raw_mode();
            self.enabled = false;
        }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

pub(crate) struct TuiOverlaySession {
    pid: Pid,
    pty_reader: OwnedReadPty,
    pty_writer: OwnedWritePty,
    compositor: FrameCompositor,
    input_events: mpsc::Receiver<InputEvent>,
    child_events: mpsc::UnboundedReceiver<ChildEvent>,
    input_shutdown: Arc<AtomicBool>,
    input_thread: Option<JoinHandle<()>>,
    monitor_thread: Option<JoinHandle<()>>,
    raw_mode: RawModeGuard,
    signals: SessionSignals,
    input_open: bool,
    output_open: bool,
    child_reaped: bool,
    termination_deadline: Option<Instant>,
    completed: bool,
}

impl std::fmt::Debug for TuiOverlaySession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TuiOverlaySession")
            .field("pid", &self.pid)
            .field("input_open", &self.input_open)
            .field("output_open", &self.output_open)
            .field("completed", &self.completed)
            .finish_non_exhaustive()
    }
}

pub(crate) fn try_spawn(
    codex_bin: &str,
    codex_args: &[OsString],
    websocket_url: &str,
    token_env: &str,
    token: &str,
) -> Result<OverlaySpawn> {
    let stdin = io::stdin();
    let raw_mode_enabled = match terminal_is_noncanonical(&stdin) {
        Ok(enabled) => enabled,
        Err(err) => {
            return Ok(OverlaySpawn::Direct {
                reason: format!("failed to inspect outer raw mode: {err}"),
            });
        }
    };
    let term = std::env::var("TERM").ok();
    let facts = TerminalFacts {
        stdin_is_terminal: io::stdin().is_terminal(),
        stdout_is_terminal: io::stdout().is_terminal(),
        stderr_is_terminal: io::stderr().is_terminal(),
        raw_mode_enabled,
        term: term.as_deref(),
    };
    if let OverlayEligibility::Direct(reason) = overlay_eligibility(codex_args, facts) {
        return Ok(OverlaySpawn::Direct {
            reason: reason.to_string(),
        });
    }

    let (columns, rows) = match crossterm::terminal::size() {
        Ok((columns, rows)) if columns > 0 && rows > 0 => (columns, rows),
        Ok(_) => {
            return Ok(OverlaySpawn::Direct {
                reason: "outer terminal reported a zero size".to_string(),
            });
        }
        Err(err) => {
            return Ok(OverlaySpawn::Direct {
                reason: format!("failed to read outer terminal size: {err}"),
            });
        }
    };

    let (blocking_pty, pts) = match pty_process::blocking::open() {
        Ok(pair) => pair,
        Err(err) => {
            return Ok(OverlaySpawn::Direct {
                reason: format!("failed to open a PTY: {err}"),
            });
        }
    };
    if let Err(err) = blocking_pty.resize(Size::new(rows, columns)) {
        return Ok(OverlaySpawn::Direct {
            reason: format!("failed to set the initial PTY size: {err}"),
        });
    }

    let pty_fd: OwnedFd = blocking_pty.into();
    let current_flags = match fcntl(&pty_fd, FcntlArg::F_GETFL) {
        Ok(flags) => OFlag::from_bits_truncate(flags),
        Err(err) => {
            return Ok(OverlaySpawn::Direct {
                reason: format!("failed to inspect PTY flags: {err}"),
            });
        }
    };
    if let Err(err) = fcntl(
        &pty_fd,
        FcntlArg::F_SETFL(current_flags | OFlag::O_NONBLOCK),
    ) {
        return Ok(OverlaySpawn::Direct {
            reason: format!("failed to make PTY nonblocking: {err}"),
        });
    }
    // SAFETY: `pty_fd` came from pty-process, remains open, belongs to the PTY
    // master, and was set to nonblocking mode immediately above.
    let async_pty = match unsafe { pty_process::Pty::from_fd(pty_fd) } {
        Ok(pty) => pty,
        Err(err) => {
            return Ok(OverlaySpawn::Direct {
                reason: format!("failed to register PTY with the async runtime: {err}"),
            });
        }
    };
    let (pty_reader, pty_writer) = async_pty.into_split();

    let terminal_input = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_NONBLOCK | OFlag::O_CLOEXEC).bits())
        .open("/dev/tty")
    {
        Ok(terminal) => terminal,
        Err(err) => {
            return Ok(OverlaySpawn::Direct {
                reason: format!("failed to open controlling terminal input: {err}"),
            });
        }
    };

    let input_shutdown = Arc::new(AtomicBool::new(false));
    let (input_start_tx, input_start_rx) = std_mpsc::sync_channel(0);
    let (input_event_tx, input_events) = mpsc::channel(INPUT_EVENT_CAPACITY);
    let input_thread = match std::thread::Builder::new()
        .name("codex-switch-tui-input".to_string())
        .spawn({
            let input_shutdown = Arc::clone(&input_shutdown);
            move || {
                run_input_relay(
                    input_start_rx,
                    terminal_input,
                    input_shutdown,
                    input_event_tx,
                )
            }
        }) {
        Ok(thread) => thread,
        Err(err) => {
            return Ok(OverlaySpawn::Direct {
                reason: format!("failed to start terminal input relay: {err}"),
            });
        }
    };

    let (child_handle_tx, child_handle_rx) = std_mpsc::sync_channel(0);
    let (child_event_tx, child_events) = mpsc::unbounded_channel();
    let monitor_thread = match std::thread::Builder::new()
        .name("codex-switch-tui-child".to_string())
        .spawn(move || run_child_monitor(child_handle_rx, child_event_tx))
    {
        Ok(thread) => thread,
        Err(err) => {
            drop(input_start_tx);
            input_shutdown.store(true, Ordering::Relaxed);
            let _ = input_thread.join();
            return Ok(OverlaySpawn::Direct {
                reason: format!("failed to start PTY child monitor: {err}"),
            });
        }
    };

    let mut raw_mode = match RawModeGuard::enable() {
        Ok(guard) => guard,
        Err(err) => {
            drop(input_start_tx);
            drop(child_handle_tx);
            input_shutdown.store(true, Ordering::Relaxed);
            let _ = input_thread.join();
            let _ = monitor_thread.join();
            return Ok(OverlaySpawn::Direct {
                reason: format!("failed to enable outer raw mode: {err}"),
            });
        }
    };

    let command = pty_process::blocking::Command::new(codex_bin)
        .args(codex_args)
        .arg("--remote")
        .arg(websocket_url)
        .arg("--remote-auth-token-env")
        .arg(token_env)
        .env(token_env, token);
    let mut child = match command.spawn(pts) {
        Ok(child) => child,
        Err(err) => {
            raw_mode.restore();
            drop(input_start_tx);
            drop(child_handle_tx);
            input_shutdown.store(true, Ordering::Relaxed);
            let _ = input_thread.join();
            let _ = monitor_thread.join();
            return Ok(OverlaySpawn::Direct {
                reason: format!("failed to spawn Codex on the PTY: {err}"),
            });
        }
    };
    let pid_raw = match i32::try_from(child.id()) {
        Ok(pid) => pid,
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            raw_mode.restore();
            drop(input_start_tx);
            drop(child_handle_tx);
            input_shutdown.store(true, Ordering::Relaxed);
            let _ = input_thread.join();
            let _ = monitor_thread.join();
            return Err(err).context("Codex PID does not fit in a Unix PID");
        }
    };
    let pid = Pid::from_raw(pid_raw);
    let signals = match SessionSignals::new() {
        Ok(signals) => signals,
        Err(err) => {
            let _ = killpg(pid, UnixSignal::SIGKILL);
            let _ = child.wait();
            raw_mode.restore();
            drop(input_start_tx);
            drop(child_handle_tx);
            input_shutdown.store(true, Ordering::Relaxed);
            let _ = input_thread.join();
            let _ = monitor_thread.join();
            return Err(err).context("Failed to register PTY session signals");
        }
    };
    if let Err(send_error) = child_handle_tx.send(child) {
        let mut child = send_error.0;
        let _ = killpg(pid, UnixSignal::SIGKILL);
        let _ = child.wait();
        raw_mode.restore();
        drop(input_start_tx);
        input_shutdown.store(true, Ordering::Relaxed);
        let _ = input_thread.join();
        let _ = monitor_thread.join();
        anyhow::bail!("PTY child monitor stopped before receiving Codex");
    }
    if input_start_tx.send(()).is_err() {
        let _ = killpg(pid, UnixSignal::SIGKILL);
        raw_mode.restore();
        input_shutdown.store(true, Ordering::Relaxed);
        let _ = input_thread.join();
        let _ = monitor_thread.join();
        anyhow::bail!("terminal input relay stopped before Codex started");
    }

    Ok(OverlaySpawn::Spawned(Box::new(TuiOverlaySession {
        pid,
        pty_reader,
        pty_writer,
        compositor: FrameCompositor::new(env!("CARGO_PKG_VERSION"), columns),
        input_events,
        child_events,
        input_shutdown,
        input_thread: Some(input_thread),
        monitor_thread: Some(monitor_thread),
        raw_mode,
        signals,
        input_open: true,
        output_open: true,
        child_reaped: false,
        termination_deadline: None,
        completed: false,
    })))
}

fn run_input_relay(
    start: std_mpsc::Receiver<()>,
    mut terminal: std::fs::File,
    shutdown: Arc<AtomicBool>,
    events: mpsc::Sender<InputEvent>,
) {
    if start.recv().is_err() {
        return;
    }

    let mut buffer = [0_u8; 4096];
    while !shutdown.load(Ordering::Relaxed) {
        let mut read_fds = FdSet::new();
        read_fds.insert(terminal.as_fd());
        let mut select_timeout = TimeVal::milliseconds(50);
        match select(None, &mut read_fds, None, None, &mut select_timeout) {
            Ok(0) => continue,
            Ok(_) => {}
            Err(Errno::EINTR) => continue,
            Err(err) => {
                let _ = events.blocking_send(InputEvent::Error(format!(
                    "terminal input poll failed: {err}"
                )));
                return;
            }
        }
        if read_fds.contains(terminal.as_fd()) {
            match terminal.read(&mut buffer) {
                Ok(0) => {
                    let _ = events.blocking_send(InputEvent::Eof);
                    return;
                }
                Ok(read) => {
                    if events
                        .blocking_send(InputEvent::Bytes(buffer[..read].to_vec()))
                        .is_err()
                    {
                        return;
                    }
                }
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                    ) => {}
                Err(err) => {
                    let _ = events.blocking_send(InputEvent::Error(format!(
                        "failed to read terminal input: {err}"
                    )));
                    return;
                }
            }
        }
    }
}

fn run_child_monitor(
    child: std_mpsc::Receiver<std::process::Child>,
    events: mpsc::UnboundedSender<ChildEvent>,
) {
    let Ok(child) = child.recv() else {
        return;
    };
    let Ok(pid_raw) = i32::try_from(child.id()) else {
        let _ = events.send(ChildEvent::Error(
            "Codex PID does not fit in a Unix PID".to_string(),
        ));
        return;
    };
    let pid = Pid::from_raw(pid_raw);
    let flags = WaitPidFlag::WUNTRACED | WaitPidFlag::WCONTINUED;
    loop {
        match waitpid(pid, Some(flags)) {
            Ok(WaitStatus::Exited(_, code)) => {
                let _ = events.send(ChildEvent::Exited(ExitStatus::from_raw(code << 8)));
                return;
            }
            Ok(WaitStatus::Signaled(_, signal, dumped_core)) => {
                let core_flag = if dumped_core { 0x80 } else { 0 };
                let _ = events.send(ChildEvent::Exited(ExitStatus::from_raw(
                    signal as i32 | core_flag,
                )));
                return;
            }
            Ok(WaitStatus::Stopped(_, _)) => {
                let _ = events.send(ChildEvent::Stopped);
            }
            Ok(WaitStatus::Continued(_)) => {
                let _ = events.send(ChildEvent::Continued);
            }
            Ok(WaitStatus::StillAlive) => {}
            #[cfg(any(target_os = "linux", target_os = "android"))]
            Ok(WaitStatus::PtraceEvent(_, _, _) | WaitStatus::PtraceSyscall(_)) => {
                let _ = events.send(ChildEvent::Stopped);
            }
            Err(Errno::EINTR) => {}
            Err(err) => {
                let _ = events.send(ChildEvent::Error(format!(
                    "failed to wait for Codex PTY child: {err}"
                )));
                return;
            }
        }
    }
}

enum SessionEvent {
    Output(io::Result<Vec<u8>>),
    Input(Option<InputEvent>),
    Child(Option<ChildEvent>),
    Resize,
    Terminate(UnixSignal),
    TerminationTimeout,
}

impl TuiOverlaySession {
    pub(crate) fn id(&self) -> u32 {
        self.pid.as_raw() as u32
    }

    pub(crate) async fn wait(&mut self) -> Result<ExitStatus> {
        let result = self.wait_inner().await;
        if result.is_err() {
            self.shutdown().await;
        }
        result
    }

    async fn wait_inner(&mut self) -> Result<ExitStatus> {
        loop {
            match self.next_event().await {
                SessionEvent::Output(result) => self.handle_output(result)?,
                SessionEvent::Input(Some(InputEvent::Bytes(bytes))) => {
                    if let Err(err) = self.pty_writer.write_all(&bytes).await {
                        if pty_input_closed(&err) {
                            self.input_open = false;
                        } else {
                            return Err(err).context("Failed to relay terminal input to Codex");
                        }
                    }
                }
                SessionEvent::Input(Some(InputEvent::Eof) | None) => {
                    self.input_open = false;
                }
                SessionEvent::Input(Some(InputEvent::Error(message))) => {
                    anyhow::bail!(message);
                }
                SessionEvent::Child(Some(ChildEvent::Stopped)) => {
                    self.handle_child_stopped().await?;
                }
                SessionEvent::Child(Some(ChildEvent::Continued)) => {}
                SessionEvent::Child(Some(ChildEvent::Exited(status))) => {
                    self.child_reaped = true;
                    self.drain_output_until_quiet(Duration::from_millis(50))
                        .await?;
                    self.finalize()?;
                    return Ok(status);
                }
                SessionEvent::Child(Some(ChildEvent::Error(message))) => {
                    anyhow::bail!(message);
                }
                SessionEvent::Child(None) => {
                    anyhow::bail!("Codex PTY child monitor stopped unexpectedly");
                }
                SessionEvent::Resize => self.apply_resize()?,
                SessionEvent::Terminate(signal) => {
                    self.forward_termination(signal)?;
                }
                SessionEvent::TerminationTimeout => {
                    self.termination_deadline = None;
                    signal_process_group(self.pid, UnixSignal::SIGKILL)?;
                }
            }
        }
    }

    async fn next_event(&mut self) -> SessionEvent {
        let deadline = self.termination_deadline;
        tokio::select! {
            result = read_pty_chunk(&mut self.pty_reader), if self.output_open => {
                SessionEvent::Output(result)
            }
            input = self.input_events.recv(), if self.input_open => SessionEvent::Input(input),
            child = self.child_events.recv() => SessionEvent::Child(child),
            _ = self.signals.resize.recv() => SessionEvent::Resize,
            _ = self.signals.hangup.recv() => SessionEvent::Terminate(UnixSignal::SIGHUP),
            _ = self.signals.interrupt.recv() => SessionEvent::Terminate(UnixSignal::SIGINT),
            _ = self.signals.quit.recv() => SessionEvent::Terminate(UnixSignal::SIGQUIT),
            _ = self.signals.terminate.recv() => SessionEvent::Terminate(UnixSignal::SIGTERM),
            _ = async {
                match deadline {
                    Some(deadline) => sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            } => SessionEvent::TerminationTimeout,
        }
    }

    fn handle_output(&mut self, result: io::Result<Vec<u8>>) -> Result<()> {
        match result {
            Ok(bytes) if bytes.is_empty() => {
                self.output_open = false;
            }
            Ok(bytes) => {
                let mut output = Vec::with_capacity(bytes.len() + 64);
                self.compositor.process(&bytes, &mut output)?;
                write_terminal(&output)?;
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) if err.raw_os_error() == Some(Errno::EIO as i32) => {
                self.output_open = false;
            }
            Err(err) => return Err(err).context("Failed to read Codex PTY output"),
        }
        Ok(())
    }

    fn apply_resize(&mut self) -> Result<()> {
        let (columns, rows) =
            crossterm::terminal::size().context("Failed to read terminal size after resize")?;
        if columns == 0 || rows == 0 {
            anyhow::bail!("terminal reported a zero size after resize");
        }
        self.compositor.set_columns(columns);
        self.pty_writer
            .resize(Size::new(rows, columns))
            .context("Failed to resize Codex PTY")?;
        Ok(())
    }

    fn forward_termination(&mut self, signal: UnixSignal) -> Result<()> {
        if self.termination_deadline.is_some() {
            signal_process_group(self.pid, UnixSignal::SIGKILL)?;
            self.termination_deadline = None;
        } else {
            signal_process_group(self.pid, signal)?;
            self.termination_deadline = Some(Instant::now() + Duration::from_secs(2));
        }
        Ok(())
    }

    async fn handle_child_stopped(&mut self) -> Result<()> {
        self.drain_output_until_quiet(Duration::from_millis(50))
            .await?;
        let forced_alternate_restore = self.compositor.alternate_screen_active();
        if self.compositor.terminal_context_active() {
            let mut cleanup = Vec::new();
            self.compositor.append_terminal_cleanup(&mut cleanup);
            write_terminal(&cleanup)?;
        } else {
            io::stdout()
                .flush()
                .context("Failed to flush terminal before suspension")?;
        }

        self.raw_mode
            .suspend()
            .context("Failed to restore terminal before suspension")?;
        kill(Pid::from_raw(0), UnixSignal::SIGTSTP)
            .context("Failed to suspend codex-switch for shell job control")?;
        self.raw_mode
            .resume()
            .context("Failed to re-enable terminal raw mode after resume")?;

        if forced_alternate_restore {
            let mut enter_alternate_screen = Vec::new();
            self.compositor
                .process(ENTER_ALTERNATE_SCREEN, &mut enter_alternate_screen)?;
            write_terminal(&enter_alternate_screen)?;
        }
        self.apply_resize()?;
        signal_process_group(self.pid, UnixSignal::SIGCONT)?;
        Ok(())
    }

    async fn drain_output_until_quiet(&mut self, quiet: Duration) -> Result<()> {
        while self.output_open {
            match timeout(quiet, read_pty_chunk(&mut self.pty_reader)).await {
                Ok(result) => self.handle_output(result)?,
                Err(_) => break,
            }
        }
        Ok(())
    }

    pub(crate) async fn shutdown(&mut self) {
        if self.completed {
            return;
        }
        if !self.child_reaped {
            let _ = killpg(self.pid, UnixSignal::SIGKILL);
            let wait_for_exit = async {
                while let Some(event) = self.child_events.recv().await {
                    match event {
                        ChildEvent::Exited(_) => {
                            self.child_reaped = true;
                            break;
                        }
                        ChildEvent::Stopped | ChildEvent::Continued => {}
                        ChildEvent::Error(_) => break,
                    }
                }
            };
            let _ = timeout(Duration::from_secs(2), wait_for_exit).await;
        }
        let _ = self
            .drain_output_until_quiet(Duration::from_millis(50))
            .await;
        let _ = self.finalize();
    }

    fn finalize(&mut self) -> Result<()> {
        if self.completed {
            return Ok(());
        }
        self.input_shutdown.store(true, Ordering::Relaxed);
        self.input_open = false;
        self.input_events.close();

        let mut final_output = Vec::new();
        if self.compositor.terminal_context_active() {
            self.compositor.append_terminal_cleanup(&mut final_output);
        } else {
            self.compositor.finish(&mut final_output);
        }
        let terminal_result = write_terminal(&final_output);
        self.raw_mode.restore();

        if let Some(thread) = self.input_thread.take() {
            let _ = thread.join();
        }
        if self.child_reaped
            && let Some(thread) = self.monitor_thread.take()
        {
            let _ = thread.join();
        }
        self.completed = true;
        terminal_result.context("Failed to flush final Codex terminal output")
    }
}

impl Drop for TuiOverlaySession {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        self.input_shutdown.store(true, Ordering::Relaxed);
        if !self.child_reaped {
            let _ = killpg(self.pid, UnixSignal::SIGKILL);
        }
        let mut cleanup = Vec::new();
        self.compositor.append_terminal_cleanup(&mut cleanup);
        let _ = write_terminal(&cleanup);
        self.raw_mode.restore();
    }
}

async fn read_pty_chunk(reader: &mut OwnedReadPty) -> io::Result<Vec<u8>> {
    let mut buffer = vec![0_u8; 8192];
    let read = reader.read(&mut buffer).await?;
    buffer.truncate(read);
    Ok(buffer)
}

fn write_terminal(bytes: &[u8]) -> io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    stdout.write_all(bytes)?;
    stdout.flush()
}

fn pty_input_closed(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::BrokenPipe || err.raw_os_error() == Some(Errno::EIO as i32)
}

fn signal_process_group(pid: Pid, signal: UnixSignal) -> Result<()> {
    match killpg(pid, signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(err) => Err(err).with_context(|| format!("Failed to send {signal:?} to Codex PTY")),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BEGIN_SYNCHRONIZED_UPDATE, ChildEvent, END_SYNCHRONIZED_UPDATE, ENTER_ALTERNATE_SCREEN,
        FrameCompositor, LEAVE_ALTERNATE_SCREEN, OverlayEligibility, OverlaySpawn, TerminalFacts,
        overlay_eligibility, pty_input_closed, run_child_monitor, terminal_is_noncanonical,
        try_spawn,
    };
    use std::ffi::OsString;
    use std::io;
    use std::os::fd::OwnedFd;
    use std::sync::mpsc as std_mpsc;

    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    use nix::sys::signal::{Signal as UnixSignal, killpg};
    use nix::sys::termios::{LocalFlags, SetArg, tcgetattr, tcsetattr};
    use nix::unistd::Pid;
    use pty_process::Size;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc;
    use tokio::time::{Duration, timeout};

    const VERSION: &str = "1.2.3";
    const LABEL: &str = "codex-switch 1.2.3";

    fn frame(content: &[u8]) -> Vec<u8> {
        [
            ENTER_ALTERNATE_SCREEN,
            BEGIN_SYNCHRONIZED_UPDATE,
            content,
            END_SYNCHRONIZED_UPDATE,
            LEAVE_ALTERNATE_SCREEN,
        ]
        .concat()
    }

    fn badge(column: u16) -> Vec<u8> {
        format!("\x1b7\x1b[1;{}H{LABEL}\x1b8", column + 1).into_bytes()
    }

    fn render_in_chunks(input: &[u8], chunks: &[usize], columns: u16) -> Vec<u8> {
        let mut compositor = FrameCompositor::new(VERSION, columns);
        let mut output = Vec::new();
        let mut offset = 0;
        for &length in chunks {
            compositor
                .process(&input[offset..offset + length], &mut output)
                .unwrap();
            offset += length;
        }
        compositor.process(&input[offset..], &mut output).unwrap();
        compositor.finish(&mut output);
        output
    }

    #[test]
    fn inserts_badge_before_end_of_synchronized_frame() {
        let input = frame(b"frame");
        let output = render_in_chunks(&input, &[], 80);
        let mut expected = [ENTER_ALTERNATE_SCREEN, BEGIN_SYNCHRONIZED_UPDATE, b"frame"].concat();
        expected.extend_from_slice(&badge(80 - LABEL.len() as u16));
        expected.extend_from_slice(END_SYNCHRONIZED_UPDATE);
        expected.extend_from_slice(LEAVE_ALTERNATE_SCREEN);
        assert_eq!(output, expected);
    }

    #[test]
    fn recognizes_markers_across_every_read_boundary() {
        let input = frame(b"frame");
        let expected = render_in_chunks(&input, &[], 80);
        for split in 0..=input.len() {
            assert_eq!(render_in_chunks(&input, &[split], 80), expected);
        }
        assert_eq!(
            render_in_chunks(&input, &vec![1; input.len()], 80),
            expected
        );
    }

    #[test]
    fn preserves_unrelated_and_incomplete_escape_sequences() {
        let input = b"plain\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\\x1b[31mred\x1b[?202";
        assert_eq!(render_in_chunks(input, &vec![1; input.len()], 80), input);
    }

    #[test]
    fn suppresses_badge_outside_supported_frame_context() {
        let inputs = [
            [BEGIN_SYNCHRONIZED_UPDATE, b"frame", END_SYNCHRONIZED_UPDATE].concat(),
            [ENTER_ALTERNATE_SCREEN, b"frame", LEAVE_ALTERNATE_SCREEN].concat(),
            [
                ENTER_ALTERNATE_SCREEN,
                BEGIN_SYNCHRONIZED_UPDATE,
                BEGIN_SYNCHRONIZED_UPDATE,
                END_SYNCHRONIZED_UPDATE,
            ]
            .concat(),
            [
                BEGIN_SYNCHRONIZED_UPDATE,
                ENTER_ALTERNATE_SCREEN,
                END_SYNCHRONIZED_UPDATE,
            ]
            .concat(),
        ];
        for input in inputs {
            assert_eq!(render_in_chunks(&input, &[], 80), input);
        }
    }

    #[test]
    fn exact_width_fits_and_narrow_width_is_unchanged() {
        let input = frame(b"frame");
        let exact = render_in_chunks(&input, &[], LABEL.len() as u16);
        assert!(
            exact
                .windows(LABEL.len())
                .any(|bytes| bytes == LABEL.as_bytes())
        );
        assert_eq!(render_in_chunks(&input, &[], LABEL.len() as u16 - 1), input);
    }

    #[test]
    fn repaints_badge_for_each_synchronized_frame() {
        let input = [
            ENTER_ALTERNATE_SCREEN,
            BEGIN_SYNCHRONIZED_UPDATE,
            b"first",
            END_SYNCHRONIZED_UPDATE,
            BEGIN_SYNCHRONIZED_UPDATE,
            b"second",
            END_SYNCHRONIZED_UPDATE,
            LEAVE_ALTERNATE_SCREEN,
        ]
        .concat();
        let output = render_in_chunks(&input, &[], 80);

        assert_eq!(
            output
                .windows(LABEL.len())
                .filter(|bytes| *bytes == LABEL.as_bytes())
                .count(),
            2
        );
    }

    #[test]
    fn resize_clears_previous_placement_before_drawing_new_one() {
        let mut compositor = FrameCompositor::new(VERSION, 80);
        let mut output = Vec::new();
        compositor
            .process(ENTER_ALTERNATE_SCREEN, &mut output)
            .unwrap();
        compositor
            .process(
                &[BEGIN_SYNCHRONIZED_UPDATE, END_SYNCHRONIZED_UPDATE].concat(),
                &mut output,
            )
            .unwrap();
        output.clear();

        compositor.set_columns(100);
        compositor
            .process(
                &[BEGIN_SYNCHRONIZED_UPDATE, END_SYNCHRONIZED_UPDATE].concat(),
                &mut output,
            )
            .unwrap();

        let old_column = 80 - LABEL.len() as u16;
        let new_column = 100 - LABEL.len() as u16;
        let expected_overlay = format!(
            "\x1b7\x1b[1;{}H{}\x1b8\x1b7\x1b[1;{}H{LABEL}\x1b8",
            old_column + 1,
            " ".repeat(LABEL.len()),
            new_column + 1,
        );
        assert!(output.starts_with(BEGIN_SYNCHRONIZED_UPDATE));
        assert!(
            output
                .windows(expected_overlay.len())
                .any(|bytes| bytes == expected_overlay.as_bytes())
        );
        assert!(output.ends_with(END_SYNCHRONIZED_UPDATE));
    }

    #[test]
    fn resize_clears_stale_badge_before_child_frame_content() {
        let mut compositor = FrameCompositor::new(VERSION, 80);
        let mut output = Vec::new();
        compositor
            .process(ENTER_ALTERNATE_SCREEN, &mut output)
            .unwrap();
        compositor
            .process(
                &[BEGIN_SYNCHRONIZED_UPDATE, END_SYNCHRONIZED_UPDATE].concat(),
                &mut output,
            )
            .unwrap();
        output.clear();

        compositor.set_columns(100);
        compositor
            .process(
                &[
                    BEGIN_SYNCHRONIZED_UPDATE,
                    b"child-frame",
                    END_SYNCHRONIZED_UPDATE,
                ]
                .concat(),
                &mut output,
            )
            .unwrap();

        let stale_clear = " ".repeat(LABEL.len());
        let clear_offset = output
            .windows(stale_clear.len())
            .position(|bytes| bytes == stale_clear.as_bytes())
            .unwrap();
        let child_offset = output
            .windows(b"child-frame".len())
            .position(|bytes| bytes == b"child-frame")
            .unwrap();
        let badge_offset = output
            .windows(LABEL.len())
            .position(|bytes| bytes == LABEL.as_bytes())
            .unwrap();
        assert!(clear_offset < child_offset);
        assert!(child_offset < badge_offset);
    }

    #[test]
    fn resize_during_frame_waits_for_next_coherent_frame() {
        let mut compositor = FrameCompositor::new(VERSION, 80);
        let mut output = Vec::new();
        compositor
            .process(ENTER_ALTERNATE_SCREEN, &mut output)
            .unwrap();
        compositor
            .process(
                &[BEGIN_SYNCHRONIZED_UPDATE, END_SYNCHRONIZED_UPDATE].concat(),
                &mut output,
            )
            .unwrap();
        output.clear();

        compositor
            .process(BEGIN_SYNCHRONIZED_UPDATE, &mut output)
            .unwrap();
        compositor.set_columns(100);
        compositor
            .process(END_SYNCHRONIZED_UPDATE, &mut output)
            .unwrap();
        assert_eq!(
            output,
            [BEGIN_SYNCHRONIZED_UPDATE, END_SYNCHRONIZED_UPDATE].concat()
        );

        output.clear();
        compositor
            .process(
                &[BEGIN_SYNCHRONIZED_UPDATE, END_SYNCHRONIZED_UPDATE].concat(),
                &mut output,
            )
            .unwrap();
        assert!(
            output
                .windows(LABEL.len())
                .any(|bytes| bytes == LABEL.as_bytes())
        );
    }

    #[test]
    fn resize_below_label_width_clears_visible_badge_without_redrawing() {
        let exact_width = LABEL.len() as u16;
        let narrow_width = exact_width - 1;
        let mut compositor = FrameCompositor::new(VERSION, exact_width);
        let mut output = Vec::new();
        compositor
            .process(ENTER_ALTERNATE_SCREEN, &mut output)
            .unwrap();
        compositor
            .process(
                &[BEGIN_SYNCHRONIZED_UPDATE, END_SYNCHRONIZED_UPDATE].concat(),
                &mut output,
            )
            .unwrap();
        output.clear();

        compositor.set_columns(narrow_width);
        compositor
            .process(
                &[BEGIN_SYNCHRONIZED_UPDATE, END_SYNCHRONIZED_UPDATE].concat(),
                &mut output,
            )
            .unwrap();

        let expected_overlay = format!("\x1b7\x1b[1;1H{}\x1b8", " ".repeat(narrow_width.into()));
        assert!(output.starts_with(BEGIN_SYNCHRONIZED_UPDATE));
        assert!(
            output
                .windows(expected_overlay.len())
                .any(|bytes| bytes == expected_overlay.as_bytes())
        );
        assert!(
            !output
                .windows(LABEL.len())
                .any(|bytes| bytes == LABEL.as_bytes())
        );
        assert!(output.ends_with(END_SYNCHRONIZED_UPDATE));
    }

    #[test]
    fn cleanup_flushes_partial_marker_and_restores_active_modes() {
        let mut compositor = FrameCompositor::new(VERSION, 80);
        let mut output = Vec::new();
        compositor
            .process(
                &[
                    ENTER_ALTERNATE_SCREEN,
                    BEGIN_SYNCHRONIZED_UPDATE,
                    b"\x1b[?20",
                ]
                .concat(),
                &mut output,
            )
            .unwrap();
        compositor.append_terminal_cleanup(&mut output);
        assert!(output.ends_with(b"\x1b[?20\x1b[?2026l\x1b[?1049l\x1b[0m\x1b[?25h"));
        assert!(!compositor.alternate_screen_active());
    }

    #[test]
    fn eligibility_requires_unredirected_normal_terminal() {
        let eligible = TerminalFacts {
            stdin_is_terminal: true,
            stdout_is_terminal: true,
            stderr_is_terminal: true,
            raw_mode_enabled: false,
            term: Some("xterm-256color"),
        };
        assert_eq!(
            overlay_eligibility(&[], eligible),
            OverlayEligibility::Eligible
        );

        for facts in [
            TerminalFacts {
                stdin_is_terminal: false,
                ..eligible
            },
            TerminalFacts {
                stdout_is_terminal: false,
                ..eligible
            },
            TerminalFacts {
                stderr_is_terminal: false,
                ..eligible
            },
            TerminalFacts {
                raw_mode_enabled: true,
                ..eligible
            },
            TerminalFacts {
                term: Some("dumb"),
                ..eligible
            },
        ] {
            assert!(matches!(
                overlay_eligibility(&[], facts),
                OverlayEligibility::Direct(_)
            ));
        }
        assert!(matches!(
            overlay_eligibility(&[OsString::from("--no-alt-screen")], eligible),
            OverlayEligibility::Direct(_)
        ));
    }

    #[test]
    fn detects_noncanonical_terminal_state_from_termios() {
        let (_pty, pts) = pty_process::blocking::open().unwrap();
        assert!(!terminal_is_noncanonical(&pts).unwrap());

        let mut attributes = tcgetattr(&pts).unwrap();
        attributes.local_flags.remove(LocalFlags::ICANON);
        tcsetattr(&pts, SetArg::TCSANOW, &attributes).unwrap();
        assert!(terminal_is_noncanonical(&pts).unwrap());
    }

    #[test]
    fn closed_pty_input_errors_are_nonfatal() {
        assert!(pty_input_closed(&io::Error::new(
            io::ErrorKind::BrokenPipe,
            "closed"
        )));
        assert!(pty_input_closed(&io::Error::from_raw_os_error(
            nix::errno::Errno::EIO as i32
        )));
        assert!(!pty_input_closed(&io::Error::new(
            io::ErrorKind::PermissionDenied,
            "denied"
        )));
    }

    #[tokio::test]
    async fn pty_relays_input_output_and_supports_resize() {
        let (blocking_pty, pts) = pty_process::blocking::open().unwrap();
        blocking_pty.resize(Size::new(24, 80)).unwrap();
        let fd: OwnedFd = blocking_pty.into();
        let flags = OFlag::from_bits_truncate(fcntl(&fd, FcntlArg::F_GETFL).unwrap());
        fcntl(&fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).unwrap();
        // SAFETY: this is the nonblocking master descriptor returned by pty-process.
        let pty = unsafe { pty_process::Pty::from_fd(fd) }.unwrap();
        let (mut reader, mut writer) = pty.into_split();

        let mut child = pty_process::blocking::Command::new("/bin/sh")
            .arg("-c")
            .arg("IFS= read -r value; printf '<%s>' \"$value\"")
            .spawn(pts)
            .unwrap();
        writer.resize(Size::new(40, 100)).unwrap();
        writer.write_all(b"round-trip\n").await.unwrap();

        let output = timeout(Duration::from_secs(2), async {
            let mut output = Vec::new();
            let mut buffer = [0_u8; 1024];
            loop {
                match reader.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(read) => output.extend_from_slice(&buffer[..read]),
                    Err(err) if err.raw_os_error() == Some(nix::errno::Errno::EIO as i32) => {
                        break;
                    }
                    Err(err) => panic!("PTY read failed: {err}"),
                }
            }
            output
        })
        .await
        .unwrap();
        assert!(child.wait().unwrap().success());
        assert!(
            output
                .windows(b"<round-trip>".len())
                .any(|bytes| bytes == b"<round-trip>")
        );
    }

    #[tokio::test]
    async fn child_monitor_reports_stop_resume_and_exit() {
        let (pty, pts) = pty_process::blocking::open().unwrap();
        let child = pty_process::blocking::Command::new("/bin/sh")
            .arg("-c")
            .arg("kill -STOP $$; sleep 0.1; exit 0")
            .spawn(pts)
            .unwrap();
        let pid = Pid::from_raw(i32::try_from(child.id()).unwrap());
        let (child_tx, child_rx) = std_mpsc::sync_channel(0);
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let monitor = std::thread::spawn(move || run_child_monitor(child_rx, event_tx));
        child_tx.send(child).unwrap();

        assert!(matches!(
            timeout(Duration::from_secs(2), event_rx.recv())
                .await
                .unwrap(),
            Some(ChildEvent::Stopped)
        ));
        killpg(pid, UnixSignal::SIGCONT).unwrap();

        let status = timeout(Duration::from_secs(2), async {
            loop {
                match event_rx.recv().await {
                    Some(ChildEvent::Continued) => {}
                    Some(ChildEvent::Exited(status)) => break status,
                    Some(ChildEvent::Stopped) => {}
                    Some(ChildEvent::Error(message)) => panic!("child monitor failed: {message}"),
                    None => panic!("child monitor channel closed before exit"),
                }
            }
        })
        .await
        .unwrap();
        assert!(status.success());
        monitor.join().unwrap();
        drop(pty);
    }

    #[tokio::test]
    #[ignore = "requires a real outer controlling TTY"]
    async fn real_terminal_session_restores_modes_after_overlay() {
        let script = "printf '\\033[?1049h\\033[?2026hframe\\033[?2026l'; \
                      sleep 0.05; printf '\\033[?1049l'";
        let args = [OsString::from("-c"), OsString::from(script)];
        let mut session = match try_spawn(
            "/bin/sh",
            &args,
            "ws://127.0.0.1:1",
            "IGNORED_TOKEN",
            "ignored",
        )
        .unwrap()
        {
            OverlaySpawn::Spawned(session) => session,
            OverlaySpawn::Direct { reason } => {
                panic!("real terminal smoke unexpectedly selected direct mode: {reason}");
            }
        };
        assert!(session.wait().await.unwrap().success());
        assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
    }

    #[test]
    #[ignore = "requires a real outer controlling TTY"]
    fn real_terminal_nonblocking_descriptor_can_be_selected() {
        use std::os::fd::AsFd;
        use std::os::unix::fs::OpenOptionsExt;

        use nix::sys::select::{FdSet, select};
        use nix::sys::time::{TimeVal, TimeValLike};

        let terminal = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags((OFlag::O_NONBLOCK | OFlag::O_CLOEXEC).bits())
            .open("/dev/tty")
            .unwrap();
        assert!(fcntl(&terminal, FcntlArg::F_GETFL).is_ok());
        let mut read_fds = FdSet::new();
        read_fds.insert(terminal.as_fd());
        let mut timeout = TimeVal::milliseconds(1);
        select(None, &mut read_fds, None, None, &mut timeout).unwrap();
    }

    #[tokio::test]
    #[ignore = "requires a real outer shell with job control"]
    async fn real_terminal_child_stop_round_trips_through_shell_job_control() {
        let script = "printf '\\033[?1049h\\033[?2026hbefore-stop\\033[?2026l'; \
                      sleep 0.05; printf '\\033[?1049l'; kill -STOP $$; \
                      printf '\\033[?1049h\\033[?2026hafter-resume\\033[?2026l'; \
                      sleep 0.05; printf '\\033[?1049l'";
        let args = [OsString::from("-c"), OsString::from(script)];
        let mut session = match try_spawn(
            "/bin/sh",
            &args,
            "ws://127.0.0.1:1",
            "IGNORED_TOKEN",
            "ignored",
        )
        .unwrap()
        {
            OverlaySpawn::Spawned(session) => session,
            OverlaySpawn::Direct { reason } => {
                panic!("job-control smoke unexpectedly selected direct mode: {reason}");
            }
        };
        assert!(session.wait().await.unwrap().success());
        assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
    }
}
