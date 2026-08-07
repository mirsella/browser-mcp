use std::{
    fs::{self, File, OpenOptions},
    os::unix::{
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use rmcp::{
    ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
        ToolAnnotations,
    },
    service::RequestContext,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, copy},
    net::{UnixListener, UnixStream},
    sync::Notify,
    task::JoinSet,
    time::{Instant, interval, sleep, sleep_until, timeout},
};
use tracing::{error, info, warn};
use uuid::Uuid;

use super::{
    BrowserProxy, Config, MAX_SESSION_ID_BYTES, RoutingMode, SESSION_KEY, browser_tool,
    persistence::OwnershipPersistence, validate_config,
};

const SOCKET_NAME: &str = "daemon.sock";
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const LAST_CLIENT_GRACE: Duration = Duration::from_secs(2);
const DAEMON_TRANSITION_TIMEOUT: Duration = Duration::from_secs(360);
const START_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const HANDSHAKE_MAGIC: &[u8] = b"opencode-browser-mcp\0v2\n";
const MAX_IDENTITY_BYTES: usize = 8 * 1024;
const STOP_HANDSHAKE: u32 = u32::MAX;
const RELEASE_HANDSHAKE: u32 = u32::MAX - 1;
const OPENCODE_PREFIX: &str = "opencode:";
const MAX_OPENCODE_SESSION_ID_BYTES: usize = MAX_SESSION_ID_BYTES - OPENCODE_PREFIX.len();

pub(super) async fn run(config: Arc<Config>) -> Result<()> {
    let paths = RuntimePaths::new(config.socket.as_deref())?;
    if config.stop_daemon {
        return stop_daemon(&paths).await;
    }
    if let Some(session_id) = config.release_session.as_deref() {
        return release_session(&paths, session_id).await;
    }
    let identity = Arc::<[u8]>::from(config.daemon_identity()?);
    if config.daemon {
        validate_config(&config)?;
        return run_daemon(config, paths, identity).await;
    }
    bridge(paths, identity, config.routing).await
}

struct RuntimePaths {
    socket: PathBuf,
    lock: PathBuf,
    state: PathBuf,
    log: PathBuf,
}

impl RuntimePaths {
    fn new(override_path: Option<&Path>) -> Result<Self> {
        if let Some(socket) = override_path {
            validate_private_parent(socket)?;
            return Ok(Self::from_socket(socket.to_path_buf()));
        }
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("XDG_RUNTIME_DIR is required for the browser daemon socket"))?;
        validate_private_directory(&runtime)?;
        let directory = runtime.join("opencode-browser-mcp");
        fs::create_dir_all(&directory)
            .with_context(|| format!("failed to create {}", directory.display()))?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        Ok(Self::from_socket(directory.join(SOCKET_NAME)))
    }

    fn from_socket(socket: PathBuf) -> Self {
        let mut lock = socket.as_os_str().to_owned();
        lock.push(".lock");
        let mut state = socket.as_os_str().to_owned();
        state.push(".state.json");
        let mut log = socket.as_os_str().to_owned();
        log.push(".stderr.log");
        Self {
            socket,
            lock: PathBuf::from(lock),
            state: PathBuf::from(state),
            log: PathBuf::from(log),
        }
    }
}

fn validate_private_parent(socket: &Path) -> Result<()> {
    let parent = socket
        .parent()
        .ok_or_else(|| anyhow!("daemon socket has no parent directory"))?;
    validate_private_directory(parent)
}

fn validate_private_directory(directory: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(directory)
        .with_context(|| format!("failed to inspect {}", directory.display()))?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
        bail!(
            "daemon socket parent must be a user-private directory: {}",
            directory.display()
        );
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        bail!(
            "daemon socket parent is not owned by the current user: {}",
            directory.display()
        );
    }
    Ok(())
}

async fn bridge(paths: RuntimePaths, identity: Arc<[u8]>, routing: RoutingMode) -> Result<()> {
    let mut stream = match connect_or_absent(&paths.socket).await? {
        Some(stream) => stream,
        None => start_daemon_and_connect(&paths).await?,
    };
    client_handshake(&mut stream, &identity, routing).await?;
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let (mut socket_read, mut socket_write) = stream.into_split();
    let input = tokio::spawn(async move { copy(&mut stdin, &mut socket_write).await });
    copy(&mut socket_read, &mut stdout)
        .await
        .context("stdio bridge failed")?;
    stdout.flush().await.context("failed to flush stdio")?;
    // Once the daemon closes its half, retaining stdin would keep the bridge alive forever.
    input.abort();
    Ok(())
}

fn spawn_daemon(paths: &RuntimePaths) -> Result<()> {
    let executable = std::env::current_exe().context("failed to locate current executable")?;
    let args = std::env::args_os()
        .skip(1)
        .filter(|arg| arg != "--daemon" && arg != "--stop-daemon")
        .collect::<Vec<_>>();
    let mut command = Command::new(executable);
    let log = private_append_log(&paths.log)?;
    command
        .args(args)
        .arg("--daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().context("failed to spawn browser daemon")?;
    std::thread::Builder::new()
        .name("browser-daemon-reaper".to_owned())
        .spawn(move || {
            let _ = child.wait();
        })
        .context("failed to start browser daemon reaper")?;
    Ok(())
}

async fn start_daemon_and_connect(paths: &RuntimePaths) -> Result<UnixStream> {
    let deadline = Instant::now() + DAEMON_TRANSITION_TIMEOUT;
    let mut last_error = None;
    let mut next_start = Instant::now();
    while Instant::now() < deadline {
        match connect_or_absent(&paths.socket).await? {
            Some(stream) => return Ok(stream),
            None => last_error = Some("socket is absent or daemon refused the connection"),
        }
        if Instant::now() >= next_start && lease_is_available(&paths.lock)? {
            spawn_daemon(paths)?;
            next_start = Instant::now() + START_RETRY_INTERVAL;
        }
        sleep(Duration::from_millis(50)).await;
    }
    Err(anyhow!(
        "browser daemon did not become available at {} within {:?}: {}; inspect {}",
        paths.socket.display(),
        DAEMON_TRANSITION_TIMEOUT,
        last_error.unwrap_or("no connection attempt"),
        paths.log.display(),
    ))
}

async fn connect_or_absent(path: &Path) -> Result<Option<UnixStream>> {
    match UnixStream::connect(path).await {
        Ok(stream) => Ok(Some(stream)),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error).context("failed to connect to browser daemon"),
    }
}

async fn run_daemon(config: Arc<Config>, paths: RuntimePaths, identity: Arc<[u8]>) -> Result<()> {
    let _socket_lease = exclusive_lease(&paths.lock, "browser daemon socket")?;
    let _profile_lease = profile_lease(&config.profile)?;
    let persistence = OwnershipPersistence::open(paths.state.clone())?;
    let signal = shutdown_signal();
    tokio::pin!(signal);
    // Initialization owns a kill-on-drop browser child. Finish it rather than
    // cancelling midway and process any queued signal through normal shutdown.
    let proxy = BrowserProxy::new(config, persistence).await?;
    // Keep the socket unpublished until initialization succeeds so clients
    // cannot connect to a daemon that has not become ready yet.
    let listener = tokio::select! {
        biased;
        _ = &mut signal => {
            shutdown_proxy(&proxy).await;
            return Ok(());
        }
        listener = bind_socket(&paths) => listener,
    };
    let (listener, socket_guard) = match listener {
        Ok(listener) => listener,
        Err(error) => {
            shutdown_proxy(&proxy).await;
            return Err(error);
        }
    };
    info!(socket = %paths.socket.display(), "browser daemon ready");
    let cleanup_proxy = proxy.clone();
    let cleanup = tokio::spawn(async move {
        let mut timer = interval(cleanup_proxy.cleanup_cadence());
        loop {
            timer.tick().await;
            if let Err(error) = cleanup_proxy.cleanup_idle_sessions().await {
                warn!(%error, "idle browser-page cleanup failed");
            }
        }
    });
    let mut connections = JoinSet::new();
    let mut idle_deadline = Some(Instant::now() + LAST_CLIENT_GRACE);
    loop {
        tokio::select! {
            biased;
            _ = &mut signal => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    idle_deadline = None;
                    let proxy = proxy.clone();
                    let identity = identity.clone();
                    connections.spawn(async move { serve_connection(proxy, identity, stream).await });
                },
                Err(error) => { error!(%error, "Unix socket accept failed"); break; }
            },
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                match result {
                    Err(error) => warn!(%error, "connection task panicked"),
                    Ok(Err(error)) => warn!(%error, "browser daemon connection failed"),
                    Ok(Ok(true)) => break,
                    Ok(Ok(false)) => {}
                }
                if connections.is_empty() {
                    idle_deadline = Some(Instant::now() + LAST_CLIENT_GRACE);
                }
            },
            _ = sleep_until(idle_deadline.unwrap_or_else(Instant::now)), if idle_deadline.is_some() => {
                info!(grace = ?LAST_CLIENT_GRACE, "no MCP clients remain; stopping browser daemon");
                break;
            }
        }
    }
    drop(listener);
    drop(socket_guard);
    cleanup.abort();
    let _ = cleanup.await;
    let deadline = Instant::now() + DRAIN_TIMEOUT;
    while !connections.is_empty() && Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if timeout(remaining, connections.join_next()).await.is_err() {
            break;
        }
    }
    connections.abort_all();
    shutdown_proxy(&proxy).await;
    Ok(())
}

fn profile_lease(profile: &Path) -> Result<File> {
    let existed = profile.exists();
    fs::create_dir_all(profile)
        .with_context(|| format!("failed to create profile directory {}", profile.display()))?;
    if !existed {
        fs::set_permissions(profile, fs::Permissions::from_mode(0o700)).with_context(|| {
            format!("failed to restrict profile directory {}", profile.display())
        })?;
    }
    validate_private_directory(profile)?;
    let path = profile.join("opencode-browser-mcp.lock");
    exclusive_lease(&path, "browser profile")
}

fn exclusive_lease(path: &Path, purpose: &str) -> Result<File> {
    try_exclusive_lease(path)?
        .ok_or_else(|| anyhow!("another daemon owns {purpose} {}", path.display()))
}

fn try_exclusive_lease(path: &Path) -> Result<Option<File>> {
    let lock = open_private_file(path, PrivateFileOpen::Lock)?
        .expect("lock creation must return an open file");
    match lock.try_lock() {
        Ok(()) => Ok(Some(lock)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => {
            Err(error).with_context(|| format!("failed to lock {}", path.display()))
        }
    }
}

fn validate_owned_regular_file(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
    {
        bail!("refusing unsafe private file {}", path.display());
    }
    Ok(())
}

fn lease_is_available(path: &Path) -> Result<bool> {
    let Some(lock) = open_private_file(path, PrivateFileOpen::Existing)? else {
        return Ok(true);
    };
    match lock.try_lock() {
        Ok(()) => Ok(true),
        Err(std::fs::TryLockError::WouldBlock) => Ok(false),
        Err(std::fs::TryLockError::Error(error)) => {
            Err(error).with_context(|| format!("failed to inspect lease {}", path.display()))
        }
    }
}

fn private_append_log(path: &Path) -> Result<File> {
    Ok(open_private_file(path, PrivateFileOpen::Append)?
        .expect("append log creation must return an open file"))
}

enum PrivateFileOpen {
    Lock,
    Append,
    Existing,
}

fn open_private_file(path: &Path, kind: PrivateFileOpen) -> Result<Option<File>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_owned_regular_file(path, &metadata)?,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && matches!(kind, PrivateFileOpen::Existing) =>
        {
            return Ok(None);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    }
    let mut options = OpenOptions::new();
    match kind {
        PrivateFileOpen::Lock => {
            options.create(true).truncate(false).read(true).write(true);
        }
        PrivateFileOpen::Append => {
            options.create(true).append(true);
        }
        PrivateFileOpen::Existing => {
            options.read(true).write(true);
        }
    }
    let file = options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("failed to open private file {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect opened private file {}", path.display()))?;
    validate_owned_regular_file(path, &metadata)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to restrict private file {}", path.display()))?;
    Ok(Some(file))
}

async fn shutdown_proxy(proxy: &BrowserProxy) {
    if let Err(error) = proxy.shutdown().await {
        warn!(%error, "browser proxy shutdown was incomplete; recovery state was retained");
    }
}

struct SocketGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let Ok(metadata) = fs::symlink_metadata(&self.path) else {
            return;
        };
        if metadata.file_type().is_socket()
            && metadata.dev() == self.device
            && metadata.ino() == self.inode
            && let Err(error) = fs::remove_file(&self.path)
        {
            warn!(%error, socket = %self.path.display(), "failed to remove daemon socket");
        }
    }
}

async fn bind_socket(paths: &RuntimePaths) -> Result<(UnixListener, SocketGuard)> {
    match UnixListener::bind(&paths.socket) {
        Ok(listener) => set_socket_mode(&paths.socket, listener),
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            match connect_or_absent(&paths.socket).await? {
                Some(_) => bail!("a browser daemon already owns {}", paths.socket.display()),
                None => {
                    let metadata = fs::symlink_metadata(&paths.socket)
                        .context("failed to inspect existing daemon socket")?;
                    if !metadata.file_type().is_socket()
                        || metadata.uid() != unsafe { libc::geteuid() }
                    {
                        bail!(
                            "refusing to remove unsafe stale socket {}",
                            paths.socket.display()
                        );
                    }
                    fs::remove_file(&paths.socket)
                        .context("failed to remove stale daemon socket")?;
                    set_socket_mode(
                        &paths.socket,
                        UnixListener::bind(&paths.socket)
                            .context("failed to rebind daemon socket")?,
                    )
                }
            }
        }
        Err(error) => Err(error).context("failed to bind daemon socket"),
    }
}

fn set_socket_mode(path: &Path, listener: UnixListener) -> Result<(UnixListener, SocketGuard)> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to restrict {}", path.display()))?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect bound socket {}", path.display()))?;
    Ok((
        listener,
        SocketGuard {
            path: path.to_path_buf(),
            device: metadata.dev(),
            inode: metadata.ino(),
        },
    ))
}

fn peer_pid(stream: &UnixStream) -> Result<libc::pid_t> {
    let credentials = stream
        .peer_cred()
        .context("failed to read peer credentials")?;
    if credentials.uid() != unsafe { libc::geteuid() } {
        bail!("peer UID does not match daemon UID");
    }
    credentials
        .pid()
        .ok_or_else(|| anyhow!("peer did not provide a PID"))
}

fn peer_workspace(pid: libc::pid_t) -> Result<PathBuf> {
    let workspace = fs::read_link(format!("/proc/{pid}/cwd"))
        .with_context(|| format!("failed to resolve workspace for peer {pid}"))?;
    if !workspace.is_dir() {
        bail!("peer workspace is not a directory: {}", workspace.display());
    }
    Ok(workspace)
}

#[derive(Clone)]
struct Connection {
    proxy: BrowserProxy,
    workspace: PathBuf,
    routing: ConnectionRouting,
    lifecycle: Option<Arc<ConnectionLifecycle>>,
}

#[derive(Default)]
struct ConnectionLifecycle {
    closed: AtomicBool,
    active: AtomicUsize,
    idle: Notify,
}

impl ConnectionLifecycle {
    fn enter(self: &Arc<Self>) -> Option<ConnectionLease> {
        if self.closed.load(Ordering::SeqCst) {
            return None;
        }
        self.active.fetch_add(1, Ordering::SeqCst);
        if self.closed.load(Ordering::SeqCst) {
            self.leave();
            return None;
        }
        Some(ConnectionLease(self.clone()))
    }

    async fn close_and_wait(&self) {
        self.closed.store(true, Ordering::SeqCst);
        while self.active.load(Ordering::SeqCst) != 0 {
            self.idle.notified().await;
        }
    }

    fn leave(&self) {
        if self.active.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.idle.notify_one();
        }
    }
}

struct ConnectionLease(Arc<ConnectionLifecycle>);

impl Drop for ConnectionLease {
    fn drop(&mut self) {
        self.0.leave();
    }
}

#[derive(Clone)]
enum ConnectionRouting {
    Connection(String),
    Opencode { compact: bool },
}

impl ConnectionRouting {
    fn compact(&self) -> bool {
        matches!(self, Self::Opencode { compact: true })
    }

    fn route(&self, mut request: CallToolRequestParams) -> Result<(String, CallToolRequestParams)> {
        let arguments = request.arguments.get_or_insert_default();
        let id = match self {
            Self::Connection(id) => {
                if arguments.remove(SESSION_KEY).is_some() {
                    bail!("generic MCP clients cannot set the reserved OpenCode routing field");
                }
                id.clone()
            }
            Self::Opencode { .. } => arguments
                .remove(SESSION_KEY)
                .and_then(|value| value.as_str().map(str::to_owned))
                .filter(|id| !id.is_empty() && id.len() <= MAX_OPENCODE_SESSION_ID_BYTES)
                .ok_or_else(|| anyhow!("browser call is missing the OpenCode session identifier"))
                .map(|id| format!("{OPENCODE_PREFIX}{id}"))?,
        };
        Ok((id, request))
    }
}

fn compact_tools() -> Vec<Tool> {
    let dispatch_schema = json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "minLength": 1,
                "description": "Browser action name. Use browser_help when its arguments are unknown."
            },
            "arguments": {
                "type": "object",
                "description": "Arguments for the action. Omit when none are needed."
            }
        },
        "required": ["action"],
        "additionalProperties": false
    })
    .as_object()
    .expect("dispatch schema is an object")
    .clone();
    let help_schema = json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "minLength": 1,
                "description": "Action to document. Omit to list action names."
            }
        },
        "additionalProperties": false
    })
    .as_object()
    .expect("help schema is an object")
    .clone();
    vec![
        Tool::new(
            "dispatch",
            "Execute a browser action. Use browser_help first when its arguments are unknown.",
            dispatch_schema,
        )
        .with_annotations(
            ToolAnnotations::new()
                .read_only(false)
                .destructive(true)
                .idempotent(false)
                .open_world(true),
        ),
        Tool::new(
            "help",
            "List browser actions or load the current schema for one action.",
            help_schema,
        )
        .with_annotations(
            ToolAnnotations::new()
                .read_only(true)
                .destructive(false)
                .idempotent(true)
                .open_world(false),
        ),
    ]
}

fn only_fields(arguments: &serde_json::Map<String, Value>, allowed: &[&str]) -> Result<()> {
    if let Some(field) = arguments
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        bail!("unexpected field {field:?}");
    }
    Ok(())
}

fn expand_dispatch(
    mut request: CallToolRequestParams,
    tools: &[Tool],
) -> Result<CallToolRequestParams> {
    let mut envelope = request.arguments.take().unwrap_or_default();
    only_fields(&envelope, &["action", "arguments"])?;
    let action = envelope
        .remove("action")
        .and_then(|value| value.as_str().map(str::to_owned))
        .filter(|action| !action.is_empty())
        .ok_or_else(|| anyhow!("browser_dispatch requires a non-empty string action"))?;
    if browser_tool(tools, &action).is_none() {
        bail!("unknown browser action {action:?}; call browser_help to list available actions");
    }
    let arguments = match envelope.remove("arguments") {
        Some(Value::Object(arguments)) => arguments,
        Some(_) => bail!("browser_dispatch arguments must be an object"),
        None => Default::default(),
    };
    if arguments.contains_key(SESSION_KEY) {
        bail!("browser action arguments contain a reserved routing field");
    }
    request.name = action.into();
    request.arguments = Some(arguments);
    Ok(request)
}

fn compact_help(request: CallToolRequestParams, tools: &[Tool]) -> Result<CallToolResult> {
    let mut arguments = request.arguments.unwrap_or_default();
    only_fields(&arguments, &["action"])?;
    let action = match arguments.remove("action") {
        Some(Value::String(action)) if !action.is_empty() => Some(action),
        Some(_) => bail!("browser_help action must be a non-empty string"),
        None => None,
    };
    if let Some(action) = action {
        let tool = browser_tool(tools, &action)
            .ok_or_else(|| anyhow!("unknown browser action {action:?}; call browser_help without an action to list available actions"))?;
        let text = serde_json::to_string_pretty(tool)
            .context("failed to format browser action definition")?;
        return Ok(CallToolResult::success(vec![ContentBlock::text(text)]));
    }
    let mut actions = tools
        .iter()
        .map(|tool| tool.name.to_string())
        .collect::<Vec<_>>();
    actions.sort_unstable();
    let text = format!(
        "Available browser actions: {}\nUse browser_help with an action to load its schema.",
        actions.join(", ")
    );
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

impl ServerHandler for Connection {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_instructions(
                "Controls a shared Chromium-family browser with connection-isolated pages."
                    .to_owned(),
            )
    }
    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        let _ = (request, context);
        let tools = if self.routing.compact() {
            compact_tools()
        } else {
            self.proxy.tools.as_ref().clone()
        };
        Ok(ListToolsResult::with_all_items(tools))
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        if self.routing.compact() && request.name != "dispatch" && request.name != "help" {
            return Err(rmcp::ErrorData::invalid_params(
                "tool not found; use browser_dispatch or browser_help",
                None,
            ));
        }
        let _lease = match &self.lifecycle {
            Some(lifecycle) => match lifecycle.enter() {
                Some(lease) => Some(lease),
                None => {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(
                        "browser connection is closed",
                    )])
                    .into());
                }
            },
            None => None,
        };
        let result = match self.routing.route(request) {
            Ok((id, request)) => match (self.routing.compact(), request.name.as_ref()) {
                (false, _) => self.proxy.route_call(&self.workspace, &id, request).await,
                (true, "dispatch") => match expand_dispatch(request, &self.proxy.tools) {
                    Ok(request) => self.proxy.route_call(&self.workspace, &id, request).await,
                    Err(error) => Err(error),
                },
                (true, "help") => compact_help(request, &self.proxy.tools),
                (true, _) => unreachable!("compact tool name checked above"),
            },
            Err(error) => Err(error),
        };
        match result {
            Ok(result) => Ok(result.into()),
            Err(error) => {
                error!(%error, workspace = %self.workspace.display(), "browser tool call failed");
                Ok(CallToolResult::error(vec![ContentBlock::text(error.to_string())]).into())
            }
        }
    }
}

async fn serve_connection(
    proxy: BrowserProxy,
    identity: Arc<[u8]>,
    stream: UnixStream,
) -> Result<bool> {
    let mut stream = stream;
    let pid = peer_pid(&stream)?;
    let routing = match server_handshake(&mut stream, &identity).await? {
        HandshakeRequest::Identity(_, RoutingMode::Connection) => {
            ConnectionRouting::Connection(format!("connection:{}", Uuid::new_v4()))
        }
        HandshakeRequest::Identity(_, RoutingMode::OpencodeDirect) => {
            ConnectionRouting::Opencode { compact: false }
        }
        HandshakeRequest::Identity(_, RoutingMode::Opencode) => {
            ConnectionRouting::Opencode { compact: true }
        }
        HandshakeRequest::Stop => return Ok(true),
        HandshakeRequest::Release(session_id) => {
            let result = proxy
                .release_session(&format!("{OPENCODE_PREFIX}{session_id}"))
                .await;
            stream.write_u8(u8::from(result.is_ok())).await?;
            stream.flush().await?;
            result?;
            return Ok(false);
        }
    };
    let workspace = peer_workspace(pid)?;
    let (lifecycle, release) = match &routing {
        ConnectionRouting::Connection(id) => (
            Some(Arc::new(ConnectionLifecycle::default())),
            Some(id.clone()),
        ),
        ConnectionRouting::Opencode { .. } => (None, None),
    };
    let service = Connection {
        proxy: proxy.clone(),
        workspace,
        routing,
        lifecycle: lifecycle.clone(),
    }
    .serve(stream)
    .await
    .context("failed to initialize MCP connection")?;
    let result = service.waiting().await.context("MCP connection failed");
    if let (Some(id), Some(lifecycle)) = (release, lifecycle) {
        lifecycle.close_and_wait().await;
        if let Err(error) = proxy.release_session(&id).await {
            warn!(%error, session_id = %id, "failed to release disconnected MCP client tabs");
        }
    }
    result?;
    Ok(false)
}

async fn client_handshake(
    stream: &mut UnixStream,
    identity: &[u8],
    routing: RoutingMode,
) -> Result<()> {
    timeout(HANDSHAKE_TIMEOUT, async {
        write_handshake(
            stream,
            &HandshakeRequest::Identity(identity.to_vec(), routing),
        )
        .await?;
        match stream.read_u8().await? {
            1 => Ok(()),
            _ => bail!("existing browser daemon uses incompatible configuration"),
        }
    })
    .await
    .context("timed out negotiating with browser daemon")?
}

async fn server_handshake(stream: &mut UnixStream, expected: &[u8]) -> Result<HandshakeRequest> {
    timeout(HANDSHAKE_TIMEOUT, async {
        let request = read_handshake(stream).await?;
        match &request {
            HandshakeRequest::Identity(identity, _) => {
                let matches = identity == expected;
                stream.write_u8(u8::from(matches)).await?;
                stream.flush().await?;
                if !matches {
                    bail!("client browser configuration does not match the running daemon");
                }
            }
            HandshakeRequest::Stop => {
                stream.write_u8(1).await?;
                stream.flush().await?;
            }
            HandshakeRequest::Release(_) => {}
        }
        Ok(request)
    })
    .await
    .context("timed out negotiating browser daemon connection")?
}

enum HandshakeRequest {
    Identity(Vec<u8>, RoutingMode),
    Stop,
    Release(String),
}

async fn write_handshake(stream: &mut UnixStream, request: &HandshakeRequest) -> Result<()> {
    stream.write_all(HANDSHAKE_MAGIC).await?;
    match request {
        HandshakeRequest::Identity(identity, routing) => {
            if identity.len() > MAX_IDENTITY_BYTES {
                bail!("browser daemon configuration identity is too large");
            }
            stream.write_u32(identity.len() as u32).await?;
            stream.write_all(identity).await?;
            stream
                .write_u8(match routing {
                    RoutingMode::Connection => 0,
                    RoutingMode::OpencodeDirect => 1,
                    RoutingMode::Opencode => 2,
                })
                .await?;
        }
        HandshakeRequest::Stop => stream.write_u32(STOP_HANDSHAKE).await?,
        HandshakeRequest::Release(session_id) => {
            let bytes = session_id.as_bytes();
            if bytes.is_empty() || bytes.len() > MAX_OPENCODE_SESSION_ID_BYTES {
                bail!("invalid browser session identifier");
            }
            stream.write_u32(RELEASE_HANDSHAKE).await?;
            stream.write_u32(bytes.len() as u32).await?;
            stream.write_all(bytes).await?;
        }
    }
    stream.flush().await?;
    Ok(())
}

async fn read_handshake(stream: &mut UnixStream) -> Result<HandshakeRequest> {
    let mut magic = [0; HANDSHAKE_MAGIC.len()];
    stream.read_exact(&mut magic).await?;
    if magic != HANDSHAKE_MAGIC {
        bail!("invalid browser daemon handshake");
    }
    let length = stream.read_u32().await?;
    if length == STOP_HANDSHAKE {
        return Ok(HandshakeRequest::Stop);
    }
    if length == RELEASE_HANDSHAKE {
        let length = stream.read_u32().await? as usize;
        if length == 0 || length > MAX_OPENCODE_SESSION_ID_BYTES {
            bail!("invalid browser session identifier length");
        }
        let mut session_id = vec![0; length];
        stream.read_exact(&mut session_id).await?;
        return Ok(HandshakeRequest::Release(
            String::from_utf8(session_id).context("browser session identifier is not UTF-8")?,
        ));
    }
    let length = length as usize;
    if length > MAX_IDENTITY_BYTES {
        bail!("browser daemon configuration identity is too large");
    }
    let mut identity = vec![0; length];
    stream.read_exact(&mut identity).await?;
    let routing = match stream.read_u8().await? {
        0 => RoutingMode::Connection,
        1 => RoutingMode::OpencodeDirect,
        2 => RoutingMode::Opencode,
        _ => bail!("invalid browser connection mode"),
    };
    Ok(HandshakeRequest::Identity(identity, routing))
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        let mut interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .expect("install SIGINT handler");
        tokio::select! { _ = term.recv() => {}, _ = interrupt.recv() => {} }
    }
}

async fn stop_daemon(paths: &RuntimePaths) -> Result<()> {
    let deadline = Instant::now() + DAEMON_TRANSITION_TIMEOUT;
    if !send_control(paths, &HandshakeRequest::Stop, "stop").await? {
        return Ok(());
    }
    while !lease_is_available(&paths.lock)? {
        if Instant::now() >= deadline {
            bail!("browser daemon did not stop within {DAEMON_TRANSITION_TIMEOUT:?}");
        }
        sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

async fn release_session(paths: &RuntimePaths, session_id: &str) -> Result<()> {
    if session_id.is_empty() || session_id.len() > MAX_OPENCODE_SESSION_ID_BYTES {
        bail!("invalid browser session identifier");
    }
    let request = HandshakeRequest::Release(session_id.to_owned());
    let lease = loop {
        if send_control(paths, &request, "release session").await? {
            return Ok(());
        }
        match try_exclusive_lease(&paths.lock)? {
            Some(lease) => break lease,
            None => continue,
        }
    };
    if connect_or_absent(&paths.socket).await?.is_some() {
        drop(lease);
        if send_control(paths, &request, "release session").await? {
            return Ok(());
        }
        bail!("browser daemon disappeared while releasing the session");
    }
    if paths.state.exists()
        && OwnershipPersistence::open(paths.state.clone())?
            .request_cleanup(&format!("{OPENCODE_PREFIX}{session_id}"))?
    {
        return Ok(());
    }
    bail!("browser daemon is not running and no persisted session could be released")
}

async fn send_control(
    paths: &RuntimePaths,
    request: &HandshakeRequest,
    action: &str,
) -> Result<bool> {
    let deadline = Instant::now() + DAEMON_TRANSITION_TIMEOUT;
    loop {
        if let Some(mut stream) = connect_or_absent(&paths.socket).await? {
            let credentials = stream
                .peer_cred()
                .context("failed to read browser daemon credentials")?;
            if credentials.uid() != unsafe { libc::geteuid() } {
                bail!("refusing to control daemon owned by another user");
            }
            timeout(DAEMON_TRANSITION_TIMEOUT, async {
                write_handshake(&mut stream, request).await?;
                if stream.read_u8().await? != 1 {
                    bail!("browser daemon rejected {action} control request");
                }
                Ok(())
            })
            .await
            .with_context(|| format!("timed out waiting for browser daemon to {action}"))??;
            return Ok(true);
        }
        if lease_is_available(&paths.lock)? {
            return Ok(false);
        }
        if Instant::now() >= deadline {
            bail!(
                "browser daemon did not become controllable within {DAEMON_TRANSITION_TIMEOUT:?}"
            );
        }
        sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn runtime_paths(name: &str) -> RuntimePaths {
        let directory = std::env::temp_dir().join(format!(
            "opencode-browser-daemon-{}-{name}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        RuntimePaths::from_socket(directory.join("daemon.sock"))
    }

    #[test]
    fn generic_connections_use_their_connection_session() {
        let routing = ConnectionRouting::Connection("connection-a".into());
        let (id, request) = routing
            .route(CallToolRequestParams::new("list_pages"))
            .expect("route generic request");
        assert_eq!(id, "connection-a");
        assert!(
            request
                .arguments
                .is_none_or(|arguments| !arguments.contains_key(SESSION_KEY))
        );
    }

    #[test]
    fn generic_connections_reject_opencode_routing_data() {
        let routing = ConnectionRouting::Connection("connection-a".into());
        let request = CallToolRequestParams::new("list_pages").with_arguments(
            json!({(SESSION_KEY): "forged"})
                .as_object()
                .expect("request arguments")
                .clone(),
        );
        assert!(routing.route(request).is_err());
    }

    #[test]
    fn opencode_connections_require_and_remove_the_session_id() {
        let request = CallToolRequestParams::new("list_pages").with_arguments(
            json!({(SESSION_KEY): "session-a"})
                .as_object()
                .expect("request arguments")
                .clone(),
        );
        let routing = ConnectionRouting::Opencode { compact: true };
        let (id, request) = routing.route(request).expect("route OpenCode request");
        assert_eq!(id, "opencode:session-a");
        assert!(
            request
                .arguments
                .is_some_and(|arguments| !arguments.contains_key(SESSION_KEY))
        );
        assert!(
            routing
                .route(CallToolRequestParams::new("list_pages"))
                .is_err()
        );
    }

    #[test]
    fn compact_interface_validates_and_expands_dispatch() {
        let tools = [Tool::new(
            "navigate_page",
            "Navigate",
            json!({"type": "object", "properties": {"url": {"type": "string"}}})
                .as_object()
                .unwrap()
                .clone(),
        )];
        assert_eq!(
            compact_tools()
                .iter()
                .map(|tool| tool.name.as_ref())
                .collect::<Vec<_>>(),
            ["dispatch", "help"]
        );
        let request = CallToolRequestParams::new("dispatch")
            .with_arguments(
                json!({"action": "navigate_page", "arguments": {"url": "https://example.com"}})
                    .as_object()
                    .unwrap()
                    .clone(),
            )
            .with_request_state("state-a");
        let expanded = expand_dispatch(request, &tools).unwrap();
        assert_eq!(expanded.name, "navigate_page");
        assert_eq!(expanded.arguments.unwrap()["url"], "https://example.com");
        assert_eq!(expanded.request_state.as_deref(), Some("state-a"));
        for arguments in [
            json!({}),
            json!({"action": "missing"}),
            json!({"action": "navigate_page", "arguments": []}),
            json!({"action": "navigate_page", "extra": true}),
            json!({"action": "navigate_page", "arguments": {(SESSION_KEY): "forged"}}),
        ] {
            let request = CallToolRequestParams::new("dispatch")
                .with_arguments(arguments.as_object().unwrap().clone());
            assert!(expand_dispatch(request, &tools).is_err());
        }
    }

    #[test]
    fn compact_help_lists_and_loads_tool_definitions() {
        let tools = [Tool::new(
            "navigate_page",
            "Navigate",
            json!({"type": "object", "properties": {"url": {"type": "string"}}})
                .as_object()
                .unwrap()
                .clone(),
        )];
        let listed = compact_help(CallToolRequestParams::new("help"), &tools).unwrap();
        assert!(
            listed.content[0]
                .as_text()
                .unwrap()
                .text
                .contains("navigate_page")
        );
        let detailed = compact_help(
            CallToolRequestParams::new("help").with_arguments(
                json!({"action": "navigate_page"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            &tools,
        )
        .unwrap();
        let definition: Value = serde_json::from_str(&detailed.content[0].as_text().unwrap().text)
            .expect("browser tool definition");
        assert_eq!(definition["name"], "navigate_page");
        assert_eq!(
            definition["inputSchema"]["properties"]["url"]["type"],
            "string"
        );
    }

    #[tokio::test]
    async fn closing_a_connection_waits_for_active_calls_and_rejects_new_ones() {
        let lifecycle = Arc::new(ConnectionLifecycle::default());
        let lease = lifecycle.enter().expect("enter open connection");
        let closing = tokio::spawn({
            let lifecycle = lifecycle.clone();
            async move { lifecycle.close_and_wait().await }
        });
        tokio::task::yield_now().await;
        assert!(lifecycle.enter().is_none());
        assert!(!closing.is_finished());
        drop(lease);
        timeout(Duration::from_secs(1), closing)
            .await
            .expect("connection close timed out")
            .expect("connection close task failed");
    }

    #[tokio::test]
    async fn offline_releases_merge_cleanup_intent() {
        let paths = runtime_paths("offline-release");
        let persistence = OwnershipPersistence::open(paths.state.clone()).unwrap();
        persistence.activate_generation("generation").unwrap();
        for id in ["opencode:session-a", "opencode:session-b"] {
            persistence
                .update_session(
                    id,
                    super::super::persistence::PersistedSession {
                        workspace: PathBuf::from("/workspace"),
                        current_tab: None,
                        last_activity_unix_seconds: 1,
                        cleanup_pending: false,
                    },
                )
                .unwrap();
        }
        drop(persistence);

        let (first, second) = tokio::join!(
            release_session(&paths, "session-a"),
            release_session(&paths, "session-b")
        );
        first.unwrap();
        second.unwrap();
        let restored = OwnershipPersistence::open(paths.state.clone())
            .unwrap()
            .activate_generation("generation")
            .unwrap();
        assert!(
            restored
                .sessions
                .values()
                .all(|session| session.cleanup_pending)
        );
        fs::remove_dir_all(paths.socket.parent().unwrap()).unwrap();
    }
}
