use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use parking_lot::Mutex;
use pi_agent::AgentEvent;
use pi_ai::{AssistantMessageEvent, ContentBlock};
use serde_json::Value;
use tokio::task::JoinHandle;

use super::{workflow_backend::WorkflowApplicationBackend, workflow_events::{WorkflowForwardingState, event_workflow_id}, Application, ApplicationRuntimeFactory};
use crate::workflow_worktree::{
    CreateWorktreeOptions, IntegrateOptions, IntegrateOutcome, TrustedWorkflowCwd,
    WorkflowIsolation, WorkflowWorktreeIdentity, WorkflowWorktreeManager,
};
use crate::{
    ApplicationEvent, OrchestrationConcurrencyGate, OrchestrationEvent, OrchestrationRuntime,
    SessionOptions, TodoState, WorkflowIntegration, WorkflowRuntimeFactory,
    WorkflowRuntimeIdentity, WorkflowRuntimeProjectionSink, WorkflowRuntimeRequest,
    WorkflowRuntimeScope, WorkflowRuntimeUpdate, WorkflowSnapshot, WorkflowStatus,
    WorkflowSupervisor, WorkflowSupervisorContract, WorkflowSupervisorEvent,
    WorkflowSupervisorTodoObservation,
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
    projection_task: Mutex<Option<JoinHandle<()>>>,
    forwarder_task: Mutex<Option<JoinHandle<()>>>,
}

struct WorkflowRegistryEntry {
    identity: Mutex<WorkflowWorktreeIdentity>,
    runtime: Mutex<Option<Arc<WorkflowChildRuntime>>>,
}

/// Application-backed workflow runtime factory with exact isolation and
/// generation ownership. The isolation backend (git worktree, overlayfs, or
/// none) is selected from `settings.orchestration.isolation`.
pub struct ApplicationWorkflowRuntimeFactory {
    worktrees: Arc<dyn WorkflowIsolation>,
    managed_root: PathBuf,
    runtime_factory: Arc<dyn ApplicationRuntimeFactory>,
    session_options: SessionOptions,
    max_concurrency: usize,
    global_concurrency: OrchestrationConcurrencyGate,
    /// Wall-clock budget (millis) for one workflow planning prompt; 0 keeps
    /// the crate default (90s). Atomic so the factory (held behind `Arc`) can
    /// expose a setter; read per child build.
    planning_deadline_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
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
        if let Some(task) = self.projection_task.lock().take() {
            task.abort();
        }
        if let Some(task) = self.forwarder_task.lock().take() {
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
        worktrees: Arc<dyn WorkflowIsolation>,
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
            planning_deadline_ms: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            registry: Mutex::new(HashMap::new()),
            projection_sink: Mutex::new(None),
        })
    }

    /// Override the wall-clock budget for workflow planning prompts on this
    /// factory (a short deadline lets tests exercise the P0-1 timeout bound).
    pub fn set_workflow_planning_deadline(&self, deadline: Duration) {
        use std::sync::atomic::Ordering;
        self.planning_deadline_ms.store(
            u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
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
            // Surface the supervisor's failure reason (e.g. the actionable
            // "planning produced no tasks" stuck-planning message) while
            // still redacting any secrets the failure text may embed.
            failure: projection.failure.as_deref().map(|message| crate::WorkflowFailure {
                message: crate::redact_value(&serde_json::Value::String(message.to_owned()))
                    .as_str()
                    .map_or_else(|| message.to_string(), str::to_owned),
            }),
            integration,
        }
    }

    /// Whether an `ApplicationEvent` is workflow-relevant for the supervisor
    /// and should be forwarded. Only events the supervisor consumes are
    /// forwarded: `RunFailed`, workflow-scoped `JobUpdated`,
    /// `MessageDelivered`, and `TodoUpdated` (the workflow child's canonical
    /// Todo DAG is the workflow's todo, so the supervisor must observe every
    /// mutation to keep its projection live — including mid-planning, see
    /// BUG-2). Foreign/stale `JobUpdated` (different workflow id or
    /// generation) are filtered here so they never occupy supervisor command
    /// capacity; the supervisor's own generation/terminal guards remain the
    /// authoritative backstop.
    fn supervisor_relevant_event(workflow_id: &str, generation: u64, event: &ApplicationEvent) -> bool {
        match event {
            ApplicationEvent::RunFailed { .. } => true,
            ApplicationEvent::TodoUpdated { .. } => true,
            ApplicationEvent::Orchestration(OrchestrationEvent::JobUpdated { job, .. }) => {
                let foreign_workflow = job
                    .workflow_id
                    .as_deref()
                    .is_some_and(|id| id != workflow_id);
                let foreign_generation = job
                    .workflow_generation
                    .is_some_and(|job_generation| job_generation != generation);
                !(foreign_workflow || foreign_generation)
            }
            ApplicationEvent::Orchestration(OrchestrationEvent::MessageDelivered { .. }) => true,
            _ => false,
        }
    }

    /// Map an agent event of the supervisor's own turn to a coalescable
    /// planning-activity delta: streaming thinking/text chunks and tool-call
    /// starts. Tool calls flush immediately; thinking/text deltas are
    /// accumulated by the forwarder and flushed as bounded chunks (bounded
    /// interval + on the next non-delta event) so the workflow page sees real
    /// movement without a per-token projection storm.
    fn supervisor_activity_delta(event: &ApplicationEvent) -> Option<(crate::WorkflowSupervisorActivityKind, String)> {
        match event {
            ApplicationEvent::Agent(AgentEvent::MessageUpdate {
                assistant_message_event,
                ..
            }) => match assistant_message_event {
                AssistantMessageEvent::ThinkingDelta { delta, .. } => Some((
                    crate::WorkflowSupervisorActivityKind::Thinking,
                    delta.clone(),
                )),
                AssistantMessageEvent::TextDelta { delta, .. } => Some((
                    crate::WorkflowSupervisorActivityKind::Text,
                    delta.clone(),
                )),
                _ => None,
            },
            ApplicationEvent::Agent(AgentEvent::ToolExecutionStart {
                tool_name,
                arguments,
                ..
            }) => Some((
                crate::WorkflowSupervisorActivityKind::Tool,
                Self::tool_activity_summary(tool_name, arguments),
            )),
            _ => None,
        }
    }

    /// One-line bounded summary of a supervisor tool call for the planning
    /// feed: the tool name plus a bounded argument fragment for calls that
    /// carry user-visible intent — `bash` shows the command after the `$`
    /// (first ~40 chars), `goal`/`todo` the operation name, and
    /// `read`/`edit`/`write` the target path. Fragments are control-stripped,
    /// credential-redacted, and capped at 60 chars; the summary is capped
    /// again by the supervisor's activity feed before it reaches the page.
    fn tool_activity_summary(tool_name: &str, arguments: &Value) -> String {
        const FRAGMENT_CAP: usize = 60;
        const BASH_COMMAND_CAP: usize = 40;
        let fragment = match tool_name {
            "bash" => arguments
                .get("command")
                .and_then(Value::as_str)
                .map(|command| command.strip_prefix('$').unwrap_or(command).trim())
                .map(|command| Self::bounded_activity_fragment(command, BASH_COMMAND_CAP)),
            "goal" | "todo" => Self::todo_arguments_fingerprint(arguments)
                .0
                .map(|op| Self::bounded_activity_fragment(&op, FRAGMENT_CAP)),
            "read" | "edit" | "write" => arguments
                .get("path")
                .and_then(Value::as_str)
                .map(|path| Self::bounded_activity_fragment(path, FRAGMENT_CAP)),
            _ => None,
        };
        fragment.map_or_else(
            || tool_name.to_owned(),
            |fragment| format!("{tool_name} · {fragment}"),
        )
    }

    /// Control-strip, credential-redact, and bound one activity fragment.
    fn bounded_activity_fragment(raw: &str, cap: usize) -> String {
        let cleaned: String = raw
            .chars()
            .map(|character| if character.is_control() { ' ' } else { character })
            .collect();
        let redacted = crate::redact_value(&serde_json::Value::String(cleaned.clone()))
            .as_str()
            .map_or_else(|| cleaned, str::to_owned);
        if redacted.chars().count() <= cap {
            return redacted;
        }
        let prefix: String = redacted.chars().take(cap.saturating_sub(3)).collect();
        format!("{prefix}...")
    }

    /// Pair a completed supervisor tool call with its recorded start summary
    /// and mark it `ok`/`err`, so the planning feed's tool rows carry an
    /// outcome instead of only a start. Unpaired starts stay in `pending`
    /// (the same bounded-lifetime pattern as `supervisor_todo_observation`).
    fn supervisor_tool_end_activity(
        event: &ApplicationEvent,
        pending: &mut HashMap<String, String>,
    ) -> Option<String> {
        match event {
            ApplicationEvent::Agent(AgentEvent::ToolExecutionEnd {
                tool_call_id,
                is_error,
                ..
            }) => pending.remove(tool_call_id).map(|summary| {
                format!("{summary} · {}", if *is_error { "err" } else { "ok" })
            }),
            _ => None,
        }
    }

    /// Map a completed `todo` tool call of the supervisor's own turn to a
    /// semantic observation for planning non-progress detection (P1-2). The
    /// `todo` tool's operation name and target IDs come from the call start
    /// (paired by `tool_call_id`); the error state and normalized error
    /// prefix come from the completion. Todo-state change is evaluated by the
    /// supervisor against its canonical projection, so the observation itself
    /// carries no state.
    fn supervisor_todo_observation(
        event: &ApplicationEvent,
        pending: &mut HashMap<String, (Option<String>, Vec<String>)>,
    ) -> Option<WorkflowSupervisorTodoObservation> {
        match event {
            ApplicationEvent::Agent(AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                arguments,
                ..
            }) if tool_name == "todo" => {
                let (op, target_ids) = Self::todo_arguments_fingerprint(arguments);
                pending.insert(tool_call_id.clone(), (op, target_ids));
                None
            }
            ApplicationEvent::Agent(AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            }) if tool_name == "todo" => {
                let (op, target_ids) = pending.remove(tool_call_id).unwrap_or_default();
                let error_prefix = (*is_error).then(|| Self::todo_error_prefix(result)).flatten();
                Some(WorkflowSupervisorTodoObservation {
                    op,
                    target_ids,
                    is_error: *is_error,
                    error_prefix,
                })
            }
            _ => None,
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

    /// Extract the normalized Todo operation name and target task/dependency
    /// IDs (plus `init` items) from a `todo` tool-call's arguments.
    fn todo_arguments_fingerprint(arguments: &Value) -> (Option<String>, Vec<String>) {
        let op = arguments
            .get("op")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let mut target_ids = Vec::new();
        if let Some(task) = arguments.get("task").and_then(Value::as_str) {
            target_ids.push(task.to_owned());
        }
        if let Some(dependencies) = arguments.get("dependsOn").and_then(Value::as_array) {
            target_ids.extend(dependencies.iter().filter_map(Value::as_str).map(str::to_owned));
        }
        if let Some(items) = arguments.get("items").and_then(Value::as_array) {
            target_ids.extend(items.iter().filter_map(Value::as_str).map(str::to_owned));
        }
        if let Some(list) = arguments.get("list").and_then(Value::as_array) {
            for phase in list {
                if let Some(items) = phase.get("items").and_then(Value::as_array) {
                    target_ids.extend(items.iter().filter_map(Value::as_str).map(str::to_owned));
                }
            }
        }
        (op, target_ids)
    }

    /// Normalized, bounded error prefix of a failed `todo` tool result.
    fn todo_error_prefix(result: &pi_agent::AgentToolResult) -> Option<String> {
        let text = result
            .content
            .iter()
            .find_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })?;
        Some(text.chars().take(120).collect())
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
        // The workflow page's Recent IRC reads the group's delivered-message
        // log — every durably delivered message (subagent ⇄ subagent
        // included), independent of mailbox consumption — so messages stay
        // visible after the recipient reads them. The supervisor projection's
        // own inbox is redundant with that log and is not re-added.
        let irc = orchestration.delivered_messages();
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
        // Stamp the orchestration as workflow-scoped BEFORE the candidate is
        // attached to an Application: the todo mutation commit hook and the
        // attach-time DAG arm consult this scope to decide whether the
        // workflow child may auto-arm DAG execution (it may not — the
        // supervisor owns DAG execution for workflow children, see BUG-1).
        // The later configure_workflow_runtime call re-validates the same
        // scope and is idempotent while no jobs are active.
        orchestration.set_workflow_scope(WorkflowRuntimeScope {
            workflow_id: request.workflow_id.as_str().to_owned(),
            generation: request.generation,
        })?;
        let application = Application::from_runtime_candidate(candidate)
            .await
            .map_err(|_| anyhow!("workflow child Application construction failed"))?;
        let backend = Arc::new(WorkflowApplicationBackend::new(
            application.clone(),
            orchestration.clone(),
            request.workflow_id.as_str().to_owned(),
            request.generation,
            self.planning_deadline_ms.load(std::sync::atomic::Ordering::Relaxed),
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
        let forward_supervisor = supervisor.clone();
        let generation = request.generation;
        let workflow_id = request.workflow_id.clone();
        let projection_sink = self.projection_sink()?;
        let mut supervisor_events = supervisor.subscribe();
        let projection_workflow_id = workflow_id.clone();

        // Projection forwarding runs in its own task so a blocked supervisor
        // request (e.g. `prompt_supervisor` awaiting the entire planning turn
        // via `wait_for_idle`) can never starve `ProjectionChanged` delivery
        // to the projection sink. The TUI relies on these updates to leave
        // "queued"/"planning" while planning is in flight.
        let projection_task = tokio::spawn(async move {
            loop {
                match supervisor_events.recv().await {
                    Ok(WorkflowSupervisorEvent::ProjectionChanged { projection }) => {
                        let update = ApplicationWorkflowRuntimeFactory::projection_update(
                            projection,
                            WorkflowIntegration::None,
                        );
                        let _ = projection_sink
                            .project(&projection_workflow_id, generation, update)
                            .await;
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        // Application event forwarding is bounded and ordered: a single
        // forwarder drains the application broadcast serially and awaits
        // supervisor observation in arrival order (no per-event spawn). Only
        // events the supervisor consumes (`RunFailed`, workflow-scoped
        // `JobUpdated`, `MessageDelivered`, `TodoUpdated`) are forwarded;
        // foreign/stale `JobUpdated` are filtered here so they never occupy
        // supervisor command capacity. When the supervisor actor is busy, this
        // forwarder blocks on the supervisor command channel and the
        // application broadcast backpressures by lagging, coalescing
        // unconsumed events. The projection task above is independent and
        // keeps the projection sink live during the stall.
        //
        // The supervisor's own turn additionally streams live planning
        // activity (thinking/text chunks, tool calls) into the projection so
        // the workflow page never reads as a static spinner. Thinking/text
        // deltas are per-token and are coalesced here into bounded chunks:
        // flushed on a 1s tick while accumulating, on the next forwarded
        // event, or when a chunk size cap is reached — never one projection
        // per token.
        let forwarder_task = tokio::spawn(async move {
            const ACTIVITY_FLUSH_INTERVAL: Duration = Duration::from_secs(1);
            const ACTIVITY_CHUNK_CHARS: usize = 600;
            let mut activity_kind = crate::WorkflowSupervisorActivityKind::Thinking;
            let mut activity_buffer = String::new();
            // Pairs in-flight `todo` tool calls (start args) with their
            // completion so the supervisor gets one semantic observation per
            // call for non-progress detection (P1-2).
            let mut todo_pending: HashMap<String, (Option<String>, Vec<String>)> =
                HashMap::new();
            // Start summaries of in-flight supervisor tool calls, paired by
            // tool_call_id so the matching ToolExecutionEnd can append an
            // ok/err outcome to the same bounded tool row.
            let mut tool_pending: HashMap<String, String> = HashMap::new();
            let mut flush_ticker = tokio::time::interval(ACTIVITY_FLUSH_INTERVAL);
            flush_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first interval tick fires immediately; discard it so the
            // buffer only flushes after a full interval of accumulation.
            flush_ticker.tick().await;
            loop {
                tokio::select! {
                    event = application_events.recv() => {
                        match event {
                            Ok(event) => {
                                if let Some(observation) =
                                    ApplicationWorkflowRuntimeFactory::supervisor_todo_observation(
                                        &event,
                                        &mut todo_pending,
                                    )
                                {
                                    let _ = forward_supervisor
                                        .observe_todo_observation(generation, observation)
                                        .await;
                                }
                                if let Some((kind, delta)) =
                                    ApplicationWorkflowRuntimeFactory::supervisor_activity_delta(&event)
                                {
                                    // Tool calls are milestones: flush any
                                    // accumulated text and deliver them
                                    // immediately. Thinking/text chunks
                                    // accumulate until a flush boundary.
                                    if kind == crate::WorkflowSupervisorActivityKind::Tool {
                                        // Remember the delivered start summary
                                        // so the tool's end can append ok/err.
                                        if let ApplicationEvent::Agent(AgentEvent::ToolExecutionStart { tool_call_id, .. }) = &event {
                                            tool_pending.insert(tool_call_id.clone(), delta.clone());
                                        }
                                        if !activity_buffer.is_empty() {
                                            let _ = forward_supervisor
                                                .observe_activity(generation, activity_kind, std::mem::take(&mut activity_buffer))
                                                .await;
                                        }
                                        let _ = forward_supervisor
                                            .observe_activity(generation, kind, delta)
                                            .await;
                                    } else {
                                        activity_kind = kind;
                                        activity_buffer.push_str(&delta);
                                        if activity_buffer.chars().count() >= ACTIVITY_CHUNK_CHARS {
                                            let _ = forward_supervisor
                                                .observe_activity(generation, activity_kind, std::mem::take(&mut activity_buffer))
                                                .await;
                                        }
                                    }
                                    continue;
                                }
                                if let Some(outcome) = ApplicationWorkflowRuntimeFactory::supervisor_tool_end_activity(&event, &mut tool_pending) {
                                    let _ = forward_supervisor
                                        .observe_activity(generation, crate::WorkflowSupervisorActivityKind::Tool, outcome)
                                        .await;
                                    continue;
                                }
                                if !activity_buffer.is_empty() {
                                    let _ = forward_supervisor
                                        .observe_activity(generation, activity_kind, std::mem::take(&mut activity_buffer))
                                        .await;
                                }
                                if !ApplicationWorkflowRuntimeFactory::supervisor_relevant_event(
                                    workflow_id.as_str(),
                                    generation,
                                    &event,
                                ) {
                                    continue;
                                }
                                let _ = forward_supervisor
                                    .observe_application_event(generation, event)
                                    .await;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                    _ = flush_ticker.tick() => {
                        if !activity_buffer.is_empty() {
                            let _ = forward_supervisor
                                .observe_activity(generation, activity_kind, std::mem::take(&mut activity_buffer))
                                .await;
                        }
                    }
                }
            }
        });

        let runtime = Arc::new(WorkflowChildRuntime {
            backend,
            supervisor,
            projection_task: Mutex::new(Some(projection_task)),
            forwarder_task: Mutex::new(Some(forwarder_task)),
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
                // Start the supervisor asynchronously. The first model call
                // (planning turn) can take 10+ seconds; awaiting it inline
                // leaves the TUI stuck on "Creating workflow…". The
                // supervisor's event loop updates projections (Planning →
                // Running/Failed) through the projection sink, so the TUI
                // sees status changes live without blocking creation.
                let supervisor = runtime.supervisor.clone();
                tokio::spawn(async move {
                    let _ = supervisor.start().await;
                });
                let projection = runtime.supervisor.projection();
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
            Err(error) => {
                self.rollback_create(&key, &identity).await;
                Err(error)
            }
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
        // A restored non-Paused runtime must never come back frozen: Planning
        // continues the bounded planning flow, Running re-arms Todo DAG
        // execution over the stored tasks, Queued starts. The supervisor's
        // event loop pushes the resulting status/todo through the projection
        // sink, so the durable record updates without blocking restore.
        if !matches!(snapshot.status, WorkflowStatus::Paused) {
            if let Some(runtime) = entry.runtime() {
                let supervisor = runtime.supervisor.clone();
                tokio::spawn(async move {
                    let _ = supervisor.continue_restored().await;
                });
            }
        }
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
        let isolation = self.runtime_settings().orchestration_isolation;
        let managed_root = managed_root.into();
        // Isolation backend selection (`settings.orchestration.isolation`):
        // git worktree (default), overlayfs (source repo as the read-only
        // lower layer), or none (workflows operate directly on the source
        // working tree).
        let worktrees: Arc<dyn WorkflowIsolation> = match isolation {
            crate::WorkflowIsolationSetting::Worktree => {
                Arc::new(WorkflowWorktreeManager::new(source_cwd.clone()))
            }
            crate::WorkflowIsolationSetting::Overlayfs => Arc::new(
                crate::workflow_worktree::OverlayWorkflowManager::new(
                    source_cwd.clone(),
                    managed_root.clone(),
                ),
            ),
            crate::WorkflowIsolationSetting::None => Arc::new(
                crate::workflow_worktree::NoopWorkflowIsolation::new(source_cwd.clone()),
            ),
        };
        let factory = Arc::new(ApplicationWorkflowRuntimeFactory::new(
            worktrees,
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

    /// Rebind the canonical workflow manager + worktree roots to a new
    /// session identity. Shuts down the current manager and runtime factory
    /// (cancelling/cleaning every live child runtime), then re-runs
    /// [`Self::setup_workflows`] against the given roots.
    ///
    /// Session-scoped isolation: `new_session`/`switch`/`fork` change the
    /// session id, and the workflow store + managed worktrees are namespaced
    /// by that id — a fresh session must start with an empty workflow list
    /// while a resumed session (same id) restores its own workflows. Call
    /// this after the session cutover commits with roots computed from the
    /// NEW session id (see `workflow_storage_roots` in pi-cli).
    pub async fn rebind_workflows(
        &self,
        store_root: impl Into<PathBuf>,
        managed_root: impl Into<PathBuf>,
    ) -> Result<Arc<ApplicationWorkflowRuntimeFactory>> {
        // Teardown mirrors `Application::cleanup`: abort the manager event
        // forwarder first, then shut down every live child runtime through
        // the factory, then drop the manager so a rebind never restores the
        // old session's workflows or leaks its runtimes.
        if let Some(task) = self.inner.workflow_events.lock().take() {
            task.abort();
        }
        let workflow_factory = self
            .inner
            .workflow_runtime_factory
            .lock()
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        if let Some(factory) = workflow_factory {
            factory.shutdown_all().await;
        }
        *self.inner.workflow_runtime_factory.lock() = None;
        *self.inner.workflow_manager.lock() = None;
        let source_cwd = self.runtime().session().cwd().to_path_buf();
        self.setup_workflows(source_cwd, store_root, managed_root).await
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
        let result = child.set_todos(phases)?;
        child.execute_todo_dag()?;
        Ok(result)
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

#[cfg(test)]
mod tests {
    use super::*;
    use pi_agent::AgentToolResult;
    use serde_json::json;

    fn tool_start(tool_call_id: &str, tool_name: &str, arguments: Value) -> ApplicationEvent {
        ApplicationEvent::Agent(AgentEvent::ToolExecutionStart {
            tool_call_id: tool_call_id.to_owned(),
            tool_name: tool_name.to_owned(),
            arguments,
        })
    }

    fn tool_end(tool_call_id: &str, tool_name: &str, is_error: bool) -> ApplicationEvent {
        ApplicationEvent::Agent(AgentEvent::ToolExecutionEnd {
            tool_call_id: tool_call_id.to_owned(),
            tool_name: tool_name.to_owned(),
            result: AgentToolResult::default(),
            is_error,
        })
    }

    /// The delta text of a tool-start event, panicking unless it is one.
    fn tool_delta(event: &ApplicationEvent) -> String {
        match ApplicationWorkflowRuntimeFactory::supervisor_activity_delta(event) {
            Some((crate::WorkflowSupervisorActivityKind::Tool, text)) => text,
            other => panic!("expected a tool delta, got {other:?}"),
        }
    }

    #[test]
    fn bash_tool_start_records_bounded_command_summary() {
        // A short command is shown verbatim after the leading `$`.
        let short = tool_start("call-1", "bash", json!({ "command": "$cargo build --release" }));
        assert_eq!(tool_delta(&short), "bash · cargo build --release");

        // A long command is cut to the first ~40 chars with an ellipsis.
        let long = tool_start(
            "call-2",
            "bash",
            json!({ "command": "$cargo +1.88.0 test -p pi-coding --lib --quiet && cargo +1.88.0 test -p pi-cli --lib --quiet && git diff --check" }),
        );
        assert_eq!(tool_delta(&long), "bash · cargo +1.88.0 test -p pi-coding --lib...");
    }

    #[test]
    fn tool_start_summary_redacts_secrets_and_strips_control() {
        // Credential-shaped content is redacted before it reaches the page.
        let token = ["s", "k-", "abc1234567890def"].concat();
        let secret = tool_start(
            "call-1",
            "bash",
            json!({ "command": format!("$curl -H 'Authorization: Bearer {token}' https://api.example.com") }),
        );
        let summary = tool_delta(&secret);
        assert!(summary.contains("[REDACTED]"), "secret must be redacted: {summary}");
        assert!(!summary.contains(token.as_str()), "token must not leak: {summary}");

        // Control characters are blanked so the row stays on one line.
        let control = tool_start("call-2", "bash", json!({ "command": "$printf 'a\\nb'" }));
        let summary = tool_delta(&control);
        assert!(!summary.contains('\n'), "control chars must not reach the feed: {summary:?}");
        assert!(summary.contains(' '), "control chars must be blanked: {summary:?}");
    }

    #[test]
    fn tool_start_summary_read_path_and_todo_op() {
        let read = tool_start("call-1", "read", json!({ "path": "crates/pi-cli/src/workflow_panel.rs" }));
        assert_eq!(tool_delta(&read), "read · crates/pi-cli/src/workflow_panel.rs");

        let write = tool_start("call-2", "write", json!({ "path": "src/lib.rs" }));
        assert_eq!(tool_delta(&write), "write · src/lib.rs");

        let todo = tool_start("call-3", "todo", json!({ "op": "add", "task": "design-id" }));
        assert_eq!(tool_delta(&todo), "todo · add");

        let goal = tool_start("call-4", "goal", json!({ "op": "extend" }));
        assert_eq!(tool_delta(&goal), "goal · extend");

        // Tools without a user-visible argument fragment keep the plain name.
        let generic = tool_start("call-5", "web_search", json!({ "query": "rust" }));
        assert_eq!(tool_delta(&generic), "web_search");
    }

    #[test]
    fn tool_end_pairs_start_summary_with_ok_err_outcome() {
        let mut pending = HashMap::new();
        pending.insert("call-1".to_owned(), "bash · cargo build --release".to_owned());

        let ok = tool_end("call-1", "bash", false);
        assert_eq!(
            ApplicationWorkflowRuntimeFactory::supervisor_tool_end_activity(&ok, &mut pending).as_deref(),
            Some("bash · cargo build --release · ok"),
        );
        assert!(pending.is_empty(), "paired end must remove the start");

        // Errors are marked, and unpaired ends yield nothing.
        pending.insert("call-2".to_owned(), "read · src/lib.rs".to_owned());
        let err = tool_end("call-2", "read", true);
        assert_eq!(
            ApplicationWorkflowRuntimeFactory::supervisor_tool_end_activity(&err, &mut pending).as_deref(),
            Some("read · src/lib.rs · err"),
        );
        let orphan = tool_end("call-3", "bash", false);
        assert_eq!(
            ApplicationWorkflowRuntimeFactory::supervisor_tool_end_activity(&orphan, &mut pending),
            None,
        );
    }
}
