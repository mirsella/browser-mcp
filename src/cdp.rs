//! Browser-level CDP Target ownership tracking.
//!
//! `TargetId`s are stable only for one browser websocket.  Consequently a
//! tracker is deliberately tied to the websocket URL used to create it; that
//! URL is its generation and state is never reused after a disconnect.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use clap::ValueEnum;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    sync::{Mutex, mpsc, oneshot, watch},
    time::timeout,
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::warn;

use crate::persistence::OwnershipPersistence;

pub(crate) type TargetId = String;
type BrowserSessionId = String;
const TRACKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum SessionLayout {
    #[default]
    SharedWindow,
    WindowPerSession,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TargetRecord {
    id: TargetId,
    target_type: String,
    opener_id: Option<TargetId>,
    /// The browser `tab` target containing this target, if CDP reported it.
    tab_id: Option<TargetId>,
}

#[derive(Debug)]
enum OwnershipError {
    Unavailable,
    NotATab(TargetId),
    UnknownTarget(TargetId),
    OwnerConflict { target: TargetId },
    NotOwner { target: TargetId },
    TooManyPages { owned: usize, max: usize },
}

impl std::fmt::Display for OwnershipError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => write!(
                formatter,
                "CDP target tracker is disconnected or desynchronized"
            ),
            Self::NotATab(target) => write!(formatter, "target {target} is not a browser tab"),
            Self::UnknownTarget(target) => write!(formatter, "target {target} is unknown"),
            Self::OwnerConflict { target } => write!(
                formatter,
                "target {target} is owned by another browser session"
            ),
            Self::NotOwner { target } => {
                write!(formatter, "browser session does not own target {target}")
            }
            Self::TooManyPages { owned, max } => write!(
                formatter,
                "browser session already owns {owned} pages (max {max}); close the current page with close_page or reuse a page with navigate_page instead of new_page"
            ),
        }
    }
}

impl std::error::Error for OwnershipError {}

/// Enforces the per-session page cap for newly created tabs. Popup and
/// recovery claims bypass this: they adopt pre-existing targets rather
/// than growing the session.
fn ensure_page_capacity(owned: usize, max: usize) -> Result<()> {
    if owned >= max {
        return Err(OwnershipError::TooManyPages { owned, max }.into());
    }
    Ok(())
}

#[derive(Default)]
struct State {
    connected: bool,
    records: HashMap<TargetId, TargetRecord>,
    owners: HashMap<TargetId, BrowserSessionId>, // keyed by stable tab target
    owner_revision: u64,
    retired_sessions: HashSet<BrowserSessionId>,
    /// Every flattened CDP attachment, including direct tab attachments. This
    /// is the canonical session tree; a root has no parent session.
    sessions: HashMap<String, CdpSession>,
}

/// The target and containing tab represented by one attached CDP session.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CdpSession {
    target_id: TargetId,
    tab_id: TargetId,
    parent_session: Option<String>,
}

impl State {
    fn unavailable(&mut self) {
        self.connected = false;
    }

    fn upsert(&mut self, mut record: TargetRecord) -> Result<()> {
        // targetInfoChanged does not carry our attachment-derived join. Never
        // discard it merely because a later discovery event omitted it.
        if let Some(existing) = self.records.get(&record.id)
            && existing.tab_id.is_some()
        {
            record.tab_id = existing.tab_id.clone();
        }
        self.records.insert(record.id.clone(), record);
        self.propagate_owners()
    }

    fn remove(&mut self, target: &str) {
        self.records.remove(target);
        self.owners.remove(target);
        let roots = self
            .sessions
            .iter()
            .filter(|(_, session)| session.target_id == target)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for session in roots {
            self.detach_subtree(&session);
        }
        // A destroyed tab invalidates joins that point at it, even when a
        // sibling root was attached independently.
        for record in self.records.values_mut() {
            if record.tab_id.as_deref() == Some(target) {
                record.tab_id = None;
            }
        }
    }

    fn replace_snapshot(&mut self, records: Vec<TargetRecord>) -> Result<()> {
        let previous = std::mem::take(&mut self.records);
        self.records = records
            .into_iter()
            .map(|mut record| {
                if let Some(existing) = previous.get(&record.id) {
                    record.tab_id = existing.tab_id.clone();
                }
                (record.id.clone(), record)
            })
            .collect();
        self.owners.retain(|tab, _| {
            self.records
                .get(tab)
                .is_some_and(|record| record.target_type == "tab")
        });
        self.sessions.retain(|_, session| {
            self.records.contains_key(&session.tab_id)
                && self.records.contains_key(&session.target_id)
        });
        // Snapshot application is a protocol boundary; a conflicting lineage
        // means ownership can no longer be trusted.
        self.propagate_owners()
    }

    /// Assign tab lineage repeatedly. This is intentionally a fixed-point
    /// calculation: popup creation and parent attachment can arrive in either
    /// order.
    fn propagate_owners(&mut self) -> Result<()> {
        loop {
            let mut changed = false;
            let records = self.records.clone();
            for record in records.values() {
                // A popup has its own `tab` target.  Its opener can be a page
                // target in the opener tab, so copy the opener's owner onto
                // the *popup tab*, never onto the opener.
                if record.target_type == "tab"
                    && let Some(opener) = record.opener_id.as_deref()
                    && let Some(owner) = self.owner(opener).map(str::to_owned)
                {
                    if let Some(existing) = self.owners.get(&record.id)
                        && existing != &owner
                    {
                        return Err(OwnershipError::OwnerConflict {
                            target: record.id.clone(),
                        }
                        .into());
                    }
                    if !self.owners.contains_key(&record.id) {
                        self.owners.insert(record.id.clone(), owner);
                        changed = true;
                    }
                }
                if record.tab_id.is_some() {
                    continue;
                }
                let Some(opener) = record.opener_id.as_ref() else {
                    continue;
                };
                let Some(parent) = records.get(opener) else {
                    continue;
                };
                let tab = parent
                    .tab_id
                    .clone()
                    .or_else(|| (parent.target_type == "tab").then(|| parent.id.clone()));
                if let Some(tab) = tab
                    && let Some(current) = self.records.get_mut(&record.id)
                    && current.tab_id.as_ref() != Some(&tab)
                {
                    current.tab_id = Some(tab);
                    changed = true;
                }
            }
            if !changed {
                return Ok(());
            }
        }
    }

    fn owner(&self, target: &str) -> Option<&str> {
        let tab = self
            .records
            .get(target)
            .and_then(|record| record.tab_id.as_deref())
            .or_else(|| (self.records.get(target)?.target_type == "tab").then_some(target))?;
        self.owners.get(tab).map(String::as_str)
    }

    fn ownership_snapshot_if_changed(
        &mut self,
        previous: &HashMap<TargetId, BrowserSessionId>,
    ) -> Option<(u64, HashMap<TargetId, BrowserSessionId>)> {
        if &self.owners == previous {
            return None;
        }
        self.owner_revision = self.owner_revision.saturating_add(1);
        Some((self.owner_revision, self.owners.clone()))
    }

    /// Claims one currently live browser tab. Reclaiming by its existing owner
    /// is idempotent, but a retiring session may never claim or reclaim a tab.
    fn claim_tab(&mut self, session: &str, tab: &str) -> Result<()> {
        if self.retired_sessions.contains(session) {
            bail!("browser session {session} is retiring");
        }
        let record = self
            .records
            .get(tab)
            .ok_or_else(|| OwnershipError::UnknownTarget(tab.into()))?;
        if record.target_type != "tab" {
            return Err(OwnershipError::NotATab(tab.into()).into());
        }
        if let Some(owner) = self.owners.get(tab) {
            if owner != session {
                return Err(OwnershipError::OwnerConflict { target: tab.into() }.into());
            }
            return Ok(());
        }
        self.owners.insert(tab.into(), session.into());
        self.propagate_owners()
    }

    /// All tabs owned by `session`, in stable order. This is the single
    /// traversal behind page listing and creation-time capacity checks.
    fn session_tabs(&self, session: &str) -> Vec<TargetId> {
        let mut tabs = self
            .owners
            .iter()
            .filter(|(_, owner)| owner.as_str() == session)
            .map(|(tab, _)| tab.clone())
            .collect::<Vec<_>>();
        tabs.sort();
        tabs
    }

    /// Removes one session and all of its descendants, without affecting
    /// independently attached sibling roots (even when they share a tab).
    fn detach_subtree(&mut self, root: &str) {
        let mut pending = vec![root.to_owned()];
        let mut detached = Vec::new();
        while let Some(session) = pending.pop() {
            pending.extend(
                self.sessions
                    .iter()
                    .filter(|(_, child)| child.parent_session.as_deref() == Some(&session))
                    .map(|(id, _)| id.clone()),
            );
            if let Some(session) = self.sessions.remove(&session) {
                detached.push(session);
            }
        }
        for session in detached {
            let still_joined = self.sessions.values().any(|remaining| {
                remaining.target_id == session.target_id && remaining.tab_id == session.tab_id
            });
            if !still_joined
                && self
                    .records
                    .get(&session.target_id)
                    .is_some_and(|record| record.tab_id.as_deref() == Some(&session.tab_id))
                && let Some(record) = self.records.get_mut(&session.target_id)
            {
                record.tab_id = None;
            }
        }
    }
}

struct Command {
    method: &'static str,
    params: Value,
    session_id: Option<String>,
    reply: oneshot::Sender<Result<Value>>,
}

struct TrackerControl {
    shutdown: watch::Sender<()>,
    run_loop: Mutex<Option<tokio::task::JoinHandle<()>>>,
    abort: tokio::task::AbortHandle,
    stopped: watch::Sender<bool>,
}

/// A live, fail-closed browser Target tracker.
#[derive(Clone)]
pub struct TargetTracker {
    generation: String,
    state: Arc<Mutex<State>>,
    commands: mpsc::Sender<Command>,
    synchronize_lock: Arc<Mutex<()>>,
    tab_creation: Arc<Mutex<()>>,
    persistence: Arc<OwnershipPersistence>,
    control: Arc<TrackerControl>,
}

impl TargetTracker {
    /// Connects to a browser-level `webSocketDebuggerUrl`. The URL itself is
    /// the generation token; callers choose when to synchronize target state.
    pub async fn connect(
        websocket_url: impl Into<String>,
        persistence: Arc<OwnershipPersistence>,
        owners: HashMap<TargetId, BrowserSessionId>,
        owner_revision: u64,
    ) -> Result<Self> {
        let generation = websocket_url.into();
        let (socket, _) = connect_async(&generation)
            .await
            .with_context(|| format!("failed to connect to browser CDP websocket {generation}"))?;
        let state = Arc::new(Mutex::new(State {
            connected: true,
            owners,
            owner_revision,
            ..State::default()
        }));
        let (commands, receiver) = mpsc::channel(64);
        let (shutdown, shutdown_receiver) = watch::channel(());
        let (stopped, _) = watch::channel(false);
        let run_loop = tokio::spawn(run_loop(
            socket,
            receiver,
            state.clone(),
            generation.clone(),
            persistence.clone(),
            shutdown_receiver,
            stopped.clone(),
        ));
        let abort = run_loop.abort_handle();
        let tracker = Self {
            generation,
            state,
            commands,
            synchronize_lock: Arc::new(Mutex::new(())),
            tab_creation: Arc::new(Mutex::new(())),
            persistence,
            control: Arc::new(TrackerControl {
                shutdown,
                run_loop: Mutex::new(Some(run_loop)),
                abort,
                stopped,
            }),
        };
        Ok(tracker)
    }

    pub fn generation(&self) -> &str {
        &self.generation
    }

    /// Stops the event loop and waits for it to finish. Safe to call from any
    /// clone, and safe to call more than once.
    pub async fn shutdown(&self) {
        Self::shutdown_control(&self.control).await;
    }

    async fn shutdown_control(control: &Arc<TrackerControl>) {
        let _ = control.shutdown.send(());
        if let Some(mut run_loop) = control.run_loop.lock().await.take() {
            if timeout(TRACKER_SHUTDOWN_TIMEOUT, &mut run_loop)
                .await
                .is_err()
            {
                run_loop.abort();
                let _ = run_loop.await;
            }
            let _ = control.stopped.send(true);
            return;
        }
        let mut stopped = control.stopped.subscribe();
        let completed = timeout(TRACKER_SHUTDOWN_TIMEOUT, async {
            while !*stopped.borrow() {
                if stopped.changed().await.is_err() {
                    return;
                }
            }
        })
        .await;
        if completed.is_err() {
            control.abort.abort();
            let _ = control.stopped.send(true);
        }
    }

    pub async fn is_available(&self) -> bool {
        self.state.lock().await.connected
    }

    /// Refreshes the target snapshot and configures every discovered browser
    /// tab for session-scoped auto-attach. This is also the recovery boundary:
    /// a failed command leaves the tracker unavailable rather than stale.
    pub async fn synchronize(&self) -> Result<()> {
        let _single_flight = self.synchronize_lock.lock().await;
        let result = self.synchronize_inner().await;
        if result.is_err() {
            self.fail_closed().await;
        }
        result
    }

    async fn synchronize_inner(&self) -> Result<()> {
        let filter = json!([
            {"type":"tab","exclude":false},
            {"type":"page","exclude":false},
            {"exclude":true}
        ]);
        self.command(
            "Target.setDiscoverTargets",
            json!({"discover": true, "filter": &filter}),
            None,
        )
        .await?;
        self.command("Target.getTargets", json!({"filter": filter}), None)
            .await?;
        let state = self.state.lock().await;
        let attached = state
            .sessions
            .values()
            .filter(|session| session.parent_session.is_none())
            .map(|session| session.tab_id.clone())
            .collect::<HashSet<_>>();
        let tabs = state
            .records
            .values()
            .filter(|target| target.target_type == "tab" && !attached.contains(&target.id))
            .map(|target| target.id.clone())
            .collect::<Vec<_>>();
        drop(state);
        for tab in tabs {
            self.attach_tab(&tab).await?;
        }
        Ok(())
    }

    /// Explicitly assigns one exact browser `tab` to a browser session.
    /// Existing ownership is never overwritten.
    async fn claim_root_tab(&self, session: &str, tab: &str) -> Result<()> {
        let mut state = self.available().await?;
        let previous = state.owners.clone();
        state.claim_tab(session, tab)?;
        let snapshot = state.ownership_snapshot_if_changed(&previous);
        drop(state);
        if let Some((revision, owners)) = snapshot
            && let Err(error) = self
                .persistence
                .replace_owners(&self.generation, revision, owners)
        {
            return Err(error.context("failed to persist browser target ownership"));
        }
        Ok(())
    }

    pub async fn owner_of(&self, target: &str) -> Result<Option<BrowserSessionId>> {
        Ok(self.available().await?.owner(target).map(str::to_owned))
    }

    pub async fn activate_session(&self, session: &str) -> Result<()> {
        self.available().await?.retired_sessions.remove(session);
        Ok(())
    }

    /// Prevents later target events from recreating ownership after cleanup.
    pub async fn finalize_session(&self, session: &str) -> Result<()> {
        let mut state = self.available().await?;
        if state.owners.values().any(|owner| owner == session) {
            bail!("browser session still owns browser tabs");
        }
        state.retired_sessions.insert(session.to_owned());
        Ok(())
    }

    pub async fn owned_tabs(&self, session: &str) -> Result<Vec<TargetId>> {
        Ok(self.available().await?.session_tabs(session))
    }

    pub async fn is_live_tab(&self, tab: &str) -> Result<bool> {
        Ok(self
            .available()
            .await?
            .records
            .get(tab)
            .is_some_and(|record| record.target_type == "tab"))
    }

    /// Closes only a tab presently owned by `session`; target disappearance is
    /// confirmed by a later event/synchronization, never assumed locally.
    pub async fn close_owned_target(&self, session: &str, tab: &str) -> Result<()> {
        let state = self.available().await?;
        if state.owners.get(tab).map(String::as_str) != Some(session) {
            return Err(OwnershipError::NotOwner { target: tab.into() }.into());
        }
        drop(state);
        let result = self
            .command("Target.closeTarget", json!({"targetId": tab}), None)
            .await?;
        if result.get("success").and_then(Value::as_bool) != Some(true) {
            bail!("Target.closeTarget refused target {tab}");
        }
        self.wait_until_gone(tab, Duration::from_secs(5)).await
    }

    pub async fn close_browser(&self) -> Result<()> {
        self.command("Browser.close", json!({}), None).await?;
        Ok(())
    }

    /// Creates and claims exactly the target CDP created, never inferring
    /// ownership from another client's page-list delta.
    pub async fn create_owned_tab(
        &self,
        session: &str,
        url: &str,
        background: bool,
        layout: SessionLayout,
        preferred_anchor: Option<&str>,
        max_pages: usize,
    ) -> Result<TargetId> {
        let owned = {
            let state = self.available().await?;
            if state.retired_sessions.contains(session) {
                bail!("browser session {session} is retiring");
            }
            let owned = state.session_tabs(session);
            ensure_page_capacity(owned.len(), max_pages)?;
            owned
        };
        let (_creation, anchor) = match layout {
            SessionLayout::SharedWindow => (None, None),
            SessionLayout::WindowPerSession => {
                let creation = self.tab_creation.lock().await;
                let anchor = preferred_anchor
                    .filter(|preferred| owned.iter().any(|tab| tab == preferred))
                    .map(str::to_owned)
                    .or_else(|| owned.first().cloned());
                (Some(creation), anchor)
            }
        };
        let new_window = _creation.is_some() && anchor.is_none();
        let expected_window = if let Some(anchor) = anchor.as_deref() {
            let window = match self.target_window(anchor).await {
                Ok(window) => Some(window),
                Err(error) => {
                    warn!(%error, session_id = %session, tab = anchor, "could not identify session window before tab creation");
                    None
                }
            };
            if let Err(error) = self.activate_target(anchor).await {
                warn!(%error, session_id = %session, tab = anchor, "could not activate session window before tab creation");
            }
            window
        } else {
            None
        };
        let mut parameters = json!({"url": url, "background": background, "forTab": true});
        if new_window {
            parameters["newWindow"] = Value::Bool(true);
        }
        let result = self
            .command("Target.createTarget", parameters, None)
            .await?;
        let tab = result
            .get("targetId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow!("Target.createTarget returned no targetId"))?
            .to_owned();
        if let Err(error) = self.synchronize().await {
            // Best-effort cleanup of the exact ID returned by CDP. Do not
            // substitute a similarly timed target.
            let _ = self
                .command("Target.closeTarget", json!({"targetId": &tab}), None)
                .await;
            return Err(error);
        }
        if let Err(error) = self.claim_root_tab(session, &tab).await {
            let _ = self
                .command("Target.closeTarget", json!({"targetId": &tab}), None)
                .await;
            self.fail_closed().await;
            return Err(error);
        }
        if !background && let Err(error) = self.activate_target(&tab).await {
            warn!(%error, session_id = %session, tab, "could not focus new browser tab");
        }
        drop(_creation);
        if let Some(expected_window) = expected_window {
            match self.target_window(&tab).await {
                Ok(actual_window) if actual_window != expected_window => {
                    warn!(session_id = %session, tab, expected_window, actual_window, "browser placed new tab outside the session window");
                }
                Ok(_) => {}
                Err(error) if self.is_available().await => {
                    warn!(%error, session_id = %session, tab, "could not verify new tab window placement")
                }
                Err(error) => return Err(error),
            }
        }
        Ok(tab)
    }

    async fn activate_target(&self, tab: &str) -> Result<()> {
        self.command("Target.activateTarget", json!({"targetId": tab}), None)
            .await
            .map(|_| ())
    }

    async fn target_window(&self, tab: &str) -> Result<i64> {
        self.command("Browser.getWindowForTarget", json!({"targetId": tab}), None)
            .await?
            .get("windowId")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow!("Browser.getWindowForTarget returned no windowId for {tab}"))
    }

    pub async fn wait_until_gone(&self, tab: &str, duration: Duration) -> Result<()> {
        timeout(duration, async {
            loop {
                if !self.is_live_tab(tab).await? {
                    return Ok(());
                }
                self.synchronize().await?;
                if !self.is_live_tab(tab).await? {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .context("timed out waiting for closed CDP target")?
    }

    async fn available(&self) -> Result<tokio::sync::MutexGuard<'_, State>> {
        let state = self.state.lock().await;
        if !state.connected {
            return Err(OwnershipError::Unavailable.into());
        }
        Ok(state)
    }

    async fn fail_closed(&self) {
        self.state.lock().await.unavailable();
    }

    async fn attach_tab(&self, tab: &str) -> Result<()> {
        let result = self
            .command(
                "Target.attachToTarget",
                json!({"targetId": tab, "flatten": true}),
                None,
            )
            .await?;
        let session = result
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Target.attachToTarget returned no sessionId"))?
            .to_owned();
        self.state.lock().await.sessions.insert(
            session.clone(),
            CdpSession {
                target_id: tab.into(),
                tab_id: tab.into(),
                parent_session: None,
            },
        );
        self.command("Target.setAutoAttach", json!({"autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true, "filter": [{"type":"page","exclude":false}]}), Some(session)).await?;
        Ok(())
    }

    async fn command(
        &self,
        method: &'static str,
        params: Value,
        session_id: Option<String>,
    ) -> Result<Value> {
        drop(self.available().await?);
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Command {
                method,
                params,
                session_id,
                reply,
            })
            .await
            .map_err(|_| OwnershipError::Unavailable)?;
        match tokio::time::timeout(Duration::from_secs(5), receive).await {
            Ok(result) => result.map_err(|_| OwnershipError::Unavailable)?,
            Err(_) => {
                self.fail_closed().await;
                bail!("timed out waiting for CDP response to {method}");
            }
        }
    }
}

impl Drop for TargetTracker {
    fn drop(&mut self) {
        // Replacing a lifecycle drops its last tracker. Request shutdown here
        // as well as exposing `shutdown` so that an old generation cannot keep
        // processing events if its owner forgets to await explicit cleanup.
        if Arc::strong_count(&self.control) != 1 {
            return;
        }
        let _ = self.control.shutdown.send(());
        let control = self.control.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                TargetTracker::shutdown_control(&control).await;
            });
        }
    }
}

async fn run_loop<S>(
    socket: S,
    mut commands: mpsc::Receiver<Command>,
    state: Arc<Mutex<State>>,
    generation: String,
    persistence: Arc<OwnershipPersistence>,
    mut shutdown: watch::Receiver<()>,
    stopped: watch::Sender<bool>,
) where
    S: futures_util::Stream<
            Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>,
        > + futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error>
        + Unpin,
{
    let (mut write, mut read) = socket.split();
    let mut next_id = 1_u64;
    let mut pending = HashMap::new();
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_ok() {
                    break;
                }
            },
            command = commands.recv() => match command {
                Some(command) => {
                    let id = next_id; next_id += 1;
                    let mut request = json!({"id": id, "method": command.method, "params": command.params});
                    if let Some(session) = command.session_id { request["sessionId"] = Value::String(session); }
                    if let Err(error) = write.send(Message::Text(request.to_string().into())).await {
                        let _ = command.reply.send(Err(error.into())); break;
                    }
                    pending.insert(id, PendingCommand { method: command.method, reply: command.reply });
                }
                None => break,
            },
            message = read.next() => match message {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<Value>(&text) {
                    Ok(value) => handle_message(value, &mut pending, &state, &generation, &persistence).await,
                    Err(error) => { warn!(%error, "invalid CDP message; failing closed"); break; }
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {},
            },
        }
    }
    state.lock().await.unavailable();
    for (_, pending) in pending {
        let _ = pending.reply.send(Err(OwnershipError::Unavailable.into()));
    }
    let _ = stopped.send(true);
}

async fn handle_message(
    message: Value,
    pending: &mut HashMap<u64, PendingCommand>,
    state: &Arc<Mutex<State>>,
    generation: &str,
    persistence: &OwnershipPersistence,
) {
    let owners_before = state.lock().await.owners.clone();
    if let Some(id) = message.get("id").and_then(Value::as_u64) {
        if let Some(pending) = pending.remove(&id) {
            if !state.lock().await.connected {
                let _ = pending.reply.send(Err(OwnershipError::Unavailable.into()));
                return;
            }
            let mut answer = if let Some(error) = message.get("error") {
                Err(anyhow!("CDP {} command failed: {error}", pending.method))
            } else {
                let result = message.get("result").cloned().unwrap_or(Value::Null);
                if pending.method == "Target.getTargets" {
                    let applied = result
                        .get("targetInfos")
                        .and_then(Value::as_array)
                        .ok_or_else(|| anyhow!("Target.getTargets returned no targetInfos"))
                        .and_then(|targets| {
                            targets.iter().map(parse_target).collect::<Result<Vec<_>>>()
                        });
                    let snapshot_result = match applied {
                        Ok(records) => state.lock().await.replace_snapshot(records),
                        Err(error) => {
                            state.lock().await.unavailable();
                            Err(error)
                        }
                    };
                    if let Err(error) = snapshot_result {
                        state.lock().await.unavailable();
                        Err(error)
                    } else {
                        Ok(result)
                    }
                } else {
                    Ok(result)
                }
            };
            if answer.is_ok() {
                let snapshot = state
                    .lock()
                    .await
                    .ownership_snapshot_if_changed(&owners_before);
                if let Some((revision, owners)) = snapshot
                    && let Err(error) = persistence.replace_owners(generation, revision, owners)
                {
                    state.lock().await.unavailable();
                    answer = Err(error.context("failed to persist browser target ownership"));
                }
            }
            let _ = pending.reply.send(answer);
        }
        return;
    }
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return;
    };
    let params = message.get("params").unwrap_or(&Value::Null);
    let mut guard = state.lock().await;
    let outcome = match method {
        "Target.targetCreated" | "Target.targetInfoChanged" => params
            .get("targetInfo")
            .ok_or_else(|| anyhow!("{method} missing targetInfo"))
            .and_then(parse_target)
            .and_then(|record| guard.upsert(record)),
        "Target.targetDestroyed" => params
            .get("targetId")
            .and_then(Value::as_str)
            .map(|id| {
                guard.remove(id);
                Ok(())
            })
            .unwrap_or_else(|| Err(anyhow!("Target.targetDestroyed missing targetId"))),
        "Target.attachedToTarget" => attach_event(
            params,
            message.get("sessionId").and_then(Value::as_str),
            &mut guard,
        ),
        "Target.detachedFromTarget" => detach_event(params, &mut guard),
        _ => Ok(()),
    };
    if let Err(error) = outcome {
        warn!(%error, "invalid CDP Target event; failing closed");
        guard.unavailable();
    }
    let snapshot = guard.ownership_snapshot_if_changed(&owners_before);
    drop(guard);
    if let Some((revision, owners)) = snapshot
        && let Err(error) = persistence.replace_owners(generation, revision, owners)
    {
        warn!(%error, "failed to persist browser target ownership; failing closed");
        state.lock().await.unavailable();
    }
}

fn attach_event(params: &Value, enclosing_session: Option<&str>, state: &mut State) -> Result<()> {
    let session = params
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Target.attachedToTarget missing sessionId"))?;
    let record = parse_target(
        params
            .get("targetInfo")
            .ok_or_else(|| anyhow!("Target.attachedToTarget missing targetInfo"))?,
    )?;
    // For flattened sessions the enclosing CDP message carries the parent
    // session. Some protocol implementations additionally expose it in params.
    let parent_session =
        enclosing_session.or_else(|| params.get("parentSessionId").and_then(Value::as_str));
    let parent_tab = parent_session
        .map(|parent| {
            state
                .sessions
                .get(parent)
                .map(|session| session.tab_id.clone())
                .ok_or_else(|| {
                    anyhow!("Target.attachedToTarget has unknown parent session {parent}")
                })
        })
        .transpose()?;
    let id = record.id.clone();
    state.upsert(record)?;
    let tab_id = if let Some(parent_tab) = parent_tab
        && let Some(record) = state.records.get_mut(&id)
    {
        record.tab_id = Some(parent_tab.clone());
        parent_tab
    } else if state
        .records
        .get(&id)
        .is_some_and(|record| record.target_type == "tab")
    {
        id.clone()
    } else {
        bail!("Target.attachedToTarget root session {session} is not a tab");
    };
    state.sessions.insert(
        session.into(),
        CdpSession {
            target_id: id,
            tab_id,
            parent_session: parent_session.map(str::to_owned),
        },
    );
    state.propagate_owners()?;
    Ok(())
}

fn detach_event(params: &Value, state: &mut State) -> Result<()> {
    let session = params
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Target.detachedFromTarget missing sessionId"))?;
    if !state.sessions.contains_key(session) {
        bail!("Target.detachedFromTarget has unknown session {session}");
    }
    state.detach_subtree(session);
    Ok(())
}

struct PendingCommand {
    method: &'static str,
    reply: oneshot::Sender<Result<Value>>,
}

fn parse_target(value: &Value) -> Result<TargetRecord> {
    Ok(TargetRecord {
        id: value
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("targetInfo missing targetId"))?
            .into(),
        target_type: value
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("targetInfo missing type"))?
            .into(),
        opener_id: value
            .get("openerId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        tab_id: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn target(id: &str, kind: &str, opener: Option<&str>) -> TargetRecord {
        TargetRecord {
            id: id.into(),
            target_type: kind.into(),
            opener_id: opener.map(str::to_owned),
            tab_id: None,
        }
    }
    #[test]
    fn popup_lineage_propagates_after_out_of_order_events() {
        let mut state = State {
            connected: true,
            ..State::default()
        };
        state.upsert(target("popup", "page", Some("page"))).unwrap();
        state.upsert(target("page", "page", Some("tab"))).unwrap();
        state.upsert(target("tab", "tab", None)).unwrap();
        state.records.get_mut("page").unwrap().tab_id = Some("tab".into());
        state.owners.insert("tab".into(), "session-a".into());
        state.propagate_owners().unwrap();
        assert_eq!(state.owner("popup"), Some("session-a"));
    }
    #[test]
    fn distinct_popup_tab_inherits_its_openers_owner() {
        let mut state = State {
            connected: true,
            ..State::default()
        };
        state.upsert(target("opener-tab", "tab", None)).unwrap();
        state
            .upsert(TargetRecord {
                id: "opener-page".into(),
                target_type: "page".into(),
                opener_id: None,
                tab_id: Some("opener-tab".into()),
            })
            .unwrap();
        state.owners.insert("opener-tab".into(), "session-a".into());
        state
            .upsert(target("popup-tab", "tab", Some("opener-page")))
            .unwrap();
        assert_eq!(state.owner("popup-tab"), Some("session-a"));
        assert_eq!(
            state.owners.get("popup-tab").map(String::as_str),
            Some("session-a")
        );
        assert_eq!(state.owner("opener-tab"), Some("session-a"));
    }
    #[test]
    fn removing_tab_removes_its_explicit_ownership() {
        let mut state = State::default();
        state.upsert(target("tab", "tab", None)).unwrap();
        state.owners.insert("tab".into(), "session-a".into());
        state.remove("tab");
        assert_eq!(state.owner("tab"), None);
        assert!(state.owners.is_empty());
    }
    #[test]
    fn disconnect_preserves_generation_scoped_ownership_for_recovery() {
        let mut state = State {
            connected: true,
            ..State::default()
        };
        state.upsert(target("tab", "tab", None)).unwrap();
        state.owners.insert("tab".into(), "session-a".into());
        state.unavailable();
        assert!(!state.connected);
        assert!(state.records.contains_key("tab"));
        assert_eq!(
            state.owners.get("tab").map(String::as_str),
            Some("session-a")
        );
    }

    #[test]
    fn snapshots_preserve_joins_and_drop_stale_owners() {
        let mut state = State {
            connected: true,
            ..State::default()
        };
        state
            .upsert(TargetRecord {
                id: "page".into(),
                target_type: "page".into(),
                opener_id: None,
                tab_id: Some("tab".into()),
            })
            .unwrap();
        state.upsert(target("tab", "tab", None)).unwrap();
        state.owners.insert("tab".into(), "session-a".into());
        state
            .replace_snapshot(vec![
                target("page", "page", None),
                target("tab", "tab", None),
            ])
            .unwrap();
        assert_eq!(state.records["page"].tab_id.as_deref(), Some("tab"));
        assert_eq!(state.owner("page"), Some("session-a"));
        state.replace_snapshot(vec![]).unwrap();
        assert!(state.owners.is_empty());
    }

    #[test]
    fn target_info_change_preserves_attachment_derived_tab_join() {
        let mut state = State::default();
        state
            .upsert(TargetRecord {
                id: "page".into(),
                target_type: "page".into(),
                opener_id: None,
                tab_id: Some("tab".into()),
            })
            .unwrap();
        state.upsert(target("page", "page", None)).unwrap();
        assert_eq!(state.records["page"].tab_id.as_deref(), Some("tab"));
    }

    #[test]
    fn conflicting_popup_lineage_fails_closed() {
        let mut state = State::default();
        state.upsert(target("opener", "tab", None)).unwrap();
        state.owners.insert("opener".into(), "session-a".into());
        state.owners.insert("popup".into(), "session-b".into());
        let error = state
            .upsert(target("popup", "tab", Some("opener")))
            .unwrap_err();
        assert!(error.downcast_ref::<OwnershipError>().is_some());
    }

    #[test]
    fn detaching_child_attachment_preserves_sibling_joins() {
        let mut state = State::default();
        state.upsert(target("tab", "tab", None)).unwrap();
        state
            .upsert(TargetRecord {
                id: "page".into(),
                target_type: "page".into(),
                opener_id: None,
                tab_id: Some("tab".into()),
            })
            .unwrap();
        state.sessions.insert(
            "root-session".into(),
            CdpSession {
                target_id: "tab".into(),
                tab_id: "tab".into(),
                parent_session: None,
            },
        );
        state.sessions.insert(
            "child-session".into(),
            CdpSession {
                target_id: "page".into(),
                tab_id: "tab".into(),
                parent_session: Some("root-session".into()),
            },
        );
        state
            .upsert(TargetRecord {
                id: "sibling".into(),
                target_type: "page".into(),
                opener_id: None,
                tab_id: Some("tab".into()),
            })
            .unwrap();
        state.sessions.insert(
            "sibling-session".into(),
            CdpSession {
                target_id: "sibling".into(),
                tab_id: "tab".into(),
                parent_session: Some("root-session".into()),
            },
        );
        detach_event(&json!({"sessionId":"child-session"}), &mut state).unwrap();
        assert!(!state.sessions.contains_key("child-session"));
        assert_eq!(state.records["page"].tab_id, None);
        assert_eq!(state.records["sibling"].tab_id.as_deref(), Some("tab"));
        assert!(state.sessions.contains_key("sibling-session"));
    }

    #[test]
    fn nested_attachment_resolves_enclosing_attachment_session() {
        let mut state = State::default();
        state.upsert(target("tab", "tab", None)).unwrap();
        state.sessions.insert(
            "root".into(),
            CdpSession {
                target_id: "tab".into(),
                tab_id: "tab".into(),
                parent_session: None,
            },
        );
        attach_event(
            &json!({"sessionId":"child", "targetInfo":{"targetId":"page", "type":"page"}}),
            Some("root"),
            &mut state,
        )
        .unwrap();
        attach_event(
            &json!({"sessionId":"grandchild", "targetInfo":{"targetId":"worker", "type":"page"}}),
            Some("child"),
            &mut state,
        )
        .unwrap();
        assert_eq!(state.records["worker"].tab_id.as_deref(), Some("tab"));
        assert_eq!(state.sessions["grandchild"].target_id, "worker");
        detach_event(&json!({"sessionId":"child"}), &mut state).unwrap();
        assert_eq!(state.records["page"].tab_id, None);
        assert_eq!(state.records["worker"].tab_id, None);
        assert!(!state.sessions.contains_key("grandchild"));
    }

    #[test]
    fn detaching_root_session_preserves_sibling_root_on_same_tab() {
        let mut state = State::default();
        state.upsert(target("tab", "tab", None)).unwrap();
        state
            .upsert(TargetRecord {
                id: "page".into(),
                target_type: "page".into(),
                opener_id: None,
                tab_id: Some("tab".into()),
            })
            .unwrap();
        state.sessions.insert(
            "root".into(),
            CdpSession {
                target_id: "tab".into(),
                tab_id: "tab".into(),
                parent_session: None,
            },
        );
        state.sessions.insert(
            "child".into(),
            CdpSession {
                target_id: "page".into(),
                tab_id: "tab".into(),
                parent_session: Some("root".into()),
            },
        );
        state
            .upsert(TargetRecord {
                id: "independent-page".into(),
                target_type: "page".into(),
                opener_id: None,
                tab_id: Some("tab".into()),
            })
            .unwrap();
        state.sessions.insert(
            "sibling-root".into(),
            CdpSession {
                target_id: "independent-page".into(),
                tab_id: "tab".into(),
                parent_session: None,
            },
        );
        detach_event(&json!({"sessionId":"root"}), &mut state).unwrap();
        assert!(!state.sessions.contains_key("root"));
        assert!(!state.sessions.contains_key("child"));
        assert!(state.sessions.contains_key("sibling-root"));
        assert_eq!(state.records["page"].tab_id, None);
        assert_eq!(
            state.records["independent-page"].tab_id.as_deref(),
            Some("tab")
        );
    }

    #[test]
    fn unknown_attachment_parent_fails_closed() {
        let mut state = State::default();
        let error = attach_event(
            &json!({"sessionId":"child", "targetInfo":{"targetId":"page", "type":"page"}}),
            Some("missing-root"),
            &mut state,
        )
        .unwrap_err();
        assert!(error.to_string().contains("unknown parent session"));
        assert!(!state.records.contains_key("page"));
        assert!(state.sessions.is_empty());
    }

    #[test]
    fn finalized_session_cannot_reclaim_a_late_target() {
        let mut state = State::default();
        state.retired_sessions.insert("session-a".into());
        let error = state.claim_tab("session-a", "late-tab").unwrap_err();
        assert!(error.to_string().contains("retiring"));
        assert!(!state.owners.contains_key("late-tab"));
    }

    #[test]
    fn page_capacity_counts_only_the_requesting_session() {
        let mut state = State::default();
        state.owners.insert("tab-b".into(), "session-a".into());
        state.owners.insert("tab-a".into(), "session-a".into());
        state.owners.insert("tab-c".into(), "session-b".into());
        assert_eq!(state.session_tabs("session-a"), ["tab-a", "tab-b"]);
        assert!(ensure_page_capacity(2, 3).is_ok());
        let error = ensure_page_capacity(2, 2).unwrap_err();
        assert!(error.to_string().contains("already owns 2 pages (max 2)"));
    }
}
