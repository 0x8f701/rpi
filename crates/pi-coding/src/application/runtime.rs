use std::{
    collections::HashSet,
    sync::Arc,
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

use parking_lot::{Mutex, RwLock};
use tokio::{sync::Notify, task::JoinHandle};
use tokio::sync::{Mutex as AsyncMutex, broadcast};

use super::{ApplicationEvent, GoalToolBinding, GoalWorkKey, todo_execution};
use crate::{
    ExtensionPermissionSet, ExtensionRuntime, OrchestrationRuntime, ProcessManager, ProcessOwnerId,
    Session,
};

pub(super) const INITIAL_RUNTIME_EPOCH: u64 = 1;

pub(super) struct ApplicationRuntime {
    pub(super) epoch: u64,
    pub(super) session: Session,
    pub(super) extension_runtime: Mutex<Option<(ExtensionRuntime, ExtensionPermissionSet)>>,
    pub(super) orchestration_runtime: Mutex<Option<OrchestrationRuntime>>,
    pub(super) goal_tool_binding: Mutex<Option<GoalToolBinding>>,
    pub(super) process_manager: ProcessManager,
    pub(super) process_owner_id: ProcessOwnerId,
    pub(super) runtime_settings: Mutex<Arc<crate::RuntimeSettingsSnapshot>>,
    pub(super) session_subscription: Mutex<Option<pi_agent::Subscription>>,
    pub(super) active_run: Mutex<Option<JoinHandle<()>>>,
    pub(super) orchestration_events: Mutex<Option<JoinHandle<()>>>,
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
    ) -> Self {
        let process_manager = session.process_manager();
        let process_owner_id = session.process_owner_id();
        let runtime_settings = session
            .resource_manager()
            .map(|resources| resources.snapshot().settings.runtime_settings())
            .transpose()
            .expect("attached resource settings were already validated")
            .unwrap_or_else(|| crate::Settings::default().runtime_settings().expect("default settings"));
        Self {
            epoch,
            session,
            extension_runtime: Mutex::new(extension_runtime),
            orchestration_runtime: Mutex::new(orchestration_runtime),
            goal_tool_binding: Mutex::new(goal_tool_binding),
            process_manager,
            process_owner_id,
            runtime_settings: Mutex::new(Arc::new(runtime_settings)),
            session_subscription: Mutex::new(None),
            active_run: Mutex::new(None),
            orchestration_events: Mutex::new(None),
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

    pub(super) fn extension_runtime(
        &self,
    ) -> Option<(ExtensionRuntime, ExtensionPermissionSet)> {
        self.extension_runtime.lock().clone()
    }

    pub(super) fn orchestration_runtime(&self) -> Option<OrchestrationRuntime> {
        self.orchestration_runtime.lock().clone()
    }

    pub(super) fn goal_tool_binding(&self) -> Option<GoalToolBinding> {
        self.goal_tool_binding.lock().clone()
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
}

impl ApplicationRuntimeCandidate {
    #[must_use]
    pub fn new(session: Session) -> Self {
        Self {
            session,
            extension_runtime: None,
            orchestration_runtime: None,
            goal_tool_binding: None,
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

    pub(super) fn activate(self, epoch: u64) -> ApplicationRuntime {
        ApplicationRuntime::new(
            epoch,
            self.session,
            self.extension_runtime,
            self.orchestration_runtime,
            self.goal_tool_binding,
        )
    }

    pub(super) async fn shutdown(self, reason: &'static str) {
        self.session.abort().await;
        self.session.wait_for_idle().await;
        self.session
            .process_manager()
            .shutdown_owner(&self.session.process_owner_id())
            .await;
        if let Some(runtime) = self.orchestration_runtime {
            runtime.shutdown().await;
        }
        if let Some((runtime, _)) = self.extension_runtime {
            runtime.shutdown_with_reason(reason).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use pi_ai::Model;
    use tokio::sync::broadcast;

    use super::*;
    use crate::SessionOptions;

    fn session(cwd: &Path, provider: &str) -> Session {
        Session::new(SessionOptions {
            model: Model {
                provider: provider.to_owned(),
                id: format!("{provider}-model"),
                ..Model::default()
            },
            cwd: cwd.to_path_buf(),
            system_prompt: String::new(),
            thinking_level: pi_agent::ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
        })
        .expect("test session")
    }

    #[test]
    fn clones_load_the_runtime_replaced_in_the_shared_slot() {
        let source = tempfile::tempdir().expect("source cwd");
        let target = tempfile::tempdir().expect("target cwd");
        let (events, _) = broadcast::channel(8);
        let slot = Arc::new(ApplicationRuntimeSlot::new(
            ApplicationRuntimeCandidate::new(session(source.path(), "source"))
                .activate(INITIAL_RUNTIME_EPOCH),
            events,
        ));
        let clone = slot.clone();
        let retained = slot.runtime().session();
        let epoch = slot.next_epoch();

        slot.replace(
            ApplicationRuntimeCandidate::new(session(target.path(), "target")).activate(epoch),
        );

        assert_eq!(clone.runtime().epoch(), epoch);
        assert_eq!(clone.runtime().session().cwd(), target.path());
        assert_eq!(retained.cwd(), source.path());
    }
}
