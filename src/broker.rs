use std::{
    cmp::Ordering as CmpOrdering,
    fs::{self, File, OpenOptions},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::ffi::OsStrExt,
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use super::{
    BrowserProxy, Cli, Config, MAX_SESSION_ID_BYTES, ProcessMode, browser_tool,
    persistence::OwnershipPersistence, validate_config,
};
use anyhow::{Context, Result, anyhow, bail};
use futures_util::FutureExt;
use rmcp::{
    RoleClient, ServerHandler, ServiceExt,
    model::{
        CallToolRequest, CallToolRequestParams, CallToolResponse, CallToolResult,
        CancelledNotificationParam, ClientRequest, ContentBlock, Implementation, ListToolsRequest,
        ListToolsResult, PaginatedRequestParams, RequestMetaObject, ServerCapabilities, ServerInfo,
        ServerResult, Tool, ToolAnnotations,
    },
    service::{PeerRequestOptions, RequestContext, RunningService, ServiceError},
};
use semver::Version;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::{Mutex, Notify, watch},
    task::JoinSet,
    time::{Instant, interval, sleep, timeout},
};
use tracing::{Instrument, info, warn};
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
const CODEX_RELEASE_HANDSHAKE: u8 = 5;
const VERSIONED_CONNECT_HANDSHAKE: u8 = 4;
const ACCEPTED_RESPONSE: u8 = 1;
const CONFIG_MISMATCH_RESPONSE: u8 = 2;
const RESTARTING_RESPONSE: u8 = 3;
const MAX_VERSION_BYTES: usize = 256;
const MAX_CONFIG_BYTES: usize = 16 * 1024;
static PACKAGE_VERSION: LazyLock<Version> = LazyLock::new(|| {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("package version must be valid SemVer")
});

pub(super) const SESSION_KEY: &str = "__opencode_session_id";
pub(super) const CODEX_SESSION_KEY: &str = "__codex_session_id";
const SESSION_META_KEY: &str = "ai.opencode/sessionID";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SessionClient {
    OpenCode,
    Codex,
}

impl SessionClient {
    fn prefix(self) -> &'static str {
        match self {
            Self::OpenCode => "opencode:",
            Self::Codex => "codex:",
        }
    }

    pub(super) fn validate_id(self, id: &str) -> Result<()> {
        if id.is_empty() || id.len() > MAX_SESSION_ID_BYTES - self.prefix().len() {
            bail!("invalid browser session identifier");
        }
        Ok(())
    }

    fn owner_id(self, id: &str) -> Result<String> {
        self.validate_id(id)?;
        Ok(format!("{}{id}", self.prefix()))
    }
}

pub(super) async fn run(cli: Cli) -> Result<()> {
    let Cli {
        direct_tools,
        profile_override,
        mut config,
        command,
    } = cli;
    match command {
        Some(ProcessMode::BrowserStderr { .. }) => {
            bail!("browser stderr writer must run outside the broker runtime")
        }
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
        Some(ProcessMode::CodexHook) => super::codex::run(config, profile_override).await,
        None => {
            config.resolve(profile_override).await?;
            validate_config(&config)?;
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
    let adapter = Adapter {
        paths,
        direct_tools,
        config,
        broker: Mutex::new(None),
    };
    // Validate configuration before publishing the stdio MCP server, as before.
    adapter.connection().await?;
    adapter
        .serve(rmcp::transport::stdio())
        .await
        .context("failed to initialize browser MCP adapter")?
        .waiting()
        .await
        .context("browser MCP adapter failed")?;
    Ok(())
}

type BrokerClient = RunningService<RoleClient, ()>;

struct Adapter {
    paths: RuntimePaths,
    direct_tools: bool,
    config: Config,
    broker: Mutex<Option<Arc<BrokerClient>>>,
}

impl Adapter {
    async fn connection(&self) -> Result<Arc<BrokerClient>> {
        let mut current = self.broker.lock().await;
        if let Some(client) = current.as_ref() {
            if !client.is_closed() && !client.is_transport_closed() {
                return Ok(client.clone());
            }
            warn!("browser broker connection lost; reconnecting the adapter");
            client.cancellation_token().cancel();
            *current = None;
        }
        let stream = connect_current_broker(&self.paths, self.direct_tools, &self.config).await?;
        let client = Arc::new(
            timeout(HANDSHAKE_TIMEOUT, ().serve(stream))
                .await
                .context("timed out initializing broker MCP connection")?
                .context("failed to initialize broker MCP connection")?,
        );
        *current = Some(client.clone());
        Ok(client)
    }

    async fn forward(
        &self,
        request: ClientRequest,
        context: RequestContext<rmcp::RoleServer>,
    ) -> Result<ServerResult, rmcp::ErrorData> {
        let client = self.connection().await.map_err(|error| {
            rmcp::ErrorData::internal_error(format!("browser broker unavailable: {error}"), None)
        })?;
        if context.ct.is_cancelled() {
            return Err(rmcp::ErrorData::internal_error(
                "browser request cancelled before dispatch",
                None,
            ));
        }
        let operation_timeout = Duration::from_secs(self.config.operation_seconds);
        let budget = match &request {
            ClientRequest::ListToolsRequest(_) => super::CATALOG_TIMEOUT,
            ClientRequest::CallToolRequest(call) => {
                tool_budget(&call.params.name, operation_timeout)
            }
            _ => operation_timeout,
        };
        let request_timeout = budget + HANDSHAKE_TIMEOUT;
        let handle = client
            .send_cancellable_request(
                request,
                PeerRequestOptions::with_timeout(request_timeout).with_meta(context.meta.clone()),
            )
            .await
            .map_err(adapter_request_error)?;
        let request_id = handle.id.clone();
        // A dispatched operation may already have taken effect. Never replay it
        // after a disconnect; the next request reconnects and reconciles ownership.
        let result = tokio::select! {
            result = handle.await_response() => result,
            _ = context.ct.cancelled() => {
                let _ = client.notify_cancelled(CancelledNotificationParam::new(
                    Some(request_id),
                    Some("upstream request cancelled".into()),
                )).await;
                Err(ServiceError::Cancelled { reason: Some("upstream request cancelled".into()) })
            }
        };
        result.map_err(adapter_request_error)
    }
}

fn adapter_request_error(error: ServiceError) -> rmcp::ErrorData {
    match error {
        ServiceError::McpError(error) => error,
        error => {
            warn!(%error, "browser broker request interrupted; request was not replayed");
            rmcp::ErrorData::internal_error(
                format!(
                    "Browser broker request interrupted: {error}. The action may have taken effect; inspect browser state before retrying. Subsequent calls reconnect automatically."
                ),
                None,
            )
        }
    }
}

impl ServerHandler for Adapter {
    fn get_info(&self) -> ServerInfo {
        browser_server_info()
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        match self
            .forward(
                ClientRequest::ListToolsRequest(ListToolsRequest {
                    method: Default::default(),
                    params: request,
                    extensions: Default::default(),
                }),
                context,
            )
            .await?
        {
            ServerResult::ListToolsResult(result) => Ok(result),
            _ => Err(rmcp::ErrorData::internal_error(
                "unexpected broker tool list",
                None,
            )),
        }
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        match self
            .forward(
                ClientRequest::CallToolRequest(CallToolRequest::new(request)),
                context,
            )
            .await
        {
            Ok(ServerResult::CallToolResult(result)) => Ok(result.into()),
            Ok(_) => Err(rmcp::ErrorData::internal_error(
                "unexpected broker tool response",
                None,
            )),
            Err(error) => Ok(CallToolResult::error(vec![ContentBlock::text(error.message)]).into()),
        }
    }
}

async fn connect_current_broker(
    paths: &RuntimePaths,
    direct_tools: bool,
    config: &Config,
) -> Result<UnixStream> {
    let deadline = Instant::now() + BROKER_START_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            bail!("managed browser broker did not become current within {BROKER_START_TIMEOUT:?}");
        }
        let mut stream = connect_or_start(paths, config, deadline).await?;
        validate_broker_peer(&stream)?;
        let peer = broker_peer(&stream);
        match client_handshake(&mut stream, direct_tools, config, Some(&PACKAGE_VERSION)).await? {
            ConnectResponse::Accepted => return Ok(stream),
            ConnectResponse::Restarting => {
                info!(adapter_version = %*PACKAGE_VERSION, "browser broker restarting for upgrade");
            }
            ConnectResponse::Legacy => {
                let observed = peer?;
                if replace_compatible_legacy(paths, config, &observed).await? {
                    info!(pid = observed.pid, adapter_version = %*PACKAGE_VERSION, "replacing legacy browser broker");
                }
            }
        }
        sleep(Duration::from_millis(50)).await;
    }
}

async fn connect_or_start(
    paths: &RuntimePaths,
    config: &Config,
    deadline: Instant,
) -> Result<UnixStream> {
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

struct BrokerPeer {
    pid: libc::pid_t,
    pidfd: OwnedFd,
}

fn broker_peer(stream: &UnixStream) -> Result<BrokerPeer> {
    let pid = peer_pid(stream)?;
    let mut fd = -1;
    let mut length = std::mem::size_of_val(&fd) as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERPIDFD,
            (&raw mut fd).cast(),
            &raw mut length,
        )
    };
    if result == -1 {
        return Err(std::io::Error::last_os_error())
            .context("failed to get a stable browser broker process handle");
    }
    if fd < 0 || length as usize != std::mem::size_of_val(&fd) {
        if fd >= 0 {
            unsafe { libc::close(fd) };
        }
        bail!("browser broker returned an invalid process handle");
    }
    Ok(BrokerPeer {
        pid,
        pidfd: unsafe { OwnedFd::from_raw_fd(fd) },
    })
}

async fn replace_compatible_legacy(
    paths: &RuntimePaths,
    config: &Config,
    observed: &BrokerPeer,
) -> Result<bool> {
    let Some(mut stream) = connect_or_absent(&paths.socket).await? else {
        return Ok(false);
    };
    if peer_pid(&stream)? != observed.pid || !signal_peer(observed, 0)? {
        return Ok(false);
    }
    // Connect is understood by every v5 broker and validates its full configuration.
    if client_handshake(&mut stream, false, config, None).await? != ConnectResponse::Accepted {
        bail!("legacy browser broker returned an invalid connection response");
    }
    signal_peer(observed, libc::SIGTERM)
}

fn signal_peer(peer: &BrokerPeer, signal: libc::c_int) -> Result<bool> {
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            peer.pidfd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result == -1 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(false);
        }
        return Err(error).context("failed to signal legacy browser broker");
    }
    Ok(true)
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
    super::logging::enable(&config.profile)?;
    let persistence = OwnershipPersistence::open(config.profile.join(STATE_FILE))?;
    let signal = shutdown_signal();
    tokio::pin!(signal);
    let proxy = BrowserProxy::new(config.clone(), persistence).await?;
    // Keep the socket unpublished until initialization succeeds so clients
    // cannot connect to a broker that has not become ready yet.
    let listener = tokio::select! {
        biased;
        _ = &mut signal => {
            return shutdown_proxy(&proxy, false).await;
        }
        listener = bind_socket(&paths) => listener,
    };
    let (listener, socket_guard) = match listener {
        Ok(listener) => listener,
        Err(error) => {
            if let Err(shutdown_error) = shutdown_proxy(&proxy, false).await {
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
    let idle = sleep(BROKER_START_TIMEOUT);
    tokio::pin!(idle);
    let exit = loop {
        tokio::select! {
            biased;
            _ = &mut signal => break Ok(false),
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let proxy = proxy.clone();
                    let shutdown = shutdown.subscribe();
                    let config = config.clone();
                    connections.spawn(async move {
                        serve_connection(proxy, stream, shutdown, config).await
                    });
                },
                Err(error) => break Err(error).context("Unix socket accept failed"),
            },
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                match result {
                    Err(error) => warn!(%error, "connection task panicked"),
                    Ok(Err(error)) => warn!(%error, "browser broker connection failed"),
                    Ok(Ok(false)) => {}
                    Ok(Ok(true)) => {
                        info!(version = %*PACKAGE_VERSION, "browser broker stopping for upgrade");
                        break Ok(true);
                    }
                }
                if connections.is_empty() {
                    idle.as_mut().reset(Instant::now() + MANAGED_IDLE_GRACE);
                }
            }
            _ = &mut idle, if connections.is_empty() => break Ok(false),
        }
    };
    drop(listener);
    drop(socket_guard);
    let _ = shutdown.send(true);
    let _ = timeout(DRAIN_TIMEOUT, async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    connections.shutdown().await;
    cleanup.abort();
    let _ = cleanup.await;
    match exit {
        Ok(preserve_browser) => shutdown_proxy(&proxy, preserve_browser).await,
        Err(error) => {
            if let Err(shutdown_error) = shutdown_proxy(&proxy, false).await {
                warn!(%shutdown_error, "browser proxy shutdown after broker failure was incomplete");
            }
            Err(error)
        }
    }
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

async fn shutdown_proxy(proxy: &BrowserProxy, preserve_browser: bool) -> Result<()> {
    proxy
        .shutdown(preserve_browser)
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

fn route_request(
    mut request: CallToolRequestParams,
    meta: &RequestMetaObject,
) -> Result<(String, CallToolRequestParams)> {
    let arguments = request.arguments.get_or_insert_default();
    let opencode = arguments.remove(SESSION_KEY);
    let codex = arguments.remove(CODEX_SESSION_KEY);
    let (client, id) = match (
        meta.get(SESSION_META_KEY),
        opencode.as_ref(),
        codex.as_ref(),
    ) {
        (Some(id), injected, None) => {
            if injected.is_some_and(|injected| injected != id) {
                bail!("browser call contains conflicting agent session identifiers");
            }
            (SessionClient::OpenCode, id)
        }
        (None, Some(id), None) => (SessionClient::OpenCode, id),
        (None, None, Some(id)) => (SessionClient::Codex, id),
        (None, None, None) => bail!(
            "browser call is missing host session identity; use OpenCode MCP session metadata or the OpenCode/Codex routing hooks"
        ),
        _ => bail!("browser call contains conflicting agent session identifiers"),
    };
    let id = id
        .as_str()
        .context("browser session identifier must be a string")?;
    Ok((client.owner_id(id)?, request))
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
    if arguments.contains_key(SESSION_KEY) || arguments.contains_key(CODEX_SESSION_KEY) {
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
        browser_server_info()
    }
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        let tools = if self.compact {
            compact_tools()
        } else {
            bounded_request(
                super::CATALOG_TIMEOUT,
                &context,
                self.proxy.catalog.get(&self.workspace),
            )
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
        context: RequestContext<rmcp::RoleServer>,
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
        let budget = tool_budget(&request.name, self.proxy.operation_timeout);
        let span = tracing::info_span!("browser_request", request_id = ?context.id, tool = %request.name, session_id = tracing::field::Empty);
        let result: Result<_> = bounded_request(budget, &context, async {
            info!(
                timeout_seconds = budget.as_secs(),
                workspace = %self.workspace.display(),
                "browser request started"
            );
            let (id, mut request) = route_request(request, &context.meta)?;
            tracing::Span::current().record("session_id", &id);
            info!("waiting for Chrome DevTools tool catalog");
            let catalog = self.proxy.catalog.get(&self.workspace).await?;
            info!("Chrome DevTools tool catalog available");
            if self.compact {
                match request.name.as_ref() {
                    "dispatch" => request = expand_dispatch(request, &catalog.tools)?,
                    "help" => return compact_help(request, &catalog.tools),
                    _ => unreachable!("compact tool name checked above"),
                }
            }
            self.proxy
                .route_call(&catalog, &self.workspace, &id, request, &context)
                .await
        })
        .instrument(span)
        .await;
        Ok(result
            .unwrap_or_else(|error| {
                CallToolResult::error(vec![ContentBlock::text(error.to_string())])
            })
            .into())
    }
}

fn tool_budget(name: &str, operation_timeout: Duration) -> Duration {
    if name == "help" {
        super::CATALOG_TIMEOUT
    } else {
        operation_timeout
    }
}

async fn bounded_request<T>(
    budget: Duration,
    context: &RequestContext<rmcp::RoleServer>,
    request: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    fn interrupted(reason: impl std::fmt::Display) -> anyhow::Error {
        anyhow!(
            "Browser request {reason}. The action may have taken effect; inspect browser state before retrying. Interrupted workers are replaced on the next call."
        )
    }

    let started = Instant::now();
    let result = tokio::select! {
        biased;
        _ = context.ct.cancelled() => Err(interrupted("cancelled")),
        result = timeout(budget, std::panic::AssertUnwindSafe(request).catch_unwind()) => match result {
            Ok(Ok(result)) => result,
            Ok(Err(payload)) => Err(interrupted(format_args!("panicked: {}", super::panic_message(payload.as_ref())))),
            Err(_) => Err(interrupted(format_args!("exceeded its {}-second deadline", budget.as_secs()))),
        },
    };
    match &result {
        Ok(_) => info!(
            elapsed_ms = started.elapsed().as_millis(),
            "browser request finished"
        ),
        Err(error) => {
            warn!(error = %format_args!("{error:#}"), elapsed_ms = started.elapsed().as_millis(), "browser request failed")
        }
    }
    result
}

fn browser_server_info() -> ServerInfo {
    ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
        .with_server_info(Implementation::from_build_env())
        .with_instructions(
            "Controls a shared Chromium-family browser with connection-isolated pages.".to_owned(),
        )
}

async fn serve_connection(
    proxy: BrowserProxy,
    stream: UnixStream,
    mut shutdown: watch::Receiver<bool>,
    config: Arc<Config>,
) -> Result<bool> {
    let mut stream = stream;
    let pid = peer_pid(&stream)?;
    let Some(request) = server_handshake(&mut stream, &config, &PACKAGE_VERSION).await? else {
        return Ok(true);
    };
    let compact = match request {
        HandshakeRequest::Connect { direct_tools, .. } => !direct_tools,
        HandshakeRequest::Release {
            session_id, client, ..
        } => {
            let owner = client.owner_id(&session_id)?;
            let queued = client == SessionClient::Codex;
            if queued {
                // This connection task keeps the broker alive until cleanup completes.
                stream.write_u8(ACCEPTED_RESPONSE).await?;
                stream.flush().await?;
            }
            let result = proxy.release_session(&owner).await;
            if !queued {
                stream.write_u8(u8::from(result.is_ok())).await?;
                stream.flush().await?;
            }
            result?;
            return Ok(false);
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
    Ok(false)
}

#[derive(Debug, Eq, PartialEq)]
enum ConnectResponse {
    Accepted,
    Restarting,
    Legacy,
}

async fn client_handshake(
    stream: &mut UnixStream,
    direct_tools: bool,
    config: &Config,
    version: Option<&Version>,
) -> Result<ConnectResponse> {
    timeout(HANDSHAKE_TIMEOUT, async {
        let written = write_handshake(
            stream,
            &HandshakeRequest::Connect {
                direct_tools,
                version: version.cloned(),
                config: config.clone(),
            },
        )
        .await;
        if let Err(error) = written {
            if version.is_some() && is_transient_disconnect(&error) {
                return Ok(ConnectResponse::Legacy);
            }
            return Err(error);
        }
        match stream.read_u8().await {
            Ok(ACCEPTED_RESPONSE) => Ok(ConnectResponse::Accepted),
            Ok(CONFIG_MISMATCH_RESPONSE) => {
                bail!("browser configuration does not match the running managed broker")
            }
            Ok(RESTARTING_RESPONSE) if version.is_some() => Ok(ConnectResponse::Restarting),
            Ok(_) => bail!("browser broker rejected MCP adapter"),
            Err(error)
                if version.is_some()
                    && matches!(
                        error.kind(),
                        std::io::ErrorKind::UnexpectedEof
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                    ) =>
            {
                Ok(ConnectResponse::Legacy)
            }
            Err(error) => Err(error.into()),
        }
    })
    .await
    .context("timed out negotiating with browser broker")?
}

async fn server_handshake(
    stream: &mut UnixStream,
    config: &Config,
    broker_version: &Version,
) -> Result<Option<HandshakeRequest>> {
    timeout(HANDSHAKE_TIMEOUT, async {
        let request = read_handshake(stream).await?;
        let request_config = match &request {
            HandshakeRequest::Connect { config, .. } | HandshakeRequest::Release { config, .. } => {
                config
            }
        };
        if request_config != config {
            stream.write_u8(CONFIG_MISMATCH_RESPONSE).await?;
            stream.flush().await?;
            bail!("browser configuration does not match the running managed broker");
        }
        if let HandshakeRequest::Connect {
            version: Some(version),
            ..
        } = &request
            && version.cmp_precedence(broker_version) == CmpOrdering::Greater
        {
            stream.write_u8(RESTARTING_RESPONSE).await?;
            stream.flush().await?;
            return Ok(None);
        }
        if matches!(request, HandshakeRequest::Connect { .. }) {
            stream.write_u8(ACCEPTED_RESPONSE).await?;
            stream.flush().await?;
        }
        Ok(Some(request))
    })
    .await
    .context("timed out negotiating browser broker connection")?
}

enum HandshakeRequest {
    Connect {
        direct_tools: bool,
        version: Option<Version>,
        config: Config,
    },
    Release {
        session_id: String,
        client: SessionClient,
        config: Config,
    },
}

async fn write_handshake(stream: &mut UnixStream, request: &HandshakeRequest) -> Result<()> {
    stream.write_all(HANDSHAKE_MAGIC).await?;
    match request {
        HandshakeRequest::Connect {
            direct_tools,
            version,
            config,
        } => {
            stream
                .write_u8(if version.is_some() {
                    VERSIONED_CONNECT_HANDSHAKE
                } else {
                    CONNECT_HANDSHAKE
                })
                .await?;
            if let Some(version) = version {
                write_bounded_string(stream, &version.to_string(), MAX_VERSION_BYTES).await?;
            }
            stream.write_u8(u8::from(*direct_tools)).await?;
            write_config(stream, config).await?;
        }
        HandshakeRequest::Release {
            session_id,
            client,
            config,
        } => {
            client.validate_id(session_id)?;
            let bytes = session_id.as_bytes();
            stream
                .write_u8(match client {
                    SessionClient::OpenCode => RELEASE_HANDSHAKE,
                    SessionClient::Codex => CODEX_RELEASE_HANDSHAKE,
                })
                .await?;
            if *client == SessionClient::Codex {
                stream.write_all(&[1, 1]).await?;
            }
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
        kind @ (CONNECT_HANDSHAKE | VERSIONED_CONNECT_HANDSHAKE) => {
            let version = if kind == VERSIONED_CONNECT_HANDSHAKE {
                let version =
                    read_bounded_string(stream, MAX_VERSION_BYTES, "adapter version").await?;
                Some(Version::parse(&version).context("invalid adapter version")?)
            } else {
                None
            };
            let direct_tools = match stream.read_u8().await? {
                0 => false,
                1 => true,
                _ => bail!("invalid browser tool mode"),
            };
            Ok(HandshakeRequest::Connect {
                direct_tools,
                version,
                config: read_config(stream).await?,
            })
        }
        kind @ (RELEASE_HANDSHAKE | CODEX_RELEASE_HANDSHAKE) => {
            let client = if kind == RELEASE_HANDSHAKE {
                SessionClient::OpenCode
            } else {
                if stream.read_u8().await? != 1 || stream.read_u8().await? != 1 {
                    bail!("invalid queued Codex session release");
                }
                SessionClient::Codex
            };
            let length = stream.read_u32().await? as usize;
            if length == 0 || length > MAX_SESSION_ID_BYTES {
                bail!("invalid browser session identifier length");
            }
            let mut session_id = vec![0; length];
            stream.read_exact(&mut session_id).await?;
            let session_id =
                String::from_utf8(session_id).context("browser session identifier is not UTF-8")?;
            client.validate_id(&session_id)?;
            Ok(HandshakeRequest::Release {
                session_id,
                client,
                config: read_config(stream).await?,
            })
        }
        _ => bail!("invalid browser broker request"),
    }
}

async fn write_bounded_string(stream: &mut UnixStream, value: &str, maximum: usize) -> Result<()> {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > maximum {
        bail!("broker handshake string has an invalid length");
    }
    stream.write_u16(bytes.len() as u16).await?;
    stream.write_all(bytes).await?;
    Ok(())
}

async fn read_bounded_string(
    stream: &mut UnixStream,
    maximum: usize,
    description: &str,
) -> Result<String> {
    let length = stream.read_u16().await? as usize;
    if length == 0 || length > maximum {
        bail!("invalid {description} length");
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    String::from_utf8(bytes).with_context(|| format!("{description} is not UTF-8"))
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
        ACCEPTED_RESPONSE => Ok(()),
        CONFIG_MISMATCH_RESPONSE => {
            bail!("browser configuration does not match the running managed broker")
        }
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
    SessionClient::OpenCode.validate_id(session_id)?;
    timeout(RELEASE_CONTROL_TIMEOUT, async {
        loop {
            let result = async {
                let mut stream =
                    connect_or_start(paths, config, Instant::now() + BROKER_START_TIMEOUT).await?;
                validate_broker_peer(&stream)?;
                write_handshake(
                    &mut stream,
                    &HandshakeRequest::Release {
                        session_id: session_id.to_owned(),
                        client: SessionClient::OpenCode,
                        config: config.clone(),
                    },
                )
                .await?;
                expect_acceptance(&mut stream, "session release").await
            }
            .await;
            match result {
                Ok(()) => return Ok(()),
                Err(error) if is_transient_disconnect(&error) => {
                    sleep(Duration::from_millis(50)).await;
                }
                Err(error) => return Err(error),
            }
        }
    })
    .await
    .context("timed out waiting for browser session release")?
}

pub(super) async fn queue_codex_release(
    mut config: Config,
    profile: Option<PathBuf>,
    session_id: String,
) -> Result<()> {
    timeout(Duration::from_secs(2), async {
        // Codex's three-second hook deadline includes configuration and IPC.
        config.resolve(profile).await?;
        let paths = RuntimePaths::new(&config)?;
        // A stopped broker has no live owned tabs. Cleanup must not start a browser.
        let Some(mut stream) = connect_or_absent(&paths.socket).await? else {
            return Ok(());
        };
        validate_broker_peer(&stream)?;
        write_handshake(
            &mut stream,
            &HandshakeRequest::Release {
                session_id,
                client: SessionClient::Codex,
                config,
            },
        )
        .await?;
        expect_acceptance(&mut stream, "Codex session release").await
    })
    .await
    .context("timed out queueing Codex browser session release")?
}

fn is_transient_disconnect(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::NotConnected
            )
        })
    })
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use futures_util::FutureExt;
    use serde_json::{Value, json};

    use super::*;

    fn session_meta(id: impl Into<Value>) -> RequestMetaObject {
        let mut meta = RequestMetaObject::default();
        meta.insert(SESSION_META_KEY.to_owned(), id.into());
        meta
    }

    #[tokio::test]
    async fn stalled_request_returns_an_error_and_next_request_can_succeed() {
        let path =
            std::env::temp_dir().join(format!("browser-mcp-pending-{}.json", std::process::id()));
        let config = Arc::new(
            Cli::try_parse_from(["browser-mcp", "--headless=true"])
                .unwrap()
                .config,
        );
        let mut proxy = BrowserProxy::new(config, OwnershipPersistence::open(path).unwrap())
            .await
            .unwrap();
        proxy.operation_timeout = Duration::from_millis(10);
        *proxy.catalog.future.lock().await = Some(futures_util::future::pending().boxed().shared());
        let connection = Connection {
            proxy: proxy.clone(),
            workspace: PathBuf::from("/"),
            compact: true,
            lifecycle: Arc::default(),
        };
        let (server_io, client_io) = tokio::io::duplex(4096);
        let (server, client) = tokio::join!(connection.serve(server_io), ().serve(client_io));
        let server = server.unwrap();
        let client = client.unwrap();
        let mut request = CallToolRequestParams::new("dispatch");
        request.meta = Some(session_meta("session-a"));
        let result = timeout(Duration::from_secs(1), client.call_tool(request))
            .await
            .expect("stalled catalog kept the request open")
            .unwrap();
        assert!(result.is_error.unwrap_or(false));
        assert!(
            result.content[0]
                .as_text()
                .unwrap()
                .text
                .contains("deadline")
        );
        *proxy.catalog.future.lock().await = Some(
            async {
                Ok(Arc::new(super::super::ToolCatalog {
                    tools: Vec::new(),
                    page_scoped: Default::default(),
                }))
            }
            .boxed()
            .shared(),
        );
        let mut request = CallToolRequestParams::new("help");
        request.meta = Some(session_meta("session-a"));
        let result = client.call_tool(request).await.unwrap();
        assert!(!result.is_error.unwrap_or(false));
        client.cancel().await.unwrap();
        server.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn adapter_preserves_host_metadata_and_cancellation() {
        struct PendingBroker(tokio::sync::mpsc::UnboundedSender<RequestContext<rmcp::RoleServer>>);
        impl ServerHandler for PendingBroker {
            fn get_info(&self) -> ServerInfo {
                browser_server_info()
            }

            async fn call_tool(
                &self,
                _request: CallToolRequestParams,
                context: RequestContext<rmcp::RoleServer>,
            ) -> Result<CallToolResponse, rmcp::ErrorData> {
                let cancellation = context.ct.clone();
                self.0.send(context).unwrap();
                cancellation.cancelled().await;
                Ok(CallToolResult::error(vec![ContentBlock::text("cancelled")]).into())
            }
        }

        let (started, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let (server_io, client_io) = tokio::io::duplex(4096);
        let (broker, client) =
            tokio::join!(PendingBroker(started).serve(server_io), ().serve(client_io));
        let broker = broker.unwrap();
        let adapter = Adapter {
            paths: RuntimePaths {
                socket: PathBuf::new(),
                lock: PathBuf::new(),
            },
            direct_tools: false,
            config: Cli::try_parse_from(["browser-mcp", "--headless=true"])
                .unwrap()
                .config,
            broker: Mutex::new(Some(Arc::new(client.unwrap()))),
        };
        let (server_io, client_io) = tokio::io::duplex(4096);
        let (adapter, client) = tokio::join!(adapter.serve(server_io), ().serve(client_io));
        let adapter = adapter.unwrap();
        let client = client.unwrap();
        let request = client
            .send_cancellable_request(
                ClientRequest::CallToolRequest(CallToolRequest::new(CallToolRequestParams::new(
                    "pending",
                ))),
                PeerRequestOptions::no_options().with_meta(session_meta("session-a")),
            )
            .await
            .unwrap();
        let dispatched = timeout(Duration::from_secs(1), requests.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            dispatched.meta.get(SESSION_META_KEY),
            Some(&json!("session-a"))
        );
        request.cancel(Some("caller stopped".into())).await.unwrap();
        timeout(Duration::from_secs(1), dispatched.ct.cancelled())
            .await
            .expect("broker did not receive cancellation");
        client.cancel().await.unwrap();
        adapter.cancel().await.unwrap();
        broker.cancel().await.unwrap();
    }

    #[test]
    fn legacy_session_id_is_removed_and_unattributed_calls_are_rejected() {
        let request = CallToolRequestParams::new("list_pages").with_arguments(
            json!({(SESSION_KEY): "session-a"})
                .as_object()
                .expect("request arguments")
                .clone(),
        );
        let (id, request) =
            route_request(request, &RequestMetaObject::default()).expect("route OpenCode request");
        assert_eq!(id, "opencode:session-a");
        assert!(
            request
                .arguments
                .is_some_and(|arguments| !arguments.contains_key(SESSION_KEY))
        );
        assert!(
            route_request(
                CallToolRequestParams::new("list_pages"),
                &RequestMetaObject::default()
            )
            .unwrap_err()
            .to_string()
            .contains("missing host session identity")
        );
    }

    #[test]
    fn host_metadata_routes_without_plugin_injection_and_rejects_conflicts() {
        for injected in [json!({}), json!({(SESSION_KEY): "session-a"})] {
            let (id, request) = route_request(
                CallToolRequestParams::new("dispatch")
                    .with_arguments(injected.as_object().unwrap().clone()),
                &session_meta("session-a"),
            )
            .unwrap();
            assert_eq!(id, "opencode:session-a");
            assert!(request.arguments.unwrap().is_empty());
        }
        for arguments in [
            json!({(SESSION_KEY): "another-session"}),
            json!({(SESSION_KEY): null}),
            json!({(CODEX_SESSION_KEY): "session-a"}),
        ] {
            assert!(
                route_request(
                    CallToolRequestParams::new("dispatch")
                        .with_arguments(arguments.as_object().unwrap().clone()),
                    &session_meta("session-a"),
                )
                .is_err()
            );
        }
        for id in [
            Value::Null,
            json!(42),
            json!(""),
            json!("x".repeat(MAX_SESSION_ID_BYTES)),
        ] {
            assert!(
                route_request(CallToolRequestParams::new("dispatch"), &session_meta(id)).is_err()
            );
        }
    }

    #[test]
    fn agent_session_namespaces_do_not_collide_or_allow_ambiguous_routing() {
        let request = CallToolRequestParams::new("help").with_arguments(
            json!({(CODEX_SESSION_KEY): "session-a"})
                .as_object()
                .unwrap()
                .clone(),
        );
        let (id, request) = route_request(request, &RequestMetaObject::default()).unwrap();
        assert_eq!(id, "codex:session-a");
        assert!(request.arguments.unwrap().is_empty());
        for arguments in [
            json!({(CODEX_SESSION_KEY): "session-a", (SESSION_KEY): "session-a"}),
            json!({(CODEX_SESSION_KEY): null}),
            json!({(CODEX_SESSION_KEY): ""}),
        ] {
            assert!(
                route_request(
                    CallToolRequestParams::new("help")
                        .with_arguments(arguments.as_object().unwrap().clone()),
                    &RequestMetaObject::default(),
                )
                .is_err()
            );
        }
    }

    #[tokio::test]
    async fn release_handshakes_preserve_existing_wire_formats() {
        let config = Cli::try_parse_from(["browser-mcp", "--headless=true"])
            .unwrap()
            .config;
        let config_bytes = serde_json::to_vec(&config).unwrap();
        for (client, header) in [
            (SessionClient::OpenCode, &[1][..]),
            (SessionClient::Codex, &[5, 1, 1][..]),
        ] {
            let mut fixture = b"opencode-browser-mcp\0v5\n".to_vec();
            fixture.extend_from_slice(header);
            fixture.extend_from_slice(&[0, 0, 0, 4]);
            fixture.extend_from_slice(b"same");
            fixture.extend_from_slice(&(config_bytes.len() as u32).to_be_bytes());
            fixture.extend_from_slice(&config_bytes);

            let (mut writer, mut reader) = UnixStream::pair().unwrap();
            write_handshake(
                &mut writer,
                &HandshakeRequest::Release {
                    session_id: "same".to_owned(),
                    client,
                    config: config.clone(),
                },
            )
            .await
            .unwrap();
            let mut actual = vec![0; fixture.len()];
            reader.read_exact(&mut actual).await.unwrap();
            assert_eq!(actual, fixture);

            let (mut writer, mut reader) = UnixStream::pair().unwrap();
            writer.write_all(&fixture).await.unwrap();
            let HandshakeRequest::Release {
                session_id,
                client: actual_client,
                config: actual_config,
            } = read_handshake(&mut reader).await.unwrap()
            else {
                panic!("expected release")
            };
            assert_eq!(session_id, "same");
            assert_eq!(actual_client, client);
            assert_eq!(actual_config, config);
        }
        for flags in [[0, 0], [0, 1], [1, 0], [2, 1]] {
            let (mut writer, mut reader) = UnixStream::pair().unwrap();
            writer.write_all(HANDSHAKE_MAGIC).await.unwrap();
            writer.write_u8(CODEX_RELEASE_HANDSHAKE).await.unwrap();
            writer.write_all(&flags).await.unwrap();
            assert!(read_handshake(&mut reader).await.is_err());
        }
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
            json!({"action": "navigate_page", "arguments": {(CODEX_SESSION_KEY): "forged"}}),
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

    #[tokio::test]
    async fn versioned_connect_accepts_compatible_adapters_and_restarts_for_newer_ones() {
        let config = Cli::try_parse_from(["browser-mcp", "--headless=true"])
            .expect("parse test configuration")
            .config;
        let broker_version = Version::parse("2.0.0").unwrap();
        for (adapter, restarting) in [
            ("1.9.9", false),
            ("2.0.0-alpha.1", false),
            ("2.0.0", false),
            ("2.0.0+build.1", false),
            ("2.0.1", true),
        ] {
            let adapter_version = Version::parse(adapter).unwrap();
            let (mut client, mut server) = UnixStream::pair().expect("create broker socket pair");
            let server_config = config.clone();
            let server_version = broker_version.clone();
            let serving = tokio::spawn(async move {
                server_handshake(&mut server, &server_config, &server_version).await
            });
            let response = client_handshake(&mut client, false, &config, Some(&adapter_version))
                .await
                .expect("negotiate versioned connection");
            let request = serving
                .await
                .expect("join broker handshake")
                .expect("serve broker handshake");
            if restarting {
                assert_eq!(response, ConnectResponse::Restarting);
                assert!(request.is_none());
            } else {
                assert_eq!(response, ConnectResponse::Accepted);
                assert!(matches!(
                    request,
                    Some(HandshakeRequest::Connect { version, .. })
                        if version == Some(adapter_version)
                ));
            }
        }
    }

    #[tokio::test]
    async fn versioned_connect_distinguishes_legacy_disconnects() {
        let config = Cli::try_parse_from(["browser-mcp", "--headless=true"])
            .expect("parse test configuration")
            .config;
        let (mut client, mut server) = UnixStream::pair().expect("create broker socket pair");
        let serving = tokio::spawn(async move {
            let mut magic = [0; HANDSHAKE_MAGIC.len()];
            server
                .read_exact(&mut magic)
                .await
                .expect("read handshake magic");
            assert_eq!(magic, HANDSHAKE_MAGIC);
            assert_eq!(
                server.read_u8().await.expect("read handshake kind"),
                VERSIONED_CONNECT_HANDSHAKE
            );
        });
        assert_eq!(
            client_handshake(&mut client, false, &config, Some(&Version::new(2, 0, 0)))
                .await
                .expect("detect legacy broker"),
            ConnectResponse::Legacy
        );
        serving.await.expect("join legacy broker");
    }
}
