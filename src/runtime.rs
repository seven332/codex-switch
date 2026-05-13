use std::collections::HashMap;
use std::ffi::OsString;
use std::net::TcpListener as StdTcpListener;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use anyhow::{Context, Result};
use futures_util::stream::SplitSink;
use futures_util::{Sink, SinkExt, StreamExt};
use rand::Rng as _;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Duration, Instant, sleep, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, accept_hdr_async, connect_async};
use uuid::Uuid;

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
const ACTIVE_ACCOUNT_WATCH_INTERVAL: Duration = Duration::from_secs(1);
const RUNTIME_COMMAND_BUFFER: usize = 4;
const INTERNAL_REQUEST_ID_PREFIX: &str = "codex-switch/";

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type ProxyClientStream = WebSocketStream<TcpStream>;

#[derive(Debug)]
enum RuntimeCommand {
    SyncActiveAccount,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ActiveAccountWatchStatus {
    Unchanged,
    Queued,
    Full,
    Closed,
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
    if let AutoSwitchResult::ActiveUnsupported { reason, .. } = initial {
        anyhow::bail!("active account does not support runtime auto-switch: {reason}");
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
    let (maintenance_shutdown_tx, maintenance_shutdown_rx) = watch::channel(false);
    let maintenance_task = tokio::spawn(run_auto_switch_maintenance(
        runtime_command_tx.clone(),
        maintenance_shutdown_rx.clone(),
    ));
    let active_account_watch_task = tokio::spawn(run_active_account_watcher(
        runtime_command_tx,
        maintenance_shutdown_rx,
    ));
    let mut proxy_task = tokio::spawn(run_websocket_proxy(
        proxy_listener,
        app_server_url.clone(),
        token.clone(),
        runtime_command_rx,
    ));

    let mut codex_child = match spawn_remote_codex(&codex_bin, &codex_args, &proxy_url, &token)
        .context("Failed to start codex")
    {
        Ok(child) => child,
        Err(err) => {
            stop_runtime_background_tasks(
                maintenance_shutdown_tx,
                maintenance_task,
                active_account_watch_task,
            )
            .await;
            proxy_task.abort();
            let _ = proxy_task.await;
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
        maintenance_task,
        active_account_watch_task,
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
    mut runtime_commands: mpsc::Receiver<RuntimeCommand>,
) -> Result<()> {
    let mut state = ProxyState::new();

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
    maintenance_task: JoinHandle<()>,
    active_account_watch_task: JoinHandle<()>,
) {
    let _ = shutdown.send(true);
    let _ = maintenance_task.await;
    let _ = active_account_watch_task.await;
}

async fn run_auto_switch_maintenance(
    runtime_commands: mpsc::Sender<RuntimeCommand>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        if sleep_until_shutdown(random_auto_switch_maintenance_interval(), &mut shutdown).await {
            return;
        }

        if auto_switch_maintenance_switched_account().await {
            match runtime_commands.try_send(RuntimeCommand::SyncActiveAccount) {
                Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
                Err(mpsc::error::TrySendError::Closed(_)) => return,
            }
        }
    }
}

async fn sleep_until_shutdown(duration: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        _ = sleep(duration) => false,
        changed = shutdown.changed() => changed.is_ok() && *shutdown.borrow(),
    }
}

async fn auto_switch_maintenance_switched_account() -> bool {
    matches!(
        auto_switch::auto_switch_allow_running().await,
        Ok(AutoSwitchResult::Switched { .. })
    )
}

async fn run_active_account_watcher(
    runtime_commands: mpsc::Sender<RuntimeCommand>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut last_snapshot = active_account_snapshot().ok();

    loop {
        if sleep_until_shutdown(ACTIVE_ACCOUNT_WATCH_INTERVAL, &mut shutdown).await {
            return;
        }

        let Ok(current_snapshot) = active_account_snapshot() else {
            continue;
        };
        match handle_active_account_snapshot_change(
            &runtime_commands,
            &mut last_snapshot,
            current_snapshot,
        ) {
            ActiveAccountWatchStatus::Unchanged
            | ActiveAccountWatchStatus::Queued
            | ActiveAccountWatchStatus::Full => {}
            ActiveAccountWatchStatus::Closed => return,
        }
    }
}

fn handle_active_account_snapshot_change(
    runtime_commands: &mpsc::Sender<RuntimeCommand>,
    last_snapshot: &mut Option<ActiveAccountSnapshot>,
    current_snapshot: ActiveAccountSnapshot,
) -> ActiveAccountWatchStatus {
    if Some(current_snapshot.clone()) == *last_snapshot {
        return ActiveAccountWatchStatus::Unchanged;
    }

    match runtime_commands.try_send(RuntimeCommand::SyncActiveAccount) {
        Ok(()) => {
            *last_snapshot = Some(current_snapshot);
            ActiveAccountWatchStatus::Queued
        }
        Err(mpsc::error::TrySendError::Full(_)) => ActiveAccountWatchStatus::Full,
        Err(mpsc::error::TrySendError::Closed(_)) => ActiveAccountWatchStatus::Closed,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ActiveAccountSnapshot {
    active_account_id: Option<String>,
    auth_marker: Option<String>,
}

fn active_account_snapshot() -> Result<ActiveAccountSnapshot> {
    let store = store::load_accounts()?;
    let active_account_id = store.active_account_id.clone();
    let auth_marker = active_account_id
        .as_deref()
        .and_then(|active_id| {
            store
                .accounts
                .iter()
                .find(|account| account.id == active_id)
        })
        .map(active_account_auth_marker);

    Ok(ActiveAccountSnapshot {
        active_account_id,
        auth_marker,
    })
}

fn active_account_auth_marker(account: &StoredAccount) -> String {
    let last_used_at = account
        .last_used_at
        .map(|value| value.timestamp_millis())
        .unwrap_or_default();
    match &account.auth_data {
        AuthData::ChatGPT { account_id, .. } => format!(
            "chatgpt:{}:{}:{}",
            account_id.as_deref().unwrap_or_default(),
            account
                .token_last_refresh_at
                .map(|value| value.timestamp_millis())
                .unwrap_or_default(),
            last_used_at
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
                    Some(RuntimeCommand::SyncActiveAccount) => {
                        state.login_active_chatgpt_account(&mut app_server_write).await?;
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
    pending_login_account_id: Option<String>,
}

enum PendingInternalRequest {
    Login { account_id: String },
}

impl ProxyState {
    fn new() -> Self {
        Self {
            request_prefix: format!("{INTERNAL_REQUEST_ID_PREFIX}{}", Uuid::new_v4()),
            next_request_id: 1,
            pending_internal: HashMap::new(),
            app_server_account_id: None,
            pending_login_account_id: None,
        }
    }

    fn next_internal_request_id(&mut self) -> String {
        let id = format!("{}/{}", self.request_prefix, self.next_request_id);
        self.next_request_id += 1;
        id
    }

    fn clear_connection_pending(&mut self) {
        self.pending_internal.clear();
        self.pending_login_account_id = None;
    }

    async fn auto_switch_and_login(
        &mut self,
        app_server_write: &mut SplitSink<WsStream, Message>,
    ) -> Result<()> {
        let result = auto_switch::auto_switch_allow_running().await?;
        match result {
            AutoSwitchResult::Switched { to, .. } => {
                self.login_chatgpt_account(app_server_write, *to).await
            }
            AutoSwitchResult::ActiveKept { account, .. } => {
                if self.app_server_account_id.as_deref() != Some(account.id.as_str()) {
                    self.login_chatgpt_account(app_server_write, *account)
                        .await?;
                }
                Ok(())
            }
            AutoSwitchResult::ActiveUnsupported { .. } => Ok(()),
        }
    }

    async fn login_active_chatgpt_account(
        &mut self,
        app_server_write: &mut SplitSink<WsStream, Message>,
    ) -> Result<()> {
        let Some(account) = store::get_active_account()? else {
            return Ok(());
        };
        if matches!(account.auth_data, AuthData::ChatGPT { .. }) {
            self.login_chatgpt_account(app_server_write, account)
                .await?;
        }
        Ok(())
    }

    async fn login_chatgpt_account(
        &mut self,
        app_server_write: &mut SplitSink<WsStream, Message>,
        account: StoredAccount,
    ) -> Result<()> {
        if self.app_server_account_id.as_deref() == Some(account.id.as_str())
            || self.pending_login_account_id.as_deref() == Some(account.id.as_str())
        {
            return Ok(());
        }

        let request_id = self.next_internal_request_id();
        let request = login_request(Value::String(request_id.clone()), &account).await?;
        send_json(app_server_write, request).await?;
        self.pending_internal.insert(
            request_id,
            PendingInternalRequest::Login {
                account_id: account.id.clone(),
            },
        );
        self.pending_login_account_id = Some(account.id);
        Ok(())
    }

    fn handle_internal_response(&mut self, value: &Value) -> bool {
        let Some(request_id) = value.get("id").and_then(Value::as_str) else {
            return false;
        };
        let Some(pending) = self.pending_internal.remove(request_id) else {
            return false;
        };

        match pending {
            PendingInternalRequest::Login { account_id } => {
                if self.pending_login_account_id.as_deref() == Some(account_id.as_str()) {
                    self.pending_login_account_id = None;
                }
                if response_to_result(value).is_ok() {
                    self.app_server_account_id = Some(account_id);
                }
            }
        }
        true
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
    let mut rate_limits_require_switch = false;
    let mut should_forward = true;

    if let Some(value) = message_json(&message)? {
        if state.handle_internal_response(&value) {
            should_forward = false;
        } else if is_jsonrpc_request(&value)
            && value.get("method").and_then(Value::as_str)
                == Some("account/chatgptAuthTokens/refresh")
        {
            handle_server_request(app_server_write, value).await?;
            should_forward = false;
        } else if rate_limit_notification_requires_switch(&value)
            || usage_limit_error_requires_switch(&value)
        {
            rate_limits_require_switch = true;
        }
    }

    let should_continue = !matches!(message, Message::Close(_));
    if should_forward {
        client_write
            .send(message)
            .await
            .context("Failed to forward Codex app-server message to client")?;
    }
    if rate_limits_require_switch {
        let _ = state.auto_switch_and_login(app_server_write).await;
    }

    Ok(should_continue)
}

fn rate_limit_notification_requires_switch(value: &Value) -> bool {
    if value.get("method").and_then(Value::as_str) != Some("account/rateLimits/updated") {
        return false;
    }
    let Some(params) = value.get("params") else {
        return false;
    };
    let Ok(params) = serde_json::from_value::<AccountRateLimitsUpdatedParams>(params.clone())
    else {
        return false;
    };
    let info = params.rate_limits.into_usage_info();
    auto_switch::usage_requires_switch(&info)
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

async fn login_request(request_id: Value, account: &StoredAccount) -> Result<Value> {
    let payload = external_auth_payload(account).await?;
    Ok(json!({
        "id": request_id,
        "method": "account/login/start",
        "params": {
            "type": "chatgptAuthTokens",
            "accessToken": payload.access_token,
            "chatgptAccountId": payload.chatgpt_account_id,
            "chatgptPlanType": payload.chatgpt_plan_type
        }
    }))
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

    let active_id = store.active_account_id.as_deref();
    store
        .accounts
        .into_iter()
        .find(|account| {
            Some(account.id.as_str()) == active_id
                && matches!(account.auth_data, AuthData::ChatGPT { .. })
        })
        .context("No active ChatGPT account available for auth refresh")
}

fn chatgpt_account_id(account: &StoredAccount) -> Option<String> {
    match &account.auth_data {
        AuthData::ChatGPT { account_id, .. } => account_id.clone(),
        AuthData::ApiKey { .. } => None,
    }
}

struct ExternalAuthPayload {
    access_token: String,
    chatgpt_account_id: String,
    chatgpt_plan_type: Option<String>,
}

async fn external_auth_payload(account: &StoredAccount) -> Result<ExternalAuthPayload> {
    let account = token::ensure_chatgpt_tokens_fresh(account).await?;
    match account.auth_data {
        AuthData::ChatGPT {
            access_token,
            account_id,
            ..
        } => Ok(ExternalAuthPayload {
            access_token,
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

fn is_jsonrpc_request(value: &Value) -> bool {
    value.get("method").is_some() && value.get("id").is_some()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::Path;
    use std::time::Duration;

    use serde_json::json;
    use tokio::sync::mpsc;

    use super::{
        ActiveAccountSnapshot, ActiveAccountWatchStatus, RuntimeCommand,
        codex_args_with_default_cwd, handle_active_account_snapshot_change, has_cwd_arg,
        initialize_app_server_request, random_duration_between,
        rate_limit_notification_requires_switch, usage_limit_error_requires_switch,
        validate_remote_capable_codex_args,
    };

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
    fn rate_limit_notification_triggers_when_hard_limit_is_reached() {
        let notification = json!({
            "method": "account/rateLimits/updated",
            "params": {
                "rateLimits": {
                    "limitId": "codex",
                    "limitName": null,
                    "primary": {
                        "usedPercent": 100,
                        "windowDurationMins": 300,
                        "resetsAt": 1_800_000_000
                    },
                    "secondary": null,
                    "credits": null,
                    "planType": "plus",
                    "rateLimitReachedType": null
                }
            }
        });

        assert!(rate_limit_notification_requires_switch(&notification));

        let available_notification = json!({
            "method": "account/rateLimits/updated",
            "params": {
                "rateLimits": {
                    "limitId": "codex",
                    "limitName": null,
                    "primary": {
                        "usedPercent": 96,
                        "windowDurationMins": 300,
                        "resetsAt": 1_800_000_000
                    },
                    "secondary": null,
                    "credits": null,
                    "planType": "plus",
                    "rateLimitReachedType": null
                }
            }
        });
        assert!(!rate_limit_notification_requires_switch(
            &available_notification
        ));
    }

    #[test]
    fn rate_limit_notification_triggers_when_limit_type_is_set() {
        let notification = json!({
            "method": "account/rateLimits/updated",
            "params": {
                "rateLimits": {
                    "limitId": "codex",
                    "limitName": null,
                    "primary": null,
                    "secondary": null,
                    "credits": null,
                    "planType": "plus",
                    "rateLimitReachedType": "workspace_owner_usage_limit_reached"
                }
            }
        });

        assert!(rate_limit_notification_requires_switch(&notification));
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
    fn active_account_watcher_advances_snapshot_only_after_queueing_sync() {
        let (sender, mut receiver) = mpsc::channel(1);
        let current_snapshot = active_snapshot("account-a");
        let mut last_snapshot = None;

        let status = handle_active_account_snapshot_change(
            &sender,
            &mut last_snapshot,
            current_snapshot.clone(),
        );

        assert_eq!(status, ActiveAccountWatchStatus::Queued);
        assert_eq!(last_snapshot, Some(current_snapshot));
        assert!(matches!(
            receiver.try_recv(),
            Ok(RuntimeCommand::SyncActiveAccount)
        ));
    }

    #[test]
    fn active_account_watcher_retries_when_sync_queue_is_full() {
        let (sender, _receiver) = mpsc::channel(1);
        sender
            .try_send(RuntimeCommand::SyncActiveAccount)
            .expect("test queue has capacity for first command");
        let current_snapshot = active_snapshot("account-a");
        let mut last_snapshot = None;

        let status = handle_active_account_snapshot_change(
            &sender,
            &mut last_snapshot,
            current_snapshot.clone(),
        );

        assert_eq!(status, ActiveAccountWatchStatus::Full);
        assert_eq!(last_snapshot, None);
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

    fn active_snapshot(account_id: &str) -> ActiveAccountSnapshot {
        ActiveAccountSnapshot {
            active_account_id: Some(account_id.to_string()),
            auth_marker: Some(format!("chatgpt:{account_id}:1:1")),
        }
    }
}
