use std::{
    fs::{self, File, OpenOptions},
    os::unix::{
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use rmcp::{
    ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo,
    },
    service::RequestContext,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, copy},
    net::{UnixListener, UnixStream},
    task::JoinSet,
    time::{Instant, interval, sleep, sleep_until, timeout},
};
use tracing::{error, info, warn};

use super::{BrowserProxy, Config, persistence::OwnershipPersistence, validate_config};

const SOCKET_NAME: &str = "daemon.sock";
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const LAST_CLIENT_GRACE: Duration = Duration::from_secs(2);
const DAEMON_TRANSITION_TIMEOUT: Duration = Duration::from_secs(360);
const START_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const HANDSHAKE_MAGIC: &[u8] = b"opencode-browser-mcp\0v1\n";
const MAX_IDENTITY_BYTES: usize = 8 * 1024;
const STOP_HANDSHAKE: u32 = u32::MAX;

pub(super) async fn run(config: Arc<Config>) -> Result<()> {
    let paths = RuntimePaths::new(config.socket.as_deref())?;
    if config.stop_daemon {
        return stop_daemon(&paths).await;
    }
    let identity = Arc::<[u8]>::from(config.daemon_identity()?);
    if config.daemon {
        validate_config(&config)?;
        return run_daemon(config, paths, identity).await;
    }
    bridge(paths, identity).await
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

async fn bridge(paths: RuntimePaths, identity: Arc<[u8]>) -> Result<()> {
    let mut stream = match connect_or_absent(&paths.socket).await? {
        Some(stream) => stream,
        None => start_daemon_and_connect(&paths).await?,
    };
    client_handshake(&mut stream, &identity).await?;
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
                info!(grace = ?LAST_CLIENT_GRACE, "no OpenCode clients remain; stopping browser daemon");
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
    fs::create_dir_all(profile)
        .with_context(|| format!("failed to create profile directory {}", profile.display()))?;
    validate_private_directory(profile)?;
    let path = profile.join("opencode-browser-session.lock");
    exclusive_lease(&path, "browser profile")
}

fn exclusive_lease(path: &Path, purpose: &str) -> Result<File> {
    let lock = open_private_file(path, PrivateFileOpen::Lock)?
        .expect("lock creation must return an open file");
    lock.try_lock()
        .with_context(|| format!("another daemon owns {purpose} {}", path.display()))?;
    Ok(lock)
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
}

impl ServerHandler for Connection {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_instructions(
                "Controls a Chromium-based browser with pages isolated by OpenCode session."
                    .to_owned(),
            )
    }
    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        let _ = (request, context);
        Ok(ListToolsResult::with_all_items(
            self.proxy.tools.as_ref().clone(),
        ))
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        match self.proxy.route_call(&self.workspace, request).await {
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
    if !server_handshake(&mut stream, &identity).await? {
        return Ok(true);
    }
    let workspace = peer_workspace(pid)?;
    let service = Connection { proxy, workspace }
        .serve(stream)
        .await
        .context("failed to initialize MCP connection")?;
    service.waiting().await.context("MCP connection failed")?;
    Ok(false)
}

async fn client_handshake(stream: &mut UnixStream, identity: &[u8]) -> Result<()> {
    timeout(HANDSHAKE_TIMEOUT, async {
        write_handshake(stream, &HandshakeRequest::Identity(identity.to_vec())).await?;
        match stream.read_u8().await? {
            1 => Ok(()),
            _ => bail!("existing browser daemon uses incompatible configuration"),
        }
    })
    .await
    .context("timed out negotiating with browser daemon")?
}

async fn server_handshake(stream: &mut UnixStream, expected: &[u8]) -> Result<bool> {
    timeout(HANDSHAKE_TIMEOUT, async {
        let HandshakeRequest::Identity(identity) = read_handshake(stream).await? else {
            stream.write_u8(1).await?;
            stream.flush().await?;
            return Ok(false);
        };
        let matches = identity == expected;
        stream.write_u8(u8::from(matches)).await?;
        stream.flush().await?;
        if !matches {
            bail!("client browser configuration does not match the running daemon");
        }
        Ok(true)
    })
    .await
    .context("timed out negotiating browser daemon connection")?
}

enum HandshakeRequest {
    Identity(Vec<u8>),
    Stop,
}

async fn write_handshake(stream: &mut UnixStream, request: &HandshakeRequest) -> Result<()> {
    stream.write_all(HANDSHAKE_MAGIC).await?;
    match request {
        HandshakeRequest::Identity(identity) => {
            if identity.len() > MAX_IDENTITY_BYTES {
                bail!("browser daemon configuration identity is too large");
            }
            stream.write_u32(identity.len() as u32).await?;
            stream.write_all(identity).await?;
        }
        HandshakeRequest::Stop => stream.write_u32(STOP_HANDSHAKE).await?,
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
    let length = length as usize;
    if length > MAX_IDENTITY_BYTES {
        bail!("browser daemon configuration identity is too large");
    }
    let mut identity = vec![0; length];
    stream.read_exact(&mut identity).await?;
    Ok(HandshakeRequest::Identity(identity))
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
    loop {
        if let Some(mut stream) = connect_or_absent(&paths.socket).await? {
            let credentials = stream
                .peer_cred()
                .context("failed to read browser daemon credentials")?;
            if credentials.uid() != unsafe { libc::geteuid() } {
                bail!("refusing to stop daemon owned by another user");
            }
            stop_handshake(&mut stream).await?;
            break;
        }
        if lease_is_available(&paths.lock)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "browser daemon did not become controllable within {DAEMON_TRANSITION_TIMEOUT:?}"
            );
        }
        sleep(Duration::from_millis(50)).await;
    }
    while !lease_is_available(&paths.lock)? {
        if Instant::now() >= deadline {
            bail!("browser daemon did not stop within {DAEMON_TRANSITION_TIMEOUT:?}");
        }
        sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

async fn stop_handshake(stream: &mut UnixStream) -> Result<()> {
    timeout(HANDSHAKE_TIMEOUT, async {
        write_handshake(stream, &HandshakeRequest::Stop).await?;
        if stream.read_u8().await? != 1 {
            bail!("browser daemon rejected stop control connection");
        }
        Ok(())
    })
    .await
    .context("timed out negotiating browser daemon stop")?
}
