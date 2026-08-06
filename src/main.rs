use std::{
    collections::{HashMap, HashSet},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use reqwest::{Client, Url, redirect::Policy};
#[allow(deprecated)]
use rmcp::model::{ListRootsResult, Root};
use rmcp::{
    ClientHandler, RoleClient, ServiceExt,
    model::{
        CallToolRequest, CallToolRequestParams, CallToolResult, ClientCapabilities, ClientInfo,
        ClientRequest, ContentBlock, Implementation, JsonObject, ServerResult, Tool,
    },
    service::{PeerRequestOptions, RequestContext, RunningService},
    transport::TokioChildProcess,
};
use serde_json::{Value, json};
use tokio::{
    process::{Child, Command},
    sync::{Mutex, RwLock},
    time::{sleep, timeout},
};
use tracing::{error, info, warn};

const SESSION_KEY: &str = "__opencode_session_id";
const INTERNAL_URL: &str = "about:blank";
const WORKER_STOP_TIMEOUT: Duration = Duration::from_secs(5);
const WORKER_START_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_SESSION_ID_BYTES: usize = 256;
const MAX_VERSION_RESPONSE_BYTES: usize = 64 * 1024;

fn default_browser_executable() -> PathBuf {
    let mut candidates = vec![
        PathBuf::from("/opt/helium-browser-bin/helium-wrapper"),
        PathBuf::from("/usr/local/bin/helium"),
        PathBuf::from("/usr/bin/helium"),
        PathBuf::from("/usr/bin/chromium"),
        PathBuf::from("/usr/bin/chromium-browser"),
        PathBuf::from("/usr/bin/google-chrome-stable"),
        PathBuf::from("/usr/bin/google-chrome"),
        PathBuf::from("/usr/bin/brave-browser"),
        PathBuf::from("/usr/bin/microsoft-edge-stable"),
    ];
    if let Some(path) = std::env::var_os("PATH") {
        const BROWSERS: &[&str] = &[
            "helium",
            "chromium",
            "chromium-browser",
            "google-chrome-stable",
            "google-chrome",
            "brave-browser",
            "microsoft-edge-stable",
        ];
        for directory in std::env::split_paths(&path) {
            candidates.extend(BROWSERS.iter().map(|browser| directory.join(browser)));
        }
    }
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("chromium"))
}

fn default_browser_profile() -> PathBuf {
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    let executable = std::env::var_os("OPENCODE_BROWSER_EXECUTABLE")
        .map(PathBuf::from)
        .unwrap_or_else(default_browser_executable);
    let name = executable
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    match name {
        "helium" | "helium-wrapper" => config_home.join("net.imput.helium"),
        "google-chrome" | "google-chrome-stable" => config_home.join("google-chrome"),
        "brave-browser" => config_home.join("BraveSoftware/Brave-Browser"),
        "microsoft-edge" | "microsoft-edge-stable" => config_home.join("microsoft-edge"),
        _ => config_home.join("chromium"),
    }
}

mod cdp;
mod daemon;
mod persistence;
use cdp::{TargetId, TargetTracker};
use persistence::{OwnershipPersistence, PersistedSession, RestoredState};

#[derive(Debug, Parser)]
#[command(version, about = "Session-isolated browser MCP for OpenCode")]
struct Config {
    #[arg(long, hide = true, conflicts_with = "stop_daemon")]
    daemon: bool,
    #[arg(long, hide = true, conflicts_with = "daemon")]
    stop_daemon: bool,
    #[arg(long, env = "OPENCODE_BROWSER_SOCKET")]
    socket: Option<PathBuf>,
    #[arg(
        long,
        env = "OPENCODE_BROWSER_EXECUTABLE",
        default_value_os_t = default_browser_executable()
    )]
    browser_executable: PathBuf,
    #[arg(
        long,
        env = "OPENCODE_BROWSER_PROFILE",
        default_value_os_t = default_browser_profile()
    )]
    profile: PathBuf,
    #[arg(long, env = "OPENCODE_BROWSER_DISABLE_CRASH_REPORTER")]
    disable_crash_reporter: bool,
    #[arg(long, env = "OPENCODE_BROWSER_CDP_HOST", default_value = "127.0.0.1")]
    cdp_host: String,
    #[arg(long, env = "OPENCODE_BROWSER_CDP_PORT", default_value_t = 9222)]
    cdp_port: u16,
    #[arg(long, env = "OPENCODE_BROWSER_IDLE_MINUTES", default_value_t = 60)]
    idle_minutes: u64,
    #[arg(long, env = "OPENCODE_BROWSER_STARTUP_SECONDS", default_value_t = 20)]
    startup_seconds: u64,
    #[arg(
        long,
        env = "OPENCODE_BROWSER_OPERATION_SECONDS",
        default_value_t = 540
    )]
    operation_seconds: u64,
    #[arg(
        long,
        env = "OPENCODE_BROWSER_DOWNSTREAM_COMMAND",
        default_value = "bunx"
    )]
    downstream_command: PathBuf,
}

impl Config {
    fn browser_url(&self) -> String {
        format!("http://{}:{}", self.cdp_host, self.cdp_port)
    }

    fn daemon_identity(&self) -> Result<Vec<u8>> {
        fn field(identity: &mut Vec<u8>, value: &[u8]) {
            identity.extend_from_slice(&(value.len() as u64).to_be_bytes());
            identity.extend_from_slice(value);
        }

        let mut identity = Vec::new();
        field(&mut identity, b"daemon-config-v2");
        field(&mut identity, env!("CARGO_PKG_VERSION").as_bytes());
        let executable = std::fs::metadata("/proc/self/exe")
            .context("failed to identify the browser MCP executable")?;
        field(&mut identity, &executable.dev().to_be_bytes());
        field(&mut identity, &executable.ino().to_be_bytes());
        field(
            &mut identity,
            self.browser_executable.as_os_str().as_bytes(),
        );
        field(&mut identity, self.profile.as_os_str().as_bytes());
        field(&mut identity, &[self.disable_crash_reporter.into()]);
        field(&mut identity, self.cdp_host.as_bytes());
        field(&mut identity, &self.cdp_port.to_be_bytes());
        field(&mut identity, &self.idle_minutes.to_be_bytes());
        field(&mut identity, &self.startup_seconds.to_be_bytes());
        field(&mut identity, &self.operation_seconds.to_be_bytes());
        field(
            &mut identity,
            self.downstream_command.as_os_str().as_bytes(),
        );
        Ok(identity)
    }
}

struct BrowserSupervisor {
    config: Arc<Config>,
    client: Client,
    state: Mutex<SupervisorState>,
}

struct SupervisorState {
    process: Option<Child>,
    trusted_generation: Option<String>,
}

impl BrowserSupervisor {
    fn new(config: Arc<Config>, trusted_generation: Option<String>) -> Result<Self> {
        Ok(Self {
            config,
            client: Client::builder()
                .no_proxy()
                .redirect(Policy::none())
                .timeout(Duration::from_millis(500))
                .build()
                .context("failed to configure the browser endpoint client")?,
            state: Mutex::new(SupervisorState {
                process: None,
                trusted_generation,
            }),
        })
    }

    async fn endpoint_identity(&self) -> Result<Option<String>> {
        let response = match self
            .client
            .get(format!("{}/json/version", self.config.browser_url()))
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => response,
            _ => return Ok(None),
        };
        let body = response
            .bytes()
            .await
            .context("failed to read browser /json/version response")?;
        if body.len() > MAX_VERSION_RESPONSE_BYTES {
            bail!("browser /json/version response is too large");
        }
        let version: Value =
            serde_json::from_slice(&body).context("invalid browser /json/version response")?;
        let identity = version
            .get("webSocketDebuggerUrl")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("browser /json/version has no webSocketDebuggerUrl"))?;
        let endpoint = Url::parse(identity).context("invalid browser WebSocket debugger URL")?;
        let loopback_host = matches!(endpoint.host_str(), Some("127.0.0.1" | "localhost"));
        if !matches!(endpoint.scheme(), "ws" | "wss")
            || !loopback_host
            || endpoint.port_or_known_default() != Some(self.config.cdp_port)
        {
            bail!("browser returned an unexpected WebSocket debugger URL: {identity}");
        }
        Ok(Some(identity.to_owned()))
    }

    async fn ensure_running(&self) -> Result<String> {
        let mut state = self.state.lock().await;
        let endpoint = self.endpoint_identity().await?;
        if let Some(child) = state.process.as_mut()
            && child
                .try_wait()
                .context("failed to inspect browser process")?
                .is_some()
        {
            state.process = None;
        }
        if let Some(identity) = endpoint {
            if state.process.is_none()
                && state.trusted_generation.as_deref() != Some(identity.as_str())
            {
                bail!(
                    "refusing to adopt an unrecognized browser CDP endpoint at {}",
                    self.config.browser_url()
                );
            }
            state.trusted_generation = Some(identity.clone());
            return Ok(identity);
        }
        if state.process.is_none() {
            let mut command = Command::new(&self.config.browser_executable);
            command
                .arg(format!("--user-data-dir={}", self.config.profile.display()))
                .arg(format!(
                    "--remote-debugging-address={}",
                    self.config.cdp_host
                ))
                .arg(format!("--remote-debugging-port={}", self.config.cdp_port))
                .arg("--no-first-run")
                .arg("--no-default-browser-check")
                .arg("--disable-popup-blocking")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            if self.config.disable_crash_reporter {
                command
                    .arg("--disable-breakpad")
                    .arg("--disable-crash-reporter");
            }
            info!(executable = %self.config.browser_executable.display(), profile = %self.config.profile.display(), "starting browser");
            state.process = Some(command.spawn().context("failed to start browser")?);
        }
        match self.wait_until_ready().await {
            Ok(identity) => {
                state.trusted_generation = Some(identity.clone());
                Ok(identity)
            }
            Err(error) => {
                if let Some(mut child) = state.process.take() {
                    warn!("tracked browser did not become ready; killing it");
                    terminate_child(&mut child).await;
                }
                Err(error)
            }
        }
    }

    async fn wait_until_ready(&self) -> Result<String> {
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(self.config.startup_seconds))
            .ok_or_else(|| anyhow!("browser startup timeout is too large"))?;
        while Instant::now() < deadline {
            if let Some(identity) = self.endpoint_identity().await? {
                info!(url = %self.config.browser_url(), "browser CDP endpoint is ready");
                return Ok(identity);
            }
            sleep(Duration::from_millis(200)).await;
        }
        bail!(
            "browser CDP endpoint {} did not become ready",
            self.config.browser_url()
        )
    }

    async fn wait_for_browser_exit(&self) -> Result<()> {
        let deadline = Instant::now() + WORKER_STOP_TIMEOUT;
        let endpoint_still_running = loop {
            match self.endpoint_identity().await? {
                Some(_) if Instant::now() < deadline => {
                    sleep(Duration::from_millis(100)).await;
                }
                Some(_) => break true,
                None => break false,
            }
        };

        let Some(mut child) = self.state.lock().await.process.take() else {
            if endpoint_still_running {
                bail!("adopted browser did not exit after Browser.close");
            }
            return Ok(());
        };
        match child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) if endpoint_still_running => terminate_child(&mut child).await,
            Ok(None) => {
                if timeout(WORKER_STOP_TIMEOUT, child.wait()).await.is_err() {
                    warn!("timed out reaping browser after Browser.close; killing it");
                    terminate_child(&mut child).await;
                }
            }
            Err(error) => return Err(error).context("failed to inspect browser during shutdown"),
        }
        if self.endpoint_identity().await?.is_some() {
            bail!("browser remained reachable after shutdown");
        }
        Ok(())
    }
}

async fn terminate_child(child: &mut Child) {
    if let Err(error) = child.start_kill() {
        warn!(%error, "failed to signal child process");
        return;
    }
    if timeout(WORKER_STOP_TIMEOUT, child.wait()).await.is_err() {
        warn!("timed out reaping child process");
    }
}

#[derive(Clone)]
struct DownstreamClient {
    root_uri: String,
}

#[allow(deprecated)]
impl ClientHandler for DownstreamClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::new(
            ClientCapabilities::builder().enable_roots().build(),
            Implementation::new("opencode-browser-mcp", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn list_roots(
        &self,
        _: RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, rmcp::ErrorData> {
        Ok(ListRootsResult::new(vec![Root::new(&self.root_uri)]))
    }
}

type Downstream = RunningService<RoleClient, DownstreamClient>;
struct Session {
    persisted: PersistedSession,
    /// Worker-local numeric IDs are merely routes.  TargetTracker is the
    /// ownership authority; this map is rebuilt from it on every reconcile.
    pages: PageState,
    downstream: Option<Downstream>,
}

impl Session {
    fn needs_cleanup(&self, now: u64, idle: Duration) -> bool {
        self.persisted.cleanup_pending
            || (!idle.is_zero()
                && now.saturating_sub(self.persisted.last_activity_unix_seconds) >= idle.as_secs())
    }
}

#[derive(Default)]
struct PageState {
    local_to_tab: HashMap<u64, TargetId>,
}
type SessionSlot = Arc<Mutex<Option<Session>>>;
type Sessions = Mutex<HashMap<String, SessionSlot>>;

fn unix_time_seconds() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs())
}

async fn restore_sessions(
    tracker: &TargetTracker,
    restored: RestoredState,
) -> Result<HashMap<String, SessionSlot>> {
    let mut sessions = HashMap::new();
    for (id, persisted) in restored.sessions {
        let owned = tracker.owned_tabs(&id).await?;
        if owned.is_empty() {
            continue;
        }
        let current_tab = persisted
            .current_tab
            .filter(|tab| owned.iter().any(|owned| owned == tab));
        sessions.insert(
            id,
            Arc::new(Mutex::new(Some(Session {
                persisted: PersistedSession {
                    current_tab,
                    ..persisted
                },
                pages: PageState::default(),
                downstream: None,
            }))),
        );
    }
    Ok(sessions)
}

async fn restore_tracker(
    identity: String,
    persistence: Arc<OwnershipPersistence>,
) -> Result<(TargetTracker, RestoredState)> {
    let restored = persistence.activate_generation(&identity)?;
    let tracker = TargetTracker::connect(
        identity,
        persistence,
        restored.owners.clone(),
        restored.owner_revision,
    )
    .await?;
    Ok((tracker, restored))
}

async fn spawn_downstream(config: &Config, workspace: &std::path::Path) -> Result<Downstream> {
    let mut command = Command::new(&config.downstream_command);
    command
        .arg("chrome-devtools-mcp@1.6.0")
        .arg("--browser-url")
        .arg(config.browser_url())
        .arg("--experimental-page-id-routing")
        .arg("--experimental-interop-tools")
        .arg("--experimental-structured-content")
        .arg("--no-usage-statistics")
        .current_dir(workspace)
        .stderr(Stdio::inherit());
    let transport =
        TokioChildProcess::new(command).context("failed to start Chrome DevTools MCP")?;
    let root_uri = Url::from_directory_path(workspace)
        .map_err(|_| anyhow!("failed to convert {} to a file URL", workspace.display()))?
        .to_string();
    timeout(
        WORKER_START_TIMEOUT,
        DownstreamClient { root_uri }.serve(transport),
    )
    .await
    .context("timed out initializing Chrome DevTools MCP")?
    .context("failed to initialize Chrome DevTools MCP")
}

async fn stop_worker(worker: Downstream) -> Result<()> {
    timeout(WORKER_STOP_TIMEOUT, worker.cancel())
        .await
        .context("timed out stopping Chrome DevTools MCP")?
        .context("failed to stop Chrome DevTools MCP")?;
    Ok(())
}

#[derive(Clone)]
struct BrowserProxy {
    config: Arc<Config>,
    supervisor: Arc<BrowserSupervisor>,
    tools: Arc<Vec<Tool>>,
    exposed_tools: Arc<HashSet<String>>,
    page_scoped: Arc<HashSet<String>>,
    sessions: Arc<Sessions>,
    tracker: Arc<RwLock<TargetTracker>>,
    idle: Duration,
    operation_timeout: Duration,
    persistence: Arc<OwnershipPersistence>,
}

impl BrowserProxy {
    async fn new(config: Arc<Config>, persistence: Arc<OwnershipPersistence>) -> Result<Self> {
        let supervisor = Arc::new(BrowserSupervisor::new(
            config.clone(),
            persistence.generation(),
        )?);
        // The tracker must be connected before tool discovery: discovery is a
        // browser operation too and must use the same endpoint generation.
        let identity = supervisor.ensure_running().await?;
        let (tracker, restored) = restore_tracker(identity, persistence.clone()).await?;
        let workspace =
            std::env::current_dir().context("failed to determine the daemon workspace")?;
        let worker = spawn_downstream(&config, &workspace).await?;
        let listed = timeout(WORKER_START_TIMEOUT, worker.list_all_tools())
            .await
            .map_err(|_| anyhow!("timed out retrieving Chrome DevTools tools"))
            .and_then(|result| result.context("failed to retrieve Chrome DevTools tools"));
        let stopped = stop_worker(worker).await;
        let listed = listed?;
        stopped?;
        let mut page_scoped = HashSet::new();
        let mut has_get_tab_id = false;
        let mut tools = Vec::with_capacity(listed.len());
        for mut tool in listed {
            if tool.name == "select_page" || tool.name == "get_tab_id" {
                has_get_tab_id |= tool.name == "get_tab_id";
                continue;
            }
            let mut schema = (*tool.input_schema).clone();
            if remove_schema_property(&mut schema, "pageId") {
                page_scoped.insert(tool.name.to_string());
            }
            tool.input_schema = Arc::new(schema);
            tools.push(tool);
        }
        if !has_get_tab_id {
            bail!("Chrome DevTools MCP does not provide required get_tab_id capability");
        }
        let exposed_tools = tools.iter().map(|tool| tool.name.to_string()).collect();
        let sessions = restore_sessions(&tracker, restored).await?;
        let proxy = Self {
            config: config.clone(),
            supervisor,
            tools: Arc::new(tools),
            exposed_tools: Arc::new(exposed_tools),
            page_scoped: Arc::new(page_scoped),
            sessions: Arc::new(Mutex::new(sessions)),
            tracker: Arc::new(RwLock::new(tracker)),
            idle: Duration::from_secs(config.idle_minutes.saturating_mul(60)),
            operation_timeout: Duration::from_secs(config.operation_seconds),
            persistence,
        };
        Ok(proxy)
    }

    fn cleanup_cadence(&self) -> Duration {
        if self.idle.is_zero() {
            Duration::from_secs(60)
        } else {
            self.idle.min(Duration::from_secs(60))
        }
    }

    async fn tracker_lease(&self) -> Result<tokio::sync::RwLockReadGuard<'_, TargetTracker>> {
        loop {
            let read = self.tracker.read().await;
            if let Some(identity) = self.supervisor.endpoint_identity().await?
                && read.generation() == identity
                && read.is_available().await
            {
                return Ok(read);
            }
            drop(read);
            let mut write = self.tracker.write().await;
            let identity = self.supervisor.ensure_running().await?;
            let generation_changed = write.generation() != identity;
            let tracker_unavailable = !generation_changed && !write.is_available().await;
            if generation_changed || tracker_unavailable {
                if generation_changed {
                    warn!("browser endpoint identity changed; retiring all session workers");
                } else {
                    warn!("CDP target tracker disconnected; restoring target ownership");
                }
                if generation_changed {
                    self.retire_all_sessions().await;
                }
                write.shutdown().await;
                let (tracker, _) =
                    restore_tracker(identity.clone(), self.persistence.clone()).await?;
                *write = tracker;
            }
        }
    }

    async fn retire_all_sessions(&self) {
        let slots = self.sessions.lock().await.drain().collect::<Vec<_>>();
        for (id, slot) in slots {
            if let Some(session) = slot.lock().await.take() {
                // Targets belong to the previous endpoint, so never close them here.
                self.stop_session(&id, session).await;
            }
        }
    }

    async fn slot(&self, id: &str) -> SessionSlot {
        let mut sessions = self.sessions.lock().await;
        sessions
            .entry(id.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone()
    }

    async fn prune_slot(&self, id: &str, slot: &SessionSlot) {
        let mut sessions = self.sessions.lock().await;
        let removable = sessions
            .get(id)
            .is_some_and(|registered| Arc::ptr_eq(registered, slot))
            && Arc::strong_count(slot) == 2
            && slot.try_lock().is_ok_and(|session| session.is_none());
        if removable {
            sessions.remove(id);
        }
    }

    async fn route_call(
        &self,
        workspace: &std::path::Path,
        mut request: CallToolRequestParams,
    ) -> Result<CallToolResult> {
        let arguments = request.arguments.get_or_insert_default();
        let id = arguments
            .remove(SESSION_KEY)
            .and_then(|v| v.as_str().map(str::to_owned))
            .filter(|id| !id.is_empty() && id.len() <= MAX_SESSION_ID_BYTES)
            .ok_or_else(|| anyhow!("browser call is missing the OpenCode session identifier"))?;
        if !self.exposed_tools.contains(request.name.as_ref()) {
            bail!("browser tool is not exposed: {}", request.name);
        }
        let tracker_lease = self.tracker_lease().await?; // Must outlive every browser operation below.
        let tracker = &*tracker_lease;
        let slot = self.slot(&id).await;
        let mut guard = slot.lock().await;
        if guard.is_none() {
            tracker.activate_session(&id).await?;
            let started = spawn_downstream(&self.config, workspace).await;
            match started {
                Ok(downstream) => {
                    *guard = Some(Session {
                        persisted: PersistedSession {
                            workspace: workspace.to_path_buf(),
                            current_tab: None,
                            last_activity_unix_seconds: unix_time_seconds()?,
                            cleanup_pending: false,
                        },
                        pages: PageState::default(),
                        downstream: Some(downstream),
                    });
                    if let Err(error) =
                        self.persist_session(&id, guard.as_ref().expect("session exists"))
                    {
                        let session = guard.take().expect("session exists");
                        self.stop_session(&id, session).await;
                        drop(guard);
                        self.prune_slot(&id, &slot).await;
                        return Err(error);
                    }
                }
                Err(error) => {
                    drop(guard);
                    self.prune_slot(&id, &slot).await;
                    return Err(error);
                }
            }
        }
        if guard
            .as_ref()
            .is_some_and(|session| session.persisted.workspace != workspace)
        {
            error!(session_id = %id, existing_workspace = %guard.as_ref().expect("session checked").persisted.workspace.display(), requested_workspace = %workspace.display(), "session identifier was reused from a different workspace");
            bail!("browser session identifier is already bound to a different workspace");
        }
        if guard
            .as_ref()
            .is_some_and(|session| session.persisted.cleanup_pending)
        {
            bail!("browser session is pending owned-tab cleanup");
        }
        if guard
            .as_ref()
            .is_some_and(|session| session.downstream.is_none())
        {
            let session = guard.as_mut().expect("session exists");
            session.downstream = Some(spawn_downstream(&self.config, workspace).await?);
        }
        let closes_page = request.name == "close_page";
        let mut result = self
            .call_session(
                tracker,
                &id,
                guard.as_mut().expect("session was just started"),
                request,
            )
            .await;
        if let Some(session) = guard.as_mut() {
            session.persisted.last_activity_unix_seconds = unix_time_seconds()?;
            if let Err(error) = self.persist_session(&id, session) {
                result = Err(error);
            }
        }
        if closes_page
            && result
                .as_ref()
                .is_ok_and(|result| !result.is_error.unwrap_or(false))
            && guard
                .as_ref()
                .is_some_and(|session| session.persisted.current_tab.is_none())
            && let Err(error) = self.finalize_session(&id, tracker, &mut guard).await
        {
            result = Err(error.context("failed to finalize closed browser session"));
        }
        if let Err(error) = &result
            && guard.is_some()
        {
            error!(%error, session_id = %id, "retiring failed Chrome DevTools worker");
            let cleanup_marked = self
				.mark_cleanup_pending(&id, guard.as_mut().expect("session exists"))
				.map_err(|persist_error| {
					warn!(%persist_error, session_id = %id, "failed to persist cleanup-pending browser session");
					persist_error
				})
				.is_ok();
            if let Some(session) = guard.as_mut() {
                if let Some(worker) = session.downstream.take()
                    && let Err(stop_error) = stop_worker(worker).await
                {
                    warn!(%stop_error, session_id = %id, "failed to stop failed Chrome DevTools worker");
                }
                session.pages = PageState::default();
            }
            let cleanup_succeeded = cleanup_marked
                && match self.close_owned_pages(&id, tracker).await {
                    Ok(()) => true,
                    Err(cleanup_error) => {
                        warn!(%cleanup_error, session_id = %id, "owned tabs remain queued for cleanup");
                        false
                    }
                };
            if cleanup_succeeded
                && let Err(finalize_error) = self.finalize_session(&id, tracker, &mut guard).await
            {
                warn!(%finalize_error, session_id = %id, "browser session retirement failed");
            }
        }
        drop(guard);
        self.prune_slot(&id, &slot).await;
        result
    }

    async fn stop_session(&self, id: &str, session: Session) {
        if let Some(worker) = session.downstream
            && let Err(error) = stop_worker(worker).await
        {
            warn!(%error, session_id = %id, "failed to stop retired Chrome DevTools MCP worker");
        }
    }

    async fn finalize_session(
        &self,
        id: &str,
        tracker: &TargetTracker,
        slot: &mut Option<Session>,
    ) -> Result<()> {
        tracker.finalize_session(id).await?;
        let persisted = self.persistence.remove_session_if_unowned(id);
        let session = slot
            .take()
            .ok_or_else(|| anyhow!("browser session disappeared during finalization"))?;
        self.stop_session(id, session).await;
        persisted.context("failed to remove persisted browser session")
    }

    fn persist_session(&self, id: &str, session: &Session) -> Result<()> {
        self.persistence
            .update_session(id, session.persisted.clone())
    }

    fn mark_cleanup_pending(&self, id: &str, session: &mut Session) -> Result<()> {
        let mut persisted = session.persisted.clone();
        persisted.cleanup_pending = true;
        let result = self.persistence.update_session(id, persisted.clone());
        // A storage failure cannot provide crash durability, but the live
        // daemon must still reject work and retry cleanup rather than fail open.
        session.persisted = persisted;
        result
    }

    async fn call_session(
        &self,
        tracker: &TargetTracker,
        id: &str,
        session: &mut Session,
        mut request: CallToolRequestParams,
    ) -> Result<CallToolResult> {
        let result = match request.name.as_ref() {
            "new_page" => {
                self.create_page(
                    tracker,
                    id,
                    session,
                    request.arguments.take().unwrap_or_default(),
                )
                .await?
            }
            "close_page" => {
                self.ensure_page(tracker, id, session).await?;
                self.close_page(tracker, id, session).await?
            }
            _ => {
                let page = self.ensure_page(tracker, id, session).await?;
                if self.page_scoped.contains(request.name.as_ref()) {
                    request
                        .arguments
                        .get_or_insert_default()
                        .insert("pageId".into(), page.into());
                }
                let result = self.call_downstream(session, request).await?;
                // A successful worker result is not safe to return unless its
                // post-call ownership reconciliation also succeeded.
                self.reconcile(tracker, id, session).await?;
                result
            }
        };
        filter_page_content(
            result,
            &session.pages.local_to_tab.keys().copied().collect(),
        )
    }

    async fn call_downstream(
        &self,
        session: &Session,
        request: CallToolRequestParams,
    ) -> Result<CallToolResult> {
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(request));
        let response = session
            .downstream
            .as_ref()
            .ok_or_else(|| anyhow!("Chrome DevTools worker is not running"))?
            .send_cancellable_request(
                request,
                PeerRequestOptions::with_timeout(self.operation_timeout),
            )
            .await
            .context("failed to send Chrome DevTools tool call")?
            .await_response()
            .await
            .context("Chrome DevTools tool call failed")?;
        match response {
            ServerResult::CallToolResult(result) => Ok(result),
            _ => bail!("Chrome DevTools returned an unexpected tool response"),
        }
    }
    async fn list_snapshot(&self, session: &Session) -> Result<(HashSet<u64>, Option<u64>)> {
        let result = self
            .call_downstream(
                session,
                CallToolRequestParams::new("list_pages").with_arguments(JsonObject::new()),
            )
            .await?;
        parse_pages(&result).with_context(|| format!("invalid list_pages response: {result:?}"))
    }
    async fn tab_id(&self, session: &Session, page: u64) -> Result<Option<TargetId>> {
        let result = self
            .call_downstream(
                session,
                CallToolRequestParams::new("get_tab_id")
                    .with_arguments(json!({"pageId": page}).as_object().expect("object").clone()),
            )
            .await?;
        if result.is_error.unwrap_or(false) {
            return Ok(None);
        }
        parse_tab_id(&result)
            .map(Some)
            .with_context(|| format!("get_tab_id returned no structured tabId for page {page}"))
    }
    async fn reconcile(
        &self,
        tracker: &TargetTracker,
        id: &str,
        session: &mut Session,
    ) -> Result<()> {
        self.reconcile_with_current(tracker, id, session, None)
            .await
    }
    /// Current-page policy is deterministic: an explicitly created target
    /// wins, then the prior live target, then the downstream selected owned
    /// tab, then lexicographically smallest stable target ID. Popup discovery
    /// and stale worker selection never replace a live canonical current.
    async fn reconcile_with_current(
        &self,
        tracker: &TargetTracker,
        id: &str,
        session: &mut Session,
        explicit_current: Option<&str>,
    ) -> Result<()> {
        tracker.synchronize().await?;
        let (pages, selected) = self.list_snapshot(session).await?;
        let mut mappings = HashMap::new();
        for page in pages {
            let Some(tab) = self.tab_id(session, page).await? else {
                continue;
            };
            if tracker.is_live_tab(&tab).await?
                && tracker.owner_of(&tab).await?.as_deref() == Some(id)
            {
                mappings.insert(page, tab);
            }
        }
        let selected_tab = selected.and_then(|page| mappings.get(&page).cloned());
        let current_live = session
            .persisted
            .current_tab
            .as_ref()
            .is_some_and(|tab| mappings.values().any(|mapped| mapped == tab));
        session.pages.local_to_tab = mappings;
        session.persisted.current_tab = explicit_current
            .filter(|tab| {
                session
                    .pages
                    .local_to_tab
                    .values()
                    .any(|mapped| mapped == *tab)
            })
            .map(str::to_owned)
            .or_else(|| {
                current_live
                    .then(|| session.persisted.current_tab.clone())
                    .flatten()
            })
            .or(selected_tab)
            .or_else(|| session.pages.local_to_tab.values().min().cloned());
        Ok(())
    }
    async fn ensure_page(
        &self,
        tracker: &TargetTracker,
        id: &str,
        session: &mut Session,
    ) -> Result<u64> {
        self.reconcile(tracker, id, session).await?;
        if let Some(tab) = &session.persisted.current_tab
            && let Some(page) = session
                .pages
                .local_to_tab
                .iter()
                .find_map(|(page, mapped)| (mapped == tab).then_some(*page))
        {
            return Ok(page);
        }
        let result = self
            .create_page(
                tracker,
                id,
                session,
                json!({"url": INTERNAL_URL, "background": true})
                    .as_object()
                    .expect("object")
                    .clone(),
            )
            .await?;
        if result.is_error.unwrap_or(false) {
            bail!("Chrome DevTools failed to create a session page");
        }
        session
            .pages
            .local_to_tab
            .iter()
            .find_map(|(page, tab)| {
                (Some(tab) == session.persisted.current_tab.as_ref()).then_some(*page)
            })
            .ok_or_else(|| anyhow!("Chrome DevTools did not report the new session page for {id}"))
    }
    async fn create_page(
        &self,
        tracker: &TargetTracker,
        id: &str,
        session: &mut Session,
        arguments: JsonObject,
    ) -> Result<CallToolResult> {
        if arguments.contains_key("isolatedContext") {
            bail!("isolatedContext is not supported for session-isolated pages");
        }
        let url = match arguments.get("url") {
            Some(Value::String(url)) => url.as_str(),
            Some(_) => bail!("new_page url must be a string"),
            None => INTERNAL_URL,
        };
        let background = match arguments.get("background") {
            Some(Value::Bool(background)) => *background,
            Some(_) => bail!("new_page background must be a boolean"),
            None => false,
        };
        let tab = tracker
            .create_owned_tab(id, INTERNAL_URL, background)
            .await?;
        let established = timeout(WORKER_STOP_TIMEOUT, async {
            loop {
                self.reconcile_with_current(tracker, id, session, Some(&tab))
                    .await?;
                if session
                    .pages
                    .local_to_tab
                    .values()
                    .any(|mapped| mapped == &tab)
                    && session.persisted.current_tab.as_deref() == Some(&tab)
                {
                    return Ok(());
                }
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await;
        if established.is_err() || established.as_ref().is_ok_and(|result| result.is_err()) {
            if let Err(error) = tracker.close_owned_target(id, &tab).await {
                warn!(%error, tab, "failed to clean up unreported newly created tab");
            }
            match established {
                Ok(Err(error)) => return Err(error),
                Err(_) => bail!("timed out reconciling exact CDP-created page"),
                Ok(Ok(())) => unreachable!("successful reconciliation was handled above"),
            }
        }
        if url != INTERNAL_URL {
            let page = session
                .pages
                .local_to_tab
                .iter()
                .find_map(|(page, mapped)| (mapped == &tab).then_some(*page))
                .ok_or_else(|| anyhow!("newly created tab has no worker-local page ID"))?;
            let result = self
                .call_downstream(
                    session,
                    CallToolRequestParams::new("navigate_page").with_arguments(
                        json!({"type": "url", "url": url, "pageId": page})
                            .as_object()
                            .expect("object")
                            .clone(),
                    ),
                )
                .await?;
            self.reconcile_with_current(tracker, id, session, Some(&tab))
                .await?;
            if result.is_error.unwrap_or(false) {
                return Ok(result);
            }
        }
        // Preserve useful new_page behavior without exposing the target ID:
        // return the worker's ordinary page listing after it maps to this tab.
        self.call_downstream(
            session,
            CallToolRequestParams::new("list_pages").with_arguments(JsonObject::new()),
        )
        .await
    }
    async fn close_page(
        &self,
        tracker: &TargetTracker,
        id: &str,
        session: &mut Session,
    ) -> Result<CallToolResult> {
        let tab = session
            .persisted
            .current_tab
            .clone()
            .ok_or_else(|| anyhow!("the current page is not owned by this OpenCode session"))?;
        tracker.close_owned_target(id, &tab).await?;
        self.reconcile(tracker, id, session).await?;
        Ok(CallToolResult::success(vec![ContentBlock::text(
            "Closed current page",
        )]))
    }
    async fn close_owned_pages(&self, id: &str, tracker: &TargetTracker) -> Result<()> {
        let tabs = tracker.owned_tabs(id).await?;
        let mut failed = false;
        for tab in tabs {
            match timeout(WORKER_STOP_TIMEOUT, tracker.close_owned_target(id, &tab)).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    failed = true;
                    warn!(%error, session_id = %id, tab, "failed to close owned tab")
                }
                Err(_) => {
                    failed = true;
                    warn!(session_id = %id, tab, "timed out closing owned tab")
                }
            }
        }
        if failed {
            bail!("one or more owned tabs could not be closed");
        }
        Ok(())
    }
    async fn cleanup_idle_sessions(&self) -> Result<()> {
        let now = unix_time_seconds()?;
        let slots = self
            .sessions
            .lock()
            .await
            .iter()
            .map(|(id, slot)| (id.clone(), slot.clone()))
            .collect::<Vec<_>>();
        let mut idle_slots = Vec::new();
        for (id, slot) in slots {
            let Ok(guard) = slot.try_lock() else {
                continue;
            };
            if guard
                .as_ref()
                .is_some_and(|session| session.needs_cleanup(now, self.idle))
            {
                idle_slots.push((id, slot.clone()));
            }
        }
        if idle_slots.is_empty() {
            return Ok(());
        }
        let tracker_lease = self.tracker_lease().await?;
        let tracker = &*tracker_lease;
        for (id, slot) in idle_slots {
            let Ok(mut guard) = slot.try_lock() else {
                continue;
            };
            let Some(session) = guard.as_mut() else {
                continue;
            };
            if !session.needs_cleanup(now, self.idle) {
                continue;
            }
            if let Err(error) = self.mark_cleanup_pending(&id, session) {
                warn!(%error, session_id = %id, "failed to persist idle cleanup intent");
                continue;
            }
            if let Err(error) = self.close_owned_pages(&id, tracker).await {
                warn!(%error, session_id = %id, "failed to close idle browser session tabs");
                continue;
            }
            match self.finalize_session(&id, tracker, &mut guard).await {
                Ok(()) => info!(session_id = %id, "cleaned up idle browser session"),
                Err(error) => {
                    warn!(%error, session_id = %id, "idle session retirement failed")
                }
            }
            drop(guard);
            self.prune_slot(&id, &slot).await;
        }
        Ok(())
    }

    async fn shutdown(&self) -> Result<()> {
        let slots = self.sessions.lock().await.drain().collect::<Vec<_>>();
        let mut sessions = Vec::new();
        for (id, slot) in slots {
            if let Some(session) = slot.lock().await.take() {
                sessions.push((id, session));
            }
        }
        futures_util::future::join_all(
            sessions
                .into_iter()
                .map(|(id, session)| async move { self.stop_session(&id, session).await }),
        )
        .await;
        let tracker = self.tracker.read().await.clone();
        let generation = tracker.generation().to_owned();
        if let Err(error) = tracker.close_browser().await {
            warn!(%error, "failed to request browser shutdown through CDP");
        }
        let stopped = self.supervisor.wait_for_browser_exit().await;
        tracker.shutdown().await;
        stopped?;
        self.persistence.clear_generation(&generation)
    }
}

fn remove_schema_property(schema: &mut JsonObject, property: &str) -> bool {
    let removed = schema
        .get_mut("properties")
        .and_then(Value::as_object_mut)
        .and_then(|properties| properties.remove(property))
        .is_some();
    if let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) {
        required.retain(|value| value.as_str() != Some(property));
    }
    removed
}
fn content_text(content: &ContentBlock) -> Option<&str> {
    content.as_text().map(|text| text.text.as_str())
}

fn parse_tab_id(result: &CallToolResult) -> Result<TargetId> {
    result
        .structured_content
        .as_ref()
        .and_then(|value| value.get("tabId"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("missing nonempty structured_content.tabId"))
}

fn page_line(line: &str) -> Result<(u64, String, bool)> {
    let line = line.trim();
    let (id, text) = line
        .split_once(": ")
        .ok_or_else(|| anyhow!("malformed page line: {line}"))?;
    let id = id
        .parse()
        .with_context(|| format!("invalid page ID in {line}"))?;
    let mut text = text.to_owned();
    let selected = text.rfind(" [selected]").is_some_and(|position| {
        let suffix = &text[position + " [selected]".len()..];
        suffix.is_empty() || suffix.starts_with(" isolatedContext=")
    });
    if selected {
        let position = text.rfind(" [selected]").expect("selected marker exists");
        text.replace_range(position..position + " [selected]".len(), "");
    }
    if text.is_empty() {
        bail!("malformed page line: {line}");
    }
    Ok((id, text, selected))
}

fn structured_page_snapshot(
    result: &CallToolResult,
) -> Result<Option<(HashSet<u64>, Option<u64>)>> {
    let Some(structured) = result.structured_content.as_ref() else {
        return Ok(None);
    };
    let Some(pages) = structured.get("pages") else {
        return Ok(None);
    };
    let pages = pages
        .as_array()
        .ok_or_else(|| anyhow!("structured page list is not an array"))?;
    let mut ids = HashSet::new();
    let mut selected = None;
    for page in pages {
        let page = page
            .as_object()
            .ok_or_else(|| anyhow!("structured page entry is not an object"))?;
        let id = page
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("structured page entry has no numeric ID"))?;
        if !ids.insert(id) {
            bail!("duplicate page ID {id}");
        }
        if page.get("selected").and_then(Value::as_bool) == Some(true)
            && selected.replace(id).is_some()
        {
            bail!("structured page list has multiple selected pages");
        }
    }
    Ok(Some((ids, selected)))
}

fn parse_pages(result: &CallToolResult) -> Result<(HashSet<u64>, Option<u64>)> {
    if let Some(snapshot) = structured_page_snapshot(result)? {
        return Ok(snapshot);
    }
    let mut pages = HashSet::new();
    let mut selected = None;
    let mut found = false;
    for content in &result.content {
        let Some(text) = content_text(content) else {
            continue;
        };
        let mut inside = false;
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed == "## Pages" || trimmed == "Pages:" {
                inside = true;
                found = true;
                continue;
            }
            if inside && trimmed.starts_with("## ") {
                inside = false;
            }
            if inside && !trimmed.is_empty() {
                let (id, _, is_selected) = page_line(line)?;
                if !pages.insert(id) {
                    bail!("duplicate page ID {id}");
                }
                if is_selected {
                    selected = Some(id);
                }
            }
        }
    }
    if !found {
        bail!("Chrome DevTools did not return a page section");
    }
    Ok((pages, selected))
}
fn filter_page_content(
    mut result: CallToolResult,
    allowed: &HashSet<u64>,
) -> Result<CallToolResult> {
    let structured_pages_are_authoritative = result
        .structured_content
        .as_ref()
        .and_then(|structured| structured.get("pages"))
        .is_some();
    if let Some(structured) = result.structured_content.as_mut()
        && let Some(pages) = structured.get_mut("pages")
    {
        let pages = pages
            .as_array_mut()
            .ok_or_else(|| anyhow!("structured page list is not an array"))?;
        let mut filtered = Vec::new();
        for page in std::mem::take(pages) {
            let mut page = page
                .as_object()
                .cloned()
                .ok_or_else(|| anyhow!("structured page entry is not an object"))?;
            let id = page
                .remove("id")
                .and_then(|id| id.as_u64())
                .ok_or_else(|| anyhow!("structured page entry has no numeric ID"))?;
            if allowed.contains(&id) {
                filtered.push(Value::Object(page));
            }
        }
        *pages = filtered;
    }
    for content in &mut result.content {
        let ContentBlock::Text(text) = content else {
            continue;
        };
        let mut inside = false;
        let mut filtered = Vec::new();
        for line in text.text.lines() {
            let trimmed = line.trim();
            if trimmed == "## Pages" || trimmed == "Pages:" {
                inside = true;
                filtered.push(line.to_owned());
                continue;
            }
            if inside && trimmed.starts_with("## ") {
                inside = false;
            }
            if inside && !trimmed.is_empty() {
                if structured_pages_are_authoritative {
                    continue;
                }
                let (id, page, selected) = page_line(line)?;
                if allowed.contains(&id) {
                    filtered.push(if selected {
                        format!("- {page} [selected]")
                    } else {
                        format!("- {page}")
                    });
                }
                continue;
            }
            filtered.push(line.to_owned());
        }
        text.text = filtered.join("\n");
    }
    Ok(result)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "opencode_browser_mcp=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let config = Arc::new(Config::parse());
    daemon::run(config).await
}
fn validate_config(config: &Config) -> Result<()> {
    if !config.browser_executable.is_file() {
        bail!(
            "browser executable does not exist: {}",
            config.browser_executable.display()
        );
    }
    if config.cdp_host != "127.0.0.1" && config.cdp_host != "localhost" {
        bail!("CDP must be bound to localhost, got {}", config.cdp_host);
    }
    if config.idle_minutes == 0 {
        warn!("idle expiration is disabled; cleanup-pending sessions will still be retried");
    }
    if config.startup_seconds == 0 || config.startup_seconds > 300 {
        bail!("browser startup timeout must be between 1 and 300 seconds");
    }
    if config.operation_seconds == 0 || config.operation_seconds > 3600 {
        bail!("browser operation timeout must be between 1 and 3600 seconds");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn removes_page_id_from_schema_and_required_fields() {
        let mut schema = json!({"type":"object","properties":{"pageId":{"type":"number"},"url":{"type":"string"}},"required":["pageId","url"]}).as_object().unwrap().clone();
        assert!(remove_schema_property(&mut schema, "pageId"));
        assert!(schema["properties"].get("pageId").is_none());
        assert_eq!(schema["required"], json!(["url"]));
    }
    #[test]
    fn parses_only_strict_page_sections() {
        let result = CallToolResult::success(vec![ContentBlock::text(
            "# response\n## Pages\n1: https://one.test/\n22: https://two.test/ [selected]\n## Done",
        )]);
        let (pages, selected) = parse_pages(&result).unwrap();
        assert_eq!(pages, HashSet::from([1, 22]));
        assert_eq!(selected, Some(22));
    }
    #[test]
    fn parses_selected_page_with_isolated_context_suffix() {
        let result = CallToolResult::success(vec![ContentBlock::text(
            "## Pages\n7: https://one.test/\n8: https://two.test/ [selected] isolatedContext=private",
        )]);
        let (pages, selected) = parse_pages(&result).unwrap();
        assert_eq!(pages, HashSet::from([7, 8]));
        assert_eq!(selected, Some(8));
    }
    #[test]
    fn structured_pages_are_authoritative() {
        let mut result = CallToolResult::success(vec![ContentBlock::text(
            "## Pages\n99: https://stale.test/ [selected]",
        )]);
        result.structured_content = Some(json!({
            "pages": [
                {"id": 1, "url": "https://one.test/", "selected": false},
                {"id": 2, "url": "https://two.test/", "selected": true}
            ]
        }));
        let (pages, selected) = parse_pages(&result).unwrap();
        assert_eq!(pages, HashSet::from([1, 2]));
        assert_eq!(selected, Some(2));
    }
    #[test]
    fn malformed_page_lines_fail_closed() {
        let result = CallToolResult::success(vec![ContentBlock::text("## Pages\nnot a page")]);
        assert!(parse_pages(&result).is_err());
        assert!(filter_page_content(result, &HashSet::new()).is_err());
    }
    #[test]
    fn filters_unowned_pages_and_redacts_ids() {
        let mut result = CallToolResult::success(vec![ContentBlock::text(
            "# response\n## Pages\n1: https://one.test/\n2: https://two.test/ [selected]",
        )]);
        result.structured_content = Some(json!({
            "pages": [
                {"id": 1, "url": "https://one.test/", "selected": false},
                {"id": 2, "url": "https://two.test/", "selected": true}
            ],
            "unrelated": "preserved"
        }));
        let filtered = filter_page_content(result, &HashSet::from([2])).unwrap();
        let text = content_text(&filtered.content[0]).unwrap();
        assert!(!text.contains("one.test"));
        assert!(!text.contains("2:"));
        assert!(!text.contains("two.test"));
        let structured = filtered.structured_content.unwrap();
        let pages = structured["pages"].as_array().unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0]["url"], "https://two.test/");
        assert!(pages[0].get("id").is_none());
        assert_eq!(structured["unrelated"], "preserved");
    }
    #[test]
    fn error_content_without_page_section_is_not_filtered() {
        let result = CallToolResult::error(vec![ContentBlock::text("downstream failure")]);
        let filtered = filter_page_content(result, &HashSet::new()).unwrap();
        assert!(filtered.is_error.unwrap_or(false));
        assert_eq!(
            content_text(&filtered.content[0]),
            Some("downstream failure")
        );
    }
    #[test]
    fn parses_only_structured_nonempty_tab_id() {
        let mut result = CallToolResult::success(vec![]);
        result.structured_content = Some(json!({"tabId": "stable-target"}));
        assert_eq!(parse_tab_id(&result).unwrap(), "stable-target");
        result.structured_content = Some(json!({"tabId": ""}));
        assert!(parse_tab_id(&result).is_err());
        result.structured_content = Some(json!({"tabId": 9}));
        assert!(parse_tab_id(&result).is_err());
    }
    #[test]
    fn page_state_keeps_numeric_routes_separate_from_stable_ownership() {
        let state = PageState {
            local_to_tab: HashMap::from([(3, "tab-a".to_owned())]),
        };
        assert_eq!(
            state.local_to_tab.get(&3).map(String::as_str),
            Some("tab-a")
        );
        assert!(!state.local_to_tab.contains_key(&4));
    }

    #[test]
    fn restored_session_uses_persisted_activity_for_idle_cleanup() {
        let session = Session {
            persisted: PersistedSession {
                workspace: PathBuf::from("/workspace"),
                current_tab: None,
                last_activity_unix_seconds: 100,
                cleanup_pending: false,
            },
            pages: PageState::default(),
            downstream: None,
        };
        assert!(!session.needs_cleanup(120, Duration::from_secs(60)));
        assert!(session.needs_cleanup(160, Duration::from_secs(60)));
    }

    #[test]
    fn failed_session_is_immediately_cleanup_eligible() {
        let session = Session {
            persisted: PersistedSession {
                workspace: PathBuf::from("/workspace"),
                current_tab: None,
                last_activity_unix_seconds: 100,
                cleanup_pending: true,
            },
            pages: PageState::default(),
            downstream: None,
        };
        assert!(session.needs_cleanup(100, Duration::from_secs(60)));
        assert!(session.needs_cleanup(100, Duration::ZERO));
    }

    #[test]
    fn disabled_idle_expiration_does_not_retire_healthy_sessions() {
        let session = Session {
            persisted: PersistedSession {
                workspace: PathBuf::from("/workspace"),
                current_tab: None,
                last_activity_unix_seconds: 1,
                cleanup_pending: false,
            },
            pages: PageState::default(),
            downstream: None,
        };
        assert!(!session.needs_cleanup(u64::MAX, Duration::ZERO));
    }
}
