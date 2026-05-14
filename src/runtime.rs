use std::collections::HashMap;
use std::ffi::OsString;
use std::future::Future;
use std::net::TcpListener as StdTcpListener;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{Context, Result};
use futures_util::stream::SplitSink;
use futures_util::{Sink, SinkExt, StreamExt};
use rand::Rng as _;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Duration, Instant, sleep, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, accept_hdr_async, connect_async};
use uuid::Uuid;

use crate::account_selector::{self, SelectionConfig};
use crate::auth_json;
use crate::auto_switch::{self, AutoSwitchResult};
use crate::codex_http;
use crate::store;
use crate::token;
use crate::types::{AuthData, StoredAccount};

const REMOTE_TOKEN_ENV: &str = "CODEX_SWITCH_REMOTE_TOKEN";
const APP_SERVER_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const APP_SERVER_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const AUTO_SWITCH_MAINTENANCE_MIN_INTERVAL: Duration = Duration::from_secs(15 * 60);
const AUTO_SWITCH_MAINTENANCE_MAX_INTERVAL: Duration = Duration::from_secs(45 * 60);
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
const INTERNAL_REQUEST_ID_PREFIX: &str = "codex-switch/";

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type ProxyClientStream = WebSocketStream<TcpStream>;

enum RuntimeCommand {
    LoginPreparedAccount(RuntimeLoginCommand),
}

struct RuntimeLoginCommand {
    prepared: PreparedAccountLogin,
    expected_snapshot: Option<CurrentAccountSnapshot>,
    completion: Option<oneshot::Sender<RuntimeLoginResult>>,
}

impl RuntimeLoginCommand {
    fn fire_and_forget(prepared: PreparedAccountLogin) -> Self {
        let expected_snapshot = prepared.current_snapshot.clone();
        Self {
            prepared,
            expected_snapshot,
            completion: None,
        }
    }

    fn reconciled(
        prepared: PreparedAccountLogin,
        expected_snapshot: CurrentAccountSnapshot,
        completion: oneshot::Sender<RuntimeLoginResult>,
    ) -> Self {
        Self {
            prepared,
            expected_snapshot: Some(expected_snapshot),
            completion: Some(completion),
        }
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

#[derive(Debug)]
enum BackgroundRuntimeRequest {
    AutoSwitch,
}

pub async fn run_codex(codex_bin: String, codex_args: Vec<String>) -> Result<ExitStatus> {
    codex_http::set_codex_bin_for_user_agent(codex_bin.clone());
    validate_remote_capable_codex_args(&codex_args)?;
    reject_remote_args(&codex_args)?;
    let current_dir = std::env::current_dir().context("Failed to read current directory")?;
    let codex_args = codex_args_with_default_cwd(&codex_args, &current_dir);

    let initial = auto_switch::auto_switch_allow_running()
        .await
        .context("Initial account auto-switch failed")?;
    if let AutoSwitchResult::CurrentUnsupported { reason, .. } = initial {
        anyhow::bail!("current account does not support runtime auto-switch: {reason}");
    }

    let port = reserve_local_port()?;
    let app_server_url = format!("ws://127.0.0.1:{port}");
    let token = websocket_token();
    let token_path = runtime_token_path()?;
    store::write_private_file(&token_path, &token)?;

    let mut app_server = match spawn_app_server(&codex_bin, &app_server_url, &token_path) {
        Ok(app_server) => app_server,
        Err(err) => {
            let _ = std::fs::remove_file(&token_path);
            return Err(err);
        }
    };
    if let Err(err) = wait_for_app_server_ready(&app_server_url, &token, &mut app_server).await {
        shutdown_child(&mut app_server).await;
        let _ = std::fs::remove_file(&token_path);
        return Err(err);
    }

    let (proxy_listener, proxy_url) = match bind_proxy_listener().await {
        Ok(listener) => listener,
        Err(err) => {
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
        maintenance_shutdown_rx,
    ));
    let mut proxy_task = tokio::spawn(run_websocket_proxy(
        proxy_listener,
        app_server_url.clone(),
        token.clone(),
        background_runtime_tx,
        runtime_auto_switch_coordinator,
        runtime_command_rx,
    ));

    let mut codex_child = match spawn_remote_codex(&codex_bin, &codex_args, &proxy_url, &token)
        .context("Failed to start codex")
    {
        Ok(child) => child,
        Err(err) => {
            proxy_task.abort();
            let _ = proxy_task.await;
            stop_runtime_background_tasks(
                maintenance_shutdown_tx,
                background_runtime_task,
                maintenance_task,
                current_account_reconcile_task,
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
                    shutdown_child(&mut codex_child).await;
                    Err(err).context("Codex websocket proxy failed")
                }
                Err(err) => {
                    shutdown_child(&mut codex_child).await;
                    Err(anyhow::anyhow!("Codex websocket proxy task failed: {err}"))
                }
            }
        }
    };

    if !proxy_task_completed {
        proxy_task.abort();
        let _ = proxy_task.await;
    }
    stop_runtime_background_tasks(
        maintenance_shutdown_tx,
        background_runtime_task,
        maintenance_task,
        current_account_reconcile_task,
    )
    .await;
    shutdown_child(&mut app_server).await;
    let _ = std::fs::remove_file(&token_path);

    status
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
) -> Result<Child> {
    let mut command = Command::new(codex_bin);
    command
        .args(codex_args)
        .arg("--remote")
        .arg(websocket_url)
        .arg("--remote-auth-token-env")
        .arg(REMOTE_TOKEN_ENV)
        .env(REMOTE_TOKEN_ENV, token);
    Ok(command.spawn()?)
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
    mut runtime_commands: mpsc::Receiver<RuntimeCommand>,
) -> Result<()> {
    let mut state = ProxyState::new(background_requests, runtime_auto_switch_coordinator);

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
) {
    let _ = shutdown.send(true);
    stop_task_with_timeout(background_runtime_task).await;
    stop_task_with_timeout(maintenance_task).await;
    stop_task_with_timeout(current_account_reconcile_task).await;
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
    execution: AsyncMutex<()>,
}

impl RuntimeAutoSwitchCoordinator {
    fn new(cooldown: Duration) -> Self {
        Self {
            schedule: StdMutex::new(RuntimeAutoSwitchSchedule::new(cooldown)),
            execution: AsyncMutex::new(()),
        }
    }
}

#[derive(Debug)]
struct RuntimeAutoSwitchSchedule {
    cooldown: Duration,
    last_attempt: Option<Instant>,
    in_flight: bool,
}

impl RuntimeAutoSwitchSchedule {
    fn new(cooldown: Duration) -> Self {
        Self {
            cooldown,
            last_attempt: None,
            in_flight: false,
        }
    }

    fn try_start_background(&mut self, now: Instant) -> BackgroundAutoSwitchQueueStatus {
        if self.in_flight {
            return BackgroundAutoSwitchQueueStatus::InFlight;
        }
        if let Some(last_attempt) = self.last_attempt
            && now < last_attempt + self.cooldown
        {
            return BackgroundAutoSwitchQueueStatus::Cooldown;
        }

        self.last_attempt = Some(now);
        self.in_flight = true;
        BackgroundAutoSwitchQueueStatus::Queued
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
    let status = schedule.try_start_background(now);
    if status != BackgroundAutoSwitchQueueStatus::Queued {
        return status;
    }

    match requests.try_send(BackgroundRuntimeRequest::AutoSwitch) {
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

fn finish_background_auto_switch(coordinator: &SharedRuntimeAutoSwitchCoordinator) {
    if let Ok(mut schedule) = coordinator.schedule.lock() {
        schedule.finish();
    }
}

async fn run_serialized_auto_switch_operation<T, F, Fut>(
    coordinator: &SharedRuntimeAutoSwitchCoordinator,
    operation: F,
) -> T
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    let _guard = coordinator.execution.lock().await;
    operation().await
}

async fn run_background_runtime_worker(
    mut requests: mpsc::Receiver<BackgroundRuntimeRequest>,
    runtime_commands: mpsc::Sender<RuntimeCommand>,
    coordinator: SharedRuntimeAutoSwitchCoordinator,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_ok() && *shutdown.borrow() {
                    return;
                }
            }
            request = requests.recv() => {
                let Some(request) = request else {
                    return;
                };

                let command = match request {
                    BackgroundRuntimeRequest::AutoSwitch => {
                        let command = background_auto_switch_login_command(&coordinator).await;
                        finish_background_auto_switch(&coordinator);
                        command
                    }
                };
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

async fn background_auto_switch_login_command(
    coordinator: &SharedRuntimeAutoSwitchCoordinator,
) -> Option<RuntimeCommand> {
    match serialized_auto_switch_prepared_login(coordinator, AutoSwitchLoginMode::SwitchedOnly)
        .await
    {
        Ok(Some(prepared)) => Some(RuntimeCommand::LoginPreparedAccount(
            RuntimeLoginCommand::fire_and_forget(prepared),
        )),
        Ok(None) => None,
        Err(_) => None,
    }
}

#[derive(Debug, Clone)]
enum AutoSwitchLoginMode {
    SwitchedOnly,
    EnsureCurrentLogged {
        app_server_account_id: Option<String>,
        app_server_snapshot: Option<CurrentAccountSnapshot>,
    },
}

async fn serialized_auto_switch_prepared_login(
    coordinator: &SharedRuntimeAutoSwitchCoordinator,
    mode: AutoSwitchLoginMode,
) -> Result<Option<PreparedAccountLogin>> {
    run_serialized_auto_switch_operation(coordinator, || async move {
        let result = auto_switch::auto_switch_allow_running().await?;
        let account = auto_switch_login_account(result, &mode)?;
        match account {
            Some(account) => prepare_login(account).await.map(Some),
            None => Ok(None),
        }
    })
    .await
}

fn current_account_snapshot_for_account(account: &StoredAccount) -> CurrentAccountSnapshot {
    CurrentAccountSnapshot {
        current_account_id: Some(account.id.clone()),
        auth_marker: Some(current_account_auth_marker(account)),
    }
}

fn auto_switch_login_account(
    result: AutoSwitchResult,
    mode: &AutoSwitchLoginMode,
) -> Result<Option<StoredAccount>> {
    match result {
        AutoSwitchResult::Switched { to, .. } => Ok(Some(*to)),
        AutoSwitchResult::CurrentKept { account, .. } => match mode {
            AutoSwitchLoginMode::SwitchedOnly => Ok(None),
            AutoSwitchLoginMode::EnsureCurrentLogged {
                app_server_account_id,
                app_server_snapshot,
            } => {
                let (account, current_snapshot) = latest_current_account_snapshot(*account)?;
                Ok(select_current_kept_login_account(
                    account,
                    current_snapshot,
                    app_server_account_id.as_deref(),
                    app_server_snapshot.as_ref(),
                ))
            }
        },
        AutoSwitchResult::CurrentUnsupported { .. } => Ok(None),
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

        if let ActiveAccountReconcileAction::Attempt { snapshot } =
            state.next_action(current_snapshot, Instant::now())
        {
            let result = attempt_current_account_reconcile(
                &runtime_commands,
                snapshot.clone(),
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
    settled: bool,
    attempts_started: usize,
    next_attempt_at: Instant,
}

impl ActiveAccountReconcileState {
    fn new(initial_snapshot: Option<CurrentAccountSnapshot>, now: Instant) -> Self {
        Self {
            desired_snapshot: initial_snapshot,
            settled: true,
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
        ActiveAccountReconcileAction::Attempt { snapshot }
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
    Attempt { snapshot: CurrentAccountSnapshot },
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
    let current_account = auth_json::current_stored_account(&store)?;
    let current_account_id = current_account.as_ref().map(|account| account.id.clone());
    let auth_marker = current_account.as_ref().map(current_account_auth_marker);

    Ok(CurrentAccountSnapshot {
        current_account_id,
        auth_marker,
    })
}

fn current_snapshot_matches(expected: &CurrentAccountSnapshot) -> Result<bool> {
    Ok(current_account_snapshot()? == *expected)
}

fn current_account_auth_marker(account: &StoredAccount) -> String {
    let last_used_at = account
        .last_used_at
        .map(|value| value.timestamp_millis())
        .unwrap_or_default();
    match &account.auth_data {
        AuthData::ChatGPT { account_id, .. } => format!(
            "chatgpt:{}:{}:{}:{}",
            account_id.as_deref().unwrap_or_default(),
            account
                .token_last_refresh_at
                .map(|value| value.timestamp_millis())
                .unwrap_or_default(),
            last_used_at,
            account.plan_type.as_deref().unwrap_or_default()
        ),
        AuthData::ApiKey { .. } => format!("api_key:{last_used_at}"),
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
    accept_hdr_async(stream, move |request: &Request, response: Response| {
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
    })
    .await
    .context("Failed to accept Codex websocket client")
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

    loop {
        tokio::select! {
            command = runtime_commands.recv(), if runtime_commands_open => {
                match command {
                    Some(RuntimeCommand::LoginPreparedAccount(prepared)) => {
                        state
                            .login_prepared_chatgpt_account(&mut app_server_write, prepared)
                            .await?;
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
                if !handle_client_proxy_message(message, &mut app_server_write).await? {
                    return Ok(());
                }
            }
            message = app_server_read.next() => {
                let Some(message) = message else {
                    return Ok(());
                };
                let message = message.context("Failed to read Codex app-server websocket message")?;
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
    app_server_snapshot: Option<CurrentAccountSnapshot>,
    pending_login_account_id: Option<String>,
    background_requests: mpsc::Sender<BackgroundRuntimeRequest>,
    runtime_auto_switch_coordinator: SharedRuntimeAutoSwitchCoordinator,
}

enum PendingInternalRequest {
    Login {
        account_id: String,
        started_at: Instant,
        expected_snapshot: Option<CurrentAccountSnapshot>,
        completion: Option<oneshot::Sender<RuntimeLoginResult>>,
    },
}

impl ProxyState {
    fn new(
        background_requests: mpsc::Sender<BackgroundRuntimeRequest>,
        runtime_auto_switch_coordinator: SharedRuntimeAutoSwitchCoordinator,
    ) -> Self {
        Self {
            request_prefix: format!("{INTERNAL_REQUEST_ID_PREFIX}{}", Uuid::new_v4()),
            next_request_id: 1,
            pending_internal: HashMap::new(),
            app_server_account_id: None,
            app_server_snapshot: None,
            pending_login_account_id: None,
            background_requests,
            runtime_auto_switch_coordinator,
        }
    }

    fn next_internal_request_id(&mut self) -> String {
        let id = format!("{}/{}", self.request_prefix, self.next_request_id);
        self.next_request_id += 1;
        id
    }

    fn clear_connection_pending(&mut self) {
        self.cancel_pending_logins(RuntimeLoginResult::Transient(
            "Codex app-server connection closed before login completed".to_string(),
        ));
    }

    async fn auto_switch_and_login(
        &mut self,
        app_server_write: &mut SplitSink<WsStream, Message>,
    ) -> Result<()> {
        if let Some(prepared) = serialized_auto_switch_prepared_login(
            &self.runtime_auto_switch_coordinator,
            AutoSwitchLoginMode::EnsureCurrentLogged {
                app_server_account_id: self.app_server_account_id.clone(),
                app_server_snapshot: self.app_server_snapshot.clone(),
            },
        )
        .await?
        {
            self.login_prepared_chatgpt_account(
                app_server_write,
                RuntimeLoginCommand::fire_and_forget(prepared),
            )
            .await?;
        }
        Ok(())
    }

    async fn login_prepared_chatgpt_account(
        &mut self,
        app_server_write: &mut SplitSink<WsStream, Message>,
        command: RuntimeLoginCommand,
    ) -> Result<()> {
        let RuntimeLoginCommand {
            prepared,
            expected_snapshot,
            completion,
        } = command;
        self.clear_expired_pending_logins(Instant::now());

        let snapshot_matches = match &expected_snapshot {
            Some(snapshot) => match current_snapshot_matches(snapshot) {
                Ok(matches) => matches,
                Err(err) => {
                    complete_runtime_login(
                        completion,
                        RuntimeLoginResult::Transient(err.to_string()),
                    );
                    return Ok(());
                }
            },
            None => current_account_id_matches(&prepared.account_id)?,
        };
        if !snapshot_matches {
            complete_runtime_login(completion, RuntimeLoginResult::Stale);
            return Ok(());
        }

        match self.pending_login_status(&prepared.account_id, expected_snapshot.as_ref()) {
            PendingLoginStatus::None => {}
            PendingLoginStatus::Matching => {
                complete_runtime_login(
                    completion,
                    RuntimeLoginResult::Deferred("Login request is already pending".to_string()),
                );
                return Ok(());
            }
            PendingLoginStatus::Other => {
                complete_runtime_login(
                    completion,
                    RuntimeLoginResult::Deferred(
                        "Another login request is already pending".to_string(),
                    ),
                );
                return Ok(());
            }
        }

        if self.runtime_login_already_matches(&prepared.account_id, expected_snapshot.as_ref()) {
            complete_runtime_login(completion, RuntimeLoginResult::Success);
            return Ok(());
        }

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
                completion,
            },
        );
        self.pending_login_account_id = Some(prepared.account_id);
        Ok(())
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
            Some(snapshot) => self.app_server_snapshot.as_ref() == Some(snapshot),
            None => self.app_server_account_id.as_deref() == Some(account_id),
        }
    }

    fn pending_login_status(
        &self,
        account_id: &str,
        expected_snapshot: Option<&CurrentAccountSnapshot>,
    ) -> PendingLoginStatus {
        let mut has_pending_login = false;
        for pending in self.pending_internal.values() {
            let PendingInternalRequest::Login {
                account_id: pending_account_id,
                expected_snapshot: pending_snapshot,
                ..
            } = pending;
            has_pending_login = true;
            if pending_account_id == account_id && pending_snapshot.as_ref() == expected_snapshot {
                return PendingLoginStatus::Matching;
            }
        }

        if has_pending_login {
            PendingLoginStatus::Other
        } else {
            PendingLoginStatus::None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingLoginStatus {
    None,
    Matching,
    Other,
}

impl ProxyState {
    fn handle_internal_response(&mut self, value: &Value) -> bool {
        self.clear_expired_pending_logins(Instant::now());
        let Some(request_id) = value.get("id").and_then(Value::as_str) else {
            return false;
        };
        let Some(pending) = self.pending_internal.remove(request_id) else {
            return request_id.starts_with(&self.request_prefix);
        };

        match pending {
            PendingInternalRequest::Login {
                account_id,
                started_at: _,
                expected_snapshot,
                completion,
            } => {
                if self.pending_login_account_id.as_deref() == Some(account_id.as_str()) {
                    self.pending_login_account_id = None;
                }
                let result = match &expected_snapshot {
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
                };
                if result == RuntimeLoginResult::Success {
                    self.app_server_account_id = Some(account_id);
                    self.app_server_snapshot = expected_snapshot;
                }
                complete_runtime_login(completion, result);
            }
        }
        true
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
    message: Message,
    app_server_write: &mut SplitSink<WsStream, Message>,
) -> Result<bool> {
    let should_continue = !matches!(message, Message::Close(_));
    app_server_write
        .send(message)
        .await
        .context("Failed to forward Codex client message to app-server")?;
    Ok(should_continue)
}

async fn handle_app_server_proxy_message(
    message: Message,
    client_write: &mut SplitSink<ProxyClientStream, Message>,
    app_server_write: &mut SplitSink<WsStream, Message>,
    state: &mut ProxyState,
) -> Result<bool> {
    let mut auto_switch_trigger = RateLimitAutoSwitchTrigger::None;
    let mut should_forward = true;
    state.clear_expired_pending_logins(Instant::now());

    if let Some(value) = message_json(&message)? {
        if state.handle_internal_response(&value) {
            should_forward = false;
        } else if is_jsonrpc_request(&value)
            && value.get("method").and_then(Value::as_str)
                == Some("account/chatgptAuthTokens/refresh")
        {
            handle_server_request(app_server_write, value).await?;
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
    match auto_switch_trigger {
        RateLimitAutoSwitchTrigger::Hard => {
            let _ = state.auto_switch_and_login(app_server_write).await;
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
        info.primary_used_percent
            .and_then(headroom_from_used_percent),
        info.secondary_used_percent.and_then(|used| {
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

    let (websocket, _) = connect_async(request)
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

async fn handle_server_request<S>(websocket: &mut S, request: Value) -> Result<()>
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
                Ok(payload) => {
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
                    .await
                }
                Err(err) => send_error_response(websocket, id, &err.to_string()).await,
            }
        }
        _ => send_error_response(websocket, id, "unsupported request").await,
    }
}

async fn refresh_external_auth(previous_account_id: Option<String>) -> Result<ExternalAuthPayload> {
    let account = find_refresh_account(previous_account_id.as_deref())?;
    external_auth_payload(&account).await
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
    payload: ExternalAuthPayload,
    current_snapshot: Option<CurrentAccountSnapshot>,
}

async fn prepare_login(account: StoredAccount) -> Result<PreparedAccountLogin> {
    let account = token::ensure_chatgpt_tokens_fresh(&account).await?;
    let account_id = account.id.clone();
    let current_snapshot = prepared_current_account_snapshot(&account)?;
    let payload = external_auth_payload_from_fresh_account(account)?;
    Ok(PreparedAccountLogin {
        account_id,
        payload,
        current_snapshot,
    })
}

struct ExternalAuthPayload {
    access_token: String,
    chatgpt_account_id: String,
    chatgpt_plan_type: Option<String>,
}

async fn external_auth_payload(account: &StoredAccount) -> Result<ExternalAuthPayload> {
    let account = token::ensure_chatgpt_tokens_fresh(account).await?;
    external_auth_payload_from_fresh_account(account)
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

    use chrono::Utc;
    use serde_json::json;
    use tokio::sync::{mpsc, oneshot};
    use tokio::time::{Instant, timeout};

    use super::{
        ACTIVE_ACCOUNT_MAX_ATTEMPTS, ACTIVE_ACCOUNT_RETRY_DELAYS, ACTIVE_ACCOUNT_WATCH_INTERVAL,
        APP_SERVER_REQUEST_TIMEOUT, ActiveAccountReconcileAction, ActiveAccountReconcileOutcome,
        ActiveAccountReconcileState, BackgroundAutoSwitchQueueStatus, BackgroundRuntimeRequest,
        CurrentAccountSnapshot, ExternalAuthPayload, PendingInternalRequest, PendingLoginStatus,
        PreparedAccountLogin, ProxyState, RateLimitAutoSwitchTrigger, RuntimeCommand,
        RuntimeCommandSendStatus, RuntimeLoginCommand, RuntimeLoginResult,
        classify_rate_limit_notification, classify_runtime_login_error,
        codex_args_with_default_cwd, current_account_snapshot_for_account,
        finish_background_auto_switch, has_cwd_arg, initialize_app_server_request,
        queue_background_auto_switch, random_duration_between,
        run_serialized_auto_switch_operation, select_current_kept_login_account,
        shared_runtime_auto_switch_coordinator, try_send_background_runtime_command,
        usage_limit_error_requires_switch, validate_remote_capable_codex_args,
    };
    use crate::types::{NewChatGptAccount, StoredAccount};

    #[test]
    fn app_server_probe_uses_codex_daemon_identity() {
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
        assert!(request.pointer("/params/capabilities").is_none());
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

    #[tokio::test]
    async fn runtime_auto_switch_operations_are_serialized() {
        let coordinator = shared_runtime_auto_switch_coordinator();
        let (background_entered_tx, mut background_entered_rx) = mpsc::channel(1);
        let (release_background_tx, release_background_rx) = oneshot::channel();
        let (hard_entered_tx, mut hard_entered_rx) = mpsc::channel(1);

        let background_coordinator = coordinator.clone();
        let background_task = tokio::spawn(async move {
            run_serialized_auto_switch_operation(&background_coordinator, || async move {
                background_entered_tx
                    .send(())
                    .await
                    .expect("background entered signal should be received");
                let _ = release_background_rx.await;
            })
            .await;
        });

        assert!(background_entered_rx.recv().await.is_some());

        let hard_coordinator = coordinator.clone();
        let hard_task = tokio::spawn(async move {
            run_serialized_auto_switch_operation(&hard_coordinator, || async move {
                hard_entered_tx
                    .send(())
                    .await
                    .expect("hard entered signal should be received");
            })
            .await;
        });

        assert!(matches!(
            hard_entered_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        release_background_tx
            .send(())
            .expect("background task should still be waiting");
        assert!(
            timeout(Duration::from_secs(1), hard_entered_rx.recv())
                .await
                .expect("hard operation should enter after background releases")
                .is_some()
        );

        background_task
            .await
            .expect("background task should finish");
        hard_task.await.expect("hard task should finish");
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
            .try_send(BackgroundRuntimeRequest::AutoSwitch)
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
    fn active_account_reconciler_retries_transient_failure_then_settles_success() {
        let now = Instant::now();
        let initial_snapshot = current_snapshot("account-a");
        let changed_snapshot = current_snapshot("account-b");
        let mut state = ActiveAccountReconcileState::new(Some(initial_snapshot), now);

        assert_eq!(
            state.next_action(changed_snapshot.clone(), now),
            ActiveAccountReconcileAction::Attempt {
                snapshot: changed_snapshot.clone()
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
                snapshot: changed_snapshot.clone()
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
                    snapshot: changed_snapshot.clone()
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
                snapshot: changed_snapshot.clone()
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
                    snapshot: changed_snapshot.clone()
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
                snapshot: changed_snapshot
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
                snapshot: second_snapshot
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
    fn proxy_reconciled_login_matches_snapshot_not_only_account_id() {
        let (background_tx, _background_rx) = mpsc::channel(1);
        let mut state = ProxyState::new(background_tx, shared_runtime_auto_switch_coordinator());
        let older_snapshot = current_snapshot("account-a");
        let mut refreshed_snapshot = current_snapshot("account-a");
        refreshed_snapshot.auth_marker = Some("chatgpt:account-a:2:1:pro".to_string());
        state.app_server_account_id = Some("account-a".to_string());
        state.app_server_snapshot = Some(older_snapshot);

        assert!(!state.runtime_login_already_matches("account-a", Some(&refreshed_snapshot)));
        assert!(state.runtime_login_already_matches("account-a", None));
    }

    #[test]
    fn proxy_pending_login_status_matches_snapshot_not_only_account_id() {
        let (background_tx, _background_rx) = mpsc::channel(1);
        let mut state = ProxyState::new(background_tx, shared_runtime_auto_switch_coordinator());
        let older_snapshot = current_snapshot("account-a");
        let mut refreshed_snapshot = current_snapshot("account-a");
        refreshed_snapshot.auth_marker = Some("chatgpt:account-a:2:1:pro".to_string());
        state.pending_internal.insert(
            "request-id".to_string(),
            PendingInternalRequest::Login {
                account_id: "account-a".to_string(),
                started_at: Instant::now(),
                expected_snapshot: Some(older_snapshot.clone()),
                completion: None,
            },
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
        stale_snapshot.auth_marker = Some("chatgpt:account-a:0:0:free".to_string());

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

    #[tokio::test]
    async fn proxy_internal_login_success_marks_runtime_account() {
        let (background_tx, _background_rx) = mpsc::channel(1);
        let mut state = ProxyState::new(background_tx, shared_runtime_auto_switch_coordinator());
        let (completion_tx, completion_rx) = oneshot::channel();
        state.pending_login_account_id = Some("account-a".to_string());
        state.pending_internal.insert(
            "request-id".to_string(),
            PendingInternalRequest::Login {
                account_id: "account-a".to_string(),
                started_at: Instant::now(),
                expected_snapshot: None,
                completion: Some(completion_tx),
            },
        );

        assert!(state.handle_internal_response(&json!({
            "id": "request-id",
            "result": null
        })));

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
    async fn proxy_internal_login_rejection_does_not_mark_runtime_account() {
        let (background_tx, _background_rx) = mpsc::channel(1);
        let mut state = ProxyState::new(background_tx, shared_runtime_auto_switch_coordinator());
        let (completion_tx, completion_rx) = oneshot::channel();
        state.pending_login_account_id = Some("account-a".to_string());
        state.pending_internal.insert(
            "request-id".to_string(),
            PendingInternalRequest::Login {
                account_id: "account-a".to_string(),
                started_at: Instant::now(),
                expected_snapshot: None,
                completion: Some(completion_tx),
            },
        );

        assert!(state.handle_internal_response(&json!({
            "id": "request-id",
            "error": {
                "message": "invalid_grant"
            }
        })));

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
        let mut state = ProxyState::new(background_tx, shared_runtime_auto_switch_coordinator());
        let request_id = state.next_internal_request_id();
        let (completion_tx, completion_rx) = oneshot::channel();
        state.pending_login_account_id = Some("account-a".to_string());
        state.pending_internal.insert(
            request_id.clone(),
            PendingInternalRequest::Login {
                account_id: "account-a".to_string(),
                started_at: Instant::now() - APP_SERVER_REQUEST_TIMEOUT - Duration::from_secs(1),
                expected_snapshot: None,
                completion: Some(completion_tx),
            },
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
        assert!(state.handle_internal_response(&json!({
            "id": request_id,
            "result": null
        })));
        assert_eq!(state.app_server_account_id, None);
    }

    #[tokio::test]
    async fn proxy_cancels_connection_pending_login_and_swallows_late_response() {
        let (background_tx, _background_rx) = mpsc::channel(1);
        let mut state = ProxyState::new(background_tx, shared_runtime_auto_switch_coordinator());
        let request_id = state.next_internal_request_id();
        let (completion_tx, completion_rx) = oneshot::channel();
        state.pending_login_account_id = Some("account-a".to_string());
        state.pending_internal.insert(
            request_id.clone(),
            PendingInternalRequest::Login {
                account_id: "account-a".to_string(),
                started_at: Instant::now(),
                expected_snapshot: None,
                completion: Some(completion_tx),
            },
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
        assert!(state.handle_internal_response(&json!({
            "id": request_id,
            "result": null
        })));
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
            auth_marker: Some(format!("chatgpt:{account_id}:1:1:pro")),
        }
    }

    fn prepared_login(
        account_id: &str,
        current_snapshot: Option<CurrentAccountSnapshot>,
    ) -> PreparedAccountLogin {
        PreparedAccountLogin {
            account_id: account_id.to_string(),
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

    fn rate_limit_notification(
        primary_used_percent: Option<f64>,
        secondary_used_percent: Option<f64>,
        rate_limit_reached_type: Option<&str>,
    ) -> serde_json::Value {
        json!({
            "method": "account/rateLimits/updated",
            "params": {
                "rateLimits": {
                    "limitId": "codex",
                    "limitName": null,
                    "primary": primary_used_percent.map(|used_percent| {
                        json!({
                            "usedPercent": used_percent,
                            "windowDurationMins": 300,
                            "resetsAt": 1_800_000_000
                        })
                    }),
                    "secondary": secondary_used_percent.map(|used_percent| {
                        json!({
                            "usedPercent": used_percent,
                            "windowDurationMins": 10080,
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
