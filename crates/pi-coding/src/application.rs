mod todo_execution;
mod runtime;
mod workflows;
mod workflow_backend;
mod workflow_events;
pub use todo_execution::{TodoDagExecutionOutcome, TodoDagExecutionStatus};
pub use runtime::ApplicationRuntimeCandidate;
pub use workflows::ApplicationWorkflowRuntimeFactory;

use std::{collections::HashSet, path::{Path, PathBuf}, sync::{Arc, Weak, atomic::{AtomicBool, AtomicUsize, Ordering}}, time::{Duration, Instant}};

use anyhow::{Context, Result, anyhow};
use parking_lot::Mutex;
use pi_agent::{AgentEvent, AgentTool, AgentToolResult, Subscription, ThinkingLevel};
use pi_ai::{ContentBlock, CustomMessage, Message, Model, Schema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{sync::{Mutex as AsyncMutex, Notify, OwnedMutexGuard, broadcast}, task::JoinHandle};

use crate::{
    CompactionReason, ExtensionActionHost, ExtensionCancellation, ExtensionCommandDescriptor,
    ExtensionContextSnapshot, ExtensionContextUsage, ExtensionEvent, ExtensionFuture,
    ExtensionInstanceId, ExtensionMessageDelivery, ExtensionPermissionSet, ExtensionRuntime,
    ExtensionRuntimeAction, Goal, GoalContinuationDecision, GoalError, GoalState, GoalUsageDelta,
    MessageDelivery, ProcessEvent, ProcessId, ProcessInfo, ProcessKey, ProcessLogs, ProcessManager,
    ProcessOwnerId, ProcessSignal, ProcessSpawnSpec, ProcessTerminalSize, Session, SessionEvent,
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
}

impl Application {
    fn runtime(&self) -> Arc<runtime::ApplicationRuntime> {
        self.inner.runtime()
    }
}

struct ApplicationInner {
    session: Session,
    events: broadcast::Sender<ApplicationEvent>,
    runtime_factory: Mutex<Option<Arc<dyn ApplicationRuntimeFactory>>>,
    runtime: runtime::ApplicationRuntimeSlot,
    session_subscription: Mutex<Option<Subscription>>,
    active_run: Mutex<Option<JoinHandle<()>>>,
    extension_runtime: Mutex<Option<(ExtensionRuntime, ExtensionPermissionSet)>>,
    todo_transition_active: AtomicBool,
    orchestration_runtime: Mutex<Option<crate::OrchestrationRuntime>>,
    orchestration_events: Mutex<Option<JoinHandle<()>>>,
    workflow_manager: Mutex<Option<crate::WorkflowManager>>,
    workflow_runtime_factory: Mutex<Option<Weak<workflows::ApplicationWorkflowRuntimeFactory>>>,
    workflow_events: Mutex<Option<JoinHandle<()>>>,
    todo_dag: Mutex<todo_execution::TodoDagCoordinator>,
    todo_dag_changed: Notify,
    todo_cycle_pending: AtomicBool,
    todo_continuation_suppressed: AtomicBool,
    todo_resume_requested: AtomicBool,
    charged_goal_jobs: Mutex<HashSet<String>>,
    goal_tool_binding: Mutex<Option<GoalToolBinding>>,
    goal_work_activation: Mutex<Option<GoalWorkKey>>,
    goal_work_pending: AtomicUsize,
    goal_work_changed: Notify,
    orchestration_explicit: AtomicBool,
    runtime_settings: Mutex<Arc<crate::RuntimeSettingsSnapshot>>,
    process_manager: ProcessManager,
    process_owner_id: ProcessOwnerId,
    process_events: Mutex<Option<JoinHandle<()>>>,
    session_events: Mutex<Option<JoinHandle<()>>>,
    loop_scheduler: Mutex<Option<crate::LoopSchedulerRuntime>>,
    turn_gate: Arc<AsyncMutex<()>>,
    loop_turn_active: AtomicBool,
    operation_gate: AsyncMutex<()>,
    cleanup_lock: AsyncMutex<()>,
    cleaned: AtomicBool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GoalWorkKey {
    goal_id: String,
    revision: u64,
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
            application.attach_orchestration(runtime)?;
        }
        if let Some(binding) = goal_tool_binding {
            application.attach_goal_tool(binding)?;
        }
        Ok(application)
    }

    pub async fn new_with_orchestration(
        session: Session,
        runtime: crate::OrchestrationRuntime,
    ) -> Self {
        let application = Self::build(session, None).await;
        application.attach_orchestration(runtime).expect("fresh application accepts orchestration");
        application
            .inner
            .orchestration_explicit
            .store(true, Ordering::Release);
        application
    }

    async fn build(
        session: Session,
        extension_runtime: Option<(ExtensionRuntime, ExtensionPermissionSet)>,
    ) -> Self {
        let (events, _) = broadcast::channel(EVENT_BUFFER_CAPACITY);
        let process_manager = session.process_manager();
        let process_owner_id = session.process_owner_id();
        let runtime_settings = session
            .resource_manager()
            .map(|resources| resources.snapshot().settings.runtime_settings())
            .transpose()
            .expect("attached resource settings were already validated")
            .unwrap_or_else(|| crate::Settings::default().runtime_settings().expect("default settings"));
        let shell_extensions = extension_runtime.clone();
        let initial_runtime = runtime::ApplicationRuntime::new(
            runtime::INITIAL_RUNTIME_EPOCH,
            session.clone(),
            extension_runtime,
            None,
            None,
        );
        let inner = Arc::new(ApplicationInner {
            session: session.clone(),
            runtime: runtime::ApplicationRuntimeSlot::new(initial_runtime, events.clone()),
            runtime_factory: Mutex::new(None),
            events,
            active_run: Mutex::new(None),
            session_subscription: Mutex::new(None),
            session_events: Mutex::new(None),
            extension_runtime: Mutex::new(shell_extensions),
            orchestration_runtime: Mutex::new(None),
            orchestration_events: Mutex::new(None),
            workflow_manager: Mutex::new(None),
            workflow_runtime_factory: Mutex::new(None),
            workflow_events: Mutex::new(None),
            todo_dag: Mutex::new(todo_execution::TodoDagCoordinator::default()),
            todo_dag_changed: Notify::new(),
            todo_cycle_pending: AtomicBool::new(false),
            todo_continuation_suppressed: AtomicBool::new(false),
            todo_transition_active: AtomicBool::new(false),
            todo_resume_requested: AtomicBool::new(false),
            charged_goal_jobs: Mutex::new(HashSet::new()),
            goal_tool_binding: Mutex::new(None),
            goal_work_activation: Mutex::new(None),
            goal_work_pending: AtomicUsize::new(0),
            goal_work_changed: Notify::new(),
            orchestration_explicit: AtomicBool::new(false),
            runtime_settings: Mutex::new(Arc::new(runtime_settings)),
            process_manager: process_manager.clone(),
            process_owner_id,
            process_events: Mutex::new(None),
            loop_scheduler: Mutex::new(None),
            turn_gate: Arc::new(AsyncMutex::new(())),
            loop_turn_active: AtomicBool::new(false),
            operation_gate: AsyncMutex::new(()),
            cleanup_lock: AsyncMutex::new(()),
            cleaned: AtomicBool::new(false),
        });
        let todo_gate = inner.todo_dag_transaction_gate();
        let todo_check_inner = Arc::downgrade(&inner);
        let todo_commit_inner = Arc::downgrade(&inner);
        session.set_todo_mutation_transaction(crate::todo::TodoMutationTransaction {
            gate: todo_gate,
            check: Arc::new(move || {
                let inner = todo_check_inner
                    .upgrade()
                    .ok_or_else(|| anyhow!("application stopped"))?;
                if inner.todo_transition_active.load(Ordering::Acquire) {
                    return Err(anyhow!("Todo mutation rejected during session transition"));
                }
                Ok(())
            }),
            commit: Arc::new(move || {
                let Some(inner) = todo_commit_inner.upgrade() else {
                    return;
                };
                if let Err(error) = inner.arm_todo_dag_after_mutation_locked() {
                    inner.todo_dag_failed(&error);
                }
            }),
        });
        let extension = inner.extension_runtime.lock().clone();
        if let Some((runtime, _)) = extension.as_ref() {
            runtime
                .set_action_host(Arc::new(ApplicationExtensionHost {
                    application: Arc::downgrade(&inner),
                }))
                .expect("new extension runtime must not already have an action host");
            let before_start_runtime = runtime.clone();
            session.set_before_agent_start(Some(Arc::new(move |context| {
                let runtime = before_start_runtime.clone();
                Box::pin(async move {
                    let event = serde_json::json!({
                        "prompt": prompt_text(&context.messages),
                        "images": prompt_images(&context.messages),
                    });
                    let reduction = runtime
                        .reduce_before_agent_start(event, context.system_prompt)
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
            let tool_call_runtime = runtime.clone();
            session.set_before_tool_call(Some(Arc::new(move |context| {
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
                })
            })));
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
        let event_inner = Arc::downgrade(&inner);
        let subscription = session
            .subscribe(move |event| {
                let event_inner = event_inner.clone();
                async move {
                    let Some(inner) = event_inner.upgrade() else {
                        return Ok(());
                    };
                    let extension = inner
                        .extension_runtime
                        .lock()
                        .as_ref()
                        .map(|(runtime, _)| runtime.clone());
                    if let Some(runtime) = extension
                        && let Some(extension_event) = agent_extension_event(&event)
                    {
                        let _ = runtime.emit(extension_event).await;
                    }
                    if let AgentEvent::ToolExecutionEnd {
                        tool_name,
                        is_error: false,
                        result,
                        ..
                    } = &event
                        && tool_name == "todo"
                    {
                        let state = inner.session.todo_state();
                        let completed_tasks = serde_json::from_value::<crate::TodoToolDetails>(
                            result.details.clone(),
                        )
                        .map(|details| details.completed_tasks)
                        .unwrap_or_default();
                        inner.publish(ApplicationEvent::TodoUpdated {
                            phases: state.phases,
                            completed_tasks,
                        });
                    }
                    inner.publish(ApplicationEvent::Agent(event));
                    Ok(())
                }
            })
            .await;
        *inner.session_subscription.lock() = Some(subscription);
        let mut session_events = session.subscribe_session_events();
        let session_event_inner = Arc::downgrade(&inner);
        let session_event_task = tokio::spawn(async move {
            while let Ok(event) = session_events.recv().await {
                let Some(inner) = session_event_inner.upgrade() else {
                    break;
                };
                let extension = session_extension_event(&event);
                let runtime = inner
                    .extension_runtime
                    .lock()
                    .as_ref()
                    .map(|(runtime, _)| runtime.clone());
                if let (Some(extension), Some(runtime)) = (extension, runtime) {
                    let _ = runtime.emit(extension).await;
                }
                inner.publish(ApplicationEvent::Session(event));
            }
        });
        *inner.session_events.lock() = Some(session_event_task);
        let mut process_events = process_manager.subscribe();
        let process_event_inner = Arc::downgrade(&inner);
        let process_event_task = tokio::spawn(async move {
            loop {
                match process_events.recv().await {
                    Ok(event) => {
                        let Some(inner) = process_event_inner.upgrade() else {
                            break;
                        };
                        if process_event_owner(&event) == &inner.process_owner_id {
                            inner.publish(ApplicationEvent::Process(event));
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        *inner.process_events.lock() = Some(process_event_task);
        let loop_runner_inner = Arc::downgrade(&inner);
        let loop_runner: crate::LoopTurnRunner = Arc::new(move |request, cancel| {
            let inner = loop_runner_inner.clone();
            Box::pin(async move { run_loop_turn(inner, request, cancel).await })
        });
        let loop_event_inner = Arc::downgrade(&inner);
        let loop_events: crate::LoopEventSink = Arc::new(move |event| {
            if let Some(inner) = loop_event_inner.upgrade() {
                inner.publish(ApplicationEvent::Loop(event));
            }
        });
        let session_file = session.recorder_info().map(|(_, path)| path);
        let loop_scheduler = crate::start_loop_scheduler(
            session_file.as_deref(),
            loop_runner,
            loop_events,
        );
        *inner.loop_scheduler.lock() = Some(loop_scheduler);
        Self { inner }
    }

    async fn bind_runtime_generation(
        &self,
        generation: &runtime::ApplicationRuntime,
    ) -> Result<()> {
        let epoch = generation.epoch();
        let session = generation.session();
        let extension = generation.extension_runtime().map(|(runtime, _)| runtime);
        if let Some(runtime) = &extension {
            runtime.set_action_host(Arc::new(ApplicationExtensionHost {
                application: Arc::downgrade(&self.inner),
            }))?;
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
            let tool_call_runtime = runtime.clone();
            session.set_before_tool_call(Some(Arc::new(move |context| {
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
                })
            })));
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
            let mut events = orchestration.subscribe();
            let orchestration_inner = Arc::downgrade(&self.inner);
            *generation.orchestration_events.lock() = Some(tokio::spawn(async move {
                loop {
                    match events.recv().await {
                        Ok(event) => {
                            let Some(inner) = orchestration_inner.upgrade() else { break; };
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
                self.inner.session.apply_runtime_settings(settings.clone()).await;
                *self.runtime().runtime_settings.lock() = Arc::new(settings);
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
        self.inner.session.apply_runtime_settings(settings.clone()).await;
        *self.runtime().runtime_settings.lock() = Arc::new(settings);
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
        let mut current = self.inner.goal_tool_binding.lock();
        if current.is_some() {
            return Err(anyhow!("application goal tool is already configured"));
        }
        *current = Some(binding);
        Ok(())
    }

    pub fn orchestration_runtime(&self) -> Option<crate::OrchestrationRuntime> {
        self.inner.orchestration_runtime.lock().as_ref().cloned()
    }

    pub fn attach_orchestration(&self, runtime: crate::OrchestrationRuntime) -> Result<()> {
        self.attach_orchestration_with_override(runtime, true)
    }

    pub fn attach_orchestration_with_override(
        &self,
        runtime: crate::OrchestrationRuntime,
        explicit: bool,
    ) -> Result<()> {
        let mut current = self.inner.orchestration_runtime.lock();
        if current.is_some() {
            return Err(anyhow!("application orchestration is already configured"));
        }
        let mut events = runtime.subscribe();
        let event_runtime = runtime.clone();
        let event_inner = Arc::downgrade(&self.inner);
        let event_task = tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        let Some(inner) = event_inner.upgrade() else {
                            break;
                        };
                        if matches!(&event, crate::OrchestrationEvent::JobUpdated { job, .. } if !job.status.is_settled()) {
                            inner.todo_cycle_pending.store(true, Ordering::Release);
                        }
                        let allow_spawn = !inner.todo_continuation_suppressed.load(Ordering::Acquire);
                        if let Err(error) = inner.observe_orchestration_event(&event_runtime, &event, allow_spawn) {
                            inner.todo_dag_failed(&error);
                        }
                        inner.publish(ApplicationEvent::Orchestration(event));
                        inner.finish_todo_cycle_if_idle(Some(&event_runtime), false);
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let Some(inner) = event_inner.upgrade() else {
                            break;
                        };
                        for event in event_runtime.presentation_events() {
                            if matches!(&event, crate::OrchestrationEvent::JobUpdated { job, .. } if !job.status.is_settled()) {
                                inner.todo_cycle_pending.store(true, Ordering::Release);
                            }
                            let allow_spawn = !inner.todo_continuation_suppressed.load(Ordering::Acquire);
                            if let Err(error) = inner.observe_orchestration_event(&event_runtime, &event, allow_spawn) {
                                inner.todo_dag_failed(&error);
                            }
                            inner.publish(ApplicationEvent::Orchestration(event));
                            inner.finish_todo_cycle_if_idle(Some(&event_runtime), false);
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        *current = Some(runtime);
        *self.inner.orchestration_events.lock() = Some(event_task);
        self.inner.orchestration_explicit.store(explicit, Ordering::Release);
        drop(current);
        self.inner.arm_todo_dag_after_mutation()?;
        Ok(())
    }

    #[must_use]
    pub fn session(&self) -> Session {
        self.inner.session.clone()
    }

    #[must_use]
    pub fn get_active_tool_names(&self) -> Vec<String> {
        self.inner.session.get_active_tool_names()
    }

    #[must_use]
    pub fn get_all_tools(&self) -> Vec<pi_agent::AgentTool> {
        self.inner.session.get_all_tools()
    }

    #[must_use]
    pub fn get_tool_definition(&self, name: &str) -> Option<pi_agent::AgentTool> {
        self.inner.session.get_tool_definition(name)
    }

    pub async fn set_active_tools_by_name(&self, names: &[String]) -> Result<()> {
        self.inner.session.set_active_tools_by_name(names).await
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<ApplicationEvent> {
        self.inner.events.subscribe()
    }

    #[must_use]
    pub fn todo_state(&self) -> crate::TodoState {
        self.inner.session.todo_state()
    }

    pub fn apply_todo(&self, op: crate::TodoOp) -> Result<crate::TodoApplyResult> {
        match self.inner.session.apply_todo(op) {
            Ok(result) => {
                self.inner.publish(ApplicationEvent::TodoUpdated {
                    phases: result.phases.clone(),
                    completed_tasks: result.completed_tasks.clone(),
                });
                Ok(result)
            }
            Err(error) => {
                self.inner.session.schedule_todo_reminder();
                Err(error)
            }
        }
    }

    pub fn set_todos(&self, phases: Vec<crate::TodoPhase>) -> Result<crate::TodoApplyResult> {
        let result = self.inner.session.set_todos(phases)?;
        self.inner.publish(ApplicationEvent::TodoUpdated {
            phases: result.phases.clone(),
            completed_tasks: result.completed_tasks.clone(),
        });
        Ok(result)
    }
    #[must_use]
    pub fn goal_state(&self) -> GoalState {
        self.inner.session.goal_runtime().get()
    }

    pub fn goal_create(&self, objective: impl Into<String>, token_budget: Option<u64>) -> Result<Goal> {
        self.goal_mutation("create", |runtime| runtime.create(objective, token_budget))
    }

    /// Creates a goal and immediately starts its first model turn, or queues
    /// that turn behind the Application's current work.
    pub async fn activate_goal(
        &self,
        objective: impl Into<String>,
        token_budget: Option<u64>,
    ) -> Result<GoalActivationOutcome> {
        self.goal_create(objective, token_budget)?;
        self.start_goal_work().await
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
    pub async fn resume_goal_work(&self) -> Result<GoalActivationOutcome> {
        self.goal_resume()?;
        self.start_goal_work().await
    }

    pub fn goal_complete(&self) -> Result<Goal> {
        self.goal_mutation("complete", |runtime| runtime.complete())
    }

    pub fn goal_drop(&self) -> Result<Goal> {
        self.goal_mutation("drop", |runtime| runtime.drop())
    }

    pub fn goal_update_usage(&self, delta: GoalUsageDelta) -> Result<Goal> {
        let goal = self
            .inner
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
        let decision = self.inner.session.goal_runtime().continuation_decision();
        self.inner.publish(ApplicationEvent::GoalContinuation {
            decision: decision.clone(),
        });
        decision
    }

    fn goal_mutation<F>(&self, operation: &'static str, mutation: F) -> Result<Goal>
    where
        F: FnOnce(&crate::GoalRuntime) -> std::result::Result<Goal, GoalError>,
    {
        let goal = mutation(&self.inner.session.goal_runtime())
            .map_err(|error| anyhow!(error.to_string()))?;
        self.inner.publish(ApplicationEvent::GoalUpdated {
            operation,
            state: self.goal_state(),
        });
        Ok(goal)
    }

    async fn start_goal_work(&self) -> Result<GoalActivationOutcome> {
        let state = self.goal_state();
        let goal = state
            .current
            .as_ref()
            .filter(|goal| goal.lifecycle == crate::GoalLifecycle::Active)
            .ok_or_else(|| anyhow!("goal is not active"))?;
        let key = GoalWorkKey {
            goal_id: goal.id.clone(),
            revision: state.revision,
        };
        let turn_guard = self.inner.turn_gate.clone().try_lock_owned().ok();
        let outcome = if turn_guard.is_some() {
            GoalActivationOutcome::Started
        } else {
            GoalActivationOutcome::Queued
        };
        {
            let mut activation = self.inner.goal_work_activation.lock();
            if activation.as_ref() == Some(&key) {
                return Ok(GoalActivationOutcome::AlreadyActive);
            }
            *activation = Some(key.clone());
        }
        self.inner.goal_work_pending.fetch_add(1, Ordering::AcqRel);
        let inner = Arc::downgrade(&self.inner);
        tokio::spawn(run_goal_work(inner, key, turn_guard));
        Ok(outcome)
    }

    pub fn prepare_resumed_goal(&self, forked: bool) -> Result<()> {
        let runtime = self.inner.session.goal_runtime();
        let Some(source) = runtime.get().current else {
            return Ok(());
        };
        if forked {
            runtime
                .fork_clone(&source)
                .map_err(|error| anyhow!(error.to_string()))?;
            self.inner.publish(ApplicationEvent::GoalUpdated {
                operation: "fork_clone",
                state: runtime.get(),
            });
        } else {
            self.inner.pause_goal_after_resume()?;
        }
        self.inner.charged_goal_jobs.lock().clear();
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
        self.inner.process_manager.clone()
    }

    pub async fn process_spawn(&self, spec: ProcessSpawnSpec) -> Result<ProcessInfo> {
        self.inner.process_manager.spawn(self.inner.process_owner_id.clone(), spec).await
    }

    #[must_use]
    pub fn process_list(&self) -> Vec<ProcessInfo> {
        self.inner.process_manager.list(&self.inner.process_owner_id)
    }

    pub fn process_describe(&self, id: &ProcessId) -> Result<ProcessInfo> {
        self.inner.process_manager.describe(&self.inner.process_owner_id, id)
    }

    pub async fn process_logs(&self, id: &ProcessId, cursor: u64, max_bytes: Option<usize>, follow: bool, timeout: Option<Duration>) -> Result<ProcessLogs> {
        self.inner.process_manager.logs(&self.inner.process_owner_id, id, cursor, max_bytes, follow, timeout).await
    }

    pub async fn process_write(&self, id: &ProcessId, bytes: Vec<u8>, close_stdin: bool) -> Result<()> {
        self.inner.process_manager.write(&self.inner.process_owner_id, id, bytes, close_stdin).await
    }

    pub async fn process_send_keys(&self, id: &ProcessId, keys: &[ProcessKey]) -> Result<()> {
        self.inner.process_manager.send_keys(&self.inner.process_owner_id, id, keys).await
    }

    pub fn process_resize(&self, id: &ProcessId, size: ProcessTerminalSize) -> Result<()> {
        self.inner.process_manager.resize(&self.inner.process_owner_id, id, size)
    }

    pub fn process_signal(&self, id: &ProcessId, signal: ProcessSignal) -> Result<()> {
        self.inner.process_manager.signal(&self.inner.process_owner_id, id, signal)
    }

    pub async fn process_stop(&self, id: &ProcessId, grace: Option<Duration>) -> Result<ProcessInfo> {
        self.inner.process_manager.stop(&self.inner.process_owner_id, id, grace).await
    }

    pub async fn process_wait(&self, id: &ProcessId, timeout: Option<Duration>) -> Result<ProcessInfo> {
        self.inner.process_manager.wait(&self.inner.process_owner_id, id, timeout).await
    }

    #[must_use]
    pub fn session_header(&self) -> Option<SessionStartedEvent> {
        self.inner.session.session_header().map(|header| SessionStartedEvent {
            record_type: header.record_type,
            version: header.version,
            id: header.id,
            timestamp: header.timestamp,
            cwd: header.cwd.to_string_lossy().into_owned(),
        })
    }

    #[must_use]
    pub fn messages(&self) -> Vec<Message> {
        self.inner.session.history()
    }

    #[must_use]
    pub fn last_assistant_text(&self) -> Option<String> {
        let text = self.inner.session.last_assistant_text();
        (!text.is_empty()).then_some(text)
    }

    #[must_use]
    pub async fn state(&self) -> ApplicationState {
        let session = &self.inner.session;
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
        self.inner.loop_turn_active.load(Ordering::Acquire)
            || self.inner.goal_work_pending.load(Ordering::Acquire) != 0
            || self
                .inner
                .active_run
                .lock()
                .as_ref()
                .is_some_and(|run| !run.is_finished())
            || self.inner.todo_cycle_pending.load(Ordering::Acquire)
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
        if self.inner.todo_continuation_suppressed.load(Ordering::Acquire) {
            self.inner.todo_resume_requested.store(true, Ordering::Release);
        }
        if self.inner.loop_turn_active.load(Ordering::Acquire) {
            return match streaming_behavior {
                Some(StreamingBehavior::Steer) => {
                    self.inner.session.steer(user_message(message, images)).await;
                    Ok(())
                }
                Some(StreamingBehavior::FollowUp) => {
                    self.inner.session.follow_up(user_message(message, images)).await;
                    Ok(())
                }
                None => Err(anyhow!(
                    "session is already processing; choose steer or followUp"
                )),
            };
        }
        let Ok(turn_guard) = self.inner.turn_gate.clone().try_lock_owned() else {
            return match streaming_behavior {
                Some(StreamingBehavior::Steer) => {
                    self.inner.session.steer(user_message(message, images)).await;
                    Ok(())
                }
                Some(StreamingBehavior::FollowUp) => {
                    self.inner.session.follow_up(user_message(message, images)).await;
                    Ok(())
                }
                None => Err(anyhow!(
                    "session is already processing; choose steer or followUp"
                )),
            };
        };
        let session = self.inner.session.clone();
        let inner = self.inner.clone();
        let selection = session.select_for_request(&message).await;
        // Exact trusted agent mention + delegation verb spawns through the
        // normal orchestration path (AgentUpdated/JobUpdated). Generic skill
        // or semantic text stays a selection recommendation only.
        if allow_natural_language_spawn
            && let Some(runtime) = self.orchestration_runtime()
            && runtime
                .spawn_from_natural_language(runtime.main_agent_id(), 0, &message)?
                .is_some()
        {
            inner.todo_cycle_pending.store(true, Ordering::Release);
        }
        inner.publish(ApplicationEvent::Selection(selection));
        if session.todo_reminder_pending() {
            inner.publish(ApplicationEvent::TodoReminder {
                phases: session.todo_state().phases,
            });
        }
        let turn_guard = turn_guard;
        let goal_was_active = self
            .inner
            .session
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
        *self.inner.active_run.lock() = Some(handle);
        Ok(())
    }

    pub async fn steer(&self, message: String, images: Vec<ContentBlock>) {
        if self.inner.todo_continuation_suppressed.load(Ordering::Acquire) {
            self.inner.todo_resume_requested.store(true, Ordering::Release);
        }
        self.inner.session.steer(user_message(message, images)).await;
    }

    pub async fn follow_up(&self, message: String, images: Vec<ContentBlock>) {
        if self.inner.todo_continuation_suppressed.load(Ordering::Acquire) {
            self.inner.todo_resume_requested.store(true, Ordering::Release);
        }
        self.inner.session.follow_up(user_message(message, images)).await;
    }

    pub async fn abort(&self) {
        self.inner.todo_resume_requested.store(false, Ordering::Release);
        self.inner.todo_continuation_suppressed.store(true, Ordering::Release);
        if let Some(runtime) = self.orchestration_runtime() {
            runtime.cancel_active();
            self.inner.finish_todo_cycle_if_idle(Some(&runtime), false);
        }
        self.inner.session.abort().await;
    }

    pub fn abort_compaction(&self) {
        self.inner.session.abort_compaction();
    }

    pub fn set_auto_retry_enabled(&self, enabled: bool) {
        self.inner.session.set_auto_retry_enabled(enabled);
    }

    pub fn abort_retry(&self) {
        self.inner.session.abort_retry();
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
        if let Some(runtime) = self.extension_runtime() {
            let reduction = runtime
                .reduce_user_bash(serde_json::json!({
                    "command": command,
                    "excludeFromContext": exclude_from_context,
                    "cwd": self.inner.session.cwd(),
                }))
                .await?;
            if let Some(result) = reduction.result {
                self.inner
                    .session
                    .record_bash_result(&command, exclude_from_context, &result)
                    .await?;
                return Ok(result);
            }
        }
        self.inner
            .session
            .execute_bash_with_id(&command, exclude_from_context, id)
            .await
    }

    pub fn abort_bash(&self) {
        self.inner.session.abort_bash();
    }

    #[must_use]
    pub fn is_bash_running(&self) -> bool {
        self.inner.session.is_bash_running()
    }

    #[must_use]
    pub fn session_stats(&self) -> crate::SessionStats {
        self.inner.session.session_stats()
    }

    pub async fn queued_messages(&self) -> (Vec<Message>, Vec<Message>) {
        self.inner.session.queued_messages().await
    }

    pub async fn drain_queued_messages(&self) -> (Vec<Message>, Vec<Message>) {
        self.inner.session.drain_queued_messages().await
    }

    pub async fn wait_for_idle(&self) {
        loop {
            self.inner.session.wait_for_idle().await;
            let handle = self.inner.active_run.lock().take();
            if let Some(handle) = handle {
                let _ = handle.await;
            }
            let changed = self.inner.goal_work_changed.notified();
            if self.inner.goal_work_pending.load(Ordering::Acquire) == 0 {
                return;
            }
            changed.await;
        }
    }
    async fn begin_todo_session_transition(&self) -> Result<()> {
        self.inner.todo_continuation_suppressed.store(true, Ordering::Release);
        self.inner.todo_resume_requested.store(false, Ordering::Release);
        self.inner.todo_cycle_pending.store(false, Ordering::Release);
        let runtime = self.orchestration_runtime();
        let gate = self.inner.todo_dag_transaction_gate();
        let job_ids = {
            let _transaction = gate.lock();
            self.inner.todo_transition_active.store(true, Ordering::Release);
            runtime.as_ref().map_or_else(Vec::new, |runtime| {
                self.inner.begin_todo_dag_transition_locked(runtime)
            })
        };
        let Some(runtime) = runtime else {
            return Ok(());
        };
        runtime.cancel_jobs(&job_ids);
        let mut remaining = job_ids;
        while !remaining.is_empty() {
            runtime.wait_jobs(&remaining, None, None).await?;
            remaining.retain(|job_id| {
                runtime
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
        let gate = self.inner.todo_dag_transaction_gate();
        let _transaction = gate.lock();
        self.inner.todo_transition_active.store(false, Ordering::Release);
        if let Err(error) = self.inner.arm_todo_dag_after_mutation_locked() {
            self.inner.todo_dag_failed(&error);
        }
    }

    pub async fn switch_session(&self, path: &Path) -> Result<()> {
        let prepared = crate::PreparedSessionResume::prepare_path(path)?;
        self.switch_prepared_session(prepared).await
    }

    pub async fn switch_prepared_session(
        &self,
        prepared: crate::PreparedSessionResume,
    ) -> Result<()> {
        if let Some(runtime) = self.extension_runtime()
            && runtime
                .reduce_before_switch(serde_json::json!({
                    "reason": "resume",
                    "targetSessionFile": prepared.path(),
                }))
                .await?
        {
            return Err(anyhow!("session switch cancelled by extension"));
        }

        let target_session_file = prepared.path().to_path_buf();
        let target_cwd = prepared
            .target_cwd()
            .canonicalize()
            .with_context(|| format!("resolving resumed working directory {}", prepared.target_cwd().display()))?;
        let active = self.runtime();
        if target_cwd == active.session.cwd() {
            let loops = self.loop_handle()?;
            let previous = active.session.recorder_info().map(|(_, path)| path);
            loops.suspend(crate::LoopRemovalReason::SessionChanged).await?;
            active.session.wait_for_idle().await;
            if let Err(error) = self.begin_todo_session_transition().await {
                let _ = loops.activate(previous).await;
                self.finish_todo_session_transition();
                return Err(error);
            }
            if let Err(error) = active.session.switch_prepared_session(prepared).await {
                let _ = loops.activate(previous).await;
                self.finish_todo_session_transition();
                return Err(error);
            }
            let current = active.session.recorder_info().map(|(_, path)| path);
            if let Err(error) = loops.activate(current).await {
                self.finish_todo_session_transition();
                return Err(error.into());
            }
            active.charged_goal_jobs.lock().clear();
            if let Err(error) = self.inner.pause_goal_after_resume() {
                self.finish_todo_session_transition();
                return Err(error);
            }
            self.finish_todo_session_transition();
            return Ok(());
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

        let _operation = self.inner.operation_gate.lock().await;
        let loops = self.loop_handle()?;
        let epoch = self.inner.runtime.next_epoch();
        let next = candidate.activate(epoch);
        self.bind_runtime_generation(&next).await?;
        loops
            .commit_session_switch(prepared_loops, crate::LoopRemovalReason::SessionChanged)
            .await?;
        let old = self.inner.runtime.replace(next);

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
        Ok(())
    }
    pub async fn new_session(&self) -> Result<()> {
        self.new_session_with_parent(None).await
    }

    pub async fn new_session_with_parent(&self, parent_session: Option<&Path>) -> Result<()> {
        if let Some(runtime) = self.extension_runtime()
            && runtime
                .reduce_before_switch(serde_json::json!({ "reason": "new" }))
                .await?
        {
            return Err(anyhow!("session switch cancelled by extension"));
        }
        let loops = self.loop_handle()?;
        let previous = self.inner.session.recorder_info().map(|(_, path)| path);
        loops.suspend(crate::LoopRemovalReason::SessionChanged).await?;
        self.wait_for_idle().await;
        if let Err(error) = self.begin_todo_session_transition().await {
            let _ = loops.activate(previous).await;
            self.finish_todo_session_transition();
            return Err(error);
        }
        self.inner
            .process_manager
            .shutdown_owner(&self.inner.process_owner_id)
            .await;
        let result = self
            .inner
            .session
            .reset()
            .await
            .and_then(|()| self.inner.session.start_new_recording_with_parent(parent_session));
        if let Err(error) = result {
            let _ = loops.activate(previous).await;
            self.finish_todo_session_transition();
            return Err(error);
        }
        let current = self.inner.session.recorder_info().map(|(_, path)| path);
        if let Err(error) = loops.activate(current).await {
            self.finish_todo_session_transition();
            return Err(error.into());
        }
        self.inner.charged_goal_jobs.lock().clear();
        self.finish_todo_session_transition();
        Ok(())
    }

    pub async fn compact(
        &self,
        custom_instructions: Option<&str>,
    ) -> Result<crate::CompactionResult> {
        self.wait_for_idle().await;
        self.inner.session.compact(custom_instructions).await
    }

    pub async fn fork_session(&self, entry_id: &str) -> Result<String> {
        let source_goal = self.inner.session.goal_runtime().get().current;
        let restore_conversation = if let Some(runtime) = self.extension_runtime() {
            let reduction = runtime
                .reduce_before_fork(serde_json::json!({
                    "entryId": entry_id,
                    "position": "before",
                }))
                .await?;
            if reduction.cancel {
                return Err(anyhow!("session fork cancelled by extension"));
            }
            !reduction.skip_conversation_restore
        } else {
            true
        };
        let loops = self.loop_handle()?;
        let previous = self.inner.session.recorder_info().map(|(_, path)| path);
        loops.suspend(crate::LoopRemovalReason::SessionChanged).await?;
        self.wait_for_idle().await;
        if let Err(error) = self.begin_todo_session_transition().await {
            let _ = loops.activate(previous).await;
            self.finish_todo_session_transition();
            return Err(error);
        }
        self.inner
            .process_manager
            .shutdown_owner(&self.inner.process_owner_id)
            .await;
        self.inner.publish(ApplicationEvent::SessionBeforeFork(SessionBeforeForkEvent { target_id: entry_id.to_owned() }));
        let editor_text = match self.inner.session.fork_session(entry_id, restore_conversation).await {
            Ok(editor_text) => editor_text,
            Err(error) => {
                let _ = loops.activate(previous).await;
                self.finish_todo_session_transition();
                return Err(error);
            }
        };
        let (session_id, session_file) = match self.inner.session.recorder_info() {
            Some(info) => info,
            None => {
                self.finish_todo_session_transition();
                return Err(anyhow!("forked session recording is unavailable"));
            }
        };
        if let Err(error) = loops.activate(Some(session_file.clone())).await {
            self.finish_todo_session_transition();
            return Err(error.into());
        }
        if let Some(source_goal) = source_goal {
            if let Err(error) = self
                .inner
                .session
                .goal_runtime()
                .fork_clone(&source_goal)
                .map_err(|error| anyhow!(error.to_string()))
            {
                self.finish_todo_session_transition();
                return Err(error);
            }
            self.inner.publish(ApplicationEvent::GoalUpdated {
                operation: "fork_clone",
                state: self.goal_state(),
            });
        }
        self.inner.charged_goal_jobs.lock().clear();
        self.finish_todo_session_transition();
        self.inner.publish(ApplicationEvent::SessionForked(SessionForkedEvent { target_id: entry_id.to_owned(), session_id, session_file: session_file.to_string_lossy().into_owned(), editor_text: editor_text.clone() }));
        Ok(editor_text)
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
                let active_leaf_id = self.inner.session.session_tree()?.active_leaf_id;
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
        let result = match self.inner.session.navigate_tree(entry_id, options).await {
            Ok(result) => result,
            Err(error) => {
                self.finish_todo_session_transition();
                return Err(error);
            }
        };
        self.inner.publish(ApplicationEvent::SessionTree(SessionTreeEvent { target_id: entry_id.to_owned(), active_leaf_id: result.active_leaf_id.clone(), editor_text: result.editor_text.clone(), summary_entry_id: result.summary_entry_id.clone(), changed: result.changed, cancelled: result.cancelled }));
        self.finish_todo_session_transition();
        Ok(result)
    }

    pub fn set_session_label(&self, target_id: &str, label: Option<&str>) -> Result<()> {
        self.inner.session.set_session_label(target_id, label)
    }

    pub async fn clone_session(&self) -> Result<()> {
        let source_goal = self.inner.session.goal_runtime().get().current;
        let loops = self.loop_handle()?;
        let previous = self.inner.session.recorder_info().map(|(_, path)| path);
        loops.suspend(crate::LoopRemovalReason::SessionChanged).await?;
        self.wait_for_idle().await;
        if let Err(error) = self.begin_todo_session_transition().await {
            let _ = loops.activate(previous).await;
            self.finish_todo_session_transition();
            return Err(error);
        }
        self.inner
            .process_manager
            .shutdown_owner(&self.inner.process_owner_id)
            .await;
        if let Err(error) = self.inner.session.clone_session().await {
            let _ = loops.activate(previous).await;
            self.finish_todo_session_transition();
            return Err(error);
        }
        let current = self.inner.session.recorder_info().map(|(_, path)| path);
        if let Err(error) = loops.activate(current).await {
            self.finish_todo_session_transition();
            return Err(error.into());
        }
        if let Some(source_goal) = source_goal {
            if let Err(error) = self
                .inner
                .session
                .goal_runtime()
                .fork_clone(&source_goal)
                .map_err(|error| anyhow!(error.to_string()))
            {
                self.finish_todo_session_transition();
                return Err(error);
            }
            self.inner.publish(ApplicationEvent::GoalUpdated {
                operation: "fork_clone",
                state: self.goal_state(),
            });
        }
        self.inner.charged_goal_jobs.lock().clear();
        self.finish_todo_session_transition();
        Ok(())
    }

    pub fn fork_messages(&self) -> Result<Vec<crate::ForkMessage>> {
        self.inner.session.fork_messages()
    }

    pub fn session_entries(&self, since: Option<&str>) -> Result<crate::SessionEntries> {
        self.inner.session.session_entries(since)
    }

    pub fn session_tree(&self) -> Result<crate::SessionTreeResult> {
        self.inner.session.session_tree()
    }

    pub fn export_html(&self, output: Option<&Path>) -> Result<PathBuf> {
        crate::export_live_session(&self.inner.session, output, &crate::ExportOptions::default())
    }

    pub fn export_jsonl(&self, output: Option<&Path>) -> Result<PathBuf> {
        let (_, path) = self
            .inner
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
        let enabled = settings.orchestration_enabled
            || self.inner.orchestration_explicit.load(Ordering::Acquire);
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
        config = config.with_selector_settings(snapshot.settings.selector.clone().unwrap_or_default());
        config.agent_settings = snapshot.settings.agents.clone();
        if let Some(model) = self.inner.session.model() {
            config.parent_model = model;
        }
        // Live parent model so /model switches apply to child resolution without rebuild.
        let session_for_parent = self.inner.session.clone();
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
            &self.inner.session,
            Some(resolver),
        );
        let runtime = crate::OrchestrationRuntime::new(config, factory)?;
        if let Some(current) = self.orchestration_runtime()
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
    ) {
        let next_runtime = candidate.as_ref().cloned();
        let (previous, runtime_changed) = {
            let mut current = self.inner.orchestration_runtime.lock();
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
            if let Some(task) = self.inner.orchestration_events.lock().take() {
                task.abort();
            }
            if let Some(runtime) = next_runtime {
                let mut events = runtime.subscribe();
                let event_runtime = runtime.clone();
                let event_inner = Arc::downgrade(&self.inner);
                *self.inner.orchestration_events.lock() = Some(tokio::spawn(async move {
                    loop {
                        match events.recv().await {
                            Ok(event) => {
                                let Some(inner) = event_inner.upgrade() else { break; };
                                let allow_spawn = !inner.todo_continuation_suppressed.load(Ordering::Acquire);
                                if let Err(error) = inner.observe_orchestration_event(&event_runtime, &event, allow_spawn) {
                                    inner.todo_dag_failed(&error);
                                }
                                inner.publish(ApplicationEvent::Orchestration(event));
                                inner.finish_todo_cycle_if_idle(Some(&event_runtime), false);
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                let Some(inner) = event_inner.upgrade() else { break; };
                                for event in event_runtime.presentation_events() {
                                    let allow_spawn = !inner.todo_continuation_suppressed.load(Ordering::Acquire);
                                    if let Err(error) = inner.observe_orchestration_event(&event_runtime, &event, allow_spawn) {
                                        inner.todo_dag_failed(&error);
                                    }
                                    inner.publish(ApplicationEvent::Orchestration(event));
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
    }

    /// The resource snapshot, extension registry, and agent tool set are all staged
    /// before the active resource and extension generations are replaced.
    pub async fn reload(&self) -> Result<crate::ReloadResult> {
        self.wait_for_idle().await;
        let resources = self
            .inner
            .session
            .resource_manager()
            .ok_or_else(|| anyhow!("session has no resource manager"))?;
        let resource_candidate = resources.stage_reload()?;
        let next_runtime_settings = Arc::new(resource_candidate.snapshot().settings.runtime_settings()?);
        let orchestration_candidate =
            self.orchestration_candidate(&resource_candidate.snapshot(), &next_runtime_settings)?;
        let extension_runtime = self.inner.extension_runtime.lock().clone();
        let Some((runtime, permissions)) = extension_runtime else {
            let mut additional_tools = Vec::new();
            if let Some(orchestration) = &orchestration_candidate {
                additional_tools.extend(orchestration.agent_tools("Main", 0));
            }
            if let Some(binding) = self.inner.goal_tool_binding.lock().as_ref() {
                additional_tools.push(binding.tool());
            }
            let update = self
                .inner
                .session
                .prepare_resource_update(resource_candidate.snapshot(), additional_tools)?;
            let result = resources.commit_reload(resource_candidate)?;
            self.inner.session.commit_resource_update(update).await;
            *self.inner.runtime_settings.lock() = next_runtime_settings;
            self.commit_orchestration_candidate(orchestration_candidate).await;
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
        if let Some(binding) = self.inner.goal_tool_binding.lock().as_ref() {
            additional_tools.push(binding.tool());
        }
        let update = match self.inner.session.prepare_resource_update(
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
        self.inner.session.commit_resource_update(update).await;
        *self.inner.runtime_settings.lock() = next_runtime_settings;
        self.commit_orchestration_candidate(orchestration_candidate).await;
        Ok(result)
    }

    #[must_use]
    pub fn resource_generation(&self) -> Option<u64> {
        self.inner
            .session
            .resource_manager()
            .map(|resources| resources.generation())
    }

    #[must_use]
    pub fn resource_snapshot(&self) -> Option<Arc<crate::ResourceSnapshot>> {
        self.inner
            .session
            .resource_manager()
            .map(|resources| resources.snapshot())
    }

    pub async fn set_steering_mode(&self, mode: pi_agent::QueueMode) {
        self.inner.session.set_steering_mode(mode).await;
    }

    pub async fn set_follow_up_mode(&self, mode: pi_agent::QueueMode) {
        self.inner.session.set_follow_up_mode(mode).await;
    }

    pub fn set_auto_compaction_enabled(&self, enabled: bool) {
        self.inner.session.set_auto_compaction_enabled(enabled);
    }

    pub fn set_session_name(&self, name: &str) -> Result<()> {
        self.inner.session.set_session_name(name)
    }

    pub async fn set_project_trust(
        &self,
        decision: crate::TrustDecision,
    ) -> Result<crate::ReloadResult> {
        let resources = self
            .inner
            .session
            .resource_manager()
            .ok_or_else(|| anyhow!("session has no resource manager"))?;
        resources
            .trust_store()
            .set(self.inner.session.cwd(), decision)?;
        self.reload().await
    }

    pub fn set_model(&self, model: Model, api_key: String) -> crate::ThinkingLevelChange {
        self.inner.session.set_model(model, api_key)
    }

    pub async fn set_model_with_resolved_auth(
        &self,
        model: Model,
    ) -> Result<crate::ThinkingLevelChange> {
        self.inner.session.set_model_with_resolved_auth(model).await
    }

    pub fn set_thinking_level(&self, level: ThinkingLevel) -> crate::ThinkingLevelChange {
        self.inner.session.set_thinking_level(level)
    }

    /// Stops every runtime owned by this application. Safe to call repeatedly;
    /// concurrent callers wait for the in-flight cleanup to finish.
    pub async fn cleanup(&self) {
        let _cleanup = self.inner.cleanup_lock.lock().await;
        if self.inner.cleaned.load(Ordering::Acquire) {
            return;
        }

        self.inner.cleaned.store(true, Ordering::Release);
        self.inner.session.abort().await;
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
        if let Some(task) = self.inner.orchestration_events.lock().take() {
            task.abort();
        }
        let orchestration_runtime = self.inner.orchestration_runtime.lock().take();
        if let Some(runtime) = orchestration_runtime {
            runtime.shutdown().await;
        }

        self.inner
            .process_manager
            .shutdown_owner(&self.inner.process_owner_id)
            .await;
        let process_events = self.inner.process_events.lock().take();
        if let Some(task) = process_events {
            task.abort();
        }
        let session_events = self.inner.session_events.lock().take();
        if let Some(task) = session_events {
            task.abort();
        }
        self.inner.session_subscription.lock().take();

        let runtime = self
            .inner
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
        let session = self.inner.session.clone();
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

    /// Share the current live session to a secret GitHub gist.
    ///
    /// Exports the session to HTML, uploads it via `gh gist create` using gh's
    /// default secret visibility, and publishes a [`ShareSucceeded`] event with
    /// the viewer URL — or a [`ShareFailed`] event with an actionable error.
    pub fn share_session(&self) {
        let session = self.inner.session.clone();
        let events = self.inner.events.clone();
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
            let stats = application.inner.session.session_stats();
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
                system_prompt: application.inner.session.current_system_prompt().await,
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
                        .inner
                        .session
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
                        .inner
                        .session
                        .send_user_message(content, message_delivery(delivery))
                        .await?;
                    Ok(Value::Null)
                }
                ExtensionRuntimeAction::AppendEntry { custom_type, data } => {
                    serde_json::to_value(application.inner.session.append_custom_entry(&custom_type, data)?)
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
                        .inner
                        .session
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
        let gate = self.todo_dag_transaction_gate();
        let _transaction = gate.lock();
        if self.todo_transition_active.load(Ordering::Acquire) {
            return Err(anyhow!("Todo mutation rejected during session transition"));
        }
        self.todo_continuation_suppressed.store(false, Ordering::Release);
        self.todo_resume_requested.store(false, Ordering::Release);
        self.todo_cycle_pending.store(true, Ordering::Release);
        self.arm_todo_dag_locked(runtime, reset_attempts)
    }
    fn has_ready_open_todo(&self) -> bool {
        self.session
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
        let gate = self.todo_dag_transaction_gate();
        let _transaction = gate.lock();
        self.arm_todo_dag_after_mutation_locked()
    }

    fn arm_todo_dag_after_mutation_locked(&self) -> Result<()> {
        if self.todo_transition_active.load(Ordering::Acquire) {
            return Err(anyhow!("Todo mutation rejected during session transition"));
        }
        self.todo_continuation_suppressed.store(false, Ordering::Release);
        self.todo_resume_requested.store(false, Ordering::Release);
        let runtime = self.orchestration_runtime.lock().as_ref().cloned();
        let Some(runtime) = runtime else {
            return Ok(());
        };
        if self.has_ready_open_todo() {
            self.todo_cycle_pending.store(true, Ordering::Release);
            self.arm_todo_dag_locked(&runtime, true)?;
        } else if self.todo_dag.lock().status != TodoDagExecutionStatus::Dormant {
            self.reconcile_todo_dag_locked(&runtime, true)?;
        }
        self.finish_todo_cycle_if_idle(Some(&runtime), false);
        Ok(())
    }
    fn finish_parent_turn(&self) {
        if !self.todo_cycle_pending.load(Ordering::Acquire) {
            self.publish(ApplicationEvent::AgentSettled);
            return;
        }
        if self.todo_resume_requested.swap(false, Ordering::AcqRel) {
            self.todo_continuation_suppressed.store(false, Ordering::Release);
        }
        if !self.todo_continuation_suppressed.load(Ordering::Acquire)
            && let Some(runtime) = self.orchestration_runtime.lock().as_ref().cloned()
        {
            let result = if self.has_ready_open_todo()
                && self.todo_dag.lock().status.is_terminal()
            {
                self.arm_todo_dag(&runtime, true).map(|_| ())
            } else if self.todo_dag.lock().status != TodoDagExecutionStatus::Dormant {
                self.reconcile_todo_dag(&runtime, true).map(|_| ())
            } else {
                Ok(())
            };
            if let Err(error) = result {
                self.todo_dag_failed(&error);
            }
            self.finish_todo_cycle_if_idle(Some(&runtime), true);
            return;
        }
        let runtime = self.orchestration_runtime.lock().as_ref().cloned();
        self.finish_todo_cycle_if_idle(runtime.as_ref(), true);
    }
    fn has_active_jobs(runtime: &crate::OrchestrationRuntime) -> bool {
        runtime.jobs(None).iter().any(|job| {
            matches!(job.status, crate::JobStatus::Queued | crate::JobStatus::Running)
        })
    }

    fn todo_dag_failed(&self, error: &anyhow::Error) {
        self.todo_dag.lock().status = TodoDagExecutionStatus::Blocked;
        self.todo_dag_changed.notify_waiters();
        self.publish(ApplicationEvent::RunFailed {
            message: format!("failed to reconcile Todo DAG execution: {error}"),
        });
        let runtime = self.orchestration_runtime.lock().as_ref().cloned();
        self.finish_todo_cycle_if_idle(runtime.as_ref(), false);
    }

    fn finish_todo_cycle_if_idle(
        &self,
        runtime: Option<&crate::OrchestrationRuntime>,
        parent_settled: bool,
    ) {
        if !self.todo_cycle_pending.load(Ordering::Acquire) {
            return;
        }
        if !parent_settled
            && self.active_run.lock().as_ref().is_some_and(|run| !run.is_finished())
        {
            return;
        }
        if runtime.is_some_and(Self::has_active_jobs) {
            return;
        }
        if self.todo_continuation_suppressed.load(Ordering::Acquire) {
            let mut coordinator = self.todo_dag.lock();
            if coordinator.status == TodoDagExecutionStatus::Active {
                coordinator.status = TodoDagExecutionStatus::Blocked;
                self.todo_dag_changed.notify_waiters();
            }
        } else if self.todo_dag.lock().status == TodoDagExecutionStatus::Active {
            return;
        }
        if self.todo_cycle_pending.swap(false, Ordering::AcqRel) {
            self.publish(ApplicationEvent::AgentSettled);
        }
    }

    fn finish_goal_turn(
        &self,
        parent_usage: pi_ai::Usage,
        started: Instant,
        goal_was_active: bool,
    ) {
        if self.session.goal_runtime().get().current.is_none() {
            return;
        }
        let mut tokens = usage_tokens(&parent_usage);
        let mut settled_jobs = Vec::new();
        if let Some(runtime) = self.orchestration_runtime.lock().as_ref() {
            let charged = self.charged_goal_jobs.lock();
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
            match self.session.goal_runtime().update_usage(delta) {
                Ok(_) => {
                    self.charged_goal_jobs.lock().extend(settled_jobs);
                    self.publish(ApplicationEvent::GoalUsageCharged {
                        delta,
                        state: self.session.goal_runtime().get(),
                    });
                }
                Err(error) => self.publish(ApplicationEvent::RunFailed {
                    message: format!("failed to charge goal usage: {error}"),
                }),
            }
        } else {
            self.charged_goal_jobs.lock().extend(settled_jobs);
        }
        self.publish(ApplicationEvent::GoalContinuation {
            decision: self.session.goal_runtime().continuation_decision(),
        });
    }

    fn pause_goal_after_resume(&self) -> Result<()> {
        let runtime = self.session.goal_runtime();
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
        self.process_manager.shutdown_owner_now(&self.process_owner_id);
        if let Some(task) = self.orchestration_events.get_mut().take() {
            task.abort();
        }
        if let Some(task) = self.process_events.get_mut().take() {
            task.abort();
        }
        if let Some(task) = self.session_events.get_mut().take() {
            task.abort();
        }
        self.session_subscription.get_mut().take();

        let session = self.session.clone();
        let active_run = self.active_run.get_mut().take();
        let loop_scheduler = self.loop_scheduler.get_mut().take();
        let orchestration_runtime = self.orchestration_runtime.get_mut().take();
        let extension_runtime = self
            .extension_runtime
            .get_mut()
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
    inner: Weak<ApplicationInner>,
}

impl Drop for GoalWorkPendingGuard {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            inner.goal_work_pending.fetch_sub(1, Ordering::AcqRel);
            inner.goal_work_changed.notify_waiters();
        }
    }
}

async fn run_goal_work(
    inner: Weak<ApplicationInner>,
    key: GoalWorkKey,
    turn_guard: Option<OwnedMutexGuard<()>>,
) {
    let _pending = GoalWorkPendingGuard {
        inner: inner.clone(),
    };
    let turn_guard = match turn_guard {
        Some(turn_guard) => turn_guard,
        None => {
            let Some(application) = inner.upgrade() else {
                return;
            };
            application.turn_gate.clone().lock_owned().await
        }
    };
    let Some(inner) = inner.upgrade() else {
        return;
    };
    if inner.cleaned.load(Ordering::Acquire) {
        clear_goal_work_activation(&inner, &key);
        return;
    }
    let state = inner.session.goal_runtime().get();
    if state.revision != key.revision
        || !state.current.as_ref().is_some_and(|goal| {
            goal.id == key.goal_id && goal.lifecycle == crate::GoalLifecycle::Active
        })
    {
        clear_goal_work_activation(&inner, &key);
        return;
    }

    inner.publish(ApplicationEvent::GoalContinuation {
        decision: inner.session.goal_runtime().continuation_decision(),
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
    match inner.session.run_messages(vec![message]).await {
        Ok(result) => inner.finish_goal_turn(result.usage, started, true),
        Err(error) => {
            clear_goal_work_activation(&inner, &key);
            inner.publish(ApplicationEvent::RunFailed {
                message: error.to_string(),
            });
            inner.finish_goal_turn(pi_ai::Usage::default(), started, true);
        }
    }
    inner.finish_parent_turn();
    drop(turn_guard);
}

fn clear_goal_work_activation(inner: &ApplicationInner, key: &GoalWorkKey) {
    let mut activation = inner.goal_work_activation.lock();
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
    request.report(crate::LoopRunState::Queued);
    let turn_guard = tokio::select! {
        _ = cancel.cancelled() => return Err("loop cancelled".to_owned()),
        guard = inner.turn_gate.clone().lock_owned() => guard,
    };
    if cancel.is_cancelled() {
        return Err("loop cancelled".to_owned());
    }
    request.report(crate::LoopRunState::Started);
    let session = inner.session.clone();
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
    inner.loop_turn_active.store(false, Ordering::Release);
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
    use super::{Application, public_extension_model};
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
}
