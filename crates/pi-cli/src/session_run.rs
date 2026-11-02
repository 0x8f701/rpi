//! Session setup, resume, and print-mode execution.
//!
//! This wires the parsed CLI flags into the `pi_coding::Session` facade,
//! mirroring the Go `main()`: resume-path resolution, model restoration from
//! a resumed branch, API-key gating, reasoning-level priority, session
//! recording, and print-mode streaming.

use std::collections::HashSet;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{
    Arc, OnceLock, Weak,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use crate::args::Cli;
use crate::commands::{resolve_model_spec, resolve_resume_for_startup};
use crate::extension_ui::ExtensionUiAdapter;
use crate::models_config::ModelRequestAuth;
use crate::output::{parse_thinking_level, thinking_level_str, warn_line};
use crate::session_run_blueprint::RunSessionBlueprint;
use pi_coding::{
    Application, ApplicationRuntimeCandidate, ApplicationRuntimeFactory, AuthManager,
    BranchContext, DEFAULT_COMPACTION_SETTINGS, ExtensionMode, MaterializedFork, ResourceManager,
    ResourceManagerOptions, Session, SessionOptions, TodoChildService,
};


static OFFLINE: AtomicBool = AtomicBool::new(false);

pub fn set_offline(offline: bool) {
    OFFLINE.store(offline, Ordering::Release);
}

/// Canonical process env var for the offline contract. Legacy `PI_OFFLINE`
/// is no longer read at runtime; the canonical name is the only activation
/// path, so a stale legacy variable can never force network-touching
/// operations offline.
const RPI_OFFLINE_ENV: &str = "RPI_OFFLINE";

/// Truthy semantics for `RPI_OFFLINE`, matching the CLI and
/// `crate::share`/`crate::tools::web_search`: any of `1`/`true`/`yes`
/// (case-insensitive, trimmed) is "on". Kept as a tiny pure seam so the
/// contract is unit-testable without mutating the process environment
/// (`std::env::set_var` is `unsafe` in edition 2024 and forbidden here).
#[must_use]
fn offline_from(value: Option<&str>) -> bool {
    matches!(
        value
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

#[must_use]
pub fn offline() -> bool {
    OFFLINE.load(Ordering::Acquire) || offline_from(std::env::var(RPI_OFFLINE_ENV).ok().as_deref())
}

/// The fully-resolved live application and its mode-specific extension UI adapter.
pub struct RunSession {
    pub application: Application,
    pub extension_ui: Option<ExtensionUiAdapter>,
    pub model: pi_ai::Model,
    pub scoped_models: Option<Vec<pi_ai::Model>>,
    /// Non-fatal startup warnings (settings deprecations, resource diagnostics)
    /// collected for interactive TUI display. Empty for non-interactive modes
    /// where warnings are emitted directly to stderr instead.
    pub startup_warnings: Vec<String>,
    /// User-facing notice when a resumed durable sidecar mismatched current
    /// policy and orchestration was left unbound. `None` on a normal start.
    pub resume_notice: Option<String>,
    /// Cloneable factory for building additional session runtimes for the
    /// Web-only listener (`session_run::build_session` policy from a sanitized
    /// clone of the startup CLI). Consumed by `modes::listen`.
    pub spawner: RunSessionSpawner,
    /// Host-owned Todo service for this process/run. Applications hold a weak
    /// client; parent cleanup never shuts the service down.
    pub todo_child_service: Arc<TodoChildService>,
}

/// Builds manager-owned session runtimes for the Web/listen control plane.
///
/// Children are built with the SAME production policy as startup
/// (`RunSessionBlueprint` + `ApplicationRuntimeFactory`) from a sanitized
/// clone of the startup [`Cli`], so auth resolver, orchestration, goal,
/// resources, and extension approval isolation are preserved — without
/// re-running process-global catalog loads or clearing runtime API keys.
///
/// Every child gets its own `Application` and a NON-INTERACTIVE extension UI
/// host (`extension_ui: None` in the blueprint): secondary Web-only sessions
/// must never share the primary TUI's approval slot, and approval-required
/// tools fail closed instead of hanging or routing to the wrong session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SessionStorage {
    DefaultTree {
        native_root: PathBuf,
    },
    ExplicitRoot(PathBuf),
    #[cfg(test)]
    Hermetic {
        sessions_home: PathBuf,
    },
}

impl SessionStorage {
    fn default_tree() -> Self {
        Self::DefaultTree {
            native_root: pi_coding::native_sessions_root(),
        }
    }

    pub(crate) fn session_dir_for(&self, cwd: &Path) -> Result<PathBuf> {
        match self {
            Self::DefaultTree { native_root } => {
                let encoded = pi_coding::default_session_dir(cwd)
                    .file_name()
                    .ok_or_else(|| anyhow!("default session directory has no project component"))?
                    .to_owned();
                Ok(native_root.join(encoded))
            }
            Self::ExplicitRoot(root) => Ok(root.clone()),
            #[cfg(test)]
            Self::Hermetic { .. } => bail!("hermetic test storage has no session_dir_for"),
        }
    }

    pub(crate) fn catalog(&self) -> Result<pi_coding::SessionCatalog> {
        match self {
            #[cfg(test)]
            Self::Hermetic { sessions_home } => {
                return Ok(pi_coding::SessionCatalog::new(sessions_home));
            }
            _ => {}
        }
        let catalog = pi_coding::SessionCatalog::from_env()?;
        match self {
            Self::DefaultTree { native_root } => {
                let agent_dir = native_root
                    .parent()
                    .ok_or_else(|| anyhow!("native sessions root has no agent directory"))?;
                Ok(catalog.with_native_agent_dir(agent_dir))
            }
            Self::ExplicitRoot(root) => Ok(catalog.with_native_session_root(root)),
            #[cfg(test)]
            Self::Hermetic { .. } => unreachable!(),
        }
    }
}

#[derive(Clone)]
/// Builds manager-owned session runtimes for the Web/listen control plane.
pub struct RunSessionSpawner {
    cli: Cli,
    storage: SessionStorage,
    todo_child_service: Weak<TodoChildService>,
    runtime_factory: Arc<std::sync::OnceLock<Arc<dyn ApplicationRuntimeFactory>>>,
    detached_cleanups: Arc<parking_lot::Mutex<Option<SpawnRollbackTracker>>>,
}

impl RunSessionSpawner {
    pub(crate) fn from_startup(cli: &Cli, storage: SessionStorage) -> Self {
        Self::from_startup_with_service(cli, storage, TodoChildService::new())
    }

    pub(crate) fn from_startup_with_service(
        cli: &Cli,
        storage: SessionStorage,
        todo_child_service: Arc<TodoChildService>,
    ) -> Self {
        Self {
            cli: sanitize_cli_for_children(cli),
            storage,
            todo_child_service: Arc::downgrade(&todo_child_service),
            runtime_factory: Arc::new(std::sync::OnceLock::new()),
            detached_cleanups: Arc::new(parking_lot::Mutex::new(None)),
        }
    }

    fn detached_rollback_tracker(&self) -> Option<SpawnRollbackTracker> {
        self.detached_cleanups.lock().clone()
    }

    #[cfg(test)]
    pub(crate) fn detached_tracker_for_test(&self) -> Option<SpawnRollbackTracker> {
        self.detached_cleanups.lock().clone()
    }

    pub(crate) fn with_runtime_factory(self, factory: Arc<dyn ApplicationRuntimeFactory>) -> Self {
        let _ = self.runtime_factory.set(factory);
        self
    }

    pub(crate) fn todo_child_service(&self) -> Option<Arc<TodoChildService>> {
        self.todo_child_service.upgrade()
    }

    pub(crate) fn storage(&self) -> SessionStorage {
        self.storage.clone()
    }

    fn attach_host_todo(
        &self,
        application: &Application,
        blueprint: &RunSessionBlueprint,
    ) -> Result<()> {
        let todo_child_service = self
            .todo_child_service
            .upgrade()
            .ok_or_else(|| anyhow!("Todo child service is unavailable"))?;
        let factory = self
            .runtime_factory
            .get_or_init(|| Arc::new(blueprint.clone()) as Arc<dyn ApplicationRuntimeFactory>)
            .clone();
        application.attach_runtime_factory(factory.clone())?;
        todo_child_service.configure_factory(factory)?;
        application.attach_todo_child_client(todo_child_service.client())?;
        Ok(())
    }

    /// Resolve session storage for one already-authoritative child cwd.
    fn child_placement(&self, cwd: &Path) -> Result<(PathBuf, PathBuf)> {
        Ok((cwd.to_path_buf(), self.storage.session_dir_for(cwd)?))
    }

    /// Open (resume) the persisted session at `path` as an independent
    /// runtime. The target cwd, model, and history come from the recorded
    /// session file (backend-authoritative), never from a frontend cache.
    pub(crate) async fn open_resumed(
        &self,
        source: &Application,
        prepared: pi_coding::PreparedSessionResume,
    ) -> Result<crate::modes::session_runtime_manager::SessionSpawnResult> {
        let mut rollback = SpawnFinishRollback {
            session: None,
            journal_recorder: None,
            extension_runtime: None,
            application: None,
            armed: true,
            cleanup_settled: false,
            recording_settled: false,
            extension_settled: false,
            runtime: tokio::runtime::Handle::current(),
            tracker: self.detached_rollback_tracker(),
            #[cfg(test)]
            park: None,
            #[cfg(test)]
            detached: false,
        };
        let result = async {
            let recorded_cwd = prepared.target_cwd();
            heal_missing_recorded_cwd(recorded_cwd);
            let mut target_cwd = source.session().cwd().to_path_buf();
            if !recorded_cwd.as_os_str().is_empty()
                && let Ok(inspected) = pi_coding::inspect_operator_directory(recorded_cwd)
            {
                target_cwd = inspected;
            }
            let blueprint = self.child_blueprint()?;
            let options = child_session_options(source, &target_cwd);
            let context = prepared.build_context();
            let resume_has_thinking_entry = prepared.tree().has_thinking_entry();
            let recorder = prepared.into_recorder()?;
            let candidate = blueprint
                .build(&target_cwd, options.clone())
                .await
                .context("building resumed session runtime")?;
            rollback.adopt_extension_runtime(candidate.extension_runtime.clone());
            let retry_model = context
                .provider
                .as_deref()
                .zip(context.model_id.as_deref())
                .and_then(|(provider, model_id)| pi_ai::get_model(provider, model_id));
            let retry_thinking = if resume_has_thinking_entry {
                parse_thinking_level(&context.thinking_level)
            } else {
                candidate.session.thinking_level()
            };
            let retry_model = retry_model.or_else(|| candidate.session.model());
            let candidate = candidate
                .restore_resume(
                    context.clone(),
                    recorder.clone(),
                    None,
                    retry_model.clone(),
                    retry_thinking,
                )
                .await?;
            let retry_session_name = candidate.session.session_name();
            let (application, resume_notice) =
                match application_from_session_candidate(candidate).await {
                    Ok(application) => application,
                    Err(error) if durable_resume_error(&error).is_some() => {
                        let warning = durable_resume_notice(&error);
                        let retry = blueprint
                            .build_without_orchestration(&target_cwd, options)
                            .await
                            .context("rebuilding resumed session runtime without orchestration")?;
                        rollback.adopt_extension_runtime(retry.extension_runtime.clone());
                        let retry = retry
                            .restore_resume(
                                context,
                                recorder,
                                retry_session_name,
                                retry_model,
                                retry_thinking,
                            )
                            .await?;
                        (
                            application_from_session_candidate(retry).await?.0,
                            Some(warning),
                        )
                    }
                    Err(error) => return Err(error),
                };
            rollback.adopt_application(application.clone());
            if let Some(notice) = resume_notice.as_deref() {
                warn_line(&format!("switch_session degraded: {notice}"));
            }
            let initialized = async {
                self.attach_host_todo(&application, &blueprint)?;
                application.prepare_resumed_goal(false)?;
                let (session_id, session_file) = application
                    .session()
                    .recorder_info()
                    .ok_or_else(|| anyhow!("resumed session has no recorder"))?;
                let file_identity = application
                    .session()
                    .session_file_identity()
                    .ok_or_else(|| anyhow!("resumed session has no durable file identity"))?;
                setup_child_workflows(&application, &target_cwd, &session_id).await?;
                Ok::<_, anyhow::Error>((session_id, session_file, file_identity))
            }
            .await;
            let (session_id, session_file, file_identity) = initialized?;
            Ok(crate::modes::session_runtime_manager::SessionSpawnResult {
                session_id,
                file_identity,
                session_file,
                application,
                extension_ui: ExtensionUiAdapter::default(),
                resume_notice,
                journal_ownership:
                    crate::modes::session_runtime_manager::SpawnedJournalOwnership::Existing,
                editor_text: None,
            })
        }
        .await;
        match result {
            Ok(result) => {
                rollback.disarm();
                Ok(result)
            }
            Err(error) => {
                let (cleanup, recording) = rollback.settle().await;
                Err(spawn_rollback_error(error, cleanup, recording))
            }
        }
    }

    async fn open_branched(
        &self,
        source: &Application,
        recorder: pi_coding::SessionRecorder,
        editor_text: Option<String>,
    ) -> Result<crate::modes::session_runtime_manager::SessionSpawnResult> {
        let mut rollback = SpawnFinishRollback {
            session: None,
            journal_recorder: Some(recorder.clone()),
            extension_runtime: None,
            application: None,
            armed: true,
            cleanup_settled: false,
            recording_settled: false,
            extension_settled: false,
            runtime: tokio::runtime::Handle::current(),
            tracker: self.detached_rollback_tracker(),
            #[cfg(test)]
            park: None,
            #[cfg(test)]
            detached: false,
        };
        let result = async {
            // A Web lifecycle success is a durable publication boundary:
            // create and fsync the native journal before building any
            // externally observable runtime state. Header-only root forks
            // therefore survive refresh exactly like retained-history forks.
            recorder.persist_now_durable()?;
            let tree = recorder.tree()?;
            let mut target_cwd = source.session().cwd().to_path_buf();
            if !tree.header.cwd.as_os_str().is_empty()
                && let Ok(inspected) = pi_coding::inspect_operator_directory(&tree.header.cwd)
            {
                target_cwd = inspected;
            }
            let context = tree.build_context(None);
            let blueprint = self.child_blueprint()?;
            let options = child_session_options(source, &target_cwd);
            let candidate = blueprint.build(&target_cwd, options).await?;
            let crate::session_run_blueprint::RunSessionCandidate {
                session,
                extension_runtime,
                extension_permissions,
                orchestration,
                goal_tool,
                todo_child_tool,
            } = candidate;
            rollback.adopt_extension_runtime(extension_runtime.clone());

            if let Some(provider) = context.provider.as_deref()
                && let Some(model_id) = context.model_id.as_deref()
                && let Some(model) = pi_ai::get_model(provider, model_id)
            {
                session.set_model_with_resolved_auth(model).await?;
            }
            session.resume_recording(recorder, context.messages).await?;
            let mut runtime = pi_coding::ApplicationRuntimeCandidate::new(session)
                .with_extensions(extension_runtime, extension_permissions);
            if let Some(orchestration) = orchestration {
                runtime = runtime.with_orchestration(orchestration);
            }
            if let Some(goal_tool) = goal_tool {
                runtime = runtime.with_goal_tool(goal_tool);
            }
            if let Some(todo_child_tool) = todo_child_tool {
                runtime = runtime.with_todo_child_tool(todo_child_tool);
            }
            let application = Application::from_runtime_candidate(runtime).await?;
            rollback.adopt_application(application.clone());
            let initialized: anyhow::Result<(
                String,
                PathBuf,
                pi_coding::SessionFileIdentity,
            )> = async {
                self.attach_host_todo(&application, &blueprint)?;
                application.prepare_resumed_goal(false)?;
                let (session_id, session_file) = application
                    .session()
                    .recorder_info()
                    .ok_or_else(|| anyhow!("branched session has no recorder"))?;
                let file_identity = application
                    .session()
                    .session_file_identity()
                    .ok_or_else(|| anyhow!("branched session has no durable file identity"))?;
                setup_child_workflows(&application, &target_cwd, &session_id).await?;
                Ok((session_id, session_file, file_identity))
            }
            .await;
            let (session_id, session_file, file_identity) = initialized?;
            Ok(crate::modes::session_runtime_manager::SessionSpawnResult {
                session_id,
                session_file,
                file_identity,
                application,
                extension_ui: ExtensionUiAdapter::default(),
                resume_notice: None,
                journal_ownership:
                    crate::modes::session_runtime_manager::SpawnedJournalOwnership::Created,
                editor_text,
            })
        }
        .await;
        match result {
            Ok(result) => {
                rollback.disarm();
                Ok(result)
            }
            Err(error) => {
                let (cleanup, recording) = rollback.settle().await;
                Err(spawn_rollback_error(error, cleanup, recording))
            }
        }
    }

}

pub type SpawnRollbackTracker =
    std::sync::Arc<dyn Fn(tokio::task::JoinHandle<()>) + Send + Sync>;

pub(crate) struct PreparedFreshSession {
    spawner: RunSessionSpawner,
    tracker: Option<SpawnRollbackTracker>,
    blueprint: RunSessionBlueprint,
    cwd: PathBuf,
    workspace: pi_coding::WorkspaceRoots,
    candidate: crate::session_run_blueprint::RunSessionCandidate,
    /// Test seam: parks `finish` after the created journal exists, so
    /// manager-level tests can hold a production Fresh creation between
    /// journal creation and application activation. `None` in production.
    #[cfg(test)]
    park: Option<FreshFinishParkGate>,

}
/// Deterministic park gate for `PreparedFreshSession::finish` tests,
/// mirroring `pi_coding::TodoChildCreationTestGate`: the finish parks inside
/// `enter` until released (or forever, for shutdown-abort tests), after
/// recording the created journal path for the test to observe.
#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct FreshFinishParkGate {
    entered: tokio_util::sync::CancellationToken,
    release: tokio_util::sync::CancellationToken,
    journal_path: Arc<parking_lot::Mutex<Option<PathBuf>>>,
    rollback_settled: tokio_util::sync::CancellationToken,
    rollback_entered: tokio_util::sync::CancellationToken,
    rollback_release: tokio_util::sync::CancellationToken,
    fail_on_release: Arc<parking_lot::Mutex<bool>>,
}

#[cfg(test)]
impl FreshFinishParkGate {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) async fn wait_entered(&self) {
        self.entered.cancelled().await;
    }

    pub(crate) fn journal_path(&self) -> Option<PathBuf> {
        self.journal_path.lock().clone()
    }

    pub(crate) fn mark_rollback_settled(&self) {
        self.rollback_settled.cancel();
    }

    pub(crate) fn rollback_settled(&self) -> bool {
        self.rollback_settled.is_cancelled()
    }

    #[allow(dead_code)]
    pub(crate) fn release(&self) {
        self.release.cancel();
    }

    async fn enter(&self, journal_path: Option<PathBuf>) {
        *self.journal_path.lock() = journal_path;
        self.entered.cancel();
        self.release.cancelled().await;
    }

    pub(crate) fn fail_on_release(&self) {
        *self.fail_on_release.lock() = true;
    }

    fn released_finish_fails(&self) -> bool {
        *self.fail_on_release.lock()
    }

    async fn enter_rollback(&self) {
        self.rollback_entered.cancel();
        self.rollback_release.cancelled().await;
    }

    pub(crate) async fn wait_rollback_entered(&self) {
        self.rollback_entered.cancelled().await;
    }

    #[allow(dead_code)]
    pub(crate) fn release_rollback(&self) {
        self.rollback_release.cancel();
    }
}

impl PreparedFreshSession {
    pub(crate) async fn finish(
        self,
    ) -> Result<crate::modes::session_runtime_manager::SessionSpawnResult> {
        #[cfg(test)]
        let park = self.park.clone();
        let Self {
            spawner,
            tracker,
            blueprint,
            cwd,
            workspace,
            candidate,
            ..
        } = self;
        let crate::session_run_blueprint::RunSessionCandidate {
            session,
            extension_runtime,
            extension_permissions,
            orchestration,
            goal_tool,
            todo_child_tool,
        } = candidate;
        let mut rollback = SpawnFinishRollback {
            session: Some(session.clone()),
            journal_recorder: None,
            extension_runtime: Some(extension_runtime.clone()),
            application: None,
            armed: true,
            cleanup_settled: false,
            recording_settled: false,
            extension_settled: false,
            runtime: tokio::runtime::Handle::current(),
            tracker,
            #[cfg(test)]
            park: park.clone(),
            #[cfg(test)]
            detached: false,
        };
        if let Err(error) = workspace
            .validate_authorities()
            .and_then(|_| pi_coding::inspect_operator_directory(&cwd).map(|_| ()))
            .context("validating fresh workspace authorities before publication")
        {
            let (cleanup, recording) = rollback.settle().await;
            return Err(spawn_rollback_error(error, cleanup, recording));
        }
        if let Err(error) = session.start_new_recording() {
            let (cleanup, recording) = rollback.settle().await;
            return Err(spawn_rollback_error(error, cleanup, recording));
        }
        let mut runtime = pi_coding::ApplicationRuntimeCandidate::new(session)
            .with_extensions(extension_runtime, extension_permissions);
        if let Some(orchestration) = orchestration {
            runtime = runtime.with_orchestration(orchestration);
        }
        if let Some(goal_tool) = goal_tool {
            runtime = runtime.with_goal_tool(goal_tool);
        }
        if let Some(todo_child_tool) = todo_child_tool {
            runtime = runtime.with_todo_child_tool(todo_child_tool);
        }
        let application = match Application::from_runtime_candidate(runtime).await {
            Ok(application) => application,
            Err(error) => {
                let (cleanup, recording) = rollback.settle().await;
                return Err(spawn_rollback_error(error, cleanup, recording));
            }
        };
        rollback.adopt_application(application.clone());
        let initialized = async {
            spawner.attach_host_todo(&application, &blueprint)?;
            application.session().persist_recording_now_durable()?;
            // Test park (never in production): the journal is durably on
            // disk and the application is live — the maximal partial Fresh
            // state a shutdown can abort mid-finish.
            #[cfg(test)]
            if let Some(gate) = park.as_ref() {
                gate.enter(
                    application
                        .session()
                        .recorder_info()
                        .map(|(_, session_file)| session_file),
                )
                .await;
                if gate.released_finish_fails() {
                    return Err(anyhow!("released parked fresh finish failed"));
                }
            }
            let (session_id, session_file) = application
                .session()
                .recorder_info()
                .ok_or_else(|| anyhow!("new session has no recorder"))?;
            let file_identity = application
                .session()
                .session_file_identity()
                .ok_or_else(|| anyhow!("new session has no durable file identity"))?;
            setup_child_workflows(&application, &cwd, &session_id).await?;
            Ok::<_, anyhow::Error>((session_id, session_file, file_identity))
        }
        .await;
        match initialized {
            Ok((session_id, session_file, file_identity)) => {
                rollback.disarm();
                Ok(crate::modes::session_runtime_manager::SessionSpawnResult::created(
                    session_id,
                    session_file,
                    file_identity,
                    application,
                    ExtensionUiAdapter::default(),
                    None,
                    None,
                ))
            }
            Err(error) => {
                let (cleanup, recording) = rollback.settle().await;
                Err(spawn_rollback_error(error, cleanup, recording))
            }
        }
    }
}

/// Cancellation-safe ownership of one spawn finish's partial state: created
/// before the first fallible await; an abandoned finish settles owned state
/// detached and tracked, and a cancelled pass re-arms that fallback per-leg.
struct SpawnFinishRollback {
    session: Option<Session>,
    /// Journal capability; never owned for pre-existing (resumed) journals.
    journal_recorder: Option<pi_coding::SessionRecorder>,
    extension_runtime: Option<pi_coding::ExtensionRuntime>,
    application: Option<Application>,
    armed: bool,
    cleanup_settled: bool,
    recording_settled: bool,
    extension_settled: bool,
    runtime: tokio::runtime::Handle,
    tracker: Option<SpawnRollbackTracker>,
    #[cfg(test)]
    park: Option<FreshFinishParkGate>,
    #[cfg(test)]
    detached: bool,
}

impl SpawnFinishRollback {
    fn adopt_extension_runtime(&mut self, extension_runtime: pi_coding::ExtensionRuntime) {
        self.extension_runtime = Some(extension_runtime);
    }
    fn adopt_application(&mut self, application: Application) {
        self.application = Some(application);
        self.extension_runtime = None;
        self.extension_settled = true;
    }

    fn disarm(&mut self) {
        self.armed = false;
        self.application = None;
        self.journal_recorder = None;
    }

    async fn rollback_pass(&mut self) -> (Result<()>, Result<()>) {
        #[cfg(test)]
        if !self.detached
            && let Some(gate) = self.park.as_ref()
        {
            gate.enter_rollback().await;
        }
        let cleanup = if self.cleanup_settled {
            Ok(())
        } else {
            let result = match self.application.as_ref() {
                Some(application) => application.cleanup().await,
                None => Ok(()),
            };
            if result.is_ok() {
                self.application = None;
                self.cleanup_settled = true;
            }
            result
        };
        let recording = if self.recording_settled {
            Ok(())
        } else if let Some(session) = self.session.as_ref()
            && session.session_file_identity().is_some()
        {
            let result = session.discard_persisted_recording();
            if result.is_ok() {
                self.recording_settled = true;
            }
            result
        } else if let Some(recorder) = self.journal_recorder.as_ref()
            && recorder.file_identity().is_some()
        {
            let result = recorder.discard_persisted();
            if result.is_ok() {
                self.recording_settled = true;
                self.journal_recorder = None;
            }
            result
        } else {
            self.recording_settled = true;
            Ok(())
        };
        if !self.extension_settled
            && let Some(extension_runtime) = self.extension_runtime.as_ref()
        {
            extension_runtime.shutdown().await;
            self.extension_settled = true;
        }
        #[cfg(test)]
        if let Some(gate) = self.park.as_ref() {
            gate.mark_rollback_settled();
        }
        (cleanup, recording)
    }

    async fn settle(mut self) -> (Result<()>, Result<()>) {
        let outcome = self.rollback_pass().await;
        self.armed = false;
        outcome
    }
}

impl Drop for SpawnFinishRollback {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        let mut fallback = SpawnFinishRollback {
            session: self.session.clone(),
            journal_recorder: self.journal_recorder.clone(),
            extension_runtime: self.extension_runtime.clone(),
            application: self.application.take(),
            armed: false,
            cleanup_settled: self.cleanup_settled,
            recording_settled: self.recording_settled,
            extension_settled: self.extension_settled,
            runtime: self.runtime.clone(),
            tracker: None,
            #[cfg(test)]
            park: self.park.clone(),
            #[cfg(test)]
            detached: true,
        };
        let tracker = self.tracker.take();
        let handle = self.runtime.spawn(async move {
            let _ = fallback.rollback_pass().await;
        });
        if let Some(tracker) = tracker {
            tracker(handle);
        }
    }
}

fn spawn_rollback_error(
    error: anyhow::Error,
    cleanup: Result<()>,
    recording: Result<()>,
) -> anyhow::Error {
    match (cleanup, recording) {
        (Ok(()), Ok(())) => error,
        (cleanup, recording) => error.context(format!(
            "unpublished session rollback failed: runtime={}; recorder={}",
            cleanup
                .err()
                .map(|failure| failure.to_string())
                .unwrap_or_else(|| "ok".to_owned()),
            recording
                .err()
                .map(|failure| failure.to_string())
                .unwrap_or_else(|| "ok".to_owned()),
        )),
    }
}

impl PreparedFreshSession {
    /// Test seam: arms the park inside `finish` for a manager-level
    /// shutdown-abort test. Production preparations never carry a gate.
    #[cfg(test)]
    pub(crate) fn park_finish_after_journal_for_test(
        mut self,
        gate: FreshFinishParkGate,
    ) -> Self {
        self.park = Some(gate);
        self
    }
}

impl PreparedFreshSession {
    /// Explicit async discard of a preparation that never reaches `finish`
    /// (loaded-capacity exhaustion, shutdown cancellation). Preparation
    /// settles target-root resources, trust, and the runtime candidate before
    /// any journal exists; the discard tears the prepared extension runtime
    /// down through the same async shutdown the blueprint applies.
    pub(crate) async fn discard(self) {
        let Self { tracker, candidate, .. } = self;
        let mut rollback = SpawnFinishRollback {
            session: None,
            journal_recorder: None,
            extension_runtime: Some(candidate.extension_runtime),
            application: None,
            armed: true,
            cleanup_settled: false,
            recording_settled: false,
            extension_settled: false,
            runtime: tokio::runtime::Handle::current(),
            tracker,
            #[cfg(test)]
            park: None,
            #[cfg(test)]
            detached: false,
        };
        let _ = rollback.settle().await;
    }
}

impl RunSessionSpawner {
    /// Prepare every deterministic target-root dependency without activating
    /// or persisting the fresh session. Capacity is reserved before `finish`.
    pub(crate) async fn prepare_new_session(
        &self,
        source: &Application,
        directory: pi_coding::PreparedOperatorDirectory,
        tracker: Option<SpawnRollbackTracker>,
    ) -> Result<PreparedFreshSession> {
        let cwd = directory.path().to_path_buf();
        let workspace = directory.into_workspace(&self.cli.add_dirs)?;
        let mut blueprint = self.child_blueprint()?;
        let storage = if cwd == source.session().cwd() {
            self.storage.clone()
        } else {
            SessionStorage::default_tree()
        };
        blueprint.set_session_storage(storage);
        let options = child_session_options(source, &cwd);
        let candidate = blueprint
            .build_with_workspace(workspace.clone(), options)
            .await
            .context("building fresh session runtime")?;
        Ok(PreparedFreshSession {
            spawner: self.clone(),
            tracker,
            blueprint,
            cwd,
            workspace,
            candidate,
            #[cfg(test)]
            park: None,
        })
    }

    /// Fork the source session at `entry_id` into an independent runtime.
    ///
    /// Live and prepared forks share one create-time materializer: the
    /// selected user is resolved from a single captured tree, the retained
    /// cut is `selected.parent_id`, and the child JSONL is fsync'd before
    /// publication. A `None` parent is a true root — empty ancestry, never a
    /// synthetic model/thinking prefix from the live runtime.
    pub(crate) async fn fork_session(
        &self,
        source: &Application,
        entry_id: &str,
    ) -> Result<crate::modes::session_runtime_manager::SessionSpawnResult> {
        let (_, session_dir) = self.child_placement(source.session().cwd())?;
        let recorder = source
            .session()
            .current_recorder_for_fork()
            .ok_or_else(|| anyhow!("fork requires a recorded source session"))?;
        let MaterializedFork {
            recorder,
            editor_text,
        } = pi_coding::fork_live_session_at_user(recorder.as_ref(), entry_id, Some(&session_dir))?;
        self.open_branched(source, recorder, Some(editor_text)).await
    }

    pub(crate) async fn fork_prepared_session(
        &self,
        source: &Application,
        prepared: pi_coding::PreparedSessionResume,
        entry_id: &str,
    ) -> Result<crate::modes::session_runtime_manager::SessionSpawnResult> {
        let (_, session_dir) = self.child_placement(source.session().cwd())?;
        let MaterializedFork {
            recorder,
            editor_text,
        } = pi_coding::materialize_fork_at_user(prepared.into(), entry_id, Some(&session_dir))?;
        self.open_branched(source, recorder, Some(editor_text)).await
    }


    /// Clone the source session's active leaf into an independent runtime
    /// (mirrors in-place `/clone`).
    pub(crate) async fn clone_session(
        &self,
        source: &Application,
    ) -> Result<crate::modes::session_runtime_manager::SessionSpawnResult> {
        let leaf_id = source
            .session()
            .session_tree()?
            .active_leaf_id
            .clone()
            .ok_or_else(|| anyhow!("cannot clone session: no current entry selected"))?;
        let (_, session_dir) = self.child_placement(source.session().cwd())?;
        let recorder = source
            .session()
            .create_branched_recorder(&leaf_id, Some(&session_dir))?;
        self.open_branched(source, recorder, None).await
    }

    pub(crate) async fn clone_prepared_session(
        &self,
        source: &Application,
        prepared: pi_coding::PreparedSessionResume,
    ) -> Result<crate::modes::session_runtime_manager::SessionSpawnResult> {
        let leaf_id = prepared
            .tree()
            .active_leaf_id
            .clone()
            .ok_or_else(|| anyhow!("cannot clone session: no current entry selected"))?;
        let (_, session_dir) = self.child_placement(source.session().cwd())?;
        let recorder = pi_coding::create_branched_session_from_prepared(
            prepared,
            &leaf_id,
            Some(&session_dir),
        )?;
        self.open_branched(source, recorder, None).await
    }

    /// Rebuild the child blueprint from the sanitized CLI. `extension_ui` is
    /// deliberately None so the child uses a per-session non-interactive
    /// (fail-closed) approval host instead of the primary TUI adapter.
    fn child_blueprint(&self) -> Result<RunSessionBlueprint> {
        let (_, resource_options) = startup_resource_options(&self.cli, true)?;
        let mut blueprint = RunSessionBlueprint::from_cli(&self.cli, resource_options, None);
        blueprint.set_session_storage(self.storage.clone());
        Ok(blueprint)
    }
}


impl crate::modes::session_runtime_manager::SessionSpawner for RunSessionSpawner {
    fn prepare(
        &self,
        request: crate::modes::session_runtime_manager::SessionSpawnRequest,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<crate::modes::session_runtime_manager::PreparedSessionSpawn>,
                > + Send,
        >,
    > {
        let tracker = self.detached_rollback_tracker();
        let this = self.clone();
        Box::pin(async move {
            let crate::modes::session_runtime_manager::SessionSpawnRequest { kind, source } =
                request;
            match kind {
                crate::modes::session_runtime_manager::SessionSpawnKind::Fresh { directory } => {
                    this.prepare_new_session(&source, directory, tracker)
                        .await
                        .map(crate::modes::session_runtime_manager::PreparedSessionSpawn::Fresh)
                }
                kind => Ok(crate::modes::session_runtime_manager::PreparedSessionSpawn::Request(
                    crate::modes::session_runtime_manager::SessionSpawnRequest { kind, source },
                )),
            }
        })
    }
    fn spawn(
        &self,
        request: crate::modes::session_runtime_manager::SessionSpawnRequest,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<crate::modes::session_runtime_manager::SessionSpawnResult>>
                + Send,
        >,
    > {
        let this = self.clone();
        Box::pin(async move {
            let crate::modes::session_runtime_manager::SessionSpawnRequest { kind, source } =
                request;
            match kind {
                crate::modes::session_runtime_manager::SessionSpawnKind::Open { prepared } => {
                    this.open_resumed(&source, prepared).await
                }
                crate::modes::session_runtime_manager::SessionSpawnKind::Fresh { .. } => {
                    unreachable!("fresh sessions are completed during preparation")
                }
                crate::modes::session_runtime_manager::SessionSpawnKind::Fork { entry_id } => {
                    this.fork_session(&source, &entry_id).await
                }
                crate::modes::session_runtime_manager::SessionSpawnKind::ForkPrepared {
                    prepared,
                    entry_id,
                } => {
                    this.fork_prepared_session(&source, prepared, &entry_id)
                        .await
                }
                crate::modes::session_runtime_manager::SessionSpawnKind::Clone => {
                    this.clone_session(&source).await
                }
                crate::modes::session_runtime_manager::SessionSpawnKind::ClonePrepared {
                    prepared,
                } => this.clone_prepared_session(&source, prepared).await,
            }
        })
    }

    fn session_storage(&self) -> Option<SessionStorage> {
        Some(self.storage())
    }

    fn track_detached_cleanups(&self, tracker: SpawnRollbackTracker) {
        *self.detached_cleanups.lock() = Some(tracker);
    }


    fn todo_child_service(&self) -> Option<Arc<TodoChildService>> {
        self.todo_child_service.upgrade()
    }
}

/// Child `SessionOptions` derived from the source runtime's live snapshot,
/// mirroring the cross-directory switch construction exactly.
fn child_session_options(source: &Application, cwd: &Path) -> SessionOptions {
    let options = source.session().child_session_options_snapshot();
    SessionOptions {
        model: options.model,
        cwd: cwd.to_path_buf(),
        system_prompt: String::new(),
        thinking_level: options.thinking_level,
        api_key: options.api_key,
        compaction: source.session().compaction_settings(),
        stream_options: options.stream_options,
        tools: None,
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: Some(options.stream_fn),
        auth_resolver: options.auth_resolver,
        auth_manager: options.auth_manager,
    }
}

/// Recreate the LEAF of a saved session's recorded working directory when it
/// no longer exists (deleted temp directories, workflow-worktree roots, and
/// similar ephemeral placements). Opening an EXISTING (saved/imported)
/// session must never fail or silently relocate merely because its cwd
/// vanished — but the heal is deliberately conservative: only the leaf is
/// recreated, and only when its parent already exists as a valid operator
/// directory. Missing parent trees are NEVER resurrected; the pre-existing
/// fallback (source runtime cwd) applies and the missing path stays
/// unbound. This is NOT applied to new-session creation with a user-typed
/// target cwd — those keep their explicit bounded failure. Log lines are
/// path-free so absolute filesystem paths never leak into stderr.
fn heal_missing_recorded_cwd(recorded_cwd: &Path) {
    if recorded_cwd.as_os_str().is_empty() || std::fs::metadata(recorded_cwd).is_ok() {
        return;
    }
    let (Some(parent), Some(leaf)) = (recorded_cwd.parent(), recorded_cwd.file_name()) else {
        return;
    };
    // Capability-safe creation: the parent is validated no-follow and the
    // leaf is created through the held parent lease, closing the
    // inspect-then-mkdir symlink-swap window. Missing parents are NEVER
    // resurrected; the pre-existing fallback (source runtime cwd) applies
    // and the missing path stays unbound. Log lines are path-free so
    // absolute filesystem paths never leak into stderr.
    match pi_coding::create_operator_leaf_directory(parent, leaf) {
        Ok(_) => warn_line("session_open category=missing_cwd_recreated"),
        Err(error) => warn_line(&format!(
            "session_open category=missing_cwd_recreate_failed error_kind={error}"
        )),
    }
}

/// Attach session-scoped workflow storage for a spawned child: same policy as
/// startup (`setup_workflows` with roots namespaced by the child's session
/// identity), so per-session Workflow RPC state is isolated.
async fn setup_child_workflows(
    application: &Application,
    cwd: &Path,
    session_id: &str,
) -> Result<()> {
    let agent_dir = pi_coding::agent_dir_path();
    let (store_root, worktree_root) = workflow_storage_roots(cwd, &agent_dir, session_id);
    application
        .setup_workflows(
            pi_coding::project_context_boundary(cwd).unwrap_or_else(|_| cwd.to_path_buf()),
            store_root,
            worktree_root,
        )
        .await
        .context("binding workflow storage for spawned session")?;
    Ok(())
}

/// The child CLI inherits every startup policy (tools, extensions, trust,
/// approval mode, model scoping, profiles) but drops per-run baggage: no
/// nested listener, no resume/open/fork selection, no initial prompts, and
/// recording is FORCED so every manager-owned runtime has a durable session
/// id (the routing registry must never key an empty id).
fn sanitize_cli_for_children(cli: &Cli) -> Cli {
    let mut clone = cli.clone();
    clone.listen = None;
    clone.listen_token_file = None;
    clone.listen_allow_insecure_remote = false;
    clone.prompt = Vec::new();
    clone.resume = None;
    clone.session = None;
    clone.fork = None;
    clone.continue_latest = false;
    clone.session_id = None;
    clone.no_session = false;
    clone.name = None;
    clone.export = None;
    clone.output = None;
    clone.jsonl = false;
    clone.list_models = None;
    clone.print = false;
    clone
}

pub(crate) fn extension_mode(cli: &Cli) -> ExtensionMode {
    match cli.mode {
        Some(crate::args::Mode::Json) => ExtensionMode::Json,
        Some(crate::args::Mode::Rpc) => ExtensionMode::Rpc,
        _ if cli.listen.is_some() => ExtensionMode::Rpc,
        Some(crate::args::Mode::Text) => ExtensionMode::Print,
        None if cli.is_print_mode() => ExtensionMode::Print,
        None => ExtensionMode::Tui,
    }
}

async fn authenticated_model(
    model: pi_ai::Model,
    explicit_key: Option<&str>,
) -> Result<Option<(pi_ai::Model, ModelRequestAuth)>> {
    if explicit_key.is_none() && !crate::models_config::has_configured_auth(&model) {
        return Ok(None);
    }
    #[cfg(test)]
    let auth = if explicit_key.is_some() {
        crate::models_config::resolve_model_request_auth(&model, explicit_key, None)?
    } else {
        crate::models_config::resolve_model_request_auth_async(&model, None, None).await?
    };
    #[cfg(not(test))]
    let auth =
        crate::models_config::resolve_model_request_auth_async(&model, explicit_key, None).await?;
    Ok(
        crate::models_config::model_is_available_for_request_auth(&model, &auth)
            .then_some((model, auth)),
    )
}

async fn first_authenticated_model(
    explicit_key: Option<&str>,
) -> Result<Option<(pi_ai::Model, ModelRequestAuth)>> {
    let mut providers = pi_ai::get_providers();
    providers.sort();
    providers.retain(|provider| provider != "faux");
    let mut candidates = Vec::new();
    for provider in &providers {
        if let Some(id) = pi_coding::default_model_per_provider(provider)
            && let Some(model) = pi_ai::get_model(provider, id)
        {
            candidates.push(model);
        }
    }
    let mut seen = candidates
        .iter()
        .map(|model| (model.provider.clone(), model.id.clone()))
        .collect::<HashSet<_>>();
    for provider in providers {
        let mut models = pi_ai::get_models(&provider);
        models.sort_by(|left, right| left.id.cmp(&right.id));
        for model in models {
            if seen.insert((model.provider.clone(), model.id.clone())) {
                candidates.push(model);
            }
        }
    }
    for model in candidates {
        if let Some(selected) = authenticated_model(model, explicit_key).await? {
            return Ok(Some(selected));
        }
    }
    Ok(None)
}

/// Resolve the initial model for a session: explicit CLI spec, an
/// authenticated resumed model, the authenticated settings default, then the
/// first authenticated model (only when no settings default is configured).
/// Returns `(model, api_key, parsed_think)` where `parsed_think` is the
/// reasoning-level suffix extracted from an explicit model spec or the
/// settings default spec (empty when none was present).
///
/// Settings defaults are interpreted exclusively by the shared pi-coding
/// authority [`pi_coding::resolve_default_model_pair`] in catalog-ready mode,
/// so validation and startup agree on trimmed views, provider/model slash
/// semantics, case-insensitive provider matching, and `:level` suffixes. A
/// configured default that resolves but cannot authenticate (or is
/// unavailable for the resolved credential) fails startup with an error
/// naming the configured model — it never silently falls through to another
/// authenticated model.
///
/// Shared by [`build_session`] and the ACP agent mode so both entrypoints
/// resolve models with identical precedence.
pub(crate) async fn resolve_initial_model(
    cli: &Cli,
    settings: &pi_coding::Settings,
    resume_ctx: Option<&BranchContext>,
) -> Result<(pi_ai::Model, String, String)> {
    let explicit_model = cli.model.as_deref().filter(|spec| !spec.is_empty());
    let explicit_key = cli.api_key.as_deref();
    let mut parsed_think = String::new();
    let mut selected: Option<(pi_ai::Model, ModelRequestAuth)> = None;
    let explicit_model_spec = explicit_model.map(|spec| {
        let Some(provider) = cli.provider.as_deref() else {
            return spec.to_owned();
        };
        let prefix = format!("{provider}/");
        if spec
            .get(..prefix.len())
            .is_some_and(|value| value.eq_ignore_ascii_case(&prefix))
        {
            spec.to_owned()
        } else {
            format!("{provider}/{spec}")
        }
    });
    if let Some(spec) = explicit_model_spec.as_deref() {
        let (model, level) = resolve_model_spec(spec)?;
        parsed_think = level;
        selected = authenticated_model(model, explicit_key).await?;
        if selected.is_none() {
            bail!("Model {spec:?} is not available for the resolved credential");
        }
    } else if let Some(ctx) = resume_ctx
        && let (Some(provider), Some(id)) = (ctx.provider.as_ref(), ctx.model_id.as_ref())
    {
        match pi_ai::get_model(provider, id) {
            Some(model) => match authenticated_model(model, explicit_key).await? {
                Some(resolved) => selected = Some(resolved),
                None => warn_line(&format!(
                    "Warning: Could not restore model {provider}/{id} (no auth configured)"
                )),
            },
            None => warn_line(&format!("Warning: Could not restore model {provider}/{id}")),
        }
    }
    // Settings defaults go through the shared pi-coding authority in
    // catalog-ready mode: startup has already applied the full model catalog
    // (`load_startup_catalogs`), so unknown combined prefixes fail here
    // instead of silently falling through.
    if selected.is_none() {
        match pi_coding::resolve_default_model_pair(
            settings,
            pi_coding::DefaultModelCatalogMode::CatalogReady,
        ) {
            Ok(pi_coding::ResolvedDefaultModelPair::Absent) => {}
            Ok(pi_coding::ResolvedDefaultModelPair::UnconfirmedCombined(spec)) => bail!(
                "settings defaultModel {spec:?} does not resolve against the loaded model catalog"
            ),
            Ok(pi_coding::ResolvedDefaultModelPair::DeferredEnv) => bail!(
                "settings default model still contains a $ environment reference; \
                 the expanded runtime view is required"
            ),
            Ok(pi_coding::ResolvedDefaultModelPair::Resolved(resolved)) => {
                if !resolved.warning.is_empty() {
                    warn_line(&format!("Warning: {}", resolved.warning));
                }
                if !resolved.thinking_level.is_empty() {
                    parsed_think = resolved.thinking_level;
                }
                let configured = format!("{}/{}", resolved.model.provider, resolved.model.id);
                selected = authenticated_model(resolved.model, explicit_key).await?;
                if selected.is_none() {
                    bail!(
                        "Configured settings default model {configured:?} is not available for the resolved credential; configure authentication for it or clear the settings default"
                    );
                }
            }
            Err(error) => return Err(error),
        }
    }
    if selected.is_none() {
        selected = first_authenticated_model(explicit_key).await?;
    }
    let (model, auth) = selected.ok_or_else(|| {
        anyhow!(
            "No authenticated models available. Configure auth.json, models.json, a provider API-key environment variable, or pass --model with --api-key."
        )
    })?;
    #[cfg(not(test))]
    if let Some(key) = explicit_key.filter(|key| !key.trim().is_empty()) {
        crate::models_config::set_runtime_api_key(&model.provider, key);
    }
    Ok((model, auth.api_key, parsed_think))
}

pub(crate) fn resolve_initial_thinking_level(
    cli_level: Option<&str>,
    model_level: &str,
    resume: Option<&BranchContext>,
    resume_has_thinking_entry: bool,
    settings_level: Option<&str>,
) -> pi_agent::ThinkingLevel {
    let level = cli_level
        .filter(|value| !value.is_empty())
        .or_else(|| (!model_level.is_empty()).then_some(model_level))
        .or_else(|| {
            resume
                .filter(|_| resume_has_thinking_entry)
                .map(|context| context.thinking_level.as_str())
        })
        .or_else(|| settings_level.filter(|value| !value.is_empty()))
        .unwrap_or_default();
    parse_thinking_level(level)
}

pub(crate) async fn resolve_model_scope(patterns: &[String]) -> Result<Vec<pi_ai::Model>> {
    use globset::GlobBuilder;

    let mut available = pi_ai::get_providers()
        .into_iter()
        .flat_map(|provider| pi_ai::get_models(&provider))
        .collect::<Vec<_>>();
    available.sort_by(|left, right| (&left.provider, &left.id).cmp(&(&right.provider, &right.id)));
    available.dedup_by(|left, right| left.provider == right.provider && left.id == right.id);

    available = crate::models_config::filter_models_for_resolved_auth_async(available, None).await;
    let mut scoped = Vec::new();
    let mut seen = HashSet::new();
    for pattern in patterns {
        let trimmed = pattern.trim();
        let base_pattern = trimmed.rsplit_once(':').map_or(trimmed, |(base, suffix)| {
            matches!(
                suffix,
                "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
            )
            .then_some(base)
            .unwrap_or(trimmed)
        });
        let matches = if base_pattern.contains(['*', '?', '[']) {
            let matcher = GlobBuilder::new(base_pattern)
                .case_insensitive(true)
                .build()
                .with_context(|| format!("invalid model pattern {pattern:?}"))?
                .compile_matcher();
            available
                .iter()
                .filter(|model| {
                    matcher.is_match(format!("{}/{}", model.provider, model.id))
                        || matcher.is_match(&model.id)
                })
                .cloned()
                .collect::<Vec<_>>()
        } else {
            available
                .iter()
                .filter(|model| {
                    format!("{}/{}", model.provider, model.id).eq_ignore_ascii_case(base_pattern)
                        || model.id.eq_ignore_ascii_case(base_pattern)
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        if matches.is_empty() {
            warn_line(&format!("Warning: No models match pattern {pattern:?}"));
        }
        for model in matches {
            if seen.insert((model.provider.clone(), model.id.clone())) {
                scoped.push(model);
            }
        }
    }
    Ok(scoped)
}

pub(crate) fn resolve_prompt_input(
    input: &str,
    cwd: &Path,
    description: &str,
) -> Result<(String, Option<PathBuf>)> {
    let workspace = pi_coding::WorkspaceRoots::new(cwd, std::iter::empty::<&Path>())
        .with_context(|| format!("resolving working directory {}", cwd.display()))?;
    match pi_coding::try_read_operator_file_under_roots(
        &workspace,
        Path::new(input),
        pi_coding::MAX_RESOURCE_FILE_BYTES,
    ) {
        Ok(None) => Ok((input.to_owned(), None)),
        Ok(Some(file)) => {
            let prompt = String::from_utf8(file.bytes)
                .with_context(|| format!("decoding {description} file {input:?} as UTF-8"))?;
            let source = if Path::new(input).is_absolute() {
                PathBuf::from(input)
            } else {
                cwd.join(input)
            };
            Ok((prompt, Some(source)))
        }
        Err(error) => Err(error).with_context(|| format!("reading {description} file {input:?}")),
    }
}

const SESSION_DIR_ENV: &str = "PI_CODING_AGENT_SESSION_DIR";

/// Resolve whether session persistence uses the default per-project tree or an
/// exact custom root. Precedence matches upstream: CLI, non-empty environment,
/// effective settings, then the default tree.
pub(crate) fn effective_session_storage(
    cli_session_dir: Option<&Path>,
    settings_session_dir: Option<&Path>,
) -> Result<SessionStorage> {
    let env_session_dir = std::env::var_os(SESSION_DIR_ENV).filter(|value| !value.is_empty());
    resolve_effective_session_storage(
        cli_session_dir,
        env_session_dir.as_deref().map(Path::new),
        settings_session_dir,
    )
}

/// Resolve the session directory for one working directory.
pub(crate) fn effective_session_dir(
    cwd: &Path,
    cli_session_dir: Option<&Path>,
    settings_session_dir: Option<&Path>,
) -> Result<PathBuf> {
    effective_session_storage(cli_session_dir, settings_session_dir)?.session_dir_for(cwd)
}

fn resolve_effective_session_storage(
    cli_session_dir: Option<&Path>,
    env_session_dir: Option<&Path>,
    settings_session_dir: Option<&Path>,
) -> Result<SessionStorage> {
    let configured = cli_session_dir.or(env_session_dir).or(settings_session_dir);
    let Some(configured) = configured else {
        return Ok(SessionStorage::default_tree());
    };
    let expanded = expand_session_dir_tilde(configured)?;
    let root =
        pi_coding::canonical_path(&expanded).context("resolving effective session directory")?;
    Ok(SessionStorage::ExplicitRoot(root))
}

fn expand_session_dir_tilde(path: &Path) -> Result<PathBuf> {
    if path == Path::new("~") || path.strip_prefix(Path::new("~")).is_ok() {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| {
                anyhow!(
                    "home directory is unavailable for session directory {}",
                    path.display()
                )
            })?;
        return Ok(if path == Path::new("~") {
            home
        } else {
            home.join(
                path.strip_prefix(Path::new("~"))
                    .expect("tilde prefix checked"),
            )
        });
    }
    Ok(path.to_path_buf())
}

/// Environment variable honored for the active config profile when the
/// `--profile` flag is absent.
pub(crate) const PROFILE_ENV: &str = "PI_PROFILE";

/// Resolve the active config profile from the CLI and `PI_PROFILE`, validate
/// it, and install it process-wide so every agent-dir-derived path (settings,
/// auth, sessions, memory, skills, workflows) relocates under
/// `<base>/profiles/<name>`. `default`, empty, and whitespace select the
/// default profile (no relocation). Must run before any dispatch that
/// resolves an agent-dir-derived path.
pub(crate) fn activate_profile(cli: &Cli) -> Result<()> {
    let profile = resolve_active_profile(cli.profile.as_deref())?;
    pi_coding::set_active_profile(profile.as_deref());
    Ok(())
}

/// Resolve the active config profile name: the CLI `--profile` flag wins over
/// the `PI_PROFILE` environment variable; `default`, empty, or whitespace
/// selects the default profile (no relocation). Named profiles are validated
/// with an actionable error.
pub(crate) fn resolve_active_profile(cli_profile: Option<&str>) -> Result<Option<String>> {
    let env_profile = std::env::var(PROFILE_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|name| !name.is_empty());
    resolve_profile_precedence(
        cli_profile.map(str::trim).filter(|name| !name.is_empty()),
        env_profile.as_deref(),
    )
}

/// Pure precedence resolver for [`resolve_active_profile`], factored out so
/// CLI-over-env precedence and name validation are unit-testable without
/// mutating process environment.
fn resolve_profile_precedence(
    cli_profile: Option<&str>,
    env_profile: Option<&str>,
) -> Result<Option<String>> {
    match cli_profile.or(env_profile) {
        Some(name) if name == "default" => Ok(None),
        Some(name) => {
            crate::args::validate_profile_name(name).map_err(anyhow::Error::msg)?;
            Ok(Some(name.to_owned()))
        }
        None => Ok(None),
    }
}

/// Startup TTL for expired-session pruning: the `sessionTtlDays` setting when
/// present, otherwise [`pi_coding::DEFAULT_SESSION_TTL_DAYS`]. Settings
/// validation rejects `0`, and zero defensively falls back to the default
/// here rather than ever pruning everything.
fn session_ttl_from_settings(settings: &pi_coding::Settings) -> Duration {
    settings
        .session_ttl_days
        .filter(|days| *days > 0)
        .map(|days| Duration::from_secs(days.saturating_mul(24 * 60 * 60)))
        .unwrap_or(Duration::from_secs(
            pi_coding::DEFAULT_SESSION_TTL_DAYS * 24 * 60 * 60,
        ))
}
fn adopted_session_dir(storage: &SessionStorage, cwd: &Path) -> Result<PathBuf> {
    storage.session_dir_for(cwd)
}

fn resolve_session_argument(
    argument: &str,
    cwd: &Path,
    session_dir: Option<&Path>,
) -> Result<pi_coding::PreparedSessionResume> {
    let catalog = pi_coding::SessionCatalog::from_env().map_err(anyhow::Error::new)?;
    let catalog = match session_dir {
        Some(root) => catalog.with_native_session_root(root),
        None => catalog,
    };
    catalog
        .prepare_native_resume(argument)
        .map_err(anyhow::Error::new)
        .with_context(|| format!("resolving session {argument:?} from {}", cwd.display()))
}

/// Stable per-process fallback identity for `--no-session` runs, where no
/// recorder exists to bind workflow storage to. Each process gets its own
/// namespace, so a fresh run never sees another run's workflows.
pub(crate) fn fallback_session_identity() -> &'static str {
    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN.get_or_init(|| format!("proc-{}", uuid::Uuid::now_v7()))
}

/// Resolve per-session workflow storage roots: the workflow store and the
/// managed worktree root for the given session identity. The namespace is
/// session-scoped (repository digest + session id for git directories, session
/// id alone otherwise), so a resumed session (same session id) restores its
/// workflows while a new session id in the same repository starts empty.
pub(crate) fn workflow_storage_roots(
    cwd: &Path,
    agent_dir: &Path,
    session_id: &str,
) -> (PathBuf, PathBuf) {
    let namespace = pi_coding::workflow_worktree::session_namespace(None, cwd, session_id);
    (
        agent_dir.join("workflows").join(&namespace),
        agent_dir.join("workflow-worktrees").join(namespace),
    )
}

/// Rebind workflow storage to the ACTIVE session identity after a session
/// cutover (`/new`, `/fresh`, resume, fork, import). `setup_workflows` runs
/// once at startup with the initial session id; without this rebind every
/// later session in the process would keep showing (and mutating) the first
/// session's workflows. The roots are recomputed from the live recorder id +
/// cwd + agent dir, so a fresh session starts with an empty workflow list
/// while a resumed session (same id) restores its own workflows. No-op when
/// workflows were never configured (e.g. RPC-only applications).
pub(crate) async fn rebind_workflows_for_active_session(
    application: &pi_coding::Application,
) -> Result<()> {
    if application.workflow_manager().is_err() {
        return Ok(());
    }
    let Some((session_id, _)) = application.session().recorder_info() else {
        return Ok(());
    };
    let cwd = application.session().cwd().to_path_buf();
    let agent_dir = pi_coding::agent_dir_path();
    let (workflow_store_root, workflow_worktree_root) =
        workflow_storage_roots(&cwd, &agent_dir, &session_id);
    application
        .rebind_workflows(workflow_store_root, workflow_worktree_root)
        .await?;
    Ok(())
}

/// Load process-wide model catalogs from local stores and clear runtime-key
/// state. Live Radius and llama.cpp refreshes are deferred until the
/// Application generation has been published.
pub(crate) async fn load_startup_catalogs(cli: &Cli) -> Result<()> {
    crate::models_config::load_custom_models()?;
    // The full catalog is applied: flip default-pair validation to the
    // strict catalog-ready authority so every persistence path in this
    // process (settings drafts, RPC/TUI writes) rejects unknown combined
    // prefixes instead of silently persisting them.
    pi_coding::set_default_model_catalog_ready(true);
    if let Err(error) = crate::models_config::restore_radius_catalog().await
        && cli.verbose
    {
        warn_line(&format!(
            "Warning: Could not load cached Radius catalog: {error:#}"
        ));
    }
    let llama = pi_coding::LlamaManager::global()?;
    if pi_ai::get_models(pi_ai::LLAMA_PROVIDER_ID).is_empty() {
        llama.load_cached_catalog()?;
    }
    for provider in pi_ai::get_providers() {
        crate::models_config::clear_runtime_api_key(&provider);
    }
    Ok(())
}

pub(crate) async fn refresh_radius_catalog(
    _application: &Application,
    should_warn: bool,
) -> Result<()> {
    if offline() {
        return Ok(());
    }
    match crate::models_config::refresh_radius_catalog().await {
        Ok(Some(warning)) if should_warn => warn_line(&format!(
            "Warning: Radius gateway unavailable; using cached catalog: {warning}"
        )),
        Err(error) if should_warn => warn_line(&format!(
            "Warning: Could not refresh Radius catalog: {error:#}"
        )),
        Ok(_) | Err(_) => {}
    }
    Ok(())
}

pub(crate) async fn refresh_llama_catalog(
    application: &Application,
    should_warn: bool,
) -> Result<()> {
    if offline() {
        return Ok(());
    }
    let llama = pi_coding::LlamaManager::global()?;
    if llama.effective_settings()?.is_none() {
        return Ok(());
    }
    if let Ok(refreshed) = llama
        .refresh_catalog(application.http_executor(pi_coding::HostHttpClass::LlamaRouter))
        .await
        && should_warn
        && let Some(warning) = refreshed.warning
    {
        warn_line(&format!(
            "Warning: llama.cpp router unavailable; using cached catalog: {warning}"
        ));
    }
    Ok(())
}

/// Resolve the retained-inspected working directory and resource-manager
/// options for a run. The returned path is a diagnostic coordinate; governed
/// filesystem operations capture their own directory authority. Interactive
/// TUI runs pass false; protocol, print, and Web-only listener modes pass true.
pub(crate) fn startup_resource_options(
    cli: &Cli,
    headless: bool,
) -> Result<(PathBuf, ResourceManagerOptions)> {
    let cwd = match &cli.cwd {
        Some(cwd) => cwd.clone(),
        None => std::env::current_dir().context("getting current directory")?,
    };
    let cwd = pi_coding::inspect_operator_directory(&cwd)
        .with_context(|| format!("resolving working directory {}", cwd.display()))?;
    if let Some(id) = cli.session_id.as_deref() {
        pi_coding::validate_session_id(id)?;
    }
    let mut resource_options = ResourceManagerOptions::new(&cwd);
    resource_options.headless = headless;
    resource_options.project_trust_override = if cli.approve {
        Some(true)
    } else if cli.no_approve {
        Some(false)
    } else {
        None
    };
    resource_options
        .explicit_extension_paths
        .clone_from(&cli.extensions);
    resource_options
        .explicit_skill_paths
        .clone_from(&cli.skills);
    resource_options
        .explicit_prompt_paths
        .clone_from(&cli.prompt_templates);
    resource_options
        .explicit_theme_paths
        .clone_from(&cli.themes);
    resource_options.disable_extensions = cli.no_extensions;
    resource_options.disable_skills = cli.no_skills;
    resource_options.disable_prompt_templates = cli.no_prompt_templates;
    resource_options.disable_themes = cli.no_themes;
    resource_options.disable_context_files = cli.no_context_files;
    Ok((cwd, resource_options))
}

/// Build the live [`Session`] from the parsed CLI flags, applying resume,
/// model restoration, and recording exactly as the Go CLI does.
pub async fn build_session(cli: &Cli) -> Result<RunSession> {
    load_startup_catalogs(cli).await?;
    let stdin_tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let stdout_tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let headless = cli.listen.is_some()
        || matches!(
            cli.mode,
            Some(crate::args::Mode::Json | crate::args::Mode::Rpc)
        )
        || cli.is_print_mode()
        || !stdin_tty
        || !stdout_tty;
    let (mut cwd, resource_options) = startup_resource_options(cli, headless)?;
    let auth_manager = Arc::new(pi_coding::AuthManager::new(
        resource_options.agent_dir.join("auth.json"),
    )?);
    // Interactive TUI mode: capture settings diagnostics so they can be shown
    // in the UI after startup instead of vanishing into pre-TUI stderr. Non-
    // interactive modes keep the existing stderr behavior.
    let is_tui = !headless;
    if is_tui {
        pi_coding::arm_settings_diagnostic_capture();
    }
    let mut blueprint = RunSessionBlueprint::from_cli(
        cli,
        resource_options,
        matches!(extension_mode(cli), ExtensionMode::Tui | ExtensionMode::Rpc)
            .then(ExtensionUiAdapter::new),
    );
    let mut preview_resources =
        pi_coding::ResourceManager::new(blueprint.resource_options_for_startup(&cwd)?)
            .context("loading settings and resources")?;
    let mut settings = preview_resources.snapshot().settings.clone();
    let storage =
        effective_session_storage(cli.session_dir.as_deref(), settings.session_dir.as_deref())?;
    let mut session_dir = storage.session_dir_for(&cwd)?;

    let mut prepared_resume: Option<pi_coding::PreparedSessionResume> =
        if let Some(input) = cli.resume.as_deref() {
            let sources = settings.effective_session_import_sources();
            let selection = resolve_resume_for_startup(input, Some(&cwd), &session_dir, &sources)?;
            Some(pi_coding::PreparedSessionResume::prepare_path(
                selection.path,
            )?)
        } else if let Some(argument) = cli.session.as_deref() {
            Some(resolve_session_argument(
                argument,
                &cwd,
                Some(&session_dir),
            )?)
        } else if let Some(argument) = cli.fork.as_deref() {
            Some(resolve_session_argument(
                argument,
                &cwd,
                Some(&session_dir),
            )?)
        } else if cli.continue_latest {
            pi_coding::list_sessions_in(&cwd, Some(&session_dir))
                .into_iter()
                .next()
                .map(|session| pi_coding::PreparedSessionResume::prepare_path(session.path))
                .transpose()?
        } else if let Some(id) = cli.session_id.as_deref() {
            pi_coding::list_sessions_in(&cwd, Some(&session_dir))
                .into_iter()
                .find(|session| session.id == id)
                .map(|session| pi_coding::PreparedSessionResume::prepare_path(session.path))
                .transpose()?
        } else {
            None
        };
    let resume_path = prepared_resume
        .as_ref()
        .map(|prepared| prepared.path().to_path_buf());
    let mut resume_ctx: Option<BranchContext> = None;
    let mut resume_has_thinking_entry = false;
    if let Some(prepared) = prepared_resume.as_ref() {
        if cli.cwd.is_none()
            && !prepared.target_cwd().as_os_str().is_empty()
        {
            heal_missing_recorded_cwd(prepared.target_cwd());
            if let Ok(inspected) = pi_coding::inspect_operator_directory(prepared.target_cwd())
            {
                cwd = inspected;
            }
        }
        resume_has_thinking_entry = prepared.tree().has_thinking_entry();
        resume_ctx = Some(prepared.build_context());
        preview_resources = preview_resources
            .rebuild_for_cwd(&cwd)
            .context("reloading settings and resources for resumed working directory")?;
        settings = preview_resources.snapshot().settings.clone();
    }
    session_dir = adopted_session_dir(&storage, &cwd)?;
    blueprint.set_session_storage(storage.clone());
    // Collect non-fatal startup warnings for TUI display. Settings diagnostics
    // were captured (stderr suppressed) when the TUI capture is armed; resource
    // diagnostics are collected from the snapshot. Error-level resource
    // diagnostics already bailed inside build_candidate, so only warnings remain.
    let mut startup_warnings = if is_tui {
        pi_coding::drain_settings_diagnostics()
    } else {
        Vec::new()
    };
    if is_tui {
        for diagnostic in preview_resources.diagnostics() {
            let path = diagnostic
                .path
                .as_ref()
                .map_or(String::new(), |path| format!(" ({})", path.display()));
            startup_warnings.push(format!(
                "{:?}: {}{}",
                diagnostic.level, diagnostic.message, path
            ));
        }
    }
    let model_patterns = cli
        .models
        .as_deref()
        .or_else(|| settings.scoped_model_patterns());
    let scoped_models = match model_patterns {
        Some(patterns) => Some(resolve_model_scope(patterns).await?),
        None => None,
    };

    // 3. Resolve the initial model: explicit CLI, authenticated resumed model,
    //    authenticated settings default, then the first authenticated model.
    let (model, api_key, parsed_think) =
        resolve_initial_model(cli, &settings, resume_ctx.as_ref()).await?;

    // 4. Thinking priority: --think, model suffix, resumed recorded thinking,
    //    settings default, then the existing medium default.
    let thinking_level = resolve_initial_thinking_level(
        cli.think.as_deref(),
        &parsed_think,
        resume_ctx.as_ref(),
        resume_has_thinking_entry,
        settings.default_thinking_level.map(thinking_level_str),
    );

    let options = SessionOptions {
        model: model.clone(),
        cwd: cwd.clone(),
        system_prompt: String::new(),
        thinking_level,
        api_key: api_key.clone(),
        compaction: Some(DEFAULT_COMPACTION_SETTINGS),
        stream_options: Default::default(),
        tools: None,
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
        auth_manager: Some(auth_manager),
    };
    let mut candidate = blueprint.build(&cwd, options.clone()).await?;
    let mut retry_resume: Option<(
        BranchContext,
        pi_coding::SessionRecorder,
        Option<String>,
        Option<pi_ai::Model>,
        pi_agent::ThinkingLevel,
    )> = None;
    let setup_result: Result<()> = async {
        if let Some(path) = &resume_path {
            let ctx = resume_ctx.take().expect("resume context");
            let messages_len = ctx.messages.len();

            eprintln!(
                "\x1b[2mresumed {} messages from {}\x1b[0m",
                messages_len,
                path.display()
            );
            if !cli.no_session {
                let prepared = prepared_resume.take().expect("prepared resume");
                let recorder = if cli.fork.is_some() {
                    pi_coding::fork_prepared_session_in(
                        prepared,
                        &cwd,
                        Some(&session_dir),
                        cli.session_id.as_deref(),
                    )?
                } else {
                    prepared.into_recorder()?
                };
                if !resume_has_thinking_entry {
                    recorder.record_thinking_level(thinking_level_str(thinking_level))?;
                }
                candidate
                    .session
                    .resume_recording(recorder.clone(), ctx.messages.clone())
                    .await?;
                retry_resume = Some((
                    ctx,
                    recorder,
                    None,
                    candidate.session.model(),
                    candidate.session.thinking_level(),
                ));
            } else {
                candidate.session.load_history(ctx.messages).await?;
            }
        } else if !cli.no_session {
            let recorder = pi_coding::start_session_in(
                &cwd,
                Some(&model),
                Some(thinking_level_str(thinking_level)),
                Some(&session_dir),
                cli.session_id.as_deref(),
                None,
            )?;
            candidate.session.record(recorder)?;
        }
        if let Some(name) = cli.name.as_deref() {
            candidate.session.set_session_name(name)?;
            if let Some((_, _, session_name, _, _)) = retry_resume.as_mut() {
                *session_name = candidate.session.session_name();
            }
        }
        Ok(())
    }
    .await;
    if let Err(error) = setup_result {
        candidate.session.abort().await;
        candidate.session.wait_for_idle().await;
        if let Some(orchestration) = candidate.orchestration {
            orchestration.shutdown().await;
        }
        candidate.extension_runtime.shutdown().await;
        return Err(error);
    }

    // Best-effort TTL cleanup of expired native session files, run after
    // resume/start resolution so the current run's session file is known and
    // skipped (a resumed session may itself be older than the TTL). The prune
    // swallows I/O errors, so cleanup can never fail startup.
    {
        let native_root = pi_coding::native_sessions_root();
        let mut prune_roots = Vec::new();
        if !session_dir.starts_with(&native_root) {
            prune_roots.push(session_dir.clone());
        }
        prune_roots.push(native_root);
        let mut prune_skip = Vec::new();
        if let Some(path) = &resume_path {
            prune_skip.push(path.clone());
        }
        if let Some((_, path)) = candidate.session.recorder_info() {
            prune_skip.push(path);
        }
        // Directory-level guard: the prune best-effort-removes emptied
        // per-cwd dirs, and a just-started auto-id recorder has NO file on
        // disk yet, so its dir is empty and would be deleted before the first
        // flush (ENOENT on the next persist). Never remove this run's session
        // dir root, nor the parent of the current/resumed session file.
        let mut prune_dir_skip = Vec::new();
        prune_dir_skip.push(session_dir.clone());
        if let Some(path) = &resume_path {
            if let Some(parent) = path.parent() {
                prune_dir_skip.push(parent.to_path_buf());
            }
        }
        if let Some((_, path)) = candidate.session.recorder_info()
            && let Some(parent) = path.parent()
        {
            prune_dir_skip.push(parent.to_path_buf());
        }
        let _ = pi_coding::prune_expired_sessions(
            &prune_roots,
            std::time::SystemTime::now(),
            session_ttl_from_settings(&settings),
            &prune_skip,
            &prune_dir_skip,
        );
    }
    let (application, resume_notice) = match application_from_session_candidate(candidate).await {
        Ok(application) => application,
        Err(error) if retry_resume.is_some() && durable_resume_error(&error).is_some() => {
            let warning = durable_resume_notice(&error);
            let mut retry = blueprint
                .build_without_orchestration(&cwd, options)
                .await
                .context("rebuilding resumed session runtime without orchestration")?;
            if let Some((context, recorder, session_name, resolved_model, thinking_level)) =
                retry_resume
            {
                retry = retry
                    .restore_resume(
                        context,
                        recorder,
                        session_name,
                        resolved_model,
                        thinking_level,
                    )
                    .await?;
            }
            (
                application_from_session_candidate(retry).await?.0,
                Some(warning),
            )
        }
        Err(error) => return Err(error),
    };
    let todo_child_service = TodoChildService::new();
    let factory: Arc<dyn ApplicationRuntimeFactory> = Arc::new(blueprint.clone());
    let application_setup: Result<()> = async {
        application.attach_runtime_factory(factory.clone())?;
        todo_child_service.configure_factory(factory.clone())?;
        application.attach_todo_child_client(todo_child_service.client())?;
        refresh_radius_catalog(&application, true).await?;
        refresh_llama_catalog(&application, true).await?;
        if resume_path.is_some() {
            application.prepare_resumed_goal(cli.fork.is_some())?;
        }
        let agent_dir = pi_coding::agent_dir_path();
        // Workflow storage is scoped to the owning session: the recorder's id for
        // recorded sessions (new or resumed), a stable per-process token for
        // `--no-session` runs where no recorder exists.
        let session_identity = application
            .session()
            .recorder_info()
            .map(|(id, _)| id)
            .unwrap_or_else(|| fallback_session_identity().to_owned());
        let (workflow_store_root, workflow_worktree_root) =
            workflow_storage_roots(&cwd, &agent_dir, &session_identity);
        application
            .setup_workflows(
                pi_coding::project_context_boundary(&cwd).unwrap_or_else(|_| cwd.clone()),
                workflow_store_root,
                workflow_worktree_root,
            )
            .await?;
        Ok(())
    }
    .await;
    if let Err(error) = application_setup {
        let shutdown = todo_child_service
            .shutdown()
            .await
            .context("shutting down Todo child service");
        application.cleanup().await;
        return match shutdown {
            Ok(()) => Err(error),
            Err(shutdown_error) => Err(error.context(format!(
                "Todo child service shutdown also failed: {shutdown_error:#}"
            ))),
        };
    }
    // Drain any settings diagnostics emitted after the initial resource load
    // (e.g. by blueprint.build which reloads resources). Due to process-wide
    // dedupe, these are typically empty unless new paths were loaded.
    if is_tui {
        startup_warnings.extend(pi_coding::drain_settings_diagnostics());
    }
    Ok(RunSession {
        application,
        extension_ui: blueprint.extension_ui(),
        model,
        scoped_models,
        startup_warnings,
        resume_notice,
        spawner: RunSessionSpawner::from_startup_with_service(
            cli,
            storage,
            todo_child_service.clone(),
        )
        .with_runtime_factory(factory),
        todo_child_service,
    })
}

/// Build an [`Application`] from one complete, independently-owned runtime
/// candidate. Durable degradation is handled only by blueprint-backed callers
/// that can reconstruct a fresh [`Session`] and [`ResourceManager`]; this
/// helper never retries an already-published owner.
pub(crate) async fn application_from_session_candidate(
    candidate: crate::session_run_blueprint::RunSessionCandidate,
) -> Result<(Application, Option<String>)> {
    let crate::session_run_blueprint::RunSessionCandidate {
        session,
        extension_runtime,
        extension_permissions,
        orchestration,
        goal_tool,
        todo_child_tool,
    } = candidate;
    let session_cleanup = session.clone();
    let extension_cleanup = extension_runtime.clone();
    let orchestration_cleanup = orchestration.as_ref().map(Clone::clone);
    let mut runtime = ApplicationRuntimeCandidate::new(session)
        .with_extensions(extension_runtime, extension_permissions);
    if let Some(orchestration) = orchestration {
        runtime = runtime.with_orchestration(orchestration);
    }
    if let Some(goal_tool) = goal_tool {
        runtime = runtime.with_goal_tool(goal_tool);
    }
    if let Some(todo_child_tool) = todo_child_tool {
        runtime = runtime.with_todo_child_tool(todo_child_tool);
    }
    match Application::from_runtime_candidate(runtime).await {
        Ok(application) => Ok((application, None)),
        Err(error) => {
            session_cleanup.abort().await;
            session_cleanup.wait_for_idle().await;
            session_cleanup
                .process_manager()
                .shutdown_owner(&session_cleanup.process_owner_id())
                .await;
            extension_cleanup
                .shutdown_with_reason("application_construction_failed")
                .await;
            if let Some(runtime) = orchestration_cleanup {
                runtime.shutdown().await;
            }
            Err(error)
        }
    }
}

fn durable_resume_error(error: &anyhow::Error) -> Option<&pi_coding::DurableResumeError> {
    if let Some(resume) = pi_coding::DurableResumeError::find_in_chain(error) {
        return Some(resume);
    }
    // A durable validation error can lose its `DurableResumeError` wrapper
    // while still carrying the typed validation cause (map_err/context
    // chains). The validation type itself is the degradation contract: any
    // persisted-state mismatch must degrade to an unbound resume, never fail
    // the whole switch.
    let has_validation = error.chain().any(|cause| {
        cause
            .downcast_ref::<pi_coding::DurableStateValidationError>()
            .is_some()
    });
    if has_validation {
        Some(durable_resume_sentinel())
    } else {
        None
    }
}

/// Sentinel backing [`durable_resume_error`]'s fallback match. The caller
/// never reads fields off it — it only checks `.is_some()` and formats the
/// ORIGINAL error — so a lazily-built default keeps the existing
/// `Option<&DurableResumeError>` contract without unwrapping.
fn durable_resume_sentinel() -> &'static pi_coding::DurableResumeError {
    SENTINEL.get_or_init(|| {
        pi_coding::DurableResumeError::State(pi_coding::DurableStateValidationError::Policy(
            pi_sandbox::PolicyValidationError::InvalidField {
                field: "snapshot.policy",
                reason: "durable state validation degraded the resume (see the original error)",
            },
        ))
    })
}

static SENTINEL: std::sync::OnceLock<pi_coding::DurableResumeError> = std::sync::OnceLock::new();

fn durable_resume_notice(error: &anyhow::Error) -> String {
    pi_coding::DurableResumeError::degrade_notice(error)
}

/// Build a generation-only operator [`Application`] for CLI subcommands that
/// need governed HTTP/process authority (`http_executor`, governed spawns)
/// but never run an agent turn: `models`/`--list-models`, `llama`, `plugin`,
/// and `package install/list/update`.
///
/// Unlike [`build_session`], this path:
/// - never resolves an authenticated model — a synthetic faux operator model
///   (the production `faux` builtin, whose provider derives no `ProviderApi`
///   HTTP destination) stands in, so no API key, auth resolver, or catalog
///   fallback is consulted;
/// - never starts a recorder or writes a session file — the session is
///   unrecorded, so `rpi plugin list` no longer creates a session record;
/// - still publishes a governed sandbox generation, so `http_executor(class)`
///   and `governed_process_context()` honor the settings-derived host HTTP
///   policy and governed spawn authority exactly as a real session would.
///
/// On platforms without confined sandbox support (Windows), the sandbox mode
/// follows the user's settings exactly as a normal session does; the operator
/// context adds no confined-bootstrap requirement of its own beyond that.
pub(crate) async fn build_operator_application(cli: &Cli) -> Result<Application> {
    load_startup_catalogs(cli).await?;
    let (cwd, resource_options) = startup_resource_options(cli, true)?;
    build_operator_application_from_options(cwd, resource_options).await
}

/// Generation-only operator [`Application`] construction from already-resolved
/// resource options, factored out so the no-model/no-recorder/governed-authority
/// contract is unit-testable with an isolated agent dir (no process-global
/// catalog loading or runtime-key clearing).
///
/// See [`build_operator_application`] for the contract this upholds.
async fn build_operator_application_from_options(
    cwd: PathBuf,
    resource_options: ResourceManagerOptions,
) -> Result<Application> {
    let auth_manager = Arc::new(AuthManager::new(
        resource_options.agent_dir.join("auth.json"),
    )?);
    let mut operator_model = pi_ai::Model::default();
    operator_model.id = "operator".to_owned();
    operator_model.name = "Operator".to_owned();
    operator_model.api = pi_ai::API_FAUX.to_owned();
    operator_model.provider = "faux".to_owned();
    operator_model.base_url = "http://localhost:0".to_owned();
    let options = SessionOptions {
        model: operator_model,
        cwd: cwd.clone(),
        system_prompt: String::new(),
        thinking_level: pi_agent::ThinkingLevel::Off,
        api_key: String::new(),
        compaction: None,
        stream_options: Default::default(),
        tools: None,
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
        auth_manager: Some(auth_manager),
    };
    let session = Session::new(options)?;
    // The operator context never records (no recorder is bound, so no session
    // file is ever materialized), but generation validation requires the
    // session-store root to exist. Scope it to the operator's own agent dir:
    // the default `~/.rpi/sessions/<cwd>` would point into the ambient
    // agent base even when an isolated agent dir is configured, and would
    // break for working directories that never had a session.
    let session_dir = resource_options.agent_dir.join("sessions");
    // Create the operator session-store root through the capability-filesystem
    // bootstrap primitive (the same governed seam `CapabilityFs` itself uses),
    // never a raw ambient `std::fs` path in this governed module.
    cap_std::fs::Dir::create_ambient_dir_all(&session_dir, cap_std::ambient_authority())
        .with_context(|| format!("creating operator session store {}", session_dir.display()))?;
    session.set_session_dir(session_dir);
    let resources = ResourceManager::new(resource_options)
        .context("loading settings and resources for operator context")?;
    session.attach_resources(resources).await?;
    let application =
        Application::from_runtime_candidate(ApplicationRuntimeCandidate::new(session))
            .await
            .context("building operator application generation")?;
    Ok(application)
}

/// Run a single prompt in print mode through the shared [`Application`]
/// lifecycle and human event renderer. Returns the final assistant text.
pub async fn run_print_to<W>(
    application: &Application,
    prompt: &str,
    writer: &mut W,
    ansi: bool,
) -> Result<String>
where
    W: std::io::Write,
{
    if let Some(argument) = prompt.trim().strip_prefix("/goal") {
        let command = crate::goal_commands::parse_interactive_goal_command(
            (!argument.trim().is_empty()).then_some(argument.trim()),
        )?;
        let output =
            crate::goal_commands::execute_interactive_goal_command(application, command).await?;
        writer.write_all(output.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        return Ok(output);
    }
    if let Some(argument) = prompt.trim().strip_prefix("/workflow") {
        // Exact prefix: "/workflow" must not match "/workfloww".
        if argument.is_empty() || argument.starts_with(char::is_whitespace) {
            let rest = argument.trim_start();
            let command = crate::workflow_commands::parse_interactive_workflow_command(
                (!rest.is_empty()).then_some(rest),
            )?;
            let effect = crate::workflow_commands::execute_interactive_workflow_on_application(
                application,
                command,
            )
            .await?;
            let output = match effect {
                crate::workflow_commands::WorkflowCommandEffect::OpenPage => {
                    "Open the workflows page in the full-screen TUI (bare /workflow).".to_owned()
                }
                crate::workflow_commands::WorkflowCommandEffect::Message(message) => message,
            };
            writer.write_all(output.as_bytes())?;
            writer.write_all(b"\n")?;
            writer.flush()?;
            return Ok(output);
        }
    }
    if prompt.trim().is_empty() {
        bail!("print mode requires a prompt");
    }
    crate::human_event_renderer::run_human_turn_to(application, prompt, writer, ansi).await
}

/// Run a single prompt in print mode, streaming to stdout.
pub async fn run_print(application: &Application, prompt: &str) -> Result<String> {
    use std::io::IsTerminal as _;

    let mut stdout = std::io::stdout();
    let ansi = stdout.is_terminal();
    run_print_to(application, prompt, &mut stdout, ansi).await
}

/// `rpi --print` / non-interactive text entrypoint.
pub async fn print_mode(cli: &Cli) -> Result<()> {
    let prompts = cli
        .prompt
        .iter()
        .filter(|prompt| !prompt.is_empty())
        .collect::<Vec<_>>();
    if prompts.is_empty() {
        bail!("print mode requires a prompt");
    }
    let RunSession {
        application,
        todo_child_service,
        ..
    } = build_session(cli).await?;
    let result = async {
        for prompt in prompts {
            run_print(&application, prompt).await?;
        }
        Ok(())
    }
    .await;
    let shutdown = todo_child_service
        .shutdown()
        .await
        .context("shutting down Todo child service");
    application.cleanup().await;
    match (result, shutdown) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(shutdown_error)) => Err(error.context(format!(
            "Todo child service shutdown also failed: {shutdown_error:#}"
        ))),
    }
}

pub(crate) fn extension_startup_error(report: &pi_coding::ExtensionLoadReport) -> anyhow::Error {
    anyhow!(
        "extension startup rejected {} extension(s): {}",
        report.failures.len(),
        report
            .failures
            .iter()
            .map(|failure| format!(
                "{} ({}): {}",
                failure.extension_id,
                failure.path.display(),
                failure.message
            ))
            .collect::<Vec<_>>()
            .join("; ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::sync::Arc;

    use clap::Parser as _;
    use parking_lot::Mutex;
    use pi_agent::{AbortController, AgentToolResult, ThinkingLevel, ToolCallContext};
    use pi_ai::{
        Model, SimpleStreamOptions,
        providers::{FauxProviderOptions, register_faux_provider},
    };
    use pi_coding::{
        ChildSessionFactory, DURABLE_STATE_VERSION, DurablePolicyState, DurableState, GoalState,
        GoalToolBinding, OrchestrationRuntime, ResourceDiscovery, ResourceManager, Session,
        Settings, TaskItem, ToolSelection,
    };
    use pi_sandbox::{CredentialGrantId, CredentialPolicy, PolicySnapshot, SandboxPolicy};
    use serde_json::json;

    #[test]
    fn canonical_offline_env_name_is_exact_and_legacy_is_unread() {
        // The runtime reads only the canonical name, so a stale legacy
        // `PI_OFFLINE` variable can never activate the offline contract.
        assert_eq!(RPI_OFFLINE_ENV, "RPI_OFFLINE");
        assert_ne!(RPI_OFFLINE_ENV, "PI_OFFLINE");
    }

    #[test]
    fn canonical_offline_control_preserves_truthy_semantics() {
        for value in ["1", "true", "TRUE", " yes ", "tRuE"] {
            assert!(offline_from(Some(value)), "{value:?} must count as offline");
        }
        for value in ["", "0", "false", "no", "off", "2", "disabled"] {
            assert!(!offline_from(Some(value)), "{value:?} must stay online");
        }
        assert!(!offline_from(None), "missing RPI_OFFLINE stays online");
    }

    fn blueprint(cli: &Cli) -> RunSessionBlueprint {
        let root =
            std::env::temp_dir().join(format!("pi-cli-blueprint-fixture-{}", uuid::Uuid::now_v7()));
        let mut options = ResourceManagerOptions::new(root.join("cwd"));
        options.agent_dir = root.join("agent");
        options.project_trust_override = Some(false);
        options.disable_extensions = true;
        options.disable_skills = true;
        options.disable_prompt_templates = true;
        options.disable_themes = true;
        options.disable_context_files = true;
        RunSessionBlueprint::from_cli(cli, options, None)
    }

    fn hermetic_session(options: SessionOptions) -> Session {
        Session::new_with_additional_tools_filtered_and_discovery(
            options,
            Vec::new(),
            ToolSelection::default(),
            ResourceDiscovery::Disabled,
        )
        .expect("session")
    }

    fn isolated_resource_manager(cwd: &Path, agent_dir: &Path) -> ResourceManager {
        let mut options = ResourceManagerOptions::new(cwd);
        options.agent_dir = agent_dir.to_path_buf();
        options.project_trust_override = Some(false);
        options.disable_extensions = true;
        options.disable_skills = true;
        options.disable_prompt_templates = true;
        options.disable_themes = true;
        options.disable_context_files = true;
        ResourceManager::new(options).expect("isolated resources")
    }

    async fn hermetic_application(options: SessionOptions, agent_dir: &Path) -> Application {
        let cwd = options.cwd.clone();
        let session = hermetic_session(options);
        let session_dir = agent_dir.join(format!("sessions-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&session_dir).expect("isolated session dir");
        fs::write(
            agent_dir.join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )
        .expect("isolated sandbox settings");
        session.set_session_dir(session_dir);
        session
            .attach_resources(isolated_resource_manager(&cwd, agent_dir))
            .await
            .expect("attach isolated resources");
        Application::from_runtime_candidate(pi_coding::ApplicationRuntimeCandidate::new(session))
            .await
            .expect("isolated application")
    }

    #[tokio::test]
    async fn operator_context_publishes_governed_authority_without_model_or_recorder() {
        // The operator context relies on the production `faux` builtin
        // (registered by `register_builtins`), whose provider derives no
        // `ProviderApi` HTTP destination — no authenticated model resolution.
        pi_ai::providers::register_builtins();
        let cwd = tempfile::tempdir().expect("cwd");
        let agent = tempfile::tempdir().expect("agent dir");
        fs::write(
            agent.path().join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )
        .expect("isolated sandbox settings");
        let mut resource_options = ResourceManagerOptions::new(cwd.path());
        resource_options.agent_dir = agent.path().to_path_buf();
        resource_options.project_trust_override = Some(false);
        resource_options.disable_extensions = true;
        resource_options.disable_skills = true;
        resource_options.disable_prompt_templates = true;
        resource_options.disable_themes = true;
        resource_options.disable_context_files = true;

        let application =
            build_operator_application_from_options(cwd.path().to_path_buf(), resource_options)
                .await
                .expect("operator application");

        // No authenticated model resolution: the session carries the synthetic
        // faux operator model, not a settings/auth-resolved provider model.
        let model = application.session().model().expect("operator model");
        assert_eq!(model.api, pi_ai::API_FAUX);
        assert_eq!(model.provider, "faux");

        // No recorder/session side effect: the operator context never started a
        // recorder, so no session file is bound to the session.
        assert!(
            application.session().recorder_info().is_none(),
            "operator context must not record a session"
        );

        // Governed HTTP/process authority is published through the generation:
        // the operator HTTP classes and governed spawn context are reachable.
        let _http = application.http_executor(pi_coding::HostHttpClass::PluginMarketplace);
        let _process = application.governed_process_context();

        // No session file is materialized: with no recorder bound, the session
        // never receives a recording path, so `rpi plugin list`/`rpi models`
        // cannot create a session record (verified above by recorder_info).

        application.cleanup().await;
    }

    fn registered_faux_model(label: &str) -> (Model, pi_ai::providers::FauxProviderRegistration) {
        let suffix = uuid::Uuid::now_v7().simple();
        let mut model = Model::default();
        model.id = label.to_owned();
        model.name = label.to_owned();
        model.api = format!("{label}-{suffix}-api");
        model.provider = format!("{label}-{suffix}-provider");
        model.base_url = "http://localhost:0".to_owned();
        let registration = register_faux_provider(FauxProviderOptions {
            api: model.api.clone(),
            provider: model.provider.clone(),
            models: vec![model.clone()],
            chunk_size: 12,
        });
        (model, registration)
    }

    #[test]
    fn effective_session_dir_obeys_cli_env_settings_default_precedence() {
        let cwd = tempfile::tempdir().expect("cwd");
        let cli = cwd.path().join("cli");
        let env = cwd.path().join("env");
        let settings = cwd.path().join("settings");

        assert_eq!(
            resolve_effective_session_storage(Some(&cli), Some(&env), Some(&settings))
                .expect("CLI"),
            SessionStorage::ExplicitRoot(cli)
        );
        assert_eq!(
            resolve_effective_session_storage(None, Some(&env), Some(&settings))
                .expect("environment"),
            SessionStorage::ExplicitRoot(env)
        );
        assert_eq!(
            resolve_effective_session_storage(None, None, Some(&settings)).expect("settings"),
            SessionStorage::ExplicitRoot(settings)
        );
        let storage = resolve_effective_session_storage(None, None, None).expect("default");
        assert_eq!(
            storage
                .session_dir_for(cwd.path())
                .expect("project directory"),
            pi_coding::default_session_dir(cwd.path())
        );
        assert_eq!(
            resolve_effective_session_storage(None, None, Some(Path::new("relative-sessions")))
                .expect("relative settings"),
            SessionStorage::ExplicitRoot(
                pi_coding::canonical_path(Path::new("relative-sessions")).expect("relative path")
            )
        );
    }

    #[test]
    fn effective_session_dir_ignores_empty_environment_value() {
        let cwd = tempfile::tempdir().expect("cwd");
        let settings = cwd.path().join("settings");
        let env_session_dir = std::ffi::OsStr::new("");
        let env_session_dir = (!env_session_dir.is_empty()).then(|| Path::new(env_session_dir));
        assert_eq!(
            resolve_effective_session_storage(None, env_session_dir, Some(&settings))
                .expect("settings"),
            SessionStorage::ExplicitRoot(settings)
        );
    }

    #[test]
    fn session_storage_keeps_default_projects_separate_and_custom_root_exact() {
        let root = tempfile::tempdir().expect("native root");
        let project_a = tempfile::tempdir().expect("project A");
        let project_b = tempfile::tempdir().expect("project B");
        let default = SessionStorage::DefaultTree {
            native_root: root.path().to_path_buf(),
        };
        let dir_a = default
            .session_dir_for(project_a.path())
            .expect("project A dir");
        let dir_b = default
            .session_dir_for(project_b.path())
            .expect("project B dir");
        assert_ne!(dir_a, dir_b);
        assert_eq!(dir_a.parent(), Some(root.path()));
        assert_eq!(dir_b.parent(), Some(root.path()));

        let custom = tempfile::tempdir().expect("custom root");
        let explicit = SessionStorage::ExplicitRoot(custom.path().to_path_buf());
        assert_eq!(
            explicit.session_dir_for(project_a.path()).expect("A"),
            custom.path()
        );
        assert_eq!(
            explicit.session_dir_for(project_b.path()).expect("B"),
            custom.path()
        );
    }

    #[test]
    fn default_tree_startup_resume_a_to_b_selects_b_tree() {
        let native_root = tempfile::tempdir().expect("native root");
        let project_a = tempfile::tempdir().expect("project A");
        let project_b = tempfile::tempdir().expect("project B");
        let source_root = tempfile::tempdir().expect("source root");
        let recorder = pi_coding::start_session_in(
            project_b.path(),
            None,
            Some("off"),
            Some(source_root.path()),
            None,
            None,
        )
        .expect("source recorder");
        recorder.persist_now().expect("persist source recorder");
        let prepared = pi_coding::PreparedSessionResume::prepare_path(recorder.path())
            .expect("prepare source resume");
        let storage = SessionStorage::DefaultTree {
            native_root: native_root.path().to_path_buf(),
        };

        let selection_dir = storage
            .session_dir_for(project_a.path())
            .expect("project A selection tree");
        let authoritative_cwd = pi_coding::inspect_operator_directory(prepared.target_cwd())
            .expect("authoritative resumed cwd");
        let live_dir =
            adopted_session_dir(&storage, &authoritative_cwd).expect("project B live tree");

        assert_eq!(authoritative_cwd, project_b.path());
        assert_eq!(
            live_dir,
            storage
                .session_dir_for(project_b.path())
                .expect("project B tree")
        );
        assert_ne!(live_dir, selection_dir);
    }
    /// A runtime whose session runs in `cwd`, standing in for an
    /// activated/resumed manager-owned runtime (child placement only reads
    /// the source session's live cwd).
    async fn session_application(
        cwd: &Path,
    ) -> (
        Application,
        tempfile::TempDir,
        pi_ai::providers::FauxProviderRegistration,
    ) {
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let (model, faux) = registered_faux_model("session-application");
        let application = hermetic_application(
            SessionOptions {
                model,
                cwd: cwd.to_path_buf(),
                system_prompt: String::new(),
                thinking_level: ThinkingLevel::Off,
                api_key: String::new(),
                compaction: None,
                stream_options: SimpleStreamOptions::default(),
                tools: Some(Vec::new()),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn: None,
                auth_resolver: None,
                auth_manager: None,
            },
            agent_dir.path(),
        )
        .await;
        (application, agent_dir, faux)
    }
    #[tokio::test]
    async fn fresh_target_drives_cwd_default_tree_storage_and_resources() {
        let root = tempfile::tempdir().expect("native root");
        let project_a = tempfile::tempdir().expect("project A");
        let project_b = tempfile::tempdir().expect("project B");
        let storage = SessionStorage::DefaultTree {
            native_root: root.path().to_path_buf(),
        };
        let cli = <Cli as clap::Parser>::try_parse_from([
            "rpi",
            "-C",
            project_a.path().to_str().expect("project A utf8"),
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
        ])
        .expect("cli");
        let spawner = RunSessionSpawner::from_startup(&cli, storage);

        let (source, _source_agent_dir, _source_faux) = session_application(project_b.path()).await;
        let (target_cwd, target_session_dir) = spawner
            .child_placement(project_a.path())
            .expect("target placement");
        assert_eq!(target_cwd, project_a.path());
        assert_eq!(
            target_session_dir,
            spawner
                .storage()
                .session_dir_for(project_a.path())
                .expect("project A dir")
        );
        assert_ne!(
            target_session_dir,
            spawner
                .storage()
                .session_dir_for(project_b.path())
                .expect("project B dir")
        );

        let blueprint = spawner.child_blueprint().expect("target blueprint");
        assert_eq!(
            blueprint
                .resource_options_for_startup(&target_cwd)
                .expect("target resources")
                .cwd,
            project_a.path()
        );
        let options = child_session_options(&source, &target_cwd);
        assert_eq!(options.cwd, project_a.path());
        // setup_child_workflows consumes this same target cwd when deriving
        // the session namespace; source B must map to different roots.

        let agent_root = tempfile::tempdir().expect("workflow agent root");
        let workflow_roots = workflow_storage_roots(project_a.path(), agent_root.path(), "fresh-a");
        assert_eq!(
            workflow_roots,
            workflow_storage_roots(&target_cwd, agent_root.path(), "fresh-a")
        );
        assert_ne!(
            workflow_roots,
            workflow_storage_roots(project_b.path(), agent_root.path(), "fresh-a")
        );
        source.cleanup().await;
    }

    #[tokio::test]
    async fn fresh_from_explicit_root_source_places_target_under_native_encoded_tree() {
        let custom_root = tempfile::tempdir().expect("custom source root");
        let project_a = tempfile::tempdir().expect("project A target");
        let project_b = tempfile::tempdir().expect("project B source");
        // The spawner belongs to a source session launched with an explicit
        // `--session-dir` custom root: startup cli cwd is the SOURCE project.
        let cli = Cli::try_parse_from([
            "rpi",
            "-C",
            project_b.path().to_str().expect("project B utf8"),
            "--api-key",
            "test-key",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
        ])
        .expect("cli");
        let todo_child_service = TodoChildService::new();
        let spawner = RunSessionSpawner::from_startup_with_service(
            &cli,
            SessionStorage::ExplicitRoot(custom_root.path().to_path_buf()),
            todo_child_service.clone(),
        );

        // Production sources always carry a live authentication owner; the
        // fresh child snapshot inherits it (build_session fails closed
        // otherwise). fork_source_application is the existing hermetic source
        // fixture with that contract.
        let (source_model, _source_faux) = registered_faux_model("fresh-explicit-source");
        let (source, _source_agent_dir) =
            fork_source_application(project_b.path(), &source_model).await;
        let source_session_dir = source.session().session_dir();
        // Contrast baseline: the spawner's own storage (which resume/fork use)
        // resolves to the custom root — Fresh must override exactly this.
        assert_eq!(
            spawner
                .child_placement(project_a.path())
                .expect("source storage placement")
                .1,
            custom_root.path(),
        );

        let directory =
            pi_coding::prepare_operator_directory(project_a.path()).expect("target directory");
        let prepared = spawner
            .prepare_new_session(&source, directory, None)
            .await
            .expect("prepare fresh from explicit-root source");

        // The TARGET cwd drives placement: exact target, not the source
        // project and not the startup cli cwd (which is also project B here).
        assert_eq!(prepared.cwd, project_a.path());
        assert_eq!(prepared.candidate.session.cwd(), project_a.path());

        // Fresh must override the source's ExplicitRoot with the process-native
        // default tree, encoded for the TARGET cwd's project component.
        let expected_target_tree = pi_coding::native_sessions_root().join(
            pi_coding::default_session_dir(project_a.path())
                .file_name()
                .expect("target project component"),
        );
        assert_eq!(
            prepared.candidate.session.session_dir(),
            expected_target_tree,
            "fresh target must be placed under the target cwd's encoded native tree",
        );
        assert_ne!(
            prepared.candidate.session.session_dir(),
            custom_root.path(),
            "fresh must not inherit the source's custom explicit root",
        );
        assert_ne!(
            prepared.candidate.session.session_dir(),
            pi_coding::native_sessions_root().join(
                pi_coding::default_session_dir(project_b.path())
                    .file_name()
                    .expect("source project component"),
            ),
            "fresh must not reuse the source project's encoded tree",
        );

        // The placement decision above pinned the production path; redirect the
        // recording into a hermetic root so the JSONL write cannot pollute the
        // operator's real native sessions root during tests.
        let hermetic_dir = tempfile::tempdir().expect("hermetic recording root");
        prepared
            .candidate
            .session
            .set_session_dir(hermetic_dir.path().to_path_buf());
        let spawned = prepared.finish().await.expect("finish fresh session");
        let (session_id, session_file) = spawned
            .application
            .session()
            .recorder_info()
            .expect("fresh recorder");
        assert_eq!(spawned.session_id, session_id);
        assert_eq!(spawned.session_file, session_file);
        assert_eq!(
            spawned.application.session().session_file_identity(),
            Some(spawned.file_identity)
        );

        // header.cwd is the exact target and no parentSession link exists: a
        // fresh session is a true root even when its source carries storage
        // configuration of its own.
        let tree = pi_coding::load_session_tree(&session_file).expect("fresh tree");
        assert_eq!(tree.header.cwd, project_a.path());
        assert!(tree.header.parent_session.is_none());
        let raw_header: serde_json::Value = serde_json::from_str(
            fs::read_to_string(&session_file)
                .expect("read fresh session file")
                .lines()
                .next()
                .expect("header line"),
        )
        .expect("parse fresh header");
        assert!(
            raw_header
                .get("parentSession")
                .and_then(serde_json::Value::as_str)
                .is_none(),
            "fresh header must not carry a parentSession reference: {raw_header}",
        );

        // Catalog row inputs for the Web selector: source native/rpi, status
        // native, cwd the exact target project (the selector groups rows by
        // cwd, so the target basename is the group the row must land in), and
        // no parent lineage. The source project must not gain a row.
        let catalog = pi_coding::SessionCatalog::new(hermetic_dir.path())
            .with_native_session_root(hermetic_dir.path());
        let rows = catalog.scan(&[pi_coding::SessionSourceKind::NativePi]);
        let row = rows
            .iter()
            .find(|row| row.session_id == session_id)
            .expect("fresh session must appear in the native catalog");
        assert_eq!(row.kind.as_str(), "rpi");
        assert_eq!(row.kind.label(), "rpi");
        assert!(row.kind.is_native());
        assert!(row.status.is_native(), "fresh row must be native status");
        assert_eq!(
            row.cwd,
            project_a.path(),
            "catalog row cwd must be the exact target",
        );
        assert_eq!(row.cwd.file_name(), project_a.path().file_name());
        assert_eq!(row.path, session_file);
        assert!(
            !row.has_parent_session,
            "fresh catalog row must have no parentSession lineage",
        );
        assert!(
            rows.iter().all(|row| row.cwd != project_b.path()),
            "the source project must not surface a fresh-session row",
        );

        // The source runtime is untouched: same cwd, same session dir, no
        // recorder created on it by the fresh preparation.
        assert_eq!(source.session().cwd(), project_b.path());
        assert_eq!(source.session().session_dir(), source_session_dir);
        assert!(source.session().recorder_info().is_none());

        spawned.application.cleanup().await;
        source.cleanup().await;
        todo_child_service
            .shutdown()
            .await
            .expect("shutdown Todo service");
    }

    #[tokio::test]
    async fn fresh_same_cwd_on_explicit_root_stays_in_the_listener_store() {
        let custom_root = tempfile::tempdir().expect("custom source root");
        let project = tempfile::tempdir().expect("listener cwd");
        let cli = Cli::try_parse_from([
            "rpi",
            "-C",
            project.path().to_str().expect("project utf8"),
            "--api-key",
            "test-key",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
        ])
        .expect("cli");
        let todo_child_service = TodoChildService::new();
        let spawner = RunSessionSpawner::from_startup_with_service(
            &cli,
            SessionStorage::ExplicitRoot(custom_root.path().to_path_buf()),
            todo_child_service.clone(),
        );
        let (source_model, _source_faux) = registered_faux_model("fresh-same-cwd-explicit");
        let (source, _source_agent_dir) =
            fork_source_application(project.path(), &source_model).await;
        let directory =
            pi_coding::prepare_operator_directory(project.path()).expect("same cwd directory");
        let prepared = spawner
            .prepare_new_session(&source, directory, None)
            .await
            .expect("prepare same-cwd fresh on explicit root");
        assert_eq!(prepared.cwd, project.path());
        assert_eq!(prepared.candidate.session.cwd(), project.path());
        assert_eq!(
            prepared.candidate.session.session_dir(),
            custom_root.path(),
            "same-cwd New must remain in the listener ExplicitRoot store",
        );
        source.cleanup().await;
        todo_child_service
            .shutdown()
            .await
            .expect("shutdown Todo service");
    }


    #[tokio::test]
    async fn fresh_from_default_tree_source_keeps_native_encoded_placement() {
        let project_a = tempfile::tempdir().expect("project A target");
        let project_b = tempfile::tempdir().expect("project B source");
        let cli = Cli::try_parse_from([
            "rpi",
            "-C",
            project_b.path().to_str().expect("project B utf8"),
            "--api-key",
            "test-key",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
        ])
        .expect("cli");
        let todo_child_service = TodoChildService::new();
        // Production startup storage for a default-tree source process.
        let spawner = RunSessionSpawner::from_startup_with_service(
            &cli,
            SessionStorage::default_tree(),
            todo_child_service.clone(),
        );
        // Production sources always carry a live authentication owner; the
        // fresh child snapshot inherits it (build_session fails closed
        // otherwise). fork_source_application is the existing hermetic source
        // fixture with that contract.
        let (source_model, _source_faux) = registered_faux_model("fresh-default-source");
        let (source, _source_agent_dir) =
            fork_source_application(project_b.path(), &source_model).await;
        let directory =
            pi_coding::prepare_operator_directory(project_a.path()).expect("target directory");
        let prepared = spawner
            .prepare_new_session(&source, directory, None)
            .await
            .expect("prepare fresh from default-tree source");

        // A default-tree source stays on the process-native tree, encoded for
        // the target cwd; it must not drift to the source project's encoded
        // tree or lose its cwd.
        let expected_target_tree = pi_coding::native_sessions_root().join(
            pi_coding::default_session_dir(project_a.path())
                .file_name()
                .expect("target project component"),
        );
        assert_eq!(prepared.cwd, project_a.path());
        assert_eq!(prepared.candidate.session.cwd(), project_a.path());
        assert_eq!(prepared.candidate.session.session_dir(), expected_target_tree);
        assert_ne!(
            prepared.candidate.session.session_dir(),
            pi_coding::native_sessions_root().join(
                pi_coding::default_session_dir(project_b.path())
                    .file_name()
                    .expect("source project component"),
            ),
        );

        source.cleanup().await;
        todo_child_service
            .shutdown()
            .await
            .expect("shutdown Todo service");
    }

    #[tokio::test]
    async fn manager_spawner_reopens_fresh_session_recording() {
        let native_root = tempfile::tempdir().expect("native root");
        let project = tempfile::tempdir().expect("project");
        let agent = tempfile::tempdir().expect("agent dir");
        fs::write(
            agent.path().join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )
        .expect("isolated sandbox settings");
        let (model, _faux) = registered_faux_model("manager-open-resumed");
        let source = hermetic_application(
            SessionOptions {
                model: model.clone(),
                cwd: project.path().to_path_buf(),
                system_prompt: String::new(),
                thinking_level: ThinkingLevel::Off,
                api_key: String::new(),
                compaction: None,
                stream_options: SimpleStreamOptions::default(),
                tools: Some(Vec::new()),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn: None,
                auth_resolver: None,
                auth_manager: Some(Arc::new(
                    AuthManager::new(agent.path().join("auth.json")).expect("auth manager"),
                )),
            },
            agent.path(),
        )
        .await;
        let recorder = pi_coding::start_session_in(
            project.path(),
            Some(&model),
            Some("off"),
            Some(native_root.path()),
            None,
            None,
        )
        .expect("source recording");
        recorder
            .record_message(&pi_ai::Message::user_text("retained history", 0))
            .expect("record source message");
        recorder.persist_now().expect("persist source");
        let source_path = recorder.path();
        source
            .session()
            .record(recorder)
            .expect("attach source recording");

        let cli = Cli::try_parse_from([
            "rpi",
            "--api-key",
            "test-key",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
        ])
        .expect("cli");
        let todo_child_service = TodoChildService::new();
        let spawner = RunSessionSpawner::from_startup_with_service(
            &cli,
            SessionStorage::ExplicitRoot(native_root.path().to_path_buf()),
            todo_child_service.clone(),
        );
        let prepared =
            pi_coding::PreparedSessionResume::prepare_path(&source_path).expect("prepare source");
        let reopened = spawner
            .open_resumed(&source, prepared)
            .await
            .unwrap_or_else(|error| panic!("production resumed-session spawn failed: {error:#}"));
        assert_eq!(
            reopened.session_id,
            source.session().recorder_info().expect("source id").0
        );
        let snapshot = reopened.application.protocol_snapshot().await;
        assert!(
            format!("{:?}", snapshot.messages).contains("retained history"),
            "reopened runtime must retain the authoritative transcript",
        );
        reopened.application.cleanup().await;
        source.cleanup().await;
    }

    #[tokio::test]
    async fn open_resumed_restores_journal_thinking_level() {
        let native_root = tempfile::tempdir().expect("native root");
        let project = tempfile::tempdir().expect("project");
        let agent = tempfile::tempdir().expect("agent dir");
        fs::write(
            agent.path().join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )
        .expect("isolated sandbox settings");
        let suffix = uuid::Uuid::now_v7().simple();
        let mut model = Model::default();
        model.id = "open-resume-thinking".to_owned();
        model.name = "open-resume-thinking".to_owned();
        model.api = format!("open-resume-thinking-{suffix}-api");
        model.provider = format!("open-resume-thinking-{suffix}-provider");
        model.base_url = "http://localhost:0".to_owned();
        model.reasoning = true;
        let _faux = register_faux_provider(FauxProviderOptions {
            api: model.api.clone(),
            provider: model.provider.clone(),
            models: vec![model.clone()],
            chunk_size: 12,
        });
        let source = hermetic_application(
            SessionOptions {
                model: model.clone(),
                cwd: project.path().to_path_buf(),
                system_prompt: String::new(),
                thinking_level: ThinkingLevel::Medium,
                api_key: String::new(),
                compaction: None,
                stream_options: SimpleStreamOptions::default(),
                tools: Some(Vec::new()),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn: None,
                auth_resolver: None,
                auth_manager: Some(Arc::new(
                    AuthManager::new(agent.path().join("auth.json")).expect("auth manager"),
                )),
            },
            agent.path(),
        )
        .await;
        let recorder = pi_coding::start_session_in(
            project.path(),
            Some(&model),
            Some("high"),
            Some(native_root.path()),
            None,
            None,
        )
        .expect("source recording");
        recorder
            .record_message(&pi_ai::Message::user_text("rm-v-persist", 0))
            .expect("record source message");
        recorder.persist_now().expect("persist source");
        let source_path = recorder.path();
        source
            .session()
            .record(recorder)
            .expect("attach source recording");

        let cli = Cli::try_parse_from([
            "rpi",
            "--api-key",
            "test-key",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
        ])
        .expect("cli");
        let todo_child_service = TodoChildService::new();
        let spawner = RunSessionSpawner::from_startup_with_service(
            &cli,
            SessionStorage::ExplicitRoot(native_root.path().to_path_buf()),
            todo_child_service.clone(),
        );
        let prepared =
            pi_coding::PreparedSessionResume::prepare_path(&source_path).expect("prepare source");
        assert!(prepared.tree().has_thinking_entry());
        assert_eq!(prepared.build_context().thinking_level, "high");
        let reopened = spawner
            .open_resumed(&source, prepared)
            .await
            .unwrap_or_else(|error| panic!("production resumed-session spawn failed: {error:#}"));
        assert_eq!(
            reopened.application.session().thinking_level(),
            ThinkingLevel::High,
            "cold IsolatedApplication resume must restore journal thinking, not the source default"
        );
        reopened.application.cleanup().await;
        source.cleanup().await;
    }

    /// /web regression: a thinking level set on a LIVE recorded runtime must
    /// survive an unload→reload cycle. The user adjusts the level on /web, it
    /// appears applied, then reverts — reproduce by setting the level through
    /// the same Application entry the `set_thinking_level` RPC uses, closing
    /// the runtime, and reopening it through the production `open_resumed`
    /// spawn path.
    #[tokio::test]
    async fn open_resumed_persists_live_thinking_level_change() {
        let native_root = tempfile::tempdir().expect("native root");
        let project = tempfile::tempdir().expect("project");
        let agent = tempfile::tempdir().expect("agent dir");
        fs::write(
            agent.path().join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )
        .expect("isolated sandbox settings");
        let suffix = uuid::Uuid::now_v7().simple();
        let mut model = Model::default();
        model.id = "live-thinking-persist".to_owned();
        model.name = "live-thinking-persist".to_owned();
        model.api = format!("live-thinking-persist-{suffix}-api");
        model.provider = format!("live-thinking-persist-{suffix}-provider");
        model.base_url = "http://localhost:0".to_owned();
        model.reasoning = true;
        let _faux = register_faux_provider(FauxProviderOptions {
            api: model.api.clone(),
            provider: model.provider.clone(),
            models: vec![model.clone()],
            chunk_size: 12,
        });
        let source = hermetic_application(
            SessionOptions {
                model: model.clone(),
                cwd: project.path().to_path_buf(),
                system_prompt: String::new(),
                thinking_level: ThinkingLevel::Off,
                api_key: String::new(),
                compaction: None,
                stream_options: SimpleStreamOptions::default(),
                tools: Some(Vec::new()),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn: None,
                auth_resolver: None,
                auth_manager: Some(Arc::new(
                    AuthManager::new(agent.path().join("auth.json")).expect("auth manager"),
                )),
            },
            agent.path(),
        )
        .await;
        let recorder = pi_coding::start_session_in(
            project.path(),
            Some(&model),
            Some("off"),
            Some(native_root.path()),
            None,
            None,
        )
        .expect("source recording");
        recorder
            .record_message(&pi_ai::Message::user_text("live-thinking-set", 0))
            .expect("record source message");
        recorder.persist_now().expect("persist source");
        let source_path = recorder.path();
        source
            .session()
            .record(recorder)
            .expect("attach source recording");

        // The /web set_thinking_level RPC path.
        let change = source.set_thinking_level(ThinkingLevel::High);
        assert!(!change.clamped, "{}", change.message);
        assert_eq!(source.session().thinking_level(), ThinkingLevel::High);

        let disk = pi_coding::load_session_tree(&source_path).expect("reload live journal");
        assert!(
            disk.has_thinking_entry(),
            "live set_thinking_level must record a thinking_level_change entry"
        );
        assert_eq!(
            disk.build_context(None).thinking_level,
            "high",
            "the live journal must carry the newly selected level"
        );

        source.cleanup().await;
        let cli = Cli::try_parse_from([
            "rpi",
            "--api-key",
            "test-key",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
        ])
        .expect("cli");
        let todo_child_service = TodoChildService::new();
        let spawner = RunSessionSpawner::from_startup_with_service(
            &cli,
            SessionStorage::ExplicitRoot(native_root.path().to_path_buf()),
            todo_child_service.clone(),
        );
        let prepared =
            pi_coding::PreparedSessionResume::prepare_path(&source_path).expect("prepare source");
        let reopened = spawner
            .open_resumed(&source, prepared)
            .await
            .unwrap_or_else(|error| panic!("production resumed-session spawn failed: {error:#}"));
        assert_eq!(
            reopened.application.session().thinking_level(),
            ThinkingLevel::High,
            "thinking level set on a live /web session must survive unload→reload"
        );
        reopened.application.cleanup().await;
        source.cleanup().await;
    }

    /// Regression: opening a saved session whose recorded cwd was deleted
    /// (temp dirs, workflow worktrees) must succeed, recreate the directory,
    /// and open the runtime IN the recorded cwd — never fall back to the
    /// source runtime's cwd.
    #[tokio::test]
    async fn open_resumed_recreates_deleted_recorded_cwd() {
        let native_root = tempfile::tempdir().expect("native root");
        let root = tempfile::tempdir().expect("root");
        let recorded_cwd = root.path().join("recorded-project");
        fs::create_dir_all(&recorded_cwd).expect("recorded cwd");
        let source_project = root.path().join("source-project");
        fs::create_dir_all(&source_project).expect("source cwd");
        let agent = root.path().join("agent");
        fs::create_dir_all(&agent).expect("agent dir");
        fs::write(
            agent.join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )
        .expect("isolated sandbox settings");
        let (model, _faux) = registered_faux_model("open-heal-deleted-cwd");
        let source = hermetic_application(
            SessionOptions {
                model: model.clone(),
                cwd: source_project.clone(),
                system_prompt: String::new(),
                thinking_level: ThinkingLevel::Off,
                api_key: String::new(),
                compaction: None,
                stream_options: SimpleStreamOptions::default(),
                tools: Some(Vec::new()),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn: None,
                auth_resolver: None,
                auth_manager: Some(Arc::new(
                    AuthManager::new(agent.join("auth.json")).expect("auth manager"),
                )),
            },
            &agent,
        )
        .await;
        let recorder = pi_coding::start_session_in(
            &recorded_cwd,
            Some(&model),
            Some("off"),
            Some(native_root.path()),
            None,
            None,
        )
        .expect("recorded session");
        recorder
            .record_message(&pi_ai::Message::user_text("history from lost cwd", 0))
            .expect("record message");
        recorder.persist_now().expect("persist recorded session");
        let session_file = recorder.path().to_path_buf();
        drop(recorder);

        // The recorded working directory disappears before the open.
        fs::remove_dir_all(&recorded_cwd).expect("delete recorded cwd");

        let cli = Cli::try_parse_from([
            "rpi",
            "--api-key",
            "test-key",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
        ])
        .expect("cli");
        let todo_child_service = TodoChildService::new();
        let spawner = RunSessionSpawner::from_startup_with_service(
            &cli,
            SessionStorage::ExplicitRoot(native_root.path().to_path_buf()),
            todo_child_service.clone(),
        );
        let prepared = pi_coding::PreparedSessionResume::prepare_path(&session_file)
            .expect("prepare recorded session");
        let reopened = spawner
            .open_resumed(&source, prepared)
            .await
            .unwrap_or_else(|error| panic!("healed open must succeed: {error:#}"));
        assert!(
            recorded_cwd.is_dir(),
            "the deleted recorded cwd must be recreated by the open"
        );
        assert_eq!(
            reopened.application.session().cwd(),
            recorded_cwd,
            "resumed runtime must run in the recorded cwd, not the source cwd"
        );
        let snapshot = reopened.application.protocol_snapshot().await;
        assert!(
            format!("{:?}", snapshot.messages).contains("history from lost cwd"),
            "healed open must retain the recorded transcript"
        );
        reopened.application.cleanup().await;
        source.cleanup().await;
        todo_child_service.shutdown().await.expect("shutdown Todo service");
    }

    /// A recorded cwd whose PARENT is gone is never resurrected and never
    /// bound: the open falls back to the source runtime's cwd (pre-existing
    /// bounded-open behavior) and the missing path stays absent.
    #[tokio::test]
    async fn open_resumed_with_missing_parent_cwd_falls_back_without_creation() {
        let native_root = tempfile::tempdir().expect("native root");
        let root = tempfile::tempdir().expect("root");
        let orphan_cwd = root.path().join("absent-parent").join("orphan-project");
        fs::create_dir_all(&orphan_cwd).expect("orphan cwd");
        let source_project = root.path().join("source-project");
        fs::create_dir_all(&source_project).expect("source cwd");
        let agent = root.path().join("agent");
        fs::create_dir_all(&agent).expect("agent dir");
        fs::write(
            agent.join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )
        .expect("isolated sandbox settings");
        let (model, _faux) = registered_faux_model("open-heal-orphan-cwd");
        let source = hermetic_application(
            SessionOptions {
                model: model.clone(),
                cwd: source_project.clone(),
                system_prompt: String::new(),
                thinking_level: ThinkingLevel::Off,
                api_key: String::new(),
                compaction: None,
                stream_options: SimpleStreamOptions::default(),
                tools: Some(Vec::new()),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn: None,
                auth_resolver: None,
                auth_manager: Some(Arc::new(
                    AuthManager::new(agent.join("auth.json")).expect("auth manager"),
                )),
            },
            &agent,
        )
        .await;
        let recorder = pi_coding::start_session_in(
            &orphan_cwd,
            Some(&model),
            Some("off"),
            Some(native_root.path()),
            None,
            None,
        )
        .expect("orphan recorded session");
        recorder
            .record_message(&pi_ai::Message::user_text("orphan history", 0))
            .expect("record message");
        recorder.persist_now().expect("persist orphan session");
        let session_file = recorder.path().to_path_buf();
        drop(recorder);

        // The whole parent tree disappears before the open.
        fs::remove_dir_all(orphan_cwd.parent().expect("orphan parent"))
            .expect("delete orphan parent");

        let cli = Cli::try_parse_from([
            "rpi",
            "--api-key",
            "test-key",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
        ])
        .expect("cli");
        let todo_child_service = TodoChildService::new();
        let spawner = RunSessionSpawner::from_startup_with_service(
            &cli,
            SessionStorage::ExplicitRoot(native_root.path().to_path_buf()),
            todo_child_service.clone(),
        );
        let prepared = pi_coding::PreparedSessionResume::prepare_path(&session_file)
            .expect("prepare orphan session");
        let reopened = spawner
            .open_resumed(&source, prepared)
            .await
            .unwrap_or_else(|error| panic!("fallback open must succeed: {error:#}"));
        assert!(
            !orphan_cwd.exists(),
            "missing parent trees must never be resurrected by the open"
        );
        assert_eq!(
            reopened.application.session().cwd(),
            source_project,
            "unhealable recorded cwd must not bind: fall back to the source cwd"
        );
        reopened.application.cleanup().await;
        source.cleanup().await;
        todo_child_service.shutdown().await.expect("shutdown Todo service");
    }

    /// New-session creation with a user-typed target cwd keeps its explicit
    /// bounded failure: no heal applies outside saved-session opens.
    #[test]
    fn new_session_nonexistent_cwd_still_fails_bounded() {
        let root = tempfile::tempdir().expect("root");
        let missing = root.path().join("never-created");
        let cli = Cli::try_parse_from(["rpi", "-C", missing.to_str().expect("utf8 cwd")])
            .expect("cli");
        let error = startup_resource_options(&cli, true)
            .err()
            .expect("nonexistent user-typed cwd must keep failing");
        assert!(
            error.to_string().contains("resolving working directory"),
            "unexpected error for nonexistent new-session cwd: {error:#}"
        );
        assert!(
            !missing.exists(),
            "new-session cwd resolution must never create the target"
        );
    }

    /// The heal is idempotent: an existing cwd is a no-op, and recreating a
    /// missing cwd twice is stable without side effects.
    #[test]
    fn heal_missing_recorded_cwd_is_idempotent() {
        let root = tempfile::tempdir().expect("root");
        let existing = root.path().join("already-here");
        fs::create_dir_all(&existing).expect("existing cwd");
        fs::write(existing.join("marker"), "keep").expect("marker file");
        heal_missing_recorded_cwd(&existing);
        assert!(existing.is_dir());
        assert_eq!(
            fs::read_to_string(existing.join("marker")).expect("marker intact"),
            "keep",
            "heal must not touch an existing cwd"
        );

        // Leaf-only recreation: the parent exists, so the missing leaf is
        // created and a second heal is a stable no-op.
        let parent = root.path().join("parent");
        fs::create_dir_all(&parent).expect("parent cwd");
        let missing = parent.join("gone");
        heal_missing_recorded_cwd(&missing);
        assert!(missing.is_dir(), "first heal recreates the missing leaf");
        heal_missing_recorded_cwd(&missing);
        assert!(missing.is_dir(), "second heal is a stable no-op");

        // Missing parent trees are NEVER resurrected.
        let orphan = root.path().join("absent-parent").join("leaf");
        heal_missing_recorded_cwd(&orphan);
        assert!(
            !orphan.exists(),
            "heal must not create missing parent trees"
        );

        // A symlinked parent is rejected by the no-follow capability check:
        // the leaf is never created through a swapped parent.
        let real_parent = root.path().join("real-parent");
        fs::create_dir_all(&real_parent).expect("real parent");
        let symlink_parent = root.path().join("symlink-parent");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real_parent, &symlink_parent).expect("parent symlink");
        #[cfg(not(unix))]
        {
            let _ = &symlink_parent;
            return;
        }
        let leaf_under_symlink = symlink_parent.join("leaf");
        heal_missing_recorded_cwd(&leaf_under_symlink);
        assert!(
            !leaf_under_symlink.exists(),
            "heal must not create leaves under a symlinked parent"
        );

        // Empty recorded cwd stays a harmless no-op.
        heal_missing_recorded_cwd(Path::new(""));
    }


    #[tokio::test]
    async fn production_manager_spawner_reopen_appends_and_renames_through_retained_writer() {
        let native_root = tempfile::tempdir().expect("native root");
        let project = tempfile::tempdir().expect("project");
        let agent = tempfile::tempdir().expect("agent dir");
        fs::write(
            agent.path().join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )
        .expect("isolated sandbox settings");
        let (model, _faux) = registered_faux_model("manager-restart-writer");
        let source = hermetic_application(
            SessionOptions {
                model: model.clone(),
                cwd: project.path().to_path_buf(),
                system_prompt: String::new(),
                thinking_level: ThinkingLevel::Off,
                api_key: String::new(),
                compaction: None,
                stream_options: SimpleStreamOptions::default(),
                tools: Some(Vec::new()),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn: None,
                auth_resolver: None,
                auth_manager: Some(Arc::new(
                    AuthManager::new(agent.path().join("auth.json")).expect("auth manager"),
                )),
            },
            agent.path(),
        )
        .await;
        let recorder = pi_coding::start_session_in(
            project.path(),
            Some(&model),
            Some("off"),
            Some(native_root.path()),
            Some("restart-writer"),
            None,
        )
        .expect("source recording");
        recorder
            .record_message(&pi_ai::Message::user_text("before restart", 0))
            .expect("source message");
        recorder.persist_now_durable().expect("persist source");
        let source_path = recorder.path();
        drop(recorder);

        let cli = Cli::try_parse_from([
            "rpi",
            "--api-key",
            "test-key",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
        ])
        .expect("cli");
        let todo_child_service = TodoChildService::new();
        let spawner = RunSessionSpawner::from_startup_with_service(
            &cli,
            SessionStorage::ExplicitRoot(native_root.path().to_path_buf()),
            todo_child_service.clone(),
        );
        let reopened = spawner
            .open_resumed(
                &source,
                pi_coding::PreparedSessionResume::prepare_path(&source_path)
                    .expect("prepare source"),
            )
            .await
            .expect("reopen source");
        let writer = reopened
            .application
            .session()
            .recording_writer_info()
            .expect("attached writer");
        assert_eq!(writer.0, "restart-writer");
        assert_eq!(writer.1, source_path);
        reopened
            .application
            .session()
            .append_custom_entry("restart_probe", None)
            .expect("append resumed record");
        reopened
            .application
            .set_session_name("renamed after restart")
            .expect("rename resumed session");
        reopened
            .application
            .cleanup()
            .await
            .expect("cleanup reopened application");

        let tree = pi_coding::load_session_tree(&source_path).expect("reload resumed tree");
        assert_eq!(tree.session_name().as_deref(), Some("renamed after restart"));
        assert!(tree.entries.iter().any(|entry| {
            entry.entry_type == "custom"
                && entry.custom_type.as_deref() == Some("restart_probe")
        }));
        source.cleanup().await;
        todo_child_service.shutdown().await.expect("shutdown Todo service");
    }

    #[test]
    fn profile_precedence_cli_wins_over_env() {
        // CLI --profile beats PI_PROFILE.
        assert_eq!(
            resolve_profile_precedence(Some("cli-work"), Some("env-work")).expect("CLI wins"),
            Some("cli-work".to_owned())
        );
        // Env is used when no flag is present.
        assert_eq!(
            resolve_profile_precedence(None, Some("env-work")).expect("env honored"),
            Some("env-work".to_owned())
        );
        // Neither source selects the default profile.
        assert_eq!(
            resolve_profile_precedence(None, None).expect("no profile"),
            None
        );
        // The flag position does not matter; empty values are filtered before
        // this resolver runs.
        assert_eq!(
            resolve_profile_precedence(Some("work"), Some("")).expect("empty env ignored"),
            Some("work".to_owned())
        );
    }

    #[test]
    fn profile_precedence_default_selects_default_profile() {
        assert_eq!(
            resolve_profile_precedence(Some("default"), None).expect("explicit default"),
            None
        );
        assert_eq!(
            resolve_profile_precedence(None, Some("default")).expect("env default"),
            None
        );
        // The CLI flag wins over the environment in either direction.
        assert_eq!(
            resolve_profile_precedence(Some("work"), Some("default"))
                .expect("named CLI beats env default"),
            Some("work".to_owned())
        );
        assert_eq!(
            resolve_profile_precedence(Some("default"), Some("env-work"))
                .expect("explicit CLI default beats env profile"),
            None
        );
    }

    #[test]
    fn profile_precedence_validates_names_actionably() {
        for (cli, env) in [
            (Some("bad/name"), None),
            (None, Some("bad/name")),
            (Some("with space"), None),
        ] {
            let error =
                resolve_profile_precedence(cli, env).expect_err("invalid profile name must fail");
            let message = format!("{error:#}");
            assert!(
                message.contains("profile name") && message.contains("letters, digits"),
                "error must be actionable, got: {message}"
            );
        }
        // A valid CLI profile shadows an invalid environment value entirely.
        assert_eq!(
            resolve_profile_precedence(Some("x"), Some("bad/name")).expect("CLI wins"),
            Some("x".to_owned())
        );
    }

    #[test]
    fn profile_precedence_accepts_valid_and_max_length_names() {
        let max = "a".repeat(crate::args::MAX_PROFILE_NAME_LENGTH);
        assert_eq!(
            resolve_profile_precedence(Some(&max), None).expect("max length valid"),
            Some(max)
        );
        assert_eq!(
            resolve_profile_precedence(Some("my-profile_2"), None).expect("charset valid"),
            Some("my-profile_2".to_owned())
        );
    }

    #[test]
    fn session_ttl_defaults_to_30_days_and_honors_the_setting() {
        assert_eq!(
            session_ttl_from_settings(&Settings::default()),
            Duration::from_secs(30 * 24 * 60 * 60)
        );
        let mut settings = Settings::default();
        settings.session_ttl_days = Some(7);
        assert_eq!(
            session_ttl_from_settings(&settings),
            Duration::from_secs(7 * 24 * 60 * 60)
        );
        // Zero (rejected by settings validation) defensively falls back to the
        // default rather than pruning everything.
        settings.session_ttl_days = Some(0);
        assert_eq!(
            session_ttl_from_settings(&settings),
            Duration::from_secs(30 * 24 * 60 * 60)
        );
    }

    #[test]
    fn workflow_storage_roots_are_stable_and_session_scoped() {
        fn init_repo(path: &Path) {
            fs::create_dir_all(path).expect("repo directory");
            for args in [
                vec!["init"],
                vec!["config", "user.name", "Pi Test"],
                vec!["config", "user.email", "pi@example.test"],
            ] {
                let status = std::process::Command::new("git")
                    .args(["-c", "init.defaultBranch=main"])
                    .args(args)
                    .current_dir(path)
                    .status()
                    .expect("git command");
                assert!(status.success());
            }
            fs::write(path.join("README.md"), "base\n").expect("base file");
            let status = std::process::Command::new("git")
                .args(["add", "README.md"])
                .current_dir(path)
                .status()
                .expect("git add");
            assert!(status.success());
            let status = std::process::Command::new("git")
                .args(["-c", "commit.gpgsign=false", "commit", "-m", "initial"])
                .current_dir(path)
                .status()
                .expect("git commit");
            assert!(status.success());
        }

        let sandbox = tempfile::tempdir().expect("sandbox");
        let agent_dir = sandbox.path().join("agent");
        let first = sandbox.path().join("first");
        let second = sandbox.path().join("second");
        init_repo(&first);
        init_repo(&second);

        // Same session id resolves to the same roots, deterministically.
        let first_roots = workflow_storage_roots(&first, &agent_dir, "session-a");
        assert_eq!(
            first_roots,
            workflow_storage_roots(&first, &agent_dir, "session-a")
        );
        // Different session ids in the same repository are isolated.
        assert_ne!(
            first_roots,
            workflow_storage_roots(&first, &agent_dir, "session-b")
        );
        // Different repositories keep distinct namespaces.
        assert_ne!(
            first_roots,
            workflow_storage_roots(&second, &agent_dir, "session-a")
        );
        assert!(first_roots.0.starts_with(agent_dir.join("workflows")));
        assert!(
            first_roots
                .1
                .starts_with(agent_dir.join("workflow-worktrees"))
        );

        // Non-git directories still resolve a valid, session-scoped namespace.
        let plain = sandbox.path().join("plain");
        fs::create_dir_all(&plain).expect("plain directory");
        let plain_roots = workflow_storage_roots(&plain, &agent_dir, "session-a");
        assert_eq!(
            plain_roots,
            workflow_storage_roots(&plain, &agent_dir, "session-a")
        );
        assert_ne!(
            plain_roots,
            workflow_storage_roots(&plain, &agent_dir, "session-b")
        );
    }

    #[test]
    fn settings_and_cli_selection_drive_identical_initial_tool_gates() {
        let mut settings = Settings::default();
        settings.orchestration = Some(pi_coding::OrchestrationSettings {
            tasks: Some(true),
            process: Some(true),
            todo: Some(true),
            glob: Some(true),
            ..pi_coding::OrchestrationSettings::default()
        });
        let cli = <Cli as clap::Parser>::try_parse_from(["rpi"]).expect("default cli");
        let gates = blueprint(&cli).test_tool_gates(&settings);
        assert_eq!(gates, (true, true, true, true, true));

        let selected = <Cli as clap::Parser>::try_parse_from([
            "rpi",
            "--tools",
            "task,hub,process,todo,glob,goal",
        ])
        .expect("selected tools");
        let gates = blueprint(&selected).test_tool_gates(&Settings::default());
        assert_eq!(gates, (true, true, true, true, true));

        let excluded = <Cli as clap::Parser>::try_parse_from(["rpi", "--exclude-tools", "goal"])
            .expect("excluded goal tool");
        let gates = blueprint(&excluded).test_tool_gates(&Settings::default());
        assert!(!gates.4);

        let disabled =
            <Cli as clap::Parser>::try_parse_from(["rpi", "--no-tools"]).expect("disabled tools");
        let gates = blueprint(&disabled).test_tool_gates(&settings);
        assert_eq!(gates, (false, false, false, false, false));
    }

    #[test]
    fn goal_catalog_defaults_on_but_no_tools_and_explicit_exclude_win() {
        let cwd = tempfile::tempdir().expect("cwd");
        let options = || SessionOptions {
            model: Model::default(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: SimpleStreamOptions::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        };
        let with_selection = |selection: ToolSelection| {
            Session::new_with_additional_tools_filtered_and_discovery(
                options(),
                vec![GoalToolBinding::default().tool()],
                selection,
                ResourceDiscovery::Disabled,
            )
            .expect("session")
            .get_active_tool_names()
        };

        assert_eq!(with_selection(ToolSelection::default()), ["goal"]);
        assert!(
            with_selection(ToolSelection {
                deny: vec!["goal".to_owned()],
                ..ToolSelection::default()
            })
            .is_empty()
        );
        assert!(
            with_selection(ToolSelection {
                disable_all: true,
                ..ToolSelection::default()
            })
            .is_empty()
        );
    }

    #[test]
    fn settings_thinking_level_is_used_after_cli_model_and_resume_priorities() {
        assert_eq!(
            resolve_initial_thinking_level(None, "", None, false, Some("high")),
            pi_agent::ThinkingLevel::High
        );
        assert_eq!(
            resolve_initial_thinking_level(Some("low"), "", None, false, Some("high")),
            pi_agent::ThinkingLevel::Low
        );
    }

    fn settings_default(provider: Option<&str>, model: Option<&str>) -> Settings {
        let mut settings = Settings::default();
        settings.default_provider = provider.map(str::to_owned);
        settings.default_model = model.map(str::to_owned);
        settings
    }

    fn settings_default_cli() -> Cli {
        Cli::try_parse_from(["rpi", "--api-key", "test-key"]).expect("settings default cli")
    }

    async fn resolve_settings_default(
        provider: Option<&str>,
        model: Option<&str>,
        cli: &Cli,
    ) -> Result<(pi_ai::Model, String, String)> {
        resolve_initial_model(cli, &settings_default(provider, model), None).await
    }

    #[tokio::test]
    async fn settings_default_historical_pair_selects_configured_model_exactly() {
        let (model, _faux) = registered_faux_model("settings-pair");
        let cli = settings_default_cli();
        let (resolved, _key, parsed_think) =
            resolve_settings_default(Some(&model.provider), Some(&model.id), &cli)
                .await
                .expect("historical pair resolves");
        assert_eq!(resolved.provider, model.provider);
        assert_eq!(resolved.id, model.id);
        assert!(
            parsed_think.is_empty(),
            "a bare pair has no thinking suffix"
        );
    }

    #[tokio::test]
    async fn settings_default_combined_spec_resolves_directly_with_provider_agreement() {
        let (model, _faux) = registered_faux_model("settings-combined");
        let cli = settings_default_cli();
        let combined = format!("{}/{}", model.provider, model.id);

        // A combined spec alone carries its own provider.
        let (resolved, _key, parsed_think) = resolve_settings_default(None, Some(&combined), &cli)
            .await
            .expect("combined spec resolves");
        assert_eq!(resolved.provider, model.provider);
        assert_eq!(resolved.id, model.id);
        assert!(parsed_think.is_empty());

        // With defaultProvider also set, the spec's provider must agree.
        let (resolved, _key, _) =
            resolve_settings_default(Some(&model.provider), Some(&combined), &cli)
                .await
                .expect("agreeing provider resolves");
        assert_eq!(resolved.provider, model.provider);
        assert_eq!(resolved.id, model.id);

        let error = resolve_settings_default(Some("some-other-provider"), Some(&combined), &cli)
            .await
            .expect_err("disagreeing provider must fail validation");
        assert!(error.to_string().contains("does not match"), "{error}");
    }

    #[tokio::test]
    async fn settings_default_combined_spec_with_effort_suffix_parses_think() {
        let (model, _faux) = registered_faux_model("settings-suffix");
        let cli = settings_default_cli();
        let combined = format!("{}/{}:high", model.provider, model.id);
        let (resolved, _key, parsed_think) = resolve_settings_default(None, Some(&combined), &cli)
            .await
            .expect("combined spec with suffix resolves");
        assert_eq!(resolved.provider, model.provider);
        assert_eq!(resolved.id, model.id);
        assert_eq!(parsed_think, "high");

        // The historical pair honors the suffix on the bare id too.
        let (resolved, _key, parsed_think) = resolve_settings_default(
            Some(&model.provider),
            Some(&format!("{}:max", model.id)),
            &cli,
        )
        .await
        .expect("bare id with suffix resolves");
        assert_eq!(resolved.id, model.id);
        assert_eq!(parsed_think, "max");
    }

    #[tokio::test]
    async fn settings_default_resolves_case_insensitively() {
        let (model, _faux) = registered_faux_model("settings-case");
        let cli = settings_default_cli();
        let (resolved, _key, _think) = resolve_settings_default(
            Some(&model.provider.to_uppercase()),
            Some(&model.id.to_uppercase()),
            &cli,
        )
        .await
        .expect("case-insensitive default resolves");
        assert_eq!(resolved.provider, model.provider);
        assert_eq!(resolved.id, model.id);
    }

    #[tokio::test]
    async fn settings_default_custom_known_provider_id_uses_resolver_fallback() {
        let (model, _faux) = registered_faux_model("settings-custom");
        let cli = settings_default_cli();
        let (resolved, _key, parsed_think) =
            resolve_settings_default(Some(&model.provider), Some("settings-custom-id:low"), &cli)
                .await
                .expect("custom id falls back through the resolver");
        assert_eq!(resolved.provider, model.provider);
        assert_eq!(resolved.id, "settings-custom-id");
        assert_eq!(parsed_think, "low");
    }

    #[tokio::test]
    async fn settings_default_incomplete_pairs_fail_instead_of_falling_through() {
        let (model, _faux) = registered_faux_model("settings-lone");
        let cli = settings_default_cli();

        let error = resolve_settings_default(Some(&model.provider), None, &cli)
            .await
            .expect_err("a lone defaultProvider must fail");
        assert!(
            error.to_string().contains("requires defaultModel"),
            "{error}"
        );

        let error = resolve_settings_default(None, Some(&model.id), &cli)
            .await
            .expect_err("a lone bare defaultModel must fail");
        assert!(
            error.to_string().contains("requires defaultProvider"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn settings_default_openrouter_provider_resolves_raw_slash_id() {
        let cli = settings_default_cli();
        // OpenRouter-style slash ids are interpreted by the shared resolver,
        // not read as a provider prefix: the whole id stays intact.
        let (resolved, _key, parsed_think) =
            resolve_settings_default(Some("openrouter"), Some("ai21/jamba-large-1.7"), &cli)
                .await
                .expect("openrouter raw slash id resolves");
        assert_eq!(resolved.provider, "openrouter");
        assert_eq!(resolved.id, "ai21/jamba-large-1.7");
        assert!(parsed_think.is_empty());
    }

    #[tokio::test]
    async fn settings_default_slash_bearing_custom_id_under_known_provider_resolves() {
        let (model, _faux) = registered_faux_model("settings-slash-custom");
        let cli = settings_default_cli();
        // The first slash segment does not name a provider, so the value is a
        // custom id under the configured provider, resolved through the
        // resolver's custom-id fallback.
        let (resolved, _key, parsed_think) =
            resolve_settings_default(Some(&model.provider), Some("my/team-custom"), &cli)
                .await
                .expect("slash-bearing custom id resolves");
        assert_eq!(resolved.provider, model.provider);
        assert_eq!(resolved.id, "my/team-custom");
        assert!(parsed_think.is_empty());
    }

    #[tokio::test]
    async fn settings_default_unknown_combined_prefix_fails_at_startup() {
        let cli = settings_default_cli();
        // Startup runs the catalog-ready authority: an unknown combined
        // prefix must fail, never silently fall through.
        let error = resolve_settings_default(None, Some("ghost/nowhere"), &cli)
            .await
            .expect_err("unknown combined prefix must fail startup");
        let message = format!("{error:#}");
        assert!(message.contains("ghost/nowhere"), "{message}");
    }

    #[tokio::test]
    async fn settings_default_without_auth_fails_closed_naming_configured_model() {
        let (unauth, _faux) = registered_faux_model("settings-unauth-default");
        let (authed, _faux) = registered_faux_model("settings-authed-other");
        crate::models_config::set_runtime_api_key(&authed.provider, "other-key");
        let cli = Cli::try_parse_from(["rpi"]).expect("cli without --api-key");

        // Another provider IS authenticated, but a configured default that
        // cannot authenticate must fail startup naming the configured model
        // instead of silently selecting the other one.
        let error = resolve_initial_model(
            &cli,
            &settings_default(Some(&unauth.provider), Some(&unauth.id)),
            None,
        )
        .await
        .expect_err("unauthenticated configured default must fail closed");
        let message = format!("{error:#}");
        assert!(
            message.contains("not available for the resolved credential"),
            "{message}"
        );
        assert!(
            message.contains(&unauth.id),
            "error must name the configured model: {message}"
        );
        assert!(
            !message.contains(&authed.id),
            "error must never point at the fallback model: {message}"
        );
        crate::models_config::clear_runtime_api_key(&authed.provider);
    }

    #[tokio::test]
    async fn settings_default_surrounding_whitespace_resolves_equivalently() {
        let (model, _faux) = registered_faux_model("settings-spaced");
        let cli = settings_default_cli();
        let spaced_provider = format!("  {}  ", model.provider);
        let spaced_model = format!("  {}  ", model.id);
        let settings = settings_default(Some(&spaced_provider), Some(&spaced_model));
        let (resolved, _key, parsed_think) = resolve_initial_model(&cli, &settings, None)
            .await
            .expect("whitespace-padded default resolves like the trimmed view");
        assert_eq!(resolved.provider, model.provider);
        assert_eq!(resolved.id, model.id);
        assert!(parsed_think.is_empty());
        // Trimmed views never rewrite the persisted strings.
        assert_eq!(
            settings.default_provider.as_deref(),
            Some(spaced_provider.as_str())
        );
        assert_eq!(
            settings.default_model.as_deref(),
            Some(spaced_model.as_str())
        );
    }

    #[tokio::test]
    async fn explicit_cli_model_outranks_settings_default() {
        let (settings_model, _faux) = registered_faux_model("settings-rank-default");
        let (cli_model, _faux) = registered_faux_model("settings-rank-cli");
        let cli = Cli::try_parse_from([
            "rpi",
            "--api-key",
            "test-key",
            "--model",
            &format!("{}/{}", cli_model.provider, cli_model.id),
        ])
        .expect("cli");
        let (resolved, _key, _think) = resolve_settings_default(
            Some(&settings_model.provider),
            Some(&settings_model.id),
            &cli,
        )
        .await
        .expect("explicit model wins");
        assert_eq!(resolved.provider, cli_model.provider);
        assert_eq!(resolved.id, cli_model.id);
    }

    #[tokio::test]
    async fn initial_cli_orchestration_inherits_selected_parent_model() {
        let artifacts = tempfile::tempdir().expect("artifacts root");
        let (parent_model, _faux) = registered_faux_model("selected-parent");
        let agent_dir = tempfile::tempdir().expect("agent root");
        fs::create_dir_all(agent_dir.path().join("agents")).expect("agents directory");
        fs::write(
            agent_dir.path().join("agents").join("task.md"),
            "---\nname: task\ndescription: bundled task agent\ntools: []\nthinkingLevel: off\n---\ndo the task\n",
        )
        .expect("task agent");
        let mut resource_options = ResourceManagerOptions::new(artifacts.path());
        resource_options.agent_dir = agent_dir.path().to_path_buf();
        resource_options.project_trust_override = Some(true);
        resource_options.disable_extensions = true;
        resource_options.disable_skills = true;
        resource_options.disable_prompt_templates = true;
        resource_options.disable_themes = true;
        resource_options.disable_context_files = true;
        let resources = ResourceManager::new(resource_options).expect("resources");
        let snapshot = resources.snapshot();
        // Empty settings.agents → no per-agent model override.
        let settings = Settings::default();
        let config = crate::session_run_blueprint::test_orchestration_config(
            &snapshot,
            &settings,
            &parent_model,
        );
        assert_eq!(config.parent_model.provider, parent_model.provider);
        assert_eq!(config.parent_model.id, parent_model.id);

        let seen = Arc::new(Mutex::new(None::<Model>));
        let seen_factory = seen.clone();
        let child_cwd = artifacts.path().to_path_buf();
        let factory: ChildSessionFactory = Arc::new(move |request| {
            let seen_factory = seen_factory.clone();
            let child_cwd = child_cwd.clone();
            Box::pin(async move {
                *seen_factory.lock() = Some(request.model.clone());
                // Debug must never surface secret material from the request.
                let debug = format!("{request:?}");
                assert!(
                    !debug.contains("sk-") && !debug.to_ascii_lowercase().contains("api_key"),
                    "ChildSessionRequest debug leaked secrets: {debug}"
                );
                Ok(hermetic_session(SessionOptions {
                    model: request.model,
                    cwd: child_cwd,
                    system_prompt: request.system_prompt,
                    thinking_level: request.thinking_level.unwrap_or(ThinkingLevel::Off),
                    api_key: String::new(),
                    compaction: None,
                    stream_options: SimpleStreamOptions::default(),
                    tools: Some(Vec::new()),
                    before_tool_call: None,
                    after_tool_call: None,
                    stream_fn: None,
                    auth_resolver: None,
                    auth_manager: None,
                }))
            })
        });

        let runtime = OrchestrationRuntime::new(config, factory).expect("runtime");
        let _ = runtime
            .spawn_tasks(
                "Main",
                0,
                vec![TaskItem {
                    index: 0,
                    id: "Child".to_owned(),
                    agent: "task".to_owned(),
                    assignment: "inherit parent model".to_owned(),
                    todo_task_id: None,
                    ..Default::default()
                }],
            )
            .expect("spawn");
        for _ in 0..50 {
            if seen.lock().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let captured = seen
            .lock()
            .clone()
            .expect("factory observed ChildSessionRequest.model");
        assert_eq!(captured.provider, parent_model.provider);
        assert_eq!(captured.id, parent_model.id);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn bind_parent_session_binds_and_recovers_existing_sidecar() {
        let artifacts = tempfile::tempdir().expect("artifacts");
        let recording_root = tempfile::tempdir().expect("session store");
        let (test_model, _faux) = registered_faux_model("durable-bind-test");
        // Create a parent session with a recorder.
        let recorder = pi_coding::start_session_in(
            recording_root.path(),
            Some(&test_model),
            None,
            Some(artifacts.path()),
            None,
            None,
        )
        .expect("parent recorder");
        let parent = hermetic_session(pi_coding::SessionOptions {
            model: test_model.clone(),
            cwd: artifacts.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        });
        parent.set_session_dir(recording_root.path().to_path_buf());
        parent.record(recorder).expect("record");
        let parent_agent = tempfile::tempdir().expect("parent agent dir");
        fs::write(
            parent_agent.path().join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )
        .expect("parent sandbox settings");
        parent
            .attach_resources(isolated_resource_manager(
                artifacts.path(),
                parent_agent.path(),
            ))
            .await
            .expect("attach parent resources");

        let factory: ChildSessionFactory =
            Arc::new(|_| Box::pin(async { Err(anyhow!("test factory should not be called")) }));
        let mut config = pi_coding::OrchestrationConfig::new(
            pi_coding::AgentCatalog::from_agents(vec![pi_coding::AgentDefinition {
                name: "task".to_owned(),
                description: "task".to_owned(),
                system_prompt: "task".to_owned(),
                tools: Some(Vec::new()),
                autoload_skills: Vec::new(),
                model: None,
                thinking_level: Some(ThinkingLevel::Off),
                max_turns: None,
                max_tool_calls: None,
                timeout_secs: None,
                disallowed_tools: Vec::new(),
                capability_ceiling: None,
                source: pi_coding::AgentDefinitionSource::Bundled,
                path: None,
                trusted: true,
                kind: pi_coding::AgentDefinitionKind::Agent,
                personality: None,
                soft_budget: None,
                knowledge_roots: Vec::new(),
                user_proxy: false,
            }]),
            artifacts.path(),
        );
        config.parent_model = test_model;
        config.idle_ttl = None;
        let runtime = OrchestrationRuntime::new(config, factory).expect("runtime");
        let bootstrap_root = tempfile::tempdir().expect("bootstrap root");
        let bootstrap_cwd = bootstrap_root.path().join("cwd");
        let bootstrap_agent = bootstrap_root.path().join("agent");
        let bootstrap_sessions = bootstrap_root.path().join("sessions");
        for directory in [&bootstrap_cwd, &bootstrap_agent, &bootstrap_sessions] {
            fs::create_dir_all(directory).expect("bootstrap fixture directory");
        }
        fs::write(
            bootstrap_agent.join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )
        .expect("bootstrap sandbox settings");
        let (bootstrap_model, _bootstrap_faux) = registered_faux_model("durable-bootstrap");
        let bootstrap = hermetic_session(SessionOptions {
            model: bootstrap_model,
            cwd: bootstrap_cwd.clone(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        });
        bootstrap.set_session_dir(bootstrap_sessions);
        bootstrap
            .start_new_recording()
            .expect("bootstrap recording");
        bootstrap
            .attach_resources(isolated_resource_manager(&bootstrap_cwd, &bootstrap_agent))
            .await
            .expect("bootstrap resources");
        let bootstrap_application = Application::from_runtime_candidate(
            pi_coding::ApplicationRuntimeCandidate::new(bootstrap)
                .with_orchestration(runtime.clone()),
        )
        .await
        .expect("bootstrap application");

        runtime
            .bind_parent_session(&parent)
            .expect("bind target parent");
        assert!(runtime.is_durable());
        assert!(
            runtime.recover().is_err(),
            "plain bind must not create or overwrite missing durable state"
        );
        runtime
            .recover_or_initialize()
            .expect("initialize fresh durable state");
        runtime.recover().expect("recover initialized state");
        bootstrap_application.cleanup().await;
    }

    #[tokio::test]
    async fn bind_parent_session_fails_without_recorder() {
        let artifacts = tempfile::tempdir().expect("artifacts");
        let (parent_model, _faux) = registered_faux_model("recorderless-parent");
        let parent = hermetic_session(pi_coding::SessionOptions {
            model: parent_model,
            cwd: artifacts.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        });
        // No recorder attached — bind should fail.
        let factory: ChildSessionFactory =
            Arc::new(|_| Box::pin(async { Err(anyhow!("should not be called")) }));
        let config = pi_coding::OrchestrationConfig::new(
            pi_coding::AgentCatalog::from_agents(vec![pi_coding::AgentDefinition {
                name: "task".to_owned(),
                description: "task".to_owned(),
                system_prompt: "task".to_owned(),
                tools: Some(Vec::new()),
                autoload_skills: Vec::new(),
                model: None,
                thinking_level: Some(ThinkingLevel::Off),
                max_turns: None,
                max_tool_calls: None,
                timeout_secs: None,
                disallowed_tools: Vec::new(),
                capability_ceiling: None,
                source: pi_coding::AgentDefinitionSource::Bundled,
                path: None,
                trusted: true,
                kind: pi_coding::AgentDefinitionKind::Agent,
                personality: None,
                soft_budget: None,
                knowledge_roots: Vec::new(),
                user_proxy: false,
            }]),
            artifacts.path(),
        );
        let runtime = OrchestrationRuntime::new(config, factory).expect("runtime");
        let error = runtime
            .bind_parent_session(&parent)
            .expect_err("bind without recorder should fail");
        assert!(error.to_string().contains("recording is unavailable"));
        assert!(!runtime.is_durable());
        runtime.shutdown().await;
    }

    #[test]
    fn durable_resume_degradation_requires_typed_error() {
        let typed = anyhow::Error::new(pi_coding::DurableResumeError::State(
            pi_coding::DurableStateValidationError::Policy(
                pi_sandbox::PolicyValidationError::InvalidField {
                    field: "workspaceRoots",
                    reason: "current platform or session roots do not match persisted authority",
                },
            ),
        ))
        .context("resume orchestration");
        assert!(durable_resume_error(&typed).is_some());

        for message in [
            "durable orchestration policy does not match resumed generation",
            "current workspace identities do not match persisted authority",
            "current platform or session roots do not match persisted authority",
        ] {
            assert!(
                durable_resume_error(&anyhow!(message)).is_none(),
                "message text alone must not trigger degradation: {message}"
            );
        }
    }

    #[test]
    fn durable_resume_degrades_on_wrapped_validation_without_resume_error_type() {
        // A map_err/context chain can drop the DurableResumeError wrapper
        // while keeping the typed validation cause. The validation type is
        // the degradation contract: the switch must degrade, never hard-fail.
        let wrapped = anyhow::Error::new(pi_coding::DurableStateValidationError::Policy(
            pi_sandbox::PolicyValidationError::InvalidField {
                field: "snapshot.policy.filesystem",
                reason: "current platform or session roots do not match persisted authority",
            },
        ))
        .context("resume orchestration");
        assert!(
            pi_coding::DurableResumeError::find_in_chain(&wrapped).is_none(),
            "precondition: the resume-error type must be absent from this chain"
        );
        let degraded = durable_resume_error(&wrapped);
        assert!(degraded.is_some(), "wrapped validation must still degrade");

        // Non-durable errors keep failing the switch (no false degradation).
        let unrelated = anyhow::Error::new(std::io::Error::other("disk full")).context("resume");
        assert!(durable_resume_error(&unrelated).is_none());
    }

    #[test]
    fn durable_resume_notice_renders_only_root_cause() {
        let error = anyhow::Error::new(pi_coding::DurableResumeError::State(
            pi_coding::DurableStateValidationError::Policy(
                pi_sandbox::PolicyValidationError::InvalidField {
                    field: "workspaceRoots",
                    reason: "current platform or session roots do not match persisted authority",
                },
            ),
        ))
        .context("resume orchestration");
        let notice = durable_resume_notice(&error);

        assert!(notice.contains(
            "invalid field workspaceRoots: current platform or session roots do not match persisted authority"
        ));
        assert!(!notice.contains("resume orchestration"), "{notice}");
        assert!(
            !notice.contains("durable state validation failed"),
            "{notice}"
        );
        assert!(
            !notice.contains("durable policy snapshot is invalid"),
            "{notice}"
        );
    }

    fn child_root_for(parent_path: &Path, parent_id: &str) -> PathBuf {
        parent_path
            .parent()
            .expect("parent dir")
            .join("children")
            .join(parent_id)
    }

    fn test_orchestration_runtime(artifacts: &Path, parent_model: Model) -> OrchestrationRuntime {
        let factory: ChildSessionFactory =
            Arc::new(|_| Box::pin(async { Err(anyhow!("test factory should not be called")) }));
        let mut config = pi_coding::OrchestrationConfig::new(
            pi_coding::AgentCatalog::from_agents(vec![pi_coding::AgentDefinition {
                name: "task".to_owned(),
                description: "task".to_owned(),
                system_prompt: "task".to_owned(),
                tools: Some(Vec::new()),
                autoload_skills: Vec::new(),
                model: None,
                thinking_level: Some(ThinkingLevel::Off),
                max_turns: None,
                max_tool_calls: None,
                timeout_secs: None,
                disallowed_tools: Vec::new(),
                capability_ceiling: None,
                source: pi_coding::AgentDefinitionSource::Bundled,
                path: None,
                trusted: true,
                kind: pi_coding::AgentDefinitionKind::Agent,
                personality: None,
                soft_budget: None,
                knowledge_roots: Vec::new(),
                user_proxy: false,
            }]),
            artifacts,
        );
        config.parent_model = parent_model;
        config.idle_ttl = None;
        OrchestrationRuntime::new(config, factory).expect("runtime")
    }

    #[tokio::test]
    async fn application_from_session_candidate_transfers_orchestration_owner() {
        let artifacts = tempfile::tempdir().expect("artifacts");
        let sessions = tempfile::tempdir().expect("sessions");
        let agent = tempfile::tempdir().expect("agent dir");
        let (test_model, _faux) = registered_faux_model("orchestration-owner-transfer");
        let session = hermetic_session(SessionOptions {
            model: test_model.clone(),
            cwd: artifacts.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        });
        session.set_session_dir(sessions.path().to_path_buf());
        session.start_new_recording().expect("parent recording");
        fs::write(
            agent.path().join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )
        .expect("sandbox settings");
        session
            .attach_resources(isolated_resource_manager(artifacts.path(), agent.path()))
            .await
            .expect("attach resources");

        let orchestration = test_orchestration_runtime(artifacts.path(), test_model);
        let observation = orchestration.clone();
        let candidate = crate::session_run_blueprint::RunSessionCandidate {
            session,
            extension_runtime: pi_coding::ExtensionRuntime::process(
                None,
                pi_coding::ExtensionRuntimeOptions::default(),
            ),
            extension_permissions: pi_coding::ExtensionPermissionSet::deny_all(),
            orchestration: Some(orchestration),
            goal_tool: None,
            todo_child_tool: None,
        };
        let (application, notice) = application_from_session_candidate(candidate)
            .await
            .expect("application construction");

        assert!(notice.is_none(), "fresh construction must not degrade");
        assert!(
            !observation.is_shutdown(),
            "application must retain the owner runtime"
        );
        let receipts = observation.send("Main", "Main", "owner remains live", None);
        assert_eq!(receipts.len(), 1, "single Main delivery receipt");
        assert!(
            receipts[0].error.is_none(),
            "Main delivery must succeed: {receipts:?}"
        );
        let messages = observation
            .inbox("Main", true)
            .expect("Main inbox remains registered");
        assert_eq!(messages.len(), 1, "sent message must remain retrievable");
        assert_eq!(messages[0].body, "owner remains live");

        application.cleanup().await;
        assert!(
            observation.is_shutdown(),
            "application cleanup shuts down the runtime"
        );
    }

    #[tokio::test]
    async fn durable_policy_mismatch_retry_uses_fresh_publication_owner_and_preserves_goal_binding()
    {
        let artifacts = tempfile::tempdir().expect("artifacts");
        let sessions = tempfile::tempdir().expect("sessions");
        let (test_model, _faux) = registered_faux_model("durable-goal-retry");
        let recorder = pi_coding::start_session_in(
            artifacts.path(),
            Some(&test_model),
            Some("off"),
            Some(sessions.path()),
            None,
            None,
        )
        .expect("parent recorder");
        recorder
            .persist_now()
            .expect("materialize real parent header for durable fixture");
        let parent_id = recorder.id();
        let parent_path = recorder.path();
        let parent = hermetic_session(SessionOptions {
            model: test_model.clone(),
            cwd: artifacts.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        });
        parent.set_session_dir(sessions.path().to_path_buf());
        parent.record(recorder).expect("record");
        let parent_agent = tempfile::tempdir().expect("parent agent dir");
        fs::write(
            parent_agent.path().join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )
        .expect("parent sandbox settings");
        parent
            .attach_resources(isolated_resource_manager(
                artifacts.path(),
                parent_agent.path(),
            ))
            .await
            .expect("attach parent resources");

        let sidecar = child_root_for(&parent_path, &parent_id).join("orchestration-state.json");
        let seed = Application::from_runtime_candidate(
            ApplicationRuntimeCandidate::new(parent.clone()).with_orchestration(
                test_orchestration_runtime(artifacts.path(), test_model.clone()),
            ),
        )
        .await
        .expect("seed sidecar at revision 1");
        assert!(sidecar.is_file(), "sidecar written: {}", sidecar.display());
        let persisted: DurableState =
            serde_json::from_slice(&fs::read(&sidecar).expect("read sidecar"))
                .expect("decode sidecar");
        assert_eq!(
            persisted.durable_policy.snapshot().revision().get(),
            1,
            "fresh bind writes revision 1"
        );
        seed.cleanup().await;

        let captured = persisted.durable_policy.clone();
        let drifted_creds = CredentialPolicy::new(vec![
            CredentialGrantId::parse("github-extra".to_owned()).expect("grant id"),
        ])
        .expect("drifted credentials");
        let drifted = DurablePolicyState::new(
            PolicySnapshot::new(
                captured.snapshot().revision(),
                captured.snapshot().created_on(),
                SandboxPolicy::danger_full_access(drifted_creds).expect("drifted policy"),
            )
            .expect("drifted snapshot"),
            captured.credential_grants().clone(),
        )
        .expect("drifted durable policy");
        fs::write(
            &sidecar,
            serde_json::to_vec(&DurableState {
                version: DURABLE_STATE_VERSION,
                parent_session_id: persisted.parent_session_id.clone(),
                parent_session_path: persisted.parent_session_path.clone(),
                group_id: persisted.group_id.clone(),
                durable_policy: drifted,
                main_mailbox: persisted.main_mailbox.clone(),
                delivered_messages: persisted.delivered_messages.clone(),
                agents: persisted.agents.clone(),
                jobs: persisted.jobs.clone(),
            })
            .expect("serialize drifted sidecar"),
        )
        .expect("overwrite sidecar");
        let retry_agent = tempfile::tempdir().expect("retry agent dir");
        fs::write(
            retry_agent.path().join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )
        .expect("retry sandbox settings");
        let cli =
            Cli::try_parse_from(["rpi", "--no-extensions", "--no-skills", "--api-key", "test"])
                .expect("retry cli");
        let mut resource_options = ResourceManagerOptions::new(artifacts.path());
        resource_options.agent_dir = retry_agent.path().to_path_buf();
        resource_options.project_trust_override = Some(false);
        resource_options.disable_extensions = true;
        resource_options.disable_skills = true;
        resource_options.disable_prompt_templates = true;
        resource_options.disable_themes = true;
        resource_options.disable_context_files = true;
        let retry_blueprint = RunSessionBlueprint::from_cli(&cli, resource_options, None);
        let options = SessionOptions {
            model: test_model.clone(),
            cwd: artifacts.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "test".to_owned(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: Some(Arc::new(
                AuthManager::new(retry_agent.path().join("auth.json")).expect("retry auth manager"),
            )),
        };
        let context = pi_coding::PreparedSessionResume::prepare_path(&parent_path)
            .expect("prepare restarted parent")
            .build_context();
        let recorder = pi_coding::resume_session(&parent_path).expect("resume restarted parent");
        let goal = GoalToolBinding::default();
        let mut candidate = retry_blueprint
            .build(artifacts.path(), options.clone())
            .await
            .expect("build durable candidate")
            .restore_resume(
                context.clone(),
                recorder.clone(),
                None,
                Some(test_model.clone()),
                ThinkingLevel::Off,
            )
            .await
            .expect("restore durable candidate");
        candidate.orchestration = Some(test_orchestration_runtime(
            artifacts.path(),
            test_model.clone(),
        ));
        candidate.goal_tool = Some(goal.clone());
        let failed_owner = candidate.session.process_owner_id();
        let error = match application_from_session_candidate(candidate).await {
            Ok(_) => panic!("durable mismatch must require a fresh retry owner"),
            Err(error) => error,
        };
        assert!(
            durable_resume_error(&error).is_some(),
            "typed durable mismatch required: {error:#}"
        );
        let notice = durable_resume_notice(&error);
        let mut retry = retry_blueprint
            .build_without_orchestration(artifacts.path(), options)
            .await
            .expect("build orchestration-free retry")
            .restore_resume(
                context,
                recorder,
                None,
                Some(test_model.clone()),
                ThinkingLevel::Off,
            )
            .await
            .expect("restore orchestration-free retry");
        retry.goal_tool = Some(goal.clone());
        assert_ne!(
            failed_owner,
            retry.session.process_owner_id(),
            "retry must own a fresh ProcessManager owner"
        );
        let (application, retry_notice) = application_from_session_candidate(retry)
            .await
            .expect("fresh-owner retry");
        assert!(retry_notice.is_none());
        assert!(
            notice.contains("Orchestration unbound")
                && notice.contains("durable sidecar does not match current policy"),
            "typed degrade notice required, got: {notice}"
        );
        assert!(
            !notice.contains("goal tool is already attached to an application"),
            "same GoalToolBinding must be reusable after degrade: {notice}"
        );
        assert!(
            application.orchestration_runtime().is_none(),
            "retry must unbind orchestration after durable mismatch"
        );

        let tool = goal.tool();
        assert_eq!(tool.name, "goal");
        let (controller, abort) = AbortController::new();
        let result = (tool.execute)(ToolCallContext {
            tool_call_id: "goal-get".to_owned(),
            arguments: json!({ "op": "get" }),
            on_update: Arc::new(|_: AgentToolResult| {}),
            abort,
            model: None,
            approval_token: None,
        })
        .await
        .unwrap_or_else(|error| panic!("same GoalToolBinding get must succeed: {error:#}"));
        drop(controller);
        let parsed: GoalState =
            serde_json::from_value(result.details.clone()).unwrap_or_else(|error| {
                panic!(
                    "goal get details must be valid GoalState: {error}; details={}",
                    result.details
                )
            });
        assert_eq!(parsed, application.goal_state());
        assert!(
            parsed.current.is_none(),
            "fresh resume has no current goal: {parsed:?}"
        );
        assert_eq!(parsed.revision, 0);
        application.cleanup().await;
    }

    /// Shared fixture for the fork regression tests: a hermetic source
    /// application carrying a recorded session under `native_root`, plus the
    /// agent dir backing both runtimes (must outlive them).
    async fn fork_source_application(
        project: &Path,
        model: &Model,
    ) -> (Application, tempfile::TempDir) {
        let agent = tempfile::tempdir().expect("agent dir");
        let application = hermetic_application(
            SessionOptions {
                model: model.clone(),
                cwd: project.to_path_buf(),
                system_prompt: String::new(),
                thinking_level: ThinkingLevel::Off,
                api_key: String::new(),
                compaction: None,
                stream_options: SimpleStreamOptions::default(),
                tools: Some(Vec::new()),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn: None,
                auth_resolver: None,
                auth_manager: Some(Arc::new(
                    AuthManager::new(agent.path().join("auth.json")).expect("auth manager"),
                )),
            },
            agent.path(),
        )
        .await;
        (application, agent)
    }

    fn fork_spawner(native_root: &Path) -> (RunSessionSpawner, Arc<TodoChildService>) {
        let cli = Cli::try_parse_from([
            "rpi",
            "--api-key",
            "test-key",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
        ])
        .expect("cli");
        let todo_child_service = TodoChildService::new();
        let spawner = RunSessionSpawner::from_startup_with_service(
            &cli,
            SessionStorage::ExplicitRoot(native_root.to_path_buf()),
            todo_child_service.clone(),
        );
        (spawner, todo_child_service)
    }

    fn user_text_of_entry(entry: &pi_coding::SessionEntry) -> Option<String> {
        match entry.message.as_ref()? {
            pi_ai::Message::User(user) => user.content.iter().find_map(|block| match block {
                pi_ai::ContentBlock::Text { text, .. } => Some(text.clone()),
                _ => None,
            }),
            _ => None,
        }
    }

    #[tokio::test]
    async fn fork_session_nested_user_branches_with_source_lineage() {
        let native_root = tempfile::tempdir().expect("native root");
        let project = tempfile::tempdir().expect("project");
        let (model, _faux) = registered_faux_model("nested-fork");
        let (source, _agent) = fork_source_application(project.path(), &model).await;
        // A model-rooted recorder makes every user entry NESTED: the
        // model_change record is the only top-level tree node, so the
        // top-level-forest lookup used to misroute this fork to a blank
        // `new_session`.
        let recorder = pi_coding::start_session_in(
            project.path(),
            Some(&model),
            Some("off"),
            Some(native_root.path()),
            None,
            None,
        )
        .expect("source recorder");
        let first = recorder
            .record_message(&pi_ai::Message::user_text("first question", 0))
            .expect("record first user");
        let second = recorder
            .record_message(&pi_ai::Message::user_text("second question", 1))
            .expect("record second user");
        recorder.persist_now().expect("persist source");
        let source_path = recorder.path();
        source.session().record(recorder).expect("attach recorder");

        let (spawner, _todo_child_service) = fork_spawner(native_root.path());
        let forked = spawner
            .fork_session(&source, &second)
            .await
            .expect("nested fork");
        let (fork_id, fork_file) = forked
            .application
            .session()
            .recorder_info()
            .expect("fork recorder info");
        assert_ne!(
            fork_id,
            source.session().recorder_info().expect("source id").0
        );
        assert_eq!(forked.session_id, fork_id);
        assert_eq!(forked.session_file, fork_file);
        assert_eq!(
            forked.application.session().session_file_identity(),
            Some(forked.file_identity)
        );
        assert_ne!(fork_file, source_path, "nested fork must open its own file");
        let fork_tree = pi_coding::load_session_tree(&fork_file).expect("fork tree");
        assert_eq!(
            fork_tree.header.parent_session.as_deref(),
            Some(source_path.to_string_lossy().as_ref()),
            "nested fork must record the source path as parentSession"
        );
        // The branch retains everything BEFORE the selected turn: the child
        // conversation is NOT blank and excludes the selected prompt.
        let fork_texts: Vec<String> = fork_tree
            .entries
            .iter()
            .filter_map(user_text_of_entry)
            .collect();
        assert_eq!(fork_texts, vec!["first question".to_owned()]);
        assert_eq!(
            forked.editor_text.as_deref(),
            Some("second question"),
            "nested fork must return the selected prompt as editor draft"
        );
        assert_eq!(fork_tree.active_leaf_id.as_deref(), Some(first.as_str()));
        let catalog = pi_coding::SessionCatalog::new(native_root.path())
            .with_native_session_root(native_root.path());
        assert!(
            catalog
                .scan(&[pi_coding::SessionSourceKind::NativePi])
                .iter()
                .any(|row| row.session_id == fork_id && row.path == fork_file),
            "native catalog must rediscover the fork by its durable identity"
        );

        forked.application.cleanup().await;
        source.cleanup().await;
    }

    #[tokio::test]
    async fn fork_session_root_user_records_fresh_session_with_parent_lineage() {
        let native_root = tempfile::tempdir().expect("native root");
        let project = tempfile::tempdir().expect("project");
        let (model, _faux) = registered_faux_model("root-fork");
        let (source, _agent) = fork_source_application(project.path(), &model).await;
        // No model/thinking records: the first user entry is a ROOT node
        // (parent_id None), exercising the fresh-session fork path.
        let recorder = pi_coding::start_session_in(
            project.path(),
            None,
            None,
            Some(native_root.path()),
            None,
            None,
        )
        .expect("source recorder");
        let root_user = recorder
            .record_message(&pi_ai::Message::user_text("root prompt", 0))
            .expect("record root user");
        recorder.persist_now().expect("persist source");
        let source_id = recorder.id();
        let source_path = recorder.path();
        source.session().record(recorder).expect("attach recorder");

        let (spawner, _todo_child_service) = fork_spawner(native_root.path());
        let forked = spawner
            .fork_session(&source, &root_user)
            .await
            .expect("root fork");
        assert_ne!(forked.session_id, source_id);
        let (_, fork_file) = forked
            .application
            .session()
            .recorder_info()
            .expect("fork recorder info");
        let catalog = pi_coding::SessionCatalog::new(native_root.path())
            .with_native_session_root(native_root.path());
        assert_eq!(forked.session_file, fork_file);
        assert_eq!(
            forked.application.session().session_file_identity(),
            Some(forked.file_identity)
        );
        assert!(
            catalog
                .scan(&[pi_coding::SessionSourceKind::NativePi])
                .iter()
                .any(|row| row.session_id == forked.session_id && row.path == fork_file),
            "header-only root fork must survive catalog refresh"
        );
        assert_ne!(fork_file, source_path, "root fork must open its own file");
        let fork_tree = pi_coding::load_session_tree(&fork_file).expect("fork tree");
        assert_eq!(
            fork_tree.header.parent_session.as_deref(),
            Some(source_path.to_string_lossy().as_ref()),
            "root fork must record the source path as parentSession (not a blank unrelated new_session)"
        );
        // Fresh child: no retained turns at all — the selected root prompt is
        // excluded exactly as the in-place `/fork` semantics require.
        assert!(
            fork_tree
                .entries
                .iter()
                .all(|entry| entry.message.is_none()),
            "root fork must exclude the selected prompt: {:?}",
            fork_tree
                .entries
                .iter()
                .filter_map(user_text_of_entry)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            forked.editor_text.as_deref(),
            Some("root prompt"),
            "root fork must return the selected prompt as editor draft"
        );

        forked.application.cleanup().await;
        source.cleanup().await;
    }

    #[tokio::test]
    async fn fork_session_nested_survives_parent_deletion_and_independent_append() {
        let native_root = tempfile::tempdir().expect("native root");
        let project = tempfile::tempdir().expect("project");
        let (model, _faux) = registered_faux_model("delete-parent-fork");
        let (source, _agent) = fork_source_application(project.path(), &model).await;
        let recorder = pi_coding::start_session_in(
            project.path(),
            Some(&model),
            Some("off"),
            Some(native_root.path()),
            None,
            None,
        )
        .expect("source recorder");
        let first = recorder
            .record_message(&pi_ai::Message::user_text("first question", 0))
            .expect("record first user");
        let second = recorder
            .record_message(&pi_ai::Message::user_text("second question", 1))
            .expect("record second user");
        recorder.persist_now_durable().expect("persist source");
        let source_path = recorder.path();
        source.session().record(recorder).expect("attach recorder");

        let (spawner, _todo_child_service) = fork_spawner(native_root.path());
        let forked = spawner
            .fork_session(&source, &second)
            .await
            .expect("nested fork");
        let fork_file = forked.session_file.clone();
        let fork_id = forked.session_id.clone();
        assert_eq!(forked.editor_text.as_deref(), Some("second question"));
        forked.application.cleanup().await;
        source.cleanup().await;

        fs::remove_file(&source_path).expect("delete parent jsonl");
        assert!(!source_path.exists());

        let reopened = pi_coding::resume_session(&fork_file).expect("reopen child");
        assert_eq!(reopened.id(), fork_id);
        let tree = reopened.tree().expect("reopened tree");
        let texts: Vec<String> = tree.entries.iter().filter_map(user_text_of_entry).collect();
        assert_eq!(texts, vec!["first question".to_owned()]);
        assert!(tree.entries.iter().any(|entry| entry.id == first));
        assert!(tree.entries.iter().all(|entry| entry.id != second));
        reopened
            .record_message(&pi_ai::Message::user_text("child-only", 2))
            .expect("append after parent deletion");
        reopened
            .persist_now_durable()
            .expect("persist independent append");
        let reloaded = pi_coding::load_session_tree(&fork_file).expect("reload after append");
        let texts: Vec<String> = reloaded
            .entries
            .iter()
            .filter_map(user_text_of_entry)
            .collect();
        assert_eq!(
            texts,
            vec!["first question".to_owned(), "child-only".to_owned()]
        );
        let catalog = pi_coding::SessionCatalog::new(native_root.path())
            .with_native_session_root(native_root.path());
        assert!(
            catalog
                .scan(&[pi_coding::SessionSourceKind::NativePi])
                .iter()
                .any(|row| row.session_id == fork_id && row.path == fork_file),
            "child must remain catalog-listed after parent deletion"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fresh_finish_validation_error_awaits_armed_rollback() {
        let custom_root = tempfile::tempdir().expect("custom source root");
        let project_a = tempfile::tempdir().expect("project A target");
        let project_b = tempfile::tempdir().expect("project B source");
        let cli = Cli::try_parse_from([
            "rpi",
            "-C",
            project_b.path().to_str().expect("project B utf8"),
            "--api-key",
            "test-key",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
        ])
        .expect("cli");
        let todo_child_service = TodoChildService::new();
        let spawner = RunSessionSpawner::from_startup_with_service(
            &cli,
            SessionStorage::ExplicitRoot(custom_root.path().to_path_buf()),
            todo_child_service.clone(),
        );
        let (source_model, _source_faux) = registered_faux_model("fresh-validation-source");
        let (source, _source_agent_dir) =
            fork_source_application(project_b.path(), &source_model).await;

        let directory =
            pi_coding::prepare_operator_directory(project_a.path()).expect("target directory");
        let prepared = spawner
            .prepare_new_session(&source, directory, None)
            .await
            .expect("prepare fresh from explicit-root source");
        let gate = FreshFinishParkGate::new();
        let prepared = prepared.park_finish_after_journal_for_test(gate.clone());

        fs::remove_dir_all(project_a.path()).expect("delete target root");

        let task = tokio::spawn(prepared.finish());
        gate.wait_rollback_entered().await;
        gate.release_rollback();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), task)
            .await
            .expect("settled finish joins")
            .expect("finish task");
        let error = match outcome { Err(error) => error, Ok(_) => panic!("deleted target root must fail finish validation") };
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("validating fresh workspace authorities"),
            "validation failure must name the phase: {rendered}"
        );
        assert!(
            gate.rollback_settled(),
            "the settled error path awaited the armed rollback pass to completion"
        );
        source.cleanup().await;
    }

    struct ParkedBuildSpawner {
        inner: RunSessionSpawner,
        entered: tokio_util::sync::CancellationToken,
        release: tokio_util::sync::CancellationToken,
    }

    impl crate::modes::session_runtime_manager::SessionSpawner for ParkedBuildSpawner {
        fn prepare(
            &self,
            request: crate::modes::session_runtime_manager::SessionSpawnRequest,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = Result<
                            crate::modes::session_runtime_manager::PreparedSessionSpawn,
                        >,
                    > + Send,
                >,
            > {
            let inner = self.inner.clone();
            let entered = self.entered.clone();
            let release = self.release.clone();
            Box::pin(async move {
                let prepared = crate::modes::session_runtime_manager::SessionSpawner::prepare(
                    &inner, request,
                );
                entered.cancel();
                release.cancelled().await;
                prepared.await
            })
        }

        fn spawn(
            &self,
            request: crate::modes::session_runtime_manager::SessionSpawnRequest,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = Result<
                            crate::modes::session_runtime_manager::SessionSpawnResult,
                        >,
                    > + Send,
                >,
            > {
            self.inner.spawn(request)
        }

        fn session_storage(&self) -> Option<SessionStorage> {
            self.inner.session_storage()
        }

        fn track_detached_cleanups(&self, tracker: SpawnRollbackTracker) {
            crate::modes::session_runtime_manager::SessionSpawner::track_detached_cleanups(
                &self.inner,
                tracker,
            );
        }

        fn catalog(&self) -> Option<pi_coding::SessionCatalog> {
            self.inner.catalog()
        }

        fn todo_child_service(&self) -> Option<Arc<TodoChildService>> {
            self.inner.todo_child_service()
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn prepared_fresh_tracker_survives_build_suspension_swap() {
        let custom_root = tempfile::tempdir().expect("custom source root");
        let project_a = tempfile::tempdir().expect("project A target");
        let project_b = tempfile::tempdir().expect("project B source");
        let cli = Cli::try_parse_from([
            "rpi",
            "-C",
            project_b.path().to_str().expect("project B utf8"),
            "--api-key",
            "test-key",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
        ])
        .expect("cli");
        let todo_child_service = TodoChildService::new();
        let inner = RunSessionSpawner::from_startup_with_service(
            &cli,
            SessionStorage::ExplicitRoot(custom_root.path().to_path_buf()),
            todo_child_service.clone(),
        );
        let (source_model, _source_faux) = registered_faux_model("build-swap-source");
        let (source, _source_agent_dir) =
            fork_source_application(project_b.path(), &source_model).await;
        let (tracker_a_tx, mut tracker_a_rx) = tokio::sync::mpsc::unbounded_channel();
        let (tracker_b_tx, mut tracker_b_rx) = tokio::sync::mpsc::unbounded_channel();
        let tracker_a: SpawnRollbackTracker = Arc::new(move |handle| {
            let _ = tracker_a_tx.send(handle);
        });
        let tracker_b: SpawnRollbackTracker = Arc::new(move |handle| {
            let _ = tracker_b_tx.send(handle);
        });
        crate::modes::session_runtime_manager::SessionSpawner::track_detached_cleanups(
            &inner,
            tracker_a,
        );
        let parked = ParkedBuildSpawner {
            inner: inner.clone(),
            entered: tokio_util::sync::CancellationToken::new(),
            release: tokio_util::sync::CancellationToken::new(),
        };
        let directory =
            pi_coding::prepare_operator_directory(project_a.path()).expect("target directory");
        let request = crate::modes::session_runtime_manager::SessionSpawnRequest {
            kind: crate::modes::session_runtime_manager::SessionSpawnKind::Fresh { directory },
            source: source.clone(),
        };
        let parked_task = ParkedBuildSpawner {
            inner: parked.inner.clone(),
            entered: parked.entered.clone(),
            release: parked.release.clone(),
        };
        let prepare_task = tokio::spawn(async move {
            crate::modes::session_runtime_manager::SessionSpawner::prepare(&parked_task, request)
                .await
                .expect("parked prepare")
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            parked.entered.cancelled(),
        )
        .await
        .expect("prepare entered the parked build");
        crate::modes::session_runtime_manager::SessionSpawner::track_detached_cleanups(
            &inner,
            tracker_b,
        );
        parked.release.cancel();
        let prepared = tokio::time::timeout(std::time::Duration::from_secs(30), prepare_task)
            .await
            .expect("parked prepare settles")
            .expect("prepare task");
        let prepared = match prepared {
            crate::modes::session_runtime_manager::PreparedSessionSpawn::Fresh(prepared) => prepared,
            _ => panic!("expected a fresh preparation"),
        };
        let gate = FreshFinishParkGate::new();
        let prepared = prepared.park_finish_after_journal_for_test(gate.clone());
        let finish = tokio::spawn(async move { prepared.finish().await });
        tokio::time::timeout(std::time::Duration::from_secs(30), gate.wait_entered())
            .await
            .expect("finish parked after the journal became durable");
        finish.abort();
        let handle = tokio::time::timeout(std::time::Duration::from_secs(5), tracker_a_rx.recv())
            .await
            .expect("fallback tracked by the pre-build tracker")
            .expect("tracker a channel");
        drop(handle);
        assert!(
            tracker_b_rx.try_recv().is_err(),
            "the post-capture manager registration must not capture a prepare-time rollback"
        );
        gate.release_rollback();
        let journal = gate.journal_path().expect("journal path recorded");
        for _ in 0..200 {
            if !journal.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            !journal.exists(),
            "the prepare-time tracked fallback rolled the created journal back"
        );
        assert!(
            gate.rollback_settled(),
            "the detached fallback settled every rollback leg"
        );
        source.cleanup().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fresh_finish_start_recording_error_rolls_back_partial_candidate() {
        let custom_root = tempfile::tempdir().expect("custom source root");
        let project_a = tempfile::tempdir().expect("project A target");
        let project_b = tempfile::tempdir().expect("project B source");
        let cli = Cli::try_parse_from([
            "rpi",
            "-C",
            project_b.path().to_str().expect("project B utf8"),
            "--api-key",
            "test-key",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
        ])
        .expect("cli");
        let todo_child_service = TodoChildService::new();
        let spawner = RunSessionSpawner::from_startup_with_service(
            &cli,
            SessionStorage::ExplicitRoot(custom_root.path().to_path_buf()),
            todo_child_service.clone(),
        );
        let (source_model, _source_faux) = registered_faux_model("fresh-recording-source");
        let (source, _source_agent_dir) =
            fork_source_application(project_b.path(), &source_model).await;

        let directory =
            pi_coding::prepare_operator_directory(project_a.path()).expect("target directory");
        let prepared = spawner
            .prepare_new_session(&source, directory, None)
            .await
            .expect("prepare fresh from explicit-root source");
        let gate = FreshFinishParkGate::new();
        let prepared = prepared.park_finish_after_journal_for_test(gate.clone());

        let blocked = custom_root.path().join("sessions");
        fs::write(&blocked, b"not a directory").expect("block recording root");
        prepared
            .candidate
            .session
            .set_session_dir(blocked.clone());

        let task = tokio::spawn(prepared.finish());
        gate.wait_rollback_entered().await;
        gate.release_rollback();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), task)
            .await
            .expect("settled finish joins")
            .expect("finish task");
        assert!(
            outcome.is_err(),
            "a non-directory recording root must fail start_new_recording"
        );
        assert!(
            gate.rollback_settled(),
            "the settled error path awaited the armed rollback pass to completion"
        );
        assert!(
            blocked.is_file(),
            "the rollback must not have touched the blocked recording path"
        );
        source.cleanup().await;
    }


    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn prepared_fresh_captures_tracker_at_prepare_time() {
        let custom_root = tempfile::tempdir().expect("custom source root");
        let project_a = tempfile::tempdir().expect("project A target");
        let project_b = tempfile::tempdir().expect("project B source");
        let cli = Cli::try_parse_from([
            "rpi",
            "-C",
            project_b.path().to_str().expect("project B utf8"),
            "--api-key",
            "test-key",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
        ])
        .expect("cli");
        let todo_child_service = TodoChildService::new();
        let spawner = RunSessionSpawner::from_startup_with_service(
            &cli,
            SessionStorage::ExplicitRoot(custom_root.path().to_path_buf()),
            todo_child_service.clone(),
        );
        let (source_model, _source_faux) = registered_faux_model("prepared-tracker-source");
        let (source, _source_agent_dir) =
            fork_source_application(project_b.path(), &source_model).await;
        let (tracker_a_tx, mut tracker_a_rx) = tokio::sync::mpsc::unbounded_channel();
        let (tracker_b_tx, mut tracker_b_rx) = tokio::sync::mpsc::unbounded_channel();
        let tracker_a: SpawnRollbackTracker = Arc::new(move |handle| {
            let _ = tracker_a_tx.send(handle);
        });
        let tracker_b: SpawnRollbackTracker = Arc::new(move |handle| {
            let _ = tracker_b_tx.send(handle);
        });
        crate::modes::session_runtime_manager::SessionSpawner::track_detached_cleanups(
            &spawner,
            tracker_a,
        );
        let directory =
            pi_coding::prepare_operator_directory(project_a.path()).expect("target directory");
        let prepared = spawner
            .prepare_new_session(&source, directory, spawner.detached_tracker_for_test())
            .await
            .expect("prepare fresh");
        crate::modes::session_runtime_manager::SessionSpawner::track_detached_cleanups(
            &spawner,
            tracker_b,
        );
        let gate = FreshFinishParkGate::new();
        let prepared = prepared.park_finish_after_journal_for_test(gate.clone());
        let finish = tokio::spawn(async move { prepared.finish().await });
        tokio::time::timeout(std::time::Duration::from_secs(30), gate.wait_entered())
            .await
            .expect("finish parked after the journal became durable");
        finish.abort();
        let handle = tokio::time::timeout(std::time::Duration::from_secs(5), tracker_a_rx.recv())
            .await
            .expect("fallback tracked by the prepare-time tracker")
            .expect("tracker a channel");
        drop(handle);
        assert!(
            tracker_b_rx.try_recv().is_err(),
            "the later manager registration must not capture a prepare-time rollback"
        );
        gate.release_rollback();
        let journal = gate.journal_path().expect("journal path recorded");
        for _ in 0..200 {
            if !journal.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            !journal.exists(),
            "the prepare-time tracked fallback rolled the created journal back"
        );
        assert!(
            gate.rollback_settled(),
            "the detached fallback settled every rollback leg"
        );
        source.cleanup().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn request_fork_after_durable_journal_rolls_back_before_error_observed() {
        let native_root = tempfile::tempdir().expect("native root");
        let project = tempfile::tempdir().expect("project");
        let (model, _faux) = registered_faux_model("fork-journal-rollback");
        let (source, _agent) = fork_source_application(project.path(), &model).await;
        let recorder = pi_coding::start_session_in(
            project.path(),
            Some(&model),
            Some("off"),
            Some(native_root.path()),
            None,
            None,
        )
        .expect("source recorder");
        let _first = recorder
            .record_message(&pi_ai::Message::user_text("first question", 0))
            .expect("record first user");
        let second = recorder
            .record_message(&pi_ai::Message::user_text("second question", 1))
            .expect("record second user");
        recorder.persist_now_durable().expect("persist source");
        let source_path = recorder.path();
        source.session().record(recorder).expect("attach recorder");

        let cli = Cli::try_parse_from([
            "rpi",
            "--api-key",
            "test-key",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
        ])
        .expect("cli");
        let todo_child_service = TodoChildService::new();
        let spawner = RunSessionSpawner::from_startup_with_service(
            &cli,
            SessionStorage::ExplicitRoot(native_root.path().to_path_buf()),
            todo_child_service.clone(),
        );
        drop(todo_child_service);

        let catalog = pi_coding::SessionCatalog::new(native_root.path())
            .with_native_session_root(native_root.path());
        let before = catalog
            .scan(&[pi_coding::SessionSourceKind::NativePi])
            .iter()
            .map(|row| row.path.clone())
            .collect::<std::collections::HashSet<_>>();

        let outcome = spawner.fork_session(&source, &second).await;
        assert!(
            outcome.is_err(),
            "an unavailable Todo child service must fail the fork closed"
        );
        let after = catalog
            .scan(&[pi_coding::SessionSourceKind::NativePi])
            .iter()
            .map(|row| row.path.clone())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(
            before, after,
            "the just-created fork journal was removed before the error was observed"
        );
        assert!(
            source_path.exists(),
            "the recorded source journal is untouched by the rollback"
        );
        source.cleanup().await;
    }

}
