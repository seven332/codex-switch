use std::collections::HashMap;
use std::ffi::OsString;
use std::future::pending;
use std::net::TcpListener as StdTcpListener;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use futures_util::stream::SplitSink;
use futures_util::{Sink, SinkExt, StreamExt};
use rand::RngExt as _;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Duration, Instant, sleep, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, accept_hdr_async_with_config, connect_async_with_config,
};
use uuid::Uuid;

use crate::account_selector::{self, SelectionConfig};
use crate::auth_json;
use crate::auto_switch::{self, AutoSwitchPlan, AutoSwitchResult};
use crate::codex_http;
use crate::redaction::redact_known_secrets;
use crate::runtime_log as runtime_logging;
use crate::store;
use crate::token;
use crate::types::{AuthData, StoredAccount};

const REMOTE_TOKEN_ENV: &str = "CODEX_SWITCH_REMOTE_TOKEN";
const APP_SERVER_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const APP_SERVER_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
pub(crate) const AUTO_SWITCH_MAINTENANCE_MIN_INTERVAL: Duration = Duration::from_secs(15 * 60);
pub(crate) const AUTO_SWITCH_MAINTENANCE_MAX_INTERVAL: Duration = Duration::from_secs(45 * 60);
const AUTO_SWITCH_SOFT_COOLDOWN: Duration = Duration::from_secs(15 * 60);
const ACTIVE_ACCOUNT_WATCH_INTERVAL: Duration = Duration::from_secs(1);
const ACTIVE_ACCOUNT_MAX_ATTEMPTS: usize = 5;
const ACTIVE_ACCOUNT_RETRY_DELAYS: [Duration; ACTIVE_ACCOUNT_MAX_ATTEMPTS - 1] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
];
const RUNTIME_BACKGROUND_TASK_STOP_TIMEOUT: Duration = Duration::from_secs(2);
const RUNTIME_COMMAND_BUFFER: usize = 4;
const BACKGROUND_RUNTIME_REQUEST_BUFFER: usize = 4;
const RUNTIME_ACTIVITY_UPDATE_MIN_INTERVAL: Duration = Duration::from_secs(30);
const TOKEN_PREWARM_MIN_WINDOW: Duration = Duration::from_secs(5 * 60);
const TOKEN_PREWARM_MAX_WINDOW: Duration = Duration::from_secs(8 * 60);
const TOKEN_PREWARM_IDLE_AFTER: Duration = Duration::from_secs(10 * 60);
const TOKEN_PREWARM_RETRY_DELAY: Duration = Duration::from_secs(60);
const INTERNAL_REQUEST_ID_PREFIX: &str = "codex-switch/";
const RUNTIME_WEBSOCKET_MAX_MESSAGE_SIZE: usize = 128 * 1024 * 1024;
const RUNTIME_WEBSOCKET_MAX_FRAME_SIZE: usize = RUNTIME_WEBSOCKET_MAX_MESSAGE_SIZE;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type ProxyClientStream = WebSocketStream<TcpStream>;

enum RuntimeCommand {
    LoginPreparedAccount(RuntimeLoginCommand),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RuntimeClientNotification {
    Warning { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RuntimeLoginSuccessNotification {
    Client(RuntimeClientNotification),
    AuthJsonSwitch { to_label: String },
}

struct RuntimeLoginCommand {
    prepared: PreparedAccountLogin,
    expected_snapshot: Option<CurrentAccountSnapshot>,
    generation: Option<u64>,
    completion: Option<oneshot::Sender<RuntimeLoginResult>>,
    success_notification: Option<RuntimeLoginSuccessNotification>,
}

impl RuntimeLoginCommand {
    #[cfg(test)]
    fn fire_and_forget(prepared: PreparedAccountLogin) -> Self {
        let expected_snapshot = prepared.current_snapshot.clone();
        Self {
            prepared,
            expected_snapshot,
            generation: None,
            completion: None,
            success_notification: None,
        }
    }

    fn auto_switch(
        prepared: PreparedAccountLogin,
        generation: u64,
        success_notification: Option<RuntimeLoginSuccessNotification>,
    ) -> Self {
        let expected_snapshot = prepared.current_snapshot.clone();
        Self {
            prepared,
            expected_snapshot,
            generation: Some(generation),
            completion: None,
            success_notification,
        }
    }

    fn reconciled(
        prepared: PreparedAccountLogin,
        expected_snapshot: CurrentAccountSnapshot,
        completion: oneshot::Sender<RuntimeLoginResult>,
        success_notification: Option<RuntimeLoginSuccessNotification>,
    ) -> Self {
        Self {
            prepared,
            expected_snapshot: Some(expected_snapshot),
            generation: None,
            completion: Some(completion),
            success_notification,
        }
    }

    fn prewarm(prepared: PreparedAccountLogin, expected_snapshot: CurrentAccountSnapshot) -> Self {
        Self {
            prepared,
            expected_snapshot: Some(expected_snapshot),
            generation: None,
            completion: None,
            success_notification: None,
        }
    }

    fn with_completion(mut self, completion: oneshot::Sender<RuntimeLoginResult>) -> Self {
        self.completion = Some(completion);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RuntimeLoginResult {
    Success,
    Stale,
    Deferred(String),
    Transient(String),
    Terminal(String),
}

#[derive(Debug, Clone)]
struct RuntimeAuthState {
    loaded: Option<RuntimeLoadedAuth>,
    last_activity_at: Instant,
}

impl RuntimeAuthState {
    fn new(now: Instant) -> Self {
        Self {
            loaded: None,
            last_activity_at: now,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeLoadedAuth {
    account_id: String,
    account_label: String,
    chatgpt_account_id: String,
    access_token_expires_at: Option<DateTime<Utc>>,
    current_snapshot: Option<CurrentAccountSnapshot>,
}

#[derive(Debug)]
enum BackgroundRuntimeRequest {
    AutoSwitch {
        priority: RuntimeAutoSwitchPriority,
        generation: u64,
        mode: AutoSwitchLoginMode,
    },
}

enum RemoteCodexSession {
    Direct(Child),
    #[cfg(unix)]
    Overlay(Box<crate::tui_overlay::TuiOverlaySession>),
}

impl RemoteCodexSession {
    fn pid_for_log(&self) -> String {
        match self {
            Self::Direct(child) => child_pid_for_log(child),
            #[cfg(unix)]
            Self::Overlay(session) => session.id().to_string(),
        }
    }

    async fn wait(&mut self) -> Result<ExitStatus> {
        match self {
            Self::Direct(child) => Ok(child.wait().await?),
            #[cfg(unix)]
            Self::Overlay(session) => session.wait().await,
        }
    }

    async fn shutdown(&mut self) {
        match self {
            Self::Direct(child) => shutdown_child(child).await,
            #[cfg(unix)]
            Self::Overlay(session) => session.shutdown().await,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeAutoSwitchPriority {
    Soft,
    Hard,
}

pub async fn run_codex(codex_bin: String, codex_args: Vec<String>) -> Result<ExitStatus> {
    codex_http::set_codex_bin_for_user_agent(codex_bin.clone());
    validate_remote_capable_codex_args(&codex_args)?;
    reject_remote_args(&codex_args)?;
    let current_dir = std::env::current_dir().context("Failed to read current directory")?;
    let codex_args_summary = format_codex_args_summary_for_log(&codex_args);
    let codex_args = codex_args_with_default_cwd(&codex_args, &current_dir);
    let runtime_log = runtime_logging::init_runtime_tracing()?;
    startup_log(format_args!(
        "runtime log: {}",
        runtime_log.path().display()
    ));
    startup_log(format_args!(
        "run start: codex-bin={codex_bin}, args={}",
        codex_args_summary
    ));
    startup_log(format_args!(
        "codex-switch version: {}",
        env!("CARGO_PKG_VERSION")
    ));
    startup_log(format_args!(
        "codex version: {}",
        codex_http::detected_codex_version().unwrap_or_else(|| "unknown".to_string())
    ));

    let stage_start = Instant::now();
    startup_log("initial auto-switch: start");
    let initial = match auto_switch::auto_switch_allow_running().await {
        Ok(initial) => {
            startup_log(format_args!(
                "initial auto-switch: done in {} ({})",
                format_elapsed(stage_start.elapsed()),
                format_auto_switch_result_for_log(&initial)
            ));
            Some(initial)
        }
        Err(err) => {
            if let Some(reason) = initial_auto_switch_nonfatal_reason(&err) {
                startup_log(format_args!(
                    "initial auto-switch: no usable replacement in {}; continuing without switch: {reason}",
                    format_elapsed(stage_start.elapsed())
                ));
                None
            } else {
                startup_log(format_args!(
                    "initial auto-switch: failed in {}: {err:#}",
                    format_elapsed(stage_start.elapsed())
                ));
                return Err(err).context("Initial account auto-switch failed");
            }
        }
    };
    if let Some(AutoSwitchResult::CurrentUnsupported { reason, .. }) = initial {
        startup_log(format_args!(
            "initial auto-switch: current account unsupported: {reason}"
        ));
        anyhow::bail!("current account does not support runtime auto-switch: {reason}");
    }

    let port = reserve_local_port()?;
    let app_server_url = format!("ws://127.0.0.1:{port}");
    let token = websocket_token();
    let token_path = runtime_token_path()?;
    store::write_private_file(&token_path, &token)?;

    let stage_start = Instant::now();
    startup_log(format_args!("app-server: spawn start ({app_server_url})"));
    let mut app_server = match spawn_app_server(&codex_bin, &app_server_url, &token_path) {
        Ok(app_server) => {
            startup_log(format_args!(
                "app-server: spawned pid={} in {}",
                child_pid_for_log(&app_server),
                format_elapsed(stage_start.elapsed())
            ));
            app_server
        }
        Err(err) => {
            startup_log(format_args!(
                "app-server: spawn failed in {}: {err:#}",
                format_elapsed(stage_start.elapsed())
            ));
            let _ = std::fs::remove_file(&token_path);
            return Err(err);
        }
    };
    let stage_start = Instant::now();
    startup_log("app-server: waiting for ready probe");
    if let Err(err) = wait_for_app_server_ready(&app_server_url, &token, &mut app_server).await {
        startup_log(format_args!(
            "app-server: ready probe failed in {}: {err:#}",
            format_elapsed(stage_start.elapsed())
        ));
        shutdown_child(&mut app_server).await;
        let _ = std::fs::remove_file(&token_path);
        return Err(err);
    }
    startup_log(format_args!(
        "app-server: ready in {}",
        format_elapsed(stage_start.elapsed())
    ));

    let stage_start = Instant::now();
    startup_log("proxy: bind start");
    let (proxy_listener, proxy_url) = match bind_proxy_listener().await {
        Ok(listener) => {
            startup_log(format_args!(
                "proxy: listening at {} in {}",
                listener.1,
                format_elapsed(stage_start.elapsed())
            ));
            listener
        }
        Err(err) => {
            startup_log(format_args!(
                "proxy: bind failed in {}: {err:#}",
                format_elapsed(stage_start.elapsed())
            ));
            shutdown_child(&mut app_server).await;
            let _ = std::fs::remove_file(&token_path);
            return Err(err);
        }
    };
    let (runtime_command_tx, runtime_command_rx) = mpsc::channel(RUNTIME_COMMAND_BUFFER);
    let (background_runtime_tx, background_runtime_rx) =
        mpsc::channel(BACKGROUND_RUNTIME_REQUEST_BUFFER);
    let runtime_auto_switch_coordinator = shared_runtime_auto_switch_coordinator();
    let (maintenance_shutdown_tx, maintenance_shutdown_rx) = watch::channel(false);
    let (runtime_auth_tx, runtime_auth_rx) = watch::channel(RuntimeAuthState::new(Instant::now()));
    let background_runtime_task = tokio::spawn(run_background_runtime_worker(
        background_runtime_rx,
        runtime_command_tx.clone(),
        runtime_auto_switch_coordinator.clone(),
        maintenance_shutdown_rx.clone(),
    ));
    let maintenance_task = tokio::spawn(run_auto_switch_maintenance(
        background_runtime_tx.clone(),
        runtime_auto_switch_coordinator.clone(),
        maintenance_shutdown_rx.clone(),
    ));
    let current_account_reconcile_task = tokio::spawn(run_current_account_reconciler(
        runtime_command_tx.clone(),
        maintenance_shutdown_rx.clone(),
    ));
    let token_prewarm_task = tokio::spawn(run_token_prewarm_maintenance(
        runtime_auth_rx,
        runtime_command_tx.clone(),
        maintenance_shutdown_rx,
    ));
    let mut proxy_task = tokio::spawn(run_websocket_proxy(
        proxy_listener,
        app_server_url.clone(),
        token.clone(),
        background_runtime_tx,
        runtime_auto_switch_coordinator,
        runtime_auth_tx,
        runtime_command_rx,
    ));

    let stage_start = Instant::now();
    startup_log(format_args!(
        "codex tui: spawn start with proxy {proxy_url}"
    ));
    runtime_log.disable_stderr();
    let mut codex_child = match spawn_remote_codex(&codex_bin, &codex_args, &proxy_url, &token)
        .context("Failed to start codex")
    {
        Ok(child) => {
            startup_log(format_args!(
                "codex tui: spawned pid={} in {}; handing terminal to codex",
                child.pid_for_log(),
                format_elapsed(stage_start.elapsed())
            ));
            child
        }
        Err(err) => {
            runtime_log.enable_stderr();
            startup_log(format_args!(
                "codex tui: spawn failed in {}: {err:#}",
                format_elapsed(stage_start.elapsed())
            ));
            proxy_task.abort();
            let _ = proxy_task.await;
            stop_runtime_background_tasks(
                maintenance_shutdown_tx,
                background_runtime_task,
                maintenance_task,
                current_account_reconcile_task,
                token_prewarm_task,
            )
            .await;
            shutdown_child(&mut app_server).await;
            let _ = std::fs::remove_file(&token_path);
            return Err(err);
        }
    };

    let mut proxy_task_completed = false;
    let status = tokio::select! {
        status = codex_child.wait() => status.context("Failed to wait for codex"),
        proxy_result = &mut proxy_task => {
            proxy_task_completed = true;
            match proxy_result {
                Ok(Ok(())) => codex_child.wait().await.context("Failed to wait for codex"),
                Ok(Err(err)) => {
                    codex_child.shutdown().await;
                    Err(err).context("Codex websocket proxy failed")
                }
                Err(err) => {
                    codex_child.shutdown().await;
                    Err(anyhow::anyhow!("Codex websocket proxy task failed: {err}"))
                }
            }
        }
    };
    runtime_log.enable_stderr();

    if !proxy_task_completed {
        proxy_task.abort();
        let _ = proxy_task.await;
    }
    stop_runtime_background_tasks(
        maintenance_shutdown_tx,
        background_runtime_task,
        maintenance_task,
        current_account_reconcile_task,
        token_prewarm_task,
    )
    .await;
    shutdown_child(&mut app_server).await;
    let _ = std::fs::remove_file(&token_path);

    status
}

fn startup_log(message: impl std::fmt::Display) {
    runtime_log(message);
}

fn runtime_log(message: impl std::fmt::Display) {
    let message = sanitize_startup_log_message(&message.to_string());
    tracing::info!(target: "codex_switch", "{message}");
}

fn runtime_log_redacted(message: impl std::fmt::Display, secrets: &[String]) {
    let message = redact_runtime_log_message(&message.to_string(), secrets);
    tracing::info!(target: "codex_switch", "{message}");
}

fn redact_runtime_log_message(message: &str, secrets: &[String]) -> String {
    let message = sanitize_startup_log_message(message);
    let secrets = secrets.iter().map(String::as_str).collect::<Vec<_>>();
    redact_known_secrets(message, &secrets)
}
fn sanitize_startup_log_message(message: &str) -> String {
    message
        .chars()
        .map(|ch| if ch.is_control() { '?' } else { ch })
        .collect()
}

fn format_elapsed(duration: Duration) -> String {
    if duration.as_millis() < 1_000 {
        format!("{}ms", duration.as_millis())
    } else {
        format!("{:.1}s", duration.as_secs_f64())
    }
}

fn format_token_expiry_distance(expires_at: Option<DateTime<Utc>>) -> String {
    match expires_at {
        Some(expires_at) => format!(
            "expires in {}",
            format_elapsed(duration_until_utc(Utc::now(), expires_at))
        ),
        None => "expiry unknown".to_string(),
    }
}

fn format_codex_args_summary_for_log(args: &[String]) -> String {
    let command = match args.first().map(String::as_str) {
        None => "default",
        Some("resume") => "resume",
        Some("fork") => "fork",
        Some(first) if first.starts_with('-') => "default",
        Some(_) => "interactive",
    };
    format!("{command} ({} args)", args.len())
}

fn child_pid_for_log(child: &Child) -> String {
    child
        .id()
        .map(|pid| pid.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn format_auto_switch_result_for_log(result: &AutoSwitchResult) -> String {
    match result {
        AutoSwitchResult::CurrentKept { account, reason } => {
            let reason = sanitize_startup_log_message(reason);
            format!(
                "kept {} ({}) - {reason}",
                sanitize_startup_log_message(&account.name),
                store::short_id(&account.id)
            )
        }
        AutoSwitchResult::CurrentUnsupported { account, reason } => {
            let reason = sanitize_startup_log_message(reason);
            format!(
                "unsupported {} ({}) - {reason}",
                sanitize_startup_log_message(&account.name),
                store::short_id(&account.id)
            )
        }
        AutoSwitchResult::Switched { from, to, reason } => {
            let reason = sanitize_startup_log_message(reason);
            if let Some(from) = from {
                format!(
                    "switched {} ({}) -> {} ({}) - {reason}",
                    sanitize_startup_log_message(&from.name),
                    store::short_id(&from.id),
                    sanitize_startup_log_message(&to.name),
                    store::short_id(&to.id)
                )
            } else {
                format!(
                    "switched to {} ({}) - {reason}",
                    sanitize_startup_log_message(&to.name),
                    store::short_id(&to.id)
                )
            }
        }
    }
}

fn initial_auto_switch_nonfatal_reason(err: &anyhow::Error) -> Option<&str> {
    err.downcast_ref::<auto_switch::NoUsableReplacement>()
        .map(auto_switch::NoUsableReplacement::reason)
}

fn validate_remote_capable_codex_args(args: &[String]) -> Result<()> {
    let Some(first_arg) = args.first() else {
        return Ok(());
    };
    if first_arg.starts_with('-') || matches!(first_arg.as_str(), "resume" | "fork") {
        return Ok(());
    }
    anyhow::bail!(
        "codex-switch run only supports Codex interactive commands that accept --remote: codex, codex resume, or codex fork"
    );
}

fn reject_remote_args(args: &[String]) -> Result<()> {
    if args
        .iter()
        .any(|arg| arg == "--remote" || arg.starts_with("--remote="))
    {
        anyhow::bail!("do not pass --remote to codex-switch run");
    }
    if args
        .iter()
        .any(|arg| arg == "--remote-auth-token-env" || arg.starts_with("--remote-auth-token-env="))
    {
        anyhow::bail!("do not pass --remote-auth-token-env to codex-switch run");
    }
    Ok(())
}

fn codex_args_with_default_cwd(args: &[String], cwd: &Path) -> Vec<OsString> {
    let mut result: Vec<OsString> = args.iter().map(OsString::from).collect();
    if has_cwd_arg(args) {
        return result;
    }

    let insert_at = match args.first().map(String::as_str) {
        Some("resume" | "fork") => 1,
        _ => 0,
    };
    result.insert(insert_at, OsString::from("-C"));
    result.insert(insert_at + 1, cwd.as_os_str().to_os_string());
    result
}

fn has_cwd_arg(args: &[String]) -> bool {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == "-C" || arg == "--cd" || arg.starts_with("--cd=") || is_short_cd_arg(arg))
}

fn is_short_cd_arg(arg: &str) -> bool {
    arg.len() > 2 && arg.starts_with("-C")
}

fn reserve_local_port() -> Result<u16> {
    let listener = StdTcpListener::bind(("127.0.0.1", 0))
        .context("Failed to reserve local app-server port")?;
    Ok(listener
        .local_addr()
        .context("Failed to read local app-server port")?
        .port())
}

async fn bind_proxy_listener() -> Result<(TcpListener, String)> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("Failed to bind local Codex websocket proxy")?;
    let port = listener
        .local_addr()
        .context("Failed to read local Codex websocket proxy port")?
        .port();
    Ok((listener, format!("ws://127.0.0.1:{port}")))
}

fn websocket_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn runtime_token_path() -> Result<PathBuf> {
    Ok(store::config_dir()?
        .join("runtime")
        .join(format!("ws-token-{}", Uuid::new_v4())))
}

fn spawn_app_server(codex_bin: &str, websocket_url: &str, token_path: &Path) -> Result<Child> {
    Command::new(codex_bin)
        .arg("app-server")
        .arg("--listen")
        .arg(websocket_url)
        .arg("--ws-auth")
        .arg("capability-token")
        .arg("--ws-token-file")
        .arg(token_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("Failed to start Codex app-server with {codex_bin}"))
}

async fn wait_for_app_server_ready(
    websocket_url: &str,
    token: &str,
    app_server: &mut Child,
) -> Result<()> {
    let deadline = Instant::now() + APP_SERVER_STARTUP_TIMEOUT;
    let mut last_error_message = None;

    loop {
        if Instant::now() >= deadline {
            let detail =
                last_error_message.unwrap_or_else(|| "connection was not ready".to_string());
            anyhow::bail!("Timed out waiting for Codex app-server: {detail}");
        }

        if let Some(status) = app_server
            .try_wait()
            .context("Failed to inspect Codex app-server status")?
        {
            anyhow::bail!("Codex app-server exited during startup: {status}");
        }

        match probe_app_server(websocket_url, token).await {
            Ok(()) => return Ok(()),
            Err(err) => last_error_message = Some(err.to_string()),
        }

        sleep(Duration::from_millis(100)).await;
    }
}

async fn probe_app_server(websocket_url: &str, token: &str) -> Result<()> {
    let mut websocket = connect_app_server_websocket(websocket_url, token).await?;
    initialize_app_server(&mut websocket).await?;
    websocket
        .close(None)
        .await
        .context("Failed to close Codex app-server probe websocket")
}

fn spawn_remote_codex(
    codex_bin: &str,
    codex_args: &[OsString],
    websocket_url: &str,
    token: &str,
) -> Result<RemoteCodexSession> {
    #[cfg(unix)]
    match crate::tui_overlay::try_spawn(
        codex_bin,
        codex_args,
        websocket_url,
        REMOTE_TOKEN_ENV,
        token,
    )? {
        crate::tui_overlay::OverlaySpawn::Spawned(session) => {
            runtime_log("codex tui terminal mode: PTY overlay");
            return Ok(RemoteCodexSession::Overlay(session));
        }
        crate::tui_overlay::OverlaySpawn::Direct { reason } => {
            runtime_log(format_args!("codex tui terminal mode: direct ({reason})"));
        }
    }

    let mut command = Command::new(codex_bin);
    command
        .args(codex_args)
        .arg("--remote")
        .arg(websocket_url)
        .arg("--remote-auth-token-env")
        .arg(REMOTE_TOKEN_ENV)
        .env(REMOTE_TOKEN_ENV, token);
    Ok(RemoteCodexSession::Direct(command.spawn()?))
}

async fn shutdown_child(child: &mut Child) {
    match child.try_wait() {
        Ok(Some(_)) => {}
        Ok(None) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        Err(_) => {
            let _ = child.start_kill();
        }
    }
}

async fn run_websocket_proxy(
    listener: TcpListener,
    app_server_url: String,
    token: String,
    background_requests: mpsc::Sender<BackgroundRuntimeRequest>,
    runtime_auto_switch_coordinator: SharedRuntimeAutoSwitchCoordinator,
    runtime_auth: watch::Sender<RuntimeAuthState>,
    mut runtime_commands: mpsc::Receiver<RuntimeCommand>,
) -> Result<()> {
    let mut state = ProxyState::new(
        background_requests,
        runtime_auto_switch_coordinator,
        runtime_auth,
    );

    loop {
        let (client_stream, _) = listener
            .accept()
            .await
            .context("Failed to accept Codex websocket client")?;
        let client_websocket = accept_proxy_client(client_stream, &token).await?;
        let app_server_websocket = connect_app_server_websocket(&app_server_url, &token).await?;

        proxy_websockets(
            client_websocket,
            app_server_websocket,
            &mut state,
            &mut runtime_commands,
        )
        .await?;
        state.clear_connection_pending();
    }
}

async fn stop_runtime_background_tasks(
    shutdown: watch::Sender<bool>,
    background_runtime_task: JoinHandle<()>,
    maintenance_task: JoinHandle<()>,
    current_account_reconcile_task: JoinHandle<()>,
    token_prewarm_task: JoinHandle<()>,
) {
    let _ = shutdown.send(true);
    stop_task_with_timeout(background_runtime_task).await;
    stop_task_with_timeout(maintenance_task).await;
    stop_task_with_timeout(current_account_reconcile_task).await;
    stop_task_with_timeout(token_prewarm_task).await;
}

async fn stop_task_with_timeout(mut task: JoinHandle<()>) {
    tokio::select! {
        _ = &mut task => {}
        _ = sleep(RUNTIME_BACKGROUND_TASK_STOP_TIMEOUT) => {
            task.abort();
            let _ = task.await;
        }
    }
}

type SharedRuntimeAutoSwitchCoordinator = Arc<RuntimeAutoSwitchCoordinator>;

fn shared_runtime_auto_switch_coordinator() -> SharedRuntimeAutoSwitchCoordinator {
    Arc::new(RuntimeAutoSwitchCoordinator::new(AUTO_SWITCH_SOFT_COOLDOWN))
}

struct RuntimeAutoSwitchCoordinator {
    schedule: StdMutex<RuntimeAutoSwitchSchedule>,
}

impl RuntimeAutoSwitchCoordinator {
    fn new(cooldown: Duration) -> Self {
        Self {
            schedule: StdMutex::new(RuntimeAutoSwitchSchedule::new(cooldown)),
        }
    }

    fn current_generation(&self) -> u64 {
        self.schedule
            .lock()
            .map(|schedule| schedule.generation)
            .unwrap_or_default()
    }

    fn is_current_generation(&self, generation: u64) -> bool {
        self.current_generation() == generation
    }
}

#[derive(Debug)]
struct RuntimeAutoSwitchSchedule {
    cooldown: Duration,
    last_attempt: Option<Instant>,
    in_flight: bool,
    generation: u64,
}

impl RuntimeAutoSwitchSchedule {
    fn new(cooldown: Duration) -> Self {
        Self {
            cooldown,
            last_attempt: None,
            in_flight: false,
            generation: 0,
        }
    }

    fn try_start_background(&mut self, now: Instant) -> (BackgroundAutoSwitchQueueStatus, u64) {
        if self.in_flight {
            return (BackgroundAutoSwitchQueueStatus::InFlight, self.generation);
        }
        if let Some(last_attempt) = self.last_attempt
            && now < last_attempt + self.cooldown
        {
            return (BackgroundAutoSwitchQueueStatus::Cooldown, self.generation);
        }

        self.last_attempt = Some(now);
        self.in_flight = true;
        (BackgroundAutoSwitchQueueStatus::Queued, self.generation)
    }

    fn finish(&mut self) {
        self.in_flight = false;
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum BackgroundAutoSwitchQueueStatus {
    Queued,
    Cooldown,
    InFlight,
    Full,
    Closed,
}

fn queue_background_auto_switch(
    requests: &mpsc::Sender<BackgroundRuntimeRequest>,
    coordinator: &SharedRuntimeAutoSwitchCoordinator,
    now: Instant,
) -> BackgroundAutoSwitchQueueStatus {
    let Ok(mut schedule) = coordinator.schedule.lock() else {
        return BackgroundAutoSwitchQueueStatus::Closed;
    };

    let previous_last_attempt = schedule.last_attempt;
    let (status, generation) = schedule.try_start_background(now);
    if status != BackgroundAutoSwitchQueueStatus::Queued {
        return status;
    }

    match requests.try_send(BackgroundRuntimeRequest::AutoSwitch {
        priority: RuntimeAutoSwitchPriority::Soft,
        generation,
        mode: AutoSwitchLoginMode::SwitchedOnly,
    }) {
        Ok(()) => BackgroundAutoSwitchQueueStatus::Queued,
        Err(mpsc::error::TrySendError::Full(_)) => {
            schedule.last_attempt = previous_last_attempt;
            schedule.finish();
            BackgroundAutoSwitchQueueStatus::Full
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            schedule.last_attempt = previous_last_attempt;
            schedule.finish();
            BackgroundAutoSwitchQueueStatus::Closed
        }
    }
}

fn queue_hard_auto_switch(
    requests: &mpsc::Sender<BackgroundRuntimeRequest>,
    coordinator: &SharedRuntimeAutoSwitchCoordinator,
    mode: AutoSwitchLoginMode,
) -> BackgroundAutoSwitchQueueStatus {
    let Ok(mut schedule) = coordinator.schedule.lock() else {
        return BackgroundAutoSwitchQueueStatus::Closed;
    };
    let generation = schedule.generation.saturating_add(1);

    match requests.try_send(BackgroundRuntimeRequest::AutoSwitch {
        priority: RuntimeAutoSwitchPriority::Hard,
        generation,
        mode,
    }) {
        Ok(()) => {
            schedule.generation = generation;
            BackgroundAutoSwitchQueueStatus::Queued
        }
        Err(mpsc::error::TrySendError::Full(_)) => BackgroundAutoSwitchQueueStatus::Full,
        Err(mpsc::error::TrySendError::Closed(_)) => BackgroundAutoSwitchQueueStatus::Closed,
    }
}

fn finish_background_auto_switch(coordinator: &SharedRuntimeAutoSwitchCoordinator) {
    if let Ok(mut schedule) = coordinator.schedule.lock() {
        schedule.finish();
    }
}

async fn run_background_runtime_worker(
    mut requests: mpsc::Receiver<BackgroundRuntimeRequest>,
    runtime_commands: mpsc::Sender<RuntimeCommand>,
    coordinator: SharedRuntimeAutoSwitchCoordinator,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut in_flight: Option<RuntimeAutoSwitchInFlight> = None;

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_ok() && *shutdown.borrow() {
                    if let Some(in_flight) = in_flight.take() {
                        in_flight.handle.abort();
                    }
                    return;
                }
            }
            request = requests.recv() => {
                let Some(request) = request else {
                    if let Some(in_flight) = in_flight.take() {
                        in_flight.handle.abort();
                    }
                    return;
                };

                handle_runtime_auto_switch_request(request, &mut in_flight, &coordinator);
            }
            result = next_runtime_auto_switch_result(&mut in_flight), if in_flight.is_some() => {
                if let Some(result) = result {
                    if result.priority == RuntimeAutoSwitchPriority::Soft {
                        finish_background_auto_switch(&coordinator);
                    }
                    if runtime_auto_switch_result_is_current(&result, &coordinator) {
                        let command = runtime_auto_switch_command_from_work(
                            result.work,
                            result.generation,
                            &coordinator,
                        )
                        .await;
                        if let Some(command) = command
                            && try_send_background_runtime_command(&runtime_commands, command)
                                == RuntimeCommandSendStatus::Closed
                        {
                            return;
                        }
                    }
                }
            }
        }
    }
}

fn handle_runtime_auto_switch_request(
    request: BackgroundRuntimeRequest,
    in_flight: &mut Option<RuntimeAutoSwitchInFlight>,
    coordinator: &SharedRuntimeAutoSwitchCoordinator,
) {
    let BackgroundRuntimeRequest::AutoSwitch {
        priority,
        generation,
        mode,
    } = request;

    if !coordinator.is_current_generation(generation) {
        if priority == RuntimeAutoSwitchPriority::Soft {
            finish_background_auto_switch(coordinator);
        }
        return;
    }

    if let Some(current) = in_flight.take() {
        match (priority, current.priority) {
            (RuntimeAutoSwitchPriority::Hard, RuntimeAutoSwitchPriority::Soft) => {
                current.handle.abort();
                finish_background_auto_switch(coordinator);
            }
            (RuntimeAutoSwitchPriority::Hard, RuntimeAutoSwitchPriority::Hard) => {
                current.handle.abort();
            }
            (RuntimeAutoSwitchPriority::Soft, _) => {
                *in_flight = Some(current);
                finish_background_auto_switch(coordinator);
                return;
            }
        }
    }

    *in_flight = Some(RuntimeAutoSwitchInFlight {
        priority,
        handle: tokio::spawn(async move {
            let expected_snapshot = current_account_snapshot().ok();
            let work = runtime_auto_switch_work(mode).await.ok().flatten();
            RuntimeAutoSwitchTaskResult {
                priority,
                generation,
                expected_snapshot,
                work,
            }
        }),
    });
}

struct RuntimeAutoSwitchInFlight {
    priority: RuntimeAutoSwitchPriority,
    handle: JoinHandle<RuntimeAutoSwitchTaskResult>,
}

struct RuntimeAutoSwitchTaskResult {
    priority: RuntimeAutoSwitchPriority,
    generation: u64,
    expected_snapshot: Option<CurrentAccountSnapshot>,
    work: Option<RuntimeAutoSwitchWork>,
}

enum RuntimeAutoSwitchWork {
    Switch(AutoSwitchPlan),
    KeepCurrent(Box<StoredAccount>),
    LoginCurrent(Box<StoredAccount>),
}

fn runtime_auto_switch_result_is_current(
    result: &RuntimeAutoSwitchTaskResult,
    coordinator: &SharedRuntimeAutoSwitchCoordinator,
) -> bool {
    if !coordinator.is_current_generation(result.generation) {
        return false;
    }

    match &result.expected_snapshot {
        Some(snapshot) => current_snapshot_matches(snapshot).unwrap_or(false),
        None => current_account_snapshot()
            .map(|snapshot| snapshot.current_account_id.is_none())
            .unwrap_or(false),
    }
}

async fn next_runtime_auto_switch_result(
    in_flight: &mut Option<RuntimeAutoSwitchInFlight>,
) -> Option<RuntimeAutoSwitchTaskResult> {
    let Some(current) = in_flight else {
        pending().await
    };

    let result = (&mut current.handle).await.ok();
    *in_flight = None;
    result
}

fn try_send_background_runtime_command(
    runtime_commands: &mpsc::Sender<RuntimeCommand>,
    command: RuntimeCommand,
) -> RuntimeCommandSendStatus {
    match runtime_commands.try_send(command) {
        Ok(()) => RuntimeCommandSendStatus::Sent,
        Err(mpsc::error::TrySendError::Full(_)) => RuntimeCommandSendStatus::Full,
        Err(mpsc::error::TrySendError::Closed(_)) => RuntimeCommandSendStatus::Closed,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeCommandSendStatus {
    Sent,
    Full,
    Closed,
}

async fn runtime_auto_switch_work(
    mode: AutoSwitchLoginMode,
) -> Result<Option<RuntimeAutoSwitchWork>> {
    let plan = auto_switch::plan_auto_switch_for_runtime().await?;
    auto_switch_plan_work(plan, &mode)
}

#[derive(Debug, Clone)]
enum AutoSwitchLoginMode {
    SwitchedOnly,
    EnsureCurrentLogged {
        app_server_account_id: Option<String>,
        app_server_snapshot: Option<CurrentAccountSnapshot>,
    },
}

fn auto_switch_plan_work(
    plan: AutoSwitchPlan,
    mode: &AutoSwitchLoginMode,
) -> Result<Option<RuntimeAutoSwitchWork>> {
    match plan {
        AutoSwitchPlan::Switch { .. } => Ok(Some(RuntimeAutoSwitchWork::Switch(plan))),
        AutoSwitchPlan::CurrentKept { account, .. } => match mode {
            AutoSwitchLoginMode::SwitchedOnly => {
                Ok(Some(RuntimeAutoSwitchWork::KeepCurrent(account)))
            }
            AutoSwitchLoginMode::EnsureCurrentLogged {
                app_server_account_id,
                app_server_snapshot,
            } => {
                let (account, current_snapshot) = latest_current_account_snapshot(*account)?;
                let login_account = select_current_kept_login_account(
                    account.clone(),
                    current_snapshot,
                    app_server_account_id.as_deref(),
                    app_server_snapshot.as_ref(),
                );
                Ok(Some(match login_account {
                    Some(account) => RuntimeAutoSwitchWork::LoginCurrent(Box::new(account)),
                    None => RuntimeAutoSwitchWork::KeepCurrent(Box::new(account)),
                }))
            }
        },
        AutoSwitchPlan::CurrentUnsupported { .. } => Ok(None),
    }
}

async fn runtime_auto_switch_command_from_work(
    work: Option<RuntimeAutoSwitchWork>,
    generation: u64,
    coordinator: &SharedRuntimeAutoSwitchCoordinator,
) -> Option<RuntimeCommand> {
    if !coordinator.is_current_generation(generation) {
        return None;
    }

    let mut success_notification = None;
    let account = match work? {
        RuntimeAutoSwitchWork::Switch(plan) => {
            match auto_switch::commit_auto_switch_plan(plan, true)
                .await
                .ok()?
            {
                AutoSwitchResult::Switched { from, to, reason } => {
                    success_notification =
                        runtime_auto_switch_success_notification(from.as_deref(), &to, &reason)
                            .map(RuntimeLoginSuccessNotification::Client);
                    *to
                }
                AutoSwitchResult::CurrentKept { account, .. } => *account,
                AutoSwitchResult::CurrentUnsupported { .. } => return None,
            }
        }
        RuntimeAutoSwitchWork::KeepCurrent(account) => {
            sync_current_account_auth(*account).ok().flatten()?;
            return None;
        }
        RuntimeAutoSwitchWork::LoginCurrent(account) => {
            sync_current_account_auth(*account).ok().flatten()?
        }
    };

    if !coordinator.is_current_generation(generation) {
        return None;
    }

    let prepared = prepare_login(account).await.ok()?;
    if !coordinator.is_current_generation(generation) {
        return None;
    }

    Some(RuntimeCommand::LoginPreparedAccount(
        RuntimeLoginCommand::auto_switch(prepared, generation, success_notification),
    ))
}

fn runtime_auto_switch_success_notification(
    from: Option<&StoredAccount>,
    to: &StoredAccount,
    reason: &str,
) -> Option<RuntimeClientNotification> {
    if from.is_some_and(|from| from.id == to.id) {
        return None;
    }

    let to_label = display_safe_account_label(to);
    let reason = sanitize_startup_log_message(reason);
    let message = match from {
        Some(from) => format!(
            "codex-switch switched account from {} to {} because {reason}",
            display_safe_account_label(from),
            to_label
        ),
        None => format!("codex-switch switched account to {to_label} because {reason}"),
    };

    Some(RuntimeClientNotification::Warning { message })
}

fn runtime_auth_json_switch_notification(to: &StoredAccount) -> RuntimeLoginSuccessNotification {
    RuntimeLoginSuccessNotification::AuthJsonSwitch {
        to_label: display_safe_account_label(to),
    }
}

fn runtime_login_success_client_notification(
    notification: RuntimeLoginSuccessNotification,
    from_label: Option<&str>,
) -> RuntimeClientNotification {
    match notification {
        RuntimeLoginSuccessNotification::Client(notification) => notification,
        RuntimeLoginSuccessNotification::AuthJsonSwitch { to_label } => {
            let reason = "current auth.json changed";
            let message = match from_label {
                Some(from_label) => {
                    format!(
                        "codex-switch switched account from {from_label} to {to_label} because {reason}"
                    )
                }
                None => format!("codex-switch switched account to {to_label} because {reason}"),
            };
            RuntimeClientNotification::Warning { message }
        }
    }
}

fn runtime_login_success_client_notification_if_needed(
    notification: RuntimeLoginSuccessNotification,
    from_label: Option<&str>,
    account_changed: bool,
) -> Option<RuntimeClientNotification> {
    match notification {
        RuntimeLoginSuccessNotification::Client(notification) => Some(notification),
        RuntimeLoginSuccessNotification::AuthJsonSwitch { .. } if account_changed => Some(
            runtime_login_success_client_notification(notification, from_label),
        ),
        RuntimeLoginSuccessNotification::AuthJsonSwitch { .. } => None,
    }
}

fn display_safe_account_label(account: &StoredAccount) -> String {
    format!(
        "{} ({})",
        sanitize_startup_log_message(&account.name),
        store::short_id(&account.id)
    )
}

fn current_account_snapshot_for_account(account: &StoredAccount) -> CurrentAccountSnapshot {
    CurrentAccountSnapshot {
        current_account_id: Some(account.id.clone()),
        auth_marker: Some(current_account_auth_marker(account)),
    }
}

fn select_current_kept_login_account(
    account: StoredAccount,
    current_snapshot: CurrentAccountSnapshot,
    app_server_account_id: Option<&str>,
    app_server_snapshot: Option<&CurrentAccountSnapshot>,
) -> Option<StoredAccount> {
    if app_server_account_id == Some(account.id.as_str())
        && app_server_snapshot == Some(&current_snapshot)
    {
        None
    } else {
        Some(account)
    }
}

fn latest_current_account_snapshot(
    fallback_account: StoredAccount,
) -> Result<(StoredAccount, CurrentAccountSnapshot)> {
    let store = store::load_accounts()?;
    let account = store
        .accounts
        .into_iter()
        .find(|account| account.id == fallback_account.id)
        .unwrap_or(fallback_account);
    let snapshot = current_account_snapshot_for_account(&account);
    Ok((account, snapshot))
}

fn sync_current_account_auth(account: StoredAccount) -> Result<Option<StoredAccount>> {
    let store = store::load_accounts()?;
    let account = store
        .accounts
        .iter()
        .find(|stored| stored.id == account.id)
        .cloned()
        .unwrap_or(account);
    let Some(current_auth_account) = auth_json::current_auth_account()? else {
        return Ok(None);
    };
    let Some(current_account) = store::find_matching_account(&store, &current_auth_account) else {
        return Ok(None);
    };
    if current_account.id != account.id {
        return Ok(None);
    }

    if !auth_serialized_credentials_match(&current_auth_account, &account) {
        auth_json::write_account_auth(&account)?;
    }
    Ok(Some(account))
}

fn auth_serialized_credentials_match(left: &StoredAccount, right: &StoredAccount) -> bool {
    if left.token_last_refresh_at != right.token_last_refresh_at {
        return false;
    }

    match (&left.auth_data, &right.auth_data) {
        (AuthData::ApiKey { key: left_key }, AuthData::ApiKey { key: right_key }) => {
            left_key == right_key
        }
        (
            AuthData::ChatGPT {
                id_token: left_id_token,
                access_token: left_access_token,
                refresh_token: left_refresh_token,
                account_id: left_account_id,
            },
            AuthData::ChatGPT {
                id_token: right_id_token,
                access_token: right_access_token,
                refresh_token: right_refresh_token,
                account_id: right_account_id,
            },
        ) => {
            left_id_token == right_id_token
                && left_access_token == right_access_token
                && left_refresh_token == right_refresh_token
                && left_account_id == right_account_id
        }
        _ => false,
    }
}

async fn run_auto_switch_maintenance(
    background_requests: mpsc::Sender<BackgroundRuntimeRequest>,
    runtime_auto_switch_coordinator: SharedRuntimeAutoSwitchCoordinator,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        if sleep_until_shutdown(random_auto_switch_maintenance_interval(), &mut shutdown).await {
            return;
        }

        if queue_background_auto_switch(
            &background_requests,
            &runtime_auto_switch_coordinator,
            Instant::now(),
        ) == BackgroundAutoSwitchQueueStatus::Closed
        {
            return;
        }
    }
}

async fn run_token_prewarm_maintenance(
    mut runtime_auth: watch::Receiver<RuntimeAuthState>,
    runtime_commands: mpsc::Sender<RuntimeCommand>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut retry_after: Option<Instant> = None;
    let mut suppressed_loaded_auth: Option<RuntimeLoadedAuth> = None;

    loop {
        let now = Instant::now();
        if let Some(next_attempt) = retry_after
            && now < next_attempt
        {
            if sleep_until_runtime_auth_change_or_shutdown(
                next_attempt - now,
                &mut runtime_auth,
                &mut shutdown,
            )
            .await
            {
                return;
            }
            continue;
        }
        retry_after = None;

        let state = runtime_auth.borrow().clone();
        if token_prewarm_is_suppressed(&state, &suppressed_loaded_auth) {
            if wait_for_runtime_auth_change_or_shutdown(&mut runtime_auth, &mut shutdown).await {
                return;
            }
            continue;
        }
        suppressed_loaded_auth = None;

        match token_prewarm_decision(&state, now, Utc::now(), random_token_prewarm_window()) {
            TokenPrewarmDecision::WaitForRuntimeAuth => {
                if wait_for_runtime_auth_change_or_shutdown(&mut runtime_auth, &mut shutdown).await
                {
                    return;
                }
            }
            TokenPrewarmDecision::Wait(duration) => {
                if sleep_until_runtime_auth_change_or_shutdown(
                    duration,
                    &mut runtime_auth,
                    &mut shutdown,
                )
                .await
                {
                    return;
                }
            }
            TokenPrewarmDecision::SkipIdle => {
                runtime_log("token prewarm: skipped idle runtime");
                if wait_for_runtime_auth_change_or_shutdown(&mut runtime_auth, &mut shutdown).await
                {
                    return;
                }
            }
            TokenPrewarmDecision::Refresh(loaded_auth) => {
                match run_token_prewarm_attempt(
                    loaded_auth.clone(),
                    &runtime_commands,
                    &mut shutdown,
                )
                .await
                {
                    TokenPrewarmAttemptResult::RetryLater => {
                        retry_after = Some(Instant::now() + TOKEN_PREWARM_RETRY_DELAY);
                    }
                    TokenPrewarmAttemptResult::SuppressUntilRuntimeAuthChanges => {
                        suppressed_loaded_auth = Some(loaded_auth);
                    }
                }
            }
        }
    }
}

fn token_prewarm_is_suppressed(
    state: &RuntimeAuthState,
    suppressed_loaded_auth: &Option<RuntimeLoadedAuth>,
) -> bool {
    suppressed_loaded_auth.as_ref().is_some_and(|suppressed| {
        state
            .loaded
            .as_ref()
            .is_some_and(|loaded| runtime_loaded_auth_matches_for_suppression(loaded, suppressed))
    })
}

fn runtime_loaded_auth_matches_for_suppression(
    loaded: &RuntimeLoadedAuth,
    suppressed: &RuntimeLoadedAuth,
) -> bool {
    loaded.account_id == suppressed.account_id
        && loaded.chatgpt_account_id == suppressed.chatgpt_account_id
        && loaded.access_token_expires_at == suppressed.access_token_expires_at
        && loaded.current_snapshot == suppressed.current_snapshot
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TokenPrewarmDecision {
    WaitForRuntimeAuth,
    Wait(Duration),
    SkipIdle,
    Refresh(RuntimeLoadedAuth),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenPrewarmAttemptResult {
    RetryLater,
    SuppressUntilRuntimeAuthChanges,
}

fn token_prewarm_decision(
    state: &RuntimeAuthState,
    now: Instant,
    utc_now: DateTime<Utc>,
    prewarm_window: Duration,
) -> TokenPrewarmDecision {
    let Some(loaded_auth) = state.loaded.clone() else {
        return TokenPrewarmDecision::WaitForRuntimeAuth;
    };
    let Some(expires_at) = loaded_auth.access_token_expires_at else {
        return TokenPrewarmDecision::WaitForRuntimeAuth;
    };
    let expires_in = duration_until_utc(utc_now, expires_at);
    if expires_in > prewarm_window {
        return TokenPrewarmDecision::Wait(expires_in - prewarm_window);
    }
    if now.saturating_duration_since(state.last_activity_at) >= TOKEN_PREWARM_IDLE_AFTER {
        return TokenPrewarmDecision::SkipIdle;
    }
    TokenPrewarmDecision::Refresh(loaded_auth)
}

async fn run_token_prewarm_attempt(
    loaded_auth: RuntimeLoadedAuth,
    runtime_commands: &mpsc::Sender<RuntimeCommand>,
    shutdown: &mut watch::Receiver<bool>,
) -> TokenPrewarmAttemptResult {
    let stage_start = Instant::now();
    let account_label = &loaded_auth.account_label;
    let mut log_secrets = stored_account_secret_values(&loaded_auth.account_id);
    runtime_log(format_args!(
        "token prewarm: started for {account_label} ({})",
        format_token_expiry_distance(loaded_auth.access_token_expires_at)
    ));

    let result = token_prewarm_login_command(&loaded_auth).await;
    let command = match result {
        Ok(Some(command)) => command,
        Ok(None) => {
            runtime_log(format_args!(
                "token prewarm: stale for {account_label} in {}",
                format_elapsed(stage_start.elapsed())
            ));
            return TokenPrewarmAttemptResult::RetryLater;
        }
        Err(err) => {
            let result = classify_runtime_login_error(&err.to_string());
            runtime_log_redacted(
                format_args!(
                    "token prewarm: failed for {account_label} in {}: {err:#}",
                    format_elapsed(stage_start.elapsed())
                ),
                &log_secrets,
            );
            return token_prewarm_attempt_result_for_login_result(&result);
        }
    };
    log_secrets.extend(runtime_login_command_secret_values(&command));

    let (completion_tx, completion_rx) = oneshot::channel();
    let command = command.with_completion(completion_tx);
    match runtime_commands.try_send(RuntimeCommand::LoginPreparedAccount(command)) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            runtime_log_redacted(
                format_args!(
                    "token prewarm: deferred for {account_label} in {}: runtime command queue is full",
                    format_elapsed(stage_start.elapsed())
                ),
                &log_secrets,
            );
            return TokenPrewarmAttemptResult::RetryLater;
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            return TokenPrewarmAttemptResult::RetryLater;
        }
    }

    let result = wait_for_prewarm_login_completion(completion_rx, shutdown).await;
    match &result {
        Some(RuntimeLoginResult::Success) => {
            runtime_log(format_args!(
                "token prewarm: succeeded for {account_label} in {}",
                format_elapsed(stage_start.elapsed())
            ));
        }
        Some(RuntimeLoginResult::Stale) => {
            runtime_log(format_args!(
                "token prewarm: discarded stale result for {account_label} in {}",
                format_elapsed(stage_start.elapsed())
            ));
        }
        Some(RuntimeLoginResult::Deferred(reason)) => runtime_log_redacted(
            format_args!(
                "token prewarm: deferred for {account_label} in {}: {reason}",
                format_elapsed(stage_start.elapsed())
            ),
            &log_secrets,
        ),
        Some(RuntimeLoginResult::Transient(reason) | RuntimeLoginResult::Terminal(reason)) => {
            runtime_log_redacted(
                format_args!(
                    "token prewarm: failed for {account_label} in {}: {reason}",
                    format_elapsed(stage_start.elapsed())
                ),
                &log_secrets,
            );
        }
        None => {}
    }

    result
        .as_ref()
        .map(token_prewarm_attempt_result_for_login_result)
        .unwrap_or(TokenPrewarmAttemptResult::RetryLater)
}

fn token_prewarm_attempt_result_for_login_result(
    result: &RuntimeLoginResult,
) -> TokenPrewarmAttemptResult {
    match result {
        RuntimeLoginResult::Success
        | RuntimeLoginResult::Stale
        | RuntimeLoginResult::Deferred(_)
        | RuntimeLoginResult::Transient(_) => TokenPrewarmAttemptResult::RetryLater,
        RuntimeLoginResult::Terminal(_) => {
            TokenPrewarmAttemptResult::SuppressUntilRuntimeAuthChanges
        }
    }
}

async fn token_prewarm_login_command(
    loaded_auth: &RuntimeLoadedAuth,
) -> Result<Option<RuntimeLoginCommand>> {
    let Some(account) = current_runtime_oauth_account(&loaded_auth.account_id)? else {
        return Ok(None);
    };
    if !runtime_chatgpt_account_matches(&account, loaded_auth) {
        return Ok(None);
    }
    let account = if current_account_has_newer_access_token(&account, loaded_auth) {
        let Some(account) = sync_current_account_auth(account)? else {
            return Ok(None);
        };
        account
    } else {
        token::refresh_chatgpt_tokens(&account).await?
    };
    if !runtime_chatgpt_account_matches(&account, loaded_auth) {
        return Ok(None);
    }
    if !current_account_id_matches(&loaded_auth.account_id)? {
        return Ok(None);
    }
    let Some(expected_snapshot) = current_prewarm_snapshot_for_account(&account)? else {
        return Ok(None);
    };
    let prepared = prepare_login_from_fresh_account(account)?;
    Ok(Some(RuntimeLoginCommand::prewarm(
        prepared,
        expected_snapshot,
    )))
}

fn current_runtime_oauth_account(account_id: &str) -> Result<Option<StoredAccount>> {
    let store = store::load_accounts()?;
    let Some(account) = auth_json::current_stored_account(&store)? else {
        return Ok(None);
    };
    if account.id != account_id {
        return Ok(None);
    }
    if !matches!(account.auth_data, AuthData::ChatGPT { .. }) {
        return Ok(None);
    }
    Ok(Some(account))
}

fn runtime_chatgpt_account_matches(
    account: &StoredAccount,
    loaded_auth: &RuntimeLoadedAuth,
) -> bool {
    chatgpt_account_id(account).as_deref() == Some(loaded_auth.chatgpt_account_id.as_str())
}

fn stored_account_secret_values(account_id: &str) -> Vec<String> {
    store::get_account_by_selector(account_id)
        .ok()
        .map(|account| account_secret_values(&account))
        .unwrap_or_default()
}

fn account_secret_values(account: &StoredAccount) -> Vec<String> {
    match &account.auth_data {
        AuthData::ChatGPT {
            id_token,
            access_token,
            refresh_token,
            ..
        } => vec![
            id_token.expose_secret().to_string(),
            access_token.expose_secret().to_string(),
            refresh_token.expose_secret().to_string(),
        ],
        AuthData::ApiKey { key } => vec![key.expose_secret().to_string()],
    }
}

fn runtime_login_command_secret_values(command: &RuntimeLoginCommand) -> Vec<String> {
    vec![command.prepared.payload.access_token.clone()]
}

fn current_account_has_newer_access_token(
    account: &StoredAccount,
    loaded_auth: &RuntimeLoadedAuth,
) -> bool {
    match &account.auth_data {
        AuthData::ChatGPT { access_token, .. } => {
            let Some(current_expires_at) =
                token::access_token_expires_at(access_token.expose_secret())
            else {
                return false;
            };
            match loaded_auth.access_token_expires_at {
                Some(loaded_expires_at) => current_expires_at > loaded_expires_at,
                None => true,
            }
        }
        AuthData::ApiKey { .. } => false,
    }
}

fn current_prewarm_snapshot_for_account(
    account: &StoredAccount,
) -> Result<Option<CurrentAccountSnapshot>> {
    let store = store::load_accounts()?;
    let Some(current_auth_account) = auth_json::current_auth_account()? else {
        return Ok(None);
    };
    let Some(current_account) = store::find_matching_account(&store, &current_auth_account) else {
        return Ok(None);
    };
    Ok(prewarm_snapshot_for_matching_auth(
        account,
        &current_auth_account,
        &current_account.id,
    ))
}

fn prewarm_snapshot_for_matching_auth(
    account: &StoredAccount,
    current_auth_account: &StoredAccount,
    current_account_id: &str,
) -> Option<CurrentAccountSnapshot> {
    if current_account_id != account.id {
        return None;
    }
    if !auth_serialized_credentials_match(current_auth_account, account) {
        return None;
    }
    Some(CurrentAccountSnapshot {
        current_account_id: Some(account.id.clone()),
        auth_marker: Some(current_account_auth_marker(current_auth_account)),
    })
}

async fn wait_for_prewarm_login_completion(
    mut completion: oneshot::Receiver<RuntimeLoginResult>,
    shutdown: &mut watch::Receiver<bool>,
) -> Option<RuntimeLoginResult> {
    let deadline = Instant::now() + APP_SERVER_REQUEST_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Some(RuntimeLoginResult::Transient(
                "Timed out waiting for Codex app-server login response".to_string(),
            ));
        }

        tokio::select! {
            result = &mut completion => {
                return Some(result.unwrap_or_else(|_| {
                    RuntimeLoginResult::Transient("Codex app-server login response was dropped".to_string())
                }));
            }
            changed = shutdown.changed() => {
                if changed.is_ok() && *shutdown.borrow() {
                    return None;
                }
            }
            _ = sleep(remaining.min(ACTIVE_ACCOUNT_WATCH_INTERVAL)) => {}
        }
    }
}

async fn sleep_until_runtime_auth_change_or_shutdown(
    duration: Duration,
    runtime_auth: &mut watch::Receiver<RuntimeAuthState>,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    tokio::select! {
        _ = sleep(duration) => false,
        changed = runtime_auth.changed() => changed.is_err(),
        changed = shutdown.changed() => changed.is_ok() && *shutdown.borrow(),
    }
}

async fn wait_for_runtime_auth_change_or_shutdown(
    runtime_auth: &mut watch::Receiver<RuntimeAuthState>,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    tokio::select! {
        changed = runtime_auth.changed() => changed.is_err(),
        changed = shutdown.changed() => changed.is_ok() && *shutdown.borrow(),
    }
}

fn duration_until_utc(now: DateTime<Utc>, target: DateTime<Utc>) -> Duration {
    target
        .signed_duration_since(now)
        .to_std()
        .unwrap_or(Duration::ZERO)
}

fn random_token_prewarm_window() -> Duration {
    random_duration_between(TOKEN_PREWARM_MIN_WINDOW, TOKEN_PREWARM_MAX_WINDOW)
}

async fn sleep_until_shutdown(duration: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        _ = sleep(duration) => false,
        changed = shutdown.changed() => changed.is_ok() && *shutdown.borrow(),
    }
}

async fn run_current_account_reconciler(
    runtime_commands: mpsc::Sender<RuntimeCommand>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut state =
        ActiveAccountReconcileState::new(current_account_snapshot().ok(), Instant::now());

    loop {
        let delay = state.next_poll_delay(Instant::now());
        if sleep_until_shutdown(delay, &mut shutdown).await {
            return;
        }

        let Ok(current_snapshot) = current_account_snapshot() else {
            continue;
        };

        if let ActiveAccountReconcileAction::Attempt {
            snapshot,
            notify_on_success,
        } = state.next_action(current_snapshot, Instant::now())
        {
            let result = attempt_current_account_reconcile(
                &runtime_commands,
                snapshot.clone(),
                notify_on_success,
                &mut shutdown,
            )
            .await;
            match result {
                ActiveAccountReconcileAttemptResult::Completed(outcome) => {
                    state.finish_attempt(&snapshot, outcome, Instant::now());
                }
                ActiveAccountReconcileAttemptResult::Shutdown => return,
            }
        }
    }
}

#[derive(Debug)]
struct ActiveAccountReconcileState {
    desired_snapshot: Option<CurrentAccountSnapshot>,
    notify_on_success: bool,
    settled: bool,
    attempts_started: usize,
    next_attempt_at: Instant,
}

impl ActiveAccountReconcileState {
    fn new(initial_snapshot: Option<CurrentAccountSnapshot>, now: Instant) -> Self {
        let settled = initial_snapshot.is_none();
        Self {
            desired_snapshot: initial_snapshot,
            notify_on_success: false,
            settled,
            attempts_started: 0,
            next_attempt_at: now,
        }
    }

    fn next_poll_delay(&self, now: Instant) -> Duration {
        if self.settled || now >= self.next_attempt_at {
            ACTIVE_ACCOUNT_WATCH_INTERVAL
        } else {
            (self.next_attempt_at - now).min(ACTIVE_ACCOUNT_WATCH_INTERVAL)
        }
    }

    fn next_action(
        &mut self,
        current_snapshot: CurrentAccountSnapshot,
        now: Instant,
    ) -> ActiveAccountReconcileAction {
        let current_snapshot = Some(current_snapshot);
        if current_snapshot != self.desired_snapshot {
            let changed_after_startup = self.desired_snapshot.is_some();
            self.notify_on_success = self.notify_on_success || changed_after_startup;
            self.desired_snapshot = current_snapshot;
            self.settled = self.desired_snapshot.is_none();
            self.attempts_started = 0;
            self.next_attempt_at = now;
        }

        if self.settled || now < self.next_attempt_at {
            return ActiveAccountReconcileAction::Wait;
        }
        if self.attempts_started >= ACTIVE_ACCOUNT_MAX_ATTEMPTS {
            self.settled = true;
            return ActiveAccountReconcileAction::Wait;
        }

        let Some(snapshot) = self.desired_snapshot.clone() else {
            self.settled = true;
            return ActiveAccountReconcileAction::Wait;
        };

        self.attempts_started += 1;
        ActiveAccountReconcileAction::Attempt {
            snapshot,
            notify_on_success: self.notify_on_success,
        }
    }

    fn finish_attempt(
        &mut self,
        snapshot: &CurrentAccountSnapshot,
        outcome: ActiveAccountReconcileOutcome,
        now: Instant,
    ) {
        if self.desired_snapshot.as_ref() != Some(snapshot) {
            return;
        }

        match outcome {
            ActiveAccountReconcileOutcome::Success | ActiveAccountReconcileOutcome::Terminal(_) => {
                self.settled = true;
                self.notify_on_success = false;
                self.attempts_started = 0;
            }
            ActiveAccountReconcileOutcome::Stale => {}
            ActiveAccountReconcileOutcome::Deferred(_) => {
                self.attempts_started = self.attempts_started.saturating_sub(1);
                self.next_attempt_at = now + ACTIVE_ACCOUNT_WATCH_INTERVAL;
            }
            ActiveAccountReconcileOutcome::Transient(_) => {
                if self.attempts_started >= ACTIVE_ACCOUNT_MAX_ATTEMPTS {
                    self.settled = true;
                    return;
                }
                let delay = ACTIVE_ACCOUNT_RETRY_DELAYS[self.attempts_started - 1];
                self.next_attempt_at = now + delay;
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ActiveAccountReconcileAction {
    Wait,
    Attempt {
        snapshot: CurrentAccountSnapshot,
        notify_on_success: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ActiveAccountReconcileOutcome {
    Success,
    Terminal(String),
    Deferred(String),
    Transient(String),
    Stale,
}

enum ActiveAccountReconcileAttemptResult {
    Completed(ActiveAccountReconcileOutcome),
    Shutdown,
}

async fn attempt_current_account_reconcile(
    runtime_commands: &mpsc::Sender<RuntimeCommand>,
    expected_snapshot: CurrentAccountSnapshot,
    notify_on_success: bool,
    shutdown: &mut watch::Receiver<bool>,
) -> ActiveAccountReconcileAttemptResult {
    match current_snapshot_matches(&expected_snapshot) {
        Ok(true) => {}
        Ok(false) => {
            return ActiveAccountReconcileAttemptResult::Completed(
                ActiveAccountReconcileOutcome::Stale,
            );
        }
        Err(err) => {
            return ActiveAccountReconcileAttemptResult::Completed(
                ActiveAccountReconcileOutcome::Transient(err.to_string()),
            );
        }
    }

    let account = match current_reconcile_account() {
        Ok(Some(account)) => account,
        Ok(None) => {
            return ActiveAccountReconcileAttemptResult::Completed(
                ActiveAccountReconcileOutcome::Terminal("No active account".to_string()),
            );
        }
        Err(err) => {
            return ActiveAccountReconcileAttemptResult::Completed(
                ActiveAccountReconcileOutcome::Transient(err.to_string()),
            );
        }
    };

    if !matches!(account.auth_data, AuthData::ChatGPT { .. }) {
        return ActiveAccountReconcileAttemptResult::Completed(
            ActiveAccountReconcileOutcome::Terminal(
                "API key accounts do not support runtime switching".to_string(),
            ),
        );
    }

    let success_notification =
        notify_on_success.then(|| runtime_auth_json_switch_notification(&account));
    let prepared = match prepare_login(account).await {
        Ok(prepared) => prepared,
        Err(err) => {
            return ActiveAccountReconcileAttemptResult::Completed(
                runtime_login_result_to_reconcile_outcome(classify_runtime_login_error(
                    &err.to_string(),
                )),
            );
        }
    };

    match current_snapshot_matches(&expected_snapshot) {
        Ok(true) => {}
        Ok(false) => {
            return ActiveAccountReconcileAttemptResult::Completed(
                ActiveAccountReconcileOutcome::Stale,
            );
        }
        Err(err) => {
            return ActiveAccountReconcileAttemptResult::Completed(
                ActiveAccountReconcileOutcome::Transient(err.to_string()),
            );
        }
    }

    let (completion_tx, completion_rx) = oneshot::channel();
    let command = RuntimeCommand::LoginPreparedAccount(RuntimeLoginCommand::reconciled(
        prepared,
        expected_snapshot.clone(),
        completion_tx,
        success_notification,
    ));
    match runtime_commands.try_send(command) {
        Ok(()) => {
            wait_for_runtime_login_completion(completion_rx, &expected_snapshot, shutdown).await
        }
        Err(mpsc::error::TrySendError::Full(_)) => ActiveAccountReconcileAttemptResult::Completed(
            ActiveAccountReconcileOutcome::Deferred("Runtime command queue is full".to_string()),
        ),
        Err(mpsc::error::TrySendError::Closed(_)) => ActiveAccountReconcileAttemptResult::Shutdown,
    }
}

fn current_reconcile_account() -> Result<Option<StoredAccount>> {
    let store = store::load_accounts()?;
    auth_json::current_stored_account(&store)
}

async fn wait_for_runtime_login_completion(
    mut completion: oneshot::Receiver<RuntimeLoginResult>,
    expected_snapshot: &CurrentAccountSnapshot,
    shutdown: &mut watch::Receiver<bool>,
) -> ActiveAccountReconcileAttemptResult {
    let deadline = Instant::now() + APP_SERVER_REQUEST_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return ActiveAccountReconcileAttemptResult::Completed(
                ActiveAccountReconcileOutcome::Transient(
                    "Timed out waiting for Codex app-server login response".to_string(),
                ),
            );
        }

        tokio::select! {
            result = &mut completion => {
                let result = result.unwrap_or_else(|_| {
                    RuntimeLoginResult::Transient("Codex app-server login response was dropped".to_string())
                });
                return ActiveAccountReconcileAttemptResult::Completed(
                    runtime_login_result_to_reconcile_outcome(result),
                );
            }
            changed = shutdown.changed() => {
                if changed.is_ok() && *shutdown.borrow() {
                    return ActiveAccountReconcileAttemptResult::Shutdown;
                }
            }
            _ = sleep(remaining.min(ACTIVE_ACCOUNT_WATCH_INTERVAL)) => {
                match current_snapshot_matches(expected_snapshot) {
                    Ok(true) => {}
                    Ok(false) => {
                        return ActiveAccountReconcileAttemptResult::Completed(
                            ActiveAccountReconcileOutcome::Stale,
                        );
                    }
                    Err(err) => {
                        return ActiveAccountReconcileAttemptResult::Completed(
                            ActiveAccountReconcileOutcome::Transient(err.to_string()),
                        );
                    }
                }
            }
        }
    }
}

fn runtime_login_result_to_reconcile_outcome(
    result: RuntimeLoginResult,
) -> ActiveAccountReconcileOutcome {
    match result {
        RuntimeLoginResult::Success => ActiveAccountReconcileOutcome::Success,
        RuntimeLoginResult::Stale => ActiveAccountReconcileOutcome::Stale,
        RuntimeLoginResult::Deferred(reason) => ActiveAccountReconcileOutcome::Deferred(reason),
        RuntimeLoginResult::Transient(reason) => ActiveAccountReconcileOutcome::Transient(reason),
        RuntimeLoginResult::Terminal(reason) => ActiveAccountReconcileOutcome::Terminal(reason),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CurrentAccountSnapshot {
    current_account_id: Option<String>,
    auth_marker: Option<String>,
}

fn current_account_snapshot() -> Result<CurrentAccountSnapshot> {
    let store = store::load_accounts()?;
    let current_auth_account = auth_json::current_auth_account()?;
    let current_account_id = current_auth_account
        .as_ref()
        .and_then(|account| store::find_matching_account(&store, account))
        .map(|account| account.id.clone());
    let auth_marker = current_auth_account
        .as_ref()
        .map(current_account_auth_marker);

    Ok(CurrentAccountSnapshot {
        current_account_id,
        auth_marker,
    })
}

fn current_snapshot_matches(expected: &CurrentAccountSnapshot) -> Result<bool> {
    Ok(current_account_snapshot()? == *expected)
}

fn current_account_auth_marker(account: &StoredAccount) -> String {
    match &account.auth_data {
        AuthData::ChatGPT { account_id, .. } => format!(
            "chatgpt:{}:{}:{}",
            account_id.as_deref().unwrap_or_default(),
            account
                .token_last_refresh_at
                .map(|value| value.timestamp_millis())
                .unwrap_or_default(),
            account.plan_type.as_deref().unwrap_or_default()
        ),
        AuthData::ApiKey { .. } => "api_key".to_string(),
    }
}

fn random_auto_switch_maintenance_interval() -> Duration {
    random_duration_between(
        AUTO_SWITCH_MAINTENANCE_MIN_INTERVAL,
        AUTO_SWITCH_MAINTENANCE_MAX_INTERVAL,
    )
}

fn random_duration_between(min: Duration, max: Duration) -> Duration {
    debug_assert!(min <= max);

    let min_secs = min.as_secs();
    let max_secs = max.as_secs();
    if min_secs == max_secs {
        return min;
    }

    Duration::from_secs(rand::rng().random_range(min_secs..=max_secs))
}

#[allow(clippy::result_large_err)]
async fn accept_proxy_client(stream: TcpStream, token: &str) -> Result<ProxyClientStream> {
    let expected_auth = format!("Bearer {token}");
    accept_hdr_async_with_config(
        stream,
        move |request: &Request, response: Response| {
            let authorized = request
                .headers()
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value == expected_auth);
            if authorized {
                Ok(response)
            } else {
                Err(unauthorized_response())
            }
        },
        Some(runtime_websocket_config()),
    )
    .await
    .context("Failed to accept Codex websocket client")
}

fn runtime_websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(RUNTIME_WEBSOCKET_MAX_MESSAGE_SIZE))
        .max_frame_size(Some(RUNTIME_WEBSOCKET_MAX_FRAME_SIZE))
}

fn unauthorized_response() -> ErrorResponse {
    Response::builder()
        .status(401)
        .body(Some("Unauthorized".to_string()))
        .expect("static unauthorized websocket response is valid")
}

async fn proxy_websockets(
    client_websocket: ProxyClientStream,
    app_server_websocket: WsStream,
    state: &mut ProxyState,
    runtime_commands: &mut mpsc::Receiver<RuntimeCommand>,
) -> Result<()> {
    let (mut client_write, mut client_read) = client_websocket.split();
    let (mut app_server_write, mut app_server_read) = app_server_websocket.split();
    let mut runtime_commands_open = true;
    let mut experimental_api_capability_pending = true;

    loop {
        tokio::select! {
            command = runtime_commands.recv(), if runtime_commands_open => {
                match command {
                    Some(RuntimeCommand::LoginPreparedAccount(prepared)) => {
                        if let Some(notification) = state
                            .login_prepared_chatgpt_account(&mut app_server_write, prepared)
                            .await?
                        {
                            send_runtime_client_notification(&mut client_write, notification).await?;
                        }
                    }
                    None => {
                        runtime_commands_open = false;
                    }
                }
            }
            message = client_read.next() => {
                let Some(message) = message else {
                    return Ok(());
                };
                let message = message.context("Failed to read Codex websocket client message")?;
                state.mark_activity(Instant::now());
                if !handle_client_proxy_message(
                    message,
                    &mut app_server_write,
                    &mut experimental_api_capability_pending,
                ).await? {
                    return Ok(());
                }
            }
            message = app_server_read.next() => {
                let Some(message) = message else {
                    return Ok(());
                };
                let message = message.context("Failed to read Codex app-server websocket message")?;
                state.mark_activity(Instant::now());
                if !handle_app_server_proxy_message(
                    message,
                    &mut client_write,
                    &mut app_server_write,
                    state,
                )
                .await?
                {
                    return Ok(());
                }
            }
        }
    }
}

struct ProxyState {
    request_prefix: String,
    next_request_id: u64,
    pending_internal: HashMap<String, PendingInternalRequest>,
    app_server_account_id: Option<String>,
    app_server_account_label: Option<String>,
    app_server_snapshot: Option<CurrentAccountSnapshot>,
    pending_login_account_id: Option<String>,
    runtime_auth_state: RuntimeAuthState,
    runtime_auth: watch::Sender<RuntimeAuthState>,
    background_requests: mpsc::Sender<BackgroundRuntimeRequest>,
    runtime_auto_switch_coordinator: SharedRuntimeAutoSwitchCoordinator,
}

enum PendingInternalRequest {
    Login {
        account_id: String,
        started_at: Instant,
        expected_snapshot: Option<CurrentAccountSnapshot>,
        generation: Option<u64>,
        loaded_auth: RuntimeLoadedAuth,
        completion: Option<oneshot::Sender<RuntimeLoginResult>>,
        success_notification: Option<RuntimeLoginSuccessNotification>,
    },
}

impl ProxyState {
    fn new(
        background_requests: mpsc::Sender<BackgroundRuntimeRequest>,
        runtime_auto_switch_coordinator: SharedRuntimeAutoSwitchCoordinator,
        runtime_auth: watch::Sender<RuntimeAuthState>,
    ) -> Self {
        let runtime_auth_state = runtime_auth.borrow().clone();
        Self {
            request_prefix: format!("{INTERNAL_REQUEST_ID_PREFIX}{}", Uuid::new_v4()),
            next_request_id: 1,
            pending_internal: HashMap::new(),
            app_server_account_id: None,
            app_server_account_label: None,
            app_server_snapshot: None,
            pending_login_account_id: None,
            runtime_auth_state,
            runtime_auth,
            background_requests,
            runtime_auto_switch_coordinator,
        }
    }

    fn next_internal_request_id(&mut self) -> String {
        let id = format!("{}/{}", self.request_prefix, self.next_request_id);
        self.next_request_id += 1;
        id
    }

    fn mark_activity(&mut self, now: Instant) {
        if now.saturating_duration_since(self.runtime_auth_state.last_activity_at)
            < RUNTIME_ACTIVITY_UPDATE_MIN_INTERVAL
        {
            return;
        }
        self.runtime_auth_state.last_activity_at = now;
        let _ = self.runtime_auth.send(self.runtime_auth_state.clone());
    }

    fn set_runtime_loaded_auth(&mut self, loaded_auth: RuntimeLoadedAuth) {
        self.app_server_account_id = Some(loaded_auth.account_id.clone());
        self.app_server_account_label = Some(loaded_auth.account_label.clone());
        self.app_server_snapshot = loaded_auth.current_snapshot.clone();
        self.runtime_auth_state.loaded = Some(loaded_auth);
        self.runtime_auth_state.last_activity_at = Instant::now();
        let _ = self.runtime_auth.send(self.runtime_auth_state.clone());
    }

    fn clear_connection_pending(&mut self) {
        self.cancel_pending_logins(RuntimeLoginResult::Transient(
            "Codex app-server connection closed before login completed".to_string(),
        ));
    }

    fn queue_hard_auto_switch(&self) -> BackgroundAutoSwitchQueueStatus {
        queue_hard_auto_switch(
            &self.background_requests,
            &self.runtime_auto_switch_coordinator,
            AutoSwitchLoginMode::EnsureCurrentLogged {
                app_server_account_id: self.app_server_account_id.clone(),
                app_server_snapshot: self.app_server_snapshot.clone(),
            },
        )
    }

    async fn login_prepared_chatgpt_account(
        &mut self,
        app_server_write: &mut SplitSink<WsStream, Message>,
        command: RuntimeLoginCommand,
    ) -> Result<Option<RuntimeClientNotification>> {
        let RuntimeLoginCommand {
            prepared,
            expected_snapshot,
            generation,
            completion,
            success_notification,
        } = command;
        self.clear_expired_pending_logins(Instant::now());

        if let Some(generation) = generation
            && !self
                .runtime_auto_switch_coordinator
                .is_current_generation(generation)
        {
            complete_runtime_login(completion, RuntimeLoginResult::Stale);
            return Ok(None);
        }

        let snapshot_matches = match &expected_snapshot {
            Some(snapshot) => match current_snapshot_matches(snapshot) {
                Ok(matches) => matches,
                Err(err) => {
                    complete_runtime_login(
                        completion,
                        RuntimeLoginResult::Transient(err.to_string()),
                    );
                    return Ok(None);
                }
            },
            None => current_account_id_matches(&prepared.account_id)?,
        };
        if !snapshot_matches {
            complete_runtime_login(completion, RuntimeLoginResult::Stale);
            return Ok(None);
        }

        match self.pending_login_status_and_merge_notification(
            &prepared.account_id,
            expected_snapshot.as_ref(),
            success_notification.clone(),
        ) {
            PendingLoginStatus::None => {}
            PendingLoginStatus::Matching => {
                complete_runtime_login(
                    completion,
                    RuntimeLoginResult::Deferred("Login request is already pending".to_string()),
                );
                return Ok(None);
            }
            PendingLoginStatus::Other => {
                complete_runtime_login(
                    completion,
                    RuntimeLoginResult::Deferred(
                        "Another login request is already pending".to_string(),
                    ),
                );
                return Ok(None);
            }
        }

        if self.runtime_login_already_matches(&prepared.account_id, expected_snapshot.as_ref()) {
            return Ok(self.complete_already_matched_runtime_login(
                &prepared,
                completion,
                success_notification,
            ));
        }

        let loaded_auth = runtime_loaded_auth_from_prepared(&prepared, expected_snapshot.clone());
        let request_id = self.next_internal_request_id();
        let request =
            login_request_from_payload(Value::String(request_id.clone()), &prepared.payload);
        if let Err(err) = send_json(app_server_write, request).await {
            complete_runtime_login(
                completion,
                RuntimeLoginResult::Transient(format!(
                    "Failed to send Codex app-server login request: {err}"
                )),
            );
            return Err(err);
        }
        self.pending_internal.insert(
            request_id,
            PendingInternalRequest::Login {
                account_id: prepared.account_id.clone(),
                started_at: Instant::now(),
                expected_snapshot,
                generation,
                loaded_auth,
                completion,
                success_notification,
            },
        );
        self.pending_login_account_id = Some(prepared.account_id);
        Ok(None)
    }

    fn queue_background_auto_switch(&self) -> BackgroundAutoSwitchQueueStatus {
        queue_background_auto_switch(
            &self.background_requests,
            &self.runtime_auto_switch_coordinator,
            Instant::now(),
        )
    }

    fn runtime_login_already_matches(
        &self,
        account_id: &str,
        expected_snapshot: Option<&CurrentAccountSnapshot>,
    ) -> bool {
        match expected_snapshot {
            Some(snapshot) => {
                self.app_server_account_id.as_deref() == Some(account_id)
                    && self.app_server_snapshot.as_ref() == Some(snapshot)
            }
            None => self.app_server_account_id.as_deref() == Some(account_id),
        }
    }

    fn mark_runtime_login_already_matched(&mut self, prepared: &PreparedAccountLogin) {
        self.app_server_account_label = Some(prepared.account_label.clone());
        let mut runtime_auth_changed = false;
        if let Some(loaded_auth) = self.runtime_auth_state.loaded.as_mut()
            && loaded_auth.account_id == prepared.account_id
            && loaded_auth.account_label != prepared.account_label
        {
            loaded_auth.account_label = prepared.account_label.clone();
            runtime_auth_changed = true;
        }
        if runtime_auth_changed {
            let _ = self.runtime_auth.send(self.runtime_auth_state.clone());
        }
    }

    fn complete_already_matched_runtime_login(
        &mut self,
        prepared: &PreparedAccountLogin,
        completion: Option<oneshot::Sender<RuntimeLoginResult>>,
        success_notification: Option<RuntimeLoginSuccessNotification>,
    ) -> Option<RuntimeClientNotification> {
        let account_changed = self
            .app_server_account_id
            .as_deref()
            .is_some_and(|current| current != prepared.account_id);
        let previous_label = self.app_server_account_label.clone();
        self.mark_runtime_login_already_matched(prepared);
        complete_runtime_login(completion, RuntimeLoginResult::Success);
        success_notification.and_then(|notification| {
            runtime_login_success_client_notification_if_needed(
                notification,
                previous_label.as_deref(),
                account_changed,
            )
        })
    }

    fn pending_login_status_and_merge_notification(
        &mut self,
        account_id: &str,
        expected_snapshot: Option<&CurrentAccountSnapshot>,
        success_notification: Option<RuntimeLoginSuccessNotification>,
    ) -> PendingLoginStatus {
        let mut has_pending_login = false;
        for pending in self.pending_internal.values_mut() {
            match pending {
                PendingInternalRequest::Login {
                    account_id: pending_account_id,
                    expected_snapshot: pending_snapshot,
                    success_notification: pending_success_notification,
                    ..
                } => {
                    has_pending_login = true;
                    if pending_account_id == account_id
                        && pending_snapshot.as_ref() == expected_snapshot
                    {
                        merge_runtime_login_success_notification(
                            pending_success_notification,
                            success_notification.clone(),
                        );
                        return PendingLoginStatus::Matching;
                    }
                }
            }
        }

        if has_pending_login {
            PendingLoginStatus::Other
        } else {
            PendingLoginStatus::None
        }
    }

    #[cfg(test)]
    fn pending_login_status(
        &mut self,
        account_id: &str,
        expected_snapshot: Option<&CurrentAccountSnapshot>,
    ) -> PendingLoginStatus {
        self.pending_login_status_and_merge_notification(account_id, expected_snapshot, None)
    }
}

fn merge_runtime_login_success_notification(
    target: &mut Option<RuntimeLoginSuccessNotification>,
    incoming: Option<RuntimeLoginSuccessNotification>,
) {
    let Some(incoming) = incoming else {
        return;
    };

    match (target.as_ref(), &incoming) {
        (Some(RuntimeLoginSuccessNotification::Client(_)), _) => {}
        (_, RuntimeLoginSuccessNotification::Client(_)) | (None, _) => {
            *target = Some(incoming);
        }
        (
            Some(RuntimeLoginSuccessNotification::AuthJsonSwitch { .. }),
            RuntimeLoginSuccessNotification::AuthJsonSwitch { .. },
        ) => {}
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingLoginStatus {
    None,
    Matching,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InternalResponseOutcome {
    handled: bool,
    client_notification: Option<RuntimeClientNotification>,
}

impl InternalResponseOutcome {
    fn forward() -> Self {
        Self {
            handled: false,
            client_notification: None,
        }
    }

    fn handled(client_notification: Option<RuntimeClientNotification>) -> Self {
        Self {
            handled: true,
            client_notification,
        }
    }

    #[cfg(test)]
    fn is_handled(&self) -> bool {
        self.handled
    }
}

impl ProxyState {
    fn handle_internal_response(&mut self, value: &Value) -> InternalResponseOutcome {
        self.clear_expired_pending_logins(Instant::now());
        let Some(request_id) = value.get("id").and_then(Value::as_str) else {
            return InternalResponseOutcome::forward();
        };
        let Some(pending) = self.pending_internal.remove(request_id) else {
            return if request_id.starts_with(&self.request_prefix) {
                InternalResponseOutcome::handled(None)
            } else {
                InternalResponseOutcome::forward()
            };
        };

        let mut client_notification = None;
        match pending {
            PendingInternalRequest::Login {
                account_id,
                started_at: _,
                expected_snapshot,
                generation,
                loaded_auth,
                completion,
                success_notification,
            } => {
                if self.pending_login_account_id.as_deref() == Some(account_id.as_str()) {
                    self.pending_login_account_id = None;
                }
                let result = if generation.is_some_and(|generation| {
                    !self
                        .runtime_auto_switch_coordinator
                        .is_current_generation(generation)
                }) {
                    RuntimeLoginResult::Stale
                } else {
                    match &expected_snapshot {
                        Some(snapshot) => match current_snapshot_matches(snapshot) {
                            Ok(true) => match response_to_result(value) {
                                Ok(()) => RuntimeLoginResult::Success,
                                Err(err) => classify_runtime_login_error(&err.to_string()),
                            },
                            Ok(false) => RuntimeLoginResult::Stale,
                            Err(err) => RuntimeLoginResult::Transient(err.to_string()),
                        },
                        None => match response_to_result(value) {
                            Ok(()) => RuntimeLoginResult::Success,
                            Err(err) => classify_runtime_login_error(&err.to_string()),
                        },
                    }
                };
                if result == RuntimeLoginResult::Success {
                    let account_changed = self
                        .app_server_account_id
                        .as_deref()
                        .is_some_and(|current| current != account_id);
                    let previous_label = self.app_server_account_label.clone();
                    self.set_runtime_loaded_auth(loaded_auth);
                    client_notification = success_notification.and_then(|notification| {
                        runtime_login_success_client_notification_if_needed(
                            notification,
                            previous_label.as_deref(),
                            account_changed,
                        )
                    });
                }
                complete_runtime_login(completion, result);
            }
        }
        InternalResponseOutcome::handled(client_notification)
    }

    fn clear_expired_pending_logins(&mut self, now: Instant) {
        let mut expired_requests = Vec::new();
        for (request_id, pending) in &self.pending_internal {
            match pending {
                PendingInternalRequest::Login { started_at, .. }
                    if now.saturating_duration_since(*started_at) >= APP_SERVER_REQUEST_TIMEOUT =>
                {
                    expired_requests.push(request_id.clone());
                }
                PendingInternalRequest::Login { .. } => {}
            }
        }

        for request_id in expired_requests {
            if let Some(pending) = self.pending_internal.remove(&request_id) {
                let PendingInternalRequest::Login { account_id, .. } = &pending;
                if self.pending_login_account_id.as_deref() == Some(account_id.as_str()) {
                    self.pending_login_account_id = None;
                }
                complete_pending_internal_request(
                    pending,
                    RuntimeLoginResult::Transient(
                        "Timed out waiting for Codex app-server login response".to_string(),
                    ),
                );
            }
        }
    }

    fn cancel_pending_logins(&mut self, result: RuntimeLoginResult) {
        for (_, pending) in self.pending_internal.drain() {
            complete_pending_internal_request(pending, result.clone());
        }
        self.pending_login_account_id = None;
    }
}

fn complete_runtime_login(
    completion: Option<oneshot::Sender<RuntimeLoginResult>>,
    result: RuntimeLoginResult,
) {
    if let Some(completion) = completion {
        let _ = completion.send(result);
    }
}

fn complete_pending_internal_request(pending: PendingInternalRequest, result: RuntimeLoginResult) {
    match pending {
        PendingInternalRequest::Login { completion, .. } => {
            complete_runtime_login(completion, result);
        }
    }
}

async fn handle_client_proxy_message(
    mut message: Message,
    app_server_write: &mut SplitSink<WsStream, Message>,
    experimental_api_capability_pending: &mut bool,
) -> Result<bool> {
    enable_experimental_api_capability_for_message(
        &mut message,
        experimental_api_capability_pending,
    );
    let should_continue = !matches!(message, Message::Close(_));
    app_server_write
        .send(message)
        .await
        .context("Failed to forward Codex client message to app-server")?;
    Ok(should_continue)
}

fn enable_experimental_api_capability_for_message(
    message: &mut Message,
    capability_pending: &mut bool,
) {
    if !*capability_pending {
        return;
    }
    let Message::Text(text) = message else {
        return;
    };
    let Ok(mut value) = serde_json::from_str::<Value>(text.as_str()) else {
        return;
    };
    if !enable_experimental_api_capability(&mut value) {
        return;
    }
    *message = Message::Text(value.to_string().into());
    *capability_pending = false;
}

fn enable_experimental_api_capability(value: &mut Value) -> bool {
    if value.get("method").and_then(Value::as_str) != Some("initialize") {
        return false;
    }
    let Some(params) = value.get_mut("params").and_then(Value::as_object_mut) else {
        return false;
    };
    let capabilities = params.entry("capabilities").or_insert_with(|| json!({}));
    if capabilities.is_null() {
        *capabilities = json!({});
    }
    let Some(capabilities) = capabilities.as_object_mut() else {
        return false;
    };
    capabilities.insert("experimentalApi".to_string(), Value::Bool(true));
    true
}

async fn handle_app_server_proxy_message(
    message: Message,
    client_write: &mut SplitSink<ProxyClientStream, Message>,
    app_server_write: &mut SplitSink<WsStream, Message>,
    state: &mut ProxyState,
) -> Result<bool> {
    let mut auto_switch_trigger = RateLimitAutoSwitchTrigger::None;
    let mut should_forward = true;
    let mut client_notification = None;
    state.clear_expired_pending_logins(Instant::now());

    if let Some(value) = message_json(&message)? {
        let internal_response = state.handle_internal_response(&value);
        if internal_response.handled {
            should_forward = false;
            client_notification = internal_response.client_notification;
        } else if is_jsonrpc_request(&value)
            && value.get("method").and_then(Value::as_str)
                == Some("account/chatgptAuthTokens/refresh")
        {
            if let Some(loaded_auth) = handle_server_request(app_server_write, value).await? {
                state.set_runtime_loaded_auth(loaded_auth);
            }
            should_forward = false;
        } else if usage_limit_error_requires_switch(&value) {
            auto_switch_trigger = RateLimitAutoSwitchTrigger::Hard;
        } else {
            auto_switch_trigger = classify_rate_limit_notification(&value);
        }
    }

    let should_continue = !matches!(message, Message::Close(_));
    if should_forward {
        client_write
            .send(message)
            .await
            .context("Failed to forward Codex app-server message to client")?;
    }
    if let Some(notification) = client_notification {
        send_runtime_client_notification(client_write, notification).await?;
    }
    match auto_switch_trigger {
        RateLimitAutoSwitchTrigger::Hard => {
            let _ = state.queue_hard_auto_switch();
        }
        RateLimitAutoSwitchTrigger::Soft => {
            let _ = state.queue_background_auto_switch();
        }
        RateLimitAutoSwitchTrigger::None => {}
    }

    Ok(should_continue)
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum RateLimitAutoSwitchTrigger {
    None,
    Soft,
    Hard,
}

fn classify_rate_limit_notification(value: &Value) -> RateLimitAutoSwitchTrigger {
    if value.get("method").and_then(Value::as_str) != Some("account/rateLimits/updated") {
        return RateLimitAutoSwitchTrigger::None;
    }
    let Some(params) = value.get("params") else {
        return RateLimitAutoSwitchTrigger::None;
    };
    let Ok(params) = serde_json::from_value::<AccountRateLimitsUpdatedParams>(params.clone())
    else {
        return RateLimitAutoSwitchTrigger::None;
    };
    let info = params.rate_limits.into_usage_info();
    if auto_switch::usage_requires_switch(&info) {
        return RateLimitAutoSwitchTrigger::Hard;
    }
    if usage_needs_soft_switch(&info) {
        return RateLimitAutoSwitchTrigger::Soft;
    }
    RateLimitAutoSwitchTrigger::None
}

fn usage_needs_soft_switch(info: &crate::types::UsageInfo) -> bool {
    let config = SelectionConfig::default();
    if let Some(metrics) = account_selector::usage_selection_metrics(info, config) {
        return metrics.bottleneck_headroom <= account_selector::DEFAULT_MIN_SAFE_HEADROOM;
    }

    soft_headroom_units(info, config)
        .into_iter()
        .flatten()
        .any(|headroom| headroom <= account_selector::DEFAULT_MIN_SAFE_HEADROOM)
}

fn soft_headroom_units(
    info: &crate::types::UsageInfo,
    config: SelectionConfig,
) -> [Option<f64>; 2] {
    [
        info.five_hour_window()
            .and_then(|window| window.used_percent)
            .and_then(headroom_from_used_percent),
        info.weekly_window()
            .and_then(|window| window.used_percent)
            .and_then(|used| {
                headroom_from_used_percent(used)
                    .map(|headroom| headroom * config.weekly_to_five_hour_ratio)
            }),
    ]
}

fn headroom_from_used_percent(used_percent: f64) -> Option<f64> {
    if used_percent.is_finite() {
        Some((100.0 - used_percent).clamp(0.0, 100.0))
    } else {
        None
    }
}

fn usage_limit_error_requires_switch(value: &Value) -> bool {
    if value.get("method").and_then(Value::as_str) != Some("error") {
        return false;
    }

    if value
        .pointer("/params/error/codexErrorInfo")
        .and_then(Value::as_str)
        == Some("usageLimitExceeded")
    {
        return true;
    }

    value
        .pointer("/params/error/message")
        .and_then(Value::as_str)
        .is_some_and(|message| message.to_ascii_lowercase().contains("usage limit"))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccountRateLimitsUpdatedParams {
    rate_limits: AppServerRateLimitSnapshot,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppServerRateLimitSnapshot {
    limit_id: Option<String>,
    limit_name: Option<String>,
    primary: Option<AppServerRateLimitWindow>,
    secondary: Option<AppServerRateLimitWindow>,
    credits: Option<AppServerCreditsSnapshot>,
    plan_type: Option<String>,
    rate_limit_reached_type: Option<String>,
}

impl AppServerRateLimitSnapshot {
    fn into_usage_info(self) -> crate::types::UsageInfo {
        crate::types::UsageInfo {
            account_id: String::new(),
            limit_id: self.limit_id,
            limit_name: self.limit_name,
            plan_type: self.plan_type,
            primary_used_percent: self.primary.as_ref().map(|window| window.used_percent),
            primary_window_minutes: self
                .primary
                .as_ref()
                .and_then(|window| window.window_duration_mins),
            primary_resets_at: self.primary.as_ref().and_then(|window| window.resets_at),
            secondary_used_percent: self.secondary.as_ref().map(|window| window.used_percent),
            secondary_window_minutes: self
                .secondary
                .as_ref()
                .and_then(|window| window.window_duration_mins),
            secondary_resets_at: self.secondary.as_ref().and_then(|window| window.resets_at),
            has_credits: self.credits.as_ref().map(|credits| credits.has_credits),
            unlimited_credits: self.credits.as_ref().map(|credits| credits.unlimited),
            credits_balance: self.credits.and_then(|credits| credits.balance),
            rate_limit_reset_credits_available: None,
            rate_limit_reached_type: self.rate_limit_reached_type,
            additional_limits: Vec::new(),
            error: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppServerRateLimitWindow {
    used_percent: f64,
    window_duration_mins: Option<i64>,
    resets_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppServerCreditsSnapshot {
    has_credits: bool,
    unlimited: bool,
    balance: Option<String>,
}

async fn connect_app_server_websocket(websocket_url: &str, token: &str) -> Result<WsStream> {
    let mut request = websocket_url
        .into_client_request()
        .with_context(|| format!("Invalid websocket URL: {websocket_url}"))?;
    request.headers_mut().insert(
        AUTHORIZATION,
        format!("Bearer {token}")
            .parse()
            .context("Invalid websocket auth token")?,
    );

    let (websocket, _) =
        connect_async_with_config(request, Some(runtime_websocket_config()), false)
            .await
            .with_context(|| format!("Failed to connect to Codex app-server at {websocket_url}"))?;
    Ok(websocket)
}

async fn initialize_app_server(websocket: &mut WsStream) -> Result<()> {
    let initialize_request_id = json!(1);
    send_json(
        websocket,
        initialize_app_server_request(initialize_request_id.clone()),
    )
    .await?;

    let deadline = Instant::now() + APP_SERVER_REQUEST_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            anyhow::bail!("Timed out waiting for Codex app-server initialize response");
        }
        let message = timeout(remaining, websocket.next())
            .await
            .context("Timed out waiting for Codex app-server initialize response")?
            .context("Codex app-server closed during initialize")?
            .context("Failed to read Codex app-server initialize response")?;

        let Some(value) = message_json(&message)? else {
            continue;
        };
        if value.get("id") == Some(&initialize_request_id) {
            response_to_result(&value)?;
            send_json(
                websocket,
                json!({
                    "method": "initialized",
                    "params": null
                }),
            )
            .await?;
            return Ok(());
        }
        if is_jsonrpc_request(&value) {
            send_error_response(websocket, value.get("id").cloned(), "unsupported request").await?;
        }
    }
}

fn initialize_app_server_request(request_id: Value) -> Value {
    json!({
        "id": request_id,
        "method": "initialize",
        "params": {
            "clientInfo": {
                "name": codex_http::CODEX_APP_SERVER_DAEMON_CLIENT_NAME,
                "title": "Codex App Server Daemon",
                "version": codex_http::codex_version()
            },
            "capabilities": {
                "experimentalApi": true
            }
        }
    })
}

fn login_request_from_payload(request_id: Value, payload: &ExternalAuthPayload) -> Value {
    json!({
        "id": request_id,
        "method": "account/login/start",
        "params": {
            "type": "chatgptAuthTokens",
            "accessToken": &payload.access_token,
            "chatgptAccountId": &payload.chatgpt_account_id,
            "chatgptPlanType": &payload.chatgpt_plan_type
        }
    })
}

async fn handle_server_request<S>(
    websocket: &mut S,
    request: Value,
) -> Result<Option<RuntimeLoadedAuth>>
where
    S: Sink<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    let id = request.get("id").cloned();
    let method = request.get("method").and_then(Value::as_str);
    match method {
        Some("account/chatgptAuthTokens/refresh") => {
            let previous_account_id = request
                .get("params")
                .and_then(|params| params.get("previousAccountId"))
                .and_then(Value::as_str)
                .map(str::to_string);
            match refresh_external_auth(previous_account_id).await {
                Ok(refreshed) => {
                    let payload = refreshed.payload;
                    let loaded_auth =
                        runtime_loaded_auth_from_account_and_payload(&refreshed.account, &payload);
                    send_json(
                        websocket,
                        json!({
                            "id": id,
                            "result": {
                                "accessToken": payload.access_token,
                                "chatgptAccountId": payload.chatgpt_account_id,
                                "chatgptPlanType": payload.chatgpt_plan_type
                            }
                        }),
                    )
                    .await?;
                    Ok(Some(loaded_auth))
                }
                Err(err) => {
                    send_error_response(websocket, id, &err.to_string()).await?;
                    Ok(None)
                }
            }
        }
        _ => {
            send_error_response(websocket, id, "unsupported request").await?;
            Ok(None)
        }
    }
}

struct RefreshedExternalAuth {
    account: StoredAccount,
    payload: ExternalAuthPayload,
}

async fn refresh_external_auth(
    previous_account_id: Option<String>,
) -> Result<RefreshedExternalAuth> {
    let account = find_refresh_account(previous_account_id.as_deref())?;
    let account = token::ensure_chatgpt_tokens_fresh(&account).await?;
    let payload = external_auth_payload_from_fresh_account(account.clone())?;
    Ok(RefreshedExternalAuth { account, payload })
}

fn find_refresh_account(previous_account_id: Option<&str>) -> Result<StoredAccount> {
    let store = store::load_accounts()?;
    if let Some(previous_account_id) = previous_account_id {
        return store
            .accounts
            .into_iter()
            .find(|account| chatgpt_account_id(account).as_deref() == Some(previous_account_id))
            .with_context(|| {
                format!("No stored account matches ChatGPT account id {previous_account_id}")
            });
    }

    auth_json::current_stored_account(&store)?
        .filter(|account| matches!(account.auth_data, AuthData::ChatGPT { .. }))
        .context("No current ChatGPT account available for auth refresh")
}

fn chatgpt_account_id(account: &StoredAccount) -> Option<String> {
    match &account.auth_data {
        AuthData::ChatGPT { account_id, .. } => account_id.clone(),
        AuthData::ApiKey { .. } => None,
    }
}

fn current_account_id_matches(account_id: &str) -> Result<bool> {
    let store = store::load_accounts()?;
    Ok(auth_json::current_stored_account_best_effort(&store)
        .is_some_and(|account| account.id == account_id))
}

struct PreparedAccountLogin {
    account_id: String,
    account_label: String,
    payload: ExternalAuthPayload,
    current_snapshot: Option<CurrentAccountSnapshot>,
}

async fn prepare_login(account: StoredAccount) -> Result<PreparedAccountLogin> {
    let account = token::ensure_chatgpt_tokens_fresh(&account).await?;
    prepare_login_from_fresh_account(account)
}

fn prepare_login_from_fresh_account(account: StoredAccount) -> Result<PreparedAccountLogin> {
    let account_id = account.id.clone();
    let account_label = display_safe_account_label(&account);
    let current_snapshot = prepared_current_account_snapshot(&account)?;
    let payload = external_auth_payload_from_fresh_account(account)?;
    Ok(PreparedAccountLogin {
        account_id,
        account_label,
        payload,
        current_snapshot,
    })
}

struct ExternalAuthPayload {
    access_token: String,
    chatgpt_account_id: String,
    chatgpt_plan_type: Option<String>,
}

fn external_auth_payload_from_fresh_account(account: StoredAccount) -> Result<ExternalAuthPayload> {
    match account.auth_data {
        AuthData::ChatGPT {
            access_token,
            account_id,
            ..
        } => Ok(ExternalAuthPayload {
            access_token: access_token.into_inner(),
            chatgpt_account_id: account_id
                .filter(|account_id| !account_id.trim().is_empty())
                .context("ChatGPT account id is required for runtime switching")?,
            chatgpt_plan_type: account.plan_type,
        }),
        AuthData::ApiKey { .. } => {
            anyhow::bail!("API key accounts do not support runtime switching")
        }
    }
}

fn runtime_loaded_auth_from_prepared(
    prepared: &PreparedAccountLogin,
    current_snapshot: Option<CurrentAccountSnapshot>,
) -> RuntimeLoadedAuth {
    RuntimeLoadedAuth {
        account_id: prepared.account_id.clone(),
        account_label: prepared.account_label.clone(),
        chatgpt_account_id: prepared.payload.chatgpt_account_id.clone(),
        access_token_expires_at: token::access_token_expires_at(&prepared.payload.access_token),
        current_snapshot,
    }
}

fn runtime_loaded_auth_from_account_and_payload(
    account: &StoredAccount,
    payload: &ExternalAuthPayload,
) -> RuntimeLoadedAuth {
    let current_snapshot = current_account_id_matches(&account.id)
        .ok()
        .filter(|matches| *matches)
        .map(|_| current_account_snapshot_for_account(account));
    RuntimeLoadedAuth {
        account_id: account.id.clone(),
        account_label: display_safe_account_label(account),
        chatgpt_account_id: payload.chatgpt_account_id.clone(),
        access_token_expires_at: token::access_token_expires_at(&payload.access_token),
        current_snapshot,
    }
}

fn prepared_current_account_snapshot(
    account: &StoredAccount,
) -> Result<Option<CurrentAccountSnapshot>> {
    let snapshot = current_account_snapshot()?;
    let prepared_snapshot = current_account_snapshot_for_account(account);
    Ok((snapshot == prepared_snapshot).then_some(prepared_snapshot))
}

async fn send_error_response<S>(websocket: &mut S, id: Option<Value>, message: &str) -> Result<()>
where
    S: Sink<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    send_json(
        websocket,
        json!({
            "id": id,
            "error": {
                "code": -32000,
                "message": message
            }
        }),
    )
    .await
}

async fn send_json<S>(websocket: &mut S, value: Value) -> Result<()>
where
    S: Sink<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    websocket
        .send(Message::Text(value.to_string().into()))
        .await
        .context("Failed to write Codex app-server websocket message")
}

async fn send_runtime_client_notification<S>(
    websocket: &mut S,
    notification: RuntimeClientNotification,
) -> Result<()>
where
    S: Sink<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    let value = runtime_client_notification_value(notification);
    websocket
        .send(Message::Text(value.to_string().into()))
        .await
        .context("Failed to write Codex runtime notification to client")
}

fn runtime_client_notification_value(notification: RuntimeClientNotification) -> Value {
    match notification {
        RuntimeClientNotification::Warning { message } => json!({
            "method": "warning",
            "params": {
                "threadId": Value::Null,
                "message": message,
            },
        }),
    }
}

fn message_json(message: &Message) -> Result<Option<Value>> {
    match message {
        Message::Text(text) => {
            let value = serde_json::from_str(text.as_str())
                .context("Failed to parse Codex app-server websocket message")?;
            Ok(Some(value))
        }
        Message::Close(_)
        | Message::Ping(_)
        | Message::Pong(_)
        | Message::Binary(_)
        | Message::Frame(_) => Ok(None),
    }
}

fn response_to_result(value: &Value) -> Result<()> {
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Codex app-server request failed");
        anyhow::bail!("{message}");
    }
    if value.get("result").is_some() {
        return Ok(());
    }
    anyhow::bail!("Codex app-server response did not include result")
}

fn classify_runtime_login_error(message: &str) -> RuntimeLoginResult {
    let normalized = message.to_ascii_lowercase();
    if [
        "api key accounts do not support runtime switching",
        "chatgpt account id is required",
        "missing refresh token",
        "invalid_grant",
        "invalid refresh",
        "revoked",
        "unauthorized",
        "forbidden",
        "invalid_request",
        "400",
        "401",
        "403",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
    {
        RuntimeLoginResult::Terminal(message.to_string())
    } else {
        RuntimeLoginResult::Transient(message.to_string())
    }
}

fn is_jsonrpc_request(value: &Value) -> bool {
    value.get("method").is_some() && value.get("id").is_some()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::Path;
    use std::time::Duration;

    use base64::Engine;
    use chrono::Utc;
    use serde_json::json;
    use tokio::sync::{mpsc, oneshot, watch};
    use tokio::time::Instant;

    use super::{
        ACTIVE_ACCOUNT_MAX_ATTEMPTS, ACTIVE_ACCOUNT_RETRY_DELAYS, ACTIVE_ACCOUNT_WATCH_INTERVAL,
        APP_SERVER_REQUEST_TIMEOUT, ActiveAccountReconcileAction, ActiveAccountReconcileOutcome,
        ActiveAccountReconcileState, AutoSwitchLoginMode, AutoSwitchResult,
        BackgroundAutoSwitchQueueStatus, BackgroundRuntimeRequest, CurrentAccountSnapshot,
        ExternalAuthPayload, PendingInternalRequest, PendingLoginStatus, PreparedAccountLogin,
        ProxyState, RUNTIME_WEBSOCKET_MAX_FRAME_SIZE, RUNTIME_WEBSOCKET_MAX_MESSAGE_SIZE,
        RateLimitAutoSwitchTrigger, RuntimeAuthState, RuntimeAutoSwitchPriority,
        RuntimeClientNotification, RuntimeCommand, RuntimeCommandSendStatus, RuntimeLoadedAuth,
        RuntimeLoginCommand, RuntimeLoginResult, RuntimeLoginSuccessNotification,
        TOKEN_PREWARM_IDLE_AFTER, TokenPrewarmAttemptResult, TokenPrewarmDecision,
        classify_rate_limit_notification, classify_runtime_login_error,
        codex_args_with_default_cwd, current_account_auth_marker,
        current_account_has_newer_access_token, current_account_snapshot_for_account,
        duration_until_utc, enable_experimental_api_capability,
        enable_experimental_api_capability_for_message, external_auth_payload_from_fresh_account,
        finish_background_auto_switch, format_auto_switch_result_for_log,
        format_codex_args_summary_for_log, has_cwd_arg, initial_auto_switch_nonfatal_reason,
        initialize_app_server_request, prewarm_snapshot_for_matching_auth,
        queue_background_auto_switch, queue_hard_auto_switch, random_duration_between,
        redact_runtime_log_message, runtime_auth_json_switch_notification,
        runtime_auto_switch_success_notification, runtime_chatgpt_account_matches,
        runtime_client_notification_value, runtime_login_success_client_notification,
        runtime_websocket_config, sanitize_startup_log_message, select_current_kept_login_account,
        shared_runtime_auto_switch_coordinator, token_prewarm_attempt_result_for_login_result,
        token_prewarm_decision, token_prewarm_is_suppressed, try_send_background_runtime_command,
        usage_limit_error_requires_switch, validate_remote_capable_codex_args,
    };
    use crate::types::{AuthData, NewChatGptAccount, StoredAccount};
    use tokio_tungstenite::tungstenite::Message;

    #[test]
    fn app_server_probe_uses_codex_daemon_identity_and_experimental_api() {
        let request = initialize_app_server_request(json!(1));

        assert_eq!(request.get("id"), Some(&json!(1)));
        assert_eq!(
            request.get("method").and_then(|value| value.as_str()),
            Some("initialize")
        );
        assert_eq!(
            request
                .pointer("/params/clientInfo/name")
                .and_then(|value| value.as_str()),
            Some(crate::codex_http::CODEX_APP_SERVER_DAEMON_CLIENT_NAME)
        );
        assert_eq!(
            request
                .pointer("/params/clientInfo/title")
                .and_then(|value| value.as_str()),
            Some("Codex App Server Daemon")
        );
        assert!(
            request
                .pointer("/params/clientInfo/version")
                .and_then(|value| value.as_str())
                .is_some_and(|value| !value.is_empty())
        );
        assert_eq!(
            request
                .pointer("/params/capabilities/experimentalApi")
                .and_then(|value| value.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn proxy_initialize_enables_experimental_api_and_preserves_capabilities() {
        let mut request = json!({
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "codex-tui",
                    "version": "1.0.0"
                },
                "capabilities": {
                    "experimentalApi": false,
                    "requestAttestation": true
                }
            }
        });

        assert!(enable_experimental_api_capability(&mut request));
        assert_eq!(
            request.pointer("/params/capabilities/experimentalApi"),
            Some(&json!(true))
        );
        assert_eq!(
            request.pointer("/params/capabilities/requestAttestation"),
            Some(&json!(true))
        );
    }

    #[test]
    fn proxy_initialize_adds_missing_capabilities() {
        let mut request = json!({
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "codex-tui",
                    "version": "1.0.0"
                }
            }
        });

        assert!(enable_experimental_api_capability(&mut request));
        assert_eq!(
            request.pointer("/params/capabilities/experimentalApi"),
            Some(&json!(true))
        );
    }

    #[test]
    fn proxy_non_initialize_message_is_unchanged() {
        let mut request = json!({
            "id": 1,
            "method": "thread/start",
            "params": {}
        });
        let original = request.clone();

        assert!(!enable_experimental_api_capability(&mut request));
        assert_eq!(request, original);
    }

    #[test]
    fn proxy_only_inspects_client_messages_until_initialize_is_enabled() {
        let mut capability_pending = true;
        let mut non_initialize = Message::Text(
            json!({
                "id": 1,
                "method": "thread/start",
                "params": {}
            })
            .to_string()
            .into(),
        );
        let original_non_initialize = non_initialize.clone();

        enable_experimental_api_capability_for_message(
            &mut non_initialize,
            &mut capability_pending,
        );

        assert!(capability_pending);
        assert_eq!(non_initialize, original_non_initialize);

        let mut initialize = Message::Text(
            json!({
                "id": 2,
                "method": "initialize",
                "params": {
                    "clientInfo": {
                        "name": "codex-tui",
                        "version": "1.0.0"
                    }
                }
            })
            .to_string()
            .into(),
        );

        enable_experimental_api_capability_for_message(&mut initialize, &mut capability_pending);

        assert!(!capability_pending);
        let Message::Text(initialize) = initialize else {
            panic!("initialize should remain a text message");
        };
        let initialize: serde_json::Value =
            serde_json::from_str(initialize.as_str()).expect("initialize should remain valid JSON");
        assert_eq!(
            initialize.pointer("/params/capabilities/experimentalApi"),
            Some(&json!(true))
        );

        let mut later_message = Message::Text("not-json".into());
        enable_experimental_api_capability_for_message(&mut later_message, &mut capability_pending);
        assert_eq!(later_message, Message::Text("not-json".into()));
    }

    #[test]
    fn runtime_websocket_config_allows_large_app_server_snapshots() {
        let config = runtime_websocket_config();

        assert_eq!(
            config.max_message_size,
            Some(RUNTIME_WEBSOCKET_MAX_MESSAGE_SIZE)
        );
        assert_eq!(
            config.max_frame_size,
            Some(RUNTIME_WEBSOCKET_MAX_FRAME_SIZE)
        );
        assert!(config.max_frame_size.unwrap_or_default() > 16 * 1024 * 1024);
    }

    #[test]
    fn startup_log_message_sanitizes_control_characters() {
        assert_eq!(
            sanitize_startup_log_message("before\n\u{1b}[31mafter\tend"),
            "before??[31mafter?end"
        );
    }

    #[test]
    fn initial_auto_switch_no_usable_replacement_is_nonfatal() {
        let err = anyhow::Error::new(crate::auto_switch::NoUsableReplacement::new(
            "rate limit reached".to_string(),
        ));

        assert_eq!(
            initial_auto_switch_nonfatal_reason(&err),
            Some("rate limit reached")
        );
    }

    #[test]
    fn initial_auto_switch_other_errors_remain_fatal() {
        let err = anyhow::anyhow!("network failed");

        assert_eq!(initial_auto_switch_nonfatal_reason(&err), None);
    }

    #[test]
    fn runtime_log_redaction_sanitizes_known_secrets() {
        let secret = "secret value/with-symbols".to_string();
        let encoded = urlencoding::encode(&secret);
        let message = format!("before\nraw={secret} encoded={encoded}");

        let redacted = redact_runtime_log_message(&message, &[secret]);

        assert_eq!(redacted, "before?raw=<redacted> encoded=<redacted>");
    }

    #[test]
    fn codex_args_summary_does_not_log_argument_values() {
        assert_eq!(format_codex_args_summary_for_log(&[]), "default (0 args)");
        assert_eq!(
            format_codex_args_summary_for_log(&["resume".to_string(), "session-id".to_string()]),
            "resume (2 args)"
        );
        assert_eq!(
            format_codex_args_summary_for_log(&["--model".to_string(), "gpt-test".to_string()]),
            "default (2 args)"
        );
    }

    #[test]
    fn auto_switch_log_result_sanitizes_account_names_and_reasons() {
        let mut from = chatgpt_account("from-account-id");
        from.name = "from\naccount".to_string();
        let mut to = chatgpt_account("to-account-id");
        to.name = "to\taccount".to_string();
        let result = AutoSwitchResult::Switched {
            from: Some(Box::new(from)),
            to: Box::new(to),
            reason: "weekly\nusage is 100.0%".to_string(),
        };

        assert_eq!(
            format_auto_switch_result_for_log(&result),
            "switched from?account (from-acc) -> to?account (to-accou) - weekly?usage is 100.0%"
        );
    }

    #[test]
    fn rate_limit_notification_classifies_hard_limit() {
        let notification = rate_limit_notification(Some(100.0), None, None);

        assert_eq!(
            classify_rate_limit_notification(&notification),
            RateLimitAutoSwitchTrigger::Hard
        );

        let available_notification = rate_limit_notification(Some(94.0), None, None);
        assert_eq!(
            classify_rate_limit_notification(&available_notification),
            RateLimitAutoSwitchTrigger::None
        );
    }

    #[test]
    fn rate_limit_notification_classifies_limit_type_as_hard() {
        let notification =
            rate_limit_notification(None, None, Some("workspace_owner_usage_limit_reached"));

        assert_eq!(
            classify_rate_limit_notification(&notification),
            RateLimitAutoSwitchTrigger::Hard
        );
    }

    #[test]
    fn rate_limit_notification_classifies_near_five_hour_bottleneck_as_soft() {
        let notification = rate_limit_notification(Some(95.0), Some(20.0), None);

        assert_eq!(
            classify_rate_limit_notification(&notification),
            RateLimitAutoSwitchTrigger::Soft
        );
    }

    #[test]
    fn rate_limit_notification_classifies_near_weekly_bottleneck_as_soft() {
        let notification = rate_limit_notification(Some(50.0), Some(99.0), None);

        assert_eq!(
            classify_rate_limit_notification(&notification),
            RateLimitAutoSwitchTrigger::Soft
        );
    }

    #[test]
    fn rate_limit_notification_soft_triggers_with_only_near_primary_window() {
        let notification = rate_limit_notification(Some(95.0), None, None);

        assert_eq!(
            classify_rate_limit_notification(&notification),
            RateLimitAutoSwitchTrigger::Soft
        );
    }

    #[test]
    fn rate_limit_notification_soft_triggers_with_only_near_secondary_window() {
        let notification = rate_limit_notification(None, Some(99.0), None);

        assert_eq!(
            classify_rate_limit_notification(&notification),
            RateLimitAutoSwitchTrigger::Soft
        );
    }

    #[test]
    fn rate_limit_notification_scales_swapped_windows_by_duration() {
        let notification = rate_limit_notification_with_windows(
            Some((98.0, Some(10_080))),
            Some((94.0, Some(300))),
            None,
        );

        assert_eq!(
            classify_rate_limit_notification(&notification),
            RateLimitAutoSwitchTrigger::None
        );
    }

    #[test]
    fn secondary_five_hour_window_can_trigger_soft_switch() {
        let notification =
            rate_limit_notification_with_windows(None, Some((95.0, Some(300))), None);

        assert_eq!(
            classify_rate_limit_notification(&notification),
            RateLimitAutoSwitchTrigger::Soft
        );
    }

    #[test]
    fn missing_duration_window_only_triggers_at_hard_limit() {
        let soft = rate_limit_notification_with_windows(Some((99.0, None)), None, None);
        let hard = rate_limit_notification_with_windows(Some((100.0, None)), None, None);

        assert_eq!(
            classify_rate_limit_notification(&soft),
            RateLimitAutoSwitchTrigger::None
        );
        assert_eq!(
            classify_rate_limit_notification(&hard),
            RateLimitAutoSwitchTrigger::Hard
        );
    }

    #[test]
    fn rate_limit_notification_does_not_soft_trigger_with_safe_headroom() {
        let notification = rate_limit_notification(Some(94.0), Some(90.0), None);

        assert_eq!(
            classify_rate_limit_notification(&notification),
            RateLimitAutoSwitchTrigger::None
        );
    }

    #[test]
    fn usage_limit_error_triggers_switch() {
        assert!(usage_limit_error_requires_switch(&json!({
            "method": "error",
            "params": {
                "error": {
                    "message": "You've hit your usage limit. Try again later.",
                    "codexErrorInfo": null,
                    "additionalDetails": null
                },
                "willRetry": false,
                "threadId": "thread",
                "turnId": "turn"
            }
        })));
        assert!(usage_limit_error_requires_switch(&json!({
            "method": "error",
            "params": {
                "error": {
                    "message": "rate limited",
                    "codexErrorInfo": "usageLimitExceeded",
                    "additionalDetails": null
                },
                "willRetry": false,
                "threadId": "thread",
                "turnId": "turn"
            }
        })));
        assert!(!usage_limit_error_requires_switch(&json!({
            "method": "error",
            "params": {
                "error": {
                    "message": "context window exceeded",
                    "codexErrorInfo": "contextWindowExceeded",
                    "additionalDetails": null
                },
                "willRetry": false,
                "threadId": "thread",
                "turnId": "turn"
            }
        })));
    }

    #[test]
    fn hard_auto_switch_queue_supersedes_soft_generation() {
        let coordinator = shared_runtime_auto_switch_coordinator();
        let (sender, mut receiver) = mpsc::channel(2);

        assert_eq!(
            queue_background_auto_switch(&sender, &coordinator, Instant::now()),
            BackgroundAutoSwitchQueueStatus::Queued
        );
        let soft = receiver.try_recv().expect("soft request should be queued");

        assert_eq!(
            queue_hard_auto_switch(&sender, &coordinator, AutoSwitchLoginMode::SwitchedOnly),
            BackgroundAutoSwitchQueueStatus::Queued
        );
        let hard = receiver.try_recv().expect("hard request should be queued");

        let BackgroundRuntimeRequest::AutoSwitch {
            priority: soft_priority,
            generation: soft_generation,
            ..
        } = soft;
        let BackgroundRuntimeRequest::AutoSwitch {
            priority: hard_priority,
            generation: hard_generation,
            ..
        } = hard;

        assert_eq!(soft_priority, RuntimeAutoSwitchPriority::Soft);
        assert_eq!(hard_priority, RuntimeAutoSwitchPriority::Hard);
        assert!(hard_generation > soft_generation);
        assert!(!coordinator.is_current_generation(soft_generation));
        assert!(coordinator.is_current_generation(hard_generation));
    }

    #[test]
    fn background_auto_switch_queue_allows_after_cooldown() {
        let (sender, mut receiver) = mpsc::channel(1);
        let coordinator = shared_runtime_auto_switch_coordinator();
        let now = Instant::now();

        assert_eq!(
            queue_background_auto_switch(&sender, &coordinator, now),
            BackgroundAutoSwitchQueueStatus::Queued
        );
        assert!(receiver.try_recv().is_ok());
        finish_background_auto_switch(&coordinator);

        assert_eq!(
            queue_background_auto_switch(&sender, &coordinator, now + Duration::from_secs(14 * 60)),
            BackgroundAutoSwitchQueueStatus::Cooldown
        );
        assert_eq!(
            queue_background_auto_switch(&sender, &coordinator, now + Duration::from_secs(15 * 60)),
            BackgroundAutoSwitchQueueStatus::Queued
        );
    }

    #[test]
    fn background_auto_switch_queue_rejects_while_in_flight() {
        let (sender, mut receiver) = mpsc::channel(1);
        let coordinator = shared_runtime_auto_switch_coordinator();
        let now = Instant::now();

        assert_eq!(
            queue_background_auto_switch(&sender, &coordinator, now),
            BackgroundAutoSwitchQueueStatus::Queued
        );
        assert!(receiver.try_recv().is_ok());

        assert_eq!(
            queue_background_auto_switch(&sender, &coordinator, now + Duration::from_secs(15 * 60)),
            BackgroundAutoSwitchQueueStatus::InFlight
        );
    }

    #[test]
    fn background_auto_switch_queue_reports_full_without_blocking() {
        let (sender, mut receiver) = mpsc::channel(1);
        let coordinator = shared_runtime_auto_switch_coordinator();
        let now = Instant::now();
        sender
            .try_send(test_auto_switch_request(RuntimeAutoSwitchPriority::Soft, 0))
            .expect("test queue has room for the first request");

        assert_eq!(
            queue_background_auto_switch(&sender, &coordinator, now),
            BackgroundAutoSwitchQueueStatus::Full
        );
        assert!(receiver.try_recv().is_ok());
        assert_eq!(
            queue_background_auto_switch(&sender, &coordinator, now),
            BackgroundAutoSwitchQueueStatus::Queued
        );
    }

    #[test]
    fn hard_auto_switch_queue_keeps_generation_when_full() {
        let (sender, mut receiver) = mpsc::channel(1);
        let coordinator = shared_runtime_auto_switch_coordinator();

        assert_eq!(
            queue_hard_auto_switch(&sender, &coordinator, AutoSwitchLoginMode::SwitchedOnly),
            BackgroundAutoSwitchQueueStatus::Queued
        );
        let queued_generation = coordinator.current_generation();
        assert_eq!(
            queue_hard_auto_switch(&sender, &coordinator, AutoSwitchLoginMode::SwitchedOnly),
            BackgroundAutoSwitchQueueStatus::Full
        );
        assert_eq!(coordinator.current_generation(), queued_generation);

        let BackgroundRuntimeRequest::AutoSwitch { generation, .. } = receiver
            .try_recv()
            .expect("queued hard request should remain runnable");
        assert_eq!(generation, queued_generation);
    }

    #[test]
    fn background_runtime_command_send_drops_when_queue_is_full() {
        let (sender, receiver) = mpsc::channel(1);
        let first = runtime_login_command("account-a", None);
        let second = runtime_login_command("account-b", None);

        assert_eq!(
            try_send_background_runtime_command(&sender, first),
            RuntimeCommandSendStatus::Sent
        );
        assert_eq!(
            try_send_background_runtime_command(&sender, second),
            RuntimeCommandSendStatus::Full
        );

        drop(receiver);
        assert_eq!(
            try_send_background_runtime_command(&sender, runtime_login_command("account-c", None)),
            RuntimeCommandSendStatus::Closed
        );
    }

    #[test]
    fn random_duration_between_stays_within_bounds() {
        for _ in 0..100 {
            let duration =
                random_duration_between(Duration::from_secs(10), Duration::from_secs(20));
            assert!(duration >= Duration::from_secs(10));
            assert!(duration <= Duration::from_secs(20));
        }

        assert_eq!(
            random_duration_between(Duration::from_secs(30), Duration::from_secs(30)),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn token_prewarm_waits_without_runtime_auth_or_expiry() {
        let now = Instant::now();
        let state = RuntimeAuthState::new(now);
        assert_eq!(
            token_prewarm_decision(&state, now, Utc::now(), Duration::from_secs(300)),
            TokenPrewarmDecision::WaitForRuntimeAuth
        );

        let state = RuntimeAuthState {
            loaded: Some(runtime_loaded_auth("account-a")),
            last_activity_at: now,
        };
        assert_eq!(
            token_prewarm_decision(&state, now, Utc::now(), Duration::from_secs(300)),
            TokenPrewarmDecision::WaitForRuntimeAuth
        );
    }

    #[test]
    fn token_prewarm_waits_until_expiry_window() {
        let now = Instant::now();
        let utc_now = Utc::now();
        let loaded = RuntimeLoadedAuth {
            access_token_expires_at: Some(utc_now + chrono::Duration::minutes(30)),
            ..runtime_loaded_auth("account-a")
        };
        let state = RuntimeAuthState {
            loaded: Some(loaded),
            last_activity_at: now,
        };

        assert_eq!(
            token_prewarm_decision(&state, now, utc_now, Duration::from_secs(5 * 60)),
            TokenPrewarmDecision::Wait(Duration::from_secs(25 * 60))
        );
    }

    #[test]
    fn token_prewarm_refreshes_near_expiry_when_active() {
        let now = Instant::now();
        let loaded = runtime_loaded_auth_expires_in("account-a", 4 * 60);
        let state = RuntimeAuthState {
            loaded: Some(loaded.clone()),
            last_activity_at: now - Duration::from_secs(60),
        };

        assert_eq!(
            token_prewarm_decision(&state, now, Utc::now(), Duration::from_secs(5 * 60)),
            TokenPrewarmDecision::Refresh(loaded)
        );
    }

    #[test]
    fn token_prewarm_skips_idle_runtime() {
        let now = Instant::now();
        let state = RuntimeAuthState {
            loaded: Some(runtime_loaded_auth_expires_in("account-a", 4 * 60)),
            last_activity_at: now - TOKEN_PREWARM_IDLE_AFTER - Duration::from_secs(1),
        };

        assert_eq!(
            token_prewarm_decision(&state, now, Utc::now(), Duration::from_secs(5 * 60)),
            TokenPrewarmDecision::SkipIdle
        );
    }

    #[test]
    fn token_prewarm_suppression_only_matches_same_loaded_auth() {
        let loaded = runtime_loaded_auth_expires_in("account-a", 4 * 60);
        let same_state = RuntimeAuthState {
            loaded: Some(loaded.clone()),
            last_activity_at: Instant::now(),
        };
        let changed_state = RuntimeAuthState {
            loaded: Some(RuntimeLoadedAuth {
                access_token_expires_at: Some(Utc::now() + chrono::Duration::minutes(30)),
                ..loaded.clone()
            }),
            last_activity_at: Instant::now(),
        };

        assert!(token_prewarm_is_suppressed(
            &same_state,
            &Some(loaded.clone())
        ));
        assert!(!token_prewarm_is_suppressed(&changed_state, &Some(loaded)));
        assert!(!token_prewarm_is_suppressed(&same_state, &None));
    }

    #[test]
    fn token_prewarm_suppression_ignores_display_label_changes() {
        let loaded = runtime_loaded_auth_expires_in("account-a", 4 * 60);
        let renamed_state = RuntimeAuthState {
            loaded: Some(RuntimeLoadedAuth {
                account_label: "renamed account".to_string(),
                ..loaded.clone()
            }),
            last_activity_at: Instant::now(),
        };

        assert!(token_prewarm_is_suppressed(&renamed_state, &Some(loaded)));
    }

    #[test]
    fn token_prewarm_terminal_result_suppresses_retries() {
        assert_eq!(
            token_prewarm_attempt_result_for_login_result(&RuntimeLoginResult::Terminal(
                "invalid_grant".to_string()
            )),
            TokenPrewarmAttemptResult::SuppressUntilRuntimeAuthChanges
        );
        assert_eq!(
            token_prewarm_attempt_result_for_login_result(&RuntimeLoginResult::Stale),
            TokenPrewarmAttemptResult::RetryLater
        );
        assert_eq!(
            token_prewarm_attempt_result_for_login_result(&RuntimeLoginResult::Transient(
                "temporary failure".to_string()
            )),
            TokenPrewarmAttemptResult::RetryLater
        );
        assert_eq!(
            token_prewarm_attempt_result_for_login_result(&RuntimeLoginResult::Success),
            TokenPrewarmAttemptResult::RetryLater
        );
    }

    #[test]
    fn duration_until_utc_clamps_past_time_to_zero() {
        let now = Utc::now();
        assert_eq!(
            duration_until_utc(now, now - chrono::Duration::seconds(10)),
            Duration::ZERO
        );
    }

    #[test]
    fn current_account_newer_access_token_ignores_snapshot_change_without_later_expiry() {
        let mut account = chatgpt_account("account-a");
        let loaded = RuntimeLoadedAuth {
            current_snapshot: Some(current_account_snapshot_for_account(&account)),
            ..runtime_loaded_auth("account-a")
        };
        account.token_last_refresh_at = account
            .token_last_refresh_at
            .map(|value| value + chrono::Duration::seconds(1));

        assert!(!current_account_has_newer_access_token(&account, &loaded));
    }

    #[test]
    fn current_account_newer_access_token_detects_later_access_token_expiry() {
        let exp = Utc::now().timestamp() + 30 * 60;
        let mut account = chatgpt_account("account-a");
        set_account_access_token(&mut account, test_jwt_with_exp(exp));
        let loaded = RuntimeLoadedAuth {
            access_token_expires_at: chrono::DateTime::from_timestamp(exp - 10 * 60, 0),
            current_snapshot: Some(current_account_snapshot_for_account(&account)),
            ..runtime_loaded_auth("account-a")
        };

        assert!(current_account_has_newer_access_token(&account, &loaded));
    }

    #[test]
    fn current_account_newer_access_token_rejects_same_snapshot_and_expiry() {
        let exp = Utc::now().timestamp() + 30 * 60;
        let mut account = chatgpt_account("account-a");
        set_account_access_token(&mut account, test_jwt_with_exp(exp));
        let loaded = RuntimeLoadedAuth {
            access_token_expires_at: chrono::DateTime::from_timestamp(exp, 0),
            current_snapshot: Some(current_account_snapshot_for_account(&account)),
            ..runtime_loaded_auth("account-a")
        };

        assert!(!current_account_has_newer_access_token(&account, &loaded));
    }

    #[test]
    fn prewarm_snapshot_preserves_current_auth_marker_for_matching_credentials() {
        let mut account = chatgpt_account("account-a");
        account.plan_type = Some("new-plan".to_string());
        let mut current_auth_account = account.clone();
        current_auth_account.plan_type = Some("old-plan".to_string());

        assert_eq!(
            prewarm_snapshot_for_matching_auth(&account, &current_auth_account, "account-a"),
            Some(CurrentAccountSnapshot {
                current_account_id: Some("account-a".to_string()),
                auth_marker: Some(current_account_auth_marker(&current_auth_account)),
            })
        );
    }

    #[test]
    fn prewarm_snapshot_rejects_other_current_account() {
        let account = chatgpt_account("account-a");
        let current_auth_account = account.clone();

        assert_eq!(
            prewarm_snapshot_for_matching_auth(&account, &current_auth_account, "account-b"),
            None
        );
    }

    #[test]
    fn prewarm_snapshot_rejects_credential_mismatch_for_same_account() {
        let account = chatgpt_account("account-a");
        let mut current_auth_account = account.clone();
        set_account_access_token(&mut current_auth_account, "older-access-token".to_string());

        assert_eq!(
            prewarm_snapshot_for_matching_auth(&account, &current_auth_account, "account-a"),
            None
        );
    }

    #[test]
    fn runtime_chatgpt_account_match_rejects_replaced_local_account() {
        let account = chatgpt_account("account-a");
        let loaded = RuntimeLoadedAuth {
            chatgpt_account_id: "account-b".to_string(),
            ..runtime_loaded_auth("account-a")
        };

        assert!(!runtime_chatgpt_account_matches(&account, &loaded));
    }

    #[test]
    fn active_account_reconciler_retries_transient_failure_then_settles_success() {
        let now = Instant::now();
        let initial_snapshot = current_snapshot("account-a");
        let changed_snapshot = current_snapshot("account-b");
        let mut state = ActiveAccountReconcileState::new(Some(initial_snapshot), now);

        assert_eq!(
            state.next_action(changed_snapshot.clone(), now),
            ActiveAccountReconcileAction::Attempt {
                snapshot: changed_snapshot.clone(),
                notify_on_success: true,
            }
        );
        state.finish_attempt(
            &changed_snapshot,
            ActiveAccountReconcileOutcome::Transient("temporary failure".to_string()),
            now,
        );
        assert_eq!(
            state.next_action(changed_snapshot.clone(), now),
            ActiveAccountReconcileAction::Wait
        );

        let retry_at = now + ACTIVE_ACCOUNT_RETRY_DELAYS[0];
        assert_eq!(
            state.next_action(changed_snapshot.clone(), retry_at),
            ActiveAccountReconcileAction::Attempt {
                snapshot: changed_snapshot.clone(),
                notify_on_success: true,
            }
        );
        state.finish_attempt(
            &changed_snapshot,
            ActiveAccountReconcileOutcome::Success,
            retry_at,
        );
        assert_eq!(
            state.next_action(changed_snapshot, retry_at + Duration::from_secs(60)),
            ActiveAccountReconcileAction::Wait
        );
    }

    #[test]
    fn active_account_reconciler_attempts_initial_snapshot() {
        let now = Instant::now();
        let initial_snapshot = current_snapshot("account-a");
        let mut state = ActiveAccountReconcileState::new(Some(initial_snapshot.clone()), now);

        assert_eq!(
            state.next_action(initial_snapshot.clone(), now),
            ActiveAccountReconcileAction::Attempt {
                snapshot: initial_snapshot,
                notify_on_success: false,
            }
        );
    }

    #[test]
    fn active_account_reconciler_does_not_notify_after_initial_snapshot_error() {
        let now = Instant::now();
        let initial_snapshot = current_snapshot("account-a");
        let mut state = ActiveAccountReconcileState::new(None, now);

        assert_eq!(
            state.next_action(initial_snapshot.clone(), now),
            ActiveAccountReconcileAction::Attempt {
                snapshot: initial_snapshot,
                notify_on_success: false,
            }
        );
    }

    #[test]
    fn active_account_reconciler_notifies_for_runtime_same_account_snapshot_change() {
        let now = Instant::now();
        let initial_snapshot = current_snapshot("account-a");
        let mut refreshed_snapshot = current_snapshot("account-a");
        refreshed_snapshot.auth_marker = Some("chatgpt:account-a:2:pro".to_string());
        let mut state = ActiveAccountReconcileState::new(Some(initial_snapshot), now);

        assert_eq!(
            state.next_action(refreshed_snapshot.clone(), now),
            ActiveAccountReconcileAction::Attempt {
                snapshot: refreshed_snapshot,
                notify_on_success: true,
            }
        );
    }

    #[test]
    fn active_account_reconciler_preserves_notification_across_stale_token_refresh() {
        let now = Instant::now();
        let initial_snapshot = current_snapshot("account-a");
        let changed_snapshot = current_snapshot("account-b");
        let mut refreshed_snapshot = current_snapshot("account-b");
        refreshed_snapshot.auth_marker = Some("chatgpt:account-b:2:pro".to_string());
        let mut state = ActiveAccountReconcileState::new(Some(initial_snapshot), now);

        assert_eq!(
            state.next_action(changed_snapshot.clone(), now),
            ActiveAccountReconcileAction::Attempt {
                snapshot: changed_snapshot.clone(),
                notify_on_success: true,
            }
        );
        state.finish_attempt(&changed_snapshot, ActiveAccountReconcileOutcome::Stale, now);

        assert_eq!(
            state.next_action(refreshed_snapshot.clone(), now),
            ActiveAccountReconcileAction::Attempt {
                snapshot: refreshed_snapshot.clone(),
                notify_on_success: true,
            }
        );
        state.finish_attempt(
            &refreshed_snapshot,
            ActiveAccountReconcileOutcome::Success,
            now,
        );

        let mut second_refresh = current_snapshot("account-b");
        second_refresh.auth_marker = Some("chatgpt:account-b:3:pro".to_string());
        assert_eq!(
            state.next_action(second_refresh.clone(), now),
            ActiveAccountReconcileAction::Attempt {
                snapshot: second_refresh,
                notify_on_success: true,
            }
        );
    }

    #[test]
    fn active_account_reconciler_notifies_after_terminal_then_same_account_relogin() {
        let now = Instant::now();
        let initial_snapshot = current_snapshot("account-a");
        let first_snapshot = current_snapshot("account-b");
        let mut relogin_snapshot = current_snapshot("account-b");
        relogin_snapshot.auth_marker = Some("chatgpt:account-b:replacement:pro".to_string());
        let mut state = ActiveAccountReconcileState::new(Some(initial_snapshot), now);

        assert_eq!(
            state.next_action(first_snapshot.clone(), now),
            ActiveAccountReconcileAction::Attempt {
                snapshot: first_snapshot.clone(),
                notify_on_success: true,
            }
        );
        state.finish_attempt(
            &first_snapshot,
            ActiveAccountReconcileOutcome::Terminal("Token refresh failed".to_string()),
            now,
        );

        assert_eq!(
            state.next_action(relogin_snapshot.clone(), now),
            ActiveAccountReconcileAction::Attempt {
                snapshot: relogin_snapshot,
                notify_on_success: true,
            }
        );
    }

    #[test]
    fn active_account_reconciler_keeps_polling_during_retry_backoff() {
        let now = Instant::now();
        let initial_snapshot = current_snapshot("account-a");
        let changed_snapshot = current_snapshot("account-b");
        let mut state = ActiveAccountReconcileState::new(Some(initial_snapshot), now);

        assert!(matches!(
            state.next_action(changed_snapshot.clone(), now),
            ActiveAccountReconcileAction::Attempt { .. }
        ));
        state.finish_attempt(
            &changed_snapshot,
            ActiveAccountReconcileOutcome::Transient("temporary failure".to_string()),
            now,
        );

        let first_retry_at = now + ACTIVE_ACCOUNT_RETRY_DELAYS[0];
        assert!(matches!(
            state.next_action(changed_snapshot.clone(), first_retry_at),
            ActiveAccountReconcileAction::Attempt { .. }
        ));
        state.finish_attempt(
            &changed_snapshot,
            ActiveAccountReconcileOutcome::Transient("temporary failure".to_string()),
            first_retry_at,
        );

        assert_eq!(
            state.next_poll_delay(first_retry_at),
            ACTIVE_ACCOUNT_WATCH_INTERVAL
        );
    }

    #[test]
    fn active_account_reconciler_stops_after_bounded_transient_failures() {
        let mut now = Instant::now();
        let initial_snapshot = current_snapshot("account-a");
        let changed_snapshot = current_snapshot("account-b");
        let mut state = ActiveAccountReconcileState::new(Some(initial_snapshot), now);

        for delay in ACTIVE_ACCOUNT_RETRY_DELAYS {
            assert_eq!(
                state.next_action(changed_snapshot.clone(), now),
                ActiveAccountReconcileAction::Attempt {
                    snapshot: changed_snapshot.clone(),
                    notify_on_success: true,
                }
            );
            state.finish_attempt(
                &changed_snapshot,
                ActiveAccountReconcileOutcome::Transient("temporary failure".to_string()),
                now,
            );
            now += delay;
        }

        assert_eq!(
            ACTIVE_ACCOUNT_MAX_ATTEMPTS,
            ACTIVE_ACCOUNT_RETRY_DELAYS.len() + 1
        );
        assert_eq!(
            state.next_action(changed_snapshot.clone(), now),
            ActiveAccountReconcileAction::Attempt {
                snapshot: changed_snapshot.clone(),
                notify_on_success: true,
            }
        );
        state.finish_attempt(
            &changed_snapshot,
            ActiveAccountReconcileOutcome::Transient("temporary failure".to_string()),
            now,
        );

        assert_eq!(
            state.next_action(changed_snapshot, now + Duration::from_secs(3600)),
            ActiveAccountReconcileAction::Wait
        );
    }

    #[test]
    fn active_account_reconciler_does_not_exhaust_attempts_for_deferred_work() {
        let mut now = Instant::now();
        let initial_snapshot = current_snapshot("account-a");
        let changed_snapshot = current_snapshot("account-b");
        let mut state = ActiveAccountReconcileState::new(Some(initial_snapshot), now);

        for _ in 0..(ACTIVE_ACCOUNT_MAX_ATTEMPTS + 2) {
            assert_eq!(
                state.next_action(changed_snapshot.clone(), now),
                ActiveAccountReconcileAction::Attempt {
                    snapshot: changed_snapshot.clone(),
                    notify_on_success: true,
                }
            );
            state.finish_attempt(
                &changed_snapshot,
                ActiveAccountReconcileOutcome::Deferred(
                    "Another login request is already pending".to_string(),
                ),
                now,
            );
            assert_eq!(
                state.next_action(changed_snapshot.clone(), now),
                ActiveAccountReconcileAction::Wait
            );
            now += ACTIVE_ACCOUNT_WATCH_INTERVAL;
        }

        assert_eq!(
            state.next_action(changed_snapshot.clone(), now),
            ActiveAccountReconcileAction::Attempt {
                snapshot: changed_snapshot,
                notify_on_success: true,
            }
        );
    }

    #[test]
    fn active_account_reconciler_treats_terminal_failure_as_settled() {
        let now = Instant::now();
        let initial_snapshot = current_snapshot("account-a");
        let changed_snapshot = current_snapshot("account-b");
        let mut state = ActiveAccountReconcileState::new(Some(initial_snapshot), now);

        assert!(matches!(
            state.next_action(changed_snapshot.clone(), now),
            ActiveAccountReconcileAction::Attempt { .. }
        ));
        state.finish_attempt(
            &changed_snapshot,
            ActiveAccountReconcileOutcome::Terminal(
                "API key accounts do not support runtime switching".to_string(),
            ),
            now,
        );

        assert_eq!(
            state.next_action(changed_snapshot, now + Duration::from_secs(3600)),
            ActiveAccountReconcileAction::Wait
        );
    }

    #[test]
    fn active_account_reconciler_ignores_stale_result_and_handles_new_snapshot() {
        let now = Instant::now();
        let initial_snapshot = current_snapshot("account-a");
        let first_snapshot = current_snapshot("account-b");
        let second_snapshot = current_snapshot("account-c");
        let mut state = ActiveAccountReconcileState::new(Some(initial_snapshot), now);

        assert!(matches!(
            state.next_action(first_snapshot.clone(), now),
            ActiveAccountReconcileAction::Attempt { .. }
        ));
        state.finish_attempt(&first_snapshot, ActiveAccountReconcileOutcome::Stale, now);

        assert_eq!(
            state.next_action(second_snapshot.clone(), now),
            ActiveAccountReconcileAction::Attempt {
                snapshot: second_snapshot,
                notify_on_success: true,
            }
        );
    }

    #[test]
    fn runtime_login_error_classification_separates_terminal_and_transient() {
        assert!(matches!(
            classify_runtime_login_error("invalid_grant"),
            RuntimeLoginResult::Terminal(_)
        ));
        assert!(matches!(
            classify_runtime_login_error("token endpoint timed out"),
            RuntimeLoginResult::Transient(_)
        ));
    }

    #[test]
    fn runtime_auth_payload_rejects_api_key_accounts() {
        let result = external_auth_payload_from_fresh_account(StoredAccount::new_api_key(
            "api-key".to_string(),
            "sk-test".to_string(),
        ));
        let Err(err) = result else {
            panic!("API key accounts cannot be loaded as ChatGPT runtime auth");
        };

        assert!(
            err.to_string()
                .contains("API key accounts do not support runtime switching")
        );
    }

    #[test]
    fn proxy_reconciled_login_matches_snapshot_not_only_account_id() {
        let (background_tx, _background_rx) = mpsc::channel(1);
        let mut state = proxy_state(background_tx);
        let older_snapshot = current_snapshot("account-a");
        let mut refreshed_snapshot = current_snapshot("account-a");
        refreshed_snapshot.auth_marker = Some("chatgpt:account-a:2:pro".to_string());
        state.app_server_account_id = Some("account-a".to_string());
        state.app_server_snapshot = Some(older_snapshot);

        assert!(!state.runtime_login_already_matches("account-a", Some(&refreshed_snapshot)));
        assert!(state.runtime_login_already_matches("account-a", None));
    }

    #[test]
    fn proxy_reconciled_login_requires_matching_account_and_snapshot() {
        let (background_tx, _background_rx) = mpsc::channel(1);
        let mut state = proxy_state(background_tx);
        let snapshot = current_snapshot("account-a");
        state.app_server_account_id = Some("account-b".to_string());
        state.app_server_snapshot = Some(snapshot.clone());

        assert!(!state.runtime_login_already_matches("account-a", Some(&snapshot)));
    }

    #[test]
    fn proxy_pending_login_status_matches_snapshot_not_only_account_id() {
        let (background_tx, _background_rx) = mpsc::channel(1);
        let mut state = proxy_state(background_tx);
        let older_snapshot = current_snapshot("account-a");
        let mut refreshed_snapshot = current_snapshot("account-a");
        refreshed_snapshot.auth_marker = Some("chatgpt:account-a:2:pro".to_string());
        state.pending_internal.insert(
            "request-id".to_string(),
            pending_login("account-a", Some(older_snapshot.clone()), None, None),
        );

        assert_eq!(
            state.pending_login_status("account-a", Some(&older_snapshot)),
            PendingLoginStatus::Matching
        );
        assert_eq!(
            state.pending_login_status("account-a", Some(&refreshed_snapshot)),
            PendingLoginStatus::Other
        );
        assert_eq!(
            state.pending_login_status("account-a", None),
            PendingLoginStatus::Other
        );
        assert_eq!(
            state.pending_login_status("account-b", Some(&older_snapshot)),
            PendingLoginStatus::Other
        );
    }

    #[test]
    fn ensure_current_logged_requires_snapshot_match_for_current_account() {
        let account = chatgpt_account("account-a");
        let matching_snapshot = current_account_snapshot_for_account(&account);
        let mut stale_snapshot = matching_snapshot.clone();
        stale_snapshot.auth_marker = Some("chatgpt:account-a:0:free".to_string());

        let stale_selection = select_current_kept_login_account(
            account.clone(),
            matching_snapshot.clone(),
            Some(&account.id),
            Some(&stale_snapshot),
        );
        assert_eq!(
            stale_selection.as_ref().map(|account| account.id.as_str()),
            Some("account-a")
        );

        let matching_selection = select_current_kept_login_account(
            account.clone(),
            matching_snapshot.clone(),
            Some(&account.id),
            Some(&matching_snapshot),
        );
        assert!(matching_selection.is_none());
    }

    #[test]
    fn fire_and_forget_login_tracks_prepared_current_snapshot() {
        let snapshot = current_snapshot("account-a");
        let command = RuntimeLoginCommand::fire_and_forget(prepared_login(
            "account-a",
            Some(snapshot.clone()),
        ));

        assert_eq!(command.expected_snapshot, Some(snapshot));
    }

    #[test]
    fn reconciled_login_command_preserves_success_notification() {
        let snapshot = current_snapshot("account-a");
        let (completion_tx, _completion_rx) = oneshot::channel();
        let notification = RuntimeClientNotification::Warning {
            message: "codex-switch switched account to account-a".to_string(),
        };
        let command = RuntimeLoginCommand::reconciled(
            prepared_login("account-a", Some(snapshot.clone())),
            snapshot.clone(),
            completion_tx,
            Some(RuntimeLoginSuccessNotification::Client(
                notification.clone(),
            )),
        );

        assert_eq!(command.expected_snapshot, Some(snapshot));
        assert_eq!(
            command.success_notification,
            Some(RuntimeLoginSuccessNotification::Client(notification))
        );
    }

    #[test]
    fn prewarm_login_command_targets_refreshed_snapshot() {
        let snapshot = current_snapshot("account-a");
        let command = RuntimeLoginCommand::prewarm(
            prepared_login("account-a", Some(snapshot.clone())),
            snapshot.clone(),
        );

        assert_eq!(command.expected_snapshot, Some(snapshot));
        assert_eq!(command.generation, None);
        assert!(command.completion.is_none());
    }

    #[test]
    fn runtime_auto_switch_success_notification_formats_display_safe_warning() {
        let mut from = chatgpt_account("from-account-id");
        from.name = "from\naccount".to_string();
        let mut to = chatgpt_account("to-account-id");
        to.name = "to\taccount".to_string();

        let notification =
            runtime_auto_switch_success_notification(Some(&from), &to, "weekly\nusage is 100.0%");

        assert_eq!(
            notification,
            Some(RuntimeClientNotification::Warning {
                message:
                    "codex-switch switched account from from?account (from-acc) to to?account (to-accou) because weekly?usage is 100.0%"
                        .to_string()
            })
        );
    }

    #[test]
    fn runtime_auto_switch_success_notification_skips_same_account() {
        let account = chatgpt_account("account-a");

        assert_eq!(
            runtime_auto_switch_success_notification(Some(&account), &account, "usage changed"),
            None
        );
    }

    #[test]
    fn runtime_auth_json_switch_notification_formats_warning() {
        let mut account = chatgpt_account("account-a");
        account.name = "work\naccount".to_string();

        assert_eq!(
            runtime_login_success_client_notification(
                runtime_auth_json_switch_notification(&account),
                Some("personal (a6bf89b9)")
            ),
            RuntimeClientNotification::Warning {
                message:
                    "codex-switch switched account from personal (a6bf89b9) to work?account (account-) because current auth.json changed"
                        .to_string()
            }
        );
    }

    #[test]
    fn runtime_client_warning_notification_uses_codex_warning_shape() {
        assert_eq!(
            runtime_client_notification_value(RuntimeClientNotification::Warning {
                message: "codex-switch switched account from a to b".to_string()
            }),
            json!({
                "method": "warning",
                "params": {
                    "threadId": null,
                    "message": "codex-switch switched account from a to b",
                },
            })
        );
    }

    #[tokio::test]
    async fn proxy_internal_login_success_marks_runtime_account() {
        let (background_tx, _background_rx) = mpsc::channel(1);
        let mut state = proxy_state(background_tx);
        let (completion_tx, completion_rx) = oneshot::channel();
        state.pending_login_account_id = Some("account-a".to_string());
        state.pending_internal.insert(
            "request-id".to_string(),
            pending_login("account-a", None, None, Some(completion_tx)),
        );

        assert!(
            state
                .handle_internal_response(&json!({
                    "id": "request-id",
                    "result": null
                }))
                .is_handled()
        );

        assert_eq!(
            completion_rx
                .await
                .expect("login completion should be sent"),
            RuntimeLoginResult::Success
        );
        assert_eq!(state.app_server_account_id.as_deref(), Some("account-a"));
        assert_eq!(state.app_server_snapshot, None);
        assert_eq!(state.pending_login_account_id, None);
    }

    #[tokio::test]
    async fn proxy_internal_login_success_returns_runtime_switch_warning() {
        let (background_tx, _background_rx) = mpsc::channel(1);
        let mut state = proxy_state(background_tx);
        state.app_server_account_id = Some("account-b".to_string());
        state.pending_login_account_id = Some("account-a".to_string());
        let notification = RuntimeClientNotification::Warning {
            message: "codex-switch switched account from a to b".to_string(),
        };
        state.pending_internal.insert(
            "request-id".to_string(),
            pending_login_with_success_notification("account-a", notification.clone()),
        );

        let outcome = state.handle_internal_response(&json!({
            "id": "request-id",
            "result": null
        }));

        assert!(outcome.is_handled());
        assert_eq!(outcome.client_notification, Some(notification));
        assert_eq!(state.app_server_account_id.as_deref(), Some("account-a"));
        assert_eq!(state.pending_login_account_id, None);
    }

    #[tokio::test]
    async fn proxy_internal_login_success_returns_client_warning_without_previous_account() {
        let (background_tx, _background_rx) = mpsc::channel(1);
        let mut state = proxy_state(background_tx);
        state.pending_login_account_id = Some("account-a".to_string());
        let notification = RuntimeClientNotification::Warning {
            message: "codex-switch switched account from a to b".to_string(),
        };
        state.pending_internal.insert(
            "request-id".to_string(),
            pending_login_with_success_notification("account-a", notification.clone()),
        );

        let outcome = state.handle_internal_response(&json!({
            "id": "request-id",
            "result": null
        }));

        assert!(outcome.is_handled());
        assert_eq!(outcome.client_notification, Some(notification));
        assert_eq!(state.app_server_account_id.as_deref(), Some("account-a"));
        assert_eq!(state.pending_login_account_id, None);
    }

    #[tokio::test]
    async fn proxy_internal_login_success_suppresses_auth_json_warning_when_account_did_not_change()
    {
        let (background_tx, _background_rx) = mpsc::channel(1);
        let mut state = proxy_state(background_tx);
        state.app_server_account_id = Some("account-a".to_string());
        state.pending_login_account_id = Some("account-a".to_string());
        state.pending_internal.insert(
            "request-id".to_string(),
            pending_login_with_auth_json_success_notification("account-a"),
        );

        let outcome = state.handle_internal_response(&json!({
            "id": "request-id",
            "result": null
        }));

        assert!(outcome.is_handled());
        assert_eq!(outcome.client_notification, None);
        assert_eq!(state.app_server_account_id.as_deref(), Some("account-a"));
        assert_eq!(state.pending_login_account_id, None);
    }

    #[tokio::test]
    async fn proxy_internal_login_success_suppresses_auth_json_warning_without_previous_account() {
        let (background_tx, _background_rx) = mpsc::channel(1);
        let mut state = proxy_state(background_tx);
        state.pending_login_account_id = Some("account-a".to_string());
        state.pending_internal.insert(
            "request-id".to_string(),
            pending_login_with_auth_json_success_notification("account-a"),
        );

        let outcome = state.handle_internal_response(&json!({
            "id": "request-id",
            "result": null
        }));

        assert!(outcome.is_handled());
        assert_eq!(outcome.client_notification, None);
        assert_eq!(state.app_server_account_id.as_deref(), Some("account-a"));
        assert_eq!(state.pending_login_account_id, None);
    }

    #[tokio::test]
    async fn proxy_matching_pending_login_upgrades_auth_json_warning_to_client_warning() {
        let (background_tx, _background_rx) = mpsc::channel(1);
        let mut state = proxy_state(background_tx);
        state.app_server_account_id = Some("account-b".to_string());
        state.pending_login_account_id = Some("account-a".to_string());
        state.pending_internal.insert(
            "request-id".to_string(),
            pending_login_with_auth_json_success_notification("account-a"),
        );
        let notification = RuntimeClientNotification::Warning {
            message: "codex-switch switched account from account-b to account-a because weekly usage is 100.0%".to_string(),
        };

        assert_eq!(
            state.pending_login_status_and_merge_notification(
                "account-a",
                None,
                Some(RuntimeLoginSuccessNotification::Client(
                    notification.clone()
                ))
            ),
            PendingLoginStatus::Matching
        );
        let outcome = state.handle_internal_response(&json!({
            "id": "request-id",
            "result": null
        }));

        assert!(outcome.is_handled());
        assert_eq!(outcome.client_notification, Some(notification));
        assert_eq!(state.app_server_account_id.as_deref(), Some("account-a"));
        assert_eq!(state.pending_login_account_id, None);
    }

    #[test]
    fn proxy_set_runtime_loaded_auth_updates_account_label() {
        let (background_tx, _background_rx) = mpsc::channel(1);
        let mut state = proxy_state(background_tx);
        state.app_server_account_id = Some("account-a".to_string());
        state.app_server_account_label = Some("account-a label".to_string());

        state.set_runtime_loaded_auth(runtime_loaded_auth("account-a"));
        assert_eq!(
            state.app_server_account_label.as_deref(),
            Some("account-a (account-a)")
        );

        state.set_runtime_loaded_auth(runtime_loaded_auth("account-b"));
        assert_eq!(
            state.app_server_account_label.as_deref(),
            Some("account-b (account-b)")
        );
    }

    #[test]
    fn proxy_mark_runtime_login_already_matched_updates_account_label() {
        let (background_tx, _background_rx) = mpsc::channel(1);
        let mut state = proxy_state(background_tx);
        let prepared = prepared_login("account-a", None);
        state.runtime_auth_state.loaded = Some(RuntimeLoadedAuth {
            account_label: "old label".to_string(),
            ..runtime_loaded_auth("account-a")
        });

        state.mark_runtime_login_already_matched(&prepared);

        assert_eq!(
            state.app_server_account_label.as_deref(),
            Some("account-a (account-a)")
        );
        assert_eq!(
            state
                .runtime_auth_state
                .loaded
                .as_ref()
                .map(|loaded| loaded.account_label.as_str()),
            Some("account-a (account-a)")
        );
    }

    #[test]
    fn proxy_mark_runtime_login_already_matched_only_notifies_runtime_auth_on_label_change() {
        let (background_tx, _background_rx) = mpsc::channel(1);
        let (runtime_auth_tx, runtime_auth_rx) =
            watch::channel(RuntimeAuthState::new(Instant::now()));
        let mut state = ProxyState::new(
            background_tx,
            shared_runtime_auto_switch_coordinator(),
            runtime_auth_tx,
        );
        let prepared = prepared_login("account-a", None);
        state.runtime_auth_state.loaded = Some(runtime_loaded_auth("account-a"));

        state.mark_runtime_login_already_matched(&prepared);
        assert!(matches!(runtime_auth_rx.has_changed(), Ok(false)));

        state.runtime_auth_state.loaded = Some(RuntimeLoadedAuth {
            account_label: "old label".to_string(),
            ..runtime_loaded_auth("account-a")
        });
        state.mark_runtime_login_already_matched(&prepared);
        assert!(matches!(runtime_auth_rx.has_changed(), Ok(true)));
    }

    #[test]
    fn proxy_already_matched_runtime_login_returns_client_warning() {
        let (background_tx, _background_rx) = mpsc::channel(1);
        let mut state = proxy_state(background_tx);
        state.app_server_account_id = Some("account-a".to_string());
        state.app_server_account_label = Some("account-a (account-a)".to_string());
        let prepared = prepared_login("account-a", None);
        let notification = RuntimeClientNotification::Warning {
            message: "codex-switch switched account from account-b to account-a because weekly usage is 100.0%".to_string(),
        };

        assert_eq!(
            state.complete_already_matched_runtime_login(
                &prepared,
                None,
                Some(RuntimeLoginSuccessNotification::Client(
                    notification.clone()
                ))
            ),
            Some(notification)
        );
    }

    #[test]
    fn proxy_already_matched_runtime_login_suppresses_auth_json_warning() {
        let (background_tx, _background_rx) = mpsc::channel(1);
        let mut state = proxy_state(background_tx);
        state.app_server_account_id = Some("account-a".to_string());
        state.app_server_account_label = Some("account-a (account-a)".to_string());
        let prepared = prepared_login("account-a", None);

        assert_eq!(
            state.complete_already_matched_runtime_login(
                &prepared,
                None,
                Some(RuntimeLoginSuccessNotification::AuthJsonSwitch {
                    to_label: "account-a (account-a)".to_string()
                })
            ),
            None
        );
    }

    #[tokio::test]
    async fn proxy_internal_login_success_with_stale_generation_is_ignored() {
        let (background_tx, mut background_rx) = mpsc::channel(2);
        let coordinator = shared_runtime_auto_switch_coordinator();
        assert_eq!(
            queue_hard_auto_switch(
                &background_tx,
                &coordinator,
                AutoSwitchLoginMode::SwitchedOnly,
            ),
            BackgroundAutoSwitchQueueStatus::Queued
        );
        let stale_generation = coordinator.current_generation();
        assert!(background_rx.try_recv().is_ok());
        assert_eq!(
            queue_hard_auto_switch(
                &background_tx,
                &coordinator,
                AutoSwitchLoginMode::SwitchedOnly,
            ),
            BackgroundAutoSwitchQueueStatus::Queued
        );

        let mut state = proxy_state_with_coordinator(background_tx, coordinator);
        let (completion_tx, completion_rx) = oneshot::channel();
        state.pending_login_account_id = Some("account-a".to_string());
        state.pending_internal.insert(
            "request-id".to_string(),
            pending_login(
                "account-a",
                None,
                Some(stale_generation),
                Some(completion_tx),
            ),
        );

        let outcome = state.handle_internal_response(&json!({
            "id": "request-id",
            "result": null
        }));
        assert!(outcome.is_handled());
        assert_eq!(outcome.client_notification, None);

        assert_eq!(
            completion_rx
                .await
                .expect("login completion should be sent"),
            RuntimeLoginResult::Stale
        );
        assert_eq!(state.app_server_account_id, None);
        assert_eq!(state.app_server_snapshot, None);
        assert_eq!(state.pending_login_account_id, None);
    }

    #[tokio::test]
    async fn proxy_internal_login_rejection_does_not_mark_runtime_account() {
        let (background_tx, _background_rx) = mpsc::channel(1);
        let mut state = proxy_state(background_tx);
        let (completion_tx, completion_rx) = oneshot::channel();
        state.pending_login_account_id = Some("account-a".to_string());
        state.pending_internal.insert(
            "request-id".to_string(),
            pending_login("account-a", None, None, Some(completion_tx)),
        );

        let outcome = state.handle_internal_response(&json!({
            "id": "request-id",
            "error": {
                "message": "invalid_grant"
            }
        }));
        assert!(outcome.is_handled());
        assert_eq!(outcome.client_notification, None);

        assert_eq!(
            completion_rx
                .await
                .expect("login completion should be sent"),
            RuntimeLoginResult::Terminal("invalid_grant".to_string())
        );
        assert_eq!(state.app_server_account_id, None);
        assert_eq!(state.pending_login_account_id, None);
    }

    #[tokio::test]
    async fn proxy_expired_internal_login_clears_pending_and_swallows_late_response() {
        let (background_tx, _background_rx) = mpsc::channel(1);
        let mut state = proxy_state(background_tx);
        let request_id = state.next_internal_request_id();
        let (completion_tx, completion_rx) = oneshot::channel();
        state.pending_login_account_id = Some("account-a".to_string());
        state.pending_internal.insert(
            request_id.clone(),
            pending_login_started(
                "account-a",
                Instant::now() - APP_SERVER_REQUEST_TIMEOUT - Duration::from_secs(1),
                None,
                None,
                Some(completion_tx),
            ),
        );

        state.clear_expired_pending_logins(Instant::now());

        assert!(matches!(
            completion_rx
                .await
                .expect("login completion should be sent"),
            RuntimeLoginResult::Transient(_)
        ));
        assert_eq!(state.pending_login_account_id, None);
        assert!(state.pending_internal.is_empty());
        assert!(
            state
                .handle_internal_response(&json!({
                    "id": request_id,
                    "result": null
                }))
                .is_handled()
        );
        assert_eq!(state.app_server_account_id, None);
    }

    #[tokio::test]
    async fn proxy_cancels_connection_pending_login_and_swallows_late_response() {
        let (background_tx, _background_rx) = mpsc::channel(1);
        let mut state = proxy_state(background_tx);
        let request_id = state.next_internal_request_id();
        let (completion_tx, completion_rx) = oneshot::channel();
        state.pending_login_account_id = Some("account-a".to_string());
        state.pending_internal.insert(
            request_id.clone(),
            pending_login("account-a", None, None, Some(completion_tx)),
        );

        state.cancel_pending_logins(RuntimeLoginResult::Stale);

        assert_eq!(
            completion_rx
                .await
                .expect("login completion should be sent"),
            RuntimeLoginResult::Stale
        );
        assert_eq!(state.pending_login_account_id, None);
        assert!(state.pending_internal.is_empty());
        assert!(
            state
                .handle_internal_response(&json!({
                    "id": request_id,
                    "result": null
                }))
                .is_handled()
        );
        assert_eq!(state.app_server_account_id, None);
    }

    #[test]
    fn run_rejects_non_remote_codex_subcommands() {
        assert!(validate_remote_capable_codex_args(&[]).is_ok());
        assert!(validate_remote_capable_codex_args(&["resume".to_string()]).is_ok());
        assert!(validate_remote_capable_codex_args(&["fork".to_string()]).is_ok());
        assert!(validate_remote_capable_codex_args(&["--model".to_string()]).is_ok());
        assert!(validate_remote_capable_codex_args(&["exec".to_string()]).is_err());
    }

    #[test]
    fn run_injects_cwd_for_default_tui() {
        assert_eq!(
            codex_args_with_default_cwd(&[], Path::new("/repo")),
            vec![OsString::from("-C"), OsString::from("/repo")]
        );
    }

    #[test]
    fn run_injects_cwd_after_interactive_subcommand() {
        assert_eq!(
            codex_args_with_default_cwd(&["resume".to_string()], Path::new("/repo")),
            vec![
                OsString::from("resume"),
                OsString::from("-C"),
                OsString::from("/repo")
            ]
        );
        assert_eq!(
            codex_args_with_default_cwd(
                &["fork".to_string(), "session-id".to_string()],
                Path::new("/repo"),
            ),
            vec![
                OsString::from("fork"),
                OsString::from("-C"),
                OsString::from("/repo"),
                OsString::from("session-id")
            ]
        );
    }

    #[test]
    fn run_does_not_override_explicit_cwd() {
        assert!(has_cwd_arg(&["-C".to_string(), "/other".to_string()]));
        assert!(has_cwd_arg(&["--cd".to_string(), "/other".to_string()]));
        assert!(has_cwd_arg(&["--cd=/other".to_string()]));

        assert_eq!(
            codex_args_with_default_cwd(
                &["resume".to_string(), "--cd=/other".to_string()],
                Path::new("/repo"),
            ),
            vec![OsString::from("resume"), OsString::from("--cd=/other")]
        );
    }

    fn current_snapshot(account_id: &str) -> CurrentAccountSnapshot {
        CurrentAccountSnapshot {
            current_account_id: Some(account_id.to_string()),
            auth_marker: Some(format!("chatgpt:{account_id}:1:pro")),
        }
    }

    fn prepared_login(
        account_id: &str,
        current_snapshot: Option<CurrentAccountSnapshot>,
    ) -> PreparedAccountLogin {
        PreparedAccountLogin {
            account_id: account_id.to_string(),
            account_label: format!("{account_id} ({account_id})"),
            payload: ExternalAuthPayload {
                access_token: "access-token".to_string(),
                chatgpt_account_id: account_id.to_string(),
                chatgpt_plan_type: Some("pro".to_string()),
            },
            current_snapshot,
        }
    }

    fn runtime_login_command(
        account_id: &str,
        current_snapshot: Option<CurrentAccountSnapshot>,
    ) -> RuntimeCommand {
        RuntimeCommand::LoginPreparedAccount(RuntimeLoginCommand::fire_and_forget(prepared_login(
            account_id,
            current_snapshot,
        )))
    }

    fn runtime_auth_sender() -> watch::Sender<RuntimeAuthState> {
        let (sender, _receiver) = watch::channel(RuntimeAuthState::new(Instant::now()));
        sender
    }

    fn proxy_state(background_requests: mpsc::Sender<BackgroundRuntimeRequest>) -> ProxyState {
        ProxyState::new(
            background_requests,
            shared_runtime_auto_switch_coordinator(),
            runtime_auth_sender(),
        )
    }

    fn proxy_state_with_coordinator(
        background_requests: mpsc::Sender<BackgroundRuntimeRequest>,
        coordinator: super::SharedRuntimeAutoSwitchCoordinator,
    ) -> ProxyState {
        ProxyState::new(background_requests, coordinator, runtime_auth_sender())
    }

    fn runtime_loaded_auth(account_id: &str) -> RuntimeLoadedAuth {
        RuntimeLoadedAuth {
            account_id: account_id.to_string(),
            account_label: format!("{account_id} ({account_id})"),
            chatgpt_account_id: account_id.to_string(),
            access_token_expires_at: None,
            current_snapshot: None,
        }
    }

    fn runtime_loaded_auth_expires_in(account_id: &str, seconds: i64) -> RuntimeLoadedAuth {
        RuntimeLoadedAuth {
            access_token_expires_at: Some(Utc::now() + chrono::Duration::seconds(seconds)),
            ..runtime_loaded_auth(account_id)
        }
    }

    fn pending_login(
        account_id: &str,
        expected_snapshot: Option<CurrentAccountSnapshot>,
        generation: Option<u64>,
        completion: Option<oneshot::Sender<RuntimeLoginResult>>,
    ) -> PendingInternalRequest {
        pending_login_started(
            account_id,
            Instant::now(),
            expected_snapshot,
            generation,
            completion,
        )
    }

    fn pending_login_with_success_notification(
        account_id: &str,
        success_notification: RuntimeClientNotification,
    ) -> PendingInternalRequest {
        pending_login_with_login_success_notification(
            account_id,
            RuntimeLoginSuccessNotification::Client(success_notification),
        )
    }

    fn pending_login_with_auth_json_success_notification(
        account_id: &str,
    ) -> PendingInternalRequest {
        pending_login_with_login_success_notification(
            account_id,
            RuntimeLoginSuccessNotification::AuthJsonSwitch {
                to_label: format!("{account_id} ({account_id})"),
            },
        )
    }

    fn pending_login_with_login_success_notification(
        account_id: &str,
        success_notification: RuntimeLoginSuccessNotification,
    ) -> PendingInternalRequest {
        let PendingInternalRequest::Login {
            account_id,
            started_at,
            expected_snapshot,
            generation,
            loaded_auth,
            completion,
            success_notification: _,
        } = pending_login(account_id, None, None, None);
        PendingInternalRequest::Login {
            account_id,
            started_at,
            expected_snapshot,
            generation,
            loaded_auth,
            completion,
            success_notification: Some(success_notification),
        }
    }

    fn pending_login_started(
        account_id: &str,
        started_at: Instant,
        expected_snapshot: Option<CurrentAccountSnapshot>,
        generation: Option<u64>,
        completion: Option<oneshot::Sender<RuntimeLoginResult>>,
    ) -> PendingInternalRequest {
        PendingInternalRequest::Login {
            account_id: account_id.to_string(),
            started_at,
            expected_snapshot,
            generation,
            loaded_auth: runtime_loaded_auth(account_id),
            completion,
            success_notification: None,
        }
    }

    fn test_auto_switch_request(
        priority: RuntimeAutoSwitchPriority,
        generation: u64,
    ) -> BackgroundRuntimeRequest {
        BackgroundRuntimeRequest::AutoSwitch {
            priority,
            generation,
            mode: AutoSwitchLoginMode::SwitchedOnly,
        }
    }

    fn chatgpt_account(account_id: &str) -> StoredAccount {
        let mut account = StoredAccount::new_chatgpt(NewChatGptAccount {
            name: account_id.to_string(),
            email: None,
            plan_type: Some("pro".to_string()),
            chatgpt_user_id: None,
            chatgpt_account_is_fedramp: false,
            token_last_refresh_at: Utc::now(),
            subscription_expires_at: None,
            id_token: "id-token".into(),
            access_token: "access-token".into(),
            refresh_token: "refresh-token".into(),
            account_id: Some(account_id.to_string()),
        });
        account.id = account_id.to_string();
        account
    }

    fn set_account_access_token(account: &mut StoredAccount, token: String) {
        let AuthData::ChatGPT { access_token, .. } = &mut account.auth_data else {
            panic!("test account should use ChatGPT auth");
        };
        *access_token = token.into();
    }

    fn test_jwt_with_exp(exp: i64) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#);
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
        format!("{header}.{payload}.")
    }

    fn rate_limit_notification(
        primary_used_percent: Option<f64>,
        secondary_used_percent: Option<f64>,
        rate_limit_reached_type: Option<&str>,
    ) -> serde_json::Value {
        rate_limit_notification_with_windows(
            primary_used_percent.map(|used_percent| (used_percent, Some(300))),
            secondary_used_percent.map(|used_percent| (used_percent, Some(10_080))),
            rate_limit_reached_type,
        )
    }

    fn rate_limit_notification_with_windows(
        primary: Option<(f64, Option<i64>)>,
        secondary: Option<(f64, Option<i64>)>,
        rate_limit_reached_type: Option<&str>,
    ) -> serde_json::Value {
        json!({
            "method": "account/rateLimits/updated",
            "params": {
                "rateLimits": {
                    "limitId": "codex",
                    "limitName": null,
                    "primary": primary.map(|(used_percent, window_duration_mins)| {
                        json!({
                            "usedPercent": used_percent,
                            "windowDurationMins": window_duration_mins,
                            "resetsAt": 1_800_000_000
                        })
                    }),
                    "secondary": secondary.map(|(used_percent, window_duration_mins)| {
                        json!({
                            "usedPercent": used_percent,
                            "windowDurationMins": window_duration_mins,
                            "resetsAt": 1_800_500_000
                        })
                    }),
                    "credits": null,
                    "planType": "plus",
                    "rateLimitReachedType": rate_limit_reached_type
                }
            }
        })
    }
}
