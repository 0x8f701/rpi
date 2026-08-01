use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    future::Future,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    task::{Context as TaskContext, Poll, Wake, Waker},
    time::Duration,
};

use anyhow::{Result, anyhow};
use parking_lot::{Mutex, RwLock};
use pi_agent::{
    AbortController, AbortSignal, AfterToolCallFn, AfterToolCallResult, Agent, AgentEvent,
    AgentOptions, AgentState, AgentTool, BeforeToolCallFn, QueueMode, StreamFn, Subscription,
    ThinkingLevel,
};
use pi_ai::{
    AssistantMessageEvent, BashExecutionMessage, CacheRetention, ContentBlock, Context,
    CustomMessage, CustomMessageContent, Message, Model, SimpleStreamOptions, StopReason, ToolCall,
    Usage, UserMessage,
};
use tokio::sync::{Notify, broadcast, mpsc};
use uuid::Uuid;

use crate::system_prompt::BuildSystemPromptOptions;
use crate::{
    BashProcessContext, CompactionDetails, CompactionResult, CompactionSettings, GoalLifecycle,
    GoalRuntime, GoalState, ProcessManager, ProcessOwnerId, RequestAuth, ResourceManager,
    SessionRecorder,
    SUMMARIZATION_PROMPT, SUMMARIZATION_SYSTEM_PROMPT, TURN_PREFIX_SUMMARIZATION_PROMPT,
    UPDATE_SUMMARIZATION_PROMPT, TodoApplyResult, TodoOp, TodoPhase, TodoRuntime, TodoState,
    TodoStorage, apply_checkpoint, build_system_prompt, compute_file_lists, create_todo_tool,
    estimate_context_tokens_usage_aware, find_cut_point, format_file_operations,
    ActiveRetryFallbackState, CatalogModelLookup, RetryFallbackModelLookup,
    RetryFallbackResolutionContext, aggregate_retry_diagnostics, find_retry_fallback_candidates,
    format_retry_fallback_selector, is_context_overflow, is_hard_error_fallback_eligible,
    is_retryable_assistant_error,
    load_context_files, load_project_context_files, load_skills, load_skills_trusted,
    messages_as_llm, process_tool, resolve_retry_fallback_chain_key, serialize_conversation,
    should_compact, tool_snippet,
};
pub const DEFAULT_THINKING_LEVEL: ThinkingLevel = ThinkingLevel::Medium;
/// Ephemeral custom message type projected only into parent model requests.
pub const ACTIVE_GOAL_CUSTOM_TYPE: &str = "pi.goal.active";

/// Result of applying a thinking-level change after model capability clamping.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingLevelChange {
    /// Level the caller requested.
    pub requested: ThinkingLevel,
    /// Level actually stored after clamping to the active model.
    pub effective: ThinkingLevel,
    /// True when `effective` differs from `requested`.
    pub clamped: bool,
    /// Actionable status line for product surfaces.
    pub message: String,
}


pub type SessionAuthResolver = Arc<
    dyn Fn(Model) -> pi_agent::BoxFuture<Result<RequestAuth>> + Send + Sync,
>;
pub type BeforeCompactionFn = Arc<
    dyn Fn(BeforeCompactionContext) -> pi_agent::BoxFuture<Result<BeforeCompactionResult>>
        + Send
        + Sync,
>;

#[derive(Clone, Debug)]
pub struct BeforeCompactionContext {
    pub reason: CompactionReason,
    pub will_retry: bool,
    pub custom_instructions: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct BeforeCompactionResult {
    pub cancel: bool,
    pub compaction: Option<CompactionResult>,
}

pub type BeforeAgentStartFn = pi_agent::BeforeAgentStartFn;
pub type TransformMessageFn = pi_agent::TransformMessageFn;
pub type TransformContextFn = pi_agent::TransformContextFn;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ResourceDiscovery {
    Disabled,
    Global,
    #[default]
    TrustedProject,
}

#[derive(Clone)]
pub struct SessionOptions {
    pub model: Model,
    pub cwd: PathBuf,
    pub system_prompt: String,
    pub thinking_level: ThinkingLevel,
    pub api_key: String,
    pub compaction: Option<CompactionSettings>,
    pub stream_options: SimpleStreamOptions,
    pub tools: Option<Vec<AgentTool>>,
    pub before_tool_call: Option<BeforeToolCallFn>,
    pub after_tool_call: Option<AfterToolCallFn>,
    pub stream_fn: Option<StreamFn>,
    pub auth_resolver: Option<SessionAuthResolver>,
}


struct SessionState {
    model: Model,
    thinking_level: ThinkingLevel,
    api_key: String,
    messages: Vec<Message>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MessageDelivery {
    Steer,
    FollowUp,
    NextTurn,
}

#[derive(Default)]
struct CompactionRuntime {
    prefix_len: usize,
    summary: String,
    read_files: BTreeSet<String>,
    modified_files: BTreeSet<String>,
}

struct SessionRuntime {
    state: RwLock<SessionState>,
    recorder: Mutex<Option<Arc<SessionRecorder>>>,
    goal: RwLock<GoalRuntime>,
    compaction: RwLock<Option<CompactionSettings>>,
    compaction_runtime: Mutex<CompactionRuntime>,
    compaction_active: AtomicBool,
    compaction_controller: Mutex<Option<AbortController>>,
    before_compaction: RwLock<Option<BeforeCompactionFn>>,
    overflow_recovery_attempted: AtomicBool,
    session_name: RwLock<Option<String>>,
    retry_settings: RwLock<RetrySettings>,
    retry_controller: Mutex<Option<AbortController>>,
    retry_attempt: AtomicUsize,
    active_retry_fallback: Mutex<Option<ActiveRetryFallbackState>>,
    fallback_attempt_errors: Mutex<Vec<String>>,
    events: broadcast::Sender<SessionEvent>,
    pending_next_turn: Mutex<Vec<Message>>,
    stream_options: RwLock<SimpleStreamOptions>,
    recorded_count: AtomicUsize,
    stream_fn: StreamFn,
    auth_resolver: Option<SessionAuthResolver>,
    selector_settings: RwLock<crate::SelectorSettings>,
    selector_skills: RwLock<Vec<crate::Skill>>,
    selector_agents: RwLock<Vec<crate::AgentDefinition>>,
    branch_summary: RwLock<crate::EffectiveBranchSummarySettings>,
    expose_session_environment: AtomicBool,
    last_selection: RwLock<Option<crate::SelectionPlan>>,
    /// Detached bash success-path spill files owned by this session.
    /// Cleaned via [`Session::cleanup_bash_spills`] / Drop — never process-wide drain.
    bash_spill_paths: Mutex<HashSet<String>>,
}

struct CompactionActivityGuard {
    inner: Arc<SessionRuntime>,
}

impl CompactionActivityGuard {
    fn begin(inner: Arc<SessionRuntime>) -> Self {
        inner.compaction_active.store(true, Ordering::Release);
        Self { inner }
    }
}

impl Drop for CompactionActivityGuard {
    fn drop(&mut self) {
        self.inner.compaction_active.store(false, Ordering::Release);
    }
}

struct SessionInner {
    cwd: PathBuf,
    workspace: crate::WorkspaceRoots,
    system_prompt: String,
    base_tools: Vec<pi_agent::AgentTool>,
    tools: RwLock<Vec<pi_agent::AgentTool>>,
    all_tools: RwLock<Vec<pi_agent::AgentTool>>,
    tool_selection: ToolSelection,
    shared: Arc<SessionRuntime>,
    agent: Agent,
    _history_subscription: Subscription,
    run_slot: Mutex<RunSlot>,
    idle: Notify,
    abort_notify: Notify,
    resources: RwLock<Option<ResourceManager>>,
    bash_controller: Mutex<Option<ActiveBash>>,
    bash_generation: AtomicU64,
    bash_append_lock: tokio::sync::Mutex<()>,
    pending_bash_messages: Mutex<Vec<Message>>,
    process_manager: ProcessManager,
    process_owner_id: ProcessOwnerId,
    skill_snapshot: Arc<RwLock<Vec<crate::Skill>>>,
    todo: TodoRuntime,
}

struct ActiveBash {
    generation: u64,
    controller: AbortController,
}

struct BashActivityGuard {
    inner: Arc<SessionInner>,
    controller: AbortController,
    generation: u64,
    released: bool,
}

impl BashActivityGuard {
    fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let mut active = self.inner.bash_controller.lock();
        if active.as_ref().is_some_and(|active| active.generation == self.generation) {
            active.take();
        }
    }
}

impl Drop for BashActivityGuard {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        self.controller.abort();
        let mut active = self.inner.bash_controller.lock();
        if active.as_ref().is_some_and(|active| active.generation == self.generation) {
            active.take();
        }
    }
}
#[derive(Default)]
struct RunSlot {
    active: bool,
    abort_requested: bool,
    generation: u64,
}

struct ActiveRunGuard {
    inner: Arc<SessionInner>,
    released: bool,
    generation: u64,
}

impl ActiveRunGuard {
    fn release(&mut self) {
        if !self.released {
            self.released = true;
            let mut slot = self.inner.run_slot.lock();
            if slot.generation == self.generation {
                slot.active = false;
                slot.abort_requested = false;
                self.inner.idle.notify_waiters();
            }
        }
    }
}

impl Drop for ActiveRunGuard {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let inner = self.inner.clone();
        let generation = self.generation;
        spawn_cleanup(move || async move {
            inner.agent.wait_for_idle().await;
            let mut slot = inner.run_slot.lock();
            if slot.generation == generation {
                slot.active = false;
                slot.abort_requested = false;
                inner.idle.notify_waiters();
            }
        });
    }
}

struct ClaimedRun {
    inner: Arc<SessionInner>,
    guard: Option<ActiveRunGuard>,
    before_count: usize,
    recorder_subscription: Option<Subscription>,
}

impl Drop for ClaimedRun {
    fn drop(&mut self) {
        let Some(mut guard) = self.guard.take() else {
            return;
        };
        guard.released = true;
        let subscription = self.recorder_subscription.take();
        let inner = self.inner.clone();
        spawn_cleanup(move || async move {
            inner.agent.wait_for_idle().await;
            let state = inner.agent.state().await;
            inner.shared.state.write().messages = state.messages;
            drop(subscription);
            let mut slot = inner.run_slot.lock();
            if slot.generation == guard.generation {
                slot.active = false;
                slot.abort_requested = false;
                inner.idle.notify_waiters();
            }
        });
    }
}

pub(crate) struct SessionResourceUpdate {
    tools: Vec<AgentTool>,
    all_tools: Option<Vec<AgentTool>>,
    system_prompt: String,
    skills: Vec<crate::Skill>,
    agents: Vec<crate::AgentDefinition>,
    selector_settings: crate::SelectorSettings,
    runtime_settings: crate::RuntimeSettingsSnapshot,
}
fn spawn_cleanup<F, Fut>(cleanup: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + 'static,
{
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("building session cleanup runtime");
        runtime.block_on(cleanup());
    });
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunResult {
    pub text: String,
    pub messages: Vec<Message>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    pub error_message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetrySettings {
    pub enabled: bool,
    pub max_retries: usize,
    pub base_delay_ms: u64,
    #[serde(default = "default_model_fallback")]
    pub model_fallback: bool,
    #[serde(default, skip_serializing_if = "crate::RetryFallbackChains::is_empty")]
    pub fallback_chains: crate::RetryFallbackChains,
}

fn default_model_fallback() -> bool {
    true
}

impl Default for RetrySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_retries: 3,
            base_delay_ms: 2_000,
            model_fallback: true,
            fallback_chains: crate::RetryFallbackChains::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CompactionReason { Manual, Threshold, Overflow }

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SummarizationSource { Compaction, BranchSummary }

#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum SessionEvent {
    AutoRetryStart { attempt: usize, max_attempts: usize, delay_ms: u64, error_message: String },
    AutoRetryEnd { success: bool, attempt: usize, #[serde(skip_serializing_if = "Option::is_none")] final_error: Option<String> },
    RetryFallbackApplied { from: String, to: String, role: String },
    RetryFallbackSucceeded { model: String, role: String },
    BashExecutionUpdate { #[serde(skip_serializing_if = "Option::is_none")] id: Option<String>, delta: String },
    BashExecutionEnd { message: BashExecutionMessage },
    CompactionStart { reason: CompactionReason },
    CompactionEnd { reason: CompactionReason, result: Option<CompactionResult>, aborted: bool, will_retry: bool, #[serde(skip_serializing_if = "Option::is_none")] error_message: Option<String> },
    SummarizationRetryScheduled { attempt: usize, max_attempts: usize, delay_ms: u64, error_message: String },
    SummarizationRetryAttemptStart { source: SummarizationSource, #[serde(skip_serializing_if = "Option::is_none")] reason: Option<CompactionReason> },
    SummarizationRetryFinished,
    QueueUpdate { steering: Vec<Message>, follow_up: Vec<Message> },
    EntryAppended { entry: crate::SessionEntry },
    SessionInfoChanged { name: Option<String> },
    ThinkingLevelChanged { thinking_level: ThinkingLevel },
    ModelSelect { model: Model },
    ThinkingLevelSelect { thinking_level: ThinkingLevel },
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTokenStats {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub total: i64,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionContextUsage {
    pub tokens: Option<i64>,
    pub context_window: i64,
    pub percent: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStats {
    pub session_file: Option<String>,
    pub session_id: Option<String>,
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub tool_calls: usize,
    pub tool_results: usize,
    pub total_messages: usize,
    pub tokens: SessionTokenStats,
    pub cost: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_usage: Option<SessionContextUsage>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NavigateTreeOptions {
    #[serde(default)]
    pub summarize: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace_instructions: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NavigateTreeResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub editor_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_leaf_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary_entry_id: Option<String>,
    pub changed: bool,
    pub cancelled: bool,
}


fn goal_context_message(state: &GoalState) -> Option<Message> {
    let goal = state
        .current
        .as_ref()
        .filter(|goal| goal.lifecycle != GoalLifecycle::Dropped)?;
    let lifecycle = match goal.lifecycle {
        GoalLifecycle::Active => "active",
        GoalLifecycle::Paused => "paused",
        GoalLifecycle::Completed => "completed",
        GoalLifecycle::Dropped => unreachable!("dropped goals are filtered above"),
    };
    let objective = escape_goal_objective(&goal.objective);
    let budget = goal
        .token_budget
        .map_or_else(|| "unlimited".to_owned(), |budget| budget.to_string());
    let content = format!(
        "<system-reminder>\nActive session goal (revision {}, lifecycle {lifecycle}).\nObjective: {objective}\nToken budget: {}/{budget}.\nKeep this goal in scope. Use the goal tool to inspect, pause, or complete it when appropriate.\n</system-reminder>",
        state.revision, goal.usage.tokens_used
    );
    Some(Message::Custom(CustomMessage {
        custom_type: ACTIVE_GOAL_CUSTOM_TYPE.to_owned(),
        content: content.into(),
        display: false,
        details: serde_json::to_value(state).ok(),
        timestamp: pi_ai::now_millis(),
    }))
}

fn escape_goal_objective(objective: &str) -> String {
    let mut escaped = String::with_capacity(objective.len());
    let mut chars = objective.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                escaped.push('\n');
            }
            character if character.is_control() && character != '\n' && character != '\t' => {
                escaped.push(' ');
            }
            character => escaped.push(character),
        }
    }
    escaped
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolSelection {
    pub allow: Option<Vec<String>>,
    pub deny: Vec<String>,
    pub disable_all: bool,
    pub disable_builtins: bool,
    pub enable_process: bool,
    /// When true, the main catalog includes the native sandboxed `glob` tool
    /// without changing the default [read,bash,edit,write] baseline.
    pub enable_glob: bool,
}

#[derive(Clone)]
pub struct ChildSessionOptionsSnapshot {
    pub model: Model,
    pub cwd: PathBuf,
    pub thinking_level: ThinkingLevel,
    pub api_key: String,
    pub stream_options: SimpleStreamOptions,
    pub stream_fn: StreamFn,
    pub auth_resolver: Option<SessionAuthResolver>,
}

#[derive(Clone)]
pub struct Session {
    inner: Arc<SessionInner>,
}


impl Session {
    pub fn new(options: SessionOptions) -> Result<Self> {
        Self::new_with_additional_tools(options, Vec::new())
    }

    pub fn new_with_todo(options: SessionOptions) -> Result<Self> {
        Self::new_with_todo_and_additional_tools(options, Vec::new())
    }

    pub fn new_with_todo_and_additional_tools(
        options: SessionOptions,
        additional_tools: Vec<AgentTool>,
    ) -> Result<Self> {
        Self::new_with_todo_and_additional_tools_filtered(
            options,
            additional_tools,
            ToolSelection::default(),
        )
    }

    pub fn new_with_todo_and_additional_tools_filtered(
        options: SessionOptions,
        additional_tools: Vec<AgentTool>,
        selection: ToolSelection,
    ) -> Result<Self> {
        Self::new_with_todo_and_additional_tools_filtered_and_discovery(
            options,
            additional_tools,
            selection,
            ResourceDiscovery::TrustedProject,
        )
    }

    pub fn new_with_todo_and_additional_tools_filtered_and_discovery(
        options: SessionOptions,
        additional_tools: Vec<AgentTool>,
        selection: ToolSelection,
        resource_discovery: ResourceDiscovery,
    ) -> Result<Self> {
        Self::new_configured(
            options,
            additional_tools,
            selection,
            true,
            resource_discovery,
            None,
            None,
        )
    }

    pub fn new_with_todo_additional_tools_filtered_discovery_and_uri(
        options: SessionOptions,
        additional_tools: Vec<AgentTool>,
        selection: ToolSelection,
        resource_discovery: ResourceDiscovery,
        uri_resolver: Option<crate::InternalUriResolverFn>,
    ) -> Result<Self> {
        Self::new_configured(
            options,
            additional_tools,
            selection,
            true,
            resource_discovery,
            None,
            uri_resolver,
        )
    }

    pub fn new_with_additional_tools(
        options: SessionOptions,
        additional_tools: Vec<AgentTool>,
    ) -> Result<Self> {
        Self::new_with_additional_tools_filtered(
            options,
            additional_tools,
            ToolSelection::default(),
        )
    }

    pub fn new_with_additional_tools_filtered(
        options: SessionOptions,
        additional_tools: Vec<AgentTool>,
        selection: ToolSelection,
    ) -> Result<Self> {
        Self::new_with_additional_tools_filtered_and_discovery(
            options,
            additional_tools,
            selection,
            ResourceDiscovery::TrustedProject,
        )
    }

    pub fn new_with_additional_tools_filtered_and_discovery(
        options: SessionOptions,
        additional_tools: Vec<AgentTool>,
        selection: ToolSelection,
        resource_discovery: ResourceDiscovery,
    ) -> Result<Self> {
        Self::new_configured(
            options,
            additional_tools,
            selection,
            false,
            resource_discovery,
            None,
            None,
        )
    }

    pub fn new_with_additional_tools_filtered_discovery_and_uri(
        options: SessionOptions,
        additional_tools: Vec<AgentTool>,
        selection: ToolSelection,
        resource_discovery: ResourceDiscovery,
        uri_resolver: Option<crate::InternalUriResolverFn>,
    ) -> Result<Self> {
        Self::new_configured(
            options,
            additional_tools,
            selection,
            false,
            resource_discovery,
            None,
            uri_resolver,
        )
    }

    pub fn new_with_todo_additional_tools_filtered_discovery_workspace_and_uri(
        options: SessionOptions,
        additional_tools: Vec<AgentTool>,
        selection: ToolSelection,
        resource_discovery: ResourceDiscovery,
        workspace: crate::WorkspaceRoots,
        uri_resolver: Option<crate::InternalUriResolverFn>,
    ) -> Result<Self> {
        Self::new_configured(
            options,
            additional_tools,
            selection,
            true,
            resource_discovery,
            Some(workspace),
            uri_resolver,
        )
    }

    pub fn new_with_additional_tools_filtered_discovery_workspace_and_uri(
        options: SessionOptions,
        additional_tools: Vec<AgentTool>,
        selection: ToolSelection,
        resource_discovery: ResourceDiscovery,
        workspace: crate::WorkspaceRoots,
        uri_resolver: Option<crate::InternalUriResolverFn>,
    ) -> Result<Self> {
        Self::new_configured(
            options,
            additional_tools,
            selection,
            false,
            resource_discovery,
            Some(workspace),
            uri_resolver,
        )
    }

    fn new_configured(
        options: SessionOptions,
        additional_tools: Vec<AgentTool>,
        selection: ToolSelection,
        todo_enabled: bool,
        resource_discovery: ResourceDiscovery,
        workspace: Option<crate::WorkspaceRoots>,
        uri_resolver: Option<crate::InternalUriResolverFn>,
    ) -> Result<Self> {
        let configured_cwd = if options.cwd.as_os_str().is_empty() {
            std::env::current_dir()?
        } else {
            options.cwd
        };
        let workspace = match workspace {
            Some(workspace) => workspace,
            None => crate::WorkspaceRoots::new(&configured_cwd, Vec::<PathBuf>::new())?,
        };
        let cwd = workspace.cwd().to_path_buf();
        let cwd_text = cwd.to_string_lossy().into_owned();
        let custom_tools = options.tools;
        let custom_prompt = options.system_prompt;
        let before_tool_call = options.before_tool_call;
        let after_tool_call = if todo_enabled {
            Some(todo_after_tool_call(options.after_tool_call))
        } else {
            options.after_tool_call
        };
        let auth_resolver = options.auth_resolver;
        let effective_stream_fn = options
            .stream_fn
            .unwrap_or_else(|| AgentOptions::default().stream_fn);
        let model = options.model;
        let api_key = options.api_key;
        let compaction = options.compaction;
        let thinking_level = clamp_thinking_level(&model, options.thinking_level);
        let mut stream_options = options.stream_options;
        let session_id = stream_options
            .stream
            .session_id
            .clone()
            .unwrap_or_else(|| Uuid::now_v7().to_string());
        stream_options.stream.api_key = Some(api_key.clone());
        stream_options.stream.session_id = Some(session_id.clone());
        let process_manager = ProcessManager::new();
        let process_owner_id = ProcessOwnerId::new(session_id.clone());
        let (events, _) = broadcast::channel(512);
        let shared = Arc::new(SessionRuntime {
            state: RwLock::new(SessionState {
                model: model.clone(),
                thinking_level,
                api_key: api_key.clone(),
                messages: Vec::new(),
            }),
            pending_next_turn: Mutex::new(Vec::new()),
            recorder: Mutex::new(None),
            goal: RwLock::new(GoalRuntime::memory()),
            compaction: RwLock::new(compaction),
            compaction_runtime: Mutex::new(CompactionRuntime::default()),
            compaction_active: AtomicBool::new(false),
            compaction_controller: Mutex::new(None),
            before_compaction: RwLock::new(None),
            overflow_recovery_attempted: AtomicBool::new(false),
            session_name: RwLock::new(None),
            retry_settings: RwLock::new(RetrySettings::default()),
            retry_controller: Mutex::new(None),
            retry_attempt: AtomicUsize::new(0),
            active_retry_fallback: Mutex::new(None),
            fallback_attempt_errors: Mutex::new(Vec::new()),
            events,
            stream_options: RwLock::new(stream_options.clone()),
            recorded_count: AtomicUsize::new(0),
            stream_fn: effective_stream_fn.clone(),
            auth_resolver: auth_resolver.clone(),
            selector_settings: RwLock::new(crate::SelectorSettings::default()),
            selector_skills: RwLock::new(Vec::new()),
            selector_agents: RwLock::new(Vec::new()),
            branch_summary: RwLock::new(crate::EffectiveBranchSummarySettings {
                reserve_tokens: 16_384,
                skip_prompt: false,
            }),
            expose_session_environment: AtomicBool::new(true),
            last_selection: RwLock::new(None),
            bash_spill_paths: Mutex::new(HashSet::new()),
        });
        let storage_shared = shared.clone();
        let persist_shared = shared.clone();
        let todo = TodoRuntime::with_persistence(
            Arc::new(move || {
                if storage_shared.recorder.lock().is_some() {
                    TodoStorage::Session
                } else {
                    TodoStorage::Memory
                }
            }),
            Arc::new(move |state| {
                if let Some(recorder) = persist_shared.recorder.lock().as_ref().cloned() {
                    recorder.record_todo_snapshot(state)?;
                }
                Ok(())
            }),
        );
        let env_runtime = shared.clone();
        let session_env: crate::SessionEnvFn = Arc::new(move || {
            if !env_runtime.expose_session_environment.load(Ordering::Acquire) {
                return HashMap::new();
            }
            let state = env_runtime.state.read();
            let mut env = HashMap::from([
                ("PI_PROVIDER".to_owned(), state.model.provider.clone()),
                ("PI_MODEL".to_owned(), state.model.id.clone()),
                (
                    "PI_REASONING_LEVEL".to_owned(),
                    thinking_level_name(state.thinking_level).to_owned(),
                ),
            ]);
            drop(state);
            if let Some(recorder) = env_runtime.recorder.lock().as_ref() {
                env.insert("PI_SESSION_ID".to_owned(), recorder.id());
                env.insert(
                    "PI_SESSION_FILE".to_owned(),
                    recorder.path().to_string_lossy().into_owned(),
                );
            }
            env
        });
        let include_project_resources = resource_discovery == ResourceDiscovery::TrustedProject;
        let initial_skills = match resource_discovery {
            ResourceDiscovery::Disabled => Vec::new(),
            ResourceDiscovery::Global | ResourceDiscovery::TrustedProject => {
                load_skills_trusted(&cwd_text, include_project_resources).0
            }
        };
        let context_files = match resource_discovery {
            ResourceDiscovery::Disabled => Vec::new(),
            ResourceDiscovery::Global | ResourceDiscovery::TrustedProject => {
                load_context_files(&cwd_text, include_project_resources)
            }
        };
        let skill_snapshot = Arc::new(RwLock::new(initial_skills.clone()));
        *shared.selector_skills.write() = initial_skills.clone();
        let skills_runtime = skill_snapshot.clone();
        let skill_provider: crate::SkillSnapshotFn =
            Arc::new(move || skills_runtime.read().clone());

        let base_tools = custom_tools.unwrap_or_else(|| {
            crate::create_coding_tools_for_workspace_with_context_and_resolver(
                workspace.clone(),
                Some(session_env),
                Some(skill_provider),
                Some(BashProcessContext {
                    manager: process_manager.clone(),
                    owner_id: process_owner_id.clone(),
                }),
                uri_resolver,
            )
        });
        let mut available_tools = merge_tools(&base_tools, additional_tools)?;
        if todo_enabled {
            available_tools.push(create_todo_tool(todo.clone()));
        }
        let process_requested = selection.enable_process
            || selection
                .allow
                .as_ref()
                .is_some_and(|tools| tools.iter().any(|tool| tool == "process"));
        if process_requested && !available_tools.iter().any(|tool| tool.name == "process") {
            available_tools.push(process_tool(
                &cwd,
                process_manager.clone(),
                process_owner_id.clone(),
            ));
        }
        let glob_requested = selection.enable_glob
            || selection
                .allow
                .as_ref()
                .is_some_and(|tools| tools.iter().any(|tool| tool == "glob"));
        if glob_requested && !available_tools.iter().any(|tool| tool.name == "glob") {
            available_tools.push(crate::create_glob_tool_for_workspace(workspace.clone()));
        }
        let tools = select_tools(available_tools.clone(), &selection)?;
        let selected_tools = tools.iter().map(|tool| tool.name.clone()).collect::<Vec<_>>();
        let tool_snippets = selected_tools
            .iter()
            .filter_map(|name| tool_snippet(name).map(|snippet| (name.clone(), snippet.to_owned())))
            .collect::<HashMap<_, _>>();
        let prompt_guidelines = tools
            .iter()
            .flat_map(|tool| tool.prompt_guidelines.iter().cloned())
            .collect();
        let (readme_path, docs_path, examples_path) = match resource_discovery {
            ResourceDiscovery::Disabled => (
                "README.md".to_owned(),
                "docs".to_owned(),
                "examples".to_owned(),
            ),
            ResourceDiscovery::Global | ResourceDiscovery::TrustedProject => {
                (String::new(), String::new(), String::new())
            }
        };
        let system_prompt = build_system_prompt(BuildSystemPromptOptions {
            custom_prompt,
            selected_tools,
            tool_snippets,
            prompt_guidelines,
            cwd: cwd_text.clone(),
            additional_workspace_roots: workspace
                .additional_roots()
                .iter()
                .map(|root| root.to_string_lossy().into_owned())
                .collect(),
            context_files,
            skills: initial_skills,
            readme_path,
            docs_path,
            examples_path,
            ..BuildSystemPromptOptions::default()
        });
        let key_runtime = shared.clone();
        let stream_fn = if let Some(resolver) = auth_resolver {
            let fallback = effective_stream_fn.clone();
            Arc::new(move |model: Model, context: Context, mut options: SimpleStreamOptions| {
                let resolver = resolver.clone();
                let fallback = fallback.clone();
                let future: pi_agent::BoxFuture<pi_ai::AssistantMessageEventStream> = Box::pin(async move {
                    match resolver(model.clone()).await {
                        Ok(auth) => {
                            options.stream.api_key = Some(auth.api_key);
                            merge_headers_case_insensitive(&mut options.stream.headers, auth.headers);
                            options.stream.env.extend(auth.env);
                            fallback(model, context, options).await
                        }
                        Err(error) => auth_error_stream(&model, error.to_string()).await,
                    }
                });
                future
            }) as StreamFn
        } else {
            effective_stream_fn.clone()
        };
        let goal_runtime = shared.clone();
        let agent = Agent::new(AgentOptions {
            initial_state: AgentState {
                system_prompt: system_prompt.clone(),
                model,
                thinking_level,
                tools: tools.clone(),
                messages: Vec::new(),
                ..AgentState::default()
            },
            stream_options,
            get_api_key: Some(Arc::new(move |_| {
                Some(key_runtime.state.read().api_key.clone())
            })),
            convert_to_llm: Some(Arc::new(move |mut messages| {
                if let Some(message) = goal_context_message(&goal_runtime.goal.read().get()) {
                    messages.push(message);
                }
                Ok(pi_ai::messages_to_llm(&messages))
            })),
            transform_context: None,
            before_tool_call,
            after_tool_call: Some(compose_bash_spill_after_tool_call(
                shared.clone(),
                after_tool_call,
            )),
            stream_fn,
            ..AgentOptions::default()
        });
        let history_shared = shared.clone();
        let history_subscription = poll_immediate(agent.subscribe_simple(move |event| {
            let shared = history_shared.clone();
            async move {
                if let AgentEvent::MessageEnd { message } = event {
                    shared.state.write().messages.push(message);
                }
                Ok(())
            }
        }))?;
        Ok(Self {
            inner: Arc::new(SessionInner {
                cwd,
                workspace,
                system_prompt,
                abort_notify: Notify::new(),
                base_tools,
                all_tools: RwLock::new(available_tools),
                tools: RwLock::new(tools),
                tool_selection: selection,
                shared,
                agent,
                _history_subscription: history_subscription,
                run_slot: Mutex::new(RunSlot::default()),
                idle: Notify::new(),
                resources: RwLock::new(None),
                bash_controller: Mutex::new(None),
                bash_generation: AtomicU64::new(0),
                bash_append_lock: tokio::sync::Mutex::new(()),
                pending_bash_messages: Mutex::new(Vec::new()),
                process_manager,
                process_owner_id,
                skill_snapshot,
                todo,
            }),
        })
    }

    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.inner.cwd
    }

    #[must_use]
    pub fn workspace_roots(&self) -> &crate::WorkspaceRoots {
        &self.inner.workspace
    }

    pub async fn system_prompt(&self) -> String {
        self.inner.agent.state().await.system_prompt
    }

    #[must_use]
    pub fn process_manager(&self) -> ProcessManager {
        self.inner.process_manager.clone()
    }

    #[must_use]
    pub fn process_owner_id(&self) -> ProcessOwnerId {
        self.inner.process_owner_id.clone()
    }

    #[must_use]
    pub fn model(&self) -> Option<Model> {
        Some(self.inner.shared.state.read().model.clone())
    }

    #[must_use]
    pub fn thinking_level(&self) -> ThinkingLevel {
        self.inner.shared.state.read().thinking_level
    }

    #[must_use]
    pub fn child_session_options_snapshot(&self) -> ChildSessionOptionsSnapshot {
        let state = self.inner.shared.state.read();
        ChildSessionOptionsSnapshot {
            model: state.model.clone(),
            cwd: self.inner.cwd.clone(),
            thinking_level: state.thinking_level,
            api_key: state.api_key.clone(),
            stream_options: self.inner.shared.stream_options.read().clone(),
            stream_fn: self.inner.shared.stream_fn.clone(),
            auth_resolver: self.inner.shared.auth_resolver.clone(),
        }
    }



    #[must_use]
    pub fn stream_options(&self) -> SimpleStreamOptions {
        self.inner.shared.stream_options.read().clone()
    }

    pub async fn set_stream_options(&self, options: SimpleStreamOptions) {
        *self.inner.shared.stream_options.write() = options.clone();
        self.inner.agent.set_stream_options(options).await;
    }

    #[must_use]
    pub fn branch_summary_settings(&self) -> crate::EffectiveBranchSummarySettings {
        *self.inner.shared.branch_summary.read()
    }

    #[must_use]
    pub fn expose_session_environment(&self) -> bool {
        self.inner.shared.expose_session_environment.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn compaction_settings(&self) -> Option<CompactionSettings> {
        *self.inner.shared.compaction.read()
    }

    #[must_use]
    pub fn is_compacting(&self) -> bool {
        self.inner.shared.compaction_active.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn auto_compaction_enabled(&self) -> bool {
        self.inner
            .shared
            .compaction
            .read()
            .is_some_and(|settings| settings.enabled)
    }

    #[must_use]
    pub fn session_name(&self) -> Option<String> {
        self.inner.shared.session_name.read().clone()
    }

    pub fn set_session_name(&self, name: &str) -> Result<()> {
        let normalized = crate::session_store::normalize_session_name(name);
        if let Some(recorder) = self.inner.shared.recorder.lock().as_ref().cloned() {
            recorder.record_session_name(normalized.as_deref().unwrap_or_default())?;
        }
        *self.inner.shared.session_name.write() = normalized;
        Ok(())
    }

    #[must_use]
    pub fn todo_state(&self) -> TodoState {
        self.inner.todo.state()
    }
    pub(crate) fn set_todo_mutation_transaction(&self, transaction: crate::todo::TodoMutationTransaction) {
        self.inner.todo.set_mutation_transaction(transaction);
    }

    pub(crate) fn apply_todo_raw(&self, op: TodoOp) -> Result<TodoApplyResult> {
        self.inner.todo.apply_raw(op).inspect_err(|_| self.schedule_todo_reminder())
    }

    pub(crate) fn set_todos_raw(&self, phases: Vec<TodoPhase>) -> Result<TodoApplyResult> {
        self.inner.todo.set_phases_raw(phases)
    }

    pub fn apply_todo(&self, op: TodoOp) -> Result<TodoApplyResult> {
        self.inner.todo.apply(op).inspect_err(|_| self.schedule_todo_reminder())
    }

    pub fn set_todos(&self, phases: Vec<TodoPhase>) -> Result<TodoApplyResult> {
        self.inner.todo.set_phases(phases)
    }

    pub fn schedule_todo_reminder(&self) {
        self.inner.todo.schedule_reminder();
    }

    #[must_use]
    pub fn todo_reminder_pending(&self) -> bool {
        self.inner.todo.reminder_pending()
    }

    pub async fn steering_mode(&self) -> QueueMode {
        self.inner.agent.steering_mode().await
    }


    pub async fn send_custom_message(
        &self,
        message: CustomMessage,
        delivery: MessageDelivery,
        trigger_turn: bool,
    ) -> Result<()> {
        validate_extension_message_content(&message.content)?;
        let message = Message::Custom(message);
        if delivery == MessageDelivery::NextTurn {
            self.inner.shared.pending_next_turn.lock().push(message);
            return Ok(());
        }
        if self.inner.run_slot.lock().active {
            match delivery {
                MessageDelivery::FollowUp => self.inner.agent.follow_up(message).await,
                MessageDelivery::Steer => self.inner.agent.steer(message).await,
                MessageDelivery::NextTurn => unreachable!("handled above"),
            }
            self.publish_queue_update().await;
            return Ok(());
        }
        if trigger_turn {
            self.run_messages(vec![message]).await?;
        } else {
            self.append_idle_message(message).await?;
        }
        Ok(())
    }

    pub async fn send_user_message(
        &self,
        content: CustomMessageContent,
        delivery: MessageDelivery,
    ) -> Result<()> {
        validate_extension_message_content(&content)?;
        if delivery == MessageDelivery::NextTurn {
            return Err(anyhow!("user messages do not support nextTurn delivery"));
        }
        let message = Message::User(UserMessage {
            content: content.into_blocks(),
            timestamp: pi_ai::now_millis(),
        });
        if self.inner.run_slot.lock().active {
            match delivery {
                MessageDelivery::FollowUp => self.inner.agent.follow_up(message).await,
                MessageDelivery::Steer => self.inner.agent.steer(message).await,
                MessageDelivery::NextTurn => unreachable!("rejected above"),
            }
            self.publish_queue_update().await;
            return Ok(());
        }
        self.run_messages(vec![message]).await?;
        Ok(())
    }

    pub fn append_custom_entry(&self, custom_type: &str, data: Option<serde_json::Value>) -> Result<String> {
        let recorder = self.current_recorder()?;
        let id = recorder.record_custom_entry(custom_type, data)?;
        self.publish_recorded_entry(&recorder, &id)?;
        Ok(id)
    }

    async fn append_idle_message(&self, message: Message) -> Result<()> {
        let recorder = self.current_recorder()?;
        let id = match &message {
            Message::Custom(custom) => recorder.record_custom_message(custom)?,
            _ => recorder.record_message(&message)?,
        };
        let messages = {
            let mut state = self.inner.shared.state.write();
            state.messages.push(message);
            state.messages.clone()
        };
        self.inner.agent.set_messages(messages).await;
        self.inner.shared.recorded_count.fetch_add(1, Ordering::AcqRel);
        self.publish_recorded_entry(&recorder, &id)?;
        Ok(())
    }

    async fn append_bash_message(&self, message: Message) -> Result<()> {
        let _append = self.inner.bash_append_lock.lock().await;
        if self.inner.run_slot.lock().active {
            self.inner.pending_bash_messages.lock().push(message);
            return Ok(());
        }
        self.append_bash_message_now(message).await
    }

    async fn append_bash_message_now(&self, message: Message) -> Result<()> {
        if self.inner.shared.recorder.lock().is_some() {
            return self.append_idle_message(message).await;
        }
        let messages = {
            let mut state = self.inner.shared.state.write();
            state.messages.push(message);
            state.messages.clone()
        };
        self.inner.agent.set_messages(messages).await;
        Ok(())
    }

    async fn flush_pending_bash_messages(&self) -> Result<()> {
        let _append = self.inner.bash_append_lock.lock().await;
        let pending = std::mem::take(&mut *self.inner.pending_bash_messages.lock());
        for message in pending {
            self.append_bash_message_now(message).await?;
        }
        Ok(())
    }

    fn publish_recorded_entry(&self, recorder: &SessionRecorder, id: &str) -> Result<()> {
        let entry = recorder
            .tree()?
            .entries
            .into_iter()
            .find(|entry| entry.id == id)
            .ok_or_else(|| anyhow!("recorded session entry {id} was not found"))?;
        self.publish_session_event(SessionEvent::EntryAppended { entry });
        Ok(())
    }

    async fn publish_queue_update(&self) {
        let (steering, follow_up) = self.inner.agent.queued_messages().await;
        self.publish_session_event(SessionEvent::QueueUpdate { steering, follow_up });
    }
    pub async fn set_steering_mode(&self, mode: QueueMode) {
        self.inner.agent.set_steering_mode(mode).await;
    }

    pub async fn follow_up_mode(&self) -> QueueMode {
        self.inner.agent.follow_up_mode().await
    }

    pub async fn set_follow_up_mode(&self, mode: QueueMode) {
        self.inner.agent.set_follow_up_mode(mode).await;
    }

    pub async fn pending_message_count(&self) -> usize {
        self.inner.agent.pending_message_count().await
    }

    pub async fn queued_messages(&self) -> (Vec<Message>, Vec<Message>) {
        self.inner.agent.queued_messages().await
    }

    pub async fn drain_queued_messages(&self) -> (Vec<Message>, Vec<Message>) {
        let drained = self.inner.agent.drain_queued_messages().await;
        self.publish_session_event(SessionEvent::QueueUpdate { steering: Vec::new(), follow_up: Vec::new() });
        drained
    }

    #[must_use]
    pub fn subscribe_session_events(&self) -> broadcast::Receiver<SessionEvent> {
        self.inner.shared.events.subscribe()
    }

    pub(crate) fn publish_session_event(&self, event: SessionEvent) {
        let _ = self.inner.shared.events.send(event);
    }

    pub async fn attach_resources(&self, resources: ResourceManager) -> Result<()> {
        let tools = self.inner.tools.read().clone();
        let update = self.prepare_resource_update_with_tools(resources.snapshot(), tools)?;
        self.commit_resource_update(update).await;
        *self.inner.resources.write() = Some(resources);
        Ok(())
    }

    #[must_use]
    pub fn resource_manager(&self) -> Option<ResourceManager> {
        self.inner.resources.read().clone()
    }

    #[must_use]
    pub fn last_selection(&self) -> Option<crate::SelectionPlan> {
        self.inner.shared.last_selection.read().clone()
    }

    pub fn take_prepared_selection(&self, request: &str) -> Option<crate::SelectionPlan> {
        let mut selection = self.inner.shared.last_selection.write();
        selection
            .as_ref()
            .is_some_and(|plan| plan.request == request)
            .then(|| selection.take())
            .flatten()
    }

    pub async fn select_for_request(&self, request: &str) -> crate::SelectionPlan {
        let settings = self.inner.shared.selector_settings.read().clone();
        let skills = self.inner.shared.selector_skills.read().clone();
        let agents = self.inner.shared.selector_agents.read().clone();
        let classifier = if settings.classifier.enabled {
            let state = self.inner.shared.state.read();
            let model = resolve_classifier_model(&settings, &state.model);
            drop(state);
            model.map(|model| {
                let (_, abort) = AbortController::new();
                crate::ProviderClassifier {
                    model,
                    stream: self.inner.shared.stream_fn.clone(),
                    options: self.inner.shared.stream_options.read().clone(),
                    abort,
                }
            })
        } else {
            None
        };
        let plan = crate::select(
            crate::SelectionInput {
                request,
                skills: &skills,
                agents: &agents,
                settings: &settings,
            },
            classifier.as_ref(),
        )
        .await;
        *self.inner.shared.last_selection.write() = Some(plan.clone());
        plan
    }

    pub async fn reload_resources(&self) -> Result<crate::ReloadResult> {

        let resources = self
            .resource_manager()
            .ok_or_else(|| anyhow!("session has no resource manager"))?;
        let candidate = resources.stage_reload()?;
        let update = self.prepare_resource_update(candidate.snapshot(), Vec::new())?;
        let result = resources.commit_reload(candidate)?;
        self.commit_resource_update(update).await;
        Ok(result)
    }

    pub(crate) fn prepare_resource_update(
        &self,
        snapshot: Arc<crate::ResourceSnapshot>,
        mut additional_tools: Vec<AgentTool>,
    ) -> Result<SessionResourceUpdate> {
        let runtime_settings = snapshot.settings.runtime_settings()?;
        if runtime_settings.process_tool_enabled
            && !additional_tools.iter().any(|tool| tool.name == "process")
        {
            additional_tools.push(process_tool(
                &self.inner.cwd,
                self.inner.process_manager.clone(),
                self.inner.process_owner_id.clone(),
            ));
        }
        if runtime_settings.todo_tool_enabled
            && !additional_tools.iter().any(|tool| tool.name == "todo")
        {
            additional_tools.push(create_todo_tool(self.inner.todo.clone()));
        }
        if runtime_settings.glob_tool_enabled
            && !additional_tools.iter().any(|tool| tool.name == "glob")
        {
            additional_tools.push(crate::create_glob_tool_for_workspace(
                self.inner.workspace.clone(),
            ));
        }
        let all_tools = merge_tools(&self.inner.base_tools, additional_tools)?;
        let tools = select_tools(all_tools.clone(), &self.inner.tool_selection)?;
        self.prepare_resource_update_with_runtime(
            snapshot,
            tools,
            Some(all_tools),
            runtime_settings,
        )
    }

    fn prepare_resource_update_with_tools(
        &self,
        snapshot: Arc<crate::ResourceSnapshot>,
        tools: Vec<AgentTool>,
    ) -> Result<SessionResourceUpdate> {
        let runtime_settings = snapshot.settings.runtime_settings()?;
        self.prepare_resource_update_with_runtime(snapshot, tools, None, runtime_settings)
    }

    fn prepare_resource_update_with_runtime(
        &self,
        snapshot: Arc<crate::ResourceSnapshot>,
        tools: Vec<AgentTool>,
        all_tools: Option<Vec<AgentTool>>,
        runtime_settings: crate::RuntimeSettingsSnapshot,
    ) -> Result<SessionResourceUpdate> {
        if self.inner.run_slot.lock().active {
            return Err(anyhow!("cannot reload resources while session is processing"));
        }
        let custom_prompt = snapshot
            .system_prompt
            .clone()
            .unwrap_or_else(|| self.inner.system_prompt.clone());
        let selected_tools = tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>();
        let tool_snippets = selected_tools
            .iter()
            .filter_map(|name| tool_snippet(name).map(|snippet| (name.clone(), snippet.to_owned())))
            .collect::<HashMap<_, _>>();
        let prompt_guidelines = tools
            .iter()
            .flat_map(|tool| tool.prompt_guidelines.iter().cloned())
            .collect();
        let system_prompt = build_system_prompt(BuildSystemPromptOptions {
            custom_prompt,
            selected_tools,
            tool_snippets,
            prompt_guidelines,
            append_system_prompt: snapshot.append_system_prompt.join("\n\n"),
            cwd: self.inner.cwd.to_string_lossy().into_owned(),
            additional_workspace_roots: self
                .inner
                .workspace
                .additional_roots()
                .iter()
                .map(|root| root.to_string_lossy().into_owned())
                .collect(),
            context_files: snapshot.context_files.clone(),
            skills: snapshot.skills.clone(),
            ..BuildSystemPromptOptions::default()
        });
        let agents = crate::enabled_agent_definitions(
            &snapshot.agents,
            &snapshot.settings.agents,
        )
        .into_iter()
        .cloned()
        .collect();
        Ok(SessionResourceUpdate {
            tools,
            all_tools,
            system_prompt,
            skills: snapshot.skills.clone(),
            agents,
            selector_settings: snapshot.settings.selector.clone().unwrap_or_default(),
            runtime_settings,
        })
    }

    async fn commit_runtime_settings(&self, settings: crate::RuntimeSettingsSnapshot) {
        self.inner.agent.set_steering_mode(settings.steering_mode).await;
        self.inner.agent.set_follow_up_mode(settings.follow_up_mode).await;
        self.set_retry_settings(settings.retry);
        self.enable_compaction(settings.compaction);
        let desired = settings.stream_options;
        let mut stream_options = self.stream_options();
        stream_options.stream.temperature = desired.stream.temperature;
        stream_options.stream.max_tokens = desired.stream.max_tokens;
        stream_options.stream.transport = desired.stream.transport;
        stream_options.stream.cache_retention = desired.stream.cache_retention;
        stream_options.stream.timeout_ms = desired.stream.timeout_ms;
        stream_options.stream.websocket_connect_timeout_ms =
            desired.stream.websocket_connect_timeout_ms;
        stream_options.stream.max_retries = desired.stream.max_retries;
        stream_options.stream.max_retry_delay_ms = desired.stream.max_retry_delay_ms;
        stream_options.reasoning = desired.reasoning;
        stream_options.thinking_budgets = desired.thinking_budgets;
        self.set_stream_options(stream_options).await;
        *self.inner.shared.branch_summary.write() = settings.branch_summary;
        self.inner
            .shared
            .expose_session_environment
            .store(settings.expose_session_environment, Ordering::Release);
    }

    /// Applies only runtime-projected settings without rebuilding resources,
    /// extensions, tools, or prompt state.
    pub(crate) async fn apply_runtime_settings(&self, settings: crate::RuntimeSettingsSnapshot) {
        self.commit_runtime_settings(settings).await;
    }

    pub(crate) async fn commit_resource_update(&self, update: SessionResourceUpdate) {
        self.inner
            .agent
            .set_tools_and_system_prompt(update.tools.clone(), update.system_prompt)
            .await;
        self.commit_runtime_settings(update.runtime_settings).await;
        *self.inner.tools.write() = update.tools;
        if let Some(all_tools) = update.all_tools {
            *self.inner.all_tools.write() = all_tools;
        }
        *self.inner.skill_snapshot.write() = update.skills.clone();
        *self.inner.shared.selector_skills.write() = update.skills;
        *self.inner.shared.selector_agents.write() = update.agents;
        *self.inner.shared.selector_settings.write() = update.selector_settings;
    }
    #[must_use]
    pub fn get_active_tool_names(&self) -> Vec<String> {
        self.inner
            .tools
            .read()
            .iter()
            .map(|tool| tool.name.clone())
            .collect()
    }

    #[must_use]
    pub fn get_all_tools(&self) -> Vec<AgentTool> {
        self.inner.all_tools.read().clone()
    }

    #[must_use]
    pub fn get_tool_definition(&self, name: &str) -> Option<AgentTool> {
        self.inner
            .all_tools
            .read()
            .iter()
            .find(|tool| tool.name == name)
            .cloned()
    }

    pub async fn set_active_tools_by_name(&self, names: &[String]) -> Result<()> {
        let tools = select_tools(
            self.inner.all_tools.read().clone(),
            &ToolSelection {
                allow: Some(names.to_vec()),
                deny: Vec::new(),
                disable_all: false,
                disable_builtins: self.inner.tool_selection.disable_builtins,
                enable_process: self.inner.tool_selection.enable_process,
                enable_glob: self.inner.tool_selection.enable_glob,
            },
        )?;
        let snapshot = self
            .resource_manager()
            .map(|resources| resources.snapshot())
            .ok_or_else(|| anyhow!("session has no resource manager"))?;
        let update = self.prepare_resource_update_with_tools(snapshot, tools)?;
        self.inner
            .agent
            .set_tools_and_system_prompt(update.tools.clone(), update.system_prompt.clone())
            .await;
        *self.inner.tools.write() = update.tools;
        Ok(())
    }

    pub async fn current_system_prompt(&self) -> String {
        self.inner.agent.state().await.system_prompt
    }

    #[must_use]
    pub fn current_api_key(&self) -> String {
        self.inner.shared.state.read().api_key.clone()
    }


    pub fn set_model(&self, model: Model, api_key: String) -> ThinkingLevelChange {
        self.clear_active_retry_fallback();
        self.set_model_internal(model, api_key)
    }

    fn set_model_internal(&self, model: Model, api_key: String) -> ThinkingLevelChange {
        let change = {
            let mut state = self.inner.shared.state.write();
            let requested = state.thinking_level;
            let effective = clamp_thinking_level(&model, requested);
            state.thinking_level = effective;
            state.model = model.clone();
            state.api_key = api_key;
            thinking_level_change(&model, requested, effective)
        };
        if let Some(recorder) = self.inner.shared.recorder.lock().as_ref() {
            let _ = recorder.record_model_change(&model.provider, &model.id);
            if change.clamped {
                let _ = recorder.record_thinking_level(thinking_level_name(change.effective));
            }
        }
        change
    }

    #[must_use]
    pub fn retry_fallback_model(&self) -> Option<String> {
        self.inner.shared.active_retry_fallback.lock().as_ref()?;
        self.model().map(|model| {
            format_retry_fallback_selector(
                &model,
                Some(thinking_level_name(self.thinking_level())),
            )
        })
    }

    pub fn clear_active_retry_fallback(&self) {
        *self.inner.shared.active_retry_fallback.lock() = None;
        self.inner.shared.fallback_attempt_errors.lock().clear();
    }

    pub async fn set_model_with_resolved_auth(&self, model: Model) -> Result<ThinkingLevelChange> {
        if let Some(resolver) = &self.inner.shared.auth_resolver {
            let auth = resolver(model.clone()).await?;
            return Ok(self.set_model(model, auth.api_key));
        }
        let current = self.inner.shared.state.read();
        if current.model.provider != model.provider {
            return Err(anyhow!(
                "cannot switch extension model provider without an auth resolver"
            ));
        }
        let api_key = current.api_key.clone();
        drop(current);
        Ok(self.set_model(model, api_key))
    }

    pub fn set_thinking_level(&self, level: ThinkingLevel) -> ThinkingLevelChange {
        let change = {
            let mut state = self.inner.shared.state.write();
            let effective = clamp_thinking_level(&state.model, level);
            state.thinking_level = effective;
            thinking_level_change(&state.model, level, effective)
        };
        if let Some(recorder) = self.inner.shared.recorder.lock().as_ref() {
            let _ = recorder.record_thinking_level(thinking_level_name(change.effective));
        }
        change
    }

    pub async fn load_history(&self, messages: Vec<Message>) -> Result<()> {
        let mut guard = self.claim_exclusive()?;
        self.inner.agent.set_messages(messages.clone()).await;
        self.inner.agent.clear_all_queues().await;
        self.inner.shared.state.write().messages = messages;
        *self.inner.shared.compaction_runtime.lock() = CompactionRuntime::default();
        self.inner.shared.compaction_active.store(false, Ordering::Release);
        guard.release();
        Ok(())
    }

    #[must_use]
    pub fn history(&self) -> Vec<Message> {
        self.inner.shared.state.read().messages.clone()
    }

    pub async fn reset(&self) -> Result<()> {
        let mut guard = self.claim_exclusive()?;
        self.inner.agent.reset().await;
        self.inner.shared.state.write().messages.clear();
        *self.inner.shared.compaction_runtime.lock() = CompactionRuntime::default();
        self.inner.shared.compaction_active.store(false, Ordering::Release);
        *self.inner.shared.session_name.write() = None;
        // Prior conversation is discarded; release any detached bash spill files.
        self.cleanup_bash_spills();
        guard.release();
        Ok(())
    }

    pub fn set_before_compaction(&self, hook: Option<BeforeCompactionFn>) {
        *self.inner.shared.before_compaction.write() = hook;
    }

    pub(crate) fn before_compaction(&self) -> Option<BeforeCompactionFn> {
        self.inner.shared.before_compaction.read().clone()
    }

    pub async fn compact(&self, custom_instructions: Option<&str>) -> Result<CompactionResult> {
        let mut guard = self.claim_exclusive()?;
        let result = self.perform_compaction(CompactionReason::Manual, false, custom_instructions).await;
        guard.release();
        result
    }

    async fn perform_compaction(
        &self,
        reason: CompactionReason,
        will_retry: bool,
        custom_instructions: Option<&str>,
    ) -> Result<CompactionResult> {
        if let Some(hook) = self.before_compaction() {
            let reduction = hook(BeforeCompactionContext {
                reason,
                will_retry,
                custom_instructions: custom_instructions.map(str::to_owned),
            }).await?;
            if reduction.cancel {
                return Err(anyhow!("Compaction cancelled by extension"));
            }
            if let Some(result) = reduction.compaction {
                self.publish_session_event(SessionEvent::CompactionStart { reason });
                let applied = self.apply_external_compaction(&result).await;
                match applied {
                    Ok(()) => {
                        self.publish_session_event(SessionEvent::CompactionEnd {
                            reason,
                            result: Some(result.clone()),
                            aborted: false,
                            will_retry,
                            error_message: None,
                        });
                        return Ok(result);
                    }
                    Err(error) => {
                        self.publish_session_event(SessionEvent::CompactionEnd {
                            reason,
                            result: None,
                            aborted: false,
                            will_retry: false,
                            error_message: Some(error.to_string()),
                        });
                        return Err(error);
                    }
                }
            }
        }
        let (controller, abort) = AbortController::new();
        *self.inner.shared.compaction_controller.lock() = Some(controller);
        self.publish_session_event(SessionEvent::CompactionStart { reason });
        let _activity = CompactionActivityGuard::begin(self.inner.shared.clone());
        let result = self.generate_compaction(reason, custom_instructions, abort.clone()).await;
        self.inner.shared.compaction_controller.lock().take();
        match &result {
            Ok(compaction) => self.publish_session_event(SessionEvent::CompactionEnd {
                reason,
                result: Some(compaction.clone()),
                aborted: false,
                will_retry,
                error_message: None,
            }),
            Err(error) => {
                let aborted = abort.is_aborted();
                self.publish_session_event(SessionEvent::CompactionEnd {
                    reason,
                    result: None,
                    aborted,
                    will_retry: false,
                    error_message: (!aborted).then(|| error.to_string()),
                });
            }
        }
        result
    }

    async fn apply_external_compaction(&self, result: &CompactionResult) -> Result<()> {
        let recorder = self.current_recorder()?;
        let tree = recorder.tree()?;
        let first_kept_index = tree
            .branch(None)
            .into_iter()
            .filter(|entry| entry.entry_type == "message")
            .position(|entry| entry.id == result.first_kept_entry_id)
            .ok_or_else(|| anyhow!("extension compaction firstKeptEntryId was not found"))?;
        let messages = self.inner.agent.state().await.messages;
        if first_kept_index >= messages.len() {
            return Err(anyhow!("extension compaction firstKeptEntryId is outside the live context"));
        }
        let compacted = apply_checkpoint(&result.summary, &messages, first_kept_index);
        recorder.record_compaction(
            &result.summary,
            Some(&result.first_kept_entry_id),
            result.tokens_before,
            &messages[first_kept_index..],
        )?;
        {
            let mut state = self.inner.shared.compaction_runtime.lock();
            state.prefix_len = 1;
            state.summary.clone_from(&result.summary);
            if let Some(details) = &result.details {
                state.read_files = details.read_files.iter().cloned().collect();
                state.modified_files = details.modified_files.iter().cloned().collect();
            }
        }
        self.inner.agent.set_messages(compacted.clone()).await;
        self.inner.shared.state.write().messages = compacted;
        Ok(())
    }

    async fn generate_compaction(
        &self,
        reason: CompactionReason,
        custom_instructions: Option<&str>,
        abort: AbortSignal,
    ) -> Result<CompactionResult> {
        let messages = self.inner.agent.state().await.messages;
        let settings = self.inner.shared.compaction.read().unwrap_or(crate::DEFAULT_COMPACTION_SETTINGS);
        let (prefix_len, previous_summary, mut read_files, mut modified_files) = {
            let state = self.inner.shared.compaction_runtime.lock();
            (state.prefix_len.min(messages.len()), state.summary.clone(), state.read_files.clone(), state.modified_files.clone())
        };
        let current = if previous_summary.is_empty() { messages.clone() } else { apply_checkpoint(&previous_summary, &messages, prefix_len) };
        let tokens_before = estimate_context_tokens_usage_aware(&current);
        let cut = find_cut_point(&messages, prefix_len, messages.len(), settings.keep_recent_tokens);
        if cut.first_kept_index <= prefix_len || cut.first_kept_index >= messages.len() {
            return Err(anyhow!("Nothing to compact (session too small)"));
        }
        let history_end = if cut.is_split_turn { cut.turn_start_index.unwrap_or(cut.first_kept_index) } else { cut.first_kept_index };
        let history = &messages[prefix_len..history_end];
        let turn_prefix = if cut.is_split_turn { &messages[history_end..cut.first_kept_index] } else { &[] };
        let mut summary = if turn_prefix.is_empty() {
            summarize_messages(&self.inner.shared, history, &previous_summary, custom_instructions, settings.reserve_tokens, 0.8, abort.clone(), Some(reason)).await?
        } else {
            let history_summary = if history.is_empty() { "No prior history.".to_owned() } else {
                summarize_messages(&self.inner.shared, history, &previous_summary, custom_instructions, settings.reserve_tokens, 0.8, abort.clone(), Some(reason)).await?
            };
            let prefix_summary = summarize_turn_prefix(&self.inner.shared, turn_prefix, settings.reserve_tokens, abort.clone(), reason).await?;
            format!("{history_summary}\n\n---\n\n**Turn Context (split turn):**\n\n{prefix_summary}")
        };
        if abort.is_aborted() { return Err(anyhow!("Compaction cancelled")); }
        let (new_read_files, new_modified_files) = compute_file_lists(&messages[prefix_len..cut.first_kept_index]);
        for path in new_modified_files { read_files.remove(&path); modified_files.insert(path); }
        for path in new_read_files { if !modified_files.contains(&path) { read_files.insert(path); } }
        let all_read_files = read_files.iter().cloned().collect::<Vec<_>>();
        let all_modified_files = modified_files.iter().cloned().collect::<Vec<_>>();
        summary.push_str(&format_file_operations(&all_read_files, &all_modified_files));
        let recorder = self.inner.shared.recorder.lock().as_ref().cloned();
        let first_kept_entry_id = recorder.as_ref().and_then(|recorder| recorder.tree().ok()).and_then(|tree| {
            tree.branch(None).into_iter().filter(|entry| entry.entry_type == "message").nth(cut.first_kept_index).map(|entry| entry.id.clone())
        }).unwrap_or_else(|| Uuid::now_v7().to_string());
        let compacted = apply_checkpoint(&summary, &messages, cut.first_kept_index);
        let estimated_tokens_after = estimate_context_tokens_usage_aware(&compacted);
        if abort.is_aborted() { return Err(anyhow!("Compaction cancelled")); }
        if let Some(recorder) = recorder {
            recorder.record_compaction(&summary, Some(&first_kept_entry_id), tokens_before, &messages[cut.first_kept_index..])?;
        }
        {
            let mut state = self.inner.shared.compaction_runtime.lock();
            state.prefix_len = 1;
            state.summary.clone_from(&summary);
            state.read_files = read_files;
            state.modified_files = modified_files;
        }
        self.inner.agent.set_messages(compacted.clone()).await;
        self.inner.shared.state.write().messages = compacted;
        Ok(CompactionResult {
            summary,
            first_kept_entry_id,
            tokens_before,
            estimated_tokens_after: Some(estimated_tokens_after),
            usage: None,
            details: Some(CompactionDetails { read_files: all_read_files, modified_files: all_modified_files }),
        })
    }

    pub fn abort_compaction(&self) {
        if let Some(controller) = self.inner.shared.compaction_controller.lock().as_ref() { controller.abort(); }
    }


    fn claim_exclusive(&self) -> Result<ActiveRunGuard> {
        let mut slot = self.inner.run_slot.lock();
        if slot.active {
            return Err(anyhow!("session is already processing a prompt"));
        }
        slot.active = true;
        slot.abort_requested = false;
        slot.generation = slot.generation.wrapping_add(1);
        let generation = slot.generation;
        drop(slot);
        Ok(ActiveRunGuard {
            inner: self.inner.clone(),
            released: false,
            generation,
        })
    }

    pub fn record(&self, recorder: SessionRecorder) -> Result<()> {
        let goal = GoalRuntime::from_session_recorder(recorder.clone())
            .map_err(|error| anyhow!(error.to_string()))?;
        let phases = recorder
            .latest_todo_state()
            .ok()
            .flatten()
            .map_or_else(Vec::new, |state| state.phases);
        self.inner.todo.restore_state(phases)?;
        *self.inner.shared.session_name.write() = recorder.session_name();
        *self.inner.shared.recorder.lock() = Some(Arc::new(recorder));
        *self.inner.shared.goal.write() = goal;
        Ok(())
    }

    pub fn start_new_recording(&self) -> Result<()> {
        let state = self.inner.shared.state.read();
        let recorder = crate::start_session(
            &self.inner.cwd,
            Some(&state.model),
            Some(thinking_level_name(state.thinking_level)),
        )?;
        drop(state);
        self.record(recorder)
    }

    pub fn start_new_recording_with_parent(&self, parent_session: Option<&Path>) -> Result<()> {
        let state = self.inner.shared.state.read();
        let recorder = crate::start_session_with_parent(
            &self.inner.cwd,
            Some(&state.model),
            Some(thinking_level_name(state.thinking_level)),
            parent_session,
        )?;
        drop(state);
        self.record(recorder)
    }

    pub async fn switch_session(&self, path: &Path) -> Result<()> {
        self.switch_prepared_session(crate::PreparedSessionResume::prepare_path(path)?)
            .await
    }

    pub async fn switch_prepared_session(
        &self,
        prepared: crate::PreparedSessionResume,
    ) -> Result<()> {
        if !prepared.target_cwd().as_os_str().is_empty()
            && prepared.target_cwd() != self.inner.cwd
        {
            return Err(anyhow!(
                "session working directory {} does not match {}",
                prepared.target_cwd().display(),
                self.inner.cwd.display()
            ));
        }
        let context = prepared.build_context();
        let recorder = prepared.into_recorder()?;
        self.cleanup_bash_spills();
        self.load_history(context.messages).await?;
        if let Some(provider) = context.provider.as_deref()
            && let Some(model_id) = context.model_id.as_deref()
            && let Some(model) = pi_ai::get_model(provider, model_id)
        {
            let api_key = self.inner.shared.state.read().api_key.clone();
            self.set_model(model, api_key);
        }
        self.set_thinking_level(parse_recorded_thinking_level(&context.thinking_level));
        self.record(recorder)?;
        Ok(())
    }

    pub async fn navigate_tree(&self, target_id: &str, options: NavigateTreeOptions) -> Result<NavigateTreeResult> {
        let mut guard = self.claim_exclusive()?;
        let result = self.navigate_tree_claimed(target_id, options).await;
        guard.release();
        result
    }

    async fn navigate_tree_claimed(&self, target_id: &str, options: NavigateTreeOptions) -> Result<NavigateTreeResult> {
        let recorder = self.current_recorder()?;
        let tree = recorder.tree()?;
        let target = tree.entries.iter().find(|entry| entry.id == target_id).cloned().ok_or_else(|| anyhow!("Entry not found: {target_id}"))?;
        let current_leaf = recorder.active_leaf_id();
        let (new_leaf, editor_text) = navigation_target(&target);
        if current_leaf == new_leaf && editor_text.is_none() {
            return Ok(NavigateTreeResult { editor_text, active_leaf_id: current_leaf, summary_entry_id: None, changed: false, cancelled: false });
        }
        let abandoned = collect_abandoned_messages(&tree, current_leaf.as_deref(), new_leaf.as_deref());
        let summary = if let Some(summary) = options.summary.clone() {
            Some(summary)
        } else if options.summarize && !abandoned.is_empty() {
            let settings = self.branch_summary_settings();
            let default_prompt = "Summarize only the abandoned branch so the conversation can continue from an earlier point.";
            let custom_prompt = match options.custom_instructions.as_deref() {
                Some(instructions) => Some(instructions),
                None if !settings.skip_prompt => Some(default_prompt),
                None => None,
            };
            let (controller, abort) = AbortController::new();
            let operation = summarize_messages(&self.inner.shared, &abandoned, "", custom_prompt, settings.reserve_tokens, 0.5, abort, None);
            let mut operation = Box::pin(operation);
            let notified = self.inner.abort_notify.notified();
            if self.inner.run_slot.lock().abort_requested {
                controller.abort();
                let _ = operation.await;
                return Ok(NavigateTreeResult { editor_text: None, active_leaf_id: current_leaf, summary_entry_id: None, changed: false, cancelled: true });
            }
            tokio::select! {
                result = &mut operation => Some(result?),
                () = notified => {
                    controller.abort();
                    let _ = operation.await;
                    return Ok(NavigateTreeResult { editor_text: None, active_leaf_id: current_leaf, summary_entry_id: None, changed: false, cancelled: true });
                }
            }
        } else { None };
        let summary_entry_id = if let Some(summary) = summary.filter(|summary| !summary.trim().is_empty()) {
            let id = recorder.branch_with_summary(new_leaf.as_deref(), &summary)?;
            if let Some(label) = options.label.as_deref() {
                recorder.record_label(&id, Some(label))?;
            }
            Some(id)
        } else {
            match new_leaf.as_deref() { Some(entry_id) => recorder.branch(entry_id)?, None => recorder.reset_leaf() }
            None
        };
        let active_leaf_id = recorder.active_leaf_id();
        let tree = recorder.tree()?;
        let context = tree.build_context(None);
        let todo_phases = tree.latest_todo_state().map_or_else(Vec::new, |state| state.phases);
        self.inner.agent.set_messages(context.messages.clone()).await;
        self.inner.shared.state.write().messages = context.messages;
        self.inner.todo.restore_state(todo_phases)?;
        *self.inner.shared.compaction_runtime.lock() = CompactionRuntime::default();
        self.inner.shared.compaction_active.store(false, Ordering::Release);
        *self.inner.shared.goal.write() = GoalRuntime::from_session_recorder((*recorder).clone())
            .map_err(|error| anyhow!(error.to_string()))?;
        Ok(NavigateTreeResult { editor_text, active_leaf_id, summary_entry_id, changed: true, cancelled: false })
    }

    pub fn set_session_label(&self, target_id: &str, label: Option<&str>) -> Result<()> {
        let normalized = label.map(str::trim).filter(|label| !label.is_empty());
        self.current_recorder()?.record_label(target_id, normalized)?;
        Ok(())
    }

    pub async fn fork_session(&self, entry_id: &str, restore_conversation: bool) -> Result<String> {
        let recorder = self.current_recorder()?;
        let tree = recorder.tree()?;
        let selected = tree
            .entries
            .iter()
            .find(|entry| entry.id == entry_id)
            .ok_or_else(|| anyhow!("Invalid entry ID for forking"))?;
        let Some(Message::User(user)) = selected.message.as_ref() else {
            return Err(anyhow!("Invalid entry ID for forking"));
        };
        let text = content_text(&user.content);
        let current_path = recorder.path();
        let replacement = if let Some(parent_id) = selected.parent_id.as_deref() {
            crate::create_branched_session(&current_path, parent_id)?
        } else {
            let state = self.inner.shared.state.read();
            crate::start_session_with_parent(
                &self.inner.cwd,
                Some(&state.model),
                Some(thinking_level_name(state.thinking_level)),
                Some(&current_path),
            )?
        };
        replacement.persist_now()?;
        if restore_conversation {
            let context = replacement.tree()?.build_context(None);
            self.load_history(context.messages).await?;
        } else {
            self.load_history(Vec::new()).await?;
        }
        self.record(replacement)?;
        Ok(text)
    }

    pub async fn clone_session(&self) -> Result<()> {
        let recorder = self.current_recorder()?;
        let leaf_id = recorder
            .last_entry_id()
            .ok_or_else(|| anyhow!("Cannot clone session: no current entry selected"))?;
        let replacement = crate::create_branched_session(recorder.path(), &leaf_id)?;
        let context = replacement.tree()?.build_context(None);
        self.load_history(context.messages).await?;
        self.record(replacement)
    }

    pub fn fork_messages(&self) -> Result<Vec<crate::ForkMessage>> {
        let tree = self.current_recorder()?.tree()?;
        Ok(tree
            .entries
            .iter()
            .filter_map(|entry| {
                let Message::User(user) = entry.message.as_ref()? else {
                    return None;
                };
                let text = content_text(&user.content);
                (!text.is_empty()).then(|| crate::ForkMessage {
                    entry_id: entry.id.clone(),
                    text,
                })
            })
            .collect())
    }

    pub fn session_entries(&self, since: Option<&str>) -> Result<crate::SessionEntries> {
        let tree = self.current_recorder()?.tree()?;
        let entries = if let Some(since) = since {
            let index = tree
                .entries
                .iter()
                .position(|entry| entry.id == since)
                .ok_or_else(|| anyhow!("Entry not found: {since}"))?;
            tree.entries[index + 1..].to_vec()
        } else {
            tree.entries.clone()
        };
        Ok(crate::SessionEntries {
            entries,
            leaf_id: tree.leaf_id,
        })
    }

    pub fn session_tree(&self) -> Result<crate::SessionTreeResult> {
        let tree = self.current_recorder()?.tree()?;
        Ok(crate::SessionTreeResult {
            tree: tree.tree(),
            leaf_id: tree.leaf_id,
            active_leaf_id: tree.active_leaf_id,
        })
    }

    fn current_recorder(&self) -> Result<Arc<SessionRecorder>> {
        self.inner
            .shared
            .recorder
            .lock()
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow!("session recording is unavailable"))
    }


    #[must_use]
    pub fn goal_runtime(&self) -> GoalRuntime {
        self.inner.shared.goal.read().clone()
    }

    pub fn rebuild_goal_runtime(&self) -> Result<()> {
        let recorder = self.current_recorder()?;
        let runtime = GoalRuntime::from_session_recorder((*recorder).clone())
            .map_err(|error| anyhow!(error.to_string()))?;
        *self.inner.shared.goal.write() = runtime;
        Ok(())
    }

    #[must_use]
    pub fn recorder_info(&self) -> Option<(String, PathBuf)> {
        self.inner
            .shared
            .recorder
            .lock()
            .as_ref()
            .map(|recorder| (recorder.id(), recorder.path()))
    }

    #[must_use]
    pub fn session_header(&self) -> Option<crate::SessionHeader> {
        self.inner
            .shared
            .recorder
            .lock()
            .as_ref()
            .map(|recorder| recorder.header())
    }

    #[must_use]
    pub fn last_assistant_text(&self) -> String {
        self.inner
            .shared
            .state
            .read()
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::Assistant(message) => Some(message.text()),
                _ => None,
            })
            .unwrap_or_default()
    }

    #[must_use]
    pub fn retry_settings(&self) -> RetrySettings {
        self.inner.shared.retry_settings.read().clone()
    }

    pub fn set_retry_settings(&self, settings: RetrySettings) {
        let enabled = settings.enabled;
        *self.inner.shared.retry_settings.write() = settings;
        if !enabled {
            self.abort_retry();
        }
    }

    #[must_use]
    pub fn auto_retry_enabled(&self) -> bool {
        self.retry_settings().enabled
    }

    pub fn set_auto_retry_enabled(&self, enabled: bool) {
        let mut settings = self.retry_settings();
        settings.enabled = enabled;
        self.set_retry_settings(settings);
    }

    #[must_use]
    pub fn is_retrying(&self) -> bool {
        self.inner.shared.retry_controller.lock().is_some()
    }

    pub fn abort_retry(&self) {
        if let Some(controller) = self.inner.shared.retry_controller.lock().as_ref() {
            controller.abort();
        }
    }

    pub fn set_before_agent_start(&self, hook: Option<BeforeAgentStartFn>) {
        self.inner.agent.set_before_agent_start(hook);
    }

    pub fn set_transform_message(&self, hook: Option<TransformMessageFn>) {
        self.inner.agent.set_transform_message(hook);
    }

    pub fn set_transform_context(&self, hook: Option<TransformContextFn>) {
        self.inner.agent.set_transform_context(hook);
    }

    pub fn set_before_tool_call(&self, hook: Option<BeforeToolCallFn>) {
        self.inner.agent.set_before_tool_call(hook);
    }

    pub fn set_after_tool_call(&self, hook: Option<AfterToolCallFn>) {
        // Compose with spill tracking so agent `bash` tool success paths are
        // registered even when Application/extensions replace the after-hook.
        self.inner.agent.set_after_tool_call(Some(
            compose_bash_spill_after_tool_call(self.inner.shared.clone(), hook),
        ));
    }

    pub async fn execute_bash(&self, command: &str, exclude_from_context: bool) -> Result<crate::BashResult> {
        self.execute_bash_with_id(command, exclude_from_context, None).await
    }

    pub async fn execute_bash_with_id(
        &self,
        command: &str,
        exclude_from_context: bool,
        id: Option<String>,
    ) -> Result<crate::BashResult> {
        let (controller, signal) = AbortController::new();
        let generation = self.inner.bash_generation.fetch_add(1, Ordering::AcqRel) + 1;
        {
            let mut active = self.inner.bash_controller.lock();
            if active.is_some() {
                return Err(anyhow!("a bash command is already running"));
            }
            *active = Some(ActiveBash { generation, controller: controller.clone() });
        }
        let mut activity = BashActivityGuard {
            inner: self.inner.clone(),
            controller,
            generation,
            released: false,
        };
        let events = self.inner.shared.events.clone();
        let chunk_id = id.clone();
        let on_chunk: Arc<dyn Fn(String) + Send + Sync> = Arc::new(move |delta| {
            let _ = events.send(SessionEvent::BashExecutionUpdate { id: chunk_id.clone(), delta });
        });
        let result = crate::execute_bash(
            &self.inner.cwd,
            command,
            Some(self.session_env()),
            on_chunk,
            signal,
        )
        .await;
        activity.release();
        let result = result?;
        self.record_bash_result(command, exclude_from_context, &result)
            .await?;
        Ok(result)
    }

    pub async fn record_bash_result(
        &self,
        command: &str,
        exclude_from_context: bool,
        result: &crate::BashResult,
    ) -> Result<()> {
        if let Some(path) = result.full_output_path.as_deref() {
            self.track_bash_spill_path(path);
        }
        let bash_message = BashExecutionMessage {
            command: command.to_owned(),
            output: result.output.clone(),
            exit_code: result.exit_code,
            cancelled: result.cancelled,
            truncated: result.truncated,
            full_output_path: result.full_output_path.clone(),
            timestamp: pi_ai::now_millis(),
            exclude_from_context: exclude_from_context.then_some(true),
        };
        let message = Message::BashExecution(bash_message.clone());
        self.append_bash_message(message).await?;
        self.publish_session_event(SessionEvent::BashExecutionEnd { message: bash_message });
        Ok(())
    }

    /// Records a detached bash spill path so [`Self::cleanup_bash_spills`] can
    /// remove it later. Empty paths are ignored. Paths remain readable until cleanup.
    pub fn track_bash_spill_path(&self, path: &str) {
        track_bash_spill_path(&self.inner.shared, path);
    }

    /// Removes every bash spill file tracked by this session. Idempotent.
    /// Safe to call repeatedly; does not touch untracked/unrelated files and
    /// never drains the process-wide spill registry.
    pub fn cleanup_bash_spills(&self) {
        cleanup_bash_spills(&self.inner.shared);
    }

    pub fn abort_bash(&self) {
        if let Some(active) = self.inner.bash_controller.lock().as_ref() {
            active.controller.abort();
        }
    }

    #[must_use]
    pub fn is_bash_running(&self) -> bool {
        self.inner.bash_controller.lock().is_some()
    }

    pub fn session_stats(&self) -> SessionStats {
        let messages = self.current_recorder()
            .and_then(|recorder| recorder.tree())
            .map_or_else(
                |_| self.history(),
                |tree| tree.entries.into_iter().filter_map(|entry| entry.message).collect(),
            );
        let mut user_messages = 0;
        let mut assistant_messages = 0;
        let mut tool_calls = 0;
        let mut tool_results = 0;
        let mut total = Usage::default();
        for message in &messages {
            match message {
                Message::User(_) => user_messages += 1,
                Message::Assistant(message) => {
                    assistant_messages += 1;
                    tool_calls += message.content.iter().filter(|block| matches!(block, ContentBlock::ToolCall(_))).count();
                    add_usage(&mut total, &message.usage);
                }
                Message::ToolResult(_) => tool_results += 1,
                Message::BashExecution(_) | Message::Custom(_) | Message::BranchSummary(_) | Message::CompactionSummary(_) => {}
            }
        }
        let (session_id, session_file) = self.recorder_info().map_or((None, None), |(id, path)| {
            (Some(id), Some(path.to_string_lossy().into_owned()))
        });
        let context_usage = self.model().and_then(|model| {
            (model.context_window > 0).then(|| {
                let tokens = estimate_context_tokens_usage_aware(&self.history());
                SessionContextUsage {
                    tokens: Some(tokens),
                    context_window: model.context_window,
                    percent: Some(tokens as f64 / model.context_window as f64 * 100.0),
                }
            })
        });
        SessionStats {
            session_file,
            session_id,
            user_messages,
            assistant_messages,
            tool_calls,
            tool_results,
            total_messages: messages.len(),
            tokens: SessionTokenStats {
                input: total.input,
                output: total.output,
                cache_read: total.cache_read,
                cache_write: total.cache_write,
                total: total.input + total.output + total.cache_read + total.cache_write,
            },
            cost: total.cost.total,
            context_usage,
        }
    }

    fn session_env(&self) -> crate::SessionEnvFn {
        let shared = self.inner.shared.clone();
        Arc::new(move || {
            let state = shared.state.read();
            let mut env = HashMap::from([
                ("PI_PROVIDER".to_owned(), state.model.provider.clone()),
                ("PI_MODEL".to_owned(), state.model.id.clone()),
                ("PI_REASONING_LEVEL".to_owned(), thinking_level_name(state.thinking_level).to_owned()),
            ]);
            drop(state);
            if let Some(recorder) = shared.recorder.lock().as_ref() {
                env.insert("PI_SESSION_ID".to_owned(), recorder.id());
                env.insert("PI_SESSION_FILE".to_owned(), recorder.path().to_string_lossy().into_owned());
            }
            env
        })
    }

    pub async fn wait_for_idle(&self) {
        loop {
            let notified = self.inner.idle.notified();
            if !self.inner.run_slot.lock().active {
                return;
            }
            notified.await;
        }
    }

    pub async fn abort(&self) {
        self.abort_retry();
        self.abort_compaction();
        self.abort_bash();
        let active = {
            let mut slot = self.inner.run_slot.lock();
            let active = slot.active;
            if active {
                slot.abort_requested = true;
            }
            active
        };
        if active {
            self.inner.abort_notify.notify_waiters();
            self.inner.agent.abort().await;
        }
    }

    pub async fn run_print<W: Write>(&self, writer: &mut W, prompt: &str) -> Result<String> {
        self.run_print_with_images(writer, prompt, Vec::new()).await
    }

    pub async fn run_print_with_images<W: Write>(
        &self,
        writer: &mut W,
        prompt: &str,
        images: Vec<ContentBlock>,
    ) -> Result<String> {
        if images
            .iter()
            .any(|block| !matches!(block, ContentBlock::Image { .. }))
        {
            return Err(anyhow!("images must contain only image content blocks"));
        }
        if prompt.trim().is_empty() && images.is_empty() {
            return Err(anyhow!("prompt must not be empty"));
        }
        let mut messages = vec![Message::User(UserMessage {
            content: Vec::new(),
            timestamp: pi_ai::now_millis(),
        })];
        let claim = self.begin_run().await?;
        let mut content = Vec::with_capacity(images.len() + usize::from(!prompt.is_empty()));
        if !prompt.is_empty() {
            content.push(ContentBlock::text(prompt));
        }
        content.extend(images);
        messages[0] = Message::User(UserMessage {
            content,
            timestamp: pi_ai::now_millis(),
        });
        let messages = self.inject_selection_messages(messages).await;
        let (event_sender, mut event_receiver) = mpsc::unbounded_channel::<AgentEvent>();
        let subscription = self
            .subscribe(move |event| {
                let event_sender = event_sender.clone();
                async move {
                    let _ = event_sender.send(event);
                    Ok(())
                }
            })
            .await;
        let session = self.clone();
        let mut operation = Box::pin(async move { session.execute_with_retries(Some(messages)).await });
        let mut saw_text = false;
        let mut wrote_output = false;
        let mut ends_with_newline = true;
        let mut writer_error = None;
        let operation_result = loop {
            tokio::select! {
                result = &mut operation => break result,
                event = event_receiver.recv(), if writer_error.is_none() => {
                    if let Some(event) = event {
                        if let Err(error) = render_print_event(
                            writer,
                            event,
                            &mut saw_text,
                            &mut wrote_output,
                            &mut ends_with_newline,
                        ) {
                            writer_error = Some(error);
                            self.inner.agent.abort().await;
                        }
                    }
                }
            }
        };
        if writer_error.is_none() {
            while let Ok(event) = event_receiver.try_recv() {
                if let Err(error) = render_print_event(
                    writer,
                    event,
                    &mut saw_text,
                    &mut wrote_output,
                    &mut ends_with_newline,
                ) {
                    writer_error = Some(error);
                    break;
                }
            }
        }
        drop(subscription);
        let run_result = self.finish_run(claim, operation_result).await;
        if let Some(error) = writer_error {
            return Err(error);
        }
        let result = run_result?;
        if !saw_text && !result.text.is_empty() {
            writer.write_all(result.text.as_bytes())?;
        }
        writer.flush()?;
        Ok(result.text)
    }

    pub async fn run(&self, prompt: &str, images: Vec<ContentBlock>) -> Result<RunResult> {
        if images
            .iter()
            .any(|block| !matches!(block, ContentBlock::Image { .. }))
        {
            return Err(anyhow!("images must contain only image content blocks"));
        }
        if prompt.trim().is_empty() && images.is_empty() {
            return Err(anyhow!("prompt must not be empty"));
        }
        let mut content = Vec::with_capacity(images.len() + usize::from(!prompt.is_empty()));
        if !prompt.is_empty() {
            content.push(ContentBlock::text(prompt));
        }
        content.extend(images);
        self.run_messages(vec![Message::User(UserMessage {
            content,
            timestamp: pi_ai::now_millis(),
        })])
        .await
    }

    pub async fn run_messages(&self, messages: Vec<Message>) -> Result<RunResult> {
        if messages.is_empty() {
            return Err(anyhow!("messages must not be empty"));
        }
        let messages = self.inject_selection_messages(messages).await;
        let claim = self.begin_run().await?;
        let operation = self.execute_with_retries(Some(messages)).await;
        self.finish_run(claim, operation).await
    }

    pub async fn continue_run(&self) -> Result<RunResult> {
        let claim = self.begin_run().await?;
        let operation = self.execute_with_retries(None).await;
        self.finish_run(claim, operation).await
    }

    fn is_expanded_skill_command(request: &str, name: &str, body: &str) -> bool {
        let uri = format!("skill://{name}");
        let wrapper = format!(
            "<skill name=\"{name}\" location=\"{uri}\">\nReferences are relative to {uri}/.\n\n{}\n</skill>",
            body.trim()
        );
        request == wrapper || request.strip_prefix(&wrapper).is_some_and(|rest| rest.starts_with("\n\n"))
    }

    async fn inject_selection_messages(&self, mut messages: Vec<Message>) -> Vec<Message> {
        let request = messages.iter().rev().find_map(|message| match message {
            Message::User(user) => Some(content_text(&user.content)),
            _ => None,
        });
        let Some(request) = request.filter(|request| !request.trim().is_empty()) else {
            return messages;
        };
        let plan = if let Some(plan) = self.take_prepared_selection(&request) {
            *self.inner.shared.last_selection.write() = Some(plan.clone());
            plan
        } else {
            self.select_for_request(&request).await
        };
        let prompt = crate::render_selection_prompt(&plan);
        let autoload = crate::load_autoload_skill_bodies(
            &plan,
            &self.inner.shared.selector_skills.read(),
        )
        .into_iter()
        .filter(|(name, body)| !Self::is_expanded_skill_command(&request, name, body))
        .collect::<Vec<_>>();
        if prompt.is_empty() && autoload.is_empty() {
            return messages;
        }
        let mut content = prompt;
        for (name, body) in autoload {
            content.push_str(&format!(
                "\n\n<autoloaded_skill name=\"{}\">\n{}\n</autoloaded_skill>",
                name, body
            ));
        }
        messages.insert(
            0,
            Message::Custom(pi_ai::CustomMessage {
                custom_type: "selection_recommendations".to_owned(),
                content: content.into(),
                display: false,
                details: serde_json::to_value(&plan).ok(),
                timestamp: pi_ai::now_millis(),
            }),
        );
        messages
    }

    fn prepare_initial_messages(&self, mut messages: Vec<Message>) -> Vec<Message> {
        if messages.iter().any(|message| matches!(message, Message::User(_)))
            && self.inner.todo.take_reminder()
        {
            let state = self.inner.todo.state();
            messages.insert(
                0,
                Message::Custom(CustomMessage {
                    custom_type: "todo-error-reminder".to_owned(),
                    content: format!(
                        "A previous todo operation failed. Reconcile the canonical todo DAG before continuing. Prefer any ready task; phase order is presentation only, and blockedBy explains what remains unsatisfied:\n{}",
                        serde_json::to_string(&state).unwrap_or_default()
                    )
                    .into(),
                    display: false,
                    details: serde_json::to_value(&state).ok(),
                    timestamp: pi_ai::now_millis(),
                }),
            );
        }
        messages.extend(self.inner.shared.pending_next_turn.lock().drain(..));
        messages
    }

    async fn execute_with_retries(&self, initial: Option<Vec<Message>>) -> Result<()> {
        self.inner.shared.overflow_recovery_attempted.store(false, Ordering::Release);
        self.inner.shared.fallback_attempt_errors.lock().clear();
        let mut operation = match initial {
            Some(messages) => {
                self.settle_operation(
                    self.inner
                        .agent
                        .prompt_messages(self.prepare_initial_messages(messages)),
                )
                .await
            }
            None => self.settle_operation(self.inner.agent.continue_run()).await,
        };
        let mut attempt = 0usize;
        loop {
            let state = self.inner.agent.state().await;
            let Some(Message::Assistant(failure)) = state.messages.last().cloned() else {
                self.finish_retry_success(attempt);
                return operation;
            };
            // Successful assistant turns end the retry/fallback saga. Only Error
            // (and overflow) stops continue into recovery; otherwise primary or
            // fallback success must emit lifecycle completion and return.
            if failure.stop_reason != StopReason::Error {
                self.finish_retry_success(attempt);
                return operation;
            }
            if is_context_overflow(&failure, state.model.context_window) {
                if self
                    .inner
                    .shared
                    .overflow_recovery_attempted
                    .swap(true, Ordering::AcqRel)
                {
                    self.inner.shared.retry_attempt.store(0, Ordering::Release);
                    return Err(anyhow!(
                        "Context overflow persisted after automatic compaction. Reduce the prompt or start a new session."
                    ));
                }
                let mut live_messages = state.messages;
                live_messages.pop();
                self.inner.agent.set_messages(live_messages).await;
                self.perform_compaction(CompactionReason::Overflow, true, None)
                    .await
                    .map_err(|error| anyhow!("Context overflow recovery failed: {error}"))?;
                operation = self.settle_operation(self.inner.agent.continue_run()).await;
                continue;
            }

            let settings = self.retry_settings();
            let retryable = is_retryable_assistant_error(&failure);
            let has_tool_call = failure
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolCall(_)));
            let lower_error = failure
                .error_message
                .as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase();
            let is_abort = failure.stop_reason == StopReason::Aborted
                || lower_error.contains("aborted")
                || lower_error.contains("cancelled");
            let error_message = failure
                .error_message
                .clone()
                .unwrap_or_else(|| "transient provider error".to_owned());
            self.inner
                .shared
                .fallback_attempt_errors
                .lock()
                .push(error_message.clone());

            let current_selector = format_retry_fallback_selector(
                &state.model,
                Some(thinking_level_name(state.thinking_level)),
            );
            let may_fallback = settings.enabled
                && settings.model_fallback
                && !has_tool_call
                && !is_abort
                && (retryable
                    || is_hard_error_fallback_eligible(
                        failure.stop_reason == StopReason::Error,
                        retryable,
                        has_tool_call,
                        false,
                        is_abort,
                        settings.model_fallback,
                        true,
                    ));
            let switched_model = if may_fallback {
                self.try_apply_retry_model_fallback(&current_selector, &state.model)
                    .await
            } else {
                false
            };

            if switched_model {
                attempt = 0;
                self.inner.shared.retry_attempt.store(0, Ordering::Release);
                let mut live_messages = state.messages;
                let _failed = live_messages.pop();
                let (next_model, next_thinking) = {
                    let shared = self.inner.shared.state.read();
                    (shared.model.clone(), shared.thinking_level)
                };
                self.inner.agent.set_model(next_model).await;
                self.inner.agent.set_thinking_level(next_thinking).await;
                self.inner.agent.set_messages(live_messages).await;
                self.inner.agent.clear_error_message().await;
                operation = self.settle_operation(self.inner.agent.continue_run()).await;
                continue;
            }

            if !retryable {
                self.finish_retry_failure(attempt, &error_message);
                return operation;
            }
            if !settings.enabled || attempt >= settings.max_retries {
                self.finish_retry_failure(attempt, &error_message);
                return operation;
            }
            attempt += 1;
            self.inner.shared.retry_attempt.store(attempt, Ordering::Release);
            let shift = u32::try_from(attempt.saturating_sub(1))
                .unwrap_or(u32::MAX)
                .min(63);
            let delay_ms = settings.base_delay_ms.saturating_mul(1u64 << shift);
            let (controller, abort) = AbortController::new();
            *self.inner.shared.retry_controller.lock() = Some(controller);
            self.publish_session_event(SessionEvent::AutoRetryStart {
                attempt,
                max_attempts: settings.max_retries,
                delay_ms,
                error_message: crate::redact_retry_diagnostic(&error_message),
            });
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
                () = abort.cancelled() => {
                    self.inner.shared.retry_controller.lock().take();
                    self.publish_session_event(SessionEvent::AutoRetryEnd {
                        success: false,
                        attempt,
                        final_error: Some("Retry cancelled".to_owned()),
                    });
                    self.inner.shared.retry_attempt.store(0, Ordering::Release);
                    return Err(anyhow!("Retry cancelled"));
                }
            }
            if !self.retry_settings().enabled || abort.is_aborted() {
                self.inner.shared.retry_controller.lock().take();
                self.publish_session_event(SessionEvent::AutoRetryEnd {
                    success: false,
                    attempt,
                    final_error: Some("Retry cancelled".to_owned()),
                });
                self.inner.shared.retry_attempt.store(0, Ordering::Release);
                return Err(anyhow!("Retry cancelled"));
            }
            let mut live_messages = state.messages;
            live_messages.pop();
            self.inner.agent.set_messages(live_messages).await;
            let mut retry = Box::pin(self.settle_operation(self.inner.agent.continue_run()));
            operation = tokio::select! {
                result = &mut retry => result,
                () = abort.cancelled() => {
                    self.inner.agent.abort().await;
                    self.inner.shared.retry_controller.lock().take();
                    self.publish_session_event(SessionEvent::AutoRetryEnd {
                        success: false,
                        attempt,
                        final_error: Some("Retry cancelled".to_owned()),
                    });
                    self.inner.shared.retry_attempt.store(0, Ordering::Release);
                    return Err(anyhow!("Retry cancelled"));
                }
            };
            self.inner.shared.retry_controller.lock().take();
            let retry_state = self.inner.agent.state().await;
            let retry_failed = retry_state.messages.last().and_then(|message| match message {
                Message::Assistant(assistant) => Some(assistant),
                _ => None,
            });
            if retry_failed.is_none_or(|assistant| assistant.stop_reason != StopReason::Error) {
                self.finish_retry_success(attempt);
                return operation;
            }
        }
    }

    fn finish_retry_success(&self, attempt: usize) {
        if attempt > 0 {
            self.publish_session_event(SessionEvent::AutoRetryEnd {
                success: true,
                attempt,
                final_error: None,
            });
        }
        if let Some(active) = self.inner.shared.active_retry_fallback.lock().clone() {
            if let Some(model) = self.model() {
                self.publish_session_event(SessionEvent::RetryFallbackSucceeded {
                    model: format_retry_fallback_selector(
                        &model,
                        Some(thinking_level_name(self.thinking_level())),
                    ),
                    role: active.role,
                });
            }
        }
        self.inner.shared.retry_attempt.store(0, Ordering::Release);
        self.inner.shared.fallback_attempt_errors.lock().clear();
    }

    fn finish_retry_failure(&self, attempt: usize, latest_error: &str) {
        let errors = self.inner.shared.fallback_attempt_errors.lock().clone();
        let final_error = if errors.is_empty() {
            crate::redact_retry_diagnostic(latest_error)
        } else {
            aggregate_retry_diagnostics(&errors)
        };
        if attempt > 0 || errors.len() > 1 {
            self.publish_session_event(SessionEvent::AutoRetryEnd {
                success: false,
                attempt,
                final_error: Some(final_error),
            });
        }
        self.inner.shared.retry_attempt.store(0, Ordering::Release);
    }

    async fn try_apply_retry_model_fallback(
        &self,
        current_selector: &str,
        current_model: &Model,
    ) -> bool {
        let settings = self.retry_settings();
        if !settings.model_fallback || settings.fallback_chains.is_empty() {
            return false;
        }
        let roles = BTreeMap::new();
        let lookup = CatalogModelLookup;
        let context = RetryFallbackResolutionContext {
            chains: &settings.fallback_chains,
            model_roles: &roles,
            model_lookup: &lookup,
        };
        let active_role = self
            .inner
            .shared
            .active_retry_fallback
            .lock()
            .as_ref()
            .map(|state| state.role.clone());
        let role = active_role.or_else(|| {
            resolve_retry_fallback_chain_key(&context, current_selector, Some(current_model), None)
        });
        let Some(role) = role else {
            return false;
        };
        let candidates = find_retry_fallback_candidates(
            &context,
            &role,
            current_selector,
            Some(current_model),
            true,
        );
        for candidate in candidates {
            let Some(model) = lookup.find(&candidate.provider, &candidate.id) else {
                continue;
            };
            let api_key = if let Some(resolver) = &self.inner.shared.auth_resolver {
                match resolver(model.clone()).await {
                    Ok(auth) => auth.api_key,
                    Err(_) => continue,
                }
            } else {
                let current = self.inner.shared.state.read();
                if current.model.provider != model.provider {
                    continue;
                }
                current.api_key.clone()
            };
            if api_key.trim().is_empty() {
                continue;
            }
            {
                let mut active = self.inner.shared.active_retry_fallback.lock();
                if active.is_none() {
                    *active = Some(ActiveRetryFallbackState { role: role.clone() });
                }
            }
            self.set_model_internal(model, api_key);
            if let Some(level) = candidate
                .thinking_level
                .as_deref()
                .and_then(parse_thinking_level_name)
            {
                let _ = self.set_thinking_level(level);
            }
            let applied_model = self.model().map_or(candidate.raw, |model| {
                format_retry_fallback_selector(
                    &model,
                    Some(thinking_level_name(self.thinking_level())),
                )
            });
            self.publish_session_event(SessionEvent::RetryFallbackApplied {
                from: current_selector.to_owned(),
                to: applied_model,
                role,
            });
            return true;
        }
        false
    }


    async fn settle_operation<F>(&self, operation: F) -> Result<()>
    where
        F: Future<Output = Result<()>>,
    {
        let mut operation = Box::pin(operation);
        loop {
            let notified = self.inner.abort_notify.notified();
            if self.inner.run_slot.lock().abort_requested {
                tokio::select! {
                    biased;
                    result = &mut operation => return result,
                    () = tokio::task::yield_now() => {}
                }
                self.inner.agent.abort().await;
            }
            tokio::select! {
                result = &mut operation => return result,
                () = notified => {}
            }
        }
    }

    pub async fn subscribe<F, Fut>(&self, listener: F) -> Subscription
    where
        F: Fn(AgentEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        self.inner.agent.subscribe_simple(listener).await
    }

    pub async fn subscribe_with_signal<F, Fut>(&self, listener: F) -> Subscription
    where
        F: Fn(AgentEvent, AbortSignal) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        self.inner.agent.subscribe(listener).await
    }

    pub async fn steer(&self, message: Message) {
        self.inner.agent.steer(message).await;
        self.publish_queue_update().await;
    }

    pub async fn follow_up(&self, message: Message) {
        self.inner.agent.follow_up(message).await;
        self.publish_queue_update().await;
    }

    pub fn enable_compaction(&self, settings: CompactionSettings) {
        *self.inner.shared.compaction.write() = Some(settings);
    }

    pub fn set_auto_compaction_enabled(&self, enabled: bool) {
        let mut compaction = self.inner.shared.compaction.write();
        match compaction.as_mut() {
            Some(settings) => settings.enabled = enabled,
            None if enabled => *compaction = Some(crate::DEFAULT_COMPACTION_SETTINGS),
            None => {}
        }
    }

    async fn begin_run(&self) -> Result<ClaimedRun> {
        let guard = self.claim_exclusive()?;
        let _ = &guard;
        let (before_count, model, thinking_level, messages) = {
            let state = self.inner.shared.state.read();
            (
                state.messages.len(),
                state.model.clone(),
                state.thinking_level,
                state.messages.clone(),
            )
        };
        self.inner
            .shared
            .recorded_count
            .store(before_count, Ordering::Release);
        self.inner.agent.set_model(model).await;
        self.inner.agent.set_thinking_level(thinking_level).await;
        self.inner.agent.set_messages(messages).await;
        let shared = self.inner.shared.clone();
        let recorder_subscription = self
            .inner
            .agent
            .subscribe_simple(move |event| {
                let shared = shared.clone();
                async move {
                    if let AgentEvent::MessageEnd { message } = event {
                        if let Some(recorder) = shared.recorder.lock().as_ref().cloned() {
                            let id = match &message {
                                Message::Custom(custom) => recorder.record_custom_message(custom),
                                _ => recorder.record_message(&message),
                            }
                            .map_err(|error| anyhow!("recording session message: {error}"))?;
                            if let Some(entry) = recorder.tree()?.entries.into_iter().find(|entry| entry.id == id) {
                                let _ = shared.events.send(SessionEvent::EntryAppended { entry });
                            }
                        }
                        shared.recorded_count.fetch_add(1, Ordering::AcqRel);
                    }
                    Ok(())
                }
            })
            .await;
        Ok(ClaimedRun {
            inner: self.inner.clone(),
            guard: Some(guard),
            before_count,
            recorder_subscription: Some(recorder_subscription),
        })
    }

    async fn finish_run(&self, mut claim: ClaimedRun, operation: Result<()>) -> Result<RunResult> {
        let final_state = self.inner.agent.state().await;
        let new_start = claim.before_count.min(final_state.messages.len());
        let messages = final_state.messages[new_start..].to_vec();
        let threshold_compaction = operation.is_ok() && {
            let settings = self.inner.shared.compaction.read();
            settings.is_some_and(|settings| {
                should_compact(
                    estimate_context_tokens_usage_aware(&final_state.messages),
                    final_state.model.context_window,
                    &settings,
                )
            })
        };
        self.inner.shared.state.write().messages = final_state.messages;
        let finalized = Ok(build_run_result(messages, final_state.error_message));
        drop(claim.recorder_subscription.take());
        if threshold_compaction {
            let _ = self.perform_compaction(CompactionReason::Threshold, false, None).await;
        }
        if let Some(mut guard) = claim.guard.take() {
            guard.release();
        }
        let flush_result = self.flush_pending_bash_messages().await;
        self.publish_queue_update().await;
        match (operation, flush_result) {
            (Err(error), _) => {
                let _ = finalized;
                Err(error)
            }
            (Ok(()), Err(error)) => Err(error),
            (Ok(()), Ok(())) => finalized,
        }
    }


}

impl Drop for SessionInner {
    fn drop(&mut self) {
        // Last Session clone dropped: release any remaining detached spills.
        cleanup_bash_spills(&self.shared);
    }
}

fn track_bash_spill_path(shared: &SessionRuntime, path: &str) {
    if path.is_empty() {
        return;
    }
    shared.bash_spill_paths.lock().insert(path.to_owned());
}

fn track_bash_spill_from_details(shared: &SessionRuntime, details: &serde_json::Value) {
    if let Some(path) = details
        .get("fullOutputPath")
        .and_then(serde_json::Value::as_str)
    {
        track_bash_spill_path(shared, path);
    }
}

fn cleanup_bash_spills(shared: &SessionRuntime) {
    let paths: Vec<String> = {
        let mut set = shared.bash_spill_paths.lock();
        set.drain().collect()
    };
    for path in paths {
        crate::cleanup_full_output_path(&path);
    }
}

fn compose_bash_spill_after_tool_call(
    shared: Arc<SessionRuntime>,
    hook: Option<AfterToolCallFn>,
) -> AfterToolCallFn {
    Arc::new(move |context| {
        let hook = hook.clone();
        let shared = shared.clone();
        Box::pin(async move {
            if context.tool_call.name == "bash" && !context.is_error {
                track_bash_spill_from_details(&shared, &context.result.details);
            }
            match hook {
                Some(hook) => hook(context).await,
                None => Ok(AfterToolCallResult::default()),
            }
        })
    })
}

fn todo_after_tool_call(callback: Option<AfterToolCallFn>) -> AfterToolCallFn {
    Arc::new(move |context| {
        let callback = callback.clone();
        Box::pin(async move {
            let internal_error = context.tool_call.name == "todo"
                && context
                    .result
                    .details
                    .get(crate::todo::TODO_ERROR_MARKER)
                    .and_then(serde_json::Value::as_bool)
                    == Some(true);
            let original_details = context.result.details.clone();
            let mut update = match callback {
                Some(callback) => callback(context).await?,
                None => AfterToolCallResult::default(),
            };
            let mut effective_details = update.details.clone().unwrap_or(original_details);
            let marker_survives = internal_error
                && effective_details
                    .get(crate::todo::TODO_ERROR_MARKER)
                    .and_then(serde_json::Value::as_bool)
                    == Some(true);
            if effective_details
                .as_object_mut()
                .is_some_and(|details| details.remove(crate::todo::TODO_ERROR_MARKER).is_some())
            {
                update.details = Some(effective_details);
            }
            if marker_survives {
                update.is_error = Some(true);
            }
            Ok(update)
        })
    })
}

fn add_usage(total: &mut Usage, usage: &Usage) {
    total.input += usage.input;
    total.output += usage.output;
    total.cache_read += usage.cache_read;
    total.cache_write += usage.cache_write;
    total.cache_write_1h += usage.cache_write_1h;
    total.reasoning += usage.reasoning;
    total.total_tokens += usage.total_tokens;
    total.cost.input += usage.cost.input;
    total.cost.output += usage.cost.output;
    total.cost.cache_read += usage.cost.cache_read;
    total.cost.cache_write += usage.cost.cache_write;
    total.cost.total += usage.cost.total;
}

fn select_tools(tools: Vec<AgentTool>, selection: &ToolSelection) -> Result<Vec<AgentTool>> {
    let available = tools.iter().map(|tool| tool.name.as_str()).collect::<BTreeSet<_>>();
    if let Some(unknown) = selection.allow.iter().flatten().chain(&selection.deny)
        .find(|name| !available.contains(name.as_str()))
    {
        return Err(anyhow!(
            "unknown requested tool {unknown:?}; available tools: {}",
            available.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    let allow = selection.allow.as_ref()
        .map(|names| names.iter().map(String::as_str).collect::<BTreeSet<_>>());
    let deny = selection.deny.iter().map(String::as_str).collect::<BTreeSet<_>>();
    Ok(tools.into_iter().filter(|tool| {
        if selection.disable_all { return false; }
        if selection.disable_builtins && crate::TOOL_NAMES.contains(&tool.name.as_str()) {
            return false;
        }
        allow.as_ref().is_none_or(|allow| allow.contains(tool.name.as_str()))
            && !deny.contains(tool.name.as_str())
    }).collect())
}

fn merge_tools(base: &[AgentTool], additional: Vec<AgentTool>) -> Result<Vec<AgentTool>> {
    let mut names = base
        .iter()
        .map(|tool| tool.name.clone())
        .collect::<BTreeSet<_>>();
    for tool in &additional {
        if !names.insert(tool.name.clone()) {
            return Err(anyhow!("duplicate agent tool {:?}", tool.name));
        }
    }
    let mut tools = Vec::with_capacity(base.len() + additional.len());
    tools.extend_from_slice(base);
    tools.extend(additional);
    Ok(tools)
}

struct SessionSubscribeWake;

impl Wake for SessionSubscribeWake {
    fn wake(self: Arc<Self>) {}
}

fn poll_immediate<F: Future>(future: F) -> Result<F::Output> {
    let waker = Waker::from(Arc::new(SessionSubscribeWake));
    let mut context = TaskContext::from_waker(&waker);
    let mut future = Box::pin(future);
    match future.as_mut().poll(&mut context) {
        Poll::Ready(output) => Ok(output),
        Poll::Pending => Err(anyhow!("agent subscription registration unexpectedly yielded")),
    }
}

fn build_run_result(messages: Vec<Message>, state_error: Option<String>) -> RunResult {
    let mut usage = Usage::default();
    let mut tool_calls = Vec::new();
    let mut text = String::new();
    let mut stop_reason = StopReason::Stop;
    let mut error_message = state_error;
    for message in &messages {
        let Message::Assistant(assistant) = message else {
            continue;
        };
        add_usage(&mut usage, &assistant.usage);
        for block in &assistant.content {
            if let ContentBlock::ToolCall(call) = block {
                tool_calls.push(call.clone());
            }
        }
        text = assistant.text();
        stop_reason = assistant.stop_reason;
        if assistant.error_message.is_some() {
            error_message.clone_from(&assistant.error_message);
        }
    }
    RunResult {
        text,
        messages,
        tool_calls,
        usage,
        stop_reason,
        error_message,
    }
}


fn merge_headers_case_insensitive(
    headers: &mut HashMap<String, String>,
    source: HashMap<String, String>,
) {
    for (name, value) in source {
        if let Some(existing) = headers
            .keys()
            .find(|existing| existing.eq_ignore_ascii_case(&name))
            .cloned()
        {
            headers.remove(&existing);
        }
        headers.insert(name, value);
    }
}

async fn auth_error_stream(model: &Model, message: String) -> pi_ai::AssistantMessageEventStream {
    let stream = pi_ai::new_assistant_message_event_stream();
    let mut error = pi_ai::AssistantMessage::pending(model);
    error.stop_reason = StopReason::Error;
    error.error_message = Some(message);
    stream
        .push(AssistantMessageEvent::Error {
            reason: StopReason::Error,
            error: error.clone(),
        })
        .await;
    stream.end(Some(error)).await;
    stream
}

fn render_print_event<W: Write>(
    writer: &mut W,
    event: AgentEvent,
    saw_text: &mut bool,
    wrote_output: &mut bool,
    ends_with_newline: &mut bool,
) -> Result<()> {
    match event {
        AgentEvent::MessageUpdate {
            assistant_message_event: AssistantMessageEvent::TextDelta { delta, .. },
            ..
        } => {
            writer.write_all(delta.as_bytes())?;
            *saw_text = true;
            *wrote_output = true;
            *ends_with_newline = delta.ends_with('\n');
        }
        AgentEvent::ToolExecutionStart {
            tool_name,
            arguments,
            ..
        } => {
            let arguments = compact_tool_arguments(&arguments);
            writeln!(writer, "\n\x1b[2m\u{b7} {tool_name}({arguments})\x1b[0m")?;
            *wrote_output = true;
            *ends_with_newline = true;
        }
        AgentEvent::ToolExecutionEnd { is_error, .. } => {
            writeln!(
                writer,
                "\x1b[2m  \u{2514} {}\x1b[0m",
                if is_error { "error" } else { "ok" }
            )?;
            *wrote_output = true;
            *ends_with_newline = true;
        }
        _ => {}
    }
    Ok(())
}

fn compact_tool_arguments(arguments: &serde_json::Value) -> String {
    let value = ["command", "path", "pattern"]
        .into_iter()
        .find_map(|key| arguments.get(key).and_then(serde_json::Value::as_str))
        .unwrap_or_default();
    if value.len() <= 60 {
        return value.to_owned();
    }
    let boundary = value
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= 57)
        .last()
        .unwrap_or(0);
    format!("{}...", &value[..boundary])
}


async fn summarize_messages(
    inner: &SessionRuntime,
    messages: &[Message],
    previous_summary: &str,
    custom_instructions: Option<&str>,
    reserve_tokens: i64,
    fraction: f64,
    abort: AbortSignal,
    reason: Option<CompactionReason>,
) -> Result<String> {
    let mut prompt = format!(
        "<conversation>\n{}\n</conversation>\n\n",
        serialize_conversation(&messages_as_llm(messages))
    );
    if previous_summary.is_empty() {
        prompt.push_str(SUMMARIZATION_PROMPT);
    } else {
        prompt.push_str(&format!(
            "<previous-summary>\n{previous_summary}\n</previous-summary>\n\n{UPDATE_SUMMARIZATION_PROMPT}"
        ));
    }
    if let Some(instructions) = custom_instructions.filter(|instructions| !instructions.trim().is_empty()) {
        prompt.push_str("\n\nAdditional focus: ");
        prompt.push_str(instructions);
    }
    complete_summary(inner, prompt, reserve_tokens, fraction, abort, reason).await
}

async fn summarize_turn_prefix(
    inner: &SessionRuntime,
    messages: &[Message],
    reserve_tokens: i64,
    abort: AbortSignal,
    reason: CompactionReason,
) -> Result<String> {
    let prompt = format!(
        "<conversation>\n{}\n</conversation>\n\n{TURN_PREFIX_SUMMARIZATION_PROMPT}",
        serialize_conversation(&messages_as_llm(messages))
    );
    complete_summary(inner, prompt, reserve_tokens, 0.5, abort, Some(reason)).await
}

async fn complete_summary(
    inner: &SessionRuntime,
    prompt: String,
    reserve_tokens: i64,
    fraction: f64,
    abort: AbortSignal,
    reason: Option<CompactionReason>,
) -> Result<String> {
    let settings = inner.retry_settings.read().clone();
    let mut attempt = 0usize;
    loop {
        if attempt > 0 {
            let _ = inner.events.send(SessionEvent::SummarizationRetryAttemptStart {
                source: if reason.is_some() { SummarizationSource::Compaction } else { SummarizationSource::BranchSummary },
                reason,
            });
        }
        let (model, thinking_level, fallback_api_key) = {
            let state = inner.state.read();
            (state.model.clone(), state.thinking_level, state.api_key.clone())
        };
        let reasoning = if model.reasoning {
            match thinking_level {
                ThinkingLevel::Off => None,
                ThinkingLevel::Minimal => Some(pi_ai::ThinkingLevel::Minimal),
                ThinkingLevel::Low => Some(pi_ai::ThinkingLevel::Low),
                ThinkingLevel::Medium => Some(pi_ai::ThinkingLevel::Medium),
                ThinkingLevel::High => Some(pi_ai::ThinkingLevel::High),
                ThinkingLevel::Xhigh => Some(pi_ai::ThinkingLevel::XHigh),
                ThinkingLevel::Max => Some(pi_ai::ThinkingLevel::Max),
            }
        } else { None };
        let requested_max = (reserve_tokens as f64 * fraction).floor() as i64;
        let max_tokens = if model.max_tokens > 0 { requested_max.min(model.max_tokens).max(1) } else { requested_max.max(1) };
        let mut stream_options = inner.stream_options.read().clone();
        if let Some(resolver) = &inner.auth_resolver {
            let auth = resolver(model.clone()).await?;
            stream_options.stream.api_key = Some(auth.api_key);
            merge_headers_case_insensitive(&mut stream_options.stream.headers, auth.headers);
            stream_options.stream.env.extend(auth.env);
        } else {
            stream_options.stream.api_key = Some(fallback_api_key);
        }
        stream_options.stream.max_tokens = Some(max_tokens);
        stream_options.stream.cache_retention = CacheRetention::None;
        stream_options.stream.session_id = Some(Uuid::now_v7().to_string());
        stream_options.stream.abort_signal = Some(abort.cancellation_token());
        stream_options.reasoning = reasoning;
        let stream = (inner.stream_fn)(
            model,
            Context {
                system_prompt: SUMMARIZATION_SYSTEM_PROMPT.to_owned(),
                messages: vec![Message::user_text(prompt.clone(), pi_ai::now_millis())],
                tools: Vec::new(),
            },
            stream_options,
        ).await;
        while stream.next().await.is_some() {}
        let response = stream.result().await.ok_or_else(|| anyhow!("compaction summarization returned no message"))?;
        if abort.is_aborted() || response.stop_reason == StopReason::Aborted {
            if attempt > 0 { let _ = inner.events.send(SessionEvent::SummarizationRetryFinished); }
            return Err(anyhow!("Compaction cancelled"));
        }
        if response.stop_reason != StopReason::Error {
            if attempt > 0 { let _ = inner.events.send(SessionEvent::SummarizationRetryFinished); }
            return Ok(response.text());
        }
        let error_message = response.error_message.clone().unwrap_or_else(|| "compaction summarization failed".to_owned());
        if abort.is_aborted() || !settings.enabled || !is_retryable_assistant_error(&response) || attempt >= settings.max_retries {
            if attempt > 0 { let _ = inner.events.send(SessionEvent::SummarizationRetryFinished); }
            return Err(anyhow!(error_message));
        }
        attempt += 1;
        let shift = u32::try_from(attempt.saturating_sub(1)).unwrap_or(u32::MAX).min(63);
        let delay_ms = settings.base_delay_ms.saturating_mul(1u64 << shift);
        let _ = inner.events.send(SessionEvent::SummarizationRetryScheduled {
            attempt,
            max_attempts: settings.max_retries,
            delay_ms,
            error_message,
        });
        tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
            () = abort.cancelled() => {
                let _ = inner.events.send(SessionEvent::SummarizationRetryFinished);
                return Err(anyhow!("Compaction cancelled"));
            }
        }
    }
}

fn validate_extension_message_content(content: &CustomMessageContent) -> Result<()> {
    if let CustomMessageContent::Blocks(blocks) = content {
        for block in blocks {
            if !matches!(block, ContentBlock::Text { .. } | ContentBlock::Image { .. }) {
                return Err(anyhow!("extension messages support only text and image content"));
            }
        }
    }
    Ok(())
}

fn navigation_target(entry: &crate::SessionEntry) -> (Option<String>, Option<String>) {
    match entry.message.as_ref() {
        Some(Message::User(message)) => (entry.parent_id.clone(), Some(content_text(&message.content))),
        _ if entry.entry_type == "custom_message" => {
            let text = entry.content.as_ref().map(|content| match content {
                pi_ai::CustomMessageContent::Text(text) => text.clone(),
                pi_ai::CustomMessageContent::Blocks(content) => content_text(content),
            });
            (entry.parent_id.clone(), text)
        }
        _ => (Some(entry.id.clone()), None),
    }
}

fn collect_abandoned_messages(tree: &crate::SessionTree, current_leaf: Option<&str>, new_leaf: Option<&str>) -> Vec<Message> {
    let current = current_leaf.map_or_else(Vec::new, |leaf| tree.branch(Some(leaf)));
    let target = new_leaf.map_or_else(Vec::new, |leaf| tree.branch(Some(leaf)));
    let common = current.iter().zip(target.iter()).take_while(|(left, right)| left.id == right.id).count();
    current[common..].iter().filter_map(|entry| match entry.message.as_ref() {
        Some(message) => Some(message.clone()),
        None if entry.entry_type == "branch_summary" => entry.summary.as_ref().map(|summary| Message::user_text(summary.clone(), 0)),
        None => None,
    }).collect()
}

fn resolve_classifier_model(
    settings: &crate::SelectorSettings,
    session_model: &Model,
) -> Option<Model> {
    let Some(configured) = settings.classifier.model.as_deref() else {
        return Some(session_model.clone());
    };
    let (provider, id) = configured.split_once('/')?;
    pi_ai::get_model(provider, id)
}

fn content_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn parse_recorded_thinking_level(level: &str) -> ThinkingLevel {
    match level {
        "minimal" => ThinkingLevel::Minimal,
        "low" => ThinkingLevel::Low,
        "medium" => ThinkingLevel::Medium,
        "high" => ThinkingLevel::High,
        "xhigh" => ThinkingLevel::Xhigh,
        "max" => ThinkingLevel::Max,
        _ => ThinkingLevel::Off,
    }
}

fn clamp_thinking_level(model: &Model, requested: ThinkingLevel) -> ThinkingLevel {
    match pi_ai::clamp_thinking_level(model, thinking_level_name(requested)) {
        "minimal" => ThinkingLevel::Minimal,
        "low" => ThinkingLevel::Low,
        "medium" => ThinkingLevel::Medium,
        "high" => ThinkingLevel::High,
        "xhigh" => ThinkingLevel::Xhigh,
        "max" => ThinkingLevel::Max,
        _ => ThinkingLevel::Off,
    }
}

fn parse_thinking_level_name(name: &str) -> Option<ThinkingLevel> {
    match name.trim().to_ascii_lowercase().as_str() {
        "off" => Some(ThinkingLevel::Off),
        "minimal" => Some(ThinkingLevel::Minimal),
        "low" => Some(ThinkingLevel::Low),
        "medium" => Some(ThinkingLevel::Medium),
        "high" => Some(ThinkingLevel::High),
        "xhigh" => Some(ThinkingLevel::Xhigh),
        "max" => Some(ThinkingLevel::Max),
        _ => None,
    }
}

fn thinking_level_name(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::Xhigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}

fn thinking_level_change(
    model: &Model,
    requested: ThinkingLevel,
    effective: ThinkingLevel,
) -> ThinkingLevelChange {
    let clamped = requested != effective;
    let message = if !clamped {
        format!("Thinking level: {}", thinking_level_name(effective))
    } else if !model.reasoning {
        format!(
            "Thinking level {} unsupported by {}/{} (reasoning disabled); using {}",
            thinking_level_name(requested),
            model.provider,
            model.id,
            thinking_level_name(effective)
        )
    } else {
        format!(
            "Thinking level {} unsupported by {}/{}; using {}",
            thinking_level_name(requested),
            model.provider,
            model.id,
            thinking_level_name(effective)
        )
    };
    ThinkingLevelChange {
        requested,
        effective,
        clamped,
        message,
    }
}

#[cfg(test)]
mod thinking_level_change_tests {
    use super::*;

    fn model(reasoning: bool) -> Model {
        Model {
            id: if reasoning {
                "reasoner".to_owned()
            } else {
                "qwen".to_owned()
            },
            provider: "test".to_owned(),
            reasoning,
            ..Model::default()
        }
    }

    fn session_with(model: Model) -> Session {
        Session::new(SessionOptions {
            model,
            cwd: std::env::temp_dir(),
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
        })
        .expect("session")
    }

    #[test]
    fn unsupported_reasoning_model_clamps_high_to_off_with_actionable_status() {
        let session = session_with(model(false));
        let change = session.set_thinking_level(ThinkingLevel::High);
        assert!(change.clamped);
        assert_eq!(change.requested, ThinkingLevel::High);
        assert_eq!(change.effective, ThinkingLevel::Off);
        assert_eq!(session.thinking_level(), ThinkingLevel::Off);
        assert!(
            change.message.contains("unsupported") && change.message.contains("off"),
            "{}",
            change.message
        );
        // Must not report the requested level as success.
        assert!(!change.message.eq_ignore_ascii_case("Thinking level: high"));
    }

    #[test]
    fn supported_reasoning_model_keeps_high() {
        let session = session_with(model(true));
        let change = session.set_thinking_level(ThinkingLevel::High);
        assert!(!change.clamped);
        assert_eq!(change.effective, ThinkingLevel::High);
        assert_eq!(session.thinking_level(), ThinkingLevel::High);
        assert_eq!(change.message, "Thinking level: high");
    }

    #[test]
    fn model_switch_reclamps_existing_thinking_level() {
        let session = session_with(model(true));
        let _ = session.set_thinking_level(ThinkingLevel::High);
        let change = session.set_model(model(false), String::new());
        assert!(change.clamped);
        assert_eq!(change.effective, ThinkingLevel::Off);
        assert_eq!(session.thinking_level(), ThinkingLevel::Off);
        assert!(change.message.contains("unsupported"), "{}", change.message);
    }
}

#[cfg(test)]
mod todo_persistence_tests {
    use super::*;

    fn test_session(cwd: &Path) -> Session {
        Session::new(SessionOptions {
            model: Model::default(),
            cwd: cwd.to_path_buf(),
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
        })
        .expect("session")
    }

    #[test]
    fn attaching_resumed_session_restores_complete_todo_dag() {
        let cwd = tempfile::tempdir().expect("cwd");
        let recorder = crate::start_session_in(cwd.path(), None, Some("off"), Some(cwd.path()), Some("todo-dag-resume"), None).expect("start recorder");
        let state = TodoState {
            phases: vec![TodoPhase {
                name: "Build".to_owned(),
                tasks: vec![
                    crate::TodoItem {
                        id: "task-root".to_owned(),
                        content: "root".to_owned(),
                        status: crate::TodoStatus::Completed,
                        depends_on: Vec::new(),
                        ready: false,
                        blocked_by: Vec::new(),
                    },
                    crate::TodoItem {
                        id: "task-child".to_owned(),
                        content: "child".to_owned(),
                        status: crate::TodoStatus::InProgress,
                        depends_on: vec!["task-root".to_owned()],
                        ready: true,
                        blocked_by: Vec::new(),
                    },
                ],
            }],
            storage: TodoStorage::Session,
        };
        recorder.record_todo_snapshot(&state).expect("record todo snapshot");
        recorder.persist_now().expect("persist session");
        let path = recorder.path();
        recorder.close().expect("close recorder");

        let session = test_session(cwd.path());
        session.record(crate::resume_session(&path).expect("resume recorder")).expect("attach recorder");
        assert_eq!(session.todo_state(), state);
    }
}

#[cfg(test)]
mod bash_spill_lifecycle_tests {
    use super::*;
    use std::fs;

    fn test_session(cwd: &Path) -> Session {
        Session::new(SessionOptions {
            model: Model::default(),
            cwd: cwd.to_path_buf(),
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
        })
        .expect("session")
    }

    #[test]
    fn cleanup_bash_spills_removes_tracked_only_and_is_idempotent() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_session(cwd.path());
        let dir = crate::bash_spill_dir();
        fs::create_dir_all(&dir).expect("spill dir");
        let spill = dir.join(format!("session-tracked-{}.log", Uuid::now_v7()));
        let unrelated = dir.join(format!("session-unrelated-{}.log", Uuid::now_v7()));
        fs::write(&spill, b"tracked").expect("write spill");
        fs::write(&unrelated, b"keep").expect("write unrelated");
        let spill_s = spill.to_string_lossy().into_owned();

        session.track_bash_spill_path(&spill_s);
        assert!(spill.exists(), "spill must remain available while session is live");
        assert!(unrelated.exists());

        session.cleanup_bash_spills();
        assert!(!spill.exists(), "tracked spill must be removed on cleanup");
        assert!(unrelated.exists(), "unrelated file must not be touched");

        // Double cleanup must succeed without error or collateral damage.
        session.cleanup_bash_spills();
        assert!(!spill.exists());
        assert!(unrelated.exists());

        let _ = fs::remove_file(&unrelated);
    }

    #[tokio::test]
    async fn execute_bash_success_spill_exists_then_cleanup_removes() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_session(cwd.path());
        // >50 KiB display cap → success path detaches a full-output spill file.
        let result = session
            .execute_bash("yes x | head -c 60000", false)
            .await
            .expect("bash");
        let path = result
            .full_output_path
            .expect("successful truncated bash must publish full_output_path");
        assert!(
            std::path::Path::new(&path).exists(),
            "spill must remain readable during the active session: {path}"
        );

        session.cleanup_bash_spills();
        assert!(
            !std::path::Path::new(&path).exists(),
            "session cleanup must remove the detached success spill"
        );
        // Idempotent second cleanup.
        session.cleanup_bash_spills();
    }
}
