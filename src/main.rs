use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use clap::{ArgAction, Args, Parser, Subcommand};
use futures_util::{FutureExt, future::BoxFuture, future::Shared};
use reqwest::{Client, Url, redirect::Policy};
#[allow(deprecated)]
use rmcp::model::{ListRootsResult, Root};
use rmcp::{
    ClientHandler, RoleClient, ServiceExt,
    model::{
        CallToolRequest, CallToolRequestParams, CallToolResult, ClientCapabilities, ClientInfo,
        ClientRequest, ContentBlock, Implementation, JsonObject, PingRequest, ServerResult, Tool,
    },
    service::{PeerRequestOptions, RequestContext, RunningService},
    transport::TokioChildProcess,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    process::{Child, Command},
    sync::{Mutex, RwLock, watch},
    time::{sleep, timeout},
};
use tracing::{error, info, warn};

const INTERNAL_URL: &str = "about:blank";
const WORKER_STOP_TIMEOUT: Duration = Duration::from_secs(5);
const WORKER_START_TIMEOUT: Duration = Duration::from_secs(30);
const WORKER_HEALTH_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_SESSION_ID_BYTES: usize = 256;
const MAX_POOLED_WORKERS: usize = 8;
const MAX_PAGES_PER_SESSION_DEFAULT: usize = 5;
const MAX_PAGES_PER_SESSION_HARD_LIMIT: usize = 32;
const fn default_max_pages_per_session() -> usize {
    MAX_PAGES_PER_SESSION_DEFAULT
}
const MAX_VERSION_RESPONSE_BYTES: usize = 64 * 1024;
const GRAPHICAL_ENVIRONMENT_TIMEOUT: Duration = Duration::from_secs(2);
const GRAPHICAL_ENVIRONMENT_KEYS: &[&str] = &[
    "DBUS_SESSION_BUS_ADDRESS",
    "DESKTOP_SESSION",
    "DISPLAY",
    "GDK_BACKEND",
    "OZONE_PLATFORM",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "XDG_CURRENT_DESKTOP",
    "XDG_RUNTIME_DIR",
    "XDG_SESSION_TYPE",
];
fn default_browser_executable() -> PathBuf {
    const CHROMIUM: &[&str] = &["chromium", "chromium-browser"];
    const ALTERNATIVES: &[&str] = &[
        "google-chrome-stable",
        "google-chrome",
        "chrome-for-testing",
        "brave-browser",
        "brave-browser-stable",
        "brave",
        "microsoft-edge-stable",
        "microsoft-edge",
        "microsoft-edge-beta",
        "microsoft-edge-dev",
        "helium-browser",
        "helium",
    ];
    let path = std::env::var_os("PATH");
    let mut candidates = Vec::new();
    let add_path_candidates = |candidates: &mut Vec<PathBuf>, browsers: &[&str]| {
        if let Some(path) = path.as_deref() {
            for browser in browsers {
                candidates
                    .extend(std::env::split_paths(path).map(|directory| directory.join(browser)));
            }
        }
    };
    add_path_candidates(&mut candidates, CHROMIUM);
    candidates.extend([
        PathBuf::from("/usr/bin/chromium"),
        PathBuf::from("/usr/bin/chromium-browser"),
    ]);
    add_path_candidates(&mut candidates, ALTERNATIVES);
    candidates.extend([
        PathBuf::from("/usr/bin/google-chrome-stable"),
        PathBuf::from("/usr/bin/google-chrome"),
        PathBuf::from("/usr/bin/brave-browser"),
        PathBuf::from("/usr/bin/microsoft-edge-stable"),
        PathBuf::from("/usr/bin/helium-browser"),
        PathBuf::from("/usr/local/bin/helium-browser"),
        PathBuf::from("/opt/helium-browser-bin/helium-wrapper"),
        PathBuf::from("/usr/local/bin/helium"),
        PathBuf::from("/usr/bin/helium"),
    ]);
    candidates
        .into_iter()
        .find(|path| is_executable_file(path))
        .unwrap_or_else(|| PathBuf::from("chromium"))
}

fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"))
}

fn browser_profile(config_home: &Path, executable: &Path) -> PathBuf {
    let name = executable
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    match name {
        "helium" | "helium-browser" | "helium-wrapper" => config_home.join("net.imput.helium"),
        "chrome-for-testing" | "google-chrome" | "google-chrome-stable" => {
            config_home.join("opencode-browser-mcp/google-chrome")
        }
        "brave" | "brave-browser" | "brave-browser-stable" => {
            config_home.join("BraveSoftware/Brave-Browser")
        }
        "microsoft-edge"
        | "microsoft-edge-stable"
        | "microsoft-edge-beta"
        | "microsoft-edge-dev" => config_home.join("microsoft-edge"),
        _ => config_home.join("chromium"),
    }
}

mod broker;
mod cdp;
mod codex;
mod persistence;
use cdp::{SessionLayout, TargetId, TargetTracker};
use persistence::{OwnershipPersistence, PersistedSession, RestoredState};

#[derive(Debug, Parser)]
#[command(version, about = "Session-isolated MCP for Chromium-family browsers")]
struct Cli {
    #[arg(long, hide = true)]
    direct_tools: bool,
    #[arg(long = "profile", env = "OPENCODE_BROWSER_PROFILE")]
    profile_override: Option<PathBuf>,
    #[command(flatten)]
    config: Config,
    #[command(subcommand)]
    command: Option<ProcessMode>,
}

#[derive(Debug, Subcommand)]
enum ProcessMode {
    #[command(hide = true)]
    ManagedBroker,
    /// Release the tabs owned by an agent session.
    #[command(hide = true)]
    ReleaseSession { session_id: String },
    /// Handle a Codex browser lifecycle hook on stdin.
    CodexHook,
}

#[derive(Clone, Debug, Args, Deserialize, Eq, PartialEq, Serialize)]
struct Config {
    #[arg(skip = current_graphical_environment())]
    graphical_environment: HashMap<String, OsString>,
    #[arg(
        long,
        value_enum,
        env = "OPENCODE_BROWSER_SESSION_LAYOUT",
        default_value_t
    )]
    session_layout: SessionLayout,
    #[arg(
        long,
        env = "OPENCODE_BROWSER_EXECUTABLE",
        default_value_os_t = default_browser_executable()
    )]
    browser_executable: PathBuf,
    #[arg(skip)]
    profile: PathBuf,
    #[arg(
        long,
        env = "OPENCODE_BROWSER_HEADLESS",
        default_value_t = false,
        action = ArgAction::Set,
        num_args = 0..=1,
        default_missing_value = "true"
    )]
    headless: bool,
    #[arg(long, env = "OPENCODE_BROWSER_DISABLE_CRASH_REPORTER")]
    disable_crash_reporter: bool,
    #[arg(long, env = "OPENCODE_BROWSER_CDP_HOST", default_value = "127.0.0.1")]
    cdp_host: String,
    #[arg(long, env = "OPENCODE_BROWSER_CDP_PORT", default_value_t = 9222)]
    cdp_port: u16,
    #[arg(long, env = "OPENCODE_BROWSER_IDLE_MINUTES", default_value_t = 30)]
    idle_minutes: u64,
    #[arg(long, env = "OPENCODE_BROWSER_MAX_PAGES", default_value_t = MAX_PAGES_PER_SESSION_DEFAULT)]
    #[serde(default = "default_max_pages_per_session")]
    max_pages_per_session: usize,
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

    async fn resolve(&mut self, profile_override: Option<PathBuf>) -> Result<()> {
        if self.headless {
            self.graphical_environment.clear();
        } else {
            self.graphical_environment = graphical_environment(&self.graphical_environment).await?;
        }

        self.browser_executable = resolve_executable(&self.browser_executable, "browser")?;
        self.downstream_command = resolve_executable(&self.downstream_command, "downstream")?;
        self.profile = canonicalize_path(
            &profile_override
                .unwrap_or_else(|| browser_profile(&config_home(), &self.browser_executable)),
        )?;
        if self.cdp_host == "localhost" {
            self.cdp_host = "127.0.0.1".to_owned();
        }
        Ok(())
    }
}

fn resolve_executable(executable: &Path, name: &str) -> Result<PathBuf> {
    let path = if executable.components().count() == 1 {
        std::env::var_os("PATH")
            .and_then(|path| {
                std::env::split_paths(&path)
                    .map(|directory| directory.join(executable))
                    .find(|candidate| is_executable_file(candidate))
            })
            .ok_or_else(|| {
                anyhow!(
                    "{name} executable was not found in PATH: {}",
                    executable.display()
                )
            })?
    } else {
        executable.to_path_buf()
    };
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .context("failed to resolve the current directory")?
            .join(path)
    };
    let parent = absolute
        .parent()
        .ok_or_else(|| anyhow!("{name} executable has no parent: {}", absolute.display()))?;
    let file_name = absolute
        .file_name()
        .ok_or_else(|| anyhow!("{name} executable has no file name: {}", absolute.display()))?;
    Ok(std::fs::canonicalize(parent)
        .with_context(|| format!("failed to resolve {name} executable {}", absolute.display()))?
        .join(file_name))
}

fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn canonicalize_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("failed to resolve the current directory")?
            .join(path)
    };
    match std::fs::canonicalize(&absolute) {
        Ok(resolved) => return Ok(resolved),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("failed to resolve {}", absolute.display()));
        }
    }

    let parent = absolute
        .parent()
        .ok_or_else(|| anyhow!("path has no existing ancestor: {}", absolute.display()))?;
    let name = absolute
        .file_name()
        .ok_or_else(|| anyhow!("path has no final component: {}", absolute.display()))?;
    Ok(canonicalize_path(parent)?.join(name))
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

impl SupervisorState {
    fn reap_exited_process(&mut self) -> Result<()> {
        if let Some(child) = self.process.as_mut()
            && let Some(status) = child
                .try_wait()
                .context("failed to inspect browser process")?
        {
            warn!(%status, "browser process exited; the next browser action will restart it");
            self.process = None;
        }
        Ok(())
    }
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
        let endpoint = self.existing_trusted_endpoint(&mut state).await?;
        if let Some(identity) = endpoint {
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
                .arg("--no-startup-window")
                .arg("--no-first-run")
                .arg("--no-default-browser-check")
                .arg("--disable-popup-blocking")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            if self.config.headless {
                command.arg("--headless=new");
            } else {
                for (key, value) in &self.config.graphical_environment {
                    command.env(key, value);
                }
            }
            if self.config.disable_crash_reporter {
                command
                    .arg("--disable-breakpad")
                    .arg("--disable-crash-reporter");
            }
            info!(executable = %self.config.browser_executable.display(), profile = %self.config.profile.display(), headless = self.config.headless, "starting browser");
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

    async fn existing_trusted_endpoint(
        &self,
        state: &mut SupervisorState,
    ) -> Result<Option<String>> {
        state.reap_exited_process()?;
        let endpoint = self.endpoint_identity().await?;
        if let Some(identity) = endpoint.as_deref()
            && state.process.is_none()
            && state.trusted_generation.as_deref() != Some(identity)
        {
            bail!(
                "refusing to adopt an unrecognized browser CDP endpoint at {}",
                self.config.browser_url()
            );
        }
        Ok(endpoint)
    }

    async fn trusted_endpoint(&self) -> Result<Option<String>> {
        let mut state = self.state.lock().await;
        self.existing_trusted_endpoint(&mut state).await
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

    async fn handoff_process(&self) {
        if let Some(process) = self.state.lock().await.process.take() {
            // The replacement broker adopts this browser through its persisted CDP generation.
            std::mem::forget(process);
        }
    }
}

fn parse_graphical_environment(output: &[u8]) -> HashMap<String, OsString> {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            (GRAPHICAL_ENVIRONMENT_KEYS.contains(&key) && !value.is_empty())
                .then(|| (key.to_owned(), OsString::from(value)))
        })
        .collect()
}

fn current_graphical_environment() -> HashMap<String, OsString> {
    GRAPHICAL_ENVIRONMENT_KEYS
        .iter()
        .filter_map(|key| {
            std::env::var_os(key)
                .filter(|value| !value.is_empty())
                .map(|value| ((*key).to_owned(), value))
        })
        .collect()
}

async fn graphical_environment(
    current: &HashMap<String, OsString>,
) -> Result<HashMap<String, OsString>> {
    let mut environment = current.clone();

    let manager_environment = match timeout(
        GRAPHICAL_ENVIRONMENT_TIMEOUT,
        Command::new("systemctl")
            .arg("--user")
            .arg("show-environment")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output(),
    )
    .await
    {
        Ok(Ok(output)) if output.status.success() => parse_graphical_environment(&output.stdout),
        _ => HashMap::new(),
    };
    for (key, value) in manager_environment {
        environment.entry(key).or_insert(value);
    }

    if !environment.contains_key("DISPLAY") && !environment.contains_key("WAYLAND_DISPLAY") {
        bail!(
            "headed browser startup requires DISPLAY or WAYLAND_DISPLAY; set OPENCODE_BROWSER_HEADLESS=true when no graphical session is available"
        );
    }
    Ok(environment)
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
            Implementation::new("browser-mcp", env!("CARGO_PKG_VERSION")),
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
type WorkerPool = Arc<Mutex<HashMap<PathBuf, Vec<Downstream>>>>;
struct Session {
    persisted: PersistedSession,
    /// Worker-local numeric IDs are merely routes.  TargetTracker is the
    /// ownership authority; this map is rebuilt from it on every reconcile.
    pages: PageState,
    downstream: Option<Downstream>,
}

impl Session {
    async fn stop_worker(&mut self, id: &str) {
        self.pages.local_to_tab.clear();
        if let Some(worker) = self.downstream.take()
            && let Err(error) = stop_worker(worker).await
        {
            warn!(%error, session_id = %id, "failed to stop Chrome DevTools MCP worker");
        }
    }
}

#[derive(Default)]
struct PageState {
    local_to_tab: HashMap<u64, TargetId>,
}
#[derive(Default)]
struct SessionSlotState {
    revision: AtomicU64,
    session: Mutex<Option<Session>>,
}
type SessionSlot = Arc<SessionSlotState>;
type Sessions = Mutex<HashMap<String, SessionSlot>>;

impl SessionSlotState {
    fn available_revision(&self) -> Option<u64> {
        let revision = self.revision.load(Ordering::SeqCst);
        (revision & 1 == 0).then_some(revision)
    }

    fn cleanup_requested(&self) -> bool {
        self.revision.load(Ordering::SeqCst) & 1 == 1
    }

    fn request_cleanup(&self) {
        self.revision.fetch_or(1, Ordering::SeqCst);
    }

    fn finish_cleanup(&self) {
        let revision = self.revision.load(Ordering::SeqCst);
        if revision & 1 == 1 {
            let _ = self.revision.compare_exchange(
                revision,
                revision.wrapping_add(1),
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
        }
    }
}

fn unix_time_seconds() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs())
}

async fn restore_sessions(
    tracker: &TargetTracker,
    persistence: &OwnershipPersistence,
    restored: HashMap<String, PersistedSession>,
) -> Result<HashMap<String, Session>> {
    let mut sessions = HashMap::new();
    for (id, mut persisted) in restored {
        let owned = tracker.owned_tabs(&id).await?;
        if owned.is_empty() {
            continue;
        }
        if id.starts_with("connection:") {
            persisted.cleanup_pending = true;
            persistence.update_session(&id, persisted.clone())?;
        }
        let current_tab = persisted
            .current_tab
            .filter(|tab| owned.iter().any(|owned| owned == tab));
        sessions.insert(
            id,
            Session {
                persisted: PersistedSession {
                    current_tab,
                    ..persisted
                },
                pages: PageState::default(),
                downstream: None,
            },
        );
    }
    Ok(sessions)
}

async fn connect_tracker(
    identity: String,
    persistence: Arc<OwnershipPersistence>,
) -> Result<(TargetTracker, HashMap<String, PersistedSession>)> {
    let RestoredState {
        owners,
        sessions,
        owner_revision,
    } = persistence.activate_generation(&identity)?;
    let tracker = TargetTracker::connect(identity, persistence, owners, owner_revision).await?;
    Ok((tracker, sessions))
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
        .arg("--no-performance-crux")
        .arg("--redact-network-headers")
        .current_dir(workspace)
        .env("CHROME_DEVTOOLS_MCP_NO_UPDATE_CHECKS", "1")
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

async fn worker_is_healthy(worker: &Downstream) -> bool {
    !worker.is_closed()
        && !worker.is_transport_closed()
        && matches!(
            timeout(
                WORKER_HEALTH_TIMEOUT,
                worker.send_request(ClientRequest::PingRequest(PingRequest::default()))
            )
            .await,
            Ok(Ok(ServerResult::EmptyResult(_)))
        )
}

async fn recycle_worker(pool: &WorkerPool, workspace: PathBuf, worker: Downstream) {
    if worker.is_closed() {
        return;
    }
    let mut pool = pool.lock().await;
    if pool.values().map(Vec::len).sum::<usize>() < MAX_POOLED_WORKERS {
        pool.entry(workspace).or_default().push(worker);
        return;
    }
    drop(pool);
    if let Err(error) = stop_worker(worker).await {
        warn!(%error, "failed to stop excess pooled Chrome DevTools MCP worker");
    }
}

struct ToolCatalog {
    tools: Vec<Tool>,
    page_scoped: HashSet<String>,
}
type CatalogResult = std::result::Result<Arc<ToolCatalog>, Arc<str>>;
type CatalogFuture = Shared<BoxFuture<'static, CatalogResult>>;

struct CatalogLoader {
    config: Arc<Config>,
    worker_pool: WorkerPool,
    future: Mutex<Option<CatalogFuture>>,
    cancel: watch::Sender<bool>,
}

impl CatalogLoader {
    async fn get(&self, workspace: &Path) -> Result<Arc<ToolCatalog>> {
        let future = {
            let mut current = self.future.lock().await;
            if current
                .as_ref()
                .and_then(CatalogFuture::peek)
                .is_some_and(|result| result.is_err())
            {
                *current = None;
            }
            current
                .get_or_insert_with(|| {
                    let config = self.config.clone();
                    let worker_pool = self.worker_pool.clone();
                    let cancel = self.cancel.subscribe();
                    let workspace = workspace.to_path_buf();
                    let future = async move {
                        let result =
                            discover_catalog(&config, &worker_pool, workspace, cancel).await;
                        if let Err(error) = &result {
                            warn!(%error, "Chrome DevTools tool catalog initialization failed");
                        }
                        result
                            .map(Arc::new)
                            .map_err(|error| Arc::from(error.to_string()))
                    }
                    .boxed()
                    .shared();
                    let background = future.clone();
                    tokio::spawn(async move { drop(background.await) });
                    future
                })
                .clone()
        };
        future.await.map_err(|error| anyhow!(error.to_string()))
    }

    fn cancel(&self) {
        self.cancel.send_replace(true);
    }

    async fn shutdown(&self) {
        self.cancel();
        let future = self.future.lock().await.clone();
        if let Some(future) = future {
            drop(future.await);
        }
    }
}

async fn discover_catalog(
    config: &Config,
    worker_pool: &WorkerPool,
    workspace: PathBuf,
    mut cancel: watch::Receiver<bool>,
) -> Result<ToolCatalog> {
    async fn cancelled(cancel: &mut watch::Receiver<bool>) {
        if *cancel.borrow() {
            return;
        }
        let _ = cancel.changed().await;
    }

    if *cancel.borrow() {
        bail!("Chrome DevTools tool catalog initialization stopped");
    }
    let worker = tokio::select! {
        biased;
        _ = cancelled(&mut cancel) => bail!("Chrome DevTools tool catalog initialization stopped"),
        result = spawn_downstream(config, &workspace) => result?,
    };
    let catalog = async {
        let listed = tokio::select! {
            biased;
            _ = cancelled(&mut cancel) => bail!("Chrome DevTools tool catalog initialization stopped"),
            result = timeout(WORKER_START_TIMEOUT, worker.list_all_tools()) => result
                .context("timed out retrieving Chrome DevTools tools")?
                .context("failed to retrieve Chrome DevTools tools")?,
        };
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
        Ok(ToolCatalog { tools, page_scoped })
    }.await;
    match catalog {
        Ok(catalog) => {
            recycle_worker(worker_pool, workspace, worker).await;
            Ok(catalog)
        }
        Err(error) => {
            if let Err(stop_error) = stop_worker(worker).await {
                warn!(%stop_error, "failed to stop Chrome DevTools worker after catalog initialization failed");
            }
            Err(error)
        }
    }
}

#[derive(Clone)]
struct BrowserProxy {
    config: Arc<Config>,
    supervisor: Arc<BrowserSupervisor>,
    catalog: Arc<CatalogLoader>,
    sessions: Arc<Sessions>,
    worker_pool: WorkerPool,
    tracker: Arc<RwLock<Option<TargetTracker>>>,
    idle: Duration,
    operation_timeout: Duration,
    persistence: Arc<OwnershipPersistence>,
}

impl BrowserProxy {
    fn new(config: Arc<Config>, persistence: Arc<OwnershipPersistence>) -> Result<Self> {
        let supervisor = Arc::new(BrowserSupervisor::new(
            config.clone(),
            persistence.generation(),
        )?);
        let worker_pool = Arc::new(Mutex::new(HashMap::new()));
        let (catalog_cancel, _) = watch::channel(false);
        Ok(Self {
            config: config.clone(),
            supervisor,
            catalog: Arc::new(CatalogLoader {
                config: config.clone(),
                worker_pool: worker_pool.clone(),
                future: Mutex::new(None),
                cancel: catalog_cancel,
            }),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            worker_pool,
            tracker: Arc::new(RwLock::new(None)),
            idle: Duration::from_secs(config.idle_minutes.saturating_mul(60)),
            operation_timeout: Duration::from_secs(config.operation_seconds),
            persistence,
        })
    }

    fn cleanup_cadence(&self) -> Duration {
        if self.idle.is_zero() {
            Duration::from_secs(60)
        } else {
            self.idle.min(Duration::from_secs(60))
        }
    }

    async fn acquire_worker(&self, workspace: &std::path::Path) -> Result<Downstream> {
        loop {
            let worker = self
                .worker_pool
                .lock()
                .await
                .get_mut(workspace)
                .and_then(Vec::pop);
            let Some(worker) = worker else {
                return spawn_downstream(&self.config, workspace).await;
            };
            if worker_is_healthy(&worker).await {
                return Ok(worker);
            }
            if let Err(error) = stop_worker(worker).await {
                warn!(%error, "failed to stop unhealthy pooled Chrome DevTools MCP worker");
            }
        }
    }

    async fn stop_pooled_workers(&self) {
        let pooled = self
            .worker_pool
            .lock()
            .await
            .drain()
            .flat_map(|(_, workers)| workers)
            .collect::<Vec<_>>();
        for result in futures_util::future::join_all(pooled.into_iter().map(stop_worker)).await {
            if let Err(error) = result {
                warn!(%error, "failed to stop pooled Chrome DevTools MCP worker");
            }
        }
    }

    async fn tracker_is_current(&self, tracker: Option<&TargetTracker>) -> Result<bool> {
        let Some(tracker) = tracker else {
            return Ok(false);
        };
        if self.supervisor.endpoint_identity().await?.as_deref() != Some(tracker.generation()) {
            return Ok(false);
        }
        Ok(tracker.is_available().await)
    }

    async fn tracker_lease(&self) -> Result<tokio::sync::RwLockReadGuard<'_, TargetTracker>> {
        loop {
            let read = self.tracker.read().await;
            if self.tracker_is_current(read.as_ref()).await? {
                return Ok(tokio::sync::RwLockReadGuard::map(read, |tracker| {
                    tracker.as_ref().expect("tracker was checked")
                }));
            }
            drop(read);
            let mut current = self.tracker.write().await;
            if self.tracker_is_current(current.as_ref()).await? {
                continue;
            }

            let identity = self.supervisor.ensure_running().await?;
            let cold = current.is_none();
            let generation_changed = current
                .as_ref()
                .is_some_and(|tracker| tracker.generation() != identity);
            if let Some(previous) = current.as_ref() {
                if generation_changed {
                    warn!("browser endpoint identity changed; retiring all session workers");
                    self.retire_all_sessions().await;
                } else {
                    warn!("CDP target tracker disconnected; restoring target ownership");
                }
                previous.shutdown().await;
            }

            let (tracker, restored) = connect_tracker(identity, self.persistence.clone()).await?;
            tracker.synchronize().await?;
            if cold || generation_changed {
                let sessions = restore_sessions(&tracker, &self.persistence, restored).await?;
                let mut current_sessions = self.sessions.lock().await;
                if current_sessions.values().any(|slot| {
                    !slot
                        .session
                        .try_lock()
                        .is_ok_and(|session| session.is_none())
                }) {
                    error!("cold browser runtime unexpectedly has live session slots");
                    bail!("cannot restore browser sessions over live session state");
                }
                for (id, session) in sessions {
                    let slot = current_sessions.entry(id).or_insert_with(Arc::default);
                    if session.persisted.cleanup_pending {
                        slot.request_cleanup();
                    }
                    *slot.session.try_lock().expect("session slots were checked") = Some(session);
                }
            }
            *current = Some(tracker);
        }
    }

    async fn retire_all_sessions(&self) {
        let slots = self
            .sessions
            .lock()
            .await
            .iter()
            .map(|(id, slot)| (id.clone(), slot.clone()))
            .collect::<Vec<_>>();
        for (id, slot) in slots {
            if let Some(mut session) = slot.session.lock().await.take() {
                // Targets belong to the previous endpoint, so never close them here.
                session.stop_worker(&id).await;
            }
        }
        self.stop_pooled_workers().await;
        let mut sessions = self.sessions.lock().await;
        sessions.retain(|_, slot| Arc::strong_count(slot) > 1);
    }

    async fn slot(&self, id: &str) -> SessionSlot {
        let mut sessions = self.sessions.lock().await;
        sessions
            .entry(id.to_owned())
            .or_insert_with(Arc::default)
            .clone()
    }

    async fn prune_slot(&self, id: &str, slot: &SessionSlot) {
        let mut sessions = self.sessions.lock().await;
        let removable = sessions
            .get(id)
            .is_some_and(|registered| Arc::ptr_eq(registered, slot))
            && Arc::strong_count(slot) == 2
            && slot
                .session
                .try_lock()
                .is_ok_and(|session| session.is_none());
        if removable {
            sessions.remove(id);
        }
    }

    async fn route_call(
        &self,
        catalog: &ToolCatalog,
        workspace: &std::path::Path,
        id: &str,
        request: CallToolRequestParams,
    ) -> Result<CallToolResult> {
        if browser_tool(&catalog.tools, request.name.as_ref()).is_none() {
            bail!("browser tool is not exposed: {}", request.name);
        }
        let slot = self.slot(id).await;
        let Some(revision) = slot.available_revision() else {
            self.prune_slot(id, &slot).await;
            bail!("browser session is pending owned-tab cleanup");
        };
        let tracker_lease = match self.tracker_lease().await {
            Ok(tracker) => tracker,
            Err(error) => {
                self.prune_slot(id, &slot).await;
                return Err(error);
            }
        }; // Must outlive every browser operation below.
        let tracker = &*tracker_lease;
        let mut guard = slot.session.lock().await;
        if slot.available_revision() != Some(revision) {
            drop(guard);
            self.prune_slot(id, &slot).await;
            bail!("browser session is pending owned-tab cleanup");
        }
        if guard.is_none() && request.name == "list_pages" {
            tracker.synchronize().await?;
            if tracker.owned_tabs(id).await?.is_empty() {
                drop(guard);
                self.prune_slot(id, &slot).await;
                let mut result = CallToolResult::success(vec![ContentBlock::text("## Pages")]);
                result.structured_content = Some(json!({"pages": []}));
                return Ok(result);
            }
        }
        if guard.is_none() {
            tracker.activate_session(id).await?;
            let downstream = match self.acquire_worker(workspace).await {
                Ok(downstream) => downstream,
                Err(error) => {
                    drop(guard);
                    self.prune_slot(id, &slot).await;
                    return Err(error);
                }
            };
            let mut session = Session {
                persisted: PersistedSession {
                    workspace: workspace.to_path_buf(),
                    current_tab: None,
                    last_activity_unix_seconds: unix_time_seconds()?,
                    cleanup_pending: slot.cleanup_requested(),
                },
                pages: PageState::default(),
                downstream: Some(downstream),
            };
            if let Err(error) = self.persist_session(id, &session) {
                session.stop_worker(id).await;
                drop(guard);
                self.prune_slot(id, &slot).await;
                return Err(error);
            }
            *guard = Some(session);
        }
        let session = guard.as_mut().expect("session exists");
        if session.persisted.workspace != workspace {
            error!(session_id = %id, existing_workspace = %session.persisted.workspace.display(), requested_workspace = %workspace.display(), "browser session identifier was reused from a different workspace");
            bail!("browser session identifier is already bound to a different workspace");
        }
        if session.persisted.cleanup_pending {
            bail!("browser session is pending owned-tab cleanup");
        }
        if let Some(worker) = session.downstream.as_ref()
            && (worker.is_closed() || worker.is_transport_closed())
        {
            warn!(session_id = %id, "restarting unhealthy Chrome DevTools worker; retaining owned tabs");
            session.stop_worker(id).await;
        }
        if session.downstream.is_none() {
            session.downstream = Some(self.acquire_worker(workspace).await?);
        }
        let closes_page = request.name == "close_page";
        let mut result = self
            .call_session(catalog, tracker, id, session, request)
            .await;
        session.persisted.last_activity_unix_seconds = unix_time_seconds()?;
        if let Err(error) = self.persist_session(id, session) {
            result = Err(error);
        }
        if closes_page
            && result
                .as_ref()
                .is_ok_and(|result| !result.is_error.unwrap_or(false))
            && guard
                .as_ref()
                .is_some_and(|session| session.persisted.current_tab.is_none())
            && let Err(error) = self.finalize_session(id, tracker, &mut guard, true).await
        {
            result = Err(error.context("failed to finalize closed browser session"));
        }
        if slot.cleanup_requested() {
            let recycle_worker = result.is_ok();
            let retirement = if guard.is_some() {
                self.retire_session(id, tracker, &mut guard, recycle_worker)
                    .await
            } else {
                Ok(())
            };
            match retirement {
                Ok(()) => {
                    slot.finish_cleanup();
                }
                Err(error) => {
                    warn!(%error, session_id = %id, "requested browser session retirement failed");
                    result = Err(error.context("failed to release browser session"));
                }
            }
        } else if result
            .as_ref()
            .map_or(true, |result| result.is_error.unwrap_or(false))
            && let Some(session) = guard.as_mut()
        {
            warn!(session_id = %id, "browser call failed; resetting worker without replaying the call or closing owned tabs");
            session.stop_worker(id).await;
        }
        drop(guard);
        self.prune_slot(id, &slot).await;
        result
    }

    async fn release_session(&self, id: &str) -> Result<()> {
        if id.is_empty() || id.len() > MAX_SESSION_ID_BYTES {
            bail!("invalid browser session identifier");
        }
        let mut slot = self.sessions.lock().await.get(id).cloned();
        if let Some(slot) = slot.as_ref() {
            slot.request_cleanup();
        }
        let persisted = self.persistence.request_cleanup(id)?;
        if self.tracker.read().await.is_none() {
            if let Some(current) = slot.as_ref() {
                self.prune_slot(id, current).await;
                slot = self.sessions.lock().await.get(id).cloned();
            }
            if slot.is_none() && !persisted {
                return Ok(());
            }
            if persisted {
                // Cleanup of a concrete persisted session may recover a
                // surviving browser generation; empty placeholders do not.
                drop(self.tracker_lease().await?);
            }
        }
        if slot.is_none() {
            slot = self.sessions.lock().await.get(id).cloned();
        }
        let Some(slot) = slot else {
            let tracker_lease = self.tracker_lease().await?;
            let tracker = &*tracker_lease;
            if !tracker.owned_tabs(id).await?.is_empty() {
                error!(session_id = %id, "browser ownership exists without session state");
                bail!("browser ownership exists without session state");
            }
            return Ok(());
        };
        slot.request_cleanup();
        if !persisted {
            warn!(session_id = %id, "browser session release arrived before its state was persisted");
        }
        let tracker_lease = self.tracker_lease().await?;
        let tracker = &*tracker_lease;
        let mut guard = slot.session.lock().await;
        if guard.is_none() {
            if !tracker.owned_tabs(id).await?.is_empty() {
                error!(session_id = %id, "browser ownership exists without live session state");
                bail!("browser ownership exists without live session state");
            }
            slot.finish_cleanup();
            drop(guard);
            self.prune_slot(id, &slot).await;
            return Ok(());
        }
        self.retire_session(id, tracker, &mut guard, true).await?;
        slot.finish_cleanup();
        drop(guard);
        self.prune_slot(id, &slot).await;
        Ok(())
    }

    async fn retire_session(
        &self,
        id: &str,
        tracker: &TargetTracker,
        slot: &mut Option<Session>,
        reuse_worker: bool,
    ) -> Result<()> {
        let session = slot
            .as_mut()
            .ok_or_else(|| anyhow!("browser session disappeared during retirement"))?;
        self.mark_cleanup_pending(id, session)?;
        session.pages.local_to_tab.clear();
        let closed = self.close_owned_pages(id, tracker).await;
        if reuse_worker {
            if let Some(worker) = session.downstream.take() {
                recycle_worker(
                    &self.worker_pool,
                    session.persisted.workspace.clone(),
                    worker,
                )
                .await;
            }
        } else {
            session.stop_worker(id).await;
        }
        closed?;
        self.finalize_session(id, tracker, slot, false).await
    }

    async fn finalize_session(
        &self,
        id: &str,
        tracker: &TargetTracker,
        slot: &mut Option<Session>,
        reuse_worker: bool,
    ) -> Result<()> {
        tracker.finalize_session(id).await?;
        let persisted = self.persistence.remove_session_if_unowned(id);
        let mut session = slot
            .take()
            .ok_or_else(|| anyhow!("browser session disappeared during finalization"))?;
        if reuse_worker {
            if let Some(worker) = session.downstream {
                recycle_worker(&self.worker_pool, session.persisted.workspace, worker).await;
            }
        } else {
            session.stop_worker(id).await;
        }
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
        // broker must still reject work and retry cleanup rather than fail open.
        session.persisted = persisted;
        result
    }

    async fn call_session(
        &self,
        catalog: &ToolCatalog,
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
                if catalog.page_scoped.contains(request.name.as_ref()) {
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
            .map_err(|error| anyhow!("Chrome DevTools tool call interrupted: {error}. The action may have taken effect; inspect browser state before retrying. Subsequent calls replace failed workers automatically."))?;
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
        match self
            .reconcile_with_current(tracker, id, session, None)
            .await
        {
            Err(error)
                if session
                    .downstream
                    .as_ref()
                    .is_some_and(|worker| worker.is_closed() || worker.is_transport_closed()) =>
            {
                // Only repeat read-only reconciliation, never the caller's action.
                warn!(%error, session_id = %id, "worker exited during page reconciliation; replacing it and retaining owned tabs");
                session.stop_worker(id).await;
                session.downstream = Some(self.acquire_worker(&session.persisted.workspace).await?);
                self.reconcile_with_current(tracker, id, session, None)
                    .await
            }
            result => result,
        }
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
        if tracker.owned_tabs(id).await?.is_empty() {
            // Chrome DevTools MCP has no valid page listing before the first tab exists.
            session.pages.local_to_tab.clear();
            session.persisted.current_tab = None;
            return Ok(());
        }
        let (pages, selected) = self.list_snapshot(session).await?;
        let mut mappings = std::mem::take(&mut session.pages.local_to_tab);
        mappings.clear();
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
                json!({"url": INTERNAL_URL, "background": false})
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
            .create_owned_tab(
                id,
                INTERNAL_URL,
                background,
                self.config.session_layout,
                session.persisted.current_tab.as_deref(),
                self.config.max_pages_per_session,
            )
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
            .ok_or_else(|| anyhow!("the current page is not owned by this browser session"))?;
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
        if self.tracker.read().await.is_none() && self.persistence.cleanup_due(now, self.idle) {
            let endpoint = self.supervisor.endpoint_identity().await?;
            if endpoint.as_deref() == self.persistence.generation().as_deref() {
                drop(self.tracker_lease().await?);
            }
        }
        let slots = self
            .sessions
            .lock()
            .await
            .iter()
            .map(|(id, slot)| (id.clone(), slot.clone()))
            .collect::<Vec<_>>();
        if !slots.iter().any(|(_, slot)| {
            slot.session.try_lock().is_ok_and(|session| {
                session.as_ref().is_some_and(|session| {
                    slot.cleanup_requested() || session.persisted.needs_cleanup(now, self.idle)
                })
            })
        }) {
            return Ok(());
        }
        let tracker_lease = self.tracker_lease().await?;
        let tracker = &*tracker_lease;
        for (id, slot) in slots {
            let Ok(mut guard) = slot.session.try_lock() else {
                continue;
            };
            let Some(session) = guard.as_mut() else {
                continue;
            };
            if !slot.cleanup_requested() && !session.persisted.needs_cleanup(now, self.idle) {
                continue;
            }
            match self.retire_session(&id, tracker, &mut guard, true).await {
                Ok(()) => {
                    slot.finish_cleanup();
                    info!(session_id = %id, "cleaned up idle browser session");
                }
                Err(error) => {
                    warn!(%error, session_id = %id, "idle session retirement failed")
                }
            }
            drop(guard);
            self.prune_slot(&id, &slot).await;
        }
        Ok(())
    }

    async fn shutdown(&self, preserve_browser: bool) -> Result<()> {
        self.catalog.shutdown().await;
        let slots = self.sessions.lock().await.drain().collect::<Vec<_>>();
        let mut sessions = Vec::new();
        for (id, slot) in slots {
            if let Some(session) = slot.session.lock().await.take() {
                sessions.push((id, session));
            }
        }
        futures_util::future::join_all(
            sessions
                .into_iter()
                .map(|(id, mut session)| async move { session.stop_worker(&id).await }),
        )
        .await;
        self.stop_pooled_workers().await;
        if preserve_browser {
            if let Some(tracker) = self.tracker.write().await.take() {
                tracker.shutdown().await;
            }
            self.supervisor.handoff_process().await;
            return Ok(());
        }
        let tracker = match self.tracker.write().await.take() {
            Some(tracker) => tracker,
            None => {
                let Some(identity) = self.supervisor.trusted_endpoint().await? else {
                    self.supervisor.wait_for_browser_exit().await?;
                    return Ok(());
                };
                connect_tracker(identity, self.persistence.clone()).await?.0
            }
        };
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

fn browser_tool<'a>(tools: &'a [Tool], name: &str) -> Option<&'a Tool> {
    tools.iter().find(|tool| tool.name.as_ref() == name)
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

fn parse_pages(result: &CallToolResult) -> Result<(HashSet<u64>, Option<u64>)> {
    let pages = result
        .structured_content
        .as_ref()
        .and_then(|structured| structured.get("pages"))
        .ok_or_else(|| anyhow!("missing structured page list"))?;
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
        let is_selected = page
            .get("selected")
            .and_then(Value::as_bool)
            .ok_or_else(|| anyhow!("structured page entry has no selected state"))?;
        if is_selected && selected.replace(id).is_some() {
            bail!("structured page list has multiple selected pages");
        }
    }
    Ok((ids, selected))
}
fn filter_page_content(
    mut result: CallToolResult,
    allowed: &HashSet<u64>,
) -> Result<CallToolResult> {
    let mut structured_page_lines = None;
    if let Some(structured) = result.structured_content.as_mut()
        && let Some(pages) = structured.get_mut("pages")
    {
        let pages = pages
            .as_array_mut()
            .ok_or_else(|| anyhow!("structured page list is not an array"))?;
        let mut filtered = Vec::new();
        let mut lines = Vec::new();
        for page in std::mem::take(pages) {
            let Value::Object(mut page) = page else {
                bail!("structured page entry is not an object");
            };
            let id = page
                .remove("id")
                .and_then(|id| id.as_u64())
                .ok_or_else(|| anyhow!("structured page entry has no numeric ID"))?;
            if allowed.contains(&id) {
                let url = page
                    .get("url")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("structured page entry has no URL"))?;
                let title = page
                    .get("title")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("structured page entry has no title"))?;
                let selected = page
                    .get("selected")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| anyhow!("structured page entry has no selected state"))?;
                let label = if title.is_empty() {
                    url.to_owned()
                } else {
                    format!("{title} ({url})")
                };
                let mut line = if label.is_empty() {
                    "-".to_owned()
                } else {
                    format!("- {label}")
                };
                if selected {
                    line.push_str(" [selected]");
                }
                if let Some(context) = page.get("isolatedContext") {
                    let context = context.as_str().ok_or_else(|| {
                        anyhow!("structured page entry has invalid isolated context")
                    })?;
                    line.push_str(&format!(" isolatedContext={context}"));
                }
                lines.push(line);
                filtered.push(Value::Object(page));
            }
        }
        structured_page_lines = Some(lines);
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
                filtered.extend(
                    structured_page_lines
                        .as_ref()
                        .ok_or_else(|| anyhow!("page response has no structured page list"))?
                        .iter()
                        .cloned(),
                );
                continue;
            }
            if inside && trimmed.starts_with("## ") {
                inside = false;
            }
            if inside && !trimmed.is_empty() {
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
                .unwrap_or_else(|_| "browser_mcp=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    broker::run(Cli::parse()).await
}
fn validate_config(config: &Config) -> Result<()> {
    if !is_executable_file(&config.browser_executable) {
        bail!(
            "browser executable is missing or not executable: {}",
            config.browser_executable.display()
        );
    }
    if config.cdp_host != "127.0.0.1" && config.cdp_host != "localhost" {
        bail!("CDP must be bound to localhost, got {}", config.cdp_host);
    }
    if config.cdp_port == 0 {
        bail!("CDP port must not be zero");
    }
    if config.idle_minutes == 0 {
        warn!("idle expiration is disabled; cleanup-pending sessions will still be retried");
    }
    if config.max_pages_per_session == 0
        || config.max_pages_per_session > MAX_PAGES_PER_SESSION_HARD_LIMIT
    {
        bail!(
            "browser max pages per session must be between 1 and {MAX_PAGES_PER_SESSION_HARD_LIMIT}"
        );
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

    fn content_text(content: &ContentBlock) -> Option<&str> {
        content.as_text().map(|text| text.text.as_str())
    }

    fn parsed_cli(arguments: &[&str]) -> Cli {
        Cli::try_parse_from(
            std::iter::once("test")
                .chain(arguments.iter().copied())
                .chain(std::iter::once("managed-broker")),
        )
        .unwrap()
    }

    fn broker_config(arguments: &[&str]) -> Config {
        let cli = parsed_cli(arguments);
        let Some(ProcessMode::ManagedBroker) = cli.command else {
            panic!("expected broker configuration");
        };
        cli.config
    }

    #[test]
    fn session_limits_default_to_five_pages_and_thirty_idle_minutes() {
        let config = broker_config(&[]);
        assert_eq!(config.max_pages_per_session, MAX_PAGES_PER_SESSION_DEFAULT);
        assert_eq!(config.idle_minutes, 30);
    }
    #[test]
    fn max_pages_per_session_is_configurable_and_validated() {
        let mut config = broker_config(&["--browser-executable=/bin/true"]);
        assert!(validate_config(&config).is_ok());
        config.max_pages_per_session = 0;
        assert!(validate_config(&config).is_err());
        config.max_pages_per_session = MAX_PAGES_PER_SESSION_HARD_LIMIT + 1;
        assert!(validate_config(&config).is_err());
        config.max_pages_per_session = MAX_PAGES_PER_SESSION_DEFAULT;
        assert!(validate_config(&config).is_ok());
    }
    #[test]
    fn max_pages_per_session_deserializes_without_the_field() {
        let mut value = serde_json::to_value(broker_config(&[])).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("max_pages_per_session");
        let config: Config = serde_json::from_value(value).unwrap();
        assert_eq!(config.max_pages_per_session, MAX_PAGES_PER_SESSION_DEFAULT);
    }
    #[test]
    fn browser_is_headed_by_default_and_headless_is_configurable() {
        assert!(!broker_config(&[]).headless);
        assert!(broker_config(&["--headless=true"]).headless);
        assert!(!broker_config(&["--headless=false"]).headless);
    }
    #[tokio::test]
    async fn profile_override_is_resolved_and_serialized() {
        let profile = std::env::temp_dir().join("opencode-browser-mcp-profile-override");
        let mut cli = parsed_cli(&[
            "--headless=true",
            "--browser-executable=/bin/true",
            "--downstream-command=/bin/true",
            &format!("--profile={}", profile.display()),
        ]);
        cli.config.resolve(cli.profile_override).await.unwrap();
        assert_eq!(cli.config.profile, profile);
        let restored: Config =
            serde_json::from_str(&serde_json::to_string(&cli.config).unwrap()).unwrap();
        assert_eq!(restored.profile, profile);
    }
    #[test]
    fn helium_browser_uses_the_helium_profile() {
        assert_eq!(
            browser_profile(Path::new("/config"), Path::new("/usr/bin/helium-browser")),
            PathBuf::from("/config/net.imput.helium")
        );
    }
    #[test]
    fn user_manager_environment_is_allowlisted() {
        let environment = parse_graphical_environment(
            b"DISPLAY=:1\nWAYLAND_DISPLAY=wayland-0\nSECRET_TOKEN=not-allowed\nEMPTY=\n",
        );
        assert_eq!(environment.get("DISPLAY"), Some(&OsString::from(":1")));
        assert_eq!(
            environment.get("WAYLAND_DISPLAY"),
            Some(&OsString::from("wayland-0"))
        );
        assert!(!environment.contains_key("SECRET_TOKEN"));
        assert!(!environment.contains_key("EMPTY"));
    }
    #[test]
    fn profile_resolution_preserves_symlink_parent_semantics() {
        let root = std::env::temp_dir().join(format!(
            "opencode-browser-mcp-path-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let real = root.join("real");
        std::fs::create_dir_all(real.join("work")).unwrap();
        std::os::unix::fs::symlink(real.join("work"), root.join("linked-work")).unwrap();

        let resolved = canonicalize_path(&root.join("linked-work/../profile")).unwrap();
        assert_eq!(resolved, real.join("profile"));

        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn removes_page_id_from_schema_and_required_fields() {
        let mut schema = json!({"type":"object","properties":{"pageId":{"type":"number"},"url":{"type":"string"}},"required":["pageId","url"]}).as_object().unwrap().clone();
        assert!(remove_schema_property(&mut schema, "pageId"));
        assert!(schema["properties"].get("pageId").is_none());
        assert_eq!(schema["required"], json!(["url"]));
    }
    #[test]
    fn parses_structured_pages() {
        let mut result = CallToolResult::success(vec![]);
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
    fn page_text_without_structured_data_fails_closed() {
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
                {"id": 1, "url": "https://one.test/", "title": "", "selected": false},
                {"id": 2, "url": "https://two.test/", "title": "", "selected": true}
            ],
            "unrelated": "preserved"
        }));
        let filtered = filter_page_content(result, &HashSet::from([2])).unwrap();
        assert_eq!(
            content_text(&filtered.content[0]),
            Some("# response\n## Pages\n- https://two.test/ [selected]")
        );
        assert_eq!(
            filtered.structured_content,
            Some(json!({
                "pages": [{"url": "https://two.test/", "title": "", "selected": true}],
                "unrelated": "preserved"
            }))
        );
    }
    #[test]
    fn structured_pages_replace_transient_malformed_text() {
        let mut result = CallToolResult::success(vec![ContentBlock::text("## Pages\n3: ")]);
        result.structured_content = Some(json!({
            "pages": [{"id": 3, "url": "", "title": "", "selected": false}]
        }));
        let filtered = filter_page_content(result, &HashSet::from([3])).unwrap();
        assert_eq!(content_text(&filtered.content[0]), Some("## Pages\n-"));
        assert_eq!(
            filtered.structured_content,
            Some(json!({"pages": [{"url": "", "title": "", "selected": false}]}))
        );
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
}
