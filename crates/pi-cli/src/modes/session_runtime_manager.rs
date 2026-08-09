//! Multi-session runtime manager for the Web/listen control plane.
//!
//! The listener historically shared ONE live [`Application`] across every
//! Web client, so opening a second session stopped the first one's work
//! (`wait_for_idle` + in-place cutover). This manager instead owns an
//! independent [`Application`], [`RpcDispatcher`], and per-runtime state per
//! session:
//!
//! - Top-level RPC `sessionId` selects a runtime; absent `sessionId` targets
//!   the initial (primary) runtime for compatibility.
//! - `switch_session` / `new_session` / `fork` / `clone` are manager-level
//!   lifecycle operations: they build an INDEPENDENT runtime and return the
//!   target snapshot `{ sessionId, state, messages }` without mutating or
//!   stopping the source runtime.
//! - `close_session` is a non-destructive resource-lifecycle operation: it
//!   rejects the primary and rejects busy secondaries; only an idle
//!   secondary is cleaned up (fan-in forwarder aborted, side-chat shut down,
//!   `Application::cleanup`, registry removal).
//! - Every projected application and extension event carries a top-level
//!   `sessionId` identifying the OWNING/source runtime (the `session_forked`
//!   payload target is renamed `forkedSessionId`).
//! - Loaded sessions are capped at [`MAX_LOADED_SESSIONS`] (never evicted);
//!   non-inline commands additionally take a process-wide
//!   [`MAX_CONCURRENT_SESSION_COMMANDS`] permit; abort/stop/signal/close
//!   bypass both.
//!
//! The manager is owned by [`crate::modes::listen::ListenHandle`]: listener
//! shutdown aborts every fan-in forwarder, then cleans manager-owned
//! non-primary runtimes. The primary Web listener [`Application`] remains
//! owned and cleaned by `lib.rs`.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Weak};

use anyhow::{Context, Result, anyhow, bail};
use pi_ai::Message;
use pi_coding::Application;
use serde_json::{Value, json};
use tokio::sync::{RwLock, Semaphore, broadcast};

use crate::extension_ui::{ExtensionUiAdapter, ExtensionUiEvent};
use crate::modes::rpc::{
    MAX_CONCURRENT_COMMANDS, RpcCommand, RpcDispatcher, RpcExtensionUiRequest, RpcResponse,
    RpcSessionState, project_application_event, project_extension_ui_event, public_message,
};
use crate::session_run::fallback_session_identity;

/// Maximum concurrently loaded sessions, counting the primary TUI runtime.
/// Reaching the cap rejects further `switch_session` / `new_session` /
/// `fork` / `clone` opens; a running session is NEVER evicted implicitly.
pub const MAX_LOADED_SESSIONS: usize = 8;

/// Process-wide cap on non-inline RPC commands across ALL sessions, on top of
/// each session's own [`MAX_CONCURRENT_COMMANDS`] permit pool. Abort, stop,
/// signal, and close safety operations bypass both caps; permits release as
/// commands complete, so the overload errors recover naturally.
pub const MAX_CONCURRENT_SESSION_COMMANDS: usize = 32;

/// Fan-in event buffer: the per-runtime forwarders never block (broadcast
/// drops for lagged receivers), so this only bounds how far a slow client can
/// lag before receiving a lag-failure record.
const EVENT_FANIN_CAPACITY: usize = 1024;

/// What a spawned runtime should be built from.
#[derive(Clone, Debug)]
pub enum SessionSpawnKind {
    /// Open (resume) the persisted session at this path (switch_session).
    Open { resume_path: PathBuf },
    /// Start a brand-new recorded session (new_session).
    Fresh,
    /// Fork the source session at the given entry id.
    Fork { entry_id: String },
    /// Clone the source session's active leaf.
    Clone,
}

/// Request passed to a [`SessionSpawner`]. `source` is the runtime the
/// lifecycle command arrived on (or the primary); the spawned runtime derives
/// its `SessionOptions` (model, thinking, auth resolver, ...) from it.
pub struct SessionSpawnRequest {
    pub kind: SessionSpawnKind,
    pub source: Application,
}

/// A freshly built runtime ready for the manager registry.
pub struct SessionSpawnResult {
    pub session_id: String,
    pub session_file: Option<PathBuf>,
    pub application: Application,
    pub extension_ui: ExtensionUiAdapter,
}

/// Factory for building manager-owned runtimes. Production wires
/// [`crate::session_run::RunSessionSpawner`] (sanitized CLI clone +
/// `session_run::build_session` policy); tests inject a faux implementation.
pub trait SessionSpawner: Send + Sync {
    fn spawn(
        &self,
        request: SessionSpawnRequest,
    ) -> Pin<Box<dyn Future<Output = Result<SessionSpawnResult>> + Send>>;
}

/// One loaded session runtime: independent application, extension UI adapter,
/// dispatcher (per-session Settings/Workflow/SideChat state), and fan-in
/// forwarder.
struct SessionRuntime {
    application: Application,
    extension_ui: ExtensionUiAdapter,
    dispatcher: RpcDispatcher,
    /// Interior mutability: the fan-in forwarder task is spawned after the
    /// runtime is registered, and shutdown/close abort it through the Arc.
    forwarder: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl SessionRuntime {
    fn new(application: Application, extension_ui: ExtensionUiAdapter) -> Self {
        let dispatcher = RpcDispatcher::new(application.clone());
        Self {
            application,
            extension_ui,
            dispatcher,
            forwarder: std::sync::Mutex::new(None),
        }
    }

    /// Live recorder identity of this runtime's session (None for a primary
    /// running with `--no-session`). Re-read per event/command so in-place
    /// identity cutovers (TUI/Web fork/clone on the same runtime) are
    /// reflected without re-registering the runtime.
    fn current_identity(&self) -> Option<(String, PathBuf)> {
        self.application.session().recorder_info()
    }

    fn abort_forwarder(&self) {
        if let Some(handle) = self.forwarder.lock().expect("forwarder lock").take() {
            handle.abort();
        }
    }
}

/// The manager. All creation/open paths serialize through `create_lock` so
/// the same persisted session can never be opened twice (two recorders
/// appending one JSONL would corrupt it); prompts across runtimes execute
/// concurrently.
pub(crate) struct SessionRuntimeManager {
    primary: Arc<SessionRuntime>,
    /// Registry keyed by session id. Stale pre-cutover ids are kept as
    /// aliases so in-flight Web frames keep routing after an in-place
    /// fork/clone; the runtime is removed wholesale on close.
    by_id: RwLock<HashMap<String, Arc<SessionRuntime>>>,
    /// Registry keyed by canonical session file path (switch_session dedup).
    by_path: RwLock<HashMap<PathBuf, Arc<SessionRuntime>>>,
    /// Serializes lifecycle creation/open so duplicate opens and cap races
    /// cannot interleave.
    create_lock: tokio::sync::Mutex<()>,
    /// Process-wide non-inline command permit pool.
    global_slots: Arc<Semaphore>,
    factory: Option<Arc<dyn SessionSpawner>>,
    /// Fan-in: every runtime's forwarder publishes into these; every Web
    /// connection subscribes here (multi-client fan-out).
    events: broadcast::Sender<Value>,
    ui_events: broadcast::Sender<RpcExtensionUiRequest>,
    /// Never-read receivers keeping the fan-in channels alive while the
    /// manager lives (broadcast send fails when zero receivers exist).
    _keepalive_events: broadcast::Receiver<Value>,
    _keepalive_ui: broadcast::Receiver<RpcExtensionUiRequest>,
}

impl SessionRuntimeManager {
    pub(crate) async fn new(
        primary_application: Application,
        primary_extension_ui: ExtensionUiAdapter,
        factory: Option<Arc<dyn SessionSpawner>>,
    ) -> Arc<Self> {
        let (events, keepalive_events) = broadcast::channel(EVENT_FANIN_CAPACITY);
        let (ui_events, keepalive_ui) = broadcast::channel(EVENT_FANIN_CAPACITY);
        let primary = Arc::new(SessionRuntime::new(
            primary_application,
            primary_extension_ui,
        ));
        let manager = Arc::new(Self {
            primary: primary.clone(),
            by_id: RwLock::new(HashMap::new()),
            by_path: RwLock::new(HashMap::new()),
            create_lock: tokio::sync::Mutex::new(()),
            global_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_SESSION_COMMANDS)),
            factory,
            events,
            ui_events,
            _keepalive_events: keepalive_events,
            _keepalive_ui: keepalive_ui,
        });
        manager
            .register_runtime(
                &primary,
                primary.current_identity().or_else(|| {
                    // A `--no-session` primary has no recorder identity, but it
                    // still occupies a concurrent-session slot: register it under
                    // the same process-unique fallback the RpcDispatcher reports,
                    // so the cap counts it and never double-registers.
                    Some((fallback_session_identity().to_owned(), PathBuf::new()))
                }),
            )
            .await;
        SessionRuntime::start_forwarder(&manager, &primary, manager.events.clone(), manager.ui_events.clone());
        manager
    }

    pub(crate) fn events(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }

    pub(crate) fn ui_events(&self) -> broadcast::Receiver<RpcExtensionUiRequest> {
        self.ui_events.subscribe()
    }

    async fn register_runtime(
        &self,
        runtime: &Arc<SessionRuntime>,
        identity: Option<(String, PathBuf)>,
    ) {
        if let Some((session_id, session_file)) = identity {
            let mut by_id = self.by_id.write().await;
            by_id.entry(session_id).or_insert_with(|| runtime.clone());
            drop(by_id);
            if !session_file.as_os_str().is_empty() {
                let mut by_path = self.by_path.write().await;
                by_path
                    .entry(Self::canonical_session_key(&session_file))
                    .or_insert_with(|| runtime.clone());
            }
        }
    }

    /// Canonical key for the by_path registry. The same physical session
    /// file must map to one key no matter which side produced the path
    /// (prepared resume path vs spawned recorder path); otherwise dedup
    /// misses and two recorders append to one JSONL.
    fn canonical_session_key(path: &Path) -> PathBuf {
        path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
    }

    /// Add the runtime's CURRENT id/path as aliases (called by the forwarder
    /// when an in-place identity cutover is detected). Stale aliases are kept
    /// so the Web client's pre-cutover sessionId keeps routing.
    async fn update_identity(&self, runtime: &Arc<SessionRuntime>) {
        let Some((session_id, session_file)) = runtime.current_identity() else {
            return;
        };
        let mut by_id = self.by_id.write().await;
        by_id.entry(session_id.clone()).or_insert_with(|| runtime.clone());
        drop(by_id);
        if !session_file.as_os_str().is_empty() {
            let mut by_path = self.by_path.write().await;
            by_path
                .entry(Self::canonical_session_key(&session_file))
                .or_insert_with(|| runtime.clone());
        }
    }

    /// Resolve the target runtime for a command. Absent sessionId targets the
    /// primary; unknown ids fail closed (never silently routed to primary).
    async fn resolve(&self, session_id: Option<&str>) -> Result<Arc<SessionRuntime>> {
        match session_id {
            None => Ok(self.primary.clone()),
            Some(id) => self.by_id.read().await.get(id).cloned().ok_or_else(|| {
                anyhow!("unknown session {id}")
            }),
        }
    }
    /// Recorder-authoritative, path-free public snapshot for a collaboration room.
    pub(crate) async fn collab_snapshot(
        &self,
        session_id: Option<&str>,
        max_entries: usize,
        max_bytes: usize,
    ) -> Result<(String, Value)> {
        let runtime = self.resolve(session_id).await?;
        let (session_id, _) = runtime
            .current_identity()
            .ok_or_else(|| anyhow!("session recording is unavailable"))?;
        let snapshot = runtime
            .application
            .collab_public_snapshot(max_entries, max_bytes)
            .map_err(|_| anyhow!("collaboration snapshot is unavailable"))?;
        Ok((session_id, snapshot))
    }

    /// Resolve the source runtime for manager-level lifecycle commands. The
    /// lifecycle op itself never mutates the source; the id only selects
    /// which runtime's options the child derives from. Unknown ids FAIL
    /// CLOSED: a stale or fabricated sessionId must not silently rebind the
    /// op to the primary.
    async fn resolve_source(self: &Arc<Self>, session_id: Option<&str>) -> Result<Arc<SessionRuntime>> {
        match session_id {
            None => Ok(self.primary.clone()),
            Some(id) => self.by_id.read().await.get(id).cloned().ok_or_else(|| {
                anyhow!("unknown session {id}")
            }),
        }
    }

    /// Count DISTINCT runtimes, not identity aliases: in-place cutovers keep
    /// the pre-cutover sessionId routing, so one runtime can occupy several
    /// by_id keys. Counting keys would let aliases exhaust the cap and reject
    /// genuinely free slots.
    async fn loaded_count(&self) -> usize {
        let by_id = self.by_id.read().await;
        let mut seen = std::collections::HashSet::new();
        for runtime in by_id.values() {
            seen.insert(Arc::as_ptr(runtime) as usize);
        }
        seen.len()
    }

    async fn check_cap(&self) -> Result<()> {
        if self.loaded_count().await >= MAX_LOADED_SESSIONS {
            bail!(
                "too many concurrent sessions (limit {MAX_LOADED_SESSIONS}); close an idle session first"
            );
        }
        Ok(())
    }

    /// HTTP `/rpc` entry: full caps (global 32 + per-session 16), safety and
    /// manager-level commands bypass both.
    pub(crate) async fn dispatch(
        self: &Arc<Self>,
        command: RpcCommand,
        session_id: Option<String>,
    ) -> RpcResponse {
        if command.bypasses_command_slots() || is_manager_level_command(&command) {
            return self.dispatch_inner(command, session_id).await;
        }
        let id = command.id();
        let name = command.command_name();
        let Ok(_global) = self.global_slots.clone().try_acquire_owned() else {
            return RpcResponse::failure(
                id,
                name,
                format!(
                    "too many concurrent RPC commands across sessions (limit {MAX_CONCURRENT_SESSION_COMMANDS})"
                ),
            );
        };
        self.dispatch_target(command, session_id, true).await
    }

    /// WebSocket inline entry (no per-session permit pool; the connection's
    /// own inline loop is inherently serial). Manager-level commands are
    /// handled; safety commands bypass the global cap.
    pub(crate) async fn dispatch_inner(
        self: &Arc<Self>,
        command: RpcCommand,
        session_id: Option<String>,
    ) -> RpcResponse {
        if is_manager_level_command(&command) {
            return self.manager_level(command, session_id).await;
        }
        let id = command.id();
        let name = command.command_name();
        let Some(runtime) = self.resolve(session_id.as_deref()).await.ok() else {
            return unknown_session_response(id, name, session_id);
        };
        let response = runtime.dispatcher.dispatch_inner(command).await;
        self.overlay_loaded_sessions(response).await
    }

    /// WebSocket spawned-command entry: takes the global permit (the
    /// per-connection JoinSet bounds per-connection concurrency), then runs
    /// without the per-session pool exactly like the pre-manager WebSocket
    /// path did.
    pub(crate) async fn dispatch_spawned(
        self: &Arc<Self>,
        command: RpcCommand,
        session_id: Option<String>,
    ) -> RpcResponse {
        if command.bypasses_command_slots() || is_manager_level_command(&command) {
            return self.dispatch_inner(command, session_id).await;
        }
        let id = command.id();
        let name = command.command_name();
        let Ok(_global) = self.global_slots.clone().try_acquire_owned() else {
            return RpcResponse::failure(
                id,
                name,
                format!(
                    "too many concurrent RPC commands across sessions (limit {MAX_CONCURRENT_SESSION_COMMANDS})"
                ),
            );
        };
        self.dispatch_target(command, session_id, false).await
    }

    /// Route to the target runtime's dispatcher, optionally through its own
    /// per-session permit pool, then enrich session-scoped responses.
    async fn dispatch_target(
        self: &Arc<Self>,
        command: RpcCommand,
        session_id: Option<String>,
        use_per_session_slots: bool,
    ) -> RpcResponse {
        let id = command.id();
        let name = command.command_name();
        let Some(runtime) = self.resolve(session_id.as_deref()).await.ok() else {
            return unknown_session_response(id, name, session_id);
        };
        let response = if use_per_session_slots {
            runtime.dispatcher.dispatch(command).await
        } else {
            runtime.dispatcher.dispatch_inner(command).await
        };
        self.overlay_loaded_sessions(response).await
    }

    /// Manager-level lifecycle commands: switch_session, new_session, fork,
    /// clone, close_session. The source runtime is never mutated or stopped.
    async fn manager_level(
        self: &Arc<Self>,
        command: RpcCommand,
        session_id: Option<String>,
    ) -> RpcResponse {
        let id = command.id();
        let name = command.command_name();
        let source = match self.resolve_source(session_id.as_deref()).await {
            Ok(source) => source,
            Err(error) => return RpcResponse::failure(id, name, error.to_string()),
        };
        match command {
            RpcCommand::SwitchSession { session_path, .. } => {
                match self.open_persisted(&source, &session_path).await {
                    Ok(runtime) => self.snapshot_response(id, name, &runtime).await,
                    Err(error) => RpcResponse::failure(id, name, error.to_string()),
                }
            }
            RpcCommand::NewSession { .. } => match self.create_fresh(&source).await {
                Ok(runtime) => self.snapshot_response(id, name, &runtime).await,
                Err(error) => RpcResponse::failure(id, name, error.to_string()),
            },
            RpcCommand::Fork { entry_id, .. } => match self.fork_from(&source, &entry_id).await {
                Ok(runtime) => self.snapshot_response(id, name, &runtime).await,
                Err(error) => RpcResponse::failure(id, name, error.to_string()),
            },
            RpcCommand::Clone { .. } => match self.clone_from(&source).await {
                Ok(runtime) => self.snapshot_response(id, name, &runtime).await,
                Err(error) => RpcResponse::failure(id, name, error.to_string()),
            },
            RpcCommand::CloseSession { .. } => self.close_session(id, name, session_id).await,
            _ => unreachable!("manager_level only receives lifecycle commands"),
        }
    }

    /// `switch_session`: ensure/open the requested persisted session without
    /// mutating the source. The target is identified by trusted server-side
    /// path validation ([`pi_coding::PreparedSessionResume::prepare_path`],
    /// the same validation the in-place switch uses) and canonical-path
    /// dedup: an already-loaded canonical path returns the SAME runtime so
    /// two recorders never append one JSONL.
    async fn open_persisted(self: &Arc<Self>, source: &Arc<SessionRuntime>, session_path: &str) -> Result<Arc<SessionRuntime>> {
        let _guard = self.create_lock.lock().await;
        let raw = Path::new(session_path);
        // Dedup to an already-loaded runtime BEFORE requiring the file to
        // exist on disk. A session that is loaded but whose recorder has not
        // yet flushed its first assistant message has NO file on disk (the
        // lazy recorder holds the header in memory), so `prepare_path` would
        // reject its valid path and `switch_session` to it would fail with
        // "invalid session path". The `by_path` registry keys the runtime by
        // its recorder path from the moment it is loaded, so a loaded target
        // (flushed or not) always dedups here; `prepare_path` only runs for a
        // genuinely-not-loaded resume, which requires the persisted file.
        // Both the canonical key (matches a flushed, canonical-registered
        // runtime) and the raw key (matches a runtime registered while its
        // file was still unflushed) are checked so a flush after load cannot
        // split one physical session into two recorders.
        let existing = {
            let by_path = self.by_path.read().await;
            by_path.get(&Self::canonical_session_key(raw)).cloned()
                .or_else(|| by_path.get(raw).cloned())
        };
        if let Some(runtime) = existing {
            return Ok(runtime);
        }
        let prepared = pi_coding::PreparedSessionResume::prepare_path(raw)
            .with_context(|| format!("invalid session path {session_path:?}"))?;
        let canonical = Self::canonical_session_key(prepared.path());
        if let Some(runtime) = self.by_path.read().await.get(&canonical).cloned() {
            return Ok(runtime);
        }
        self.check_cap().await?;
        let factory = self
            .factory
            .as_ref()
            .ok_or_else(|| anyhow!("session factory is unavailable"))?
            .clone();
        let request = SessionSpawnRequest {
            kind: SessionSpawnKind::Open { resume_path: canonical },
            source: source.application.clone(),
        };
        let result = factory.spawn(request).await?;
        let runtime = self.register_spawned(result).await;
        Ok(runtime)
    }

    /// `new_session`: build an independent runtime even while the source is
    /// prompting, and return its snapshot.
    async fn create_fresh(self: &Arc<Self>, source: &Arc<SessionRuntime>) -> Result<Arc<SessionRuntime>> {
        let _guard = self.create_lock.lock().await;
        self.check_cap().await?;
        let factory = self
            .factory
            .as_ref()
            .ok_or_else(|| anyhow!("session factory is unavailable"))?
            .clone();
        let request = SessionSpawnRequest {
            kind: SessionSpawnKind::Fresh,
            source: source.application.clone(),
        };
        let result = factory.spawn(request).await?;
        Ok(self.register_spawned(result).await)
    }

    async fn fork_from(self: &Arc<Self>, source: &Arc<SessionRuntime>, entry_id: &str) -> Result<Arc<SessionRuntime>> {
        let _guard = self.create_lock.lock().await;
        self.check_cap().await?;
        let factory = self
            .factory
            .as_ref()
            .ok_or_else(|| anyhow!("session factory is unavailable"))?
            .clone();
        let request = SessionSpawnRequest {
            kind: SessionSpawnKind::Fork { entry_id: entry_id.to_owned() },
            source: source.application.clone(),
        };
        let result = factory.spawn(request).await?;
        Ok(self.register_spawned(result).await)
    }

    async fn clone_from(self: &Arc<Self>, source: &Arc<SessionRuntime>) -> Result<Arc<SessionRuntime>> {
        let _guard = self.create_lock.lock().await;
        self.check_cap().await?;
        let factory = self
            .factory
            .as_ref()
            .ok_or_else(|| anyhow!("session factory is unavailable"))?
            .clone();
        let request = SessionSpawnRequest {
            kind: SessionSpawnKind::Clone,
            source: source.application.clone(),
        };
        let result = factory.spawn(request).await?;
        Ok(self.register_spawned(result).await)
    }

    async fn register_spawned(self: &Arc<Self>, result: SessionSpawnResult) -> Arc<SessionRuntime> {
        let runtime = Arc::new(SessionRuntime::new(result.application, result.extension_ui));
        self.register_runtime(&runtime, Some((result.session_id, result.session_file.unwrap_or_default()))).await;
        SessionRuntime::start_forwarder(self, &runtime, self.events.clone(), self.ui_events.clone());
        runtime
    }

    /// `close_session`: requires a sessionId; rejects the primary and rejects
    /// busy secondaries WITHOUT cancelling any active work; only an idle
    /// secondary is cleaned up and removed from the registry.
    async fn close_session(
        self: &Arc<Self>,
        id: Option<String>,
        name: &str,
        session_id: Option<String>,
    ) -> RpcResponse {
        let Some(sid) = session_id else {
            return RpcResponse::failure(id, name, "close_session requires a sessionId");
        };
        let Some(runtime) = self.by_id.read().await.get(&sid).cloned() else {
            return RpcResponse::failure(id, name, format!("unknown session {sid}"));
        };
        if Arc::ptr_eq(&runtime, &self.primary) {
            return RpcResponse::failure(id, name, "cannot close the primary TUI session");
        }
        if let Err(error) = self.ensure_idle(&runtime).await {
            return RpcResponse::failure(id, name, format!("session is busy: {error}"));
        }
        self.shutdown_runtime(&runtime).await;
        RpcResponse::success(id, name, Some(json!({ "closed": true, "sessionId": sid })))
    }

    /// Conservative idle check: close must never silently cancel user work.
    async fn ensure_idle(self: &Arc<Self>, runtime: &Arc<SessionRuntime>) -> Result<()> {
        let application = &runtime.application;
        let state = application.state().await;
        if state.is_streaming {
            bail!("a turn is in progress");
        }
        if state.is_compacting {
            bail!("compaction is in progress");
        }
        if state.pending_message_count > 0 {
            bail!("queued messages are pending");
        }
        if !application.process_list().is_empty() {
            bail!("supervised processes are running");
        }
        if application.goal_state().current.is_some() {
            bail!("a goal is active");
        }
        let todo = application.todo_state();
        if todo.phases.iter().any(|phase| {
            phase
                .tasks
                .iter()
                .any(|task| task.status == pi_coding::todo::TodoStatus::InProgress)
        }) {
            bail!("todo tasks are in progress");
        }
        if let Ok(manager) = application.workflow_manager()
            && manager
                .list()
                .iter()
                .any(|workflow| workflow.status.is_active())
        {
            bail!("workflows are running");
        }
        if let Some(orchestration) = application.orchestration_runtime()
            && orchestration
                .jobs(None)
                .iter()
                .any(|job| !job.status.is_settled())
        {
            bail!("orchestration jobs are running");
        }
        if let Ok(loops) = application.loop_list().await
            && !loops.is_empty()
        {
            bail!("loops are scheduled or running");
        }
        if runtime.dispatcher.side_chat_busy().await {
            bail!("a side-chat turn is in progress");
        }
        Ok(())
    }

    /// Abort the fan-in forwarder, shut down the side-chat controller, clean
    /// the Application (processes, workflows, loops, orchestration,
    /// extensions, MCP), and drop every registry entry pointing at it.
    async fn shutdown_runtime(self: &Arc<Self>, runtime: &Arc<SessionRuntime>) {
        runtime.abort_forwarder();
        runtime.dispatcher.shutdown_side_chat().await;
        runtime.application.cleanup().await;
        let mut by_id = self.by_id.write().await;
        by_id.retain(|_, value| !Arc::ptr_eq(value, runtime));
        drop(by_id);
        let mut by_path = self.by_path.write().await;
        by_path.retain(|_, value| !Arc::ptr_eq(value, runtime));
    }

    /// Listener shutdown: abort EVERY fan-in forwarder (primary + children),
    /// then clean manager-owned non-primary runtimes. The primary Application
    /// stays alive — `lib.rs` owns and cleans it after the listener stops.
    pub(crate) async fn shutdown(&self) {
        let runtimes = {
            let by_id = self.by_id.read().await;
            by_id.values().cloned().collect::<Vec<_>>()
        };
        for runtime in &runtimes {
            runtime.abort_forwarder();
        }
        for runtime in &runtimes {
            if !Arc::ptr_eq(runtime, &self.primary) {
                runtime.dispatcher.shutdown_side_chat().await;
                runtime.application.cleanup().await;
            }
        }
        let mut by_id = self.by_id.write().await;
        by_id.retain(|_, value| Arc::ptr_eq(value, &self.primary));
        drop(by_id);
        let mut by_path = self.by_path.write().await;
        by_path.retain(|_, value| Arc::ptr_eq(value, &self.primary));
    }

    /// The `{ sessionId, state, messages }` snapshot contract shared by every
    /// lifecycle response: target identity, full RPC state, and the public
    /// message transcript straight from the backend recorder (authoritative
    /// history, never a frontend cache).
    async fn snapshot_response(
        self: &Arc<Self>,
        id: Option<String>,
        name: &str,
        runtime: &Arc<SessionRuntime>,
    ) -> RpcResponse {
        let result: Result<Value> = async {
            let state = RpcSessionState::from_application(
                runtime.application.state().await,
                runtime.application.runtime_settings_state(),
                runtime.application.session().cwd(),
            );
            let messages: Vec<Message> = runtime
                .application
                .messages()
                .into_iter()
                .map(public_message)
                .collect();
            let session_id = runtime
                .application
                .session()
                .recorder_info()
                .map(|(id, _)| id)
                .ok_or_else(|| anyhow!("session has no recorder id"))?;
            Ok(json!({
                "sessionId": session_id,
                "state": state,
                "messages": messages,
            }))
        }
        .await;
        match result {
            Ok(data) => RpcResponse::success(id, name, Some(data)),
            Err(error) => RpcResponse::failure(id, name, error.to_string()),
        }
    }

    /// Mark rows of `session_list` whose session is currently loaded, so the
    /// Web sidebar can reconcile live sessions against the persisted catalog.
    async fn overlay_loaded_sessions(self: &Arc<Self>, response: RpcResponse) -> RpcResponse {
        let Some(mut data) = response.data else {
            return response;
        };
        if data.get("sessions").is_none() {
            return RpcResponse { data: Some(data), ..response };
        }
        let loaded: HashSet<String> = {
            let by_id = self.by_id.read().await;
            by_id.keys().cloned().collect()
        };
        let Some(sessions) = data.get_mut("sessions").and_then(Value::as_array_mut) else {
            return RpcResponse { data: Some(data), ..response };
        };
        for row in sessions.iter_mut() {
            if let Some(sid) = row.get("sessionId").and_then(Value::as_str)
                && loaded.contains(sid)
                && let Some(object) = row.as_object_mut()
            {
                object.insert("loaded".to_owned(), json!(true));
            }
        }
        let listed = sessions
            .iter()
            .filter_map(|row| row.get("sessionId").and_then(Value::as_str))
            .map(str::to_owned)
            .collect::<HashSet<_>>();
        let runtimes = {
            let by_id = self.by_id.read().await;
            let mut seen = HashSet::new();
            by_id
                .values()
                .filter(|runtime| seen.insert(Arc::as_ptr(runtime) as usize))
                .cloned()
                .collect::<Vec<_>>()
        };
        for runtime in runtimes {
            let Some((session_id, path)) = runtime.current_identity() else {
                continue;
            };
            if listed.contains(&session_id) {
                continue;
            }
            let state = runtime.application.state().await;
            sessions.push(json!({
                "source": if Arc::ptr_eq(&runtime, &self.primary) { "primary" } else { "native" },
                "sessionId": session_id,
                "name": state.session_name,
                "cwd": runtime.application.session().cwd().to_string_lossy(),
                "displayTime": "",
                "modifiedEpoch": 0.0,
                "summary": "",
                "path": path.to_string_lossy(),
                "size": 0,
                "messageCount": state.message_count,
                "status": "native",
                "loaded": true,
            }));
        }
        RpcResponse { data: Some(data), ..response }
    }
}

/// Which commands are handled by the manager instead of a session dispatcher.
fn is_manager_level_command(command: &RpcCommand) -> bool {
    matches!(
        command,
        RpcCommand::SwitchSession { .. }
            | RpcCommand::NewSession { .. }
            | RpcCommand::Fork { .. }
            | RpcCommand::Clone { .. }
            | RpcCommand::CloseSession { .. }
    )
}

fn unknown_session_response(
    id: Option<String>,
    name: &str,
    session_id: Option<String>,
) -> RpcResponse {
    let error = match session_id {
        Some(sid) => format!("unknown session {sid}"),
        None => "session is unavailable".to_owned(),
    };
    RpcResponse::failure(id, name, error)
}

fn host_owned_ui_event(event: &ExtensionUiEvent) -> bool {
    matches!(
        event,
        ExtensionUiEvent::InteractionRequested { interaction }
            if interaction.context.instance.extension_id == "host"
    )
}

impl SessionRuntime {
    /// Fan-in forwarder: subscribe to the runtime's application and extension
    /// UI event streams, tag every projected event with the OWNING runtime's
    /// top-level `sessionId`, and publish into the manager's broadcast
    /// channels. Every Web connection sees every session's events (multi-
    /// client fan-out); commands route explicitly by sessionId.
    fn start_forwarder(
        manager: &Arc<SessionRuntimeManager>,
        runtime: &Arc<SessionRuntime>,
        events: broadcast::Sender<Value>,
        ui_events: broadcast::Sender<RpcExtensionUiRequest>,
    ) {
        let manager: Weak<SessionRuntimeManager> = Arc::downgrade(manager);
        let runtime_for_task = runtime.clone();
        let handle = tokio::spawn(async move {
            let mut application_events = runtime_for_task.application.subscribe();
            let mut ui_events_rx = runtime_for_task.extension_ui.subscribe();
            loop {
                tokio::select! {
                    event = application_events.recv() => {
                        let Some(manager) = manager.upgrade() else { break };
                        let payload = match event {
                            Ok(event) => {
                                // Keep the identity fresh: in-place fork/clone
                                // cutovers change the recorder id without
                                // re-registering the runtime.
                                manager.update_identity(&runtime_for_task).await;
                                match project_application_event(event) {
                                    Ok(mut value) => {
                                        if let Some((session_id, _)) = runtime_for_task.current_identity() {
                                            value["sessionId"] = json!(session_id);
                                        }
                                        Ok(value)
                                    }
                                    Err(error) => Err(error),
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(count)) => Ok(
                                serde_json::to_value(RpcResponse::failure(
                                    None, "events",
                                    format!("application event stream lagged by {count} records"),
                                ))
                                .unwrap_or(Value::Null),
                            ),
                            Err(broadcast::error::RecvError::Closed) => break,
                        };
                        let payload = payload.unwrap_or_else(|error| {
                            serde_json::to_value(RpcResponse::failure(
                                None, "events",
                                format!("failed to project application event: {error}"),
                            ))
                            .unwrap_or(Value::Null)
                        });
                        if events.send(payload).is_err() {
                            break;
                        }
                    }
                    event = ui_events_rx.recv() => {
                        let Some(manager) = manager.upgrade() else { break };
                        let payload = match event {
                            Ok(event) => {
                                if host_owned_ui_event(&event) {
                                    continue;
                                }
                                match project_extension_ui_event(event) {
                                    Ok(Some(mut request)) => {
                                        if let Some((session_id, _)) = runtime_for_task.current_identity() {
                                            request.session_id = Some(session_id);
                                        }
                                        Some(request)
                                    }
                                    Ok(None) => None,
                                    Err(error) => Some(RpcExtensionUiRequest::error_notice(
                                        runtime_for_task.current_identity().map(|(id, _)| id),
                                        format!("failed to project extension UI event: {error}"),
                                    ))
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(count)) => {
                                Some(RpcExtensionUiRequest::error_notice(
                                    runtime_for_task.current_identity().map(|(id, _)| id),
                                    format!("extension UI event stream lagged by {count} records"),
                                ))
                            }
                            Err(broadcast::error::RecvError::Closed) => break,
                        };
                        if let Some(payload) = payload {
                            if ui_events.send(payload).is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        });
        *runtime.forwarder.lock().expect("forwarder lock") = Some(handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::rpc::RpcCommand;
    use pi_ai::providers::{
        FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
    };
    use pi_ai::Model;
    use pi_agent::ThinkingLevel;
    use pi_coding::{ApplicationEvent, Session, SessionOptions};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    fn faux_model(label: &str) -> (Model, FauxProviderRegistration) {
        faux_model_sized(label, 8)
    }

    fn faux_model_sized(label: &str, chunk_size: usize) -> (Model, FauxProviderRegistration) {
        let suffix = unique(label);
        let mut model = Model::default();
        model.id = format!("{label}-model");
        model.name = format!("{label} Model");
        model.api = format!("{suffix}-api");
        model.provider = format!("{suffix}-provider");
        model.base_url = "http://localhost:0".into();
        let registration = register_faux_provider(FauxProviderOptions {
            api: model.api.clone(),
            provider: model.provider.clone(),
            models: vec![model.clone()],
            chunk_size,
        });
        (model, registration)
    }

    fn unique(label: &str) -> String {
        format!("{label}-{}", uuid::Uuid::now_v7().simple())
    }

    fn session_options(model: Model, cwd: &Path) -> SessionOptions {
        SessionOptions {
            model,
            cwd: cwd.to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "faux".into(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
        }
    }

    /// A session without a recorder (like a `--no-session` primary).
    async fn bare_application(label: &str) -> (Application, TempDir) {
        let (model, _registration) = faux_model(label);
        bare_application_with(model).await
    }

    async fn bare_application_with(model: Model) -> (Application, TempDir) {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = Session::new(session_options(model, cwd.path())).expect("session");
        (Application::new(session).await, cwd)
    }

    /// A recorded session whose session file EXISTS on disk (header persisted
    /// immediately, mirroring a production session that has started a
    /// recording), so resume-catalog scans and switch dedup see it.
    async fn recorded_application(label: &str) -> (Application, String, TempDir) {
        let (model, _registration) = faux_model(label);
        recorded_application_with(model).await
    }

    async fn recorded_application_with(model: Model) -> (Application, String, TempDir) {
        let dir = tempfile::tempdir().expect("session dir");
        let session = Session::new(session_options(model.clone(), dir.path())).expect("session");
        session.set_session_dir(dir.path().to_path_buf());
        let recorder = pi_coding::start_session_in(
            dir.path(),
            Some(&model),
            Some("off"),
            Some(dir.path()),
            None,
            None,
        )
        .expect("start session");
        recorder.persist_now().expect("persist header");
        session.record(recorder).expect("record");
        let application = Application::new(session).await;
        let (id, _) = application.session().recorder_info().expect("recorder id");
        (application, id, dir)
    }

    /// Deterministic faux spawner: every spawn builds a fresh recorded
    /// runtime from one shared controllable model (tests drive it through
    /// [`TestSpawner::registration`]) and counts calls so tests can assert
    /// dedup/cap behavior. An `Open` child claims the requested resume path
    /// as its session file, exactly like a production spawner that resumes
    /// the opened recording in place, so canonical-path dedup works.
    #[derive(Clone)]
    struct TestSpawner {
        spawns: Arc<AtomicUsize>,
        opened: Arc<std::sync::Mutex<Vec<PathBuf>>>,
        kinds: Arc<std::sync::Mutex<Vec<SessionSpawnKind>>>,
        model: Model,
        registration: FauxProviderRegistration,
    }

    impl Default for TestSpawner {
        fn default() -> Self {
            // chunk_size 1: prompts stream byte-by-byte, so tests can queue
            // arbitrarily long replies and keep the child busy on demand.
            let (model, registration) = faux_model_sized("spawner-child", 1);
            Self {
                spawns: Arc::new(AtomicUsize::new(0)),
                opened: Arc::new(std::sync::Mutex::new(Vec::new())),
                kinds: Arc::new(std::sync::Mutex::new(Vec::new())),
                model,
                registration,
            }
        }
    }

    impl SessionSpawner for TestSpawner {
        fn spawn(
            &self,
            request: SessionSpawnRequest,
        ) -> Pin<Box<dyn Future<Output = Result<SessionSpawnResult>> + Send>> {
            let this = self.clone();
            Box::pin(async move {
                this.spawns.fetch_add(1, Ordering::SeqCst);
                this.kinds.lock().expect("kinds").push(request.kind.clone());
                let resume_path = match &request.kind {
                    SessionSpawnKind::Open { resume_path } => {
                        this.opened.lock().expect("opened").push(resume_path.clone());
                        Some(resume_path.clone())
                    }
                    _ => None,
                };
                let (application, session_id, dir) =
                    recorded_application_with(this.model.clone()).await;
                // The spawned session's temp dir must outlive the spawn so
                // the persisted session file stays visible to resume-catalog
                // scans (session_list) and switch dedup.
                let _kept = dir.keep();
                let session_file = resume_path.or_else(|| {
                    application.session().recorder_info().map(|(_, path)| path)
                });
                Ok(SessionSpawnResult {
                    session_id,
                    session_file,
                    application,
                    extension_ui: ExtensionUiAdapter::default(),
                })
            })
        }
    }

    async fn manager_with(
        factory: Option<Arc<dyn SessionSpawner>>,
    ) -> (Arc<SessionRuntimeManager>, FauxProviderRegistration) {
        manager_with_sized(factory, 8).await
    }

    async fn manager_with_sized(
        factory: Option<Arc<dyn SessionSpawner>>,
        chunk_size: usize,
    ) -> (Arc<SessionRuntimeManager>, FauxProviderRegistration) {
        let (model, registration) = faux_model_sized("mgr-primary", chunk_size);
        let (application, _cwd) = bare_application_with(model).await;
        (
            SessionRuntimeManager::new(application, ExtensionUiAdapter::default(), factory).await,
            registration,
        )
    }

    #[tokio::test]
    async fn absent_session_id_routes_to_primary() {
        let (manager, _primary_registration) = manager_with(None).await;
        let response = manager
            .dispatch(RpcCommand::GetState { id: Some("r".into()) }, None)
            .await;
        assert!(response.success, "{}", response.error.unwrap_or_default());
        let data = response.data.expect("state data");
        // The no-recorder primary reports a null sessionId (never a real
        // session's id); absent sessionId in the request routes to it.
        assert!(data.get("sessionId").and_then(Value::as_str).is_none());
        assert!(data.get("cwd").is_some());
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn unknown_session_id_fails_closed() {
        let (manager, _primary_registration) = manager_with(None).await;
        let response = manager
            .dispatch(RpcCommand::GetState { id: Some("r".into()) }, Some("nope".into()))
            .await;
        assert!(!response.success);
        let error = response.error.expect("error");
        assert!(error.contains("unknown session nope"), "{error}");
        let ok = manager.dispatch(RpcCommand::GetState { id: None }, None).await;
        assert!(ok.success);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn new_session_returns_snapshot_and_leaves_source_intact() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let before = manager.dispatch(RpcCommand::GetState { id: None }, None).await;
        let before_messages: Vec<Message> = manager
            .primary
            .application
            .messages()
            .into_iter()
            .map(public_message)
            .collect();

        let response = manager
            .dispatch(RpcCommand::NewSession { id: Some("n1".into()), parent_session: None }, None)
            .await;
        assert!(response.success, "{}", response.error.unwrap_or_default());
        let data = response.data.expect("snapshot");
        let child_id = data["sessionId"].as_str().expect("child sessionId").to_owned();
        assert!(data["state"]["sessionId"].as_str() == Some(child_id.as_str()));
        assert!(data["messages"].is_array());
        assert_eq!(spawner.spawns.load(Ordering::SeqCst), 1);

        let after = manager.dispatch(RpcCommand::GetState { id: None }, None).await;
        assert!(after.success);
        assert_eq!(
            after.data.as_ref().unwrap().get("sessionId"),
            before.data.as_ref().unwrap().get("sessionId")
        );
        let after_messages: Vec<Message> = manager.primary.application.messages();
        assert_eq!(after_messages.len(), before_messages.len());

        let child_state = manager
            .dispatch(RpcCommand::GetState { id: None }, Some(child_id.clone()))
            .await;
        assert!(child_state.success, "{}", child_state.error.unwrap_or_default());
        assert_eq!(
            child_state.data.as_ref().unwrap()["sessionId"].as_str(),
            Some(child_id.as_str())
        );
        let unknown = manager
            .dispatch(RpcCommand::GetState { id: None }, Some(format!("{child_id}x")))
            .await;
        assert!(!unknown.success);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn switch_session_dedups_by_canonical_path_and_returns_same_runtime() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let dir = tempfile::tempdir().expect("session dir");
        let (model, _registration) = faux_model("dedup");
        let recorder = pi_coding::start_session_in(
            dir.path(),
            Some(&model),
            Some("off"),
            Some(dir.path()),
            None,
            None,
        )
        .expect("start session");
        recorder.persist_now().expect("persist");
        let path = recorder.path();

        let first = manager
            .dispatch(
                RpcCommand::SwitchSession {
                    id: None,
                    session_path: path.to_string_lossy().into_owned(),
                },
                None,
            )
            .await;
        assert!(first.success, "{}", first.error.unwrap_or_default());
        let first_id = first.data.as_ref().unwrap()["sessionId"].as_str().expect("first id").to_owned();

        let second = manager
            .dispatch(
                RpcCommand::SwitchSession {
                    id: None,
                    session_path: path.to_string_lossy().into_owned(),
                },
                None,
            )
            .await;
        assert!(second.success, "{}", second.error.unwrap_or_default());
        let second_id = second.data.as_ref().unwrap()["sessionId"].as_str().expect("second id").to_owned();
        assert_eq!(first_id, second_id, "dedup must return the same runtime");
        assert_eq!(spawner.spawns.load(Ordering::SeqCst), 1, "no second open");
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn slow_prompt_on_a_does_not_block_b_command() {
        let spawner = Arc::new(TestSpawner::default());
        // The primary's model streams one byte per chunk: a 200k-char reply
        // keeps A busy for well past B's timeout window.
        let (manager, primary_registration) = manager_with_sized(Some(spawner.clone()), 1).await;
        let opened = manager
            .dispatch(RpcCommand::NewSession { id: None, parent_session: None }, None)
            .await;
        let b_id = opened.data.unwrap()["sessionId"].as_str().unwrap().to_owned();

        primary_registration.set_responses(vec![FauxResponse::text("x".repeat(200_000))]);

        let prompt = RpcCommand::Prompt {
            id: Some("p".into()),
            message: "slow".into(),
            images: Vec::new(),
            streaming_behavior: None,
        };
        let manager_task = manager.clone();
        let a_task = tokio::spawn(async move { manager_task.dispatch(prompt, None).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let b_response = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            manager.dispatch(RpcCommand::GetState { id: None }, Some(b_id.clone())),
        )
        .await
        .expect("B get_state must complete while A is prompting");
        assert!(b_response.success, "{}", b_response.error.unwrap_or_default());
        assert_eq!(b_response.data.unwrap()["sessionId"].as_str(), Some(b_id.as_str()));

        let a_result = a_task.await.expect("A prompt task joins");
        assert!(a_result.success, "A prompt completed: {}", a_result.error.unwrap_or_default());
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn commands_and_abort_route_by_session_id() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        // The RPC prompt response is acceptance: the turn runs asynchronously
        // and completes with a message_end event on the owning session.
        let mut events = manager.events();
        let opened = manager
            .dispatch(RpcCommand::NewSession { id: None, parent_session: None }, None)
            .await;
        let b_id = opened.data.unwrap()["sessionId"].as_str().unwrap().to_owned();

        let b_messages = manager
            .dispatch(RpcCommand::GetMessages { id: None }, Some(b_id.clone()))
            .await;
        assert!(b_messages.success);
        assert_eq!(b_messages.data.unwrap()["messages"].as_array().unwrap().len(), 0);

        spawner.registration.set_responses(vec![FauxResponse::text("b-reply")]);
        manager
            .dispatch(
                RpcCommand::Prompt {
                    id: None,
                    message: "hello b".into(),
                    images: Vec::new(),
                    streaming_behavior: None,
                },
                Some(b_id.clone()),
            )
            .await
            .success_or("prompt on B");
        wait_message_end(&mut events, &b_id).await;

        let b_history = manager
            .dispatch(RpcCommand::GetMessages { id: None }, Some(b_id.clone()))
            .await;
        let b_texts = history_texts(&b_history);
        assert!(b_texts.iter().any(|text| text.contains("hello b")), "{b_texts:?}");
        assert!(b_texts.iter().any(|text| text.contains("b-reply")), "{b_texts:?}");

        let a_history = manager.dispatch(RpcCommand::GetMessages { id: None }, None).await;
        let a_texts = history_texts(&a_history);
        assert!(!a_texts.iter().any(|text| text.contains("hello b")), "{a_texts:?}");

        let abort_ok = manager
            .dispatch(RpcCommand::Abort { id: None }, Some(b_id.clone()))
            .await;
        assert!(abort_ok.success);
        let abort_unknown = manager
            .dispatch(RpcCommand::Abort { id: None }, Some("missing".into()))
            .await;
        assert!(!abort_unknown.success);
        assert!(abort_unknown.error.unwrap().contains("unknown session missing"));
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn events_carry_owning_session_id_on_fan_in() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let mut events = manager.events();
        let opened = manager
            .dispatch(RpcCommand::NewSession { id: None, parent_session: None }, None)
            .await;
        let b_id = opened.data.unwrap()["sessionId"].as_str().unwrap().to_owned();

        spawner.registration.set_responses(vec![FauxResponse::text("event-reply")]);
        manager
            .dispatch(
                RpcCommand::Prompt {
                    id: None,
                    message: "ping".into(),
                    images: Vec::new(),
                    streaming_behavior: None,
                },
                Some(b_id.clone()),
            )
            .await
            .success_or("prompt on B");

        let mut saw_b_tagged = false;
        loop {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                .await
                .expect("fan-in frame")
                .expect("fan-in open");
            if frame.get("sessionId").and_then(Value::as_str) == Some(b_id.as_str()) {
                saw_b_tagged = true;
                if frame.get("type").and_then(Value::as_str) == Some("message_end") {
                    break;
                }
            }
        }
        assert!(saw_b_tagged);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn primary_without_recorder_emits_no_session_id() {
        let (manager, primary_registration) = manager_with(None).await;
        let mut events = manager.events();
        primary_registration.set_responses(vec![FauxResponse::text("a-reply")]);
        manager
            .dispatch(
                RpcCommand::Prompt {
                    id: None,
                    message: "ping".into(),
                    images: Vec::new(),
                    streaming_behavior: None,
                },
                None,
            )
            .await
            .success_or("prompt on primary");
        loop {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                .await
                .expect("fan-in frame")
                .expect("fan-in open");
            if frame.get("type").and_then(Value::as_str) == Some("message_end") {
                assert!(
                    frame.get("sessionId").is_none(),
                    "no-recorder primary emits no sessionId: {frame}"
                );
                break;
            }
        }
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn loaded_session_cap_rejects_without_evicting() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let mut opened_ids = Vec::new();
        for _ in 0..(MAX_LOADED_SESSIONS - 1) {
            let response = manager
                .dispatch(RpcCommand::NewSession { id: None, parent_session: None }, None)
                .await;
            assert!(response.success, "open within cap: {}", response.error.unwrap_or_default());
            opened_ids.push(response.data.unwrap()["sessionId"].as_str().unwrap().to_owned());
        }
        let rejected = manager
            .dispatch(RpcCommand::NewSession { id: None, parent_session: None }, None)
            .await;
        assert!(!rejected.success);
        let error = rejected.error.unwrap();
        assert!(error.contains("too many concurrent sessions"), "{error}");
        assert!(error.contains("8"), "{error}");

        let existing = manager
            .dispatch(RpcCommand::GetState { id: None }, Some(opened_ids[0].clone()))
            .await;
        assert!(existing.success, "{}", existing.error.unwrap_or_default());
        assert_eq!(existing.data.unwrap()["sessionId"].as_str(), Some(opened_ids[0].as_str()));
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn close_session_rejects_primary_unknown_and_busy_but_closes_idle() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let opened = manager
            .dispatch(RpcCommand::NewSession { id: None, parent_session: None }, None)
            .await;
        let b_id = opened.data.unwrap()["sessionId"].as_str().unwrap().to_owned();

        let no_sid = manager.dispatch(RpcCommand::CloseSession { id: None }, None).await;
        assert!(!no_sid.success);
        assert!(no_sid.error.unwrap().contains("requires a sessionId"));

        let unknown = manager
            .dispatch(RpcCommand::CloseSession { id: None }, Some("ghost".into()))
            .await;
        assert!(!unknown.success);
        assert!(unknown.error.unwrap().contains("unknown session ghost"));

        let (primary_app, primary_id, _dir) = recorded_application("primary-recorded").await;
        let manager2 = SessionRuntimeManager::new(
            primary_app,
            ExtensionUiAdapter::default(),
            Some(spawner.clone()),
        )
        .await;
        let primary_close = manager2
            .dispatch(RpcCommand::CloseSession { id: None }, Some(primary_id.clone()))
            .await;
        assert!(!primary_close.success);
        assert!(primary_close.error.unwrap().contains("primary"));
        manager2.shutdown().await;

        // Queue a 200k-char reply that B streams byte-by-byte (the spawner
        // child model uses chunk_size 1), so the turn is verifiably in flight
        // when close_session is attempted.
        spawner.registration.set_responses(vec![FauxResponse::text("x".repeat(200_000))]);
        let prompt = RpcCommand::Prompt {
            id: None,
            message: "busy".into(),
            images: Vec::new(),
            streaming_behavior: None,
        };
        let manager_task = manager.clone();
        let busy_b_id = b_id.clone();
        let b_task = tokio::spawn(async move {
            manager_task.dispatch(prompt, Some(busy_b_id)).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let busy_close = manager
            .dispatch(RpcCommand::CloseSession { id: None }, Some(b_id.clone()))
            .await;
        assert!(!busy_close.success, "busy close must be rejected");
        assert!(busy_close.error.unwrap().contains("busy"), "error should name the busy reason");
        let prompt_result = b_task.await.expect("prompt task");
        assert!(prompt_result.success, "turn survived the rejected close");

        // No cancellation: the turn is STILL streaming after the rejection.
        let streaming = manager
            .dispatch(RpcCommand::GetState { id: None }, Some(b_id.clone()))
            .await;
        assert!(streaming.success, "{}", streaming.error.unwrap_or_default());
        assert_eq!(
            streaming.data.as_ref().unwrap()["isStreaming"],
            json!(true),
            "rejected close must not cancel the turn"
        );

        // Explicit abort, then poll until the runtime reaches idle, then close.
        let aborted = manager
            .dispatch(RpcCommand::Abort { id: None }, Some(b_id.clone()))
            .await;
        assert!(aborted.success, "{}", aborted.error.unwrap_or_default());
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            let state = manager
                .dispatch(RpcCommand::GetState { id: None }, Some(b_id.clone()))
                .await;
            if state.data.as_ref().and_then(|data| data.get("isStreaming"))
                == Some(&json!(false))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let closed = manager
            .dispatch(RpcCommand::CloseSession { id: None }, Some(b_id.clone()))
            .await;
        assert!(closed.success, "{}", closed.error.unwrap_or_default());
        assert_eq!(closed.data.unwrap()["closed"], json!(true));
        let gone = manager
            .dispatch(RpcCommand::GetState { id: None }, Some(b_id.clone()))
            .await;
        assert!(!gone.success);
        assert!(gone.error.unwrap().contains("unknown session"));
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_cleans_manager_owned_runtimes_only() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let opened = manager
            .dispatch(RpcCommand::NewSession { id: None, parent_session: None }, None)
            .await;
        let b_id = opened.data.unwrap()["sessionId"].as_str().unwrap().to_owned();
        assert!(manager
            .dispatch(RpcCommand::GetState { id: None }, Some(b_id.clone()))
            .await
            .success);

        manager.shutdown().await;

        let gone = manager
            .dispatch(RpcCommand::GetState { id: None }, Some(b_id.clone()))
            .await;
        assert!(!gone.success, "child must be cleaned by shutdown");
        assert!(gone.error.unwrap().contains("unknown session"));
        let primary = manager.dispatch(RpcCommand::GetState { id: None }, None).await;
        assert!(primary.success);
        manager.shutdown().await;
        manager.primary.application.cleanup().await;
    }

    #[tokio::test]
    async fn session_list_includes_every_loaded_runtime() {
        let spawner = Arc::new(TestSpawner::default());
        let (primary_application, primary_id, _primary_dir) = recorded_application("primary-list").await;
        let manager = SessionRuntimeManager::new(
            primary_application,
            ExtensionUiAdapter::default(),
            Some(spawner.clone()),
        )
        .await;
        let opened = manager
            .dispatch(RpcCommand::NewSession { id: None, parent_session: None }, None)
            .await;
        let b_id = opened.data.unwrap()["sessionId"].as_str().unwrap().to_owned();

        let listed = manager
            .overlay_loaded_sessions(RpcResponse::success(
                None,
                "session_list",
                Some(json!({"sessions": []})),
            ))
            .await;
        let listed_data = listed.data.unwrap();
        let sessions = listed_data["sessions"].as_array().expect("sessions");
        let child = sessions
            .iter()
            .find(|row| row.get("sessionId").and_then(Value::as_str) == Some(b_id.as_str()))
            .expect("fresh child row in session_list");
        assert_eq!(child.get("loaded").and_then(Value::as_bool), Some(true));
        assert!(child.get("path").and_then(Value::as_str).is_some_and(|path| !path.is_empty()));

        let primary = sessions
            .iter()
            .find(|row| row.get("sessionId").and_then(Value::as_str) == Some(primary_id.as_str()))
            .expect("fresh primary row in session_list");
        assert_eq!(primary.get("loaded").and_then(Value::as_bool), Some(true));
        assert_eq!(primary.get("source").and_then(Value::as_str), Some("primary"));
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn session_forked_projection_uses_forked_session_id() {
        let target = pi_coding::SessionForkedEvent {
            target_id: "entry-1".into(),
            session_id: "fork-target-id".into(),
            session_file: "/tmp/fork.jsonl".into(),
            editor_text: "prompt text".into(),
        };
        let projected = project_application_event(ApplicationEvent::SessionForked(target))
            .expect("project");
        assert_eq!(projected["type"], "session_forked");
        assert_eq!(projected["forkedSessionId"], "fork-target-id");
        assert!(projected.get("sessionId").is_none(), "{projected}");
    }

    #[tokio::test]
    async fn parse_input_extracts_top_level_session_id() {
        use crate::modes::rpc::parse_input;
        let Ok(crate::modes::rpc::RpcInput::Command { command, session_id }) =
            parse_input(br#"{"type":"get_state","id":"x","sessionId":"sess-1"}"#)
        else {
            panic!("parse must succeed")
        };
        assert_eq!(session_id.as_deref(), Some("sess-1"));
        assert_eq!(command.id().as_deref(), Some("x"));
        let Ok(crate::modes::rpc::RpcInput::Command { session_id, .. }) = parse_input(
            br#"{"type":"workflow_list","id":"w","sessionId":"sess-2"}"#,
        ) else {
            panic!("workflow parse must succeed")
        };
        assert_eq!(session_id.as_deref(), Some("sess-2"));
    }

    /// The RPC prompt response is acceptance (the turn runs asynchronously);
    /// the turn's completion is the `message_end` application event tagged
    /// with the owning session. Wait for it before asserting recorded state.
    async fn wait_message_end(events: &mut broadcast::Receiver<Value>, session_id: &str) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let frame = match tokio::time::timeout_at(deadline, events.recv()).await {
                Ok(Ok(frame)) => frame,
                // A burst of stream deltas can outrun this receiver; skip the
                // lagged records and keep waiting for the completion marker.
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) => {
                    panic!("fan-in closed before message_end for {session_id}")
                }
                Err(_) => panic!("timed out waiting for message_end for {session_id}"),
            };
            if frame.get("type").and_then(Value::as_str) == Some("message_end")
                && frame.get("sessionId").and_then(Value::as_str) == Some(session_id)
            {
                return;
            }
        }
    }

    fn history_texts(response: &RpcResponse) -> Vec<String> {
        let mut texts = Vec::new();
        if let Some(messages) = response
            .data
            .as_ref()
            .and_then(|data| data.get("messages"))
            .and_then(Value::as_array)
        {
            for message in messages {
                if let Some(content) = message.get("content").and_then(Value::as_array) {
                    for block in content {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            texts.push(text.to_owned());
                        }
                    }
                }
            }
        }
        texts
    }

    trait SuccessOr {
        fn success_or(self, context: &str);
    }
    impl SuccessOr for RpcResponse {
        fn success_or(self, context: &str) {
            assert!(self.success, "{context}: {}", self.error.unwrap_or_default());
        }
    }
}