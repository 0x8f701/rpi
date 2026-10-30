pub(crate) mod generation;
mod runtime;
mod side_chat;
mod todo_child;
mod todo_execution;
mod workflow_backend;
mod workflow_events;
mod workflows;
pub use runtime::ApplicationRuntimeCandidate;
pub use side_chat::{
    SideChatFork, SideChatMainPeek, bind_code_review_orchestration_tools, create_peek_main_tool,
    filter_tools_by_capabilities, tools_include_mutation,
};
pub use todo_child::{
    TodoChildAddTaskRequest, TodoChildAddTaskResult, TodoChildClient, TodoChildCreateRequest,
    TodoChildCreateResult, TodoChildDetailActivity, TodoChildDetailActivityKind,
    TodoChildDetailAgent, TodoChildDetailIrcMessage, TodoChildDetailJob, TodoChildDetailSnapshot,
    TodoChildDetailTranscriptEntry, TodoChildIdentity, TodoChildKind, TodoChildManifest,
    TodoChildManifestEntry, TodoChildParentProvenance, TodoChildReceipt, TodoChildReceiptStatus,
    TodoChildService, TodoChildSnapshot, TodoChildStatus, TodoChildTask, TodoChildTaskStatus,
    TodoChildWorkspaceIdentity,
};
pub use todo_execution::TodoDagExecutionOutcome;
use todo_execution::{TodoDagCoordinatorState, persist_coordinator_status_for_runtime};
pub use crate::{TodoDagExecutionStatus, TodoDagFailureReason};
pub use workflows::ApplicationWorkflowRuntimeFactory;

use std::{
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use parking_lot::Mutex;
use pi_agent::{
    AgentEvent, AgentTool, AgentToolResult, ThinkingLevel, ToolCapability, compose_before_tool_call,
    queued_user_previews,
};
use pi_ai::{ContentBlock, CustomMessage, Message, Model, Schema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    sync::{Mutex as AsyncMutex, OwnedMutexGuard, broadcast, watch},
    task::JoinHandle,
};

pub use generation::ApplicationGeneration;
use generation::{
    ApplicationGenerationRevision, ApplicationPublication,
    BootstrapGenerationLease, DurableApplicationComponents, DurableGenerationLease,
    ExtensionGeneration, GenerationCaptureFn, OrchestrationGeneration, PostPublishActions,
    RuntimeGeneration, SecurityGeneration,
};

use crate::{
    CompactionReason, ExtensionActionHost, ExtensionCancellation, ExtensionCommandDescriptor,
    ExtensionContextSnapshot, ExtensionContextUsage, ExtensionEvent, ExtensionFuture,
    ExtensionInstanceId, ExtensionMessageDelivery, ExtensionPermissionSet, ExtensionRuntime,
    ExtensionRuntimeAction, Goal, GoalContinuationDecision, GoalError, GoalEvent, GoalState,
    GoalUsageDelta, Handoff, MessageDelivery, ProcessEvent, ProcessId, ProcessInfo, ProcessKey,
    ProcessLogs, ProcessManager, ProcessOwnerId, ProcessSignal, ProcessSpawnSpec,
    ProcessTerminalSize, Session, SessionEvent,
};

const EVENT_BUFFER_CAPACITY: usize = 512;
const GOAL_CONTINUATION_CUSTOM_TYPE: &str = "pi.goal.continue";
/// Bound on how long `run_loop_turn` waits for the pinned provider run to
/// settle after the loop iteration token wins the cancel race. The token is
/// already cancelled, so the scheduler classifies every outcome in this
/// window as `RunResult::Cancelled` (no `LoopEvent::Failed`); on expiry the
/// pinned run is dropped and the existing `ClaimedRun`/`Session` Drop cleanup
/// releases the run slot. Mirrors `CHILD_ABORT_GRACE` for the same reason:
/// a stalled provider must not hold the loop's `ActiveRun` slot or the
/// parent turn forever. A normal abort settles well inside this bound.
const LOOP_CANCEL_SETTLE_GRACE: std::time::Duration = std::time::Duration::from_secs(2);
struct StagedImageGenerationHttp {
    model: Model,
    base_url: Option<String>,
    env: HashMap<String, String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamingBehavior {
    Steer,
    FollowUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionAlreadyProcessing;

impl SessionAlreadyProcessing {
    pub const CODE: &'static str = "session_processing";

    pub const fn code(self) -> &'static str {
        Self::CODE
    }
}

impl fmt::Display for SessionAlreadyProcessing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("session is already processing; choose steer or followUp")
    }
}

impl std::error::Error for SessionAlreadyProcessing {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnTerminalStatus {
    Succeeded,
    Failed,
    Aborted,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnAccepted {
    pub turn_id: String,
    pub model: Option<Model>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnLifecycleSnapshot {
    pub revision: u64,
    pub active_turn_id: Option<String>,
    pub active_turn_model: Option<Model>,
    pub last_terminal: Option<TurnTerminalSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnTerminalSnapshot {
    pub turn_id: String,
    pub status: TurnTerminalStatus,
    pub message: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalActivationOutcome {
    Started,
    Queued,
    AlreadyActive,
}

/// Outcome shared by lifecycle operations that either complete or are cancelled by an extension.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionChangeOutcome {
    pub cancelled: bool,
    /// User-facing notice when a resume completed with orchestration left
    /// unbound after a durable policy mismatch. `None` on a normal change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

impl SessionChangeOutcome {
    const COMPLETED: Self = Self {
        cancelled: false,
        notice: None,
    };
    const CANCELLED: Self = Self {
        cancelled: true,
        notice: None,
    };

    fn completed_with_notice(notice: String) -> Self {
        Self {
            cancelled: false,
            notice: Some(notice),
        }
    }
}

/// Outcome of a fork operation, including the selected prompt text on success.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionForkOutcome {
    pub text: String,
    pub cancelled: bool,
}

impl SessionForkOutcome {
    fn completed(text: String) -> Self {
        Self {
            text,
            cancelled: false,
        }
    }

    fn cancelled() -> Self {
        Self {
            text: String::new(),
            cancelled: true,
        }
    }
}

enum OrchestrationCutover {
    Failed(anyhow::Error),
    Unbound(String),
}

fn durable_resume_error(error: &anyhow::Error) -> Option<&crate::DurableResumeError> {
    crate::DurableResumeError::find_in_chain(error)
}

fn durable_resume_notice(error: &anyhow::Error) -> String {
    crate::DurableResumeError::degrade_notice(error)
}

enum PreparedOrchestrationRuntime {
    Fresh(crate::OrchestrationRuntime),
    Resident(ResidentOrchestrationClaim),
}

impl PreparedOrchestrationRuntime {
    fn runtime(&self) -> &crate::OrchestrationRuntime {
        match self {
            Self::Fresh(runtime) => runtime,
            Self::Resident(claim) => claim.runtime(),
        }
    }

    fn into_runtime(self) -> crate::OrchestrationRuntime {
        match self {
            Self::Fresh(runtime) => runtime,
            Self::Resident(claim) => claim.commit().runtime,
        }
    }
}

struct PreparedOrchestrationCutover {
    runtime: PreparedOrchestrationRuntime,
    binding: crate::PreparedDurableBinding,
}

struct PreparedSameCwdCutover {
    session: crate::session::PreparedSessionReplacementCommit,
    orchestration: Option<PreparedOrchestrationCutover>,
    loop_handle: crate::LoopSchedulerHandle,
    loops: crate::loop_scheduler::PreparedLoopSwitchReservation,
    notice: Option<String>,
}

struct RecoveredParentRevival {
    runtime: crate::OrchestrationRuntime,
}

struct PreparedGoalToolBinding {
    binding: GoalToolBinding,
    application: Weak<ApplicationInner>,
    committed: bool,
}

impl PreparedGoalToolBinding {
    fn new(binding: GoalToolBinding, application: &Application) -> Result<Self> {
        binding.bind(application)?;
        Ok(Self {
            binding,
            application: Arc::downgrade(&application.inner),
            committed: false,
        })
    }

    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for PreparedGoalToolBinding {
    fn drop(&mut self) {
        if !self.committed {
            self.binding.unbind(&self.application);
        }
    }
}

struct PreparedTodoChildToolBinding {
    binding: TodoChildToolBinding,
    application: Weak<ApplicationInner>,
    committed: bool,
}

impl PreparedTodoChildToolBinding {
    fn new(binding: TodoChildToolBinding, application: &Application) -> Result<Self> {
        binding.bind(application)?;
        Ok(Self {
            binding,
            application: Arc::downgrade(&application.inner),
            committed: false,
        })
    }

    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for PreparedTodoChildToolBinding {
    fn drop(&mut self) {
        if !self.committed {
            self.binding.unbind_ptr(self.application.as_ptr());
        }
    }
}

struct PreparedRuntimeGenerationBindings {
    orchestration: Option<(crate::OrchestrationRuntime, crate::PreparedDurableBinding)>,
    goal: Option<PreparedGoalToolBinding>,
    todo_child: Option<PreparedTodoChildToolBinding>,
}
impl PreparedRuntimeGenerationBindings {
    fn install_orchestration(&mut self) -> Option<crate::OrchestrationRuntime> {
        let (orchestration, binding) = self.orchestration.take()?;
        orchestration.install_prepared_parent(binding);
        Some(orchestration)
    }

    fn commit_non_orchestration(mut self) {
        if let Some(goal) = self.goal.take() {
            goal.commit();
        }
        if let Some(todo_child) = self.todo_child.take() {
            todo_child.commit();
        }
    }
}

#[doc(hidden)]
#[derive(Clone, Default)]
pub struct ApplicationCutoverTestGate {
    entered: tokio_util::sync::CancellationToken,
    release: tokio_util::sync::CancellationToken,
}

impl ApplicationCutoverTestGate {
    pub async fn wait_entered(&self) {
        self.entered.cancelled().await;
    }

    pub fn release(&self) {
        self.release.cancel();
    }

    async fn enter(&self) {
        self.entered.cancel();
        self.release.cancelled().await;
    }
}

#[cfg(test)]
#[derive(Clone, Default)]
struct ApplicationCleanupTestGate {
    entered: tokio_util::sync::CancellationToken,
    release: tokio_util::sync::CancellationToken,
    fail: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(test)]
impl ApplicationCleanupTestGate {
    async fn wait_entered(&self) {
        self.entered.cancelled().await;
    }

    fn release(&self) {
        self.release.cancel();
    }

    fn fail_on_release(&self) {
        self.fail.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn should_fail(&self) -> bool {
        self.fail.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn enter(&self) {
        self.entered.cancel();
        self.release.cancelled().await;
    }
}
struct CleanupFlight {
    outcome: watch::Receiver<Option<Result<(), String>>>,
}

async fn wait_for_cleanup_outcome(
    outcome: &mut watch::Receiver<Option<Result<(), String>>>,
) -> Result<()> {
    loop {
        if let Some(settled) = outcome.borrow_and_update().as_ref() {
            return settled.clone().map_err(anyhow::Error::msg);
        }
        if outcome.changed().await.is_err() {
            if let Some(settled) = outcome.borrow_and_update().as_ref() {
                return settled.clone().map_err(anyhow::Error::msg);
            }
            bail!("application cleanup task was lost before completion");
        }
    }
}

struct TodoSessionTransitionGuard {
    application: Application,
    previous_status: TodoDagExecutionStatus,
    previous_failure: Option<TodoDagFailureReason>,
    previous_coordinator: TodoDagCoordinatorState,
    previous_suppressed: bool,
    armed: bool,
}


impl TodoSessionTransitionGuard {
    fn commit(mut self) {
        self.application.finish_todo_session_transition(true);
        self.armed = false;
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for TodoSessionTransitionGuard {
    fn drop(&mut self) {
        if self.armed {
            self.application.abort_todo_session_transition(
                self.previous_status,
                self.previous_failure.clone(),
                self.previous_coordinator.clone(),
                self.previous_suppressed,
            );
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationState {
    pub model: Option<Model>,
    pub thinking_level: ThinkingLevel,
    pub is_streaming: bool,
    pub is_compacting: bool,
    pub steering_mode: pi_agent::QueueMode,
    pub follow_up_mode: pi_agent::QueueMode,
    pub session_file: Option<String>,
    pub session_id: Option<String>,
    pub session_name: Option<String>,
    pub auto_compaction_enabled: bool,
    pub message_count: usize,
    pub pending_message_count: usize,
    #[serde(default)]
    pub skipped_idless: usize,
    #[serde(default)]
    pub undecodable_messages: usize,
    #[serde(default)]
    pub queued_steering: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_selection: Option<crate::SelectionPlan>,
    pub todo_phases: Vec<crate::TodoPhase>,
    pub todo_execution_status: TodoDagExecutionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todo_failure_reason: Option<TodoDagFailureReason>,
    pub goal: GoalState,
}
#[derive(Clone, Debug)]
pub struct ApplicationProtocolSnapshot {
    pub epoch: u64,
    pub event_barrier: u64,
    pub state: ApplicationState,
    pub active_turn_model: Option<Model>,
    pub lifecycle: TurnLifecycleSnapshot,
    pub messages: Vec<Message>,
    pub cwd: PathBuf,
    pub runtime_settings: crate::RuntimeSettingsSnapshot,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandSourceInfo {
    pub path: String,
    pub source: String,
    pub scope: String,
    pub origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_dir: Option<String>,
}

impl CommandSourceInfo {
    fn local(path: &Path, scope: &str) -> Self {
        Self {
            path: path.to_string_lossy().into_owned(),
            source: "local".to_owned(),
            scope: scope.to_owned(),
            origin: "top-level".to_owned(),
            base_dir: path
                .parent()
                .map(|parent| parent.to_string_lossy().into_owned()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationCommand {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub source: String,
    pub source_info: CommandSourceInfo,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingApplyOutcome {
    pub writes: Vec<crate::SettingWriteResult>,
    pub applied_live: bool,
    pub reloaded: bool,
    pub restart_required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reload_generation: Option<u64>,
}

impl SettingApplyOutcome {
    #[must_use]
    pub fn changed(&self) -> bool {
        !self.writes.is_empty()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStartedEvent {
    #[serde(rename = "type")]
    pub record_type: String,
    pub version: u32,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeTreeEvent {
    pub target_id: String,
    pub summarize: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTreeEvent {
    pub target_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_leaf_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_entry_id: Option<String>,
    pub changed: bool,
    pub cancelled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeForkEvent {
    pub target_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionForkedEvent {
    pub target_id: String,
    pub session_id: String,
    pub session_file: String,
    pub editor_text: String,
}

#[derive(Clone, Debug)]
pub enum ApplicationEvent {
    RuntimeChanged {
        epoch: u64,
    },
    SessionStarted(SessionStartedEvent),
    Session(SessionEvent),
    Agent(AgentEvent),
    /// Real Session-agent publication. `epoch` is the exact runtime
    /// generation that owns the event and `barrier` is the event's exact
    /// runtime-relative Agent ordinal (post-history session count minus the
    /// generation base), so snapshot cuts and protocol projection condition
    /// coverage by the `(epoch, barrier)` pair rather than a barrier that can
    /// recur across generations. Serialization is identical to
    /// [`ApplicationEvent::Agent`]: both fields are protocol-internal and
    /// never cross the wire.
    AgentPublished {
        event: AgentEvent,
        epoch: u64,
        barrier: u64,
    },
    RunFailed {
        message: String,
    },
    TurnStarted {
        turn_id: String,
        model: Option<Model>,
    },
    TurnTerminal {
        turn_id: String,
        status: TurnTerminalStatus,
        message: Option<String>,
    },
    AgentSettled,
    Exported {
        path: String,
    },
    ShareSucceeded {
        url: String,
    },
    ShareFailed {
        message: String,
    },
    SessionBeforeTree(SessionBeforeTreeEvent),
    SessionTree(SessionTreeEvent),
    SessionBeforeFork(SessionBeforeForkEvent),
    SessionForked(SessionForkedEvent),
    Process(ProcessEvent),
    Selection(crate::SelectionPlan),
    /// Deterministic auto-mode classification of the last user prompt,
    /// published before the run starts when `selector.autoMode` is enabled
    /// and the classifier detected a code task or long-running goal.
    ModeDetected {
        mode: crate::PromptMode,
        hint: String,
    },
    TodoChildRouted {
        receipt: TodoChildReceipt,
    },
    Loop(crate::LoopEvent),
    Orchestration(crate::OrchestrationEvent),
    Workflow(crate::WorkflowEvent),
    TodoUpdated {
        phases: Vec<crate::TodoPhase>,
        completed_tasks: Vec<crate::TodoCompletionTransition>,
        execution_status: TodoDagExecutionStatus,
        failure_reason: Option<TodoDagFailureReason>,
    },
    TodoReminder {
        phases: Vec<crate::TodoPhase>,
        execution_status: TodoDagExecutionStatus,
        failure_reason: Option<TodoDagFailureReason>,
    },
    GoalUpdated {
        operation: &'static str,
        state: GoalState,
    },
    GoalUsageCharged {
        delta: GoalUsageDelta,
        state: GoalState,
    },
    GoalContinuation {
        decision: GoalContinuationDecision,
    },
}

impl Serialize for ApplicationEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::RuntimeChanged { epoch } => {
                serde_json::json!({ "type": "runtime_changed", "epoch": epoch })
                    .serialize(serializer)
            }
            Self::SessionStarted(event) => event.serialize(serializer),
            Self::Agent(event) | Self::AgentPublished { event, .. } => event.serialize(serializer),
            Self::Session(event) => event.serialize(serializer),
            Self::RunFailed { message } => {
                serde_json::json!({ "type": "run_failed", "message": message }).serialize(serializer)
            }
            Self::TurnStarted { turn_id, model } => {
                serde_json::json!({ "type": "turn_started", "turnId": turn_id, "model": model })
                    .serialize(serializer)
            }
            Self::TurnTerminal { turn_id, status, message } => {
                serde_json::json!({ "type": "turn_terminal", "turnId": turn_id, "status": status, "message": message })
                    .serialize(serializer)
            }
            Self::AgentSettled => {
                serde_json::json!({ "type": "agent_settled" }).serialize(serializer)
            }
            Self::Exported { path } => {
                serde_json::json!({ "type": "exported", "path": path }).serialize(serializer)
            }
            Self::ShareSucceeded { url } => {
                serde_json::json!({ "type": "share_succeeded", "url": url }).serialize(serializer)
            }
            Self::ShareFailed { message } => {
                serde_json::json!({ "type": "share_failed", "message": message }).serialize(serializer)
            }
            Self::SessionBeforeTree(event) => {
                serde_json::json!({ "type": "session_before_tree", "targetId": event.target_id, "summarize": event.summarize }).serialize(serializer)
            }
            Self::SessionTree(event) => {
                serde_json::json!({ "type": "session_tree", "targetId": event.target_id, "activeLeafId": event.active_leaf_id, "editorText": event.editor_text, "summaryEntryId": event.summary_entry_id, "changed": event.changed, "cancelled": event.cancelled }).serialize(serializer)
            }
            Self::SessionBeforeFork(event) => {
                serde_json::json!({ "type": "session_before_fork", "targetId": event.target_id }).serialize(serializer)
            }
            Self::SessionForked(event) => {
                serde_json::json!({ "type": "session_forked", "targetId": event.target_id, "sessionId": event.session_id, "sessionFile": event.session_file, "editorText": event.editor_text }).serialize(serializer)
            }
            Self::Process(event) => event.serialize(serializer),
            Self::Selection(plan) => {
                serde_json::json!({ "type": "selection", "selection": plan }).serialize(serializer)
            }
            Self::ModeDetected { mode, hint } => {
                serde_json::json!({ "type": "mode_detected", "mode": mode, "hint": hint })
                    .serialize(serializer)
            }
            Self::TodoChildRouted { receipt } => {
                serde_json::json!({ "type": "todo_child_routed", "receipt": receipt })
                    .serialize(serializer)
            }
            Self::Loop(event) => event.serialize(serializer),
            Self::Orchestration(event) => event.serialize(serializer),
            Self::Workflow(event) => event.serialize(serializer),
            Self::TodoUpdated { phases, completed_tasks, execution_status, failure_reason } => {
                serde_json::json!({
                    "type": "todo_updated",
                    "phases": phases,
                    "completed_tasks": completed_tasks,
                    "executionStatus": execution_status,
                    "failureReason": failure_reason,
                })
                .serialize(serializer)
            }
            Self::TodoReminder { phases, execution_status, failure_reason } => {
                serde_json::json!({ "type": "todo_reminder", "phases": phases, "executionStatus": execution_status, "failureReason": failure_reason })
                    .serialize(serializer)
            }
            Self::GoalUpdated { operation, state } => {
                serde_json::json!({ "type": "goal_updated", "operation": operation, "state": state })
                    .serialize(serializer)
            }
            Self::GoalUsageCharged { delta, state } => {
                serde_json::json!({ "type": "goal_usage_charged", "delta": delta, "state": state })
                    .serialize(serializer)
            }
            Self::GoalContinuation { decision } => {
                serde_json::json!({ "type": "goal_continuation", "decision": decision })
                    .serialize(serializer)
            }
        }
    }
}

#[derive(Clone)]
pub struct Application {
    inner: Arc<ApplicationInner>,
}

#[derive(Clone, Default)]
pub struct TodoChildToolBinding {
    application: Arc<Mutex<Option<Weak<ApplicationInner>>>>,
}

impl TodoChildToolBinding {
    #[must_use]
    pub fn tools(&self) -> Vec<AgentTool> {
        vec![
            self.send_tool(),
            self.add_task_tool(),
            self.list_tool(),
            self.status_tool(),
            self.select_tool(),
            self.stop_tool(),
            self.pause_tool(),
            self.resume_tool(),
            self.cancel_tool(),
            self.archive_tool(),
            self.remove_tool(),
        ]
    }

    fn send_tool(&self) -> AgentTool {
        let binding = self.clone();
        let parameters = Schema::object_ordered(vec![
            ("parentSessionId".to_owned(), Schema::string(), false),
            ("childSessionId".to_owned(), Schema::string(), false),
            ("message".to_owned(), Schema::string(), true),
        ]);
        AgentTool::new(
            "todo_child_send",
            "Send a directed message to a durable Todo child fork of this parent session. When childSessionId is omitted, routes to the currently selected or sole active child. Use when the user asks to tell or send work to a Todo child session.",
            parameters,
            move |context| {
                let binding = binding.clone();
                async move {
                    let arguments: TodoChildToolArguments = serde_json::from_value(context.arguments)
                        .map_err(|error| anyhow!("invalid todo child send arguments: {error}"))?;
                    let receipt = binding
                        .application()?
                        .todo_child_send(
                            arguments.parent_session_id.as_deref(),
                            arguments.child_session_id.as_deref(),
                            arguments.message,
                        )
                        .await?;
                    Ok(AgentToolResult {
                        content: vec![ContentBlock::text(serde_json::to_string_pretty(&receipt)?)],
                        details: serde_json::to_value(&receipt)?,
                        ..AgentToolResult::default()
                    })
                }
            },
        )
        .with_capability(ToolCapability::Write)
        .with_label("Todo child send")
    }

    fn add_task_tool(&self) -> AgentTool {
        let binding = self.clone();
        let string_array = Schema {
            schema_type: Some(Value::String("array".to_owned())),
            items: Some(Box::new(Schema::string())),
            ..Schema::default()
        };
        let revision = Schema {
            schema_type: Some(Value::String("integer".to_owned())),
            minimum: Some(0.0),
            ..Schema::default()
        };
        let parameters = Schema::object_ordered(vec![
            ("parentSessionId".to_owned(), Schema::string(), false),
            ("childSessionId".to_owned(), Schema::string(), true),
            ("content".to_owned(), Schema::string(), true),
            ("dependsOn".to_owned(), string_array, false),
            ("agent".to_owned(), Schema::string(), false),
            ("role".to_owned(), Schema::string(), false),
            ("expectedRevision".to_owned(), revision, false),
        ]);
        AgentTool::new(
            "todo_child_add_task",
            "Add a typed task to an existing durable Todo child fork using its exact child session identity.",
            parameters,
            move |context| {
                let binding = binding.clone();
                async move {
                    let arguments: TodoChildAddTaskToolArguments =
                        serde_json::from_value(context.arguments).map_err(|error| {
                            anyhow!("invalid todo child add task arguments: {error}")
                        })?;
                    let result = binding
                        .application()?
                        .todo_child_add_task(
                            arguments.parent_session_id.as_deref(),
                            &arguments.child_session_id,
                            arguments.request,
                        )
                        .await?;
                    Ok(AgentToolResult {
                        content: vec![ContentBlock::text(serde_json::to_string_pretty(&result)?)],
                        details: serde_json::to_value(&result)?,
                        ..AgentToolResult::default()
                    })
                }
            },
        )
        .with_capability(ToolCapability::Write)
        .with_label("Todo child add task")
    }

    /// Shared schema for the identity-scoped lifecycle tools: the parent
    /// session id defaults to the attached application's session, and the
    /// child session id falls back to the selected or sole active child.
    fn identity_parameters(child_required: bool) -> Schema {
        Schema::object_ordered(vec![
            ("parentSessionId".to_owned(), Schema::string(), false),
            ("childSessionId".to_owned(), Schema::string(), child_required),
        ])
    }

    /// Builds one of the identity-scoped lifecycle tools. `run` receives the
    /// attached application plus the optional parent/child identities and
    /// returns the serialized tool-result payload (also used as `details`).
    fn identity_tool(
        &self,
        name: &'static str,
        label: &'static str,
        description: &str,
        child_required: bool,
        capability: ToolCapability,
        run: impl Fn(
                Application,
                Option<String>,
                Option<String>,
            ) -> std::pin::Pin<
                Box<dyn Future<Output = anyhow::Result<Value>> + Send>,
            > + Send
            + Sync
            + 'static,
    ) -> AgentTool {
        let binding = self.clone();
        let run = Arc::new(run);
        AgentTool::new(
            name,
            description,
            Self::identity_parameters(child_required),
            move |context| {
                let binding = binding.clone();
                let run = run.clone();
                async move {
                    let arguments: TodoChildIdentityToolArguments =
                        serde_json::from_value(context.arguments)
                            .map_err(|error| anyhow!("invalid {name} arguments: {error}"))?;
                    let application = binding.application()?;
                    let value = run(
                        application,
                        arguments.parent_session_id,
                        arguments.child_session_id,
                    )
                    .await?;
                    Ok(AgentToolResult {
                        content: vec![ContentBlock::text(serde_json::to_string_pretty(&value)?)],
                        details: value,
                        ..AgentToolResult::default()
                    })
                }
            },
        )
        .with_capability(capability)
        .with_label(label)
    }

    fn list_tool(&self) -> AgentTool {
        self.identity_tool(
            "todo_child_list",
            "Todo child list",
            "List the durable Todo child forks of this parent session with their child session ids, labels, statuses, and creation times. Use when the user asks to show or list Todo children.",
            false,
            ToolCapability::Read,
            |application, parent_session_id, _child_session_id| {
                Box::pin(async move {
                    let entries = application
                        .todo_child_list(parent_session_id.as_deref())
                        .await?;
                    Ok(serde_json::to_value(&entries)?)
                })
            },
        )
    }

    fn status_tool(&self) -> AgentTool {
        self.identity_tool(
            "todo_child_status",
            "Todo child status",
            "Read the durable status snapshot of a Todo child (identity, status, task list, receipts, and revisions). When childSessionId is omitted, reports the currently selected or sole active child. Use when the user asks about a Todo child's status or progress.",
            false,
            ToolCapability::Read,
            |application, parent_session_id, child_session_id| {
                Box::pin(async move {
                    let snapshot = application
                        .todo_child_status(parent_session_id.as_deref(), child_session_id.as_deref())
                        .await?;
                    Ok(serde_json::to_value(&snapshot)?)
                })
            },
        )
    }

    fn select_tool(&self) -> AgentTool {
        self.identity_tool(
            "todo_child_select",
            "Todo child select",
            "Select which durable Todo child subsequent child operations target. Requires the exact child session id (list them with todo_child_list).",
            true,
            ToolCapability::Write,
            |application, parent_session_id, child_session_id| {
                Box::pin(async move {
                    let child_session_id = child_session_id
                        .ok_or_else(|| anyhow!("todo_child_select requires childSessionId"))?;
                    application
                        .todo_child_select(parent_session_id.as_deref(), &child_session_id)
                        .await?;
                    Ok(serde_json::json!({
                        "selected": true,
                        "childSessionId": child_session_id,
                    }))
                })
            },
        )
    }

    fn stop_tool(&self) -> AgentTool {
        self.identity_tool(
            "todo_child_stop",
            "Todo child stop",
            "Stop a durable Todo child's execution loop. When childSessionId is omitted, stops the currently selected or sole active child. Use when the user asks to stop a Todo child.",
            false,
            ToolCapability::Write,
            |application, parent_session_id, child_session_id| {
                Box::pin(async move {
                    let snapshot = application
                        .todo_child_stop(parent_session_id.as_deref(), child_session_id.as_deref())
                        .await?;
                    Ok(serde_json::to_value(&snapshot)?)
                })
            },
        )
    }

    fn pause_tool(&self) -> AgentTool {
        self.identity_tool(
            "todo_child_pause",
            "Todo child pause",
            "Pause a durable Todo child's execution. When childSessionId is omitted, pauses the currently selected or sole active child. Use when the user asks to pause a Todo child.",
            false,
            ToolCapability::Write,
            |application, parent_session_id, child_session_id| {
                Box::pin(async move {
                    let snapshot = application
                        .todo_child_pause(parent_session_id.as_deref(), child_session_id.as_deref())
                        .await?;
                    Ok(serde_json::to_value(&snapshot)?)
                })
            },
        )
    }

    fn resume_tool(&self) -> AgentTool {
        self.identity_tool(
            "todo_child_resume",
            "Todo child resume",
            "Resume a paused durable Todo child's execution. When childSessionId is omitted, resumes the currently selected or sole active child. Use when the user asks to resume a Todo child.",
            false,
            ToolCapability::Write,
            |application, parent_session_id, child_session_id| {
                Box::pin(async move {
                    let snapshot = application
                        .todo_child_resume(parent_session_id.as_deref(), child_session_id.as_deref())
                        .await?;
                    Ok(serde_json::to_value(&snapshot)?)
                })
            },
        )
    }

    fn cancel_tool(&self) -> AgentTool {
        self.identity_tool(
            "todo_child_cancel",
            "Todo child cancel",
            "Cancel a durable Todo child's active execution and discard its pending work. When childSessionId is omitted, cancels the currently selected or sole active child. Idempotent: canceling a child with no active execution succeeds as a no-op. Use when the user asks to cancel a Todo child or its current run.",
            false,
            ToolCapability::Write,
            |application, parent_session_id, child_session_id| {
                Box::pin(async move {
                    match application
                        .todo_child_cancel(parent_session_id.as_deref(), child_session_id.as_deref())
                        .await?
                    {
                        Some(snapshot) => serde_json::to_value(&snapshot).map_err(Into::into),
                        None => Ok(serde_json::json!({
                            "cancelled": false,
                            "reason": "no active durable Todo child execution to cancel",
                        })),
                    }
                })
            },
        )
    }

    fn archive_tool(&self) -> AgentTool {
        self.identity_tool(
            "todo_child_archive",
            "Todo child archive",
            "Archive a finished durable Todo child so it leaves the active child list. Requires the exact child session id. Use when the user asks to archive or clean up a Todo child.",
            true,
            ToolCapability::Write,
            |application, parent_session_id, child_session_id| {
                Box::pin(async move {
                    let child_session_id = child_session_id
                        .ok_or_else(|| anyhow!("todo_child_archive requires childSessionId"))?;
                    application
                        .todo_child_archive(parent_session_id.as_deref(), &child_session_id)
                        .await?;
                    Ok(serde_json::json!({
                        "archived": true,
                        "childSessionId": child_session_id,
                    }))
                })
            },
        )
    }

    fn remove_tool(&self) -> AgentTool {
        self.identity_tool(
            "todo_child_remove",
            "Todo child remove",
            "Remove a durable Todo child by its exact child session id: any active execution is cancelled and awaited, then the manifest entry, transcript, and receipts are permanently deleted. Idempotent for an already-removed child. Use when the user asks to remove or delete a Todo child entirely.",
            true,
            ToolCapability::Write,
            |application, parent_session_id, child_session_id| {
                Box::pin(async move {
                    let child_session_id = child_session_id
                        .ok_or_else(|| anyhow!("todo_child_remove requires childSessionId"))?;
                    application
                        .todo_child_remove(parent_session_id.as_deref(), &child_session_id)
                        .await?;
                    Ok(serde_json::json!({
                        "removed": true,
                        "childSessionId": child_session_id,
                    }))
                })
            },
        )
    }


    fn application(&self) -> Result<Application> {
        self.application
            .lock()
            .as_ref()
            .and_then(Weak::upgrade)
            .map(|inner| Application { inner })
            .ok_or_else(|| anyhow!("todo child tool is not attached to an application"))
    }

    fn bind(&self, application: &Application) -> Result<()> {
        let mut binding = self.application.lock();
        if binding.as_ref().and_then(Weak::upgrade).is_some() {
            return Err(anyhow!(
                "todo child tool is already attached to an application"
            ));
        }
        *binding = Some(Arc::downgrade(&application.inner));
        Ok(())
    }

    fn unbind_ptr(&self, application: *const ApplicationInner) {
        let mut binding = self.application.lock();
        if binding
            .as_ref()
            .is_some_and(|bound| bound.as_ptr() == application)
        {
            *binding = None;
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TodoChildToolArguments {
    #[serde(default)]
    parent_session_id: Option<String>,
    #[serde(default)]
    child_session_id: Option<String>,
    message: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TodoChildAddTaskToolArguments {
    #[serde(default)]
    parent_session_id: Option<String>,
    child_session_id: String,
    #[serde(flatten)]
    request: TodoChildAddTaskRequest,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TodoChildIdentityToolArguments {
    #[serde(default)]
    parent_session_id: Option<String>,
    #[serde(default)]
    child_session_id: Option<String>,
}

#[derive(Clone, Default)]
pub struct GoalToolBinding {
    application: Arc<Mutex<Option<Weak<ApplicationInner>>>>,
}

impl GoalToolBinding {
    #[must_use]
    pub fn tool(&self) -> AgentTool {
        let binding = self.clone();
        AgentTool::new(
            "goal",
            "Inspect the session goal, pause it, or explicitly complete it. Creation, resumption, dropping, and usage accounting remain host-controlled.",
            goal_tool_schema(),
            move |context| {
                let binding = binding.clone();
                async move {
                    let arguments: GoalToolArguments = serde_json::from_value(context.arguments)
                        .map_err(|error| anyhow!("invalid goal arguments: {error}"))?;
                    let application = binding
                        .application
                        .lock()
                        .as_ref()
                        .and_then(Weak::upgrade)
                        .map(|inner| Application { inner })
                        .ok_or_else(|| anyhow!("goal tool is not attached to an application"))?;
                    match arguments.op.as_str() {
                        "get" => {}
                        "pause" => {
                            application.goal_pause()?;
                        }
                        "complete" => {
                            application.goal_complete()?;
                        }
                        operation => return Err(anyhow!("unsupported goal operation {operation:?}")),
                    }
                    let state = application.goal_state();
                    Ok(AgentToolResult {
                        content: vec![ContentBlock::text(serde_json::to_string_pretty(&state)?)],
                        details: serde_json::to_value(&state)?,
                        ..AgentToolResult::default()
                    })
                }
            },
        )
        .with_capability(ToolCapability::Write)
    }

    fn bind(&self, application: &Application) -> Result<()> {
        let mut binding = self.application.lock();
        if binding.as_ref().and_then(Weak::upgrade).is_some() {
            return Err(anyhow!("goal tool is already attached to an application"));
        }
        *binding = Some(Arc::downgrade(&application.inner));
        Ok(())
    }

    fn unbind(&self, application: &Weak<ApplicationInner>) {
        let mut binding = self.application.lock();
        if binding
            .as_ref()
            .is_some_and(|bound| bound.ptr_eq(application))
        {
            *binding = None;
        }
    }

    fn unbind_ptr(&self, application: *const ApplicationInner) {
        let mut binding = self.application.lock();
        if binding
            .as_ref()
            .is_some_and(|bound| bound.as_ptr() == application)
        {
            *binding = None;
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalToolArguments {
    op: String,
}
pub type ApplicationRuntimeFuture =
    std::pin::Pin<Box<dyn Future<Output = Result<ApplicationRuntimeCandidate>> + Send>>;

pub(crate) type TodoChildRuntimeFactory =
    Arc<dyn Fn(crate::SessionOptions) -> ApplicationRuntimeFuture + Send + Sync>;

/// Builds a complete inactive runtime generation for an arbitrary trusted cwd.
///
/// The factory owns product-specific construction policy. The caller supplies
/// session options plus an optional retained native resume capability; no
/// active [`Application`] state may be mutated during candidate construction.
pub trait ApplicationRuntimeFactory: Send + Sync {
    fn build_runtime_candidate(
        &self,
        cwd: PathBuf,
        options: crate::SessionOptions,
        resume: Option<crate::PreparedSessionResume>,
    ) -> ApplicationRuntimeFuture;

    fn build_trusted_workflow_candidate(
        &self,
        _cwd: crate::workflow_worktree::TrustedWorkflowCwd,
        _options: crate::SessionOptions,
    ) -> ApplicationRuntimeFuture {
        Box::pin(async {
            Err(anyhow!(
                "application runtime factory does not support trusted workflow worktrees"
            ))
        })
    }
}

async fn hydrate_runtime_candidate(
    candidate: ApplicationRuntimeCandidate,
    resume: crate::PreparedSessionResume,
) -> Result<ApplicationRuntimeCandidate> {
    let context = resume.build_context();
    let recorder = resume.into_recorder()?;

    if let Some(provider) = context.provider.as_deref()
        && let Some(model_id) = context.model_id.as_deref()
        && let Some(model) = pi_ai::get_model(provider, model_id)
    {
        candidate
            .session
            .set_model_with_resolved_auth(model)
            .await?;
    }
    candidate
        .session
        .resume_recording(recorder, context.messages)
        .await?;
    Ok(candidate)
}

impl ApplicationInner {
    fn runtime(&self) -> Arc<runtime::ApplicationRuntime> {
        self.runtime.runtime()
    }

    fn resident_identity(session_id: String, session_path: PathBuf) -> Result<ResidentIdentity> {
        Ok(ResidentIdentity {
            session_id,
            recorder_path: crate::trust::canonical_path(&session_path).with_context(|| {
                format!("canonicalizing recorder path {}", session_path.display())
            })?,
        })
    }

    fn session_identity(session: &Session) -> Result<ResidentIdentity> {
        let (session_id, session_path) = session
            .recorder_info()
            .ok_or_else(|| anyhow!("session recording is unavailable"))?;
        Self::resident_identity(session_id, session_path)
    }

    fn take_resident_orchestration(
        self: &Arc<Self>,
        identity: &ResidentIdentity,
    ) -> Option<ResidentOrchestrationClaim> {
        self.resident_orchestration
            .lock()
            .remove(identity)
            .map(|resident| ResidentOrchestrationClaim {
                inner: self.clone(),
                identity: identity.clone(),
                resident: Some(resident),
            })
    }

    fn retain_resident_orchestration(
        &self,
        identity: ResidentIdentity,
        resident: ResidentOrchestration,
    ) {
        let replaced = self
            .resident_orchestration
            .lock()
            .insert(identity, resident);
        debug_assert!(
            replaced.is_none(),
            "resident orchestration identity must be unique"
        );
    }

    fn drain_resident_orchestration(&self) -> Vec<ResidentOrchestration> {
        self.resident_orchestration
            .lock()
            .drain()
            .map(|(_, resident)| resident)
            .collect()
    }

    /// Deliver an unsolicited child → Main mailbox message into the live main
    /// session's steering queue, so the parent model receives the reply at
    /// its next safe turn boundary (running) or the next run's initial
    /// steering drain (idle). Messages already claimed by an active `hub
    /// wait` are skipped: the wait tool returns the body as its own tool
    /// result, so steering it again would duplicate it in the model context.
    /// `MessageDelivered` presentation events still publish for every message
    /// (TUI/Web rendering is unchanged). A failed steer does not drain the
    /// mailbox, so the committed reply is never lost — callers must surface
    /// the failure (bounded structured diagnostic) rather than swallow it.
    async fn steer_main_delivered_message(
        &self,
        active: &runtime::ApplicationRuntime,
        event: &crate::OrchestrationEvent,
    ) -> Result<()> {
        let crate::OrchestrationEvent::MessageDelivered {
            message,
            waiter_claimed: false,
            ..
        } = event
        else {
            return Ok(());
        };
        {
            let mut steered = self.steered_main_message_ids.lock();
            if !steered.insert(message.id.clone()) {
                return Ok(());
            }
        }
        let session = active.session.clone();
        if let Err(error) = session
            .steer(crate::orchestration::mailbox_message_as_custom(message))
            .await
        {
            self.steered_main_message_ids.lock().remove(&message.id);
            return Err(error);
        }
        Ok(())
    }

    async fn replay_main_mailbox(
        &self,
        active: &runtime::ApplicationRuntime,
        orchestration: &crate::OrchestrationRuntime,
    ) {
        let messages = match orchestration.inbox(orchestration.main_agent_id(), true) {
            Ok(messages) => messages,
            Err(error) => {
                eprintln!("orchestration: reading main mailbox for replay failed: {error:#}");
                return;
            }
        };
        for message in messages {
            let event = crate::OrchestrationEvent::MessageDelivered {
                group_id: orchestration.group_id().to_owned(),
                message,
                waiter_claimed: false,
            };
            if let Err(error) = self.steer_main_delivered_message(active, &event).await {
                self.report_steer_main_delivery_failure(&event, &error);
            }
        }
    }

    /// Bounded structured diagnostic for a failed main-session steer: one
    /// stderr line identifying the message (never the raw body).
    fn report_steer_main_delivery_failure(
        &self,
        event: &crate::OrchestrationEvent,
        error: &anyhow::Error,
    ) {
        let id = match event {
            crate::OrchestrationEvent::MessageDelivered { message, .. } => message.id.as_str(),
            _ => "?",
        };
        eprintln!(
            "orchestration: steering delivered message {id} into the main session failed: {error:#}"
        );
    }

    fn report_main_delivery_ack_failure(&self, message_id: &str, error: &anyhow::Error) {
        eprintln!(
            "orchestration: acknowledging recorded Main delivery {message_id} failed: {error:#}"
        );
    }

    #[cfg(test)]
    fn main_delivery_is_claimed_for_test(&self, message_id: &str) -> bool {
        self.steered_main_message_ids.lock().contains(message_id)
    }
}

impl Application {
    fn runtime(&self) -> Arc<runtime::ApplicationRuntime> {
        self.inner.runtime()
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ResidentIdentity {
    session_id: String,
    recorder_path: PathBuf,
}

struct ResidentOrchestration {
    runtime: crate::OrchestrationRuntime,
    publication: Arc<ApplicationPublication>,
}

struct ResidentOrchestrationClaim {
    inner: Arc<ApplicationInner>,
    identity: ResidentIdentity,
    resident: Option<ResidentOrchestration>,
}

impl ResidentOrchestrationClaim {
    fn runtime(&self) -> &crate::OrchestrationRuntime {
        &self
            .resident
            .as_ref()
            .expect("resident claim owns its entry")
            .runtime
    }

    fn commit(mut self) -> ResidentOrchestration {
        self.resident.take().expect("resident claim commits once")
    }
}

impl Drop for ResidentOrchestrationClaim {
    fn drop(&mut self) {
        let Some(resident) = self.resident.take() else {
            return;
        };
        let replaced = self
            .inner
            .resident_orchestration
            .lock()
            .insert(self.identity.clone(), resident);
        debug_assert!(
            replaced.is_none(),
            "resident orchestration rollback must restore its exact entry"
        );
    }
}

struct ApplicationInner {
    events: broadcast::Sender<ApplicationEvent>,
    runtime_factory: Mutex<Option<Arc<dyn ApplicationRuntimeFactory>>>,
    todo_child_client: Mutex<TodoChildClient>,
    runtime: runtime::ApplicationRuntimeSlot,
    steered_main_message_ids: Mutex<std::collections::BTreeSet<String>>,
    resident_orchestration: Mutex<HashMap<ResidentIdentity, ResidentOrchestration>>,
    workflow_manager: Mutex<Option<crate::WorkflowManager>>,
    workflow_runtime_factory: Mutex<Option<Weak<workflows::ApplicationWorkflowRuntimeFactory>>>,
    workflow_events: Mutex<Option<JoinHandle<()>>>,
    settings_watch: Mutex<Vec<JoinHandle<()>>>,
    loop_scheduler: Mutex<Option<crate::LoopSchedulerRuntime>>,
    operation_gate: Arc<AsyncMutex<()>>,
    cutover_before_spawn_gate: Mutex<Option<ApplicationCutoverTestGate>>,
    /// Test gate parked inside `commit_orchestration_candidate`'s
    /// spawn-guarded section, so a racing stale-handle spawn can be
    /// deterministically interleaved against the retirement protocol.
    orchestration_commit_gate: Mutex<Option<ApplicationCutoverTestGate>>,
    restart_checkpointed: AtomicBool,
    prompt_before_turn_start_gate: Mutex<Option<ApplicationCutoverTestGate>>,
    prompt_after_settle_gate: Mutex<Option<ApplicationCutoverTestGate>>,
    cutover_after_loop_commit_gate: Mutex<Option<ApplicationCutoverTestGate>>,
    cutover_after_resident_take_gate: Mutex<Option<ApplicationCutoverTestGate>>,
    cleanup_flight: AsyncMutex<Option<CleanupFlight>>,
    #[cfg(test)]
    cleanup_gate: Mutex<Option<ApplicationCleanupTestGate>>,
    #[cfg(test)]
    cleanup_teardown_entries: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    cleanup_teardown_completions: std::sync::atomic::AtomicUsize,
    repository_test_executor: Mutex<Option<crate::process::HermeticRepositoryExecutor>>,
    cleaned: AtomicBool,
}

struct OuterTurnLifecycle {
    inner: Arc<ApplicationInner>,
    runtime: Arc<runtime::ApplicationRuntime>,
    turn_id: String,
    settled: bool,
}

impl ApplicationInner {
    fn publish_turn_started(
        &self,
        runtime: &runtime::ApplicationRuntime,
        turn_id: &str,
        model: Option<&Model>,
    ) -> bool {
        self.runtime.publish_turn_started(runtime, turn_id, model)
    }

    fn commit_turn_terminal(
        &self,
        runtime: &runtime::ApplicationRuntime,
        turn_id: &str,
        status: &TurnTerminalStatus,
        message: Option<&String>,
    ) -> Option<runtime::CommittedTurnTerminal> {
        self.runtime
            .commit_turn_terminal(runtime, turn_id, status, message)
    }

    fn emit_turn_terminal(&self, terminal: runtime::CommittedTurnTerminal) {
        self.runtime.emit_turn_terminal(terminal);
    }
}

impl OuterTurnLifecycle {
    fn new(
        inner: Arc<ApplicationInner>,
        runtime: Arc<runtime::ApplicationRuntime>,
        turn_id: String,
    ) -> Self {
        Self {
            inner,
            runtime,
            turn_id,
            settled: false,
        }
    }

    fn commit(
        &mut self,
        status: TurnTerminalStatus,
        message: Option<String>,
    ) -> Option<runtime::CommittedTurnTerminal> {
        if self.settled {
            return None;
        }
        let terminal = self.inner.commit_turn_terminal(
            &self.runtime,
            &self.turn_id,
            &status,
            message.as_ref(),
        )?;
        self.settled = true;
        Some(terminal)
    }

    fn settle(&mut self, status: TurnTerminalStatus, message: Option<String>) {
        if let Some(terminal) = self.commit(status, message) {
            self.inner.emit_turn_terminal(terminal);
        }
    }
}

impl Drop for OuterTurnLifecycle {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let (status, message) = if std::thread::panicking() {
            (TurnTerminalStatus::Failed, "turn task panicked")
        } else {
            (TurnTerminalStatus::Aborted, "turn task cancelled")
        };
        self.settle(status, Some(message.to_owned()));
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GoalWorkKey {
    goal_id: String,
    revision: u64,
    /// Goal-state lifecycle epoch the reservation was made against; a newer
    /// lifecycle-changing commit invalidates the key (see
    /// [`crate::GoalState::lifecycle_revision`]).
    lifecycle_revision: u64,
}

struct ApplicationExtensionHost {
    application: Weak<ApplicationInner>,
}

impl Application {
    pub async fn new(session: Session) -> Self {
        Self::build(session, None, None, None, None)
            .await
            .expect("validated Application bootstrap")
    }

    pub async fn new_with_extensions(
        session: Session,
        runtime: ExtensionRuntime,
        permissions: ExtensionPermissionSet,
    ) -> Self {
        Self::build(session, Some((runtime, permissions)), None, None, None)
            .await
            .expect("validated Application bootstrap")
    }

    pub fn attach_runtime_factory(
        &self,
        factory: Arc<dyn ApplicationRuntimeFactory>,
    ) -> Result<()> {
        let mut current = self.inner.runtime_factory.lock();
        if let Some(configured) = current.as_ref() {
            if Arc::ptr_eq(configured, &factory) {
                return Ok(());
            }
            return Err(anyhow!("application runtime factory is already configured"));
        }
        if let Ok(service) = self.inner.todo_child_client.lock().service() {
            service.configure_factory(factory.clone())?;
        }
        *current = Some(factory);
        Ok(())
    }

    #[must_use]
    pub fn runtime_epoch(&self) -> u64 {
        self.runtime().epoch()
    }

    #[must_use]
    pub(crate) fn publication(&self) -> Arc<ApplicationPublication> {
        self.runtime().publication()
    }

    #[must_use]
    pub fn capture_generation(&self) -> Arc<ApplicationGeneration> {
        self.publication().capture()
    }

    pub fn durable_policy_state(
        &self,
    ) -> std::result::Result<crate::DurablePolicyState, crate::DurableStateValidationError> {
        crate::DurablePolicyState::capture(&self.capture_generation())
    }

    #[must_use]
    pub fn http_executor(&self, class: pi_sandbox::HostHttpClass) -> pi_ai::SharedHttpExecutor {
        self.capture_generation().sandbox().http().executor(class)
    }

    #[cfg(test)]
    pub(crate) fn approval_test_authorities(
        &self,
    ) -> (
        Arc<crate::sandbox::SandboxGeneration>,
        Arc<crate::sandbox::ApprovedSpawnRegistry>,
    ) {
        let generation = self.capture_generation();
        (
            generation.sandbox().clone(),
            generation.security().approvals().clone(),
        )
    }

    /// Reads a session-governed file through the currently published immutable generation.
    /// The generation is captured once and retained for the complete operation.
    pub fn read_generation_file_bounded(
        &self,
        path: &std::path::Path,
        max_bytes: u64,
    ) -> anyhow::Result<Vec<u8>> {
        self.capture_generation().read_file_bounded(path, max_bytes)
    }

    /// Verifies a session-governed directory and returns its diagnostic display path.
    /// The returned path is never filesystem authority; callers must not reopen it.
    pub fn inspect_generation_directory(
        &self,
        path: &std::path::Path,
    ) -> anyhow::Result<std::path::PathBuf> {
        self.capture_generation().inspect_directory(path)
    }

    #[must_use]
    pub(crate) fn generation_capture_fn(&self) -> GenerationCaptureFn {
        self.publication().capture_fn()
    }
    #[must_use]
    pub fn governed_process_context(&self) -> crate::GovernedProcessContext {
        let active = self.runtime();
        let mut context = crate::GovernedProcessContext::new(
            active.publication().capture(),
            active.session.auth_manager(),
        );
        if let Some(executor) = self.inner.repository_test_executor.lock().clone() {
            context = context.with_hermetic_repository_executor(executor);
        }
        context
    }

    /// Install a hermetic repository helper executor (external test seam) used
    /// by every [`Application::governed_process_context`] and
    /// [`Application::repository_process_context`] afterward. The governed
    /// bridge still validates retained repository authority and cwd provenance
    /// before execution; production never calls this.
    pub fn with_hermetic_repository_executor(
        &self,
        executor: crate::process::HermeticRepositoryExecutor,
    ) {
        *self.inner.repository_test_executor.lock() = Some(executor);
    }

    /// Build a repository-authorized [`crate::GovernedProcessContext`] for
    /// `source_cwd` and `managed_root`, opening the retained
    /// `ProjectResources` and `WorkflowWorktrees` capability leases against the
    /// current published generation and installing any hermetic repository
    /// executor. This is the context namespace helpers and workflow repository
    /// helpers must use; a plain [`Application::governed_process_context`] has
    /// no retained repository authority.
    pub fn repository_process_context(
        &self,
        source_cwd: &std::path::Path,
        managed_root: &std::path::Path,
    ) -> Result<crate::GovernedProcessContext> {
        let generation = self.capture_generation();
        let capabilities = generation.sandbox().capabilities().clone();
        let source_root = pi_sandbox::AbsolutePath::parse(source_cwd.to_path_buf())
            .map_err(|_| anyhow!("workflow source path is invalid"))?;
        let source_lease = capabilities
            .open_root(
                crate::fs::CapabilityRootPurpose::ProjectResources,
                source_root,
                crate::fs::CapabilityAccess::ReadWrite,
                crate::fs::RootCreation::Existing,
            )
            .map_err(|_| anyhow!("workflow source repository could not be opened"))?;
        let managed_root = pi_sandbox::AbsolutePath::parse(managed_root.to_path_buf())
            .map_err(|_| anyhow!("workflow worktree path is invalid"))?;
        let managed_lease = capabilities
            .open_root(
                crate::fs::CapabilityRootPurpose::WorkflowWorktrees,
                managed_root,
                crate::fs::CapabilityAccess::PrivateReadWrite,
                crate::fs::RootCreation::CreateDirectory,
            )
            .map_err(|_| anyhow!("workflow worktree storage could not be opened"))?;
        let context = self.governed_process_context();
        #[cfg(unix)]
        let context = context.with_repository_authorities(source_lease, managed_lease);
        Ok(context)
    }

    pub async fn from_runtime_candidate(candidate: ApplicationRuntimeCandidate) -> Result<Self> {
        let durable_policy = match candidate.orchestration_runtime.as_ref() {
            Some(orchestration) => orchestration.durable_policy_for_parent(&candidate.session)?,
            None => None,
        };
        if let Some(durable_policy) = durable_policy {
            return Self::from_durable_runtime_candidate(candidate, &durable_policy).await;
        }
        let ApplicationRuntimeCandidate {
            session,
            extension_runtime,
            orchestration_runtime,
            goal_tool_binding,
            todo_child_tool_binding,
        } = candidate;
        Self::build(
            session,
            extension_runtime,
            orchestration_runtime,
            goal_tool_binding,
            todo_child_tool_binding,
        )
        .await
    }

    pub(crate) async fn from_durable_runtime_candidate(
        candidate: ApplicationRuntimeCandidate,
        durable_policy: &crate::DurablePolicyState,
    ) -> Result<Self> {
        let ApplicationRuntimeCandidate {
            session,
            extension_runtime,
            orchestration_runtime,
            goal_tool_binding,
            todo_child_tool_binding,
        } = candidate;
        let resources = match session.resource_manager() {
            Some(resources) => resources,
            None => crate::ResourceManager::new(crate::ResourceManagerOptions::new(session.cwd()))?,
        };
        let revision =
            ApplicationGenerationRevision::new(durable_policy.snapshot().revision().get())?;
        let lease = DurableGenerationLease::new(revision)?;
        let provenance = lease.provenance().clone();
        let resource = resources.stage_initial_candidate(provenance.clone(), None)?;
        let settings = resource.snapshot().settings.clone();
        let policy_inputs = crate::sandbox::SessionPolicyInputs::validate_with_capabilities(
            session.session_policy_id(),
            session.workspace_roots().clone(),
            resources.options().agent_dir,
            session.session_dir(),
            resource.capabilities(),
        )?;
        let model = session
            .model()
            .ok_or_else(|| anyhow!("session has no model"))?;
        let request_auth = session.request_auth_for_model(&model).await?;
        let stream = session.stream_options().stream;
        let image_generation =
            Self::staged_image_generation_http(&session, &settings, &model, &request_auth).await?;
        let http_requirements = crate::sandbox::SessionHttpRequirements::derive_with_builtins(
            &model,
            &request_auth,
            &stream,
            image_generation
                .as_ref()
                .map(|image| (&image.model, image.base_url.as_deref(), &image.env)),
            &Self::builtin_http_endpoints(&settings),
        )?;
        let staged_update = Arc::new(Mutex::new(None));
        let staged_extension = Arc::new(Mutex::new(None));
        let component_session = session.clone();
        let component_resources = resources.clone();
        let component_extensions = extension_runtime.clone();
        let component_orchestration = orchestration_runtime.clone();
        let component_goal = goal_tool_binding.clone();
        let component_todo_child = todo_child_tool_binding.clone();
        let update_slot = staged_update.clone();
        let extension_slot = staged_extension.clone();
        let stage_components: crate::sandbox::generation::DurableComponentStager<'_> =
            Box::new(move |provenance, resource| {
                Box::pin(async move {
                    let extension_candidate =
                        if let Some((runtime, permissions)) = &component_extensions {
                            let specs = resource.extension_specs(permissions)?;
                            let staged = runtime
                                .stage_reload(
                                    specs,
                                    crate::extensions::ExtensionCandidateAuthority::new(
                                        resource.sandbox().clone(),
                                        provenance.clone(),
                                        component_session.auth_manager(),
                                    ),
                                )
                                .await;
                            let report = staged.report();
                            if !report.failures.is_empty() {
                                runtime.discard_reload(staged).await;
                                return Err(extension_load_report_error(
                                    "extension durable resume",
                                    &report,
                                ));
                            }
                            Some(staged)
                        } else {
                            None
                        };
                    let mut additional_tools = Vec::new();
                    if let (Some((runtime, _)), Some(candidate)) =
                        (&component_extensions, extension_candidate.as_ref())
                    {
                        additional_tools.extend(candidate.agent_tools(runtime));
                    }
                    if let Some(orchestration) = &component_orchestration {
                        additional_tools.extend(orchestration.agent_tools("Main", 0));
                    }
                    if let Some(binding) = &component_goal {
                        additional_tools.push(binding.tool());
                    }
                    if let Some(binding) = &component_todo_child {
                        additional_tools.extend(binding.tools());
                    }
                    let (tools, update) = component_session.stage_tool_generation(
                        provenance.clone(),
                        resource.clone(),
                        additional_tools,
                    )?;
                    let extensions = match (&component_extensions, extension_candidate.as_ref()) {
                        (Some((runtime, permissions)), Some(candidate)) => {
                            ExtensionGeneration::enabled(
                                provenance.clone(),
                                runtime.clone(),
                                candidate.stage_snapshot(),
                                Arc::new(permissions.clone()),
                            )?
                        }
                        (None, None) => ExtensionGeneration::disabled(provenance.clone()),
                        _ => bail!("extension durable resume candidate state is inconsistent"),
                    };
                    let orchestration = match &component_orchestration {
                        Some(runtime) => runtime.stage_generation(provenance.clone())?,
                        None => OrchestrationGeneration::disabled(provenance.clone()),
                    };
                    let runtime =
                        RuntimeGeneration::new(provenance.clone(), update.runtime_settings.clone());
                    let capture_generation = component_resources.capture_fn();
                    let host_hooks = settings
                        .hooks
                        .clone()
                        .filter(|hooks| !hooks.is_empty())
                        .map(|hooks| {
                            Arc::new(crate::HostHooks::new(
                                hooks,
                                component_session.cwd().to_path_buf(),
                                component_session.session_policy_id(),
                                capture_generation,
                            ))
                        });
                    let security = SecurityGeneration::new(
                        provenance.clone(),
                        Arc::new(crate::sandbox::ApprovedSpawnRegistry::default()),
                        settings.permission_rules.clone(),
                        host_hooks,
                    );
                    *update_slot.lock() = Some(update);
                    *extension_slot.lock() = extension_candidate;
                    DurableApplicationComponents::new(
                        &provenance,
                        tools,
                        extensions,
                        orchestration,
                        runtime,
                        security,
                    )
                })
            });
        let generation = crate::sandbox::resume_durable_generation(
            durable_policy,
            crate::sandbox::DurableResumeStaging::new(
                lease,
                resource,
                policy_inputs,
                http_requirements,
                stage_components,
                session.brush_exec_lock(),
            ),
        )
        .await?;
        let publication = ApplicationPublication::from_durable(generation)?;
        if let Some(orchestration) = orchestration_runtime.as_ref() {
            // Recovery is committed by the prepared runtime-generation binding
            // after session/orchestration subscriptions exist. Binding and
            // reviving here would race the later prepared rebind reservation.
            orchestration.bind_generation_capture(publication.capture_fn())?;
        }
        let (events, _) = broadcast::channel(EVENT_BUFFER_CAPACITY);
        let loop_storage = session
            .recorder_file_authority()
            .map(|(lease, relative)| {
                crate::loop_scheduler::LoopStorage::from_session_authority(lease, &relative)
            })
            .transpose()?;
        let initial_runtime = runtime::ApplicationRuntime::new(
            runtime::INITIAL_RUNTIME_EPOCH,
            session.clone(),
            extension_runtime.clone(),
            orchestration_runtime,
            goal_tool_binding,
            todo_child_tool_binding,
        );
        initial_runtime.bind_publication(publication.clone())?;
        let inner = Arc::new(ApplicationInner {
            runtime: runtime::ApplicationRuntimeSlot::new(initial_runtime, events.clone()),
            runtime_factory: Mutex::new(None),
            todo_child_client: Mutex::new(TodoChildClient::default()),
            events,
            steered_main_message_ids: Mutex::new(std::collections::BTreeSet::new()),
            resident_orchestration: Mutex::new(std::collections::HashMap::new()),
            workflow_manager: Mutex::new(None),
            workflow_runtime_factory: Mutex::new(None),
            workflow_events: Mutex::new(None),
            settings_watch: Mutex::new(Vec::new()),
            loop_scheduler: Mutex::new(None),
            operation_gate: Arc::new(AsyncMutex::new(())),
            cutover_before_spawn_gate: Mutex::new(None),
            orchestration_commit_gate: Mutex::new(None),
            prompt_before_turn_start_gate: Mutex::new(None),
            prompt_after_settle_gate: Mutex::new(None),
            cutover_after_loop_commit_gate: Mutex::new(None),
            cutover_after_resident_take_gate: Mutex::new(None),
            cleanup_flight: AsyncMutex::new(None),
            #[cfg(test)]
            cleanup_gate: Mutex::new(None),
            #[cfg(test)]
            cleanup_teardown_entries: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            cleanup_teardown_completions: std::sync::atomic::AtomicUsize::new(0),
            repository_test_executor: Mutex::new(None),
            cleaned: AtomicBool::new(false),
            restart_checkpointed: AtomicBool::new(false),
        });
        let application = Self { inner };
        let active = application.runtime();
        let prepared_bindings = application.prepare_runtime_generation_bindings(&active)?;
        application
            .bind_runtime_generation(active.clone(), Some(prepared_bindings))
            .await?;
        resources.bind_publication(&publication)?;
        let update = staged_update
            .lock()
            .take()
            .ok_or_else(|| anyhow!("durable session resource update is unavailable"))?;
        session.publish_resource_manager(resources, update).await;
        if let (Some((runtime, _)), Some(candidate)) =
            (extension_runtime.as_ref(), staged_extension.lock().take())
        {
            candidate.activate_after_application_publish(runtime)?;
        }
        let loop_runner_inner = Arc::downgrade(&application.inner);
        let loop_runner: crate::LoopTurnRunner = Arc::new(move |request, cancel| {
            let inner = loop_runner_inner.clone();
            Box::pin(async move { run_loop_turn(inner, request, cancel).await })
        });
        let loop_event_inner = Arc::downgrade(&application.inner);
        let loop_events: crate::LoopEventSink = Arc::new(move |event| {
            if let Some(inner) = loop_event_inner.upgrade() {
                inner.publish(ApplicationEvent::Loop(event));
            }
        });
        *application.inner.loop_scheduler.lock() = Some(crate::start_loop_scheduler(
            loop_storage,
            loop_runner,
            loop_events,
        ));
        application.start_settings_watch();

        Ok(application)
    }

    pub async fn new_with_orchestration(
        session: Session,
        orchestration: crate::OrchestrationRuntime,
    ) -> Self {
        Self::build(session, None, Some(orchestration), None, None)
            .await
            .expect("validated Application bootstrap")
    }

    async fn staged_image_generation_http(
        session: &Session,
        settings: &crate::Settings,
        selected_model: &Model,
        selected_auth: &crate::RequestAuth,
    ) -> Result<Option<StagedImageGenerationHttp>> {
        let runtime = settings.image_gen_runtime();
        let model = match runtime.gen_model.as_deref() {
            Some(spec) => crate::resolve_model(spec).map_err(|error| anyhow!(error))?,
            None => selected_model.clone(),
        };
        if !model.image_generation {
            return Ok(None);
        }
        let auth = if model == *selected_model {
            selected_auth.clone()
        } else {
            session.request_auth_for_model(&model).await?
        };
        Ok(Some(StagedImageGenerationHttp {
            model,
            base_url: (!runtime.gen_base_url.trim().is_empty()).then_some(runtime.gen_base_url),
            env: auth.env,
        }))
    }

    /// Build the dynamic built-in HTTP endpoint configuration from operator
    /// settings and environment so a confined `workspace-write` session
    /// derives the exact destinations the operator configured
    /// (memory/Hindsight, live STT, realtime, llama.cpp router) alongside the
    /// always-on static built-in product destinations. Environment reads
    /// (`LLAMA_BASE_URL`, `HF_ENDPOINT`) happen here at the application layer,
    /// keeping HTTP requirement derivation deterministic in tests.
    fn builtin_http_endpoints(settings: &crate::Settings) -> crate::sandbox::BuiltinHttpEndpoints {
        let clean = |value: Option<&str>| {
            value
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        };
        let memory_api_url = settings
            .memory
            .as_ref()
            .filter(|memory| {
                matches!(
                    memory.backend,
                    Some(crate::settings::MemoryBackend::Hindsight)
                )
            })
            .and_then(|memory| clean(memory.hindsight_api_url.as_deref()));
        let live = settings.live.as_ref();
        let live_stt_url = live.and_then(|live| clean(live.stt_base_url.as_deref()));
        let realtime_url = live.and_then(|live| clean(live.realtime_base_url.as_deref()));
        let llama_router_url = clean(std::env::var("LLAMA_BASE_URL").ok().as_deref());
        let hugging_face_url = clean(std::env::var("HF_ENDPOINT").ok().as_deref());
        crate::sandbox::BuiltinHttpEndpoints {
            memory_api_url,
            live_stt_url,
            realtime_url,
            llama_router_url,
            hugging_face_url,
            plugin_marketplace_url: None,
        }
    }

    async fn compose_candidate_host_trust(
        session: &Session,
        resources: &crate::ResourceManager,
        provenance: Arc<generation::GenerationProvenance>,
        extension_runtime: Option<&ExtensionRuntime>,
    ) -> Result<Option<crate::TrustResolution>> {
        let Some(candidate) = resources.stage_global_pre_trust_candidate(provenance.clone())?
        else {
            return Ok(None);
        };
        if !Arc::ptr_eq(candidate.provenance(), &provenance) {
            bail!("global pre-trust candidate provenance does not match staging lease");
        }
        let settings = candidate.settings();
        let options = resources.options();
        let default_trust = settings
            .default_project_trust
            .unwrap_or(crate::DefaultProjectTrust::Ask);
        let (mut resolution, observation) = crate::trust::resolve_project_trust_with_observation(
            &resources.trust_store(),
            &options.cwd,
            options.project_trust_override,
            default_trust,
            options.headless,
        )?;
        let Some(observation) = observation else {
            return Ok(Some(resolution));
        };
        let hooks = crate::HostHooks::new_host(
            settings.hooks.clone().unwrap_or_default(),
            session.cwd().to_path_buf(),
            session.session_policy_id(),
        );
        let host_blocked = hooks
            .fire_trust_decision(
                &observation.path.to_string_lossy(),
                observation.decision.as_str(),
                observation.is_new,
            )
            .await
            .block;
        let extension_approved = if let Some(runtime) = extension_runtime {
            runtime
                .reduce_trust_decision(observation.to_payload())
                .await?
                .is_some_and(|reduction| reduction.approve)
        } else {
            false
        };
        resolution.decision = crate::trust::apply_trust_hook_outcomes(
            observation.decision,
            host_blocked,
            extension_approved,
        );
        Ok(Some(resolution))
    }

    async fn stage_bootstrap_generation(
        session: &Session,
        resources: &crate::ResourceManager,
        extension_runtime: Option<&(ExtensionRuntime, ExtensionPermissionSet)>,
        orchestration_runtime: Option<&crate::OrchestrationRuntime>,
        goal_tool_binding: Option<&GoalToolBinding>,
        todo_child_tool_binding: Option<&TodoChildToolBinding>,
        lease: BootstrapGenerationLease,
    ) -> Result<(Arc<ApplicationPublication>, crate::SessionResourceUpdate)> {
        let provenance = lease.provenance().clone();
        let model = session
            .model()
            .ok_or_else(|| anyhow!("session has no model"))?;
        let request_auth = session.request_auth_for_model(&model).await?;
        let stream = session.stream_options().stream;
        let composed_trust = Self::compose_candidate_host_trust(
            session,
            resources,
            provenance.clone(),
            extension_runtime.map(|(runtime, _)| runtime),
        )
        .await?;
        let candidate =
            resources.stage_initial_candidate(provenance.clone(), composed_trust.as_ref())?;
        let settings = candidate.snapshot().settings.clone();
        let image_generation =
            Self::staged_image_generation_http(session, &settings, &model, &request_auth).await?;
        let policy_inputs = crate::sandbox::SessionPolicyInputs::validate_with_capabilities(
            session.session_policy_id(),
            session.workspace_roots().clone(),
            resources.options().agent_dir,
            session.session_dir(),
            candidate.capabilities(),
        )?;
        let sandbox = crate::sandbox::build_sandbox_generation(
            settings.sandbox.as_ref(),
            &settings.mcp_servers,
            pi_sandbox::PolicyRevision::new(provenance.revision().get())?,
            provenance.clone(),
            candidate.capabilities(),
            policy_inputs,
            &model,
            &request_auth,
            &stream,
            image_generation
                .as_ref()
                .map(|image| (&image.model, image.base_url.as_deref(), &image.env)),
            &Self::builtin_http_endpoints(&settings),
        )?;

        let resource = candidate.finish(sandbox)?;
        let extension_candidate = if let Some((runtime, permissions)) = extension_runtime {
            let specs = resource.extension_specs(permissions)?;
            let staged = runtime
                .stage_reload(
                    specs,
                    crate::extensions::ExtensionCandidateAuthority::new(
                        resource.sandbox().clone(),
                        provenance.clone(),
                        session.auth_manager(),
                    ),
                )
                .await;
            let report = staged.report();
            if !report.failures.is_empty() {
                runtime.discard_reload(staged).await;
                return Err(extension_load_report_error("extension startup", &report));
            }
            Some(staged)
        } else {
            None
        };
        let mut composed_trust = composed_trust;
        if let (Some((runtime, _)), Some(staged), Some(trust)) = (
            extension_runtime,
            extension_candidate.as_ref(),
            composed_trust.as_mut(),
        ) && trust.decision == crate::TrustDecision::Ask
        {
            let default_trust = settings
                .default_project_trust
                .unwrap_or(crate::DefaultProjectTrust::Ask);
            let (_, observation) = crate::trust::resolve_project_trust_with_observation(
                &resources.trust_store(),
                &resources.options().cwd,
                resources.options().project_trust_override,
                default_trust,
                resources.options().headless,
            )?;
            if let Some(observation) = observation {
                let extension_approved = staged
                    .reduce_trust_decision(runtime, observation.to_payload())
                    .await?
                    .is_some_and(|reduction| reduction.approve);
                trust.decision = crate::trust::apply_trust_hook_outcomes(
                    observation.decision,
                    false,
                    extension_approved,
                );
            }
        }
        let resource = if composed_trust
            .as_ref()
            .is_some_and(|trust| trust.decision == crate::TrustDecision::Trusted)
        {
            let restaged =
                resources.stage_initial_candidate(provenance.clone(), composed_trust.as_ref())?;
            let restaged_settings = restaged.snapshot().settings.clone();
            let restaged_image = Self::staged_image_generation_http(
                session,
                &restaged_settings,
                &model,
                &request_auth,
            )
            .await?;
            let restaged_inputs = crate::sandbox::SessionPolicyInputs::validate_with_capabilities(
                session.session_policy_id(),
                session.workspace_roots().clone(),
                resources.options().agent_dir,
                session.session_dir(),
                restaged.capabilities(),
            )?;
            let restaged_sandbox = crate::sandbox::build_sandbox_generation(
                restaged_settings.sandbox.as_ref(),
                &restaged_settings.mcp_servers,
                pi_sandbox::PolicyRevision::new(provenance.revision().get())?,
                provenance.clone(),
                restaged.capabilities(),
                restaged_inputs,
                &model,
                &request_auth,
                &stream,
                restaged_image
                    .as_ref()
                    .map(|image| (&image.model, image.base_url.as_deref(), &image.env)),
                &Self::builtin_http_endpoints(&restaged_settings),
            )?;
            restaged.finish(restaged_sandbox)?
        } else {
            resource
        };

        // Generation-owned tools: provisioned fresh in every generation from
        // the same owners — bootstrap here, reload at the matching site — and
        // never persisted into the session's base tools.
        let mut additional_tools = Vec::new();
        if let (Some((runtime, _)), Some(candidate)) =
            (extension_runtime, extension_candidate.as_ref())
        {
            additional_tools.extend(candidate.agent_tools(runtime));
        }
        if let Some(orchestration) = orchestration_runtime {
            additional_tools.extend(orchestration.agent_tools("Main", 0));
        }
        if let Some(binding) = goal_tool_binding {
            additional_tools.push(binding.tool());
        }
        if let Some(binding) = todo_child_tool_binding {
            additional_tools.extend(binding.tools());
        }
        let (tools, update) = session.stage_tool_generation(
            provenance.clone(),
            resource.clone(),
            additional_tools,
        )?;
        let extensions = match (extension_runtime, extension_candidate.as_ref()) {
            (Some((runtime, permissions)), Some(candidate)) => ExtensionGeneration::enabled(
                provenance.clone(),
                runtime.clone(),
                candidate.stage_snapshot(),
                Arc::new(permissions.clone()),
            )?,
            (None, None) => ExtensionGeneration::disabled(provenance.clone()),
            _ => bail!("extension startup candidate state is inconsistent"),
        };
        let orchestration = match orchestration_runtime {
            Some(runtime) => runtime.stage_generation(provenance.clone())?,
            None => OrchestrationGeneration::disabled(provenance.clone()),
        };
        let runtime = RuntimeGeneration::new(provenance.clone(), update.runtime_settings.clone());
        let capture_generation = resources.capture_fn();
        let host_hooks = settings
            .hooks
            .clone()
            .filter(|hooks| !hooks.is_empty())
            .map(|hooks| {
                Arc::new(crate::HostHooks::new(
                    hooks,
                    session.cwd().to_path_buf(),
                    session.session_policy_id(),
                    capture_generation.clone(),
                ))
            });
        let security = SecurityGeneration::new(
            provenance.clone(),
            Arc::new(crate::sandbox::ApprovedSpawnRegistry::default()),
            settings.permission_rules.clone(),
            host_hooks,
        );
        let generation = ApplicationGeneration::new(
            provenance,
            resource,
            tools,
            extensions,
            orchestration,
            runtime,
            security,
            session.brush_exec_lock(),
        )?;
        let publication = lease.finish(generation)?;

        if let (Some((runtime, _)), Some(candidate)) = (extension_runtime, extension_candidate) {
            candidate.activate_after_application_publish(runtime)?;
        }
        Ok((publication, update))
    }

    async fn build(
        session: Session,
        extension_runtime: Option<(ExtensionRuntime, ExtensionPermissionSet)>,
        orchestration_runtime: Option<crate::OrchestrationRuntime>,
        goal_tool_binding: Option<GoalToolBinding>,
        todo_child_tool_binding: Option<TodoChildToolBinding>,
    ) -> Result<Self> {
        let lease = BootstrapGenerationLease::new()?;
        let attached_resources = session.resource_manager();
        let resources = match attached_resources.clone() {
            Some(resources) => resources,
            None => crate::ResourceManager::new(crate::ResourceManagerOptions::new(session.cwd()))?,
        };
        let (publication, update) = Self::stage_bootstrap_generation(
            &session,
            &resources,
            extension_runtime.as_ref(),
            orchestration_runtime.as_ref(),
            goal_tool_binding.as_ref(),
            todo_child_tool_binding.as_ref(),
            lease,
        )
        .await?;
        resources.bind_publication(&publication)?;
        session.publish_resource_manager(resources, update).await;
        let (events, _) = broadcast::channel(EVENT_BUFFER_CAPACITY);
        let loop_storage = session
            .recorder_file_authority()
            .map(|(lease, relative)| {
                crate::loop_scheduler::LoopStorage::from_session_authority(lease, &relative)
            })
            .transpose()?;
        let initial_runtime = runtime::ApplicationRuntime::new(
            runtime::INITIAL_RUNTIME_EPOCH,
            session,
            extension_runtime,
            orchestration_runtime,
            goal_tool_binding,
            todo_child_tool_binding,
        );
        initial_runtime.bind_publication(publication.clone())?;
        if let Some(orchestration) = initial_runtime.orchestration_runtime() {
            orchestration.bind_generation_capture(publication.capture_fn())?;
        }
        let inner = Arc::new(ApplicationInner {
            runtime: runtime::ApplicationRuntimeSlot::new(initial_runtime, events.clone()),
            runtime_factory: Mutex::new(None),
            todo_child_client: Mutex::new(TodoChildClient::default()),
            events,
            steered_main_message_ids: Mutex::new(std::collections::BTreeSet::new()),
            resident_orchestration: Mutex::new(std::collections::HashMap::new()),
            workflow_manager: Mutex::new(None),
            workflow_runtime_factory: Mutex::new(None),
            workflow_events: Mutex::new(None),
            settings_watch: Mutex::new(Vec::new()),
            loop_scheduler: Mutex::new(None),
            operation_gate: Arc::new(AsyncMutex::new(())),
            cutover_before_spawn_gate: Mutex::new(None),
            orchestration_commit_gate: Mutex::new(None),
            prompt_before_turn_start_gate: Mutex::new(None),
            prompt_after_settle_gate: Mutex::new(None),
            cutover_after_loop_commit_gate: Mutex::new(None),
            cutover_after_resident_take_gate: Mutex::new(None),
            cleanup_flight: AsyncMutex::new(None),
            #[cfg(test)]
            cleanup_gate: Mutex::new(None),
            #[cfg(test)]
            cleanup_teardown_entries: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            cleanup_teardown_completions: std::sync::atomic::AtomicUsize::new(0),
            repository_test_executor: Mutex::new(None),
            cleaned: AtomicBool::new(false),
            restart_checkpointed: AtomicBool::new(false),
        });
        let application = Self { inner };
        let generation = application.runtime();
        // Goal binding happens once inside bind_runtime_generation.
        if let Some(binding) = generation.todo_child_tool_binding() {
            binding.bind(&application)?;
        }
        application
            .bind_runtime_generation(generation.clone(), None)
            .await?;
        let loop_runner_inner = Arc::downgrade(&application.inner);
        let loop_runner: crate::LoopTurnRunner = Arc::new(move |request, cancel| {
            let inner = loop_runner_inner.clone();
            Box::pin(async move { run_loop_turn(inner, request, cancel).await })
        });
        let loop_event_inner = Arc::downgrade(&application.inner);
        let loop_events: crate::LoopEventSink = Arc::new(move |event| {
            if let Some(inner) = loop_event_inner.upgrade() {
                inner.publish(ApplicationEvent::Loop(event));
            }
        });
        let loop_scheduler = crate::start_loop_scheduler(loop_storage, loop_runner, loop_events);
        *application.inner.loop_scheduler.lock() = Some(loop_scheduler);
        application.start_settings_watch();
        Ok(application)
    }

    fn start_settings_watch(&self) {
        let Ok(manager) = self.settings_manager() else {
            return;
        };
        let Ok((watcher, mut reload_rx)) = crate::settings_watch::spawn(manager.paths()) else {
            return;
        };
        let weak = Arc::downgrade(&self.inner);
        let reloader = tokio::spawn(async move {
            while reload_rx.recv().await.is_some() {
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                let application = Application { inner };
                if application.reload().await.is_err() {
                    continue;
                }
            }
        });
        *self.inner.settings_watch.lock() = vec![watcher, reloader];
    }

    fn prepare_runtime_generation_bindings(
        &self,
        generation: &Arc<runtime::ApplicationRuntime>,
    ) -> Result<PreparedRuntimeGenerationBindings> {
        let orchestration = generation.orchestration_runtime();
        self.prepare_runtime_generation_bindings_for_orchestration(
            generation,
            orchestration.as_ref(),
        )
    }

    fn prepare_runtime_generation_bindings_for_orchestration(
        &self,
        generation: &Arc<runtime::ApplicationRuntime>,
        orchestration: Option<&crate::OrchestrationRuntime>,
    ) -> Result<PreparedRuntimeGenerationBindings> {
        let orchestration = match orchestration {
            Some(orchestration) => {
                let mut binding = orchestration.prepare_parent_binding(&generation.session)?;
                orchestration.initialize_prepared_parent(&mut binding)?;
                Some((orchestration.clone(), binding))
            }
            None => None,
        };
        if let Some((extension, _)) = generation.extension_runtime() {
            extension.set_action_host(Arc::new(ApplicationExtensionHost {
                application: Arc::downgrade(&self.inner),
            }))?;
        }
        let goal = generation
            .goal_tool_binding()
            .map(|binding| PreparedGoalToolBinding::new(binding, self))
            .transpose()?;
        let todo_child = generation
            .todo_child_tool_binding()
            .map(|binding| PreparedTodoChildToolBinding::new(binding, self))
            .transpose()?;
        Ok(PreparedRuntimeGenerationBindings {
            orchestration,
            goal,
            todo_child,
        })
    }

    async fn bind_runtime_generation(
        &self,
        generation: Arc<runtime::ApplicationRuntime>,
        prepared: Option<PreparedRuntimeGenerationBindings>,
    ) -> Result<()> {
        let epoch = generation.epoch();
        let session = generation.session();
        let extension = generation.extension_runtime().map(|(runtime, _)| runtime);
        let mut prepared = prepared;
        let installed_orchestration = prepared
            .as_mut()
            .and_then(PreparedRuntimeGenerationBindings::install_orchestration);
        if prepared.is_none()
            && let Some(orchestration) = generation.orchestration_runtime()
        {
            let mut binding = orchestration.prepare_parent_binding(&session)?;
            orchestration.initialize_prepared_parent(&mut binding)?;
            orchestration.install_prepared_parent(binding);
        }
        let jobs = generation
            .orchestration_runtime()
            .map(|runtime| runtime.jobs(None))
            .unwrap_or_default();
        generation
            .todo_dag
            .lock()
            .restore_from_session_with_jobs(&generation.session, &jobs);
        for job in jobs.iter().filter(|job| job.status.is_settled()) {
            self.inner
                .record_terminal_todo_job_for_recovery(&generation, job)?;
        }
        if let Some(orchestration) = generation.orchestration_runtime()
            && (installed_orchestration.is_some() || prepared.is_none())
        {
            orchestration.revive_prepared_parent();
        }
        if let Some(runtime) = &extension {
            // Route this session's extension-owned provider lookups through
            // this runtime's namespace (fail closed on any other runtime's
            // registration of the same api).
            session.set_provider_namespace(Some(runtime.provider_namespace().to_owned()));
            if prepared.is_none() {
                runtime.set_action_host(Arc::new(ApplicationExtensionHost {
                    application: Arc::downgrade(&self.inner),
                }))?;
            }
            // Host hooks (Settings.hooks) exclude extension tool calls in the
            // MVP: record the extension-provided tool names so the session's
            // pre/post_tool_call firing points can skip them.
            session
                .set_extension_tool_names(runtime.agent_tools().into_iter().map(|tool| tool.name));
            let before_start_runtime = runtime.clone();
            session.set_before_agent_start(Some(Arc::new(move |context| {
                let runtime = before_start_runtime.clone();
                Box::pin(async move {
                    let reduction = runtime
                        .reduce_before_agent_start(
                            serde_json::json!({
                                "prompt": prompt_text(&context.messages),
                                "images": prompt_images(&context.messages),
                            }),
                            context.system_prompt,
                        )
                        .await?;
                    let mut messages = context.messages;
                    messages.extend(reduction.messages.into_iter().map(extension_custom_message));
                    Ok(pi_agent::BeforeAgentStartResult {
                        system_prompt: reduction.system_prompt,
                        messages,
                    })
                })
            })));
            let context_runtime = runtime.clone();
            session.set_transform_context(Some(Arc::new(move |messages, _| {
                let runtime = context_runtime.clone();
                Box::pin(async move { runtime.reduce_context(messages).await })
            })));
            let message_runtime = runtime.clone();
            session.set_transform_message(Some(Arc::new(move |message, _| {
                let runtime = message_runtime.clone();
                Box::pin(async move { runtime.reduce_message_end(message).await })
            })));
            let host_hook = session.raw_before_tool_call();
            let tool_call_runtime = runtime.clone();
            let extension_hook = Arc::new(move |context: pi_agent::BeforeToolCallContext| {
                let runtime = tool_call_runtime.clone();
                Box::pin(async move {
                    let reduction = runtime
                        .reduce_tool_call(
                            &context.tool_call.id,
                            &context.tool_call.name,
                            context.arguments,
                        )
                        .await?;
                    Ok(pi_agent::BeforeToolCallResult {
                        block: reduction.block,
                        reason: reduction.reason,
                        arguments: Some(reduction.input),
                        approval_token: None,
                    })
                }) as pi_agent::BoxFuture<Result<pi_agent::BeforeToolCallResult>>
            });
            session.set_before_tool_call(compose_before_tool_call(host_hook, Some(extension_hook)));
            let tool_result_runtime = runtime.clone();
            session.set_after_tool_call(Some(Arc::new(move |context| {
                let runtime = tool_result_runtime.clone();
                Box::pin(async move {
                    let reduction = runtime
                        .reduce_tool_result(
                            &context.tool_call.id,
                            &context.tool_call.name,
                            context.arguments,
                            context.result.content,
                            Some(context.result.details),
                            context.is_error,
                        )
                        .await?;
                    Ok(pi_agent::AfterToolCallResult {
                        content: Some(reduction.content),
                        details: reduction.details,
                        is_error: Some(reduction.is_error),
                        usage: reduction.usage.map(extension_usage),
                        terminate: None,
                    })
                })
            })));
            let mut stream_options = session.stream_options();
            let request_runtime = runtime.clone();
            stream_options.stream.before_provider_request = Some(Arc::new(move |payload, _| {
                let runtime = request_runtime.clone();
                Box::pin(async move { runtime.reduce_provider_request(payload).await })
            }));
            let headers_runtime = runtime.clone();
            stream_options.stream.before_provider_headers = Some(Arc::new(move |headers, _| {
                let runtime = headers_runtime.clone();
                Box::pin(async move { runtime.reduce_provider_headers(headers).await })
            }));
            let response_runtime = runtime.clone();
            stream_options.stream.after_provider_response = Some(Arc::new(move |response, _| {
                let runtime = response_runtime.clone();
                Box::pin(async move {
                    runtime
                        .emit_checked(ExtensionEvent::new(
                            "after_provider_response",
                            serde_json::json!({ "status": response.status, "headers": response.headers }),
                        ))
                        .await
                })
            }));
            session.set_stream_options(stream_options).await;
            let compact_runtime = runtime.clone();
            session.set_before_compaction(Some(Arc::new(move |context| {
                let runtime = compact_runtime.clone();
                Box::pin(async move {
                    let reduction = runtime
                        .reduce_before_compact(serde_json::json!({
                            "customInstructions": context.custom_instructions,
                            "reason": context.reason,
                            "willRetry": context.will_retry,
                        }))
                        .await?;
                    Ok(crate::BeforeCompactionResult {
                        cancel: reduction.cancel,
                        compaction: reduction.compaction,
                    })
                })
            })));
        }
        let todo_gate = generation.todo_dag.lock().reconcile_gate.clone();
        let todo_check_inner = Arc::downgrade(&self.inner);
        let todo_commit_inner = Arc::downgrade(&self.inner);
        session.set_todo_mutation_transaction(crate::todo::TodoMutationTransaction {
            gate: todo_gate,
            check: Arc::new(move || {
                let inner = todo_check_inner
                    .upgrade()
                    .ok_or_else(|| anyhow!("application stopped"))?;
                if inner
                    .runtime()
                    .todo_transition_active
                    .load(Ordering::Acquire)
                {
                    return Err(anyhow!("Todo mutation rejected during session transition"));
                }
                Ok(())
            }),
            commit: Arc::new(move || {
                let Some(inner) = todo_commit_inner.upgrade() else {
                    return;
                };
                // Workflow jobs own their DAG transitions; the ambient mutation hook
                // must not auto-arm workflow children through the main-session DAG.
                let workflow_owned = inner
                    .runtime()
                    .orchestration_runtime()
                    .is_some_and(|runtime| runtime.workflow_scope().is_some());
                if workflow_owned {
                    return;
                }
                if let Err(error) = inner.arm_todo_dag_after_mutation_locked() {
                    inner.todo_dag_failed(&error);
                }
            }),
        });
        let prepared_goal = if prepared.is_none() {
            generation
                .goal_tool_binding()
                .map(|binding| PreparedGoalToolBinding::new(binding, self))
                .transpose()?
        } else {
            None
        };
        let agent_barrier_base = session.agent_event_barrier();
        generation.reset_agent_event_barrier_base(agent_barrier_base);
        let agent_inner = Arc::downgrade(&self.inner);
        let event_session = session.clone();
        let agent_extension = extension.clone();
        let subscription = session
            .subscribe(move |event| {
                let inner = agent_inner.clone();
                let session = event_session.clone();
                let extension = agent_extension.clone();
                async move {
                    let Some(inner) = inner.upgrade() else { return Ok(()); };
                    if let Some(runtime) = &extension
                        && let Some(extension_event) = agent_extension_event(&event)
                    {
                        let _ = runtime.emit(extension_event).await;
                    }
                    if matches!(&event, AgentEvent::ToolExecutionEnd { tool_name, is_error: false, .. } if tool_name == "todo") {
                        let outcome = inner.todo_dag_snapshot();
                        let _ = inner.runtime.publish(epoch, ApplicationEvent::TodoUpdated {
                            phases: session.todo_state().phases,
                            completed_tasks: Vec::new(),
                            execution_status: outcome.status,
                            failure_reason: outcome.failure_reason,
                        });
                    }
                    // The Session history subscription (registered first at
                    // Session construction) increments the barrier before this
                    // listener runs, so the read is this event's exact
                    // post-history ordinal on the runtime-relative scale the
                    // protocol snapshot also reports.
                    let barrier = session
                        .agent_event_barrier()
                        .saturating_sub(agent_barrier_base);
                    let _ = inner.runtime.publish(
                        epoch,
                        ApplicationEvent::AgentPublished { event, epoch, barrier },
                    );
                    Ok(())
                }
            })
            .await;
        *generation.session_subscription.lock() = Some(subscription);

        let mut session_events = session.subscribe_session_events();
        let session_inner = Arc::downgrade(&self.inner);
        let session_extension = extension;
        *generation.session_events.lock() = Some(tokio::spawn(async move {
            while let Ok(event) = session_events.recv().await {
                let Some(inner) = session_inner.upgrade() else {
                    break;
                };
                if let Some(extension_event) = session_extension_event(&event)
                    && let Some(runtime) = &session_extension
                {
                    let _ = runtime.emit(extension_event).await;
                }
                if let SessionEvent::EntryAppended { entry } = &event
                    && entry.custom_type.as_deref()
                        == Some(crate::orchestration::ORCHESTRATION_MESSAGE_TYPE)
                    && let Some(details) = entry.details.as_ref()
                    && let Some(message_id) = details.get("id").and_then(Value::as_str)
                    && let Some(orchestration) = inner.runtime().orchestration_runtime()
                {
                    if let Err(error) = orchestration.acknowledge_main_delivery(message_id) {
                        inner.report_main_delivery_ack_failure(message_id, &error);
                    } else {
                        inner.steered_main_message_ids.lock().remove(message_id);
                    }
                }
                let _ = inner
                    .runtime
                    .publish(epoch, ApplicationEvent::Session(event));
            }
        }));

        let mut process_events = generation.process_manager.subscribe();
        let process_inner = Arc::downgrade(&self.inner);
        let owner = generation.process_owner_id.clone();
        *generation.process_events.lock() = Some(tokio::spawn(async move {
            loop {
                match process_events.recv().await {
                    Ok(event) if process_event_owner(&event) == &owner => {
                        let Some(inner) = process_inner.upgrade() else {
                            break;
                        };
                        let _ = inner
                            .runtime
                            .publish(epoch, ApplicationEvent::Process(event));
                    }
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }));

        if let Some(prepared) = prepared {
            prepared.commit_non_orchestration();
        }
        if let Some(orchestration) = generation.orchestration_runtime() {
            let mut events = orchestration.subscribe();
            self.inner
                .replay_main_mailbox(&generation, &orchestration)
                .await;
            let event_runtime = orchestration.clone();
            let orchestration_inner = Arc::downgrade(&self.inner);
            let event_generation = generation.clone();
            *generation.orchestration_events.lock() = Some(tokio::spawn(async move {
                loop {
                    match events.recv().await {
                        Ok(event) => {
                            let Some(inner) = orchestration_inner.upgrade() else {
                                break;
                            };
                            let active = inner.runtime();
                            if active.epoch() != epoch
                                || !active.orchestration_runtime().is_some_and(|current| {
                                    current.shares_runtime(&event_runtime)
                                })
                            {
                                break;
                            }
                            if matches!(&event, crate::OrchestrationEvent::JobUpdated { job, .. } if !job.status.is_settled())
                            {
                                active.todo_cycle_pending.store(true, Ordering::Release);
                            }
                            let allow_spawn =
                                !active.todo_continuation_suppressed.load(Ordering::Acquire);
                            if let Err(error) = inner.observe_orchestration_event(
                                &event_runtime,
                                &event,
                                allow_spawn,
                            ) {
                                inner.todo_dag_failed(&error);
                            }
                            if let Err(error) = inner
                                .steer_main_delivered_message(&event_generation, &event)
                                .await
                            {
                                inner.report_steer_main_delivery_failure(&event, &error);
                            }
                            let _ = inner
                                .runtime
                                .publish(epoch, ApplicationEvent::Orchestration(event));
                            inner.finish_todo_cycle_if_idle(Some(&event_runtime), false);
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            let Some(inner) = orchestration_inner.upgrade() else {
                                break;
                            };
                            let active = inner.runtime();
                            if active.epoch() != epoch
                                || !active.orchestration_runtime().is_some_and(|current| {
                                    current.shares_runtime(&event_runtime)
                                })
                            {
                                break;
                            }
                            for event in event_runtime.presentation_events() {
                                if matches!(&event, crate::OrchestrationEvent::JobUpdated { job, .. } if !job.status.is_settled())
                                {
                                    active.todo_cycle_pending.store(true, Ordering::Release);
                                }
                                let allow_spawn =
                                    !active.todo_continuation_suppressed.load(Ordering::Acquire);
                                if let Err(error) = inner.observe_orchestration_event(
                                    &event_runtime,
                                    &event,
                                    allow_spawn,
                                ) {
                                    inner.todo_dag_failed(&error);
                                }
                                let _ = inner
                                    .runtime
                                    .publish(epoch, ApplicationEvent::Orchestration(event));
                                inner.finish_todo_cycle_if_idle(Some(&event_runtime), false);
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }));
        }
        if let Some(goal) = prepared_goal {
            goal.commit();
        }
        Ok(())
    }

    fn captured_extension_runtime(&self) -> Option<ExtensionRuntime> {
        self.capture_generation().extensions().runtime().cloned()
    }

    #[must_use]
    pub fn extension_runtime(&self) -> Option<ExtensionRuntime> {
        self.captured_extension_runtime()
    }

    #[must_use]
    pub fn runtime_settings(&self) -> Arc<crate::RuntimeSettingsSnapshot> {
        self.runtime().runtime_settings.lock().clone()
    }

    #[must_use]
    pub fn runtime_settings_state(&self) -> crate::RuntimeSettingsState {
        self.runtime_settings().state()
    }

    pub fn settings_manager(&self) -> Result<crate::SettingsManager> {
        self.runtime()
            .session
            .resource_manager()
            .ok_or_else(|| anyhow!("session has no resource manager"))?
            .settings_manager()
    }

    #[must_use]
    pub fn settings_catalog_snapshot(&self) -> Option<crate::SettingsCatalogSnapshot> {
        self.settings_manager()
            .ok()
            .map(|manager| crate::SettingsCatalog::inspect(&manager))
    }
    pub fn settings_draft(&self, scope: crate::SettingsScope) -> Result<crate::SettingsDraft> {
        let runtime = self.runtime();
        let mut draft = crate::SettingsCatalog::draft(&self.settings_manager()?, scope)?;
        draft.overlay_runtime_thinking_level(runtime.session.thinking_level());
        Ok(draft)
    }

    /// Persist one atomic settings draft and dispatch its runtime behavior.
    /// Reload changes are only reported as applied when the reload succeeds;
    /// restart changes persist but truthfully remain pending process restart.
    pub async fn apply_settings_draft(
        &self,
        draft: crate::SettingsDraft,
    ) -> Result<SettingApplyOutcome> {
        let manager = self.settings_manager()?;
        let writes = draft.apply(&manager)?;
        if writes.is_empty() {
            return Ok(SettingApplyOutcome {
                writes,
                applied_live: true,
                reloaded: false,
                restart_required: false,
                reload_generation: None,
            });
        }
        let needs_reload = writes.iter().any(|write| write.needs_reload);
        let restart_required = writes.iter().any(|write| write.needs_restart);
        if needs_reload {
            let reload = self.reload().await?;
            return Ok(SettingApplyOutcome {
                writes,
                applied_live: true,
                reloaded: true,
                restart_required,
                reload_generation: Some(reload.generation),
            });
        }
        if restart_required {
            let has_live = writes
                .iter()
                .any(|write| write.behavior == crate::SettingApplyBehavior::Live);
            if has_live {
                let settings = manager.settings().runtime_settings()?;
                let active = self.runtime();
                active
                    .session
                    .apply_runtime_settings(settings.clone())
                    .await;
                *active.runtime_settings.lock() = Arc::new(settings);
            }
            return Ok(SettingApplyOutcome {
                writes,
                applied_live: has_live,
                reloaded: false,
                restart_required: true,
                reload_generation: None,
            });
        }

        let settings = manager.settings().runtime_settings()?;
        let active = self.runtime();
        active
            .session
            .apply_runtime_settings(settings.clone())
            .await;
        *active.runtime_settings.lock() = Arc::new(settings);
        Ok(SettingApplyOutcome {
            writes,
            applied_live: true,
            reloaded: false,

            restart_required: false,
            reload_generation: None,
        })
    }
    #[must_use]
    pub fn attach_goal_tool(&self, binding: GoalToolBinding) -> Result<()> {
        let active = self.runtime();
        let mut current = active.goal_tool_binding.lock();
        if current.is_some() {
            return Err(anyhow!("application goal tool is already configured"));
        }
        let prepared = PreparedGoalToolBinding::new(binding.clone(), self)?;
        *current = Some(binding);
        prepared.commit();
        Ok(())
    }

    pub fn orchestration_runtime(&self) -> Option<crate::OrchestrationRuntime> {
        self.runtime().orchestration_runtime()
    }

    pub fn create_code_review_orchestration(
        &self,
        review_id: &str,
    ) -> Result<Option<crate::OrchestrationRuntime>> {
        let Some(parent) = self.orchestration_runtime() else {
            return Ok(None);
        };
        Ok(Some(
            parent.create_unbound_review_runtime(review_id, &self.session())?,
        ))
    }
    pub fn attach_todo_child_client(&self, client: TodoChildClient) -> Result<()> {
        let service = client.service()?;
        if let Some(factory) = self.inner.runtime_factory.lock().clone() {
            service.configure_factory(factory)?;
        }
        let mut current = self.inner.todo_child_client.lock();
        if let Ok(current_service) = current.service() {
            if Arc::ptr_eq(&current_service, &service) {
                return Ok(());
            }
            bail!("application Todo child service is already configured");
        }
        *current = client;
        Ok(())
    }

    fn todo_child_service(&self) -> Result<Arc<TodoChildService>> {
        self.inner.todo_child_client.lock().service()
    }

    pub async fn todo_child_create(
        &self,
        request: TodoChildCreateRequest,
    ) -> Result<TodoChildCreateResult> {
        let parent = self.runtime().session();
        self.todo_child_service()?
            .create_child(&parent, request)
            .await
    }

    pub async fn todo_child_send(
        &self,
        parent_session_id: Option<&str>,
        child_session_id: Option<&str>,
        message: String,
    ) -> Result<TodoChildReceipt> {
        let parent = self.runtime().session();
        self.todo_child_service()?
            .send(&parent, parent_session_id, child_session_id, message)
            .await
    }

    pub async fn todo_child_add_task(
        &self,
        parent_session_id: Option<&str>,
        child_session_id: &str,
        request: TodoChildAddTaskRequest,
    ) -> Result<TodoChildAddTaskResult> {
        let parent = self.runtime().session();
        self.todo_child_service()?
            .add_task(&parent, parent_session_id, child_session_id, request)
            .await
    }

    pub async fn todo_child_detail(
        &self,
        parent_session_id: &str,
        child_session_id: &str,
    ) -> Result<TodoChildDetailSnapshot> {
        let parent = self.runtime().session();
        self.todo_child_service()?
            .detail(&parent, Some(parent_session_id), child_session_id)
            .await
    }

    pub async fn todo_child_hub_send(
        &self,
        parent_session_id: &str,
        child_session_id: &str,
        recipient: &str,
        body: &str,
        reply_to: Option<String>,
    ) -> Result<Vec<crate::DeliveryReceipt>> {
        let parent = self.runtime().session();
        self.todo_child_service()?
            .hub_send(
                &parent,
                Some(parent_session_id),
                child_session_id,
                recipient,
                body,
                reply_to,
            )
            .await
    }

    pub async fn todo_child_status(
        &self,
        parent_session_id: Option<&str>,
        child_session_id: Option<&str>,
    ) -> Result<TodoChildSnapshot> {
        let parent = self.runtime().session();
        self.todo_child_service()?
            .status(&parent, parent_session_id, child_session_id)
            .await
    }

    pub async fn todo_child_stop(
        &self,
        parent_session_id: Option<&str>,
        child_session_id: Option<&str>,
    ) -> Result<TodoChildSnapshot> {
        let parent = self.runtime().session();
        self.todo_child_service()?
            .stop(&parent, parent_session_id, child_session_id)
            .await
    }

    pub async fn todo_child_pause(
        &self,
        parent_session_id: Option<&str>,
        child_session_id: Option<&str>,
    ) -> Result<TodoChildSnapshot> {
        let parent = self.runtime().session();
        self.todo_child_service()?
            .pause(&parent, parent_session_id, child_session_id)
            .await
    }

    pub async fn todo_child_resume(
        &self,
        parent_session_id: Option<&str>,
        child_session_id: Option<&str>,
    ) -> Result<TodoChildSnapshot> {
        let parent = self.runtime().session();
        self.todo_child_service()?
            .resume(&parent, parent_session_id, child_session_id)
            .await
    }

    pub async fn todo_child_cancel(
        &self,
        parent_session_id: Option<&str>,
        child_session_id: Option<&str>,
    ) -> Result<Option<TodoChildSnapshot>> {
        let parent = self.runtime().session();
        self.todo_child_service()?
            .cancel(&parent, parent_session_id, child_session_id)
            .await
    }

    pub async fn todo_child_list(
        &self,
        parent_session_id: Option<&str>,
    ) -> Result<Vec<TodoChildManifestEntry>> {
        let parent = self.runtime().session();
        self.todo_child_service()?
            .list(&parent, parent_session_id)
            .await
    }

    pub async fn todo_child_select(
        &self,
        parent_session_id: Option<&str>,
        child_session_id: &str,
    ) -> Result<()> {
        let parent = self.runtime().session();
        self.todo_child_service()?
            .select(&parent, parent_session_id, child_session_id)
            .await
    }

    pub async fn todo_child_archive(
        &self,
        parent_session_id: Option<&str>,
        child_session_id: &str,
    ) -> Result<()> {
        let parent = self.runtime().session();
        self.todo_child_service()?
            .archive(&parent, parent_session_id, child_session_id)
            .await
    }

    pub async fn todo_child_remove(
        &self,
        parent_session_id: Option<&str>,
        child_session_id: &str,
    ) -> Result<()> {
        let parent = self.runtime().session();
        self.todo_child_service()?
            .remove(&parent, parent_session_id, child_session_id)
            .await
    }

    pub fn attach_todo_child_tool(&self, binding: TodoChildToolBinding) -> Result<()> {
        let active = self.runtime();
        let mut current = active.todo_child_tool_binding.lock();
        if current.is_some() {
            return Err(anyhow!("application todo child tool is already configured"));
        }
        let prepared = PreparedTodoChildToolBinding::new(binding.clone(), self)?;
        *current = Some(binding);
        prepared.commit();
        Ok(())
    }

    pub async fn attach_orchestration(&self, runtime: crate::OrchestrationRuntime) -> Result<()> {
        self.attach_orchestration_with_override(runtime, true).await
    }

    pub async fn attach_orchestration_with_override(
        &self,
        runtime: crate::OrchestrationRuntime,
        explicit: bool,
    ) -> Result<()> {
        let active = self.runtime();
        if active.session.recorder_info().is_some() {
            runtime
                .bind_and_recover(&active.session)
                .context("binding orchestration to application session")?;
        }
        let mut events = runtime.subscribe();
        let event_runtime = runtime.clone();
        {
            let mut current = active.orchestration_runtime.lock();
            if current.is_some() {
                return Err(anyhow!("application orchestration is already configured"));
            }
            *current = Some(runtime);
        }
        self.inner
            .replay_main_mailbox(&active, &event_runtime)
            .await;
        let event_inner = Arc::downgrade(&self.inner);
        let epoch = active.epoch();
        let event_task = tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        let Some(inner) = event_inner.upgrade() else {
                            break;
                        };
                        let active = inner.runtime();
                        if active.epoch() != epoch {
                            break;
                        }
                        if matches!(&event, crate::OrchestrationEvent::JobUpdated { job, .. } if !job.status.is_settled())
                        {
                            active.todo_cycle_pending.store(true, Ordering::Release);
                        }
                        let allow_spawn =
                            !active.todo_continuation_suppressed.load(Ordering::Acquire);
                        if let Err(error) =
                            inner.observe_orchestration_event(&event_runtime, &event, allow_spawn)
                        {
                            inner.todo_dag_failed(&error);
                        }
                        inner
                            .steer_main_delivered_message(&active, &event)
                            .await
                            .unwrap_or_else(|error| {
                                inner.report_steer_main_delivery_failure(&event, &error)
                            });
                        let _ = inner
                            .runtime
                            .publish(epoch, ApplicationEvent::Orchestration(event));
                        inner.finish_todo_cycle_if_idle(Some(&event_runtime), false);
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let Some(inner) = event_inner.upgrade() else {
                            break;
                        };
                        let active = inner.runtime();
                        if active.epoch() != epoch {
                            break;
                        }
                        for event in event_runtime.presentation_events() {
                            if matches!(&event, crate::OrchestrationEvent::JobUpdated { job, .. } if !job.status.is_settled())
                            {
                                active.todo_cycle_pending.store(true, Ordering::Release);
                            }
                            let allow_spawn =
                                !active.todo_continuation_suppressed.load(Ordering::Acquire);
                            if let Err(error) = inner.observe_orchestration_event(
                                &event_runtime,
                                &event,
                                allow_spawn,
                            ) {
                                inner.todo_dag_failed(&error);
                            }
                            // `presentation_events` is a UI snapshot (jobs and
                            // agents), never an unconsumed context queue:
                            // historical `MessageDelivered` must NOT be steered
                            // into the main session here.
                            let _ = inner
                                .runtime
                                .publish(epoch, ApplicationEvent::Orchestration(event));
                            inner.finish_todo_cycle_if_idle(Some(&event_runtime), false);
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        *active.orchestration_events.lock() = Some(event_task);
        active
            .orchestration_explicit
            .store(explicit, Ordering::Release);
        self.inner.arm_todo_dag_after_mutation()?;
        Ok(())
    }

    #[must_use]
    pub fn session(&self) -> Session {
        self.runtime().session()
    }

    #[must_use]
    pub fn get_active_tool_names(&self) -> Vec<String> {
        self.runtime().session.get_active_tool_names()
    }

    #[must_use]
    pub fn get_all_tools(&self) -> Vec<pi_agent::AgentTool> {
        self.runtime().session.get_all_tools()
    }

    #[must_use]
    pub fn get_tool_definition(&self, name: &str) -> Option<pi_agent::AgentTool> {
        self.runtime().session.get_tool_definition(name)
    }

    pub async fn set_active_tools_by_name(&self, names: &[String]) -> Result<()> {
        self.runtime().session.set_active_tools_by_name(names).await
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<ApplicationEvent> {
        self.inner.events.subscribe()
    }

    /// Test-only sender for protocol-owner lifecycle regressions that must
    /// inject a synthetic runtime event without constructing a Session.
    #[cfg(any(test, feature = "test-support"))]
    pub fn runtime_events_for_test(&self) -> broadcast::Sender<ApplicationEvent> {
        self.inner.events.clone()
    }

    #[must_use]
    pub fn todo_state(&self) -> crate::TodoState {
        self.runtime().session.todo_state()
    }

    pub fn apply_todo(&self, op: crate::TodoOp) -> Result<crate::TodoApplyResult> {
        let active = self.runtime();
        match active.session.apply_todo(op) {
            Ok(result) => {
                let outcome = self.inner.todo_dag_snapshot();
                self.inner.publish(ApplicationEvent::TodoUpdated {
                    phases: result.phases.clone(),
                    completed_tasks: result.completed_tasks.clone(),
                    execution_status: outcome.status,
                    failure_reason: outcome.failure_reason,
                });
                Ok(result)
            }
            Err(error) => {
                active.session.schedule_todo_reminder();
                Err(error)
            }
        }
    }

    pub fn set_todos(&self, phases: Vec<crate::TodoPhase>) -> Result<crate::TodoApplyResult> {
        let result = self.runtime().session.set_todos(phases)?;
        let outcome = self.inner.todo_dag_snapshot();
        self.inner.publish(ApplicationEvent::TodoUpdated {
            phases: result.phases.clone(),
            completed_tasks: result.completed_tasks.clone(),
            execution_status: outcome.status,
            failure_reason: outcome.failure_reason,
        });
        Ok(result)
    }
    #[must_use]
    pub fn goal_state(&self) -> GoalState {
        self.runtime().session.goal_runtime().get()
    }

    /// Replays the goal journal (every goal event on the active session
    /// branch) for the panel's history view. Fails when the session has no
    /// recorder attached (e.g. an unrecorded bare session).
    pub fn goal_journal(&self) -> Result<Vec<GoalEvent>> {
        self.runtime().session.goal_journal()
    }

    /// Deterministic handoff envelope (goal, todo counts, active orchestration
    /// jobs, environment, recent asks, next-step hints). No model call; the
    /// envelope is well-formed even for an empty session.
    pub fn generate_handoff(&self) -> Result<Handoff> {
        let jobs = self
            .orchestration_runtime()
            .map(|runtime| runtime.jobs(None))
            .unwrap_or_default();
        self.runtime().session.generate_handoff(&jobs)
    }

    /// Envelope plus a prose handoff paragraph from the existing summarization
    /// path (one bounded provider call, no retries).
    pub async fn generate_handoff_with_prose(&self) -> Result<Handoff> {
        let jobs = self
            .orchestration_runtime()
            .map(|runtime| runtime.jobs(None))
            .unwrap_or_default();
        self.runtime()
            .session
            .generate_handoff_with_prose(&jobs)
            .await
    }

    pub fn goal_create(
        &self,
        objective: impl Into<String>,
        token_budget: Option<u64>,
    ) -> Result<Goal> {
        self.goal_mutation("create", |runtime| runtime.create(objective, token_budget))
    }

    /// Creates a goal and immediately starts its first model turn, or queues
    /// that turn behind the Application's current work.
    ///
    /// The activation slot is reserved against the [`GoalState`] snapshot the
    /// create committed — goal id plus resulting revision — inside the same
    /// critical section as the commit (see
    /// [`crate::GoalRuntime::create_and_reserve`]), so the reservation is
    /// linearized with every lifecycle transition: no pause/complete/drop or
    /// newer create/resume can interleave between the commit and the
    /// reservation, and no concurrent transition can open a window between
    /// the commit and the schedule decision.
    pub async fn activate_goal(
        &self,
        objective: impl Into<String>,
        token_budget: Option<u64>,
    ) -> Result<GoalActivationOutcome> {
        let _operation = self.inner.operation_gate.lock().await;
        let active = self.runtime();
        let (state, outcome) = active
            .session
            .goal_runtime()
            .create_and_reserve(objective, token_budget, |state| {
                self.reserve_goal_work(&active, state)
            })
            .map_err(|error| anyhow!(error.to_string()))?;
        self.inner.publish(ApplicationEvent::GoalUpdated {
            operation: "create",
            state,
        });
        Ok(outcome)
    }

    pub fn goal_pause(&self) -> Result<Goal> {
        self.goal_mutation("pause", |runtime| runtime.pause())
    }

    pub fn goal_resume(&self) -> Result<Goal> {
        self.goal_mutation("resume", |runtime| runtime.resume())
    }

    /// Resumes a paused goal and schedules exactly one continuation for the
    /// resulting goal revision. Repeating resume on the same active revision
    /// never starts a duplicate turn.
    ///
    /// The activation slot is reserved against the [`GoalState`] snapshot the
    /// resume committed — goal id plus resulting revision — inside the same
    /// critical section as the commit (see
    /// [`crate::GoalRuntime::resume_and_reserve`]), so the reservation is
    /// linearized with every lifecycle transition: no pause/complete/drop or
    /// newer create/resume can interleave between the commit and the
    /// reservation, and no concurrent transition can open a window between
    /// the commit and the schedule decision.
    pub async fn resume_goal_work(&self) -> Result<GoalActivationOutcome> {
        let _operation = self.inner.operation_gate.lock().await;
        let active = self.runtime();
        let (state, outcome) = active
            .session
            .goal_runtime()
            .resume_and_reserve(|state| self.reserve_goal_work(&active, state))
            .map_err(|error| anyhow!(error.to_string()))?;
        self.inner.publish(ApplicationEvent::GoalUpdated {
            operation: "resume",
            state,
        });
        Ok(outcome)
    }

    pub fn goal_complete(&self) -> Result<Goal> {
        self.goal_mutation("complete", |runtime| runtime.complete())
    }

    /// Appends a role-model pin shown in the goal turn's system context.
    pub fn goal_pin(&self, text: impl Into<String>) -> Result<Goal> {
        self.goal_mutation("pin", |runtime| runtime.pin(text))
    }

    /// Removes the role-model pin at `index` (0-based).
    pub fn goal_unpin(&self, index: usize) -> Result<Goal> {
        self.goal_mutation("unpin", |runtime| runtime.unpin(index))
    }

    pub fn goal_drop(&self) -> Result<Goal> {
        self.goal_mutation("drop", |runtime| runtime.drop())
    }

    pub fn goal_update_usage(&self, delta: GoalUsageDelta) -> Result<Goal> {
        let goal = self
            .runtime()
            .session
            .goal_runtime()
            .update_usage(delta)
            .map_err(|error| anyhow!(error.to_string()))?;
        self.inner.publish(ApplicationEvent::GoalUsageCharged {
            delta,
            state: self.goal_state(),
        });
        Ok(goal)
    }

    #[must_use]
    pub fn goal_continuation_decision(&self) -> GoalContinuationDecision {
        let decision = self
            .runtime()
            .session
            .goal_runtime()
            .continuation_decision();
        self.inner.publish(ApplicationEvent::GoalContinuation {
            decision: decision.clone(),
        });
        decision
    }

    fn goal_mutation<F>(&self, operation: &'static str, mutation: F) -> Result<Goal>
    where
        F: FnOnce(&crate::GoalRuntime) -> std::result::Result<Goal, GoalError>,
    {
        let goal = mutation(&self.runtime().session.goal_runtime())
            .map_err(|error| anyhow!(error.to_string()))?;
        self.inner.publish(ApplicationEvent::GoalUpdated {
            operation,
            state: self.goal_state(),
        });
        Ok(goal)
    }

    /// Reserves the single activation slot for the given committed state
    /// snapshot and spawns the continuation turn.
    ///
    /// The snapshot MUST be the result of the create/resume transition this
    /// reservation belongs to, delivered through
    /// [`crate::GoalRuntime::create_and_reserve`] /
    /// [`crate::GoalRuntime::resume_and_reserve`] inside the same lock hold
    /// as the commit, so the reservation is linearized with every lifecycle
    /// transition: pause/complete/drop/budget exhaustion/fork can only
    /// invalidate this reservation after its commit, and a newer create or
    /// resume can only supersede it after its own commit — the ordering can
    /// never invert. Repeating the reservation for the same goal revision
    /// returns [`GoalActivationOutcome::AlreadyActive`].
    fn reserve_goal_work(
        &self,
        active: &Arc<runtime::ApplicationRuntime>,
        state: &GoalState,
    ) -> GoalActivationOutcome {
        let goal = state
            .current
            .as_ref()
            .filter(|goal| goal.lifecycle == crate::GoalLifecycle::Active)
            .expect("create/resume commits always yield an active goal");
        let key = GoalWorkKey {
            goal_id: goal.id.clone(),
            revision: state.revision,
            lifecycle_revision: state.lifecycle_revision,
        };
        let turn_guard = active.turn_gate.clone().try_lock_owned().ok();
        let outcome = if turn_guard.is_some() {
            GoalActivationOutcome::Started
        } else {
            GoalActivationOutcome::Queued
        };
        {
            let mut activation = active.goal_work_activation.lock();
            if activation.as_ref() == Some(&key) {
                return GoalActivationOutcome::AlreadyActive;
            }
            *activation = Some(key.clone());
        }
        active.goal_work_pending.fetch_add(1, Ordering::AcqRel);
        let inner = Arc::downgrade(&self.inner);
        tokio::spawn(run_goal_work(inner, active.clone(), key, turn_guard));
        outcome
    }

    pub fn prepare_resumed_goal(&self, forked: bool) -> Result<()> {
        let active = self.runtime();
        let goal_runtime = active.session.goal_runtime();
        let Some(source) = goal_runtime.get().current else {
            return Ok(());
        };
        if forked {
            goal_runtime
                .fork_clone(&source)
                .map_err(|error| anyhow!(error.to_string()))?;
            self.inner.publish(ApplicationEvent::GoalUpdated {
                operation: "fork_clone",
                state: goal_runtime.get(),
            });
        } else {
            self.inner.pause_goal_after_resume()?;
        }
        active.charged_goal_jobs.lock().clear();
        Ok(())
    }

    #[doc(hidden)]
    pub fn set_loop_persist_failure_for_test(&self, fail: bool) -> Result<()> {
        Ok(self.loop_handle()?.set_persist_failure_for_test(fail)?)
    }

    #[doc(hidden)]
    pub fn arm_cutover_before_spawn_gate_for_test(&self) -> ApplicationCutoverTestGate {
        let gate = ApplicationCutoverTestGate::default();
        *self.inner.cutover_before_spawn_gate.lock() = Some(gate.clone());
        gate
    }

    #[doc(hidden)]
    pub fn arm_prompt_before_turn_start_gate_for_test(&self) -> ApplicationCutoverTestGate {
        let gate = ApplicationCutoverTestGate::default();
        *self.inner.prompt_before_turn_start_gate.lock() = Some(gate.clone());
        gate
    }

    #[doc(hidden)]
    pub fn arm_prompt_after_settle_gate_for_test(&self) -> ApplicationCutoverTestGate {
        let gate = ApplicationCutoverTestGate::default();
        *self.inner.prompt_after_settle_gate.lock() = Some(gate.clone());
        gate
    }


    #[cfg(test)]
    #[doc(hidden)]
    pub fn todo_transition_clear_for_test(&self) -> bool {
        let active = self.runtime();
        !active.todo_transition_active.load(Ordering::Acquire)
            && !active.todo_continuation_suppressed.load(Ordering::Acquire)
    }

    #[cfg(test)]
    #[doc(hidden)]
    pub fn todo_coordinator_state_for_test(&self) -> TodoDagCoordinatorState {
        self.runtime().todo_dag.lock().snapshot_state()
    }

    #[cfg(test)]
    #[doc(hidden)]
    pub fn todo_coordinator_matches_for_test(&self, expected: &TodoDagCoordinatorState) -> bool {
        self.runtime().todo_dag.lock().state_eq_for_test(expected)
    }

    #[cfg(test)]
    #[doc(hidden)]
    pub fn seed_todo_coordinator_nontrivial_for_test(&self) {
        self.runtime()
            .todo_dag
            .lock()
            .seed_nontrivial_state_for_test();
    }

    #[cfg(test)]
    #[doc(hidden)]
    pub fn fail_next_todo_execution_persist_for_test(&self) {
        self.session().fail_next_todo_execution_persist_for_test();
    }

    #[cfg(test)]
    #[doc(hidden)]
    pub fn persist_todo_execution_for_test(
        &self,
        status: TodoDagExecutionStatus,
        failure_reason: Option<TodoDagFailureReason>,
    ) -> Result<()> {
        self.session().persist_todo_execution(status, failure_reason)
    }


    #[cfg(test)]
    #[doc(hidden)]
    pub fn todo_coordinator_generation_for_test(&self) -> u64 {
        self.runtime().todo_dag.lock().generation_for_test()
    }

    #[cfg(test)]
    #[doc(hidden)]
    pub fn todo_dag_execution_for_test(
        &self,
    ) -> (TodoDagExecutionStatus, Option<TodoDagFailureReason>) {
        let active = self.runtime();
        let coordinator = active.todo_dag.lock();
        (coordinator.status, coordinator.failure_reason.clone())
    }


    #[cfg(test)]
    #[doc(hidden)]
    pub fn recovered_todo_job_owned_for_test(&self, job_id: &str) -> bool {
        self.runtime().todo_dag.lock().owns_job_for_test(job_id)
    }

    #[doc(hidden)]
    pub fn arm_cutover_after_loop_commit_gate_for_test(&self) -> ApplicationCutoverTestGate {
        let gate = ApplicationCutoverTestGate::default();
        *self.inner.cutover_after_loop_commit_gate.lock() = Some(gate.clone());
        gate
    }
    #[doc(hidden)]
    pub fn arm_cutover_after_resident_take_gate_for_test(&self) -> ApplicationCutoverTestGate {
        let gate = ApplicationCutoverTestGate::default();
        *self.inner.cutover_after_resident_take_gate.lock() = Some(gate.clone());
        gate
    }

    #[doc(hidden)]
    pub fn drop_resident_orchestrations_for_test(&self) {
        self.inner.resident_orchestration.lock().clear();
    }

    pub async fn loop_create(&self, request: crate::LoopCreateRequest) -> Result<crate::LoopTask> {
        Ok(self.loop_handle()?.create(request).await?)
    }

    pub async fn loop_update(&self, request: crate::LoopUpdateRequest) -> Result<crate::LoopTask> {
        Ok(self.loop_handle()?.update(request).await?)
    }

    pub async fn loop_list(&self) -> Result<Vec<crate::LoopTask>> {
        Ok(self.loop_handle()?.list().await?)
    }

    pub async fn loop_delete(&self, task_id: &str) -> Result<bool> {
        Ok(self.loop_handle()?.delete(task_id).await?)
    }

    pub async fn loop_cancel(&self, task_id: &str) -> Result<bool> {
        Ok(self.loop_handle()?.cancel(task_id).await?)
    }

    fn loop_handle(&self) -> Result<crate::LoopSchedulerHandle> {
        self.inner
            .loop_scheduler
            .lock()
            .as_ref()
            .map(|runtime| runtime.handle.clone())
            .ok_or_else(|| anyhow!("loop scheduler is unavailable"))
    }

    #[must_use]
    pub fn process_manager(&self) -> ProcessManager {
        self.runtime().process_manager()
    }

    pub async fn process_spawn(&self, spec: ProcessSpawnSpec) -> Result<ProcessInfo> {
        let active = self.runtime();
        active
            .process_manager
            .spawn(active.process_owner_id.clone(), spec)
            .await
    }

    #[must_use]
    pub fn process_list(&self) -> Vec<ProcessInfo> {
        let active = self.runtime();
        active.process_manager.list(&active.process_owner_id)
    }

    pub fn process_describe(&self, id: &ProcessId) -> Result<ProcessInfo> {
        let active = self.runtime();
        active
            .process_manager
            .describe(&active.process_owner_id, id)
    }

    pub async fn process_logs(
        &self,
        id: &ProcessId,
        cursor: u64,
        max_bytes: Option<usize>,
        follow: bool,
        timeout: Option<Duration>,
    ) -> Result<ProcessLogs> {
        let active = self.runtime();
        active
            .process_manager
            .logs(
                &active.process_owner_id,
                id,
                cursor,
                max_bytes,
                follow,
                timeout,
            )
            .await
    }

    pub async fn process_write(
        &self,
        id: &ProcessId,
        bytes: Vec<u8>,
        close_stdin: bool,
    ) -> Result<()> {
        let active = self.runtime();
        active
            .process_manager
            .write(&active.process_owner_id, id, bytes, close_stdin)
            .await
    }

    pub async fn process_send_keys(&self, id: &ProcessId, keys: &[ProcessKey]) -> Result<()> {
        let active = self.runtime();
        active
            .process_manager
            .send_keys(&active.process_owner_id, id, keys)
            .await
    }

    pub fn process_resize(&self, id: &ProcessId, size: ProcessTerminalSize) -> Result<()> {
        let active = self.runtime();
        active
            .process_manager
            .resize(&active.process_owner_id, id, size)
    }

    pub fn process_signal(&self, id: &ProcessId, signal: ProcessSignal) -> Result<()> {
        let active = self.runtime();
        active
            .process_manager
            .signal(&active.process_owner_id, id, signal)
    }

    pub async fn process_stop(
        &self,
        id: &ProcessId,
        grace: Option<Duration>,
    ) -> Result<ProcessInfo> {
        let active = self.runtime();
        active
            .process_manager
            .stop(&active.process_owner_id, id, grace)
            .await
    }

    pub async fn process_wait(
        &self,
        id: &ProcessId,
        timeout: Option<Duration>,
    ) -> Result<ProcessInfo> {
        let active = self.runtime();
        active
            .process_manager
            .wait(&active.process_owner_id, id, timeout)
            .await
    }

    #[must_use]
    pub fn session_header(&self) -> Option<SessionStartedEvent> {
        self.runtime()
            .session
            .session_header()
            .map(|header| SessionStartedEvent {
                record_type: header.record_type,
                version: header.version,
                id: header.id,
                timestamp: header.timestamp,
                cwd: header.cwd.to_string_lossy().into_owned(),
            })
    }

    #[must_use]
    pub fn messages(&self) -> Vec<Message> {
        self.runtime().session.history()
    }

    #[must_use]
    pub fn last_assistant_text(&self) -> Option<String> {
        let text = self.runtime().session.last_assistant_text();
        (!text.is_empty()).then_some(text)
    }

    async fn snapshot_for_runtime(
        active: Arc<runtime::ApplicationRuntime>,
    ) -> ApplicationProtocolSnapshot {
        let session = &active.session;
        let (messages, event_barrier) = session.protocol_history();
        let (session_id, session_file) =
            session.recorder_info().map_or((None, None), |(id, path)| {
                (Some(id), Some(path.to_string_lossy().into_owned()))
            });
        let (todo_execution_status, todo_failure_reason) = {
            let coordinator = active.todo_dag.lock();
            (coordinator.status, coordinator.failure_reason.clone())
        };
        let (pending_message_count, queued_steering) = Self::live_queue_projection(session).await;
        let state = ApplicationState {
            model: session.model(),
            thinking_level: session.thinking_level(),
            is_streaming: Self::is_streaming_for_runtime(&active),
            is_compacting: session.is_compacting(),
            steering_mode: session.steering_mode().await,
            follow_up_mode: session.follow_up_mode().await,
            session_file,
            session_id,
            session_name: session.session_name(),
            auto_compaction_enabled: session.auto_compaction_enabled(),
            message_count: messages.len(),
            pending_message_count,
            skipped_idless: session.loader_drop_counts().0,
            undecodable_messages: session.loader_drop_counts().1,
            queued_steering,
            last_selection: session.last_selection(),
            todo_phases: session.todo_state().phases,
            todo_execution_status,
            todo_failure_reason,
            goal: session.goal_runtime().get(),
        };
        let lifecycle = active.turn_lifecycle_snapshot();
        let active_turn_model = lifecycle.active_turn_model.clone();
        let cwd = session.cwd().to_path_buf();
        let runtime_settings = active.runtime_settings.lock().as_ref().clone();
        ApplicationProtocolSnapshot {
            epoch: active.epoch(),
            event_barrier: event_barrier.saturating_sub(active.agent_event_barrier_base()),
            state,
            active_turn_model,
            lifecycle,
            messages,
            cwd,
            runtime_settings,
        }
    }

    #[must_use]
    pub async fn protocol_snapshot(&self) -> ApplicationProtocolSnapshot {
        Self::snapshot_for_runtime(self.runtime()).await
    }
    async fn state_for_runtime(active: Arc<runtime::ApplicationRuntime>) -> ApplicationState {
        let session = &active.session;
        let (session_id, session_file) =
            session.recorder_info().map_or((None, None), |(id, path)| {
                (Some(id), Some(path.to_string_lossy().into_owned()))
            });
        let (todo_execution_status, todo_failure_reason) = {
            let coordinator = active.todo_dag.lock();
            (coordinator.status, coordinator.failure_reason.clone())
        };
        let (pending_message_count, queued_steering) = Self::live_queue_projection(session).await;
        ApplicationState {
            model: session.model(),
            thinking_level: session.thinking_level(),
            is_streaming: Self::is_streaming_for_runtime(&active),
            is_compacting: session.is_compacting(),
            steering_mode: session.steering_mode().await,
            follow_up_mode: session.follow_up_mode().await,
            session_file,
            session_id,
            session_name: session.session_name(),
            auto_compaction_enabled: session.auto_compaction_enabled(),
            message_count: session.history().len(),
            pending_message_count,
            skipped_idless: session.loader_drop_counts().0,
            undecodable_messages: session.loader_drop_counts().1,
            queued_steering,
            last_selection: session.last_selection(),
            todo_phases: session.todo_state().phases,
            todo_execution_status,
            todo_failure_reason,
            goal: session.goal_runtime().get(),
        }
    }

    /// The single live source for the pending steering/follow-up projection:
    /// explicit `get_state`, the bootstrap (sessionId-less) `get_state`, and
    /// the Web subscription snapshot all recompute through here so a queued
    /// message can never disagree between surfaces (reconnect must not wipe
    /// acknowledged tray rows).
    async fn live_queue_projection(session: &Session) -> (usize, Vec<String>) {
        let (queued_steering_messages, queued_follow_up) = session.queued_messages().await;
        (
            queued_steering_messages.len() + queued_follow_up.len(),
            queued_user_previews(&queued_steering_messages),
        )
    }

    #[must_use]
    pub async fn state(&self) -> ApplicationState {
        Self::state_for_runtime(self.runtime()).await
    }

    fn is_streaming_for_runtime(active: &runtime::ApplicationRuntime) -> bool {
        active.loop_turn_active.load(Ordering::Acquire)
            || active.goal_work_pending.load(Ordering::Acquire) != 0
            || active.turn_lifecycle_snapshot().active_turn_id.is_some()
            || active
                .active_run
                .lock()
                .as_ref()
                .is_some_and(|run| !run.is_finished())
            || active.todo_cycle_pending.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn is_streaming(&self) -> bool {
        Self::is_streaming_for_runtime(&self.runtime())
    }

    #[must_use]
    pub fn is_cleaned(&self) -> bool {
        self.inner.cleaned.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn active_turn_model(&self) -> Option<Model> {
        self.runtime().turn_lifecycle_snapshot().active_turn_model
    }

    #[must_use]
    pub fn active_turn_id(&self) -> Option<String> {
        self.runtime().turn_lifecycle_snapshot().active_turn_id
    }

    #[must_use]
    pub fn turn_lifecycle_snapshot(&self) -> TurnLifecycleSnapshot {
        self.runtime().turn_lifecycle_snapshot()
    }

    pub async fn prompt(
        &self,
        message: String,
        images: Vec<ContentBlock>,
        streaming_behavior: Option<StreamingBehavior>,
    ) -> Result<Option<TurnAccepted>> {
        self.prompt_inner(message, images, streaming_behavior, true)
            .await
    }

    pub(crate) async fn prompt_without_auto_todo(
        &self,
        message: String,
        images: Vec<ContentBlock>,
        streaming_behavior: Option<StreamingBehavior>,
    ) -> Result<Option<TurnAccepted>> {
        self.prompt_inner(message, images, streaming_behavior, false)
            .await
    }

    async fn prompt_inner(
        &self,
        message: String,
        images: Vec<ContentBlock>,
        streaming_behavior: Option<StreamingBehavior>,
        allow_auto_todo: bool,
    ) -> Result<Option<TurnAccepted>> {
        let (message, images) = if let Some(runtime) = self.extension_runtime() {
            match runtime
                .reduce_input(
                    message,
                    images,
                    "interactive",
                    streaming_behavior.map(streaming_behavior_name),
                )
                .await?
            {
                crate::ExtensionInputReduction::Continue { text, images } => (text, images),
                crate::ExtensionInputReduction::Handled => return Ok(None),
            }
        } else {
            (message, images)
        };
        let active = self.runtime();
        let session = active.session.clone();
        let inner = self.inner.clone();
        let orchestration = active.orchestration_runtime();
        let auto_mode = session.selector_settings().auto_mode;
        let workflow_owned = orchestration
            .as_ref()
            .is_some_and(|runtime| runtime.workflow_scope().is_some());
        if allow_auto_todo
            && !workflow_owned
            && auto_mode.is_auto()
            && crate::selector::classify_prompt(&message) == crate::PromptMode::CodeTask
            // Todo-management commands ("add task: fix the parser crash",
            // "cancel the todo") target the canonical Todo DAG and must stay
            // with Main's typed `todo` tool / child lifecycle controls, never
            // be swallowed as a routed code task.
            && !crate::selector::is_todo_management_prompt(&message)
        {
            if let Some(hint) = crate::selector::mode_hint(crate::PromptMode::CodeTask) {
                inner.publish(ApplicationEvent::ModeDetected {
                    mode: crate::PromptMode::CodeTask,
                    hint: hint.to_owned(),
                });
            }
            let receipt = self
                .todo_child_service()?
                .route_code_task(&session, message, images)
                .await
                .context("routing the code task to the durable Todo child")?;
            inner.publish(ApplicationEvent::TodoChildRouted { receipt });
            return Ok(None);
        }
        let operation = self.inner.operation_gate.clone().lock_owned().await;
        let active = self.runtime();
        let admission = active.turn_admission.clone().lock_owned().await;
        if active.todo_continuation_suppressed.load(Ordering::Acquire) {
            active.todo_resume_requested.store(true, Ordering::Release);
        }
        if active.loop_turn_active.load(Ordering::Acquire) {
            drop(admission);
            return match streaming_behavior {
                Some(StreamingBehavior::Steer) => active
                    .session
                    .steer(user_message(message, images))
                    .await
                    .map(|()| None),
                Some(StreamingBehavior::FollowUp) => active
                    .session
                    .follow_up(user_message(message, images))
                    .await
                    .map(|()| None),
                None => Err(SessionAlreadyProcessing.into()),
            };
        }
        let Ok(turn_guard) = active.turn_gate.clone().try_lock_owned() else {
            drop(admission);
            return match streaming_behavior {
                Some(StreamingBehavior::Steer) => active
                    .session
                    .steer(user_message(message, images))
                    .await
                    .map(|()| None),
                Some(StreamingBehavior::FollowUp) => active
                    .session
                    .follow_up(user_message(message, images))
                    .await
                    .map(|()| None),
                None => Err(SessionAlreadyProcessing.into()),
            };
        };
        let session = active.session.clone();
        let inner = self.inner.clone();
        let selection = session.select_for_request(&message).await;
        // Deterministic auto-mode classifier (`selector.autoMode`):
        // - off: nothing.
        // - suggest: publish a status hint when a code task / goal is detected.
        // - auto: route a code task to the selected durable Todo child. This
        //   path returns before parent turn admission, so the parent model,
        //   Todo state, and ordinary orchestration jobs remain untouched.
        // Workflow-owned runs (supervisor prompts) never classify: the
        // workflow supervisor owns the canonical Todo DAG and user-facing
        // hints would be noise for internal prompts.
        if allow_auto_todo && !workflow_owned && auto_mode.is_enabled() {
            let mode = crate::selector::classify_prompt(&message);
            if let Some(hint) = crate::selector::mode_hint(mode) {
                inner.publish(ApplicationEvent::ModeDetected {
                    mode,
                    hint: hint.to_owned(),
                });
            }
        }
        inner.publish(ApplicationEvent::Selection(selection));
        if session.todo_reminder_pending() {
            let outcome = inner.todo_dag_snapshot();
            inner.publish(ApplicationEvent::TodoReminder {
                phases: session.todo_state().phases,
                execution_status: outcome.status,
                failure_reason: outcome.failure_reason,
            });
        }
        let prompt_gate = self.inner.prompt_before_turn_start_gate.lock().take();
        if let Some(gate) = prompt_gate {
            gate.enter().await;
        }
        let turn_id = uuid::Uuid::now_v7().to_string();
        let turn_model = session.model();
        if !inner.publish_turn_started(&active, &turn_id, turn_model.as_ref()) {
            return Err(anyhow!(
                "session changed before prompt admission completed; retry the prompt"
            ));
        }
        let goal_was_active = session
            .goal_runtime()
            .get()
            .current
            .is_some_and(|goal| goal.lifecycle == crate::GoalLifecycle::Active);
        inner.publish(ApplicationEvent::GoalContinuation {
            decision: session.goal_runtime().continuation_decision(),
        });
        let accepted = TurnAccepted {
            turn_id: turn_id.clone(),
            model: turn_model,
        };
        let mut lifecycle = OuterTurnLifecycle::new(inner.clone(), active.clone(), turn_id);
        let task_active = active.clone();
        let mention = orchestration.as_ref().and_then(|runtime| {
            match crate::selector::delegation_agent_mention(&message, runtime.catalog().agents()) {
                crate::selector::ExactAgentMention::Unique(name) => {
                    Some(Session::agent_mention_message(&name))
                }
                crate::selector::ExactAgentMention::None
                | crate::selector::ExactAgentMention::Ambiguous(_) => None,
            }
        });
        let handle = tokio::spawn(async move {
            let started = Instant::now();
            let run = match mention {
                Some(mention) => {
                    session
                        .run_messages(vec![mention, user_message(message, images)])
                        .await
                }
                None => session.run(&message, images).await,
            };
            let (status, message) = match run {
                Ok(result) => {
                    inner.finish_goal_turn_for_runtime(
                        &task_active,
                        result.usage,
                        started,
                        goal_was_active,
                    );
                    match result.stop_reason {
                        pi_ai::StopReason::Aborted => {
                            (TurnTerminalStatus::Aborted, result.error_message)
                        }
                        pi_ai::StopReason::Error => {
                            (TurnTerminalStatus::Failed, result.error_message)
                        }
                        _ => (TurnTerminalStatus::Succeeded, None),
                    }
                }
                Err(error) => {
                    let message = error.to_string();
                    inner.publish(ApplicationEvent::RunFailed {
                        message: message.clone(),
                    });
                    inner.finish_goal_turn_for_runtime(
                        &task_active,
                        pi_ai::Usage::default(),
                        started,
                        goal_was_active,
                    );
                    let status = session
                        .history()
                        .last()
                        .and_then(pi_ai::Message::as_assistant)
                        .map(|assistant| assistant.stop_reason)
                        .filter(|reason| *reason == pi_ai::StopReason::Aborted)
                        .map_or(TurnTerminalStatus::Failed, |_| TurnTerminalStatus::Aborted);
                    (status, Some(message))
                }
            };
            let admission = task_active.turn_admission.clone().lock_owned().await;
            let terminal = lifecycle
                .commit(status, message)
                .expect("the admitted turn commits exactly one terminal event");
            drop(task_active.active_run.lock().take());
            drop(turn_guard);
            inner.emit_turn_terminal(terminal);
            // Hold after the earliest terminal event while admission remains
            // serialized, so a successor cannot start before AgentSettled.
            let settle_gate = inner.prompt_after_settle_gate.lock().take();
            if let Some(gate) = settle_gate {
                gate.enter().await;
            }
            inner.hold_terminal_until_queue_drained(&task_active).await;
            inner.finish_parent_turn_for_runtime(&task_active);
            drop(admission);
        });
        *active.active_run.lock() = Some(handle);
        drop(admission);
        drop(operation);
        Ok(Some(accepted))
    }

    pub async fn steer(&self, message: String, images: Vec<ContentBlock>) -> Result<()> {
        let active = self.runtime();
        if active.todo_continuation_suppressed.load(Ordering::Acquire) {
            active.todo_resume_requested.store(true, Ordering::Release);
        }
        active.session.steer(user_message(message, images)).await
    }

    pub async fn follow_up(&self, message: String, images: Vec<ContentBlock>) -> Result<()> {
        let active = self.runtime();
        if active.todo_continuation_suppressed.load(Ordering::Acquire) {
            active.todo_resume_requested.store(true, Ordering::Release);
        }
        active
            .session
            .follow_up(user_message(message, images))
            .await
    }

    /// Task-scoped interrupt: cancels the active agent turn, its in-flight tool
    /// calls, and the turn's foreground commands — WITHOUT stopping supervised
    /// `/ps` processes.
    ///
    /// This is the single choke point every turn-cancel path funnels through
    /// (TUI Esc/Ctrl-C, loop task cancel, human-mode Ctrl-C, extension Abort).
    /// The boundary follows Codex `Op::Interrupt` semantics ("abort the current
    /// task without killing background terminals"):
    /// - Killed: the active turn's foreground commands — the foreground bash
    ///   abort arm in `run_bash_core` kills the child process group
    ///   (`tools.rs`), and any user-initiated foreground bash is aborted via
    ///   `abort_bash`. Retry/compaction work and the active loop iteration are
    ///   cancelled as well.
    /// - Kept alive: every process supervised by the per-session
    ///   [`crate::ProcessManager`] (`process/manager.rs`), regardless of which
    ///   turn started it — including `bash background=true` and `/ps`
    ///   processes. In-flight `process` tool `wait`/`logs`/`stop` calls observe
    ///   the abort signal and return "Operation aborted" (`process/tool.rs`)
    ///   without signalling the supervised child.
    /// - The manager is never dropped or `shutdown_owner`'d here: it is owned
    ///   by [`ApplicationRuntime`] (and the `Session`), which task interrupts
    ///   never replace. Only explicit `/ps` stop/signal, session switches
    ///   (`new_session`/`switch_session`/fork/goal), or application teardown
    ///   (`cleanup`/`shutdown`) stop supervised processes.
    pub async fn abort(&self) {
        let active = self.runtime();
        active.todo_resume_requested.store(false, Ordering::Release);
        active
            .todo_continuation_suppressed
            .store(true, Ordering::Release);
        if let Some(runtime) = active.orchestration_runtime() {
            runtime.cancel_active();
            self.inner.finish_todo_cycle_if_idle(Some(&runtime), false);
        }
        // Ownership boundary: cancel the loop iteration token BEFORE the
        // session abort so the scheduler observes `runner_cancel.is_cancelled()`
        // and classifies the turn as `Cancelled` (silent; the loop remains
        // scheduled). If the session abort completed first, the runner could
        // settle with a provider "Request was aborted" error before the loop
        // token fired, and the scheduler would misclassify a user Esc as
        // `LoopEvent::Failed`. Cancelling the token first is race-safe: the
        // runner's `tokio::select!` either observes the token (returns via the
        // cancel arm) or has already completed, but `runner_cancel.is_cancelled()`
        // is now true in both cases → `RunResult::Cancelled`. Real provider/
        // runtime failures are unaffected: they complete without the token
        // cancelled and still surface as `LoopEvent::Failed`.
        if let Ok(handle) = self.loop_handle() {
            let _ = handle.cancel_active_iteration().await;
        }
        active.session.abort().await;
    }

    pub fn abort_compaction(&self) {
        self.runtime().session.abort_compaction();
    }

    /// Arm (or disarm) the interactive `ask` round trip. Only interactive
    /// frontends (the TUI) arm it; every other mode keeps `ask` rejecting.
    pub fn set_ask_interactive(&self, interactive: bool) {
        self.runtime().session.set_ask_interactive(interactive);
    }

    /// Override the answer-wait bound for pending `ask` questions (default 60s).
    pub fn set_ask_timeout(&self, timeout: std::time::Duration) {
        self.runtime().session.set_ask_timeout(timeout);
    }

    /// The currently pending `ask` as `(id, prompt)`, if any.
    #[must_use]
    pub fn pending_ask(&self) -> Option<(String, String)> {
        self.runtime().session.pending_ask()
    }

    /// Deliver the user's answer to the pending `ask` question.
    pub fn answer_ask(&self, id: &str, answer: String) -> Result<()> {
        self.runtime().session.answer_ask(id, answer)
    }

    /// Cancel the pending `ask` question (Esc / shutdown).
    pub fn cancel_ask(&self, id: &str) -> Result<()> {
        self.runtime().session.cancel_ask(id)
    }

    /// Cancel whatever question is pending, regardless of id (TUI shutdown).
    /// Returns whether a pending ask was cancelled.
    pub fn cancel_pending_ask(&self) -> bool {
        self.runtime().session.cancel_pending_ask()
    }

    pub fn set_auto_retry_enabled(&self, enabled: bool) {
        self.runtime().session.set_auto_retry_enabled(enabled);
    }

    pub fn abort_retry(&self) {
        self.runtime().session.abort_retry();
    }

    pub async fn execute_bash(
        &self,
        command: String,
        exclude_from_context: bool,
    ) -> Result<crate::BashResult> {
        self.execute_bash_with_id(command, exclude_from_context, None)
            .await
    }

    pub async fn execute_bash_with_id(
        &self,
        command: String,
        exclude_from_context: bool,
        id: Option<String>,
    ) -> Result<crate::BashResult> {
        let active = self.runtime();
        if let Some(runtime) = active
            .publication()
            .capture()
            .extensions()
            .runtime()
            .cloned()
        {
            let reduction = runtime
                .reduce_user_bash(serde_json::json!({
                    "command": command,
                    "excludeFromContext": exclude_from_context,
                    "cwd": active.session.cwd(),
                }))
                .await?;
            if let Some(result) = reduction.result {
                active
                    .session
                    .record_bash_result(&command, exclude_from_context, &result)
                    .await?;
                return Ok(result);
            }
        }
        active
            .session
            .execute_bash_with_id(&command, exclude_from_context, id)
            .await
    }

    pub fn abort_bash(&self) {
        self.runtime().session.abort_bash();
    }

    #[must_use]
    pub fn is_bash_running(&self) -> bool {
        self.runtime().session.is_bash_running()
    }

    #[must_use]
    pub fn session_stats(&self) -> crate::SessionStats {
        self.runtime().session.session_stats()
    }

    pub async fn queued_messages(&self) -> (Vec<Message>, Vec<Message>) {
        self.runtime().session.queued_messages().await
    }

    pub async fn drain_queued_messages(&self) -> (Vec<Message>, Vec<Message>) {
        self.runtime().session.drain_queued_messages().await
    }

    pub async fn drain_steering_messages(&self) -> Vec<Message> {
        self.runtime().session.drain_steering_messages().await
    }

    pub async fn remove_steering_at(&self, index: usize) -> Option<Message> {
        self.runtime().session.remove_steering_at(index).await
    }

    pub async fn send_queued_steering_now(&self) -> Result<crate::SteeringSendNow> {
        let session = self.runtime().session.clone();
        let drained = session.drain_steering_messages().await;
        let sent = drained.len();
        if sent == 0 {
            return Ok(crate::SteeringSendNow {
                sent: 0,
                combined: false,
            });
        }
        // The drained rows are re-delivered as a COMBINED fresh prompt whose
        // timestamp can never value-match their journal markers, and the
        // passthrough/re-queue paths journal fresh markers. Retire the
        // originals first or every send-now leaves a stale marker that
        // requeues the delivered message on the next resume.
        session
            .retire_pending_steering_markers_filtered("send-now", Some(false))
            .await;
        // Only User messages combine; non-User rows (e.g. orchestration
        // mailbox custom messages) are silently DISCARDED by
        // `combine_queued_user_messages`, so return them to the queue first —
        // they were acknowledged and must survive send-now.
        let (users, passthrough): (Vec<_>, Vec<_>) = drained
            .into_iter()
            .partition(|message| matches!(message, Message::User(_)));
        for message in &passthrough {
            if let Err(error) = self.runtime().session.steer(message.clone()).await {
                return Err(anyhow!(
                    "re-queueing non-user queued steering: {error:#}"
                ));
            }
        }
        let restore = |combined: Message| {
            let session = self.runtime().session.clone();
            async move { session.steer(combined).await }
        };
        let Some(combined) = pi_agent::combine_queued_user_messages(users.clone()) else {
            // No user content: restore the whole user batch in FIFO order
            // instead of destroying acknowledged messages.
            for message in &users {
                restore(message.clone()).await.map_err(|restore| {
                    anyhow!("restore steering batch: {restore:#}")
                })?;
            }
            bail!("queued steering contained no user content");
        };
        let (text, images) = pi_agent::split_queued_user_message(&combined);
        let result = crate::SteeringSendNow {
            sent: users.len(),
            combined: users.len() > 1,
        };
        // Abort does not wait for run_slot; prompt() rejects while the turn
        // is held. Wait until idle, then prompt — never re-steer.
        if self.is_streaming() {
            self.abort().await;
            self.runtime().session.wait_for_idle().await;
            let handle = self.runtime().active_run.lock().take();
            if let Some(handle) = handle {
                let _ = handle.await;
            }
        }
        match self.prompt(text, images, None).await {
            Ok(_) => Ok(result),
            Err(error) => {
                if let Err(restore) = restore(combined).await {
                    return Err(anyhow!("{error:#}; restore steering: {restore:#}"));
                }
                Err(error)
            }
        }
    }

    pub async fn wait_for_idle(&self) {
        let active = self.runtime();
        loop {
            active.session.wait_for_idle().await;
            let handle = active.active_run.lock().take();
            if let Some(handle) = handle {
                let _ = handle.await;
            }
            let changed = active.goal_work_changed.notified();
            if active.goal_work_pending.load(Ordering::Acquire) == 0 {
                return;
            }
            changed.await;
        }
    }

    async fn begin_todo_session_transition(&self) -> Result<TodoSessionTransitionGuard> {
        let active = self.runtime();
        let (previous_status, previous_failure) = active.session.todo_execution();
        let previous_coordinator = active.todo_dag.lock().snapshot_state();
        let previous_suppressed = active
            .todo_continuation_suppressed
            .load(Ordering::Acquire);
        active
            .todo_continuation_suppressed
            .store(true, Ordering::Release);
        active.todo_resume_requested.store(false, Ordering::Release);
        active.todo_cycle_pending.store(false, Ordering::Release);
        let gate = active.todo_dag.lock().reconcile_gate.clone();
        {
            let _transaction = gate.lock();
            active.todo_transition_active.store(true, Ordering::Release);
            if let Some(runtime) = active.orchestration_runtime().as_ref()
                && let Err(error) = self.inner.begin_todo_dag_transition_locked(runtime)
            {
                active
                    .todo_transition_active
                    .store(false, Ordering::Release);
                active
                    .todo_continuation_suppressed
                    .store(previous_suppressed, Ordering::Release);
                active
                    .session
                    .restore_todo_execution(previous_status, previous_failure.clone());
                active.todo_dag.lock().restore_state(previous_coordinator.clone());
                return Err(error).context("beginning Todo DAG session transition");
            }
        }
        Ok(TodoSessionTransitionGuard {
            application: self.clone(),
            previous_status,
            previous_failure,
            previous_coordinator,
            previous_suppressed,
            armed: true,
        })
    }

    fn abort_todo_session_transition(
        &self,
        previous_status: TodoDagExecutionStatus,
        previous_failure: Option<TodoDagFailureReason>,
        previous_coordinator: TodoDagCoordinatorState,
        previous_suppressed: bool,
    ) {
        let active = self.runtime();
        let gate = active.todo_dag.lock().reconcile_gate.clone();
        let _transaction = gate.lock();
        active
            .session
            .restore_todo_execution(previous_status, previous_failure);
        active.todo_dag.lock().restore_state(previous_coordinator);
        active
            .todo_transition_active
            .store(false, Ordering::Release);
        active
            .todo_continuation_suppressed
            .store(previous_suppressed, Ordering::Release);
    }

    fn restore_incoming_todo_coordinator_locked(
        &self,
        commit_target: bool,
        jobs: &[crate::JobSnapshot],
    ) -> bool {
        let active = self.runtime();
        {
            let mut coordinator = active.todo_dag.lock();
            coordinator.restore_from_session_with_jobs(&active.session, jobs);
            if commit_target
                && let Err(error) = persist_coordinator_status_for_runtime(&active, &coordinator)
            {
                coordinator.restore_from_session_with_jobs(&active.session, jobs);
                drop(coordinator);
                active
                    .todo_transition_active
                    .store(false, Ordering::Release);
                active
                    .todo_continuation_suppressed
                    .store(false, Ordering::Release);
                self.inner.todo_dag_failed(&error);
                return false;
            }
        }
        true
    }

    fn complete_todo_session_transition_locked(&self) {
        let active = self.runtime();
        active
            .todo_transition_active
            .store(false, Ordering::Release);
        active
            .todo_continuation_suppressed
            .store(false, Ordering::Release);
        if let Some(runtime) = active.orchestration_runtime() {
            let result = if self.inner.has_ready_open_todo() {
                active.todo_cycle_pending.store(true, Ordering::Release);
                self.inner.arm_todo_dag_locked(&runtime, false).map(|_| ())
            } else if active.todo_dag.lock().status != TodoDagExecutionStatus::Dormant {
                self.inner.reconcile_todo_dag_locked(&runtime, true).map(|_| ())
            } else {
                Ok(())
            };
            if let Err(error) = result {
                self.inner.todo_dag_failed(&error);
            }
        }
    }

    fn finish_todo_session_transition(&self, commit_target: bool) {
        let active = self.runtime();
        let gate = active.todo_dag.lock().reconcile_gate.clone();
        let _transaction = gate.lock();
        let jobs = active
            .orchestration_runtime()
            .map(|runtime| runtime.jobs(None))
            .unwrap_or_default();
        if !self.restore_incoming_todo_coordinator_locked(commit_target, &jobs) {
            return;
        }
        self.complete_todo_session_transition_locked();
    }


    fn prepare_orchestration_cutover(
        &self,
        active: &runtime::ApplicationRuntime,
        session: &crate::PreparedSessionReplacement,
    ) -> Result<Option<PreparedOrchestrationCutover>, OrchestrationCutover> {
        let (session_id, session_path) = session.recorder_info();
        let identity =
            ApplicationInner::resident_identity(session_id.clone(), session_path.clone())
                .map_err(OrchestrationCutover::Failed)?;
        let runtime = match self.inner.take_resident_orchestration(&identity) {
            Some(resident) => PreparedOrchestrationRuntime::Resident(resident),
            None => {
                let Some(fresh) = self
                    .fresh_same_cwd_orchestration(active)
                    .map_err(OrchestrationCutover::Failed)?
                else {
                    return Ok(None);
                };
                PreparedOrchestrationRuntime::Fresh(fresh)
            }
        };
        let orchestration = runtime.runtime();
        let (parent_directory, parent_relative) =
            session.recorder_file_authority().ok_or_else(|| {
                OrchestrationCutover::Unbound(
                    "Orchestration unbound: prepared session recording authority is unavailable."
                        .to_owned(),
                )
            })?;
        let mut binding = orchestration
            .prepare_parent_identity(session_id, session_path, parent_directory, parent_relative)
            .map_err(|error| {
                if durable_resume_error(&error).is_some() {
                    OrchestrationCutover::Unbound(durable_resume_notice(&error))
                } else {
                    OrchestrationCutover::Failed(error)
                }
            })?;
        orchestration
            .initialize_prepared_parent(&mut binding)
            .map_err(|error| {
                if durable_resume_error(&error).is_some() {
                    OrchestrationCutover::Unbound(durable_resume_notice(&error))
                } else {
                    OrchestrationCutover::Failed(error)
                }
            })?;
        Ok(Some(PreparedOrchestrationCutover { runtime, binding }))
    }

    fn fresh_same_cwd_orchestration(
        &self,
        active: &runtime::ApplicationRuntime,
    ) -> Result<Option<crate::OrchestrationRuntime>> {
        let Some(current) = active.orchestration_runtime() else {
            return Ok(None);
        };
        let fresh = current.fresh_for_parent(&active.session)?;
        fresh.bind_generation_capture(active.publication().capture_fn())?;
        Ok(Some(fresh))
    }

    async fn prepare_same_cwd_cutover(
        &self,
        active: &runtime::ApplicationRuntime,
        session: crate::PreparedSessionReplacement,
    ) -> Result<(PreparedSameCwdCutover, Option<String>)> {
        let target_storage = match session
            .recorder_file_authority()
            .map(|(lease, relative)| {
                crate::loop_scheduler::LoopStorage::from_session_authority(lease, &relative)
            })
            .transpose()
        {
            Ok(target_storage) => target_storage,
            Err(error) => return Err(session.rollback_unpublished(error.into())),
        };
        let target_loops = match crate::prepare_loop_activation(target_storage).await {
            Ok(target_loops) => target_loops,
            Err(error) => return Err(session.rollback_unpublished(error.into())),
        };
        let (orchestration, notice) = match self.prepare_orchestration_cutover(active, &session) {
            Ok(orchestration) => (orchestration, None),
            Err(OrchestrationCutover::Unbound(notice)) => (None, Some(notice)),
            Err(OrchestrationCutover::Failed(error)) => {
                return Err(session.rollback_unpublished(error));
            }
        };
        self.wait_cutover_after_resident_take_gate().await;
        let session = active
            .session
            .prepare_session_replacement_commit(session)
            .await;
        if self.inner.cleaned.load(Ordering::Acquire) {
            drop(orchestration);
            return Err(session.rollback_unpublished(anyhow!(
                "application shut down during session cutover"
            )));
        }
        let handle = match self.loop_handle() {
            Ok(handle) => handle,
            Err(error) => return Err(session.rollback_unpublished(error)),
        };
        let loops = match handle
            .prepare_session_switch(crate::prepare_loop_session_switch(target_loops))
            .await
        {
            Ok(loops) => loops,
            Err(error) => {
                drop(orchestration);
                return Err(session.rollback_unpublished(error.into()));
            }
        };
        Ok((
            PreparedSameCwdCutover {
                session,
                orchestration,
                loop_handle: handle,
                loops,
                notice: notice.clone(),
            },
            notice,
        ))
    }
    fn spawn_orchestration_forwarder(
        &self,
        active: Arc<runtime::ApplicationRuntime>,
        orchestration: &crate::OrchestrationRuntime,
    ) -> JoinHandle<()> {
        let mut events = orchestration.subscribe();
        let event_runtime = orchestration.clone();
        let event_inner = Arc::downgrade(&self.inner);
        let epoch = active.epoch();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        let Some(inner) = event_inner.upgrade() else {
                            break;
                        };
                        if !Arc::ptr_eq(&inner.runtime(), &active) {
                            break;
                        }
                        if matches!(&event, crate::OrchestrationEvent::JobUpdated { job, .. } if !job.status.is_settled())
                        {
                            active.todo_cycle_pending.store(true, Ordering::Release);
                        }
                        let allow_spawn =
                            !active.todo_continuation_suppressed.load(Ordering::Acquire);
                        if let Err(error) =
                            inner.observe_orchestration_event(&event_runtime, &event, allow_spawn)
                        {
                            inner.todo_dag_failed(&error);
                        }
                        inner
                            .steer_main_delivered_message(&active, &event)
                            .await
                            .unwrap_or_else(|error| {
                                inner.report_steer_main_delivery_failure(&event, &error)
                            });
                        let _ = inner
                            .runtime
                            .publish(epoch, ApplicationEvent::Orchestration(event));
                        inner.finish_todo_cycle_if_idle(Some(&event_runtime), false);
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let Some(inner) = event_inner.upgrade() else {
                            break;
                        };
                        if !Arc::ptr_eq(&inner.runtime(), &active) {
                            break;
                        }
                        for event in event_runtime.presentation_events() {
                            let allow_spawn =
                                !active.todo_continuation_suppressed.load(Ordering::Acquire);
                            if let Err(error) = inner.observe_orchestration_event(
                                &event_runtime,
                                &event,
                                allow_spawn,
                            ) {
                                inner.todo_dag_failed(&error);
                            }
                            let _ = inner
                                .runtime
                                .publish(epoch, ApplicationEvent::Orchestration(event));
                            inner.finish_todo_cycle_if_idle(Some(&event_runtime), false);
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    async fn commit_same_cwd_orchestration(
        &self,
        active: Arc<runtime::ApplicationRuntime>,
        target: Option<PreparedOrchestrationCutover>,
        outgoing_identity: Option<ResidentIdentity>,
        transition: TodoSessionTransitionGuard,
    ) -> Option<RecoveredParentRevival> {
        let Some(target) = target else {
            transition.commit();
            return None;
        };
        let PreparedOrchestrationCutover { runtime, binding } = target;
        let target_runtime = runtime.into_runtime();
        self.inner.steered_main_message_ids.lock().clear();
        let outgoing_forwarder = active.orchestration_events.lock().take();
        if let Some(task) = outgoing_forwarder {
            task.abort();
            let _ = task.await;
        }
        let replay_runtime = target_runtime.clone();
        let outgoing_runtime = active.orchestration_runtime.lock().replace(target_runtime);
        // Install recovered snapshots and reconstruct Todo ownership from the
        // installed jobs() snapshot (todoTaskId correlation, attempted set,
        // owned_jobs) while those identities are still Running. Do not arm:
        // attempted_task_ids already makes the recovered task ineligible.
        // Revival and the event forwarder wait until after the cutover join
        // so a stub settle cannot drop owned_jobs before switch_session
        // returns.
        let installed_recovered = replay_runtime.install_prepared_parent(binding);
        {
            let gate = active.todo_dag.lock().reconcile_gate.clone();
            let _transaction = gate.lock();
            let jobs = replay_runtime.jobs(None);
            if self.restore_incoming_todo_coordinator_locked(true, &jobs) {
                for job in jobs.iter().filter(|job| job.status.is_settled()) {
                    if let Err(error) = self
                        .inner
                        .record_terminal_todo_job_for_recovery(&active, job)
                    {
                        self.inner.todo_dag_failed(&error);
                    }
                }
            }
            active
                .todo_transition_active
                .store(false, Ordering::Release);
            active
                .todo_continuation_suppressed
                .store(false, Ordering::Release);
        }
        transition.disarm();
        if let Some(outgoing_runtime) = outgoing_runtime {
            if let Some(outgoing_identity) = outgoing_identity {
                self.inner.retain_resident_orchestration(
                    outgoing_identity,
                    ResidentOrchestration {
                        runtime: outgoing_runtime,
                        publication: active.publication(),
                    },
                );
            }
            // An unrecorded session (`--no-session`) has no identity to resume.
            // Dropping the owner runtime cancels its jobs.
        }
        self.inner
            .replay_main_mailbox(&active, &replay_runtime)
            .await;
        installed_recovered.then_some(RecoveredParentRevival {
            runtime: replay_runtime,
        })
    }

    fn revive_recovered_parent_after_cutover(&self, recovered: Option<RecoveredParentRevival>) {
        let Some(RecoveredParentRevival { runtime }) = recovered else {
            return;
        };
        runtime.revive_prepared_parent();
        let active = self.runtime();
        let target_forwarder = self.spawn_orchestration_forwarder(active.clone(), &runtime);
        *active.orchestration_events.lock() = Some(target_forwarder);
    }

    async fn wait_cutover_before_spawn_gate(&self) {
        let gate = self.inner.cutover_before_spawn_gate.lock().take();
        if let Some(gate) = gate {
            gate.enter().await;
        }
    }
    async fn wait_cutover_after_resident_take_gate(&self) {
        let gate = self.inner.cutover_after_resident_take_gate.lock().take();
        if let Some(gate) = gate {
            gate.enter().await;
        }
    }

    async fn wait_cutover_after_loop_commit_gate(&self) {
        let gate = self.inner.cutover_after_loop_commit_gate.lock().take();
        if let Some(gate) = gate {
            gate.enter().await;
        }
    }

    async fn commit_same_cwd_cutover(
        &self,
        active: Arc<runtime::ApplicationRuntime>,
        prepared: PreparedSameCwdCutover,
        operation: OwnedMutexGuard<()>,
        transition: TodoSessionTransitionGuard,
        commit_events: Vec<ApplicationEvent>,
    ) -> Result<()> {
        let application = self.clone();
        let recovered = tokio::spawn(async move {
            let _operation = operation;
            let PreparedSameCwdCutover {
                session,
                orchestration,
                loop_handle,
                loops,
                notice: _,
            } = prepared;
            // A recorded session parks its outgoing orchestration under its
            // identity. `--no-session` has no recorder, so cutover continues
            // and the owner runtime is dropped instead of panicking.
            let previous_identity = match active.session.recorder_info() {
                Some((session_id, session_path)) => {
                    Some(ApplicationInner::resident_identity(session_id, session_path)?)
                }
                None => None,
            };
            // Every fallible preparation gate has passed. The detached
            // transaction now crosses the MCP reset point of no return before
            // invalidating queued goal admission or cancelling loop work.
            let session = session.commit_mcp_reset().await;
            active.goal_work_activation.lock().take();
            loop_handle
                .commit_prepared_session_switch(loops, crate::LoopRemovalReason::SessionChanged)
                .await;
            application.wait_cutover_after_loop_commit_gate().await;
            active.session.abort().await;
            let active_run = { active.active_run.lock().take() };
            if let Some(run) = active_run {
                let _ = run.await;
            } else {
                active.session.wait_for_idle().await;
            }
            while active.goal_work_pending.load(Ordering::Acquire) != 0 {
                active.goal_work_changed.notified().await;
            }
            active
                .process_manager
                .shutdown_owner(&active.process_owner_id)
                .await;
            active.session.install_session_replacement(session).await;
            let recovered = application
                .commit_same_cwd_orchestration(
                    active.clone(),
                    orchestration,
                    previous_identity,
                    transition,
                )
                .await;
            active.charged_goal_jobs.lock().clear();
            for event in commit_events {
                application.inner.publish(event);
            }
            Ok::<_, anyhow::Error>(recovered)
        })
        .await
        .map_err(|error| anyhow!("session cutover task failed: {error}"))??;
        self.revive_recovered_parent_after_cutover(recovered);
        Ok(())
    }


    pub async fn switch_session(&self, path: &Path) -> Result<SessionChangeOutcome> {
        let prepared = crate::PreparedSessionResume::prepare_path(path)?;
        self.switch_prepared_session(prepared).await
    }

    pub async fn switch_prepared_session(
        &self,
        prepared: crate::PreparedSessionResume,
    ) -> Result<SessionChangeOutcome> {
        if let Some(runtime) = self.extension_runtime()
            && runtime
                .reduce_before_switch(serde_json::json!({
                    "reason": "resume",
                    "targetSessionFile": prepared.path(),
                }))
                .await?
        {
            return Ok(SessionChangeOutcome::CANCELLED);
        }

        let target_session_file = prepared.path().to_path_buf();
        let (target_session_lease, target_session_relative) = prepared.file_authority();
        let target_loop_storage = crate::loop_scheduler::LoopStorage::from_session_authority(
            target_session_lease,
            &target_session_relative,
        )?;
        let target_cwd =
            crate::inspect_operator_directory(prepared.target_cwd()).with_context(|| {
                format!(
                    "opening resumed working directory {}",
                    prepared.target_cwd().display()
                )
            })?;
        let active = self.runtime();
        if target_cwd == active.session.cwd() {
            let operation = self.inner.operation_gate.clone().lock_owned().await;
            active.session.wait_for_idle().await;
            let transition = self.begin_todo_session_transition().await?;
            let session = match active.session.prepare_resume_replacement(prepared).await {
                Ok(session) => session,
                Err(error) => return Err(error),
            };
            let goal = session.goal_runtime();
            if let Some(current) = goal.get().current
                && current.lifecycle == crate::GoalLifecycle::Active
                && let Err(error) = goal.pause_on_resume()
            {
                return Err(anyhow!(error.to_string()));
            }
            let (prepared, notice) = match self.prepare_same_cwd_cutover(&active, session).await {
                Ok(prepared) => prepared,
                Err(error) => return Err(error),
            };
            if let Err(error) = self
                .commit_same_cwd_cutover(
                    active.clone(),
                    prepared,
                    operation,
                    transition,
                    Vec::new(),
                )
                .await
            {
                return Err(error);
            }
            return Ok(match notice {
                Some(notice) => SessionChangeOutcome::completed_with_notice(notice),
                None => SessionChangeOutcome::COMPLETED,
            });
        }
        let factory = self.inner.runtime_factory.lock().clone().ok_or_else(|| {
            anyhow!("cross-directory session switching requires an application runtime factory")
        })?;

        let mut options = active.session.child_session_options_snapshot();
        options.workspace =
            crate::WorkspaceRoots::new(&target_cwd, Vec::<std::path::PathBuf>::new())?;
        let session_options = crate::SessionOptions {
            model: options.model,
            cwd: options.workspace.cwd().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: options.thinking_level,
            api_key: options.api_key,
            compaction: active.session.compaction_settings(),
            stream_options: options.stream_options,
            tools: None,
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(options.stream_fn),
            auth_resolver: options.auth_resolver,
            auth_manager: options.auth_manager,
        };
        let prepared_loops = crate::prepare_loop_activation(Some(target_loop_storage)).await?;
        let candidate = factory
            .build_runtime_candidate(target_cwd, session_options, Some(prepared))
            .await?;
        let operation = self.inner.operation_gate.clone().lock_owned().await;
        if !Arc::ptr_eq(&self.runtime(), &active) {
            return Err(anyhow!(
                "application runtime changed during cross-directory cutover staging"
            ));
        }
        let target_identity = ApplicationInner::session_identity(&candidate.session)?;
        let mut resident_claim = self.inner.take_resident_orchestration(&target_identity);
        let mut candidate = candidate;
        let target_resources = candidate
            .session
            .resource_manager()
            .ok_or_else(|| anyhow!("target session has no resource manager"))?;
        let target_orchestration = resident_claim
            .as_ref()
            .map(ResidentOrchestrationClaim::runtime)
            .or(candidate.orchestration_runtime.as_ref());
        let target_lease = BootstrapGenerationLease::new()?;
        let (target_publication, target_update) = Self::stage_bootstrap_generation(
            &candidate.session,
            &target_resources,
            candidate.extension_runtime.as_ref(),
            target_orchestration,
            candidate.goal_tool_binding.as_ref(),
            candidate.todo_child_tool_binding.as_ref(),
            target_lease,
        )
        .await?;
        if resident_claim.is_none()
            && let Some(orchestration) = candidate.orchestration_runtime.as_ref()
        {
            orchestration.bind_generation_capture(target_publication.capture_fn())?;
        }
        target_resources.bind_publication(&target_publication)?;
        if resident_claim.is_some() {
            drop(candidate.orchestration_runtime.take());
        }
        let loops = self.loop_handle()?;
        let epoch = self.inner.runtime.next_epoch();
        let next = Arc::new(candidate.activate(epoch));
        next.bind_publication(target_publication.clone())?;
        next.session.commit_resource_update(target_update).await;
        let prepared_bindings = match resident_claim.as_ref() {
            Some(resident) => self.prepare_runtime_generation_bindings_for_orchestration(
                &next,
                Some(resident.runtime()),
            )?,
            None => self.prepare_runtime_generation_bindings(&next)?,
        };
        let prepared_loops = loops
            .prepare_session_switch(crate::prepare_loop_session_switch(prepared_loops))
            .await?;
        self.wait_cutover_after_resident_take_gate().await;
        if self.inner.cleaned.load(Ordering::Acquire) {
            return Err(anyhow!("application shut down during session cutover"));
        }
        self.wait_cutover_before_spawn_gate().await;
        if let Some(resident) = resident_claim
            .take()
            .map(ResidentOrchestrationClaim::commit)
        {
            resident
                .runtime
                .install_generation_capture(target_publication.capture_fn());
            let replaced = next.orchestration_runtime.lock().replace(resident.runtime);
            debug_assert!(
                replaced.is_none(),
                "resident runtime transfers into an empty candidate slot"
            );
            drop(resident.publication);
        }
        let application = self.clone();
        tokio::spawn(async move {
            let _operation = operation;
            // Last fallible gate passed. Invalidate goal admission before loop
            // cancellation can release the old generation's shared turn guard.
            active.goal_work_activation.lock().take();
            loops
                .commit_prepared_session_switch(
                    prepared_loops,
                    crate::LoopRemovalReason::SessionChanged,
                )
                .await;
            application.wait_cutover_after_loop_commit_gate().await;
            active.session.abort().await;
            let active_run = { active.active_run.lock().take() };
            if let Some(run) = active_run {
                let _ = run.await;
            } else {
                active.session.wait_for_idle().await;
            }
            application
                .bind_runtime_generation(next.clone(), Some(prepared_bindings))
                .await
                .expect("prepared runtime binding installation is infallible");
            let stale_subscription = active.session_subscription.lock().take();
            let old = application.inner.runtime.replace_arc(next);
            drop(stale_subscription);
            if let Some(binding) = old.goal_tool_binding.lock().take() {
                binding.unbind(&Arc::downgrade(&application.inner));
            }
            if let Some(binding) = old.todo_child_tool_binding.lock().take() {
                binding.unbind_ptr(Arc::as_ptr(&application.inner));
            }
            let orchestration = old.orchestration_runtime.lock().take();
            let old_identity = ApplicationInner::session_identity(&old.session)
                .expect("active application runtimes have a recorded session identity");
            let outgoing_forwarder = old.orchestration_events.lock().take();
            if let Some(task) = outgoing_forwarder {
                task.abort();
                let _ = task.await;
            }
            if let Some(orchestration) = orchestration {
                application.inner.retain_resident_orchestration(
                    old_identity,
                    ResidentOrchestration {
                        runtime: orchestration,
                        publication: old.publication(),
                    },
                );
            }
            let process_task = old.process_events.lock().take();
            let session_task = old.session_events.lock().take();
            let extension = old.extension_runtime.lock().take();
            if let Some(task) = process_task {
                task.abort();
            }
            if let Some(task) = session_task {
                task.abort();
            }
            if let Some((runtime, _)) = extension {
                runtime.shutdown_with_reason("session_switch").await;
            }
        })
        .await
        .map_err(|error| anyhow!("cross-directory cutover task failed: {error}"))?;
        Ok(SessionChangeOutcome::COMPLETED)
    }
    pub async fn new_session(&self) -> Result<SessionChangeOutcome> {
        self.new_session_with_parent(None).await
    }

    pub async fn new_session_with_parent(
        &self,
        parent_session: Option<&Path>,
    ) -> Result<SessionChangeOutcome> {
        if let Some(runtime) = self.extension_runtime()
            && runtime
                .reduce_before_switch(serde_json::json!({ "reason": "new" }))
                .await?
        {
            return Ok(SessionChangeOutcome::CANCELLED);
        }
        let operation = self.inner.operation_gate.clone().lock_owned().await;
        let active = self.runtime();
        self.wait_for_idle().await;
        let transition = self.begin_todo_session_transition().await?;
        let session = match active
            .session
            .prepare_new_session_replacement(parent_session)
        {
            Ok(session) => session,
            Err(error) => return Err(error),
        };
        let (prepared, _notice) = match self.prepare_same_cwd_cutover(&active, session).await {
            Ok(prepared) => prepared,
            Err(error) => return Err(error),
        };
        if let Err(error) = self
            .commit_same_cwd_cutover(
                active.clone(),
                prepared,
                operation,
                transition,
                Vec::new(),
            )
            .await
        {
            return Err(error);
        }
        Ok(SessionChangeOutcome::COMPLETED)
    }

    pub async fn compact(
        &self,
        custom_instructions: Option<&str>,
    ) -> Result<crate::CompactionResult> {
        self.wait_for_idle().await;
        self.runtime().session.compact(custom_instructions).await
    }

    /// Deterministic context archive without any provider call (snapcompact):
    /// older turns are replaced by a statistics block and the original entries
    /// are preserved in a `.snapcompact-<timestamp>.jsonl` sidecar.
    pub async fn compact_snap(&self) -> Result<crate::CompactionResult> {
        self.wait_for_idle().await;
        self.runtime().session.compact_snap().await
    }

    pub async fn fork_session(&self, entry_id: &str) -> Result<SessionForkOutcome> {
        let operation = self.inner.operation_gate.clone().lock_owned().await;
        let active = self.runtime();
        let source_goal = active.session.goal_runtime().get().current;
        let restore_conversation = if let Some(runtime) = active
            .publication()
            .capture()
            .extensions()
            .runtime()
            .cloned()
        {
            let reduction = runtime
                .reduce_before_fork(serde_json::json!({
                    "entryId": entry_id,
                    "position": "before",
                }))
                .await?;
            if reduction.cancel {
                return Ok(SessionForkOutcome::cancelled());
            }
            !reduction.skip_conversation_restore
        } else {
            true
        };
        self.wait_for_idle().await;
        let transition = self.begin_todo_session_transition().await?;
        self.inner.publish(ApplicationEvent::SessionBeforeFork(
            SessionBeforeForkEvent {
                target_id: entry_id.to_owned(),
            },
        ));
        let (session, editor_text) = active
            .session
            .prepare_fork_replacement(entry_id, restore_conversation)
            .await?;
        let goal = session.goal_runtime();
        if let Some(source_goal) = source_goal
            && let Err(error) = goal.fork_clone(&source_goal)
        {
            return Err(session.rollback_unpublished(anyhow!(error.to_string())));
        }
        let (session_id, session_file) = session.recorder_info();
        let (prepared, _notice) = self.prepare_same_cwd_cutover(&active, session).await?;
        let mut commit_events = Vec::with_capacity(2);
        if goal.get().current.is_some() {
            commit_events.push(ApplicationEvent::GoalUpdated {
                operation: "fork_clone",
                state: goal.get(),
            });
        }
        commit_events.push(ApplicationEvent::SessionForked(SessionForkedEvent {
            target_id: entry_id.to_owned(),
            session_id,
            session_file: session_file.to_string_lossy().into_owned(),
            editor_text: editor_text.clone(),
        }));
        self.commit_same_cwd_cutover(
            active.clone(),
            prepared,
            operation,
            transition,
            commit_events,
        )
        .await?;
        Ok(SessionForkOutcome::completed(editor_text))
    }
    pub async fn navigate_tree(
        &self,
        entry_id: &str,
        options: crate::NavigateTreeOptions,
    ) -> Result<crate::NavigateTreeResult> {
        let mut options = options;
        #[cfg(test)]
        let force_cancel =
            options.custom_instructions.as_deref() == Some("__test_cancel_after_claim__");
        if let Some(runtime) = self.extension_runtime() {
            let reduction = runtime
                .reduce_before_tree(serde_json::json!({
                    "preparation": {
                        "targetId": entry_id,
                        "userWantsSummary": options.summarize,
                        "customInstructions": options.custom_instructions.as_deref(),
                        "replaceInstructions": options.replace_instructions,
                        "label": options.label.as_deref(),
                    }
                }))
                .await?;
            if reduction.cancel {
                let active_leaf_id = self
                    .runtime()
                    .session
                    .session_tree()?
                    .active_leaf_id
                    .clone();
                return Ok(crate::NavigateTreeResult {
                    editor_text: None,
                    active_leaf_id,
                    summary_entry_id: None,
                    changed: false,
                    cancelled: true,
                });
            }
            if let Some(summary) = reduction.summary {
                options.summary = Some(summary.summary);
                options.summarize = true;
            }
            if reduction.custom_instructions.is_some() {
                options.custom_instructions = reduction.custom_instructions;
            }
            if reduction.replace_instructions.is_some() {
                options.replace_instructions = reduction.replace_instructions;
            }
            if reduction.label.is_some() {
                options.label = reduction.label;
            }
        }
        self.wait_for_idle().await;
        let transition = self.begin_todo_session_transition().await?;
        self.inner.publish(ApplicationEvent::SessionBeforeTree(
            SessionBeforeTreeEvent {
                target_id: entry_id.to_owned(),
                summarize: options.summarize,
            },
        ));
        #[cfg(test)]
        let result = if force_cancel {
            self.runtime()
                .session
                .navigate_tree_cancel_for_test(entry_id, options)
                .await?
        } else {
            self.runtime().session.navigate_tree(entry_id, options).await?
        };
        #[cfg(not(test))]
        let result = self.runtime().session.navigate_tree(entry_id, options).await?;
        self.inner
            .publish(ApplicationEvent::SessionTree(SessionTreeEvent {
                target_id: entry_id.to_owned(),
                active_leaf_id: result.active_leaf_id.clone(),
                editor_text: result.editor_text.clone(),
                summary_entry_id: result.summary_entry_id.clone(),
                changed: result.changed,
                cancelled: result.cancelled,
            }));
        if let Some(runtime) = self.extension_runtime() {
            let _ = runtime
                .emit(ExtensionEvent::new(
                    "session_tree",
                    serde_json::json!({
                        "targetId": entry_id,
                        "activeLeafId": result.active_leaf_id,
                        "editorText": result.editor_text,
                        "summaryEntryId": result.summary_entry_id,
                        "changed": result.changed,
                        "cancelled": result.cancelled,
                    }),
                ))
                .await;
        }
        if !result.cancelled {
            transition.commit();
        }
        Ok(result)
    }

    pub fn set_session_label(&self, target_id: &str, label: Option<&str>) -> Result<()> {
        self.runtime().session.set_session_label(target_id, label)
    }

    /// Mark the current position as a named rewind target.
    pub fn set_checkpoint(&self, name: &str) -> Result<String> {
        self.runtime().session.set_checkpoint(name)
    }

    /// Render the last `limit` records (index + preview) for the `/rewind`
    /// picker.
    pub fn rewind_preview(&self, limit: usize) -> Result<Vec<crate::RewindEntryPreview>> {
        self.runtime().session.rewind_preview(limit)
    }

    /// Roll the session back to a rewind target.
    ///
    /// Safety rules enforced here, on top of the session-level guards:
    /// rewinding is refused while orchestration jobs are queued/running, while
    /// any workflow is active, or while bash is still executing — truncating
    /// the journal under live work would orphan running jobs and corrupt the
    /// state those jobs keep reading from. The active turn is drained first
    /// via [`Application::wait_for_idle`]; the session-level exclusive run
    /// slot additionally refuses a rewind issued while a prompt is running.
    pub async fn rewind(&self, target: crate::RewindTarget) -> Result<crate::RewindOutcome> {
        let _operation = self.inner.operation_gate.lock().await;
        // Drain the active turn first so the safety checks below observe the
        // post-turn state (a running turn may spawn orchestration jobs).
        self.wait_for_idle().await;
        let active = self.runtime();
        let jobs = active
            .orchestration_runtime()
            .map(|runtime| runtime.jobs(None))
            .unwrap_or_default();
        let workflows = self.workflow_list();
        if let Some(refusal) = Self::rewind_refusal(&jobs, &workflows) {
            bail!("{refusal}");
        }
        if active.session.is_bash_running() {
            bail!("rewind refused: bash is still running — cancel it first");
        }
        let outcome = active.session.rewind(target).await?;
        active
            .todo_dag
            .lock()
            .restore_from_session_with_jobs(&active.session, &jobs);
        Ok(outcome)
    }

    /// Pure refusal predicate for the rewind safety rules, factored out so it
    /// is unit-testable with snapshot vectors: refuses while any orchestration
    /// job is queued/running or any workflow is active. Returns the actionable
    /// message to surface, or `None` when rewinding is safe.
    fn rewind_refusal(
        jobs: &[crate::JobSnapshot],
        workflows: &[crate::WorkflowSnapshot],
    ) -> Option<String> {
        let running_jobs = jobs
            .iter()
            .filter(|job| {
                matches!(
                    job.status,
                    crate::JobStatus::Queued | crate::JobStatus::Running
                )
            })
            .count();
        if running_jobs > 0 {
            return Some(format!(
                "rewind refused: {running_jobs} orchestration job(s) are still queued or running — wait for them to finish, or cancel them first (orchestration /queue cancel, workflow /workflow cancel)"
            ));
        }
        let active_workflows = workflows
            .iter()
            .filter(|workflow| workflow.status.is_active())
            .collect::<Vec<_>>();
        if !active_workflows.is_empty() {
            return Some(format!(
                "rewind refused: {} workflow(s) are still active ({}) — cancel or wait for them before rewinding",
                active_workflows.len(),
                active_workflows
                    .iter()
                    .map(|workflow| workflow.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        None
    }

    pub async fn clone_session(&self) -> Result<SessionChangeOutcome> {
        let operation = self.inner.operation_gate.clone().lock_owned().await;
        let active = self.runtime();
        let source_goal = active.session.goal_runtime().get().current;
        let target_id = active
            .session
            .session_tree()?
            .active_leaf_id
            .clone()
            .ok_or_else(|| anyhow!("Cannot clone session: no current entry selected"))?;
        let restore_conversation = if let Some(runtime) = active
            .publication()
            .capture()
            .extensions()
            .runtime()
            .cloned()
        {
            let reduction = runtime
                .reduce_before_fork(serde_json::json!({
                    "entryId": target_id,
                    "position": "at",
                }))
                .await?;
            if reduction.cancel {
                return Ok(SessionChangeOutcome::CANCELLED);
            }
            !reduction.skip_conversation_restore
        } else {
            true
        };
        self.wait_for_idle().await;
        let transition = self.begin_todo_session_transition().await?;
        let session = match active
            .session
            .prepare_clone_replacement(&target_id, restore_conversation)
        {
            Ok(session) => session,
            Err(error) => return Err(error),
        };
        let goal = session.goal_runtime();
        if let Some(source_goal) = source_goal
            && let Err(error) = goal.fork_clone(&source_goal)
        {
            return Err(anyhow!(error.to_string()));
        }
        let (prepared, _notice) = match self.prepare_same_cwd_cutover(&active, session).await {
            Ok(prepared) => prepared,
            Err(error) => return Err(error),
        };
        let mut commit_events = Vec::with_capacity(1);
        if goal.get().current.is_some() {
            commit_events.push(ApplicationEvent::GoalUpdated {
                operation: "fork_clone",
                state: goal.get(),
            });
        }
        if let Err(error) = self
            .commit_same_cwd_cutover(
                active.clone(),
                prepared,
                operation,
                transition,
                commit_events,
            )
            .await
        {
            return Err(error);
        }
        Ok(SessionChangeOutcome::COMPLETED)
    }

    pub fn fork_messages(
        &self,
        cursor: Option<&str>,
        limit: Option<usize>,
    ) -> Result<crate::ForkMessagePage> {
        self.runtime().session.fork_messages(cursor, limit)
    }

    pub fn session_entries(&self, since: Option<&str>) -> Result<crate::SessionEntries> {
        self.runtime().session.session_entries(since)
    }

    pub fn session_tree(&self) -> Result<crate::SessionTreeResult> {
        self.runtime().session.session_tree()
    }

    pub fn export_html(&self, output: Option<&Path>) -> Result<PathBuf> {
        crate::export_live_session(
            &self.runtime().session,
            output,
            &crate::ExportOptions::default(),
        )
    }

    pub fn export_jsonl(&self, output: Option<&Path>) -> Result<PathBuf> {
        let (_, path) = self
            .runtime()
            .session
            .recorder_info()
            .ok_or_else(|| anyhow!("session recording is unavailable"))?;
        crate::export_session_jsonl(&path, output)
    }

    /// Public `/reload` adapter over the session resource snapshot and extension layer.
    pub fn commands_catalog(&self) -> Vec<crate::ApplicationCommand> {
        let mut commands = Vec::new();
        if let Some(runtime) = self.extension_runtime() {
            commands.extend(runtime.command_sources().into_iter().map(|source| {
                crate::ApplicationCommand {
                    name: source.command.name,
                    description: source.command.description,
                    source: "extension".to_owned(),
                    source_info: crate::CommandSourceInfo {
                        path: source.path.to_string_lossy().into_owned(),
                        source: source.source,
                        scope: source.scope,
                        origin: source.origin,
                        base_dir: source
                            .base_dir
                            .map(|path| path.to_string_lossy().into_owned()),
                    },
                }
            }));
        }
        if let Some(snapshot) = self.resource_snapshot() {
            commands.extend(
                snapshot
                    .prompts
                    .iter()
                    .map(|prompt| crate::ApplicationCommand {
                        name: prompt.name.clone(),
                        description: (!prompt.description.is_empty())
                            .then(|| prompt.description.clone()),
                        source: "prompt".to_owned(),
                        source_info: crate::CommandSourceInfo::local(
                            &prompt.file_path,
                            match prompt.scope {
                                crate::ResourceScope::Global => "user",
                                crate::ResourceScope::Project => "project",
                                crate::ResourceScope::Explicit => "temporary",
                            },
                        ),
                    }),
            );
            if snapshot.settings.enable_skill_commands.unwrap_or(true) {
                commands.extend(
                    snapshot
                        .skills
                        .iter()
                        .filter(|skill| {
                            skill.trusted && !skill.hidden && !skill.disable_model_invocation
                        })
                        .map(|skill| crate::ApplicationCommand {
                            name: format!("skill:{}", skill.name),
                            description: (!skill.description.is_empty())
                                .then(|| skill.description.clone()),
                            source: "skill".to_owned(),
                            source_info: crate::CommandSourceInfo {
                                path: skill.file_path.clone(),
                                source: "local".to_owned(),
                                scope: if skill
                                    .file_path
                                    .starts_with(&snapshot.cwd.to_string_lossy().into_owned())
                                {
                                    "project"
                                } else {
                                    "user"
                                }
                                .to_owned(),
                                origin: "top-level".to_owned(),
                                base_dir: Some(skill.base_dir.clone()),
                            },
                        }),
                );
            }
        }
        commands
    }

    pub fn expand_resource_command(&self, name: &str, arguments: &str) -> Result<Option<String>> {
        let Some(snapshot) = self.resource_snapshot() else {
            return Ok(None);
        };
        if let Some(template) = snapshot
            .prompts
            .iter()
            .find(|template| template.name == name)
        {
            return Ok(Some(crate::substitute_args(
                &template.content,
                &crate::parse_command_args(arguments),
            )));
        }
        let Some(skill_name) = name.strip_prefix("skill:") else {
            return Ok(None);
        };
        if !snapshot.settings.enable_skill_commands.unwrap_or(true) {
            return Err(anyhow!("skill commands are disabled"));
        }
        let mut matches = snapshot
            .skills
            .iter()
            .filter(|skill| skill.name == skill_name);
        let Some(skill) = matches.next() else {
            return Err(anyhow!("unknown skill command /skill:{skill_name}"));
        };
        if matches.next().is_some() {
            return Err(anyhow!(
                "skill command /skill:{skill_name} is ambiguous; skill names must be unique"
            ));
        }
        if !skill.trusted {
            return Err(anyhow!("skill /skill:{skill_name} is not trusted"));
        }
        if skill.hidden || skill.disable_model_invocation {
            return Err(anyhow!(
                "skill /skill:{skill_name} is disabled for interactive invocation"
            ));
        }
        let uri = format!("skill://{}", skill.name);
        let content = crate::selector::read_skill_uri_with_capabilities(
            snapshot.capabilities(),
            &uri,
            &snapshot.skills,
        )?;
        let body = crate::resources::strip_skill_frontmatter(&content);
        let block = format!(
            "<skill name=\"{}\" location=\"{uri}\">\nReferences are relative to {uri}/.\n\n{}\n</skill>",
            skill.name,
            body.trim()
        );
        Ok(Some(if arguments.is_empty() {
            block
        } else {
            format!("{block}\n\n{arguments}")
        }))
    }

    fn orchestration_candidate(
        &self,
        snapshot: &crate::ResourceSnapshot,
        settings: &crate::RuntimeSettingsSnapshot,
    ) -> Result<Option<crate::OrchestrationRuntime>> {
        let active = self.runtime();
        let enabled =
            settings.orchestration_enabled || active.orchestration_explicit.load(Ordering::Acquire);
        if !enabled {
            // A live runtime with unsettled jobs must never be retired by a
            // settings reload: deferring the disable here mirrors the
            // non-equivalent replacement deferral below, so in-flight
            // delegation settles exactly-once instead of receiving
            // `orchestration shut down`. A later quiescent reload applies it.
            if let Some(current) = self.runtime().orchestration_runtime()
                && ApplicationInner::has_active_jobs(&current)
            {
                return Ok(Some(current));
            }
            return Ok(None);
        }
        let mut config = crate::OrchestrationConfig::new(
            crate::AgentCatalog::from_agents(snapshot.agents.clone()),
            snapshot.cwd.join(".rpi").join("artifacts"),
        );
        config.skills = snapshot
            .skills
            .iter()
            .map(crate::OrchestrationSkill::from)
            .collect();
        config.max_concurrency = settings.orchestration_max_concurrency;
        config.max_recursion_depth = settings.orchestration_max_recursion_depth;
        config.mailbox_capacity = settings.orchestration_mailbox_capacity;
        config.max_tools_per_agent = settings.orchestration_max_tools_per_agent;
        config.soft_budget = settings.orchestration_soft_budget;
        config.preferred_agent = snapshot
            .settings
            .orchestration
            .as_ref()
            .and_then(|orchestration| orchestration.preferred_agent.clone());
        config =
            config.with_selector_settings(snapshot.settings.selector.clone().unwrap_or_default());
        config.agent_settings = snapshot.settings.agents.clone();
        if let Some(model) = active.session.model() {
            config.parent_model = model;
        }
        // Live parent model so /model switches apply to child resolution without rebuild.
        let session_for_parent = active.session.clone();
        config = config.with_parent_model_provider(std::sync::Arc::new(move || {
            session_for_parent.model().unwrap_or_default()
        }));
        let (resolver, resolver_binding) = crate::InternalUriResolver::deferred();
        let factory = crate::OrchestrationRuntime::child_factory_from_session_and_uri(
            &active.session,
            Some(resolver),
        );
        let runtime = crate::OrchestrationRuntime::new(config, factory)?;
        if let Some(current) = self.runtime().orchestration_runtime()
            && (current.runtime_equivalent(&runtime) || ApplicationInner::has_active_jobs(&current))
        {
            return Ok(Some(current));
        }
        resolver_binding.bind(runtime.read_uri_resolver())?;
        Ok(Some(runtime))
    }

    async fn commit_orchestration_candidate(
        &self,
        candidate: Option<crate::OrchestrationRuntime>,
    ) -> Result<()> {
        let active = self.runtime();
        let same_runtime = active
            .orchestration_runtime()
            .zip(candidate.as_ref())
            .is_some_and(|(current, candidate)| current.shares_runtime(candidate));
        // Atomic retirement protocol: the same `jobs.spawn_lock` that guards
        // spawn registration also guards the retirement decision, so no spawn
        // can register on the outgoing runtime between the active-job check
        // and the replacement. Order inside the gate: check unsettled jobs →
        // mark retiring → swap the application slot. A spawn that resolved a
        // stale handle before the swap is rejected by the retiring flag inside
        // its own guarded section (never creates an authoritative child, no
        // `orchestration shut down`); a spawn that registered before the gate
        // is visible to the check and defers the whole replacement. Terminal
        // shutdown and cleanup keep their own authority.
        let next_runtime = candidate.as_ref().cloned();
        let (previous, runtime_changed) = if !same_runtime {
            let outgoing = active.orchestration_runtime();
            match outgoing.as_ref() {
                Some(current) => {
                    // Atomic retirement: the same spawn gate guards spawn
                    // registration and this decision, so no spawn can register
                    // between the active-job check and the slot swap. A stale
                    // spawn that raced ahead of the gate is visible to the
                    // check (deferral); one racing behind is rejected by the
                    // retiring flag before creating an authoritative child.
                    let spawn_guard = current.lock_spawns_for_retirement();
                    if ApplicationInner::has_active_jobs(current) {
                        return Ok(());
                    }
                    current.begin_retirement();
                    drop(spawn_guard);
                    // The constructor forwarder observes todo jobs. Stop it
                    // while this runtime is still installed, then swap.
                    let forwarder = active.orchestration_events.lock().take();
                    if let Some(task) = forwarder {
                        task.abort();
                        let _ = task.await;
                    }
                    let _spawn_guard = current.lock_spawns_for_retirement();
                    let (previous, runtime_changed) = {
                        let mut slot = active.orchestration_runtime.lock();
                        (std::mem::replace(&mut *slot, candidate), true)
                    };
                    (previous, runtime_changed)
                }
                None => {
                    let mut slot = active.orchestration_runtime.lock();
                    let installed = candidate.is_some();
                    *slot = candidate;
                    (None, installed)
                }
            }
        } else {
            let mut current = active.orchestration_runtime.lock();
            if current
                .as_ref()
                .zip(candidate.as_ref())
                .is_some_and(|(current, candidate)| current.shares_runtime(candidate))
            {
                (None, false)
            } else {
                (std::mem::replace(&mut *current, candidate), true)
            }
        };
        if runtime_changed {
            self.inner.steered_main_message_ids.lock().clear();
            let forwarder = active.orchestration_events.lock().take();
            if let Some(task) = forwarder {
                task.abort();
                let _ = task.await;
            }
            if let Some(runtime) = &next_runtime {
                let mut events = runtime.subscribe();
                self.inner.replay_main_mailbox(&active, runtime).await;
                let event_runtime = runtime.clone();
                let event_inner = Arc::downgrade(&self.inner);
                let epoch = active.epoch();
                *active.orchestration_events.lock() = Some(tokio::spawn(async move {
                    loop {
                        match events.recv().await {
                            Ok(event) => {
                                let Some(inner) = event_inner.upgrade() else {
                                    break;
                                };
                                let active = inner.runtime();
                                if active.epoch() != epoch {
                                    break;
                                }
                                let allow_spawn =
                                    !active.todo_continuation_suppressed.load(Ordering::Acquire);
                                if let Err(error) = inner.observe_orchestration_event(
                                    &event_runtime,
                                    &event,
                                    allow_spawn,
                                ) {
                                    inner.todo_dag_failed(&error);
                                }
                                inner
                                    .steer_main_delivered_message(&active, &event)
                                    .await
                                    .unwrap_or_else(|error| {
                                        inner.report_steer_main_delivery_failure(&event, &error)
                                    });
                                let _ = inner
                                    .runtime
                                    .publish(epoch, ApplicationEvent::Orchestration(event));
                                inner.finish_todo_cycle_if_idle(Some(&event_runtime), false);
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                let Some(inner) = event_inner.upgrade() else {
                                    break;
                                };
                                let active = inner.runtime();
                                if active.epoch() != epoch {
                                    break;
                                }
                                for event in event_runtime.presentation_events() {
                                    let allow_spawn = !active
                                        .todo_continuation_suppressed
                                        .load(Ordering::Acquire);
                                    if let Err(error) = inner.observe_orchestration_event(
                                        &event_runtime,
                                        &event,
                                        allow_spawn,
                                    ) {
                                        inner.todo_dag_failed(&error);
                                    }
                                    // `presentation_events` is a UI snapshot
                                    // (jobs and agents), never an unconsumed
                                    // context queue: historical
                                    // `MessageDelivered` must NOT be steered
                                    // into the main session here.
                                    let _ = inner
                                        .runtime
                                        .publish(epoch, ApplicationEvent::Orchestration(event));
                                    inner.finish_todo_cycle_if_idle(Some(&event_runtime), false);
                                }
                            }
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }));
            }
        }
        if let Some(previous) = previous {
            previous.shutdown().await;
        }
        if runtime_changed && let Some(runtime) = &next_runtime {
            runtime
                .persist_durable_state()
                .context("persisting replacement orchestration runtime")?;
        }
        Ok(())
    }

    pub async fn reload(&self) -> Result<crate::ReloadResult> {
        self.wait_for_idle().await;
        // A child spawned by the preceding todo mutation stays Queued until this
        // task yields. On a current-thread runtime it would still look active
        // at the retirement check and orchestration could never be disabled.
        // Yield only while a job has not started; a job that is already
        // Running remains a reason to defer retirement.
        self.drive_queued_orchestration().await;
        Box::pin(self.reload_with_candidate_trust()).await
    }

    async fn drive_queued_orchestration(&self) {
        for _ in 0..32 {
            let Some(runtime) = self.runtime().orchestration_runtime() else {
                return;
            };
            let queued = runtime
                .jobs(None)
                .iter()
                .any(|job| job.status == crate::JobStatus::Queued);
            if !queued {
                return;
            }
            tokio::task::yield_now().await;
        }
    }

    async fn reload_with_candidate_trust(&self) -> Result<crate::ReloadResult> {
        let active = self.runtime();
        let publication = active.publication();
        let lease = publication.begin_reload().await?;
        let provenance = lease.provenance().clone();
        let resources = active
            .session
            .resource_manager()
            .ok_or_else(|| anyhow!("session has no resource manager"))?;
        let model = active
            .session
            .model()
            .ok_or_else(|| anyhow!("session has no model"))?;
        let request_auth = active.session.request_auth_for_model(&model).await?;
        let stream = active.session.stream_options().stream;
        let extension_runtime = active
            .extension_runtime
            .lock()
            .as_ref()
            .map(|(runtime, _)| runtime.clone());
        let composed_trust = Self::compose_candidate_host_trust(
            &active.session,
            &resources,
            provenance.clone(),
            extension_runtime.as_ref(),
        )
        .await?;
        let resource_candidate =
            resources.stage_candidate(provenance.clone(), composed_trust.as_ref())?;
        let settings = resource_candidate.snapshot().settings.clone();
        let image_generation =
            Self::staged_image_generation_http(&active.session, &settings, &model, &request_auth)
                .await?;
        let next_runtime_settings = settings.runtime_settings()?;
        let policy_inputs = crate::sandbox::SessionPolicyInputs::validate_with_capabilities(
            active.session.session_policy_id(),
            active.session.workspace_roots().clone(),
            resources.options().agent_dir,
            active.session.session_dir(),
            resource_candidate.capabilities(),
        )?;
        let sandbox = crate::sandbox::build_sandbox_generation(
            settings.sandbox.as_ref(),
            &settings.mcp_servers,
            pi_sandbox::PolicyRevision::new(provenance.revision().get())?,
            provenance.clone(),
            resource_candidate.capabilities(),
            policy_inputs,
            &model,
            &request_auth,
            &stream,
            image_generation
                .as_ref()
                .map(|image| (&image.model, image.base_url.as_deref(), &image.env)),
            &Self::builtin_http_endpoints(&settings),
        )?;

        let resource = resource_candidate.finish(sandbox)?;
        let extension_runtime = active.extension_runtime.lock().clone();
        let extension_candidate = if let Some((runtime, permissions)) = &extension_runtime {
            let specs = resource.extension_specs(permissions)?;
            let candidate = runtime
                .stage_reload(
                    specs,
                    crate::extensions::ExtensionCandidateAuthority::new(
                        resource.sandbox().clone(),
                        provenance.clone(),
                        active.session.auth_manager(),
                    ),
                )
                .await;
            let report = candidate.report();
            if !report.failures.is_empty() {
                runtime.discard_reload(candidate).await;
                return Err(extension_load_report_error("extension reload", &report));
            }
            runtime.prepare_reload(&candidate).await?;
            Some(candidate)
        } else {
            None
        };

        let orchestration_candidate =
            self.orchestration_candidate(&resource, &next_runtime_settings)?;
        let mut additional_tools = Vec::new();
        if let (Some((runtime, _)), Some(candidate)) =
            (&extension_runtime, extension_candidate.as_ref())
        {
            additional_tools.extend(candidate.agent_tools(runtime));
        }
        if let Some(orchestration) = &orchestration_candidate {
            additional_tools.extend(orchestration.agent_tools("Main", 0));
        }
        if let Some(binding) = active.goal_tool_binding.lock().as_ref() {
            additional_tools.push(binding.tool());
        }
        if let Some(binding) = active.todo_child_tool_binding.lock().as_ref() {
            additional_tools.extend(binding.tools());
        }
        let (tools, update) = active.session.stage_tool_generation(
            provenance.clone(),
            resource.clone(),
            additional_tools,
        )?;
        let extensions = match (&extension_runtime, extension_candidate.as_ref()) {
            (Some((runtime, permissions)), Some(candidate)) => ExtensionGeneration::enabled(
                provenance.clone(),
                runtime.clone(),
                candidate.stage_snapshot(),
                Arc::new(permissions.clone()),
            )?,
            (None, None) => ExtensionGeneration::disabled(provenance.clone()),
            _ => bail!("extension reload candidate state is inconsistent"),
        };
        let orchestration = match &orchestration_candidate {
            Some(runtime) => runtime.stage_generation(provenance.clone())?,
            None => OrchestrationGeneration::disabled(provenance.clone()),
        };
        let runtime = RuntimeGeneration::new(provenance.clone(), next_runtime_settings.clone());
        let capture_generation = publication.capture_fn();
        let host_hooks = settings
            .hooks
            .clone()
            .filter(|hooks| !hooks.is_empty())
            .map(|hooks| {
                Arc::new(crate::HostHooks::new(
                    hooks,
                    active.session.cwd().to_path_buf(),
                    active.session.session_policy_id(),
                    capture_generation.clone(),
                ))
            });
        let security = SecurityGeneration::new(
            provenance.clone(),
            Arc::new(crate::sandbox::ApprovedSpawnRegistry::default()),
            settings.permission_rules.clone(),
            host_hooks,
        );

        let next = ApplicationGeneration::new(
            provenance.clone(),
            resource.clone(),
            tools,
            extensions,
            orchestration,
            runtime,
            security,
            active.session.brush_exec_lock(),
        )?;
        let result = crate::ReloadResult {
            generation: provenance.revision().get(),
            diagnostics: resource.diagnostics.clone(),
        };
        let post_publish = PostPublishActions::new(provenance, Vec::new(), Vec::new(), false, None);
        let prepared = lease.prepare(next, result, post_publish)?;
        let current_orchestration = active.orchestration_runtime();
        let retained_orchestration = current_orchestration
            .as_ref()
            .zip(orchestration_candidate.as_ref())
            .is_some_and(|(current, candidate)| current.shares_runtime(candidate));
        if let Some(runtime) = &orchestration_candidate
            && !retained_orchestration
        {
            runtime.bind_generation_capture(prepared.capture_fn())?;
            if current_orchestration.is_some() {
                runtime
                    .bind_and_reset(&active.session)
                    .context("resetting replacement orchestration runtime")?;
            } else {
                runtime
                    .bind_and_recover(&active.session)
                    .context("binding replacement orchestration runtime")?;
            }
        }
        let receipt = prepared.publish();
        if let Some(runtime) = &orchestration_candidate {
            runtime.install_generation_capture(capture_generation);
        }

        if let (Some((runtime, _)), Some(candidate)) = (&extension_runtime, extension_candidate) {
            if let Err(error) = candidate.activate_after_application_publish(runtime) {
                eprintln!("extensions: post-publication activation failed: {error:#}");
            } else {
                runtime.finish_reload("reload").await;
            }
        }
        active.session.commit_resource_update(update).await;
        *active.runtime_settings.lock() = Arc::new(next_runtime_settings);
        if let Err(error) = self
            .commit_orchestration_candidate(orchestration_candidate)
            .await
        {
            eprintln!("orchestration: post-publication activation failed: {error:#}");
        }
        let result = receipt.reload_result().clone();
        receipt.notify_and_retire(self).await;
        Ok(result)
    }

    #[must_use]
    pub fn resource_generation(&self) -> Option<u64> {
        self.runtime()
            .session
            .resource_manager()
            .map(|resources| resources.generation())
    }

    #[must_use]
    pub fn resource_snapshot(&self) -> Option<Arc<crate::ResourceSnapshot>> {
        self.runtime()
            .session
            .resource_manager()
            .map(|resources| resources.snapshot())
    }

    pub async fn set_steering_mode(&self, mode: pi_agent::QueueMode) {
        self.runtime().session.set_steering_mode(mode).await;
    }

    pub async fn set_follow_up_mode(&self, mode: pi_agent::QueueMode) {
        self.runtime().session.set_follow_up_mode(mode).await;
    }

    pub fn set_auto_compaction_enabled(&self, enabled: bool) {
        self.runtime().session.set_auto_compaction_enabled(enabled);
    }
    pub fn set_session_name(&self, name: &str) -> Result<()> {
        self.runtime().session.set_session_name(name)
    }

    pub async fn set_project_trust(
        &self,
        decision: crate::TrustDecision,
    ) -> Result<crate::ReloadResult> {
        let active = self.runtime();
        let resources = active
            .session
            .resource_manager()
            .ok_or_else(|| anyhow!("session has no resource manager"))?;
        resources
            .trust_store()
            .set(active.session.cwd(), decision)?;
        self.reload().await
    }

    pub fn set_model(&self, model: Model, api_key: String) -> crate::ThinkingLevelChange {
        self.runtime().session.set_model(model, api_key)
    }

    pub async fn set_model_with_resolved_auth(
        &self,
        model: Model,
    ) -> Result<crate::ThinkingLevelChange> {
        self.runtime()
            .session
            .set_model_with_resolved_auth(model)
            .await
    }
    pub fn set_thinking_level(&self, level: ThinkingLevel) -> crate::ThinkingLevelChange {
        self.runtime().session.set_thinking_level(level)
    }

    /// Persists this application's orchestration state before host process
    /// teardown. Process-level registries coordinate Todo and Workflow child
    /// applications separately so shared factories cannot recurse.
    fn checkpoint_runtimes_for_restart(&self) -> Result<()> {
        let mut checkpointed = Vec::new();
        if let Some(runtime) = self.orchestration_runtime() {
            runtime.checkpoint_for_restart()?;
            checkpointed.push(runtime);
        }
        for resident in self.inner.resident_orchestration.lock().values() {
            if let Err(error) = resident.runtime.checkpoint_for_restart() {
                return match resume_orchestration_after_failed_restart(&checkpointed) {
                    Ok(()) => Err(error),
                    Err(rollback_error) => Err(anyhow!(
                        "{error:#}; additionally failed to resume orchestration after restart checkpoint failure: {rollback_error:#}"
                    )),
                };
            }
            checkpointed.push(resident.runtime.clone());
        }
        self.inner
            .restart_checkpointed
            .store(true, Ordering::Release);
        Ok(())
    }

    pub fn checkpoint_for_restart(&self) -> Result<()> {
        self.checkpoint_runtimes_for_restart()
    }

    fn resume_after_failed_restart_checkpoint(&self) -> Result<()> {
        let mut runtimes = self
            .inner
            .resident_orchestration
            .lock()
            .values()
            .map(|resident| resident.runtime.clone())
            .collect::<Vec<_>>();
        if let Some(runtime) = self.orchestration_runtime() {
            runtimes.push(runtime);
        }
        resume_orchestration_after_failed_restart(&runtimes)?;
        self.inner
            .restart_checkpointed
            .store(false, Ordering::Release);
        Ok(())
    }

    pub fn checkpoint_process_for_restart(
        roots: &[Application],
        todo_service: Option<&TodoChildService>,
    ) -> Result<()> {
        fn checkpoint_application_once(
            application: &Application,
            seen: &mut std::collections::HashSet<usize>,
            workflow_factories: &mut Vec<Arc<ApplicationWorkflowRuntimeFactory>>,
            checkpointed: &mut Vec<Application>,
        ) -> Result<()> {
            let identity = Arc::as_ptr(&application.inner) as usize;
            if !seen.insert(identity) {
                return Ok(());
            }
            application.checkpoint_runtimes_for_restart()?;
            checkpointed.push(application.clone());
            if let Some(factory) = application
                .inner
                .workflow_runtime_factory
                .lock()
                .as_ref()
                .and_then(Weak::upgrade)
            {
                workflow_factories.push(factory);
            }
            Ok(())
        }

        let checkpoint = || -> Result<()> {
            let mut workflow_factories = Vec::new();
            let mut seen = std::collections::HashSet::new();
            let mut checkpointed = Vec::new();
            let result = (|| {
                for root in roots {
                    checkpoint_application_once(
                        root,
                        &mut seen,
                        &mut workflow_factories,
                        &mut checkpointed,
                    )?;
                }
                if let Some(service) = todo_service {
                    service.checkpoint_applications_for_restart(|application| {
                        checkpoint_application_once(
                            application,
                            &mut seen,
                            &mut workflow_factories,
                            &mut checkpointed,
                        )
                    })?;
                }
                let mut index = 0;
                while index < workflow_factories.len() {
                    for application in workflow_factories[index].restart_checkpoint_applications() {
                        checkpoint_application_once(
                            &application,
                            &mut seen,
                            &mut workflow_factories,
                            &mut checkpointed,
                        )?;
                    }
                    index += 1;
                }
                Ok(())
            })();
            let Err(error) = result else {
                return Ok(());
            };
            let rollback_errors = checkpointed
                .iter()
                .rev()
                .filter_map(|application| {
                    application.resume_after_failed_restart_checkpoint().err()
                })
                .map(|rollback| rollback.to_string())
                .collect::<Vec<_>>();
            if rollback_errors.is_empty() {
                Err(error)
            } else {
                Err(anyhow!(
                    "{error:#}; additionally failed to resume applications after restart checkpoint failure: {}",
                    rollback_errors.join("; ")
                ))
            }
        };
        checkpoint()
    }
    /// Stops every runtime owned by this application; single-flight and
    /// cancellation-safe: callers join one owned teardown's terminal outcome.
    pub async fn cleanup(&self) -> Result<()> {
        let mut flight_slot = self.inner.cleanup_flight.lock().await;
        if let Some(flight) = flight_slot.as_ref() {
            let mut outcome = flight.outcome.clone();
            drop(flight_slot);
            return wait_for_cleanup_outcome(&mut outcome).await;
        }
        if self.inner.restart_checkpointed.load(Ordering::Acquire) {
            // Checkpointed state survives restart; publish the shared terminal no-op.
            self.inner.cleaned.store(true, Ordering::Release);
            let (_sender, outcome) =
                tokio::sync::watch::channel(Some(Ok::<(), String>(())));
            *flight_slot = Some(CleanupFlight { outcome });
            return Ok(());
        }
        self.inner.cleaned.store(true, Ordering::Release);
        let (sender, outcome) = tokio::sync::watch::channel(None::<Result<(), String>>);
        *flight_slot = Some(CleanupFlight {
            outcome: outcome.clone(),
        });
        drop(flight_slot);
        let application = Application {
            inner: Arc::clone(&self.inner),
        };
        Self::spawn_cleanup_flight(application, sender);
        let mut outcome = outcome;
        wait_for_cleanup_outcome(&mut outcome).await
    }

    async fn teardown(self) -> Result<()> {
        let _operation = self.inner.operation_gate.clone().lock_owned().await;
        if self.inner.restart_checkpointed.load(Ordering::Acquire) {
            return Ok(());
        }
        let active = self.runtime();
        if let Some(binding) = active.goal_tool_binding.lock().take() {
            binding.unbind(&Arc::downgrade(&self.inner));
        }
        if let Some(binding) = active.todo_child_tool_binding.lock().take() {
            binding.unbind_ptr(Arc::as_ptr(&self.inner));
        }
        active.session.abort().await;
        self.wait_for_idle().await;

        if let Some(task) = active.active_run.lock().take() {
            task.abort();
        }
        // Stop the workflow manager event forwarder first and await its
        // termination (mirrors `rebind_workflows`): the forwarder holds a
        // strong `WorkflowManager` reference and keeps publishing
        // `ApplicationEvent::Workflow` into the application broadcast until
        // aborted, so without this, events would remain observable after
        // cleanup returns and the manager would never be released. `take()`
        // makes the abort exactly-once across cleanup/rebind/drop.
        let forwarder_task = self.inner.workflow_events.lock().take();
        if let Some(task) = forwarder_task {
            task.abort();
            let _ = task.await;
        }
        let workflow_manager = self.inner.workflow_manager.lock().take();
        let workflow_factory = self
            .inner
            .workflow_runtime_factory
            .lock()
            .as_ref()
            .and_then(Weak::upgrade);
        self.inner.workflow_runtime_factory.lock().take();
        if let Some(factory) = workflow_factory {
            factory.shutdown_all().await;
        }
        drop(workflow_manager);

        let loop_scheduler = self.inner.loop_scheduler.lock().take();
        if let Some(runtime) = loop_scheduler {
            runtime.shutdown().await;
        }
        let orchestration_forwarder = active.orchestration_events.lock().take();
        if let Some(task) = orchestration_forwarder {
            task.abort();
            let _ = task.await;
        }
        let orchestration_runtime = active.orchestration_runtime.lock().take();
        let mut orchestration_errors = Vec::new();
        if let Some(runtime) = orchestration_runtime
            && let Err(error) = runtime.shutdown().await
        {
            orchestration_errors.push(error.to_string());
        }
        for resident in self.inner.drain_resident_orchestration() {
            if let Err(error) = resident.runtime.shutdown().await {
                orchestration_errors.push(error.to_string());
            }
            drop(resident.publication);
        }

        active
            .process_manager
            .shutdown_owner(&active.process_owner_id)
            .await;
        if let Some(task) = active.process_events.lock().take() {
            task.abort();
        }
        if let Some(task) = active.session_events.lock().take() {
            task.abort();
        }
        active.session_subscription.lock().take();

        let runtime = active
            .extension_runtime
            .lock()
            .take()
            .map(|(runtime, _)| runtime);
        #[cfg(test)]
        {
            let gate = self.inner.cleanup_gate.lock().clone();
            if let Some(gate) = gate {
                self.inner
                    .cleanup_teardown_entries
                    .fetch_add(1, Ordering::Release);
                gate.enter().await;
                if gate.should_fail() {
                    orchestration_errors
                        .push("forced cleanup failure for tests".to_owned());
                }
            }
        }
        if let Some(runtime) = runtime {
            runtime.shutdown_with_reason("cleanup").await;
        }
        #[cfg(test)]
        {
            self.inner
                .cleanup_teardown_completions
                .fetch_add(1, Ordering::Release);
        }
        if orchestration_errors.is_empty() {
            Ok(())
        } else {
            bail!(
                "orchestration shutdown failed: {}",
                orchestration_errors.join("; ")
            )
        }
    }

    /// Spawns the owned teardown task outside any async fn body: the task's
    /// Send proof must not be registered inside `cleanup`'s defining scope.
    fn spawn_cleanup_flight(
        application: Application,
        sender: watch::Sender<Option<Result<(), String>>>,
    ) {
        tokio::spawn(async move {
            let settled = application.teardown().await;
            let _ = sender.send(Some(settled.map_err(|error| error.to_string())));
        });
    }

    #[cfg(test)]
    fn arm_cleanup_gate_for_test(&self) -> ApplicationCleanupTestGate {
        let gate = ApplicationCleanupTestGate::default();
        *self.inner.cleanup_gate.lock() = Some(gate.clone());
        gate
    }

    #[cfg(test)]
    fn cleanup_teardown_counts_for_test(&self) -> (usize, usize) {
        (
            self.inner.cleanup_teardown_entries.load(Ordering::SeqCst),
            self.inner
                .cleanup_teardown_completions
                .load(Ordering::SeqCst),
        )
    }

    /// Export the current live session to a self-contained HTML file.
    ///
    /// Prefers the persisted session file (preserving compaction markers)
    /// and falls back to in-memory history. Publishes an [`Exported`] event
    /// with the output path, or a [`ShareFailed`] event on error.
    pub fn export_session(&self, output: Option<PathBuf>) {
        let session = self.runtime().session();
        let events = self.inner.events.clone();
        let options = crate::export::ExportOptions::default();
        tokio::spawn(async move {
            let result = crate::export::export_live_session(&session, output.as_deref(), &options);
            match result {
                Ok(path) => {
                    let _ = events.send(ApplicationEvent::Exported {
                        path: path.display().to_string(),
                    });
                }
                Err(error) => {
                    let _ = events.send(ApplicationEvent::ShareFailed {
                        message: format!("export failed: {error}"),
                    });
                }
            }
        });
    }

    /// Encrypt the current session (JSONL branch) with `passphrase` and write
    /// `<name>.jsonl.enc` locally. See [`crate::encrypt`] for the scheme.
    ///
    /// The passphrase is never stored or logged; the plaintext JSONL is staged
    /// in the system temp dir and removed after encryption.
    pub fn share_encrypted_to_file(
        &self,
        passphrase: &str,
        output: Option<&Path>,
    ) -> Result<crate::EncryptedShareResult> {
        crate::share::encrypt_session_share_to_file(&self.runtime().session, passphrase, output)
    }

    /// Encrypt the current session to a local `.jsonl.enc` file and, when the
    /// `gh` CLI is available and authenticated, also upload the ciphertext to
    /// a secret gist (non-fatal when `gh` is missing).
    pub async fn share_session_encrypted(
        &self,
        passphrase: &str,
        output: Option<&Path>,
    ) -> Result<crate::EncryptedShareResult> {
        let generation = self.capture_generation();
        crate::share::share_session_encrypted(
            generation,
            &self.runtime().session,
            passphrase,
            output,
        )
        .await
    }
    /// Recorder-authoritative bounded snapshot for live collaboration guests
    /// (see [`Session::collab_public_snapshot`]).
    pub fn collab_public_snapshot(
        &self,
        max_entries: usize,
        max_bytes: usize,
    ) -> Result<serde_json::Value> {
        self.runtime()
            .session
            .collab_public_snapshot(max_entries, max_bytes)
    }

    /// Share the current live session to a secret GitHub gist.
    ///
    /// Exports the session to HTML, uploads it via `gh gist create` using gh's
    /// default secret visibility, and publishes a [`ShareSucceeded`] event with
    /// the viewer URL — or a [`ShareFailed`] event with an actionable error.
    pub fn share_session(&self) {
        let session = self.runtime().session();
        let generation = self.capture_generation();
        let events = self.inner.events.clone();
        // Offline contract: fail closed before spawning `gh` or touching the
        // network. `RPI_OFFLINE` (process env or session env overlay) makes
        // gist sharing deterministically unavailable, mirroring
        // `self_update`/`web_search`.
        if crate::share::share_offline_for(&session) {
            let _ = events.send(ApplicationEvent::ShareFailed {
                message: crate::share::OFFLINE_MESSAGE.to_owned(),
            });
            return;
        }
        let options = crate::export::ExportOptions::default();
        tokio::spawn(async move {
            let result = crate::share::share_session(generation, &session, &options).await;
            match result {
                Ok(share) => {
                    let _ = events.send(ApplicationEvent::ShareSucceeded {
                        url: share.viewer_url,
                    });
                }
                Err(error) => {
                    let _ = events.send(ApplicationEvent::ShareFailed {
                        message: error.to_string(),
                    });
                }
            }
        });
    }
}

impl ApplicationExtensionHost {
    fn application(&self) -> Result<Application> {
        self.application
            .upgrade()
            .map(|inner| Application { inner })
            .ok_or_else(|| anyhow!("application is shutting down"))
    }
}

/// Extension/process hosts must never observe credential-bearing model headers.
fn public_extension_model(mut model: Model) -> Model {
    model.headers = None;
    model
}

impl ExtensionActionHost for ApplicationExtensionHost {
    fn context_snapshot(&self) -> ExtensionFuture<'_, Result<ExtensionContextSnapshot>> {
        Box::pin(async move {
            let application = self.application()?;
            let state = application.state().await;
            let stats = application.session_stats();
            let project_trusted = application
                .resource_snapshot()
                .is_none_or(|snapshot| snapshot.trust.is_trusted());
            let commands = application
                .commands_catalog()
                .into_iter()
                .map(|command| ExtensionCommandDescriptor {
                    name: command.name,
                    description: command.description,
                })
                .collect();
            let all_tools = application
                .get_all_tools()
                .into_iter()
                .map(|tool| tool.name)
                .collect();
            let context_usage = stats.context_usage.map(|usage| ExtensionContextUsage {
                tokens: usage.tokens,
                context_window: usage.context_window,
                percent: usage.percent,
            });
            Ok(ExtensionContextSnapshot {
                session_name: state.session_name,
                session_id: state.session_id,
                session_file: state.session_file,
                is_idle: !state.is_streaming && !state.is_compacting,
                project_trusted,
                has_pending_messages: state.pending_message_count > 0,
                context_usage,
                active_tools: application.get_active_tool_names(),
                all_tools,
                commands,
                flag_values: std::collections::BTreeMap::new(),
                system_prompt: application.session().current_system_prompt().await,
                model: state.model.map(public_extension_model),
                thinking_level: state.thinking_level,
            })
        })
    }

    fn request(
        &self,
        _instance: ExtensionInstanceId,
        action: ExtensionRuntimeAction,
        cancellation: ExtensionCancellation,
    ) -> ExtensionFuture<'_, Result<Value>> {
        Box::pin(async move {
            let application = self.application()?;
            if cancellation.is_cancelled() {
                return Err(anyhow!("extension action was cancelled"));
            }
            match action {
                ExtensionRuntimeAction::SendMessage {
                    message,
                    delivery,
                    trigger_turn,
                } => {
                    application
                        .session()
                        .send_custom_message(
                            CustomMessage {
                                custom_type: message.custom_type,
                                content: message.content,
                                display: message.display,
                                details: message.details,
                                timestamp: pi_ai::now_millis(),
                            },
                            message_delivery(delivery),
                            trigger_turn,
                        )
                        .await?;
                    Ok(Value::Null)
                }
                ExtensionRuntimeAction::SendUserMessage { content, delivery } => {
                    application
                        .session()
                        .send_user_message(content, message_delivery(delivery))
                        .await?;
                    Ok(Value::Null)
                }
                ExtensionRuntimeAction::AppendEntry { custom_type, data } => serde_json::to_value(
                    application
                        .session()
                        .append_custom_entry(&custom_type, data)?,
                )
                .map_err(Into::into),
                ExtensionRuntimeAction::SetSessionName { name } => {
                    application.set_session_name(&name)?;
                    Ok(Value::Null)
                }
                ExtensionRuntimeAction::SetLabel { entry_id, label } => {
                    application.set_session_label(&entry_id, label.as_deref())?;
                    Ok(Value::Null)
                }
                ExtensionRuntimeAction::GetActiveTools => {
                    serde_json::to_value(application.get_active_tool_names()).map_err(Into::into)
                }
                ExtensionRuntimeAction::GetAllTools => {
                    let tools = application
                        .get_all_tools()
                        .into_iter()
                        .map(|tool| tool.name)
                        .collect::<Vec<_>>();
                    serde_json::to_value(tools).map_err(Into::into)
                }
                ExtensionRuntimeAction::SetActiveTools { tool_names } => {
                    application.set_active_tools_by_name(&tool_names).await?;
                    Ok(Value::Null)
                }
                ExtensionRuntimeAction::GetCommands => {
                    serde_json::to_value(application.commands_catalog()).map_err(Into::into)
                }
                ExtensionRuntimeAction::SetModel { model } => {
                    let canonical =
                        pi_ai::get_model(&model.provider, &model.id).ok_or_else(|| {
                            anyhow!(
                                "extension model {}/{} is not registered",
                                model.provider,
                                model.id
                            )
                        })?;
                    let _ = application.set_model_with_resolved_auth(canonical).await?;
                    Ok(Value::Bool(true))
                }
                ExtensionRuntimeAction::SetThinkingLevel { level } => {
                    let change = application.set_thinking_level(level);
                    Ok(serde_json::to_value(change)?)
                }
                ExtensionRuntimeAction::Abort => {
                    application.abort().await;
                    Ok(Value::Null)
                }
                ExtensionRuntimeAction::Shutdown => {
                    application.cleanup().await?;
                    Ok(Value::Null)
                }
                ExtensionRuntimeAction::Compact {
                    custom_instructions,
                } => {
                    serde_json::to_value(application.compact(custom_instructions.as_deref()).await?)
                        .map_err(Into::into)
                }
                ExtensionRuntimeAction::Reload => {
                    serde_json::to_value(application.reload().await?).map_err(Into::into)
                }
                ExtensionRuntimeAction::WaitForIdle => {
                    application.wait_for_idle().await;
                    Ok(Value::Null)
                }
                // GetFlag is resolved from extension registrations by the runtime
                // before reaching the action host; this arm is a compile-time
                // fallback that is never reached in practice.
                ExtensionRuntimeAction::GetFlag { .. } => Ok(Value::Null),
            }
        })
    }
}

const fn message_delivery(delivery: ExtensionMessageDelivery) -> MessageDelivery {
    match delivery {
        ExtensionMessageDelivery::Steer => MessageDelivery::Steer,
        ExtensionMessageDelivery::FollowUp => MessageDelivery::FollowUp,
        ExtensionMessageDelivery::NextTurn => MessageDelivery::NextTurn,
    }
}

impl ApplicationInner {
    fn publish(&self, event: ApplicationEvent) {
        let _ = self.events.send(event);
    }
    pub(super) fn start_todo_dag_cycle(
        &self,
        runtime: &crate::OrchestrationRuntime,
        reset_attempts: bool,
    ) -> Result<TodoDagExecutionOutcome> {
        let active = self.runtime();
        let gate = active.todo_dag.lock().reconcile_gate.clone();
        let _transaction = gate.lock();
        if active.todo_transition_active.load(Ordering::Acquire) {
            return Err(anyhow!("Todo mutation rejected during session transition"));
        }
        active
            .todo_continuation_suppressed
            .store(false, Ordering::Release);
        active.todo_resume_requested.store(false, Ordering::Release);
        active.todo_cycle_pending.store(true, Ordering::Release);
        let mut outcome = self.arm_todo_dag_locked(runtime, reset_attempts)?;
        if outcome.spawns.is_empty() {
            outcome.spawns = runtime
                .jobs(None)
                .into_iter()
                .filter(|job| {
                    job.todo_task_id.is_some()
                        && matches!(
                            job.status,
                            crate::JobStatus::Queued | crate::JobStatus::Running
                        )
                })
                .enumerate()
                .map(|(index, job)| crate::TaskSpawn {
                    index,
                    job_id: job.id,
                    agent_id: job.agent_id,
                    agent: job.agent,
                    status: job.status,
                })
                .collect();
        }
        Ok(outcome)
    }
    fn has_ready_open_todo(&self) -> bool {
        self.runtime()
            .session
            .todo_state()
            .phases
            .iter()
            .flat_map(|phase| &phase.tasks)
            .any(|task| {
                task.ready
                    && matches!(
                        task.status,
                        crate::TodoStatus::Pending | crate::TodoStatus::InProgress
                    )
            })
    }

    fn arm_todo_dag_after_mutation(&self) -> Result<()> {
        let active = self.runtime();
        let gate = active.todo_dag.lock().reconcile_gate.clone();
        let _transaction = gate.lock();
        self.arm_todo_dag_after_mutation_locked()
    }

    fn arm_todo_dag_after_mutation_locked(&self) -> Result<()> {
        let active = self.runtime();
        if active.todo_transition_active.load(Ordering::Acquire) {
            return Err(anyhow!("Todo mutation rejected during session transition"));
        }
        active
            .todo_continuation_suppressed
            .store(false, Ordering::Release);
        active.todo_resume_requested.store(false, Ordering::Release);
        let runtime = active.orchestration_runtime();
        let Some(runtime) = runtime else {
            return Ok(());
        };
        // Workflow children own their canonical Todo DAG through the
        // supervisor: a todo mutation must never silently arm DAG execution
        // (that raced the supervisor's own task-tool delegation, executing
        // every ready task twice). The supervisor actor drives
        // DAG execution explicitly (start/resume/restore continuation).
        if runtime.workflow_scope().is_some() {
            return Ok(());
        }
        if self.has_ready_open_todo() {
            active.todo_cycle_pending.store(true, Ordering::Release);
            self.arm_todo_dag_locked(&runtime, true)?;
        } else if active.todo_dag.lock().status != TodoDagExecutionStatus::Dormant {
            self.reconcile_todo_dag_locked(&runtime, true)?;
        }
        self.finish_todo_cycle_if_idle(Some(&runtime), false);
        Ok(())
    }
    /// Advisory `agent_settled` extension event for the captured generation.
    fn emit_agent_settled_for_runtime(&self, active: &runtime::ApplicationRuntime) {
        if !self.runtime.is_current(active) {
            return;
        }
        let generation = active.publication().capture();
        let Some(runtime) = generation.extensions().runtime().cloned() else {
            return;
        };
        let _ = tokio::spawn(async move {
            runtime
                .emit(ExtensionEvent::new("agent_settled", serde_json::json!({})))
                .await;
        });
    }

    /// S-RACE-4 backstop: the parent-turn terminal must never publish while
    /// steering/follow-up messages accepted during the settle window are
    /// still pending — the steering binary e2e forbids `agent_settled` with a
    /// non-empty queue. Hold the terminal while a bounded number of
    /// continuation attempts can still land; the session's settle drain
    /// already retried, so this only closes the remaining accept-vs-publish
    /// race window. Bounded: on persistent failure the terminal still
    /// publishes (never wedging the UI) after logging — it never carries a
    /// message body.
    async fn hold_terminal_until_queue_drained(
        self: &Arc<Self>,
        active: &runtime::ApplicationRuntime,
    ) {
        if !self.runtime.is_current(active) {
            return;
        }
        let session = &active.session;
        for attempt in 1..=3u32 {
            if session.pending_message_count().await == 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50 * u64::from(attempt))).await;
            let _ = session.continue_run().await;
        }
        let remaining = session.pending_message_count().await;
        if remaining > 0 {
            eprintln!(
                "application: agent_settled deferred past retries; {remaining} queued message(s) remain"
            );
        }
    }

    fn finish_parent_turn_for_runtime(&self, active: &runtime::ApplicationRuntime) {
        if !self.runtime.is_current(active) {
            return;
        }
        if !active.todo_cycle_pending.load(Ordering::Acquire) {
            self.publish(ApplicationEvent::AgentSettled);
            self.emit_agent_settled_for_runtime(active);
            return;
        }
        if active.todo_resume_requested.swap(false, Ordering::AcqRel) {
            active
                .todo_continuation_suppressed
                .store(false, Ordering::Release);
        }
        let orchestration = active.orchestration_runtime();
        if !active.todo_continuation_suppressed.load(Ordering::Acquire)
            && let Some(runtime) = orchestration.as_ref()
        {
            let result =
                if self.has_ready_open_todo() && active.todo_dag.lock().status.is_terminal() {
                    self.arm_todo_dag(runtime, true).map(|_| ())
                } else if active.todo_dag.lock().status != TodoDagExecutionStatus::Dormant {
                    self.reconcile_todo_dag(runtime, true).map(|_| ())
                } else {
                    Ok(())
                };
            if let Err(error) = result {
                self.todo_dag_failed(&error);
            }
            self.finish_todo_cycle_if_idle(Some(runtime), true);
            return;
        }
        self.finish_todo_cycle_if_idle(orchestration.as_ref(), true);
    }
    fn has_active_jobs(runtime: &crate::OrchestrationRuntime) -> bool {
        runtime.jobs(None).iter().any(|job| {
            matches!(
                job.status,
                crate::JobStatus::Queued | crate::JobStatus::Running
            )
        })
    }

    pub(super) fn todo_dag_snapshot(&self) -> TodoDagExecutionOutcome {
        let active = self.runtime();
        let coordinator = active.todo_dag.lock();
        TodoDagExecutionOutcome {
            status: coordinator.status,
            failure_reason: coordinator.failure_reason.clone(),
            spawns: Vec::new(),
        }
    }
    fn todo_dag_failed(&self, error: &anyhow::Error) {
        let active = self.runtime();
        let existing = {
            let coordinator = active.todo_dag.lock();
            (coordinator.status == TodoDagExecutionStatus::Failed)
                .then(|| coordinator.failure_reason.clone())
                .flatten()
        };
        let reason = existing.unwrap_or_else(|| TodoDagFailureReason::Reconcile {
            message: format!("Todo executor failed to reconcile authoritative DAG state: {error:#}. Resolve the reported runtime or persistence fault, then explicitly restart the Todo DAG."),
        });
        {
            let mut coordinator = active.todo_dag.lock();
            coordinator.status = TodoDagExecutionStatus::Failed;
            coordinator.failure_reason = Some(reason.clone());
            if let Err(persist_error) = active
                .session
                .persist_todo_execution(coordinator.status, coordinator.failure_reason.clone())
            {
                coordinator.failure_reason = Some(TodoDagFailureReason::Reconcile {
                    message: format!(
                        "{}; additionally failed to persist Todo DAG Failed status: {persist_error:#}",
                        reason.message()
                    ),
                });
            }
        }
        active.todo_dag_changed.notify_waiters();
        self.publish(ApplicationEvent::TodoUpdated {
            phases: active.session.todo_state().phases,
            completed_tasks: Vec::new(),
            execution_status: TodoDagExecutionStatus::Failed,
            failure_reason: Some(reason.clone()),
        });
        self.publish(ApplicationEvent::RunFailed {
            message: reason.message().to_owned(),
        });
        let runtime = active.orchestration_runtime();
        self.finish_todo_cycle_if_idle(runtime.as_ref(), false);
    }

    fn finish_todo_cycle_if_idle(
        &self,
        runtime: Option<&crate::OrchestrationRuntime>,
        parent_settled: bool,
    ) {
        let active = self.runtime();
        if !active.todo_cycle_pending.load(Ordering::Acquire) {
            return;
        }
        if !parent_settled
            && active
                .active_run
                .lock()
                .as_ref()
                .is_some_and(|run| !run.is_finished())
        {
            return;
        }
        if runtime.is_some_and(Self::has_active_jobs) {
            return;
        }
        if active.todo_continuation_suppressed.load(Ordering::Acquire) {
            let changed = {
                let mut coordinator = active.todo_dag.lock();
                if coordinator.status == TodoDagExecutionStatus::Active {
                    coordinator.status = TodoDagExecutionStatus::Dormant;
                    coordinator.failure_reason = None;
                    if let Err(persist_error) = active.session.persist_todo_execution(
                        coordinator.status,
                        coordinator.failure_reason.clone(),
                    ) {
                        coordinator.status = TodoDagExecutionStatus::Failed;
                        coordinator.failure_reason = Some(TodoDagFailureReason::Reconcile {
                            message: format!(
                                "Todo executor failed to persist Dormant status after interruption: {persist_error:#}"
                            ),
                        });
                    }
                    true
                } else {
                    false
                }
            };
            if changed {
                active.todo_dag_changed.notify_waiters();
                let outcome = self.todo_dag_snapshot();
                self.publish(ApplicationEvent::TodoUpdated {
                    phases: active.session.todo_state().phases,
                    completed_tasks: Vec::new(),
                    execution_status: outcome.status,
                    failure_reason: outcome.failure_reason,
                });
            }
        } else if active.todo_dag.lock().status == TodoDagExecutionStatus::Active {
            return;
        }
        if active.todo_cycle_pending.swap(false, Ordering::AcqRel) {
            self.publish(ApplicationEvent::AgentSettled);
            self.emit_agent_settled_for_runtime(&active);
        }
    }

    fn finish_goal_turn_for_runtime(
        &self,
        active: &runtime::ApplicationRuntime,
        parent_usage: pi_ai::Usage,
        started: Instant,
        goal_was_active: bool,
    ) {
        if active.session.goal_runtime().get().current.is_none() {
            return;
        }
        let mut tokens = usage_tokens(&parent_usage);
        let mut settled_jobs = Vec::new();
        if let Some(runtime) = active.orchestration_runtime() {
            let charged = active.charged_goal_jobs.lock();
            for job in runtime.jobs(None) {
                if !job.status.is_settled() || charged.contains(&job.id) {
                    continue;
                }
                let usage = job
                    .result
                    .as_ref()
                    .map_or(0, |result| usage_tokens(&result.usage));
                tokens = tokens.saturating_add(usage);
                settled_jobs.push(job.id);
            }
        }
        let elapsed = if goal_was_active {
            started.elapsed().as_secs()
        } else {
            0
        };
        if tokens != 0 || elapsed != 0 {
            let delta = GoalUsageDelta::new(tokens, elapsed);
            match active.session.goal_runtime().update_usage(delta) {
                Ok(_) => {
                    active.charged_goal_jobs.lock().extend(settled_jobs);
                    self.publish(ApplicationEvent::GoalUsageCharged {
                        delta,
                        state: active.session.goal_runtime().get(),
                    });
                }
                Err(error) => self.publish(ApplicationEvent::RunFailed {
                    message: format!("failed to charge goal usage: {error}"),
                }),
            }
        } else {
            active.charged_goal_jobs.lock().extend(settled_jobs);
        }
        self.publish(ApplicationEvent::GoalContinuation {
            decision: active.session.goal_runtime().continuation_decision(),
        });
    }

    fn pause_goal_after_resume(&self) -> Result<()> {
        let runtime = self.runtime().session.goal_runtime();
        let Some(goal) = runtime.get().current else {
            return Ok(());
        };
        if goal.lifecycle != crate::GoalLifecycle::Active {
            return Ok(());
        }
        runtime
            .pause_on_resume()
            .map_err(|error| anyhow!(error.to_string()))?;
        self.publish(ApplicationEvent::GoalUpdated {
            operation: "resume_safety_pause",
            state: runtime.get(),
        });
        Ok(())
    }
}

fn resume_orchestration_after_failed_restart(
    runtimes: &[crate::OrchestrationRuntime],
) -> Result<()> {
    let errors = runtimes
        .iter()
        .rev()
        .filter_map(|runtime| runtime.resume_after_failed_restart_checkpoint().err())
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    if errors.is_empty() {
        Ok(())
    } else {
        bail!(
            "orchestration checkpoint rollback failed: {}",
            errors.join("; ")
        )
    }
}

fn usage_tokens(usage: &pi_ai::Usage) -> u64 {
    [
        usage.input,
        usage.output,
        usage.cache_read,
        usage.cache_write,
    ]
    .into_iter()
    .map(|tokens| u64::try_from(tokens).unwrap_or(0))
    .fold(0, u64::saturating_add)
}

fn goal_tool_schema() -> Schema {
    let mut operation = Schema {
        schema_type: Some(Value::String("string".to_owned())),
        description: Some("Operation: get, pause, or complete".to_owned()),
        ..Schema::default()
    };
    operation.enum_values = ["get", "pause", "complete"]
        .into_iter()
        .map(|value| Value::String(value.to_owned()))
        .collect();
    let mut schema = Schema::object_ordered(vec![("op".to_owned(), operation, true)]);
    schema.additional_properties = Some(Value::Bool(false));
    schema
}
impl Drop for ApplicationInner {
    fn drop(&mut self) {
        let active = self.runtime.runtime();
        if let Some(binding) = active.goal_tool_binding.lock().take() {
            binding.unbind_ptr(self);
        }
        if let Some(binding) = active.todo_child_tool_binding.lock().take() {
            binding.unbind_ptr(self);
        }
        active
            .process_manager
            .shutdown_owner_now(&active.process_owner_id);
        if let Some(task) = active.orchestration_events.lock().take() {
            task.abort();
        }
        if let Some(task) = active.process_events.lock().take() {
            task.abort();
        }
        if let Some(task) = active.session_events.lock().take() {
            task.abort();
        }
        active.session_subscription.lock().take();
        if self.restart_checkpointed.load(Ordering::Acquire) {
            return;
        }
        // The workflow manager event forwarder also holds a strong manager
        // reference; abort it so the manager is released with the inner even
        // when `cleanup` was never called. Exactly-once with cleanup/rebind
        // via `take()`.
        if let Some(task) = self.workflow_events.lock().take() {
            task.abort();
        }
        for task in self.settings_watch.lock().drain(..) {
            task.abort();
        }

        let session = active.session.clone();
        let active_run = active.active_run.lock().take();
        let loop_scheduler = self.loop_scheduler.get_mut().take();
        let orchestration_runtime = active.orchestration_runtime.lock().take();
        let resident_orchestration = self
            .resident_orchestration
            .get_mut()
            .drain()
            .map(|(_, resident)| resident)
            .collect::<Vec<_>>();
        let extension_runtime = active
            .extension_runtime
            .lock()
            .take()
            .map(|(runtime, _)| runtime);
        let cleanup = async move {
            session.abort().await;
            if let Some(run) = active_run {
                let _ = run.await;
            } else {
                session.wait_for_idle().await;
            }
            if let Some(runtime) = loop_scheduler {
                runtime.shutdown().await;
            }
            if let Some(runtime) = orchestration_runtime {
                drop(runtime);
            }
            for resident in resident_orchestration {
                drop(resident.runtime);
                drop(resident.publication);
            }
            session.abort().await;
            session.wait_for_idle().await;
            if let Some(runtime) = extension_runtime {
                runtime.shutdown_with_reason("drop").await;
            }
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(cleanup);
        } else {
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("building application cleanup runtime");
                runtime.block_on(cleanup);
            });
        }
    }
}

struct GoalWorkPendingGuard {
    active: Arc<runtime::ApplicationRuntime>,
}

impl Drop for GoalWorkPendingGuard {
    fn drop(&mut self) {
        self.active.goal_work_pending.fetch_sub(1, Ordering::AcqRel);
        self.active.goal_work_changed.notify_waiters();
    }
}

async fn run_goal_work(
    inner: Weak<ApplicationInner>,
    active: Arc<runtime::ApplicationRuntime>,
    key: GoalWorkKey,
    turn_guard: Option<OwnedMutexGuard<()>>,
) {
    let _pending = GoalWorkPendingGuard {
        active: active.clone(),
    };
    let turn_guard = match turn_guard {
        Some(turn_guard) => turn_guard,
        None => active.turn_gate.clone().lock_owned().await,
    };
    let Some(inner) = inner.upgrade() else {
        return;
    };
    if inner.cleaned.load(Ordering::Acquire) {
        clear_goal_work_activation(&active, &key);
        return;
    }
    if !Arc::ptr_eq(&inner.runtime(), &active) {
        clear_goal_work_activation(&active, &key);
        return;
    }
    let state = active.session.goal_runtime().get();
    // A usage charge from a completed prior turn may advance the revision
    // without touching the goal's identity or lifecycle; that must not cancel
    // this scheduled continuation. The activation slot is the authority: this
    // turn runs only while it is still the scheduled work for the same active
    // goal. A lifecycle/id change (pause, complete, drop, fork, new goal,
    // budget exhaustion) advances `lifecycle_revision` and invalidates the
    // key — even across a mutation-only resume that returns the goal to
    // Active — while a usage-only revision bump keeps the key valid. A
    // superseding schedule for a newer revision replaces the slot and
    // cancels this turn.
    let scheduled = {
        let activation = active.goal_work_activation.lock();
        activation.as_ref() == Some(&key)
    };
    if !scheduled
        || !state.current.as_ref().is_some_and(|goal| {
            goal.id == key.goal_id && goal.lifecycle == crate::GoalLifecycle::Active
        })
        || state.lifecycle_revision != key.lifecycle_revision
    {
        clear_goal_work_activation(&active, &key);
        return;
    }

    inner.publish(ApplicationEvent::GoalContinuation {
        decision: active.session.goal_runtime().continuation_decision(),
    });
    let turn_id = uuid::Uuid::now_v7().to_string();
    let turn_model = active.session.model();
    if !inner.publish_turn_started(&active, &turn_id, turn_model.as_ref()) {
        clear_goal_work_activation(&active, &key);
        return;
    }
    let mut lifecycle = OuterTurnLifecycle::new(inner.clone(), active.clone(), turn_id);
    let message = Message::Custom(CustomMessage {
        custom_type: GOAL_CONTINUATION_CUSTOM_TYPE.to_owned(),
        content: "Start concrete work toward the active session goal now. Use the active goal reminder as the objective and continue through the normal Application workflow.".into(),
        display: false,
        details: Some(serde_json::json!({
            "goalId": key.goal_id,
            "revision": key.revision,
        })),
        timestamp: pi_ai::now_millis(),
    });
    let started = Instant::now();
    let (status, terminal_message) = match active.session.run_messages(vec![message]).await {
        Ok(result) => {
            inner.finish_goal_turn_for_runtime(&active, result.usage, started, true);
            match result.stop_reason {
                pi_ai::StopReason::Aborted => (TurnTerminalStatus::Aborted, result.error_message),
                pi_ai::StopReason::Error => (TurnTerminalStatus::Failed, result.error_message),
                _ => (TurnTerminalStatus::Succeeded, None),
            }
        }
        Err(error) => {
            clear_goal_work_activation(&active, &key);
            inner.publish(ApplicationEvent::RunFailed {
                message: error.to_string(),
            });
            inner.finish_goal_turn_for_runtime(&active, pi_ai::Usage::default(), started, true);
            (TurnTerminalStatus::Failed, Some(error.to_string()))
        }
    };
    if let Some(terminal) = lifecycle.commit(status, terminal_message) {
        inner.emit_turn_terminal(terminal);
    }
    inner.hold_terminal_until_queue_drained(&active).await;
    inner.finish_parent_turn_for_runtime(&active);
    drop(turn_guard);
}

fn clear_goal_work_activation(active: &runtime::ApplicationRuntime, key: &GoalWorkKey) {
    let mut activation = active.goal_work_activation.lock();
    if activation.as_ref() == Some(key) {
        *activation = None;
    }
}

async fn run_loop_turn(
    inner: std::sync::Weak<ApplicationInner>,
    request: crate::LoopRunRequest,
    cancel: tokio_util::sync::CancellationToken,
) -> std::result::Result<(), String> {
    let Some(inner) = inner.upgrade() else {
        return Err("application stopped".to_owned());
    };
    let active = inner.runtime();
    request.report(crate::LoopRunState::Queued);
    let turn_guard = tokio::select! {
        _ = cancel.cancelled() => return Err("loop cancelled".to_owned()),
        guard = active.turn_gate.clone().lock_owned() => guard,
    };
    if cancel.is_cancelled() {
        return Err("loop cancelled".to_owned());
    }
    request.report(crate::LoopRunState::Started);
    let session = active.session.clone();
    let turn_id = uuid::Uuid::now_v7().to_string();
    let turn_model = session.model();
    if !inner.publish_turn_started(&active, &turn_id, turn_model.as_ref()) {
        return Err("session changed before loop turn admission completed".to_owned());
    }
    let mut lifecycle = OuterTurnLifecycle::new(inner.clone(), active.clone(), turn_id);
    let loop_message = Message::Custom(pi_ai::CustomMessage {
        custom_type: crate::LOOP_SCHEDULED_MESSAGE_TYPE.to_owned(),
        content: request.model_prompt.into(),
        display: false,
        details: Some(serde_json::json!({
            "taskId": request.task_id,
            "prompt": request.prompt,
            "schedule": request.human_schedule,
        })),
        timestamp: pi_ai::now_millis(),
    });
    let run = session.run_messages(vec![loop_message]);
    tokio::pin!(run);
    let result = tokio::select! {
        result = &mut run => result.map(|_| ()).map_err(|error| error.to_string()),
        _ = cancel.cancelled() => {
            // Cancellation won the race: abort the session's in-flight run,
            // then bound the settle so a stalled provider cannot hold the
            // loop's ActiveRun slot (and the parent turn) forever. The
            // iteration token is already cancelled, so the scheduler classifies
            // every outcome here as `RunResult::Cancelled` — never
            // `LoopEvent::Failed` — whether the run settles cleanly, errors
            // with the abort, or is dropped on grace expiry. Dropping the
            // pinned run leaves Session/ClaimedRun cleanup to its existing
            // Drop impl, which schedules the run-slot release after the agent
            // goes idle.
            session.abort().await;
            let _ = tokio::time::timeout(LOOP_CANCEL_SETTLE_GRACE, &mut run).await;
            Err("loop cancelled".to_owned())
        }
    };
    let (status, terminal_message) = match &result {
        Ok(()) => (TurnTerminalStatus::Succeeded, None),
        Err(error) if error == "loop cancelled" => {
            (TurnTerminalStatus::Aborted, Some(error.clone()))
        }
        Err(error) => (TurnTerminalStatus::Failed, Some(error.clone())),
    };
    if let Some(terminal) = lifecycle.commit(status, terminal_message) {
        inner.emit_turn_terminal(terminal);
    }
    active.loop_turn_active.store(false, Ordering::Release);
    inner.hold_terminal_until_queue_drained(&active).await;
    inner.finish_parent_turn_for_runtime(&active);
    drop(turn_guard);
    result
}

fn process_event_owner(event: &ProcessEvent) -> &ProcessOwnerId {
    match event {
        ProcessEvent::ProcessStarted { process } | ProcessEvent::ProcessExited { process } => {
            &process.owner_id
        }
        ProcessEvent::ProcessOutput { owner_id, .. } => owner_id,
    }
}

fn session_extension_event(event: &SessionEvent) -> Option<ExtensionEvent> {
    let (name, data) = match event {
        SessionEvent::CompactionStart { .. } => return None,
        SessionEvent::CompactionEnd {
            reason,
            result: Some(result),
            aborted: false,
            will_retry,
            ..
        } => (
            "session_compact",
            serde_json::json!({
                "result": result,
                "fromExtension": false,
                "reason": reason,
                "willRetry": will_retry,
            }),
        ),
        SessionEvent::SessionInfoChanged { name } => {
            ("session_info_changed", serde_json::json!({ "name": name }))
        }
        SessionEvent::ModelSelect { model } => (
            "model_select",
            serde_json::json!({ "model": public_extension_model(model.clone()) }),
        ),
        SessionEvent::ThinkingLevelSelect { thinking_level } => (
            "thinking_level_select",
            serde_json::json!({ "thinkingLevel": thinking_level }),
        ),
        _ => return None,
    };
    Some(ExtensionEvent::new(name, data))
}

fn agent_extension_event(event: &AgentEvent) -> Option<ExtensionEvent> {
    let data = serde_json::to_value(event).ok()?;
    let name = data.get("type")?.as_str()?.to_owned();
    Some(ExtensionEvent::new(name, data))
}

fn extension_load_report_error(
    operation: &str,
    report: &crate::ExtensionLoadReport,
) -> anyhow::Error {
    anyhow!(
        "{operation} rejected {} extension(s): {}",
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

fn prompt_text(messages: &[Message]) -> String {
    messages
        .iter()
        .find_map(|message| match message {
            Message::User(user) => Some(
                user.content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

fn prompt_images(messages: &[Message]) -> Vec<ContentBlock> {
    messages
        .iter()
        .find_map(|message| match message {
            Message::User(user) => Some(
                user.content
                    .iter()
                    .filter(|block| matches!(block, ContentBlock::Image { .. }))
                    .cloned()
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

fn extension_custom_message(message: crate::ExtensionCustomMessage) -> Message {
    Message::Custom(CustomMessage {
        custom_type: message.custom_type,
        content: message.content,
        display: message.display,
        details: message.details,
        timestamp: pi_ai::now_millis(),
    })
}

fn extension_usage(usage: crate::ExtensionUsage) -> pi_ai::Usage {
    pi_ai::Usage {
        input: usage.input,
        output: usage.output,
        cache_read: usage.cache_read,
        cache_write: usage.cache_write,
        cache_write_1h: usage.cache_write_1h,
        reasoning: usage.reasoning,
        total_tokens: usage.total_tokens,
        ..pi_ai::Usage::default()
    }
}

const fn streaming_behavior_name(behavior: StreamingBehavior) -> &'static str {
    match behavior {
        StreamingBehavior::Steer => "steer",
        StreamingBehavior::FollowUp => "followUp",
    }
}

fn user_message(text: String, images: Vec<ContentBlock>) -> Message {
    let mut content = Vec::with_capacity(images.len() + usize::from(!text.is_empty()));
    if !text.is_empty() {
        content.push(ContentBlock::text(text));
    }
    content.extend(images);
    Message::User(pi_ai::UserMessage {
        content,
        timestamp: pi_ai::now_millis(),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        Application, ApplicationEvent, GoalToolBinding, PreparedGoalToolBinding,
        durable_resume_error, durable_resume_notice, public_extension_model,
    };
    use anyhow::Context as _;
    use futures_util::FutureExt;
    use pi_ai::{
        ContentBlock, Model,
        providers::{
            FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
        },
    };
    use std::{
        collections::HashMap,
        ops::Deref,
        path::{Path, PathBuf},
    };

    struct ApplicationFixture {
        agent: tempfile::TempDir,
        cwd: tempfile::TempDir,
        _faux: FauxProviderRegistration,
        application: Application,
        loop_runtime: Option<tokio::runtime::Runtime>,
    }

    impl Deref for ApplicationFixture {
        type Target = Application;

        fn deref(&self) -> &Self::Target {
            &self.application
        }
    }

    #[test]
    fn public_extension_model_strips_headers_and_keeps_identity() {
        let secret = "Bearer unit-model-header-secret";
        let model = Model {
            id: "sanitized-id".to_owned(),
            name: "Sanitized Model".to_owned(),
            api: "openai-completions".to_owned(),
            provider: "fixture".to_owned(),
            headers: Some(HashMap::from([
                ("Authorization".to_owned(), secret.to_owned()),
                ("X-Probe".to_owned(), "unit-probe-secret".to_owned()),
            ])),
            ..Model::default()
        };
        let public = public_extension_model(model.clone());
        assert!(public.headers.is_none());
        assert_eq!(public.id, model.id);
        assert_eq!(public.name, model.name);
        assert_eq!(public.provider, model.provider);
        assert_eq!(public.api, model.api);
        let encoded = serde_json::to_string(&public).expect("serialize sanitized model");
        assert!(!encoded.contains(secret));
        assert!(!encoded.contains("unit-probe-secret"));
    }

    #[test]
    fn failed_prepared_goal_binding_rolls_back_for_retry_while_owner_is_retained() {
        let binding = GoalToolBinding::default();
        let first = resource_application("{}");
        let second = resource_application("{}");

        let prepared = PreparedGoalToolBinding::new(binding.clone(), &first.application)
            .expect("bind first candidate");
        let retained = first.application.clone();
        drop(prepared);
        let retry = PreparedGoalToolBinding::new(binding, &second.application)
            .expect("failed candidate binding must not block retry");
        assert!(std::sync::Arc::ptr_eq(
            &retained.inner,
            &first.application.inner
        ));
        retry.commit();
    }

    #[test]
    fn stale_prepared_goal_binding_never_unbinds_new_owner() {
        let binding = GoalToolBinding::default();
        let first = resource_application("{}");
        let second = resource_application("{}");
        let stale = PreparedGoalToolBinding::new(binding.clone(), &first.application)
            .expect("bind stale candidate");

        binding.unbind(&std::sync::Arc::downgrade(&first.application.inner));
        binding
            .bind(&second.application)
            .expect("bind current owner");
        drop(stale);

        let occupied = match PreparedGoalToolBinding::new(binding, &first.application) {
            Ok(_) => panic!("stale guard must not release the current owner"),
            Err(error) => error,
        };
        assert!(
            occupied
                .to_string()
                .contains("goal tool is already attached to an application"),
            "unexpected binding error: {occupied:#}",
        );
    }

    fn isolated_settings(global_settings: &str) -> String {
        let mut settings: serde_json::Value =
            serde_json::from_str(global_settings).expect("global settings json");
        settings
            .as_object_mut()
            .expect("global settings object")
            .entry("sandbox".to_owned())
            .or_insert_with(|| serde_json::json!({ "mode": "danger-full-access" }));
        serde_json::to_string(&settings).expect("serialize isolated settings")
    }

    fn isolated_application_parts(
        global_settings: &str,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        FauxProviderRegistration,
        crate::ResourceManager,
        crate::Session,
    ) {
        isolated_application_parts_inner(global_settings, None, None)
    }

    fn isolated_application_parts_inner(
        global_settings: &str,
        auth_manager: Option<std::sync::Arc<crate::AuthManager>>,
        stream_fn: Option<pi_agent::StreamFn>,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        FauxProviderRegistration,
        crate::ResourceManager,
        crate::Session,
    ) {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        let session_dir = agent.path().join("sessions");
        std::fs::create_dir_all(&session_dir).expect("session dir");
        std::fs::write(
            agent.path().join("settings.json"),
            isolated_settings(global_settings),
        )
        .expect("global settings");

        let mut resource_options = crate::ResourceManagerOptions::new(cwd.path());
        resource_options.agent_dir = agent.path().to_path_buf();
        resource_options.project_trust_override = Some(true);
        resource_options.disable_extensions = true;
        resource_options.disable_skills = true;
        resource_options.disable_prompt_templates = true;
        resource_options.disable_themes = true;
        resource_options.disable_context_files = true;
        let resources = crate::ResourceManager::new(resource_options).expect("resources");

        let suffix = uuid::Uuid::now_v7();
        let api = format!("application-test-api-{suffix}");
        let provider = format!("application-test-provider-{suffix}");
        let model = Model {
            id: format!("application-test-model-{suffix}"),
            name: "Application Test Model".to_owned(),
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
        faux.set_responses(vec![FauxResponse::text("ok")]);

        let workspace =
            crate::WorkspaceRoots::new(cwd.path(), Vec::<PathBuf>::new()).expect("workspace");
        let session =
            crate::Session::new_with_additional_tools_filtered_discovery_workspace_and_uri(
                crate::SessionOptions {
                    model,
                    cwd: cwd.path().to_path_buf(),
                    system_prompt: String::new(),
                    thinking_level: pi_agent::ThinkingLevel::Off,
                    api_key: "test".to_owned(),
                    compaction: None,
                    stream_options: Default::default(),
                    tools: Some(Vec::new()),
                    before_tool_call: None,
                    after_tool_call: None,
                    stream_fn,
                    auth_resolver: None,
                    auth_manager,
                },
                Vec::new(),
                crate::ToolSelection::default(),
                crate::ResourceDiscovery::Disabled,
                workspace,
                None,
            )
            .expect("session");
        session.set_session_dir(session_dir);
        (agent, cwd, faux, resources, session)
    }

    /// [`isolated_application_parts`] with an injected auth manager so the
    /// fail-closed `bind_auth_manager` guard can be driven from construction
    /// (the manager binds whichever owner the session carries).
    fn isolated_application_parts_with_auth(
        global_settings: &str,
        auth_manager: Option<std::sync::Arc<crate::AuthManager>>,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        FauxProviderRegistration,
        crate::ResourceManager,
        crate::Session,
    ) {
        let (agent, cwd, faux, resources, session) =
            isolated_application_parts_inner(global_settings, auth_manager, None);
        (agent, cwd, faux, resources, session)
    }
    fn isolated_application_parts_with_stream(
        global_settings: &str,
        stream_fn: pi_agent::StreamFn,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        FauxProviderRegistration,
        crate::ResourceManager,
        crate::Session,
    ) {
        isolated_application_parts_inner(global_settings, None, Some(stream_fn))
    }

    fn resource_application(global_settings: &str) -> ApplicationFixture {
        let (agent, cwd, faux, resources, session) = isolated_application_parts(global_settings);
        let runtime = current_thread_runtime();
        let application = runtime.block_on(async {
            session
                .attach_resources(resources)
                .await
                .expect("attach resources");
            Application::from_runtime_candidate(crate::ApplicationRuntimeCandidate::new(session))
                .await
                .expect("publish application")
        });
        ApplicationFixture {
            agent,
            cwd,
            _faux: faux,
            application,
            loop_runtime: None,
        }
    }

    fn candidate_application(binding: GoalToolBinding) -> ApplicationFixture {
        let (agent, cwd, faux, resources, session) = isolated_application_parts("{}");
        let runtime = current_thread_runtime();
        let application = runtime.block_on(async {
            session
                .attach_resources(resources)
                .await
                .expect("attach resources");
            Application::from_runtime_candidate(
                crate::ApplicationRuntimeCandidate::new(session).with_goal_tool(binding),
            )
            .await
            .expect("publish application with goal tool")
        });
        ApplicationFixture {
            agent,
            cwd,
            _faux: faux,
            application,
            loop_runtime: None,
        }
    }

    #[test]
    fn non_durable_constructor_stores_goal_binding_and_retries_after_failure() {
        let binding = GoalToolBinding::default();
        let application = candidate_application(binding.clone());

        let error = application
            .attach_goal_tool(GoalToolBinding::default())
            .expect_err("constructor binding must occupy the runtime seat");
        assert!(
            error
                .to_string()
                .contains("application goal tool is already configured"),
            "unexpected attach error: {error:#}",
        );

        let (failed_agent, failed_cwd, failed_faux, resources, failed_session) =
            isolated_application_parts("{}");
        current_thread_runtime()
            .block_on(failed_session.attach_resources(resources))
            .expect("attach failed-candidate resources");
        let retained_failed_session = failed_session.clone();
        let error = current_thread_runtime()
            .block_on(Application::from_runtime_candidate(
                crate::ApplicationRuntimeCandidate::new(failed_session)
                    .with_goal_tool(binding.clone()),
            ))
            .err()
            .expect("a live application must make the constructor fail");
        assert!(
            error
                .to_string()
                .contains("goal tool is already attached to an application"),
            "unexpected constructor error: {error:#}",
        );
        let _retained_failed_candidate = (
            failed_agent,
            failed_cwd,
            failed_faux,
            retained_failed_session,
        );

        current_thread_runtime().block_on(application.cleanup());
        let (agent, cwd, faux, resources, session) = isolated_application_parts("{}");
        current_thread_runtime()
            .block_on(session.attach_resources(resources))
            .expect("attach retry resources");
        let retry = current_thread_runtime()
            .block_on(Application::from_runtime_candidate(
                crate::ApplicationRuntimeCandidate::new(session).with_goal_tool(binding),
            ))
            .expect("same binding must succeed on constructor retry after owner shutdown");
        let _retained_retry_fixture = (agent, cwd, faux, retry);
    }

    #[test]
    fn failed_attach_does_not_populate_runtime_seat_or_disturb_existing_owner() {
        let occupied = GoalToolBinding::default();
        let owner = resource_application("{}");
        let target = resource_application("{}");
        let prepared = PreparedGoalToolBinding::new(occupied.clone(), &owner.application)
            .expect("bind existing owner");

        let error = target
            .attach_goal_tool(occupied.clone())
            .expect_err("occupied binding must fail to attach");
        assert!(
            error
                .to_string()
                .contains("goal tool is already attached to an application"),
            "unexpected attach error: {error:#}",
        );
        assert!(
            target.runtime().goal_tool_binding().is_none(),
            "failed attach must not populate the runtime seat",
        );
        let tool = occupied.tool();
        let result = current_thread_runtime().block_on(async {
            let (_controller, abort) = pi_agent::AbortController::new();
            (tool.execute)(pi_agent::ToolCallContext {
                tool_call_id: "goal-get-after-failed-attach".to_owned(),
                arguments: serde_json::json!({ "op": "get" }),
                on_update: std::sync::Arc::new(|_: pi_agent::AgentToolResult| {}),
                abort,
                model: None,
                approval_token: None,
            })
            .await
            .expect("existing owner must still execute goal get")
        });
        let state: crate::GoalState =
            serde_json::from_value(result.details).expect("goal get details must decode");
        assert_eq!(state, owner.goal_state());

        target
            .attach_goal_tool(GoalToolBinding::default())
            .expect("failed attach must leave the runtime seat available");
        let still_occupied =
            match PreparedGoalToolBinding::new(prepared.binding.clone(), &target.application) {
                Ok(_) => panic!("failed attach must not unbind the existing owner"),
                Err(error) => error,
            };
        assert!(
            still_occupied
                .to_string()
                .contains("goal tool is already attached to an application"),
            "unexpected owner error: {still_occupied:#}",
        );
        prepared.commit();
    }

    #[test]
    fn settings_apply_dispatches_live_reload_and_restart_truthfully() {
        let application = resource_application("{}");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");

        let mut live = application
            .settings_draft(crate::SettingsScope::Global)
            .expect("live draft");
        live.set("compaction.enabled", serde_json::json!(false))
            .expect("live setting");
        let live = runtime
            .block_on(application.apply_settings_draft(live))
            .expect("live apply");
        assert!(live.applied_live);
        assert!(!live.reloaded);
        assert!(!live.restart_required);
        assert!(!application.runtime_settings().compaction.enabled);

        let mut reload = application
            .settings_draft(crate::SettingsScope::Global)
            .expect("reload draft");
        reload
            .set("enableSkillCommands", serde_json::json!(false))
            .expect("reload setting");
        let reload = runtime
            .block_on(application.apply_settings_draft(reload))
            .expect("reload apply");
        assert!(reload.applied_live);
        assert!(reload.reloaded);
        assert!(!reload.restart_required);

        let generation = application.resource_generation().expect("generation");
        let mut restart = application
            .settings_draft(crate::SettingsScope::Global)
            .expect("restart draft");
        restart
            .set("defaultThinkingLevel", serde_json::json!("max"))
            .expect("restart setting");
        let restart = runtime
            .block_on(application.apply_settings_draft(restart))
            .expect("restart apply");
        assert!(!restart.applied_live);
        assert!(!restart.reloaded);
        assert!(restart.restart_required);
        assert_eq!(application.resource_generation(), Some(generation));
        let mut mixed = application
            .settings_draft(crate::SettingsScope::Global)
            .expect("mixed draft");
        mixed
            .set("defaultProvider", serde_json::json!("anthropic"))
            .expect("restart provider");
        mixed
            .set("defaultModel", serde_json::json!("claude-sonnet-4-5:high"))
            .expect("restart model");
        mixed
            .set("compaction.enabled", serde_json::json!(true))
            .expect("live field");
        let mixed = runtime
            .block_on(application.apply_settings_draft(mixed))
            .expect("mixed apply");
        assert!(mixed.applied_live);
        assert!(!mixed.reloaded);
        assert!(mixed.restart_required);
        assert!(application.runtime_settings().compaction.enabled);

        let saved: serde_json::Value = serde_json::from_slice(
            &std::fs::read(application.agent.path().join("settings.json")).expect("settings file"),
        )
        .expect("settings json");
        assert_eq!(saved["defaultThinkingLevel"], "max");
        assert_eq!(saved["defaultProvider"], "anthropic");
        assert_eq!(saved["defaultModel"], "claude-sonnet-4-5:high");
    }

    #[test]
    fn responses_stateful_chain_setting_reaches_runtime_and_session_stream_options() {
        // Regression: commit_runtime_settings reported success while the live
        // session kept the old chain mode (responses_stateful_chain was never
        // copied into the session stream options).
        let application = resource_application("{}");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let mut draft = application
                .settings_draft(crate::SettingsScope::Global)
                .expect("draft");
            draft
                .set("responsesStatefulChain", serde_json::json!(true))
                .expect("set chain true");
            let applied = application
                .apply_settings_draft(draft)
                .await
                .expect("apply");
            assert!(applied.applied_live, "chain flag is a live setting");
            assert!(
                application
                    .runtime_settings()
                    .stream_options
                    .responses_stateful_chain,
                "runtime settings must carry the enabled chain flag"
            );
            assert!(
                application
                    .runtime()
                    .session
                    .stream_options()
                    .responses_stateful_chain,
                "the live session stream options must enable the chain"
            );

            let mut draft = application
                .settings_draft(crate::SettingsScope::Global)
                .expect("draft");
            draft
                .set("responsesStatefulChain", serde_json::json!(false))
                .expect("set chain false");
            let applied = application
                .apply_settings_draft(draft)
                .await
                .expect("apply");
            assert!(applied.applied_live);
            assert!(
                !application
                    .runtime_settings()
                    .stream_options
                    .responses_stateful_chain,
                "runtime settings must clear the chain flag"
            );
            assert!(
                !application
                    .runtime()
                    .session
                    .stream_options()
                    .responses_stateful_chain,
                "the live session stream options must clear the chain flag"
            );
        });
    }

    #[test]
    fn failed_settings_reload_never_claims_live_application() {
        let application = resource_application("{}");
        let generation = application.resource_generation();
        // The draft must be obtained while project settings are still valid;
        // corrupting the file first would fail draft creation instead.
        let mut draft = application
            .settings_draft(crate::SettingsScope::Global)
            .expect("draft");
        draft
            .set("enableSkillCommands", serde_json::json!(false))
            .expect("setting");
        // Corrupt the project settings only after the draft exists so the
        // failure lands in reload rather than draft creation.
        let project_settings = application.cwd.path().join(".rpi/settings.json");
        std::fs::create_dir_all(project_settings.parent().expect("settings parent"))
            .expect("project settings dir");
        std::fs::write(&project_settings, "{ malformed").expect("broken project settings");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let error = runtime
            .block_on(application.apply_settings_draft(draft))
            .expect_err("reload must fail");
        assert!(!error.to_string().is_empty());
        assert_eq!(application.resource_generation(), generation);
    }

    fn todo_routing_task_definition() -> crate::AgentDefinition {
        crate::AgentDefinition {
            name: "task".to_owned(),
            description: "background task".to_owned(),
            system_prompt: "prompt".to_owned(),
            tools: None,
            autoload_skills: Vec::new(),

            model: None,
            thinking_level: None,
            max_turns: None,
            max_tool_calls: None,
            timeout_secs: None,
            disallowed_tools: Vec::new(),
            capability_ceiling: None,
            source: crate::AgentDefinitionSource::Bundled,
            path: None,
            trusted: true,
            kind: crate::AgentDefinitionKind::Agent,
            personality: None,
            soft_budget: None,
            knowledge_roots: Vec::new(),
            user_proxy: false,
        }
    }
    fn mention_test_definition(
        name: &str,
        kind: crate::AgentDefinitionKind,
    ) -> crate::AgentDefinition {
        crate::AgentDefinition {
            name: name.to_owned(),
            description: format!("{name} test persona"),
            system_prompt: format!("{name} prompt"),
            tools: None,
            autoload_skills: Vec::new(),
            model: None,
            thinking_level: None,
            max_turns: None,
            max_tool_calls: None,
            timeout_secs: None,
            disallowed_tools: Vec::new(),
            capability_ceiling: None,
            source: crate::AgentDefinitionSource::User,
            path: None,
            trusted: true,
            kind,
            personality: None,
            soft_budget: None,
            knowledge_roots: Vec::new(),
            user_proxy: false,
        }
    }

    #[tokio::test]
    async fn prompt_inner_injects_unique_agent_mention_without_rewriting_user_prompt() {
        const REQUEST: &str = "你让cj也仔细查看下设计文档";
        let captured = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<pi_ai::Context>::new()));
        let stream: pi_agent::StreamFn = {
            let captured = captured.clone();
            std::sync::Arc::new(move |model, context, _| {
                let captured = captured.clone();
                async move {
                    captured.lock().push(context);
                    let events = pi_ai::new_assistant_message_event_stream();
                    let writer = events.clone();
                    tokio::spawn(async move {
                        let mut message = pi_ai::AssistantMessage::pending(&model);
                        message.stop_reason = pi_ai::StopReason::Stop;
                        writer.end(Some(message)).await;
                    });
                    events
                }
                .boxed()
            })
        };
        let (agent, cwd, faux, resources, session) =
            isolated_application_parts_with_stream("{}", stream);
        session
            .attach_resources(resources)
            .await
            .expect("attach resources");
        session.start_new_recording().expect("record parent");
        let mut config = crate::OrchestrationConfig::new(
            crate::AgentCatalog::from_agents(vec![mention_test_definition(
                "cj",
                crate::AgentDefinitionKind::Persona,
            )]),
            cwd.path().join("artifacts"),
        );
        config.default_agent = "cj".to_owned();
        let orchestration = crate::OrchestrationRuntime::new(
            config,
            std::sync::Arc::new(|_| Box::pin(async { unreachable!() })),
        )
        .expect("orchestration");
        let application = Application::from_runtime_candidate(
            crate::ApplicationRuntimeCandidate::new(session).with_orchestration(orchestration),
        )
        .await
        .expect("application");

        application
            .prompt(REQUEST.to_owned(), Vec::new(), None)
            .await
            .expect("prompt")
            .expect("turn accepted");
        application.wait_for_idle().await;

        let captured = captured.lock();
        assert_eq!(captured.len(), 1);
        let messages = &captured[0].messages;
        let mention = messages
            .iter()
            .find_map(|message| match message {
                pi_ai::Message::User(user)
                    if user.content.iter().any(|block| {
                        matches!(block, ContentBlock::Text { text, .. }
                        if text.contains("task.agent") && text.contains("`cj`"))
                    }) =>
                {
                    Some(user)
                }
                _ => None,
            })
            .expect("hidden mention hint projected before the model turn");
        let history = application.session().history();
        let custom_index = history
            .iter()
            .position(|message| matches!(message, pi_ai::Message::Custom(custom)
                if custom.custom_type == crate::AGENT_MENTION_CUSTOM_TYPE
                    && !custom.display
                    && custom.details.as_ref().and_then(|details| details.get("agent")).and_then(serde_json::Value::as_str) == Some("cj")))
            .expect("typed hidden mention message retained in session history");
        let user_index = history
            .iter()
            .position(|message| matches!(message, pi_ai::Message::User(user)
                if user.content.iter().any(|block| matches!(block, ContentBlock::Text { text, .. } if text == REQUEST))))
            .expect("exact user prompt retained in session history");
        assert!(
            custom_index < user_index,
            "mention hint must precede the user prompt"
        );
        assert!(
            mention
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Text { text, .. }
            if text.contains("do not rephrase") && text.contains("do not substitute")))
        );
        assert_eq!(
            messages
                .iter()
                .filter(|message| matches!(message, pi_ai::Message::User(user)
                    if user.content.iter().any(|block| matches!(block, ContentBlock::Text { text, .. } if text == REQUEST))))
                .count(),
            1,
            "exact raw user prompt reaches Main unchanged",
        );
        drop(captured);
        application.cleanup().await;
        let _owners = (agent, cwd, faux);
    }

    #[tokio::test]
    async fn prompt_inner_scopes_agent_mention_hint_to_addressed_turn() {
        const ADDRESSED: &str = "你让cj也仔细查看下设计文档";
        const FOLLOW_UP: &str = "现在总结一下你的发现";
        let captured = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<pi_ai::Context>::new()));
        let stream: pi_agent::StreamFn = {
            let captured = captured.clone();
            std::sync::Arc::new(move |model, context, _| {
                let captured = captured.clone();
                async move {
                    captured.lock().push(context);
                    let events = pi_ai::new_assistant_message_event_stream();
                    let writer = events.clone();
                    tokio::spawn(async move {
                        let mut message = pi_ai::AssistantMessage::pending(&model);
                        message.stop_reason = pi_ai::StopReason::Stop;
                        writer.end(Some(message)).await;
                    });
                    events
                }
                .boxed()
            })
        };
        let (agent, cwd, faux, resources, session) =
            isolated_application_parts_with_stream("{}", stream);
        session
            .attach_resources(resources)
            .await
            .expect("attach resources");
        session.start_new_recording().expect("record parent");
        let mut config = crate::OrchestrationConfig::new(
            crate::AgentCatalog::from_agents(vec![mention_test_definition(
                "cj",
                crate::AgentDefinitionKind::Persona,
            )]),
            cwd.path().join("artifacts"),
        );
        config.default_agent = "cj".to_owned();
        let orchestration = crate::OrchestrationRuntime::new(
            config,
            std::sync::Arc::new(|_| Box::pin(async { unreachable!() })),
        )
        .expect("orchestration");
        let application = Application::from_runtime_candidate(
            crate::ApplicationRuntimeCandidate::new(session).with_orchestration(orchestration),
        )
        .await
        .expect("application");

        for request in [ADDRESSED, FOLLOW_UP] {
            application
                .prompt(request.to_owned(), Vec::new(), None)
                .await
                .expect("prompt")
                .expect("turn accepted");
            application.wait_for_idle().await;
        }

        let captured = captured.lock();
        assert_eq!(captured.len(), 2);
        let hint_count = |context: &pi_ai::Context| {
            context
                .messages
                .iter()
                .filter(|message| match message {
                    pi_ai::Message::User(user) => user.content.iter().any(|block| {
                        matches!(block, ContentBlock::Text { text, .. }
                            if text.contains("task.agent") && text.contains("`cj`"))
                    }),
                    _ => false,
                })
                .count()
        };
        assert_eq!(hint_count(&captured[0]), 1, "addressed turn gets one hint");
        assert_eq!(
            hint_count(&captured[1]),
            0,
            "historical mention must not leak into a later unaddressed turn"
        );
        drop(captured);
        application.cleanup().await;
        let _owners = (agent, cwd, faux);
    }

    #[tokio::test]
    async fn prompt_inner_injects_only_unique_agent_mention_hint() {
        for (request, expect_hint) in [
            ("你让cj也仔细查看下设计文档", true),
            ("请仔细查看下设计文档", false),
            ("你让cj和reviewer仔细查看下设计文档", false),
            // Polite explicit delegation injects the typed mention hint in
            // both orders.
            ("please cj fix the build", true),
            ("cj please fix the build", true),
            // Descriptive past-tense/emphatic subject forms and noun
            // compounds are references and stay with Main without a hint.
            ("I did use cj for this", false),
            ("she does use cj for this", false),
            ("review the run cj produced", false),
        ] {
            let captured =
                std::sync::Arc::new(parking_lot::Mutex::new(Vec::<pi_ai::Context>::new()));
            let stream: pi_agent::StreamFn = {
                let captured = captured.clone();
                std::sync::Arc::new(move |model, context, _| {
                    let captured = captured.clone();
                    async move {
                        captured.lock().push(context);
                        let events = pi_ai::new_assistant_message_event_stream();
                        let writer = events.clone();
                        tokio::spawn(async move {
                            let mut message = pi_ai::AssistantMessage::pending(&model);
                            message.stop_reason = pi_ai::StopReason::Stop;
                            writer.end(Some(message)).await;
                        });
                        events
                    }
                    .boxed()
                })
            };
            let (agent, cwd, faux, resources, session) =
                isolated_application_parts_with_stream("{}", stream);
            session
                .attach_resources(resources)
                .await
                .expect("attach resources");
            session.start_new_recording().expect("record parent");
            let mut config = crate::OrchestrationConfig::new(
                crate::AgentCatalog::from_agents(vec![
                    mention_test_definition("cj", crate::AgentDefinitionKind::Persona),
                    mention_test_definition("reviewer", crate::AgentDefinitionKind::Agent),
                ]),
                cwd.path().join("artifacts"),
            );
            config.default_agent = "reviewer".to_owned();
            let orchestration = crate::OrchestrationRuntime::new(
                config,
                std::sync::Arc::new(|_| Box::pin(async { unreachable!() })),
            )
            .expect("orchestration");
            let application = Application::from_runtime_candidate(
                crate::ApplicationRuntimeCandidate::new(session).with_orchestration(orchestration),
            )
            .await
            .expect("application");

            application
                .prompt(request.to_owned(), Vec::new(), None)
                .await
                .expect("prompt")
                .expect("turn accepted");
            application.wait_for_idle().await;
            let captured = captured.lock();
            assert_eq!(captured.len(), 1);
            let has_agent_hint = captured[0].messages.iter().any(|message| match message {
                pi_ai::Message::User(user) => user.content.iter().any(|block| match block {
                    ContentBlock::Text { text, .. } => text.contains("task.agent"),
                    _ => false,
                }),
                _ => false,
            });
            assert_eq!(
                has_agent_hint, expect_hint,
                "unexpected mention-hint state for {request:?}"
            );
            drop(captured);
            application.cleanup().await;
            let _owners = (agent, cwd, faux);
        }
    }

    struct TodoRoutingRuntimeOwners {
        agent_dirs: Vec<tempfile::TempDir>,
        session_dirs: Vec<tempfile::TempDir>,
    }

    #[derive(Clone)]
    struct TodoRoutingRuntimeFactory {
        owners: std::sync::Arc<parking_lot::Mutex<TodoRoutingRuntimeOwners>>,
    }

    impl TodoRoutingRuntimeFactory {
        fn new() -> Self {
            Self {
                owners: std::sync::Arc::new(parking_lot::Mutex::new(TodoRoutingRuntimeOwners {
                    agent_dirs: Vec::new(),
                    session_dirs: Vec::new(),
                })),
            }
        }
    }

    impl super::ApplicationRuntimeFactory for TodoRoutingRuntimeFactory {
        fn build_runtime_candidate(
            &self,
            cwd: PathBuf,
            mut options: crate::SessionOptions,
            resume: Option<crate::PreparedSessionResume>,
        ) -> super::ApplicationRuntimeFuture {
            let owners = self.owners.clone();
            Box::pin(async move {
                options.cwd.clone_from(&cwd);
                let agent_dir = tempfile::tempdir()?;
                let session_dir = tempfile::tempdir()?;
                std::fs::write(
                    agent_dir.path().join("settings.json"),
                    isolated_settings(
                        r#"{"orchestration":{"tasks":true,"todo":true},"selector":{"autoMode":"auto"}}"#,
                    ),
                )?;
                let mut resource_options = crate::ResourceManagerOptions::new(&cwd);
                resource_options.agent_dir = agent_dir.path().to_path_buf();
                resource_options.project_trust_override = Some(true);
                resource_options.disable_extensions = true;
                resource_options.disable_skills = true;
                resource_options.disable_prompt_templates = true;
                resource_options.disable_themes = true;
                resource_options.disable_context_files = true;
                let resources = crate::ResourceManager::new(resource_options)?;
                let session =
                    crate::Session::new_with_todo_and_additional_tools_filtered_and_discovery(
                        options,
                        Vec::new(),
                        crate::ToolSelection::default(),
                        crate::ResourceDiscovery::Disabled,
                    )?;
                session.set_session_dir(session_dir.path().to_path_buf());
                session.attach_resources(resources).await?;
                if let Some(resume) = resume {
                    let context = resume.build_context();
                    let recorder = resume.into_recorder()?;
                    session.resume_recording(recorder, context.messages).await?;
                }
                let mut config = crate::OrchestrationConfig::new(
                    crate::AgentCatalog::from_agents(vec![todo_routing_task_definition()]),
                    cwd.join("orchestration-artifacts"),
                );
                config.idle_ttl = None;
                let factory: crate::ChildSessionFactory = std::sync::Arc::new(|_request| {
                    Box::pin(async move { Err(anyhow::anyhow!("child factory is not exercised")) })
                });
                let orchestration = crate::OrchestrationRuntime::new(config, factory)?;
                let candidate = crate::ApplicationRuntimeCandidate::new(session)
                    .with_orchestration(orchestration);
                let mut retained = owners.lock();
                retained.agent_dirs.push(agent_dir);
                retained.session_dirs.push(session_dir);
                Ok(candidate)
            })
        }

        fn build_trusted_workflow_candidate(
            &self,
            cwd: crate::workflow_worktree::TrustedWorkflowCwd,
            options: crate::SessionOptions,
        ) -> super::ApplicationRuntimeFuture {
            self.build_runtime_candidate(cwd.path().to_path_buf(), options, None)
        }
    }

    async fn attach_todo_routing_service(
        application: &Application,
    ) -> std::sync::Arc<crate::TodoChildService> {
        let factory: std::sync::Arc<dyn super::ApplicationRuntimeFactory> =
            std::sync::Arc::new(TodoRoutingRuntimeFactory::new());
        let service = crate::TodoChildService::new();
        service
            .configure_factory(factory.clone())
            .expect("configure Todo routing factory");
        application
            .attach_runtime_factory(factory)
            .expect("attach Todo routing factory");
        application
            .attach_todo_child_client(service.client())
            .expect("attach Todo routing service");
        service
    }
    /// Application with orchestration enabled in the global settings file and
    /// a real [`crate::OrchestrationRuntime`] attached (stub child factory —
    /// the DAG-creation contract is synchronous; child turns are never run).
    fn orchestration_application(global_settings: &str) -> ApplicationFixture {
        let factory: crate::ChildSessionFactory = std::sync::Arc::new(|_request| {
            Box::pin(async move { Err(anyhow::anyhow!("child factory is not exercised")) })
        });
        orchestration_application_with_factory(global_settings, factory)
    }

    fn orchestration_application_with_factory(
        global_settings: &str,
        factory: crate::ChildSessionFactory,
    ) -> ApplicationFixture {
        let (agent, cwd, faux, resources, session) = isolated_application_parts(global_settings);
        let runtime = current_thread_runtime();
        runtime
            .block_on(session.attach_resources(resources))
            .expect("attach resources");
        session
            .start_new_recording()
            .expect("start orchestration recording");
        let task = todo_routing_task_definition();
        let mut config = crate::OrchestrationConfig::new(
            crate::AgentCatalog::from_agents(vec![task]),
            cwd.path().join("artifacts"),
        );
        config.idle_ttl = None;
        config.max_tools_per_agent = 64;
        let orchestration =
            crate::OrchestrationRuntime::new(config, factory).expect("orchestration");
        let application = runtime
            .block_on(Application::from_runtime_candidate(
                crate::ApplicationRuntimeCandidate::new(session).with_orchestration(orchestration),
            ))
            .expect("publish orchestration application");
        ApplicationFixture {
            agent,
            cwd,
            _faux: faux,
            application,
            loop_runtime: Some(runtime),
        }
    }

    #[test]
    fn orchestration_reload_candidate_keeps_persisted_preferred_agent() {
        let application = orchestration_application(
            r#"{"orchestration":{"tasks":true,"preferredAgent":"task"}}"#,
        );
        let active = application.runtime();
        let snapshot = active
            .session
            .resource_manager()
            .expect("resources")
            .snapshot();
        let settings = snapshot
            .settings
            .runtime_settings()
            .expect("runtime settings");
        let candidate = application
            .orchestration_candidate(&snapshot, &settings)
            .expect("candidate")
            .expect("orchestration candidate");
        assert_eq!(candidate.preferred_agent().as_deref(), Some("task"));
    }

    /// Wait (bounded) for the next `ModeDetected` event on the stream.
    async fn wait_for_mode_detected(
        events: &mut tokio::sync::broadcast::Receiver<ApplicationEvent>,
    ) -> Option<(crate::PromptMode, String)> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match tokio::time::timeout(remaining, events.recv()).await {
                Ok(Ok(ApplicationEvent::ModeDetected { mode, hint })) => {
                    return Some((mode, hint));
                }
                Ok(Ok(_)) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => return None,
                Err(_) => return None,
            }
        }
    }
    async fn wait_for_todo_child_routed(
        events: &mut tokio::sync::broadcast::Receiver<ApplicationEvent>,
    ) -> Option<crate::TodoChildReceipt> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match tokio::time::timeout(remaining, events.recv()).await {
                Ok(Ok(ApplicationEvent::TodoChildRouted { receipt })) => return Some(receipt),
                Ok(Ok(_)) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) | Err(_) => return None,
            }
        }
    }

    /// Wait (bounded) for the next `AgentSettled` event on the stream.
    async fn wait_for_agent_settled(
        events: &mut tokio::sync::broadcast::Receiver<ApplicationEvent>,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            match tokio::time::timeout(remaining, events.recv()).await {
                Ok(Ok(ApplicationEvent::AgentSettled)) => return true,
                Ok(Ok(_)) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => return false,
                Err(_) => return false,
            }
        }
    }

    async fn wait_for_turn_terminal(
        events: &mut tokio::sync::broadcast::Receiver<ApplicationEvent>,
        turn_id: &str,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            match tokio::time::timeout(remaining, events.recv()).await {
                Ok(Ok(ApplicationEvent::TurnTerminal {
                    turn_id: observed, ..
                })) if observed == turn_id => {
                    return true;
                }
                Ok(Ok(_)) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => return false,
                Err(_) => return false,
            }
        }
    }

    fn current_thread_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    #[test]
    fn prompt_abort_emits_one_authoritative_terminal() {
        let (agent, cwd, faux, resources, base_session) = isolated_application_parts("{}");
        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let stream_started = started.clone();
        let stream: pi_agent::StreamFn = std::sync::Arc::new(move |model, _context, options| {
            let stream_started = stream_started.clone();
            Box::pin(async move {
                let events = pi_ai::new_assistant_message_event_stream();
                let writer = events.clone();
                tokio::spawn(async move {
                    stream_started.notify_waiters();
                    options
                        .stream
                        .abort_signal
                        .expect("abort signal")
                        .cancelled()
                        .await;
                    let mut message = pi_ai::AssistantMessage::pending(&model);
                    message.stop_reason = pi_ai::StopReason::Aborted;
                    message.error_message = Some("aborted".to_owned());
                    writer
                        .push(pi_ai::AssistantMessageEvent::Error {
                            reason: pi_ai::StopReason::Aborted,
                            error: message.clone(),
                        })
                        .await;
                    writer.end(Some(message)).await;
                });
                events
            })
        });
        let options = base_session.child_session_options_snapshot();
        let session = crate::Session::new_with_additional_tools_filtered_and_discovery(
            crate::SessionOptions {
                model: options.model,
                cwd: cwd.path().to_path_buf(),
                system_prompt: String::new(),
                thinking_level: options.thinking_level,
                api_key: options.api_key,
                compaction: None,
                stream_options: options.stream_options,
                tools: Some(Vec::new()),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn: Some(stream),
                auth_resolver: options.auth_resolver,
                auth_manager: options.auth_manager,
            },
            Vec::new(),
            crate::ToolSelection::default(),
            crate::ResourceDiscovery::Disabled,
        )
        .expect("session");
        session.set_session_dir(agent.path().join("sessions"));
        current_thread_runtime().block_on(async {
            session
                .attach_resources(resources)
                .await
                .expect("attach resources");
            let application = Application::from_runtime_candidate(
                crate::ApplicationRuntimeCandidate::new(session),
            )
            .await
            .expect("publish application");
            let mut events = application.subscribe();
            let accepted = application
                .prompt("cancel this turn".to_owned(), Vec::new(), None)
                .await
                .expect("prompt")
                .expect("accepted turn");
            started.notified().await;
            application.abort().await;
            application.wait_for_idle().await;

            let mut terminals = Vec::new();
            while let Ok(event) = events.try_recv() {
                if let ApplicationEvent::TurnTerminal {
                    turn_id, status, ..
                } = event
                    && turn_id == accepted.turn_id
                {
                    terminals.push(status);
                }
            }
            assert_eq!(terminals, vec![super::TurnTerminalStatus::Aborted]);
            assert!(application.active_turn_id().is_none());
            assert!(application.active_turn_model().is_none());
            let lifecycle = application.turn_lifecycle_snapshot();
            assert_eq!(lifecycle.revision, 2);
            assert_eq!(
                lifecycle
                    .last_terminal
                    .as_ref()
                    .map(|terminal| terminal.turn_id.as_str()),
                Some(accepted.turn_id.as_str())
            );
            drop((agent, cwd, faux));
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn abort_during_in_flight_bash_kills_the_command() {
        let (agent, cwd, faux, resources, session) = isolated_application_parts("{}");
        let ready = cwd.path().join("abort-bash.ready");
        let pid_path = cwd.path().join("abort-bash.pid");
        current_thread_runtime().block_on(async {
            session
                .attach_resources(resources)
                .await
                .expect("attach resources");
            let application = Application::from_runtime_candidate(
                crate::ApplicationRuntimeCandidate::new(session),
            )
            .await
            .expect("publish application");
            let runner = application.clone();
            let run = tokio::spawn(async move {
                runner
                    .execute_bash(
                        "sh -c 'echo $$ > abort-bash.pid; printf started > abort-bash.ready; exec sleep 30'"
                            .to_owned(),
                        false,
                    )
                    .await
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !ready.exists() && std::time::Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            assert!(ready.exists(), "bash must start before abort");
            let pid: i32 = std::fs::read_to_string(&pid_path)
                .expect("pid file")
                .trim()
                .parse()
                .expect("pid");
            assert!(
                std::path::Path::new(&format!("/proc/{pid}")).exists(),
                "sleep {pid} must be live"
            );
            tokio::time::timeout(std::time::Duration::from_secs(2), application.abort())
                .await
                .expect("abort must not wait for bash to finish");
            let gone_deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while std::path::Path::new(&format!("/proc/{pid}")).exists()
                && std::time::Instant::now() < gone_deadline
            {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            assert!(
                !std::path::Path::new(&format!("/proc/{pid}")).exists(),
                "abort must kill in-flight bash {pid}"
            );
            let bash = tokio::time::timeout(std::time::Duration::from_secs(3), run)
                .await
                .expect("bash task joins")
                .expect("bash task")
                .expect("bash result");
            assert!(bash.cancelled, "aborted bash must report cancelled");
            assert!(!application.is_streaming());
            drop((agent, cwd, faux));
        });
    }

    #[test]
    fn send_queued_steering_now_prompts_after_streaming_abort() {
        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let stream_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stream_started = started.clone();
        let call_count = stream_calls.clone();
        let stream: pi_agent::StreamFn = std::sync::Arc::new(move |model, _context, options| {
            let stream_started = stream_started.clone();
            let n = call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                let events = pi_ai::new_assistant_message_event_stream();
                let writer = events.clone();
                tokio::spawn(async move {
                    if n == 0 {
                        stream_started.notify_waiters();
                        options
                            .stream
                            .abort_signal
                            .expect("abort signal")
                            .cancelled()
                            .await;
                        let mut message = pi_ai::AssistantMessage::pending(&model);
                        message.stop_reason = pi_ai::StopReason::Aborted;
                        message.error_message = Some("aborted".to_owned());
                        writer
                            .push(pi_ai::AssistantMessageEvent::Error {
                                reason: pi_ai::StopReason::Aborted,
                                error: message.clone(),
                            })
                            .await;
                        writer.end(Some(message)).await;
                        return;
                    }
                    let mut message = pi_ai::AssistantMessage::pending(&model);
                    message.stop_reason = pi_ai::StopReason::Stop;
                    writer.end(Some(message)).await;
                });
                events
            })
        });
        let (agent, cwd, faux, resources, session) =
            isolated_application_parts_with_stream("{}", stream);
        current_thread_runtime().block_on(async {
            session
                .attach_resources(resources)
                .await
                .expect("attach resources");
            let application = Application::from_runtime_candidate(
                crate::ApplicationRuntimeCandidate::new(session),
            )
            .await
            .expect("publish application");
            application
                .prompt("hold the turn".to_owned(), Vec::new(), None)
                .await
                .expect("prompt")
                .expect("accepted turn");
            tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
                .await
                .expect("first stream must start");
            application
                .steer("inject me now".to_owned(), Vec::new())
                .await
                .expect("queue steering");
            let (steering, _) = application.queued_messages().await;
            assert!(
                !steering.is_empty(),
                "steering must be queued before send-now"
            );
            let sent = tokio::time::timeout(
                std::time::Duration::from_secs(8),
                application.send_queued_steering_now(),
            )
            .await
            .expect("send-now must not hang waiting for idle")
            .expect("send now");
            assert_eq!(sent.sent, 1);
            assert!(!sent.combined);
            application.wait_for_idle().await;
            let (steering, _) = application.queued_messages().await;
            assert!(steering.is_empty(), "send-now must drain steering");
            let user_texts: Vec<String> = application
                .messages()
                .iter()
                .filter_map(|message| match message {
                    pi_ai::Message::User(user) => Some(
                        user.content
                            .iter()
                            .filter_map(|block| match block {
                                ContentBlock::Text { text, .. } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<String>(),
                    ),
                    _ => None,
                })
                .collect();
            assert!(
                user_texts.iter().any(|text| text.contains("inject me now")),
                "send-now must prompt the drained steering, got {user_texts:?}"
            );
            assert!(
                stream_calls.load(std::sync::atomic::Ordering::SeqCst) >= 2,
                "send-now must start a new prompt after abort, not re-steer into the aborted turn"
            );
            drop((agent, cwd, faux));
        });
    }

    #[test]
    fn protocol_snapshot_keeps_one_runtime_generation_across_cutover() {
        let old = resource_application("{}");
        let (next_agent, next_cwd, next_faux, next_resources, next_session) =
            isolated_application_parts(r#"{"compaction":{"enabled":false}}"#);
        current_thread_runtime().block_on(async {
            let old_runtime = old.runtime();
            let old_model = old_runtime.session.model().expect("old model");
            old_runtime.start_turn("old-turn", Some(&old_model));
            old_runtime
                .session
                .load_history(vec![super::user_message(
                    "old history".to_owned(),
                    Vec::new(),
                )])
                .await
                .expect("old history");
            old_runtime
                .session
                .set_session_name("old-session")
                .expect("old name");

            next_session
                .attach_resources(next_resources)
                .await
                .expect("attach next resources");
            let next_epoch = old.inner.runtime.next_epoch();
            let next = std::sync::Arc::new(super::runtime::ApplicationRuntime::new(
                next_epoch,
                next_session,
                None,
                None,
                None,
                None,
            ));
            let next_model = next.session.model().expect("next model");
            next.start_turn("new-turn", Some(&next_model));
            next.session
                .set_session_name("new-session")
                .expect("next name");
            old_runtime.session_subscription.lock().take();
            let stale_runtime = old.inner.runtime.replace_arc(next.clone());

            let stale_snapshot = Application::snapshot_for_runtime(old_runtime.clone()).await;
            let current_snapshot = old.protocol_snapshot().await;
            assert_eq!(stale_snapshot.epoch, super::runtime::INITIAL_RUNTIME_EPOCH);
            assert_eq!(
                stale_snapshot.state.session_name.as_deref(),
                Some("old-session")
            );
            assert_eq!(stale_snapshot.state.message_count, 1);
            assert_eq!(stale_snapshot.messages.len(), 1);
            assert_eq!(
                stale_snapshot.lifecycle.active_turn_id.as_deref(),
                Some("old-turn")
            );
            assert_eq!(
                stale_snapshot
                    .active_turn_model
                    .as_ref()
                    .map(|model| &model.id),
                Some(&old_model.id)
            );
            assert_eq!(stale_snapshot.cwd, old_runtime.session.cwd());

            assert_eq!(current_snapshot.epoch, next_epoch);
            assert_eq!(
                current_snapshot.state.session_name.as_deref(),
                Some("new-session")
            );
            assert_eq!(current_snapshot.state.message_count, 0);
            assert!(current_snapshot.messages.is_empty());
            assert_eq!(
                current_snapshot.lifecycle.active_turn_id.as_deref(),
                Some("new-turn")
            );
            assert_eq!(
                current_snapshot
                    .active_turn_model
                    .as_ref()
                    .map(|model| &model.id),
                Some(&next_model.id)
            );
            assert_eq!(current_snapshot.cwd, next.session.cwd());
            assert!(!current_snapshot.runtime_settings.compaction.enabled);

            if let Some(terminal) = old.inner.commit_turn_terminal(
                &old_runtime,
                "old-turn",
                &super::TurnTerminalStatus::Aborted,
                None,
            ) {
                old.inner.emit_turn_terminal(terminal);
            }
            let stale_lifecycle = old_runtime.turn_lifecycle_snapshot();
            assert_eq!(stale_lifecycle.active_turn_id.as_deref(), Some("old-turn"));
            assert!(stale_lifecycle.last_terminal.is_none());
            drop(stale_runtime);
            drop((next_agent, next_cwd, next_faux));
        });
    }

    #[test]
    fn stale_prompt_admission_returns_error_instead_of_silent_success() {
        let application = resource_application("{}");
        current_thread_runtime().block_on(async {
            let gate = application.arm_prompt_before_turn_start_gate_for_test();
            let prompt = tokio::spawn({
                let application = application.clone();
                async move {
                    application
                        .prompt("race".to_owned(), Vec::new(), None)
                        .await
                }
            });
            gate.wait_entered().await;
            let active = application.runtime();
            let next_epoch = application.inner.runtime.next_epoch();
            let next = std::sync::Arc::new(super::runtime::ApplicationRuntime::new(
                next_epoch,
                active.session.clone(),
                None,
                None,
                None,
                None,
            ));
            active.session_subscription.lock().take();
            application.inner.runtime.replace_arc(next);
            gate.release();
            let error = prompt
                .await
                .expect("prompt task")
                .expect_err("stale prompt must fail");
            assert!(error.to_string().contains("retry the prompt"), "{error:#}");
        });
    }

    #[test]
    fn auto_mode_suggest_publishes_hint_without_creating_todos() {
        let application = resource_application("{}");
        current_thread_runtime().block_on(async {
            let mut events = application.subscribe();
            application
                .prompt(
                    "implement a parser in src/lib.rs".to_owned(),
                    Vec::new(),
                    None,
                )
                .await
                .expect("prompt");
            let (mode, hint) = wait_for_mode_detected(&mut events)
                .await
                .expect("suggest must publish a mode hint");
            assert_eq!(mode, crate::PromptMode::CodeTask);
            assert_eq!(hint, "Detected: code task — /todo to plan");
            assert!(
                application.todo_state().phases.is_empty(),
                "suggest must not create todos"
            );
        });
    }
    #[test]
    fn checkpoint_for_restart_without_todo_service_checkpoints_own_runtime() {
        let application = orchestration_application(r#"{"orchestration":{"tasks":true}}"#);
        let runtime = application
            .orchestration_runtime()
            .expect("application orchestration runtime");
        Application::checkpoint_process_for_restart(
            std::slice::from_ref(&application.application),
            None,
        )
        .expect("application without Todo service checkpoints cleanly");
        assert_eq!(runtime.restart_checkpoint_count_for_test(), 1);
    }

    #[test]
    fn checkpoint_for_restart_todo_children_are_visited_once_without_recursion() {
        let fixture =
            orchestration_application(r#"{"orchestration":{"tasks":true,"todo":true}}"#);
        let application = fixture.application.clone();
        // Teardown joins the loop-scheduler actor spawned on the fixture's
        // runtime; drive that runtime so the join can settle.
        let runtime = fixture
            .loop_runtime
            .as_ref()
            .expect("orchestration fixture keeps the loop scheduler runtime");
        runtime.block_on(async move {
            let service = attach_todo_routing_service(&application).await;
            let first = application
                .todo_child_create(crate::TodoChildCreateRequest {
                    label: Some("first".to_owned()),
                    idempotency_key: Some("checkpoint-first".to_owned()),
                })
                .await
                .expect("create first child");
            let second = application
                .todo_child_create(crate::TodoChildCreateRequest {
                    label: Some("second".to_owned()),
                    idempotency_key: Some("checkpoint-second".to_owned()),
                })
                .await
                .expect("create second child");
            let child_runtimes = service.restart_checkpoint_runtimes_for_test();
            assert_eq!(child_runtimes.len(), 2);
            let operation = async {
                Application::checkpoint_process_for_restart(
                    std::slice::from_ref(&application),
                    Some(service.as_ref()),
                )
            };
            tokio::time::timeout(std::time::Duration::from_secs(2), operation)
                .await
                .expect("checkpoint must not recurse")
                .expect("checkpoint applications");
            for runtime in child_runtimes.into_iter().flatten() {
                assert_eq!(
                    runtime.restart_checkpoint_count_for_test(),
                    1,
                    "every child orchestration sidecar must be checkpointed once",
                );
            }
            assert_eq!(
                application
                    .orchestration_runtime()
                    .expect("parent orchestration runtime")
                    .restart_checkpoint_count_for_test(),
                1,
                "parent orchestration sidecar must be checkpointed once",
            );
            let mut identities = vec![
                first.identity.child_session_id,
                second.identity.child_session_id,
            ];
            identities.sort();
            assert_eq!(identities.len(), 2);
            service.shutdown().await.expect("shutdown Todo service");
            application.cleanup().await.expect("cleanup parent");
        });
    }

    #[test]
    fn checkpoint_for_restart_one_todo_child_persists_parent_and_child_once() {
        let fixture =
            orchestration_application(r#"{"orchestration":{"tasks":true,"todo":true}}"#);
        let application = fixture.application.clone();
        // Teardown joins the loop-scheduler actor spawned on the fixture's
        // runtime; drive that runtime so the join can settle.
        let runtime = fixture
            .loop_runtime
            .as_ref()
            .expect("orchestration fixture keeps the loop scheduler runtime");
        runtime.block_on(async move {
            let service = attach_todo_routing_service(&application).await;
            application
                .todo_child_create(crate::TodoChildCreateRequest {
                    label: Some("only".to_owned()),
                    idempotency_key: Some("checkpoint-only".to_owned()),
                })
                .await
                .expect("create child");
            let child_runtime = service
                .restart_checkpoint_runtimes_for_test()
                .into_iter()
                .next()
                .flatten()
                .expect("child orchestration runtime");
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                Application::checkpoint_process_for_restart(
                    std::slice::from_ref(&application),
                    Some(service.as_ref()),
                )
            })
            .await
            .expect("checkpoint must complete")
            .expect("checkpoint process graph");
            assert_eq!(
                application
                    .orchestration_runtime()
                    .expect("parent orchestration runtime")
                    .restart_checkpoint_count_for_test(),
                1,
            );
            assert_eq!(child_runtime.restart_checkpoint_count_for_test(), 1);
            service.shutdown().await.expect("shutdown Todo service");
            application.cleanup().await.expect("cleanup parent");
        });
    }

    #[test]
    fn cleanup_cancelled_first_waiter_joins_single_flight_teardown_exactly_once() {
        let fixture = orchestration_application(r#"{"orchestration":{"tasks":true}}"#);
        let application = fixture.application.clone();
        // Teardown joins the loop-scheduler actor spawned on the fixture's
        // runtime; drive that runtime so the join can settle.
        let runtime = fixture
            .loop_runtime
            .as_ref()
            .expect("orchestration fixture keeps the loop scheduler runtime");
        runtime.block_on(async move {
            let gate = application.arm_cleanup_gate_for_test();
            let first = tokio::spawn({
                let application = application.clone();
                async move { application.cleanup().await }
            });
            tokio::time::timeout(std::time::Duration::from_secs(5), gate.wait_entered())
                .await
                .expect("the owned teardown task parked inside cleanup");
            let second = tokio::spawn({
                let application = application.clone();
                async move { application.cleanup().await }
            });
            let third = tokio::spawn({
                let application = application.clone();
                async move { application.cleanup().await }
            });
            tokio::task::yield_now().await;
            first.abort();
            gate.release();
            for (index, waiter) in [second, third].into_iter().enumerate() {
                let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
                    .await
                    .unwrap_or_else(|_| panic!("cleanup waiter {index} joined the flight"))
                    .expect("join");
                outcome.unwrap_or_else(|error| {
                    panic!("cleanup waiter {index} observed terminal completion: {error}")
                });
            }
            assert_eq!(
                application.cleanup_teardown_counts_for_test(),
                (1, 1),
                "teardown entered and completed exactly once",
            );
            application.cleanup().await.expect("retry cleanup");
            assert_eq!(application.cleanup_teardown_counts_for_test(), (1, 1));
        });
    }

    #[test]
    fn cleanup_concurrent_callers_share_exact_failure() {
        let fixture = orchestration_application(r#"{"orchestration":{"tasks":true}}"#);
        let application = fixture.application.clone();
        // Teardown joins the loop-scheduler actor spawned on the fixture's
        // runtime; drive that runtime so the join can settle.
        let runtime = fixture
            .loop_runtime
            .as_ref()
            .expect("orchestration fixture keeps the loop scheduler runtime");
        runtime.block_on(async move {
            let gate = application.arm_cleanup_gate_for_test();
            gate.fail_on_release();
            let first = tokio::spawn({
                let application = application.clone();
                async move { application.cleanup().await }
            });
            let second = tokio::spawn({
                let application = application.clone();
                async move { application.cleanup().await }
            });
            tokio::time::timeout(std::time::Duration::from_secs(5), gate.wait_entered())
                .await
                .expect("the owned teardown task parked inside cleanup");
            tokio::task::yield_now().await;
            gate.release();
            let first = tokio::time::timeout(std::time::Duration::from_secs(5), first)
                .await
                .expect("first cleanup caller joined the flight")
                .expect("join")
                .expect_err("the forced teardown failure must surface");
            let second = tokio::time::timeout(std::time::Duration::from_secs(5), second)
                .await
                .expect("second cleanup caller joined the flight")
                .expect("join")
                .expect_err("the forced teardown failure must surface");
            assert_eq!(
                first.to_string(),
                "orchestration shutdown failed: forced cleanup failure for tests"
            );
            assert_eq!(
                first.to_string(),
                second.to_string(),
                "concurrent cleanup callers share the exact failure",
            );
            assert_eq!(
                application.cleanup_teardown_counts_for_test(),
                (1, 1),
                "teardown entered and completed exactly once",
            );
            let retry = application
                .cleanup()
                .await
                .expect_err("the terminal failure replays");
            assert_eq!(retry.to_string(), first.to_string());
            assert_eq!(application.cleanup_teardown_counts_for_test(), (1, 1));
        });
    }

    #[test]
    fn checkpoint_for_restart_workflow_and_todo_shared_graph_is_exactly_once() {
        let fixture = orchestration_application(
            r#"{"orchestration":{"tasks":true,"todo":true,"isolation":"none"}}"#,
        );
        let application = fixture.application.clone();
        let mut workflow_fixture =
            orchestration_application(r#"{"orchestration":{"tasks":true,"todo":true}}"#);
        let workflow_application = workflow_fixture.application.clone();
        // Teardown joins the loop-scheduler actor spawned on each fixture's
        // runtime: drive the parent runtime with the body and pump the
        // workflow fixture's runtime from a background driver so both joins
        // can settle.
        let runtime = fixture
            .loop_runtime
            .as_ref()
            .expect("orchestration fixture keeps the loop scheduler runtime");
        std::thread::spawn({
            let workflow_runtime = workflow_fixture
                .loop_runtime
                .take()
                .expect("orchestration fixture keeps the loop scheduler runtime");
            move || workflow_runtime.block_on(std::future::pending::<()>())
        });
        runtime.block_on(async move {
            let service = attach_todo_routing_service(&application).await;
            application
                .todo_child_create(crate::TodoChildCreateRequest {
                    label: Some("todo".to_owned()),
                    idempotency_key: Some("checkpoint-workflow-todo".to_owned()),
                })
                .await
                .expect("create Todo child");
            let workflow_root = tempfile::tempdir().expect("workflow root");
            let source = workflow_root.path().join("source");
            std::fs::create_dir_all(&source).expect("workflow source");
            let factory = application
                .setup_workflows(
                    &source,
                    workflow_root.path().join("state"),
                    workflow_root.path().join("managed"),
                )
                .await
                .expect("setup workflows");
            factory.add_restart_checkpoint_application_for_test(
                workflow_application.clone(),
            );
            let todo_runtime = service
                .restart_checkpoint_runtimes_for_test()
                .into_iter()
                .next()
                .flatten()
                .expect("Todo child orchestration runtime");
            let workflow_runtimes = factory.restart_checkpoint_runtimes_for_test();
            assert_eq!(workflow_runtimes.len(), 1);
            Application::checkpoint_process_for_restart(
                std::slice::from_ref(&application),
                Some(service.as_ref()),
            )
            .expect("checkpoint shared process graph");
            assert_eq!(
                application
                    .orchestration_runtime()
                    .expect("parent orchestration runtime")
                    .restart_checkpoint_count_for_test(),
                1,
            );
            assert_eq!(todo_runtime.restart_checkpoint_count_for_test(), 1);
            for runtime in workflow_runtimes.into_iter().flatten() {
                assert_eq!(runtime.restart_checkpoint_count_for_test(), 1);
            }
            workflow_application
                .cleanup()
                .await
                .expect("cleanup workflow fixture");
            service.shutdown().await.expect("shutdown Todo service");
            application.cleanup().await.expect("cleanup parent");
        });
    }

    #[test]
    fn checkpoint_for_restart_child_failure_does_not_freeze_later_children() {
        let fixture =
            orchestration_application(r#"{"orchestration":{"tasks":true,"todo":true}}"#);
        let application = fixture.application.clone();
        // Teardown joins the loop-scheduler actor spawned on the fixture's
        // runtime; drive that runtime so the join can settle.
        let runtime = fixture
            .loop_runtime
            .as_ref()
            .expect("orchestration fixture keeps the loop scheduler runtime");
        runtime.block_on(async move {
            let service = attach_todo_routing_service(&application).await;
            application
                .todo_child_create(crate::TodoChildCreateRequest {
                    label: Some("first".to_owned()),
                    idempotency_key: Some("checkpoint-failure-first".to_owned()),
                })
                .await
                .expect("create first child");
            application
                .todo_child_create(crate::TodoChildCreateRequest {
                    label: Some("second".to_owned()),
                    idempotency_key: Some("checkpoint-failure-second".to_owned()),
                })
                .await
                .expect("create second child");
            let applications = service.restart_checkpoint_applications_for_test();
            assert_eq!(applications.len(), 2);
            let later_child = applications[1].clone();
            let later_runtime = later_child
                .orchestration_runtime()
                .expect("later child orchestration runtime");
            assert_eq!(later_runtime.restart_checkpoint_count_for_test(), 0);
            let first_identity = std::sync::Arc::as_ptr(&applications[0].inner) as usize;
            let error = service
                .checkpoint_applications_for_restart(|child| {
                    if std::sync::Arc::as_ptr(&child.inner) as usize == first_identity {
                        anyhow::bail!("injected checkpoint failure");
                    }
                    child.checkpoint_for_restart()
                })
                .expect_err("injected child failure must surface");
            assert_eq!(
                later_runtime.restart_checkpoint_count_for_test(),
                0,
                "failure before the later child must not checkpoint or freeze it",
            );
            assert!(error.to_string().contains("injected checkpoint failure"));
            later_child
                .checkpoint_for_restart()
                .expect("unrelated child remains independently checkpointable");
            assert_eq!(later_runtime.restart_checkpoint_count_for_test(), 1);
            service.shutdown().await.expect("shutdown Todo service");
            application.cleanup().await.expect("cleanup parent");
        });
    }

    #[test]
    fn checkpoint_process_failure_resumes_earlier_runtime_for_terminal_shutdown() {
        let first_release = tokio_util::sync::CancellationToken::new();
        let release = first_release.clone();
        let first_factory: crate::ChildSessionFactory = std::sync::Arc::new(move |_request| {
            let release = release.clone();
            Box::pin(async move {
                release.cancelled().await;
                Err(anyhow::anyhow!("rollback test child released"))
            })
        });
        let mut first = orchestration_application_with_factory(
            r#"{"orchestration":{"tasks":true}}"#,
            first_factory,
        );
        let mut second = orchestration_application(r#"{"orchestration":{"tasks":true}}"#);
        // Teardown joins the loop-scheduler actor spawned on each fixture's
        // runtime: drive the first runtime with the body and pump the second
        // fixture's runtime from a background driver so both joins settle.
        std::thread::spawn({
            let second_loop_runtime = second
                .loop_runtime
                .take()
                .expect("orchestration fixture keeps the loop scheduler runtime");
            move || second_loop_runtime.block_on(std::future::pending::<()>())
        });
        let first_runtime = first
            .orchestration_runtime()
            .expect("first orchestration runtime")
            .clone();
        let second_runtime = second
            .orchestration_runtime()
            .expect("second orchestration runtime");
        second_runtime.fail_next_restart_checkpoint_for_test("injected checkpoint failure");

        let error = Application::checkpoint_process_for_restart(
            &[first.application.clone(), second.application.clone()],
            None,
        )
        .expect_err("later checkpoint failure must surface");

        assert!(
            error.to_string().contains("injected checkpoint failure"),
            "{error:#}"
        );
        assert_eq!(first_runtime.restart_checkpoint_count_for_test(), 1);
        assert!(!first_runtime.is_restart_checkpointed_for_test());
        assert!(!second_runtime.is_restart_checkpointed_for_test());
        first
            .loop_runtime
            .take()
            .expect("orchestration fixture keeps the loop scheduler runtime")
            .block_on(async {
            let spawn = first_runtime
                .spawn_tasks(
                    "Main",
                    0,
                    vec![crate::TaskItem {
                        index: 0,
                        id: "RollbackLive".to_owned(),
                        agent: "task".to_owned(),
                        assignment: "prove rollback resumed durable mutation".to_owned(),
                        ..Default::default()
                    }],
                )
                .expect("rollback must restore spawn authority")
                .remove(0);
            let running = first_runtime
                .durable_state_for_test()
                .expect("load sidecar after rollback spawn");
            let persisted = running
                .jobs
                .iter()
                .find(|job| job.id == spawn.job_id)
                .expect("spawn must persist into durable sidecar");
            assert!(!persisted.status.is_settled());
            let cancelled = first_runtime
                .cancel_jobs_result(std::slice::from_ref(&spawn.job_id))
                .expect("rollback must restore cancellation authority");
            first_release.cancel();
            assert_eq!(cancelled, [spawn.job_id.clone()]);
            let terminal = first_runtime
                .durable_state_for_test()
                .expect("load sidecar after rollback cancellation");
            let persisted = terminal
                .jobs
                .iter()
                .find(|job| job.id == spawn.job_id)
                .expect("cancelled job remains durable");
            assert_eq!(persisted.status, crate::JobStatus::Cancelled);
            assert_eq!(
                persisted
                    .result
                    .as_ref()
                    .and_then(|result| result.error.as_deref()),
                Some("task cancelled"),
            );
            first
                .application
                .cleanup()
                .await
                .expect("first terminal cleanup");
            second
                .application
                .cleanup()
                .await
                .expect("second terminal cleanup");
        });
        assert!(first_runtime.is_shutdown());
        assert!(second_runtime.is_shutdown());
    }
    #[test]
    fn auto_mode_auto_routes_code_tasks_to_one_durable_child() {
        let fixture = orchestration_application(
            r#"{"orchestration": {"tasks": true, "todo": true}, "selector": {"autoMode": "auto"}}"#,
        );
        let application = fixture.application.clone();
        // Teardown joins the loop-scheduler actor spawned on the fixture's
        // runtime; drive that runtime so the join can settle.
        let runtime = fixture
            .loop_runtime
            .as_ref()
            .expect("orchestration fixture keeps the loop scheduler runtime");
        runtime.block_on(async move {
            let service = attach_todo_routing_service(&application).await;
            let parent_history_before = application.session().history();
            let parent_model_before = application.session().model();
            let mut events = application.subscribe();

            let first = application
                .prompt(
                    "implement a parser in src/lib.rs".to_owned(),
                    Vec::new(),
                    None,
                )
                .await
                .expect("route first code task");
            assert!(
                first.is_none(),
                "durable routing must not admit a parent turn"
            );
            let (mode, hint) = wait_for_mode_detected(&mut events)
                .await
                .expect("auto mode must publish the routing hint");
            assert_eq!(mode, crate::PromptMode::CodeTask);
            assert_eq!(hint, "Detected: code task — /todo to plan");
            let routed = wait_for_todo_child_routed(&mut events)
                .await
                .expect("auto mode must publish a typed durable receipt");

            let list = application
                .todo_child_list(None)
                .await
                .expect("list routed child");
            assert_eq!(
                list.len(),
                1,
                "one natural-language task must create one child"
            );
            let child_id = list[0].child_session_id.clone();
            let status = application
                .todo_child_status(None, Some(&child_id))
                .await
                .expect("routed child status");
            assert_eq!(
                status.receipts.len(),
                1,
                "one task must yield one durable receipt"
            );
            assert_eq!(status.receipts[0].child_session_id, child_id);
            assert_eq!(routed.id, status.receipts[0].id);
            assert_eq!(routed.message_id, status.receipts[0].message_id);
            assert_eq!(routed.child_session_id, child_id);
            assert!(
                application.todo_state().phases.is_empty(),
                "parent Todo must stay unchanged"
            );
            assert!(
                application
                    .orchestration_runtime()
                    .expect("orchestration attached")
                    .jobs(None)
                    .is_empty(),
                "parent orchestration must not spawn task jobs",
            );
            let child_runtime = service
                .application_for_child(&child_id)
                .expect("selected child runtime");
            let child_turn = child_runtime
                .prompt_without_auto_todo(
                    "fix nested routing in src/child.rs".to_owned(),
                    Vec::new(),
                    None,
                )
                .await
                .expect("child-owned prompt")
                .expect("child prompt must admit its own turn");
            child_runtime.wait_for_idle().await;
            assert_eq!(
                child_runtime
                    .turn_lifecycle_snapshot()
                    .last_terminal
                    .as_ref()
                    .map(|terminal| terminal.turn_id.as_str()),
                Some(child_turn.turn_id.as_str()),
            );
            assert_eq!(
                application
                    .todo_child_list(None)
                    .await
                    .expect("list after child turn")
                    .len(),
                1,
                "child execution must not recursively create another Todo child",
            );
            assert_eq!(application.session().history(), parent_history_before);
            assert_eq!(application.session().model(), parent_model_before);
            assert!(application.active_turn_id().is_none());
            assert!(!application.is_streaming());

            let follow_up = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                application.prompt("what is rust?".to_owned(), Vec::new(), None),
            )
            .await
            .expect("parent prompt gate remained held")
            .expect("accept unrelated parent follow-up")
            .expect("question must admit a parent turn");
            application.wait_for_idle().await;
            assert_eq!(
                application
                    .turn_lifecycle_snapshot()
                    .last_terminal
                    .as_ref()
                    .map(|terminal| terminal.turn_id.as_str()),
                Some(follow_up.turn_id.as_str()),
            );

            let image = ContentBlock::Image {
                data: "aW1hZ2U=".to_owned(),
                mime_type: "image/png".to_owned(),
            };
            let with_image = application
                .prompt(
                    "update the parser in src/lib.rs".to_owned(),
                    vec![image.clone()],
                    None,
                )
                .await
                .expect("route code task with image");
            assert!(with_image.is_none());
            let status = application
                .todo_child_status(None, Some(&child_id))
                .await
                .expect("image-routed child status");
            assert_eq!(
                status.receipts.len(),
                2,
                "image task must append one receipt"
            );
            let child_path = application
                .session()
                .recorder()
                .expect("parent recorder")
                .path()
                .parent()
                .expect("parent session directory")
                .join("children")
                .join(
                    application
                        .session()
                        .recorder_info()
                        .expect("parent recorder identity")
                        .0,
                )
                .join(format!("{child_id}.jsonl"));
            let child_tree = crate::session_store::load_session_tree(&child_path)
                .expect("load routed child tree");
            let image_json = serde_json::to_value(&image).expect("image json");
            let has_image = child_tree.branch(None).iter().any(|entry| {
                entry.custom_type.as_deref() == Some("pi.todo_child.receipt")
                    && entry.data.as_ref().is_some_and(|data| {
                        data["message"]["images"]
                            .as_array()
                            .is_some_and(|images| images.contains(&image_json))
                    })
            });
            assert!(
                has_image,
                "image payload must be durable before child execution"
            );
            let second = application
                .prompt(
                    "fix the parser test in tests/parser.rs".to_owned(),
                    Vec::new(),
                    None,
                )
                .await
                .expect("route second code task");
            assert!(second.is_none());
            let list = application
                .todo_child_list(None)
                .await
                .expect("list reused child");
            assert_eq!(list.len(), 1, "later routing must reuse the selected child");
            assert_eq!(list[0].child_session_id, child_id);
            let status = application
                .todo_child_status(None, Some(&child_id))
                .await
                .expect("reused child status");
            assert_eq!(
                status.receipts.len(),
                3,
                "later routing must append one receipt"
            );
            let receipt_ids = status
                .receipts
                .iter()
                .map(|receipt| receipt.id.as_str())
                .collect::<std::collections::HashSet<_>>();
            assert_eq!(
                receipt_ids.len(),
                3,
                "every routed prompt needs a distinct receipt"
            );

            service
                .shutdown()
                .await
                .expect("shutdown Todo routing service");
            application.cleanup().await;
        });
    }

    #[test]
    fn auto_mode_auto_respects_existing_todos() {
        let fixture = orchestration_application(
            r#"{"orchestration": {"tasks": true, "todo": true}, "selector": {"autoMode": "auto"}}"#,
        );
        let application = fixture.application.clone();
        // Teardown joins the loop-scheduler actor spawned on the fixture's
        // runtime; drive that runtime so the join can settle.
        let runtime = fixture
            .loop_runtime
            .as_ref()
            .expect("orchestration fixture keeps the loop scheduler runtime");
        runtime.block_on(async move {
            let service = attach_todo_routing_service(&application).await;
            let selected = application
                .todo_child_create(crate::TodoChildCreateRequest {
                    label: Some("Selected before routing".to_owned()),
                    idempotency_key: Some("selected-before-routing".to_owned()),
                })
                .await
                .expect("create selected child");
            application
                .set_todos(vec![crate::TodoPhase {
                    name: "Existing".to_owned(),
                    tasks: vec![crate::TodoItem {
                        id: "existing-1".to_owned(),
                        content: "keep this task".to_owned(),
                        status: crate::TodoStatus::Pending,
                        depends_on: Vec::new(),
                        ready: true,
                        waiting_on: Vec::new(),
                        agent: None,
                    }],
                }])
                .expect("seed existing todos");
            let mut events = application.subscribe();
            application
                .prompt(
                    "implement a parser in src/lib.rs".to_owned(),
                    Vec::new(),
                    None,
                )
                .await
                .expect("prompt");
            let (mode, _) = wait_for_mode_detected(&mut events)
                .await
                .expect("hint still fires");
            assert_eq!(mode, crate::PromptMode::CodeTask);
            let phases = application.todo_state().phases;
            assert_eq!(phases.len(), 1, "existing todos must not be replaced");
            assert_eq!(phases[0].name, "Existing");
            assert_eq!(phases[0].tasks[0].content, "keep this task");
            let list = application
                .todo_child_list(None)
                .await
                .expect("list routed child");
            assert_eq!(
                list.len(),
                1,
                "routing must reuse the explicitly selected child"
            );
            assert_eq!(list[0].child_session_id, selected.identity.child_session_id);
            let routed = application
                .todo_child_status(None, Some(&selected.identity.child_session_id))
                .await
                .expect("selected child status");
            assert_eq!(routed.receipts.len(), 1);
            service
                .shutdown()
                .await
                .expect("shutdown Todo routing service");
            application.cleanup().await;
        });
    }

    /// Todo-management commands with code-domain payload must stay with Main:
    /// the auto-mode router must not divert them to a durable Todo child, so
    /// Main's typed `todo` tool and child lifecycle controls stay reachable.
    #[test]
    fn auto_mode_auto_keeps_todo_management_commands_with_main() {
        let fixture = orchestration_application(
            r#"{"orchestration": {"tasks": true, "todo": true}, "selector": {"autoMode": "auto"}}"#,
        );
        let application = fixture.application.clone();
        // Teardown joins the loop-scheduler actor spawned on the fixture's
        // runtime; drive that runtime so the join can settle.
        let runtime = fixture
            .loop_runtime
            .as_ref()
            .expect("orchestration fixture keeps the loop scheduler runtime");
        runtime.block_on(async move {
            let service = attach_todo_routing_service(&application).await;
            let mut events = application.subscribe();

            let turn = application
                .prompt(
                    "add task: fix the crash in the parser".to_owned(),
                    Vec::new(),
                    None,
                )
                .await
                .expect("todo-management prompt")
                .expect("todo-management command must admit a parent turn");
            application.wait_for_idle().await;
            assert_eq!(
                application
                    .turn_lifecycle_snapshot()
                    .last_terminal
                    .as_ref()
                    .map(|terminal| terminal.turn_id.as_str()),
                Some(turn.turn_id.as_str()),
                "the parent turn must run to its own terminal"
            );
            let drained = tokio::time::timeout(
                std::time::Duration::from_millis(300),
                wait_for_todo_child_routed(&mut events),
            )
            .await
            .ok()
            .flatten();
            assert!(
                drained.is_none(),
                "todo-management command must not be routed to a durable child"
            );
            assert!(
                application
                    .todo_child_list(None)
                    .await
                    .expect("list children")
                    .is_empty(),
                "no durable child may be created for a todo-management command"
            );

            // A genuine code task still routes to the durable child.
            let code_task = application
                .prompt(
                    "implement a parser in src/lib.rs".to_owned(),
                    Vec::new(),
                    None,
                )
                .await
                .expect("code-task prompt");
            assert!(
                code_task.is_none(),
                "genuine code tasks must still be routed away from Main"
            );
            let routed = wait_for_todo_child_routed(&mut events)
                .await
                .expect("code task still routes");
            assert!(
                !routed.child_session_id.is_empty(),
                "routed receipt must name the durable child"
            );
            service
                .shutdown()
                .await
                .expect("shutdown Todo routing service");
            application.cleanup().await;
        });
    }

    #[test]
    fn auto_mode_off_publishes_no_mode_event() {
        let application = resource_application(r#"{"selector": {"autoMode": "off"}}"#);
        current_thread_runtime().block_on(async {
            let mut events = application.subscribe();
            application
                .prompt(
                    "implement a parser in src/lib.rs".to_owned(),
                    Vec::new(),
                    None,
                )
                .await
                .expect("prompt");
            assert!(
                wait_for_mode_detected(&mut events).await.is_none(),
                "off must not publish mode events"
            );
            assert!(application.todo_state().phases.is_empty());
        });
    }

    #[test]
    fn question_prompt_publishes_no_mode_event() {
        let application = resource_application("{}");
        current_thread_runtime().block_on(async {
            let mut events = application.subscribe();
            application
                .prompt("what is rust?".to_owned(), Vec::new(), None)
                .await
                .expect("prompt");
            assert!(
                wait_for_mode_detected(&mut events).await.is_none(),
                "questions must not fire the classifier hint"
            );
        });
    }

    #[test]
    fn settled_turn_admits_next_prompt_as_new_run_not_steer() {
        let fixture = resource_application("{}");
        fixture._faux.append_response(FauxResponse::text("ok"));
        current_thread_runtime().block_on(async {
            let mut events = fixture.subscribe();
            let settle_gate = fixture.arm_prompt_after_settle_gate_for_test();
            let first = fixture
                .prompt("first turn".to_owned(), Vec::new(), None)
                .await
                .expect("first prompt")
                .expect("first turn accepted");
            assert!(
                wait_for_turn_terminal(&mut events, &first.turn_id).await,
                "first turn must publish TurnTerminal"
            );
            assert!(
                !fixture.is_streaming(),
                "is_streaming must be false at the earliest terminal event"
            );
            let second_prompt = fixture.prompt("second turn".to_owned(), Vec::new(), None);
            settle_gate.release();
            let second = second_prompt
                .await
                .expect("second prompt")
                .expect("next prompt after TurnTerminal must start a new run");
            assert_ne!(second.turn_id, first.turn_id);
            assert!(
                wait_for_agent_settled(&mut events).await,
                "first turn must publish AgentSettled before the successor starts"
            );
            fixture.wait_for_idle().await;
        });
    }

    fn job(id: &str, status: crate::JobStatus) -> crate::JobSnapshot {
        crate::JobSnapshot {
            id: id.to_owned(),
            agent_id: "agent".to_owned(),
            agent: "task".to_owned(),
            parent_id: "main".to_owned(),
            description: None,
            todo_task_id: None,
            workflow_id: None,
            workflow_generation: None,
            status,
            created_at: 1,
            started_at: None,
            finished_at: None,
            result: None,
            soft_budget_exhausted: false,
        }
    }

    fn workflow(name: &str, status: crate::WorkflowStatus) -> crate::WorkflowSnapshot {
        crate::WorkflowSnapshot {
            workflow_id: crate::WorkflowId::new(format!("workflow-{name}")),
            name: name.to_owned(),
            objective: "objective".to_owned(),
            status,
            created_at_ms: 1,
            updated_at_ms: 1,
            generation: 1,
            durable_policy: crate::sandbox::generation::test_durable_policy_state(),
            todo: crate::TodoState {
                phases: Vec::new(),
                storage: crate::TodoStorage::Memory,
            },
            worktree_label: None,
            branch: None,
            supervisor_agent_id: None,
            supervisor_job_id: None,
            failure: None,
            integration: crate::WorkflowIntegration::None,
            planning_prompts_consumed: 0,

        }
    }

    #[test]
    fn failed_main_delivery_ack_keeps_message_claimed() {
        let fixture = resource_application("{}");
        let message_id = "message-ack-failed";
        fixture
            .application
            .inner
            .steered_main_message_ids
            .lock()
            .insert(message_id.to_owned());
        fixture
            .application
            .inner
            .report_main_delivery_ack_failure(message_id, &anyhow::anyhow!("persist failed"));
        assert!(
            fixture
                .application
                .inner
                .main_delivery_is_claimed_for_test(message_id),
            "ack persistence failure must retain the claim so replay cannot steer a duplicate"
        );
    }

    #[test]
    fn rewind_refusal_blocks_live_orchestration_jobs_and_active_workflows() {
        // No live work: rewinding is safe.
        assert_eq!(Application::rewind_refusal(&[], &[]), None);
        assert_eq!(
            Application::rewind_refusal(
                &[job("settled", crate::JobStatus::Completed)],
                &[workflow("done", crate::WorkflowStatus::Completed)],
            ),
            None
        );

        // Queued or running orchestration jobs refuse with an actionable
        // message naming the count.
        for status in [crate::JobStatus::Queued, crate::JobStatus::Running] {
            let refusal =
                Application::rewind_refusal(&[job("live", status)], &[]).expect("refusal");
            assert!(refusal.contains("rewind refused"), "{refusal}");
            assert!(refusal.contains("orchestration job(s)"), "{refusal}");
            assert!(refusal.contains('1'), "{refusal}");
        }

        // Active workflows refuse even when no orchestration job is queued.
        for status in [
            crate::WorkflowStatus::Queued,
            crate::WorkflowStatus::Planning,
            crate::WorkflowStatus::Running,
            crate::WorkflowStatus::Paused,
            crate::WorkflowStatus::Integrating,
        ] {
            let refusal =
                Application::rewind_refusal(&[], &[workflow("live", status)]).expect("refusal");
            assert!(refusal.contains("rewind refused"), "{refusal}");
            assert!(refusal.contains("workflow(s)"), "{refusal}");
            assert!(refusal.contains("live"), "{refusal}");
        }
    }

    #[test]
    fn durable_cutover_degradation_requires_typed_error_and_single_root_cause() {
        let typed = anyhow::Error::new(crate::DurableResumeError::State(
            crate::DurableStateValidationError::Policy(
                pi_sandbox::PolicyValidationError::InvalidField {
                    field: "workspaceRoots",
                    reason: "current platform or session roots do not match persisted authority",
                },
            ),
        ))
        .context("prepare parent identity");
        assert!(durable_resume_error(&typed).is_some());
        let notice = durable_resume_notice(&typed);
        assert!(notice.contains(
            "invalid field workspaceRoots: current platform or session roots do not match persisted authority"
        ));
        assert!(!notice.contains("prepare parent identity"), "{notice}");
        assert!(
            !notice.contains("durable state validation failed"),
            "{notice}"
        );

        for message in [
            "durable orchestration policy does not match resumed generation",
            "current workspace identities do not match persisted authority",
            "current platform or session roots do not match persisted authority",
        ] {
            assert!(
                durable_resume_error(&anyhow::anyhow!(message)).is_none(),
                "message text alone must not trigger degradation: {message}"
            );
        }
    }

    #[test]
    fn live_publication_cannot_be_replaced_on_same_process_manager() {
        let first = resource_application("{}");
        let second = resource_application("{}");
        let error = first
            .process_manager()
            .bind_publication(&second.publication())
            .expect_err("live publication must stay fail-closed");
        assert!(
            error
                .to_string()
                .contains("process manager publication is already bound"),
            "unexpected live rebind error: {error:#}"
        );
        let _retained = (first, second);
    }

    #[test]
    fn auth_manager_binding_is_fail_closed() {
        let directory = tempfile::tempdir().expect("auth dir");
        let second = std::sync::Arc::new(
            crate::AuthManager::new(directory.path().join("second-auth.json"))
                .expect("second auth manager"),
        );
        let bound = std::sync::Arc::new(
            crate::AuthManager::new(directory.path().join("first-auth.json"))
                .expect("bound auth manager"),
        );
        let same_owner = bound.clone();
        let (agent, cwd, faux, resources, session) =
            isolated_application_parts_with_auth("{}", Some(bound));
        session.set_session_dir(directory.path().to_path_buf());
        let runtime = current_thread_runtime();
        let application = runtime.block_on(async {
            session
                .attach_resources(resources)
                .await
                .expect("attach resources");
            Application::from_runtime_candidate(crate::ApplicationRuntimeCandidate::new(session))
                .await
                .expect("publish application")
        });

        // Re-binding the SAME owner is idempotent (restart-safe).
        application
            .process_manager()
            .bind_auth_manager(same_owner)
            .expect("same auth owner rebind must be idempotent");
        // A DIFFERENT owner must be rejected fail-closed.
        let error = application
            .process_manager()
            .bind_auth_manager(second)
            .expect_err("different auth owner must be rejected");
        assert!(
            error
                .to_string()
                .contains("process manager authentication owner is already bound"),
            "unexpected bind error: {error:#}"
        );
        let _retained = (agent, cwd, faux);
    }

    #[cfg(unix)]
    #[test]
    fn published_generation_executes_pre_tool_hook_in_sandbox_path() {
        let application = resource_application("{}");
        let hooks = crate::HostHooks::new(
            vec![crate::HookConfig {
                event: crate::HookEvent::PreToolCall,
                matcher: Some("read".to_owned()),
                command: vec![
                    "/bin/sh".to_owned(),
                    "-c".to_owned(),
                    "printf '%s\\n' '{\"decision\":\"block\",\"reason\":\"published authority\"}'"
                        .to_owned(),
                ],
                timeout_ms: Some(2_000),
                enabled: None,
                fail_closed: Some(false),
                extra: serde_json::Map::new(),
            }],
            application.cwd.path().to_path_buf(),
            "published-hook-smoke",
            application.generation_capture_fn(),
        );
        let decision = current_thread_runtime().block_on(hooks.fire(
            crate::HookEvent::PreToolCall,
            Some("read"),
            Some(&crate::HookToolPayload {
                name: "read",
                arguments: Some(&serde_json::json!({ "path": "fixture.txt" })),
                result_text: None,
                is_error: false,
            }),
        ));

        assert!(decision.block);
        assert_eq!(decision.reason.as_deref(), Some("published authority"));
    }

    /// Builds isolated Application parts using a caller-supplied agent
    /// directory (typically a permitted terminal symlink whose target lives
    /// outside the project boundary). The session directory is created
    /// independently so retargeting the agent link cannot disturb it.
    fn isolated_application_parts_with_agent_dir(
        global_settings: &str,
        agent_dir: &Path,
        cwd: &Path,
    ) -> (
        tempfile::TempDir,
        FauxProviderRegistration,
        crate::ResourceManager,
        crate::Session,
    ) {
        std::fs::write(
            agent_dir.join("settings.json"),
            isolated_settings(global_settings),
        )
        .expect("global settings");
        let session_root = tempfile::tempdir().expect("session root");
        let session_dir = session_root.path().join("sessions");
        std::fs::create_dir_all(&session_dir).expect("session dir");

        let mut resource_options = crate::ResourceManagerOptions::new(cwd);
        resource_options.agent_dir = agent_dir.to_path_buf();
        resource_options.project_trust_override = Some(true);
        resource_options.disable_extensions = true;
        resource_options.disable_skills = true;
        resource_options.disable_prompt_templates = true;
        resource_options.disable_themes = true;
        resource_options.disable_context_files = true;
        let resources = crate::ResourceManager::new(resource_options).expect("resources");

        let suffix = uuid::Uuid::now_v7();
        let api = format!("application-test-api-{suffix}");
        let provider = format!("application-test-provider-{suffix}");
        let model = Model {
            id: format!("application-test-model-{suffix}"),
            name: "Application Test Model".to_owned(),
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
        faux.set_responses(vec![FauxResponse::text("ok")]);

        let workspace = crate::WorkspaceRoots::new(cwd, Vec::<PathBuf>::new()).expect("workspace");
        let session =
            crate::Session::new_with_additional_tools_filtered_discovery_workspace_and_uri(
                crate::SessionOptions {
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
                crate::ToolSelection::default(),
                crate::ResourceDiscovery::Disabled,
                workspace,
                None,
            )
            .expect("session");
        session.set_session_dir(session_dir);
        (session_root, faux, resources, session)
    }

    /// A retargeted terminal symlink agent root must make `Application::reload`
    /// fail closed with the fixed retained-root validation context and leave
    /// the live resource snapshot and generation untouched. The test fails if
    /// reload silently rebinds to the new target, partially applies, or
    /// mutates the active snapshot.
    #[cfg(unix)]
    #[test]
    fn reload_fails_closed_when_agent_store_symlink_retargeted_after_capture() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("temporary root");
        let first = root.path().join("first-store");
        let second = root.path().join("second-store");
        let link_store = root.path().join("agent");
        let cwd = root.path().join("project");
        std::fs::create_dir_all(&cwd).expect("project");
        std::fs::create_dir_all(&first).expect("first store");
        std::fs::create_dir_all(&second).expect("second store");
        std::fs::write(
            second.join("settings.json"),
            isolated_settings(r#"{"theme":"retargeted"}"#),
        )
        .expect("second store settings");
        // The agent store root is a permitted terminal symlink whose target
        // lives outside the project boundary (sibling of cwd, not under it).
        symlink(&first, &link_store).expect("agent root symlink");

        let (_session_root, _faux, resources, session) =
            isolated_application_parts_with_agent_dir(r#"{"theme":"captured"}"#, &link_store, &cwd);
        let application = current_thread_runtime().block_on(async {
            session
                .attach_resources(resources)
                .await
                .expect("attach resources");
            Application::from_runtime_candidate(crate::ApplicationRuntimeCandidate::new(session))
                .await
                .expect("publish application")
        });

        let snapshot = application
            .resource_snapshot()
            .expect("live resource snapshot");
        let generation = application
            .resource_generation()
            .expect("resource generation");
        assert_eq!(snapshot.settings.theme.as_deref(), Some("captured"));

        // Retarget the agent root symlink to a different store identity.
        std::fs::remove_file(&link_store).expect("remove original link");
        symlink(&second, &link_store).expect("retarget to second store");

        let error = current_thread_runtime()
            .block_on(application.reload())
            .expect_err("retargeted agent root must fail closed before reload");
        let message = format!("{error:#}");
        assert!(
            message.contains("validating retained agent resource root"),
            "reload failure must carry the retained-root validation context: {message}"
        );
        // The bounded error must not leak absolute fixture paths.
        assert!(
            !message.contains(root.path().to_str().expect("root path utf-8")),
            "error must not leak filesystem paths: {message}"
        );

        // The live snapshot and generation must be unchanged.
        assert_eq!(
            application
                .resource_generation()
                .expect("resource generation"),
            generation,
            "failed reload must not advance the resource generation"
        );
        let retained = application
            .resource_snapshot()
            .expect("retained resource snapshot");
        assert!(
            std::sync::Arc::ptr_eq(&retained, &snapshot),
            "failed reload must not replace the live resource snapshot"
        );
        assert_eq!(retained.settings.theme.as_deref(), Some("captured"));
    }

    /// Persona create/edit reloads the resource catalog on the live Tokio
    /// worker. `reload_with_candidate_trust` is a large async frame; pinning
    /// it on the heap keeps that frame off the 2 MiB worker stack.
    #[test]
    fn reload_after_persona_catalog_write_fits_tokio_worker_stack() {
        const WORKER_STACK: usize = 2 * 1024 * 1024;
        let join = std::thread::Builder::new()
            .stack_size(WORKER_STACK)
            .spawn(|| {
                let runtime = current_thread_runtime();
                runtime.block_on(async {
                    let (agent, cwd, _faux, resources, session) =
                        isolated_application_parts("{}");
                    let persona_root = agent.path().join("personas").join("guide");
                    std::fs::create_dir_all(&persona_root).expect("persona root");
                    std::fs::write(
                        persona_root.join("persona.md"),
                        "---\nname: guide\ndescription: guided assistant\ntools: [read]\n---\nguide prompt\n",
                    )
                    .expect("persona.md");
                    session
                        .attach_resources(resources)
                        .await
                        .expect("attach resources");
                    let application = Application::from_runtime_candidate(
                        crate::ApplicationRuntimeCandidate::new(session),
                    )
                    .await
                    .expect("publish application");
                    application.reload().await.expect("reload");
                    let snapshot = application.resource_snapshot().expect("snapshot");
                    let names = snapshot
                        .agents
                        .iter()
                        .map(|agent| agent.name.as_str())
                        .collect::<Vec<_>>();
                    assert!(
                        names.contains(&"guide"),
                        "reloaded catalog must include the written persona: {names:?}"
                    );
                    let _ = cwd;
                    application.cleanup().await.expect("cleanup");
                });
            })
            .expect("2 MiB worker thread")
            .join();
        assert!(
            join.is_ok(),
            "persona catalog reload must not overflow the 2 MiB Tokio worker stack"
        );
    }

    /// P0 regression (active-child `orchestration shut down`): a reload that
    /// would retire the orchestration runtime must never terminate a runtime
    /// whose child is still live. The exact production cancel caller is
    /// `Application::reload` → `reload_with_candidate_trust` →
    /// `commit_orchestration_candidate` → `previous.shutdown()` (the
    /// `candidate: None` disable branch bypasses the staging-time
    /// `has_active_jobs` deferral entirely), which cancels every unsettled job
    /// with reason `orchestration shut down` and cancels the runtime shutdown
    /// token (jobs.rs wait select). Here the gated child completes three
    /// successful bash tool results, the parent's in-flight `wait_jobs` spans
    /// the reload, and the child must settle exactly-once with unchanged
    /// identities and no `orchestration shut down` error anywhere.
    #[tokio::test]
    async fn reload_never_retires_orchestration_with_live_child_or_parent_wait() {
        // Event gates only: watch channels carry child progress and the
        // release gate; no polling sleeps anywhere in this test.
        let (bash_done_tx, bash_done_rx) = tokio::sync::watch::channel(0usize);
        let (proceed_tx, proceed_rx) = tokio::sync::watch::channel(false);
        let bash_errors: std::sync::Arc<parking_lot::Mutex<Vec<String>>> =
            std::sync::Arc::default();

        let (agent_dir, cwd, _faux, resources, session) =
            isolated_application_parts(r#"{"orchestration":{"tasks":true,"maxToolsPerAgent":64}}"#);
        let child_cwd = cwd.path().to_path_buf();
        let factory: crate::ChildSessionFactory = {
            let bash_done_tx = bash_done_tx.clone();
            let proceed_rx = proceed_rx.clone();
            let bash_errors = bash_errors.clone();
            std::sync::Arc::new(move |request| {
                let bash_done_tx = bash_done_tx.clone();
                let proceed_rx = proceed_rx.clone();
                let bash_errors = bash_errors.clone();
                let child_cwd = child_cwd.clone();
                Box::pin(async move {
                    let stream_fn: pi_agent::StreamFn = std::sync::Arc::new(
                        move |model, context, _options| {
                            let bash_done_tx = bash_done_tx.clone();
                            let proceed_rx = proceed_rx.clone();
                            let bash_errors = bash_errors.clone();
                            Box::pin(async move {
                                for message in &context.messages {
                                    if let pi_ai::Message::ToolResult(result) = message
                                        && result.is_error
                                    {
                                        bash_errors.lock().push(format!(
                                            "{}: {}",
                                            result.tool_name,
                                            result
                                                .content
                                                .iter()
                                                .map(|block| match block {
                                                    ContentBlock::Text { text, .. } => {
                                                        text.clone()
                                                    }
                                                    _ => String::new(),
                                                })
                                                .collect::<String>(),
                                        ));
                                    }
                                }
                                let successful_bash = context
                                    .messages
                                    .iter()
                                    .filter(|message| match message {
                                        pi_ai::Message::BashExecution(bash) => {
                                            !bash.cancelled
                                                && bash.exit_code.is_some_and(|code| code == 0)
                                        }
                                        pi_ai::Message::ToolResult(result) => {
                                            result.tool_name == "bash" && !result.is_error
                                        }
                                        _ => false,
                                    })
                                    .count();
                                bash_done_tx.send_if_modified(|seen| {
                                    if *seen < successful_bash {
                                        *seen = successful_bash;
                                        true
                                    } else {
                                        false
                                    }
                                });
                                if successful_bash >= 3 {
                                    let mut gate = proceed_rx.clone();
                                    while !*gate.borrow_and_update() {
                                        gate.changed().await.expect("proceed gate sender alive");
                                    }
                                }
                                let stream = pi_ai::new_assistant_message_event_stream();
                                let producer = stream.clone();
                                tokio::spawn(async move {
                                    let mut message = pi_ai::AssistantMessage::pending(&model);
                                    if successful_bash < 3 {
                                        message.content = vec![ContentBlock::ToolCall(
                                            pi_ai::ToolCall {
                                                id: format!("stage-{successful_bash}"),
                                                name: "bash".to_owned(),
                                                arguments: serde_json::json!({
                                                    "command":
                                                        format!("echo stage-{}", successful_bash + 1),
                                                }),
                                                thought_signature: None,
                                            },
                                        )];
                                        message.stop_reason = pi_ai::StopReason::ToolUse;
                                    } else {
                                        message.content = vec![ContentBlock::text(
                                            "child completed after three successful bash results",
                                        )];
                                        message.stop_reason = pi_ai::StopReason::Stop;
                                    }
                                    producer.end(Some(message)).await;
                                });
                                stream
                            })
                        },
                    );
                    crate::Session::new_with_additional_tools_filtered_and_discovery(
                        crate::SessionOptions {
                            model: request.model,
                            cwd: child_cwd.clone(),
                            system_prompt: request.system_prompt,
                            thinking_level: pi_agent::ThinkingLevel::Off,
                            api_key: String::new(),
                            compaction: None,
                            stream_options: Default::default(),
                            tools: None,
                            before_tool_call: None,
                            after_tool_call: None,
                            stream_fn: Some(stream_fn),
                            auth_resolver: None,
                            auth_manager: None,
                        },
                        request.orchestration_tools,
                        Default::default(),
                        crate::ResourceDiscovery::Disabled,
                    )
                })
            })
        };
        session
            .attach_resources(resources.clone())
            .await
            .expect("attach resources");
        session
            .start_new_recording()
            .expect("start orchestration recording");
        let (session_id, _session_path) = session
            .recorder_info()
            .expect("session identity for P0 correlation");
        let mut config = crate::OrchestrationConfig::new(
            crate::AgentCatalog::from_agents(vec![todo_routing_task_definition()]),
            cwd.path().join("artifacts"),
        );
        config.parent_model = session.model().expect("parent model");
        config.idle_ttl = None;
        config.max_tools_per_agent = 64;
        let runtime = crate::OrchestrationRuntime::new(config, factory).expect("orchestration");
        let application = Application::from_runtime_candidate(
            crate::ApplicationRuntimeCandidate::new(session).with_orchestration(runtime),
        )
        .await
        .expect("publish application");
        let runtime = application.orchestration_runtime().expect("runtime");
        let group_id = runtime.group_id().to_owned();
        let parent_agent_id = runtime.main_agent_id().to_owned();
        let spawn = runtime
            .spawn_tasks(
                &parent_agent_id,
                0,
                vec![crate::TaskItem {
                    index: 0,
                    id: "P0Survivor".to_owned(),
                    agent: "task".to_owned(),
                    assignment: "progress through three bash results, then hold".to_owned(),
                    todo_task_id: None,
                    ..Default::default()
                }],
            )
            .expect("spawn")
            .remove(0);

        // Parent's hub wait in flight BEFORE the offending lifecycle action.
        let wait = tokio::spawn({
            let runtime = runtime.clone();
            let job_id = spawn.job_id.clone();
            async move {
                runtime
                    .wait_jobs(
                        std::slice::from_ref(&job_id),
                        Some(std::time::Duration::from_secs(20)),
                        None,
                    )
                    .await
            }
        });

        // The child must progress through >=3 successful bash tool results
        // before the lifecycle action fires.
        let mut bash_seen = bash_done_rx.clone();
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            while !(*bash_seen.borrow_and_update() >= 3) {
                bash_seen
                    .changed()
                    .await
                    .expect("bash progress sender alive");
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "child never completed three successful bash results\n\
                 bash tool errors observed: {:#?}\n\
                 jobs: {:#?}",
                bash_errors.lock(),
                runtime.jobs(Some(std::slice::from_ref(&spawn.job_id))),
            )
        });

        // Offending lifecycle action: a settings reload that removes the
        // orchestration section routes reload → commit_orchestration_candidate
        // → previous.shutdown() on the live runtime.
        resources
            .settings_manager()
            .expect("settings manager")
            .update_global(|settings| {
                settings.orchestration = None;
            })
            .expect("disable orchestration settings");
        application.reload().await.expect("reload must succeed");

        // The live runtime must be retained with the child still progressing.
        assert!(
            !runtime.is_shutdown(),
            "P0 violated: reload cancelled the runtime shutdown token while a child was live.\n\
             sessionId={session_id} groupId={group_id} parentAgentId={parent_agent_id} \
             jobId={} agentId={} toolCallIds=stage-0,stage-1,stage-2\n\
             cancel path: Application::reload → reload_with_candidate_trust → \
             commit_orchestration_candidate → previous.shutdown() → \
             jobs.prepare_cancellation(\"orchestration shut down\") + shutdown token cancel",
            spawn.job_id,
            spawn.agent_id,
        );
        let retained = application
            .orchestration_runtime()
            .expect("deferred reload must retain the live runtime");
        assert_eq!(
            retained.group_id(),
            group_id,
            "reload must keep the live runtime group while a child is live"
        );
        let live = retained
            .jobs(Some(std::slice::from_ref(&spawn.job_id)))
            .into_iter()
            .next()
            .expect("live job snapshot");
        assert_eq!(live.id, spawn.job_id);
        assert_eq!(live.agent_id, spawn.agent_id);
        assert_eq!(live.status, crate::JobStatus::Running);

        // Release the gate: the child finishes its turn and settles exactly-once.
        proceed_tx.send(true).expect("proceed gate receivers alive");
        let joined = tokio::time::timeout(std::time::Duration::from_secs(20), wait)
            .await
            .expect("parent wait resolves in time");
        let settled = joined
            .expect("parent wait task did not panic")
            .unwrap_or_else(|error| {
                panic!(
                    "P0 violated: parent wait failed while its child was actively progressing.\n\
                     sessionId={session_id} groupId={group_id} parentAgentId={parent_agent_id} \
                     jobId={} agentId={} toolCallIds=stage-0,stage-1,stage-2\n\
                     wait error={error:#}",
                    spawn.job_id, spawn.agent_id,
                )
            });
        let settled_job = settled
            .into_iter()
            .find(|job| job.id == spawn.job_id)
            .expect("settled job");
        assert!(
            matches!(settled_job.status, crate::JobStatus::Completed),
            "P0 violated: child did not settle Completed exactly-once.\n\
             sessionId={session_id} groupId={group_id} parentAgentId={parent_agent_id} \
             jobId={} agentId={} job={settled_job:#?}",
            settled_job.id,
            settled_job.agent_id,
        );
        let result = settled_job.result.as_ref().expect("settled result");
        assert!(
            result
                .output
                .starts_with("child completed after three successful bash results"),
            "child output must carry the completed work: {:?}",
            result.output
        );
        assert!(
            result.error.is_none(),
            "P0 violated: shutdown error reached the parent result: {:?}",
            result.error
        );
        assert_eq!(retained.group_id(), group_id);
        assert_eq!(settled_job.agent_id, spawn.agent_id);

        // The deferred disable applies on the next quiescent reload.
        application.reload().await.expect("quiescent reload");
        assert!(
            application.orchestration_runtime().is_none(),
            "deferred disable must apply once no child is live"
        );
        application.cleanup().await.expect("cleanup");
        drop(agent_dir);
    }
    /// Deterministic race, outcome (a): a spawn parked while holding the same
    /// jobs.spawn_lock gate registers its authoritative child before the
    /// commit can acquire the gate, so commit_orchestration_candidate must
    /// observe the unsettled job and defer the whole replacement — the old
    /// runtime is retained and the job settles Completed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reload_commit_defers_when_spawn_registers_during_retirement_gate() {
        let (proceed_tx, proceed_rx) = tokio::sync::watch::channel(false);
        let (agent_dir, cwd, _faux, resources, session) =
            isolated_application_parts(r#"{"orchestration":{"tasks":true,"maxToolsPerAgent":64}}"#);
        let child_cwd = cwd.path().to_path_buf();
        let factory: crate::ChildSessionFactory = {
            let proceed_rx = proceed_rx.clone();
            std::sync::Arc::new(move |request| {
                let proceed_rx = proceed_rx.clone();
                let child_cwd = child_cwd.clone();
                Box::pin(async move {
                    let stream_fn: pi_agent::StreamFn =
                        std::sync::Arc::new(move |model, _context, _options| {
                            let proceed_rx = proceed_rx.clone();
                            Box::pin(async move {
                                let mut gate = proceed_rx.clone();
                                while !*gate.borrow_and_update() {
                                    gate.changed().await.expect("proceed gate alive");
                                }
                                let stream = pi_ai::new_assistant_message_event_stream();
                                let producer = stream.clone();
                                tokio::spawn(async move {
                                    let mut message = pi_ai::AssistantMessage::pending(&model);
                                    message.content = vec![ContentBlock::text(
                                        "racing child completed after commit gate",
                                    )];
                                    message.stop_reason = pi_ai::StopReason::Stop;
                                    producer.end(Some(message)).await;
                                });
                                stream
                            })
                        });
                    crate::Session::new_with_additional_tools_filtered_and_discovery(
                        crate::SessionOptions {
                            model: request.model,
                            cwd: child_cwd.clone(),
                            system_prompt: request.system_prompt,
                            thinking_level: pi_agent::ThinkingLevel::Off,
                            api_key: String::new(),
                            compaction: None,
                            stream_options: Default::default(),
                            tools: None,
                            before_tool_call: None,
                            after_tool_call: None,
                            stream_fn: Some(stream_fn),
                            auth_resolver: None,
                            auth_manager: None,
                        },
                        request.orchestration_tools,
                        Default::default(),
                        crate::ResourceDiscovery::Disabled,
                    )
                })
            })
        };
        session
            .attach_resources(resources.clone())
            .await
            .expect("attach resources");
        session
            .start_new_recording()
            .expect("start orchestration recording");
        let mut config = crate::OrchestrationConfig::new(
            crate::AgentCatalog::from_agents(vec![todo_routing_task_definition()]),
            cwd.path().join("artifacts"),
        );
        config.parent_model = session.model().expect("parent model");
        config.idle_ttl = None;
        config.max_tools_per_agent = 64;
        let runtime = crate::OrchestrationRuntime::new(config, factory).expect("orchestration");
        let application = Application::from_runtime_candidate(
            crate::ApplicationRuntimeCandidate::new(session).with_orchestration(runtime),
        )
        .await
        .expect("publish application");
        let runtime = application.orchestration_runtime().expect("runtime");
        let group_id = runtime.group_id().to_owned();

        // Park the racing spawn while it holds the spawn lock.
        let release_spawn = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        runtime.arm_spawn_test_park(
            crate::SpawnTestParkPoint::AfterSpawnLock,
            release_spawn.clone(),
        );
        let spawn = {
            let runtime = runtime.clone();
            tokio::task::spawn_blocking(move || {
                runtime
                    .spawn_tasks(
                        "Main",
                        0,
                        vec![crate::TaskItem {
                            index: 0,
                            id: "RaceA".to_owned(),
                            agent: "task".to_owned(),
                            assignment: "registered under the spawn gate".to_owned(),
                            todo_task_id: None,
                            ..Default::default()
                        }],
                    )
                    .expect("racing spawn registers under the gate")
                    .remove(0)
            })
        };

        // Commit must block on the spawn gate until the parked spawn registers.
        resources
            .settings_manager()
            .expect("settings manager")
            .update_global(|settings| {
                settings.orchestration = Some(crate::OrchestrationSettings {
                    tasks: Some(true),
                    max_concurrency: Some(2),
                    ..crate::OrchestrationSettings::default()
                });
            })
            .expect("non-equivalent settings for the race");
        let mut reload = {
            let application = application.clone();
            tokio::spawn(async move { application.reload().await })
        };
        // The reload cannot have completed while the spawn holds the gate.
        tokio::time::timeout(std::time::Duration::from_millis(300), &mut reload)
            .await
            .err()
            .expect("commit must block behind the parked spawn's spawn lock");
        release_spawn.store(true, std::sync::atomic::Ordering::Release);

        let spawn = tokio::time::timeout(std::time::Duration::from_secs(10), spawn)
            .await
            .expect("racing spawn resolves")
            .expect("spawn task join");
        let reload = tokio::time::timeout(std::time::Duration::from_secs(10), reload)
            .await
            .expect("reload resolves")
            .expect("reload succeeds");
        let _ = reload;

        // Outcome (a): the registered child defers the replacement.
        let retained = application
            .orchestration_runtime()
            .expect("retained runtime");
        assert_eq!(
            retained.group_id(),
            group_id,
            "commit must defer when the racing spawn registered a live job"
        );
        assert!(!retained.is_shutdown());
        assert!(
            !retained.jobs(Some(std::slice::from_ref(&spawn.job_id)))[0]
                .status
                .is_settled(),
            "the racing child must still be live after the deferred commit"
        );
        proceed_tx.send(true).expect("proceed gate alive");
        let settled = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            retained.wait_jobs(std::slice::from_ref(&spawn.job_id), None, None),
        )
        .await
        .expect("wait resolves")
        .expect("job settles");
        assert_eq!(settled[0].status, crate::JobStatus::Completed);
        assert_eq!(retained.group_id(), group_id);
        application.cleanup().await.expect("cleanup");
        drop(agent_dir);
    }

    /// Deterministic race, outcome (b): a spawn parked before the spawn lock
    /// runs after the commit has retired the outgoing runtime, so it must be
    /// rejected by the retiring flag — explicitly, before creating an
    /// authoritative child, with no `orchestration shut down` and no orphans.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reload_commit_rejects_racing_stale_handle_spawn_without_shutdown_or_orphans() {
        let (agent_dir, cwd, _faux, resources, session) =
            isolated_application_parts(r#"{"orchestration":{"tasks":true,"maxToolsPerAgent":64}}"#);
        let factory: crate::ChildSessionFactory = std::sync::Arc::new(move |_request| {
            Box::pin(async move { Err(anyhow::anyhow!("child factory must not run")) })
        });
        session
            .attach_resources(resources.clone())
            .await
            .expect("attach resources");
        session
            .start_new_recording()
            .expect("start orchestration recording");
        let mut config = crate::OrchestrationConfig::new(
            crate::AgentCatalog::from_agents(vec![todo_routing_task_definition()]),
            cwd.path().join("artifacts"),
        );
        config.parent_model = session.model().expect("parent model");
        config.idle_ttl = None;
        let runtime = crate::OrchestrationRuntime::new(config, factory).expect("orchestration");
        let application = Application::from_runtime_candidate(
            crate::ApplicationRuntimeCandidate::new(session).with_orchestration(runtime),
        )
        .await
        .expect("publish application");
        let runtime = application.orchestration_runtime().expect("runtime");
        let stale_group = runtime.group_id().to_owned();

        // Park the racing spawn before it can take the spawn gate.
        let release_spawn = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        runtime.arm_spawn_test_park(
            crate::SpawnTestParkPoint::BeforeSpawnLock,
            release_spawn.clone(),
        );
        let attempted = {
            let runtime = runtime.clone();
            tokio::task::spawn_blocking(move || {
                runtime.spawn_tasks(
                    "Main",
                    0,
                    vec![crate::TaskItem {
                        index: 0,
                        id: "RaceB".to_owned(),
                        agent: "task".to_owned(),
                        assignment: "must be rejected, never orphaned".to_owned(),
                        todo_task_id: None,
                        ..Default::default()
                    }],
                )
            })
        };

        // The commit retires the (job-less) outgoing runtime while the spawn
        // is parked before the lock.
        resources
            .settings_manager()
            .expect("settings manager")
            .update_global(|settings| {
                settings.orchestration = None;
            })
            .expect("disable orchestration settings");
        application.reload().await.expect("reload must succeed");
        // The outgoing runtime was job-less, so the quiescent reload applies
        // the disable immediately (no deferral) and retires it.
        assert!(
            application.orchestration_runtime().is_none(),
            "quiescent disable must retire the outgoing runtime"
        );
        // Release before any further fallible assert: the parked spawn spins
        // on this event flag and must never outlive the test body.
        release_spawn.store(true, std::sync::atomic::Ordering::Release);

        let error = tokio::time::timeout(std::time::Duration::from_secs(10), attempted)
            .await
            .expect("attempted spawn resolves")
            .expect("spawn task join")
            .expect_err("stale-handle spawn must be rejected");
        let message = error.to_string();
        assert!(
            message.contains("orchestration runtime is being replaced"),
            "explicit pre-replacement rejection required: {message}"
        );
        assert!(
            !message.contains("orchestration shut down"),
            "no shutdown reason may reach a racing spawn: {message}"
        );
        assert!(
            runtime.jobs(None).is_empty(),
            "no authoritative child may be created: {:?}",
            runtime.jobs(None)
        );
        application.cleanup().await.expect("cleanup");
        drop(agent_dir);
    }

    #[test]
    fn navigate_tree_success_commits_todo_transition_guard() {
        let fixture = resource_application("{}");
        current_thread_runtime().block_on(async {
            let session = fixture.application.session();
            session.start_new_recording().expect("start recording");
            let recorder = session.recorder().expect("recorder");
            let target = recorder
                .record_message(&pi_ai::Message::user_text("target", 1))
                .expect("record target");
            recorder
                .record_message(&pi_ai::Message::user_text("later", 2))
                .expect("record later");

            let result = fixture
                .application
                .navigate_tree(&target, crate::NavigateTreeOptions::default())
                .await
                .expect("navigate succeeds");

            assert!(result.changed);
            assert!(!result.cancelled);
            assert!(fixture.application.todo_transition_clear_for_test());
            fixture.application.cleanup().await.expect("cleanup");
        });
    }

    #[test]
    fn navigate_tree_error_drops_guard_and_restores_previous_execution() {
        let fixture = resource_application("{}");
        current_thread_runtime().block_on(async {
            let session = fixture.application.session();
            session.start_new_recording().expect("start recording");
            let reason = crate::TodoDagFailureReason::NoProgress {
                message: "preserve failed execution".to_owned(),
            };
            session.restore_todo_execution(
                crate::TodoDagExecutionStatus::Failed,
                Some(reason.clone()),
            );

            fixture
                .application
                .navigate_tree("missing-entry", crate::NavigateTreeOptions::default())
                .await
                .expect_err("missing target must fail");

            assert_eq!(
                session.todo_execution(),
                (crate::TodoDagExecutionStatus::Failed, Some(reason))
            );
            assert!(fixture.application.todo_transition_clear_for_test());
            fixture.application.cleanup().await.expect("cleanup");
        });
    }

    #[test]
    fn navigate_tree_cancelled_result_drops_guard_and_restores_previous_execution() {
        let fixture = resource_application("{}");
        current_thread_runtime().block_on(async {
            let session = fixture.application.session();
            session.start_new_recording().expect("start recording");
            let recorder = session.recorder().expect("recorder");
            let target = recorder
                .record_message(&pi_ai::Message::user_text("target", 1))
                .expect("record target");
            recorder
                .record_message(&pi_ai::Message::user_text("later", 2))
                .expect("record later");
            let reason = crate::TodoDagFailureReason::Spawn {
                message: "preserve cancelled execution".to_owned(),
            };
            session.restore_todo_execution(
                crate::TodoDagExecutionStatus::Failed,
                Some(reason.clone()),
            );
            let result = fixture
                .application
                .navigate_tree(
                    &target,
                    crate::NavigateTreeOptions {
                        summarize: true,
                        custom_instructions: Some("__test_cancel_after_claim__".to_owned()),
                        ..Default::default()
                    },
                )
                .await
                .expect("aborted summary navigation returns cancellation");

            assert!(!result.changed);
            assert!(result.cancelled);
            assert_eq!(
                session.todo_execution(),
                (crate::TodoDagExecutionStatus::Failed, Some(reason))
            );
            assert!(fixture.application.todo_transition_clear_for_test());
            fixture.application.cleanup().await.expect("cleanup");
        });
    }


    #[test]
    fn todo_transition_persist_failure_restores_coordinator_and_unfreezes() {
        let (release_tx, release_rx) = tokio::sync::watch::channel(false);
        let factory: crate::ChildSessionFactory = std::sync::Arc::new(move |_request| {
            let release_rx = release_rx.clone();
            Box::pin(async move {
                let mut release = release_rx;
                while !*release.borrow_and_update() {
                    release.changed().await.expect("incoming job gate alive");
                }
                Err(anyhow::anyhow!("incoming job released after transition assertions"))
            })
        });
        let fixture = orchestration_application_with_factory(
            r#"{"orchestration":{"tasks":true}}"#,
            factory,
        );
        let runtime = fixture
            .loop_runtime
            .as_ref()
            .expect("orchestration fixture keeps the loop scheduler runtime");
        runtime.block_on(async {
            let session = fixture.application.session();
            session
                .set_todos(vec![crate::TodoPhase {
                    name: "Main".to_owned(),
                    tasks: vec![crate::TodoItem {
                        id: "incoming-task".to_owned(),
                        content: "incoming correlated task".to_owned(),
                        status: crate::TodoStatus::Pending,
                        depends_on: Vec::new(),
                        ready: true,
                        waiting_on: Vec::new(),
                        agent: None,
                    }],
                }])
                .expect("set incoming Todo");
            let runtime = fixture
                .application
                .orchestration_runtime()
                .expect("orchestration runtime");
            let incoming_job_id = runtime
                .spawn_tasks(
                    runtime.main_agent_id(),
                    0,
                    vec![crate::TaskItem {
                        index: 0,
                        id: "IncomingTodo".to_owned(),
                        agent: "task".to_owned(),
                        assignment: "remain authoritative across Todo transition".to_owned(),
                        todo_task_id: Some("incoming-task".to_owned()),
                        ..Default::default()
                    }],
                )
                .expect("register incoming correlated job")
                .remove(0)
                .job_id;
            let incoming_reason = crate::TodoDagFailureReason::NoProgress {
                message: "incoming durable failure".to_owned(),
            };
            fixture
                .application
                .persist_todo_execution_for_test(
                    crate::TodoDagExecutionStatus::Failed,
                    Some(incoming_reason.clone()),
                )
                .expect("setup Failed snapshot must persist");
            let recorder = session.recorder().expect("recorder");
            let target = recorder
                .record_message(&pi_ai::Message::user_text("target", 1))
                .expect("record target");
            recorder
                .record_message(&pi_ai::Message::user_text("later", 2))
                .expect("record later");

            fixture
                .application
                .seed_todo_coordinator_nontrivial_for_test();
            let outgoing = fixture.application.todo_coordinator_state_for_test();
            assert!(
                fixture
                    .application
                    .recovered_todo_job_owned_for_test("owned-job"),
                "setup must seed outgoing ownership before the transition"
            );
            assert_eq!(
                fixture.application.todo_coordinator_generation_for_test(),
                7
            );
            assert_eq!(
                session.todo_execution(),
                (
                    crate::TodoDagExecutionStatus::Failed,
                    Some(incoming_reason.clone())
                )
            );
            assert!(fixture.application.todo_transition_clear_for_test());

            fixture
                .application
                .fail_next_todo_execution_persist_for_test();
            let result = fixture
                .application
                .navigate_tree(&target, crate::NavigateTreeOptions::default())
                .await
                .expect("navigate still succeeds after target persist failure");

            assert!(result.changed);
            assert!(!result.cancelled);
            assert!(
                !fixture
                    .application
                    .todo_coordinator_matches_for_test(&outgoing),
                "committed persist failure must not restore outgoing coordinator identity"
            );
            assert_eq!(
                fixture.application.todo_coordinator_generation_for_test(),
                0,
                "incoming restore rebuilds coordinator generation from session/jobs"
            );
            assert!(
                !fixture
                    .application
                    .recovered_todo_job_owned_for_test("owned-job"),
                "outgoing owned jobs are not incoming session/job identity"
            );
            assert!(
                fixture
                    .application
                    .recovered_todo_job_owned_for_test(&incoming_job_id),
                "incoming correlated job from jobs() must be adopted after persist-failure restore"
            );
            let (status, failure_reason) = fixture.application.todo_dag_execution_for_test();
            assert_eq!(status, crate::TodoDagExecutionStatus::Failed);
            let reason = failure_reason.expect("Failed reason");
            assert!(
                matches!(reason, crate::TodoDagFailureReason::Reconcile { .. }),
                "persist failure must surface as Reconcile: {reason:?}"
            );
            assert!(
                reason
                    .message()
                    .contains("injected Todo execution persist failure"),
                "Failed reason must name the injected persist fault: {}",
                reason.message()
            );
            assert_eq!(
                session.todo_execution(),
                (crate::TodoDagExecutionStatus::Failed, Some(reason.clone())),
                "final durable snapshot must match coordinator Failed/Reconcile after one-shot consume"
            );
            assert!(fixture.application.todo_transition_clear_for_test());
            release_tx.send(true).expect("release incoming job");
            runtime
                .wait_jobs(
                    std::slice::from_ref(&incoming_job_id),
                    Some(std::time::Duration::from_secs(10)),
                    None,
                )
                .await
                .expect("incoming job settles after release");
            fixture.application.cleanup().await.expect("cleanup");
        });
    }


    #[test]
    fn same_cwd_recovered_terminal_job_is_owned_before_revival_without_duplicate_spawn() {
        let fixture = orchestration_application(r#"{"orchestration":{"tasks":true}}"#);
        let runtime = fixture
            .loop_runtime
            .as_ref()
            .expect("orchestration fixture keeps the loop scheduler runtime");
        runtime.block_on(async {
            let session = fixture.application.session();
            session
                .set_todos(vec![crate::TodoPhase {
                    name: "Main".to_owned(),
                    tasks: vec![crate::TodoItem {
                        id: "task-1".to_owned(),
                        content: "recovered task".to_owned(),
                        status: crate::TodoStatus::Pending,
                        depends_on: Vec::new(),
                        ready: true,
                        waiting_on: Vec::new(),
                        agent: None,
                    }],
                }])
                .expect("set Todo");
            let session_path = session.recorder_info().expect("session path").1;
            let initial = fixture
                .application
                .execute_todo_dag()
                .expect("spawn Todo attempt");
            assert_eq!(initial.spawns.len(), 1);
            let job_id = initial.spawns[0].job_id.clone();
            let runtime = fixture.application.orchestration_runtime().expect("orchestration");
            runtime
                .checkpoint_for_restart()
                .expect("persist live Todo attempt for recovery");
            assert!(
                runtime
                    .jobs(Some(std::slice::from_ref(&job_id)))
                    .iter()
                    .all(|job| matches!(job.status, crate::JobStatus::Queued | crate::JobStatus::Running))
            );

            fixture
                .application
                .new_session()
                .await
                .expect("switch away in same cwd");
            fixture.application.drop_resident_orchestrations_for_test();
            fixture
                .application
                .switch_session(&session_path)
                .await
                .expect("resume recovered session in same cwd");
            assert!(
                fixture.application.recovered_todo_job_owned_for_test(&job_id),
                "Todo ownership must be reconstructed from installed Running snapshot before revival",
            );

            let resumed = fixture.application.orchestration_runtime().expect("resumed runtime");
            let jobs = resumed.jobs(None);
            assert_eq!(jobs.len(), 1, "resume must not spawn a duplicate Todo attempt");
            assert_eq!(jobs[0].id, job_id, "recovered job identity must remain authoritative");
            assert_eq!(
                fixture.application.todo_state().phases[0].tasks[0].status,
                crate::TodoStatus::InProgress,
                "recovered task remains InProgress until its authoritative terminal result",
            );
        });
    }

    async fn fork_cancellation_application() -> (ApplicationFixture, String, PathBuf, Vec<u8>) {
        let (agent, cwd, faux, resources, session) = isolated_application_parts("{}");
        let session_dir = agent.path().join("sessions");
        let source = crate::start_session_in(
            cwd.path(),
            None,
            Some("off"),
            Some(&session_dir),
            Some("application-fork-parent"),
            None,
        )
        .expect("source recorder");
        source
            .record_message(&pi_ai::Message::user_text("retained", 1))
            .expect("retained message");
        let selected = source
            .record_message(&pi_ai::Message::user_text("selected", 2))
            .expect("selected message");
        session.record(source).expect("attach source");
        session.attach_resources(resources).await.expect("attach resources");
        session
            .load_history(vec![
                pi_ai::Message::user_text("retained", 1),
                pi_ai::Message::user_text("selected", 2),
            ])
            .await
            .expect("load parent history");
        let application = Application::from_runtime_candidate(
            crate::ApplicationRuntimeCandidate::new(session),
        )
        .await
        .expect("publish application");
        application
            .set_todos(vec![crate::TodoPhase {
                name: "Fork".to_owned(),
                tasks: vec![crate::TodoItem {
                    id: "fork-task".to_owned(),
                    content: "preserve fork state".to_owned(),
                    status: crate::TodoStatus::Pending,
                    depends_on: Vec::new(),
                    ready: true,
                    waiting_on: Vec::new(),
                    agent: None,
                }],
            }])
            .expect("seed fork todo");
        application.goal_create("preserve fork goal", Some(100)).expect("seed fork goal");
        application.session().persist_recording_now_durable().expect("persist seeded parent");
        let parent_path = application.session().recorder_info().expect("parent recorder").1;
        let parent_bytes = std::fs::read(&parent_path).expect("read seeded parent");
        (
            ApplicationFixture {
                agent,
                cwd,
                _faux: faux,
                application,
                loop_runtime: None,
            },
            selected,
            parent_path,
            parent_bytes,
        )
    }

    fn application_jsonl_paths(directory: &Path) -> Vec<PathBuf> {
        let mut paths = std::fs::read_dir(directory)
            .expect("read sessions")
            .map(|entry| entry.expect("session entry").path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "jsonl"))
            .collect::<Vec<_>>();
        paths.sort();
        paths
    }

    /// /web regression: a thinking level set on a runtime that reached its
    /// session through the in-place session replacement (`/new` on the live
    /// runtime) must be journaled. The replacement installs the fresh recorder
    /// without activating its durable writer.
    #[tokio::test]
    async fn new_session_set_thinking_level_is_recorded() {
        let suffix = uuid::Uuid::now_v7().simple();
        let mut model = Model::default();
        model.id = format!("thinking-persist-{suffix}");
        model.name = "Thinking Persist".to_owned();
        model.api = format!("thinking-persist-{suffix}-api");
        model.provider = format!("thinking-persist-{suffix}-provider");
        model.base_url = "http://localhost:0".to_owned();
        model.reasoning = true;
        let _reasoning = register_faux_provider(FauxProviderOptions {
            api: model.api.clone(),
            provider: model.provider.clone(),
            models: vec![model.clone()],
            chunk_size: 8,
        });

        let (agent, cwd, _faux, resources, session) = isolated_application_parts("{}");
        let session_dir = agent.path().join("sessions");
        session
            .attach_resources(resources)
            .await
            .expect("attach resources");
        session.start_new_recording().expect("record primary");
        let application = Application::from_runtime_candidate(
            crate::ApplicationRuntimeCandidate::new(session),
        )
        .await
        .expect("publish application");
        // The in-place replacement session must run the reasoning model so
        // High is never clamped to Off.
        application.set_model(model.clone(), "test".to_owned());

        application.new_session().await.expect("in-place new session");
        let (_, new_session_file) = application
            .session()
            .recorder_info()
            .expect("replacement recorder");
        application.set_thinking_level(pi_agent::ThinkingLevel::High);

        let disk = crate::load_session_tree(&new_session_file).expect("reload new journal");
        assert!(
            disk.has_thinking_entry(),
            "thinking level set on an in-place replacement session must reach the journal"
        );
        assert_eq!(
            disk.build_context(None).thinking_level,
            "high",
            "the journal must carry the level the user selected"
        );
        application.cleanup().await.expect("cleanup");
    }


    #[tokio::test]
    async fn application_fork_cancellation_before_commit_spawn_removes_child() {
        let (fixture, selected, parent_path, parent_bytes) = fork_cancellation_application().await;
        let session_dir = fixture.agent.path().join("sessions");
        let before = application_jsonl_paths(&session_dir);
        let parent_id = fixture.session().recorder_info().expect("parent remains").0;
        let parent_history = fixture.session().history();
        let parent_todo = fixture.todo_state();
        let parent_goal = fixture.goal_state();
        let mut events = fixture.subscribe();
        let gate = fixture.arm_cutover_after_resident_take_gate_for_test();
        let application = fixture.application.clone();
        let task = tokio::spawn(async move { application.fork_session(&selected).await });

        gate.wait_entered().await;
        task.abort();
        assert!(task.await.expect_err("fork task cancelled").is_cancelled());
        assert_eq!(application_jsonl_paths(&session_dir), before, "orphan child remains");
        assert_eq!(fixture.session().recorder_info().expect("parent remains"), (parent_id, parent_path.clone()));
        assert_eq!(fixture.session().history(), parent_history, "parent history changed");
        assert_eq!(fixture.todo_state(), parent_todo, "parent todo changed");
        assert_eq!(fixture.goal_state(), parent_goal, "parent goal changed");
        assert_eq!(std::fs::read(&parent_path).expect("reread parent"), parent_bytes);
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(event, ApplicationEvent::SessionForked(_)
                    | ApplicationEvent::GoalUpdated { operation: "fork_clone", .. }),
                "cancelled unpublished fork emitted a success event: {event:?}"
            );
        }
        gate.release();
    }

    #[tokio::test]
    async fn application_fork_cancellation_after_install_keeps_visible_child() {
        let (fixture, selected, parent_path, parent_bytes) = fork_cancellation_application().await;
        let session_dir = fixture.agent.path().join("sessions");
        let before = application_jsonl_paths(&session_dir);
        let mut events = fixture.subscribe();
        let expected_goal = fixture.goal_state();
        let expected_history = vec![pi_ai::Message::user_text("retained", 1)];
        let expected_todo = crate::TodoState {
            phases: Vec::new(),
            storage: crate::TodoStorage::Session,
        };
        let gate = fixture.session().arm_fork_after_install_gate_for_test();
        let application = fixture.application.clone();
        let task = tokio::spawn(async move { application.fork_session(&selected).await });

        gate.wait_entered().await;
        let child_path = fixture.session().recorder_info().expect("child installed").1;
        assert_ne!(child_path, parent_path);
        task.abort();
        assert!(task.await.expect_err("fork caller cancelled").is_cancelled());
        gate.release();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut forked = 0;
        let mut goal_updated = 0;
        while forked == 0 || goal_updated == 0 {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(!remaining.is_zero(), "detached fork commit did not publish events");
            match tokio::time::timeout(remaining, events.recv()).await {
                Ok(Ok(ApplicationEvent::SessionForked(_))) => forked += 1,
                Ok(Ok(ApplicationEvent::GoalUpdated { operation: "fork_clone", .. })) => {
                    goal_updated += 1;
                }
                Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                other => panic!("fork event stream ended early: {other:?}"),
            }
        }
        while let Ok(event) = events.try_recv() {
            match event {
                ApplicationEvent::SessionForked(_) => forked += 1,
                ApplicationEvent::GoalUpdated { operation: "fork_clone", .. } => goal_updated += 1,
                _ => {}
            }
        }
        assert_eq!(forked, 1, "SessionForked must be emitted exactly once");
        assert_eq!(goal_updated, 1, "fork GoalUpdated must be emitted exactly once");
        assert!(child_path.exists(), "visible child journal was deleted");
        assert_eq!(fixture.session().recorder_info().expect("child remains").1, child_path);
        assert_eq!(fixture.session().history(), expected_history, "shared history is not retained prefix");
        assert_eq!(fixture.todo_state(), expected_todo, "todo state did not restore cut ancestry");
        assert_eq!(fixture.goal_state().current.as_ref().map(|goal| goal.objective.as_str()), Some("preserve fork goal"));
        assert_ne!(fixture.goal_state(), expected_goal, "fork goal was not cloned");
        let mut expected = before;
        expected.push(child_path.clone());
        expected.sort();
        assert_eq!(application_jsonl_paths(&session_dir), expected, "child deleted or duplicated");
        assert_eq!(std::fs::read(&parent_path).expect("reread parent"), parent_bytes);
    }
}
