use std::{
    fs::{self, File, OpenOptions},
    os::unix::ffi::OsStrExt,
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use super::{
    BrowserProxy, Cli, Config, MAX_SESSION_ID_BYTES, ProcessMode, SESSION_KEY, browser_tool,
    persistence::OwnershipPersistence, validate_config,
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
    sync::{Notify, watch},
    task::JoinSet,
    time::{Instant, interval, sleep, timeout},
};
use tracing::{error, info, warn};
use xxhash_rust::xxh3::xxh3_64;

const STATE_FILE: &str = "opencode-browser-mcp.state.json";
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const BROKER_START_TIMEOUT: Duration = Duration::from_secs(30);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const MANAGED_IDLE_GRACE: Duration = Duration::from_secs(2);
const RELEASE_CONTROL_TIMEOUT: Duration = Duration::from_secs(3900);
const HANDSHAKE_MAGIC: &[u8] = b"opencode-browser-mcp\0v5\n";
const MANAGED_CONFIG_ENV: &str = "OPENCODE_BROWSER_INTERNAL_CONFIG";
const CONNECT_HANDSHAKE: u8 = 0;
const RELEASE_HANDSHAKE: u8 = 1;
const MAX_CONFIG_BYTES: usize = 16 * 1024;
const OPENCODE_PREFIX: &str = "opencode:";
const MAX_OPENCODE_SESSION_ID_BYTES: usize = MAX_SESSION_ID_BYTES - OPENCODE_PREFIX.len();

pub(super) async fn run(cli: Cli) -> Result<()> {
    let Cli {
        direct_tools,
        profile_override,
        mut config,
        command,
    } = cli;
    match command {
        Some(ProcessMode::ManagedBroker) => {
            let encoded = std::env::var(MANAGED_CONFIG_ENV)
                .context("managed browser broker configuration is missing")?;
            let config: Config = serde_json::from_str(&encoded)
                .context("managed browser broker configuration is invalid")?;
            validate_config(&config)?;
            let paths = RuntimePaths::new(&config)?;
            paths.prepare_parent()?;
            run_managed_broker(Arc::new(config), paths).await
        }
        Some(ProcessMode::ReleaseSession { session_id }) => {
            config.resolve(profile_override).await?;
            let paths = RuntimePaths::new(&config)?;
            release_session(&paths, &config, &session_id).await
        }
        None => {
            config.resolve(profile_override).await?;
            let paths = RuntimePaths::new(&config)?;
            bridge(paths, direct_tools, config).await
        }
    }
}

struct RuntimePaths {
    socket: PathBuf,
    lock: PathBuf,
}

impl RuntimePaths {
    fn new(config: &Config) -> Result<Self> {
        let uid = unsafe { libc::geteuid() };
        let user_runtime = PathBuf::from(format!("/run/user/{uid}"));
        let directory = if user_runtime.exists() {
            validate_private_directory(&user_runtime)?;
            user_runtime.join("opencode-browser-mcp")
        } else {
            PathBuf::from("/tmp").join(format!("opencode-browser-mcp-{uid}"))
        };
        if directory.exists() {
            validate_private_directory(&directory)?;
        }
        let profile_id = xxh3_64(config.profile.as_os_str().as_bytes());
        Ok(Self::from_socket(
            directory.join(format!("broker-{profile_id:016x}.sock")),
        ))
    }

    fn prepare_parent(&self) -> Result<()> {
        let parent = self
            .socket
            .parent()
            .ok_or_else(|| anyhow!("broker socket has no parent directory"))?;
        if !parent.exists() {
            match fs::DirBuilder::new().mode(0o700).create(parent) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to create {}", parent.display()));
                }
            }
        }
        validate_private_directory(parent)
    }

    fn from_socket(socket: PathBuf) -> Self {
        let mut lock = socket.as_os_str().to_owned();
        lock.push(".lock");
        Self {
            socket,
            lock: PathBuf::from(lock),
        }
    }
}

fn validate_private_directory(directory: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(directory)
        .with_context(|| format!("failed to inspect {}", directory.display()))?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
        bail!(
            "broker socket parent must be a user-private directory: {}",
            directory.display()
        );
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        bail!(
            "broker socket parent is not owned by the current user: {}",
            directory.display()
        );
    }
    Ok(())
}

async fn bridge(paths: RuntimePaths, direct_tools: bool, config: Config) -> Result<()> {
    let mut stream = connect_or_start(&paths, &config).await?;
    validate_broker_peer(&stream)?;
    client_handshake(&mut stream, direct_tools, config).await?;
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let (mut socket_read, mut socket_write) = stream.into_split();
    let input = tokio::spawn(async move { copy(&mut stdin, &mut socket_write).await });
    copy(&mut socket_read, &mut stdout)
        .await
        .context("stdio bridge failed")?;
    stdout.flush().await.context("failed to flush stdio")?;
    // Once the broker closes its half, retaining stdin would keep the adapter alive forever.
    input.abort();
    Ok(())
}

async fn connect_or_start(paths: &RuntimePaths, config: &Config) -> Result<UnixStream> {
    let deadline = Instant::now() + BROKER_START_TIMEOUT;
    let mut child: Option<tokio::process::Child> = None;
    loop {
        if let Some(stream) = connect_or_absent(&paths.socket).await? {
            if let Some(mut child) = child.take() {
                tokio::spawn(async move {
                    if let Err(error) = child.wait().await {
                        warn!(%error, "failed to reap managed browser broker contender");
                    }
                });
            }
            return Ok(stream);
        }
        if lease_is_available(&paths.lock)? && child.is_none() {
            paths.prepare_parent()?;
            child = Some(spawn_managed_broker(config)?);
        }
        if let Some(contender) = child.as_mut()
            && let Some(status) = contender
                .try_wait()
                .context("failed to inspect managed browser broker contender")?
        {
            if !status.success() {
                bail!("managed browser broker exited during startup with {status}");
            }
            child = None;
        }
        if Instant::now() >= deadline {
            if let Some(mut child) = child
                && child.try_wait()?.is_none()
            {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
            bail!("managed browser broker did not start within {BROKER_START_TIMEOUT:?}");
        }
        sleep(Duration::from_millis(50)).await;
    }
}

fn spawn_managed_broker(config: &Config) -> Result<tokio::process::Child> {
    let executable = std::env::current_exe().context("failed to locate the browser MCP binary")?;
    let encoded = serde_json::to_string(config).context("failed to encode broker configuration")?;
    let mut command = tokio::process::Command::new(executable);
    command
        .arg("managed-broker")
        .env(MANAGED_CONFIG_ENV, encoded)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(false);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
        .spawn()
        .context("failed to start the managed browser broker")
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
        Err(error) => Err(error).context("failed to connect to browser broker"),
    }
}

async fn run_managed_broker(config: Arc<Config>, paths: RuntimePaths) -> Result<()> {
    let Some(socket_lease) = try_exclusive_lease(&paths.lock)? else {
        // Concurrent adapters can race to start the broker. The winner owns the
        // lease; every adapter independently waits for its socket.
        return Ok(());
    };
    run_broker(config, paths, socket_lease).await
}

async fn run_broker(config: Arc<Config>, paths: RuntimePaths, _socket_lease: File) -> Result<()> {
    let _profile_lease = profile_lease(&config.profile)?;
    let persistence = OwnershipPersistence::open(config.profile.join(STATE_FILE))?;
    let signal = shutdown_signal();
    tokio::pin!(signal);
    let proxy = BrowserProxy::new(config.clone(), persistence)?;
    // Keep the socket unpublished until initialization succeeds so clients
    // cannot connect to a broker that has not become ready yet.
    let listener = tokio::select! {
        biased;
        _ = &mut signal => {
            return shutdown_proxy(&proxy).await;
        }
        listener = bind_socket(&paths) => listener,
    };
    let (listener, socket_guard) = match listener {
        Ok(listener) => listener,
        Err(error) => {
            if let Err(shutdown_error) = shutdown_proxy(&proxy).await {
                warn!(%shutdown_error, "browser proxy shutdown after socket bind failure was incomplete");
            }
            return Err(error);
        }
    };
    info!(socket = %paths.socket.display(), profile = %config.profile.display(), "browser broker ready");
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
    let (shutdown, _) = watch::channel(false);
    let mut active_connections = 0usize;
    let idle = sleep(BROKER_START_TIMEOUT);
    tokio::pin!(idle);
    let exit = loop {
        tokio::select! {
            biased;
            _ = &mut signal => break Ok(()),
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    active_connections += 1;
                    let proxy = proxy.clone();
                    let shutdown = shutdown.subscribe();
                    let config = config.clone();
                    connections.spawn(async move {
                        serve_connection(proxy, stream, shutdown, config).await
                    });
                },
                Err(error) => break Err(error).context("Unix socket accept failed"),
            },
            Some(result) = connections.join_next(), if active_connections != 0 => {
                active_connections -= 1;
                match result {
                    Err(error) => warn!(%error, "connection task panicked"),
                    Ok(Err(error)) => warn!(%error, "browser broker connection failed"),
                    Ok(Ok(())) => {}
                }
                if active_connections == 0 {
                    idle.as_mut().reset(Instant::now() + MANAGED_IDLE_GRACE);
                }
            }
            _ = &mut idle, if active_connections == 0 => break Ok(()),
        }
    };
    drop(listener);
    drop(socket_guard);
    let _ = shutdown.send(true);
    let deadline = Instant::now() + DRAIN_TIMEOUT;
    while !connections.is_empty() && Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if timeout(remaining, connections.join_next()).await.is_err() {
            break;
        }
    }
    connections.abort_all();
    proxy.catalog.cancel();
    cleanup.abort();
    let _ = cleanup.await;
    exit.and(shutdown_proxy(&proxy).await)
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
        .ok_or_else(|| anyhow!("another browser broker owns {purpose} {}", path.display()))
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
        || metadata.permissions().mode() & 0o077 != 0
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

enum PrivateFileOpen {
    Lock,
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
        PrivateFileOpen::Existing => {
            options.read(true);
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
    Ok(Some(file))
}

async fn shutdown_proxy(proxy: &BrowserProxy) -> Result<()> {
    proxy
        .shutdown()
        .await
        .context("browser proxy shutdown was incomplete; recovery state was retained")
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
            warn!(%error, socket = %self.path.display(), "failed to remove broker socket");
        }
    }
}

async fn bind_socket(paths: &RuntimePaths) -> Result<(UnixListener, SocketGuard)> {
    match UnixListener::bind(&paths.socket) {
        Ok(listener) => set_socket_mode(&paths.socket, listener),
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            match connect_or_absent(&paths.socket).await? {
                Some(_) => bail!("a browser broker already owns {}", paths.socket.display()),
                None => {
                    let metadata = fs::symlink_metadata(&paths.socket)
                        .context("failed to inspect existing broker socket")?;
                    if !metadata.file_type().is_socket()
                        || metadata.uid() != unsafe { libc::geteuid() }
                    {
                        bail!(
                            "refusing to remove unsafe stale broker socket {}",
                            paths.socket.display()
                        );
                    }
                    fs::remove_file(&paths.socket)
                        .context("failed to remove stale broker socket")?;
                    set_socket_mode(
                        &paths.socket,
                        UnixListener::bind(&paths.socket)
                            .context("failed to rebind broker socket")?,
                    )
                }
            }
        }
        Err(error) => Err(error).context("failed to bind broker socket"),
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
        bail!("peer UID does not match broker UID");
    }
    credentials
        .pid()
        .ok_or_else(|| anyhow!("peer did not provide a PID"))
}

fn validate_broker_peer(stream: &UnixStream) -> Result<()> {
    let credentials = stream
        .peer_cred()
        .context("failed to read browser broker credentials")?;
    if credentials.uid() != unsafe { libc::geteuid() } {
        bail!("refusing to connect to a browser broker owned by another user");
    }
    Ok(())
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
    compact: bool,
    lifecycle: Arc<ConnectionLifecycle>,
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

fn route_request(mut request: CallToolRequestParams) -> Result<(String, CallToolRequestParams)> {
    let id = request
        .arguments
        .get_or_insert_default()
        .remove(SESSION_KEY)
        .and_then(|value| value.as_str().map(str::to_owned))
        .filter(|id| !id.is_empty() && id.len() <= MAX_OPENCODE_SESSION_ID_BYTES)
        .ok_or_else(|| anyhow!("browser call is missing the OpenCode session identifier"))?;
    Ok((format!("{OPENCODE_PREFIX}{id}"), request))
}

fn compact_tools() -> Vec<Tool> {
    let dispatch_schema = json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "minLength": 1,
                "description": "Browser action name."
            },
            "arguments": {
                "type": "object",
                "description": "Arguments for the selected action. Omit when none are needed."
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
            "Execute a browser action. Put every action-specific field inside arguments; for example: {\"action\":\"navigate_page\",\"arguments\":{\"url\":\"https://example.com\"}}. Use browser_help first when arguments are unknown.",
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

fn validate_fields(arguments: &serde_json::Map<String, Value>, allowed: &[&str]) -> Result<()> {
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
    validate_fields(&envelope, &["action", "arguments"]).map_err(|error| {
        anyhow!("{error}; action-specific fields must be nested under \"arguments\"")
    })?;
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
    validate_fields(&arguments, &["action"])?;
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
        let tools = if self.compact {
            compact_tools()
        } else {
            self.proxy
                .catalog
                .get()
                .await
                .map_err(|error| rmcp::ErrorData::internal_error(error.to_string(), None))?
                .tools
                .clone()
        };
        Ok(ListToolsResult::with_all_items(tools))
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        if self.compact && request.name != "dispatch" && request.name != "help" {
            return Err(rmcp::ErrorData::invalid_params(
                "tool not found; use browser_dispatch or browser_help",
                None,
            ));
        }
        let _lease = match self.lifecycle.enter() {
            Some(lease) => lease,
            None => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    "browser connection is closed",
                )])
                .into());
            }
        };
        let result: Result<_> = async {
            let (id, mut request) = route_request(request)?;
            let catalog = self.proxy.catalog.get().await?;
            if self.compact {
                match request.name.as_ref() {
                    "dispatch" => request = expand_dispatch(request, &catalog.tools)?,
                    "help" => return compact_help(request, &catalog.tools),
                    _ => unreachable!("compact tool name checked above"),
                }
            }
            self.proxy
                .route_call(&catalog, &self.workspace, &id, request)
                .await
        }
        .await;
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
    stream: UnixStream,
    mut shutdown: watch::Receiver<bool>,
    config: Arc<Config>,
) -> Result<()> {
    let mut stream = stream;
    let pid = peer_pid(&stream)?;
    let compact = match server_handshake(&mut stream, &config).await? {
        HandshakeRequest::Connect { direct_tools, .. } => !direct_tools,
        HandshakeRequest::Release { session_id, .. } => {
            let result = proxy
                .release_session(&format!("{OPENCODE_PREFIX}{session_id}"))
                .await;
            stream.write_u8(u8::from(result.is_ok())).await?;
            stream.flush().await?;
            result?;
            return Ok(());
        }
    };
    let workspace = peer_workspace(pid)?;
    let lifecycle = Arc::new(ConnectionLifecycle::default());
    let service = Connection {
        proxy: proxy.clone(),
        workspace,
        compact,
        lifecycle: lifecycle.clone(),
    }
    .serve(stream)
    .await
    .context("failed to initialize MCP connection")?;
    let cancellation = service.cancellation_token();
    let waiting = service.waiting();
    tokio::pin!(waiting);
    let result = tokio::select! {
        result = &mut waiting => result,
        changed = shutdown.changed() => {
            if changed.is_ok() && *shutdown.borrow() {
                lifecycle.close_and_wait().await;
                cancellation.cancel();
            }
            waiting.await
        }
    }
    .context("MCP connection failed");
    lifecycle.close_and_wait().await;
    result?;
    Ok(())
}

async fn client_handshake(
    stream: &mut UnixStream,
    direct_tools: bool,
    config: Config,
) -> Result<()> {
    timeout(HANDSHAKE_TIMEOUT, async {
        write_handshake(
            stream,
            &HandshakeRequest::Connect {
                direct_tools,
                config,
            },
        )
        .await?;
        expect_acceptance(stream, "MCP adapter").await
    })
    .await
    .context("timed out negotiating with browser broker")?
}

async fn server_handshake(stream: &mut UnixStream, config: &Config) -> Result<HandshakeRequest> {
    timeout(HANDSHAKE_TIMEOUT, async {
        let request = read_handshake(stream).await?;
        if request.config() != config {
            stream.write_u8(2).await?;
            stream.flush().await?;
            bail!("browser configuration does not match the running managed broker");
        }
        if matches!(request, HandshakeRequest::Connect { .. }) {
            stream.write_u8(1).await?;
            stream.flush().await?;
        }
        Ok(request)
    })
    .await
    .context("timed out negotiating browser broker connection")?
}

enum HandshakeRequest {
    Connect { direct_tools: bool, config: Config },
    Release { session_id: String, config: Config },
}

impl HandshakeRequest {
    fn config(&self) -> &Config {
        match self {
            Self::Connect { config, .. } | Self::Release { config, .. } => config,
        }
    }
}

async fn write_handshake(stream: &mut UnixStream, request: &HandshakeRequest) -> Result<()> {
    stream.write_all(HANDSHAKE_MAGIC).await?;
    match request {
        HandshakeRequest::Connect {
            direct_tools,
            config,
        } => {
            stream.write_u8(CONNECT_HANDSHAKE).await?;
            stream.write_u8(u8::from(*direct_tools)).await?;
            write_config(stream, config).await?;
        }
        HandshakeRequest::Release { session_id, config } => {
            let bytes = session_id.as_bytes();
            if bytes.is_empty() || bytes.len() > MAX_OPENCODE_SESSION_ID_BYTES {
                bail!("invalid browser session identifier");
            }
            stream.write_u8(RELEASE_HANDSHAKE).await?;
            stream.write_u32(bytes.len() as u32).await?;
            stream.write_all(bytes).await?;
            write_config(stream, config).await?;
        }
    }
    stream.flush().await?;
    Ok(())
}

async fn read_handshake(stream: &mut UnixStream) -> Result<HandshakeRequest> {
    let mut magic = [0; HANDSHAKE_MAGIC.len()];
    stream.read_exact(&mut magic).await?;
    if magic != HANDSHAKE_MAGIC {
        bail!("invalid browser broker handshake");
    }
    match stream.read_u8().await? {
        CONNECT_HANDSHAKE => {
            let direct_tools = match stream.read_u8().await? {
                0 => false,
                1 => true,
                _ => bail!("invalid browser tool mode"),
            };
            Ok(HandshakeRequest::Connect {
                direct_tools,
                config: read_config(stream).await?,
            })
        }
        RELEASE_HANDSHAKE => {
            let length = stream.read_u32().await? as usize;
            if length == 0 || length > MAX_OPENCODE_SESSION_ID_BYTES {
                bail!("invalid browser session identifier length");
            }
            let mut session_id = vec![0; length];
            stream.read_exact(&mut session_id).await?;
            Ok(HandshakeRequest::Release {
                session_id: String::from_utf8(session_id)
                    .context("browser session identifier is not UTF-8")?,
                config: read_config(stream).await?,
            })
        }
        _ => bail!("invalid browser broker request"),
    }
}

async fn write_config(stream: &mut UnixStream, config: &Config) -> Result<()> {
    let bytes = serde_json::to_vec(config).context("failed to encode browser configuration")?;
    if bytes.len() > MAX_CONFIG_BYTES {
        bail!("browser configuration is too large");
    }
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await?;
    Ok(())
}

async fn read_config(stream: &mut UnixStream) -> Result<Config> {
    let length = stream.read_u32().await? as usize;
    if length == 0 || length > MAX_CONFIG_BYTES {
        bail!("invalid browser configuration length");
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes).context("invalid browser configuration")
}

async fn expect_acceptance(stream: &mut UnixStream, action: &str) -> Result<()> {
    match stream.read_u8().await? {
        1 => Ok(()),
        2 => bail!("browser configuration does not match the running managed broker"),
        _ => bail!("browser broker rejected {action}"),
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        let mut interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .expect("install SIGINT handler");
        let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            .expect("install SIGHUP handler");
        tokio::select! {
            _ = term.recv() => {},
            _ = interrupt.recv() => {},
            _ = hangup.recv() => {},
        }
    }
}

async fn release_session(paths: &RuntimePaths, config: &Config, session_id: &str) -> Result<()> {
    if session_id.is_empty() || session_id.len() > MAX_OPENCODE_SESSION_ID_BYTES {
        bail!("invalid browser session identifier");
    }
    let mut stream = connect_or_start(paths, config).await?;
    validate_broker_peer(&stream)?;
    timeout(RELEASE_CONTROL_TIMEOUT, async {
        write_handshake(
            &mut stream,
            &HandshakeRequest::Release {
                session_id: session_id.to_owned(),
                config: config.clone(),
            },
        )
        .await?;
        expect_acceptance(&mut stream, "session release").await
    })
    .await
    .context("timed out waiting for browser session release")?
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    #[test]
    fn opencode_connections_require_and_remove_the_session_id() {
        let request = CallToolRequestParams::new("list_pages").with_arguments(
            json!({(SESSION_KEY): "session-a"})
                .as_object()
                .expect("request arguments")
                .clone(),
        );
        let (id, request) = route_request(request).expect("route OpenCode request");
        assert_eq!(id, "opencode:session-a");
        assert!(
            request
                .arguments
                .is_some_and(|arguments| !arguments.contains_key(SESSION_KEY))
        );
        assert!(route_request(CallToolRequestParams::new("list_pages")).is_err());
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
            json!({"action": "navigate_page", "arguments": {(SESSION_KEY): "forged"}}),
        ] {
            let request = CallToolRequestParams::new("dispatch")
                .with_arguments(arguments.as_object().unwrap().clone());
            assert!(expand_dispatch(request, &tools).is_err());
        }
        let misplaced = CallToolRequestParams::new("dispatch").with_arguments(
            json!({"action": "navigate_page", "url": "https://example.com"})
                .as_object()
                .unwrap()
                .clone(),
        );
        assert!(
            expand_dispatch(misplaced, &tools)
                .unwrap_err()
                .to_string()
                .contains("must be nested under \"arguments\"")
        );
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
}
