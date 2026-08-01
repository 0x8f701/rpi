use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::task::JoinHandle;

use super::{workflow_backend::WorkflowApplicationBackend, workflow_events::{WorkflowForwardingState, event_workflow_id}, Application, ApplicationRuntimeFactory};
use crate::workflow_worktree::{
    CreateWorktreeOptions, IntegrateOptions, IntegrateOutcome, TrustedWorkflowCwd,
    WorkflowWorktreeIdentity, WorkflowWorktreeManager,
};
use crate::{
    OrchestrationConcurrencyGate, OrchestrationRuntime, SessionOptions, TodoState,
    WorkflowIntegration, WorkflowRuntimeFactory, WorkflowRuntimeIdentity, WorkflowRuntimeRequest,
    WorkflowRuntimeUpdate, WorkflowSnapshot, WorkflowStatus, WorkflowRuntimeProjectionSink,
    WorkflowSupervisor, WorkflowSupervisorContract, WorkflowSupervisorEvent,
};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct WorkflowRuntimeKey {
    workflow_id: String,
    generation: u64,
}

impl WorkflowRuntimeKey {
    fn from_request(request: &WorkflowRuntimeRequest) -> Self {
        Self {
            workflow_id: request.workflow_id.as_str().to_owned(),
            generation: request.generation,
        }
    }

    fn from_snapshot(snapshot: &WorkflowSnapshot) -> Self {
        Self {
            workflow_id: snapshot.workflow_id.as_str().to_owned(),
            generation: snapshot.generation,
        }
    }
}

struct WorkflowChildRuntime {
    backend: Arc<WorkflowApplicationBackend>,
    supervisor: WorkflowSupervisor,
    event_task: Mutex<Option<JoinHandle<()>>>,
}

struct WorkflowRegistryEntry {
    identity: Mutex<WorkflowWorktreeIdentity>,
    runtime: Mutex<Option<Arc<WorkflowChildRuntime>>>,
}

/// Application-backed workflow runtime factory with exact worktree and generation ownership.
pub struct ApplicationWorkflowRuntimeFactory {
    worktrees: WorkflowWorktreeManager,
    managed_root: PathBuf,
    runtime_factory: Arc<dyn ApplicationRuntimeFactory>,
    session_options: SessionOptions,
    max_concurrency: usize,
    global_concurrency: OrchestrationConcurrencyGate,
    registry: Mutex<HashMap<WorkflowRuntimeKey, Arc<WorkflowRegistryEntry>>>,
    projection_sink: Mutex<Option<WorkflowRuntimeProjectionSink>>,
}

impl fmt::Debug for ApplicationWorkflowRuntimeFactory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApplicationWorkflowRuntimeFactory")
            .field("max_concurrency", &self.max_concurrency)
            .field("global_concurrency_limit", &self.global_concurrency.limit())
            .field("runtime_count", &self.registry.lock().len())
            .field("projection_sink_attached", &self.projection_sink.lock().is_some())
            .finish_non_exhaustive()
    }
}

impl WorkflowChildRuntime {
    async fn shutdown(&self) -> Result<()> {
        if let Some(task) = self.event_task.lock().take() {
            task.abort();
        }
        Box::pin(self.backend.cleanup()).await
    }
}

impl WorkflowRegistryEntry {
    fn identity(&self) -> WorkflowWorktreeIdentity {
        self.identity.lock().clone()
    }

    fn runtime(&self) -> Option<Arc<WorkflowChildRuntime>> {
        self.runtime.lock().clone()
    }
}

impl ApplicationWorkflowRuntimeFactory {
    pub fn new(
        worktrees: WorkflowWorktreeManager,
        managed_root: impl Into<PathBuf>,
        runtime_factory: Arc<dyn ApplicationRuntimeFactory>,
        session_options: SessionOptions,
        max_concurrency: usize,
        global_concurrency: OrchestrationConcurrencyGate,
    ) -> Result<Self> {
        if max_concurrency == 0 {
            bail!("workflow max concurrency must be greater than zero");
        }
        Ok(Self {
            worktrees,
            managed_root: managed_root.into(),
            runtime_factory,
            session_options,
            max_concurrency,
            global_concurrency,
            registry: Mutex::new(HashMap::new()),
            projection_sink: Mutex::new(None),
        })
    }

    fn attach_projection_sink(&self, sink: WorkflowRuntimeProjectionSink) -> Result<()> {
        let mut current = self.projection_sink.lock();
        if current.is_some() {
            bail!("workflow runtime projection sink is already configured");
        }
        *current = Some(sink);
        Ok(())
    }

    fn projection_sink(&self) -> Result<WorkflowRuntimeProjectionSink> {
        self.projection_sink
            .lock()
            .clone()
            .ok_or_else(|| anyhow!("workflow runtime projection sink is not configured"))
    }

    pub(super) async fn shutdown_all(&self) {
        let runtimes = {
            let mut registry = self.registry.lock();
            let runtimes = registry
                .values()
                .filter_map(|entry| entry.runtime())
                .collect::<Vec<_>>();
            registry.clear();
            runtimes
        };
        for runtime in runtimes {
            // Cleanup is best-effort; child cleanup still runs after drain failures.
            let _ = runtime.shutdown().await;
        }
    }
    fn projection_update(
        projection: crate::WorkflowSupervisorProjection,
        integration: WorkflowIntegration,
    ) -> WorkflowRuntimeUpdate {
        WorkflowRuntimeUpdate {
            status: projection.status,
            todo: projection.todo,
            supervisor_agent_id: Some(projection.supervisor_agent_id),
            supervisor_job_id: None,
            failure: projection.failure.map(|_| crate::WorkflowFailure {
                message: "workflow supervisor failed".to_owned(),
            }),
            integration,
        }
    }

    #[must_use]
    pub fn global_concurrency_gate(&self) -> OrchestrationConcurrencyGate {
        self.global_concurrency.clone()
    }

    #[must_use]
    pub fn child_application(
        &self,
        workflow_id: &crate::WorkflowId,
        generation: u64,
    ) -> Option<Application> {
        let key = WorkflowRuntimeKey {
            workflow_id: workflow_id.as_str().to_owned(),
            generation,
        };
        self.registry
            .lock()
            .get(&key)
            .and_then(|entry| entry.runtime())
            .map(|runtime| runtime.backend.application().clone())
    }

    fn runtime_detail(
        &self,
        snapshot: &WorkflowSnapshot,
    ) -> Option<crate::WorkflowRuntimeDetail> {
        let key = WorkflowRuntimeKey::from_snapshot(snapshot);
        let runtime = self.registry.lock().get(&key)?.runtime()?;
        let projection = runtime.supervisor.projection();
        if projection.workflow_id != snapshot.workflow_id.as_str()
            || projection.generation != snapshot.generation
        {
            return None;
        }
        let orchestration = runtime.backend.orchestration();
        let agents = orchestration.list("");
        let jobs = orchestration.workflow_jobs(snapshot.workflow_id.as_str(), snapshot.generation);
        let mut irc = projection.irc.clone();
        irc.extend(orchestration.inbox(&projection.supervisor_agent_id, true));
        Some(crate::WorkflowRuntimeDetail::from_live(snapshot, projection, agents, jobs, irc))
    }

    fn runtime_options(&self, cwd: &TrustedWorkflowCwd) -> SessionOptions {
        let mut options = self.session_options.clone();
        options.cwd = cwd.path().to_path_buf();
        options
    }

    fn validate_snapshot_identity(
        snapshot: &WorkflowSnapshot,
        identity: &WorkflowWorktreeIdentity,
    ) -> Result<()> {
        if snapshot.workflow_id.as_str() != identity.workflow_id()
            || snapshot.branch.as_deref() != Some(identity.branch())
            || snapshot.worktree_label.as_deref() != Some(identity.workflow_id())
        {
            bail!("workflow runtime identity mismatch");
        }
        Ok(())
    }

    fn same_allocation(
        left: &WorkflowWorktreeIdentity,
        right: &WorkflowWorktreeIdentity,
    ) -> bool {
        left.workflow_id == right.workflow_id
            && left.source_root == right.source_root
            && left.common_git_dir == right.common_git_dir
            && left.worktree_path == right.worktree_path
            && left.branch == right.branch
            && left.base_commit == right.base_commit
            && left.created_at_ms == right.created_at_ms
    }

    fn refresh_entry_identity(
        &self,
        snapshot: &WorkflowSnapshot,
        entry: &WorkflowRegistryEntry,
    ) -> Result<WorkflowWorktreeIdentity> {
        let recorded = entry.identity();
        Self::validate_snapshot_identity(snapshot, &recorded)?;
        let (current, _) = self
            .worktrees
            .verify_owned_current(snapshot.workflow_id.as_str())
            .map_err(|_| anyhow!("workflow worktree ownership verification failed"))?;
        if !Self::same_allocation(&recorded, &current) {
            bail!("workflow runtime allocation changed");
        }
        *entry.identity.lock() = current.clone();
        Ok(current)
    }

    fn exact_entry(&self, snapshot: &WorkflowSnapshot) -> Result<Arc<WorkflowRegistryEntry>> {
        let key = WorkflowRuntimeKey::from_snapshot(snapshot);
        let entry = self
            .registry
            .lock()
            .get(&key)
            .cloned()
            .ok_or_else(|| anyhow!("workflow runtime is not registered"))?;
        self.refresh_entry_identity(snapshot, &entry)?;
        Ok(entry)
    }

    fn exact_runtime(&self, snapshot: &WorkflowSnapshot) -> Result<Arc<WorkflowChildRuntime>> {
        self.exact_entry(snapshot)?
            .runtime()
            .ok_or_else(|| anyhow!("workflow child runtime is not registered"))
    }

    fn ensure_catalog_entry(
        &self,
        snapshot: &WorkflowSnapshot,
    ) -> Result<Arc<WorkflowRegistryEntry>> {
        let key = WorkflowRuntimeKey::from_snapshot(snapshot);
        if let Some(entry) = self.registry.lock().get(&key).cloned() {
            self.refresh_entry_identity(snapshot, &entry)?;
            return Ok(entry);
        }
        let (identity, _) = self
            .worktrees
            .verify_owned_current(snapshot.workflow_id.as_str())
            .map_err(|_| anyhow!("workflow worktree ownership verification failed"))?;
        Self::validate_snapshot_identity(snapshot, &identity)?;
        let entry = Arc::new(WorkflowRegistryEntry {
            identity: Mutex::new(identity),
            runtime: Mutex::new(None),
        });
        let mut registry = self.registry.lock();
        Ok(registry.entry(key).or_insert_with(|| entry.clone()).clone())
    }

    fn runtime_identity(entry: &WorkflowRegistryEntry) -> WorkflowRuntimeIdentity {
        let identity = entry.identity();
        let (supervisor_agent_id, todo) = entry.runtime().map_or_else(
            || {
                (
                    None,
                    TodoState {
                        phases: Vec::new(),
                        storage: crate::TodoStorage::Memory,
                    },
                )
            },
            |runtime| {
                let projection = runtime.supervisor.projection();
                (Some(projection.supervisor_agent_id), projection.todo)
            },
        );
        WorkflowRuntimeIdentity {
            worktree_label: Some(identity.workflow_id().to_owned()),
            branch: Some(identity.branch().to_owned()),
            supervisor_agent_id,
            supervisor_job_id: None,
            todo,
        }
    }

    fn runtime_update(
        runtime: &WorkflowChildRuntime,
        integration: WorkflowIntegration,
    ) -> WorkflowRuntimeUpdate {
        Self::projection_update(runtime.supervisor.projection(), integration)
    }

    async fn build_child(
        &self,
        request: &WorkflowRuntimeRequest,
        identity: WorkflowWorktreeIdentity,
        cwd: TrustedWorkflowCwd,
        restored: Option<&WorkflowSnapshot>,
    ) -> Result<Arc<WorkflowRegistryEntry>> {
        let key = WorkflowRuntimeKey::from_request(request);
        if self.registry.lock().contains_key(&key) {
            bail!("workflow runtime is already registered");
        }
        let candidate = self
            .runtime_factory
            .build_trusted_workflow_candidate(cwd.clone(), self.runtime_options(&cwd))
            .await
            .map_err(|_| anyhow!("workflow child runtime construction failed"))?;
        if let Some(snapshot) = restored {
            candidate
                .session
                .set_todos(snapshot.todo.phases.clone())
                .map_err(|_| anyhow!("workflow Todo restoration failed"))?;
        }
        let orchestration = candidate
            .orchestration_runtime
            .clone()
            .ok_or_else(|| anyhow!("workflow child orchestration is not configured"))?;
        let application = Application::from_runtime_candidate(candidate)
            .await
            .map_err(|_| anyhow!("workflow child Application construction failed"))?;
        let backend = Arc::new(WorkflowApplicationBackend::new(
            application.clone(),
            orchestration.clone(),
            request.workflow_id.as_str().to_owned(),
            request.generation,
        )?);
        let contract = WorkflowSupervisorContract {
            workflow_id: request.workflow_id.as_str().to_owned(),
            generation: request.generation,
            worktree_label: identity.workflow_id().to_owned(),
            objective: request.objective.clone(),
            supervisor_agent_id: orchestration.main_agent_id().to_owned(),
            max_concurrency: self.max_concurrency,
        };
        let supervisor = match restored {
            Some(snapshot) => WorkflowSupervisor::spawn_restored(
                contract,
                backend.clone(),
                self.global_concurrency.clone(),
                snapshot.status,
            )?,
            None => WorkflowSupervisor::spawn(
                contract,
                backend.clone(),
                self.global_concurrency.clone(),
            )?,
        };
        let mut application_events = application.subscribe();
        let event_supervisor = supervisor.clone();
        let generation = request.generation;
        let workflow_id = request.workflow_id.clone();
        let projection_sink = self.projection_sink()?;
        let mut supervisor_events = supervisor.subscribe();
        let event_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    event = application_events.recv() => match event {
                        Ok(event) => {
                            let _ = event_supervisor.observe_application_event(generation, event).await;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                    event = supervisor_events.recv() => match event {
                        Ok(WorkflowSupervisorEvent::ProjectionChanged { projection }) => {
                            let update = ApplicationWorkflowRuntimeFactory::projection_update(projection, WorkflowIntegration::None);
                            let _ = projection_sink.project(&workflow_id, generation, update).await;
                        }
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                }
            }
        });
        let runtime = Arc::new(WorkflowChildRuntime {
            backend,
            supervisor,
            event_task: Mutex::new(Some(event_task)),
        });
        let entry = Arc::new(WorkflowRegistryEntry {
            identity: Mutex::new(identity),
            runtime: Mutex::new(Some(runtime.clone())),
        });
        if self.registry.lock().insert(key.clone(), entry.clone()).is_some() {
            self.registry.lock().remove(&key);
            let _ = runtime.shutdown().await;
            bail!("workflow runtime registration raced");
        }
        Ok(entry)
    }

    async fn rollback_create(
        &self,
        key: &WorkflowRuntimeKey,
        identity: &WorkflowWorktreeIdentity,
    ) {
        let entry = self.registry.lock().remove(key);
        if let Some(runtime) = entry.and_then(|entry| entry.runtime()) {
            let _ = runtime.shutdown().await;
        }
        let _ = self.worktrees.remove(identity);
    }
}



#[async_trait]
impl WorkflowRuntimeFactory for ApplicationWorkflowRuntimeFactory {
    async fn create(&self, request: &WorkflowRuntimeRequest) -> Result<WorkflowRuntimeIdentity> {
        let identity = self.worktrees.create(request.workflow_id.as_str(), CreateWorktreeOptions {
            managed_root: self.managed_root.clone(), base_commit: None, timeout: None,
        }).map_err(|_| anyhow!("workflow worktree creation failed"))?;
        let cwd = match self.worktrees.verify_owned(&identity) {
            Ok(cwd) => cwd,
            Err(_) => { let _ = self.worktrees.remove(&identity); bail!("workflow worktree ownership verification failed"); }
        };
        let key = WorkflowRuntimeKey::from_request(request);
        match self.build_child(request, identity.clone(), cwd, None).await {
            Ok(entry) => {
                let runtime = entry.runtime().expect("new child runtime");
                if let Err(error) = runtime.supervisor.start().await {
                    self.rollback_create(&key, &identity).await;
                    return Err(error);
                }
                let projection = runtime.supervisor.projection();
                if projection.status == WorkflowStatus::Failed {
                    self.rollback_create(&key, &identity).await;
                    bail!("workflow supervisor startup failed");
                }
                if let Err(error) = self
                    .projection_sink()?
                    .project(
                        &request.workflow_id,
                        request.generation,
                        Self::projection_update(projection, WorkflowIntegration::None),
                    )
                    .await
                {
                    self.rollback_create(&key, &identity).await;
                    return Err(error);
                }
                Ok(Self::runtime_identity(&entry))
            }
            Err(error) => { self.rollback_create(&key, &identity).await; Err(error) }
        }
    }

    async fn restore(&self, request: &WorkflowRuntimeRequest, snapshot: &WorkflowSnapshot) -> Result<WorkflowRuntimeIdentity> {
        if WorkflowRuntimeKey::from_request(request) != WorkflowRuntimeKey::from_snapshot(snapshot) { bail!("workflow restore identity mismatch"); }
        let key = WorkflowRuntimeKey::from_request(request);
        if let Some(entry) = self.registry.lock().get(&key).cloned() {
            self.refresh_entry_identity(snapshot, &entry)?;
            return Ok(Self::runtime_identity(&entry));
        }
        let (identity, cwd) = self.worktrees.verify_owned_current(request.workflow_id.as_str()).map_err(|_| anyhow!("workflow worktree recovery failed"))?;
        Self::validate_snapshot_identity(snapshot, &identity)?;
        let entry = self.build_child(request, identity, cwd, Some(snapshot)).await?;
        Ok(Self::runtime_identity(&entry))
    }

    async fn pause(&self, snapshot: &WorkflowSnapshot) -> Result<WorkflowRuntimeUpdate> {
        let runtime = self.exact_runtime(snapshot)?;
        runtime.supervisor.pause().await?;
        Ok(Self::runtime_update(&runtime, snapshot.integration.clone()))
    }

    async fn resume(&self, snapshot: &WorkflowSnapshot) -> Result<WorkflowRuntimeUpdate> {
        let runtime = self.exact_runtime(snapshot)?;
        runtime.supervisor.resume().await?;
        Ok(Self::runtime_update(&runtime, snapshot.integration.clone()))
    }

    async fn cancel(&self, snapshot: &WorkflowSnapshot) -> Result<WorkflowRuntimeUpdate> {
        let runtime = self.exact_runtime(snapshot)?;
        runtime.supervisor.cancel().await?;
        runtime.backend.abort_application().await;
        Ok(Self::runtime_update(&runtime, snapshot.integration.clone()))
    }

    async fn integrate(&self, snapshot: &WorkflowSnapshot) -> Result<WorkflowRuntimeUpdate> {
        let entry = self.ensure_catalog_entry(snapshot)?;
        let outcome = self.worktrees.integrate(snapshot.workflow_id.as_str(), IntegrateOptions::default());
        let (status, integration, failure) = match outcome {
            Ok(IntegrateOutcome::Applied { result_commit, .. }) => (WorkflowStatus::Completed, WorkflowIntegration::Applied { result_commit }, None),
            Ok(IntegrateOutcome::Conflicted { conflicts, .. }) => (WorkflowStatus::Conflicted, WorkflowIntegration::Conflicted { conflicts }, None),
            Err(_) => (WorkflowStatus::Failed, snapshot.integration.clone(), Some(crate::WorkflowFailure { message: "workflow integration failed".to_owned() })),
        };
        let (todo, supervisor_agent_id) = entry.runtime().map_or_else(
            || (snapshot.todo.clone(), snapshot.supervisor_agent_id.clone()),
            |runtime| { let projection = runtime.supervisor.projection(); (projection.todo, Some(projection.supervisor_agent_id)) },
        );
        Ok(WorkflowRuntimeUpdate { status, todo, supervisor_agent_id, supervisor_job_id: snapshot.supervisor_job_id.clone(), failure, integration })
    }

    async fn remove(&self, snapshot: &WorkflowSnapshot) -> Result<()> {
        let key = WorkflowRuntimeKey::from_snapshot(snapshot);
        let entry = self.ensure_catalog_entry(snapshot)?;
        if let Some(runtime) = entry.runtime() { runtime.shutdown().await?; *entry.runtime.lock() = None; }
        let identity = entry.identity();
        self.worktrees.remove(&identity).map_err(|_| anyhow!("workflow worktree removal failed"))?;
        self.registry.lock().remove(&key);
        Ok(())
    }
}
impl Application {
    /// Attach the canonical workflow manager and restore durable non-terminal runtimes.
    pub async fn setup_workflows(
        &self,
        source_cwd: impl Into<PathBuf>,
        store_root: impl Into<PathBuf>,
        managed_root: impl Into<PathBuf>,
    ) -> Result<Arc<ApplicationWorkflowRuntimeFactory>> {
        let runtime_factory = self
            .inner
            .runtime_factory
            .lock()
            .clone()
            .ok_or_else(|| anyhow!("application runtime factory is not configured"))?;
        let source_cwd = source_cwd.into();
        let snapshot = self.runtime().session().child_session_options_snapshot();
        let session_options = SessionOptions {
            model: snapshot.model,
            cwd: source_cwd.clone(),
            system_prompt: String::new(),
            thinking_level: snapshot.thinking_level,
            api_key: snapshot.api_key,
            compaction: None,
            stream_options: snapshot.stream_options,
            tools: None,
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(snapshot.stream_fn),
            auth_resolver: snapshot.auth_resolver,
        };
        let max_concurrency = self.runtime_settings().orchestration_max_concurrency;
        let factory = Arc::new(ApplicationWorkflowRuntimeFactory::new(
            WorkflowWorktreeManager::new(source_cwd),
            managed_root,
            runtime_factory,
            session_options,
            max_concurrency,
            OrchestrationConcurrencyGate::new(max_concurrency)?,
        )?);
        let manager = crate::WorkflowManager::open_with_factory(store_root, factory.clone())?;
        factory.attach_projection_sink(manager.runtime_projection_sink())?;
        self.attach_workflow_manager(manager.clone())?;
        *self.inner.workflow_runtime_factory.lock() = Some(Arc::downgrade(&factory));
        manager.restore_all().await?;
        Ok(factory)
    }

    pub fn attach_workflow_manager(&self, manager: crate::WorkflowManager) -> Result<()> {
        let mut current = self.inner.workflow_manager.lock();
        if current.is_some() {
            bail!("application workflow manager is already configured");
        }
        let mut events = manager.subscribe();
        let event_inner = Arc::downgrade(&self.inner);
        let event_manager = manager.clone();
        let mut forwarded = WorkflowForwardingState::new(manager.list());
        let event_task = tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        if !forwarded.accept(&event, event_manager.get(event_workflow_id(&event)).ok()) { continue; }
                        let Some(inner) = event_inner.upgrade() else { break; };
                        inner.publish(super::ApplicationEvent::Workflow(event));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let Some(inner) = event_inner.upgrade() else { break; };
                        for event in forwarded.reconcile(event_manager.list()) {
                            inner.publish(super::ApplicationEvent::Workflow(event));
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        *current = Some(manager);
        *self.inner.workflow_events.lock() = Some(event_task);
        Ok(())
    }

    pub fn workflow_manager(&self) -> Result<crate::WorkflowManager> {
        self.inner
            .workflow_manager
            .lock()
            .clone()
            .ok_or_else(|| anyhow!("application workflow manager is not configured"))
    }

    #[must_use]
    pub fn workflow_list(&self) -> Vec<crate::WorkflowSnapshot> {
        self.workflow_manager().map_or_else(|_| Vec::new(), |manager| manager.list())
    }

    pub fn workflow_get(&self, workflow_id: &crate::WorkflowId) -> Result<crate::WorkflowSnapshot> {
        self.workflow_manager()?.get(workflow_id)
    }
    /// Replace the canonical Todo DAG for the exact live runtime generation of a workflow.
    pub fn set_workflow_todos(
        &self,
        workflow_id: &crate::WorkflowId,
        phases: Vec<crate::TodoPhase>,
    ) -> Result<crate::TodoApplyResult> {
        let snapshot = self.workflow_get(workflow_id)?;
        let factory = self
            .inner
            .workflow_runtime_factory
            .lock()
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .ok_or_else(|| anyhow!("workflow runtime is unavailable"))?;
        let child = factory
            .child_application(workflow_id, snapshot.generation)
            .ok_or_else(|| anyhow!("workflow runtime is unavailable"))?;
        child.set_todos(phases)
    }


    /// Return the canonical exact-generation snapshot enriched with ephemeral live runtime state.
    pub fn workflow_detail(
        &self,
        workflow_id: &crate::WorkflowId,
        generation: u64,
    ) -> Result<crate::WorkflowRuntimeDetail> {
        let snapshot = self.workflow_get(workflow_id)?;
        if snapshot.generation != generation {
            bail!("workflow generation is stale");
        }
        let factory = self.inner.workflow_runtime_factory.lock().as_ref().and_then(std::sync::Weak::upgrade);
        if let Some(detail) = factory.as_ref().and_then(|factory| factory.runtime_detail(&snapshot)) {
            return Ok(detail);
        }
        if snapshot.status.is_terminal() {
            return Ok(crate::WorkflowRuntimeDetail::snapshot_fallback(&snapshot));
        }
        bail!("workflow runtime is unavailable")
    }

    #[must_use]
    pub fn workflow_selected(&self) -> Option<crate::WorkflowSnapshot> {
        self.workflow_manager().ok().and_then(|manager| manager.selected())
    }

    pub fn workflow_select(
        &self,
        workflow_id: Option<&crate::WorkflowId>,
    ) -> Result<Option<crate::WorkflowSnapshot>> {
        self.workflow_manager()?.select(workflow_id)
    }

    pub async fn workflow_create(
        &self,
        request: crate::WorkflowCreateRequest,
    ) -> Result<crate::WorkflowSnapshot> {
        self.workflow_manager()?.create(request).await
    }

    pub async fn workflow_pause(
        &self,
        workflow_id: &crate::WorkflowId,
        generation: u64,
    ) -> Result<crate::WorkflowSnapshot> {
        self.workflow_manager()?.pause(workflow_id, generation).await
    }

    pub async fn workflow_resume(
        &self,
        workflow_id: &crate::WorkflowId,
        generation: u64,
    ) -> Result<crate::WorkflowSnapshot> {
        self.workflow_manager()?.resume(workflow_id, generation).await
    }

    pub async fn workflow_cancel(
        &self,
        workflow_id: &crate::WorkflowId,
        generation: u64,
    ) -> Result<crate::WorkflowSnapshot> {
        self.workflow_manager()?.cancel(workflow_id, generation).await
    }

    pub async fn workflow_integrate(
        &self,
        workflow_id: &crate::WorkflowId,
        generation: u64,
    ) -> Result<crate::WorkflowSnapshot> {
        self.workflow_manager()?.integrate(workflow_id, generation).await
    }

    pub async fn workflow_remove(
        &self,
        workflow_id: &crate::WorkflowId,
        generation: u64,
    ) -> Result<crate::WorkflowSnapshot> {
        self.workflow_manager()?.remove(workflow_id, generation).await
    }
}
