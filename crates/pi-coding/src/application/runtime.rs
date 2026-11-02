use std::{
    collections::HashSet,
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    sync::{Arc, OnceLock},
};

use parking_lot::{Mutex, RwLock};
use tokio::sync::{Mutex as AsyncMutex, broadcast};
use tokio::{sync::Notify, task::JoinHandle};

use super::{
    ApplicationEvent, GoalToolBinding, GoalWorkKey, TodoChildToolBinding, TurnLifecycleSnapshot,
    TurnTerminalSnapshot, TurnTerminalStatus, todo_execution,
};
use crate::{
    ExtensionPermissionSet, ExtensionRuntime, OrchestrationRuntime, ProcessManager, ProcessOwnerId,
    Session,
};

pub(super) const INITIAL_RUNTIME_EPOCH: u64 = 1;

#[derive(Default)]
pub(super) struct TurnState {
    revision: u64,
    active_turn_id: Option<String>,
    active_turn_model: Option<pi_ai::Model>,
    last_terminal: Option<TurnTerminalSnapshot>,
}

impl TurnState {
    fn snapshot(&self) -> TurnLifecycleSnapshot {
        TurnLifecycleSnapshot {
            revision: self.revision,
            active_turn_id: self.active_turn_id.clone(),
            active_turn_model: self.active_turn_model.clone(),
            last_terminal: self.last_terminal.clone(),
        }
    }
}
pub(super) struct CommittedTurnTerminal {
    runtime_epoch: u64,
    turn_id: String,
    status: TurnTerminalStatus,
    message: Option<String>,
}

pub(super) struct ApplicationRuntime {
    pub(super) epoch: u64,
    pub(super) session: Session,
    pub(super) publication: OnceLock<Arc<super::generation::ApplicationPublication>>,
    pub(super) extension_runtime: Mutex<Option<(ExtensionRuntime, ExtensionPermissionSet)>>,
    pub(super) orchestration_runtime: Mutex<Option<OrchestrationRuntime>>,
    pub(super) goal_tool_binding: Mutex<Option<GoalToolBinding>>,
    pub(super) todo_child_tool_binding: Mutex<Option<TodoChildToolBinding>>,
    pub(super) process_manager: ProcessManager,
    pub(super) process_owner_id: ProcessOwnerId,
    pub(super) runtime_settings: Mutex<Arc<crate::RuntimeSettingsSnapshot>>,
    pub(super) session_subscription: Mutex<Option<pi_agent::Subscription>>,
    pub(super) active_run: Mutex<Option<JoinHandle<()>>>,
    pub(super) turn_state: Mutex<TurnState>,
    pub(super) agent_event_barrier_base: AtomicU64,
    pub(super) orchestration_events: Mutex<Option<JoinHandle<()>>>,
    pub(super) turn_admission: Arc<AsyncMutex<()>>,
    pub(super) process_events: Mutex<Option<JoinHandle<()>>>,
    pub(super) session_events: Mutex<Option<JoinHandle<()>>>,
    pub(super) charged_goal_jobs: Mutex<HashSet<String>>,
    pub(super) orchestration_explicit: AtomicBool,
    pub(super) turn_gate: Arc<AsyncMutex<()>>,
    pub(super) loop_turn_active: AtomicBool,
    pub(super) todo_dag: Mutex<todo_execution::TodoDagCoordinator>,
    pub(super) todo_dag_changed: Notify,
    pub(super) todo_cycle_pending: AtomicBool,
    pub(super) todo_continuation_suppressed: AtomicBool,
    pub(super) todo_resume_requested: AtomicBool,
    pub(super) todo_transition_active: AtomicBool,
    pub(super) goal_work_activation: Mutex<Option<GoalWorkKey>>,
    pub(super) goal_work_pending: AtomicUsize,
    pub(super) goal_work_changed: Notify,
}

impl ApplicationRuntime {
    pub(super) fn new(
        epoch: u64,
        session: Session,
        extension_runtime: Option<(ExtensionRuntime, ExtensionPermissionSet)>,
        orchestration_runtime: Option<OrchestrationRuntime>,
        goal_tool_binding: Option<GoalToolBinding>,
        todo_child_tool_binding: Option<TodoChildToolBinding>,
    ) -> Self {
        let process_manager = session.process_manager();
        if let Some(auth_manager) = session.auth_manager() {
            process_manager
                .bind_auth_manager(auth_manager)
                .expect("process authentication owner binds once with session construction");
        }
        let process_owner_id = session.process_owner_id();
        let runtime_settings = session
            .resource_manager()
            .map(|resources| resources.snapshot().settings.runtime_settings())
            .transpose()
            .expect("attached resource settings were already validated")
            .unwrap_or_else(|| {
                crate::Settings::default()
                    .runtime_settings()
                    .expect("default settings")
            });
        Self {
            epoch,
            publication: OnceLock::new(),
            session,
            extension_runtime: Mutex::new(extension_runtime),
            orchestration_runtime: Mutex::new(orchestration_runtime),
            goal_tool_binding: Mutex::new(goal_tool_binding),
            todo_child_tool_binding: Mutex::new(todo_child_tool_binding),
            process_manager,
            process_owner_id,
            runtime_settings: Mutex::new(Arc::new(runtime_settings)),
            session_subscription: Mutex::new(None),
            active_run: Mutex::new(None),
            turn_state: Mutex::new(TurnState::default()),
            agent_event_barrier_base: AtomicU64::new(0),
            orchestration_events: Mutex::new(None),
            turn_admission: Arc::new(AsyncMutex::new(())),
            process_events: Mutex::new(None),
            session_events: Mutex::new(None),
            charged_goal_jobs: Mutex::new(HashSet::new()),
            orchestration_explicit: AtomicBool::new(false),
            turn_gate: Arc::new(AsyncMutex::new(())),
            loop_turn_active: AtomicBool::new(false),
            todo_dag: Mutex::new(todo_execution::TodoDagCoordinator::default()),
            todo_dag_changed: Notify::new(),
            todo_cycle_pending: AtomicBool::new(false),
            todo_continuation_suppressed: AtomicBool::new(false),
            todo_resume_requested: AtomicBool::new(false),
            todo_transition_active: AtomicBool::new(false),
            goal_work_activation: Mutex::new(None),
            goal_work_pending: AtomicUsize::new(0),
            goal_work_changed: Notify::new(),
        }
    }

    pub(super) fn epoch(&self) -> u64 {
        self.epoch
    }

    pub(super) fn session(&self) -> Session {
        self.session.clone()
    }
    pub(super) fn start_turn(&self, turn_id: &str, model: Option<&pi_ai::Model>) {
        let mut turn = self.turn_state.lock();
        turn.revision = turn.revision.saturating_add(1);
        turn.active_turn_id = Some(turn_id.to_owned());
        turn.active_turn_model = model.cloned();
    }

    pub(super) fn settle_turn(
        &self,
        turn_id: &str,
        status: &TurnTerminalStatus,
        message: Option<&String>,
    ) -> bool {
        let mut turn = self.turn_state.lock();
        if turn.active_turn_id.as_deref() != Some(turn_id) {
            return false;
        }
        turn.revision = turn.revision.saturating_add(1);
        turn.active_turn_id = None;
        turn.active_turn_model = None;
        turn.last_terminal = Some(TurnTerminalSnapshot {
            turn_id: turn_id.to_owned(),
            status: status.clone(),
            message: message.cloned(),
        });
        true
    }

    pub(super) fn turn_lifecycle_snapshot(&self) -> TurnLifecycleSnapshot {
        self.turn_state.lock().snapshot()
    }

    pub(super) fn reset_agent_event_barrier_base(&self, barrier: u64) {
        self.agent_event_barrier_base
            .store(barrier, Ordering::Release);
    }

    pub(super) fn agent_event_barrier_base(&self) -> u64 {
        self.agent_event_barrier_base.load(Ordering::Acquire)
    }

    pub(super) fn bind_publication(
        &self,
        publication: Arc<super::generation::ApplicationPublication>,
    ) -> anyhow::Result<()> {
        self.process_manager.bind_publication(&publication)?;
        self.publication
            .set(publication)
            .map_err(|_| anyhow::anyhow!("application runtime publication is already bound"))
    }

    pub(super) fn publication(&self) -> Arc<super::generation::ApplicationPublication> {
        self.publication
            .get()
            .expect("application runtime publication is bound before activation")
            .clone()
    }

    pub(super) fn extension_runtime(&self) -> Option<(ExtensionRuntime, ExtensionPermissionSet)> {
        self.extension_runtime.lock().clone()
    }

    pub(super) fn orchestration_runtime(&self) -> Option<OrchestrationRuntime> {
        self.orchestration_runtime.lock().clone()
    }

    pub(super) fn goal_tool_binding(&self) -> Option<GoalToolBinding> {
        self.goal_tool_binding.lock().clone()
    }

    pub(super) fn todo_child_tool_binding(&self) -> Option<TodoChildToolBinding> {
        self.todo_child_tool_binding.lock().clone()
    }

    pub(super) fn process_manager(&self) -> ProcessManager {
        self.process_manager.clone()
    }

    pub(super) fn process_owner_id(&self) -> ProcessOwnerId {
        self.process_owner_id.clone()
    }
}

pub(super) struct ApplicationRuntimeSlot {
    active: RwLock<Arc<ApplicationRuntime>>,
    next_epoch: AtomicU64,
    events: broadcast::Sender<ApplicationEvent>,
}

impl ApplicationRuntimeSlot {
    pub(super) fn new(
        runtime: ApplicationRuntime,
        events: broadcast::Sender<ApplicationEvent>,
    ) -> Self {
        let next_epoch = runtime.epoch().saturating_add(1);
        Self {
            active: RwLock::new(Arc::new(runtime)),
            next_epoch: AtomicU64::new(next_epoch),
            events,
        }
    }

    pub(super) fn runtime(&self) -> Arc<ApplicationRuntime> {
        self.active.read().clone()
    }
    pub(super) fn is_current(&self, runtime: &ApplicationRuntime) -> bool {
        let active = self.active.read();
        std::ptr::eq(Arc::as_ptr(&active), runtime)
    }
    pub(super) fn publish_turn_started(
        &self,
        runtime: &ApplicationRuntime,
        turn_id: &str,
        model: Option<&pi_ai::Model>,
    ) -> bool {
        let active = self.active.read();
        if !std::ptr::eq(Arc::as_ptr(&active), runtime) {
            return false;
        }
        runtime.start_turn(turn_id, model);
        let _ = self.events.send(ApplicationEvent::TurnStarted {
            turn_id: turn_id.to_owned(),
            model: model.cloned(),
        });
        true
    }

    pub(super) fn commit_turn_terminal(
        &self,
        runtime: &ApplicationRuntime,
        turn_id: &str,
        status: &TurnTerminalStatus,
        message: Option<&String>,
    ) -> Option<CommittedTurnTerminal> {
        let active = self.active.read();
        if !std::ptr::eq(Arc::as_ptr(&active), runtime)
            || !runtime.settle_turn(turn_id, status, message)
        {
            return None;
        }
        Some(CommittedTurnTerminal {
            runtime_epoch: runtime.epoch(),
            turn_id: turn_id.to_owned(),
            status: status.clone(),
            message: message.cloned(),
        })
    }

    pub(super) fn emit_turn_terminal(&self, terminal: CommittedTurnTerminal) {
        if self.active.read().epoch() != terminal.runtime_epoch {
            return;
        }
        let _ = self.events.send(ApplicationEvent::TurnTerminal {
            turn_id: terminal.turn_id,
            status: terminal.status,
            message: terminal.message,
        });
    }

    pub(super) fn next_epoch(&self) -> u64 {
        self.next_epoch.fetch_add(1, Ordering::AcqRel)
    }

    pub(super) fn replace_arc(&self, runtime: Arc<ApplicationRuntime>) -> Arc<ApplicationRuntime> {
        let epoch = runtime.epoch();
        let mut active = self.active.write();
        let previous = std::mem::replace(&mut *active, runtime);
        let sent = self.events.send(ApplicationEvent::RuntimeChanged { epoch });
        drop(active);
        let _ = sent;
        previous
    }

    pub(super) fn publish(&self, epoch: u64, event: ApplicationEvent) -> bool {
        let active = self.active.read();
        if active.epoch() != epoch {
            return false;
        }
        let _ = self.events.send(event);
        true
    }
}

pub struct ApplicationRuntimeCandidate {
    pub(super) session: Session,
    pub(super) extension_runtime: Option<(ExtensionRuntime, ExtensionPermissionSet)>,
    pub(super) orchestration_runtime: Option<OrchestrationRuntime>,
    pub(super) goal_tool_binding: Option<GoalToolBinding>,
    pub(super) todo_child_tool_binding: Option<TodoChildToolBinding>,
}

impl ApplicationRuntimeCandidate {
    #[must_use]
    pub fn new(session: Session) -> Self {
        Self {
            session,
            extension_runtime: None,
            orchestration_runtime: None,
            goal_tool_binding: None,
            todo_child_tool_binding: None,
        }
    }

    #[must_use]
    pub fn with_extensions(
        mut self,
        runtime: ExtensionRuntime,
        permissions: ExtensionPermissionSet,
    ) -> Self {
        self.extension_runtime = Some((runtime, permissions));
        self
    }

    #[must_use]
    pub fn with_orchestration(mut self, runtime: OrchestrationRuntime) -> Self {
        self.orchestration_runtime = Some(runtime);
        self
    }

    #[must_use]
    pub fn with_goal_tool(mut self, binding: GoalToolBinding) -> Self {
        self.goal_tool_binding = Some(binding);
        self
    }

    #[must_use]
    pub fn with_todo_child_tool(mut self, binding: TodoChildToolBinding) -> Self {
        self.todo_child_tool_binding = Some(binding);
        self
    }

    pub(super) fn activate(self, epoch: u64) -> ApplicationRuntime {
        ApplicationRuntime::new(
            epoch,
            self.session,
            self.extension_runtime,
            self.orchestration_runtime,
            self.goal_tool_binding,
            self.todo_child_tool_binding,
        )
    }

    pub(super) async fn shutdown(self, reason: &'static str) -> anyhow::Result<()> {
        self.session.abort().await;
        self.session.wait_for_idle().await;
        self.session
            .process_manager()
            .shutdown_owner(&self.session.process_owner_id())
            .await;
        let orchestration_result = match self.orchestration_runtime {
            Some(runtime) => runtime.shutdown().await,
            None => Ok(()),
        };
        if let Some((runtime, _)) = self.extension_runtime {
            runtime.shutdown_with_reason(reason).await;
        }
        orchestration_result
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use pi_ai::{
        Model,
        providers::{FauxProviderOptions, FauxProviderRegistration, register_faux_provider},
    };
    use tokio::sync::broadcast;

    use super::*;
    use crate::{
        Application, ResourceDiscovery, ResourceManager, ResourceManagerOptions, SessionOptions,
        ToolSelection, WorkspaceRoots,
    };

    struct PublishedRuntime {
        runtime: ApplicationRuntime,
        _application: Application,
        _faux: FauxProviderRegistration,
        _agent_dir: tempfile::TempDir,
        _session_dir: tempfile::TempDir,
    }

    async fn published_runtime(cwd: &Path, tag: &str, epoch: u64) -> PublishedRuntime {
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let session_dir = tempfile::tempdir().expect("session dir");
        std::fs::write(
            agent_dir.path().join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )
        .expect("isolated settings");

        let suffix = uuid::Uuid::now_v7();
        let api = format!("runtime-slot-{tag}-api-{suffix}");
        let provider = format!("runtime-slot-{tag}-provider-{suffix}");
        let model = Model {
            id: format!("runtime-slot-{tag}-model-{suffix}"),
            name: format!("Runtime slot {tag} fixture"),
            api: api.clone(),
            provider: provider.clone(),
            base_url: "http://localhost:0".to_owned(),
            ..Model::default()
        };
        let faux = register_faux_provider(FauxProviderOptions {
            api,
            provider,
            models: vec![model.clone()],
            chunk_size: 8,
        });
        let workspace = WorkspaceRoots::new(cwd, Vec::<PathBuf>::new()).expect("workspace");
        let session = Session::new_with_additional_tools_filtered_discovery_workspace_and_uri(
            SessionOptions {
                model,
                cwd: cwd.to_path_buf(),
                system_prompt: String::new(),
                thinking_level: pi_agent::ThinkingLevel::Off,
                api_key: "test".to_owned(),
                compaction: None,
                stream_options: Default::default(),
                tools: Some(Vec::new()),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn: None,
                auth_resolver: None,
                auth_manager: None,
            },
            Vec::new(),
            ToolSelection::default(),
            ResourceDiscovery::Disabled,
            workspace,
            None,
        )
        .expect("test session");
        session.set_session_dir(session_dir.path().to_path_buf());

        let mut options = ResourceManagerOptions::new(cwd);
        options.agent_dir = agent_dir.path().to_path_buf();
        options.project_trust_override = Some(true);
        options.disable_extensions = true;
        options.disable_skills = true;
        options.disable_prompt_templates = true;
        options.disable_themes = true;
        options.disable_context_files = true;
        let resources = ResourceManager::new(options).expect("isolated resources");
        session
            .attach_resources(resources)
            .await
            .expect("attach isolated resources");
        let application =
            Application::from_runtime_candidate(ApplicationRuntimeCandidate::new(session.clone()))
                .await
                .expect("publish application");

        let runtime = ApplicationRuntimeCandidate::new(session).activate(epoch);
        runtime
            .publication
            .set(application.publication())
            .map_err(|_| ())
            .expect("bind runtime publication");
        PublishedRuntime {
            runtime,
            _application: application,
            _faux: faux,
            _agent_dir: agent_dir,
            _session_dir: session_dir,
        }
    }

    #[tokio::test]
    async fn clones_load_the_runtime_replaced_in_the_shared_slot() {
        let source = tempfile::tempdir().expect("source cwd");
        let target = tempfile::tempdir().expect("target cwd");
        let source_runtime =
            published_runtime(source.path(), "source", INITIAL_RUNTIME_EPOCH).await;
        let (events, _) = broadcast::channel(8);
        let slot = Arc::new(ApplicationRuntimeSlot::new(source_runtime.runtime, events));
        let clone = slot.clone();
        let retained = slot.runtime().session();
        let epoch = slot.next_epoch();
        let target_runtime = published_runtime(target.path(), "target", epoch).await;

        slot.replace_arc(Arc::new(target_runtime.runtime));
        assert_eq!(clone.runtime().epoch(), epoch);
        assert_eq!(clone.runtime().session().cwd(), target.path());
        assert_eq!(retained.cwd(), source.path());
    }

    #[tokio::test]
    async fn stale_runtime_terminal_cannot_clear_replacement_turn() {
        let source = tempfile::tempdir().expect("source cwd");
        let target = tempfile::tempdir().expect("target cwd");
        let source_runtime =
            published_runtime(source.path(), "source-turn", INITIAL_RUNTIME_EPOCH).await;
        let (events, _) = broadcast::channel(8);
        let slot = Arc::new(ApplicationRuntimeSlot::new(source_runtime.runtime, events));
        let stale = slot.runtime();
        stale.start_turn("old-turn", None);

        let epoch = slot.next_epoch();
        let target_runtime = published_runtime(target.path(), "target-turn", epoch).await;
        let current = Arc::new(target_runtime.runtime);
        current.start_turn(
            "new-turn",
            Some(&current.session().model().expect("target model")),
        );
        slot.replace_arc(current.clone());

        assert!(
            slot.commit_turn_terminal(&stale, "old-turn", &TurnTerminalStatus::Aborted, None,)
                .is_none()
        );
        let current_turn = current.turn_lifecycle_snapshot();
        assert_eq!(current_turn.active_turn_id.as_deref(), Some("new-turn"));
        assert!(current_turn.active_turn_model.is_some());
        assert!(current_turn.last_terminal.is_none());
    }
}
