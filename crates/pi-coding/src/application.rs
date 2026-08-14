mod todo_execution;
mod runtime;
mod workflows;
mod workflow_backend;
mod workflow_events;
mod side_chat;
pub use side_chat::{
    SideChatFork, SideChatMainPeek, create_peek_main_tool, filter_tools_by_capabilities,
    tools_include_mutation,
};
pub use todo_execution::{TodoDagExecutionOutcome, TodoDagExecutionStatus};
pub use runtime::ApplicationRuntimeCandidate;
pub use workflows::ApplicationWorkflowRuntimeFactory;

use std::{path::{Path, PathBuf}, sync::{Arc, Weak, atomic::{AtomicBool, Ordering}}, time::{Duration, Instant}};

use anyhow::{Context, Result, anyhow, bail};
use parking_lot::Mutex;
use pi_agent::{AgentEvent, AgentTool, AgentToolResult, ThinkingLevel, ToolCapability, compose_before_tool_call};
use pi_ai::{ContentBlock, CustomMessage, Message, Model, Schema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{sync::{Mutex as AsyncMutex, OwnedMutexGuard, broadcast}, task::JoinHandle};

use crate::{
    CompactionReason, ExtensionActionHost, ExtensionCancellation, ExtensionCommandDescriptor,
    ExtensionContextSnapshot, ExtensionContextUsage, ExtensionEvent, ExtensionFuture,
    ExtensionInstanceId, ExtensionMessageDelivery, ExtensionPermissionSet, ExtensionRuntime,
    ExtensionRuntimeAction, Goal, GoalContinuationDecision, GoalError, GoalEvent, GoalState,
    GoalUsageDelta, Handoff, MessageDelivery, ProcessEvent, ProcessId, ProcessInfo, ProcessKey,
    ProcessLogs, ProcessManager, ProcessOwnerId, ProcessSignal, ProcessSpawnSpec, ProcessTerminalSize,
    Session, SessionEvent,
};

const EVENT_BUFFER_CAPACITY: usize = 512;
const GOAL_CONTINUATION_CUSTOM_TYPE: &str = "pi.goal.continue";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamingBehavior {
    Steer,
    FollowUp,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalActivationOutcome {
    Started,
    Queued,
    AlreadyActive,
}

/// Outcome shared by lifecycle operations that either complete or are cancelled by an extension.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionChangeOutcome {
    pub cancelled: bool,
}

impl SessionChangeOutcome {
    const COMPLETED: Self = Self { cancelled: false };
    const CANCELLED: Self = Self { cancelled: true };
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
        Self { text, cancelled: false }
    }

    fn cancelled() -> Self {
        Self { text: String::new(), cancelled: true }
    }
}


struct PreparedSameCwdCutover {
    session: crate::PreparedSessionReplacement,
    orchestration: Option<(crate::OrchestrationRuntime, crate::PreparedDurableBinding)>,
    loop_handle: crate::LoopSchedulerHandle,
    loops: crate::PreparedLoopActivation,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_selection: Option<crate::SelectionPlan>,
    pub todo_phases: Vec<crate::TodoPhase>,
    pub goal: GoalState,
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
            base_dir: path.parent().map(|parent| parent.to_string_lossy().into_owned()),
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
    RuntimeChanged { epoch: u64 },
    SessionStarted(SessionStartedEvent),
    Session(SessionEvent),
    Agent(AgentEvent),
    RunFailed { message: String },
    AgentSettled,
    Exported { path: String },
    ShareSucceeded { url: String },
    ShareFailed { message: String },
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
    Loop(crate::LoopEvent),
    Orchestration(crate::OrchestrationEvent),
    Workflow(crate::WorkflowEvent),
    TodoUpdated {
        phases: Vec<crate::TodoPhase>,
        completed_tasks: Vec<crate::TodoCompletionTransition>,
    },
    TodoReminder {
        phases: Vec<crate::TodoPhase>,
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
            Self::Agent(event) => event.serialize(serializer),
            Self::Session(event) => event.serialize(serializer),
            Self::RunFailed { message } => {
                serde_json::json!({ "type": "run_failed", "message": message }).serialize(serializer)
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
            Self::Loop(event) => event.serialize(serializer),
            Self::Orchestration(event) => event.serialize(serializer),
            Self::Workflow(event) => event.serialize(serializer),
            Self::TodoUpdated { phases, completed_tasks } => {
                serde_json::json!({
                    "type": "todo_updated",
                    "phases": phases,
                    "completed_tasks": completed_tasks,
                })
                .serialize(serializer)
            }
            Self::TodoReminder { phases } => {
                serde_json::json!({ "type": "todo_reminder", "phases": phases })
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

    pub fn bind(&self, application: &Application) -> Result<()> {
        let mut binding = self.application.lock();
        if binding.as_ref().and_then(Weak::upgrade).is_some() {
            return Err(anyhow!("goal tool is already attached to an application"));
        }
        *binding = Some(Arc::downgrade(&application.inner));
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalToolArguments {
    op: String,
}
pub type ApplicationRuntimeFuture =
    std::pin::Pin<Box<dyn Future<Output = Result<ApplicationRuntimeCandidate>> + Send>>;

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
    candidate.session.load_history(context.messages).await?;
    if let Some(provider) = context.provider.as_deref()
        && let Some(model_id) = context.model_id.as_deref()
        && let Some(model) = pi_ai::get_model(provider, model_id)
    {
        candidate
            .session
            .set_model_with_resolved_auth(model)
            .await?;
    }
    candidate.session.record(recorder)?;
    Ok(candidate)
}

impl ApplicationInner {
    fn runtime(&self) -> Arc<runtime::ApplicationRuntime> {
        self.runtime.runtime()
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
        for message in orchestration.inbox(orchestration.main_agent_id(), true) {
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
    fn report_steer_main_delivery_failure(&self, event: &crate::OrchestrationEvent, error: &anyhow::Error) {
        let id = match event {
            crate::OrchestrationEvent::MessageDelivered { message, .. } => message.id.as_str(),
            _ => "?",
        };
        eprintln!("orchestration: steering delivered message {id} into the main session failed: {error:#}");
    }

    fn report_main_delivery_ack_failure(&self, message_id: &str, error: &anyhow::Error) {
        eprintln!(
            "orchestration: acknowledging recorded Main delivery {message_id} failed: {error:#}"
        );
    }
}

impl Application {
    fn runtime(&self) -> Arc<runtime::ApplicationRuntime> {
        self.inner.runtime()
    }
}

struct ApplicationInner {
    events: broadcast::Sender<ApplicationEvent>,
    runtime_factory: Mutex<Option<Arc<dyn ApplicationRuntimeFactory>>>,
    runtime: runtime::ApplicationRuntimeSlot,
    steered_main_message_ids: Mutex<std::collections::BTreeSet<String>>,
    workflow_manager: Mutex<Option<crate::WorkflowManager>>,
    workflow_runtime_factory: Mutex<Option<Weak<workflows::ApplicationWorkflowRuntimeFactory>>>,
    workflow_events: Mutex<Option<JoinHandle<()>>>,
    loop_scheduler: Mutex<Option<crate::LoopSchedulerRuntime>>,
    operation_gate: AsyncMutex<()>,
    cleanup_lock: AsyncMutex<()>,
    cleaned: AtomicBool,
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
        Self::build(session, None).await
    }

    pub async fn new_with_extensions(
        session: Session,
        runtime: ExtensionRuntime,
        permissions: ExtensionPermissionSet,
    ) -> Self {
        Self::build(session, Some((runtime, permissions))).await
    }

    pub fn attach_runtime_factory(
        &self,
        factory: Arc<dyn ApplicationRuntimeFactory>,
    ) -> Result<()> {
        let mut current = self.inner.runtime_factory.lock();
        if current.is_some() {
            return Err(anyhow!("application runtime factory is already configured"));
        }
        *current = Some(factory);
        Ok(())
    }

    #[must_use]
    pub fn runtime_epoch(&self) -> u64 {
        self.runtime().epoch()
    }

    pub async fn from_runtime_candidate(
        candidate: ApplicationRuntimeCandidate,
    ) -> Result<Self> {
        let ApplicationRuntimeCandidate {
            session,
            extension_runtime,
            orchestration_runtime,
            goal_tool_binding,
        } = candidate;
        let application = Self::build(session, extension_runtime).await;
        if let Some(runtime) = orchestration_runtime {
            application.attach_orchestration(runtime).await?;
        }
        if let Some(binding) = goal_tool_binding {
            application.attach_goal_tool(binding)?;
        }
        Ok(application)
    }

    pub async fn new_with_orchestration(
        session: Session,
        orchestration: crate::OrchestrationRuntime,
    ) -> Self {
        let application = Self::build(session, None).await;
        application
            .attach_orchestration(orchestration)
            .await
            .expect("fresh application accepts orchestration");
        application
    }

    async fn build(
        session: Session,
        extension_runtime: Option<(ExtensionRuntime, ExtensionPermissionSet)>,
    ) -> Self {
        let (events, _) = broadcast::channel(EVENT_BUFFER_CAPACITY);
        let initial_runtime = runtime::ApplicationRuntime::new(
            runtime::INITIAL_RUNTIME_EPOCH,
            session,
            extension_runtime,
            None,
            None,
        );
        let inner = Arc::new(ApplicationInner {
            runtime: runtime::ApplicationRuntimeSlot::new(initial_runtime, events.clone()),
            runtime_factory: Mutex::new(None),
            events,
            steered_main_message_ids: Mutex::new(std::collections::BTreeSet::new()),
            workflow_manager: Mutex::new(None),
            workflow_runtime_factory: Mutex::new(None),
            workflow_events: Mutex::new(None),
            loop_scheduler: Mutex::new(None),
            operation_gate: AsyncMutex::new(()),
            cleanup_lock: AsyncMutex::new(()),
            cleaned: AtomicBool::new(false),
        });
        let application = Self { inner };
        let generation = application.runtime();
        application
            .bind_runtime_generation(generation.clone())
            .await
            .expect("fresh runtime generation must bind");
        // Hook-wire the initial trust decision: resolve it through the
        // fail-open host hook and extension event, and re-stage the resources
        // with the composed decision when it differs from the initial build.
        application.resolve_startup_trust(&generation).await;
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
        let session_file = generation.session.recorder_info().map(|(_, path)| path);
        let loop_scheduler = crate::start_loop_scheduler(
            session_file.as_deref(),
            loop_runner,
            loop_events,
        );
        *application.inner.loop_scheduler.lock() = Some(loop_scheduler);
        application
    }

    async fn bind_runtime_generation(
        &self,
        generation: Arc<runtime::ApplicationRuntime>,
    ) -> Result<()> {
        let epoch = generation.epoch();
        let session = generation.session();
        let extension = generation.extension_runtime().map(|(runtime, _)| runtime);
        if let Some(runtime) = &extension {
            // Route this session's extension-owned provider lookups through
            // this runtime's namespace (fail closed on any other runtime's
            // registration of the same api).
            session.set_provider_namespace(Some(runtime.provider_namespace().to_owned()));
            runtime.set_action_host(Arc::new(ApplicationExtensionHost {
                application: Arc::downgrade(&self.inner),
            }))?;
            // Host hooks (Settings.hooks) exclude extension tool calls in the
            // MVP: record the extension-provided tool names so the session's
            // pre/post_tool_call firing points can skip them.
            session.set_extension_tool_names(
                runtime
                    .agent_tools()
                    .into_iter()
                    .map(|tool| tool.name),
            );
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
            let host_hook = session.before_tool_call();
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
                if inner.runtime().todo_transition_active.load(Ordering::Acquire) {
                    return Err(anyhow!("Todo mutation rejected during session transition"));
                }
                Ok(())
            }),
            commit: Arc::new(move || {
                let Some(inner) = todo_commit_inner.upgrade() else { return; };
                // Workflow children: the supervisor owns canonical Todo DAG
                // execution (BUG-1), so a session set_todos/apply_todo mutation
                // must never auto-arm the DAG. Only the parent/main application
                // arms through the mutation-transaction commit hook.
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
        if let Some(binding) = generation.goal_tool_binding() {
            binding.bind(self)?;
        }
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
                        let _ = inner.runtime.publish(epoch, ApplicationEvent::TodoUpdated {
                            phases: session.todo_state().phases,
                            completed_tasks: Vec::new(),
                        });
                    }
                    let _ = inner.runtime.publish(epoch, ApplicationEvent::Agent(event));
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
                let Some(inner) = session_inner.upgrade() else { break; };
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
                        inner.steered_main_message_ids.lock().remove(message_id);
                        inner.report_main_delivery_ack_failure(message_id, &error);
                    } else {
                        inner.steered_main_message_ids.lock().remove(message_id);
                    }
                }
                let _ = inner.runtime.publish(epoch, ApplicationEvent::Session(event));
            }
        }));

        let mut process_events = generation.process_manager.subscribe();
        let process_inner = Arc::downgrade(&self.inner);
        let owner = generation.process_owner_id.clone();
        *generation.process_events.lock() = Some(tokio::spawn(async move {
            loop {
                match process_events.recv().await {
                    Ok(event) if process_event_owner(&event) == &owner => {
                        let Some(inner) = process_inner.upgrade() else { break; };
                        let _ = inner.runtime.publish(epoch, ApplicationEvent::Process(event));
                    }
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }));

        if let Some(orchestration) = generation.orchestration_runtime() {
            orchestration
                .bind_and_recover(&session)
                .context("binding orchestration runtime generation")?;
        }
        if let Some(orchestration) = generation.orchestration_runtime() {
            let mut events = orchestration.subscribe();
            self.inner.replay_main_mailbox(&generation, &orchestration).await;
            let event_runtime = orchestration.clone();
            let orchestration_inner = Arc::downgrade(&self.inner);
            let event_generation = generation.clone();
            *generation.orchestration_events.lock() = Some(tokio::spawn(async move {
                loop {
                    match events.recv().await {
                        Ok(event) => {
                            let Some(inner) = orchestration_inner.upgrade() else { break; };
                            if let Err(error) = inner
                                .steer_main_delivered_message(&event_generation, &event)
                                .await
                            {
                                inner.report_steer_main_delivery_failure(&event, &error);
                            }
                            let _ = inner.runtime.publish(epoch, ApplicationEvent::Orchestration(event));
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }));
        }
        Ok(())
    }

    #[must_use]
    pub fn extension_runtime(&self) -> Option<ExtensionRuntime> {
        self.runtime()
            .extension_runtime()
            .map(|(runtime, _)| runtime)
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
            .map(|resources| resources.settings_manager())
            .ok_or_else(|| anyhow!("session has no resource manager"))
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
                active.session.apply_runtime_settings(settings.clone()).await;
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
        active.session.apply_runtime_settings(settings.clone()).await;
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
        binding.bind(self)?;
        let active = self.runtime();
        let mut current = active.goal_tool_binding.lock();
        if current.is_some() {
            return Err(anyhow!("application goal tool is already configured"));
        }
        *current = Some(binding);
        Ok(())
    }

    pub fn orchestration_runtime(&self) -> Option<crate::OrchestrationRuntime> {
        self.runtime().orchestration_runtime()
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
        self.inner.replay_main_mailbox(&active, &event_runtime).await;
        let event_inner = Arc::downgrade(&self.inner);
        let epoch = active.epoch();
        let event_task = tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        let Some(inner) = event_inner.upgrade() else { break; };
                        let active = inner.runtime();
                        if active.epoch() != epoch { break; }
                        if matches!(&event, crate::OrchestrationEvent::JobUpdated { job, .. } if !job.status.is_settled()) {
                            active.todo_cycle_pending.store(true, Ordering::Release);
                        }
                        let allow_spawn = !active.todo_continuation_suppressed.load(Ordering::Acquire);
                        if let Err(error) = inner.observe_orchestration_event(&event_runtime, &event, allow_spawn) {
                            inner.todo_dag_failed(&error);
                        }
                        inner.steer_main_delivered_message(&active, &event).await
                            .unwrap_or_else(|error| inner.report_steer_main_delivery_failure(&event, &error));
                        let _ = inner.runtime.publish(epoch, ApplicationEvent::Orchestration(event));
                        inner.finish_todo_cycle_if_idle(Some(&event_runtime), false);
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let Some(inner) = event_inner.upgrade() else { break; };
                        let active = inner.runtime();
                        if active.epoch() != epoch { break; }
                        for event in event_runtime.presentation_events() {
                            if matches!(&event, crate::OrchestrationEvent::JobUpdated { job, .. } if !job.status.is_settled()) {
                                active.todo_cycle_pending.store(true, Ordering::Release);
                            }
                            let allow_spawn = !active.todo_continuation_suppressed.load(Ordering::Acquire);
                            if let Err(error) = inner.observe_orchestration_event(&event_runtime, &event, allow_spawn) {
                                inner.todo_dag_failed(&error);
                            }
                            // `presentation_events` is a UI snapshot (jobs and
                            // agents), never an unconsumed context queue:
                            // historical `MessageDelivered` must NOT be steered
                            // into the main session here.
                            let _ = inner.runtime.publish(epoch, ApplicationEvent::Orchestration(event));
                            inner.finish_todo_cycle_if_idle(Some(&event_runtime), false);
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        *active.orchestration_events.lock() = Some(event_task);
        active.orchestration_explicit.store(explicit, Ordering::Release);
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

    #[must_use]
    pub fn todo_state(&self) -> crate::TodoState {
        self.runtime().session.todo_state()
    }

    pub fn apply_todo(&self, op: crate::TodoOp) -> Result<crate::TodoApplyResult> {
        let active = self.runtime();
        match active.session.apply_todo(op) {
            Ok(result) => {
                self.inner.publish(ApplicationEvent::TodoUpdated {
                    phases: result.phases.clone(),
                    completed_tasks: result.completed_tasks.clone(),
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
        self.inner.publish(ApplicationEvent::TodoUpdated {
            phases: result.phases.clone(),
            completed_tasks: result.completed_tasks.clone(),
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
    #[must_use]
    pub fn generate_handoff(&self) -> Handoff {
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
        self.runtime().session.generate_handoff_with_prose(&jobs).await
    }

    pub fn goal_create(&self, objective: impl Into<String>, token_budget: Option<u64>) -> Result<Goal> {
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
        let decision = self.runtime().session.goal_runtime().continuation_decision();
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

    pub async fn loop_create(
        &self,
        request: crate::LoopCreateRequest,
    ) -> Result<crate::LoopTask> {
        Ok(self.loop_handle()?.create(request).await?)
    }

    pub async fn loop_update(
        &self,
        request: crate::LoopUpdateRequest,
    ) -> Result<crate::LoopTask> {
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
        active.process_manager.spawn(active.process_owner_id.clone(), spec).await
    }

    #[must_use]
    pub fn process_list(&self) -> Vec<ProcessInfo> {
        let active = self.runtime();
        active.process_manager.list(&active.process_owner_id)
    }

    pub fn process_describe(&self, id: &ProcessId) -> Result<ProcessInfo> {
        let active = self.runtime();
        active.process_manager.describe(&active.process_owner_id, id)
    }

    pub async fn process_logs(&self, id: &ProcessId, cursor: u64, max_bytes: Option<usize>, follow: bool, timeout: Option<Duration>) -> Result<ProcessLogs> {
        let active = self.runtime();
        active.process_manager.logs(&active.process_owner_id, id, cursor, max_bytes, follow, timeout).await
    }

    pub async fn process_write(&self, id: &ProcessId, bytes: Vec<u8>, close_stdin: bool) -> Result<()> {
        let active = self.runtime();
        active.process_manager.write(&active.process_owner_id, id, bytes, close_stdin).await
    }

    pub async fn process_send_keys(&self, id: &ProcessId, keys: &[ProcessKey]) -> Result<()> {
        let active = self.runtime();
        active.process_manager.send_keys(&active.process_owner_id, id, keys).await
    }

    pub fn process_resize(&self, id: &ProcessId, size: ProcessTerminalSize) -> Result<()> {
        let active = self.runtime();
        active.process_manager.resize(&active.process_owner_id, id, size)
    }

    pub fn process_signal(&self, id: &ProcessId, signal: ProcessSignal) -> Result<()> {
        let active = self.runtime();
        active.process_manager.signal(&active.process_owner_id, id, signal)
    }

    pub async fn process_stop(&self, id: &ProcessId, grace: Option<Duration>) -> Result<ProcessInfo> {
        let active = self.runtime();
        active.process_manager.stop(&active.process_owner_id, id, grace).await
    }

    pub async fn process_wait(&self, id: &ProcessId, timeout: Option<Duration>) -> Result<ProcessInfo> {
        let active = self.runtime();
        active.process_manager.wait(&active.process_owner_id, id, timeout).await
    }

    #[must_use]
    pub fn session_header(&self) -> Option<SessionStartedEvent> {
        self.runtime().session.session_header().map(|header| SessionStartedEvent {
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

    #[must_use]
    pub async fn state(&self) -> ApplicationState {
        let active = self.runtime();
        let session = &active.session;
        let (session_id, session_file) = session
            .recorder_info()
            .map_or((None, None), |(id, path)| {
                (Some(id), Some(path.to_string_lossy().into_owned()))
            });
        ApplicationState {
            model: session.model(),
            thinking_level: session.thinking_level(),
            is_streaming: self.is_streaming(),
            is_compacting: session.is_compacting(),
            steering_mode: session.steering_mode().await,
            follow_up_mode: session.follow_up_mode().await,
            session_file,
            session_id,
            session_name: session.session_name(),
            auto_compaction_enabled: session.auto_compaction_enabled(),
            message_count: session.history().len(),
            pending_message_count: session.pending_message_count().await,
            last_selection: session.last_selection(),
            todo_phases: session.todo_state().phases,
            goal: session.goal_runtime().get(),
        }
    }

    #[must_use]
    pub fn is_streaming(&self) -> bool {
        let active = self.runtime();
        active.loop_turn_active.load(Ordering::Acquire)
            || active.goal_work_pending.load(Ordering::Acquire) != 0
            || active.active_run.lock().as_ref().is_some_and(|run| !run.is_finished())
            || active.todo_cycle_pending.load(Ordering::Acquire)
    }

    pub async fn prompt(
        &self,
        message: String,
        images: Vec<ContentBlock>,
        streaming_behavior: Option<StreamingBehavior>,
    ) -> Result<()> {
        self.prompt_inner(message, images, streaming_behavior, true).await
    }

    async fn prompt_without_natural_language_spawn(
        &self,
        message: String,
        images: Vec<ContentBlock>,
        streaming_behavior: Option<StreamingBehavior>,
    ) -> Result<()> {
        self.prompt_inner(message, images, streaming_behavior, false).await
    }

    async fn prompt_inner(
        &self,
        message: String,
        images: Vec<ContentBlock>,
        streaming_behavior: Option<StreamingBehavior>,
        allow_natural_language_spawn: bool,
    ) -> Result<()> {
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
                crate::ExtensionInputReduction::Handled => return Ok(()),
            }
        } else {
            (message, images)
        };
        let active = self.runtime();
        if active.todo_continuation_suppressed.load(Ordering::Acquire) {
            active.todo_resume_requested.store(true, Ordering::Release);
        }
        if active.loop_turn_active.load(Ordering::Acquire) {
            return match streaming_behavior {
                Some(StreamingBehavior::Steer) => {
                    active.session.steer(user_message(message, images)).await
                }
                Some(StreamingBehavior::FollowUp) => {
                    active.session.follow_up(user_message(message, images)).await
                }
                None => Err(anyhow!(
                    "session is already processing; choose steer or followUp"
                )),
            };
        }
        let Ok(turn_guard) = active.turn_gate.clone().try_lock_owned() else {
            return match streaming_behavior {
                Some(StreamingBehavior::Steer) => {
                    active.session.steer(user_message(message, images)).await
                }
                Some(StreamingBehavior::FollowUp) => {
                    active.session.follow_up(user_message(message, images)).await
                }
                None => Err(anyhow!(
                    "session is already processing; choose steer or followUp"
                )),
            };
        };
        let session = active.session.clone();
        let inner = self.inner.clone();
        let selection = session.select_for_request(&message).await;
        // Explicit trusted-agent delegation spawns run FIRST: a named
        // delegation must not also trigger the auto-todo classifier, which
        // would duplicate/ambiguously orchestrate the same prompt. An
        // explicit but NEGATED delegation (`别让glm…`, `Do not have glm…`)
        // suppresses auto-todo outright — the user said NOT to run it — and
        // never reaches the hook or the spawn. A positive delegation passes
        // through the task authorization boundary — the composed
        // before_tool_call pipeline (settings pre_tool_call hooks + host
        // approval mode + extension hooks) runs against a synthetic `task`
        // call. A block (or rewritten arguments) suppresses the auto-spawn
        // AND the auto-todo classifier, so nothing bypasses the gate through
        // the todo path; the delegation stays a plain parent turn and the
        // model can still call `task` through the same gate. The pipeline is
        // always composed, so its presence cannot be the gate; its OUTCOME
        // is.
        let mut delegated_prompt = false;
        if let Some(runtime) = self.runtime().orchestration_runtime() {
            // Mixed polarity: a negated clause (`Don't have glm…`) suppresses
            // auto-todo, while a positive clause in the same prompt
            // (`; have grok review it`) still authorizes and spawns its own
            // mentions. The two signals are independent.
            delegated_prompt = runtime.has_explicit_negated_delegation(&message);
            if allow_natural_language_spawn
                && !runtime.delegated_mentions_in(&message).is_empty()
            {
                let authorized = match session.before_tool_call() {
                Some(hook) => {
                    // The synthetic call matches the REAL `task` tool
                    // boundary: the parent toolset's task definition carries
                    // the exact capability into the hook context, and its
                    // prepare_arguments fills the canonical null-shaped
                    // arguments (`{task, name, agent, todoTaskId, context,
                    // tasks, outputSchema, schemaMode}`) that a real task
                    // call presents to the pipeline.
                    let task_tool = runtime
                        .agent_tools(runtime.main_agent_id(), 0)
                        .into_iter()
                        .find(|tool| tool.name == "task");
                    match task_tool {
                        None => true,
                        Some(task_tool) => {
                            let raw = serde_json::json!({ "task": message.as_str() });
                            let arguments = match task_tool.prepare_arguments.as_ref() {
                                Some(prepare) => prepare(raw)?,
                                None => raw,
                            };
                            let context = pi_agent::BeforeToolCallContext {
                                assistant_message: pi_ai::AssistantMessage::pending(&pi_ai::Model::default()),
                                tool_call: pi_ai::ToolCall {
                                    id: "natural-language-spawn".to_owned(),
                                    name: "task".to_owned(),
                                    arguments: arguments.clone(),
                                    thought_signature: None,
                                },
                                arguments: arguments.clone(),
                                context: pi_agent::AgentContext {
                                    system_prompt: String::new(),
                                    messages: Vec::new(),
                                    tools: vec![task_tool],
                                },
                            };
                            let result = hook(context).await?;
                            if result.block {
                                false
                            } else {
                                // A pre-hook may rewrite the task arguments:
                                // the auto-spawn proceeds only when they are
                                // untouched (`None` = unchanged) or
                                // semantically equal to the synthetic call.
                                // A rewritten call is left to the normal
                                // model/tool path so the modification is
                                // honored exactly once, there.
                                result
                                    .arguments
                                    .as_ref()
                                    .is_none_or(|rewritten| rewritten == &arguments)
                            }
                        }
                    }
                }
                None => true,
            };
            if authorized {
                delegated_prompt = runtime
                    .spawn_from_natural_language(runtime.main_agent_id(), 0, &message)?
                    .is_some();
                if delegated_prompt {
                    active.todo_cycle_pending.store(true, Ordering::Release);
                }
            } else {
                // The hook blocked or rewrote the task call: the delegation
                // is not auto-spawned, and auto-todo must not orchestrate it.
                delegated_prompt = true;
            }
            }
        }
        // Deterministic auto-mode classifier (`selector.autoMode`):
        // - off: nothing.
        // - suggest: publish a status hint when a code task / goal is detected.
        // - auto: additionally seed and start a todo DAG for code tasks,
        //   bounded to orchestration being enabled and no todo list existing.
        // The parent turn always runs as usual; the mode drives hints and
        // optional todo orchestration, never the model loop itself.
        // Workflow-owned runs (supervisor prompts) never classify: the
        // workflow supervisor owns the canonical Todo DAG and user-facing
        // hints would be noise for internal prompts.
        let auto_mode = session.selector_settings().auto_mode;
        let workflow_owned = self
            .runtime()
            .orchestration_runtime()
            .is_some_and(|runtime| runtime.workflow_scope().is_some());
        if !workflow_owned && auto_mode.is_enabled() && !delegated_prompt {
            let mode = crate::selector::classify_prompt(&message);
            if let Some(hint) = crate::selector::mode_hint(mode) {
                inner.publish(ApplicationEvent::ModeDetected {
                    mode,
                    hint: hint.to_owned(),
                });
            }
            if auto_mode.is_auto()
                && mode == crate::PromptMode::CodeTask
                && session.todo_state().phases.is_empty()
                && active.runtime_settings.lock().orchestration_enabled
                && self.runtime().orchestration_runtime().is_some()
            {
                match self
                    .set_todos(crate::selector::auto_create_todo_phases(&message))
                    .and_then(|_| self.execute_todo_dag())
                {
                    Ok(_) => {
                        // DAG armed; reconcile spawned child jobs that run the
                        // task. TodoUpdated/orchestration events surface them.
                    }
                    Err(error) => {
                        inner.publish(ApplicationEvent::RunFailed {
                            message: format!("auto todo DAG creation failed: {error}"),
                        });
                    }
                }
            }
        }
        inner.publish(ApplicationEvent::Selection(selection));
        if session.todo_reminder_pending() {
            inner.publish(ApplicationEvent::TodoReminder {
                phases: session.todo_state().phases,
            });
        }
        let turn_guard = turn_guard;
        let goal_was_active = session
            .goal_runtime()
            .get()
            .current
            .is_some_and(|goal| goal.lifecycle == crate::GoalLifecycle::Active);
        inner.publish(ApplicationEvent::GoalContinuation {
            decision: session.goal_runtime().continuation_decision(),
        });
        let handle = tokio::spawn(async move {
            let started = Instant::now();
            match session.run(&message, images).await {
                Ok(result) => inner.finish_goal_turn(result.usage, started, goal_was_active),
                Err(error) => {
                    inner.publish(ApplicationEvent::RunFailed {
                        message: error.to_string(),
                    });
                    inner.finish_goal_turn(pi_ai::Usage::default(), started, goal_was_active);
                }
            }
            inner.finish_parent_turn();
            drop(turn_guard);
        });
        *active.active_run.lock() = Some(handle);
        Ok(())
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
        active.session.follow_up(user_message(message, images)).await
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
        active.todo_continuation_suppressed.store(true, Ordering::Release);
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
        if let Some(runtime) = active.extension_runtime() {
            let reduction = runtime
                .0
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
    async fn begin_todo_session_transition(&self) -> Result<()> {
        let active = self.runtime();
        active.todo_continuation_suppressed.store(true, Ordering::Release);
        active.todo_resume_requested.store(false, Ordering::Release);
        active.todo_cycle_pending.store(false, Ordering::Release);
        let orchestration = active.orchestration_runtime();
        let gate = active.todo_dag.lock().reconcile_gate.clone();
        let job_ids = {
            let _transaction = gate.lock();
            active.todo_transition_active.store(true, Ordering::Release);
            orchestration.as_ref().map_or_else(Vec::new, |runtime| {
                self.inner.begin_todo_dag_transition_locked(runtime)
            })
        };
        let Some(orchestration) = orchestration else {
            return Ok(());
        };
        orchestration.cancel_jobs_result(&job_ids)?;
        self.wait_todo_jobs_settled(&orchestration, job_ids).await
    }

    /// One fixed deadline for orchestration jobs to settle during a same-CWD
    /// session transition (switch/new/fork/navigate/clone): a wedged job must
    /// not block the transition forever.
    const TRANSITION_JOB_WAIT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

    async fn wait_todo_jobs_settled(
        &self,
        orchestration: &crate::OrchestrationRuntime,
        job_ids: Vec<String>,
    ) -> Result<()> {
        self.wait_todo_jobs_settled_with_deadline(
            orchestration,
            job_ids,
            Self::TRANSITION_JOB_WAIT_DEADLINE,
        )
        .await
    }

    /// Waits for `job_ids` to leave the queued/running states with one fixed
    /// deadline covering the whole wait (never reset inside the loop). On
    /// expiry the error names the unsettled job ids so the user can cancel
    /// them explicitly.
    async fn wait_todo_jobs_settled_with_deadline(
        &self,
        orchestration: &crate::OrchestrationRuntime,
        job_ids: Vec<String>,
        deadline: std::time::Duration,
    ) -> Result<()> {
        let start = std::time::Instant::now();
        let mut remaining = job_ids;
        while !remaining.is_empty() {
            let elapsed = start.elapsed();
            let Some(timeout) = deadline.checked_sub(elapsed) else {
                return Err(anyhow!(
                    "timed out after {}s waiting for orchestration jobs to settle before the session transition: {} — cancel them explicitly and retry",
                    deadline.as_secs_f64(),
                    remaining.join(", "),
                ));
            };
            orchestration.wait_jobs(&remaining, Some(timeout), None).await?;
            remaining.retain(|job_id| {
                orchestration
                    .jobs(Some(std::slice::from_ref(job_id)))
                    .first()
                    .is_some_and(|job| {
                        matches!(job.status, crate::JobStatus::Queued | crate::JobStatus::Running)
                    })
            });
        }
        Ok(())
    }

    fn finish_todo_session_transition(&self) {
        let active = self.runtime();
        let gate = active.todo_dag.lock().reconcile_gate.clone();
        let _transaction = gate.lock();
        active.todo_transition_active.store(false, Ordering::Release);
        if let Err(error) = self.inner.arm_todo_dag_after_mutation_locked() {
            self.inner.todo_dag_failed(&error);
        }
    }

    fn prepare_orchestration_cutover(
        &self,
        active: &runtime::ApplicationRuntime,
        session: &crate::PreparedSessionReplacement,
    ) -> Result<Option<(crate::OrchestrationRuntime, crate::PreparedDurableBinding)>> {
        let Some(orchestration) = active.orchestration_runtime() else {
            return Ok(None);
        };
        let (session_id, session_path) = session.recorder_info();
        let mut binding = orchestration.prepare_parent_identity(session_id, session_path)?;
        orchestration.initialize_prepared_parent(&mut binding)?;
        Ok(Some((orchestration, binding)))
    }

    async fn prepare_same_cwd_cutover(
        &self,
        active: &runtime::ApplicationRuntime,
        session: crate::PreparedSessionReplacement,
    ) -> Result<PreparedSameCwdCutover> {
        let (_, session_file) = session.recorder_info();
        let target_loops = crate::prepare_loop_activation(Some(&session_file)).await?;
        let current_file = active.session.recorder_info().map(|(_, path)| path);
        let current_loops = crate::prepare_loop_activation(current_file.as_deref()).await?;
        let orchestration = self.prepare_orchestration_cutover(active, &session)?;
        let handle = self.loop_handle()?;
        let loops = match handle
            .prepare_session_switch(
                crate::prepare_loop_session_switch(target_loops),
                crate::LoopRemovalReason::SessionChanged,
            )
            .await
        {
            Ok(loops) => loops,
            Err(error) => {
                drop(orchestration);
                return Err(error.into());
            }
        };
        if self.inner.cleaned.load(Ordering::Acquire) {
            handle.restore_prepared_session(current_loops);
            drop(orchestration);
            return Err(anyhow!("application shut down during session cutover"));
        }
        Ok(PreparedSameCwdCutover {
            session,
            orchestration,
            loop_handle: handle,
            loops,
        })
    }

    async fn commit_same_cwd_cutover(
        &self,
        active: &runtime::ApplicationRuntime,
        prepared: PreparedSameCwdCutover,
    ) -> Result<()> {
        let PreparedSameCwdCutover {
            session,
            orchestration,
            loop_handle,
            loops,
        } = prepared;
        // The replacement commit reaps the outgoing session's MCP clients
        // (awaited shutdown) before swapping any state. If that fails nothing
        // was committed, so restore the suspended loop schedule and drop the
        // prepared orchestration binding, keeping the current session fully
        // operational instead of half-switched.
        if let Err(error) = active.session.commit_session_replacement(session).await {
            loop_handle.restore_prepared_session(loops);
            return Err(error);
        }
        if let Some((orchestration, binding)) = orchestration {
            orchestration.commit_prepared_parent(binding);
        }
        loop_handle.commit_prepared_session_switch(loops);
        active.charged_goal_jobs.lock().clear();
        self.finish_todo_session_transition();
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
        let target_cwd = prepared
            .target_cwd()
            .canonicalize()
            .with_context(|| format!("resolving resumed working directory {}", prepared.target_cwd().display()))?;
        let active = self.runtime();
        if target_cwd == active.session.cwd() {
            let _operation = self.inner.operation_gate.lock().await;
            active.session.wait_for_idle().await;
            if let Err(error) = self.begin_todo_session_transition().await {
                self.finish_todo_session_transition();
                return Err(error);
            }
            let session = match active.session.prepare_resume_replacement(prepared).await {
                Ok(session) => session,
                Err(error) => {
                    self.finish_todo_session_transition();
                    return Err(error);
                }
            };
            let goal = session.goal_runtime();
            if let Some(current) = goal.get().current
                && current.lifecycle == crate::GoalLifecycle::Active
                && let Err(error) = goal.pause_on_resume()
            {
                self.finish_todo_session_transition();
                return Err(anyhow!(error.to_string()));
            }
            let prepared = match self.prepare_same_cwd_cutover(&active, session).await {
                Ok(prepared) => prepared,
                Err(error) => {
                    self.finish_todo_session_transition();
                    return Err(error);
                }
            };
            active.process_manager.shutdown_owner(&active.process_owner_id).await;
            if let Err(error) = self.commit_same_cwd_cutover(&active, prepared).await {
                self.finish_todo_session_transition();
                return Err(error);
            }
            return Ok(SessionChangeOutcome::COMPLETED);
        }

        let factory = self
            .inner
            .runtime_factory
            .lock()
            .clone()
            .ok_or_else(|| anyhow!("cross-directory session switching requires an application runtime factory"))?;
        let mut options = active.session.child_session_options_snapshot();
        options.cwd.clone_from(&target_cwd);
        let session_options = crate::SessionOptions {
            model: options.model,
            cwd: options.cwd,
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
        };
        let prepared_loops = crate::prepare_loop_activation(Some(&target_session_file)).await?;
        let candidate = factory
            .build_runtime_candidate(target_cwd, session_options, Some(prepared))
            .await?;
        candidate.session.set_session_dir(active.session.session_dir());

        let _operation = self.inner.operation_gate.lock().await;
        active.goal_work_activation.lock().take();
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
        let loops = self.loop_handle()?;
        let epoch = self.inner.runtime.next_epoch();
        let next = Arc::new(candidate.activate(epoch));
        self.bind_runtime_generation(next.clone()).await?;
        loops
            .commit_session_switch(prepared_loops, crate::LoopRemovalReason::SessionChanged)
            .await?;
        let old = self.inner.runtime.replace_arc(next);

        old.process_manager.shutdown_owner(&old.process_owner_id).await;
        let orchestration_task = old.orchestration_events.lock().take();
        let orchestration = old.orchestration_runtime.lock().take();
        if let Some(task) = orchestration_task {
            task.abort();
        }
        if let Some(orchestration) = orchestration {
            orchestration.shutdown().await;
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
        old.session_subscription.lock().take();
        if let Some((runtime, _)) = extension {
            runtime.shutdown_with_reason("session_switch").await;
        }
        Ok(SessionChangeOutcome::COMPLETED)
    }
    pub async fn new_session(&self) -> Result<SessionChangeOutcome> {
        self.new_session_with_parent(None).await
    }

    pub async fn new_session_with_parent(&self, parent_session: Option<&Path>) -> Result<SessionChangeOutcome> {
        if let Some(runtime) = self.extension_runtime()
            && runtime
                .reduce_before_switch(serde_json::json!({ "reason": "new" }))
                .await?
        {
            return Ok(SessionChangeOutcome::CANCELLED);
        }
        let _operation = self.inner.operation_gate.lock().await;
        let active = self.runtime();
        self.wait_for_idle().await;
        if let Err(error) = self.begin_todo_session_transition().await {
            self.finish_todo_session_transition();
            return Err(error);
        }
        let session = match active
            .session
            .prepare_new_session_replacement(parent_session)
        {
            Ok(session) => session,
            Err(error) => {
                self.finish_todo_session_transition();
                return Err(error);
            }
        };
        let prepared = match self.prepare_same_cwd_cutover(&active, session).await {
            Ok(prepared) => prepared,
            Err(error) => {
                self.finish_todo_session_transition();
                return Err(error);
            }
        };
        active.process_manager.shutdown_owner(&active.process_owner_id).await;
        if let Err(error) = self.commit_same_cwd_cutover(&active, prepared).await {
            self.finish_todo_session_transition();
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
        let _operation = self.inner.operation_gate.lock().await;
        let active = self.runtime();
        let source_goal = active.session.goal_runtime().get().current;
        let restore_conversation = if let Some((runtime, _)) = active.extension_runtime() {
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
        if let Err(error) = self.begin_todo_session_transition().await {
            self.finish_todo_session_transition();
            return Err(error);
        }
        self.inner.publish(ApplicationEvent::SessionBeforeFork(SessionBeforeForkEvent { target_id: entry_id.to_owned() }));
        let (session, editor_text) = match active
            .session
            .prepare_fork_replacement(entry_id, restore_conversation)
        {
            Ok(prepared) => prepared,
            Err(error) => {
                self.finish_todo_session_transition();
                return Err(error);
            }
        };
        let goal = session.goal_runtime();
        if let Some(source_goal) = source_goal
            && let Err(error) = goal.fork_clone(&source_goal)
        {
            self.finish_todo_session_transition();
            return Err(anyhow!(error.to_string()));
        }
        let (session_id, session_file) = session.recorder_info();
        let prepared = match self.prepare_same_cwd_cutover(&active, session).await {
            Ok(prepared) => prepared,
            Err(error) => {
                self.finish_todo_session_transition();
                return Err(error);
            }
        };
        active.process_manager.shutdown_owner(&active.process_owner_id).await;
        if let Err(error) = self.commit_same_cwd_cutover(&active, prepared).await {
            self.finish_todo_session_transition();
            return Err(error);
        }
        if goal.get().current.is_some() {
            self.inner.publish(ApplicationEvent::GoalUpdated {
                operation: "fork_clone",
                state: goal.get(),
            });
        }
        self.inner.publish(ApplicationEvent::SessionForked(SessionForkedEvent { target_id: entry_id.to_owned(), session_id, session_file: session_file.to_string_lossy().into_owned(), editor_text: editor_text.clone() }));
        Ok(SessionForkOutcome::completed(editor_text))
    }

    pub async fn navigate_tree(&self, entry_id: &str, options: crate::NavigateTreeOptions) -> Result<crate::NavigateTreeResult> {
        let mut options = options;
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
                let active_leaf_id = self.runtime().session.session_tree()?.active_leaf_id;
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
        if let Err(error) = self.begin_todo_session_transition().await {
            self.finish_todo_session_transition();
            return Err(error);
        }
        self.inner.publish(ApplicationEvent::SessionBeforeTree(SessionBeforeTreeEvent { target_id: entry_id.to_owned(), summarize: options.summarize }));
        let result = match self.runtime().session.navigate_tree(entry_id, options).await {
            Ok(result) => result,
            Err(error) => {
                self.finish_todo_session_transition();
                return Err(error);
            }
        };
        self.inner.publish(ApplicationEvent::SessionTree(SessionTreeEvent { target_id: entry_id.to_owned(), active_leaf_id: result.active_leaf_id.clone(), editor_text: result.editor_text.clone(), summary_entry_id: result.summary_entry_id.clone(), changed: result.changed, cancelled: result.cancelled }));
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
        self.finish_todo_session_transition();
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
        active.session.rewind(target).await
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
                matches!(job.status, crate::JobStatus::Queued | crate::JobStatus::Running)
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
        let _operation = self.inner.operation_gate.lock().await;
        let active = self.runtime();
        let source_goal = active.session.goal_runtime().get().current;
        let target_id = active
            .session
            .session_tree()?
            .active_leaf_id
            .ok_or_else(|| anyhow!("Cannot clone session: no current entry selected"))?;
        let restore_conversation = if let Some((runtime, _)) = active.extension_runtime() {
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
        if let Err(error) = self.begin_todo_session_transition().await {
            self.finish_todo_session_transition();
            return Err(error);
        }
        let session = match active
            .session
            .prepare_clone_replacement(&target_id, restore_conversation)
        {
            Ok(session) => session,
            Err(error) => {
                self.finish_todo_session_transition();
                return Err(error);
            }
        };
        let goal = session.goal_runtime();
        if let Some(source_goal) = source_goal
            && let Err(error) = goal.fork_clone(&source_goal)
        {
            self.finish_todo_session_transition();
            return Err(anyhow!(error.to_string()));
        }
        let prepared = match self.prepare_same_cwd_cutover(&active, session).await {
            Ok(prepared) => prepared,
            Err(error) => {
                self.finish_todo_session_transition();
                return Err(error);
            }
        };
        active.process_manager.shutdown_owner(&active.process_owner_id).await;
        if let Err(error) = self.commit_same_cwd_cutover(&active, prepared).await {
            self.finish_todo_session_transition();
            return Err(error);
        }
        if goal.get().current.is_some() {
            self.inner.publish(ApplicationEvent::GoalUpdated {
                operation: "fork_clone",
                state: goal.get(),
            });
        }
        Ok(SessionChangeOutcome::COMPLETED)
    }

    pub fn fork_messages(&self) -> Result<Vec<crate::ForkMessage>> {
        self.runtime().session.fork_messages()
    }

    pub fn session_entries(&self, since: Option<&str>) -> Result<crate::SessionEntries> {
        self.runtime().session.session_entries(since)
    }

    pub fn session_tree(&self) -> Result<crate::SessionTreeResult> {
        self.runtime().session.session_tree()
    }

    pub fn export_html(&self, output: Option<&Path>) -> Result<PathBuf> {
        crate::export_live_session(&self.runtime().session, output, &crate::ExportOptions::default())
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
            commands.extend(runtime.command_sources().into_iter().map(|source| crate::ApplicationCommand {
                name: source.command.name,
                description: source.command.description,
                source: "extension".to_owned(),
                source_info: crate::CommandSourceInfo {
                    path: source.path.to_string_lossy().into_owned(),
                    source: source.source,
                    scope: source.scope,
                    origin: source.origin,
                    base_dir: source.base_dir.map(|path| path.to_string_lossy().into_owned()),
                },
            }));
        }
        if let Some(snapshot) = self.resource_snapshot() {
            commands.extend(snapshot.prompts.iter().map(|prompt| crate::ApplicationCommand {
                name: prompt.name.clone(),
                description: (!prompt.description.is_empty()).then(|| prompt.description.clone()),
                source: "prompt".to_owned(),
                source_info: crate::CommandSourceInfo::local(
                    &prompt.file_path,
                    match prompt.scope {
                        crate::ResourceScope::Global => "user",
                        crate::ResourceScope::Project => "project",
                        crate::ResourceScope::Explicit => "temporary",
                    },
                ),
            }));
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
        if let Some(template) = snapshot.prompts.iter().find(|template| template.name == name) {
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
        let path = crate::resolve_skill_uri(&uri, &snapshot.skills)?;
        let content = crate::read_resource_text(&path, "skill command")?;
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
        let enabled = settings.orchestration_enabled
            || active.orchestration_explicit.load(Ordering::Acquire);
        if !enabled {
            return Ok(None);
        }
        let mut config = crate::OrchestrationConfig::new(
            crate::AgentCatalog::from_agents(snapshot.agents.clone()),
            snapshot.cwd.join(".pi").join("artifacts"),
        );
        config.skills = snapshot.skills.iter().map(crate::OrchestrationSkill::from).collect();
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
        config = config.with_selector_settings(snapshot.settings.selector.clone().unwrap_or_default());
        config.agent_settings = snapshot.settings.agents.clone();
        if let Some(model) = active.session.model() {
            config.parent_model = model;
        }
        // Live parent model so /model switches apply to child resolution without rebuild.
        let session_for_parent = active.session.clone();
        config = config.with_parent_model_provider(std::sync::Arc::new(move || {
            session_for_parent.model().unwrap_or_default()
        }));
        let resolver_slot = Arc::new(Mutex::new(None::<crate::InternalUriResolverFn>));
        let child_resolver_slot = resolver_slot.clone();
        let resolver: crate::InternalUriResolverFn = Arc::new(move |uri| {
            child_resolver_slot
                .lock()
                .as_ref()
                .ok_or_else(|| anyhow!("orchestration URI resolver is not initialized"))?(uri)
        });
        let factory = crate::OrchestrationRuntime::child_factory_from_session_and_uri(
            &active.session,
            Some(resolver),
        );
        let runtime = crate::OrchestrationRuntime::new(config, factory)?;
        if let Some(current) = self.runtime().orchestration_runtime()
            && current.runtime_equivalent(&runtime)
        {
            return Ok(Some(current));
        }
        *resolver_slot.lock() = Some(runtime.read_uri_resolver());
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
        if !same_runtime
            && let Some(runtime) = &candidate
        {
            // Replacing a live runtime is a deliberate reconfiguration, not a
            // crash recovery: the previous runtime's jobs are cancelled by its
            // own shutdown, so the replacement starts from a clean durable
            // state. A first-time enable (no live previous) still recovers any
            // existing sidecar so resumed sessions continue their jobs.
            if active.orchestration_runtime().is_some() {
                runtime
                    .bind_and_reset(&active.session)
                    .context("resetting replacement orchestration runtime")?;
            } else {
                runtime
                    .bind_and_recover(&active.session)
                    .context("binding replacement orchestration runtime")?;
            }
        }
        let next_runtime = candidate.as_ref().cloned();
        let (previous, runtime_changed) = {
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
            if let Some(task) = active.orchestration_events.lock().take() {
                task.abort();
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
                                let Some(inner) = event_inner.upgrade() else { break; };
                                let active = inner.runtime();
                                if active.epoch() != epoch { break; }
                                let allow_spawn = !active.todo_continuation_suppressed.load(Ordering::Acquire);
                                if let Err(error) = inner.observe_orchestration_event(&event_runtime, &event, allow_spawn) {
                                    inner.todo_dag_failed(&error);
                                }
                                inner.steer_main_delivered_message(&active, &event).await
                                    .unwrap_or_else(|error| inner.report_steer_main_delivery_failure(&event, &error));
                                let _ = inner.runtime.publish(epoch, ApplicationEvent::Orchestration(event));
                                inner.finish_todo_cycle_if_idle(Some(&event_runtime), false);
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                let Some(inner) = event_inner.upgrade() else { break; };
                                let active = inner.runtime();
                                if active.epoch() != epoch { break; }
                                for event in event_runtime.presentation_events() {
                                    let allow_spawn = !active.todo_continuation_suppressed.load(Ordering::Acquire);
                                    if let Err(error) = inner.observe_orchestration_event(&event_runtime, &event, allow_spawn) {
                                        inner.todo_dag_failed(&error);
                                    }
                                    // `presentation_events` is a UI snapshot
                                    // (jobs and agents), never an unconsumed
                                    // context queue: historical
                                    // `MessageDelivered` must NOT be steered
                                    // into the main session here.
                                    let _ = inner.runtime.publish(epoch, ApplicationEvent::Orchestration(event));
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
            // `previous.shutdown()` persists the prior runtime's final state
            // to the shared per-parent-session sidecar (runtime.rs:3479),
            // undoing the replacement's clean reset and letting a later
            // restart recover pre-reload jobs. The replacement is now the sole
            // durable owner: this post-shutdown sync closes the ownership race
            // by re-writing the sidecar to the replacement's current (clean)
            // state. The pre-swap bind/reset already validated the durable
            // path, so a clean sidecar is part of the reload contract and a
            // failure here must fail the reload rather than leave stale state.
            if let Some(runtime) = &next_runtime {
                runtime
                    .persist_durable_state()
                    .context("persisting replacement orchestration runtime")?;
            }
        }
        Ok(())
    }

    /// Resolve project trust for the active session through the fail-open
    /// hook surfaces — the `pre_trust_decision` host hook and the
    /// `trust_decision` extension event — and compose their outcomes with
    /// [`crate::trust::apply_trust_hook_outcomes`] before the decision is
    /// recorded in a resource build.
    ///
    /// The observation fires exactly when a stored decision is consulted: a
    /// project with trust-gated resources and no one-run override. The host
    /// hook is fail-open by contract (`fail_closed` entries deny on failure);
    /// extension errors also fail open — no approval — so hook failures never
    /// crash trust resolution. The never-weaken invariant holds: a stored or
    /// default denial survives every hook recommendation, an extension
    /// approval upgrades only an undecided (`ask`) tentative decision, and a
    /// host block always denies.
    pub async fn resolve_project_trust_with_hooks(&self) -> Result<crate::TrustResolution> {
        let active = self.runtime();
        let resources = active
            .session
            .resource_manager()
            .ok_or_else(|| anyhow!("session has no resource manager"))?;
        let options = resources.options();
        let default_trust = resources
            .settings_manager()
            .settings()
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
            // Override or resource-less project: no stored decision is
            // consulted, so no hook observation fires.
            return Ok(resolution);
        };
        let payload = observation.to_payload();
        let host_blocked = active
            .session
            .fire_trust_decision_hook(
                &observation.path.to_string_lossy(),
                observation.decision.as_str(),
                observation.is_new,
            )
            .await
            .block;
        let extension_runtime = active.extension_runtime.lock().clone();
        let extension_approved = match extension_runtime {
            Some((runtime, _)) => match runtime.reduce_trust_decision(payload).await {
                Ok(reduction) => reduction.is_some_and(|reduction| reduction.approve),
                Err(error) => {
                    eprintln!(
                        "extensions: trust_decision reduction failed, failing open: {error:#}"
                    );
                    false
                }
            },
            None => false,
        };
        resolution.decision = crate::trust::apply_trust_hook_outcomes(
            observation.decision,
            host_blocked,
            extension_approved,
        );
        Ok(resolution)
    }

    /// Resolve the initial trust decision through the fail-open hook surfaces
    /// and, when the composed decision differs from the initial resource
    /// build, re-stage the resources with it so the snapshot records exactly
    /// what the hooks decided. Never crashes startup: hook failures fail
    /// open, and a session without attached resources has nothing to resolve.
    ///
    /// The session blueprint fires the host `pre_trust_decision` hook BEFORE
    /// loading any project extension (P0 ordering) and records the composed
    /// decision here; when present, the host hook has already fired once at
    /// startup and must not re-fire. The extension `trust_decision` event is
    /// still skipped in that case: project extensions only load after the
    /// host boundary passed, and a trusted store decision (the only case that
    /// loads them) cannot be upgraded by an extension approval.
    async fn resolve_startup_trust(&self, generation: &runtime::ApplicationRuntime) {
        let Some(resources) = generation.session.resource_manager() else {
            return;
        };
        let composed = match resources.take_startup_composed_trust() {
            Some(composed) => composed,
            None => match self.resolve_project_trust_with_hooks().await {
                Ok(composed) => composed,
                Err(error) => {
                    eprintln!(
                        "trust: resolving startup trust through hook surfaces failed, \
                         keeping the initial resource build: {error:#}"
                    );
                    return;
                }
            },
        };
        if composed.decision == resources.snapshot().trust.decision {
            return;
        }
        resources.set_composed_trust(Some(composed));
        let candidate = match resources.stage_reload() {
            Ok(candidate) => candidate,
            Err(error) => {
                eprintln!(
                    "trust: re-staging resources for the hook-composed startup decision: {error:#}"
                );
                return;
            }
        };
        if let Err(error) = resources.commit_reload(candidate) {
            eprintln!(
                "trust: committing the hook-composed startup decision: {error:#}"
            );
            return;
        }
        let session = generation.session.clone();
        if let Err(error) = session.attach_resources(resources).await {
            eprintln!(
                "trust: re-attaching resources after the hook-composed startup decision: {error:#}"
            );
        }
    }

    /// The resource snapshot, extension registry, and agent tool set are all staged
    /// before the active resource and extension generations are replaced.
    pub async fn reload(&self) -> Result<crate::ReloadResult> {
        self.wait_for_idle().await;
        let composed = self.resolve_project_trust_with_hooks().await?;
        self.reload_with_composed_trust(composed).await
    }

    /// Continue a reload with an already-composed trust decision whose hook
    /// surfaces have already fired. The decision is fed into the resource
    /// build so `build_candidate` records exactly what the fail-open hook
    /// surfaces decided without re-firing them.
    async fn reload_with_composed_trust(
        &self,
        composed: crate::TrustResolution,
    ) -> Result<crate::ReloadResult> {
        let active = self.runtime();
        let resources = active
            .session
            .resource_manager()
            .ok_or_else(|| anyhow!("session has no resource manager"))?;
        resources.set_composed_trust(Some(composed));
        let resource_candidate = resources.stage_reload()?;
        let next_runtime_settings = Arc::new(resource_candidate.snapshot().settings.runtime_settings()?);
        let orchestration_candidate =
            self.orchestration_candidate(&resource_candidate.snapshot(), &next_runtime_settings)?;
        let extension_runtime = active.extension_runtime.lock().clone();
        let Some((runtime, permissions)) = extension_runtime else {
            let mut additional_tools = Vec::new();
            if let Some(orchestration) = &orchestration_candidate {
                additional_tools.extend(orchestration.agent_tools("Main", 0));
            }
            if let Some(binding) = active.goal_tool_binding.lock().as_ref() {
                additional_tools.push(binding.tool());
            }
            let update = active
                .session
                .prepare_resource_update(resource_candidate.snapshot(), additional_tools)?;
            let result = resources.commit_reload(resource_candidate)?;
            active.session.commit_resource_update(update).await;
            *active.runtime_settings.lock() = next_runtime_settings;
            self.commit_orchestration_candidate(orchestration_candidate).await?;
            return Ok(result);
        };
        let specs = resource_candidate.extension_specs(&permissions)?;
        let extension_candidate = runtime.stage_reload(specs).await;
        let report = extension_candidate.report();
        if !report.failures.is_empty() {
            runtime.discard_reload(extension_candidate).await;
            return Err(extension_load_report_error("extension reload", &report));
        }
        let mut additional_tools = extension_candidate.agent_tools(&runtime);
        if let Some(orchestration) = &orchestration_candidate {
            additional_tools.extend(orchestration.agent_tools("Main", 0));
        }
        if let Some(binding) = active.goal_tool_binding.lock().as_ref() {
            additional_tools.push(binding.tool());
        }
        let update = match active.session.prepare_resource_update(
            resource_candidate.snapshot(),
            additional_tools,
        ) {
            Ok(update) => update,
            Err(error) => {
                runtime.discard_reload(extension_candidate).await;
                return Err(error);
            }
        };
        if let Err(error) = runtime.prepare_reload(&extension_candidate).await {
            runtime.discard_reload(extension_candidate).await;
            return Err(error);
        }
        let result = resources.commit_reload(resource_candidate)?;
        runtime.commit_reload(extension_candidate)?;
        runtime.finish_reload("reload").await;
        active.session.commit_resource_update(update).await;
        *active.runtime_settings.lock() = next_runtime_settings;
        self.commit_orchestration_candidate(orchestration_candidate).await?;
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
        self.runtime().session.set_model_with_resolved_auth(model).await
    }

    pub fn set_thinking_level(&self, level: ThinkingLevel) -> crate::ThinkingLevelChange {
        self.runtime().session.set_thinking_level(level)
    }

    /// Stops every runtime owned by this application. Safe to call repeatedly;
    /// concurrent callers wait for the in-flight cleanup to finish.
    pub async fn cleanup(&self) {
        let _cleanup = self.inner.cleanup_lock.lock().await;
        if self.inner.cleaned.load(Ordering::Acquire) {
            return;
        }

        self.inner.cleaned.store(true, Ordering::Release);
        let active = self.runtime();
        active.session.abort().await;
        self.wait_for_idle().await;

        if let Some(task) = self.inner.workflow_events.lock().take() {
            task.abort();
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
        if let Some(task) = active.orchestration_events.lock().take() {
            task.abort();
        }
        let orchestration_runtime = active.orchestration_runtime.lock().take();
        if let Some(runtime) = orchestration_runtime {
            runtime.shutdown().await;
        }

        active.process_manager.shutdown_owner(&active.process_owner_id).await;
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
        if let Some(runtime) = runtime {
            runtime.shutdown_with_reason("cleanup").await;
        }
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
        crate::share::share_session_encrypted(&self.runtime().session, passphrase, output).await
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
        let events = self.inner.events.clone();
        // Offline contract: fail closed before spawning `gh` or touching the
        // network. `PI_OFFLINE` (process env or session env overlay) makes
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
            let result = crate::share::share_session(&session, &options).await;
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
                ExtensionRuntimeAction::SendMessage { message, delivery, trigger_turn } => {
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
                ExtensionRuntimeAction::AppendEntry { custom_type, data } => {
                    serde_json::to_value(application.session().append_custom_entry(&custom_type, data)?)
                        .map_err(Into::into)
                }
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
                    let canonical = pi_ai::get_model(&model.provider, &model.id)
                        .ok_or_else(|| anyhow!(
                            "extension model {}/{} is not registered",
                            model.provider,
                            model.id
                        ))?;
                    let _ = application
                        .set_model_with_resolved_auth(canonical)
                        .await?;
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
                    application.cleanup().await;
                    Ok(Value::Null)
                }
                ExtensionRuntimeAction::Compact { custom_instructions } => {
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
        active.todo_continuation_suppressed.store(false, Ordering::Release);
        active.todo_resume_requested.store(false, Ordering::Release);
        active.todo_cycle_pending.store(true, Ordering::Release);
        self.arm_todo_dag_locked(runtime, reset_attempts)
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
                    && matches!(task.status, crate::TodoStatus::Pending | crate::TodoStatus::InProgress)
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
        active.todo_continuation_suppressed.store(false, Ordering::Release);
        active.todo_resume_requested.store(false, Ordering::Release);
        let runtime = active.orchestration_runtime();
        let Some(runtime) = runtime else {
            return Ok(());
        };
        // Workflow children own their canonical Todo DAG through the
        // supervisor: a todo mutation must never silently arm DAG execution
        // (that raced the supervisor's own task-tool delegation, executing
        // every ready task twice — see BUG-1). The supervisor actor drives
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
    /// Advisory `agent_settled` extension event: the agent has no pending
    /// parent turn, todo cycle, or orchestration jobs. Fired exactly where
    /// `ApplicationEvent::AgentSettled` is published (the parent turn end and
    /// the todo-cycle drain), so extensions observe the same settle moments
    /// as the TUI/RPC stream. Best-effort: emitted on a spawned task because
    /// the settle points are synchronous.
    fn emit_agent_settled(&self) {
        let Some(runtime) = self.runtime().extension_runtime().map(|(runtime, _)| runtime) else {
            return;
        };
        let _ = tokio::spawn(async move {
            runtime
                .emit(ExtensionEvent::new("agent_settled", serde_json::json!({})))
                .await;
        });
    }

    fn finish_parent_turn(&self) {
        let active = self.runtime();
        if !active.todo_cycle_pending.load(Ordering::Acquire) {
            self.publish(ApplicationEvent::AgentSettled);
            self.emit_agent_settled();
            return;
        }
        if active.todo_resume_requested.swap(false, Ordering::AcqRel) {
            active.todo_continuation_suppressed.store(false, Ordering::Release);
        }
        let orchestration = active.orchestration_runtime();
        if !active.todo_continuation_suppressed.load(Ordering::Acquire)
            && let Some(runtime) = orchestration.as_ref()
        {
            let result = if self.has_ready_open_todo()
                && active.todo_dag.lock().status.is_terminal()
            {
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
            matches!(job.status, crate::JobStatus::Queued | crate::JobStatus::Running)
        })
    }

    fn todo_dag_failed(&self, error: &anyhow::Error) {
        let active = self.runtime();
        active.todo_dag.lock().status = TodoDagExecutionStatus::Blocked;
        active.todo_dag_changed.notify_waiters();
        self.publish(ApplicationEvent::RunFailed {
            message: format!("failed to reconcile Todo DAG execution: {error}"),
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
            && active.active_run.lock().as_ref().is_some_and(|run| !run.is_finished())
        {
            return;
        }
        if runtime.is_some_and(Self::has_active_jobs) {
            return;
        }
        if active.todo_continuation_suppressed.load(Ordering::Acquire) {
            let mut coordinator = active.todo_dag.lock();
            if coordinator.status == TodoDagExecutionStatus::Active {
                coordinator.status = TodoDagExecutionStatus::Blocked;
                active.todo_dag_changed.notify_waiters();
            }
        } else if active.todo_dag.lock().status == TodoDagExecutionStatus::Active {
            return;
        }
        if active.todo_cycle_pending.swap(false, Ordering::AcqRel) {
            self.publish(ApplicationEvent::AgentSettled);
            self.emit_agent_settled();
        }
    }

    fn finish_goal_turn(
        &self,
        parent_usage: pi_ai::Usage,
        started: Instant,
        goal_was_active: bool,
    ) {
        let active = self.runtime();
        self.finish_goal_turn_for_runtime(&active, parent_usage, started, goal_was_active);
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

fn usage_tokens(usage: &pi_ai::Usage) -> u64 {
    [usage.input, usage.output, usage.cache_read, usage.cache_write]
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
        active.process_manager.shutdown_owner_now(&active.process_owner_id);
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

        let session = active.session.clone();
        let active_run = active.active_run.lock().take();
        let loop_scheduler = self.loop_scheduler.get_mut().take();
        let orchestration_runtime = active.orchestration_runtime.lock().take();
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
                runtime.shutdown().await;
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
    match active.session.run_messages(vec![message]).await {
        Ok(result) => inner.finish_goal_turn_for_runtime(&active, result.usage, started, true),
        Err(error) => {
            clear_goal_work_activation(&active, &key);
            inner.publish(ApplicationEvent::RunFailed {
                message: error.to_string(),
            });
            inner.finish_goal_turn_for_runtime(
                &active,
                pi_ai::Usage::default(),
                started,
                true,
            );
        }
    }
    inner.finish_parent_turn();
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
            session.abort().await;
            match run.await {
                Ok(_) => Err("loop cancelled".to_owned()),
                Err(error) => Err(error.to_string()),
            }
        }
    };
    active.loop_turn_active.store(false, Ordering::Release);
    inner.finish_parent_turn();
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
        SessionEvent::SessionInfoChanged { name } => (
            "session_info_changed",
            serde_json::json!({ "name": name }),
        ),
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
    use super::{Application, ApplicationEvent, public_extension_model};
    use pi_ai::Model;
    use std::collections::HashMap;

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

    fn resource_application(
        global_settings: &str,
    ) -> (tempfile::TempDir, tempfile::TempDir, Application) {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::write(agent.path().join("settings.json"), global_settings)
            .expect("global settings");
        let mut options = crate::ResourceManagerOptions::new(cwd.path());
        options.agent_dir = agent.path().to_path_buf();
        options.project_trust_override = Some(true);
        let resources = crate::ResourceManager::new(options).expect("resources");
        let session = crate::Session::new(crate::SessionOptions {
            model: Model::default(),
            cwd: cwd.path().to_path_buf(),
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
        })
        .expect("session");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(session.attach_resources(resources)).expect("attach resources");
        let application = runtime.block_on(Application::new(session));
        (agent, cwd, application)
    }

    #[test]
    fn settings_apply_dispatches_live_reload_and_restart_truthfully() {
        let (agent, _cwd, application) = resource_application("{}");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");

        let mut live = application.settings_draft(crate::SettingsScope::Global).expect("live draft");
        live.set("compaction.enabled", serde_json::json!(false)).expect("live setting");
        let live = runtime.block_on(application.apply_settings_draft(live)).expect("live apply");
        assert!(live.applied_live);
        assert!(!live.reloaded);
        assert!(!live.restart_required);
        assert!(!application.runtime_settings().compaction.enabled);

        let mut reload = application.settings_draft(crate::SettingsScope::Global).expect("reload draft");
        reload.set("enableSkillCommands", serde_json::json!(false)).expect("reload setting");
        let reload = runtime.block_on(application.apply_settings_draft(reload)).expect("reload apply");
        assert!(reload.applied_live);
        assert!(reload.reloaded);
        assert!(!reload.restart_required);

        let generation = application.resource_generation().expect("generation");
        let mut restart = application.settings_draft(crate::SettingsScope::Global).expect("restart draft");
        restart.set("defaultProvider", serde_json::json!("future-provider")).expect("restart setting");
        let restart = runtime.block_on(application.apply_settings_draft(restart)).expect("restart apply");
        assert!(!restart.applied_live);
        assert!(!restart.reloaded);
        assert!(restart.restart_required);
        assert_eq!(application.resource_generation(), Some(generation));
        let mut mixed = application.settings_draft(crate::SettingsScope::Global).expect("mixed draft");
        mixed.set("defaultModel", serde_json::json!("future-model")).expect("restart field");
        mixed.set("compaction.enabled", serde_json::json!(true)).expect("live field");
        let mixed = runtime.block_on(application.apply_settings_draft(mixed)).expect("mixed apply");
        assert!(mixed.applied_live);
        assert!(!mixed.reloaded);
        assert!(mixed.restart_required);
        assert!(application.runtime_settings().compaction.enabled);

        let saved: serde_json::Value = serde_json::from_slice(
            &std::fs::read(agent.path().join("settings.json")).expect("settings file"),
        )
        .expect("settings json");
        assert_eq!(saved["defaultProvider"], "future-provider");
    }

    #[test]
    fn responses_stateful_chain_setting_reaches_runtime_and_session_stream_options() {
        // Regression: commit_runtime_settings reported success while the live
        // session kept the old chain mode (responses_stateful_chain was never
        // copied into the session stream options).
        let (_agent, _cwd, application) = resource_application("{}");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let mut draft = application.settings_draft(crate::SettingsScope::Global).expect("draft");
            draft
                .set("responsesStatefulChain", serde_json::json!(true))
                .expect("set chain true");
            let applied = application.apply_settings_draft(draft).await.expect("apply");
            assert!(applied.applied_live, "chain flag is a live setting");
            assert!(
                application.runtime_settings().stream_options.responses_stateful_chain,
                "runtime settings must carry the enabled chain flag"
            );
            assert!(
                application.runtime().session.stream_options().responses_stateful_chain,
                "the live session stream options must enable the chain"
            );

            let mut draft = application.settings_draft(crate::SettingsScope::Global).expect("draft");
            draft
                .set("responsesStatefulChain", serde_json::json!(false))
                .expect("set chain false");
            let applied = application.apply_settings_draft(draft).await.expect("apply");
            assert!(applied.applied_live);
            assert!(
                !application.runtime_settings().stream_options.responses_stateful_chain,
                "runtime settings must clear the chain flag"
            );
            assert!(
                !application.runtime().session.stream_options().responses_stateful_chain,
                "the live session stream options must clear the chain flag"
            );
        });
    }

    #[test]
    fn todo_transition_wait_bounds_wedged_jobs_with_deadline() {
        // Regression: the same-CWD session transition waited on orchestration
        // jobs with no deadline, so a wedged job blocked switch/new/fork/
        // navigate/clone forever. The fixed deadline must fire and the error
        // must name the unsettled job id.
        let cwd = tempfile::tempdir().expect("cwd");
        let session = crate::Session::new(crate::SessionOptions {
            model: Model::default(),
            cwd: cwd.path().to_path_buf(),
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
        })
        .expect("session");
        let task = crate::AgentDefinition {
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
        };
        let mut config = crate::OrchestrationConfig::new(
            crate::AgentCatalog::from_agents(vec![task]),
            cwd.path().join("artifacts"),
        );
        config.idle_ttl = None;
        // The child factory never resolves: the spawned job stays running
        // forever, simulating a wedged orchestration job.
        let factory: crate::ChildSessionFactory =
            std::sync::Arc::new(|_request| Box::pin(async move { std::future::pending().await }));
        let orchestration = crate::OrchestrationRuntime::new(config, factory).expect("orchestration");

        current_thread_runtime().block_on(async {
            session.start_new_recording().expect("start test recording");
            let application = Application::new(session).await;
            orchestration
                .bind_and_recover(&application.session())
                .expect("bind orchestration");
            let runtime = orchestration;
            let spawn = runtime
                .spawn_tasks(
                    "Main",
                    0,
                    vec![crate::TaskItem {
                        index: 0,
                        id: "Wedged".to_owned(),
                        agent: "task".to_owned(),
                        assignment: "never settles".to_owned(),
                        todo_task_id: None,
                        ..Default::default()
                    }],
                )
                .expect("spawn wedged job")
                .remove(0);
            let job_id = spawn.job_id;
            // Let the spawned child task progress past job registration so the
            // job is observably queued/running when the wait starts.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            let start = std::time::Instant::now();
            let error = application
                .wait_todo_jobs_settled_with_deadline(
                    &runtime,
                    vec![job_id.clone()],
                    std::time::Duration::from_millis(200),
                )
                .await
                .expect_err("a wedged job must hit the transition deadline");
            let elapsed = start.elapsed();
            assert!(
                elapsed < std::time::Duration::from_secs(5),
                "the fixed deadline must bound the whole wait: {elapsed:?}"
            );
            let message = format!("{error:#}");
            assert!(message.contains("timed out"), "actionable timeout error: {message}");
            assert!(
                message.contains(&job_id),
                "the error must list the unsettled job id: {message}"
            );
            runtime.shutdown().await;
        });
    }

    #[test]
    fn failed_settings_reload_never_claims_live_application() {
        let (_agent, cwd, application) = resource_application("{}");
        let project_settings = cwd.path().join(".pi/settings.json");
        std::fs::create_dir_all(project_settings.parent().expect("settings parent"))
            .expect("project settings dir");
        std::fs::write(&project_settings, "{ malformed")
            .expect("broken project settings");
        let generation = application.resource_generation();
        let mut draft = application.settings_draft(crate::SettingsScope::Global).expect("draft");
        draft.set("enableSkillCommands", serde_json::json!(false)).expect("setting");
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

    /// Application with orchestration enabled in the global settings file and
    /// a real [`crate::OrchestrationRuntime`] attached (stub child factory —
    /// the DAG-creation contract is synchronous; child turns are never run).
    fn orchestration_application(
        global_settings: &str,
    ) -> (tempfile::TempDir, tempfile::TempDir, Application) {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::write(agent.path().join("settings.json"), global_settings)
            .expect("global settings");
        let mut options = crate::ResourceManagerOptions::new(cwd.path());
        options.agent_dir = agent.path().to_path_buf();
        options.project_trust_override = Some(true);
        let resources = crate::ResourceManager::new(options).expect("resources");
        let session = crate::Session::new(crate::SessionOptions {
            model: Model::default(),
            cwd: cwd.path().to_path_buf(),
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
        })
        .expect("session");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime
            .block_on(session.attach_resources(resources))
            .expect("attach resources");
        let task = crate::AgentDefinition {
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
        };
        let mut config =
            crate::OrchestrationConfig::new(crate::AgentCatalog::from_agents(vec![task]), cwd.path().join("artifacts"));
        config.idle_ttl = None;
        let factory: crate::ChildSessionFactory = std::sync::Arc::new(|_request| {
            Box::pin(async move { Err(anyhow::anyhow!("child factory is not exercised")) })
        });
        let orchestration = crate::OrchestrationRuntime::new(config, factory).expect("orchestration");
        let application = runtime.block_on(Application::new_with_orchestration(session, orchestration));
        (agent, cwd, application)
    }

    #[test]
    fn orchestration_reload_candidate_keeps_persisted_preferred_agent() {
        let (_agent, _cwd, application) = orchestration_application(
            r#"{"orchestration":{"tasks":true,"preferredAgent":"task"}}"#,
        );
        let active = application.runtime();
        let snapshot = active
            .session
            .resource_manager()
            .expect("resources")
            .snapshot();
        let settings = snapshot.settings.runtime_settings().expect("runtime settings");
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

    fn current_thread_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    #[test]
    fn auto_mode_suggest_publishes_hint_without_creating_todos() {
        let (_agent, _cwd, application) = resource_application("{}");
        current_thread_runtime().block_on(async {
            let mut events = application.subscribe();
            application
                .prompt("implement a parser in src/lib.rs".to_owned(), Vec::new(), None)
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
    fn auto_mode_auto_creates_and_starts_todo_dag_for_code_task() {
        let (_agent, _cwd, application) = orchestration_application(
            r#"{"orchestration": {"tasks": true, "todo": true}, "selector": {"autoMode": "auto"}}"#,
        );
        current_thread_runtime().block_on(async {
            assert!(application.runtime_settings().orchestration_enabled);
            let mut events = application.subscribe();
            application
                .prompt("implement a parser in src/lib.rs".to_owned(), Vec::new(), None)
                .await
                .expect("prompt");
            let (mode, hint) = wait_for_mode_detected(&mut events)
                .await
                .expect("auto mode still publishes the hint");
            assert_eq!(mode, crate::PromptMode::CodeTask);
            assert_eq!(hint, "Detected: code task — /todo to plan");
            let phases = application.todo_state().phases;
            assert_eq!(phases.len(), 1, "auto mode must seed a todo DAG");
            assert_eq!(phases[0].name, "Plan");
            assert_eq!(phases[0].tasks.len(), 1);
            assert_eq!(phases[0].tasks[0].content, "implement a parser in src/lib.rs");
            let jobs = application
                .orchestration_runtime()
                .expect("orchestration attached")
                .jobs(None);
            assert!(
                !jobs.is_empty(),
                "auto mode must spawn orchestration jobs for the seeded task"
            );
        });
    }

    #[test]
    fn auto_mode_auto_respects_existing_todos() {
        let (_agent, _cwd, application) = orchestration_application(
            r#"{"orchestration": {"tasks": true, "todo": true}, "selector": {"autoMode": "auto"}}"#,
        );
        current_thread_runtime().block_on(async {
            application
                .set_todos(vec![crate::TodoPhase {
                    name: "Existing".to_owned(),
                    tasks: vec![crate::TodoItem {
                        id: "existing-1".to_owned(),
                        content: "keep this task".to_owned(),
                        status: crate::TodoStatus::Pending,
                        depends_on: Vec::new(),
                        ready: true,
                        blocked_by: Vec::new(),
                        agent: None,
                    }],
                }])
                .expect("seed existing todos");
            let mut events = application.subscribe();
            application
                .prompt("implement a parser in src/lib.rs".to_owned(), Vec::new(), None)
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
        });
    }

    #[test]
    fn auto_mode_off_publishes_no_mode_event() {
        let (_agent, _cwd, application) =
            resource_application(r#"{"selector": {"autoMode": "off"}}"#);
        current_thread_runtime().block_on(async {
            let mut events = application.subscribe();
            application
                .prompt("implement a parser in src/lib.rs".to_owned(), Vec::new(), None)
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
        let (_agent, _cwd, application) = resource_application("{}");
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
        }
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
            let refusal = Application::rewind_refusal(&[job("live", status)], &[]).expect("refusal");
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
            let refusal = Application::rewind_refusal(&[], &[workflow("live", status)]).expect("refusal");
            assert!(refusal.contains("rewind refused"), "{refusal}");
            assert!(refusal.contains("workflow(s)"), "{refusal}");
            assert!(refusal.contains("live"), "{refusal}");
        }
    }
}
