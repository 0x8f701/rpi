use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fmt,
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

use anyhow::{Context as _, Result, anyhow};
use parking_lot::{Mutex, RwLock};
use pi_agent::{
    AbortController, AbortSignal, AfterToolCallFn, AfterToolCallResult, Agent, AgentEvent,
    AgentOptions, AgentState, AgentTool, BeforeToolCallFn, QueueMode, ShouldStopAfterTurnFn,
    StreamFn, Subscription, ThinkingLevel,
};
use pi_ai::{
    AssistantMessageEvent, BashExecutionMessage, CacheRetention, ContentBlock, Context,
    CustomMessage, CustomMessageContent, Message, Model, SimpleStreamOptions, StopReason, ToolCall,
    Usage, UserMessage,
};
use sha2::{Digest, Sha256};
use tokio::sync::{Notify, broadcast, mpsc};
use uuid::Uuid;

use crate::system_prompt::BuildSystemPromptOptions;
use crate::{
    ActiveRetryFallbackState, AskRuntime, BashProcessContext, CatalogModelLookup,
    CompactionDetails, CompactionResult, CompactionSettings, GoalLifecycle, GoalRuntime, GoalState,
    HANDOFF_PROSE_RESERVE_TOKENS, HANDOFF_SUMMARIZE_TIMEOUT, HANDOFF_SYSTEM_PROMPT, Handoff,
    HostHooks, JobSnapshot, ProcessManager, ProcessOwnerId, RequestAuth, ResourceManager,
    RetryFallbackModelLookup, RetryFallbackResolutionContext, SUMMARIZATION_PROMPT,
    SUMMARIZATION_SYSTEM_PROMPT, MaterializedFork, SessionEntry, SessionKind, SessionRecorder,
    TURN_PREFIX_SUMMARIZATION_PROMPT, TodoApplyResult, TodoOp, TodoPhase, TodoRuntime,
    TodoSnapshotRecord, TodoState, TodoStorage, UPDATE_SUMMARIZATION_PROMPT,
    aggregate_retry_diagnostics, apply_checkpoint, build_snapcompact_summary,
    build_system_prompt, compute_file_lists, create_todo_tool, elide_useless_results, elided_note,
    estimate_context_tokens, estimate_context_tokens_usage_aware, estimate_live_context_tokens,
    estimate_message_tokens, find_cut_point,
    find_retry_fallback_candidates, find_snap_cut_point, format_file_operations,
    format_retry_fallback_selector, handoff_prose_prompt, is_context_overflow,
    is_fallback_only_assistant_error, is_retryable_assistant_error, load_context_files,
    load_skills, load_skills_trusted, messages_as_llm, process_tool,
    resolve_retry_fallback_chain_key, serialize_conversation, should_compact, tool_snippet,

};
pub const DEFAULT_THINKING_LEVEL: ThinkingLevel = ThinkingLevel::Medium;

/// Upper bound on queue-drain continuations started after a run settles.
/// Each continuation consumes at least one queued message, so the drain is
/// normally zero or one iteration; the cap only bounds a hostile stream of
/// concurrent steers racing every settlement.
const SESSION_SETTLE_QUEUE_DRAIN_CAP: usize = 64;
/// Bounded retry budget for a post-settle continuation that lost the run-slot
/// claim race (the slot is released only after the agent goes idle). Backoff
/// runs 50-200ms across the attempts.
const SESSION_SETTLE_CONTINUATION_RETRIES: usize = 3;
/// Ephemeral custom message type projected only into parent model requests.
pub const ACTIVE_GOAL_CUSTOM_TYPE: &str = "pi.goal.active";
/// Ephemeral custom message type that pins a unique raw user agent/persona
/// mention to the matching `task.agent` value for the parent model turn.
pub const AGENT_MENTION_CUSTOM_TYPE: &str = "pi.agent.mention";
/// Journal marker recorded when a steering/follow-up RPC is ACCEPTED. It
/// carries the queued message so a crash between the ack and the delivery can
/// requeue the message on resume; the consuming `MessageEnd` writes a
/// [`STEERING_DELIVERED_CUSTOM_TYPE`] record that retires it.
pub const STEERING_PENDING_CUSTOM_TYPE: &str = "pi.steering.pending";
/// Journal record that retires a [`STEERING_PENDING_CUSTOM_TYPE`] marker once
/// the message was delivered into the transcript (or requeued/migrated by a
/// transcript-replacing operation).
pub const STEERING_DELIVERED_CUSTOM_TYPE: &str = "pi.steering.delivered";

/// In-memory registration of one [`STEERING_PENDING_CUSTOM_TYPE`] journal
/// entry, used to correlate the consuming `MessageEnd` with its marker.
#[derive(Clone, Debug)]
struct SteeringJournalMarker {
    marker_id: String,
    follow_up: bool,
    message: Message,
}

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

pub type SessionAuthResolver =
    Arc<dyn Fn(Model) -> pi_agent::BoxFuture<Result<RequestAuth>> + Send + Sync>;
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
    pub auth_manager: Option<Arc<crate::AuthManager>>,
}

struct SessionState {
    model: Model,
    thinking_level: ThinkingLevel,
    api_key: String,
    messages: Vec<Message>,
    agent_event_barrier: u64,
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
    /// Unique explicit agent/persona mention active for the current parent run.
    /// Historical typed mention messages remain in the transcript but are
    /// filtered from provider context once this slot is cleared.
    active_agent_mention: RwLock<Option<String>>,
    compaction: RwLock<Option<CompactionSettings>>,
    compaction_runtime: Mutex<CompactionRuntime>,
    compaction_active: AtomicBool,
    compaction_controller: Mutex<Option<AbortController>>,
    before_compaction: RwLock<Option<BeforeCompactionFn>>,
    overflow_recovery_attempted: AtomicBool,
    session_name: RwLock<Option<String>>,
    retry_settings: RwLock<RetrySettings>,
    retry_role: RwLock<Option<String>>,
    retry_controller: Mutex<Option<AbortController>>,
    retry_attempt: AtomicUsize,
    active_retry_fallback: Mutex<Option<ActiveRetryFallbackState>>,
    fallback_attempt_errors: Mutex<Vec<String>>,
    events: broadcast::Sender<SessionEvent>,
    pending_next_turn: Mutex<Vec<Message>>,
    /// Journal markers for accepted-but-undelivered steering/follow-up
    /// messages. A marker is registered when the steering RPC is accepted and
    /// removed when the consuming `MessageEnd` flips its journal record to
    /// delivered, so a crash between ack and delivery requeues the message on
    /// resume instead of losing it.
    pending_steering_markers: Mutex<Vec<SteeringJournalMarker>>,
    stream_options: RwLock<SimpleStreamOptions>,
    recorded_count: AtomicUsize,
    stream_fn: StreamFn,
    auth_resolver: Option<SessionAuthResolver>,
    auth_manager: Option<Arc<crate::AuthManager>>,
    /// Namespace of the extension runtime bound to this session (set by the
    /// Application when it attaches the runtime). Stream dispatch resolves
    /// extension-owned provider apis strictly within this namespace. Shared
    /// with the namespace-aware stream wrapper built before construction.
    provider_namespace: Arc<RwLock<Option<String>>>,
    selector_settings: RwLock<crate::SelectorSettings>,
    selector_skills: RwLock<Vec<crate::Skill>>,
    selector_agents: RwLock<Vec<crate::AgentDefinition>>,
    branch_summary: RwLock<crate::EffectiveBranchSummarySettings>,
    /// Live `settings.visionModel` spec committed by resource attach and
    /// LIVE `/settings set visionModel`. Delegation treats this as the
    /// authority: a live `None` disables vision delegation even when the
    /// published resource snapshot still has a spec.
    vision_model: RwLock<Option<String>>,
    expose_session_environment: AtomicBool,
    /// Session-scoped offline contract (see [`Session::set_offline`]): makes
    /// network-touching session operations (e.g. gist share) fail closed
    /// without spawning child processes. Never derived from or written to
    /// the process environment (`std::env::set_var` is `unsafe` in edition
    /// 2024 and forbidden in this workspace).
    offline: AtomicBool,
    last_selection: RwLock<Option<crate::SelectionPlan>>,
    /// One-entry cache for hindsight memory injection. The key is a stable
    /// SHA-256 fingerprint of the normalized request and every effective
    /// memory setting that can affect recall or its security boundary. Secret
    /// material is retained only inside the digest, never in cache state.
    hindsight_injection_cache: RwLock<Option<HindsightInjectionCacheEntry>>,
    session_id: String,
    host_hooks: RwLock<Option<Arc<HostHooks>>>,
    extension_tool_names: RwLock<HashSet<String>>,
    session_started: AtomicBool,
    /// Detached bash success-path spill files owned by this session.
    /// Cleaned via [`Session::cleanup_bash_spills`] / Drop — never process-wide drain.
    bash_spill_paths: Mutex<HashSet<String>>,
    /// Interactive `ask` tool round trip (single-pending question slot).
    ask: AskRuntime,
    /// Serializes the queue-update snapshot-then-publish pair so two
    /// concurrent publishers (steer accept, boundary drain, settle) can never
    /// deliver an older snapshot after a fresher one (S-RACE-5): the last
    /// lock holder always re-snapshots the live queues before sending.
    queue_update: tokio::sync::Mutex<()>,
    #[cfg(test)]
    fail_next_todo_execution_persist: AtomicBool,

    /// Doom-loop recovery: consecutive identical tool failures end the turn
    /// instead of letting the model retry the same failing call forever.
    doom_loop: Mutex<DoomLoopTracker>,
}
struct HindsightInjectionCacheEntry {
    key: [u8; 32],
    body: String,
}

fn hindsight_injection_cache_key(config: &crate::MemoryConfig, request: &str) -> [u8; 32] {
    fn field(hasher: &mut Sha256, value: &[u8]) {
        hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_le_bytes());
        hasher.update(value);
    }
    fn optional_field(hasher: &mut Sha256, value: Option<&str>) {
        match value {
            Some(value) => {
                hasher.update([1]);
                field(hasher, value.as_bytes());
            }
            None => hasher.update([0]),
        }
    }

    let mut hasher = Sha256::new();
    hasher.update(b"pi-hindsight-injection-cache-v1\0");
    hasher.update([match config.backend {
        crate::MemoryBackend::Off => 0,
        crate::MemoryBackend::Local => 1,
        crate::MemoryBackend::Hindsight => 2,
    }]);
    optional_field(&mut hasher, config.hindsight_api_url.as_deref());
    optional_field(&mut hasher, config.hindsight_api_token.as_deref());
    hasher.update([u8::from(config.hindsight_allow_insecure)]);
    field(&mut hasher, config.hindsight_bank_id.as_bytes());
    optional_field(&mut hasher, config.hindsight_bank_id_prefix.as_deref());
    hasher.update([match config.hindsight_scoping {
        crate::HindsightScoping::Global => 0,
        crate::HindsightScoping::PerProject => 1,
        crate::HindsightScoping::PerProjectTagged => 2,
    }]);
    optional_field(&mut hasher, config.hindsight_bank_mission.as_deref());
    optional_field(&mut hasher, config.hindsight_retain_mission.as_deref());
    hasher.update([u8::from(config.hindsight_injection)]);
    hasher.update([match config.hindsight_recall_budget {
        crate::HindsightBudget::Low => 0,
        crate::HindsightBudget::Mid => 1,
        crate::HindsightBudget::High => 2,
    }]);
    hasher.update(config.hindsight_recall_max_tokens.to_le_bytes());
    hasher.update(
        u64::try_from(config.hindsight_recall_types.len())
            .unwrap_or(u64::MAX)
            .to_le_bytes(),
    );
    for recall_type in &config.hindsight_recall_types {
        field(&mut hasher, recall_type.as_bytes());
    }
    hasher.update(config.hindsight_request_timeout_ms.to_le_bytes());
    hasher.update(config.hindsight_recall_timeout_ms.to_le_bytes());
    hasher.update(config.hindsight_retain_timeout_ms.to_le_bytes());
    hasher.update(config.hindsight_reflect_timeout_ms.to_le_bytes());
    field(&mut hasher, request.trim().as_bytes());
    hasher.finalize().into()
}

/// Consecutive identical (tool, error-prefix) tool failures required to stop
/// a turn as a doom loop.
const DOOM_LOOP_THRESHOLD: usize = 3;
/// Character budget used to fingerprint a tool error for repetition matching.
const DOOM_LOOP_ERROR_PREFIX_CHARS: usize = 80;
/// Threshold for deterministic argument errors such as schema rejections: an
/// identical retry can never succeed, so the second attempt ends the turn
/// instead of the third. Workspace-escape rejections are stricter — see
/// [`ToolErrorClass::WorkspaceEscape`] — and stop on the first error.
const DOOM_LOOP_DETERMINISTIC_THRESHOLD: usize = 2;
/// Exact `FsError::OutsideCapability` Display; longer than `DOOM_LOOP_ERROR_PREFIX_CHARS`.
const FS_OUTSIDE_CAPABILITY_ERROR: &str =
    "filesystem capability does not cover the requested path; add it with --add-dir or sandbox.readOnlyRoots";

/// Tool errors that look transient (network/timeout blips) never trip the
/// doom-loop detector: the same call may legitimately succeed on the next
/// attempt, so they must not count toward the threshold.
const TRANSIENT_TOOL_ERROR_MARKERS: &[&str] = &[
    "timed out",
    "timeout",
    "failed to connect",
    "unable to access",
    "connection reset",
    "connection refused",
    "network error",
    "temporarily unavailable",
    "transient",
];

/// Identity of the run of identical failures currently being tracked.
struct DoomLoopState {
    tool: String,
    args_hash: [u8; 32],
    error_prefix: String,
    count: usize,
}

/// Per-turn doom-loop recovery state. Reset at the start of every turn.
#[derive(Default)]
struct DoomLoopTracker {
    /// The consecutive identical (tool, error-prefix) failure run, if any.
    current: Option<DoomLoopState>,
    /// Set once the threshold trips: every further tool outcome in this turn
    /// terminates with this message (so a parallel batch cannot escape the
    /// stop), and the turn errors out with it.
    triggered_message: Option<String>,
}

impl DoomLoopTracker {
    fn reset(&mut self) {
        self.current = None;
        self.triggered_message = None;
    }
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

struct AgentMentionContextGuard {
    inner: Arc<SessionRuntime>,
}

impl AgentMentionContextGuard {
    fn begin(inner: Arc<SessionRuntime>, name: Option<String>) -> Self {
        *inner.active_agent_mention.write() = name;
        Self { inner }
    }
}

impl Drop for AgentMentionContextGuard {
    fn drop(&mut self) {
        self.inner.active_agent_mention.write().take();
    }
}

/// Upper bound for a single compaction/branch summarization provider call.
///
/// Provider SSE bodies are intentionally uncapped in `pi-ai` (only
/// time-to-headers is bounded), so a server that sends headers and then never
/// terminates the body would otherwise hang `/compact` and automatic
/// compaction forever while holding the session's exclusive run slot. A
/// legitimately long summary of a huge conversation finishes well inside this
/// bound; a stalled provider fails with an actionable error instead.
const COMPACTION_SUMMARIZE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// System prompt for the one-shot vision delegation call
/// (`settings.visionModel`): non-vision models get a text description of the
/// prompt's image blocks instead of the images themselves.
const VISION_DELEGATION_SYSTEM_PROMPT: &str = "You are a vision assistant. Describe the images in detail, focusing on code, UI, diagrams, and technical content. Be concise but thorough.";

/// Bounds the whole vision-delegation provider exchange (stream creation +
/// drain + result), mirroring the summarization timeout: provider SSE bodies
/// are uncapped in `pi-ai` (only time-to-headers is bounded), so a stalled
/// vision provider would otherwise hang the turn forever.
const VISION_DELEGATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

struct SessionInner {
    cwd: PathBuf,
    session_dir: RwLock<PathBuf>,
    workspace: crate::WorkspaceRoots,
    system_prompt: String,
    base_tools: Vec<pi_agent::AgentTool>,
    /// Ambient sessions may gain `mcp` on reload; explicit `tools` lists do not.
    ambient_tool_set: bool,
    tools: RwLock<Vec<pi_agent::AgentTool>>,
    all_tools: RwLock<Vec<pi_agent::AgentTool>>,
    tool_selection: ToolSelection,
    shared: Arc<SessionRuntime>,
    raw_before_tool_call: RwLock<Option<BeforeToolCallFn>>,
    raw_after_tool_call: RwLock<Option<AfterToolCallFn>>,
    agent: Agent,
    _history_subscription: Subscription,
    run_slot: Mutex<RunSlot>,
    idle: Notify,
    abort_notify: Notify,
    /// Resource manager backing the current captured application generation.
    resources: Arc<RwLock<Option<ResourceManager>>>,
    generation_capture: Arc<RwLock<Option<crate::tools::GenerationCaptureFn>>>,
    /// True once an explicit sandbox authority source has been bound to this
    /// session — an inherited generation capture, a `bind_generation_capture`
    /// call, or attached resources. When false the session is "raw" (no
    /// sandbox authority configured), so provider streaming falls back to the
    /// base stream function with host HTTP instead of fail-closing; when true
    /// the sandbox HTTP executor is mandatory and capture failures surface.
    generation_authority_required: Arc<AtomicBool>,
    /// Live path-permission-rule source for file-touching tools (the `lsp`
    /// tool's rename preflight). Set from the attached resource manager on
    /// `attach_resources` (same live data the host approval hook consults);
    /// orchestration children inherit the parent's source explicitly.
    permission_rules: RwLock<Option<crate::PermissionRulesSource>>,
    bash_controller: Mutex<Option<ActiveBash>>,
    bash_generation: AtomicU64,
    bash_append_lock: tokio::sync::Mutex<()>,
    brush_exec_lock: Arc<tokio::sync::Mutex<()>>,
    pending_bash_messages: Mutex<Vec<Message>>,
    process_manager: ProcessManager,
    process_owner_id: ProcessOwnerId,
    skill_snapshot: Arc<RwLock<Vec<crate::Skill>>>,
    todo: TodoRuntime,
    /// Current validated context snapshot, replaced atomically with resource
    /// reloads and copied into each newly spawned orchestration child.
    context_files: RwLock<Vec<crate::ContextFile>>,
    /// Session-scoped MCP server registry; configured from settings on every
    /// resource load so `mcpServers` changes take effect on reload.
    mcp: crate::mcp::McpRegistry,
    /// Session-level transform hook (application/extension context reduction).
    /// The agent's `transform_context` is a fixed composite that runs the
    /// vision projection first and then this hook, so extension context
    /// reduction still sees the delegated (image-free) transcript exactly as
    /// it did when delegation ran at ingest. Standalone `Arc` so the composite
    /// hook can capture it without referencing `SessionInner` (which would
    /// create a drop cycle through the agent).
    transform_context: Arc<RwLock<Option<TransformContextFn>>>,
    /// Cache of vision-delegated projections keyed by a stable hash of the
    /// original user content. Images are delegated once at ingest (fail-closed,
    /// before anything is recorded); every later model-context projection —
    /// retries, follow-up turns, reloaded sessions — is a pure cache hit, so
    /// historical images never reach a non-vision model and are never
    /// re-delegated. Standalone `Arc` for the same drop-cycle reason as
    /// `transform_context`.
    vision_delegations: Arc<Mutex<HashMap<u64, Vec<ContentBlock>>>>,
    /// Retained agent resource root for tests that reuse the session `cwd` as
    /// the agent directory. Kept on the session so the relocated directory
    /// outlives the published `ResourceManager` without leaking a `TempDir`
    /// lifetime into the caller. Test-only: production builds omit the field.
    #[cfg(test)]
    retained_agent_storage: Mutex<Option<tempfile::TempDir>>,
    #[cfg(test)]
    fail_fork_prepare_after_materialize: AtomicBool,
    #[cfg(test)]
    fail_fork_resume_after_materialize: AtomicBool,
    #[cfg(test)]
    fork_before_install_gate: Mutex<Option<SessionForkInstallTestGate>>,
    #[cfg(test)]
    fork_after_install_gate: Mutex<Option<SessionForkInstallTestGate>>,
}

#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct SessionForkInstallTestGate {
    entered: tokio_util::sync::CancellationToken,
    release: tokio_util::sync::CancellationToken,
}

#[cfg(test)]
impl SessionForkInstallTestGate {
    pub(crate) async fn wait_entered(&self) {
        self.entered.cancelled().await;
    }

    pub(crate) fn release(&self) {
        self.release.cancel();
    }

    async fn enter(&self) {
        self.entered.cancel();
        self.release.cancelled().await;
    }
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
        if active
            .as_ref()
            .is_some_and(|active| active.generation == self.generation)
        {
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
        if active
            .as_ref()
            .is_some_and(|active| active.generation == self.generation)
        {
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
    pub(crate) tools: Vec<AgentTool>,
    pub(crate) all_tools: Option<Vec<AgentTool>>,
    pub(crate) system_prompt: String,
    pub(crate) skills: Vec<crate::Skill>,
    pub(crate) agents: Vec<crate::AgentDefinition>,
    pub(crate) context_files: Vec<crate::ContextFile>,
    pub(crate) selector_settings: crate::SelectorSettings,
    pub(crate) runtime_settings: crate::RuntimeSettingsSnapshot,
    pub(crate) hooks: Vec<crate::HookConfig>,
    pub(crate) mcp_servers: Vec<crate::McpServerConfig>,
}

pub struct PreparedSessionReplacement {
    recorder: SessionRecorder,
    messages: Vec<Message>,
    model: Option<Model>,
    api_key: Option<String>,
    thinking_level: ThinkingLevel,
    session_name: Option<String>,
    todo_snapshot: TodoSnapshotRecord,
    goal: GoalRuntime,
    unpublished_fork: Option<UnpublishedFork>,
    /// Fork, resume, and branch keep acknowledged steering. A fresh session
    /// drops it.
    migrate_queues: bool,
}

pub(crate) struct PreparedSessionReplacementCommit {
    replacement: PreparedSessionReplacement,
    mcp: crate::mcp::PreparedMcpReset,
}
impl PreparedSessionReplacementCommit {
    /// Crosses the MCP reset point of no return and returns the replacement
    /// for the existing infallible in-memory install sequence.
    pub(crate) async fn commit_mcp_reset(self) -> PreparedSessionReplacement {
        let Self { replacement, mcp } = self;
        mcp.commit().await;
        replacement
    }

    pub(crate) fn rollback_unpublished(self, error: anyhow::Error) -> anyhow::Error {
        self.replacement.rollback_unpublished(error)
    }
}

impl PreparedSessionReplacement {
    #[must_use]
    pub(crate) fn recorder_info(&self) -> (String, PathBuf) {
        (self.recorder.id(), self.recorder.path())
    }

    pub(crate) fn recorder_file_authority(
        &self,
    ) -> Option<(crate::fs::CapabilityRootLease, PathBuf)> {
        self.recorder.file_authority()
    }

    #[must_use]
    pub(crate) fn goal_runtime(&self) -> GoalRuntime {
        self.goal.clone()
    }

    /// Roll back an unpublished fork while preserving the operation error.
    /// Non-fork replacements carry no cleanup ownership and return it unchanged.
    pub(crate) fn rollback_unpublished(mut self, error: anyhow::Error) -> anyhow::Error {
        match self.unpublished_fork.as_mut() {
            Some(unpublished_fork) => unpublished_fork.rollback(error),
            None => error,
        }
    }
}

/// Owns a durable fork journal until the complete child runtime becomes visible.
/// The guard is disarmed only after recorder, agent/shared messages, Todo,
/// goal, name, model, and thinking authority are coherent; cancellation before
/// that step removes the exact child, while cancellation afterward cannot
/// delete the published journal.
#[derive(Debug)]
pub(crate) struct UnpublishedFork {
    recorder: SessionRecorder,
    armed: bool,
}

impl UnpublishedFork {
    fn new(recorder: &SessionRecorder) -> Self {
        Self {
            recorder: recorder.clone(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    pub(crate) fn rollback(&mut self, error: anyhow::Error) -> anyhow::Error {
        self.armed = false;
        rollback_materialized_fork(&self.recorder, error)
    }
}

impl Drop for UnpublishedFork {
    fn drop(&mut self) {
        if self.armed {
            let stderr = std::io::stderr();
            let mut stderr = stderr.lock();
            let _ = writeln!(
                stderr,
                "session: unpublished fork reached defensive Drop cleanup; explicit rollback was skipped"
            );
            if let Err(error) = self.recorder.discard_persisted() {
                let _ = writeln!(
                    stderr,
                    "session: defensive unpublished-fork cleanup failed: {error:#}"
                );
            }
        }
    }
}

fn rollback_materialized_fork(recorder: &SessionRecorder, error: anyhow::Error) -> anyhow::Error {
    match recorder.discard_persisted() {
        Ok(()) => error,
        Err(cleanup) => error.context(format!(
            "materialized fork cleanup also failed: {cleanup:#}"
        )),
    }
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
pub enum CompactionReason {
    Manual,
    Threshold,
    Overflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SummarizationSource {
    Compaction,
    BranchSummary,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum SessionEvent {
    AutoRetryStart {
        attempt: usize,
        max_attempts: usize,
        delay_ms: u64,
        error_message: String,
    },
    AutoRetryEnd {
        success: bool,
        attempt: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        final_error: Option<String>,
    },
    RetryFallbackApplied {
        from: String,
        to: String,
        role: String,
    },
    RetryFallbackSucceeded {
        model: String,
        role: String,
    },
    BashExecutionUpdate {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        delta: String,
    },
    BashExecutionEnd {
        message: BashExecutionMessage,
    },
    CompactionStart {
        reason: CompactionReason,
    },
    CompactionEnd {
        reason: CompactionReason,
        result: Option<CompactionResult>,
        aborted: bool,
        will_retry: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error_message: Option<String>,
    },
    SummarizationRetryScheduled {
        attempt: usize,
        max_attempts: usize,
        delay_ms: u64,
        error_message: String,
    },
    SummarizationRetryAttemptStart {
        source: SummarizationSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<CompactionReason>,
    },
    SummarizationRetryFinished,
    QueueUpdate {
        steering: Vec<Message>,
        follow_up: Vec<Message>,
    },
    AskUser {
        id: String,
        prompt: String,
    },
    AskUserResolved {
        id: String,
    },
    EntryAppended {
        entry: crate::SessionEntry,
    },
    SessionInfoChanged {
        name: Option<String>,
    },
    ThinkingLevelChanged {
        thinking_level: ThinkingLevel,
    },
    ModelSelect {
        model: Model,
    },
    ThinkingLevelSelect {
        thinking_level: ThinkingLevel,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SteeringSendNow {
    pub sent: usize,
    pub combined: bool,
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

#[derive(Debug, Clone)]
pub struct TranscriptRow {
    pub id: Option<String>,
    pub message: Message,
}

#[derive(Debug)]
pub struct TranscriptUnavailable {
    source: Option<anyhow::Error>,
}

impl TranscriptUnavailable {
    fn new() -> Self {
        Self { source: None }
    }
}

impl From<anyhow::Error> for TranscriptUnavailable {
    fn from(source: anyhow::Error) -> Self {
        Self {
            source: Some(source),
        }
    }
}

impl fmt::Display for TranscriptUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.source {
            Some(source) => write!(f, "transcript unavailable: {source}"),
            None => f.write_str("transcript unavailable"),
        }
    }
}

impl std::error::Error for TranscriptUnavailable {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|error| {
            let error: &(dyn std::error::Error + 'static) = &**error;
            error
        })
    }
}

fn transcript_rows_from_branch(
    tree: &crate::SessionTree,
    leaf_id: Option<&str>,
) -> Vec<TranscriptRow> {
    tree.branch(leaf_id)
        .into_iter()
        .filter_map(|entry| {
            let mut messages = Vec::with_capacity(1);
            crate::session_store::append_entry_message(&mut messages, entry);
            let message = messages.pop()?;
            match &message {
                Message::BranchSummary(_) | Message::CompactionSummary(_) => None,
                _ => Some(TranscriptRow {
                    id: Some(entry.id.clone()),
                    message,
                }),
            }
        })
        .collect()
}

fn project_retained_transcript(
    tree: &crate::SessionTree,
) -> Result<Vec<TranscriptRow>, TranscriptUnavailable> {
    let rows = transcript_rows_from_branch(tree, None);
    if !rows.is_empty() {
        return Ok(rows);
    }
    let has_decoded_message = tree.entries.iter().any(|entry| entry.message.is_some());
    if !has_decoded_message && !tree.has_loader_drops() {
        return Ok(rows);
    }
    if has_decoded_message
        && tree
            .active_leaf_id
            .as_deref()
            .is_some_and(|id| tree.has_id(id))
    {
        return Ok(rows);
    }
    if let Some(leaf) = tree.leaf_id.as_deref() {
        let recovered = transcript_rows_from_branch(tree, Some(leaf));
        if !recovered.is_empty() {
            return Ok(recovered);
        }
    }
    Err(TranscriptUnavailable::new())
}

/// Where a `/rewind` rolls the session back to.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RewindTarget {
    /// Keep the first `index` records (drop record `index` and everything
    /// after it). `index` is the 0-based position in the session record file
    /// as shown by the bare `/rewind` listing.
    Index(usize),
    /// Roll back to the record the named checkpoint points at (the position
    /// that was current when `/checkpoint <name>` was recorded).
    Checkpoint(String),
}

/// Outcome of a session-level rewind.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RewindOutcome {
    /// Sidecar JSONL file holding the truncated tail records.
    pub archive_path: PathBuf,
    /// Number of records dropped by the truncation.
    pub dropped_entries: usize,
    /// Number of records retained after the truncation.
    pub retained_entries: usize,
    /// The resolved checkpoint name when the rewind targeted a checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
}

/// One row of the bare `/rewind` picker: record index plus a first-line
/// preview so the user can choose an entry to roll back to.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RewindEntryPreview {
    pub index: usize,
    pub entry_type: String,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint_target_id: Option<String>,
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
    let mut content = format!(
        "<system-reminder>\nActive session goal (revision {}, lifecycle {lifecycle}).\nObjective: {objective}\nToken budget: {}/{budget}.\n",
        state.revision, goal.usage.tokens_used
    );
    if !goal.pins.is_empty() {
        content.push_str("Role-model pins:\n");
        for (index, pin) in goal.pins.iter().enumerate() {
            content.push_str(&format!("{}. {}\n", index + 1, escape_goal_objective(pin)));
        }
    }
    content.push_str("Keep this goal in scope. Use the goal tool to inspect, pause, or complete it when appropriate.\n</system-reminder>");
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

pub type ContextFilesFn = Arc<dyn Fn() -> Vec<crate::ContextFile> + Send + Sync>;
#[derive(Clone)]
pub struct ChildGenerationCapture {
    inner: crate::tools::GenerationCaptureFn,
    /// Whether the parent session had an explicit sandbox authority bound
    /// (resources, inherited capture, or `bind_generation_capture`). Children
    /// inherit this so a raw parent produces raw children (host HTTP) while an
    /// authority-bound parent produces fail-closed children.
    authority_required: bool,
}

impl ChildGenerationCapture {
    pub(crate) fn new(inner: crate::tools::GenerationCaptureFn) -> Self {
        Self {
            inner,
            authority_required: true,
        }
    }

    pub(crate) fn with_authority(
        inner: crate::tools::GenerationCaptureFn,
        authority_required: bool,
    ) -> Self {
        Self {
            inner,
            authority_required,
        }
    }

    pub(crate) fn capture_fn(&self) -> crate::tools::GenerationCaptureFn {
        self.inner.clone()
    }

    pub(crate) fn authority_required(&self) -> bool {
        self.authority_required
    }
}

#[derive(Clone)]
pub struct ChildSessionOptionsSnapshot {
    pub model: Model,
    pub workspace: crate::WorkspaceRoots,
    pub thinking_level: ThinkingLevel,
    pub api_key: String,
    pub stream_options: SimpleStreamOptions,
    pub stream_fn: StreamFn,
    pub auth_resolver: Option<SessionAuthResolver>,
    pub auth_manager: Option<Arc<crate::AuthManager>>,
    pub retry_settings: RetrySettings,
    pub retry_role: Option<String>,
    /// Opaque capture of the parent's current application generation for child tools.
    pub generation_capture: ChildGenerationCapture,
    /// Resolves the parent's live memory backend for future child spawns.
    pub memory: Option<crate::MemoryConfigFn>,
    /// The session's live path-permission-rule source, inherited by children
    /// so their `lsp` tool's rename preflight obeys the same rules as the
    /// parent's host approval.
    pub permission_rules: Option<crate::PermissionRulesSource>,
    /// Resolves the validated context snapshot for each new child spawn. Live
    /// sources follow parent reloads; fixed sources keep their captured bytes.
    pub context_files_resolver: Option<ContextFilesFn>,
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
            Vec::new(),
            selection,
            true,
            resource_discovery,
            None,
            None,
            None,
            false,
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
            Vec::new(),
            selection,
            true,
            resource_discovery,
            None,
            uri_resolver,
            None,
            false,
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
            Vec::new(),
            selection,
            false,
            resource_discovery,
            None,
            None,
            None,
            false,
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
            Vec::new(),
            selection,
            false,
            resource_discovery,
            None,
            uri_resolver,
            None,
            false,
        )
    }

    pub fn new_with_todo_additional_tools_filtered_discovery_workspace_and_uri(
        options: SessionOptions,
        additional_tools: Vec<AgentTool>,
        selection: ToolSelection,
        resource_discovery: ResourceDiscovery,
        workspace: crate::WorkspaceRoots,
        uri_resolver: Option<crate::InternalUriResolverFn>,
        todo_pinned: bool,
    ) -> Result<Self> {
        Self::new_configured(
            options,
            additional_tools,
            Vec::new(),
            selection,
            true,
            resource_discovery,
            Some(workspace),
            uri_resolver,
            None,
            todo_pinned,
        )
    }
    /// `persistent_tools` persist in the reload foundation; `generation_tools` are per-generation.
    pub fn new_with_persistent_and_generation_tools_filtered_discovery_workspace_and_uri(
        options: SessionOptions,
        persistent_tools: Vec<AgentTool>,
        generation_tools: Vec<AgentTool>,
        selection: ToolSelection,
        resource_discovery: ResourceDiscovery,
        workspace: crate::WorkspaceRoots,
        uri_resolver: Option<crate::InternalUriResolverFn>,
        todo_enabled: bool,
        todo_pinned: bool,
    ) -> Result<Self> {
        Self::new_configured(
            options,
            persistent_tools,
            generation_tools,
            selection,
            todo_enabled,
            resource_discovery,
            Some(workspace),
            uri_resolver,
            None,
            todo_pinned,
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
            Vec::new(),
            selection,
            false,
            resource_discovery,
            Some(workspace),
            uri_resolver,
            None,
            false,
        )
    }

    pub(crate) fn new_with_inherited_generation(
        options: SessionOptions,
        additional_tools: Vec<AgentTool>,
        selection: ToolSelection,
        resource_discovery: ResourceDiscovery,
        workspace: crate::WorkspaceRoots,
        uri_resolver: Option<crate::InternalUriResolverFn>,
        generation_capture: crate::tools::GenerationCaptureFn,
        authority_required: bool,
    ) -> Result<Self> {
        Self::new_configured(
            options,
            additional_tools,
            Vec::new(),
            selection,
            false,
            resource_discovery,
            Some(workspace),
            uri_resolver,
            Some((generation_capture, authority_required)),
            false,
        )
    }

    fn new_configured(
        options: SessionOptions,
        additional_tools: Vec<AgentTool>,
        generation_tools: Vec<AgentTool>,
        selection: ToolSelection,
        todo_enabled: bool,
        resource_discovery: ResourceDiscovery,
        workspace: Option<crate::WorkspaceRoots>,
        uri_resolver: Option<crate::InternalUriResolverFn>,
        inherited_generation: Option<(crate::tools::GenerationCaptureFn, bool)>,
        todo_pinned: bool,
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
        let ambient_tool_set = custom_tools.is_none();
        let custom_prompt = options.system_prompt;
        let before_tool_call = options.before_tool_call;
        let after_tool_call = if todo_enabled {
            Some(todo_after_tool_call(options.after_tool_call))
        } else {
            options.after_tool_call
        };
        let auth_resolver = options.auth_resolver;
        let auth_manager = options.auth_manager;
        let effective_stream_fn = options
            .stream_fn
            .unwrap_or_else(|| AgentOptions::default().stream_fn);
        // Namespace-aware provider dispatch: when an extension runtime is
        // bound to this session (`Session::set_provider_namespace`), a model
        // whose api is owned by an extension runtime resolves strictly within
        // that runtime's namespace — its own closure, or a contextual
        // fail-closed error, never another runtime's registration. Apis that
        // are not extension-owned (builtins, unknown) fall through to the
        // base stream function unchanged, so builtin credential resolution
        // and custom stream fns keep their exact behavior. The namespace cell
        // is shared with `SessionRuntime`, so the wrapper is built here —
        // before the shared runtime — and `SessionRuntime.stream_fn` (the
        // child-session snapshot path) carries it too.
        let resources_cell: Arc<RwLock<Option<crate::ResourceManager>>> =
            Arc::new(RwLock::new(None));
        let provider_namespace: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));
        let inherited_generation_authority = inherited_generation
            .as_ref()
            .map(|(_, authority)| *authority)
            .unwrap_or(false);
        let generation_capture_cell = Arc::new(RwLock::new(
            inherited_generation.map(|(capture, _)| capture),
        ));
        let generation_authority_required =
            Arc::new(AtomicBool::new(inherited_generation_authority));
        let namespace_cell = provider_namespace.clone();
        let provider_resources = resources_cell.clone();
        let provider_generation_capture = generation_capture_cell.clone();
        let provider_authority_required = generation_authority_required.clone();
        let base_stream = effective_stream_fn.clone();
        let effective_stream_fn: StreamFn = Arc::new(
            move |model: Model, context: Context, mut options: SimpleStreamOptions| {
                let namespace = namespace_cell.read().clone();
                let resources = provider_resources.read().clone();
                let generation_capture = provider_generation_capture.read().clone();
                let authority_required = provider_authority_required.load(Ordering::Acquire);
                let base_stream = base_stream.clone();
                Box::pin(async move {
                    let generation = match resources {
                        Some(resources) => resources.capture_application_generation(),
                        None => match generation_capture {
                            Some(capture) => capture(),
                            None => Err(anyhow!("application generation is unavailable")),
                        },
                    };
                    let generation = match generation {
                        Ok(generation) => generation,
                        Err(error) => {
                            // A "raw" session — no resources, no inherited
                            // capture, no explicit `bind_generation_capture` —
                            // has no sandbox authority to enforce, so provider
                            // streaming uses the base stream (host HTTP) instead
                            // of fail-closing. Sessions that bound authority
                            // (`authority_required`) MUST route through the
                            // sandbox HTTP executor; a capture failure there is
                            // a real fail-closed error, never a silent bypass.
                            if !authority_required {
                                return base_stream(model, context, options).await;
                            }
                            return auth_error_stream(&model, error.to_string()).await;
                        }
                    };
                    options.stream.http_executor = Some(
                        generation
                            .sandbox()
                            .http()
                            .executor(pi_sandbox::HostHttpClass::ProviderApi),
                    );
                    let Some(namespace) = namespace else {
                        return base_stream(model, context, options).await;
                    };
                    match pi_ai::resolve_extension_provider(&model.api, Some(&namespace)) {
                        Ok(Some(provider)) => {
                            (provider.stream_simple)(model, context, options).await
                        }
                        Ok(None) => base_stream(model, context, options).await,
                        Err(scope_error) => provider_scope_error_stream(&model, &scope_error).await,
                    }
                })
            },
        );
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
        let ask_runtime = AskRuntime::new(events.clone());
        let shared = Arc::new(SessionRuntime {
            state: RwLock::new(SessionState {
                model: model.clone(),
                thinking_level,
                api_key: api_key.clone(),
                messages: Vec::new(),
                agent_event_barrier: 0,
            }),
            pending_next_turn: Mutex::new(Vec::new()),
            pending_steering_markers: Mutex::new(Vec::new()),
            recorder: Mutex::new(None),
            goal: RwLock::new(GoalRuntime::memory()),
            active_agent_mention: RwLock::new(None),
            compaction: RwLock::new(compaction),
            compaction_runtime: Mutex::new(CompactionRuntime::default()),
            compaction_active: AtomicBool::new(false),
            compaction_controller: Mutex::new(None),
            before_compaction: RwLock::new(None),
            overflow_recovery_attempted: AtomicBool::new(false),
            session_name: RwLock::new(None),
            retry_settings: RwLock::new(RetrySettings::default()),
            retry_role: RwLock::new(None),
            retry_controller: Mutex::new(None),
            retry_attempt: AtomicUsize::new(0),
            active_retry_fallback: Mutex::new(None),
            fallback_attempt_errors: Mutex::new(Vec::new()),
            doom_loop: Mutex::new(DoomLoopTracker::default()),
            events,
            stream_options: RwLock::new(stream_options.clone()),
            recorded_count: AtomicUsize::new(0),
            stream_fn: effective_stream_fn.clone(),
            auth_resolver: auth_resolver.clone(),
            auth_manager,
            provider_namespace: provider_namespace.clone(),
            selector_settings: RwLock::new(crate::SelectorSettings::default()),
            selector_skills: RwLock::new(Vec::new()),
            selector_agents: RwLock::new(Vec::new()),
            branch_summary: RwLock::new(crate::EffectiveBranchSummarySettings {
                reserve_tokens: 16_384,
                skip_prompt: false,
            }),
            vision_model: RwLock::new(None),
            expose_session_environment: AtomicBool::new(true),
            offline: AtomicBool::new(false),
            last_selection: RwLock::new(None),
            hindsight_injection_cache: RwLock::new(None),
            session_id: session_id.clone(),
            queue_update: tokio::sync::Mutex::new(()),
            host_hooks: RwLock::new(None),
            extension_tool_names: RwLock::new(HashSet::new()),
            session_started: AtomicBool::new(false),
            bash_spill_paths: Mutex::new(HashSet::new()),
            ask: ask_runtime,
            #[cfg(test)]
            fail_next_todo_execution_persist: AtomicBool::new(false),

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
            Arc::new(move |snapshot| {
                #[cfg(test)]
                if snapshot.execution_status.is_some()
                    && persist_shared
                        .fail_next_todo_execution_persist
                        .swap(false, Ordering::AcqRel)
                {
                    return Err(anyhow!("injected Todo execution persist failure"));
                }
                if let Some(recorder) = persist_shared.recorder.lock().as_ref().cloned() {
                    recorder.record_todo_snapshot_record(snapshot)?;
                }
                Ok(())
            }),

        );
        let env_runtime = shared.clone();
        let session_env: crate::SessionEnvFn = Arc::new(move || {
            if !env_runtime
                .expose_session_environment
                .load(Ordering::Acquire)
            {
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
        if generation_capture_cell.read().is_none() {
            let generation_resources = resources_cell.clone();
            *generation_capture_cell.write() = Some(Arc::new(move || {
                generation_resources
                    .read()
                    .as_ref()
                    .ok_or_else(|| anyhow!("application generation is unavailable"))?
                    .capture_application_generation()
            }));
        }
        let tool_generation_capture: crate::tools::GenerationCaptureFn = {
            let generation_capture_cell = generation_capture_cell.clone();
            Arc::new(move || {
                generation_capture_cell
                    .read()
                    .as_ref()
                    .ok_or_else(|| anyhow!("application generation is unavailable"))?(
                )
            })
        };
        let mcp_registry = crate::mcp::McpRegistry::new(
            tool_generation_capture.clone(),
            shared.auth_manager.clone(),
        );
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
                load_context_files(&cwd_text, include_project_resources)?
            }
        };
        let skill_snapshot = Arc::new(RwLock::new(initial_skills.clone()));
        *shared.selector_skills.write() = initial_skills.clone();
        let skills_runtime = skill_snapshot.clone();
        let skill_provider: crate::SkillSnapshotFn =
            Arc::new(move || skills_runtime.read().clone());

        // Memory backend resolver: reads the live settings snapshot so
        // `settings.memory.backend/hindsight*` changes apply to the next tool
        // rebuild (RELOAD) and to turn-start injection. Before resources are
        // attached the resolver returns `None` and the built-in `local`
        // backend is used; `attach_resources` reconciles the tool set against
        // the actual settings.
        let memory_runtime = resources_cell.clone();
        let memory_resolver: crate::MemoryConfigFn = Arc::new(move || {
            let resources = memory_runtime.read();
            let resources = resources.as_ref()?;
            Some(resources.snapshot().settings.memory_config())
        });

        // Image-generation resolver: resolves the `generate_image` tool's
        // model, endpoint overrides, and credential from live settings
        // (`images.genModel`/`genBaseUrl`/`genApiKey`), the active session
        // model, and the session auth resolver — mirroring how streaming
        // resolves its model + key. Reads settings per call so a settings
        // reload applies to the next generation. Before resources are
        // attached the resolver falls back to the session model + auth.
        let image_gen_runtime = shared.clone();
        let image_gen_auth = auth_resolver.clone();
        let image_gen_settings = resources_cell.clone();
        let image_gen_resolver: crate::ImageGenConfigFn = Arc::new(move |spec: Option<String>| {
            let state = image_gen_runtime.clone();
            let auth = image_gen_auth.clone();
            let settings = image_gen_settings.clone();
            Box::pin(async move {
                let runtime = settings
                    .read()
                    .as_ref()
                    .map(|resources| resources.snapshot().settings.image_gen_runtime());
                // Model: explicit `model` argument > settings images.genModel
                // > the active session model.
                let model = if let Some(spec) = spec {
                    crate::resolve_model(&spec).map_err(|error| anyhow!("{error}"))?
                } else if let Some(spec) = runtime
                    .as_ref()
                    .and_then(|runtime| runtime.gen_model.clone())
                {
                    crate::resolve_model(&spec).map_err(|error| anyhow!("{error}"))?
                } else {
                    state.state.read().model.clone()
                };
                // Credential and environment: resolve the model auth once so endpoint
                // placeholders use the same environment staged into the generation policy;
                // settings images.genApiKey overrides only the credential.
                let resolved_auth = match auth.as_ref() {
                    Some(resolver) => Some(resolver(model.clone()).await?),
                    None => None,
                };
                let env = resolved_auth
                    .as_ref()
                    .map(|auth| auth.env.clone())
                    .unwrap_or_else(|| state.stream_options.read().stream.env.clone());
                let api_key = if let Some(key) = runtime
                    .as_ref()
                    .map(|runtime| runtime.gen_api_key.clone())
                    .filter(|key| !key.trim().is_empty())
                {
                    Some(key)
                } else if let Some(auth) = resolved_auth {
                    Some(auth.api_key)
                } else if !state.state.read().api_key.trim().is_empty() {
                    Some(state.state.read().api_key.clone())
                } else {
                    None
                };
                Ok(crate::ImageGenConfig {
                    model,
                    base_url: runtime
                        .as_ref()
                        .map(|runtime| runtime.gen_base_url.clone())
                        .filter(|base| !base.trim().is_empty()),
                    api_key,
                    env,
                })
            })
        });

        let mut base_tools = if let Some(custom_tools) = custom_tools {
            custom_tools
        } else {
            crate::tools::create_coding_tools_for_workspace_with_generation(
                workspace.clone(),
                Some(session_env),
                Some(skill_provider),
                Some(BashProcessContext {
                    manager: process_manager.clone(),
                    owner_id: process_owner_id.clone(),
                    auth_manager: shared.auth_manager.clone(),
                }),
                uri_resolver,
                Some(memory_resolver.clone()),
                Some(image_gen_resolver.clone()),
                tool_generation_capture.clone(),
            )?
        };
        if todo_enabled && todo_pinned {
            base_tools.push(create_todo_tool(todo.clone()));
        }
        // Keep caller tools in the reload foundation across application startup.
        base_tools = merge_tools(&base_tools, additional_tools)?;
        // Generation tools resolve strict selection at construction but
        // never join the persistent base.
        let mut available_tools = merge_tools(&base_tools, generation_tools)?;
        if todo_enabled && !todo_pinned {
            available_tools.push(create_todo_tool(todo.clone()));
        }
        // The interactive `ask` tool joins ambient (default) tool sets only.
        // Callers that pass an explicit `options.tools` list keep full control
        // and opt into `ask` themselves: background subagents and read-only
        // forks shouldn't carry an interactive question tool. Non-interactive
        // frontends reject it up front regardless.
        if ambient_tool_set && !available_tools.iter().any(|tool| tool.name == "ask") {
            available_tools.push(crate::tools::ask::session_ask_tool(shared.ask.clone()));
        }
        // The `mcp` tool joins ambient tool sets bound to a session-scoped
        // registry; resource loads configure it from `settings.mcpServers`.
        if ambient_tool_set && !available_tools.iter().any(|tool| tool.name == "mcp") {
            available_tools.push(crate::mcp::mcp_tool(mcp_registry.clone()));
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
            available_tools.push(crate::create_glob_tool_for_workspace(
                workspace.clone(),
                tool_generation_capture.clone(),
            ));
        }
        let tools = select_tools(available_tools.clone(), &selection)?;
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
            context_files: context_files.clone(),
            skills: initial_skills,
            readme_path,
            docs_path,
            examples_path,
            ..BuildSystemPromptOptions::default()
        });
        let key_runtime = shared.clone();
        let stream_fn = if let Some(resolver) = auth_resolver {
            let fallback = effective_stream_fn.clone();
            Arc::new(
                move |model: Model, context: Context, mut options: SimpleStreamOptions| {
                    let resolver = resolver.clone();
                    let fallback = fallback.clone();
                    let future: pi_agent::BoxFuture<pi_ai::AssistantMessageEventStream> =
                        Box::pin(async move {
                            match resolver(model.clone()).await {
                                Ok(auth) => {
                                    options.stream.api_key = Some(auth.api_key);
                                    merge_headers_case_insensitive(
                                        &mut options.stream.headers,
                                        auth.headers,
                                    );
                                    options.stream.env.extend(auth.env);
                                    fallback(model, context, options).await
                                }
                                Err(error) => auth_error_stream(&model, error.to_string()).await,
                            }
                        });
                    future
                },
            ) as StreamFn
        } else {
            effective_stream_fn.clone()
        };
        let context_runtime = shared.clone();
        let raw_before_tool_call = before_tool_call.clone();
        let before_tool_call = compose_host_pre_tool_call(shared.clone(), before_tool_call);
        let raw_after_tool_call = after_tool_call.clone();
        let after_tool_call = compose_host_post_tool_call(shared.clone(), after_tool_call);
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
                messages.retain(|message| {
                    !matches!(message, Message::Custom(custom)
                        if custom.custom_type == AGENT_MENTION_CUSTOM_TYPE)
                });
                if let Some(name) = context_runtime.active_agent_mention.read().clone() {
                    let position = messages
                        .iter()
                        .rposition(|message| matches!(message, Message::User(_)))
                        .unwrap_or(messages.len());
                    messages.insert(position, Session::agent_mention_message(&name));
                }
                if let Some(message) = goal_context_message(&context_runtime.goal.read().get()) {
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
                let mut state = shared.state.write();
                state.agent_event_barrier = state.agent_event_barrier.saturating_add(1);
                if let AgentEvent::MessageEnd { message } = event {
                    state.messages.push(message);
                }
                Ok(())
            }
        }))?;
        let inner = Arc::new(SessionInner {
            session_dir: RwLock::new(crate::default_session_dir(&cwd)),
            cwd,
            workspace,
            system_prompt,
            abort_notify: Notify::new(),
            base_tools,
            ambient_tool_set,
            all_tools: RwLock::new(available_tools),
            tools: RwLock::new(tools),
            tool_selection: selection,
            shared,
            raw_before_tool_call: RwLock::new(raw_before_tool_call),
            raw_after_tool_call: RwLock::new(raw_after_tool_call),
            agent,
            _history_subscription: history_subscription,
            run_slot: Mutex::new(RunSlot::default()),
            idle: Notify::new(),
            resources: resources_cell,
            generation_capture: generation_capture_cell,
            generation_authority_required,
            context_files: RwLock::new(context_files),
            permission_rules: RwLock::new(None),
            bash_controller: Mutex::new(None),
            bash_generation: AtomicU64::new(0),
            bash_append_lock: tokio::sync::Mutex::new(()),
            brush_exec_lock: Arc::new(tokio::sync::Mutex::new(())),
            pending_bash_messages: Mutex::new(Vec::new()),
            process_manager,
            process_owner_id,
            skill_snapshot,
            todo,
            mcp: mcp_registry,
            transform_context: Arc::new(RwLock::new(None)),
            vision_delegations: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(test)]
            retained_agent_storage: Mutex::new(None),
            #[cfg(test)]
            fail_fork_prepare_after_materialize: AtomicBool::new(false),
            #[cfg(test)]
            fail_fork_resume_after_materialize: AtomicBool::new(false),
            #[cfg(test)]
            fork_before_install_gate: Mutex::new(None),
            #[cfg(test)]
            fork_after_install_gate: Mutex::new(None),
        });
        // The agent's context transform is a fixed composite: the vision
        // projection runs first (cache-backed, so only images never seen
        // before hit the provider), then the session-level hook installed via
        // `set_transform_context` (application/extension context reduction).
        // The projection keeps the transcript original-preserving: the agent,
        // recorder, history and UI carry the user's image blocks while the
        // model context always receives the delegated description instead.
        // The closure captures only standalone Arcs (shared runtime, resources,
        // the projection cache, the transform slot) — never `inner` itself —
        // so it cannot form a reference cycle that would leak the session.
        {
            let shared = inner.shared.clone();
            let resources = inner.resources.clone();
            let vision_cache = inner.vision_delegations.clone();
            let transform_slot = inner.transform_context.clone();
            inner
                .agent
                .set_transform_context(Some(Arc::new(move |messages, signal| {
                    let shared = shared.clone();
                    let resources = resources.clone();
                    let vision_cache = vision_cache.clone();
                    let transform_slot = transform_slot.clone();
                    Box::pin(async move {
                        let projected = Session::delegate_vision_messages_with(
                            &shared,
                            &resources,
                            &vision_cache,
                            messages,
                        )
                        .await?;
                        let hook = transform_slot.read().clone();
                        match hook {
                            Some(hook) => hook(projected, signal).await,
                            None => Ok(projected),
                        }
                    }) as pi_agent::BoxFuture<Result<Vec<Message>>>
                })));
        }
        Ok(Self { inner })
    }

    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.inner.cwd
    }

    #[must_use]
    pub(crate) fn brush_exec_lock(&self) -> Arc<tokio::sync::Mutex<()>> {
        self.inner.brush_exec_lock.clone()
    }

    /// Replace the directory used by all future session recordings and forks.
    /// The CLI resolves precedence once and stores the result here so interactive
    /// actions never re-read environment variables or settings.
    pub fn set_session_dir(&self, session_dir: PathBuf) {
        *self.inner.session_dir.write() = session_dir;
    }

    /// Set the session-scoped offline contract: when enabled, network-touching
    /// session operations (e.g. the gist share path) fail closed without
    /// spawning child processes. Never touches the process environment — the
    /// deterministic seam for simulating `RPI_OFFLINE` without
    /// `std::env::set_var` (unsafe in edition 2024 and forbidden here).
    pub fn set_offline(&self, offline: bool) {
        self.inner.shared.offline.store(offline, Ordering::Release);
    }

    /// Current session-scoped offline contract state.
    #[must_use]
    pub fn is_offline(&self) -> bool {
        self.inner.shared.offline.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn session_dir(&self) -> PathBuf {
        self.inner.session_dir.read().clone()
    }

    #[must_use]
    pub fn workspace_roots(&self) -> &crate::WorkspaceRoots {
        &self.inner.workspace
    }

    #[must_use]
    pub(crate) fn session_policy_id(&self) -> String {
        self.inner.shared.session_id.clone()
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
    pub(crate) fn auth_manager(&self) -> Option<Arc<crate::AuthManager>> {
        self.inner.shared.auth_manager.clone()
    }

    #[must_use]
    pub(crate) fn generation_capture_fn(&self) -> crate::tools::GenerationCaptureFn {
        let generation_capture = self.inner.generation_capture.clone();
        Arc::new(move || {
            generation_capture
                .read()
                .as_ref()
                .ok_or_else(|| anyhow!("application generation is unavailable"))?()
        })
    }

    pub(crate) fn capture_application_generation(
        &self,
    ) -> Result<Arc<crate::application::generation::ApplicationGeneration>> {
        self.generation_capture_fn()()
    }

    pub(crate) fn bind_generation_capture(
        &self,
        generation_capture: crate::tools::GenerationCaptureFn,
    ) {
        *self.inner.generation_capture.write() = Some(generation_capture);
        self.inner
            .generation_authority_required
            .store(true, Ordering::Release);
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
            workspace: self.inner.workspace.clone(),
            thinking_level: state.thinking_level,
            api_key: state.api_key.clone(),
            stream_options: self.inner.shared.stream_options.read().clone(),
            stream_fn: self.inner.shared.stream_fn.clone(),
            auth_manager: self.inner.shared.auth_manager.clone(),
            auth_resolver: self.inner.shared.auth_resolver.clone(),
            retry_settings: self.retry_settings(),
            retry_role: self.retry_role(),
            generation_capture: ChildGenerationCapture::with_authority(
                self.generation_capture_fn(),
                self.inner
                    .generation_authority_required
                    .load(Ordering::Acquire),
            ),
            memory: Some(self.memory_config_resolver()),
            permission_rules: self.permission_rules_source(),
            context_files_resolver: Some(self.context_files_resolver()),
        }
    }

    #[must_use]
    pub fn context_files_resolver(&self) -> crate::ContextFilesFn {
        let session = self.clone();
        Arc::new(move || session.inner.context_files.read().clone())
    }

    /// When the active model lacks image support and `settings.visionModel` is
    /// configured, sends image content to the configured vision model and
    /// replaces it with the returned description.
    async fn delegate_vision_images(
        &self,
        content: Vec<ContentBlock>,
    ) -> Result<Vec<ContentBlock>> {
        Self::delegate_vision_images_with(
            &self.inner.shared,
            &self.inner.resources,
            &self.inner.vision_delegations,
            content,
        )
        .await
    }

    async fn delegate_vision_messages(&self, messages: Vec<Message>) -> Result<Vec<Message>> {
        Self::delegate_vision_messages_with(
            &self.inner.shared,
            &self.inner.resources,
            &self.inner.vision_delegations,
            messages,
        )
        .await
    }

    async fn delegate_vision_message(&self, message: Message) -> Result<Message> {
        Self::delegate_vision_message_with(
            &self.inner.shared,
            &self.inner.resources,
            &self.inner.vision_delegations,
            message,
        )
        .await
    }

    /// Free-standing projection shared by the ingest wrappers above and the
    /// agent's composite `transform_context` hook. The hook cannot capture
    /// `SessionInner` (that would form a drop cycle through the agent), so the
    /// delegation logic operates on the standalone Arcs the session owns:
    /// `shared` (runtime state/auth/streams, including live `visionModel`)
    /// and `cache` (per-content delegation results).
    async fn delegate_vision_messages_with(
        shared: &SessionRuntime,
        resources: &RwLock<Option<ResourceManager>>,
        cache: &Mutex<HashMap<u64, Vec<ContentBlock>>>,
        messages: Vec<Message>,
    ) -> Result<Vec<Message>> {
        let mut delegated = Vec::with_capacity(messages.len());
        for message in messages {
            delegated
                .push(Self::delegate_vision_message_with(shared, resources, cache, message).await?);
        }
        Ok(delegated)
    }

    async fn delegate_vision_message_with(
        shared: &SessionRuntime,
        resources: &RwLock<Option<ResourceManager>>,
        cache: &Mutex<HashMap<u64, Vec<ContentBlock>>>,
        message: Message,
    ) -> Result<Message> {
        match message {
            Message::User(mut user) => {
                user.content =
                    Self::delegate_vision_images_with(shared, resources, cache, user.content)
                        .await?;
                Ok(Message::User(user))
            }
            message => Ok(message),
        }
    }

    async fn delegate_vision_images_with(
        shared: &SessionRuntime,
        _resources: &RwLock<Option<ResourceManager>>,
        cache: &Mutex<HashMap<u64, Vec<ContentBlock>>>,
        content: Vec<ContentBlock>,
    ) -> Result<Vec<ContentBlock>> {
        if !content
            .iter()
            .any(|block| matches!(block, ContentBlock::Image { .. }))
        {
            return Ok(content);
        }
        let active_model = shared.state.read().model.clone();
        if active_model.input.iter().any(|input| input == "image") {
            return Ok(content);
        }
        // Live `None` is an explicit disable from `/settings set visionModel`
        // (or an unconfigured session). Do not fall back to the resource
        // snapshot: LIVE apply does not restage, so the snapshot can still
        // hold a stale spec after a live clear.
        let Some(vision_spec) = shared
            .vision_model
            .read()
            .clone()
            .filter(|spec| !spec.trim().is_empty())
        else {
            return Ok(content);
        };
        let key = Self::vision_content_key(&content);
        if let Some(cached) = cache.lock().get(&key).cloned() {
            return Ok(cached);
        }
        let vision_model = crate::resolve_model(&vision_spec).map_err(|error| {
            anyhow!("configured vision model {vision_spec:?} could not be resolved: {error}")
        })?;
        if !vision_model.input.iter().any(|input| input == "image") {
            return Err(anyhow!(
                "configured vision model {}/{} does not support image input",
                vision_model.provider,
                vision_model.id,
            ));
        }

        let images = content
            .iter()
            .filter(|block| matches!(block, ContentBlock::Image { .. }))
            .cloned()
            .collect::<Vec<_>>();
        let mut stream_options = shared.stream_options.read().clone();
        stream_options.stream.api_key = None;
        stream_options.stream.headers.clear();
        stream_options.stream.env.clear();
        if let Some(resolver) = &shared.auth_resolver {
            let auth = resolver(vision_model.clone()).await.with_context(|| {
                format!(
                    "resolving authentication for configured vision model {}/{}",
                    vision_model.provider, vision_model.id,
                )
            })?;
            stream_options.stream.api_key = Some(auth.api_key);
            merge_headers_case_insensitive(&mut stream_options.stream.headers, auth.headers);
            stream_options.stream.env.extend(auth.env);
        } else {
            stream_options.stream.api_key = Some(shared.state.read().api_key.clone());
        }
        stream_options.stream.max_tokens = Some(4096);
        stream_options.stream.cache_retention = CacheRetention::None;
        stream_options.stream.session_id = Some(Uuid::now_v7().to_string());
        stream_options.reasoning = None;
        let vision_model_id = vision_model.id.clone();
        let context = Context {
            system_prompt: VISION_DELEGATION_SYSTEM_PROMPT.to_owned(),
            messages: vec![Message::User(UserMessage {
                content: {
                    let mut blocks = images;
                    blocks.push(ContentBlock::text(
                        "Describe these images in detail, focusing on code, UI, diagrams, and technical content.",
                    ));
                    blocks
                },
                timestamp: pi_ai::now_millis(),
            })],
            tools: Vec::new(),
        };
        let stream_fn = shared.stream_fn.clone();
        let produced = tokio::time::timeout(VISION_DELEGATION_TIMEOUT, async {
            let stream = (stream_fn)(vision_model, context, stream_options).await;
            let mut stream_error = None;
            while let Some(event) = stream.next().await {
                if let AssistantMessageEvent::Error { error, .. } = event {
                    stream_error = Some(error);
                }
            }
            (stream.result().await, stream_error)
        })
        .await
        .map_err(|_| {
            anyhow!(
                "vision model {vision_model_id} timed out after {}s while analyzing image input",
                VISION_DELEGATION_TIMEOUT.as_secs(),
            )
        })?;
        let (produced, stream_error) = produced;
        if let Some(response) = stream_error {
            let detail = response
                .error_message
                .as_deref()
                .filter(|message| !message.trim().is_empty())
                .unwrap_or("provider stream failed without details");
            return Err(anyhow!(
                "vision model {vision_model_id} failed while analyzing image input: {detail}"
            ));
        }
        let response = produced.ok_or_else(|| {
            anyhow!("vision model {vision_model_id} returned no result while analyzing image input")
        })?;
        if response.stop_reason == StopReason::Error || response.stop_reason == StopReason::Aborted
        {
            let detail = response
                .error_message
                .as_deref()
                .filter(|message| !message.trim().is_empty())
                .unwrap_or("provider returned an error without details");
            return Err(anyhow!(
                "vision model {vision_model_id} failed while analyzing image input: {detail}"
            ));
        }
        let description = response
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if description.trim().is_empty() {
            return Err(anyhow!(
                "vision model {vision_model_id} returned empty text while analyzing image input"
            ));
        }

        let mut replacement = Vec::with_capacity(content.len());
        let mut description_inserted = false;
        for block in content {
            if matches!(block, ContentBlock::Image { .. }) {
                if !description_inserted {
                    replacement.push(ContentBlock::text(format!(
                        "[Image analyzed by {vision_model_id}: {description}]"
                    )));
                    description_inserted = true;
                }
            } else {
                replacement.push(block);
            }
        }
        cache.lock().insert(key, replacement.clone());
        Ok(replacement)
    }

    /// Stable in-process key for a user content block list: FNV-1a over the
    /// JSON encoding. The recorded transcript keeps the original images, so
    /// the same content reappears in every later model context; the key lets
    /// the projection reuse the one-time delegation result instead of calling
    /// the vision provider again.
    fn vision_content_key(content: &[ContentBlock]) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        if let Ok(encoded) = serde_json::to_vec(content) {
            for byte in encoded {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        hash
    }

    // Child sessions inherit the captured application generation; no per-call
    // sandbox flag or compatibility resolver is permitted.

    fn memory_config_resolver(&self) -> crate::MemoryConfigFn {
        let resources = self.inner.resources.clone();
        Arc::new(move || {
            resources
                .read()
                .as_ref()
                .map(|manager| manager.snapshot().settings.memory_config())
        })
    }

    #[must_use]
    pub fn stream_options(&self) -> SimpleStreamOptions {
        self.inner.shared.stream_options.read().clone()
    }

    pub(crate) async fn request_auth_for_model(&self, model: &Model) -> Result<RequestAuth> {
        if let Some(resolver) = &self.inner.shared.auth_resolver {
            return resolver(model.clone()).await;
        }
        let state = self.inner.shared.state.read();
        let stream = self.inner.shared.stream_options.read();
        Ok(RequestAuth {
            api_key: state.api_key.clone(),
            headers: stream.stream.headers.clone(),
            env: stream.stream.env.clone(),
            available_model_ids: None,
            kind: crate::RequestAuthKind::ApiKeyOrHeaders,
        })
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
        self.inner
            .shared
            .expose_session_environment
            .load(Ordering::Acquire)
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

    /// Current selector settings, including the auto-mode classifier knob.
    #[must_use]
    pub fn selector_settings(&self) -> crate::SelectorSettings {
        self.inner.shared.selector_settings.read().clone()
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
        *self.inner.shared.session_name.write() = normalized.clone();
        self.publish_session_event(SessionEvent::SessionInfoChanged { name: normalized });
        Ok(())
    }

    #[must_use]
    pub fn todo_state(&self) -> TodoState {
        self.inner.todo.state()
    }
    #[must_use]
    pub fn todo_snapshot_record(&self) -> TodoSnapshotRecord {
        self.inner.todo.snapshot_record()
    }
    #[must_use]
    pub fn todo_execution(&self) -> (crate::TodoDagExecutionStatus, Option<crate::TodoDagFailureReason>) {
        self.inner.todo.execution()
    }
    pub(crate) fn persist_todo_execution(
        &self,
        status: crate::TodoDagExecutionStatus,
        failure_reason: Option<crate::TodoDagFailureReason>,
    ) -> Result<()> {
        self.inner.todo.persist_execution(status, failure_reason)
    }
    #[cfg(test)]
    pub(crate) fn fail_next_todo_execution_persist_for_test(&self) {
        self.inner
            .shared
            .fail_next_todo_execution_persist
            .store(true, Ordering::Release);
    }


    pub(crate) fn restore_todo_execution(
        &self,
        status: crate::TodoDagExecutionStatus,
        failure_reason: Option<crate::TodoDagFailureReason>,
    ) {
        self.inner.todo.restore_execution(status, failure_reason)
    }
    pub(crate) fn set_todo_mutation_transaction(
        &self,
        transaction: crate::todo::TodoMutationTransaction,
    ) {
        self.inner.todo.set_mutation_transaction(transaction);
    }

    pub(crate) fn apply_todo_raw(&self, op: TodoOp) -> Result<TodoApplyResult> {
        self.inner
            .todo
            .apply_raw(op)
            .inspect_err(|_| self.schedule_todo_reminder())
    }

    pub(crate) fn set_todos_raw(&self, phases: Vec<TodoPhase>) -> Result<TodoApplyResult> {
        self.inner.todo.set_phases_raw(phases)
    }
    /// Force the canonical Todo phases back in memory without persisting.
    /// The executor uses this when the durable rollback of a failed spawn
    /// was itself rejected: the started snapshot may remain recorded, but
    /// every reader (UI counters, rows, event payloads) must still converge
    /// on the pre-spawn phases.
    pub(crate) fn restore_todos_raw(&self, phases: Vec<TodoPhase>) -> Result<()> {
        self.inner.todo.restore_phases(phases)
    }

    pub fn apply_todo(&self, op: TodoOp) -> Result<TodoApplyResult> {
        self.inner
            .todo
            .apply(op)
            .inspect_err(|_| self.schedule_todo_reminder())
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
        // Fail-closed delegation before the message is queued or recorded: an
        // image delegation error surfaces here, nothing is enqueued. The
        // ORIGINAL message (image intact) is what the queue, recorder and UI
        // carry; the delegated description is projected only into the model
        // context when the queue drains or the run starts.
        self.delegate_vision_message(message.clone()).await?;
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

    pub fn append_custom_entry(
        &self,
        custom_type: &str,
        data: Option<serde_json::Value>,
    ) -> Result<String> {
        let recorder = self.current_recorder()?;
        let id = recorder.record_custom_entry(custom_type, data)?;
        self.publish_recorded_entry(&recorder, &id)?;
        Ok(id)
    }

    /// Recorder-authoritative bounded snapshot for live collaboration guests.
    ///
    /// Reads the current recorder's full session tree (the same authoritative
    /// history the session is built from) and projects it through
    /// [`crate::collab::public_snapshot`]: the most recent entries bounded by
    /// `max_entries`/`max_bytes`, with the host filesystem path and other
    /// host-side metadata excluded. Errors are path-free and secret-free.
    pub fn collab_public_snapshot(
        &self,
        max_entries: usize,
        max_bytes: usize,
    ) -> Result<serde_json::Value> {
        let recorder = self.current_recorder()?;
        let tree = recorder.tree()?;
        crate::collab::public_snapshot(&tree, max_entries, max_bytes)
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
        self.inner
            .shared
            .recorded_count
            .fetch_add(1, Ordering::AcqRel);
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
        // Snapshot and publish under one lock (S-RACE-5): two concurrent
        // publishers can otherwise deliver an older snapshot after a fresher
        // one, resurrecting a consumed row. With the lock, the last holder
        // always re-snapshots the live queues, so the final published state
        // is the freshest.
        let _publish = self.inner.shared.queue_update.lock().await;
        let (steering, follow_up) = self.inner.agent.queued_messages().await;
        self.publish_session_event(SessionEvent::QueueUpdate {
            steering,
            follow_up,
        });
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
        self.publish_session_event(SessionEvent::QueueUpdate {
            steering: Vec::new(),
            follow_up: Vec::new(),
        });
        drained
    }

    pub async fn drain_steering_messages(&self) -> Vec<Message> {
        let drained = self.inner.agent.drain_steering_messages().await;
        self.publish_queue_update().await;
        drained
    }

    pub async fn remove_steering_at(&self, index: usize) -> Option<Message> {
        let removed = self.inner.agent.remove_steering_at(index).await;
        self.publish_queue_update().await;
        removed
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
        self.publish_resource_manager(resources, update).await;
        Ok(())
    }

    pub(crate) async fn publish_resource_manager(
        &self,
        resources: ResourceManager,
        update: SessionResourceUpdate,
    ) {
        self.commit_resource_update(update).await;
        let rules_resources = resources.clone();
        *self.inner.permission_rules.write() = Some(Arc::new(move || {
            rules_resources.snapshot().settings.permission_rules.clone()
        }));
        *self.inner.resources.write() = Some(resources);
    }

    /// Replaces the session's live permission-rule source. Orchestration
    /// children inherit the parent's source this way when they never attach a
    /// resource manager of their own, so their `lsp` rename preflight obeys
    /// the same rules as the parent's host approval.
    pub fn set_permission_rules(&self, source: Option<crate::PermissionRulesSource>) {
        *self.inner.permission_rules.write() = source;
    }

    /// The session's live path-permission-rule source (the data the host
    /// approval hook and the `lsp` tool's rename preflight consult), or `None`
    /// before resources attach / when no source was inherited.
    #[must_use]
    pub fn permission_rules_source(&self) -> Option<crate::PermissionRulesSource> {
        self.inner.permission_rules.read().clone()
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
        let mut classifier_error: Option<String> = None;
        let classifier = if settings.classifier.enabled {
            let state = self.inner.shared.state.read();
            let resolved = resolve_classifier_model(&settings, &state.model);
            drop(state);
            match resolved {
                Ok(model) => Some({
                    let (_, abort) = AbortController::new();
                    crate::ProviderClassifier {
                        model,
                        stream: self.inner.shared.stream_fn.clone(),
                        options: self.inner.shared.stream_options.read().clone(),
                        abort,
                    }
                }),
                Err(error) => {
                    classifier_error = Some(error);
                    None
                }
            }
        } else {
            None
        };
        let mut plan = crate::select(
            crate::SelectionInput {
                request,
                skills: &skills,
                agents: &agents,
                settings: &settings,
            },
            classifier.as_ref(),
        )
        .await;
        if let Some(error) = classifier_error {
            plan.fallback_reason = Some(format!("classifier model invalid: {error}"));
        }
        *self.inner.shared.last_selection.write() = Some(plan.clone());
        plan
    }

    pub(crate) fn prepare_resource_update(
        &self,
        snapshot: Arc<crate::ResourceSnapshot>,
        mut additional_tools: Vec<AgentTool>,
    ) -> Result<SessionResourceUpdate> {
        let runtime_settings = snapshot.settings.runtime_settings()?;
        if runtime_settings.process_tool_enabled
            && !additional_tools.iter().any(|tool| tool.name == "process")
            && !self
                .inner
                .base_tools
                .iter()
                .any(|tool| tool.name == "process")
        {
            additional_tools.push(process_tool(
                &self.inner.cwd,
                self.inner.process_manager.clone(),
                self.inner.process_owner_id.clone(),
            ));
        }
        if runtime_settings.todo_tool_enabled
            && !additional_tools.iter().any(|tool| tool.name == "todo")
            && !self.inner.base_tools.iter().any(|tool| tool.name == "todo")
        {
            additional_tools.push(create_todo_tool(self.inner.todo.clone()));
        }
        if runtime_settings.glob_tool_enabled
            && !additional_tools.iter().any(|tool| tool.name == "glob")
            && !self.inner.base_tools.iter().any(|tool| tool.name == "glob")
        {
            additional_tools.push(crate::create_glob_tool_for_workspace(
                self.inner.workspace.clone(),
                self.generation_capture_fn(),
            ));
        }
        if self.inner.ambient_tool_set
            && !additional_tools.iter().any(|tool| tool.name == "mcp")
            && !self.inner.base_tools.iter().any(|tool| tool.name == "mcp")
        {
            additional_tools.push(crate::mcp::mcp_tool(self.inner.mcp.clone()));
        }
        let all_tools = merge_tools(&self.inner.base_tools, additional_tools)?;
        // Memory tools follow the live `settings.memory.backend` (off/local/
        // hindsight); the base tools built before resources attach default to
        // `local`, so reconcile here and on reload.
        let all_tools = reconcile_memory_tools(
            all_tools,
            snapshot.settings.memory_config(),
            &self.inner.cwd,
            Some(self.session_env()),
            Some(self.generation_capture_fn()),
        );
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
        let tools = reconcile_memory_tools(
            tools,
            snapshot.settings.memory_config(),
            &self.inner.cwd,
            Some(self.session_env()),
            Some(self.generation_capture_fn()),
        );
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
            return Err(anyhow!(
                "cannot reload resources while session is processing"
            ));
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
        let agents = crate::enabled_agent_definitions(&snapshot.agents, &snapshot.settings.agents)
            .into_iter()
            .cloned()
            .collect();
        Ok(SessionResourceUpdate {
            tools,
            all_tools,
            system_prompt,
            skills: snapshot.skills.clone(),
            agents,
            context_files: snapshot.context_files.clone(),
            selector_settings: snapshot.settings.selector.clone().unwrap_or_default(),
            runtime_settings,
            hooks: snapshot.settings.hooks.clone().unwrap_or_default(),
            mcp_servers: snapshot.settings.mcp_servers.clone(),
        })
    }

    pub(crate) fn stage_tool_generation(
        &self,
        provenance: Arc<crate::application::generation::GenerationProvenance>,
        snapshot: Arc<crate::ResourceSnapshot>,
        additional_tools: Vec<AgentTool>,
    ) -> Result<(
        Arc<crate::application::generation::ToolGeneration>,
        SessionResourceUpdate,
    )> {
        let update = self.prepare_resource_update(snapshot, additional_tools)?;
        let all_tools = update
            .all_tools
            .clone()
            .unwrap_or_else(|| update.tools.clone());
        let generation = crate::application::generation::ToolGeneration::new(
            provenance,
            update.tools.clone(),
            all_tools,
            update.system_prompt.clone(),
            update.skills.clone(),
            update.agents.clone(),
            update.context_files.clone(),
            update.selector_settings.clone(),
        )?;
        Ok((generation, update))
    }

    async fn commit_runtime_settings(&self, settings: crate::RuntimeSettingsSnapshot) {
        self.inner
            .agent
            .set_steering_mode(settings.steering_mode)
            .await;
        self.inner
            .agent
            .set_follow_up_mode(settings.follow_up_mode)
            .await;
        self.set_retry_settings(settings.retry);
        self.enable_compaction(settings.compaction);
        let desired = settings.stream_options;
        let mut stream_options = self.stream_options();
        stream_options.stream.temperature = desired.stream.temperature;
        stream_options.stream.max_tokens = desired.stream.max_tokens;
        stream_options.stream.transport = desired.stream.transport;
        stream_options.stream.cache_retention = desired.stream.cache_retention;
        stream_options.stream.timeout_ms = desired.stream.timeout_ms;
        stream_options.stream.max_retries = desired.stream.max_retries;
        stream_options.stream.max_retry_delay_ms = desired.stream.max_retry_delay_ms;
        stream_options.reasoning = desired.reasoning;
        stream_options.thinking_budgets = desired.thinking_budgets;
        stream_options.responses_stateful_chain = desired.responses_stateful_chain;
        self.set_stream_options(stream_options).await;
        *self.inner.shared.branch_summary.write() = settings.branch_summary;
        *self.inner.shared.vision_model.write() = settings.vision_model;
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
        *self.inner.context_files.write() = update.context_files;
        *self.inner.shared.selector_agents.write() = update.agents;
        *self.inner.shared.selector_settings.write() = update.selector_settings;
        self.set_host_hooks(if update.hooks.is_empty() {
            None
        } else {
            Some(update.hooks)
        });
        self.inner.mcp.configure(update.mcp_servers);
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
        let event_model = model.clone();
        let change = self.set_model_internal(model, api_key);
        self.publish_session_event(SessionEvent::ModelSelect { model: event_model });
        if change.clamped {
            self.publish_session_event(SessionEvent::ThinkingLevelSelect {
                thinking_level: change.effective,
            });
        }
        change
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
            format_retry_fallback_selector(&model, Some(thinking_level_name(self.thinking_level())))
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
        self.publish_session_event(SessionEvent::ThinkingLevelSelect {
            thinking_level: change.effective,
        });
        change
    }

    pub async fn load_history(&self, messages: Vec<Message>) -> Result<()> {
        let mut guard = self.claim_exclusive()?;
        // Drain-and-requeue: replacing the transcript must not silently
        // consume acknowledged steering. The queued messages return to the
        // (now-empty) queue head after the history swap; their journal markers
        // stay registered and are flipped when the messages are delivered.
        let (queued_steering, queued_follow_up) =
            self.inner.agent.drain_queued_messages().await;
        self.inner.agent.set_messages(messages.clone()).await;
        self.inner.agent.clear_all_queues().await;
        for message in queued_steering {
            self.inner.agent.steer(message).await;
        }
        for message in queued_follow_up {
            self.inner.agent.follow_up(message).await;
        }
        self.publish_queue_update().await;
        self.inner.shared.state.write().messages = messages;
        // The transcript was replaced wholesale; a stored Responses chain id
        // would reference a conversation that no longer matches it.
        pi_ai::providers::reset_responses_chain(&self.inner.shared.session_id);
        *self.inner.shared.compaction_runtime.lock() = CompactionRuntime::default();
        self.inner
            .shared
            .compaction_active
            .store(false, Ordering::Release);
        guard.release();
        Ok(())
    }

    pub fn history(&self) -> Vec<Message> {
        self.inner.shared.state.read().messages.clone()
    }

    /// Captures transcript and the count of agent events published from the
    /// same lock acquisition, so a completed message cannot appear without
    /// the snapshot also requiring its queued ApplicationEvent to be drained.
    #[must_use]
    pub fn protocol_history(&self) -> (Vec<Message>, u64) {
        let state = self.inner.shared.state.read();
        (state.messages.clone(), state.agent_event_barrier)
    }

    #[must_use]
    pub fn agent_event_barrier(&self) -> u64 {
        self.inner.shared.state.read().agent_event_barrier
    }

    pub async fn reset(&self) -> Result<()> {
        let mut guard = self.claim_exclusive()?;
        self.inner.agent.reset().await;
        self.inner.shared.state.write().messages.clear();
        // The conversation is discarded; a stored Responses chain id
        // would reference a conversation that no longer matches it.
        pi_ai::providers::reset_responses_chain(&self.inner.shared.session_id);
        *self.inner.shared.compaction_runtime.lock() = CompactionRuntime::default();
        self.inner
            .shared
            .compaction_active
            .store(false, Ordering::Release);
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
        let result = self
            .perform_compaction(CompactionReason::Manual, false, custom_instructions, None)
            .await;
        guard.release();
        result
    }

    /// Deterministic context archive with no provider call: all turns except
    /// the last `settings.snap_keep_turns` are replaced by a compacted summary
    /// block built from message statistics ([`build_snapcompact_summary`]),
    /// and the original entries are preserved in a
    /// `<session-file>.snapcompact-<rfc3339-millis>.jsonl` sidecar next to the
    /// session file (mirroring the rewind archive convention). The
    /// before-compaction extension hook is intentionally skipped: this path
    /// must stay offline and deterministic.
    pub async fn compact_snap(&self) -> Result<CompactionResult> {
        let mut guard = self.claim_exclusive()?;
        let result = self.perform_snap_compaction(CompactionReason::Manual).await;
        guard.release();
        result
    }

    async fn perform_snap_compaction(&self, reason: CompactionReason) -> Result<CompactionResult> {
        self.publish_session_event(SessionEvent::CompactionStart { reason });
        let _activity = CompactionActivityGuard::begin(self.inner.shared.clone());
        let result = self.generate_snap_compaction().await;
        match &result {
            Ok(compaction) => self.publish_session_event(SessionEvent::CompactionEnd {
                reason,
                result: Some(compaction.clone()),
                aborted: false,
                will_retry: false,
                error_message: None,
            }),
            Err(error) => self.publish_session_event(SessionEvent::CompactionEnd {
                reason,
                result: None,
                aborted: false,
                will_retry: false,
                error_message: Some(error.to_string()),
            }),
        }
        result
    }

    /// Builds the deterministic handoff envelope (goal, todo counts, active
    /// jobs, environment, recent asks, next-step hints). No model call.
    ///
    /// `jobs` are the orchestration job snapshots (usually
    /// `OrchestrationRuntime::jobs(None)`); only queued and running jobs are
    /// retained. The envelope is always well-formed, including for an empty
    /// session.
    pub fn generate_handoff(&self, jobs: &[JobSnapshot]) -> Result<Handoff> {
        let generation = self.capture_application_generation()?;
        Ok(Handoff {
            envelope: crate::handoff::handoff_envelope_with_generation(generation, self, jobs),
            prose: None,
        })
    }

    /// Envelope plus a prose handoff paragraph from the existing summarization
    /// path — a single bounded provider call with no retries
    /// ([`HANDOFF_SUMMARIZE_TIMEOUT`]). The envelope itself is deterministic;
    /// only the prose paragraph is model-generated.
    pub async fn generate_handoff_with_prose(&self, jobs: &[JobSnapshot]) -> Result<Handoff> {
        let generation = self.capture_application_generation()?;
        let envelope = crate::handoff::handoff_envelope_with_generation(generation, self, jobs);
        let transcript = {
            let state = self.inner.shared.state.read();
            serialize_conversation(&messages_as_llm(&state.messages))
        };
        let prompt = handoff_prose_prompt(&envelope, &transcript);
        let (_, abort) = AbortController::new();
        let prose = match run_summary_provider_call(
            &self.inner.shared,
            &prompt,
            HANDOFF_SYSTEM_PROMPT,
            HANDOFF_PROSE_RESERVE_TOKENS,
            1.0,
            abort,
            HANDOFF_SUMMARIZE_TIMEOUT,
        )
        .await
        {
            SummaryAttemptOutcome::Done(text) => text,
            SummaryAttemptOutcome::Cancelled => return Err(anyhow!("Handoff cancelled")),
            SummaryAttemptOutcome::Failed { message, .. } => return Err(anyhow!(message)),
        };
        Ok(Handoff {
            envelope,
            prose: Some(prose),
        })
    }

    async fn perform_compaction(
        &self,
        reason: CompactionReason,
        will_retry: bool,
        custom_instructions: Option<&str>,
        summarize_timeout: Option<std::time::Duration>,
    ) -> Result<CompactionResult> {
        if let Some(hook) = self.before_compaction() {
            let reduction = hook(BeforeCompactionContext {
                reason,
                will_retry,
                custom_instructions: custom_instructions.map(str::to_owned),
            })
            .await?;
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
        let result = self
            .generate_compaction(
                reason,
                custom_instructions,
                abort.clone(),
                summarize_timeout,
            )
            .await;
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
            return Err(anyhow!(
                "extension compaction firstKeptEntryId is outside the live context"
            ));
        }
        let compacted = apply_checkpoint(&result.summary, &messages, first_kept_index);
        let details = result
            .details
            .as_ref()
            .map(serde_json::to_value)
            .transpose()?;
        recorder.record_compaction_metadata(
            &result.summary,
            Some(&result.first_kept_entry_id),
            result.tokens_before,
            &messages[first_kept_index..],
            details.as_ref(),
            result.usage.as_ref(),
            Some(true),
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
        // Compaction replaced the transcript; the stored Responses chain id no
        // longer matches it, so the next turn sends full history again.
        pi_ai::providers::reset_responses_chain(&self.inner.shared.session_id);
        Ok(())
    }

    async fn generate_compaction(
        &self,
        reason: CompactionReason,
        custom_instructions: Option<&str>,
        abort: AbortSignal,
        summarize_timeout: Option<std::time::Duration>,
    ) -> Result<CompactionResult> {
        let summarize_timeout = summarize_timeout.unwrap_or(COMPACTION_SUMMARIZE_TIMEOUT);
        let messages = self.inner.agent.state().await.messages;
        let settings = self
            .inner
            .shared
            .compaction
            .read()
            .unwrap_or(crate::DEFAULT_COMPACTION_SETTINGS);
        let (prefix_len, previous_summary, mut read_files, mut modified_files) = {
            let state = self.inner.shared.compaction_runtime.lock();
            (
                state.prefix_len.min(messages.len()),
                state.summary.clone(),
                state.read_files.clone(),
                state.modified_files.clone(),
            )
        };
        let current = if previous_summary.is_empty() {
            messages.clone()
        } else {
            apply_checkpoint(&previous_summary, &messages, prefix_len)
        };
        let tokens_before = estimate_context_tokens_usage_aware(&current);
        let cut = find_cut_point(
            &messages,
            prefix_len,
            messages.len(),
            settings.keep_recent_tokens,
        );
        if cut.first_kept_index <= prefix_len || cut.first_kept_index >= messages.len() {
            return Err(anyhow!("Nothing to compact (session too small)"));
        }
        let history_end = if cut.is_split_turn {
            cut.turn_start_index.unwrap_or(cut.first_kept_index)
        } else {
            cut.first_kept_index
        };
        let history = &messages[prefix_len..history_end];
        let turn_prefix = if cut.is_split_turn {
            &messages[history_end..cut.first_kept_index]
        } else {
            &[]
        };
        // Useless-result elision applies to every compaction (LLM and snap):
        // empty/whitespace results and exact duplicates of the preceding tool
        // call's error text never reach the summarizer, and the summary notes
        // how many were dropped.
        let (history_elided, history_elided_count) = elide_useless_results(history);
        let (turn_prefix_elided, turn_prefix_elided_count) = elide_useless_results(turn_prefix);
        let elided_count = history_elided_count + turn_prefix_elided_count;
        let mut summary = if turn_prefix.is_empty() {
            summarize_messages(
                &self.inner.shared,
                &history_elided,
                &previous_summary,
                custom_instructions,
                settings.reserve_tokens,
                0.8,
                abort.clone(),
                Some(reason),
                summarize_timeout,
            )
            .await?
        } else {
            let history_summary = if history_elided.is_empty() {
                "No prior history.".to_owned()
            } else {
                summarize_messages(
                    &self.inner.shared,
                    &history_elided,
                    &previous_summary,
                    custom_instructions,
                    settings.reserve_tokens,
                    0.8,
                    abort.clone(),
                    Some(reason),
                    summarize_timeout,
                )
                .await?
            };
            let prefix_summary = summarize_turn_prefix(
                &self.inner.shared,
                &turn_prefix_elided,
                settings.reserve_tokens,
                abort.clone(),
                reason,
                summarize_timeout,
            )
            .await?;
            format!(
                "{history_summary}\n\n---\n\n**Turn Context (split turn):**\n\n{prefix_summary}"
            )
        };
        if abort.is_aborted() {
            return Err(anyhow!("Compaction cancelled"));
        }
        let (new_read_files, new_modified_files) =
            compute_file_lists(&messages[prefix_len..cut.first_kept_index]);
        for path in new_modified_files {
            read_files.remove(&path);
            modified_files.insert(path);
        }
        for path in new_read_files {
            if !modified_files.contains(&path) {
                read_files.insert(path);
            }
        }
        let all_read_files = read_files.iter().cloned().collect::<Vec<_>>();
        let all_modified_files = modified_files.iter().cloned().collect::<Vec<_>>();
        summary.push_str(&format_file_operations(
            &all_read_files,
            &all_modified_files,
        ));
        summary.push_str(&elided_note(elided_count));
        let recorder = self.inner.shared.recorder.lock().as_ref().cloned();
        let first_kept_entry_id = recorder
            .as_ref()
            .and_then(|recorder| recorder.tree().ok())
            .and_then(|tree| {
                tree.branch(None)
                    .into_iter()
                    .filter(|entry| entry.entry_type == "message")
                    .nth(cut.first_kept_index)
                    .map(|entry| entry.id.clone())
            })
            .unwrap_or_else(|| Uuid::now_v7().to_string());
        let compacted = apply_checkpoint(&summary, &messages, cut.first_kept_index);
        // Post-compaction context: the summary plus the verbatim kept tail. The
        // usage-aware estimator must NOT be used here — it anchors on the last
        // assistant turn's real usage, measured against the FULL pre-compaction
        // context, and that turn plus its tail survive the cut, so it would
        // report `tokens_before` unchanged. No assistant turn has run against
        // the new context yet, so the pure heuristic (summary + kept tail) is
        // the honest estimate.
        let estimated_tokens_after = estimate_context_tokens(&compacted);
        if abort.is_aborted() {
            return Err(anyhow!("Compaction cancelled"));
        }
        if let Some(recorder) = recorder {
            let details = serde_json::to_value(CompactionDetails {
                read_files: all_read_files.clone(),
                modified_files: all_modified_files.clone(),
            })?;
            recorder.record_compaction_metadata(
                &summary,
                Some(&first_kept_entry_id),
                tokens_before,
                &messages[cut.first_kept_index..],
                Some(&details),
                None,
                None,
            )?;
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
        // Compaction replaced the transcript; the stored Responses chain id no
        // longer matches it, so the next turn sends full history again.
        pi_ai::providers::reset_responses_chain(&self.inner.shared.session_id);
        Ok(CompactionResult {
            summary,
            first_kept_entry_id,
            tokens_before,
            estimated_tokens_after: Some(estimated_tokens_after),
            usage: None,
            details: Some(CompactionDetails {
                read_files: all_read_files,
                modified_files: all_modified_files,
            }),
        })
    }

    async fn generate_snap_compaction(&self) -> Result<CompactionResult> {
        let messages = self.inner.agent.state().await.messages;
        let settings = self
            .inner
            .shared
            .compaction
            .read()
            .unwrap_or(crate::DEFAULT_COMPACTION_SETTINGS);
        let (prefix_len, previous_summary, mut read_files, mut modified_files) = {
            let state = self.inner.shared.compaction_runtime.lock();
            (
                state.prefix_len.min(messages.len()),
                state.summary.clone(),
                state.read_files.clone(),
                state.modified_files.clone(),
            )
        };
        let current = if previous_summary.is_empty() {
            messages.clone()
        } else {
            apply_checkpoint(&previous_summary, &messages, prefix_len)
        };
        let tokens_before = estimate_context_tokens_usage_aware(&current);
        let keep_turns = settings.snap_keep_turns.max(1);
        let Some(cut) = find_snap_cut_point(&messages, prefix_len, keep_turns) else {
            return Err(anyhow!("Nothing to compact (session too small)"));
        };
        let history = &messages[prefix_len..cut];
        let (history_elided, elided_count) = elide_useless_results(history);
        let mut summary = build_snapcompact_summary(&history_elided, elided_count);
        let (new_read_files, new_modified_files) = compute_file_lists(history);
        for path in new_modified_files {
            read_files.remove(&path);
            modified_files.insert(path);
        }
        for path in new_read_files {
            if !modified_files.contains(&path) {
                read_files.insert(path);
            }
        }
        let all_read_files = read_files.iter().cloned().collect::<Vec<_>>();
        let all_modified_files = modified_files.iter().cloned().collect::<Vec<_>>();
        summary.push_str(&format_file_operations(
            &all_read_files,
            &all_modified_files,
        ));
        summary.push_str(&elided_note(elided_count));
        let recorder = self.inner.shared.recorder.lock().as_ref().cloned();
        let first_kept_entry_id = recorder
            .as_ref()
            .and_then(|recorder| recorder.tree().ok())
            .and_then(|tree| {
                tree.branch(None)
                    .into_iter()
                    .filter(|entry| entry.entry_type == "message")
                    .nth(cut)
                    .map(|entry| entry.id.clone())
            })
            .unwrap_or_else(|| Uuid::now_v7().to_string());
        let compacted = apply_checkpoint(&summary, &messages, cut);
        // Same as the LLM path: estimate the summary + kept tail heuristically.
        // The usage-aware estimator anchors on the last assistant turn's usage
        // (measured against the full pre-compaction context), which survives
        // the cut, so it would report `tokens_before` unchanged.
        let estimated_tokens_after = estimate_context_tokens(&compacted);
        if let Some(recorder) = &recorder {
            let details = serde_json::to_value(CompactionDetails {
                read_files: all_read_files.clone(),
                modified_files: all_modified_files.clone(),
            })?;
            // Lossless sidecar archive FIRST: the original replaced entries
            // (before elision) are preserved as plain JSONL records next to the
            // session file so nothing is destroyed by the deterministic
            // archive. The archive is created and fsynced before the compaction
            // record is appended: a committed compaction record must never
            // exist without its sidecar, because resume would otherwise
            // reconstruct summarized context with no recoverable original.
            let tree = recorder.tree()?;
            let entries = tree
                .branch(None)
                .into_iter()
                .filter(|entry| entry.entry_type == "message")
                .skip(prefix_len)
                .take(cut - prefix_len)
                .collect::<Vec<_>>();
            let archive_path = write_snapcompact_archive(&recorder.path(), &entries)?;
            if let Err(error) = recorder.record_compaction_metadata(
                &summary,
                Some(&first_kept_entry_id),
                tokens_before,
                &messages[cut..],
                Some(&details),
                None,
                None,
            ) {
                // The archive exists but no compaction record commits it:
                // remove the orphan (best-effort — without a record it is
                // inert) so a later successful compaction starts clean.
                let _ = std::fs::remove_file(&archive_path);
                return Err(error);
            }
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
        // Compaction replaced the transcript; the stored Responses chain id no
        // longer matches it, so the next turn sends full history again.
        pi_ai::providers::reset_responses_chain(&self.inner.shared.session_id);
        Ok(CompactionResult {
            summary,
            first_kept_entry_id,
            tokens_before,
            estimated_tokens_after: Some(estimated_tokens_after),
            usage: None,
            details: Some(CompactionDetails {
                read_files: all_read_files,
                modified_files: all_modified_files,
            }),
        })
    }

    pub fn abort_compaction(&self) {
        if let Some(controller) = self.inner.shared.compaction_controller.lock().as_ref() {
            controller.abort();
        }
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
        let mut guard = self.claim_exclusive()?;
        recorder.activate_writer()?;
        let (goal, snapshot) = self.recording_state(&recorder)?;
        self.attach_recording(recorder, goal, snapshot)?;
        guard.release();
        Ok(())
    }

    pub async fn resume_recording(
        &self,
        recorder: SessionRecorder,
        messages: Vec<Message>,
    ) -> Result<()> {
        self.resume_recording_with_policy(recorder, messages, None, None, None, None)
            .await
    }


    /// Attaches `recorder` as the session's recording, optionally restoring the
    /// model, API key, and thinking level recorded at the materialized cut
    /// alongside the messages. The optional policy is applied to both the agent
    /// and the shared state without writing new journal entries, so a fork's
    /// runtime authority matches the cut exactly.
    async fn resume_recording_with_policy(
        &self,
        recorder: SessionRecorder,
        messages: Vec<Message>,
        unpublished_fork: Option<UnpublishedFork>,
        model: Option<Model>,
        api_key: Option<String>,
        thinking_level: Option<ThinkingLevel>,
    ) -> Result<()> {
        let guard = self.claim_exclusive()?;
        recorder.activate_writer()?;
        let (goal, snapshot) = self.recording_state(&recorder)?;
        #[cfg(test)]
        let before_install_gate = self.inner.fork_before_install_gate.lock().take();
        #[cfg(test)]
        if let Some(gate) = before_install_gate {
            gate.enter().await;
        }
        let session = self.clone();
        tokio::spawn(async move {
            let mut guard = guard;
            let mut unpublished_fork = unpublished_fork;
            // The transcript-replacing install must not silently consume
            // acknowledged steering: drain the old incarnation's queues and
            // requeue them after the recording is attached (deduped against
            // the journal's still-undelivered markers by the requeue step).
            let (drained_steering, drained_follow_up) =
                session.inner.agent.drain_queued_messages().await;
            session.inner.agent.set_messages(messages.clone()).await;
            session.inner.agent.clear_all_queues().await;
            if let Some(model) = model.clone() {
                session.inner.agent.set_model(model).await;
            }
            if let Some(thinking_level) = thinking_level {
                session
                    .inner
                    .agent
                    .set_thinking_level(thinking_level)
                    .await;
            }
            {
                let mut state = session.inner.shared.state.write();
                state.messages = messages;
                if let Some(model) = model {
                    state.model = model;
                }
                if let Some(api_key) = api_key {
                    state.api_key = api_key;
                }
                if let Some(thinking_level) = thinking_level {
                    state.thinking_level = thinking_level;
                }
            }
            pi_ai::providers::reset_responses_chain(&session.inner.shared.session_id);
            *session.inner.shared.compaction_runtime.lock() = CompactionRuntime::default();
            session
                .inner
                .shared
                .compaction_active
                .store(false, Ordering::Release);
            let requeue_recorder = recorder.clone();
            session.attach_recording(recorder, goal, snapshot)?;
            // Requeue crash-lost and drained steering into the freshly
            // attached recording before the install becomes observable.
            session
                .requeue_undelivered_steering(&requeue_recorder, drained_steering, drained_follow_up)
                .await?;
            if let Some(unpublished_fork) = unpublished_fork.as_mut() {
                unpublished_fork.disarm();
            }
            #[cfg(test)]
            let after_install_gate = session.inner.fork_after_install_gate.lock().take();
            #[cfg(test)]
            if let Some(gate) = after_install_gate {
                gate.enter().await;
            }
            guard.release();
            Ok(())
        })
        .await
        .map_err(|error| anyhow!("session recording install task failed: {error}"))?
    }

    fn recording_state(
        &self,
        recorder: &SessionRecorder,
    ) -> Result<(GoalRuntime, TodoSnapshotRecord)> {
        let goal = GoalRuntime::from_session_recorder(recorder.clone())
            .map_err(|error| anyhow!(error.to_string()))?;
        let snapshot = recorder
            .latest_todo_snapshot()?
            .unwrap_or_else(|| {
                TodoSnapshotRecord::from_state(crate::TodoState {
                    phases: Vec::new(),
                    storage: crate::TodoStorage::Session,
                })
            });
        let validator = TodoRuntime::memory();
        validator.restore_snapshot(snapshot.clone())?;
        Ok((goal, snapshot))
    }

    fn attach_recording(
        &self,
        recorder: SessionRecorder,
        goal: GoalRuntime,
        snapshot: TodoSnapshotRecord,
    ) -> Result<()> {
        self.inner.todo.restore_snapshot(snapshot)?;
        *self.inner.shared.session_name.write() = recorder.session_name();
        *self.inner.shared.recorder.lock() = Some(Arc::new(recorder));
        *self.inner.shared.goal.write() = goal;
        Ok(())
    }

    /// Creates an independent retained branch recorder from the session's
    /// currently open recording. The source path is never reopened.
    pub fn create_branched_recorder(
        &self,
        leaf_id: &str,
        session_dir: Option<&Path>,
    ) -> Result<SessionRecorder> {
        let recorder = self.current_recorder()?;
        crate::session_store::create_branched_session_from_recorder(
            recorder.as_ref(),
            leaf_id,
            session_dir,
        )
    }

    fn prepare_recorder_replacement(
        &self,
        recorder: SessionRecorder,
        messages: Vec<Message>,
        model: Option<Model>,
        api_key: Option<String>,
        thinking_level: ThinkingLevel,
    ) -> Result<PreparedSessionReplacement> {
        self.prepare_recorder_replacement_with_fork(
            recorder,
            messages,
            model,
            api_key,
            thinking_level,
            None,
            true,
        )
    }

    fn prepare_recorder_replacement_with_fork(
        &self,
        recorder: SessionRecorder,
        messages: Vec<Message>,
        model: Option<Model>,
        api_key: Option<String>,
        thinking_level: ThinkingLevel,
        unpublished_fork: Option<UnpublishedFork>,
        migrate_queues: bool,
    ) -> Result<PreparedSessionReplacement> {
        let session_name = recorder.session_name();
        let todo_snapshot = recorder.latest_todo_snapshot()?.unwrap_or_else(|| {
            TodoSnapshotRecord::from_state(crate::TodoState {
                phases: Vec::new(),
                storage: crate::TodoStorage::Session,
            })
        });
        let goal = GoalRuntime::from_session_recorder(recorder.clone())
            .map_err(|error| anyhow!(error.to_string()))?;
        let validator = TodoRuntime::memory();
        validator.restore_snapshot(todo_snapshot.clone())?;
        Ok(PreparedSessionReplacement {
            recorder,
            messages,
            model,
            api_key,
            thinking_level,
            session_name,
            todo_snapshot,
            goal,
            unpublished_fork,
            migrate_queues,
        })
    }

    /// Persists the retained current recording without reopening its path.
    pub fn persist_recording_now(&self) -> Result<()> {
        self.current_recorder()?.persist_now()
    }

    /// Persists and fsyncs the retained current recording without reopening
    /// its path.
    pub fn persist_recording_now_durable(&self) -> Result<()> {
        self.current_recorder()?.persist_now_durable()
    }

    /// Delete the current recorder's unpublished durable journal by retained
    /// file identity. The session must not be reused after this rollback.
    pub fn discard_persisted_recording(&self) -> Result<()> {
        self.current_recorder()?.discard_persisted()
    }

    pub(crate) fn prepare_new_session_replacement(
        &self,
        parent_session: Option<&Path>,
    ) -> Result<PreparedSessionReplacement> {
        let state = self.inner.shared.state.read();
        let session_dir = self.inner.session_dir.read().clone();
        let recorder = crate::start_session_in(
            &self.inner.cwd,
            Some(&state.model),
            Some(thinking_level_name(state.thinking_level)),
            Some(&session_dir),
            None,
            parent_session,
        )?;
        let thinking_level = state.thinking_level;
        drop(state);
        recorder.persist_now()?;
        recorder.activate_writer()?;
        self.prepare_recorder_replacement_with_fork(
            recorder,
            Vec::new(),
            None,
            None,
            thinking_level,
            None,
            false,
        )
    }

    pub(crate) async fn prepare_resume_replacement(
        &self,
        prepared: crate::PreparedSessionResume,
    ) -> Result<PreparedSessionReplacement> {
        if !prepared.target_cwd().as_os_str().is_empty() && prepared.target_cwd() != self.inner.cwd
        {
            return Err(anyhow!(
                "session working directory {} does not match {}",
                prepared.target_cwd().display(),
                self.inner.cwd.display()
            ));
        }
        let context = prepared.build_context();
        let (model, api_key, thinking_level) =
            recorded_cut_policy(&self.inner.shared, &context).await?;
        let recorder = prepared.into_recorder()?;
        // Activate the durable append writer while the cutover can still fail
        // closed, exactly like the fork path: an unactivated recorder defers
        // every append (thinking level, session name, model change) to the
        // in-memory pending queue until the first assistant message, so a
        // later set_thinking_level never reaches the journal.
        recorder.activate_writer()?;
        self.prepare_recorder_replacement(
            recorder,
            context.messages,
            model,
            api_key,
            thinking_level,
        )
    }

    pub(crate) async fn prepare_fork_replacement(
        &self,
        entry_id: &str,
        restore_conversation: bool,
    ) -> Result<(PreparedSessionReplacement, String)> {
        let session_dir = self.inner.session_dir.read().clone();
        let MaterializedFork {
            recorder: replacement,
            editor_text,
        } = crate::session_store::fork_live_session_at_user(
            self.current_recorder()?.as_ref(),
            entry_id,
            Some(&session_dir),
        )?;
        let mut unpublished_fork = UnpublishedFork::new(&replacement);
        let result = (|| async {
            #[cfg(test)]
            if self
                .inner
                .fail_fork_prepare_after_materialize
                .swap(false, Ordering::AcqRel)
            {
                return Err(anyhow!("injected fork prepare failure after materialization"));
            }
            // Activate and validate the child's durable append writer while the
            // failure path can still remove the materialized child, so journal
            // publication happens through a verified writer and the cutover
            // fails closed exactly like the direct resume path.
            replacement.activate_writer()?;
            let context = replacement.tree()?.build_context(None);
            let (model, api_key, thinking_level) =
                recorded_cut_policy(&self.inner.shared, &context).await?;
            let messages = if restore_conversation {
                context.messages
            } else {
                Vec::new()
            };
            self.prepare_recorder_replacement_with_fork(
                replacement.clone(),
                messages,
                model,
                api_key,
                thinking_level,
                None,
                true,
            )
        })()
        .await;
        match result {
            Ok(mut prepared) => {
                prepared.unpublished_fork = Some(unpublished_fork);
                Ok((prepared, editor_text))
            }
            Err(error) => Err(unpublished_fork.rollback(error)),
        }
    }


    pub(crate) fn prepare_clone_replacement(
        &self,
        leaf_id: &str,
        restore_conversation: bool,
    ) -> Result<PreparedSessionReplacement> {
        let recorder = self.current_recorder()?;
        anyhow::ensure!(
            recorder
                .tree()?
                .entries
                .iter()
                .any(|entry| entry.id == leaf_id),
            "Cannot clone session: current entry is unavailable"
        );
        let session_dir = self.inner.session_dir.read().clone();
        let replacement = crate::session_store::create_branched_session_from_recorder(
            &recorder,
            leaf_id,
            Some(&session_dir),
        )?;
        replacement.persist_now()?;
        replacement.activate_writer()?;
        let messages = if restore_conversation {
            replacement.tree()?.build_context(None).messages
        } else {
            Vec::new()
        };
        let thinking_level = self.thinking_level();
        self.prepare_recorder_replacement(replacement, messages, None, None, thinking_level)
    }

    /// Reserve the outgoing logical session's MCP admission and live slots
    /// without mutating them. Dropping the returned wrapper leaves the old
    /// clients and their admission state unchanged.
    pub(crate) async fn prepare_session_replacement_commit(
        &self,
        replacement: PreparedSessionReplacement,
    ) -> PreparedSessionReplacementCommit {
        let mcp = self.inner.mcp.prepare_reset_live_sessions().await;
        PreparedSessionReplacementCommit { replacement, mcp }
    }

    /// Install a replacement whose fallible commit gate has completed.
    /// Every awaited agent mutation completes before recorder authority is
    /// synchronously published and the unpublished-fork guard is disarmed.
    pub(crate) async fn install_session_replacement(
        &self,
        replacement: PreparedSessionReplacement,
    ) {
        let PreparedSessionReplacement {
            recorder,
            messages,
            model,
            api_key,
            thinking_level,
            session_name,
            todo_snapshot,
            goal,
            mut unpublished_fork,
            migrate_queues,
        } = replacement;
        #[cfg(test)]
        let before_install_gate = self.inner.fork_before_install_gate.lock().take();
        #[cfg(test)]
        if let Some(gate) = before_install_gate {
            gate.enter().await;
        }
        self.cleanup_bash_spills();
        // Drain-and-requeue: the replacement swaps the transcript AND the
        // recording, so the old incarnation's acknowledged steering must
        // migrate. Drain before the wipe; after the replacement recorder is
        // installed, retire the old journal markers (best-effort: the old
        // recording may be closing) and requeue the messages through
        // steer/follow_up so they are journaled fresh into the replacement.
        let (queued_steering, queued_follow_up) =
            self.inner.agent.drain_queued_messages().await;
        self.inner.agent.set_messages(messages.clone()).await;
        self.inner.agent.clear_all_queues().await;
        if let Some(model) = model.clone() {
            self.inner.agent.set_model(model).await;
        }
        self.inner.agent.set_thinking_level(thinking_level).await;
        {
            let mut state = self.inner.shared.state.write();
            state.messages = messages;
            if let Some(model) = model {
                state.model = model;
            }
            if let Some(api_key) = api_key {
                state.api_key = api_key;
            }
            state.thinking_level = thinking_level;
        }
        pi_ai::providers::reset_responses_chain(&self.inner.shared.session_id);
        *self.inner.shared.compaction_runtime.lock() = CompactionRuntime::default();
        self.inner
            .shared
            .compaction_active
            .store(false, Ordering::Release);
        *self.inner.shared.session_name.write() = session_name;
        self.inner
            .todo
            .restore_snapshot(todo_snapshot)
            .expect("prepared todo state remains valid");
        *self.inner.shared.goal.write() = goal;
        let retired_markers: Vec<SteeringJournalMarker> = {
            let mut markers = self.inner.shared.pending_steering_markers.lock();
            std::mem::take(&mut *markers)
        };
        let old_recorder = self.inner.shared.recorder.lock().as_ref().cloned();
        *self.inner.shared.recorder.lock() = Some(Arc::new(recorder));
        // Retire the migrated markers against the OLD journal: they reference
        // entries there, and a later resume of that file must not requeue
        // messages that moved into the replacement. Best-effort — a failing
        // old writer only leaves stale undelivered markers behind.
        if let Some(old_recorder) = old_recorder {
            for marker in &retired_markers {
                let _ = old_recorder.record_custom_entry(
                    STEERING_DELIVERED_CUSTOM_TYPE,
                    Some(serde_json::json!({
                        "markerId": marker.marker_id,
                        "reason": "replaced",
                    })),
                );
            }
        }
        // Requeue into fork, resume, and branch replacements. A fresh session
        // drops the drained queue so `/new` does not carry the previous tray.
        if migrate_queues {
            let mut requeue_failures = 0usize;
            for message in queued_steering {
                if self.steer(message).await.is_err() {
                    requeue_failures += 1;
                }
            }
            for message in queued_follow_up {
                if self.follow_up(message).await.is_err() {
                    requeue_failures += 1;
                }
            }
            if requeue_failures > 0 {
                eprintln!(
                    "session_steering category=replacement_reenqueue_failed count={requeue_failures}"
                );
            }
        }
        self.publish_queue_update().await;
        if let Some(unpublished_fork) = unpublished_fork.as_mut() {
            unpublished_fork.disarm();
        }
        #[cfg(test)]
        let after_install_gate = self.inner.fork_after_install_gate.lock().take();
        #[cfg(test)]
        if let Some(gate) = after_install_gate {
            gate.enter().await;
        }
    }

    pub(crate) async fn commit_session_replacement(
        &self,
        replacement: PreparedSessionReplacement,
    ) -> Result<()> {
        let replacement = self
            .prepare_session_replacement_commit(replacement)
            .await
            .commit_mcp_reset()
            .await;
        self.install_session_replacement(replacement).await;
        Ok(())
    }

    pub fn start_new_recording(&self) -> Result<()> {
        let state = self.inner.shared.state.read();
        let session_dir = self.inner.session_dir.read().clone();
        let recorder = crate::start_session_in(
            &self.inner.cwd,
            Some(&state.model),
            Some(thinking_level_name(state.thinking_level)),
            Some(&session_dir),
            None,
            None,
        )?;
        drop(state);
        self.record(recorder)
    }

    pub fn start_new_recording_with_kind(&self, session_kind: SessionKind) -> Result<()> {
        let state = self.inner.shared.state.read();
        let session_dir = self.inner.session_dir.read().clone();
        let recorder = crate::start_session_in_with_kind(
            &self.inner.cwd,
            Some(&state.model),
            Some(thinking_level_name(state.thinking_level)),
            Some(&session_dir),
            None,
            None,
            Some(session_kind),
        )?;
        drop(state);
        self.record(recorder)
    }

    pub fn start_new_recording_with_parent(&self, parent_session: Option<&Path>) -> Result<()> {
        let state = self.inner.shared.state.read();
        let session_dir = self.inner.session_dir.read().clone();
        let recorder = crate::start_session_in(
            &self.inner.cwd,
            Some(&state.model),
            Some(thinking_level_name(state.thinking_level)),
            Some(&session_dir),
            None,
            parent_session,
        )?;
        drop(state);
        // Persist the header (including the parent link) immediately so the
        // child recording exists on disk before any external tree read, mirroring
        // `prepare_new_session_replacement` and `fork_session`. Subsequent
        // non-assistant entries still defer to the lazy flush lifecycle.
        recorder.persist_now()?;
        self.record(recorder)
    }

    /// Start a durable child recording: every entry from the header onward is
    /// fsync'd so a crash mid-turn leaves a recoverable partial transcript.
    /// `child_dir` is the child root (e.g. `<session-root>/children/<parent-id>/`).
    pub fn start_durable_child_recording(
        &self,
        child_dir: &Path,
        parent_session: &Path,
    ) -> Result<()> {
        let state = self.inner.shared.state.read();
        let recorder = crate::start_durable_child_session_in(
            &self.inner.cwd,
            Some(&state.model),
            Some(thinking_level_name(state.thinking_level)),
            child_dir,
            None,
            parent_session,
        )?;
        drop(state);
        self.record(recorder)
    }

    pub(crate) fn start_durable_child_recording_with_id(
        &self,
        child_dir: &Path,
        child_id: &str,
        parent_session: &Path,
    ) -> Result<()> {
        let state = self.inner.shared.state.read();
        let recorder = crate::start_durable_child_session_in(
            &self.inner.cwd,
            Some(&state.model),
            Some(thinking_level_name(state.thinking_level)),
            child_dir,
            Some(child_id),
            parent_session,
        )?;
        drop(state);
        self.record(recorder)
    }

    pub fn start_durable_child_recording_with_kind(
        &self,
        session_kind: SessionKind,
        child_dir: &Path,
        parent_session: &Path,
    ) -> Result<()> {
        let state = self.inner.shared.state.read();
        let recorder = crate::start_durable_child_session_in_with_kind(
            &self.inner.cwd,
            Some(&state.model),
            Some(thinking_level_name(state.thinking_level)),
            child_dir,
            None,
            parent_session,
            Some(session_kind),
        )?;
        drop(state);
        self.record(recorder)
    }

    /// Resume a durable child recording from an existing child JSONL path,
    /// continuing with durable (fsync) appends. Loads the existing history
    /// into the agent so the next turn continues the transcript.
    pub async fn resume_durable_child_recording(&self, path: &Path) -> Result<()> {
        let prepared = crate::PreparedSessionResume::prepare_path(path)?;
        if !prepared.target_cwd().as_os_str().is_empty() && prepared.target_cwd() != self.inner.cwd
        {
            return Err(anyhow!(
                "durable child session working directory {} does not match {}",
                prepared.target_cwd().display(),
                self.inner.cwd.display()
            ));
        }
        let context = prepared.build_context();
        let recorder = crate::resume_durable_child_session_from_prepared(prepared)?;
        self.cleanup_bash_spills();

        if let Some(provider) = context.provider.as_deref()
            && let Some(model_id) = context.model_id.as_deref()
            && let Some(model) = pi_ai::get_model(provider, model_id)
        {
            let api_key = self.inner.shared.state.read().api_key.clone();
            self.set_model(model, api_key);
        }
        self.set_thinking_level(parse_recorded_thinking_level(&context.thinking_level));
        self.resume_recording(recorder, context.messages).await?;
        Ok(())
    }

    pub(crate) async fn resume_durable_child_under_lease(
        &self,
        child_root: &crate::fs::CapabilityRootLease,
        relative: &Path,
    ) -> Result<()> {
        let prepared = crate::PreparedSessionResume::prepare_under_lease(child_root, relative)?;
        if !prepared.target_cwd().as_os_str().is_empty() && prepared.target_cwd() != self.inner.cwd
        {
            return Err(anyhow!(
                "durable child session working directory {} does not match {}",
                prepared.target_cwd().display(),
                self.inner.cwd.display()
            ));
        }
        let context = prepared.build_context();
        let recorder = crate::resume_durable_child_session_from_prepared(prepared)?;
        self.cleanup_bash_spills();

        if let Some(provider) = context.provider.as_deref()
            && let Some(model_id) = context.model_id.as_deref()
            && let Some(model) = pi_ai::get_model(provider, model_id)
        {
            self.set_model_with_resolved_auth(model).await?;
        }
        self.set_thinking_level(parse_recorded_thinking_level(&context.thinking_level));
        self.resume_recording(recorder, context.messages).await?;
        Ok(())
    }

    pub async fn switch_session(&self, path: &Path) -> Result<()> {
        self.switch_prepared_session(crate::PreparedSessionResume::prepare_path(path)?)
            .await
    }

    pub async fn switch_prepared_session(
        &self,
        prepared: crate::PreparedSessionResume,
    ) -> Result<()> {
        if !prepared.target_cwd().as_os_str().is_empty() && prepared.target_cwd() != self.inner.cwd
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

        if let Some(provider) = context.provider.as_deref()
            && let Some(model_id) = context.model_id.as_deref()
            && let Some(model) = pi_ai::get_model(provider, model_id)
        {
            let api_key = self.inner.shared.state.read().api_key.clone();
            self.set_model(model, api_key);
        }
        self.set_thinking_level(parse_recorded_thinking_level(&context.thinking_level));
        self.resume_recording(recorder, context.messages).await?;
        Ok(())
    }

    pub async fn navigate_tree(
        &self,
        target_id: &str,
        options: NavigateTreeOptions,
    ) -> Result<NavigateTreeResult> {
        let mut guard = self.claim_exclusive()?;
        let result = self.navigate_tree_claimed(target_id, options).await;
        guard.release();
        result
    }

    async fn navigate_tree_claimed(
        &self,
        target_id: &str,
        options: NavigateTreeOptions,
    ) -> Result<NavigateTreeResult> {
        let recorder = self.current_recorder()?;
        let tree = recorder.tree()?;
        let target = tree
            .entries
            .iter()
            .find(|entry| entry.id == target_id)
            .cloned()
            .ok_or_else(|| anyhow!("Entry not found: {target_id}"))?;
        let current_leaf = recorder.active_leaf_id();
        let (new_leaf, editor_text) = navigation_target(&target);
        if current_leaf == new_leaf && editor_text.is_none() {
            return Ok(NavigateTreeResult {
                editor_text,
                active_leaf_id: current_leaf,
                summary_entry_id: None,
                changed: false,
                cancelled: false,
            });
        }
        let abandoned =
            collect_abandoned_messages(&tree, current_leaf.as_deref(), new_leaf.as_deref());
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
            let operation = summarize_messages(
                &self.inner.shared,
                &abandoned,
                "",
                custom_prompt,
                settings.reserve_tokens,
                0.5,
                abort,
                None,
                COMPACTION_SUMMARIZE_TIMEOUT,
            );
            let mut operation = Box::pin(operation);
            let notified = self.inner.abort_notify.notified();
            if self.inner.run_slot.lock().abort_requested {
                controller.abort();
                let _ = operation.await;
                return Ok(NavigateTreeResult {
                    editor_text: None,
                    active_leaf_id: current_leaf,
                    summary_entry_id: None,
                    changed: false,
                    cancelled: true,
                });
            }
            tokio::select! {
                result = &mut operation => Some(result?),
                () = notified => {
                    controller.abort();
                    let _ = operation.await;
                    return Ok(NavigateTreeResult { editor_text: None, active_leaf_id: current_leaf, summary_entry_id: None, changed: false, cancelled: true });
                }
            }
        } else {
            None
        };
        let summary_entry_id =
            if let Some(summary) = summary.filter(|summary| !summary.trim().is_empty()) {
                let id = recorder.branch_with_summary(new_leaf.as_deref(), &summary)?;
                if let Some(label) = options.label.as_deref() {
                    recorder.record_label(&id, Some(label))?;
                }
                Some(id)
            } else {
                match new_leaf.as_deref() {
                    Some(entry_id) => recorder.branch(entry_id)?,
                    None => recorder.reset_leaf(),
                }
                None
            };
        let active_leaf_id = recorder.active_leaf_id();
        let tree = recorder.tree()?;
        let context = tree.build_context(None);
        let todo_snapshot = tree.latest_todo_snapshot().unwrap_or_else(|| {
            TodoSnapshotRecord::from_state(crate::TodoState {
                phases: Vec::new(),
                storage: crate::TodoStorage::Session,
            })
        });
        self.inner
            .agent
            .set_messages(context.messages.clone())
            .await;
        self.inner.shared.state.write().messages = context.messages;
        self.inner.todo.restore_snapshot(todo_snapshot)?;
        // Navigation swapped the transcript; drop any stored Responses chain id
        // so the next turn sends full history from the new checkpoint.
        pi_ai::providers::reset_responses_chain(&self.inner.shared.session_id);
        *self.inner.shared.compaction_runtime.lock() = CompactionRuntime::default();
        self.inner
            .shared
            .compaction_active
            .store(false, Ordering::Release);
        *self.inner.shared.goal.write() = GoalRuntime::from_session_recorder((*recorder).clone())
            .map_err(|error| anyhow!(error.to_string()))?;
        Ok(NavigateTreeResult {
            editor_text,
            active_leaf_id,
            summary_entry_id,
            changed: true,
            cancelled: false,
        })
    }

    pub fn set_session_label(&self, target_id: &str, label: Option<&str>) -> Result<()> {
        let normalized = label.map(str::trim).filter(|label| !label.is_empty());
        self.current_recorder()?
            .record_label(target_id, normalized)?;
        Ok(())
    }

    /// Mark the current position as a named rewind target.
    ///
    /// Appends a `checkpoint` journal record pointing at the current leaf.
    /// The marker is a side record: it never joins the linear record chain,
    /// never appears in the transcript, and is itself removed when a rewind
    /// rolls back past it. Recording a name that already exists shadows the
    /// older marker (the newest wins on resolve).
    pub fn set_checkpoint(&self, name: &str) -> Result<String> {
        let recorder = self.current_recorder()?;
        let id = recorder.record_checkpoint(name)?;
        self.publish_recorded_entry(&recorder, &id)?;
        Ok(id)
    }

    /// Render the last `limit` records (index + first-line preview) for the
    /// bare `/rewind` picker. Checkpoint markers are annotated with their name
    /// and target so they are discoverable rewind targets.
    pub fn rewind_preview(&self, limit: usize) -> Result<Vec<crate::RewindEntryPreview>> {
        let tree = self.current_recorder()?.tree()?;
        let start = tree.entries.len().saturating_sub(limit);
        let mut previews = Vec::with_capacity(tree.entries.len() - start);
        for (offset, entry) in tree.entries[start..].iter().enumerate() {
            let is_checkpoint = entry.entry_type == "checkpoint";
            previews.push(crate::RewindEntryPreview {
                index: start + offset,
                entry_type: entry.entry_type.clone(),
                timestamp: entry.timestamp.clone(),
                preview: entry.message.as_ref().and_then(message_first_line),
                checkpoint_name: (is_checkpoint).then(|| entry.name.clone()).flatten(),
                checkpoint_target_id: (is_checkpoint).then(|| entry.target_id.clone()).flatten(),
            });
        }
        Ok(previews)
    }

    /// Roll the session back to a rewind target.
    ///
    /// The session file is truncated at the target record (the dropped tail is
    /// archived to a `.rewind-<timestamp>.jsonl` sidecar first), then the
    /// in-memory transcript, todo list, goal state, session name, and model
    /// chain are rebuilt from the retained journal. Refuses to rewind past the
    /// first record, and refuses while a prompt is processing (the exclusive
    /// run slot is held by the live turn).
    pub async fn rewind(&self, target: RewindTarget) -> Result<RewindOutcome> {
        let mut guard = self.claim_exclusive()?;
        let outcome = self.rewind_claimed(target).await;
        guard.release();
        outcome
    }

    async fn rewind_claimed(&self, target: RewindTarget) -> Result<RewindOutcome> {
        let recorder = self.current_recorder()?;
        let tree = recorder.tree()?;
        let (keep, checkpoint_name) = match target {
            RewindTarget::Index(index) => {
                anyhow::ensure!(
                    index >= 1,
                    "rewind refused: cannot rewind past the first entry (entry index 0 is the earliest record)"
                );
                anyhow::ensure!(
                    index < tree.entries.len(),
                    "nothing to rewind: the session has {} record(s); entry index {index} is at or beyond the end",
                    tree.entries.len()
                );
                (index, None)
            }
            RewindTarget::Checkpoint(name) => {
                let checkpoint = tree
                    .entries
                    .iter()
                    .rev()
                    .find(|entry| {
                        entry.entry_type == "checkpoint" && entry.name.as_deref() == Some(name.as_str())
                    })
                    .ok_or_else(|| {
                        anyhow!(
                            "checkpoint {name:?} not found — use /checkpoint <name> to mark the current position, or /rewind to list entry indices"
                        )
                    })?;
                let target_id = checkpoint
                    .target_id
                    .as_deref()
                    .ok_or_else(|| anyhow!("checkpoint {name:?} is missing its target entry"))?;
                let target_index = tree
                    .entries
                    .iter()
                    .position(|entry| entry.id == target_id)
                    .ok_or_else(|| {
                        anyhow!(
                            "checkpoint {name:?} targets entry {target_id} which is no longer in the session (it was rewound away); mark a fresh checkpoint"
                        )
                    })?;
                (target_index + 1, Some(name))
            }
        };
        let store_outcome = recorder.rewind_to(keep)?;
        // Rebuild the in-memory transcript, todo list, goal state, and session
        // name from the truncated journal. `GoalRuntime::from_session_recorder`
        // replays the goal journal up to the cut point, so a rewind that cuts
        // through the journal re-derives the goal state (including dropping the
        // goal when its creation event was cut away).
        let tree = recorder.tree()?;
        let context = tree.build_context(None);
        self.inner
            .agent
            .set_messages(context.messages.clone())
            .await;
        // Drain-and-requeue: the rewind replaces the transcript but must not
        // silently consume acknowledged steering. The queued messages return
        // to the (now-empty) queue head after the rewind; their journal
        // markers stay registered and are flipped on delivery.
        let (queued_steering, queued_follow_up) =
            self.inner.agent.drain_queued_messages().await;
        self.inner.agent.clear_all_queues().await;
        for message in queued_steering {
            self.inner.agent.steer(message).await;
        }
        for message in queued_follow_up {
            self.inner.agent.follow_up(message).await;
        }
        self.publish_queue_update().await;
        self.inner.shared.state.write().messages = context.messages;
        let todo_snapshot = tree.latest_todo_snapshot().unwrap_or_else(|| {
            TodoSnapshotRecord::from_state(crate::TodoState {
                phases: Vec::new(),
                storage: crate::TodoStorage::Session,
            })
        });
        self.inner.todo.restore_snapshot(todo_snapshot)?;
        // The transcript was replaced wholesale; drop any stored Responses
        // chain id so the next turn sends full history from the rewind point.
        pi_ai::providers::reset_responses_chain(&self.inner.shared.session_id);
        *self.inner.shared.compaction_runtime.lock() = CompactionRuntime::default();
        self.inner
            .shared
            .compaction_active
            .store(false, Ordering::Release);
        *self.inner.shared.goal.write() = GoalRuntime::from_session_recorder((*recorder).clone())
            .map_err(|error| anyhow!(error.to_string()))?;
        *self.inner.shared.session_name.write() = recorder.session_name();
        self.inner
            .shared
            .recorded_count
            .store(tree.entries.len(), Ordering::Release);
        Ok(RewindOutcome {
            archive_path: store_outcome.archive_path,
            dropped_entries: store_outcome.dropped_entries,
            retained_entries: store_outcome.retained_entries,
            checkpoint: checkpoint_name,
        })
    }

    pub async fn fork_session(&self, entry_id: &str, restore_conversation: bool) -> Result<String> {
        let session_dir = self.inner.session_dir.read().clone();
        let MaterializedFork {
            recorder: replacement,
            editor_text,
        } = crate::session_store::fork_live_session_at_user(
            self.current_recorder()?.as_ref(),
            entry_id,
            Some(&session_dir),
        )?;
        let mut unpublished_fork = UnpublishedFork::new(&replacement);
        #[cfg(test)]
        if self
            .inner
            .fail_fork_resume_after_materialize
            .swap(false, Ordering::AcqRel)
        {
            return Err(unpublished_fork.rollback(anyhow!(
                "injected fork resume failure after materialization"
            )));
        }
        let context = match replacement.tree() {
            Ok(tree) => tree.build_context(None),
            Err(error) => return Err(unpublished_fork.rollback(error)),
        };
        let (model, api_key, thinking_level) =
            match recorded_cut_policy(&self.inner.shared, &context).await {
                Ok(policy) => policy,
                Err(error) => return Err(unpublished_fork.rollback(error)),
            };
        let messages = if restore_conversation {
            context.messages
        } else {
            Vec::new()
        };
        self.resume_recording_with_policy(
            replacement,
            messages,
            Some(unpublished_fork),
            model,
            api_key,
            Some(thinking_level),
        )
        .await?;
        Ok(editor_text)
    }

    #[cfg(test)]
    fn inject_fork_prepare_failure_after_materialize(&self) {
        self.inner
            .fail_fork_prepare_after_materialize
            .store(true, Ordering::Release);
    }

    #[cfg(test)]
    fn inject_fork_resume_failure_after_materialize(&self) {
        self.inner
            .fail_fork_resume_after_materialize
            .store(true, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn arm_fork_before_install_gate_for_test(&self) -> SessionForkInstallTestGate {
        let gate = SessionForkInstallTestGate::default();
        *self.inner.fork_before_install_gate.lock() = Some(gate.clone());
        gate
    }

    #[cfg(test)]
    pub(crate) fn arm_fork_after_install_gate_for_test(&self) -> SessionForkInstallTestGate {
        let gate = SessionForkInstallTestGate::default();
        *self.inner.fork_after_install_gate.lock() = Some(gate.clone());
        gate
    }


    pub async fn clone_session(&self, leaf_id: &str, restore_conversation: bool) -> Result<()> {
        let recorder = self.current_recorder()?;
        anyhow::ensure!(
            recorder
                .tree()?
                .entries
                .iter()
                .any(|entry| entry.id == leaf_id),
            "Cannot clone session: current entry is unavailable"
        );
        let session_dir = self.inner.session_dir.read().clone();
        let replacement = crate::session_store::create_branched_session_from_recorder(
            &recorder,
            leaf_id,
            Some(&session_dir),
        )?;
        let messages = if restore_conversation {
            replacement.tree()?.build_context(None).messages
        } else {
            Vec::new()
        };
        self.resume_recording(replacement, messages).await
    }

    pub fn fork_messages(
        &self,
        cursor: Option<&str>,
        limit: Option<usize>,
    ) -> Result<crate::ForkMessagePage> {
        let tree = self.current_recorder()?.tree()?;
        tree.fork_messages_page(cursor, limit)
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
        Ok(self.current_recorder()?.tree()?.tree())
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

    /// Live fork capture: the currently attached recorder, if any.
    #[must_use]
    pub fn current_recorder_for_fork(&self) -> Option<Arc<SessionRecorder>> {
        self.inner.shared.recorder.lock().as_ref().cloned()
    }


    #[must_use]
    pub fn goal_runtime(&self) -> GoalRuntime {
        self.inner.shared.goal.read().clone()
    }

    /// Replays the goal journal from the active session branch, oldest event
    /// first. Read-only: the in-memory goal runtime is untouched.
    pub fn goal_journal(&self) -> Result<Vec<crate::GoalEvent>> {
        let recorder = self.current_recorder()?;
        let tree = recorder.tree()?;
        crate::goal_events_from_session_tree(&tree).map_err(|error| anyhow!(error.to_string()))
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

    pub fn recording_writer_info(
        &self,
    ) -> Result<(String, PathBuf, crate::SessionFileIdentity)> {
        self.current_recorder()?.writer_info()
    }

    pub(crate) fn recorder(&self) -> Option<SessionRecorder> {
        self.inner
            .shared
            .recorder
            .lock()
            .as_ref()
            .map(|recorder| (**recorder).clone())
    }

    pub fn authoritative_transcript(&self) -> Result<Vec<TranscriptRow>, TranscriptUnavailable> {
        match self.recorder() {
            None => Ok(self
                .history()
                .into_iter()
                .map(|message| TranscriptRow {
                    id: None,
                    message,
                })
                .collect()),
            Some(recorder) => {
                let tree = recorder.tree().map_err(TranscriptUnavailable::from)?;
                project_retained_transcript(&tree)
            }
        }
    }

    pub fn loader_drop_counts(&self) -> (usize, usize) {
        self.recorder()
            .and_then(|recorder| recorder.tree().ok())
            .map(|tree| (tree.skipped_idless, tree.undecodable_messages))
            .unwrap_or((0, 0))
    }

    pub(crate) fn recorder_file_authority(
        &self,
    ) -> Option<(crate::fs::CapabilityRootLease, PathBuf)> {
        self.inner
            .shared
            .recorder
            .lock()
            .as_ref()
            .and_then(|recorder| recorder.file_authority())
    }

    #[must_use]
    pub fn session_file_identity(&self) -> Option<crate::SessionFileIdentity> {
        self.inner
            .shared
            .recorder
            .lock()
            .as_ref()
            .and_then(|recorder| recorder.file_identity())
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

    pub(crate) fn set_retry_role(&self, role: Option<String>) {
        *self.inner.shared.retry_role.write() = role;
    }

    #[must_use]
    pub fn retry_role(&self) -> Option<String> {
        self.inner.shared.retry_role.read().clone()
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
        // The agent's own transform hook is the fixed composite installed at
        // construction (vision projection first); this slot feeds the
        // user/extension half of that composite so both always run.
        *self.inner.transform_context.write() = hook;
    }

    #[must_use]
    pub fn before_tool_call(&self) -> Option<BeforeToolCallFn> {
        self.inner.agent.before_tool_call()
    }

    /// Policy (user/extension) before-tool-call chain WITHOUT the host
    /// `pre_tool_call` wrapper. Application extension bind and ACP compose
    /// onto this slot so `set_before_tool_call` can wrap host-pre exactly once.
    #[must_use]
    pub fn raw_before_tool_call(&self) -> Option<BeforeToolCallFn> {
        self.inner.raw_before_tool_call.read().clone()
    }

    #[must_use]
    pub fn approve_tool_call(&self) -> Option<BeforeToolCallFn> {
        self.inner.agent.approve_tool_call()
    }

    /// Fork-time policy snapshot for detached side-chat/code-review agents.
    ///
    /// The after hook rebuilds the raw (todo-normalized) chain under a
    /// detached host-post wrapper — raw extension reducer + fork-time host
    /// `post_tool_call` snapshot + bash spill + an isolated per-fork
    /// doom-loop tracker. Every `Arc` is captured eagerly so later
    /// `set_host_hooks` / `set_after_tool_call` changes on the main session
    /// never re-fire inside the fork.
    #[must_use]
    pub fn detached_after_tool_call(&self) -> Option<AfterToolCallFn> {
        let shared = self.inner.shared.clone();
        let hooks = shared.host_hooks.read().clone();
        let extension_names = Arc::new(shared.extension_tool_names.read().clone());
        let hook = self.inner.raw_after_tool_call.read().clone();
        // Isolated per-fork doom-loop state: the fork tracks its own
        // identical-failure runs and stops on the first workspace-escape
        // rejection, but never reads or writes Main's `shared.doom_loop`
        // tracker — a doom loop in the fork can neither poison a Main run
        // nor be pre-triggered by one.
        let doom_loop = Arc::new(DetachedDoomLoopState::default());
        Some(compose_bash_spill_after_tool_call(
            shared.clone(),
            compose_host_post_tool_call_detached_frozen(hooks, extension_names, hook, doom_loop),
        ))
    }

    /// Fork-time before-hook snapshot: frozen host `pre_tool_call` composed
    /// once with the fork-time raw policy chain (user + extension reducer).
    /// Later Main `set_host_hooks` replacements never re-fire in the fork.
    #[must_use]
    pub fn detached_before_tool_call(&self) -> Option<BeforeToolCallFn> {
        let hooks = self.inner.shared.host_hooks.read().clone();
        let extension_names = self.inner.shared.extension_tool_names.read().clone();
        let policy = self.inner.raw_before_tool_call.read().clone();
        compose_host_pre_tool_call_frozen(hooks, Arc::new(extension_names), policy)
    }

    /// Atomic snapshot of the session's active tool set (names + definitions
    /// cloned under one lock). Used by Code Review fork/refork so the exact
    /// Main tool set and hook snapshot come from one generation.
    #[must_use]
    pub fn active_tools(&self) -> Vec<AgentTool> {
        self.inner.tools.read().clone()
    }




    /// Replace the host-level hook configuration for this session.
    ///
    /// Hooks are external commands fired at session, turn, and tool-call
    /// events (see [`crate::HostHooks`]). The configuration is read live, so
    /// this may be called after the session starts; the previous config is
    /// replaced wholesale.
    pub fn set_host_hooks(&self, hooks: Option<Vec<crate::HookConfig>>) {
        let capture_generation = self
            .resource_manager()
            .map(|resources| resources.capture_fn());
        let hooks = hooks.and_then(|entries| {
            capture_generation.map(|capture_generation| {
                Arc::new(crate::HostHooks::new(
                    entries,
                    self.inner.cwd.clone(),
                    self.inner.shared.session_id.clone(),
                    capture_generation,
                ))
            })
        });
        *self.inner.shared.host_hooks.write() = hooks;
    }

    /// Names of tools supplied by the extension runtime.
    ///
    /// Host hooks do not fire for extension tool calls in the MVP.
    pub fn set_extension_tool_names(&self, names: impl IntoIterator<Item = String>) {
        *self.inner.shared.extension_tool_names.write() = names.into_iter().collect();
    }

    /// Bind the extension runtime namespace this session's stream dispatch
    /// resolves extension-owned provider apis within. The Application calls
    /// this when it attaches an [`ExtensionRuntime`]; sessions without a
    /// runtime keep `None` and resolve exactly as before (global builtins and
    /// unscoped extension entries).
    pub fn set_provider_namespace(&self, namespace: Option<String>) {
        *self.inner.shared.provider_namespace.write() = namespace;
    }

    async fn fire_host_hook(
        &self,
        event: crate::HookEvent,
        subject: Option<&str>,
        tool: Option<crate::HookToolPayload<'_>>,
    ) -> crate::HookDecision {
        let Some(hooks) = self.inner.shared.host_hooks.read().clone() else {
            return crate::HookDecision::allow();
        };
        hooks.fire(event, subject, tool.as_ref()).await
    }

    /// Fire the configured `pre_trust_decision` host hooks for a tentative
    /// trust decision (canonical project path, wire decision, new-to-store
    /// flag) before the stored decision is consulted/recorded. Fail-open:
    /// no hooks configured yields an allow decision, and hook failures deny
    /// only when the entry sets `fail_closed` (see
    /// [`crate::HostHooks::fire_trust_decision`]).
    pub async fn fire_trust_decision_hook(
        &self,
        path: &str,
        decision: &str,
        is_new: bool,
    ) -> crate::HookDecision {
        let Some(hooks) = self.inner.shared.host_hooks.read().clone() else {
            return crate::HookDecision::allow();
        };
        hooks.fire_trust_decision(path, decision, is_new).await
    }

    /// Install the policy (user/extension) before-tool-call hook chain.
    ///
    /// The incoming hook is stored RAW (no host wrapper). The host
    /// `pre_tool_call` wrapper is composed exactly once onto that raw chain
    /// before install, so Application/ACP compose onto
    /// [`Session::raw_before_tool_call`] rather than the already-wrapped
    /// agent hook.
    pub fn set_before_tool_call(&self, hook: Option<BeforeToolCallFn>) {
        *self.inner.raw_before_tool_call.write() = hook.clone();
        self.inner
            .agent
            .set_before_tool_call(compose_host_pre_tool_call(self.inner.shared.clone(), hook));
    }

    pub fn set_approve_tool_call(&self, hook: Option<BeforeToolCallFn>) {
        self.inner.agent.set_approve_tool_call(hook);
    }

    pub fn set_after_tool_call(&self, hook: Option<AfterToolCallFn>) {
        // Re-apply todo normalization ahead of the caller's hook so the
        // internal `__piTodoError` marker contract survives every replacement
        // (idempotent: the first wrap strips the marker, later wraps pass
        // through). The todo-wrapped hook is what `raw_after_tool_call`
        // stores and what detached forks rebuild from.
        let raw = Some(todo_after_tool_call(hook));
        *self.inner.raw_after_tool_call.write() = raw.clone();
        // Compose with spill tracking so agent `bash` tool success paths are
        // registered even when Application/extensions replace the after-hook.
        self.inner
            .agent
            .set_after_tool_call(Some(compose_bash_spill_after_tool_call(
                self.inner.shared.clone(),
                compose_host_post_tool_call(self.inner.shared.clone(), raw),
            )));
    }

    /// Install a per-turn stop hook consulted after each assistant turn.
    ///
    /// Returning `true` ends the run cleanly after the current turn with the
    /// partial result and accumulated usage preserved — used by orchestration
    /// to implement soft budgets and yield-driving for subagents. The hook is
    /// consulted on every turn including the final one; returning `true` there
    /// does not change the outcome. Set to `None` to disable.
    pub fn set_should_stop_after_turn(&self, hook: Option<ShouldStopAfterTurnFn>) {
        self.inner.agent.set_should_stop_after_turn(hook);
    }

    pub async fn execute_bash(
        &self,
        command: &str,
        exclude_from_context: bool,
    ) -> Result<crate::BashResult> {
        self.execute_bash_with_id(command, exclude_from_context, None)
            .await
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
            *active = Some(ActiveBash {
                generation,
                controller: controller.clone(),
            });
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
            let _ = events.send(SessionEvent::BashExecutionUpdate {
                id: chunk_id.clone(),
                delta,
            });
        });
        let generation = self.capture_application_generation()?;
        let result = crate::execute_bash(
            generation,
            self.inner.shared.auth_manager.clone(),
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
        self.publish_session_event(SessionEvent::BashExecutionEnd {
            message: bash_message,
        });
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
        let messages = self
            .current_recorder()
            .and_then(|recorder| recorder.tree())
            .map_or_else(
                |_| self.history(),
                |tree| {
                    tree.entries
                        .into_iter()
                        .filter_map(|entry| entry.message)
                        .collect()
                },
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
                    tool_calls += message
                        .content
                        .iter()
                        .filter(|block| matches!(block, ContentBlock::ToolCall(_)))
                        .count();
                    add_usage(&mut total, &message.usage);
                }
                Message::ToolResult(_) => tool_results += 1,
                Message::BashExecution(_)
                | Message::Custom(_)
                | Message::BranchSummary(_)
                | Message::CompactionSummary(_) => {}
            }
        }
        let (session_id, session_file) = self.recorder_info().map_or((None, None), |(id, path)| {
            (Some(id), Some(path.to_string_lossy().into_owned()))
        });
        let context_usage = self.model().and_then(|model| {
            (model.context_window > 0).then(|| {
                let history = self.history();
                let tokens = estimate_live_context_tokens(&history);
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
                (
                    "PI_REASONING_LEVEL".to_owned(),
                    thinking_level_name(state.thinking_level).to_owned(),
                ),
            ]);
            drop(state);
            if let Some(recorder) = shared.recorder.lock().as_ref() {
                env.insert("PI_SESSION_ID".to_owned(), recorder.id());
                env.insert(
                    "PI_SESSION_FILE".to_owned(),
                    recorder.path().to_string_lossy().into_owned(),
                );
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
            // `Agent::abort` clears both pending queues; publish so the
            // composer count/preview drop immediately. The aborted turn's
            // `finish_run` republishes on settle, but an abort that never
            // settles a run (or settles after a long cleanup) would otherwise
            // leave a stale pending count.
            self.publish_queue_update().await;
        }
    }

    #[cfg(test)]
    pub(crate) async fn navigate_tree_cancel_for_test(
        &self,
        target_id: &str,
        options: NavigateTreeOptions,
    ) -> Result<NavigateTreeResult> {
        let mut guard = self.claim_exclusive()?;
        self.inner.run_slot.lock().abort_requested = true;
        let result = self.navigate_tree_claimed(target_id, options).await;
        guard.release();
        result
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
        let mut content = Vec::with_capacity(images.len() + usize::from(!prompt.is_empty()));
        if !prompt.is_empty() {
            content.push(ContentBlock::text(prompt));
        }
        content.extend(images);
        // Fail-closed delegation before recording; the run keeps the ORIGINAL
        // content (images intact) for the transcript, with the delegated
        // description projected only into the model context via the cache.
        self.delegate_vision_images(content.clone()).await?;
        messages[0] = Message::User(UserMessage {
            content,
            timestamp: pi_ai::now_millis(),
        });
        let claim = self.begin_run("user").await?;
        let agent_mention = Self::agent_mention_name(&messages);
        let mention_context =
            AgentMentionContextGuard::begin(self.inner.shared.clone(), agent_mention);
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
        let mut operation =
            Box::pin(async move { session.execute_with_retries(Some(messages)).await });
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
        drop(mention_context);
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
        // S-RACE-2: print mode settles the same way as run_messages — a steer
        // that landed in the post-final-drain window must be delivered by a
        // continuation instead of stranding until the next prompt.
        self.deliver_queued_after_settle().await;
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
        // Fail-closed image delegation BEFORE anything is recorded: errors
        // propagate here and no unprocessed prompt reaches the recorder or the
        // agent. The ORIGINAL messages (images intact) are what the run
        // records and the history/UI carry; the delegated description is
        // applied per-turn only to the model context by the composite
        // transform hook, which hits this cache.
        self.delegate_vision_messages(messages.clone()).await?;
        let agent_mention = Self::agent_mention_name(&messages);
        let messages = self.inject_selection_messages(messages).await;
        let claim = self.begin_run("user").await?;
        let mention_context =
            AgentMentionContextGuard::begin(self.inner.shared.clone(), agent_mention);
        let operation = self.execute_with_retries(Some(messages)).await;
        let result = self.finish_run(claim, operation).await;
        drop(mention_context);
        self.deliver_queued_after_settle().await;
        result
    }

    pub async fn continue_run(&self) -> Result<RunResult> {
        let result = self.continue_run_inner().await;
        self.deliver_queued_after_settle().await;
        result
    }

    async fn continue_run_inner(&self) -> Result<RunResult> {
        let claim = self.begin_run("assistant").await?;
        let operation = self.execute_with_retries(None).await;
        self.finish_run(claim, operation).await
    }

    /// Deliver steering/follow-up messages that arrived while the run was
    /// settling. The agent loop drains the queues at every turn boundary and
    /// once more in its final window, so a message enqueued while the last
    /// turn's model call was in flight can be left behind when the run ends:
    /// nothing remains to consume it and the TUI's `⟦steering⟧` / `⚙ N`
    /// projection stays stale even though the text already reached the
    /// transcript. Start continuation runs until the queue is empty or a
    /// continuation fails. Bounded by [`SESSION_SETTLE_QUEUE_DRAIN_CAP`];
    /// diagnostics carry the remaining count only, never the message body.
    async fn deliver_queued_after_settle(&self) {
        let mut delivered = 0usize;
        while delivered < SESSION_SETTLE_QUEUE_DRAIN_CAP && self.pending_message_count().await > 0 {
            if self.continue_run_inner().await.is_ok() {
                delivered += 1;
                continue;
            }
            match self.retry_settle_delivery_after_contention().await {
                Some(()) => delivered += 1,
                None => {
                    let remaining = self.pending_message_count().await;
                    eprintln!(
                        "session: queued-message continuation failed; {remaining} message(s) remain queued"
                    );
                    return;
                }
            }
        }
        let remaining = self.pending_message_count().await;
        if remaining > 0 {
            eprintln!(
                "session: stopped queue drain after {SESSION_SETTLE_QUEUE_DRAIN_CAP} continuations; {remaining} message(s) remain queued"
            );
        }
    }

    /// Bounded retry (S-RACE-4) for a post-settle continuation that lost the
    /// run-slot claim race: the slot is released only after the agent goes
    /// idle, so an immediate continuation can fail with "session is already
    /// processing a prompt" while the release is still in flight. A steering
    /// message that landed mid-run must be delivered by this settle, not
    /// stranded until the next prompt, so retry with backoff while the slot
    /// is held. `Some(())` when a retry delivered the batch.
    async fn retry_settle_delivery_after_contention(&self) -> Option<()> {
        for attempt in 1..=SESSION_SETTLE_CONTINUATION_RETRIES {
            if !self.inner.run_slot.lock().active {
                // The slot is already free: the failure was genuine, not
                // claim contention, and must not be retried.
                return None;
            }
            tokio::time::sleep(Duration::from_millis(50 * attempt as u64)).await;
            if self.continue_run_inner().await.is_ok() {
                return Some(());
            }
        }
        None
    }

    fn is_expanded_skill_command(request: &str, name: &str, body: &str) -> bool {
        let uri = format!("skill://{name}");
        let wrapper = format!(
            "<skill name=\"{name}\" location=\"{uri}\">\nReferences are relative to {uri}/.\n\n{}\n</skill>",
            body.trim()
        );
        request == wrapper
            || request
                .strip_prefix(&wrapper)
                .is_some_and(|rest| rest.starts_with("\n\n"))
    }

    pub(crate) fn agent_mention_message(name: &str) -> Message {
        Message::Custom(CustomMessage {
            custom_type: AGENT_MENTION_CUSTOM_TYPE.to_owned(),
            content: format!(
                "<system-reminder>\nThe user directly addresses the available agent/persona `{name}`. Delegate this request with `task.agent` set exactly to `{name}`. Keep the user's delegation sentence in this parent turn; do not rephrase that sentence as the child payload and do not substitute another agent.\n</system-reminder>"
            )
            .into(),
            display: false,
            details: Some(serde_json::json!({ "agent": name })),
            timestamp: pi_ai::now_millis(),
        })
    }

    fn agent_mention_name(messages: &[Message]) -> Option<String> {
        messages.iter().rev().find_map(|message| match message {
            Message::Custom(custom) if custom.custom_type == AGENT_MENTION_CUSTOM_TYPE => custom
                .details
                .as_ref()
                .and_then(|details| details.get("agent"))
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned),
            _ => None,
        })
    }

    async fn inject_selection_messages(&self, mut messages: Vec<Message>) -> Vec<Message> {
        messages = self.inject_hindsight_memory(messages).await;
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
        let autoload = self
            .capture_application_generation()
            .map(|generation| {
                crate::selector::load_autoload_skill_bodies_with_capabilities(
                    generation.sandbox().capabilities(),
                    &plan,
                    &self.inner.shared.selector_skills.read(),
                )
            })
            .unwrap_or_default()
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

    /// Turn-start hindsight memory injection: when `settings.memory.backend`
    /// is `hindsight` and `settings.memory.hindsightInjection` is on, recall
    /// the memories related to the latest user ask and prepend the bounded,
    /// redacted output to the system context as a hidden custom message
    /// (`display: false` — never auto-submitted). One rendered body is cached
    /// by a stable fingerprint of the trimmed ask and complete effective
    /// memory configuration, so unchanged input hits while a settings change
    /// performs a fresh recall. Configuration, network, HTTP-status,
    /// response-bound, and timeout failures silently skip injection: advisory
    /// memory can never fail a turn.
    async fn inject_hindsight_memory(&self, mut messages: Vec<Message>) -> Vec<Message> {
        let resources = self.inner.resources.read().clone();
        let Some(resources) = resources else {
            return messages;
        };
        let config = resources.snapshot().settings.memory_config();
        if config.backend != crate::MemoryBackend::Hindsight || !config.hindsight_injection {
            *self.inner.shared.hindsight_injection_cache.write() = None;
            return messages;
        }
        let Some(request) = messages.iter().rev().find_map(|message| match message {
            Message::User(user) => Some(content_text(&user.content)),
            _ => None,
        }) else {
            return messages;
        };
        let request = request.trim().to_owned();
        if request.is_empty() {
            return messages;
        }
        let key = hindsight_injection_cache_key(&config, &request);
        if let Some(cached) = self.inner.shared.hindsight_injection_cache.read().as_ref() {
            if cached.key == key {
                return if cached.body.is_empty() {
                    messages
                } else {
                    prepend_hindsight_memory(messages, &cached.body)
                };
            }
        }
        // A different effective configuration/request invalidates the previous
        // entry before I/O. A failed fresh recall must not leave an older key
        // available to become a hit after another reload.
        *self.inner.shared.hindsight_injection_cache.write() = None;
        let fetched = match self.capture_application_generation().map(|generation| {
            generation
                .sandbox()
                .http()
                .executor(pi_sandbox::HostHttpClass::MemoryApi)
        }) {
            Ok(executor) => match crate::memory::HindsightClient::new_with_executor(
                &config,
                &self.inner.cwd,
                executor,
            ) {
                Ok(client) => {
                    client
                        .recall(&request, &pi_agent::AbortSignal::none())
                        .await
                }
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        let body = match fetched {
            Ok(body) => body,
            Err(_) => {
                // Fail open: injection is best-effort context, never an error
                // surface for the turn.
                return messages;
            }
        };
        let mut cache = self.inner.shared.hindsight_injection_cache.write();
        *cache = Some(HindsightInjectionCacheEntry {
            key,
            body: body.clone(),
        });
        prepend_hindsight_memory(messages, &body)
    }

    fn prepare_initial_messages(&self, mut messages: Vec<Message>) -> Vec<Message> {
        if messages
            .iter()
            .any(|message| matches!(message, Message::User(_)))
            && self.inner.todo.take_reminder()
        {
            let state = self.inner.todo.state();
            messages.insert(
                0,
                Message::Custom(CustomMessage {
                    custom_type: "todo-error-reminder".to_owned(),
                    content: format!(
                        "A previous todo operation failed. Reconcile the canonical todo DAG before continuing. Prefer any ready task; phase order is presentation only, and waitingOn explains which dependencies remain unsatisfied:\n{}",
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
        self.inner
            .shared
            .overflow_recovery_attempted
            .store(false, Ordering::Release);
        self.inner.shared.fallback_attempt_errors.lock().clear();
        // Doom-loop detection is scoped to the current turn: the run of
        // identical consecutive tool failures starts fresh on every turn.
        self.inner.shared.doom_loop.lock().reset();
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
            // A doom loop tripped inside the previous settle: the turn stops
            // with the actionable message instead of retrying/falling back.
            if let Some(message) = self.inner.shared.doom_loop.lock().triggered_message.clone() {
                return Err(anyhow!("{message}"));
            }
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
                self.perform_compaction(CompactionReason::Overflow, true, None, None)
                    .await
                    .map_err(|error| anyhow!("Context overflow recovery failed: {error}"))?;
                operation = self.settle_operation(self.inner.agent.continue_run()).await;
                continue;
            }

            let settings = self.retry_settings();
            let retryable = is_retryable_assistant_error(&failure);
            let fallback_only = is_fallback_only_assistant_error(&failure);
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

            let may_fallback = settings.enabled
                && settings.model_fallback
                && (retryable || fallback_only)
                && !has_tool_call
                && !is_abort;
            let retry_budget_exhausted = fallback_only || attempt >= settings.max_retries;
            if retry_budget_exhausted {
                let current_selector = format_retry_fallback_selector(
                    &state.model,
                    Some(thinking_level_name(state.thinking_level)),
                );
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

                let final_error = self.finish_retry_failure(attempt, &error_message);
                return Err(anyhow!(final_error));
            }

            if !retryable || !settings.enabled {
                let final_error = self.finish_retry_failure(attempt, &error_message);
                return if operation.is_err()
                    && self.inner.shared.fallback_attempt_errors.lock().len() <= 1
                {
                    operation
                } else {
                    Err(anyhow!(final_error))
                };
            }

            attempt += 1;
            self.inner
                .shared
                .retry_attempt
                .store(attempt, Ordering::Release);
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
            let retry_failed = retry_state
                .messages
                .last()
                .and_then(|message| match message {
                    Message::Assistant(assistant) => Some(assistant),
                    _ => None,
                });
            if retry_failed.is_none_or(|assistant| assistant.stop_reason != StopReason::Error) {
                // A retry that "succeeded" may actually have been stopped by
                // doom-loop recovery (terminated tool loop): surface the
                // actionable message instead of reporting a normal success.
                if let Some(message) = self.inner.shared.doom_loop.lock().triggered_message.clone()
                {
                    return Err(anyhow!("{message}"));
                }
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

    fn finish_retry_failure(&self, attempt: usize, latest_error: &str) -> String {
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
                final_error: Some(final_error.clone()),
            });
        }
        self.inner.shared.retry_attempt.store(0, Ordering::Release);
        final_error
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
        let roles =
            self.inner
                .shared
                .retry_role
                .read()
                .as_ref()
                .map_or_else(BTreeMap::new, |role| {
                    BTreeMap::from([(
                        role.clone(),
                        format_retry_fallback_selector(current_model, None),
                    )])
                });
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
        let retry_role = self.inner.shared.retry_role.read().clone();
        let role = active_role.or_else(|| {
            resolve_retry_fallback_chain_key(
                &context,
                current_selector,
                Some(current_model),
                retry_role.as_deref(),
            )
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

    pub async fn steer(&self, message: Message) -> Result<()> {
        // Fail-closed delegation before enqueue; the ORIGINAL message (image
        // intact) is queued so history/UI keep the image, and the model-context
        // projection hits the delegation cache when the queue drains.
        self.delegate_vision_message(message.clone()).await?;
        // Journal the accept BEFORE enqueueing so the RPC only acks a message
        // that is already durable: a crash between the ack and the delivery
        // leaves an undelivered marker that resume requeues.
        self.journal_steering_marker(false, &message).await?;
        self.inner.agent.steer(message).await;
        self.publish_queue_update().await;
        Ok(())
    }

    /// Arm (or disarm) the interactive `ask` round trip. Only interactive
    /// frontends (the TUI) call this with `true`; print/JSON/RPC/REPL keep
    /// `ask` rejecting with "ask requires an interactive session".
    pub fn set_ask_interactive(&self, interactive: bool) {
        self.inner.shared.ask.set_interactive(interactive);
    }

    /// Override the answer-wait bound for pending `ask` questions (default 60s).
    pub fn set_ask_timeout(&self, timeout: Duration) {
        self.inner.shared.ask.set_timeout(timeout);
    }

    /// The currently pending `ask` as `(id, prompt)`, if any.
    #[must_use]
    pub fn pending_ask(&self) -> Option<(String, String)> {
        self.inner.shared.ask.pending()
    }

    /// Deliver the user's answer to the pending `ask` question.
    pub fn answer_ask(&self, id: &str, answer: String) -> Result<()> {
        self.inner.shared.ask.answer(id, answer)
    }

    /// Cancel the pending `ask` question (Esc / shutdown).
    pub fn cancel_ask(&self, id: &str) -> Result<()> {
        self.inner.shared.ask.cancel(id)
    }

    /// Cancel whatever question is pending, regardless of id (TUI shutdown).
    /// Returns whether a pending ask was cancelled.
    pub fn cancel_pending_ask(&self) -> bool {
        self.inner.shared.ask.cancel_pending()
    }
    pub async fn follow_up(&self, message: Message) -> Result<()> {
        // Fail-closed delegation before enqueue; the ORIGINAL message (image
        // intact) is queued so history/UI keep the image, and the model-context
        // projection hits the delegation cache when the queue drains.
        self.delegate_vision_message(message.clone()).await?;
        self.journal_steering_marker(true, &message).await?;
        self.inner.agent.follow_up(message).await;
        self.publish_queue_update().await;
        Ok(())
    }

    /// Journals a `pi.steering.pending` marker for an accepted queued message
    /// and registers it for correlation with the consuming `MessageEnd`.
    /// Sessions without a recording skip the marker (nothing durable exists to
    /// lose); a journal write failure fails the accept so the caller never
    /// sees an ack for a message that is not durable.
    async fn journal_steering_marker(&self, follow_up: bool, message: &Message) -> Result<()> {
        let Some(recorder) = self.inner.shared.recorder.lock().as_ref().cloned() else {
            return Ok(());
        };
        let marker_id = Uuid::now_v7().to_string();
        recorder
            .record_custom_entry(
                STEERING_PENDING_CUSTOM_TYPE,
                Some(serde_json::json!({
                    "markerId": marker_id,
                    "queue": if follow_up { "followUp" } else { "steer" },
                    "message": message,
                })),
            )
            .map_err(|error| anyhow!("journaling steering marker: {error}"))?;
        self.inner
            .shared
            .pending_steering_markers
            .lock()
            .push(SteeringJournalMarker {
                marker_id,
                follow_up,
                message: message.clone(),
            });
        Ok(())
    }

    /// Retires the marker matching a delivered message: writes the
    /// `pi.steering.delivered` record and removes the in-memory registration.
    /// A failed journal write keeps the marker registered — the journal still
    /// shows the message as undelivered, so resume must requeue it rather than
    /// lose it (idempotent: the marker leaves the list exactly once).
    /// Returns whether a queued message was consumed — the queue just shrank,
    /// so the caller republishes the live queue (the boundary drain happens
    /// AFTER the `TurnEnd` publish, so this post-consumption publish is the
    /// corrected post-drain projection).
    async fn consume_steering_marker(&self, message: &Message) -> bool {
        let marker = {
            let mut markers = self.inner.shared.pending_steering_markers.lock();
            let index = markers
                .iter()
                .position(|marker| &marker.message == message);
            match index {
                Some(index) => markers.remove(index),
                None => return false,
            }
        };
        let Some(recorder) = self.inner.shared.recorder.lock().as_ref().cloned() else {
            return true;
        };
        let data = serde_json::json!({ "markerId": marker.marker_id, "reason": "delivered" });
        if recorder
            .record_custom_entry(STEERING_DELIVERED_CUSTOM_TYPE, Some(data))
            .is_err()
        {
            self.inner
                .shared
                .pending_steering_markers
                .lock()
                .insert(0, marker);
        }
        true
    }

    /// Retire every still-pending steering marker with a delivered-class
    /// record and clear the list. Required before ANY re-delivery that does
    /// not replay the marker's exact recorded message (send-now combined
    /// prompt, load_history/rewind/cutover re-steer): consume_steering_marker
    /// matches by value, so a retired-here marker can never go stale and be
    /// requeued as a duplicate on resume.
    pub async fn retire_pending_steering_markers(&self, reason: &str) {
        self.retire_pending_steering_markers_filtered(reason, None).await;
    }

    /// Retire only the markers of ONE queue (`follow_up: Some(false)` =
    /// steering, `Some(true)` = follow-up). send-now drains only the
    /// steering queue; retiring all markers would strand a still-queued
    /// follow-up whose marker is its only durable record.
    pub async fn retire_pending_steering_markers_filtered(
        &self,
        reason: &str,
        follow_up: Option<bool>,
    ) {
        let selected: Vec<SteeringJournalMarker> = {
            let mut markers = self.inner.shared.pending_steering_markers.lock();
            let (selected, remaining): (Vec<_>, Vec<_>) = markers
                .drain(..)
                .partition(|marker| follow_up.is_none_or(|queue| marker.follow_up == queue));
            *markers = remaining;
            selected
        };
        let Some(recorder) = self.inner.shared.recorder.lock().as_ref().cloned() else {
            return;
        };
        for marker in selected {
            let data = serde_json::json!({ "markerId": marker.marker_id, "reason": reason });
            let _ = recorder.record_custom_entry(STEERING_DELIVERED_CUSTOM_TYPE, Some(data));
        }
    }

    /// Scans a freshly attached recording for steering markers that were never
    /// retired and requeues their messages into the steering/follow-up queues.
    ///
    /// `drained_steering`/`drained_follow_up` are the old incarnation's
    /// in-memory queue contents drained before the transcript-replacing
    /// install; they requeue first (original FIFO order), deduped against the
    /// journal's still-undelivered markers by value so the same acknowledged
    /// message is never queued twice. Every requeued message is journaled
    /// fresh through `steer`/`follow_up` and every stale marker is retired
    /// with a `requeued` record, so a resumed session delivers a crash-lost
    /// steering message exactly once.
    async fn requeue_undelivered_steering(
        &self,
        recorder: &SessionRecorder,
        drained_steering: Vec<Message>,
        drained_follow_up: Vec<Message>,
    ) -> Result<()> {
        // Markers registered by the previous incarnation belong to the
        // previous journal; the freshly attached tree is authoritative.
        self.inner.shared.pending_steering_markers.lock().clear();
        let tree = recorder.tree()?;
        let mut undelivered: Vec<(String, bool, Message)> = Vec::new();
        for entry in tree.branch(None) {
            if entry.entry_type != "custom" {
                continue;
            }
            match entry.custom_type.as_deref() {
                Some(STEERING_PENDING_CUSTOM_TYPE) => {
                    let data = entry.data.as_ref();
                    let marker_id = data
                        .and_then(|data| data.get("markerId"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                    let message = data
                        .and_then(|data| data.get("message"))
                        .and_then(|message| {
                            serde_json::from_value::<Message>(message.clone()).ok()
                        });
                    let follow_up = data
                        .and_then(|data| data.get("queue"))
                        .and_then(serde_json::Value::as_str)
                        == Some("followUp");
                    match (marker_id, message) {
                        (Some(marker_id), Some(message)) => {
                            undelivered.push((marker_id, follow_up, message));
                        }
                        _ => eprintln!(
                            "session_steering category=malformed_marker entry_id={}",
                            entry.id
                        ),
                    }
                }
                Some(STEERING_DELIVERED_CUSTOM_TYPE) => {
                    let marker_id = entry
                        .data
                        .as_ref()
                        .and_then(|data| data.get("markerId"))
                        .and_then(serde_json::Value::as_str);
                    if let Some(marker_id) = marker_id {
                        undelivered.retain(|(id, _, _)| id != marker_id);
                    }
                }
                _ => {}
            }
        }
        // Retire every undelivered journal marker: its message requeues below
        // (with a fresh marker), so the stale entry must not requeue again on
        // the next resume.
        for (marker_id, _, _) in &undelivered {
            let data = serde_json::json!({ "markerId": marker_id, "reason": "requeued" });
            recorder
                .record_custom_entry(STEERING_DELIVERED_CUSTOM_TYPE, Some(data))
                .map_err(|error| anyhow!("retiring undelivered steering marker: {error}"))?;
        }
        let mut plan: Vec<(bool, Message)> = Vec::new();
        for message in drained_steering {
            plan.push((false, message));
        }
        for message in drained_follow_up {
            plan.push((true, message));
        }
        for (_, follow_up, message) in undelivered {
            if !plan.iter().any(|(_, existing)| existing == &message) {
                plan.push((follow_up, message));
            }
        }
        for (follow_up, message) in plan {
            if follow_up {
                self.follow_up(message).await?;
            } else {
                self.steer(message).await?;
            }
        }
        Ok(())
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

    async fn begin_run(&self, subject: &str) -> Result<ClaimedRun> {
        // Claim first so lifecycle hooks only fire for runs that actually start.
        let guard = self.claim_exclusive()?;
        if self
            .inner
            .shared
            .session_started
            .swap(true, Ordering::AcqRel)
            == false
        {
            // First activity of this session: fire the session_start hook once.
            // Session::new is synchronous, so the hook cannot fire until the
            // session actually begins processing.
            self.fire_host_hook(crate::HookEvent::SessionStart, Some("session"), None)
                .await;
        }
        self.fire_host_hook(crate::HookEvent::TurnStart, Some(subject), None)
            .await;
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
        let session = self.clone();
        let recorder_subscription = self
            .inner
            .agent
            .subscribe_simple(move |event| {
                let shared = shared.clone();
                let session = session.clone();
                async move {
                    // The agent loop re-polls the steering/follow-up queues at
                    // every turn boundary — the drain runs between `TurnEnd`
                    // and the next `TurnStart`, so a `TurnStart` publish is
                    // the exact POST-DRAIN projection: the composer's
                    // `⟦steering⟧` preview and `⚙ N` count drop the moment
                    // the consumed messages are handed to the running turn.
                    // The `TurnEnd` publish (taken before the drain) stays
                    // accurate-at-publish-time; with the publish lock the
                    // later, fresher snapshot always wins.
                    if matches!(
                        event,
                        AgentEvent::TurnStart | AgentEvent::TurnEnd { .. }
                    ) {
                        session.publish_queue_update().await;
                    }
                    if let AgentEvent::MessageEnd { message } = event {
                        if let Some(recorder) = shared.recorder.lock().as_ref().cloned() {
                            let id = match &message {
                                Message::Custom(custom) => recorder.record_custom_message(custom),
                                _ => recorder.record_message(&message),
                            }
                            .map_err(|error| anyhow!("recording session message: {error}"))?;
                            if let Some(entry) = recorder
                                .tree()?
                                .entries
                                .into_iter()
                                .find(|entry| entry.id == id)
                            {
                                let _ = shared.events.send(SessionEvent::EntryAppended { entry });
                            }
                        }
                        // Flip the accepted-steer journal marker to delivered:
                        // the message is now durably recorded. A consumed
                        // queued message republishes the live queue too — a
                        // correction for any stale snapshot published before
                        // the drain completed.
                        if session.consume_steering_marker(&message).await {
                            session.publish_queue_update().await;
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
        self.fire_host_hook(crate::HookEvent::TurnEnd, Some("assistant"), None)
            .await;
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
            let _ = self
                .perform_compaction(CompactionReason::Threshold, false, None, None)
                .await;
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
        // Last Session clone dropped: drop the process-global Responses chain
        // entry for this session. The stored previous_response_id references a
        // conversation that no longer has a live session; the bounded chain
        // map in pi-ai would evict it eventually, but removing it now makes
        // the lifecycle deterministic and keeps a recreated session with the
        // same id from chaining from a stale response.
        pi_ai::providers::reset_responses_chain(&self.shared.session_id);
        // Last Session clone dropped: release any remaining detached spills.
        cleanup_bash_spills(&self.shared);
        // Fire the session_end hook asynchronously. Drop cannot await, so run
        // the hook on a dedicated thread; `HostHooks` is self-contained (cwd,
        // session id, entries), so it is safe after the session runtime drops.
        // If no runtime can be built (process teardown), the hook is skipped.
        if let Some(hooks) = self.shared.host_hooks.read().clone() {
            std::thread::spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                runtime.block_on(hooks.fire(crate::HookEvent::SessionEnd, Some("session"), None));
            });
        }
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
/// Wrap a before-tool-call hook so host `pre_tool_call` hooks fire before
/// extension tool calls pass straight through to the supplied hook.
fn compose_host_pre_tool_call(
    shared: Arc<SessionRuntime>,
    hook: Option<BeforeToolCallFn>,
) -> Option<BeforeToolCallFn> {
    let host: BeforeToolCallFn = Arc::new(move |context| {
        let shared = shared.clone();
        Box::pin(async move {
            if shared
                .extension_tool_names
                .read()
                .contains(&context.tool_call.name)
            {
                return Ok(pi_agent::BeforeToolCallResult::default());
            }
            let Some(hooks) = shared.host_hooks.read().clone() else {
                return Ok(pi_agent::BeforeToolCallResult::default());
            };
            let payload = crate::HookToolPayload {
                name: &context.tool_call.name,
                arguments: Some(&context.arguments),
                result_text: None,
                is_error: false,
            };
            let decision = hooks
                .fire(
                    crate::HookEvent::PreToolCall,
                    Some(&context.tool_call.name),
                    Some(&payload),
                )
                .await;
            Ok(pi_agent::BeforeToolCallResult {
                block: decision.block,
                reason: decision.reason,
                arguments: None,
                approval_token: None,
            })
        }) as pi_agent::BoxFuture<anyhow::Result<pi_agent::BeforeToolCallResult>>
    });
    pi_agent::compose_before_tool_call(Some(host), hook)
}


/// Wrap an after-tool-call hook so host `post_tool_call` hooks fire last.
///
/// The host hook observes the final result (after extension reduction) and is
/// advisory: its output never mutates the tool result (except for doom-loop
/// recovery, which replaces the result with an actionable stop message).
/// Extension tool calls are excluded from host hooks in the MVP.
fn compose_host_post_tool_call(
    shared: Arc<SessionRuntime>,
    hook: Option<AfterToolCallFn>,
) -> Option<AfterToolCallFn> {
    let host: AfterToolCallFn = Arc::new(move |context| {
        let shared = shared.clone();
        Box::pin(async move {
            fire_host_post_tool_call(&shared, &context).await;
            doom_loop_recovery(&shared.doom_loop, &context).await
        }) as pi_agent::BoxFuture<anyhow::Result<pi_agent::AfterToolCallResult>>
    });
    compose_after_tool_call(hook, Some(host))
}
/// Detached host-post wrapper over a FORK-TIME snapshot of the host hook
/// configuration: the fork fires the `HostHooks` Arc captured at
/// `fork_side_chat`, never a live main `set_host_hooks` replacement, and
/// never the Main doom-loop tracker — recovery runs against the fork's own
/// isolated [`DetachedDoomLoopState`]. The raw hook runs first; the host
/// observer is advisory.
fn compose_host_post_tool_call_detached_frozen(
    hooks: Option<Arc<HostHooks>>,
    extension_names: Arc<HashSet<String>>,
    hook: Option<AfterToolCallFn>,
    doom_loop: Arc<DetachedDoomLoopState>,
) -> Option<AfterToolCallFn> {
    let host: AfterToolCallFn = Arc::new(move |context| {
        let hooks = hooks.clone();
        let extension_names = extension_names.clone();
        let doom_loop = doom_loop.clone();
        Box::pin(async move {
            if let Some(hooks) = hooks {
                let is_extension = extension_names.contains(&context.tool_call.name);
                if !is_extension {
                    fire_host_post_tool_call_with(&hooks, &context).await;
                }
            }
            detached_doom_loop_recovery(&doom_loop, &context).await
        }) as pi_agent::BoxFuture<anyhow::Result<pi_agent::AfterToolCallResult>>
    });
    compose_after_tool_call(hook, Some(host))
}
/// Detached (fork-time) host `pre_tool_call`: frozen host wrapper composed
/// once with the fork-time raw policy chain (user + extension reducer).
fn compose_host_pre_tool_call_frozen(
    hooks: Option<Arc<HostHooks>>,
    extension_names: Arc<HashSet<String>>,
    policy: Option<BeforeToolCallFn>,
) -> Option<BeforeToolCallFn> {
    let host: BeforeToolCallFn = Arc::new(move |context| {
        let hooks = hooks.clone();
        let extension_names = extension_names.clone();
        Box::pin(async move {
            let Some(hooks) = hooks else {
                return Ok(pi_agent::BeforeToolCallResult::default());
            };
            if extension_names.contains(&context.tool_call.name) {
                return Ok(pi_agent::BeforeToolCallResult::default());
            }
            let payload = crate::HookToolPayload {
                name: &context.tool_call.name,
                arguments: Some(&context.arguments),
                result_text: None,
                is_error: false,
            };
            let decision = hooks
                .fire(
                    crate::HookEvent::PreToolCall,
                    Some(&context.tool_call.name),
                    Some(&payload),
                )
                .await;
            Ok(pi_agent::BeforeToolCallResult {
                block: decision.block,
                reason: decision.reason,
                arguments: None,
                approval_token: None,
            }) as anyhow::Result<pi_agent::BeforeToolCallResult>
        }) as pi_agent::BoxFuture<anyhow::Result<pi_agent::BeforeToolCallResult>>
    });
    pi_agent::compose_before_tool_call(Some(host), policy)
}

async fn fire_host_post_tool_call(
    shared: &SessionRuntime,
    context: &pi_agent::AfterToolCallContext,
) {
    let hooks = shared.host_hooks.read().clone();
    if let Some(hooks) = hooks {
        let is_extension = shared
            .extension_tool_names
            .read()
            .contains(&context.tool_call.name);
        if !is_extension {
            fire_host_post_tool_call_with(&hooks, &context).await;
        }
    }
}

async fn fire_host_post_tool_call_with(
    hooks: &HostHooks,
    context: &pi_agent::AfterToolCallContext,
) {
    let result_text = summarize_tool_result(&context.result);
    let payload = crate::HookToolPayload {
        name: &context.tool_call.name,
        arguments: Some(&context.arguments),
        result_text: result_text.as_deref(),
        is_error: context.is_error,
    };
    hooks
        .fire(
            crate::HookEvent::PostToolCall,
            Some(&context.tool_call.name),
            Some(&payload),
        )
        .await;
}
/// Observe one executed tool outcome for doom-loop recovery and return the
/// `AfterToolCallResult` to apply. A workspace-escape rejection stops the
/// turn on the first error (the remedy is host-level, so no retry — typed or
/// shell — can help); other deterministic failures stop on the second
/// identical attempt (`DOOM_LOOP_DETERMINISTIC_THRESHOLD`) and generic
/// failures on the third (`DOOM_LOOP_THRESHOLD`). Once a threshold trips, the
/// result is replaced with an actionable stop message and the batch
/// terminates, ending the turn instead of letting the model retry the same
/// failing call forever.
async fn doom_loop_recovery(
    tracker: &Mutex<DoomLoopTracker>,
    context: &pi_agent::AfterToolCallContext,
) -> Result<pi_agent::AfterToolCallResult> {
    let mut tracker = tracker.lock();
    // Once tripped, every further tool outcome in this turn terminates with
    // the same message so a parallel batch cannot escape the stop.
    if let Some(message) = &tracker.triggered_message {
        return Ok(pi_agent::AfterToolCallResult {
            content: Some(vec![ContentBlock::text(message.clone())]),
            terminate: Some(true),
            ..Default::default()
        });
    }
    let Some(text) = doom_loop_error_text(&context.result) else {
        tracker.current = None;
        return Ok(pi_agent::AfterToolCallResult::default());
    };
    let prefix = doom_loop_error_fingerprint(&text);
    if !context.is_error {
        // Any success breaks the failure run.
        tracker.current = None;
        return Ok(pi_agent::AfterToolCallResult::default());
    }
    let class = classify_tool_error(&text);
    if class == ToolErrorClass::Recoverable
        && TRANSIENT_TOOL_ERROR_MARKERS
            .iter()
            .any(|marker| prefix.contains(marker))
    {
        // Transient network/timeout blips are not doom loops: the same call
        // may succeed on the next attempt, so they never count toward the
        // threshold (and reset any prior run). Deterministic classification
        // takes precedence so phrases like `Invalid network arguments` still
        // stop on the second identical retry.
        tracker.current = None;
        return Ok(pi_agent::AfterToolCallResult::default());
    }
    if class == ToolErrorClass::WorkspaceEscape {
        // The first workspace-escape error terminates the turn immediately:
        // an identical retry can never succeed, and the fix — adding the
        // project root or selecting the project — is host-level, so neither a
        // second typed attempt nor a fallback shell search is worth a turn.
        // Pinning `triggered_message` also stops any parallel batch call.
        let message = doom_loop_message(&context.tool_call.name, 1, class);
        tracker.triggered_message = Some(message.clone());
        tracker.current = None;
        return Ok(pi_agent::AfterToolCallResult {
            content: Some(vec![ContentBlock::text(message)]),
            terminate: Some(true),
            ..Default::default()
        });
    }
    let tool = context.tool_call.name.clone();
    let args_hash = doom_args_fingerprint(&context.arguments);
    let threshold = if class == ToolErrorClass::Deterministic {
        DOOM_LOOP_DETERMINISTIC_THRESHOLD
    } else {
        DOOM_LOOP_THRESHOLD
    };
    let same_run = tracker.current.as_ref().is_some_and(|state| {
        state.tool == tool && state.args_hash == args_hash && state.error_prefix == prefix
    });
    if same_run && let Some(state) = tracker.current.as_mut() {
        state.count += 1;
        if state.count >= threshold {
            let message = doom_loop_message(&tool, state.count, class);
            tracker.triggered_message = Some(message.clone());
            tracker.current = None;
            return Ok(pi_agent::AfterToolCallResult {
                content: Some(vec![ContentBlock::text(message)]),
                terminate: Some(true),
                ..Default::default()
            });
        }
    } else {
        tracker.current = Some(DoomLoopState {
            tool,
            args_hash,
            error_prefix: prefix,
            count: 1,
        });
    }
    Ok(pi_agent::AfterToolCallResult::default())
}

fn doom_loop_error_text(result: &pi_agent::AgentToolResult) -> Option<String> {
    let text = summarize_tool_result(result)?;
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    let mut full: String = collapsed;
    full.make_ascii_lowercase();
    Some(full)
}

fn doom_loop_error_fingerprint(text: &str) -> String {
    text.chars().take(DOOM_LOOP_ERROR_PREFIX_CHARS).collect()
}

fn doom_loop_error_prefix(result: &pi_agent::AgentToolResult) -> Option<String> {
    doom_loop_error_text(result).map(|text| doom_loop_error_fingerprint(&text))
}

/// SHA-256 of the recursively key-sorted canonical JSON argument payload.
/// Only the digest is stored in doom-loop state so failed-tool-call payloads
/// are never retained.
fn doom_args_fingerprint(arguments: &serde_json::Value) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"pi-doom-loop-args-v1\0");
    hash_canonical_json(&mut hasher, arguments);
    hasher.finalize().into()
}

/// Canonical JSON: object keys sort recursively; scalar leaves serialize
/// exactly as `serde_json` emits them.
fn hash_canonical_json(hasher: &mut Sha256, value: &serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            entries.sort_by(|(left, _), (right, _)| (*left).cmp(*right));
            hasher.update(b"{");
            for (offset, (key, value)) in entries.iter().enumerate() {
                if offset > 0 {
                    hasher.update(b",");
                }
                hash_canonical_json(hasher, &serde_json::Value::String((*key).clone()));
                hasher.update(b":");
                hash_canonical_json(hasher, value);
            }
            hasher.update(b"}");
        }
        serde_json::Value::Array(items) => {
            hasher.update(b"[");
            for (offset, item) in items.iter().enumerate() {
                if offset > 0 {
                    hasher.update(b",");
                }
                hash_canonical_json(hasher, item);
            }
            hasher.update(b"]");
        }
        other => {
            hasher.update(serde_json::to_vec(other).unwrap_or_default().as_slice());
        }
    }
}

/// Classification authority for tool-error recovery. Every recovery decision
/// (threshold and replacement message) derives from this single
/// classification — error-text matching lives here and nowhere else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolErrorClass {
    /// Workspace-escape rejection (`path_outside_workspace`,
    /// `Path escapes workspace`). Identical retries can never succeed and the
    /// remedy is host-level (`--add-dir <project-root>` or selecting the
    /// project), so the first error stops the turn immediately.
    WorkspaceEscape,
    /// Deterministic argument failures such as schema rejections
    /// (`Invalid <tool> arguments`): identical arguments can never succeed,
    /// so the second identical attempt ends the turn.
    Deterministic,
    /// Everything else: the generic doom-loop threshold applies (unless the
    /// failure looks transient).
    Recoverable,
}

fn classify_tool_error(prefix: &str) -> ToolErrorClass {
    // Exact structured form: the executor emits `path_outside_workspace: …`
    // as the leading text of the rejection (see `paths.rs`), so the match is
    // start-anchored on that exact prefix. An arbitrary substring match would
    // stop the turn on the first *unrelated* error whose text merely mentions
    // the marker — docs, notes, or an echoed prior stop — which must stay
    // recoverable.
    if prefix.starts_with("path_outside_workspace:") {
        return ToolErrorClass::WorkspaceEscape;
    }
    // Exact `FsError::OutsideCapability` Display (uncapped; stem-similar text stays recoverable).
    if prefix.eq_ignore_ascii_case(FS_OUTSIDE_CAPABILITY_ERROR) {
        return ToolErrorClass::WorkspaceEscape;
    }
    // Legacy free-form rejection, narrowly anchored to the message start so
    // prose that merely contains the phrase elsewhere stays recoverable.
    if prefix.starts_with("path escapes workspace") {
        return ToolErrorClass::WorkspaceEscape;
    }
    let words: Vec<&str> = prefix.split_ascii_whitespace().collect();
    if words.len() >= 3 && words[0] == "invalid" && words[2].trim_end_matches(':') == "arguments" {
        return ToolErrorClass::Deterministic;
    }
    ToolErrorClass::Recoverable
}

/// Bounded, actionable stop message per class. The workspace-escape text
/// never echoes the rejected location: the remedy (`--add-dir
/// <project-root>` or selecting the project) is host-level, not
/// path-specific, and rejected paths may be sensitive.
fn doom_loop_message(tool: &str, count: usize, class: ToolErrorClass) -> String {
    match class {
        ToolErrorClass::WorkspaceEscape => format!(
            "path_outside_workspace: /{tool} was rejected — the target lies outside the authorized workspace roots and retrying can never succeed; add the needed project root with --add-dir <project-root> or select that project in the client, then /undo"
        ),
        ToolErrorClass::Deterministic => format!(
            "repeated identical /{tool} arguments ({count}× identical errors) — stopping; change arguments or work within the authorized workspace roots, or /undo"
        ),
        ToolErrorClass::Recoverable => format!(
            "repeated failure ({count}× identical /{tool} errors) — stopping; try a different approach or /undo"
        ),
    }
}

/// Isolated doom-loop state for a detached fork (side chat / code review).
///
/// The fork tracks its own identical-failure runs and workspace-escape stops
/// here — it never reads or writes Main's `shared.doom_loop` tracker, so a
/// doom loop in the fork can neither poison a Main run nor be pre-triggered
/// by one. `turn` is the detached turn identity the state is currently
/// scoped to: Main resets its tracker at the start of every `Session::run`,
/// and the detached mirror resets here when the next `agent.prompt` turn
/// begins.
#[derive(Default)]
struct DetachedDoomLoopState {
    turn: Mutex<Option<usize>>,
    tracker: Mutex<DoomLoopTracker>,
}

/// Turn identity for a detached fork: the number of user messages in the
/// agent context. `agent.prompt` appends exactly one user message per turn
/// and serializes turns through the run claim, while within a turn only
/// assistant and tool-result messages are appended — so the count changes
/// exactly at turn boundaries.
fn detached_doom_loop_turn_key(context: &pi_agent::AgentContext) -> usize {
    context
        .messages
        .iter()
        .filter(|message| matches!(message, Message::User(_)))
        .count()
}

/// Detached mirror of [`doom_loop_recovery`] over the fork's isolated state:
/// drops the previous turn's pinned stop and failure run when the first tool
/// outcome of a new turn arrives, then applies the shared recovery rules.
async fn detached_doom_loop_recovery(
    state: &DetachedDoomLoopState,
    context: &pi_agent::AfterToolCallContext,
) -> Result<pi_agent::AfterToolCallResult> {
    let turn = detached_doom_loop_turn_key(&context.context);
    {
        let mut scoped = state.turn.lock();
        if *scoped != Some(turn) {
            *scoped = Some(turn);
            state.tracker.lock().reset();
        }
    }
    doom_loop_recovery(&state.tracker, &context).await
}

/// Compose two after-tool-call hooks: `first` runs, then `second` observes the
/// possibly-modified result. Explicit later fields win over the first hook's;
/// fields the later hook omits retain the first hook's values.
fn compose_after_tool_call(
    first: Option<AfterToolCallFn>,
    second: Option<AfterToolCallFn>,
) -> Option<AfterToolCallFn> {
    match (first, second) {
        (None, None) => None,
        (Some(hook), None) | (None, Some(hook)) => Some(hook),
        (Some(first), Some(second)) => Some(Arc::new(move |context| {
            let first = first.clone();
            let second = second.clone();
            Box::pin(async move {
                let update = first(context.clone()).await?;
                let mut next = context;
                if let Some(content) = update.content.clone() {
                    next.result.content = content;
                }
                if let Some(details) = update.details.clone() {
                    next.result.details = details;
                }
                if let Some(is_error) = update.is_error {
                    next.is_error = is_error;
                }
                let later = second(next).await?;
                // Later explicit fields win; fields the later hook omitted
                // retain the first hook's values instead of blanking them.
                Ok(AfterToolCallResult {
                    content: later.content.or(update.content),
                    details: later.details.or(update.details),
                    is_error: later.is_error.or(update.is_error),
                    usage: later.usage.or(update.usage),
                    terminate: later.terminate.or(update.terminate),
                })
            })
        })),
    }
}

/// Plain-text summary of a tool result for the `post_tool_call` payload.
fn summarize_tool_result(result: &pi_agent::AgentToolResult) -> Option<String> {
    let mut text = String::new();
    for block in &result.content {
        if let pi_ai::ContentBlock::Text {
            text: block_text, ..
        } = block
        {
            text.push_str(block_text);
            text.push('\n');
        }
    }
    if text.is_empty() { None } else { Some(text) }
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
    let available = tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<BTreeSet<_>>();
    if let Some(unknown) = selection
        .allow
        .iter()
        .flatten()
        .chain(&selection.deny)
        .find(|name| !available.contains(name.as_str()))
    {
        return Err(anyhow!(
            "unknown requested tool {unknown:?}; available tools: {}",
            available.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    let allow = selection
        .allow
        .as_ref()
        .map(|names| names.iter().map(String::as_str).collect::<BTreeSet<_>>());
    let deny = selection
        .deny
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    Ok(tools
        .into_iter()
        .filter(|tool| {
            if selection.disable_all {
                return false;
            }
            if selection.disable_builtins && crate::TOOL_NAMES.contains(&tool.name.as_str()) {
                return false;
            }
            allow
                .as_ref()
                .is_none_or(|allow| allow.contains(tool.name.as_str()))
                && !deny.contains(tool.name.as_str())
        })
        .collect())
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

/// The built-in memory tool names across backends.
const MEMORY_TOOL_NAMES: [&str; 4] = ["memory", "recall", "retain", "reflect"];

/// Replaces the memory tools in `tools` with the set for the effective memory
/// backend: `off` removes every memory tool, `local` keeps only the built-in
/// `memory` tool, `hindsight` swaps in `recall`/`retain`/`reflect`. Runs on
/// resource attach and reload so `settings.memory.backend` changes take
/// effect on the next turn. Tools named after memory built-ins but added by an
/// extension are left alone when no built-in memory tool is present.
fn reconcile_memory_tools(
    mut tools: Vec<AgentTool>,
    config: crate::MemoryConfig,
    cwd: &Path,
    session_env: Option<crate::SessionEnvFn>,
    capture: Option<crate::tools::GenerationCaptureFn>,
) -> Vec<AgentTool> {
    if !tools
        .iter()
        .any(|tool| MEMORY_TOOL_NAMES.contains(&tool.name.as_str()))
    {
        return tools;
    }
    tools.retain(|tool| !MEMORY_TOOL_NAMES.contains(&tool.name.as_str()));
    let cwd = cwd.to_string_lossy();
    tools.extend(crate::memory::memory_tools_for_with_generation(
        &cwd,
        session_env,
        Some(config),
        capture,
    ));
    tools
}

/// Prepends the hindsight memory injection message (hidden custom context) to
/// the turn's messages.
fn prepend_hindsight_memory(mut messages: Vec<Message>, body: &str) -> Vec<Message> {
    messages.insert(0, crate::memory::hindsight_injection_message(body));
    messages
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
        Poll::Pending => Err(anyhow!(
            "agent subscription registration unexpectedly yielded"
        )),
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

/// Fail-closed stream for a provider lookup that could not resolve within the
/// session's extension runtime namespace. The error names the api and the
/// owning runtimes, so a misconfigured session fails actionably instead of
/// silently streaming through another runtime.
async fn provider_scope_error_stream(
    model: &Model,
    error: &pi_ai::ProviderScopeError,
) -> pi_ai::AssistantMessageEventStream {
    auth_error_stream(model, format!("{error:#}")).await
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

/// Writes the lossless snapcompact archive: the original replaced transcript
/// entries as plain JSONL records (same serialization as the session file, no
/// header) to `<session-file-name>.snapcompact-<utc-rfc3339-millis>.jsonl` in
/// the session file's directory — the same sidecar convention as the rewind
/// archive (`.rewind-` prefix). The file is created exclusively and fsynced
/// before returning, so a completed compaction always has a recoverable copy
/// of the archived region.
fn write_snapcompact_archive(session_path: &Path, entries: &[&SessionEntry]) -> Result<PathBuf> {
    write_snapcompact_archive_at(session_path, entries, || {
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    })
}

/// Backing implementation of [`write_snapcompact_archive`] with an injectable
/// stamp source so tests can force a same-millisecond name collision
/// deterministically. On collision the stamp is refreshed (bounded attempts)
/// instead of failing: `create_new` guarantees an existing archive — a stale
/// one from a crashed run or a same-millisecond sibling — is never
/// overwritten.
fn write_snapcompact_archive_at<F>(
    session_path: &Path,
    entries: &[&SessionEntry],
    mut stamp: F,
) -> Result<PathBuf>
where
    F: FnMut() -> String,
{
    use std::fs::OpenOptions;
    use std::io::Write as _;

    let file_name = session_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("session");
    let mut last_collision = None;
    for _ in 0..3 {
        let archive_path =
            session_path.with_file_name(format!("{file_name}.snapcompact-{}.jsonl", stamp()));
        let mut file = match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&archive_path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                last_collision = Some(error);
                continue;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("creating snapcompact archive {}", archive_path.display())
                });
            }
        };
        for entry in entries {
            serde_json::to_writer(&mut file, entry)?;
            file.write_all(b"\n")?;
        }
        file.flush()?;
        file.sync_all()?;
        return Ok(archive_path);
    }
    Err(anyhow!(
        "creating snapcompact archive for {}: exhausted name collisions ({last_collision:?})",
        session_path.display()
    ))
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
    timeout: std::time::Duration,
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
    if let Some(instructions) =
        custom_instructions.filter(|instructions| !instructions.trim().is_empty())
    {
        prompt.push_str("\n\nAdditional focus: ");
        prompt.push_str(instructions);
    }
    complete_summary(
        inner,
        prompt,
        reserve_tokens,
        fraction,
        abort,
        reason,
        timeout,
    )
    .await
}

async fn summarize_turn_prefix(
    inner: &SessionRuntime,
    messages: &[Message],
    reserve_tokens: i64,
    abort: AbortSignal,
    reason: CompactionReason,
    timeout: std::time::Duration,
) -> Result<String> {
    let prompt = format!(
        "<conversation>\n{}\n</conversation>\n\n{TURN_PREFIX_SUMMARIZATION_PROMPT}",
        serialize_conversation(&messages_as_llm(messages))
    );
    complete_summary(
        inner,
        prompt,
        reserve_tokens,
        0.5,
        abort,
        Some(reason),
        timeout,
    )
    .await
}

/// Outcome of a single summarization provider call (no retry logic).
enum SummaryAttemptOutcome {
    /// Provider returned a usable summary.
    Done(String),
    /// The call was cancelled (abort or provider-aborted stop reason).
    Cancelled,
    /// The call failed. `retryable` mirrors the compaction retry policy:
    /// provider-returned error stop reasons may be retried; transport
    /// timeouts and empty responses are hard failures.
    Failed { message: String, retryable: bool },
}

/// Runs a single bounded summarization provider call — no retries.
///
/// Shared by the compaction/branch-summary retry loop and the one-shot
/// handoff-prose path. The whole provider exchange (stream creation + drain +
/// result) is bounded by `timeout`: provider SSE bodies are intentionally
/// uncapped in `pi-ai` (only time-to-headers is bounded), so a server that
/// sends headers and then never terminates the body would otherwise hang the
/// caller forever.
async fn run_summary_provider_call(
    inner: &SessionRuntime,
    prompt: &str,
    system_prompt: &str,
    reserve_tokens: i64,
    fraction: f64,
    abort: AbortSignal,
    timeout: std::time::Duration,
) -> SummaryAttemptOutcome {
    let (model, thinking_level, fallback_api_key) = {
        let state = inner.state.read();
        (
            state.model.clone(),
            state.thinking_level,
            state.api_key.clone(),
        )
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
    } else {
        None
    };
    let requested_max = (reserve_tokens as f64 * fraction).floor() as i64;
    let max_tokens = if model.max_tokens > 0 {
        requested_max.min(model.max_tokens).max(1)
    } else {
        requested_max.max(1)
    };
    let mut stream_options = inner.stream_options.read().clone();
    if let Some(resolver) = &inner.auth_resolver {
        let auth = match resolver(model.clone()).await {
            Ok(auth) => auth,
            Err(error) => {
                return SummaryAttemptOutcome::Failed {
                    message: format!("{error:#}"),
                    retryable: false,
                };
            }
        };
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
    // Bound the whole provider exchange (stream creation + drain + result).
    // Without this a stalled provider (headers received, body never
    // terminated — the SSE body is uncapped by design) hangs compaction
    // forever while the session's exclusive run slot stays held.
    let produced = tokio::time::timeout(timeout, async {
        let stream = (inner.stream_fn)(
            model,
            Context {
                system_prompt: system_prompt.to_owned(),
                messages: vec![Message::user_text(prompt.to_owned(), pi_ai::now_millis())],
                tools: Vec::new(),
            },
            stream_options,
        )
        .await;
        while stream.next().await.is_some() {}
        stream.result().await
    })
    .await;
    match produced {
        Ok(Some(response)) => {
            if abort.is_aborted() || response.stop_reason == StopReason::Aborted {
                return SummaryAttemptOutcome::Cancelled;
            }
            if response.stop_reason != StopReason::Error {
                return SummaryAttemptOutcome::Done(response.text());
            }
            let message = response
                .error_message
                .clone()
                .unwrap_or_else(|| "summarization failed".to_owned());
            SummaryAttemptOutcome::Failed {
                message,
                retryable: is_retryable_assistant_error(&response),
            }
        }
        Ok(None) => SummaryAttemptOutcome::Failed {
            message: "summarization returned no message".to_owned(),
            retryable: false,
        },
        Err(_) => SummaryAttemptOutcome::Failed {
            message: format!(
                "summarization timed out after {}s: the provider did not finish its response (is it still reachable?)",
                timeout.as_secs()
            ),
            retryable: false,
        },
    }
}

async fn complete_summary(
    inner: &SessionRuntime,
    prompt: String,
    reserve_tokens: i64,
    fraction: f64,
    abort: AbortSignal,
    reason: Option<CompactionReason>,
    timeout: std::time::Duration,
) -> Result<String> {
    let settings = inner.retry_settings.read().clone();
    let mut attempt = 0usize;
    loop {
        if attempt > 0 {
            let _ = inner
                .events
                .send(SessionEvent::SummarizationRetryAttemptStart {
                    source: if reason.is_some() {
                        SummarizationSource::Compaction
                    } else {
                        SummarizationSource::BranchSummary
                    },
                    reason,
                });
        }
        let outcome = run_summary_provider_call(
            inner,
            &prompt,
            SUMMARIZATION_SYSTEM_PROMPT,
            reserve_tokens,
            fraction,
            abort.clone(),
            timeout,
        )
        .await;
        match outcome {
            SummaryAttemptOutcome::Done(text) => {
                if attempt > 0 {
                    let _ = inner.events.send(SessionEvent::SummarizationRetryFinished);
                }
                return Ok(text);
            }
            SummaryAttemptOutcome::Cancelled => {
                if attempt > 0 {
                    let _ = inner.events.send(SessionEvent::SummarizationRetryFinished);
                }
                return Err(anyhow!("Compaction cancelled"));
            }
            SummaryAttemptOutcome::Failed { message, retryable } => {
                if !retryable || !settings.enabled || attempt >= settings.max_retries {
                    if attempt > 0 {
                        let _ = inner.events.send(SessionEvent::SummarizationRetryFinished);
                    }
                    return Err(anyhow!(message));
                }
                attempt += 1;
                let shift = u32::try_from(attempt.saturating_sub(1))
                    .unwrap_or(u32::MAX)
                    .min(63);
                let delay_ms = settings.base_delay_ms.saturating_mul(1u64 << shift);
                let _ = inner
                    .events
                    .send(SessionEvent::SummarizationRetryScheduled {
                        attempt,
                        max_attempts: settings.max_retries,
                        delay_ms,
                        error_message: message,
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
    }
}

fn validate_extension_message_content(content: &CustomMessageContent) -> Result<()> {
    if let CustomMessageContent::Blocks(blocks) = content {
        for block in blocks {
            if !matches!(
                block,
                ContentBlock::Text { .. } | ContentBlock::Image { .. }
            ) {
                return Err(anyhow!(
                    "extension messages support only text and image content"
                ));
            }
        }
    }
    Ok(())
}

fn navigation_target(entry: &crate::SessionEntry) -> (Option<String>, Option<String>) {
    match entry.message.as_ref() {
        Some(Message::User(message)) => (
            entry.parent_id.clone(),
            Some(content_text(&message.content)),
        ),
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

fn collect_abandoned_messages(
    tree: &crate::SessionTree,
    current_leaf: Option<&str>,
    new_leaf: Option<&str>,
) -> Vec<Message> {
    let current = current_leaf.map_or_else(Vec::new, |leaf| tree.branch(Some(leaf)));
    let target = new_leaf.map_or_else(Vec::new, |leaf| tree.branch(Some(leaf)));
    let common = current
        .iter()
        .zip(target.iter())
        .take_while(|(left, right)| left.id == right.id)
        .count();
    current[common..]
        .iter()
        .filter_map(|entry| match entry.message.as_ref() {
            Some(message) => Some(message.clone()),
            None if entry.entry_type == "branch_summary" => entry
                .summary
                .as_ref()
                .map(|summary| Message::user_text(summary.clone(), 0)),
            None => None,
        })
        .collect()
}

/// Resolves the model the selector classifier runs on.
///
/// An unconfigured (or whitespace-only) classifier model uses the session
/// model. A configured spec goes through the shared model resolver so
/// combined `provider/id` specs, bare catalog ids, case-insensitive provider
/// prefixes, OpenRouter-style slashed ids, known-provider custom ids, and an
/// optional `:<thinkingLevel>` suffix all resolve exactly like CLI model
/// specs. The parsed thinking level is discarded: the classifier prompt runs
/// with reasoning disabled and has no thinking-level knob. Resolution
/// failures return the resolver's error so the caller can surface the
/// invalid configuration instead of silently disabling classifier routing.
fn resolve_classifier_model(
    settings: &crate::SelectorSettings,
    session_model: &Model,
) -> Result<Model, String> {
    let Some(configured) = settings.classifier.model.as_deref().map(str::trim) else {
        return Ok(session_model.clone());
    };
    if configured.is_empty() {
        return Ok(session_model.clone());
    }
    // Resolver thinking levels are ignored: `classify` runs the prompt with
    // `options.reasoning = None`, so no classifier thinking level exists to
    // honor.
    crate::resolve_model_pattern(configured).map(|resolved| resolved.model)
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

/// First line of a user/assistant message's text, capped for the `/rewind`
/// picker listing. Tool and system messages have no picker preview.
fn message_first_line(message: &Message) -> Option<String> {
    let text = match message {
        Message::User(user) => content_text(&user.content),
        Message::Assistant(assistant) => content_text(&assistant.content),
        _ => String::new(),
    };
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let first_line = text.lines().next().unwrap_or(text).trim();
    let capped = first_line.chars().take(80).collect::<String>();
    (!capped.is_empty()).then_some(capped)
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

/// Resolves the model recorded at a branch cut through the shared model
/// catalog. A truly absent provider/model pair yields `None` (the cut records
/// no model); a recorded pair that the catalog cannot resolve must fail
/// closed — the fork must not silently fall back to the source runtime's
/// later live selection.
fn recorded_cut_model(context: &crate::BranchContext) -> Result<Option<Model>> {
    let Some((provider, model_id)) =
        context.provider.as_deref().zip(context.model_id.as_deref())
    else {
        return Ok(None);
    };
    if provider.is_empty() || model_id.is_empty() {
        // An unconfigured default model records empty metadata; treat it the
        // same way the recorded-entry parser does: absent.
        return Ok(None);
    }
    match pi_ai::get_model(provider, model_id) {
        Some(model) => Ok(Some(model)),
        None => Err(anyhow!(
            "recorded session model {provider}/{model_id} is not available in the model catalog"
        )),
    }
}

/// Clamps the thinking level recorded at a branch cut to the cut model's
/// capabilities, reusing the resume path's recorded-level parser and clamp.
fn recorded_cut_thinking(
    model: Option<&Model>,
    context: &crate::BranchContext,
) -> ThinkingLevel {
    let requested = parse_recorded_thinking_level(&context.thinking_level);
    model.map_or(requested, |model| clamp_thinking_level(model, requested))
}

/// Resolves the runtime policy (model, API key, clamped thinking level)
/// recorded at a branch cut — the same recovery the resume cutover applies —
/// so a fork restores the materialized cut's authoritative selection instead
/// of the source runtime's later live selection. The API key follows the
/// session's auth policy: resolved through the auth resolver when one is
/// bound, otherwise from the current state only when the cut model's provider
/// matches the live provider.
async fn recorded_cut_policy(
    shared: &SessionRuntime,
    context: &crate::BranchContext,
) -> Result<(Option<Model>, Option<String>, ThinkingLevel)> {
    let model = recorded_cut_model(context)?;
    let api_key = if let Some(model) = &model {
        if let Some(resolver) = &shared.auth_resolver {
            Some(resolver(model.clone()).await?.api_key)
        } else {
            let current = shared.state.read();
            if current.model.provider != model.provider {
                return Err(anyhow!(
                    "cannot switch extension model provider without an auth resolver"
                ));
            }
            Some(current.api_key.clone())
        }
    } else {
        None
    };
    let thinking_level = recorded_cut_thinking(model.as_ref(), context);
    Ok((model, api_key, thinking_level))
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
fn test_generation_capture() -> crate::tools::GenerationCaptureFn {
    struct Fixture {
        capture: crate::tools::GenerationCaptureFn,
        _runtime: tokio::runtime::Runtime,
        _application: crate::Application,
        _registration: pi_ai::providers::FauxProviderRegistration,
        _cwd: tempfile::TempDir,
        _agent_dir: tempfile::TempDir,
        _session_dir: tempfile::TempDir,
    }

    static FIXTURE: std::sync::LazyLock<Fixture> = std::sync::LazyLock::new(|| {
        std::thread::spawn(|| {
            let cwd = tempfile::tempdir().expect("generation cwd");
            let agent_dir = tempfile::tempdir().expect("generation agent directory");
            let session_dir = tempfile::tempdir().expect("generation session directory");
            let registration = pi_ai::providers::register_faux_provider(Default::default());
            let session = Session::new_with_additional_tools_filtered_and_discovery(
                SessionOptions {
                    model: registration.model(None).expect("generation faux model"),
                    cwd: cwd.path().to_path_buf(),
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
                },
                Vec::new(),
                ToolSelection::default(),
                ResourceDiscovery::Disabled,
            )
            .expect("generation session");
            session.set_session_dir(session_dir.path().to_path_buf());
            let resources = isolated_test_resources(cwd.path(), agent_dir.path(), "danger-full-access");
            let runtime = tokio::runtime::Runtime::new().expect("generation runtime");
            let application = runtime.block_on(async {
                session
                    .attach_resources(resources)
                    .await
                    .expect("attach generation resources");
                crate::Application::from_runtime_candidate(crate::ApplicationRuntimeCandidate::new(
                    session,
                ))
                .await
                .expect("publish generation application")
            });
            Fixture {
                capture: application.generation_capture_fn(),
                _runtime: runtime,
                _application: application,
                _registration: registration,
                _cwd: cwd,
                _agent_dir: agent_dir,
                _session_dir: session_dir,
            }
        })
        .join()
        .expect("generation fixture thread")
    });
    FIXTURE.capture.clone()
}

#[cfg(test)]
fn test_session_with_discovery(mut options: SessionOptions) -> Session {
    let cwd = if options.cwd.as_os_str().is_empty() {
        std::env::current_dir().expect("current working directory")
    } else {
        options.cwd.clone()
    };
    let workspace =
        crate::WorkspaceRoots::new(&cwd, Vec::<PathBuf>::new()).expect("workspace roots");
    test_session_with_roots(options, workspace).expect("session")
}

#[cfg(test)]
fn test_session_with_roots(
    mut options: SessionOptions,
    workspace: crate::WorkspaceRoots,
) -> Result<Session> {
    if options.model.api.is_empty() || pi_ai::get_api_provider(&options.model.api).is_none() {
        static FAUX_REGISTRATIONS: std::sync::LazyLock<
            Mutex<Vec<pi_ai::providers::FauxProviderRegistration>>,
        > = std::sync::LazyLock::new(|| Mutex::new(Vec::new()));
        let mut faux_model = options.model.clone();
        let suffix = uuid::Uuid::now_v7();
        let api = format!("session-test-api-{suffix}");
        let provider = format!("session-test-provider-{suffix}");
        faux_model.api = api.clone();
        faux_model.provider = provider.clone();
        faux_model.base_url = "http://localhost:0".to_owned();
        let registration =
            pi_ai::providers::register_faux_provider(pi_ai::providers::FauxProviderOptions {
                api,
                provider,
                models: vec![faux_model.clone()],
                chunk_size: 1,
            });
        options.model.api = faux_model.api;
        options.model.provider = faux_model.provider;
        options.model.base_url = faux_model.base_url;
        FAUX_REGISTRATIONS.lock().push(registration);
    }
    Session::new_with_inherited_generation(
        options,
        Vec::new(),
        ToolSelection::default(),
        ResourceDiscovery::Disabled,
        workspace,
        None,
        test_generation_capture(),
        true,
    )
}

#[cfg(test)]
fn isolated_test_resources(cwd: &Path, agent_dir: &Path, sandbox_mode: &str) -> ResourceManager {
    std::fs::create_dir_all(agent_dir).expect("test agent directory");
    let settings_path = agent_dir.join("settings.json");
    let mut settings = std::fs::read_to_string(&settings_path)
        .ok()
        .and_then(|contents| serde_json::from_str::<serde_json::Value>(&contents).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    settings["sandbox"] = serde_json::json!({ "mode": sandbox_mode });
    std::fs::write(
        &settings_path,
        serde_json::to_string(&settings).expect("test sandbox settings json"),
    )
    .expect("test sandbox settings");
    let mut options = crate::ResourceManagerOptions::new(cwd);
    options.agent_dir = agent_dir.to_path_buf();
    options.project_trust_override = Some(true);
    options.disable_extensions = true;
    options.disable_skills = true;
    options.disable_prompt_templates = true;
    options.disable_themes = true;
    options.disable_context_files = true;
    ResourceManager::new(options).expect("resource manager")
}
#[cfg(test)]
impl Session {
    /// Returns the agent resource root a test fixture should publish to.
    ///
    /// Callers of `publish_test_application` historically reused the session
    /// `cwd` as the retained agent directory. The production
    /// `ResourceManager` now fail-closes when the agent root resolves inside
    /// the canonical project boundary (`resource_manager.rs`), so a fixture
    /// that points the agent store at `cwd` trips that guard. When `agent_dir`
    /// is inside `cwd`'s project boundary this relocates it to a fresh
    /// system-temp directory (outside any project boundary by construction)
    /// and retains the `TempDir` on the session for the lifetime of the
    /// published `ResourceManager`. When `agent_dir` is already outside the
    /// boundary (the existing pattern: a dedicated `tempfile::tempdir()`), the
    /// path is returned unchanged so generation-fixture and direct-attach
    /// behavior is preserved.
    fn isolated_agent_dir(&self, agent_dir: &Path) -> PathBuf {
        let boundary = crate::project_context_boundary(self.cwd())
            .unwrap_or_else(|_| self.cwd().to_path_buf());
        let agent_canonical =
            std::fs::canonicalize(agent_dir).unwrap_or_else(|_| agent_dir.to_path_buf());
        if agent_canonical.starts_with(&boundary) {
            let relocated = tempfile::tempdir().expect("relocated test agent directory");
            let path = relocated.path().to_path_buf();
            *self.inner.retained_agent_storage.lock() = Some(relocated);
            path
        } else {
            agent_dir.to_path_buf()
        }
    }
}

#[cfg(test)]
async fn publish_test_application(session: &Session, agent_dir: &Path) -> crate::Application {
    publish_test_application_sandboxed(session, agent_dir, "danger-full-access").await
}

/// Variant of [`publish_test_application`] publishing the application under a
/// caller-supplied sandbox mode (`workspace-write` exercises the confined
/// `PolicyFs` authority path, danger-full-access the ambient fallback).
#[cfg(test)]
async fn publish_test_application_sandboxed(
    session: &Session,
    agent_dir: &Path,
    sandbox_mode: &str,
) -> crate::Application {
    // The shared fixture relocates the agent root outside the canonical
    // project boundary when a caller reused `cwd` (the historical pattern for
    // ~40 tests), retaining the `TempDir` on the session so neither the
    // published `ResourceManager` nor the recorded session storage dangles.
    // Already-isolated callers pass through unchanged.
    let agent_dir: &Path = &session.isolated_agent_dir(agent_dir);
    if session.session_dir() == crate::default_session_dir(session.cwd()) {
        let session_dir = agent_dir.join("sessions");
        std::fs::create_dir_all(&session_dir).expect("test session directory");
        session.set_session_dir(session_dir);
    } else {
        std::fs::create_dir_all(session.session_dir()).expect("configured test session directory");
    }
    if let Some(compaction) = session.compaction_settings() {
        let settings_path = agent_dir.join("settings.json");
        let mut settings = std::fs::read_to_string(&settings_path)
            .ok()
            .and_then(|contents| serde_json::from_str::<serde_json::Value>(&contents).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        settings["compaction"] = serde_json::json!({
            "enabled": compaction.enabled,
            "reserveTokens": compaction.reserve_tokens,
            "keepRecentTokens": compaction.keep_recent_tokens,
            "snapKeepTurns": compaction.snap_keep_turns,
        });
        std::fs::write(
            &settings_path,
            serde_json::to_string(&settings).expect("test settings json"),
        )
        .expect("test compaction settings");
    }
    let ambient_tools = session
        .get_all_tools()
        .into_iter()
        .filter(|tool| tool.name == "ask")
        .collect::<Vec<_>>();
    let resources = isolated_test_resources(session.cwd(), agent_dir, sandbox_mode);
    let update = session
        .prepare_resource_update(resources.snapshot(), ambient_tools.clone())
        .expect("prepare isolated resources");
    session.publish_resource_manager(resources, update).await;
    let application = publish_attached_test_application(session).await;
    if !ambient_tools.is_empty() {
        let resources = session
            .resource_manager()
            .expect("published test resources");
        let update = session
            .prepare_resource_update(resources.snapshot(), ambient_tools)
            .expect("restore ambient test tools");
        session.commit_resource_update(update).await;
    }
    application
}

#[cfg(test)]
async fn publish_attached_test_application(session: &Session) -> crate::Application {
    let session_dir = session.session_dir();
    if !session_dir.exists() {
        std::fs::create_dir_all(&session_dir).expect("attached test session directory");
    }
    let application = crate::Application::from_runtime_candidate(
        crate::ApplicationRuntimeCandidate::new(session.clone()),
    )
    .await
    .expect("publish test application");
    session.bind_generation_capture(application.generation_capture_fn());
    application
}

#[cfg(test)]
mod inherited_generation_workspace_tests {
    use super::*;

    /// A child session built through the runtime's construction path
    /// (`new_with_inherited_generation` with an explicit `WorkspaceRoots`)
    /// must expose the admitted additional root through
    /// `workspace_roots()` (tool authority) and admit it in the system
    /// prompt exactly once (prompt authority), so the two stay consistent.
    #[tokio::test]
    async fn inherited_generation_session_admits_additional_root_in_tools_and_prompt() {
        let cwd = tempfile::tempdir().expect("cwd");
        let additional = tempfile::tempdir().expect("additional root");
        let workspace = crate::WorkspaceRoots::new(cwd.path(), [additional.path().to_path_buf()])
            .expect("workspace roots");
        let session = test_session_with_roots(
            SessionOptions {
                model: Model::default(),
                cwd: cwd.path().to_path_buf(),
                system_prompt: "child prompt".to_owned(),
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
            },
            workspace,
        )
        .expect("session with additional root");
        assert_eq!(
            session.workspace_roots().additional_roots(),
            [additional.path().to_path_buf()],
            "child workspace_roots must expose the admitted additional root"
        );
        let prompt = session.system_prompt().await;
        let additional_path = additional.path().to_string_lossy().into_owned();
        assert!(
            prompt.contains(&additional_path),
            "child system prompt must admit the additional root: {prompt}"
        );
        assert_eq!(
            prompt.matches("<workspace_roots>").count(),
            1,
            "workspace section must appear exactly once"
        );
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
        test_session_with_discovery(SessionOptions {
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
            auth_manager: None,
        })
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
        let mut events = session.subscribe_session_events();
        let change = session.set_model(model(false), String::new());
        assert!(change.clamped);
        assert_eq!(change.effective, ThinkingLevel::Off);
        assert_eq!(session.thinking_level(), ThinkingLevel::Off);
        assert!(change.message.contains("unsupported"), "{}", change.message);
        assert!(matches!(
            events.try_recv(),
            Ok(SessionEvent::ModelSelect { model }) if model.id == "qwen"
        ));
        assert!(matches!(
            events.try_recv(),
            Ok(SessionEvent::ThinkingLevelSelect {
                thinking_level: ThinkingLevel::Off,
            })
        ));
    }

    #[test]
    fn model_switch_without_clamp_only_publishes_model_event() {
        let session = session_with(model(true));
        let mut events = session.subscribe_session_events();
        let change = session.set_model(model(false), String::new());
        assert!(!change.clamped);
        assert!(matches!(
            events.try_recv(),
            Ok(SessionEvent::ModelSelect { model }) if model.id == "qwen"
        ));
        assert!(matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }
}

#[cfg(test)]
mod session_directory_tests {
    use super::*;

    fn test_session(cwd: &Path, session_dir: &Path) -> Session {
        let session = test_session_with_discovery(SessionOptions {
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
            auth_manager: None,
        });
        session.set_session_dir(session_dir.to_path_buf());
        session
    }

    #[test]
    fn interactive_new_recording_uses_stored_session_directory() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let session = test_session(cwd.path(), sessions.path());
        session.start_new_recording().expect("new recording");
        let path = session.recorder_info().expect("recording path").1;
        assert_eq!(path.parent(), Some(sessions.path()));
    }

    #[tokio::test]
    async fn resume_recording_attaches_writer_before_history_and_persists_mutations() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let recorder = crate::start_session_in(
            cwd.path(),
            None,
            None,
            Some(sessions.path()),
            Some("resume-writer"),
            None,
        )
        .expect("recorder");
        recorder
            .record_message(&Message::user_text("persisted", 0))
            .expect("source message");
        recorder.persist_now_durable().expect("persist source");
        let path = recorder.path();
        drop(recorder);
        let prepared = crate::PreparedSessionResume::prepare_path(&path).expect("prepare resume");
        let context = prepared.build_context();
        let session = test_session(cwd.path(), sessions.path());
        session
            .resume_recording(prepared.into_recorder().expect("recorder"), context.messages)
            .await
            .expect("resume recording");
        session.set_session_name("resumed name").expect("rename");
        assert!(session.recording_writer_info().is_ok());
        let tree = crate::load_session_tree(path).expect("reload");
        assert_eq!(tree.session_name().as_deref(), Some("resumed name"));
        assert_eq!(session.history().len(), 1);
    }

    #[tokio::test]
    async fn empty_in_memory_history_on_loaded_recorder_projects_retained_writer() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let recorder = crate::start_session_in(
            cwd.path(),
            None,
            None,
            Some(sessions.path()),
            Some("empty-memory-loaded"),
            None,
        )
        .expect("recorder");
        recorder
            .record_message(&Message::user_text("durable user", 0))
            .expect("user");
        recorder.persist_now_durable().expect("persist");
        let path = recorder.path();
        drop(recorder);
        let prepared = crate::PreparedSessionResume::prepare_path(&path).expect("prepare");
        let context = prepared.build_context();
        let session = test_session(cwd.path(), sessions.path());
        session
            .resume_recording(prepared.into_recorder().expect("recorder"), context.messages)
            .await
            .expect("resume");
        session.reset().await.expect("clear in-memory history");
        assert!(session.history().is_empty());
        let rows = session
            .authoritative_transcript()
            .expect("loaded runtime must project retained writer");
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].message, Message::user_text("durable user", 0));
    }

    #[tokio::test]
    async fn authoritative_transcript_includes_display_true_custom_messages() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let recorder = crate::start_session_in(
            cwd.path(),
            None,
            None,
            Some(sessions.path()),
            Some("custom-transcript"),
            None,
        )
        .expect("recorder");
        recorder
            .record_message(&Message::user_text("seed", 0))
            .expect("user");
        recorder
            .record_custom_message(&CustomMessage {
                custom_type: crate::ORCHESTRATION_MESSAGE_TYPE.to_owned(),
                content: CustomMessageContent::Text("clean IRC parity body".to_owned()),
                display: true,
                details: Some(serde_json::json!({
                    "id": "m1",
                    "from": "Main",
                    "to": "Child",
                    "body": "clean IRC parity body",
                })),
                timestamp: 1,
            })
            .expect("custom");
        recorder.persist_now_durable().expect("persist");
        let path = recorder.path();
        drop(recorder);
        let prepared = crate::PreparedSessionResume::prepare_path(&path).expect("prepare");
        let context = prepared.build_context();
        let session = test_session(cwd.path(), sessions.path());
        session
            .resume_recording(prepared.into_recorder().expect("recorder"), context.messages)
            .await
            .expect("resume");
        session.reset().await.expect("clear in-memory history");
        let rows = session
            .authoritative_transcript()
            .expect("loaded runtime must project retained custom messages");
        let custom = rows.iter().find_map(|row| match &row.message {
            Message::Custom(message) => Some(message),
            _ => None,
        });
        let custom = custom.unwrap_or_else(|| panic!("custom message missing from {rows:?}"));
        assert_eq!(custom.custom_type, crate::ORCHESTRATION_MESSAGE_TYPE);
        assert!(custom.display);
        assert_eq!(
            custom.content,
            CustomMessageContent::Text("clean IRC parity body".to_owned())
        );
        assert_eq!(
            custom
                .details
                .as_ref()
                .and_then(|value| value.get("body"))
                .and_then(|value| value.as_str()),
            Some("clean IRC parity body")
        );
    }

    #[tokio::test]
    async fn handwritten_jsonl_resume_projects_orchestration_custom_without_reset() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let path = sessions.path().join("web-transcript-parity.jsonl");
        let stamp = "2026-01-01T00:00:00.000Z";
        let records = [
            serde_json::json!({
                "type": "session",
                "version": 3,
                "id": "web-transcript-parity",
                "timestamp": stamp,
                "cwd": cwd.path(),
            }),
            serde_json::json!({
                "type": "session_info",
                "id": "si-1",
                "parentId": serde_json::Value::Null,
                "timestamp": stamp,
                "name": "web-transcript-parity",
            }),
            serde_json::json!({
                "type": "message",
                "id": "u1",
                "parentId": serde_json::Value::Null,
                "timestamp": stamp,
                "message": {
                    "role": "user",
                    "content": [{"type": "text", "text": "transcript parity seed"}],
                    "timestamp": 1
                },
            }),
            serde_json::json!({
                "type": "custom_message",
                "id": "hidden",
                "parentId": "u1",
                "timestamp": stamp,
                "customType": "pi.goal.active",
                "content": "<system-reminder>internal parity secret</system-reminder>",
                "display": false,
                "details": {},
            }),
            serde_json::json!({
                "type": "custom_message",
                "id": "irc",
                "parentId": "hidden",
                "timestamp": stamp,
                "customType": crate::ORCHESTRATION_MESSAGE_TYPE,
                "content": "<orchestration-message id=\"m1\" from=\"Main\">\nclean IRC parity body\nReplying to message: parent-9\n</orchestration-message>",
                "display": true,
                "details": {
                    "id": "m1",
                    "from": "Main",
                    "to": "Child",
                    "body": "clean IRC parity body",
                    "replyTo": "parent-9",
                },
            }),
            serde_json::json!({
                "type": "message",
                "id": "bash",
                "parentId": "irc",
                "timestamp": stamp,
                "message": {
                    "role": "bashExecution",
                    "command": "seq 1 30",
                    "output": "bash-parity-30",
                    "cancelled": false,
                    "truncated": false,
                    "timestamp": 2
                },
            }),
        ];
        let body = records
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(&path, body).expect("write handwritten parity jsonl");

        let prepared = crate::PreparedSessionResume::prepare_path(&path).expect("prepare");
        let context = prepared.build_context();
        assert!(
            context.messages.iter().any(|message| matches!(
                message,
                Message::Custom(custom)
                    if custom.display
                        && custom.custom_type == crate::ORCHESTRATION_MESSAGE_TYPE
                        && match &custom.content {
                            CustomMessageContent::Text(text) => text.contains("clean IRC parity body"),
                            CustomMessageContent::Blocks(_) => false,
                        }
            )),
            "PreparedSessionResume lenient context must keep IRC custom: {:?}",
            context.messages
        );

        let session = test_session(cwd.path(), sessions.path());
        session
            .resume_recording(prepared.into_recorder().expect("recorder"), context.messages)
            .await
            .expect("resume");
        let rows = session
            .authoritative_transcript()
            .expect("web hydrate without reset must project handwritten IRC");
        let custom = rows.iter().find_map(|row| match &row.message {
            Message::Custom(message) if message.display => Some(message),
            _ => None,
        });
        let custom = custom.unwrap_or_else(|| panic!("display:true custom missing from {rows:?}"));
        assert_eq!(custom.custom_type, crate::ORCHESTRATION_MESSAGE_TYPE);
        assert_eq!(
            custom
                .details
                .as_ref()
                .and_then(|value| value.get("body"))
                .and_then(|value| value.as_str()),
            Some("clean IRC parity body")
        );
    }

    #[tokio::test]
    async fn authoritative_transcript_omits_branch_summary_on_empty_active_leaf() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let recorder = crate::start_session_in(
            cwd.path(),
            None,
            None,
            Some(sessions.path()),
            Some("empty-active-branch"),
            None,
        )
        .expect("recorder");
        recorder
            .record_message(&Message::user_text("archived other branch", 0))
            .expect("user");
        let mut assistant = pi_ai::AssistantMessage::pending(&Model::default());
        assistant.content = vec![ContentBlock::text("archived assistant")];
        assistant.stop_reason = StopReason::Stop;
        assistant.timestamp = 1;
        recorder
            .record_message(&Message::Assistant(assistant))
            .expect("assistant");
        recorder
            .branch_with_summary(None, "empty current branch")
            .expect("empty active leaf");
        recorder.persist_now_durable().expect("persist");
        let path = recorder.path();
        drop(recorder);
        let prepared = crate::PreparedSessionResume::prepare_path(&path).expect("prepare");
        let context = prepared.build_context();
        let session = test_session(cwd.path(), sessions.path());
        session
            .resume_recording(prepared.into_recorder().expect("recorder"), context.messages)
            .await
            .expect("resume");
        session.reset().await.expect("clear in-memory history");
        let rows = session
            .authoritative_transcript()
            .expect("empty active leaf must project as zero public rows");
        assert!(
            rows.iter()
                .all(|row| !matches!(row.message, Message::BranchSummary(_))),
            "{rows:?}"
        );
        assert_eq!(rows.len(), 0, "{rows:?}");
    }

    #[tokio::test]
    async fn missing_active_leaf_on_loaded_recorder_never_succeeds_empty() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let recorder = crate::start_session_in(
            cwd.path(),
            None,
            None,
            Some(sessions.path()),
            Some("missing-leaf-loaded"),
            None,
        )
        .expect("recorder");
        recorder
            .record_message(&Message::user_text("durable user", 0))
            .expect("user");
        recorder.persist_now_durable().expect("persist");
        let path = recorder.path();
        drop(recorder);
        let prepared = crate::PreparedSessionResume::prepare_path(&path).expect("prepare");
        let context = prepared.build_context();
        let session = test_session(cwd.path(), sessions.path());
        session
            .resume_recording(prepared.into_recorder().expect("recorder"), context.messages)
            .await
            .expect("resume");
        session.reset().await.expect("clear in-memory history");
        session.recorder().expect("loaded recorder").reset_leaf();
        match session.authoritative_transcript() {
            Ok(rows) => {
                assert!(
                    !rows.is_empty(),
                    "re-projection from retained leaf must not be empty: {rows:?}"
                );
                assert_eq!(rows[0].message, Message::user_text("durable user", 0));
            }
            Err(_) => {}
        }
    }

    #[tokio::test]
    async fn loader_drops_never_succeed_as_empty_transcript() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let path = sessions.path().join("loader-drops.jsonl");
        let header = serde_json::json!({
            "type": "session",
            "version": 3,
            "id": "01loaderdrop0000000000000000",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "cwd": cwd.path().to_string_lossy(),
        });
        let lines = [
            header.to_string(),
            serde_json::json!({
                "type": "message",
                "id": "bad-payload",
                "parentId": serde_json::Value::Null,
                "timestamp": "2026-01-01T00:00:01.000Z",
                "message": "not-a-message-object",
            })
            .to_string(),
            serde_json::json!({
                "type": "unknown_shape",
                "id": "force-lenient",
                "parentId": "bad-payload",
                "timestamp": "2026-01-01T00:00:02.000Z",
            })
            .to_string(),
        ];
        std::fs::write(&path, lines.join("\n")).expect("write drop session");
        let prepared = crate::PreparedSessionResume::prepare_path(&path).expect("prepare");
        let context = prepared.build_context();
        let session = test_session(cwd.path(), sessions.path());
        session
            .resume_recording(prepared.into_recorder().expect("recorder"), context.messages)
            .await
            .expect("resume");
        let (skipped, undecodable) = session.loader_drop_counts();
        assert_eq!(skipped, 0);
        assert!(undecodable >= 1, "undecodable={undecodable}");
        assert!(
            session.authoritative_transcript().is_err(),
            "loader drops must not project as genuine empty"
        );
    }

    #[tokio::test]
    async fn flushed_recorder_without_descriptor_is_typed_unavailable_with_source() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let recorder = crate::start_session_in(
            cwd.path(),
            None,
            None,
            Some(sessions.path()),
            Some("detached-descriptor"),
            None,
        )
        .expect("recorder");
        recorder
            .record_message(&Message::user_text("durable user", 0))
            .expect("user");
        recorder.persist_now_durable().expect("persist");
        let path = recorder.path();
        drop(recorder);
        let prepared = crate::PreparedSessionResume::prepare_path(&path).expect("prepare");
        let context = prepared.build_context();
        let session = test_session(cwd.path(), sessions.path());
        session
            .resume_recording(prepared.into_recorder().expect("recorder"), context.messages)
            .await
            .expect("resume");
        session
            .recorder()
            .expect("loaded recorder")
            .close()
            .expect("detach writer");
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"replacement\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":{}}}\n{{\"type\":\"message\",\"id\":\"replacement\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"message\":{}}}\n",
                serde_json::to_string(cwd.path()).expect("cwd json"),
                serde_json::to_string(&Message::user_text("path replacement", 0)).expect("message json"),
            ),
        )
        .expect("replace path");
        let error = session
            .authoritative_transcript()
            .expect_err("detached writer must not reopen the replaced path");
        let dyn_error: &(dyn std::error::Error + 'static) = &error;
        assert!(dyn_error.source().is_some(), "{error}");
        assert!(
            error.to_string().contains("session append writer is unavailable"),
            "{error}"
        );
        assert_ne!(
            session.history().first(),
            Some(&Message::user_text("path replacement", 0))
        );
    }

    #[tokio::test]
    async fn resume_recording_writer_failure_leaves_history_unchanged() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let path = sessions.path().join("stale.jsonl");
        let held = sessions.path().join("held.jsonl");
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"stale\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":{}}}\n",
                serde_json::to_string(cwd.path()).expect("cwd json")
            ),
        )
        .expect("source");
        let prepared = crate::PreparedSessionResume::prepare_path(&path).expect("prepare");
        std::fs::rename(&path, &held).expect("move source");
        std::fs::write(&path, std::fs::read(&held).expect("held bytes")).expect("replacement");
        let session = test_session(cwd.path(), sessions.path());
        let error = session
            .resume_recording(
                prepared.into_recorder().expect("recorder"),
                vec![Message::user_text("must not install", 0)],
            )
            .await
            .expect_err("stale writer must fail");
        assert!(error.to_string().contains("identity"));
        assert!(session.history().is_empty());
        assert!(session.recorder_info().is_none());
    }

    #[test]
    fn interactive_new_with_parent_uses_stored_directory_and_keeps_parent() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let source_dir = tempfile::tempdir().expect("source sessions");
        let source = crate::start_session_in(
            cwd.path(),
            None,
            Some("off"),
            Some(source_dir.path()),
            Some("source"),
            None,
        )
        .expect("source");
        source.persist_now().expect("persist source");
        let source_path = source.path();
        let session = test_session(cwd.path(), sessions.path());
        session
            .start_new_recording_with_parent(Some(&source_path))
            .expect("new with parent");
        let path = session.recorder_info().expect("recording path").1;
        assert_eq!(path.parent(), Some(sessions.path()));
        assert_eq!(
            crate::load_session_tree(&path)
                .expect("tree")
                .header
                .parent_session
                .as_deref(),
            Some(source_path.to_string_lossy().as_ref())
        );
    }

    #[tokio::test]
    async fn interactive_fork_uses_stored_session_directory() {
        let cwd = tempfile::tempdir().expect("cwd");
        let source_dir = tempfile::tempdir().expect("source sessions");
        let sessions = tempfile::tempdir().expect("selected sessions");
        let source = crate::start_session_in(
            cwd.path(),
            None,
            Some("off"),
            Some(source_dir.path()),
            Some("source-fork"),
            None,
        )
        .expect("source");
        let first = source
            .record_message(&Message::user_text("first", 0))
            .expect("first message");
        source
            .record_message(&Message::user_text("second", 1))
            .expect("second message");
        source.persist_now().expect("persist source");

        let session = test_session(cwd.path(), sessions.path());
        session.record(source).expect("attach source");
        session.fork_session(&first, true).await.expect("fork");
        let path = session.recorder_info().expect("fork path").1;
        assert_eq!(path.parent(), Some(sessions.path()));
    }

    fn fork_failure_fixture() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        Session,
        String,
        PathBuf,
        Vec<u8>,
    ) {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let source = crate::start_session_in(
            cwd.path(),
            None,
            Some("off"),
            Some(sessions.path()),
            Some("fork-failure-parent"),
            None,
        )
        .expect("source");
        source
            .record_message(&Message::user_text("retained", 1))
            .expect("retained message");
        let selected = source
            .record_message(&Message::user_text("selected", 2))
            .expect("selected message");
        source.persist_now_durable().expect("persist parent");
        let parent_path = source.path();
        let parent_bytes = std::fs::read(&parent_path).expect("read parent");
        let session = test_session(cwd.path(), sessions.path());
        session.record(source).expect("attach source");
        poll_immediate(session.load_history(vec![
            Message::user_text("retained", 1),
            Message::user_text("selected", 2),
        ]))
        .expect("load parent history");
        (
            cwd,
            sessions,
            session,
            selected,
            parent_path,
            parent_bytes,
        )
    }

    fn jsonl_paths(directory: &Path) -> Vec<PathBuf> {
        let mut paths = std::fs::read_dir(directory)
            .expect("read sessions")
            .map(|entry| entry.expect("session entry").path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "jsonl"))
            .collect::<Vec<_>>();
        paths.sort();
        paths
    }

    #[tokio::test]
    async fn prepare_fork_failure_removes_materialized_child_and_preserves_parent() {
        let (_cwd, sessions, session, selected, parent_path, parent_bytes) =
            fork_failure_fixture();
        let before = jsonl_paths(sessions.path());
        session.inject_fork_prepare_failure_after_materialize();

        let error = match session.prepare_fork_replacement(&selected, true).await {
            Ok(_) => panic!("injected prepare failure unexpectedly succeeded"),

            Err(error) => error,
        };

        assert!(error.to_string().contains("injected fork prepare failure"));
        assert_eq!(jsonl_paths(sessions.path()), before, "orphan child remains");
        assert_eq!(
            std::fs::read(&parent_path).expect("reread parent"),
            parent_bytes,
            "parent journal changed"
        );
        assert_eq!(session.recorder_info().expect("parent remains").1, parent_path);
    }
    #[tokio::test]
    async fn fork_cancellation_before_recorder_install_removes_child_once() {
        let (_cwd, sessions, session, selected, parent_path, parent_bytes) =
            fork_failure_fixture();
        let before = jsonl_paths(sessions.path());
        let parent_id = session.recorder_info().expect("parent remains").0;
        let parent_history = session.history();
        let parent_todo = session.todo_state();
        let parent_goal = session.goal_runtime().get();
        let parent_name = session.session_name();
        let gate = session.arm_fork_before_install_gate_for_test();
        let fork_session = session.clone();
        let task = tokio::spawn(async move { fork_session.fork_session(&selected, true).await });

        gate.wait_entered().await;
        task.abort();
        assert!(task.await.expect_err("fork task cancelled").is_cancelled());
        assert_eq!(session.recorder_info().expect("parent remains"), (parent_id, parent_path.clone()));
        assert_eq!(session.history(), parent_history, "shared history changed");
        assert_eq!(session.todo_state(), parent_todo, "todo authority changed");
        assert_eq!(session.goal_runtime().get(), parent_goal, "goal authority changed");
        assert_eq!(session.session_name(), parent_name, "session name changed");
        assert_eq!(
            std::fs::read(&parent_path).expect("reread parent"),
            parent_bytes,
            "parent journal changed"
        );
        gate.release();
    }

    #[tokio::test]
    async fn fork_cancellation_after_recorder_install_keeps_visible_child() {
        let (_cwd, sessions, session, selected, parent_path, parent_bytes) =
            fork_failure_fixture();
        let before = jsonl_paths(sessions.path());
        let expected_history = vec![Message::user_text("retained", 1)];
        let expected_todo = crate::TodoState {
            phases: Vec::new(),
            storage: crate::TodoStorage::Session,
        };
        let gate = session.arm_fork_after_install_gate_for_test();
        let fork_session = session.clone();
        let task = tokio::spawn(async move { fork_session.fork_session(&selected, true).await });
        gate.wait_entered().await;
        let child_path = session.recorder_info().expect("child installed").1;
        assert_ne!(child_path, parent_path);
        assert!(child_path.exists(), "visible child journal must exist");
        task.abort();
        assert!(task.await.expect_err("fork caller cancelled").is_cancelled());
        gate.release();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if session.inner.agent.state().await.messages == session.history() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached fork install did not finish");
        assert!(child_path.exists(), "cancellation deleted visible child");
        let child_id = session.recorder_info().expect("child remains").0;
        assert_ne!(child_id, "", "child recorder id must be installed");
        assert_eq!(session.history(), expected_history, "shared history is not retained prefix");
        assert_eq!(
            session.inner.agent.state().await.messages,
            expected_history,
            "agent history is not retained prefix"
        );
        assert_eq!(session.todo_state(), expected_todo, "todo state did not restore cut ancestry");
        assert_eq!(session.session_name(), None, "cut ancestry has no session name");
        assert!(!session.is_compacting(), "child inherited active compaction state");
        let mut expected = before;
        expected.push(child_path.clone());
        expected.sort();
        assert_eq!(jsonl_paths(sessions.path()), expected, "child deleted or duplicated");
        assert_eq!(
            std::fs::read(&parent_path).expect("reread parent"),
            parent_bytes,
            "parent journal changed"
        );
    }

    #[tokio::test]
    async fn fork_resume_failure_removes_materialized_child_and_preserves_parent() {
        let (_cwd, sessions, session, selected, parent_path, parent_bytes) =
            fork_failure_fixture();
        let history_before = session.history();
        let before = jsonl_paths(sessions.path());
        session.inject_fork_resume_failure_after_materialize();

        let error = session
            .fork_session(&selected, true)
            .await
            .expect_err("injected resume failure");

        assert!(error.to_string().contains("injected fork resume failure"));
        assert_eq!(jsonl_paths(sessions.path()), before, "orphan child remains");
        assert_eq!(
            std::fs::read(&parent_path).expect("reread parent"),
            parent_bytes,
            "parent journal changed"
        );
        assert_eq!(session.recorder_info().expect("parent remains").1, parent_path);
        assert_eq!(session.history(), history_before, "history changed");
    }
    #[tokio::test]
    async fn fork_rollback_preserves_original_error_when_identity_safe_cleanup_fails() {
        let (_cwd, _sessions, session, selected, parent_path, parent_bytes) =
            fork_failure_fixture();
        let (prepared, _) = session
            .prepare_fork_replacement(&selected, true)
            .await
            .expect("prepare materialized fork");
        let fork_path = prepared.recorder_info().1;

        std::fs::remove_file(&fork_path).expect("remove original child identity");
        std::fs::write(&fork_path, b"replacement identity\n")
            .expect("replace child at same path");
        let error = prepared.rollback_unpublished(anyhow!("original fork failure"));
        let message = format!("{error:#}");

        assert!(message.contains("original fork failure"), "{message}");
        assert!(
            message.contains("materialized fork cleanup also failed"),
            "{message}"
        );
        assert!(
            message.contains("identity changed before rollback"),
            "{message}"
        );
        assert_eq!(
            std::fs::read(&fork_path).expect("replacement child survives"),
            b"replacement identity\n",
            "identity-safe rollback must not remove a replacement file"
        );
        assert_eq!(
            std::fs::read(&parent_path).expect("reread parent"),
            parent_bytes,
            "parent journal changed"
        );
    }

    /// QA P1: an immediate fork must restore the materialized child cut's
    /// authoritative model/provider, API-key policy, and clamped thinking
    /// level — the values recorded in the cut — never the source runtime's
    /// later live selection, and must agree with a standalone reopen of the
    /// same child journal once the parent is deleted.
    #[tokio::test]
    async fn fork_restores_model_and_thinking_from_materialized_cut() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let suffix = uuid::Uuid::now_v7();
        let api = format!("fork-cut-api-{suffix}");
        let provider = format!("fork-cut-provider-{suffix}");
        // Model A and model B share one provider so the no-resolver API-key
        // policy (current state key when the provider is unchanged) applies to
        // the cut model. Both models are reasoning models so the recorded
        // thinking levels survive clamping where expected.
        let model_a = Model {
            id: format!("fork-cut-model-a-{suffix}"),
            name: "Fork cut model A".to_owned(),
            api: api.clone(),
            provider: provider.clone(),
            base_url: "http://localhost:0".to_owned(),
            reasoning: true,
            ..Model::default()
        };
        let model_b = Model {
            id: format!("fork-cut-model-b-{suffix}"),
            name: "Fork cut model B".to_owned(),
            api: api.clone(),
            provider: provider.clone(),
            base_url: "http://localhost:0".to_owned(),
            reasoning: true,
            ..Model::default()
        };
        let _registration = pi_ai::providers::register_faux_provider(
            pi_ai::providers::FauxProviderOptions {
                api: api.clone(),
                provider,
                models: vec![model_a.clone(), model_b.clone()],
                chunk_size: 1,
            },
        );

        let source = crate::start_session_in(
            cwd.path(),
            Some(&model_a),
            Some("low"),
            Some(sessions.path()),
            Some("fork-cut-source"),
            None,
        )
        .expect("source session");
        let session = test_session_with_discovery(SessionOptions {
            model: model_a.clone(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Low,
            api_key: "key-a".to_owned(),
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
        session.record(source.clone()).expect("attach source");

        // The fork point lives under model A / low.
        let selected = source
            .record_message(&Message::user_text("selected under model A", 0))
            .expect("selected entry");
        // The source runtime's later live selection: model B / high, recorded
        // strictly AFTER the selected entry.
        session.set_model(model_b.clone(), "key-b".to_owned());
        session.set_thinking_level(ThinkingLevel::High);
        assert_eq!(
            session.model().expect("source live model").id,
            model_b.id,
            "source runtime must actually run model B before forking"
        );
        assert_eq!(
            session.thinking_level(),
            ThinkingLevel::High,
            "source runtime must actually run high thinking before forking"
        );
        source
            .record_message(&Message::user_text("later under model B", 1))
            .expect("later entry");
        source.persist_now_durable().expect("persist source");
        let source_path = source.path();

        session
            .fork_session(&selected, true)
            .await
            .expect("fork the earlier entry");
        let (_, fork_path) = session.recorder_info().expect("fork recorder info");
        let immediate_model = session
            .model()
            .expect("immediate child runtime has a model");
        assert_eq!(
            immediate_model.id, model_a.id,
            "immediate fork must restore the cut model, not the source's later live model"
        );
        assert_eq!(
            session.thinking_level(),
            ThinkingLevel::Low,
            "immediate fork must restore the cut thinking level, not the source's later live level"
        );
        assert_eq!(
            session.current_api_key(),
            "key-b",
            "immediate fork keeps the session key for the cut model's provider"
        );

        // Standalone reopen of the same child journal after parent deletion
        // must derive the identical model/thinking from the materialized cut.
        std::fs::remove_file(&source_path).expect("delete parent session");
        let prepared =
            crate::PreparedSessionResume::prepare_path(&fork_path).expect("prepare child");
        let reopen = test_session_with_discovery(SessionOptions {
            model: model_a.clone(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Low,
            api_key: "key-b".to_owned(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        });
        reopen.set_session_dir(sessions.path().to_path_buf());
        reopen
            .switch_prepared_session(prepared)
            .await
            .expect("standalone reopen of the child");
        assert_eq!(
            reopen.model().expect("reopened model").id,
            model_a.id,
            "standalone reopen must derive the cut model"
        );
        assert_eq!(
            reopen.thinking_level(),
            ThinkingLevel::Low,
            "standalone reopen must derive the cut thinking level"
        );
        assert_eq!(
            reopen.current_api_key(),
            "key-b",
            "standalone reopen keeps the session key for the cut model's provider"
        );
    }

    /// Review P1: the prepared fork path with an auth resolver must resolve
    /// the cut model's key under the same async policy as the direct path (not
    /// copy the source's live key), must restore the cut policy with
    /// restore_conversation=false, must activate/validate the child's durable
    /// writer before the replacement can be committed, and must append no
    /// post-cut policy journal entries. The recorded thinking level must be
    /// clamped to the cut model's capabilities.
    #[tokio::test]
    async fn fork_prepared_restores_cut_policy_with_resolver_and_clamps() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let suffix = uuid::Uuid::now_v7();
        let api = format!("fork-prep-api-{suffix}");
        // The cut model omits reasoning support, so the recorded "high" must
        // clamp to off in the restored runtime.
        let provider = format!("fork-prep-provider-{suffix}");
        let model_a = Model {
            id: format!("fork-prep-model-a-{suffix}"),
            name: "Fork prep model A".to_owned(),
            api: api.clone(),
            provider: provider.clone(),
            base_url: "http://localhost:0".to_owned(),
            reasoning: false,
            ..Model::default()
        };
        let model_b = Model {
            id: format!("fork-prep-model-b-{suffix}"),
            name: "Fork prep model B".to_owned(),
            api: api.clone(),
            provider: provider.clone(),
            base_url: "http://localhost:0".to_owned(),
            reasoning: true,
            ..Model::default()
        };
        let _registration = pi_ai::providers::register_faux_provider(
            pi_ai::providers::FauxProviderOptions {
                api: api.clone(),
                provider,
                models: vec![model_a.clone(), model_b.clone()],
                chunk_size: 1,
            },
        );
        let resolved_keys = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let resolver: SessionAuthResolver = {
            let resolved_keys = Arc::clone(&resolved_keys);
            Arc::new(move |model| {
                let key = format!("key-for-{}", model.id);
                resolved_keys
                    .lock()
                    .expect("resolver calls")
                    .push(key.clone());
                Box::pin(async move {
                    Ok(RequestAuth {
                        api_key: key,
                        headers: HashMap::new(),
                        env: HashMap::new(),
                        available_model_ids: None,
                        kind: crate::RequestAuthKind::ApiKeyOrHeaders,
                    })
                })
            })
        };

        let source = crate::start_session_in(
            cwd.path(),
            Some(&model_a),
            Some("high"),
            Some(sessions.path()),
            Some("fork-prep-source"),
            None,
        )
        .expect("source session");
        let session = test_session_with_discovery(SessionOptions {
            model: model_a.clone(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::High,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: Some(resolver),
            auth_manager: None,
        });
        session.set_session_dir(sessions.path().to_path_buf());
        session.record(source.clone()).expect("attach source");
        let selected = source
            .record_message(&Message::user_text("selected under model A", 0))
            .expect("selected entry");
        session.set_model(model_b.clone(), "stale-source-key".to_owned());
        session.set_thinking_level(ThinkingLevel::High);
        source.persist_now_durable().expect("persist source");
        let source_path = source.path();

        let (prepared, editor_text) = session
            .prepare_fork_replacement(&selected, false)
            .await
            .expect("prepare fork replacement");
        // Writer activation must have persisted and validated the child before
        // the replacement can be committed.
        let fork_path = prepared.recorder_info().1;
        assert!(fork_path.is_file(), "prepared child must be durably activated");
        assert_eq!(editor_text, "selected under model A");
        assert_eq!(
            session.model().expect("source live model").id,
            model_b.id,
            "prepare must not mutate the source runtime"
        );

        session
            .commit_session_replacement(prepared)
            .await
            .expect("commit prepared fork");

        let immediate_model = session
            .model()
            .expect("immediate child runtime has a model");
        assert_eq!(
            immediate_model.id, model_a.id,
            "prepared fork must restore the cut model"
        );
        assert_eq!(
            session.thinking_level(),
            ThinkingLevel::Off,
            "recorded \"high\" must clamp to off under the non-reasoning cut model"
        );
        assert_eq!(
            session.current_api_key(),
            format!("key-for-{}", model_a.id),
            "prepared fork with a resolver must resolve the cut model's key, not copy the source's"
        );
        assert!(
            session.history().is_empty(),
            "restore_conversation=false must not restore messages"
        );

        // The install must append no post-cut policy journal entries: the
        // child journal still ends at the selected entry with exactly the
        // cut's own recorded model/thinking authority.
        let tree = crate::load_session_tree(&fork_path).expect("load child journal");
        assert_eq!(
            tree.entries
                .iter()
                .filter(|entry| entry.entry_type == "model_change")
                .count(),
            1,
            "child journal must keep exactly the cut's recorded model changes"
        );
        assert_eq!(
            tree.entries
                .iter()
                .filter(|entry| entry.entry_type == "thinking_level_change")
                .count(),
            1,
            "child journal must not gain post-cut thinking entries"
        );
        let child_leaf = tree.entries.last().expect("child leaf");
        assert_eq!(tree.active_leaf_id.as_deref(), Some(child_leaf.id.as_str()));
        assert_eq!(child_leaf.entry_type, "thinking_level_change");
        assert!(
            tree.entries.iter().all(|entry| entry.id != selected),
            "selected user must remain composer-only"
        );

        // The resolver resolved auth for the cut model, and the parent journal
        // still exists untouched for deletion.
        assert!(
            resolved_keys
                .lock()
                .expect("resolver calls")
                .iter()
                .any(|key| *key == format!("key-for-{}", model_a.id)),
            "resolver must be consulted with the cut model"
        );
        std::fs::remove_file(&source_path).expect("delete parent session");
    }

    /// Review P1: a recorded cut model on a provider that differs from the
    /// source's later live provider and cannot be keyed (no auth resolver)
    /// must fail the fork closed, remove the materialized child, and preserve
    /// the parent journal.
    #[tokio::test]
    async fn fork_provider_switch_without_resolver_fails_closed() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let suffix = uuid::Uuid::now_v7();
        let model_a = Model {
            id: format!("fork-closed-model-a-{suffix}"),
            name: "Fork closed model A".to_owned(),
            api: format!("fork-closed-api-a-{suffix}"),
            provider: format!("fork-closed-provider-a-{suffix}"),
            base_url: "http://localhost:0".to_owned(),
            reasoning: true,
            ..Model::default()
        };
        let model_b = Model {
            id: format!("fork-closed-model-b-{suffix}"),
            name: "Fork closed model B".to_owned(),
            api: format!("fork-closed-api-b-{suffix}"),
            provider: format!("fork-closed-provider-b-{suffix}"),
            base_url: "http://localhost:0".to_owned(),
            reasoning: true,
            ..Model::default()
        };
        let registration_a = pi_ai::providers::register_faux_provider(
            pi_ai::providers::FauxProviderOptions {
                api: model_a.api.clone(),
                provider: model_a.provider.clone(),
                models: vec![model_a.clone()],
                chunk_size: 1,
            },
        );
        let registration_b = pi_ai::providers::register_faux_provider(
            pi_ai::providers::FauxProviderOptions {
                api: model_b.api.clone(),
                provider: model_b.provider.clone(),
                models: vec![model_b.clone()],
                chunk_size: 1,
            },
        );

        let source = crate::start_session_in(
            cwd.path(),
            Some(&model_a),
            Some("low"),
            Some(sessions.path()),
            Some("fork-closed-source"),
            None,
        )
        .expect("source session");
        let session = test_session_with_discovery(SessionOptions {
            model: model_a.clone(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Low,
            api_key: "key-a".to_owned(),
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
        session.record(source.clone()).expect("attach source");
        let selected = source
            .record_message(&Message::user_text("selected under provider A", 0))
            .expect("selected entry");
        session.set_model(model_b.clone(), "key-b".to_owned());
        source.persist_now_durable().expect("persist source");
        let source_path = source.path();
        let parent_bytes = std::fs::read(&source_path).expect("read parent");
        let before = jsonl_paths(sessions.path());

        let error = session
            .fork_session(&selected, true)
            .await
            .expect_err("provider switch without a resolver must fail closed");

        assert!(
            error
                .to_string()
                .contains("cannot switch extension model provider without an auth resolver"),
            "{error:#}"
        );
        assert_eq!(jsonl_paths(sessions.path()), before, "materialized child remains");
        assert_eq!(
            std::fs::read(&source_path).expect("reread parent"),
            parent_bytes,
            "parent journal changed"
        );
        assert_eq!(
            session.model().expect("source live model").id,
            model_b.id,
            "failed fork must preserve the source live selection"
        );
    }

    /// QA P1: a materialized fork cut whose recorded provider/model pair is
    /// unavailable in the current catalog must fail closed with the stable
    /// catalog error, remove the materialized child journal, preserve the
    /// parent journal bytes and the source runtime's live selection, and
    /// publish no fork success.
    #[tokio::test]
    async fn fork_unresolvable_recorded_model_fails_closed_and_preserves_parent() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let suffix = uuid::Uuid::now_v7();
        let api = format!("fork-ghost-api-{suffix}");
        let provider = format!("fork-ghost-provider-{suffix}");
        let model_a = Model {
            id: format!("fork-ghost-model-a-{suffix}"),
            name: "Fork ghost model A".to_owned(),
            api: api.clone(),
            provider: provider.clone(),
            base_url: "http://localhost:0".to_owned(),
            reasoning: true,
            ..Model::default()
        };
        let model_b = Model {
            id: format!("fork-ghost-model-b-{suffix}"),
            name: "Fork ghost model B".to_owned(),
            api: api.clone(),
            provider: provider.clone(),
            base_url: "http://localhost:0".to_owned(),
            reasoning: true,
            ..Model::default()
        };
        let _registration = pi_ai::providers::register_faux_provider(
            pi_ai::providers::FauxProviderOptions {
                api: api.clone(),
                provider,
                models: vec![model_a.clone(), model_b.clone()],
                chunk_size: 1,
            },
        );
        // The recorded cut model is never registered in the catalog: the
        // journal records the pair, but `get_model` cannot resolve it.
        let ghost_provider = format!("fork-ghost-unregistered-provider-{suffix}");
        let ghost_model = format!("fork-ghost-unregistered-model-{suffix}");

        let source = crate::start_session_in(
            cwd.path(),
            Some(&model_a),
            Some("low"),
            Some(sessions.path()),
            Some("fork-ghost-source"),
            None,
        )
        .expect("source session");
        let session = test_session_with_discovery(SessionOptions {
            model: model_a.clone(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Low,
            api_key: "key-a".to_owned(),
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
        session.record(source.clone()).expect("attach source");

        // The cut records the unregistered pair: the ghost model_change is
        // journaled strictly before the selected entry, so the materialized
        // cut's authoritative model is unresolvable.
        source
            .record_model_change(&ghost_provider, &ghost_model)
            .expect("record ghost cut model");
        let selected = source
            .record_message(&Message::user_text("selected under ghost model", 0))
            .expect("selected entry");
        // The source runtime's later live selection: model B, recorded strictly
        // AFTER the selected entry.
        session.set_model(model_b.clone(), "key-b".to_owned());
        assert_eq!(
            session.model().expect("source live model").id,
            model_b.id,
            "source runtime must actually run model B before forking"
        );
        source.persist_now_durable().expect("persist source");
        let source_path = source.path();
        let parent_bytes = std::fs::read(&source_path).expect("read parent");
        let before = jsonl_paths(sessions.path());

        let error = session
            .fork_session(&selected, true)
            .await
            .expect_err("unresolvable recorded cut model must fail the fork closed");

        assert!(
            error
                .to_string()
                .contains("is not available in the model catalog"),
            "{error:#}"
        );
        assert!(
            error
                .to_string()
                .contains(&format!("{ghost_provider}/{ghost_model}")),
            "{error:#}"
        );
        assert_eq!(jsonl_paths(sessions.path()), before, "materialized child remains");
        assert_eq!(
            std::fs::read(&source_path).expect("reread parent"),
            parent_bytes,
            "parent journal changed"
        );
        assert_eq!(
            session.recorder_info().expect("parent remains").1,
            source_path,
            "failed fork must keep the parent recording attached"
        );
        assert_eq!(
            session.model().expect("source live model").id,
            model_b.id,
            "failed fork must preserve the source live selection"
        );
    }

    #[tokio::test]
    async fn interactive_nested_fork_retains_ancestry_without_reopening_parent() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let source = crate::start_session_in(
            cwd.path(),
            Some(&Model::default()),
            Some("off"),
            Some(sessions.path()),
            Some("nested-live-source"),
            None,
        )
        .expect("source");
        let first = source
            .record_message(&Message::user_text("first question", 0))
            .expect("first");
        let second = source
            .record_message(&Message::user_text("second question", 1))
            .expect("second");
        source.persist_now_durable().expect("persist source");
        let source_path = source.path();

        let session = test_session(cwd.path(), sessions.path());
        session.record(source).expect("attach source");
        let editor = session
            .fork_session(&second, true)
            .await
            .expect("nested live fork");
        assert_eq!(editor, "second question");
        let (_, fork_path) = session.recorder_info().expect("fork path");
        assert_ne!(fork_path, source_path);
        std::fs::remove_file(&source_path).expect("delete parent");
        let tree = crate::load_session_tree(&fork_path).expect("reload child after parent deletion");
        let texts: Vec<String> = tree
            .entries
            .iter()
            .filter_map(|entry| {
                let Message::User(user) = entry.message.as_ref()? else {
                    return None;
                };
                Some(content_text(&user.content))
            })
            .collect();
        assert_eq!(texts, vec!["first question".to_owned()]);
        assert!(tree.entries.iter().any(|entry| entry.id == first));
        assert!(tree.entries.iter().all(|entry| entry.id != second));
        assert!(
            tree.entries
                .iter()
                .any(|entry| entry.entry_type == "model_change"),
            "nested fork must retain model ancestry, not a synthetic empty prefix"
        );
    }

}

#[cfg(test)]
mod todo_persistence_tests {
    use super::*;

    fn test_session(cwd: &Path) -> Session {
        test_session_with_discovery(SessionOptions {
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
            auth_manager: None,
        })
    }

    #[test]
    fn attaching_resumed_session_restores_complete_todo_dag() {

        let cwd = tempfile::tempdir().expect("cwd");
        let recorder = crate::start_session_in(
            cwd.path(),
            None,
            Some("off"),
            Some(cwd.path()),
            Some("todo-dag-resume"),
            None,
        )
        .expect("start recorder");
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
                        waiting_on: Vec::new(),
                        agent: None,
                    },
                    crate::TodoItem {
                        id: "task-child".to_owned(),
                        content: "child".to_owned(),
                        status: crate::TodoStatus::InProgress,
                        depends_on: vec!["task-root".to_owned()],
                        ready: true,
                        waiting_on: Vec::new(),
                        agent: None,
                    },
                ],
            }],
            storage: TodoStorage::Session,
        };
        recorder
            .record_todo_snapshot(&state)
            .expect("record todo snapshot");
        recorder.persist_now().expect("persist session");
        let path = recorder.path();
        recorder.close().expect("close recorder");

        let session = test_session(cwd.path());
        session
            .record(crate::resume_session(&path).expect("resume recorder"))
            .expect("attach recorder");
        assert_eq!(session.todo_state(), state);
    }
}

#[cfg(test)]
mod bash_spill_lifecycle_tests {
    use super::*;

    fn test_session(cwd: &Path) -> Session {
        test_session_with_discovery(SessionOptions {
            model: Model::default(),
            cwd: cwd.to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: None,
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        })
    }

    async fn read_spill(session: &Session, path: &str) -> Result<pi_agent::AgentToolResult> {
        let tool = session
            .get_all_tools()
            .into_iter()
            .find(|tool| tool.name == "read")
            .expect("session tool set includes read");
        let (_controller, abort) = AbortController::new();
        (tool.execute)(pi_agent::ToolCallContext {
            tool_call_id: "read-bash-spill".to_owned(),
            arguments: serde_json::json!({ "path": path }),
            on_update: Arc::new(|_| {}),
            abort,
            model: None,
            approval_token: None,
        })
        .await
    }

    #[tokio::test]
    async fn execute_bash_success_spill_is_readable_until_idempotent_cleanup() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_session(cwd.path());
        let application = publish_test_application(&session, cwd.path()).await;
        let result = session
            .execute_bash("yes x | head -c 60000", false)
            .await
            .expect("bash");
        let path = result
            .full_output_path
            .expect("successful truncated bash must publish full_output_path");

        let read = read_spill(&session, &path)
            .await
            .expect("spill must remain readable during the active session");
        assert!(
            summarize_tool_result(&read)
                .is_some_and(|text| text.contains("[Showing lines 1-2000 of ")),
            "read tool must resolve the registered retained spill"
        );

        session.cleanup_bash_spills();
        assert!(
            read_spill(&session, &path).await.is_err(),
            "session cleanup must release the retained success spill"
        );
        session.cleanup_bash_spills();
        drop(application);
    }
}

#[cfg(test)]
mod session_label_tests {
    use super::*;
    use std::fs;

    fn test_session(cwd: &Path) -> Session {
        test_session_with_discovery(SessionOptions {
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
            auth_manager: None,
        })
    }

    fn label_in_tree(roots: &[crate::SessionTreeNode], entry_id: &str) -> Option<String> {
        fn walk(roots: &[crate::SessionTreeNode], entry_id: &str) -> Option<String> {
            for node in roots {
                if node.entry.id == entry_id {
                    return node.label.clone();
                }
                if let Some(found) = walk(&node.children, entry_id) {
                    return Some(found);
                }
            }
            None
        }
        walk(roots, entry_id)
    }

    fn label_for(session: &Session, entry_id: &str) -> Option<String> {
        label_in_tree(
            &session.session_tree().expect("session tree").tree,
            entry_id,
        )
    }

    fn label_rows_for(path: &Path, target_id: &str) -> Vec<serde_json::Value> {
        fs::read_to_string(path)
            .expect("read session jsonl")
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("parse jsonl row"))
            .filter(|row| {
                row.get("type").and_then(serde_json::Value::as_str) == Some("label")
                    && row.get("targetId").and_then(serde_json::Value::as_str) == Some(target_id)
            })
            .collect()
    }

    #[test]
    fn set_session_label_trims_and_treats_whitespace_as_clear() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let session = test_session(cwd.path());
        session.set_session_dir(sessions.path().to_path_buf());

        let recorder = crate::start_session_in(
            cwd.path(),
            None,
            Some("off"),
            Some(sessions.path()),
            Some("label-whitespace"),
            None,
        )
        .expect("start recorder");
        let entry_id = recorder
            .record_message(&Message::user_text("root", 0))
            .expect("record entry");
        recorder.persist_now().expect("persist");
        let path = recorder.path();
        session.record(recorder).expect("attach recorder");

        session
            .set_session_label(&entry_id, Some("  checkpoint  "))
            .expect("set trimmed label");
        assert_eq!(
            label_for(&session, &entry_id).as_deref(),
            Some("checkpoint"),
            "non-empty labels must be trimmed before storage"
        );

        // Whitespace-only input must normalize to the same clear path as None.
        for clear_input in [Some("   "), Some("\t\n  "), Some(""), None] {
            session
                .set_session_label(&entry_id, Some("keep-me"))
                .expect("re-label before clear case");
            assert_eq!(label_for(&session, &entry_id).as_deref(), Some("keep-me"),);

            session
                .set_session_label(&entry_id, clear_input)
                .expect("clear via whitespace/empty/none");
            assert_eq!(
                label_for(&session, &entry_id),
                None,
                "whitespace-only/empty/None must clear resolved label; input={clear_input:?}"
            );
        }

        // The recorder is a user-only recording, so label appends stay pending in
        // memory until an assistant message arrives. Force a durable flush of the
        // final clear mutation via the in-module persistence API before any disk or
        // resume read, so the assertions below observe a real persisted clear row
        // rather than an empty (vacuously-cleared) transcript.
        session
            .current_recorder()
            .expect("attached recorder")
            .persist_now_durable()
            .expect("durable-flush final label clear");

        let rows = label_rows_for(&path, &entry_id);
        let last_clear = rows
            .iter()
            .rev()
            .find(|row| row.get("label").is_none())
            .expect("canonical clear label row must omit optional label field");
        assert_eq!(last_clear["type"], "label");
        assert_eq!(last_clear["targetId"], entry_id);
        assert!(
            last_clear.get("label").is_none(),
            "clear representation must omit label, got {last_clear}"
        );
        assert!(
            !rows.iter().any(|row| {
                row.get("label")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|label| label.chars().all(char::is_whitespace) || label.is_empty())
            }),
            "whitespace/empty must never be persisted as a label value: {rows:?}"
        );

        // In-module read/replay path: reload through the attached recorder tree.
        assert_eq!(
            label_for(&session, &entry_id),
            None,
            "cleared label must remain absent on subsequent in-memory tree reads"
        );

        // Resume path available in-module must also resolve as cleared.
        let resumed = crate::resume_session(&path).expect("resume session");
        let resumed_label = label_in_tree(&resumed.tree().expect("resumed tree").tree().tree, &entry_id);
        assert_eq!(
            resumed_label, None,
            "whitespace clear must survive resume/read resolution"
        );
        resumed.close().expect("close resumed");
    }
}

#[cfg(test)]
mod host_hooks_firing_tests {
    use std::fs;
    use std::path::Path;
    use std::time::Duration;

    use pi_ai::providers::{
        FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
    };
    use serde_json::Map;

    use super::*;
    use crate::{HookConfig, HookEvent};

    /// Write an executable fixture hook that echoes a fixed JSON decision.
    fn write_decision_hook(dir: &Path, name: &str, body: &str) -> String {
        let tmp = dir.join(format!("{name}.tmp-{}", Uuid::now_v7()));
        fs::write(&tmp, body).expect("write hook script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&tmp).expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&tmp, permissions).expect("chmod hook");
        }
        let path = dir.join(name);
        // Atomic rename so the exec'd path was never itself open for writing
        // (avoids transient ETXTBSY under parallel test load).
        fs::rename(&tmp, &path).expect("rename hook into place");
        path.to_string_lossy().into_owned()
    }

    /// Hook that records its stdin payload into the file given as `$1`.
    fn write_recording_hook(dir: &Path, name: &str) -> String {
        write_decision_hook(
            dir,
            name,
            "#!/bin/sh\nread -r payload\nprintf '%s' \"$payload\" > \"$1\"\n",
        )
    }

    fn faux_session(cwd: &Path, tag: &str) -> (Session, FauxProviderRegistration) {
        let suffix = Uuid::now_v7().to_string();
        let api = format!("hooks-{tag}-api-{suffix}");
        let provider = format!("hooks-{tag}-provider-{suffix}");
        let model = Model {
            id: format!("hooks-{tag}-model"),
            name: format!("Hooks {tag} Model"),
            api: api.clone(),
            provider: provider.clone(),
            ..Model::default()
        };
        let registration = register_faux_provider(FauxProviderOptions {
            api,
            provider,
            models: vec![model.clone()],
            chunk_size: 1,
        });
        let session = test_session_with_discovery(SessionOptions {
            model,
            cwd: cwd.to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: None,
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        });
        (session, registration)
    }

    fn read_call(id: &str, path: &str) -> FauxResponse {
        FauxResponse {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: id.to_owned(),
                name: "read".to_owned(),
                arguments: serde_json::json!({ "path": path }),
                thought_signature: None,
            })],
            stop_reason: StopReason::ToolUse,
            error_message: None,
        }
    }

    fn settle() -> FauxResponse {
        FauxResponse::text("settled")
    }

    fn message_texts(messages: &[Message]) -> Vec<String> {
        let mut texts = Vec::new();
        for message in messages {
            if let Message::ToolResult(result) = message {
                let mut text = String::new();
                for block in &result.content {
                    if let ContentBlock::Text {
                        text: block_text, ..
                    } = block
                    {
                        text.push_str(block_text);
                    }
                }
                if !text.is_empty() {
                    texts.push(text);
                }
            }
        }
        texts
    }

    #[tokio::test]
    async fn pre_tool_call_hook_blocks_read_tool() {
        let cwd = tempfile::tempdir().expect("cwd");
        fs::write(cwd.path().join("notes.txt"), "hello").expect("write notes");
        let hook = write_decision_hook(
            cwd.path(),
            "block-read.sh",
            "#!/bin/sh\nread -r payload\necho '{\"decision\":\"block\",\"reason\":\"denied by fixture\"}'\n",
        );
        let (session, registration) = faux_session(cwd.path(), "block-read");
        let _application = publish_test_application(&session, cwd.path()).await;
        session.set_host_hooks(Some(vec![HookConfig {
            event: HookEvent::PreToolCall,
            matcher: Some("read".to_owned()),
            command: vec![hook],
            timeout_ms: Some(2_000),
            enabled: None,
            fail_closed: None,
            extra: Map::new(),
        }]));
        registration.set_responses(vec![read_call("call-blocked-read", "notes.txt"), settle()]);

        let result = session.run("read the file", Vec::new()).await.expect("run");

        let texts = message_texts(&result.messages);
        assert!(
            texts.iter().any(|text| text.contains("denied by fixture")),
            "blocked tool must surface the hook reason, got: {texts:?}"
        );
        // The tool must never have executed: the result is the hook decision.
        assert!(
            !texts.iter().any(|text| text.contains("hello")),
            "read must not execute when the pre_tool_call hook blocks: {texts:?}"
        );
        registration.unregister();
    }
    #[tokio::test]
    async fn pre_tool_call_binary_hook_blocks_read_tool() {
        let cwd = tempfile::tempdir().expect("cwd");
        fs::write(cwd.path().join("notes.txt"), "hello").expect("write notes");
        let (session, registration) = faux_session(cwd.path(), "binary-block-read");
        let _application = publish_test_application(&session, cwd.path()).await;
        session.set_host_hooks(Some(vec![HookConfig {
            event: HookEvent::PreToolCall,
            matcher: Some("read".to_owned()),
            command: vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "printf '%s\\n' '{\"decision\":\"block\",\"reason\":\"denied by binary fixture\"}'"
                    .to_owned(),
            ],
            timeout_ms: Some(2_000),
            enabled: None,
            fail_closed: None,
            extra: Map::new(),
        }]));
        registration.set_responses(vec![
            read_call("call-blocked-by-binary", "notes.txt"),
            settle(),
        ]);

        let result = session.run("read the file", Vec::new()).await.expect("run");
        let texts = message_texts(&result.messages);
        assert!(
            texts
                .iter()
                .any(|text| text.contains("denied by binary fixture")),
            "binary hook must surface its block reason, got: {texts:?}"
        );
        assert!(
            !texts.iter().any(|text| text.contains("hello")),
            "read must not execute when the binary hook blocks: {texts:?}"
        );
        registration.unregister();
    }
    #[tokio::test]
    async fn fail_closed_hook_reports_execute_target_spawn_failure() {
        let cwd = tempfile::tempdir().expect("cwd");
        fs::write(cwd.path().join("notes.txt"), "hello").expect("write notes");
        let hook = write_decision_hook(
            cwd.path(),
            "missing-interpreter.sh",
            "#!/definitely-missing-interpreter\n",
        );
        let (session, registration) = faux_session(cwd.path(), "missing-interpreter");
        let _application = publish_test_application(&session, cwd.path()).await;
        session.set_host_hooks(Some(vec![HookConfig {
            event: HookEvent::PreToolCall,
            matcher: Some("read".to_owned()),
            command: vec![hook],
            timeout_ms: Some(2_000),
            enabled: None,
            fail_closed: Some(true),
            extra: Map::new(),
        }]));
        registration.set_responses(vec![read_call("call-failed-hook", "notes.txt"), settle()]);

        let result = session.run("read the file", Vec::new()).await.expect("run");
        let texts = message_texts(&result.messages);
        let failure = texts
            .iter()
            .find(|text| text.contains("failed to spawn"))
            .expect("fail-closed hook spawn failure");
        assert!(
            failure.contains("ExecuteTarget"),
            "failure stage: {failure}"
        );
        assert!(
            !failure.contains("status 125"),
            "raw wrapper status leaked: {failure}"
        );
        assert!(
            !texts.iter().any(|text| text.contains("hello")),
            "read must not execute after fail-closed hook spawn failure: {texts:?}"
        );
        registration.unregister();
    }

    #[tokio::test]
    async fn pre_tool_call_hook_allows_other_tools() {
        let cwd = tempfile::tempdir().expect("cwd");
        fs::write(cwd.path().join("notes.txt"), "hello").expect("write notes");
        let hook = write_decision_hook(
            cwd.path(),
            "block-read.sh",
            "#!/bin/sh\nread -r payload\necho '{\"decision\":\"block\",\"reason\":\"denied by fixture\"}'\n",
        );
        let (session, registration) = faux_session(cwd.path(), "allow-other");
        let _application = publish_test_application(&session, cwd.path()).await;
        session.set_host_hooks(Some(vec![HookConfig {
            event: HookEvent::PreToolCall,
            matcher: Some("read".to_owned()),
            command: vec![hook],
            timeout_ms: Some(2_000),
            enabled: None,
            fail_closed: None,
            extra: Map::new(),
        }]));
        // bash is not matched by the "read" matcher, so the tool runs normally.
        registration.set_responses(vec![
            FauxResponse {
                content: vec![ContentBlock::ToolCall(ToolCall {
                    id: "call-bash-cat".to_owned(),
                    name: "bash".to_owned(),
                    arguments: serde_json::json!({ "command": "cat notes.txt" }),
                    thought_signature: None,
                })],
                stop_reason: StopReason::ToolUse,
                error_message: None,
            },
            settle(),
        ]);

        let result = session.run("show the file", Vec::new()).await.expect("run");

        let texts = message_texts(&result.messages);
        assert!(
            texts.iter().any(|text| text.contains("hello")),
            "bash must execute when the matcher does not match, got: {texts:?}"
        );
        registration.unregister();
    }

    #[tokio::test]
    async fn post_tool_call_receives_result_payload() {
        let cwd = tempfile::tempdir().expect("cwd");
        fs::write(cwd.path().join("notes.txt"), "hello world").expect("write notes");
        let hook = write_recording_hook(cwd.path(), "record-post.sh");
        let out_file = cwd.path().join("post-payload.json");
        let out_file_text = out_file.to_string_lossy().into_owned();
        let (session, registration) = faux_session(cwd.path(), "post-result");
        let _application = publish_test_application(&session, cwd.path()).await;
        session.set_host_hooks(Some(vec![HookConfig {
            event: HookEvent::PostToolCall,
            matcher: None,
            command: vec![hook, out_file_text],
            timeout_ms: Some(2_000),
            enabled: None,
            fail_closed: None,
            extra: Map::new(),
        }]));
        registration.set_responses(vec![read_call("call-read-ok", "notes.txt"), settle()]);

        session.run("read the file", Vec::new()).await.expect("run");

        let captured = fs::read_to_string(&out_file).expect("post payload captured");
        let payload: serde_json::Value =
            serde_json::from_str(&captured).expect("post payload is JSON");
        assert_eq!(payload["event"], "post_tool_call");
        assert_eq!(payload["subject"], "read");
        assert_eq!(payload["toolName"], "read");
        assert_eq!(payload["isError"], false);
        let result = payload["result"].as_str().expect("result summary");
        assert!(
            result.contains("hello world"),
            "post hook must observe the tool result, got: {result:?}"
        );
        assert!(
            payload["sessionId"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
        assert!(payload["cwd"].as_str().is_some_and(|cwd| !cwd.is_empty()));
        registration.unregister();
    }

    #[tokio::test]
    async fn extension_tool_names_exclude_host_hooks() {
        let cwd = tempfile::tempdir().expect("cwd");
        fs::write(cwd.path().join("notes.txt"), "hello").expect("write notes");
        let hook = write_decision_hook(
            cwd.path(),
            "block-read.sh",
            "#!/bin/sh\nread -r payload\necho '{\"decision\":\"block\",\"reason\":\"denied by fixture\"}'\n",
        );
        let (session, registration) = faux_session(cwd.path(), "exclude-ext");
        let _application = publish_test_application(&session, cwd.path()).await;
        session.set_host_hooks(Some(vec![HookConfig {
            event: HookEvent::PreToolCall,
            matcher: None,
            command: vec![hook],
            timeout_ms: Some(2_000),
            enabled: None,
            fail_closed: None,
            extra: Map::new(),
        }]));
        // Mark `read` as extension-provided: host hooks must not fire for it.
        session.set_extension_tool_names(["read".to_owned()]);
        registration.set_responses(vec![read_call("call-ext-read", "notes.txt"), settle()]);

        let result = session.run("read the file", Vec::new()).await.expect("run");

        let texts = message_texts(&result.messages);
        assert!(
            texts.iter().any(|text| text.contains("hello")),
            "extension-tool calls must bypass host hooks, got: {texts:?}"
        );
        registration.unregister();
    }

    #[tokio::test]
    async fn lifecycle_hooks_fire_in_order() {
        let cwd = tempfile::tempdir().expect("cwd");
        let events_file = cwd.path().join("events.log");
        let events_text = events_file.to_string_lossy().into_owned();
        let hook = write_decision_hook(
            cwd.path(),
            "record-event.sh",
            &format!(
                "#!/bin/sh\nread -r payload\nevent=$(printf '%s' \"$payload\" | sed -n 's/.*\"event\":\"\\([^\"]*\\)\".*/\\1/p')\nsubject=$(printf '%s' \"$payload\" | sed -n 's/.*\"subject\":\"\\([^\"]*\\)\".*/\\1/p')\necho \"$event:$subject\" >> \"{}\"\n",
                events_text
            ),
        );
        let (session, registration) = faux_session(cwd.path(), "lifecycle");
        let application = publish_test_application(&session, cwd.path()).await;
        let entries = [
            HookEvent::SessionStart,
            HookEvent::TurnStart,
            HookEvent::TurnEnd,
            HookEvent::SessionEnd,
        ]
        .into_iter()
        .map(|event| HookConfig {
            event,
            matcher: None,
            command: vec![hook.clone()],
            timeout_ms: Some(2_000),
            enabled: None,
            fail_closed: None,
            extra: Map::new(),
        })
        .collect();
        session.set_host_hooks(Some(entries));
        registration.set_responses(vec![FauxResponse::text("done")]);

        session.run("hello", Vec::new()).await.expect("run");
        // Exercise the same session-end hook while the published generation is
        // still live; Drop remains best-effort during process teardown.
        session
            .fire_host_hook(HookEvent::SessionEnd, Some("session"), None)
            .await;
        session.set_host_hooks(None);
        drop(application);
        drop(session);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(log) = fs::read_to_string(&events_file) {
                let lines: Vec<&str> = log.lines().collect();
                if lines.len() >= 4 {
                    assert_eq!(lines[0], "session_start:session");
                    assert_eq!(lines[1], "turn_start:user");
                    assert_eq!(lines[2], "turn_end:assistant");
                    assert_eq!(lines[3], "session_end:session");
                    registration.unregister();
                    return;
                }
            }
            if std::time::Instant::now() > deadline {
                let log = fs::read_to_string(&events_file).unwrap_or_default();
                panic!("lifecycle hooks did not all fire, log: {log:?}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn pre_tool_call_fail_closed_blocks_end_to_end() {
        // The HostHooks unit tests cover failClosed at the runtime level; this
        // test proves the semantics through the real firing site: a
        // pre_tool_call hook that fails (non-zero exit) with `failClosed: true`
        // must block the tool inside a session run.
        let cwd = tempfile::tempdir().expect("cwd");
        fs::write(cwd.path().join("notes.txt"), "hello").expect("write notes");
        let hook = write_decision_hook(
            cwd.path(),
            "fail-closed.sh",
            "#!/bin/sh\nread -r payload\nprintf '%s' \"$payload\" > \"$1\"\necho '{\"decision\":\"block\"}'\nexit 3\n",
        );
        let out_file = cwd.path().join("pre-payload.json");
        let out_file_text = out_file.to_string_lossy().into_owned();
        let (session, registration) = faux_session(cwd.path(), "fail-closed");
        let _application = publish_test_application(&session, cwd.path()).await;
        session.set_host_hooks(Some(vec![HookConfig {
            event: HookEvent::PreToolCall,
            matcher: Some("read".to_owned()),
            command: vec![hook, out_file_text],
            timeout_ms: Some(2_000),
            enabled: None,
            fail_closed: Some(true),
            extra: Map::new(),
        }]));
        registration.set_responses(vec![read_call("call-fail-closed", "notes.txt"), settle()]);

        let result = session.run("read the file", Vec::new()).await.expect("run");

        let texts = message_texts(&result.messages);
        assert!(
            texts.iter().any(|text| text.contains("failClosed")),
            "failClosed must surface as the block reason, got: {texts:?}"
        );
        assert!(
            !texts.iter().any(|text| text.contains("hello")),
            "read must not execute when the failClosed hook fails: {texts:?}"
        );
        // The failing hook still observed the standard pre_tool_call payload
        // before it failed (event, subject, tool name, arguments summary).
        let captured = fs::read_to_string(&out_file).expect("pre payload captured");
        let payload: serde_json::Value =
            serde_json::from_str(&captured).expect("pre payload is JSON");
        assert_eq!(payload["event"], "pre_tool_call");
        assert_eq!(payload["subject"], "read");
        assert_eq!(payload["toolName"], "read");
        assert_eq!(payload["isError"], false);
        assert!(
            payload["sessionId"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
        registration.unregister();
    }

    #[tokio::test]
    async fn session_start_fires_exactly_once_across_turns() {
        // session_start is guarded by a one-shot flag in begin_run: across
        // several turns of one session it must fire exactly once, while
        // turn_start/turn_end fire per turn.
        let cwd = tempfile::tempdir().expect("cwd");
        let events_file = cwd.path().join("events.log");
        let events_text = events_file.to_string_lossy().into_owned();
        let hook = write_decision_hook(
            cwd.path(),
            "record-event.sh",
            &format!(
                "#!/bin/sh\nread -r payload\nevent=$(printf '%s' \"$payload\" | sed -n 's/.*\"event\":\"\\([^\"]*\\)\".*/\\1/p')\nsubject=$(printf '%s' \"$payload\" | sed -n 's/.*\"subject\":\"\\([^\"]*\\)\".*/\\1/p')\necho \"$event:$subject\" >> \"{}\"\n",
                events_text
            ),
        );
        let (session, registration) = faux_session(cwd.path(), "lifecycle-once");
        let application = publish_test_application(&session, cwd.path()).await;
        let entries = [
            HookEvent::SessionStart,
            HookEvent::TurnStart,
            HookEvent::TurnEnd,
            HookEvent::SessionEnd,
        ]
        .into_iter()
        .map(|event| HookConfig {
            event,
            matcher: None,
            command: vec![hook.clone()],
            timeout_ms: Some(2_000),
            enabled: None,
            fail_closed: None,
            extra: Map::new(),
        })
        .collect();
        session.set_host_hooks(Some(entries));
        registration.set_responses(vec![
            FauxResponse::text("first"),
            FauxResponse::text("second"),
        ]);

        session
            .run("first turn", Vec::new())
            .await
            .expect("first run");
        session
            .run("second turn", Vec::new())
            .await
            .expect("second run");
        // Exercise the same session-end hook while the published generation is
        // still live; Drop remains best-effort during process teardown.
        session
            .fire_host_hook(HookEvent::SessionEnd, Some("session"), None)
            .await;
        session.set_host_hooks(None);
        drop(application);
        drop(session);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(log) = fs::read_to_string(&events_file) {
                let lines: Vec<&str> = log.lines().collect();
                if lines.len() >= 6 {
                    assert_eq!(lines[0], "session_start:session");
                    assert_eq!(lines[1], "turn_start:user");
                    assert_eq!(lines[2], "turn_end:assistant");
                    assert_eq!(lines[3], "turn_start:user");
                    assert_eq!(lines[4], "turn_end:assistant");
                    assert_eq!(lines[5], "session_end:session");
                    assert_eq!(
                        lines.len(),
                        6,
                        "session_start must not re-fire on the second turn: {lines:?}"
                    );
                    registration.unregister();
                    return;
                }
            }
            if std::time::Instant::now() > deadline {
                let log = fs::read_to_string(&events_file).unwrap_or_default();
                panic!("lifecycle hooks did not all fire, log: {log:?}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

#[cfg(test)]
mod memory_backend_tests {
    use std::fs;
    use std::path::Path;

    use pi_ai::providers::{
        FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
    };
    use serde_json::json;

    use super::*;

    struct HindsightRecallServer {
        url: String,
        address: std::net::SocketAddr,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for HindsightRecallServer {
        fn drop(&mut self) {
            let _ = std::net::TcpStream::connect(self.address);
            if let Some(worker) = self.worker.take() {
                worker.join().expect("join Hindsight mock");
            }
        }
    }

    fn serve_hindsight_recall(body: &'static str) -> HindsightRecallServer {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind Hindsight mock");
        let address = listener.local_addr().expect("mock address");
        let worker = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept Hindsight request");
            let mut request = [0u8; 8192];
            let bytes = socket.read(&mut request).expect("read Hindsight request");
            if bytes == 0 {
                return;
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .expect("write Hindsight response");
        });
        HindsightRecallServer {
            url: format!("http://{address}"),
            address,
            worker: Some(worker),
        }
    }

    fn recall_messages(messages: Vec<Message>) -> Vec<String> {
        messages
            .into_iter()
            .filter_map(|message| match message {
                Message::Custom(custom) if custom.custom_type == "hindsight_memory" => {
                    Some(match custom.content {
                        pi_ai::CustomMessageContent::Text(text) => text,
                        pi_ai::CustomMessageContent::Blocks(blocks) => blocks
                            .into_iter()
                            .filter_map(|block| match block {
                                ContentBlock::Text { text, .. } => Some(text),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    })
                }
                _ => None,
            })
            .collect()
    }

    fn faux_session(cwd: &Path, tag: &str) -> (Session, FauxProviderRegistration) {
        let suffix = Uuid::now_v7().to_string();
        let api = format!("memory-{tag}-api-{suffix}");
        let provider = format!("memory-{tag}-provider-{suffix}");
        let model = Model {
            id: format!("memory-{tag}-model"),
            name: format!("Memory {tag} Model"),
            api: api.clone(),
            provider: provider.clone(),
            ..Model::default()
        };
        let registration = register_faux_provider(FauxProviderOptions {
            api,
            provider,
            models: vec![model.clone()],
            chunk_size: 1,
        });
        let session = test_session_with_discovery(SessionOptions {
            model,
            cwd: cwd.to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: None,
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        });
        (session, registration)
    }

    /// Writes `settings.memory` into the agent dir's global settings.json and
    /// attaches the resource manager so the session reconciles its tool set.
    async fn attach_memory_settings(
        session: &Session,
        agent_dir: &Path,
        cwd: &Path,
        memory: serde_json::Value,
    ) {
        fs::write(
            agent_dir.join("settings.json"),
            serde_json::to_string(&json!({ "memory": memory })).expect("settings json"),
        )
        .expect("write settings");
        let resources = isolated_test_resources(cwd, agent_dir, "danger-full-access");
        session
            .attach_resources(resources)
            .await
            .expect("attach resources");
    }

    #[tokio::test]
    async fn backend_off_hides_memory_tools_after_attach() {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        let (session, registration) = faux_session(cwd.path(), "off");
        attach_memory_settings(
            &session,
            agent.path(),
            cwd.path(),
            json!({ "backend": "off" }),
        )
        .await;
        let _application = publish_attached_test_application(&session).await;
        // The model's tool set is the contract: every memory-family tool must
        // be gone with backend=off. (The system-prompt string may still carry
        // a loaded prompt template's stale tool list; the tool set is what the
        // agent executes.)
        let tools = session.get_active_tool_names();
        for hidden in ["memory", "recall", "retain", "reflect"] {
            assert!(
                !tools.iter().any(|name| name == hidden),
                "{hidden} must be hidden: {tools:?}"
            );
        }
        registration.unregister();
    }

    /// Attaches a resource manager whose global settings.json carries `settings`.
    async fn attach_settings(
        session: &Session,
        agent_dir: &Path,
        cwd: &Path,
        settings: serde_json::Value,
    ) {
        fs::write(
            agent_dir.join("settings.json"),
            serde_json::to_string(&settings).expect("settings json"),
        )
        .expect("write settings");
        let resources = isolated_test_resources(cwd, agent_dir, "danger-full-access");
        session
            .attach_resources(resources)
            .await
            .expect("attach resources");
    }

    #[tokio::test]
    async fn permission_rules_source_tracks_live_settings() {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        let (session, registration) = faux_session(cwd.path(), "rules-live");
        let target = cwd.path().join("lib.rs");
        attach_settings(
            &session,
            agent.path(),
            cwd.path(),
            json!({
                "permissionRules": [{
                    "action": "deny",
                    "path": target.display().to_string(),
                    "tools": ["lsp"]
                }]
            }),
        )
        .await;
        let application = publish_attached_test_application(&session).await;
        let generation = application
            .resource_generation()
            .expect("resource generation");
        assert_eq!(generation, 1);
        // After attach, the session exposes the generation-backed rule source.
        let source = session
            .permission_rules_source()
            .expect("rules source after attach");
        let rules = source();
        assert_eq!(rules.len(), 1, "{rules:?}");
        assert_eq!(rules[0].action, crate::settings::PermissionRuleAction::Deny);
        assert!(
            rules[0]
                .tools
                .as_ref()
                .is_some_and(|tools| tools.contains(&crate::settings::PermissionTool::Lsp)),
            "{rules:?}"
        );

        // The same source follows the sole published ApplicationGeneration.
        fs::write(
            agent.path().join("settings.json"),
            serde_json::to_string(&json!({
                "permissionRules": [],
                "sandbox": { "mode": "danger-full-access" },
            }))
            .expect("settings json"),
        )
        .expect("write settings");
        application.reload().await.expect("reload application");
        assert!(
            source().is_empty(),
            "live source must reflect the published rules"
        );
        registration.unregister();
    }

    #[tokio::test]
    async fn backend_hindsight_swaps_memory_tool_for_trio() {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        let (session, registration) = faux_session(cwd.path(), "trio");
        attach_memory_settings(
            &session,
            agent.path(),
            cwd.path(),
            json!({ "backend": "hindsight", "hindsightApiUrl": "http://127.0.0.1:9", "hindsightAllowInsecure": true, "hindsightBankId": "pi-test" }),
        )
        .await;
        let _application = publish_attached_test_application(&session).await;
        let tools = session.get_active_tool_names();
        assert!(
            !tools.iter().any(|name| name == "memory"),
            "local memory tool hidden in hindsight mode: {tools:?}"
        );
        for present in ["recall", "retain", "reflect"] {
            assert!(
                tools.iter().any(|name| name == present),
                "{present} must be present: {tools:?}"
            );
        }
        registration.unregister();
    }

    #[tokio::test]
    async fn injection_prepends_bounded_memory_to_turn_context() {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        let recall_server = serve_hindsight_recall(
            r#"{"results":[{"text":"the release script lives in scripts/release.sh","type":"world"}]}"#,
        );
        let (session, registration) = faux_session(cwd.path(), "inject");
        attach_memory_settings(
            &session,
            agent.path(),
            cwd.path(),
            json!({
                "backend": "hindsight",
                "hindsightApiUrl": recall_server.url,
                "hindsightAllowInsecure": true,
                "hindsightInjection": true,
            }),
        )
        .await;
        let _application = publish_attached_test_application(&session).await;
        let (event_sender, mut event_receiver) = mpsc::unbounded_channel::<AgentEvent>();
        let subscription = session
            .subscribe(move |event| {
                let event_sender = event_sender.clone();
                async move {
                    let _ = event_sender.send(event);
                    Ok(())
                }
            })
            .await;
        registration.set_responses(vec![FauxResponse::text("ok")]);
        session
            .run("where is the release script?", Vec::new())
            .await
            .expect("run");
        drop(subscription);
        let mut saw_injection = false;
        while let Ok(event) = event_receiver.try_recv() {
            if let AgentEvent::MessageStart {
                message: Message::Custom(custom),
            } = event
            {
                if custom.custom_type == "hindsight_memory" {
                    saw_injection = true;
                    let text = match &custom.content {
                        pi_ai::CustomMessageContent::Text(text) => text.clone(),
                        pi_ai::CustomMessageContent::Blocks(blocks) => blocks
                            .iter()
                            .filter_map(|block| match block {
                                ContentBlock::Text { text, .. } => Some(text.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    };
                    assert!(
                        text.contains("scripts/release.sh"),
                        "injected body must carry the recalled memory: {text}"
                    );
                }
            }
        }
        assert!(
            saw_injection,
            "turn must carry the hindsight_memory injection message"
        );
        registration.unregister();
    }

    #[tokio::test]
    async fn injection_cache_hits_for_normalized_ask_and_unchanged_config() {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        let recall_server = serve_hindsight_recall(r#"{"results":[{"text":"cached recall"}]}"#);
        let (session, registration) = faux_session(cwd.path(), "inject-cache-hit");
        attach_memory_settings(
            &session,
            agent.path(),
            cwd.path(),
            json!({
                "backend": "hindsight",
                "hindsightApiUrl": recall_server.url,
                "hindsightAllowInsecure": true,
                "hindsightInjection": true,
            }),
        )
        .await;
        let _application = publish_attached_test_application(&session).await;

        let first = session
            .inject_hindsight_memory(vec![Message::User(UserMessage {
                content: vec![ContentBlock::text("  same ask  ")],
                timestamp: 1,
            })])
            .await;
        let second = session
            .inject_hindsight_memory(vec![Message::User(UserMessage {
                content: vec![ContentBlock::text("same ask")],
                timestamp: 2,
            })])
            .await;

        assert_eq!(
            recall_messages(first),
            [
                "Related memories from the Hindsight backend for the latest user request:\n- cached recall"
            ]
        );
        assert_eq!(
            recall_messages(second),
            [
                "Related memories from the Hindsight backend for the latest user request:\n- cached recall"
            ]
        );
        registration.unregister();
    }

    #[tokio::test]
    async fn injection_cache_misses_after_relevant_config_reload() {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        let first_server = serve_hindsight_recall(r#"{"results":[{"text":"first recall"}]}"#);
        let second_server = serve_hindsight_recall(r#"{"results":[{"text":"fresh recall"}]}"#);
        let (session, registration) = faux_session(cwd.path(), "inject-cache-miss");
        attach_memory_settings(
            &session,
            agent.path(),
            cwd.path(),
            json!({
                "backend": "hindsight",
                "hindsightApiUrl": first_server.url,
                "hindsightAllowInsecure": true,
                "hindsightInjection": true,
                "hindsightRecallMaxTokens": 64,
            }),
        )
        .await;
        let application = publish_attached_test_application(&session).await;

        let first = session
            .inject_hindsight_memory(vec![Message::User(UserMessage {
                content: vec![ContentBlock::text("same ask")],
                timestamp: 1,
            })])
            .await;
        let mut reloaded_settings = json!({
            "memory": {
                "backend": "hindsight",
                "hindsightApiUrl": second_server.url,
                "hindsightAllowInsecure": true,
                "hindsightInjection": true,
                "hindsightRecallMaxTokens": 128,
            }
        });
        reloaded_settings["sandbox"] = json!({ "mode": "danger-full-access" });
        fs::write(
            agent.path().join("settings.json"),
            serde_json::to_string(&reloaded_settings).expect("settings json"),
        )
        .expect("write settings");
        application.reload().await.expect("reload application");
        let second = session
            .inject_hindsight_memory(vec![Message::User(UserMessage {
                content: vec![ContentBlock::text("same ask")],
                timestamp: 2,
            })])
            .await;

        assert!(recall_messages(first)[0].contains("first recall"));
        assert!(recall_messages(second)[0].contains("fresh recall"));
        registration.unregister();
    }

    #[test]
    fn injection_cache_key_covers_every_memory_config_field_and_request() {
        let base = crate::MemoryConfig {
            backend: crate::MemoryBackend::Hindsight,
            hindsight_api_url: Some("https://memory.invalid/api".to_owned()),
            hindsight_api_token: Some("alpha".repeat(2)),
            hindsight_allow_insecure: true,
            hindsight_bank_id: "bank-a".to_owned(),
            hindsight_bank_id_prefix: Some("prefix-a".to_owned()),
            hindsight_scoping: crate::HindsightScoping::PerProject,
            hindsight_bank_mission: Some("bank mission".to_owned()),
            hindsight_retain_mission: Some("retain mission".to_owned()),
            hindsight_injection: true,
            hindsight_recall_budget: crate::HindsightBudget::High,
            hindsight_recall_max_tokens: 2048,
            hindsight_recall_types: vec!["world".to_owned(), "experience".to_owned()],
            hindsight_request_timeout_ms: 101,
            hindsight_recall_timeout_ms: 102,
            hindsight_retain_timeout_ms: 103,
            hindsight_reflect_timeout_ms: 104,
        };
        let base_key = hindsight_injection_cache_key(&base, "ask");
        let mut changed = Vec::new();
        let mut push = |config: crate::MemoryConfig| {
            changed.push(hindsight_injection_cache_key(&config, "ask"));
        };

        let mut config = base.clone();
        config.backend = crate::MemoryBackend::Local;
        push(config);
        let mut config = base.clone();
        config.hindsight_api_url = Some("https://other.invalid".to_owned());
        push(config);
        let mut config = base.clone();
        config.hindsight_api_token = Some("beta".repeat(2));
        push(config);
        let mut config = base.clone();
        config.hindsight_allow_insecure = false;
        push(config);
        let mut config = base.clone();
        config.hindsight_bank_id = "bank-b".to_owned();
        push(config);
        let mut config = base.clone();
        config.hindsight_bank_id_prefix = None;
        push(config);
        let mut config = base.clone();
        config.hindsight_scoping = crate::HindsightScoping::Global;
        push(config);
        let mut config = base.clone();
        config.hindsight_bank_mission = None;
        push(config);
        let mut config = base.clone();
        config.hindsight_retain_mission = None;
        push(config);
        let mut config = base.clone();
        config.hindsight_injection = false;
        push(config);
        let mut config = base.clone();
        config.hindsight_recall_budget = crate::HindsightBudget::Low;
        push(config);
        let mut config = base.clone();
        config.hindsight_recall_max_tokens += 1;
        push(config);
        let mut config = base.clone();
        config.hindsight_recall_types.reverse();
        push(config);
        let mut config = base.clone();
        config.hindsight_request_timeout_ms += 1;
        push(config);
        let mut config = base.clone();
        config.hindsight_recall_timeout_ms += 1;
        push(config);
        let mut config = base.clone();
        config.hindsight_retain_timeout_ms += 1;
        push(config);
        let mut config = base.clone();
        config.hindsight_reflect_timeout_ms += 1;
        push(config);
        changed.push(hindsight_injection_cache_key(&base, "other ask"));

        assert!(changed.iter().all(|key| key != &base_key));
        assert_eq!(hindsight_injection_cache_key(&base, "  ask  "), base_key);
    }

    #[test]
    fn reconcile_memory_tools_replaces_only_builtin_memory_tools() {
        let cwd = tempfile::tempdir().expect("cwd");
        let local = crate::memory::memory_tools_for(
            &cwd.path().to_string_lossy(),
            None,
            Some(crate::MemoryConfig::default()),
        );
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].name, "memory");
        let names =
            |tools: Vec<AgentTool>| tools.into_iter().map(|tool| tool.name).collect::<Vec<_>>();
        // local backend keeps the built-in memory tool.
        let kept = reconcile_memory_tools(
            local.clone(),
            crate::MemoryConfig::default(),
            cwd.path(),
            None,
            None,
        );
        assert_eq!(names(kept), vec!["memory".to_owned()]);
        // hindsight swaps memory → recall/retain/reflect.
        let swapped = reconcile_memory_tools(
            local.clone(),
            crate::MemoryConfig {
                backend: crate::MemoryBackend::Hindsight,
                ..Default::default()
            },
            cwd.path(),
            None,
            None,
        );
        assert_eq!(
            names(swapped),
            vec![
                "recall".to_owned(),
                "retain".to_owned(),
                "reflect".to_owned()
            ]
        );
        // off removes every memory tool.
        let removed = reconcile_memory_tools(
            local,
            crate::MemoryConfig {
                backend: crate::MemoryBackend::Off,
                ..Default::default()
            },
            cwd.path(),
            None,
            None,
        );
        assert!(removed.is_empty());
        // A tool set without memory-family built-ins is left untouched (an
        // extension's own tool is not clobbered).
        let extension_tool = AgentTool::new(
            "custom_note",
            "extension tool",
            crate::tools::s_object(vec![], vec![]),
            |_ctx| async move { Ok(crate::tools::text_result("ext")) },
        );
        let untouched = reconcile_memory_tools(
            vec![extension_tool],
            crate::MemoryConfig {
                backend: crate::MemoryBackend::Off,
                ..Default::default()
            },
            cwd.path(),
            None,
            None,
        );
        assert_eq!(names(untouched), vec!["custom_note".to_owned()]);
        // Memory-family names are reserved for the backend: with backend=off a
        // lone family-named tool is removed too (reconcile keeps the family in
        // sync with settings.memory.backend across reloads).
        let family_tool = AgentTool::new(
            "recall",
            "extension squatting on a builtin name",
            crate::tools::s_object(vec![], vec![]),
            |_ctx| async move { Ok(crate::tools::text_result("ext")) },
        );
        let family_removed = reconcile_memory_tools(
            vec![family_tool],
            crate::MemoryConfig {
                backend: crate::MemoryBackend::Off,
                ..Default::default()
            },
            cwd.path(),
            None,
            None,
        );
        assert!(family_removed.is_empty());
    }

    #[tokio::test]
    async fn injection_skips_when_disabled_or_non_hindsight() {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        let recall_server = serve_hindsight_recall(r#"{"results":[{"text":"leak-me-marker"}]}"#);
        let (session, registration) = faux_session(cwd.path(), "noinject");
        // backend=local with injection on → no hindsight fetch, no injection.
        attach_memory_settings(
            &session,
            agent.path(),
            cwd.path(),
            json!({
                "backend": "local",
                "hindsightApiUrl": recall_server.url,
                "hindsightAllowInsecure": true,
                "hindsightInjection": true,
            }),
        )
        .await;
        let _application = publish_attached_test_application(&session).await;
        let (event_sender, mut event_receiver) = mpsc::unbounded_channel::<AgentEvent>();
        let subscription = session
            .subscribe(move |event| {
                let event_sender = event_sender.clone();
                async move {
                    let _ = event_sender.send(event);
                    Ok(())
                }
            })
            .await;
        registration.set_responses(vec![FauxResponse::text("ok")]);
        session.run("hello", Vec::new()).await.expect("run");
        drop(subscription);
        while let Ok(event) = event_receiver.try_recv() {
            if let AgentEvent::MessageStart {
                message: Message::Custom(custom),
            } = event
            {
                assert_ne!(custom.custom_type, "hindsight_memory");
                let text = match &custom.content {
                    pi_ai::CustomMessageContent::Text(text) => text.clone(),
                    pi_ai::CustomMessageContent::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text { text, .. } => Some(text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                assert!(
                    !text.contains("leak-me-marker"),
                    "no injection with local backend"
                );
            }
        }
        registration.unregister();
    }
}

#[cfg(test)]
mod compact_timeout_tests {
    use super::*;

    /// A provider stream that never pushes an event and never ends — the exact
    /// shape of a stalled SSE body (headers received, then silence). Without
    /// the summarization deadline this hangs `/compact` (and automatic
    /// compaction) forever while the exclusive run slot stays held.
    fn stalled_stream_fn() -> pi_agent::StreamFn {
        std::sync::Arc::new(|_model, _context, _options| {
            Box::pin(async move { pi_ai::new_assistant_message_event_stream() })
        })
    }

    fn compactable_session(cwd: &std::path::Path) -> Session {
        test_session_with_discovery(SessionOptions {
            model: Model::default(),
            cwd: cwd.to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: Some(CompactionSettings {
                enabled: true,
                reserve_tokens: 10,
                keep_recent_tokens: 4,
                snap_keep_turns: 10,
            }),
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(stalled_stream_fn()),
            auth_resolver: None,
            auth_manager: None,
        })
    }

    #[tokio::test]
    async fn stalled_summarization_times_out_with_actionable_error() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = compactable_session(cwd.path());
        let _application = publish_test_application(&session, cwd.path()).await;
        session
            .load_history(vec![
                Message::user_text("older context ".repeat(60), 1),
                Message::user_text("middle context ".repeat(60), 2),
                Message::user_text("recent context ".repeat(60), 3),
                Message::user_text("newest context ".repeat(60), 4),
            ])
            .await
            .expect("load history");

        // The stalled provider must fail the compaction via the summarization
        // deadline instead of hanging the session forever.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            session.perform_compaction(
                CompactionReason::Manual,
                false,
                None,
                Some(std::time::Duration::from_secs(1)),
            ),
        )
        .await
        .expect("compaction must not hang: stalled provider never terminates");

        let error = result.expect_err("stalled provider must fail the compaction");
        let message = format!("{error:#}");
        assert!(
            message.contains("timed out after 1s"),
            "error must name the deadline and be actionable: {message}"
        );
        assert!(
            message.contains("provider"),
            "error must point at the provider, not the session: {message}"
        );
        // The exclusive run slot / compaction activity must be released so the
        // session stays usable after a timed-out compaction.
        assert!(
            !session.is_compacting(),
            "compaction activity must be cleared"
        );
        assert!(
            session.history().len() >= 4,
            "a timed-out compaction must not destroy the conversation"
        );
    }
}

#[cfg(test)]
mod llm_compact_elision_tests {
    use pi_ai::providers::{
        FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
    };

    use super::*;

    /// A session with an enabled LLM compaction path backed by a faux
    /// provider, so the summarization call is deterministic and offline.
    fn faux_compact_session(
        cwd: &std::path::Path,
        tag: &str,
    ) -> (Session, FauxProviderRegistration) {
        let suffix = Uuid::now_v7().to_string();
        let api = format!("compact-{tag}-api-{suffix}");
        let provider = format!("compact-{tag}-provider-{suffix}");
        let model = Model {
            id: format!("compact-{tag}-model"),
            name: format!("Compact {tag} Model"),
            api: api.clone(),
            provider: provider.clone(),
            ..Model::default()
        };
        let registration = register_faux_provider(FauxProviderOptions {
            api,
            provider,
            models: vec![model.clone()],
            chunk_size: 1,
        });
        let session = test_session_with_discovery(SessionOptions {
            model,
            cwd: cwd.to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: Some(CompactionSettings {
                enabled: true,
                reserve_tokens: 10,
                keep_recent_tokens: 4,
                snap_keep_turns: 10,
            }),
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        });
        (session, registration)
    }

    fn tool_result(text: &str, is_error: bool, ts: i64) -> Message {
        Message::ToolResult(pi_ai::ToolResultMessage {
            tool_call_id: "t".to_string(),
            tool_name: "read".to_string(),
            content: vec![ContentBlock::Text {
                text: text.to_string(),
                text_signature: None,
            }],
            usage: None,
            details: None,
            added_tool_names: Vec::new(),
            is_error,
            timestamp: ts,
        })
    }

    #[tokio::test]
    async fn llm_compact_elides_useless_results_and_notes_them() {
        let cwd = tempfile::tempdir().expect("cwd");
        let (session, registration) = faux_compact_session(cwd.path(), "elide");
        let _application = publish_test_application(&session, cwd.path()).await;
        registration.set_responses(vec![FauxResponse::text("Summarized history.")]);
        // Turn 1: an empty result + a duplicate error (useless); turns 2-3: clean.
        let history = vec![
            Message::user_text("first ask", 1),
            tool_result("   ", false, 2),
            tool_result("read failed: missing", true, 3),
            tool_result("read failed: missing", true, 4),
            Message::user_text("second ask", 5),
            tool_result("ok", false, 6),
            Message::user_text("third ask", 7),
            tool_result("done", false, 8),
        ];
        session.load_history(history).await.expect("load history");
        let result = session.compact(None).await.expect("llm compact");
        assert!(
            result.summary.contains("[elided 2 useless results]"),
            "the LLM compaction summary must note the elided count: {}",
            result.summary,
        );
        // The archived turn 1 was elided; the kept tail (turn 3) is untouched.
        let kept = session.history();
        assert!(
            kept.iter().any(|m| matches!(
                m,
                Message::ToolResult(tr)
                    if tr.content.first().is_some_and(|b| matches!(b, ContentBlock::Text { text, .. } if text == "done"))
            )),
            "the kept tail must retain the final tool result"
        );
    }

    #[tokio::test]
    async fn llm_compact_reports_shrinking_counts_when_usage_anchors_before() {
        let cwd = tempfile::tempdir().expect("cwd");
        let (session, registration) = faux_compact_session(cwd.path(), "counts");
        let _application = publish_test_application(&session, cwd.path()).await;
        registration.set_responses(vec![FauxResponse::text("Summarized history.")]);
        let mut history = vec![
            Message::user_text("first ask with context", 1),
            tool_result("ok", false, 2),
            Message::user_text("second ask with context", 3),
            tool_result("ok", false, 4),
            Message::user_text("third ask with context", 5),
        ];
        // The live session's final assistant turn carries the real context
        // usage the provider reported; the pre-compaction count anchors on it,
        // so the after-count must come from the summary + kept tail rather
        // than the stale total (which would report no shrink at all).
        let mut assistant = pi_ai::AssistantMessage::pending(&Model::default());
        assistant.content = vec![ContentBlock::text("answer three")];
        assistant.stop_reason = StopReason::Stop;
        assistant.usage = Usage {
            total_tokens: 64_154,
            ..Usage::default()
        };
        assistant.timestamp = 6;
        history.push(Message::Assistant(assistant));
        session.load_history(history).await.expect("load history");
        let result = session.compact(None).await.expect("llm compact");
        assert_eq!(
            result.tokens_before, 64_154,
            "pre-compaction count anchors on the last real usage"
        );
        assert!(
            result
                .estimated_tokens_after
                .is_some_and(|after| after < result.tokens_before),
            "post-compaction estimate must cover summary + kept tail, not the stale anchor: before={} after={:?}",
            result.tokens_before,
            result.estimated_tokens_after,
        );
    }
}

#[cfg(test)]
mod snap_compact_tests {
    use super::*;

    /// A stream function that records every invocation. Snap compact must
    /// never call the provider, so the counter stays at zero.
    fn counting_stream_fn() -> (pi_agent::StreamFn, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let captured = count.clone();
        let stream_fn: pi_agent::StreamFn =
            std::sync::Arc::new(move |_model, _context, _options| {
                captured.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move { pi_ai::new_assistant_message_event_stream() })
            });
        (stream_fn, count)
    }

    fn snap_session(cwd: &std::path::Path, keep_turns: i64) -> (Session, Arc<AtomicUsize>) {
        let (stream_fn, count) = counting_stream_fn();
        let session = test_session_with_discovery(SessionOptions {
            model: Model::default(),
            cwd: cwd.to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: Some(CompactionSettings {
                enabled: true,
                reserve_tokens: 10,
                keep_recent_tokens: 4,
                snap_keep_turns: keep_turns,
            }),
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(stream_fn),
            auth_resolver: None,
            auth_manager: None,
        });
        (session, count)
    }

    /// `count` user turns, each a large user ask + a large assistant reply, so
    /// archiving older turns measurably shrinks the context.
    fn dense_history(count: usize) -> Vec<Message> {
        let padding = "x".repeat(200);
        let mut messages = Vec::new();
        for turn in 0..count {
            messages.push(Message::user_text(
                format!("ask number {turn}: {padding}"),
                turn as i64 * 2,
            ));
            let mut assistant = pi_ai::AssistantMessage::pending(&Model::default());
            assistant.content = vec![ContentBlock::text(format!("answer {turn}: {padding}"))];
            assistant.stop_reason = StopReason::Stop;
            assistant.timestamp = turn as i64 * 2 + 1;
            messages.push(Message::Assistant(assistant));
        }
        messages
    }

    /// First text block of a user message, if any.
    fn user_text_of(message: &Message) -> Option<String> {
        let Message::User(user) = message else {
            return None;
        };
        user.content.iter().find_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
    }

    #[tokio::test]
    async fn snap_compact_replaces_dense_history_without_provider_call() {
        let cwd = tempfile::tempdir().expect("cwd");
        let (session, count) = snap_session(cwd.path(), 2);
        let _application = publish_test_application(&session, cwd.path()).await;
        session
            .load_history(dense_history(12))
            .await
            .expect("load history");
        let result = session.compact_snap().await.expect("snap compact");
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "snap compact must not call the provider"
        );

        let history = session.history();
        assert!(
            matches!(history.first(), Some(Message::CompactionSummary(_))),
            "the compacted summary block leads the transcript"
        );
        assert!(
            result
                .estimated_tokens_after
                .is_some_and(|after| after < result.tokens_before),
            "snap compact must shrink the context: before={} after={:?}",
            result.tokens_before,
            result.estimated_tokens_after,
        );
        // 12 turns archived down to the last 2 user turns: the kept region
        // starts at "ask number 10".
        let kept_user_texts: Vec<String> = history.iter().filter_map(user_text_of).collect();
        assert_eq!(
            kept_user_texts.len(),
            2,
            "exactly the last 2 user turns are kept"
        );
        assert!(
            kept_user_texts
                .first()
                .is_some_and(|t| t.contains("ask number 10")),
            "archiving starts at turn 10"
        );
    }

    #[tokio::test]
    async fn snap_compact_keeps_last_k_turns_setting_honored() {
        let cwd = tempfile::tempdir().expect("cwd");
        let (session, _count) = snap_session(cwd.path(), 2);
        let _application = publish_test_application(&session, cwd.path()).await;
        session
            .load_history(dense_history(5))
            .await
            .expect("load history");
        session.compact_snap().await.expect("snap compact");
        let kept_user_texts: Vec<String> =
            session.history().iter().filter_map(user_text_of).collect();
        assert_eq!(kept_user_texts.len(), 2, "keep-turns setting is honored");
        assert!(
            kept_user_texts
                .first()
                .is_some_and(|t| t.contains("ask number 3"))
        );
        assert!(
            kept_user_texts
                .last()
                .is_some_and(|t| t.contains("ask number 4"))
        );
    }

    #[tokio::test]
    async fn snap_compact_reports_shrinking_counts_when_usage_anchors_before() {
        let cwd = tempfile::tempdir().expect("cwd");
        let (session, _count) = snap_session(cwd.path(), 2);
        let _application = publish_test_application(&session, cwd.path()).await;
        let mut history = dense_history(12);
        // Every live session's final assistant turn carries the real context
        // usage the provider reported. Without it the usage-aware pre-count
        // falls back to the pure heuristic, masking the stale-anchor bug where
        // the after-count reused the same usage total and reported no shrink.
        if let Some(Message::Assistant(assistant)) = history.last_mut() {
            assistant.usage = Usage {
                total_tokens: 64_154,
                ..Usage::default()
            };
        }
        session.load_history(history).await.expect("load history");
        let result = session.compact_snap().await.expect("snap compact");
        assert_eq!(
            result.tokens_before, 64_154,
            "pre-compaction count anchors on the last real usage"
        );
        assert!(
            result
                .estimated_tokens_after
                .is_some_and(|after| after < result.tokens_before),
            "post-compaction estimate must cover summary + kept tail, not the stale anchor: before={} after={:?}",
            result.tokens_before,
            result.estimated_tokens_after,
        );
    }

    fn tool_result(text: &str, is_error: bool, ts: i64) -> Message {
        Message::ToolResult(pi_ai::ToolResultMessage {
            tool_call_id: "t".to_string(),
            tool_name: "read".to_string(),
            content: vec![ContentBlock::Text {
                text: text.to_string(),
                text_signature: None,
            }],
            usage: None,
            details: None,
            added_tool_names: Vec::new(),
            is_error,
            timestamp: ts,
        })
    }

    #[tokio::test]
    async fn snap_compact_elides_useless_results_and_notes_them() {
        let cwd = tempfile::tempdir().expect("cwd");
        let (session, count) = snap_session(cwd.path(), 2);
        let _application = publish_test_application(&session, cwd.path()).await;
        // Turn 1: an empty result + a duplicate error (useless); turns 2-3: clean.
        let history = vec![
            Message::user_text("first ask", 1),
            tool_result("   ", false, 2),
            tool_result("read failed: missing", true, 3),
            tool_result("read failed: missing", true, 4),
            Message::user_text("second ask", 5),
            tool_result("ok", false, 6),
            Message::user_text("third ask", 7),
            tool_result("done", false, 8),
        ];
        session.load_history(history).await.expect("load history");
        let result = session.compact_snap().await.expect("snap compact");
        assert_eq!(count.load(Ordering::SeqCst), 0, "no provider call");
        assert!(
            result.summary.contains("[elided 2 useless results]"),
            "summary must note the elided count: {}",
            result.summary,
        );
        // The kept tail (turn 2) is untouched; only the archived turn 1 was elided.
        let kept = session.history();
        assert!(kept.iter().any(|m| matches!(m, Message::ToolResult(tr) if tr.content.first().is_some_and(|b| matches!(b, ContentBlock::Text { text, .. } if text == "ok")))));
    }

    #[tokio::test]
    async fn snap_compact_nothing_to_compact_when_too_small() {
        let cwd = tempfile::tempdir().expect("cwd");
        let (session, _count) = snap_session(cwd.path(), 10);
        let _application = publish_test_application(&session, cwd.path()).await;
        session
            .load_history(dense_history(5))
            .await
            .expect("load history");
        let error = session
            .compact_snap()
            .await
            .expect_err("too few turns to archive");
        assert!(
            format!("{error:#}").contains("Nothing to compact"),
            "expected an actionable too-small error: {error:#}"
        );
        // The transcript is untouched on failure.
        assert_eq!(session.history().len(), 10);
    }

    #[tokio::test]
    async fn snap_compact_transcript_stays_well_formed_for_replay() {
        let cwd = tempfile::tempdir().expect("cwd");
        let (session, _count) = snap_session(cwd.path(), 3);
        let _application = publish_test_application(&session, cwd.path()).await;
        session
            .load_history(dense_history(6))
            .await
            .expect("load history");
        session.compact_snap().await.expect("snap compact");
        let history = session.history();
        // Replay projection never panics and folds the summary into user text.
        let llm = messages_as_llm(&history);
        assert!(!llm.is_empty());
        assert!(
            matches!(llm.first(), Some(Message::User(_))),
            "the compaction summary projects to a user message for the provider"
        );
        let first_text = llm
            .first()
            .and_then(|m| match m {
                Message::User(u) => u.content.iter().find_map(|b| {
                    if let ContentBlock::Text { text, .. } = b {
                        Some(text.clone())
                    } else {
                        None
                    }
                }),
                _ => None,
            })
            .unwrap_or_default();
        assert!(
            first_text.contains("compacted into the following summary"),
            "replay wraps the summary: {first_text}"
        );
    }

    #[tokio::test]
    async fn snap_compact_writes_lossless_archive_sidecar() {
        let cwd = tempfile::tempdir().expect("cwd");
        let (session, _count) = snap_session(cwd.path(), 2);
        let _application = publish_test_application(&session, cwd.path()).await;
        let recorder = crate::start_session_in(
            cwd.path(),
            None,
            None,
            Some(cwd.path()),
            Some("snapcompact-archive-test"),
            None,
        )
        .expect("start recorder");
        // Record the messages into the session file so the recorder tree and the
        // live transcript agree (the real run loop records each message; tests
        // using load_history bypass that, so populate the journal explicitly).
        let history = dense_history(4);
        for message in &history {
            recorder.record_message(message).expect("record message");
        }
        let session_path = recorder.path();
        session.record(recorder).expect("attach recorder");
        session.load_history(history).await.expect("load history");
        session.compact_snap().await.expect("snap compact");

        let sidecars = std::fs::read_dir(session_path.parent().expect("session dir"))
            .expect("read session dir")
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".snapcompact-"))
            .collect::<Vec<_>>();
        assert_eq!(
            sidecars.len(),
            1,
            "exactly one snapcompact sidecar: {sidecars:?}"
        );
        let sidecar_path = session_path.parent().unwrap().join(&sidecars[0]);
        let records = std::fs::read_to_string(&sidecar_path).expect("read sidecar");
        let archived = records.lines().filter(|line| !line.is_empty()).count();
        // Turn 1 + turn 2 (2 user + 2 assistant) archived; the last 2 turns stay.
        assert_eq!(
            archived, 4,
            "the original archived entries are preserved verbatim: {records}"
        );
        assert!(
            records.contains("\"type\":\"message\""),
            "sidecar records use the session-file shape"
        );
    }

    /// True when the test process runs as root (uid 0), which bypasses
    /// directory write permissions — the unwritable-dir scenario cannot be
    /// constructed, so the test is skipped there.
    fn running_as_root() -> bool {
        std::fs::read_to_string("/proc/self/status")
            .map(|status| {
                status
                    .lines()
                    .find_map(|line| line.strip_prefix("Uid:"))
                    .is_some_and(|line| line.split_whitespace().next() == Some("0"))
            })
            .unwrap_or(false)
    }

    /// Counts committed compaction records in the session journal file.
    fn compaction_record_count(session_path: &std::path::Path) -> usize {
        std::fs::read_to_string(session_path)
            .expect("read session journal")
            .lines()
            .filter(|line| !line.trim().is_empty() && line.contains("\"type\":\"compaction\""))
            .count()
    }

    #[tokio::test]
    async fn snap_compact_archive_failure_commits_no_compaction_record() {
        let cwd = tempfile::tempdir().expect("cwd");
        let (session, _count) = snap_session(cwd.path(), 2);
        let _application = publish_test_application(&session, cwd.path()).await;
        let recorder = crate::start_session_in(
            cwd.path(),
            None,
            None,
            Some(cwd.path()),
            Some("snapcompact-unwritable-test"),
            None,
        )
        .expect("start recorder");
        let history = dense_history(4);
        for message in &history {
            recorder.record_message(message).expect("record message");
        }
        let session_path = recorder.path();
        session.record(recorder).expect("attach recorder");
        session.load_history(history).await.expect("load history");
        let dir = session_path.parent().expect("session dir");

        if running_as_root() {
            eprintln!("skipping unwritable-dir scenario: running as root");
            return;
        }
        // Make the session directory unwritable: archive creation must fail
        // BEFORE any compaction record is committed, so the journal never
        // carries a compaction without its lossless sidecar.
        let previous = std::fs::metadata(dir).expect("dir metadata").permissions();
        let mut readonly = previous.clone();
        use std::os::unix::fs::PermissionsExt;
        readonly.set_mode(0o555);
        std::fs::set_permissions(dir, readonly).expect("make session dir read-only");
        let error = session
            .compact_snap()
            .await
            .expect_err("archive creation must fail");
        std::fs::set_permissions(dir, previous).expect("restore session dir permissions");
        assert!(
            format!("{error:#}").contains("snapcompact archive"),
            "error must name the failing archive: {error:#}"
        );
        assert_eq!(
            compaction_record_count(&session_path),
            0,
            "a failed compaction must not commit a journal record without its archive"
        );

        // The same compaction succeeds once the directory is writable again,
        // committing exactly one record and one sidecar.
        session
            .compact_snap()
            .await
            .expect("snap compact after restore");
        assert_eq!(compaction_record_count(&session_path), 1);
        let sidecars = std::fs::read_dir(dir)
            .expect("read session dir")
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".snapcompact-"))
            .collect::<Vec<_>>();
        assert_eq!(
            sidecars.len(),
            1,
            "exactly one sidecar after retry: {sidecars:?}"
        );
    }

    #[test]
    fn snap_compact_archive_name_collision_retries_with_fresh_stamp() {
        let cwd = tempfile::tempdir().expect("cwd");
        let recorder = crate::start_session_in(
            cwd.path(),
            None,
            None,
            Some(cwd.path()),
            Some("snapcompact-collision-test"),
            None,
        )
        .expect("start recorder");
        recorder
            .record_message(&Message::user_text("archived", 1))
            .expect("record message");
        let session_path = recorder.path();
        let tree = recorder.tree().expect("recorder tree");
        let entries = tree
            .branch(None)
            .into_iter()
            .filter(|entry| entry.entry_type == "message")
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 1, "one archived entry");

        // A stale archive already occupies the first candidate name: the
        // writer must refresh the stamp and succeed instead of failing, and
        // must never overwrite the existing archive.
        let file_name = session_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("session");
        let stale = session_path.with_file_name(format!("{file_name}.snapcompact-same.jsonl"));
        std::fs::write(&stale, "stale archive payload\n").expect("pre-create colliding archive");

        let mut stamps = ["same".to_owned(), "fresh".to_owned()].into_iter();
        let written = write_snapcompact_archive_at(&session_path, &entries, || {
            stamps.next().expect("stamp sequence exhausted").clone()
        })
        .expect("collision retry must succeed");
        let written_name = written
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .expect("archive file name");
        assert!(
            written_name.ends_with(".snapcompact-fresh.jsonl"),
            "the archive must be written under the retried stamp: {written_name}"
        );
        assert_eq!(
            std::fs::read_to_string(&stale).expect("read stale archive"),
            "stale archive payload\n",
            "an existing archive must never be overwritten"
        );
        let records = std::fs::read_to_string(&written).expect("read written archive");
        assert!(
            records.contains("\"type\":\"message\""),
            "archive holds the session-file shape"
        );
    }
}

#[cfg(test)]
mod session_stats_context_usage_tests {
    use super::*;
    use crate::is_compaction_boundary;

    fn stats_session(cwd: &std::path::Path) -> Session {
        test_session_with_discovery(SessionOptions {
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
            auth_manager: None,
        })
    }

    fn usage_anchored_history() -> Vec<Message> {
        let padding = "x".repeat(4000);
        let mut messages = Vec::new();
        for turn in 0..6 {
            messages.push(Message::user_text(
                format!("ask {turn}: {padding}"),
                turn as i64 * 2,
            ));
            let mut assistant = pi_ai::AssistantMessage::pending(&Model::default());
            assistant.content = vec![ContentBlock::text(format!("answer {turn}: {padding}"))];
            assistant.stop_reason = StopReason::Stop;
            assistant.timestamp = turn as i64 * 2 + 1;
            if turn == 5 {
                assistant.usage = Usage {
                    total_tokens: 64_154,
                    ..Usage::default()
                };
            }
            messages.push(Message::Assistant(assistant));
        }
        messages
    }

    #[tokio::test]
    async fn session_stats_uses_usage_aware_tokens_for_ordinary_history() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = stats_session(cwd.path());
        let history = usage_anchored_history();
        let expected = estimate_context_tokens_usage_aware(&history);
        session.load_history(history).await.expect("load history");

        let usage = session
            .session_stats()
            .context_usage
            .expect("context usage");
        assert_eq!(
            usage.tokens,
            Some(expected),
            "ordinary history keeps the last-assistant usage-aware estimate"
        );
        assert_eq!(usage.tokens, Some(64_154));
        assert_eq!(usage.context_window, 128000);
        assert_eq!(
            usage.percent,
            Some(64_154_f64 / 128000.0 * 100.0),
            "percent remains tokens / contextWindow"
        );
        assert!(
            !session.history().iter().any(is_compaction_boundary),
            "ordinary history has no CompactionSummary cut"
        );
    }

    #[tokio::test]
    async fn session_stats_uses_compacted_history_tokens_after_compaction_cut() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = stats_session(cwd.path());
        let history = usage_anchored_history();
        session
            .load_history(history.clone())
            .await
            .expect("load history");
        let before = session
            .session_stats()
            .context_usage
            .expect("pre-compaction context usage");

        let compacted = apply_checkpoint("## Summary\narchived history", &history, 10);
        assert!(
            compacted.iter().any(is_compaction_boundary),
            "compacted history contains the typed CompactionSummary cut"
        );
        let expected = estimate_context_tokens(&compacted);
        session
            .load_history(compacted)
            .await
            .expect("load compacted history");

        let after = session
            .session_stats()
            .context_usage
            .expect("post-compaction context usage");
        assert_eq!(
            after.tokens,
            Some(expected),
            "compacted history must use estimate_context_tokens, not the stale usage anchor"
        );
        assert_eq!(before.tokens, Some(64_154));
        assert_ne!(
            after.tokens, before.tokens,
            "post-cut tokens must not reuse the discarded pre-cut provider usage"
        );
        assert!(
            after.tokens.expect("tokens") < before.tokens.expect("tokens"),
            "compacted context shrinks: before={:?} after={:?}",
            before.tokens,
            after.tokens
        );
        assert_eq!(after.context_window, before.context_window);
        assert_eq!(
            after.percent,
            after.tokens.map(|tokens| tokens as f64 / after.context_window as f64 * 100.0),
            "percent remains tokens / contextWindow"
        );
    }

    #[tokio::test]
    async fn session_stats_uses_usage_aware_tokens_after_post_cut_assistant() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = stats_session(cwd.path());
        let history = usage_anchored_history();
        let mut compacted = apply_checkpoint("## Summary\narchived history", &history, 10);
        let cut_timestamp = match compacted.first() {
            Some(Message::CompactionSummary(summary)) => summary.timestamp,
            other => panic!("compacted history must start with CompactionSummary, got {other:?}"),
        };
        let mut assistant = pi_ai::AssistantMessage::pending(&Model::default());
        assistant.content = vec![ContentBlock::text("post-cut answer")];
        assistant.stop_reason = StopReason::Stop;
        assistant.timestamp = cut_timestamp + 1;
        assistant.usage = Usage {
            total_tokens: 1_200,
            ..Usage::default()
        };
        compacted.push(Message::Assistant(assistant));
        compacted.push(Message::user_text("trailing user after the post-cut turn", cut_timestamp + 2));

        let expected = estimate_context_tokens_usage_aware(&compacted);
        let pure = estimate_context_tokens(&compacted);
        session
            .load_history(compacted)
            .await
            .expect("load post-cut history");

        let usage = session
            .session_stats()
            .context_usage
            .expect("context usage");
        assert_eq!(
            usage.tokens,
            Some(expected),
            "summary + post-cut assistant usage + trailing user must use usage-aware"
        );
        assert_eq!(expected, 1_200 + estimate_message_tokens(&Message::user_text(
            "trailing user after the post-cut turn",
            cut_timestamp + 2
        )));
        assert_ne!(
            usage.tokens,
            Some(pure),
            "must not stay on the pure compacted estimate after a valid post-cut assistant"
        );
        assert_ne!(
            usage.tokens,
            Some(64_154),
            "must not reuse the discarded pre-cut provider usage"
        );
        assert_eq!(usage.context_window, 128000);
        assert_eq!(
            usage.percent,
            usage.tokens.map(|tokens| tokens as f64 / usage.context_window as f64 * 100.0)
        );
    }


    #[test]
    fn compaction_boundary_is_typed_compaction_summary_not_custom_or_text() {
        assert!(is_compaction_boundary(&Message::CompactionSummary(
            pi_ai::CompactionSummaryMessage {
                summary: "cut".into(),
                tokens_before: 0,
                details: None,
                usage: None,
                from_hook: None,
                timestamp: 1,
            }
        )));
        assert!(!is_compaction_boundary(&Message::user_text(
            "The conversation history before this point was compacted into the following summary",
            1
        )));
        assert!(!is_compaction_boundary(&Message::Custom(CustomMessage {
            custom_type: "pi.goal.active".into(),
            content: "compaction summary lookalike".into(),
            display: false,
            details: None,
            timestamp: 1,
        })));
        assert!(!is_compaction_boundary(&Message::BranchSummary(
            pi_ai::BranchSummaryMessage {
                summary: "branch".into(),
                from_id: "root".into(),
                details: None,
                usage: None,
                from_hook: None,
                timestamp: 1,
            }
        )));
    }
}


#[cfg(test)]
mod ask_tool_round_trip_tests {
    use super::*;

    fn ask_session(cwd: &std::path::Path) -> Session {
        test_session_with_discovery(SessionOptions {
            model: Model::default(),
            cwd: cwd.to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: None,
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        })
    }

    fn ask_tool(session: &Session) -> AgentTool {
        session
            .get_all_tools()
            .into_iter()
            .find(|tool| tool.name == "ask")
            .expect("session tool set includes ask")
    }

    fn tool_context(
        question: serde_json::Value,
    ) -> (pi_agent::ToolCallContext, pi_agent::AbortController) {
        let (controller, abort) = AbortController::new();
        let context = pi_agent::ToolCallContext {
            tool_call_id: "call-ask".to_owned(),
            arguments: question,
            on_update: Arc::new(|_| {}),
            abort,
            model: None,
            approval_token: None,
        };
        (context, controller)
    }

    /// Waits for the application to publish the `AskUser` event for the
    /// pending question and returns its id.
    async fn await_ask_event(application: &crate::Application) -> String {
        let mut events = application.subscribe();
        loop {
            match events.recv().await.expect("application events") {
                crate::ApplicationEvent::Session(SessionEvent::AskUser { id, prompt }) => {
                    assert!(!prompt.is_empty());
                    return id;
                }
                crate::ApplicationEvent::Agent(AgentEvent::ToolExecutionStart { .. }) => continue,
                _ => continue,
            }
        }
    }
    #[tokio::test]
    async fn ask_round_trip_answer_flows_back_as_tool_result() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = ask_session(cwd.path());
        let application = publish_test_application(&session, cwd.path()).await;
        application.set_ask_interactive(true);
        let tool = ask_tool(&session);
        let (context, _controller) = tool_context(serde_json::json!({ "question": "continue?" }));
        let run = tokio::spawn(async move { (tool.execute)(context).await });
        let id = await_ask_event(&application).await;
        assert_eq!(
            application
                .pending_ask()
                .as_ref()
                .map(|(pending_id, _)| pending_id),
            Some(&id)
        );
        application
            .answer_ask(&id, "yes, please".to_owned())
            .expect("answer delivered");
        let result = run.await.expect("join").expect("ask succeeds");
        assert_eq!(result.content, vec![ContentBlock::text("yes, please")]);
        assert!(application.pending_ask().is_none());
    }
    #[tokio::test]
    async fn ask_requires_interactive_session_in_other_modes() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = ask_session(cwd.path());
        let application = publish_test_application(&session, cwd.path()).await;
        // Interactive flag never armed: print/JSON/RPC/REPL behavior.
        let tool = ask_tool(&session);
        let (context, _controller) = tool_context(serde_json::json!({ "question": "continue?" }));
        let error = (tool.execute)(context)
            .await
            .expect_err("non-interactive ask must reject");
        assert!(
            error
                .to_string()
                .contains("ask requires an interactive session"),
            "{error}"
        );
        assert!(application.pending_ask().is_none());
    }
    #[tokio::test]
    async fn ask_times_out_when_unanswered() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = ask_session(cwd.path());
        let application = publish_test_application(&session, cwd.path()).await;
        application.set_ask_interactive(true);
        application.set_ask_timeout(Duration::from_millis(50));
        let tool = ask_tool(&session);
        let (context, _controller) = tool_context(serde_json::json!({ "question": "continue?" }));
        let run = tokio::spawn(async move { (tool.execute)(context).await });
        let id = await_ask_event(&application).await;
        let error = run.await.expect("join").expect_err("timeout must reject");
        assert!(
            error.to_string().contains("timed out waiting for user"),
            "{error}"
        );
        assert!(
            application.pending_ask().is_none(),
            "timeout must free the pending slot"
        );
        let _ = id;
    }
    #[tokio::test]
    async fn concurrent_asks_reject_second_with_busy_error() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = ask_session(cwd.path());
        let application = publish_test_application(&session, cwd.path()).await;
        application.set_ask_interactive(true);
        let tool = ask_tool(&session);
        let (first_context, _first_controller) =
            tool_context(serde_json::json!({ "question": "first?" }));
        let (second_context, _second_controller) =
            tool_context(serde_json::json!({ "question": "second?" }));
        let first_tool = tool.clone();
        let first = tokio::spawn(async move { (first_tool.execute)(first_context).await });
        let second_tool = tool.clone();
        let second = tokio::spawn(async move { (second_tool.execute)(second_context).await });
        let id = await_ask_event(&application).await;
        // Answer before awaiting the joins: the winner's request only resolves
        // once the answer is delivered, and on a single-thread test runtime
        // joining first would deadlock the answer path.
        application
            .answer_ask(&id, "first".to_owned())
            .expect("answer");
        let first = first.await.expect("join");
        let second = second.await.expect("join");
        let (winner, loser) = match (first, second) {
            (Ok(_), Err(error)) => (None, Some(error)),
            (Err(error), Ok(_)) => (Some(error), None),
            other => panic!("expected exactly one busy rejection, got {other:?}"),
        };
        let loser = loser.or(winner).expect("one busy error");
        assert!(
            loser
                .to_string()
                .contains("another question is already pending"),
            "{loser}"
        );
    }
    #[tokio::test]
    async fn cancel_ask_aborts_with_cancelled_result() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = ask_session(cwd.path());
        let application = publish_test_application(&session, cwd.path()).await;
        application.set_ask_interactive(true);
        let tool = ask_tool(&session);
        let (context, _controller) = tool_context(serde_json::json!({ "question": "continue?" }));
        let run = tokio::spawn(async move { (tool.execute)(context).await });
        let id = await_ask_event(&application).await;
        application.cancel_ask(&id).expect("cancel");
        let error = run.await.expect("join").expect_err("cancel must reject");
        assert!(error.to_string().contains("cancelled"), "{error}");
        assert!(application.pending_ask().is_none());
    }
    #[tokio::test]
    async fn shutdown_cancel_pending_ask_aborts_with_cancelled_result() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = ask_session(cwd.path());
        let application = publish_test_application(&session, cwd.path()).await;
        application.set_ask_interactive(true);
        let tool = ask_tool(&session);
        let (context, _controller) = tool_context(serde_json::json!({ "question": "continue?" }));
        let run = tokio::spawn(async move { (tool.execute)(context).await });
        let _id = await_ask_event(&application).await;
        // TUI shutdown cancels whatever is pending without an id; the awaiting
        // tool call resolves as cancelled and the slot frees.
        assert!(
            application.cancel_pending_ask(),
            "a pending ask was cancelled"
        );
        assert!(!application.cancel_pending_ask(), "no second pending ask");
        let error = run.await.expect("join").expect_err("cancel must reject");
        assert!(error.to_string().contains("cancelled"), "{error}");
        assert!(application.pending_ask().is_none());
    }

    #[tokio::test]
    async fn standalone_ask_tool_rejects_without_session_binding() {
        let cwd = tempfile::tempdir().expect("cwd");
        let tool = crate::tools::ask::standalone_ask_tool();
        let (context, _controller) = tool_context(serde_json::json!({ "question": "continue?" }));
        let error = (tool.execute)(context)
            .await
            .expect_err("standalone ask must reject");
        assert!(error.to_string().contains("interactive session"), "{error}");
    }
}

#[cfg(test)]
mod doom_loop_recovery_tests {
    use std::fs;

    use pi_ai::providers::{
        FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
    };

    use super::*;

    fn faux_session(cwd: &std::path::Path, tag: &str) -> (Session, FauxProviderRegistration) {
        let suffix = Uuid::now_v7().to_string();
        let api = format!("doom-{tag}-api-{suffix}");
        let provider = format!("doom-{tag}-provider-{suffix}");
        let model = Model {
            id: format!("doom-{tag}-model"),
            name: format!("Doom {tag} Model"),
            api: api.clone(),
            provider: provider.clone(),
            ..Model::default()
        };
        let registration = register_faux_provider(FauxProviderOptions {
            api,
            provider,
            models: vec![model.clone()],
            chunk_size: 1,
        });
        let session = test_session_with_discovery(SessionOptions {
            model,
            cwd: cwd.to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: None,
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        });
        (session, registration)
    }

    fn read_call(id: &str, path: &str) -> FauxResponse {
        FauxResponse {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: id.to_owned(),
                name: "read".to_owned(),
                arguments: serde_json::json!({ "path": path }),
                thought_signature: None,
            })],
            stop_reason: StopReason::ToolUse,
            error_message: None,
        }
    }

    fn bash_call(id: &str, command: &str) -> FauxResponse {
        FauxResponse {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: id.to_owned(),
                name: "bash".to_owned(),
                arguments: serde_json::json!({ "command": command }),
                thought_signature: None,
            })],
            stop_reason: StopReason::ToolUse,
            error_message: None,
        }
    }

    fn settle() -> FauxResponse {
        FauxResponse::text("settled")
    }

    fn error_texts(messages: &[Message]) -> Vec<String> {
        let mut texts = Vec::new();
        for message in messages {
            if let Message::ToolResult(result) = message
                && result.is_error
            {
                let mut text = String::new();
                for block in &result.content {
                    if let ContentBlock::Text {
                        text: block_text, ..
                    } = block
                    {
                        text.push_str(block_text);
                    }
                }
                texts.push(text);
            }
        }
        texts
    }

    #[tokio::test]
    async fn identical_consecutive_tool_failures_stop_the_turn() {
        let cwd = tempfile::tempdir().expect("cwd");
        let missing = cwd.path().join("does-not-exist.txt");
        let (session, registration) = faux_session(cwd.path(), "triple-fail");
        let _application = publish_test_application(&session, cwd.path()).await;
        registration.set_responses(vec![
            read_call("call-1", missing.to_string_lossy().as_ref()),
            read_call("call-2", missing.to_string_lossy().as_ref()),
            read_call("call-3", missing.to_string_lossy().as_ref()),
        ]);

        let error = session
            .run("read the file", Vec::new())
            .await
            .expect_err("three identical failures must stop the turn");

        let message = error.to_string();
        assert!(
            message.contains("repeated failure")
                && message.contains("/read")
                && message.contains("/undo"),
            "doom-loop message must be actionable, got: {message}"
        );
        let texts = error_texts(&session.history());
        assert!(
            texts.iter().any(|text| text.contains("repeated failure")),
            "the final tool result must carry the doom-loop message, got: {texts:?}"
        );
        registration.unregister();
    }

    #[tokio::test]
    async fn interleaved_success_resets_the_failure_run() {
        let cwd = tempfile::tempdir().expect("cwd");
        fs::write(cwd.path().join("notes.txt"), "hello world").expect("write notes");
        let missing = cwd.path().join("does-not-exist.txt");
        let (session, registration) = faux_session(cwd.path(), "interleaved");
        let _application = publish_test_application(&session, cwd.path()).await;
        registration.set_responses(vec![
            read_call("call-1", missing.to_string_lossy().as_ref()),
            read_call("call-2", missing.to_string_lossy().as_ref()),
            read_call("call-ok", "notes.txt"),
            read_call("call-4", missing.to_string_lossy().as_ref()),
            read_call("call-5", missing.to_string_lossy().as_ref()),
            settle(),
        ]);

        // Two failures, then a success, then two more failures: the run of
        // identical failures never reaches the threshold, so the turn settles
        // normally instead of being stopped as a doom loop.
        let result = session
            .run("read some files", Vec::new())
            .await
            .expect("run must settle normally");
        assert!(result.text.contains("settled"), "{}", result.text);
        registration.unregister();
    }

    #[tokio::test]
    async fn differing_error_text_resets_the_failure_run() {
        let cwd = tempfile::tempdir().expect("cwd");
        let (session, registration) = faux_session(cwd.path(), "diff-errors");
        let _application = publish_test_application(&session, cwd.path()).await;
        registration.set_responses(vec![
            bash_call("call-1", "printf 'boom one\\n'; exit 1"),
            bash_call("call-2", "printf 'boom one\\n'; exit 1"),
            bash_call("call-3", "printf 'boom two\\n'; exit 1"),
            bash_call("call-4", "printf 'boom two\\n'; exit 1"),
            settle(),
        ]);

        let result = session
            .run("run some commands", Vec::new())
            .await
            .expect("different errors are not a doom loop");
        assert!(result.text.contains("settled"), "{}", result.text);
        registration.unregister();
    }

    #[tokio::test]
    async fn transient_network_errors_never_trip_the_doom_loop() {
        let cwd = tempfile::tempdir().expect("cwd");
        let (session, registration) = faux_session(cwd.path(), "transient");
        let _application = publish_test_application(&session, cwd.path()).await;
        registration.set_responses(vec![
            bash_call("net-1", "printf 'network timed out\\n'; exit 1"),
            bash_call("net-2", "printf 'network timed out\\n'; exit 1"),
            bash_call("net-3", "printf 'network timed out\\n'; exit 1"),
            bash_call("net-4", "printf 'network timed out\\n'; exit 1"),
            settle(),
        ]);

        // Four identical transient-looking failures: well past the threshold,
        // but transient network blips must never count toward a doom loop.
        let result = session
            .run("ping the registry", Vec::new())
            .await
            .expect("transient errors are not a doom loop");
        assert!(result.text.contains("settled"), "{}", result.text);
        registration.unregister();
    }

    /// A transient marker past `DOOM_LOOP_ERROR_PREFIX_CHARS` must not reset the run.
    #[tokio::test]
    async fn transient_marker_beyond_the_fingerprint_cap_does_not_evade_the_doom_loop() {
        let cwd = tempfile::tempdir().expect("cwd");
        let (session, registration) = faux_session(cwd.path(), "transient-past-cap");
        let failure = pi_agent::AgentToolResult::text(format!(
            "{} connection timed out",
            "x".repeat(DOOM_LOOP_ERROR_PREFIX_CHARS + 10)
        ));
        let context = after_context("bash", serde_json::json!({ "cmd": "a" }), failure, true);

        let first = doom_loop_recovery(&session.inner.shared.doom_loop, &context)
            .await
            .expect("first failure");
        let second = doom_loop_recovery(&session.inner.shared.doom_loop, &context)
            .await
            .expect("second failure");
        assert_ne!(first.terminate, Some(true));
        assert_ne!(
            second.terminate,
            Some(true),
            "the past-cap transient marker must not reset the failure run"
        );
        let third = doom_loop_recovery(&session.inner.shared.doom_loop, &context)
            .await
            .expect("third failure");
        assert_eq!(
            third.terminate,
            Some(true),
            "the generic doom loop must still trip at three identical failures"
        );
        registration.unregister();
    }

    #[tokio::test]
    async fn doom_loop_detection_is_scoped_to_the_current_turn() {
        let cwd = tempfile::tempdir().expect("cwd");
        let missing = cwd.path().join("does-not-exist.txt");
        let (session, registration) = faux_session(cwd.path(), "per-turn");
        let _application = publish_test_application(&session, cwd.path()).await;
        registration.set_responses(vec![
            // Turn 1: three identical failures -> doom loop stops the turn.
            read_call("t1-call-1", missing.to_string_lossy().as_ref()),
            read_call("t1-call-2", missing.to_string_lossy().as_ref()),
            read_call("t1-call-3", missing.to_string_lossy().as_ref()),
            // Turn 2: same failures, but the counter starts fresh per turn,
            // so two identical failures plus a settle end normally.
            read_call("t2-call-1", missing.to_string_lossy().as_ref()),
            read_call("t2-call-2", missing.to_string_lossy().as_ref()),
            settle(),
        ]);

        let first = session
            .run("first turn", Vec::new())
            .await
            .expect_err("turn 1 must trip the doom loop");
        assert!(first.to_string().contains("repeated failure"), "{first}");

        let second = session
            .run("second turn", Vec::new())
            .await
            .expect("turn 2 starts with a fresh counter");
        assert!(second.text.contains("settled"), "{}", second.text);
        registration.unregister();
    }

    #[test]
    fn error_prefix_is_lowercase_collapsed_and_capped() {
        let result = pi_agent::AgentToolResult::text("ERROR:   no such  file\n(system message)");
        let prefix = doom_loop_error_prefix(&result).expect("prefix");
        assert_eq!(prefix, "error: no such file (system message)");
        let long = pi_agent::AgentToolResult::text("x".repeat(300));
        let prefix = doom_loop_error_prefix(&long).expect("prefix");
        assert_eq!(prefix.len(), DOOM_LOOP_ERROR_PREFIX_CHARS);
        assert!(prefix.chars().all(|c| c == 'x'));
        assert!(doom_loop_error_prefix(&pi_agent::AgentToolResult::default()).is_none());
    }

    #[test]
    fn deterministic_arguments_fingerprint_is_key_order_independent() {
        let straight = serde_json::json!({ "path": "outside.txt", "offset": 1 });
        let shuffled = serde_json::json!({ "offset": 1, "path": "outside.txt" });
        assert_eq!(
            doom_args_fingerprint(&straight),
            doom_args_fingerprint(&shuffled)
        );
        assert_ne!(
            doom_args_fingerprint(&straight),
            doom_args_fingerprint(&serde_json::json!({ "path": "other.txt", "offset": 1 }))
        );
    }

    fn after_context(
        tool_name: &str,
        arguments: serde_json::Value,
        result: pi_agent::AgentToolResult,
        is_error: bool,
    ) -> pi_agent::AfterToolCallContext {
        pi_agent::AfterToolCallContext {
            assistant_message: pi_ai::AssistantMessage::pending(&Model::default()),
            tool_call: ToolCall {
                id: "call-1".to_owned(),
                name: tool_name.to_owned(),
                arguments: arguments.clone(),
                thought_signature: None,
            },
            arguments,
            result,
            is_error,
            context: pi_agent::AgentContext {
                system_prompt: String::new(),
                messages: Vec::new(),
                tools: Vec::new(),
            },
        }
    }

    fn result_text(result: &pi_agent::AfterToolCallResult) -> String {
        result
            .content
            .as_ref()
            .into_iter()
            .flatten()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn workspace_escape_terminates_on_the_first_error() {
        let cwd = tempfile::tempdir().expect("cwd");
        let (session, registration) = faux_session(cwd.path(), "workspace-escape");
        let arguments = serde_json::json!({ "path": "/etc/shadow" });
        let failure = pi_agent::AgentToolResult::text(
            "path_outside_workspace: <absolute path> is outside authorized workspace roots",
        );
        let context = after_context("read", arguments, failure, true);

        let first = doom_loop_recovery(&session.inner.shared.doom_loop, &context)
            .await
            .expect("first workspace-escape failure");
        assert_eq!(
            first.terminate,
            Some(true),
            "the first workspace escape must stop the turn immediately"
        );
        let message = result_text(&first);
        assert!(
            message.contains("path_outside_workspace")
                && message.contains("/read")
                && message.contains("--add-dir <project-root>"),
            "workspace-escape message must name the host-level remedy, got: {message}"
        );
        // The stop message is bounded and must never echo the rejected
        // location (here only a placeholder, but the replacement text is
        // fixed and contains no location at all).
        assert!(
            !message.contains("/etc/shadow") && !message.contains(cwd.path().to_string_lossy().as_ref()),
            "stop message must not echo the rejected path, got: {message}"
        );
        registration.unregister();
    }

    #[tokio::test]
    async fn confined_capability_rejection_terminates_on_the_first_error() {
        let cwd = tempfile::tempdir().expect("cwd");
        let (session, registration) = faux_session(cwd.path(), "capability-escape");
        let context = after_context(
            "read",
            serde_json::json!({ "path": "outside" }),
            pi_agent::AgentToolResult::text(FS_OUTSIDE_CAPABILITY_ERROR),
            true,
        );
        let first = doom_loop_recovery(&session.inner.shared.doom_loop, &context)
            .await
            .expect("first confined capability rejection");
        assert_eq!(
            first.terminate,
            Some(true),
            "the exact OutsideCapability display must stop the turn immediately"
        );
        registration.unregister();
    }

    #[tokio::test]
    async fn triggered_workspace_escape_stops_every_following_tool_outcome() {
        let cwd = tempfile::tempdir().expect("cwd");
        let (session, registration) = faux_session(cwd.path(), "escape-triggered");
        let escape = pi_agent::AgentToolResult::text(
            "path_outside_workspace: <absolute path> is outside authorized workspace roots",
        );
        let first = after_context("read", serde_json::json!({ "path": "/etc/shadow" }), escape, true);
        let pinned = doom_loop_recovery(&session.inner.shared.doom_loop, &first)
            .await
            .expect("first workspace-escape failure");
        assert_eq!(pinned.terminate, Some(true));

        // A different tool with different arguments after the stop must also
        // terminate with the same pinned message: a parallel batch cannot
        // escape the stop by switching to a shell search.
        let second = after_context(
            "bash",
            serde_json::json!({ "command": "cat /etc/shadow" }),
            pi_agent::AgentToolResult::default(),
            false,
        );
        let follow = doom_loop_recovery(&session.inner.shared.doom_loop, &second)
            .await
            .expect("follow-up tool outcome");
        assert_eq!(follow.terminate, Some(true));
        assert_eq!(result_text(&follow), result_text(&pinned));
        registration.unregister();
    }

    #[tokio::test]
    async fn invalid_network_arguments_stop_on_the_second_call() {
        let cwd = tempfile::tempdir().expect("cwd");
        let (session, registration) = faux_session(cwd.path(), "invalid-network");
        let arguments = serde_json::json!({ "url": "https://example.invalid" });
        let failure = pi_agent::AgentToolResult::text("Invalid network arguments");
        let context = after_context("web_search", arguments, failure, true);

        let first = doom_loop_recovery(&session.inner.shared.doom_loop, &context)
            .await
            .expect("first invalid-network failure");
        assert_ne!(first.terminate, Some(true));

        let second = doom_loop_recovery(&session.inner.shared.doom_loop, &context)
            .await
            .expect("second invalid-network failure");
        assert_eq!(second.terminate, Some(true));
        let message = result_text(&second);
        assert!(
            message.contains("repeated identical") && message.contains("/web_search"),
            "Invalid network arguments must trip the deterministic threshold, got: {message}"
        );
        registration.unregister();
    }

    /// The session publishes a confined `workspace-write` sandbox so the escape
    /// exercises the executor's `PolicyFs` capability rejection
    /// (`FsError::OutsideCapability`), the only escape rejection `read` can
    /// produce under the current design.
    #[tokio::test]
    async fn first_workspace_escape_error_ends_the_turn_without_a_retry() {
        let cwd = tempfile::tempdir().expect("cwd");
        let outside = tempfile::tempdir().expect("outside");
        fs::write(outside.path().join("secret.txt"), "secret").expect("write outside file");
        let (session, registration) = faux_session(cwd.path(), "escape-e2e");
        let _application = publish_test_application_sandboxed(&session, cwd.path(), "workspace-write")
            .await;
        registration.set_responses(vec![
            read_call(
                "escape-1",
                outside.path().join("secret.txt").to_string_lossy().as_ref(),
            ),
            // Tripwire: consumed only if the first workspace escape fails to
            // terminate the turn.
            settle(),
        ]);

        let error = session
            .run("read the secret", Vec::new())
            .await
            .expect_err("the first workspace escape must end the turn");
        let message = error.to_string();
        assert!(
            message.contains("path_outside_workspace")
                && message.contains("--add-dir <project-root>"),
            "run error must carry the actionable workspace-escape stop, got: {message}"
        );
        assert!(
            !message.contains(outside.path().to_string_lossy().as_ref()),
            "stop must not echo the rejected absolute path, got: {message}"
        );
        // The recorded tool results keep the executor's redacted rejection
        // and never the host location.
        let texts = error_texts(&session.history());
        assert!(
            texts.iter().any(|text| text.contains("path_outside_workspace")),
            "the failing read must be recorded as a workspace-escape error, got: {texts:?}"
        );
        assert!(
            texts
                .iter()
                .all(|text| !text.contains(outside.path().to_string_lossy().as_ref())),
            "recorded results must not disclose the outside path, got: {texts:?}"
        );
        registration.unregister();
    }


    #[tokio::test]
    async fn changed_arguments_reset_the_deterministic_run() {
        let cwd = tempfile::tempdir().expect("cwd");
        let (session, registration) = faux_session(cwd.path(), "changed-args");
        let failure = pi_agent::AgentToolResult::text(
            "Invalid bash arguments: missing required field command. Allowed fields: command",
        );
        let first_args = serde_json::json!({ "cmd": "a" });
        let second_args = serde_json::json!({ "cmd": "b" });

        for (id, arguments) in [
            ("a1", first_args.clone()),
            ("b1", second_args.clone()),
            ("a2", first_args),
            ("b2", second_args),
        ] {
            let context = after_context("bash", arguments, failure.clone(), true);
            let outcome = doom_loop_recovery(&session.inner.shared.doom_loop, &context)
                .await
                .unwrap_or_else(|_| panic!("recovery for {id}"));
            assert_ne!(
                outcome.terminate,
                Some(true),
                "changed arguments must reset the deterministic run at {id}"
            );
        }
        registration.unregister();
    }

    #[tokio::test]
    async fn timeout_errors_stay_transient() {
        let cwd = tempfile::tempdir().expect("cwd");
        let (session, registration) = faux_session(cwd.path(), "bash-timeout");
        let _application = publish_test_application(&session, cwd.path()).await;
        let timed = |id: &str| FauxResponse {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: id.to_owned(),
                name: "bash".to_owned(),
                arguments: serde_json::json!({ "command": "sleep 5", "timeout": 0.3 }),
                thought_signature: None,
            })],
            stop_reason: StopReason::ToolUse,
            error_message: None,
        };
        registration.set_responses(vec![
            timed("t-1"),
            timed("t-2"),
            timed("t-3"),
            timed("t-4"),
            settle(),
        ]);

        let result = session
            .run("wait a moment", Vec::new())
            .await
            .expect("timeouts stay transient and never trip the doom loop");
        assert!(result.text.contains("settled"), "{}", result.text);
        registration.unregister();
    }
}

#[cfg(test)]
mod rewind_tests {
    use super::*;

    fn test_session(cwd: &Path) -> Session {
        test_session_with_discovery(SessionOptions {
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
            auth_manager: None,
        })
    }

    fn user_text(message: &Message) -> String {
        match message {
            Message::User(user) => content_text(&user.content),
            _ => String::new(),
        }
    }

    fn phase(name: &str, depends_on: &[&str]) -> TodoPhase {
        TodoPhase {
            name: name.to_owned(),
            tasks: vec![crate::TodoItem {
                id: format!("task-{name}"),
                content: name.to_owned(),
                status: crate::TodoStatus::Pending,
                depends_on: depends_on.iter().map(|dep| format!("task-{dep}")).collect(),
                ready: true,
                waiting_on: Vec::new(),
                agent: None,
            }],
        }
    }

    fn recorder_with(cwd: &Path, id: &str, texts: &[&str]) -> crate::SessionRecorder {
        let recorder = crate::start_session_in(cwd, None, None, Some(cwd), Some(id), None)
            .expect("start recorder");
        for text in texts {
            recorder
                .record_message(&Message::user_text(*text, 0))
                .expect("record message");
        }
        recorder.persist_now().expect("persist recorder");
        recorder
    }

    #[tokio::test]
    async fn rewind_truncates_archives_rebuilds_transcript_and_resets_chain() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session_id = "rewind-chain-test".to_owned();
        let session = test_session_with_discovery(SessionOptions {
            model: Model::default(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: SimpleStreamOptions {
                stream: pi_ai::StreamOptions {
                    session_id: Some(session_id.clone()),
                    ..Default::default()
                },
                ..Default::default()
            },
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        });
        let _application = publish_test_application(&session, cwd.path()).await;

        let recorder = recorder_with(
            cwd.path(),
            "rewind-session",
            &["one", "two", "three", "four"],
        );
        let ids = recorder
            .tree()
            .expect("tree")
            .entries
            .iter()
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>();
        session.record(recorder).expect("attach recorder");

        // Seed the Responses chain for this session; rewind must clear it.
        pi_ai::providers::responses_chain_note_success(&session_id, "resp_test");
        assert_eq!(
            pi_ai::providers::responses_chain_previous_id(&session_id).as_deref(),
            Some("resp_test")
        );

        let outcome = session
            .rewind(RewindTarget::Index(2))
            .await
            .expect("rewind");
        assert_eq!(outcome.retained_entries, 2);
        assert_eq!(outcome.dropped_entries, 2);
        assert!(outcome.checkpoint.is_none());
        assert!(outcome.archive_path.exists(), "archive sidecar must exist");

        // In-memory transcript rebuilt from the retained journal.
        let history = session.history();
        assert_eq!(history.len(), 2);
        assert_eq!(user_text(&history[0]), "one");
        assert_eq!(user_text(&history[1]), "two");

        // Chain reset: the stored previous response id is gone.
        assert_eq!(
            pi_ai::providers::responses_chain_previous_id(&session_id),
            None
        );

        // Session file truncated; recorder leaf is the last kept entry.
        let (_, session_path) = session.recorder_info().expect("recorder info");
        let tree = crate::load_session_tree(&session_path).expect("load truncated tree");
        assert_eq!(tree.entries.len(), 2);
        assert_eq!(tree.entries[0].id, ids[0]);
        assert_eq!(tree.entries[1].id, ids[1]);
        assert_eq!(tree.leaf_id.as_deref(), Some(ids[1].as_str()));
    }

    #[tokio::test]
    async fn session_drop_resets_the_responses_chain() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session_id = "chain-drop-test".to_owned();
        let session = test_session_with_discovery(SessionOptions {
            model: Model::default(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: SimpleStreamOptions {
                stream: pi_ai::StreamOptions {
                    session_id: Some(session_id.clone()),
                    ..Default::default()
                },
                ..Default::default()
            },
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        });

        // Seed a Responses chain for this session.
        pi_ai::providers::responses_chain_note_success(&session_id, "resp_turn_one");
        assert_eq!(
            pi_ai::providers::responses_chain_previous_id(&session_id).as_deref(),
            Some("resp_turn_one")
        );

        // The last Session clone dropping must synchronously remove the
        // process-global chain entry: the id can be reused (resumed) without
        // chaining from a stale response.
        drop(session);
        assert_eq!(
            pi_ai::providers::responses_chain_previous_id(&session_id),
            None
        );
    }

    #[tokio::test]
    async fn rewind_restores_todo_and_goal_from_journal_up_to_cut() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_session(cwd.path());
        let _application = publish_test_application(&session, cwd.path()).await;
        let recorder = crate::start_session_in(
            cwd.path(),
            None,
            None,
            Some(cwd.path()),
            Some("rewind-goal"),
            None,
        )
        .expect("start recorder");
        recorder
            .record_message(&Message::user_text("one", 0))
            .expect("record one");
        recorder
            .record_todo_snapshot(&TodoState {
                phases: vec![phase("A", &[])],
                storage: TodoStorage::Session,
            })
            .expect("snapshot A");
        recorder
            .record_message(&Message::user_text("two", 0))
            .expect("record two");
        recorder
            .record_message(&Message::user_text("three", 0))
            .expect("record three");
        recorder.persist_now().expect("persist");
        session.record(recorder).expect("attach recorder");

        // Journal layout: 0=one, 1=todo A, 2=two, 3=three, 4=goal, 5=todo A+B.
        session
            .goal_runtime()
            .create("ship rewind", None)
            .expect("create goal");
        session
            .set_todos(vec![phase("A+B", &[])])
            .expect("set todos after goal");

        // Cut between the goal and the second todo snapshot: the later todo is
        // dropped but the goal journal survives the cut intact.
        let outcome = session
            .rewind(RewindTarget::Index(5))
            .await
            .expect("rewind to keep goal");
        assert_eq!(outcome.retained_entries, 5);
        assert_eq!(session.todo_state().phases.len(), 1);
        assert_eq!(session.todo_state().phases[0].name, "A");
        let goal = session.goal_runtime().get().current.expect("goal survives");
        assert_eq!(goal.objective, "ship rewind");

        // Cut through the goal journal: the goal is re-derived away.
        session
            .rewind(RewindTarget::Index(4))
            .await
            .expect("rewind through goal journal");
        assert!(session.goal_runtime().get().current.is_none());
        assert_eq!(session.todo_state().phases.len(), 1);
        assert_eq!(session.todo_state().phases[0].name, "A");

        // Cut past every todo snapshot: the todo list falls back to empty.
        session
            .rewind(RewindTarget::Index(1))
            .await
            .expect("rewind past todo snapshots");
        assert!(session.todo_state().phases.is_empty());
        let history = session.history();
        assert_eq!(history.len(), 1);
        assert_eq!(user_text(&history[0]), "one");
    }

    #[tokio::test]
    async fn rewind_refuses_past_first_entry_unknown_checkpoint_and_beyond_end() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_session(cwd.path());
        let _application = publish_test_application(&session, cwd.path()).await;
        let recorder = recorder_with(cwd.path(), "rewind-refuse", &["one"]);
        session.record(recorder).expect("attach recorder");

        let error = session
            .rewind(RewindTarget::Index(0))
            .await
            .expect_err("rewinding past the first entry must be refused");
        assert!(format!("{error:#}").contains("past the first entry"));

        let error = session
            .rewind(RewindTarget::Index(7))
            .await
            .expect_err("rewinding beyond the end must be refused");
        assert!(format!("{error:#}").contains("nothing to rewind"));

        let error = session
            .rewind(RewindTarget::Checkpoint("missing".to_owned()))
            .await
            .expect_err("unknown checkpoint must be refused");
        assert!(format!("{error:#}").contains("not found"));
    }

    #[tokio::test]
    async fn checkpoint_marks_position_and_rewind_targets_it() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_session(cwd.path());
        let _application = publish_test_application(&session, cwd.path()).await;
        let recorder = recorder_with(cwd.path(), "rewind-checkpoint", &["one", "two"]);
        session.record(recorder.clone()).expect("attach recorder");

        session.set_checkpoint("mid").expect("mark checkpoint");
        recorder
            .record_message(&Message::user_text("three", 0))
            .expect("record three");
        recorder
            .record_message(&Message::user_text("four", 0))
            .expect("record four");
        recorder.persist_now().expect("persist");

        let outcome = session
            .rewind(RewindTarget::Checkpoint("mid".to_owned()))
            .await
            .expect("rewind to checkpoint");
        assert_eq!(outcome.checkpoint.as_deref(), Some("mid"));
        assert_eq!(outcome.retained_entries, 2);
        // The dropped tail is the marker itself plus the two later messages.
        assert_eq!(outcome.dropped_entries, 3);
        let history = session.history();
        assert_eq!(history.len(), 2);
        assert_eq!(user_text(&history[0]), "one");
        assert_eq!(user_text(&history[1]), "two");
    }

    #[tokio::test]
    async fn rewind_preview_lists_indices_and_checkpoints() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_session(cwd.path());
        let _application = publish_test_application(&session, cwd.path()).await;
        let recorder = recorder_with(
            cwd.path(),
            "rewind-preview",
            &["one", "two", "three", "four"],
        );
        session.record(recorder).expect("attach recorder");
        session.set_checkpoint("mid").expect("mark checkpoint");

        let previews = session.rewind_preview(20).expect("preview");
        assert_eq!(previews.len(), 5);
        assert_eq!(previews[0].index, 0);
        assert_eq!(previews[0].entry_type, "message");
        assert_eq!(previews[0].preview.as_deref(), Some("one"));
        let last = previews.last().expect("last preview");
        assert_eq!(last.entry_type, "checkpoint");
        assert_eq!(last.checkpoint_name.as_deref(), Some("mid"));
        assert!(
            last.preview.is_none(),
            "checkpoint markers have no text preview"
        );

        // A limit keeps the most recent records only.
        let limited = session.rewind_preview(2).expect("limited preview");
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[0].index, 3);
        assert_eq!(limited[0].preview.as_deref(), Some("four"));
        assert_eq!(limited[1].entry_type, "checkpoint");
    }
}

#[cfg(test)]
mod queued_message_consumption_tests {
    use super::*;
    use std::collections::VecDeque;

    /// Scripted provider: pops one assistant message per stream invocation, so
    /// the loop's turn count is deterministic.
    fn scripted(
        messages: Vec<pi_ai::AssistantMessage>,
    ) -> (pi_agent::StreamFn, Arc<Mutex<Vec<String>>>) {
        let messages = Arc::new(Mutex::new(VecDeque::from(messages)));
        let model_ids = Arc::new(Mutex::new(Vec::new()));
        let captured_model_ids = model_ids.clone();
        let stream: pi_agent::StreamFn = Arc::new(
            move |model: Model, _context: Context, _options: SimpleStreamOptions| {
                let messages = messages.clone();
                captured_model_ids.lock().push(model.id.clone());
                Box::pin(async move {
                    let message = messages.lock().pop_front().unwrap_or_else(|| {
                        let mut fallback = pi_ai::AssistantMessage::pending(&model);
                        fallback.content = vec![ContentBlock::text("done")];
                        fallback.stop_reason = StopReason::Stop;
                        fallback
                    });
                    let stream = pi_ai::new_assistant_message_event_stream();
                    let producer = stream.clone();
                    let model = model.clone();
                    tokio::spawn(async move {
                        producer
                            .push(pi_ai::AssistantMessageEvent::Start {
                                partial: pi_ai::AssistantMessage::pending(&model),
                            })
                            .await;
                        let terminal = if matches!(
                            message.stop_reason,
                            StopReason::Error | StopReason::Aborted
                        ) {
                            pi_ai::AssistantMessageEvent::Error {
                                reason: message.stop_reason,
                                error: message.clone(),
                            }
                        } else {
                            pi_ai::AssistantMessageEvent::Done {
                                reason: message.stop_reason,
                                message: message.clone(),
                            }
                        };
                        producer.push(terminal).await;
                        producer.end(Some(message)).await;
                    });
                    stream
                })
            },
        );
        (stream, model_ids)
    }

    /// Regression: a queued steering message must leave the pending queue the
    /// moment the running turn consumes it — the count/preview cannot linger
    /// until the whole turn settles. The boundary drain runs between
    /// `TurnEnd` and the next `TurnStart`, so the `TurnStart` publish is the
    /// exact post-drain projection: each consumed message drops from the
    /// published count while the turn is still in flight (never resurrecting
    /// for a stale boundary), and `finish_run` republishes the final empty
    /// state on settle.
    #[tokio::test]
    async fn steering_consumption_publishes_queue_update_at_turn_boundaries() {
        let cwd = tempfile::tempdir().expect("cwd");
        let gate_started = Arc::new(Notify::new());
        let gate_release = Arc::new(Notify::new());
        let tool_started = gate_started.clone();
        let tool_release = gate_release.clone();
        // Turn 1 blocks on the gate tool until the test has queued its
        // steering, making the steer deterministically land between the run's
        // initial drain and the first turn boundary.
        let gate = AgentTool::new("gate", "gate", pi_ai::Schema::default(), move |_context| {
            let started = tool_started.clone();
            let release = tool_release.clone();
            async move {
                started.notify_waiters();
                release.notified().await;
                Ok(pi_agent::AgentToolResult::text("gated"))
            }
        });
        let (stream, _) = scripted(vec![
            assistant(
                vec![ContentBlock::ToolCall(ToolCall {
                    id: "c1".to_owned(),
                    name: "gate".to_owned(),
                    arguments: serde_json::json!({}),
                    thought_signature: None,
                })],
                StopReason::ToolUse,
            ),
            assistant(vec![ContentBlock::text("two")], StopReason::Stop),
            assistant(vec![ContentBlock::text("three")], StopReason::Stop),
        ]);
        let session = test_session_with_discovery(SessionOptions {
            model: Model::default(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(vec![gate]),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(stream),
            auth_resolver: None,
            auth_manager: None,
        });
        let _application = publish_test_application(&session, cwd.path()).await;
        let mut events = session.subscribe_session_events();
        let mut run = tokio::spawn({
            let session = session.clone();
            async move { session.run("first", Vec::new()).await }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), gate_started.notified())
            .await
            .expect("gate tool must start");
        session
            .steer(Message::user_text("steer one", 1))
            .await
            .expect("queue first steer");
        session
            .steer(Message::user_text("steer two", 2))
            .await
            .expect("queue second steer");
        gate_release.notify_waiters();

        // Collect every QueueUpdate while the turn is in flight, then drain
        // whatever the channel still holds after the run resolves.
        let mut counts = Vec::new();
        let mut run_result = None;
        loop {
            tokio::select! {
                biased;
                event = events.recv() => match event {
                    Ok(SessionEvent::QueueUpdate { steering, follow_up }) => {
                        counts.push((steering.len(), follow_up.len()));
                    }
                    Ok(_) => {}
                    Err(_) => break,
                },
                result = &mut run => {
                    run_result = Some(result);
                    break;
                }
            }
        }
        while let Ok(SessionEvent::QueueUpdate {
            steering,
            follow_up,
        }) = events.try_recv()
        {
            counts.push((steering.len(), follow_up.len()));
        }
        let result = run_result.expect("run resolves");
        result.expect("run succeeds");

        // TurnStart1 publishes the post-initial-drain 0; steer publishes 1
        // then 2; the first `TurnEnd` publishes the accurate pre-drain 2;
        // `TurnStart2`/`TurnStart3` publish the exact post-drain counts
        // (1, then 0 — the row leaves the tray the moment the loop hands it
        // to the running turn, never resurrecting for a boundary); the
        // trailing `TurnEnd`s publish the same count, and finish_run
        // republishes 0 on settle.
        assert_eq!(
            counts,
            vec![
                (0, 0),
                (1, 0),
                (2, 0),
                (2, 0),
                (1, 0),
                (1, 0),
                (0, 0),
                (0, 0),
                (0, 0)
            ],
            "queue counts must drop as each steering message is consumed, not only at settle"
        );
        let (steering, follow_up) = session.queued_messages().await;
        assert!(
            steering.is_empty() && follow_up.is_empty(),
            "queue must be fully drained after the run"
        );
    }

    fn steering_user_text(message: &Message) -> String {
        match message {
            Message::User(user) => content_text(&user.content),
            _ => String::new(),
        }
    }

    /// D3/F2: an acked steer must leave a durable journal marker. A crash
    /// after the ack (the marker journaled, the message never delivered)
    /// requeues the message on resume instead of losing it.
    #[tokio::test]
    async fn crash_after_acked_steering_requeues_on_resume() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let (stream, _log) = scripted(vec![assistant(
            vec![ContentBlock::text("one")],
            StopReason::Stop,
        )]);
        let session = test_session_with_discovery(SessionOptions {
            model: Model::default(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: None,
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(stream),
            auth_resolver: None,
            auth_manager: None,
        });
        session.set_session_dir(sessions.path().to_path_buf());
        session.start_new_recording().expect("start recording");
        session.run("first", Vec::new()).await.expect("first run");

        // The steer is acked; the flush window has passed (durable persist).
        session
            .steer(Message::user_text("fix it", 1))
            .await
            .expect("steer acked");
        session
            .persist_recording_now_durable()
            .expect("persist marker");
        let (_, path) = session.recorder_info().expect("recorder info");
        drop(session); // kill -9: the process dies with the steering undelivered

        // The journal carries an undelivered marker for the acked message.
        let tree = crate::load_session_tree(&path).expect("reload journal");
        let marker = tree
            .entries
            .iter()
            .find(|entry| {
                entry.entry_type == "custom"
                    && entry.custom_type.as_deref() == Some(crate::STEERING_PENDING_CUSTOM_TYPE)
            })
            .expect("acked steer must be journaled as an undelivered marker");
        assert!(
            marker
                .data
                .as_ref()
                .and_then(|data| data.get("message"))
                .is_some(),
            "the marker must embed the queued message"
        );

        // Resume requeues the still-undelivered marker into the steering queue.
        let (stream, _log) = scripted(vec![assistant(
            vec![ContentBlock::text("two")],
            StopReason::Stop,
        )]);
        let resumed = test_session_with_discovery(SessionOptions {
            model: Model::default(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: None,
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(stream),
            auth_resolver: None,
            auth_manager: None,
        });
        let prepared = crate::PreparedSessionResume::prepare_path(&path).expect("prepare resume");
        let context = prepared.build_context();
        resumed
            .resume_recording(
                prepared.into_recorder().expect("recorder"),
                context.messages,
            )
            .await
            .expect("resume");
        let (steering, follow_up) = resumed.queued_messages().await;
        assert_eq!(steering.len(), 1, "the acked steer must be requeued");
        assert_eq!(steering_user_text(&steering[0]), "fix it");
        assert!(follow_up.is_empty());
        // The message was requeued, NOT delivered: it is not in the transcript.
        assert!(
            resumed
                .history()
                .iter()
                .all(|message| steering_user_text(message) != "fix it")
        );
        // The stale marker is retired with a `requeued` record so the next
        // resume never requeues it a second time.
        let (_, resumed_path) = resumed.recorder_info().expect("resumed recorder info");
        let tree = crate::load_session_tree(&resumed_path).expect("reload resumed journal");
        assert!(
            tree.entries.iter().any(|entry| {
                entry.entry_type == "custom"
                    && entry.custom_type.as_deref() == Some(crate::STEERING_DELIVERED_CUSTOM_TYPE)
                    && entry
                        .data
                        .as_ref()
                        .and_then(|data| data.get("reason"))
                        .and_then(serde_json::Value::as_str)
                        == Some("requeued")
            }),
            "the stale marker must be retired with reason=requeued"
        );
    }

    /// A fresh session replacement drops acknowledged steering. The old
    /// journal retires the marker so a later resume cannot resurrect it.
    #[tokio::test]
    async fn install_fresh_session_replacement_drops_queued_steering() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let session = test_session_with_discovery(SessionOptions {
            model: Model::default(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: None,
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        });
        session.set_session_dir(sessions.path().to_path_buf());
        session.start_new_recording().expect("start recording");
        session
            .steer(Message::user_text("fix it", 1))
            .await
            .expect("steer acked");
        let (_, old_path) = session.recorder_info().expect("old recorder info");

        let replacement = session
            .prepare_new_session_replacement(None)
            .expect("prepare new replacement");
        session
            .commit_session_replacement(replacement)
            .await
            .expect("commit replacement");

        let (steering, follow_up) = session.queued_messages().await;
        assert!(steering.is_empty(), "fresh session must drop steering");
        assert!(follow_up.is_empty(), "fresh session must drop follow-up");
        let old_tree = crate::load_session_tree(&old_path).expect("reload old journal");
        assert!(
            old_tree.entries.iter().any(|entry| {
                entry.entry_type == "custom"
                    && entry.custom_type.as_deref() == Some(crate::STEERING_DELIVERED_CUSTOM_TYPE)
                    && entry
                        .data
                        .as_ref()
                        .and_then(|data| data.get("reason"))
                        .and_then(serde_json::Value::as_str)
                        == Some("replaced")
            }),
            "the old marker must be retired with reason=replaced"
        );
        let (_, path) = session.recorder_info().expect("replacement recorder info");
        let tree = crate::load_session_tree(&path).expect("reload replacement journal");
        assert!(
            tree.entries.iter().all(|entry| {
                entry.custom_type.as_deref() != Some(crate::STEERING_PENDING_CUSTOM_TYPE)
            }),
            "the fresh journal must not receive the dropped steer"
        );
    }

    #[tokio::test]
    async fn queued_steering_left_after_settle_is_delivered_by_continuation() {
        // Regression for the confirmed stale-projection bug: a steering
        // message that arrives after the agent loop's final-window drain is
        // left in the queue when the run settles, with no active run left to
        // consume it. The TUI then keeps showing `⟦steering⟧ <preview>` and
        // `⚙ N` even though the text already reached the transcript. The
        // settle path must start a continuation run so the message is
        // delivered and the final `QueueUpdate` is empty.
        let cwd = tempfile::tempdir().expect("cwd");
        let (stream, _) = scripted(vec![
            assistant(vec![ContentBlock::text("first")], StopReason::Stop),
            assistant(vec![ContentBlock::text("after steer")], StopReason::Stop),
        ]);
        let session = test_session_with_discovery(SessionOptions {
            model: Model::default(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: None,
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(stream),
            auth_resolver: None,
            auth_manager: None,
        });
        let _application = publish_test_application(&session, cwd.path()).await;
        let mut events = session.subscribe_session_events();

        session
            .run_messages(vec![Message::user_text("go", 0)])
            .await
            .expect("first run settles");
        // Stranded-state seam: exactly what the settle race leaves behind — a
        // queued steering message with no active run to consume it. The next
        // run entry must drain it instead of letting the projection linger.
        session
            .steer(Message::user_text("steer late", 3))
            .await
            .expect("enqueue");
        assert_eq!(session.pending_message_count().await, 1);

        session
            .continue_run()
            .await
            .expect("continuation delivers queued steer");

        assert_eq!(
            session.pending_message_count().await,
            0,
            "queue must empty after the continuation"
        );
        let (steering, follow_up) = session.queued_messages().await;
        assert!(
            steering.is_empty() && follow_up.is_empty(),
            "queues must be fully drained after the continuation"
        );
        // The projection must converge on an empty QueueUpdate (the TUI clears
        // `⟦steering⟧` / `⚙ N` from it), not linger on the stale preview.
        let mut converged = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !converged && std::time::Instant::now() < deadline {
            let remaining = std::time::Duration::from_secs(5)
                .saturating_sub(std::time::Instant::now().elapsed());
            match tokio::time::timeout(remaining, events.recv()).await {
                Ok(Ok(SessionEvent::QueueUpdate {
                    steering,
                    follow_up,
                })) => {
                    converged = steering.is_empty() && follow_up.is_empty();
                }
                Ok(Ok(_)) => {}
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }
        assert!(
            converged,
            "final QueueUpdate must be empty for the projection to clear"
        );
    }

    /// S-RACE-4 regression: the run slot is released only after the agent
    /// goes idle, so a settle-drain continuation that starts immediately can
    /// lose `claim_exclusive`. The drain must retry with bounded backoff and
    /// deliver the queued steering once the slot frees — never strand it
    /// until the next prompt.
    #[tokio::test]
    async fn settle_drain_retries_claim_contention_and_delivers() {
        let cwd = tempfile::tempdir().expect("cwd");
        let (stream, _) = scripted(vec![
            assistant(vec![ContentBlock::text("first")], StopReason::Stop),
            assistant(vec![ContentBlock::text("after steer")], StopReason::Stop),
        ]);
        let session = test_session_with_discovery(SessionOptions {
            model: Model::default(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: None,
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(stream),
            auth_resolver: None,
            auth_manager: None,
        });
        let _application = publish_test_application(&session, cwd.path()).await;
        session
            .run_messages(vec![Message::user_text("go", 0)])
            .await
            .expect("first run settles");
        session
            .steer(Message::user_text("steer late", 3))
            .await
            .expect("enqueue post-settle steer");
        assert_eq!(session.pending_message_count().await, 1);

        // Hold the run slot exactly the way an in-flight release does, then
        // free it while the drain is inside its backoff window.
        session.inner.run_slot.lock().active = true;
        let release_slot = tokio::spawn({
            let session = session.clone();
            async move {
                tokio::time::sleep(std::time::Duration::from_millis(120)).await;
                session.inner.run_slot.lock().active = false;
            }
        });
        session.deliver_queued_after_settle().await;
        release_slot.await.expect("release task");

        assert_eq!(
            session.pending_message_count().await,
            0,
            "the contention-lost continuation must be retried and deliver the steer"
        );
        assert!(
            session
                .history()
                .iter()
                .any(|message| content_text_of(message) == "steer late"),
            "the queued steer must be delivered into the transcript"
        );
    }

    fn content_text_of(message: &Message) -> String {
        match message {
            Message::User(user) => content_text(&user.content),
            _ => String::new(),
        }
    }

    #[tokio::test]
    async fn model_switch_during_active_turn_applies_to_next_turn_only() {
        let cwd = tempfile::tempdir().expect("cwd");
        let initial_model = Model {
            id: "initial-model".to_owned(),
            provider: "test-provider".to_owned(),
            ..Model::default()
        };
        let next_model = Model {
            id: "next-model".to_owned(),
            provider: "test-provider".to_owned(),
            ..Model::default()
        };
        let gate_started = Arc::new(Notify::new());
        let gate_release = Arc::new(Notify::new());
        let tool_started = gate_started.clone();
        let tool_release = gate_release.clone();
        let gate = AgentTool::new("gate", "gate", pi_ai::Schema::default(), move |_context| {
            let started = tool_started.clone();
            let release = tool_release.clone();
            async move {
                started.notify_one();
                release.notified().await;
                Ok(pi_agent::AgentToolResult::text("gated"))
            }
        });
        let (stream, model_ids) = scripted(vec![
            assistant(
                vec![ContentBlock::ToolCall(ToolCall {
                    id: "model-gate".to_owned(),
                    name: "gate".to_owned(),
                    arguments: serde_json::json!({}),
                    thought_signature: None,
                })],
                StopReason::ToolUse,
            ),
            assistant(
                vec![ContentBlock::text("first turn done")],
                StopReason::Stop,
            ),
            assistant(
                vec![ContentBlock::text("second turn done")],
                StopReason::Stop,
            ),
        ]);
        let session = test_session_with_discovery(SessionOptions {
            model: initial_model,
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(vec![gate]),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(stream),
            auth_resolver: None,
            auth_manager: None,
        });
        let _application = publish_test_application(&session, cwd.path()).await;
        let mut events = session.subscribe_session_events();
        let first_run = tokio::spawn({
            let session = session.clone();
            async move { session.run("first", Vec::new()).await }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), gate_started.notified())
            .await
            .expect("gate tool must start");

        session.set_model(next_model, String::new());
        assert_eq!(
            session.model().map(|model| model.id),
            Some("next-model".to_owned()),
            "session state must accept the next-turn model while the current turn runs"
        );
        let selected = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let SessionEvent::ModelSelect { model } =
                    events.recv().await.expect("session event")
                {
                    break model;
                }
            }
        })
        .await
        .expect("model selection event must publish");
        assert_eq!(selected.id, "next-model");

        gate_release.notify_one();
        first_run.await.expect("first run task").expect("first run");
        session.run("second", Vec::new()).await.expect("second run");

        assert_eq!(
            *model_ids.lock(),
            ["initial-model", "initial-model", "next-model"],
            "all provider calls in the active turn must retain its captured model; only the next turn may switch"
        );
    }

    fn assistant(content: Vec<ContentBlock>, stop_reason: StopReason) -> pi_ai::AssistantMessage {
        let mut message = pi_ai::AssistantMessage::pending(&Model::default());
        message.content = content;
        message.stop_reason = stop_reason;
        message
    }
}

/// Session-to-session todo isolation. Todos are per-session state, held in the
/// session's own [`TodoRuntime`] and persisted to the session's own JSONL
/// `todo_snapshot` records — there is no global or project-level todo store.
/// A fresh recording starts empty, and a fork copies the todo at the fork
/// point into an independent file whose later mutations never touch the
/// source session.
#[cfg(test)]
mod todo_session_isolation_tests {
    use super::*;

    fn test_session(cwd: &Path, sessions: &Path) -> Session {
        let session = test_session_with_discovery(SessionOptions {
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
            auth_manager: None,
        });
        session.set_session_dir(sessions.to_path_buf());
        session
    }

    fn phase(name: &str, task_id: &str, content: &str) -> TodoPhase {
        TodoPhase {
            name: name.to_owned(),
            tasks: vec![crate::TodoItem {
                id: task_id.to_owned(),
                content: content.to_owned(),
                status: crate::TodoStatus::Pending,
                depends_on: Vec::new(),
                ready: true,
                waiting_on: Vec::new(),
                agent: None,
            }],
        }
    }

    #[test]
    fn new_recording_starts_empty_even_after_prior_session_had_todos() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let session = test_session(cwd.path(), sessions.path());

        // A brand-new session has no recorder and an empty in-memory todo.
        assert_eq!(
            session.todo_state(),
            TodoState {
                phases: Vec::new(),
                storage: TodoStorage::Memory
            }
        );

        // Attaching a fresh (never-written) recorder keeps the todo empty; the
        // storage tag flips to Session because a recorder now exists.
        let recorder = crate::start_session_in(
            cwd.path(),
            None,
            Some("off"),
            Some(sessions.path()),
            Some("fresh-empty"),
            None,
        )
        .expect("start recorder");
        session.record(recorder).expect("attach fresh recorder");
        assert_eq!(
            session.todo_state(),
            TodoState {
                phases: Vec::new(),
                storage: TodoStorage::Session
            }
        );

        // Session A now has its own todos, persisted into its own file.
        session
            .set_todos(vec![phase("A", "task-a", "session A task")])
            .expect("set todos on session A");
        assert_eq!(session.todo_state().phases[0].tasks[0].id, "task-a");

        // A new recording must start empty: no shared todo store carries
        // session A's tasks over to the next session.
        session.start_new_recording().expect("start new recording");
        assert_eq!(
            session.todo_state(),
            TodoState {
                phases: Vec::new(),
                storage: TodoStorage::Session
            }
        );
    }

    #[tokio::test]
    async fn fork_copies_todo_at_fork_point_and_mutations_diverge() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let session = test_session(cwd.path(), sessions.path());
        let recorder = crate::start_session_in(
            cwd.path(),
            None,
            Some("off"),
            Some(sessions.path()),
            Some("fork-todo-source"),
            None,
        )
        .expect("start recorder");
        session.record(recorder.clone()).expect("attach recorder");

        // Journal: m1 (message) -> t1 (todo snapshot task-a) -> m2 (message).
        let first = recorder
            .record_message(&Message::user_text("first", 0))
            .expect("record first");
        assert_eq!(first, recorder.last_entry_id().expect("leaf after first"));
        session
            .set_todos(vec![phase("Build", "task-a", "source task")])
            .expect("set source todos");
        let second = recorder
            .record_message(&Message::user_text("second", 1))
            .expect("record second");
        recorder.persist_now().expect("persist source");
        let source_path = recorder.path();
        assert!(source_path.is_file());

        // Fork at "second": the fork copies the branch up to its parent (the
        // todo snapshot), so the fork's todo equals the source's at the fork
        // point — and the fork lives in its own file.
        session.fork_session(&second, true).await.expect("fork");
        let (_, fork_path) = session.recorder_info().expect("fork recorder info");
        assert_ne!(fork_path, source_path, "fork must be a new session file");
        assert_eq!(
            session.todo_state().phases[0].tasks[0].id,
            "task-a",
            "fork must copy the source todo at the fork point"
        );

        // The fork is an independent copy: mutating the fork's todo must
        // never rewrite the source session's journal.
        session
            .set_todos(vec![phase("Fork", "task-fork-only", "fork task")])
            .expect("set fork todos");
        let fork_journal = session.session_entries(None).expect("fork journal");
        assert_eq!(
            fork_journal
                .entries
                .iter()
                .rev()
                .find(|entry| entry.entry_type == "todo_snapshot")
                .and_then(|entry| entry.todo_state.clone())
                .expect("fork todo snapshot")
                .phases[0]
                .tasks[0]
                .id,
            "task-fork-only",
            "fork journal must carry the fork's own snapshot"
        );
        assert_eq!(
            crate::load_session_tree(&source_path)
                .expect("reload source")
                .latest_todo_state()
                .expect("source snapshot")
                .phases[0]
                .tasks[0]
                .id,
            "task-a",
            "source session must keep the fork-point todo"
        );
    }
}

// ---------------------------------------------------------------------------
// MCP cross-session isolation (P1: same-CWD cutover must reap old clients)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod mcp_session_reset_tests {
    use super::*;

    use crate::settings::{McpServerConfig, McpTransport};

    fn mcp_test_session(cwd: &Path, session_dir: &Path) -> Session {
        let session = test_session_with_discovery(SessionOptions {
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
            auth_manager: None,
        });
        session.set_session_dir(session_dir.to_path_buf());
        session
    }

    /// A recorder with one recorded user message (fork/clone prepare against
    /// the recorded tree).
    fn seeded_recorder(cwd: &Path, dir: &Path, id: &str) -> crate::SessionRecorder {
        let recorder = crate::start_session_in(cwd, None, None, Some(dir), Some(id), None)
            .expect("start recorder");
        recorder
            .record_message(&Message::user_text("seed message", 0))
            .expect("record message");
        recorder.persist_now().expect("persist recorder");
        recorder
    }

    /// Configures the session-scoped registry with the stateful fake server:
    /// every spawn appends its pid to `pid_file`.
    fn configure_fake_mcp(session: &Session, pid_file: &Path) {
        let exe = crate::mcp::fake_server_exe();
        session.inner.mcp.configure(vec![McpServerConfig {
            name: "fake".to_owned(),
            disabled: false,
            transport: McpTransport::Stdio,
            command: Some(exe.to_string_lossy().into_owned()),
            args: Some(vec![
                "mcp::tests::fake_mcp_server_process".to_owned(),
                "--nocapture".to_owned(),
            ]),
            url: None,
            env: Some(crate::McpEnvironmentWire::from_iter([
                ("PI_FAKE_MCP_SERVER".to_owned(), "1".to_owned()),
                (
                    "PI_FAKE_MCP_PID_FILE".to_owned(),
                    pid_file.to_string_lossy().into_owned(),
                ),
            ])),
            credential_grants: Vec::new(),
        }]);
    }

    /// Runs one `mcp call echo` against the session's live registry and
    /// returns the rendered text.
    async fn mcp_echo(session: &Session, message: &str) -> String {
        let result = crate::mcp::run_mcp(
            &session.inner.mcp,
            serde_json::json!({
                "action": "call",
                "server": "fake",
                "tool": "echo",
                "args": { "message": message },
            }),
            AbortSignal::none(),
        )
        .await
        .expect("mcp echo call succeeds");
        result_text(&result)
    }

    /// Runs `mcp list_servers` against the session's live registry.
    async fn mcp_list_servers(session: &Session) -> String {
        let result = crate::mcp::run_mcp(
            &session.inner.mcp,
            serde_json::json!({ "action": "list_servers" }),
            AbortSignal::none(),
        )
        .await
        .expect("mcp list_servers succeeds");
        result_text(&result)
    }

    /// Concatenates the text blocks of a tool result.
    fn result_text(result: &pi_agent::AgentToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|block| match block {
                pi_ai::ContentBlock::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// Pids recorded by the fake server (one line per spawn).
    fn read_pid_lines(path: &Path) -> Vec<u32> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.trim().parse().ok())
            .collect()
    }

    /// True when a process with `pid` exists (Linux procfs).
    fn process_alive(pid: u32) -> bool {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }

    /// The four logical session replacements that route through the shared
    /// same-CWD cutover (`commit_session_replacement`).
    #[derive(Clone, Copy, Debug)]
    enum ReplacementKind {
        New,
        Resume,
        Fork,
        Clone,
    }

    const ALL_REPLACEMENT_KINDS: [ReplacementKind; 4] = [
        ReplacementKind::New,
        ReplacementKind::Resume,
        ReplacementKind::Fork,
        ReplacementKind::Clone,
    ];

    async fn prepare_and_commit(session: &Session, kind: ReplacementKind) {
        let replacement = match kind {
            ReplacementKind::New => session
                .prepare_new_session_replacement(None)
                .expect("prepare new replacement"),
            ReplacementKind::Resume => {
                let path = session.current_recorder().expect("recorder").path();
                let prepared =
                    crate::PreparedSessionResume::prepare_path(&path).expect("prepare resume path");
                session
                    .prepare_resume_replacement(prepared)
                    .await
                    .expect("prepare resume replacement")
            }
            ReplacementKind::Fork => {
                let recorder = session.current_recorder().expect("recorder");
                let tree = recorder.tree().expect("recorder tree");
                let entry_id = tree
                    .entries
                    .iter()
                    .find(|entry| matches!(entry.message, Some(Message::User(_))))
                    .map(|entry| entry.id.clone())
                    .expect("a user entry exists after seeding");
                let (replacement, _) = session
                    .prepare_fork_replacement(&entry_id, false)
                    .await
                    .expect("prepare fork replacement");
                replacement
            }
            ReplacementKind::Clone => {
                let leaf_id = session
                    .current_recorder()
                    .expect("recorder")
                    .active_leaf_id()
                    .expect("active leaf after seeding");
                session
                    .prepare_clone_replacement(&leaf_id, false)
                    .expect("prepare clone replacement")
            }
        };
        session
            .commit_session_replacement(replacement)
            .await
            .expect("commit replacement");
    }

    /// Regression (P1): every logical same-CWD session replacement must reap
    /// the outgoing session's live MCP client before the cutover returns, the
    /// new logical session must lazily spawn a fresh client process, and the
    /// server configuration must survive.
    #[tokio::test]
    async fn same_cwd_cutover_reaps_mcp_client_for_every_replacement_kind() {
        for kind in ALL_REPLACEMENT_KINDS {
            let cwd = tempfile::tempdir().expect("cwd");
            let sessions = tempfile::tempdir().expect("sessions");
            let trace_dir = tempfile::tempdir().expect("trace dir");
            let pid_file = trace_dir.path().join("pids.txt");
            let session = mcp_test_session(cwd.path(), sessions.path());
            let _application = publish_test_application(&session, trace_dir.path()).await;
            let recorder = seeded_recorder(cwd.path(), sessions.path(), "seed");
            session.record(recorder).expect("attach seeded recorder");
            configure_fake_mcp(&session, &pid_file);

            // First mcp call spawns a live client; capture its pid.
            let text = mcp_echo(&session, "before").await;
            assert!(text.contains("before"), "kind={kind:?}: {text}");
            let pids = read_pid_lines(&pid_file);
            assert_eq!(pids.len(), 1, "kind={kind:?}: one spawn so far: {pids:?}");
            let old_pid = pids[0];
            assert!(
                process_alive(old_pid),
                "kind={kind:?}: client must be running before the cutover"
            );

            // The same-CWD cutover must reap the old client before returning.
            prepare_and_commit(&session, kind).await;
            assert!(
                !process_alive(old_pid),
                "kind={kind:?}: old MCP client must be dead before the cutover returns"
            );

            // The new logical session lazily spawns a fresh client...
            let text = mcp_echo(&session, "after").await;
            assert!(text.contains("after"), "kind={kind:?}: {text}");
            let pids = read_pid_lines(&pid_file);
            assert_eq!(
                pids.len(),
                2,
                "kind={kind:?}: cutover must force a fresh spawn: {pids:?}"
            );
            assert_ne!(
                pids[1], old_pid,
                "kind={kind:?}: new session must get a fresh client process"
            );
            assert!(
                process_alive(pids[1]),
                "kind={kind:?}: fresh client must be running"
            );

            // ...and the configured server is still listed.
            let listed = mcp_list_servers(&session).await;
            assert!(
                listed.contains("fake"),
                "kind={kind:?}: server config must survive the cutover: {listed}"
            );
        }
    }
}

#[cfg(test)]
mod vision_delegation_tests {
    use std::{
        collections::HashMap,
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use pi_ai::{AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream};
    use serde_json::json;

    use super::*;

    #[derive(Clone)]
    struct StreamCall {
        model: Model,
        context: Context,
        options: SimpleStreamOptions,
    }

    fn image() -> ContentBlock {
        image_with_data("aW1hZ2U=")
    }

    fn image_with_data(data: &str) -> ContentBlock {
        ContentBlock::Image {
            data: data.to_owned(),
            mime_type: "image/png".to_owned(),
        }
    }

    fn model(id: &str, provider: &str, api: &str, accepts_images: bool) -> Model {
        Model {
            id: id.to_owned(),
            name: id.to_owned(),
            provider: provider.to_owned(),
            api: api.to_owned(),
            input: if accepts_images {
                vec!["text".to_owned(), "image".to_owned()]
            } else {
                vec!["text".to_owned()]
            },
            ..Model::default()
        }
    }

    fn completed_stream(
        model: Model,
        response: Result<String, String>,
    ) -> AssistantMessageEventStream {
        let stream = pi_ai::new_assistant_message_event_stream();
        let producer = stream.clone();
        tokio::spawn(async move {
            producer
                .push(AssistantMessageEvent::Start {
                    partial: AssistantMessage::pending(&model),
                })
                .await;
            let mut message = AssistantMessage::pending(&model);
            match response {
                Ok(text) => {
                    message.content = vec![ContentBlock::text(text)];
                    message.stop_reason = StopReason::Stop;
                    producer
                        .push(AssistantMessageEvent::Done {
                            reason: StopReason::Stop,
                            message: message.clone(),
                        })
                        .await;
                }
                Err(error) => {
                    message.stop_reason = StopReason::Error;
                    message.error_message = Some(error);
                    producer
                        .push(AssistantMessageEvent::Error {
                            reason: StopReason::Error,
                            error: message.clone(),
                        })
                        .await;
                }
            }
            producer.end(Some(message)).await;
        });
        stream
    }

    fn recording_stream(
        responses: Vec<Result<&'static str, &'static str>>,
    ) -> (StreamFn, Arc<Mutex<Vec<StreamCall>>>, Arc<AtomicUsize>) {
        let responses = Arc::new(Mutex::new(std::collections::VecDeque::from(responses)));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let call_count = Arc::new(AtomicUsize::new(0));
        let captured_calls = calls.clone();
        let captured_count = call_count.clone();
        let stream: StreamFn = Arc::new(move |model, context, options| {
            captured_count.fetch_add(1, Ordering::SeqCst);
            captured_calls.lock().push(StreamCall {
                model: model.clone(),
                context,
                options,
            });
            let response = responses
                .lock()
                .pop_front()
                .unwrap_or(Ok("main response"))
                .map(str::to_owned)
                .map_err(str::to_owned);
            Box::pin(async move { completed_stream(model, response) })
        });
        (stream, calls, call_count)
    }

    async fn attach_vision_settings(
        session: &Session,
        agent_dir: &Path,
        cwd: &Path,
        vision_spec: Option<&str>,
    ) {
        let settings = vision_spec.map_or_else(|| json!({}), |spec| json!({ "visionModel": spec }));
        std::fs::write(
            agent_dir.join("settings.json"),
            serde_json::to_string(&settings).expect("settings json"),
        )
        .expect("write settings");
        session.set_session_dir(agent_dir.join("sessions"));
        let resources = isolated_test_resources(cwd, agent_dir, "danger-full-access");
        session
            .attach_resources(resources)
            .await
            .expect("attach resources");
    }

    fn test_session(
        cwd: &Path,
        main_model: Model,
        stream_fn: StreamFn,
        auth_resolver: Option<SessionAuthResolver>,
    ) -> Session {
        test_session_with_discovery(SessionOptions {
            model: main_model,
            cwd: cwd.to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "fallback-key".to_owned(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(stream_fn),
            auth_resolver,
            auth_manager: None,
        })
    }

    fn only_user_content(context: &Context) -> &[ContentBlock] {
        match context.messages.as_slice() {
            [Message::User(user)] => &user.content,
            messages => panic!("expected one user message, got {messages:?}"),
        }
    }

    #[tokio::test]
    async fn configured_vision_model_delegates_before_main_session_stream() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let api = format!("vision-delegation-api-{}", Uuid::now_v7());
        let provider = format!("vision-delegation-provider-{}", Uuid::now_v7());
        let main_model = model("main-text-only", &provider, &api, false);
        let vision_model = model("actual-vision-id", &provider, &api, true);
        let registration =
            pi_ai::providers::register_faux_provider(pi_ai::providers::FauxProviderOptions {
                api,
                provider: provider.clone(),
                models: vec![main_model.clone(), vision_model.clone()],
                chunk_size: 1,
            });
        let (stream, calls, _) =
            recording_stream(vec![Ok("screen description"), Ok("main answer")]);
        let session = test_session(cwd.path(), main_model.clone(), stream, None);
        attach_vision_settings(
            &session,
            agent_dir.path(),
            cwd.path(),
            Some(&format!("{provider}/{}", vision_model.id)),
        )
        .await;
        let _application = publish_attached_test_application(&session).await;

        session
            .run_messages(vec![Message::User(UserMessage {
                content: vec![ContentBlock::text("inspect"), image()],
                timestamp: 1,
            })])
            .await
            .expect("delegated run");

        let calls = calls.lock();
        assert_eq!(calls.len(), 2, "vision stream must run before main stream");
        assert_eq!(calls[0].model.id, vision_model.id);
        assert!(
            only_user_content(&calls[0].context)
                .iter()
                .any(|block| matches!(block, ContentBlock::Image { .. })),
            "vision context must contain the image"
        );
        assert_eq!(calls[1].model.id, main_model.id);
        let main_content = only_user_content(&calls[1].context);
        assert!(
            !main_content
                .iter()
                .any(|block| matches!(block, ContentBlock::Image { .. })),
            "main context must not contain the image"
        );
        let replacement = content_text(main_content);
        assert!(
            replacement.contains("[Image analyzed by actual-vision-id: screen description]"),
            "actual vision id and description must be inserted: {replacement}"
        );
        registration.unregister();
    }

    #[tokio::test]
    async fn run_messages_delegates_each_user_image_without_changing_other_roles() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let api = format!("vision-multi-api-{}", Uuid::now_v7());
        let provider = format!("vision-multi-provider-{}", Uuid::now_v7());
        let main_model = model("main-multi-model", &provider, &api, false);
        let vision_model = model("vision-multi-model", &provider, &api, true);
        let vision_model_id = vision_model.id.clone();
        let registration =
            pi_ai::providers::register_faux_provider(pi_ai::providers::FauxProviderOptions {
                api,
                provider: provider.clone(),
                models: vec![main_model.clone(), vision_model.clone()],
                chunk_size: 1,
            });
        let (stream, calls, _) =
            recording_stream(vec![Ok("first description"), Ok("second description")]);
        let session = test_session(cwd.path(), main_model, stream, None);
        attach_vision_settings(
            &session,
            agent_dir.path(),
            cwd.path(),
            Some(&format!("{provider}/{}", vision_model.id)),
        )
        .await;
        let _application = publish_attached_test_application(&session).await;
        let custom = Message::Custom(CustomMessage {
            custom_type: "vision-test".to_owned(),
            content: CustomMessageContent::Blocks(vec![image()]),
            display: false,
            details: None,
            timestamp: 2,
        });

        let delegated = session
            .delegate_vision_messages(vec![
                Message::User(UserMessage {
                    content: vec![image()],
                    timestamp: 1,
                }),
                custom.clone(),
                Message::User(UserMessage {
                    content: vec![image_with_data("aW1hZ2Uy")],
                    timestamp: 3,
                }),
            ])
            .await
            .expect("delegate user images");

        let calls = calls.lock();
        assert_eq!(calls.len(), 2, "each user image must make one vision call");
        assert!(
            calls.iter().all(|call| call.model.id == vision_model_id),
            "only the vision model may be called"
        );
        assert_eq!(delegated.len(), 3);
        assert_eq!(
            delegated[1], custom,
            "custom image message must remain unchanged"
        );
        for index in [0usize, 2] {
            let Message::User(user) = &delegated[index] else {
                panic!("delegated message {index} must remain a user message");
            };
            assert!(
                !user
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Image { .. })),
                "each user image must be delegated"
            );
        }
        registration.unregister();
    }

    #[tokio::test]
    async fn vision_auth_resolver_authenticates_the_vision_stream() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let api = format!("vision-auth-api-{}", Uuid::now_v7());
        let provider = format!("vision-auth-provider-{}", Uuid::now_v7());
        let main_model = model("main-auth-model", &provider, &api, false);
        let vision_model = model("vision-auth-model", &provider, &api, true);
        let registration =
            pi_ai::providers::register_faux_provider(pi_ai::providers::FauxProviderOptions {
                api,
                provider: provider.clone(),
                models: vec![main_model.clone(), vision_model.clone()],
                chunk_size: 1,
            });
        let (stream, calls, _) = recording_stream(vec![Ok("auth description")]);
        let resolved_models = Arc::new(Mutex::new(Vec::new()));
        let captured_models = resolved_models.clone();
        let resolver_calls = Arc::new(AtomicUsize::new(0));
        let captured_call_count = resolver_calls.clone();
        let resolver: SessionAuthResolver = Arc::new(move |model| {
            captured_call_count.fetch_add(1, Ordering::SeqCst);
            captured_models.lock().push(model.id.clone());
            Box::pin(async move {
                let mut headers = HashMap::new();
                headers.insert(
                    "X-Vision-Header".to_owned(),
                    format!("header-for-{}", model.id),
                );
                let mut env = HashMap::new();
                env.insert(
                    "VISION_AUTH_MARKER".to_owned(),
                    format!("env-for-{}", model.id),
                );
                Ok(RequestAuth {
                    api_key: format!("key-for-{}", model.id),
                    headers,
                    env,
                    available_model_ids: None,
                    kind: crate::RequestAuthKind::ApiKeyOrHeaders,
                })
            })
        });
        let session = test_session(cwd.path(), main_model, stream, Some(resolver));
        let mut main_options = SimpleStreamOptions::default();
        main_options.stream.api_key = Some("main-key-must-not-leak".to_owned());
        main_options
            .stream
            .headers
            .insert("X-Main-Header".to_owned(), "main-header".to_owned());
        main_options
            .stream
            .env
            .insert("MAIN_AUTH_MARKER".to_owned(), "main-env".to_owned());
        session.set_stream_options(main_options).await;
        attach_vision_settings(
            &session,
            agent_dir.path(),
            cwd.path(),
            Some(&format!("{provider}/{}", vision_model.id)),
        )
        .await;
        let _application = publish_attached_test_application(&session).await;
        resolver_calls.store(0, Ordering::SeqCst);
        resolved_models.lock().clear();

        session
            .steer(Message::User(UserMessage {
                content: vec![image()],
                timestamp: 1,
            }))
            .await
            .expect("authenticated delegated steer");

        assert_eq!(
            resolver_calls.load(Ordering::SeqCst),
            1,
            "vision delegation must resolve auth exactly once"
        );
        assert_eq!(
            resolved_models.lock().as_slice(),
            &[vision_model.id.clone()],
            "resolver must receive only the vision model"
        );
        let calls = calls.lock();
        assert_eq!(
            calls.len(),
            1,
            "queuing the delegated steer must make only the vision call"
        );
        let vision_call = &calls[0];
        assert_eq!(
            vision_call.options.stream.api_key.as_deref(),
            Some("key-for-vision-auth-model")
        );
        assert_eq!(
            vision_call
                .options
                .stream
                .headers
                .get("X-Vision-Header")
                .map(String::as_str),
            Some("header-for-vision-auth-model")
        );
        assert_eq!(
            vision_call
                .options
                .stream
                .env
                .get("VISION_AUTH_MARKER")
                .map(String::as_str),
            Some("env-for-vision-auth-model")
        );
        assert!(
            !vision_call
                .options
                .stream
                .headers
                .contains_key("X-Main-Header"),
            "main headers must not leak into vision auth"
        );
        assert!(
            !vision_call
                .options
                .stream
                .env
                .contains_key("MAIN_AUTH_MARKER"),
            "main env must not leak into vision auth"
        );
        registration.unregister();
    }

    #[tokio::test]
    async fn unconfigured_vision_model_preserves_image_for_main_stream() {
        let cwd = tempfile::tempdir().expect("cwd");
        let (stream, calls, _) = recording_stream(vec![Ok("main answer")]);
        let main_model = model(
            "main-no-vision-config",
            "vision-unconfigured-provider",
            "vision-unconfigured-api",
            false,
        );
        let session = test_session(cwd.path(), main_model, stream, None);
        let _application = publish_test_application(&session, cwd.path()).await;

        session
            .run("inspect", vec![image()])
            .await
            .expect("run without vision configuration");

        let calls = calls.lock();
        assert_eq!(calls.len(), 1);
        assert!(
            only_user_content(&calls[0].context)
                .iter()
                .any(|block| matches!(block, ContentBlock::Image { .. })),
            "unconfigured behavior must preserve the image"
        );
    }

    #[tokio::test]
    async fn live_vision_model_setting_delegates_without_resource_restage() {
        let cwd = tempfile::tempdir().expect("cwd");
        let api = format!("vision-live-api-{}", Uuid::now_v7());
        let provider = format!("vision-live-provider-{}", Uuid::now_v7());
        let main_model = model("main-live-text", &provider, &api, false);
        let vision_model = model("live-vision-id", &provider, &api, true);
        let registration =
            pi_ai::providers::register_faux_provider(pi_ai::providers::FauxProviderOptions {
                api,
                provider: provider.clone(),
                models: vec![main_model.clone(), vision_model.clone()],
                chunk_size: 1,
            });
        let (stream, calls, _) =
            recording_stream(vec![Ok("live description"), Ok("main answer")]);
        let session = test_session(cwd.path(), main_model, stream, None);
        // Publish WITHOUT visionModel so the resource snapshot stays empty —
        // the production LIVE `/settings set visionModel` path writes disk
        // and apply_runtime_settings, but does not restage resources.
        let _application = publish_test_application(&session, cwd.path()).await;
        assert!(
            session
                .resource_manager()
                .expect("resources")
                .snapshot()
                .settings
                .vision_model
                .is_none(),
            "resource snapshot must remain unconfigured after live apply"
        );

        let mut settings = crate::Settings::default();
        settings.vision_model = Some(format!("{provider}/{}", vision_model.id));
        session
            .apply_runtime_settings(settings.runtime_settings().expect("runtime settings"))
            .await;

        session
            .run("inspect", vec![image()])
            .await
            .expect("live vision setting must delegate");

        let calls = calls.lock();
        assert_eq!(calls.len(), 2, "vision stream must run before main stream");
        assert_eq!(calls[0].model.id, vision_model.id);
        assert!(
            only_user_content(&calls[0].context)
                .iter()
                .any(|block| matches!(block, ContentBlock::Image { .. })),
            "vision context must contain the image"
        );
        let main_content = only_user_content(&calls[1].context);
        assert!(
            !main_content
                .iter()
                .any(|block| matches!(block, ContentBlock::Image { .. })),
            "main context must not contain the image"
        );
        let replacement = content_text(main_content);
        assert!(
            replacement.contains("[Image analyzed by live-vision-id: live description]"),
            "live vision setting must reach the main model: {replacement}"
        );
        registration.unregister();
    }

    #[tokio::test]
    async fn live_vision_model_clear_disables_delegation_without_resource_restage() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let api = format!("vision-live-clear-api-{}", Uuid::now_v7());
        let provider = format!("vision-live-clear-provider-{}", Uuid::now_v7());
        let main_model = model("main-live-clear-text", &provider, &api, false);
        let vision_model = model("live-clear-vision-id", &provider, &api, true);
        let vision_spec = format!("{provider}/{}", vision_model.id);
        let registration =
            pi_ai::providers::register_faux_provider(pi_ai::providers::FauxProviderOptions {
                api,
                provider: provider.clone(),
                models: vec![main_model.clone(), vision_model.clone()],
                chunk_size: 1,
            });
        let (stream, calls, _) = recording_stream(vec![Ok("must not delegate"), Ok("main answer")]);
        let session = test_session(cwd.path(), main_model, stream, None);
        attach_vision_settings(&session, agent_dir.path(), cwd.path(), Some(&vision_spec)).await;
        let _application = publish_attached_test_application(&session).await;
        assert_eq!(
            session
                .resource_manager()
                .expect("resources")
                .snapshot()
                .settings
                .vision_model
                .as_deref(),
            Some(vision_spec.as_str()),
            "resource snapshot must still carry the configured vision model"
        );

        // Production LIVE `/settings set visionModel` to empty writes None
        // through apply_runtime_settings and does not restage resources.
        session
            .apply_runtime_settings(
                crate::Settings::default()
                    .runtime_settings()
                    .expect("cleared runtime settings"),
            )
            .await;
        assert_eq!(
            session
                .resource_manager()
                .expect("resources")
                .snapshot()
                .settings
                .vision_model
                .as_deref(),
            Some(vision_spec.as_str()),
            "live clear must not restage the resource snapshot"
        );

        session
            .run("inspect", vec![image()])
            .await
            .expect("live clear must skip vision delegation");

        let calls = calls.lock();
        assert_eq!(
            calls.len(),
            1,
            "cleared live visionModel must not open a vision stream"
        );
        assert_eq!(calls[0].model.id, "main-live-clear-text");
        assert!(
            only_user_content(&calls[0].context)
                .iter()
                .any(|block| matches!(block, ContentBlock::Image { .. })),
            "live clear must preserve the image for the main stream"
        );
        registration.unregister();
    }

    #[tokio::test]
    async fn vision_provider_error_prevents_main_provider_call() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let api = format!("vision-error-api-{}", Uuid::now_v7());
        let provider = format!("vision-error-provider-{}", Uuid::now_v7());
        let main_model = model("main-after-error", &provider, &api, false);
        let vision_model = model("vision-error-model", &provider, &api, true);
        let registration =
            pi_ai::providers::register_faux_provider(pi_ai::providers::FauxProviderOptions {
                api,
                provider: provider.clone(),
                models: vec![main_model.clone(), vision_model.clone()],
                chunk_size: 1,
            });
        let (stream, _, call_count) = recording_stream(vec![Err("vision service rejected image")]);
        let session = test_session(cwd.path(), main_model, stream, None);
        attach_vision_settings(
            &session,
            agent_dir.path(),
            cwd.path(),
            Some(&format!("{provider}/{}", vision_model.id)),
        )
        .await;
        let _application = publish_attached_test_application(&session).await;

        let error = session
            .run("inspect", vec![image()])
            .await
            .expect_err("vision error must fail the run");
        assert!(
            error.to_string().contains("vision service rejected image"),
            "actionable provider error: {error:#}"
        );
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "main provider must not run after vision failure"
        );
        registration.unregister();
    }

    #[tokio::test]
    async fn steer_and_follow_up_delegate_fail_closed_and_queue_originals() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let api = format!("vision-queue-api-{}", Uuid::now_v7());
        let provider = format!("vision-queue-provider-{}", Uuid::now_v7());
        let main_model = model("main-queue-model", &provider, &api, false);
        let vision_model = model("vision-queue-model", &provider, &api, true);
        let registration =
            pi_ai::providers::register_faux_provider(pi_ai::providers::FauxProviderOptions {
                api,
                provider: provider.clone(),
                models: vec![main_model.clone(), vision_model.clone()],
                chunk_size: 1,
            });
        let (stream, calls, _) =
            recording_stream(vec![Ok("steer description"), Ok("follow description")]);
        let session = test_session(cwd.path(), main_model, stream, None);
        attach_vision_settings(
            &session,
            agent_dir.path(),
            cwd.path(),
            Some(&format!("{provider}/{}", vision_model.id)),
        )
        .await;
        let _application = publish_attached_test_application(&session).await;

        let steer_image = image();
        let follow_image = image_with_data("aW1hZ2Uy");
        session
            .steer(Message::User(UserMessage {
                content: vec![steer_image.clone()],
                timestamp: 1,
            }))
            .await
            .expect("queue delegated steer");
        session
            .follow_up(Message::User(UserMessage {
                content: vec![follow_image.clone()],
                timestamp: 2,
            }))
            .await
            .expect("queue delegated follow-up");

        let (steering, follow_up) = session.queued_messages().await;
        let steering_content = match steering.as_slice() {
            [Message::User(user)] => &user.content,
            messages => panic!("expected one queued steering user message, got {messages:?}"),
        };
        let follow_up_content = match follow_up.as_slice() {
            [Message::User(user)] => &user.content,
            messages => panic!("expected one queued follow-up user message, got {messages:?}"),
        };
        // The queue carries the ORIGINAL user messages (images intact) so the
        // transcript/UI keep the images; only the model context is projected.
        assert_eq!(
            steering_content,
            &[steer_image],
            "queued steer must keep the original image"
        );
        assert_eq!(
            follow_up_content,
            &[follow_image],
            "queued follow-up must keep the original image"
        );
        assert!(
            !content_text(steering_content).contains("Image analyzed")
                && !content_text(follow_up_content).contains("Image analyzed"),
            "queued originals must not carry the delegated description"
        );
        assert_eq!(
            calls.lock().len(),
            2,
            "fail-closed delegation must run once per unique image before enqueue"
        );
        registration.unregister();
    }

    /// Concatenated text of every user message in a model context, in order.
    fn context_user_text(context: &Context) -> String {
        context
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::User(user) => Some(content_text(&user.content)),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn user_messages_contain_image(messages: &[Message]) -> bool {
        messages.iter().any(|message| match message {
            Message::User(user) => user
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Image { .. })),
            _ => false,
        })
    }

    #[tokio::test]
    async fn run_records_original_image_while_model_receives_description() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let api = format!("vision-record-api-{}", Uuid::now_v7());
        let provider = format!("vision-record-provider-{}", Uuid::now_v7());
        let main_model = model("main-record-model", &provider, &api, false);
        let vision_model = model("vision-record-model", &provider, &api, true);
        let registration =
            pi_ai::providers::register_faux_provider(pi_ai::providers::FauxProviderOptions {
                api,
                provider: provider.clone(),
                models: vec![main_model.clone(), vision_model.clone()],
                chunk_size: 1,
            });
        let (stream, calls, _) =
            recording_stream(vec![Ok("screen description"), Ok("main answer")]);
        let session = test_session(cwd.path(), main_model, stream, None);
        attach_vision_settings(
            &session,
            agent_dir.path(),
            cwd.path(),
            Some(&format!("{provider}/{}", vision_model.id)),
        )
        .await;
        let _application = publish_attached_test_application(&session).await;
        let recorder = crate::start_session_in(
            cwd.path(),
            None,
            None,
            Some(cwd.path()),
            Some("vision-recorder-test"),
            None,
        )
        .expect("start recorder");
        session.record(recorder).expect("attach recorder");

        session
            .run("inspect", vec![image()])
            .await
            .expect("vision run");

        // Model context: the vision model sees the image, the main model sees
        // only the delegated description.
        let calls = calls.lock();
        assert_eq!(
            calls.len(),
            2,
            "vision delegation must run before the main stream"
        );
        assert!(
            only_user_content(&calls[0].context)
                .iter()
                .any(|block| matches!(block, ContentBlock::Image { .. })),
            "vision context must contain the image"
        );
        let main_content = only_user_content(&calls[1].context);
        assert!(
            !main_content
                .iter()
                .any(|block| matches!(block, ContentBlock::Image { .. })),
            "main context must not contain the image"
        );
        let replacement = content_text(main_content);
        assert!(
            replacement.contains("[Image analyzed by vision-record-model: screen description]"),
            "delegated description must reach the main model: {replacement}"
        );
        drop(calls);

        // User-visible history keeps the ORIGINAL message with the image.
        let history = session.history();
        let recorded_user = match history.as_slice() {
            [Message::User(user), Message::Assistant(_)] => user,
            messages => panic!("expected user then assistant in history, got {messages:?}"),
        };
        assert!(
            recorded_user
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Image { .. })),
            "history must keep the original image"
        );
        assert!(
            !content_text(&recorded_user.content).contains("Image analyzed"),
            "history must not contain the delegated description"
        );

        // The recorder tree (what reload/build_context replay) keeps the image.
        let tree = session
            .current_recorder()
            .expect("recorder")
            .tree()
            .expect("recorder tree");
        let entries = tree
            .branch(None)
            .into_iter()
            .filter(|entry| entry.entry_type == "message")
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 2, "user + assistant recorded");
        let Message::User(recorded_user) = entries[0].message.as_ref().expect("user entry message")
        else {
            panic!("first recorded message must be user");
        };
        assert!(
            recorded_user
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Image { .. })),
            "recorder tree must keep the original image"
        );
        assert!(
            !content_text(&recorded_user.content).contains("Image analyzed"),
            "recorder tree must not contain the delegated description"
        );
        registration.unregister();
    }

    #[tokio::test]
    async fn reload_keeps_image_and_next_turn_still_projects_description() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let api = format!("vision-reload-api-{}", Uuid::now_v7());
        let provider = format!("vision-reload-provider-{}", Uuid::now_v7());
        let main_model = model("main-reload-model", &provider, &api, false);
        let vision_model = model("vision-reload-model", &provider, &api, true);
        let vision_model_id = vision_model.id.clone();
        let registration =
            pi_ai::providers::register_faux_provider(pi_ai::providers::FauxProviderOptions {
                api,
                provider: provider.clone(),
                models: vec![main_model.clone(), vision_model.clone()],
                chunk_size: 1,
            });
        let (stream, _, _) = recording_stream(vec![Ok("screen description"), Ok("main answer")]);
        let session = test_session(cwd.path(), main_model.clone(), stream, None);
        attach_vision_settings(
            &session,
            agent_dir.path(),
            cwd.path(),
            Some(&format!("{provider}/{}", vision_model.id)),
        )
        .await;
        let _application = publish_attached_test_application(&session).await;
        let recorder = crate::start_session_in(
            cwd.path(),
            None,
            None,
            Some(cwd.path()),
            Some("vision-reload-test"),
            None,
        )
        .expect("start recorder");
        let session_path = recorder.path();
        session.record(recorder).expect("attach recorder");
        session
            .run("inspect", vec![image()])
            .await
            .expect("first turn");

        // Reload: the persisted session replays the ORIGINAL image for
        // user-visible history, never the delegated description.
        let tree = crate::load_session_tree(&session_path).expect("reload session");
        let context = tree.build_context(None);
        let reloaded_user = match context.messages.as_slice() {
            [Message::User(user), Message::Assistant(_)] => user,
            messages => panic!("expected user then assistant after reload, got {messages:?}"),
        };
        assert!(
            reloaded_user
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Image { .. })),
            "reloaded context must keep the original image"
        );
        assert!(
            !content_text(&reloaded_user.content).contains("Image analyzed"),
            "reloaded context must not contain the delegated description"
        );

        // A fresh session restored from that file must still send the
        // description — never the historical image — to the non-vision model
        // on the next turn.
        let (stream2, calls2, _) =
            recording_stream(vec![Ok("screen description"), Ok("second answer")]);
        let session2 = test_session(cwd.path(), main_model, stream2, None);
        attach_vision_settings(
            &session2,
            agent_dir.path(),
            cwd.path(),
            Some(&format!("{provider}/{}", vision_model.id)),
        )
        .await;
        let _application2 = publish_attached_test_application(&session2).await;
        session2
            .record(crate::resume_session(&session_path).expect("resume recorder"))
            .expect("attach recorder");
        session2
            .load_history(context.messages)
            .await
            .expect("load history");

        session2
            .run("second turn", Vec::new())
            .await
            .expect("second turn");

        let calls = calls2.lock();
        assert_eq!(
            calls.len(),
            2,
            "lazy vision delegation then main stream on the restored session"
        );
        assert_eq!(
            calls[0].model.id, vision_model_id,
            "historical image must be delegated to the vision model"
        );
        assert!(
            only_user_content(&calls[0].context)
                .iter()
                .any(|block| matches!(block, ContentBlock::Image { .. })),
            "vision context must contain the historical image"
        );
        let main_context = &calls[1].context;
        assert!(
            !user_messages_contain_image(&main_context.messages),
            "main context must not resend the historical image"
        );
        let main_text = context_user_text(main_context);
        assert!(
            main_text.contains("[Image analyzed by vision-reload-model: screen description]"),
            "historical image must be projected to the description: {main_text}"
        );
        assert!(
            main_text.contains("second turn"),
            "new user text must reach the model"
        );
        registration.unregister();
    }
}

#[cfg(test)]
mod fallback_error_routing_tests {
    use std::{
        collections::HashSet,
        sync::atomic::{AtomicU64, Ordering},
    };

    use pi_ai::providers::{
        FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
    };

    use super::*;

    static NEXT_FALLBACK_TEST_ID: AtomicU64 = AtomicU64::new(1);

    fn model(label: &str) -> Model {
        let suffix = NEXT_FALLBACK_TEST_ID.fetch_add(1, Ordering::Relaxed);
        Model {
            id: "model".to_owned(),
            name: label.to_owned(),
            provider: format!("{label}-{suffix}"),
            api: format!("fallback-error-api-{label}-{suffix}"),
            base_url: "http://localhost:0".to_owned(),
            ..Model::default()
        }
    }

    fn register(model: &Model, responses: Vec<FauxResponse>) -> FauxProviderRegistration {
        let registration = register_faux_provider(FauxProviderOptions {
            api: model.api.clone(),
            provider: model.provider.clone(),
            models: vec![model.clone()],
            chunk_size: 64,
        });
        registration.set_responses(responses);
        registration
    }

    fn session(
        primary: Model,
        chain: &[Model],
        credentialed: &[Model],
        max_retries: usize,
    ) -> (Session, tempfile::TempDir) {
        let mut available = credentialed
            .iter()
            .map(|model| model.provider.clone())
            .collect::<HashSet<_>>();
        available.insert(primary.provider.clone());
        let resolver: SessionAuthResolver = Arc::new(move |model| {
            let is_available = available.contains(&model.provider);
            Box::pin(async move {
                if !is_available {
                    return Err(anyhow!("credentials unavailable"));
                }
                Ok(RequestAuth {
                    api_key: "fallback-test-key".to_owned(),
                    headers: Default::default(),
                    env: Default::default(),
                    available_model_ids: None,
                    kind: crate::RequestAuthKind::ApiKeyOrHeaders,
                })
            })
        });
        let cwd = tempfile::tempdir().expect("fallback cwd");
        let active = Session::new(SessionOptions {
            model: primary.clone(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "primary-key".to_owned(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: Some(resolver),
            auth_manager: None,
        })
        .expect("fallback session");
        let mut chains = crate::RetryFallbackChains::default();
        if !chain.is_empty() {
            chains.insert(
                format!("{}/{}", primary.provider, primary.id),
                chain
                    .iter()
                    .map(|model| format!("{}/{}", model.provider, model.id))
                    .collect(),
            );
        }
        active.set_retry_settings(RetrySettings {
            enabled: true,
            max_retries,
            base_delay_ms: 1,
            model_fallback: true,
            fallback_chains: chains,
        });
        (active, cwd)
    }

    async fn collect_events<T>(
        session: &Session,
        work: impl Future<Output = T>,
    ) -> (T, Vec<SessionEvent>) {
        let mut receiver = session.subscribe_session_events();
        tokio::pin!(work);
        let mut events = Vec::new();
        let output = loop {
            tokio::select! {
                output = &mut work => break output,
                event = receiver.recv() => if let Ok(event) = event { events.push(event); },
            }
        };
        while let Ok(event) = receiver.try_recv() {
            events.push(event);
        }
        (output, events)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn weekly_limit_skips_retry_and_missing_credentials_then_uses_fresh_candidate_budget() {
        let primary = model("weekly-primary");
        let missing = model("missing-credentials");
        let deepseek = model("deepseek");
        let final_model = model("final");
        let registrations = [
            register(
                &primary,
                vec![FauxResponse::error(
                    "HTTP 429 usage/quota exhausted: Weekly usage limit reached",
                )],
            ),
            register(&missing, vec![FauxResponse::text("must not run")]),
            register(
                &deepseek,
                vec![
                    FauxResponse::error("503 DeepSeek provider unavailable"),
                    FauxResponse::error("503 DeepSeek provider unavailable again"),
                ],
            ),
            register(
                &final_model,
                vec![FauxResponse::text("ordered fallback succeeded")],
            ),
        ];
        let chain = [missing.clone(), deepseek.clone(), final_model.clone()];
        let credentials = [deepseek.clone(), final_model.clone()];
        let (session, _cwd) = session(primary.clone(), &chain, &credentials, 1);

        let (output, events) = collect_events(&session, session.run("fallback", Vec::new())).await;
        assert_eq!(
            output.expect("ordered fallback").text,
            "ordered fallback succeeded"
        );
        let recovery = events
            .iter()
            .filter_map(|event| match event {
                SessionEvent::RetryFallbackApplied { from, to, .. } => {
                    Some(format!("apply:{from}->{to}"))
                }
                SessionEvent::AutoRetryStart { attempt, .. } => Some(format!("retry:{attempt}")),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            recovery,
            vec![
                format!(
                    "apply:{}/{}:off->{}/{}:off",
                    primary.provider, primary.id, deepseek.provider, deepseek.id
                ),
                "retry:1".to_owned(),
                format!(
                    "apply:{}/{}:off->{}/{}:off",
                    deepseek.provider, deepseek.id, final_model.provider, final_model.id
                ),
            ],
            "weekly exhaustion must not consume a retry and credential-missing candidates must be skipped: {events:?}"
        );
        assert!(events.iter().all(|event| !matches!(event, SessionEvent::RetryFallbackApplied { to, .. } if to.contains(&missing.provider))));
        for registration in registrations {
            registration.unregister();
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exhausted_usage_chain_returns_one_aggregate_error() {
        let primary = model("weekly-exhausted");
        let fallback = model("monthly-exhausted");
        let registrations = [
            register(
                &primary,
                vec![FauxResponse::error(
                    "HTTP 429 usage/quota exhausted: Weekly usage limit reached",
                )],
            ),
            register(
                &fallback,
                vec![FauxResponse::error(
                    "HTTP 429 usage/quota exhausted: Monthly usage limit reached",
                )],
            ),
        ];
        let (session, _cwd) = session(
            primary,
            std::slice::from_ref(&fallback),
            std::slice::from_ref(&fallback),
            3,
        );

        let (output, events) = collect_events(&session, session.run("exhaust", Vec::new())).await;
        let error = output.expect_err("all candidates exhausted").to_string();
        assert!(error.contains("Weekly usage limit reached"), "{error}");
        assert!(error.contains("Monthly usage limit reached"), "{error}");
        assert_eq!(
            error.matches("Weekly usage limit reached").count(),
            1,
            "{error}"
        );
        assert_eq!(
            error.matches("Monthly usage limit reached").count(),
            1,
            "{error}"
        );
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, SessionEvent::AutoRetryStart { .. }))
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, SessionEvent::AutoRetryEnd { success: false, .. }))
                .count(),
            1
        );
        for registration in registrations {
            registration.unregister();
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fallback_only_error_without_chain_fails_without_retry() {
        let primary = model("single-model");
        let registration = register(
            &primary,
            vec![FauxResponse::error(
                "HTTP 429 usage/quota exhausted: Weekly usage limit reached",
            )],
        );
        let (session, _cwd) = session(primary, &[], &[], 4);

        let (output, events) = collect_events(&session, session.run("single", Vec::new())).await;
        let error = output
            .expect_err("single model must fail clearly")
            .to_string();
        assert!(error.contains("Weekly usage limit reached"), "{error}");
        assert!(events.iter().all(|event| !matches!(
            event,
            SessionEvent::AutoRetryStart { .. } | SessionEvent::RetryFallbackApplied { .. }
        )));
        registration.unregister();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn abort_and_tool_call_errors_never_apply_fallback() {
        let fallback = model("guarded-fallback");
        let cases = [
            (
                FauxResponse {
                    content: Vec::new(),
                    stop_reason: StopReason::Aborted,
                    error_message: Some("Request was aborted".to_owned()),
                },
                "Request was aborted",
            ),
            (
                FauxResponse {
                    content: vec![ContentBlock::ToolCall(ToolCall {
                        id: "call".to_owned(),
                        name: "read".to_owned(),
                        arguments: serde_json::json!({"path":"safe"}),
                        thought_signature: None,
                    })],
                    stop_reason: StopReason::Error,
                    error_message: Some(
                        "HTTP 429 usage/quota exhausted: Weekly usage limit reached".to_owned(),
                    ),
                },
                "Weekly usage limit reached",
            ),
        ];
        for (response, expected_error) in cases {
            let primary = model("guarded-primary");
            let registrations = [
                register(&primary, vec![response]),
                register(&fallback, vec![FauxResponse::text("must not run")]),
            ];
            let (session, _cwd) = session(
                primary,
                std::slice::from_ref(&fallback),
                std::slice::from_ref(&fallback),
                0,
            );
            let (output, events) = collect_events(&session, session.run("guard", Vec::new())).await;
            let error = output
                .expect_err("guarded failure remains terminal")
                .to_string();
            assert!(error.contains(expected_error), "{error}");
            assert!(
                events.iter().all(|event| !matches!(
                    event,
                    SessionEvent::RetryFallbackApplied { .. } | SessionEvent::AutoRetryStart { .. }
                )),
                "{events:?}"
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(
                        event,
                        SessionEvent::AutoRetryEnd { success: false, .. }
                    ))
                    .count(),
                0,
                "single terminal guard must not duplicate the failure: {events:?}"
            );
            for registration in registrations {
                registration.unregister();
            }
        }
    }
}

#[cfg(test)]
mod classifier_model_resolution_tests {
    use pi_ai::providers::{FauxProviderOptions, register_faux_provider};

    use super::*;

    fn settings_with(spec: Option<&str>) -> crate::SelectorSettings {
        crate::SelectorSettings {
            classifier: crate::SelectorClassifierSettings {
                enabled: true,
                model: spec.map(str::to_owned),
                ..crate::SelectorClassifierSettings::default()
            },
            ..crate::SelectorSettings::default()
        }
    }

    fn session_model() -> Model {
        crate::resolve_model("anthropic/claude-sonnet-4-5").expect("catalog session model")
    }

    #[test]
    fn unconfigured_classifier_uses_session_model() {
        let session = session_model();
        assert_eq!(
            resolve_classifier_model(&settings_with(None), &session).as_ref(),
            Ok(&session)
        );
        // A whitespace-only spec is unconfigured, not a resolver default.
        assert_eq!(
            resolve_classifier_model(&settings_with(Some("  ")), &session).as_ref(),
            Ok(&session)
        );
    }

    #[test]
    fn combined_provider_id_and_case_insensitive_provider_resolve() {
        let session = session_model();
        let combined = resolve_classifier_model(&settings_with(Some("xai/grok-4.5")), &session)
            .expect("combined provider/id spec must resolve");
        assert_eq!(
            (combined.provider.as_str(), combined.id.as_str()),
            ("xai", "grok-4.5")
        );

        let upper = resolve_classifier_model(&settings_with(Some("XAI/GROK-4.5")), &session)
            .expect("case-insensitive provider must resolve");
        assert_eq!(
            (upper.provider.as_str(), upper.id.as_str()),
            ("xai", "grok-4.5")
        );
    }

    #[test]
    fn bare_catalog_id_resolves_like_the_shared_resolver() {
        // The old split_once('/') path returned None here and silently
        // disabled the classifier.
        let session = session_model();
        let expected = crate::resolve_model_pattern("grok-4.5").expect("bare catalog id");
        let resolved = resolve_classifier_model(&settings_with(Some("grok-4.5")), &session)
            .expect("bare catalog id must resolve");
        assert_eq!(resolved.provider, expected.model.provider);
        assert_eq!(resolved.id, expected.model.id);
    }

    #[test]
    fn openrouter_style_slashed_id_resolves() {
        let session = session_model();
        let resolved =
            resolve_classifier_model(&settings_with(Some("ai21/jamba-large-1.7")), &session)
                .expect("openrouter-style id must resolve");
        assert_eq!(
            (resolved.provider.as_str(), resolved.id.as_str()),
            ("openrouter", "ai21/jamba-large-1.7")
        );
    }

    #[test]
    fn thinking_suffix_is_parsed_but_ignored() {
        let session = session_model();
        let resolved =
            resolve_classifier_model(&settings_with(Some("xai/grok-4.5:high")), &session)
                .expect("suffixed spec must resolve");
        assert_eq!(
            (resolved.provider.as_str(), resolved.id.as_str()),
            ("xai", "grok-4.5"),
            "effort suffix must not leak into the model id"
        );
    }

    #[test]
    fn known_provider_custom_id_resolves_through_fallback() {
        let session = session_model();
        let resolved =
            resolve_classifier_model(&settings_with(Some("xai/my-custom-classifier")), &session)
                .expect("known-provider custom id must resolve");
        assert_eq!(
            (resolved.provider.as_str(), resolved.id.as_str()),
            ("xai", "my-custom-classifier")
        );
    }

    #[test]
    fn invalid_spec_fails_instead_of_silently_disabling() {
        let session = session_model();
        let error = resolve_classifier_model(
            &settings_with(Some("zz-absent-classifier-model-42")),
            &session,
        )
        .expect_err("invalid spec must fail validation");
        assert!(error.contains("not found"), "{error}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn invalid_classifier_model_surfaces_in_selection_fallback_reason() {
        let model = Model {
            id: "model".to_owned(),
            name: "classifier-fixture".to_owned(),
            provider: "classifier-fixture".to_owned(),
            api: "classifier-fixture-api".to_owned(),
            base_url: "http://localhost:0".to_owned(),
            ..Model::default()
        };
        let registration = register_faux_provider(FauxProviderOptions {
            api: model.api.clone(),
            provider: model.provider.clone(),
            models: vec![model.clone()],
            chunk_size: 1,
        });
        let cwd = tempfile::tempdir().expect("classifier cwd");
        let session = test_session_with_discovery(SessionOptions {
            model: model.clone(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "classifier-key".to_owned(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        });

        // Invalid configuration: the classifier is not silently dropped; the
        // selection plan names the broken model instead of the generic
        // "classifier enabled but unavailable" reason.
        *session.inner.shared.selector_settings.write() = crate::SelectorSettings {
            classifier: crate::SelectorClassifierSettings {
                enabled: true,
                model: Some("zz-absent-classifier-model-42".to_owned()),
                ..crate::SelectorClassifierSettings::default()
            },
            ..crate::SelectorSettings::default()
        };
        let plan = session.select_for_request("review the parser").await;
        let reason = plan
            .fallback_reason
            .expect("invalid classifier model must surface a fallback reason");
        assert!(reason.contains("classifier model invalid"), "{reason}");
        assert!(reason.contains("zz-absent-classifier-model-42"), "{reason}");
        assert!(!plan.classifier_used);

        // Valid configuration: the classifier object is constructed from the
        // resolved faux-provider model, so the plan reaches the deterministic
        // no-candidate skip instead of "enabled but unavailable".
        *session.inner.shared.selector_settings.write() = crate::SelectorSettings {
            classifier: crate::SelectorClassifierSettings {
                enabled: true,
                model: Some(format!("{}/model", model.provider)),
                ..crate::SelectorClassifierSettings::default()
            },
            ..crate::SelectorSettings::default()
        };
        let plan = session.select_for_request("review the parser").await;
        assert_eq!(
            plan.fallback_reason.as_deref(),
            Some("classifier skipped because deterministic ranking found no candidates"),
            "a valid configured classifier model must keep the classifier available: {plan:?}"
        );

        registration.unregister();
    }
}

// ---------------------------------------------------------------------------
// Hook composition regression tests (GPT review findings 1-4).
// Failing until the implementer lands the corresponding hook-parity fixes;
// each test names the exact production function it pins.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod hook_composition_regression_tests {
    use std::{path::Path, time::Duration};

    use pi_ai::AssistantMessage;
    use serde_json::Map;

    use super::*;
    use crate::{HookConfig, HookEvent};

    /// Executable hook fixture appending `fired` to `marker` per invocation.
    fn write_marker_hook(dir: &Path, name: &str, marker: &Path) -> String {
        let tmp = dir.join(format!("{name}.tmp-{}", Uuid::now_v7()));
        std::fs::write(
            &tmp,
            format!(
                "#!/bin/sh\nprintf 'fired\\n' >> '{}'\n",
                marker.to_string_lossy()
            ),
        )
        .expect("write hook script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&tmp).expect("hook metadata").permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&tmp, permissions).expect("chmod hook");
        }
        let path = dir.join(name);
        std::fs::rename(&tmp, &path).expect("rename hook into place");
        path.to_string_lossy().into_owned()
    }

    fn marker_lines(marker: &Path) -> usize {
        std::fs::read_to_string(marker)
            .map(|body| body.lines().count())
            .unwrap_or(0)
    }

    /// Hook firing is awaited inline, so one loop pass here is belt and braces.
    fn fired_lines(marker: &Path) -> usize {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let lines = marker_lines(marker);
            if lines > 0 || std::time::Instant::now() >= deadline {
                return lines;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn after_context(
        tool_name: &str,
        result: pi_agent::AgentToolResult,
        is_error: bool,
    ) -> pi_agent::AfterToolCallContext {
        pi_agent::AfterToolCallContext {
            assistant_message: AssistantMessage::pending(&Model::default()),
            tool_call: ToolCall {
                id: "call-1".to_owned(),
                name: tool_name.to_owned(),
                arguments: serde_json::json!({}),
                thought_signature: None,
            },
            arguments: serde_json::json!({}),
            result,
            is_error,
            context: pi_agent::AgentContext {
                system_prompt: String::new(),
                messages: Vec::new(),
                tools: Vec::new(),
            },
        }
    }

    fn before_context(tool_name: &str) -> pi_agent::BeforeToolCallContext {
        pi_agent::BeforeToolCallContext {
            assistant_message: AssistantMessage::pending(&Model::default()),
            tool_call: ToolCall {
                id: "call-2".to_owned(),
                name: tool_name.to_owned(),
                arguments: serde_json::json!({}),
                thought_signature: None,
            },
            arguments: serde_json::json!({}),
            context: pi_agent::AgentContext {
                system_prompt: String::new(),
                messages: Vec::new(),
                tools: Vec::new(),
            },
            approval_request: None,
        }
    }

    fn failing_todo_result() -> pi_agent::AgentToolResult {
        let mut details = Map::new();
        details.insert(
            "op".to_owned(),
            serde_json::Value::String("append".to_owned()),
        );
        details.insert(
            crate::todo::TODO_ERROR_MARKER.to_owned(),
            serde_json::Value::Bool(true),
        );
        pi_agent::AgentToolResult {
            content: vec![ContentBlock::text("todo op failed")],
            details: serde_json::Value::Object(details),
            ..pi_agent::AgentToolResult::default()
        }
    }

    /// GPT finding (1): `compose_after_tool_call` must retain the first hook's
    /// content/details/is_error when the second hook omits them. Today the
    /// composed result reads `later.content`/`later.details`/`later.is_error`
    /// only, so an observing second hook that returns a default result blanks
    /// the first hook's modifications.
    #[tokio::test]
    async fn compose_after_tool_call_retains_first_fields_when_second_hook_omits() {
        let first: AfterToolCallFn = Arc::new(|_| {
            Box::pin(async {
                Ok(AfterToolCallResult {
                    content: Some(vec![ContentBlock::text("first content")]),
                    details: Some(serde_json::json!({ "shape": "first" })),
                    is_error: Some(true),
                    ..AfterToolCallResult::default()
                })
            })
        });
        let second: AfterToolCallFn = Arc::new(|_| {
            Box::pin(async { Ok(AfterToolCallResult::default()) })
        });
        let composed =
            compose_after_tool_call(Some(first), Some(second)).expect("composed hook");
        let result = composed(after_context("read", pi_agent::AgentToolResult::text("original"), false))
            .await
            .expect("composed hook runs");

        assert_eq!(
            result.is_error,
            Some(true),
            "second hook omitted is_error — the first hook's Some(true) must be retained: {result:?}"
        );
        assert_eq!(
            result.details.as_ref().and_then(|details| details.get("shape")),
            Some(&serde_json::json!("first")),
            "second hook omitted details — the first hook's details must be retained: {result:?}"
        );
        let text = result
            .content
            .unwrap_or_default()
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("first content"),
            "second hook omitted content — the first hook's content must be retained: {text:?}"
        );
    }

    /// Contract guard for the finding (1) fix: when the second hook sets a
    /// field explicitly it still wins — retention must not overturn precedence.
    #[tokio::test]
    async fn compose_after_tool_call_second_fields_win_when_both_hooks_set() {
        let first: AfterToolCallFn = Arc::new(|_| {
            Box::pin(async {
                Ok(AfterToolCallResult {
                    content: Some(vec![ContentBlock::text("first content")]),
                    is_error: Some(true),
                    ..AfterToolCallResult::default()
                })
            })
        });
        let second: AfterToolCallFn = Arc::new(|_| {
            Box::pin(async {
                Ok(AfterToolCallResult {
                    content: Some(vec![ContentBlock::text("second content")]),
                    is_error: Some(false),
                    ..AfterToolCallResult::default()
                })
            })
        });
        let composed =
            compose_after_tool_call(Some(first), Some(second)).expect("composed hook");
        let result = composed(after_context("read", pi_agent::AgentToolResult::default(), false))
            .await
            .expect("composed hook runs");

        assert_eq!(result.is_error, Some(false), "later field must win");
        let text = result
            .content
            .unwrap_or_default()
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            text, "second content",
            "later field must win over the first hook's content"
        );
    }

    /// GPT finding (2): `Session::set_after_tool_call` replaces the installed
    /// chain without re-applying the construction-time `todo_after_tool_call`
    /// wrapper, so the internal `__piTodoError` marker normalization (marker
    /// stripped from details, result forced to is_error) silently disappears
    /// whenever Application/extension wiring installs a reducer hook.
    #[tokio::test]
    async fn set_after_tool_call_preserves_todo_error_normalization() {
        let cwd = tempfile::tempdir().expect("cwd");
        let suffix = Uuid::now_v7().to_string();
        let api = format!("todo-hook-api-{suffix}");
        let provider = format!("todo-hook-provider-{suffix}");
        let model = Model {
            id: "todo-hook-model".to_owned(),
            name: "Todo Hook Model".to_owned(),
            api: api.clone(),
            provider: provider.clone(),
            ..Model::default()
        };
        let registration =
            pi_ai::providers::register_faux_provider(pi_ai::providers::FauxProviderOptions {
                api,
                provider,
                models: vec![model.clone()],
                chunk_size: 1,
            });
        let session = Session::new_with_todo_and_additional_tools_filtered_and_discovery(
            SessionOptions {
                model,
                cwd: cwd.path().to_path_buf(),
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
            },
            Vec::new(),
            ToolSelection::default(),
            ResourceDiscovery::Disabled,
        )
        .expect("todo session");

        // The Application/extension replacement hook: a plain observer that
        // returns a default result (exactly what the extension reducer behaves
        // like for unreduced calls).
        session.set_after_tool_call(Some(Arc::new(|_| {
            Box::pin(async { Ok(AfterToolCallResult::default()) })
        })));
        let after = session
            .inner
            .agent
            .after_tool_call()
            .expect("agent must have an installed after hook");

        let result = after(
            after_context("todo", failing_todo_result(), false),
        )
        .await
        .expect("installed after hook runs");

        assert_eq!(
            result.is_error,
            Some(true),
            "todo internal error marker must force is_error through set_after_tool_call: {result:?}"
        );
        let details = result
            .details
            .expect("todo normalization must keep normalized details present");
        assert!(
            details.get(crate::todo::TODO_ERROR_MARKER).is_none(),
            "todo error marker must be stripped from details after set_after_tool_call: {details}"
        );
        registration.unregister();
    }

    /// GPT finding (3): a detached Code Review fork's hooks are a fork-time
    /// snapshot and may not cross the main Application's generation/tool
    /// snapshots. Today the composed pre/post hooks read
    /// `shared.host_hooks` live, so host hook changes on the main session
    /// after `fork_side_chat` silently change the fork's tool governance.
    #[tokio::test]
    async fn detached_fork_hooks_fire_the_fork_time_host_hook_snapshot() {
        let cwd = tempfile::tempdir().expect("cwd");
        let marker_fork_pre = cwd.path().join("fork-time-pre");
        let marker_fork_post = cwd.path().join("fork-time-post");
        let marker_new_pre = cwd.path().join("post-fork-pre");
        let marker_new_post = cwd.path().join("post-fork-post");
        let fork_pre_hook = write_marker_hook(cwd.path(), "fork-pre.sh", &marker_fork_pre);
        let fork_post_hook = write_marker_hook(cwd.path(), "fork-post.sh", &marker_fork_post);
        let new_pre_hook = write_marker_hook(cwd.path(), "new-pre.sh", &marker_new_pre);
        let new_post_hook = write_marker_hook(cwd.path(), "new-post.sh", &marker_new_post);

        let session = test_session_with_discovery(SessionOptions {
            model: Model::default(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: None,
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        });
        let application = publish_test_application(&session, cwd.path()).await;

        let hook_entry =
            |event: HookEvent, hook: &str| HookConfig {
                event,
                matcher: None,
                command: vec![hook.to_owned()],
                timeout_ms: Some(2_000),
                enabled: None,
                fail_closed: None,
                extra: Map::new(),
            };
        session.set_host_hooks(Some(vec![
            hook_entry(HookEvent::PreToolCall, &fork_pre_hook),
            hook_entry(HookEvent::PostToolCall, &fork_post_hook),
        ]));

        // The fork captures its hooks from the main session at fork time.
        let fork = application
            .fork_side_chat()
            .await
            .expect("fork side chat");
        let fork_before = fork
            .before_tool_call
            .expect("fork must capture a before hook");
        let fork_after = fork
            .after_tool_call
            .expect("fork must capture an after hook");

        // A later generation on the main session replaces its host hooks.
        session.set_host_hooks(Some(vec![
            hook_entry(HookEvent::PreToolCall, &new_pre_hook),
            hook_entry(HookEvent::PostToolCall, &new_post_hook),
        ]));

        fork_before(before_context("read"))
            .await
            .expect("fork before hook runs");
        fork_after(after_context("read", pi_agent::AgentToolResult::default(), false))
            .await
            .expect("fork after hook runs");

        assert_eq!(
            fired_lines(&marker_fork_pre),
            1,
            "the fork's before hook must still fire the fork-time snapshot"
        );
        assert_eq!(
            fired_lines(&marker_fork_post),
            1,
            "the fork's after hook must still fire the fork-time snapshot"
        );
        assert_eq!(
            marker_lines(&marker_new_pre),
            0,
            "main host hooks installed after the fork must not fire in the fork"
        );
        assert_eq!(
            marker_lines(&marker_new_post),
            0,
            "main host hooks installed after the fork must not fire in the fork"
        );
    }

    /// GPT finding (4): `Session::set_before_tool_call` must not double-wrap
    /// the host pre-hook. Application composes the extension reducer onto
    /// [`Session::raw_before_tool_call`] (the policy chain WITHOUT the host
    /// wrapper); `set_before_tool_call` then stores that raw chain and wraps
    /// host-pre exactly once.
    #[tokio::test]
    async fn set_before_tool_call_does_not_double_fire_host_pre_hook() {
        let cwd = tempfile::tempdir().expect("cwd");
        let marker = cwd.path().join("pre-fire-count");
        let hook = write_marker_hook(cwd.path(), "pre-count.sh", &marker);

        let session = test_session_with_discovery(SessionOptions {
            model: Model::default(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: None,
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        });
        let _application = publish_test_application(&session, cwd.path()).await;
        session.set_host_hooks(Some(vec![HookConfig {
            event: HookEvent::PreToolCall,
            matcher: None,
            command: vec![hook],
            timeout_ms: Some(2_000),
            enabled: None,
            fail_closed: None,
            extra: Map::new(),
        }]));

        // Exactly the Application wiring pattern (application.rs): compose
        // the extension hook onto the RAW policy chain, then set.
        let extension_hook: pi_agent::BeforeToolCallFn = Arc::new(|_| {
            Box::pin(async { Ok(pi_agent::BeforeToolCallResult::default()) })
        });
        session.set_before_tool_call(pi_agent::compose_before_tool_call(
            session.raw_before_tool_call(),
            Some(extension_hook),
        ));

        let before = session.before_tool_call().expect("installed before hook");
        before(before_context("read"))
            .await
            .expect("before hook runs");

        assert_eq!(
            fired_lines(&marker),
            1,
            "host pre_tool_call hook must fire exactly once per tool call after the Application-style rebind"
        );
    }

    /// Detached fork before-hook must carry the fork-time raw policy chain
    /// (extension `reduce_tool_call`) under a frozen host wrapper, so the
    /// reducer still runs after Main replaces host hooks.
    #[tokio::test]
    async fn detached_before_tool_call_includes_raw_extension_reducer() {
        let cwd = tempfile::tempdir().expect("cwd");
        let host_marker = cwd.path().join("fork-host-pre");
        let host_hook = write_marker_hook(cwd.path(), "fork-host-pre.sh", &host_marker);
        let reducer_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let session = test_session_with_discovery(SessionOptions {
            model: Model::default(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: None,
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        });
        let application = publish_test_application(&session, cwd.path()).await;
        session.set_host_hooks(Some(vec![HookConfig {
            event: HookEvent::PreToolCall,
            matcher: None,
            command: vec![host_hook],
            timeout_ms: Some(2_000),
            enabled: None,
            fail_closed: None,
            extra: Map::new(),
        }]));

        let calls = reducer_calls.clone();
        let extension_hook: pi_agent::BeforeToolCallFn = Arc::new(move |_| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Ok(pi_agent::BeforeToolCallResult::default()) })
        });
        session.set_before_tool_call(pi_agent::compose_before_tool_call(
            session.raw_before_tool_call(),
            Some(extension_hook),
        ));

        let fork = application
            .fork_side_chat()
            .await
            .expect("fork side chat");
        let fork_before = fork
            .before_tool_call
            .expect("fork must capture a before hook");

        session.set_host_hooks(None);

        fork_before(before_context("read"))
            .await
            .expect("fork before hook runs");

        assert_eq!(
            fired_lines(&host_marker),
            1,
            "frozen host pre must fire the fork-time snapshot"
        );
        assert_eq!(
            reducer_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "fork before hook must still run the raw extension reducer"
        );
    }
}
// ---------------------------------------------------------------------------
// Detached workspace-escape stop + classification (second-audit GLM wave).
// Detached side-chat/code-review forks recover tool failures against their
// own isolated doom-loop state — never Main's `shared.doom_loop` tracker —
// and workspace-escape classification matches the exact structured prefix,
// so unrelated output that merely mentions the marker stays recoverable.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod detached_workspace_stop_tests {
    use std::fs;

    use pi_ai::providers::{
        FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
    };
    use pi_ai::AssistantMessage;

    use super::*;

    fn faux_session(cwd: &std::path::Path, tag: &str) -> (Session, FauxProviderRegistration) {
        let suffix = Uuid::now_v7().to_string();
        let api = format!("detached-{tag}-api-{suffix}");
        let provider = format!("detached-{tag}-provider-{suffix}");
        let model = Model {
            id: format!("detached-{tag}-model"),
            name: format!("Detached {tag} Model"),
            api: api.clone(),
            provider: provider.clone(),
            ..Model::default()
        };
        let registration = register_faux_provider(FauxProviderOptions {
            api,
            provider,
            models: vec![model.clone()],
            chunk_size: 1,
        });
        let session = test_session_with_discovery(SessionOptions {
            model,
            cwd: cwd.to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: None,
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        });
        (session, registration)
    }

    fn read_call(id: &str, path: &str) -> FauxResponse {
        FauxResponse {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: id.to_owned(),
                name: "read".to_owned(),
                arguments: serde_json::json!({ "path": path }),
                thought_signature: None,
            })],
            stop_reason: StopReason::ToolUse,
            error_message: None,
        }
    }

    fn bash_call(id: &str, command: &str) -> FauxResponse {
        FauxResponse {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: id.to_owned(),
                name: "bash".to_owned(),
                arguments: serde_json::json!({ "command": command }),
                thought_signature: None,
            })],
            stop_reason: StopReason::ToolUse,
            error_message: None,
        }
    }

    fn settle() -> FauxResponse {
        FauxResponse::text("settled")
    }

    /// After-tool-call context carrying `user_prompts` user messages so the
    /// detached doom-loop turn key (user-message count) can be varied.
    fn detached_after_context(
        user_prompts: usize,
        tool_name: &str,
        arguments: serde_json::Value,
        result: pi_agent::AgentToolResult,
        is_error: bool,
    ) -> pi_agent::AfterToolCallContext {
        let messages = (0..user_prompts)
            .map(|index| {
                Message::User(UserMessage {
                    content: vec![ContentBlock::text(format!("turn {index}"))],
                    timestamp: 0,
                })
            })
            .collect::<Vec<_>>();
        pi_agent::AfterToolCallContext {
            assistant_message: AssistantMessage::pending(&Model::default()),
            tool_call: ToolCall {
                id: "call-1".to_owned(),
                name: tool_name.to_owned(),
                arguments: arguments.clone(),
                thought_signature: None,
            },
            arguments,
            result,
            is_error,
            context: pi_agent::AgentContext {
                system_prompt: String::new(),
                messages,
                tools: Vec::new(),
            },
        }
    }

    fn result_text(result: &pi_agent::AfterToolCallResult) -> String {
        result
            .content
            .as_ref()
            .into_iter()
            .flatten()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn tool_call_names(messages: &[Message]) -> Vec<String> {
        messages
            .iter()
            .filter_map(|message| match message {
                Message::Assistant(assistant) => Some(
                    assistant
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::ToolCall(call) => Some(call.name.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .flatten()
            .collect()
    }

    fn tool_result_texts(messages: &[Message]) -> Vec<String> {
        messages
            .iter()
            .filter_map(|message| match message {
                Message::ToolResult(result) => Some(
                    result
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                _ => None,
            })
            .collect()
    }

    fn assistant_texts(messages: &[Message]) -> Vec<String> {
        messages
            .iter()
            .filter_map(|message| match message {
                Message::Assistant(assistant) => Some(
                    assistant
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                _ => None,
            })
            .collect()
    }

    /// A detached fork's read of a path outside the workspace stops the turn
    /// on the FIRST workspace-escape rejection. The queued follow-up model
    /// responses (a bash call and a settle) are tripwires that must stay
    /// unconsumed, and Main's doom-loop state must remain untouched — the
    /// fork's isolated tracker never poisons Main.
    /// The session publishes a confined `workspace-write` sandbox so the
    /// escape exercises the executor's `PolicyFs` capability rejection
    /// (`FsError::OutsideCapability`), the only escape rejection `read` can
    /// produce under the current authority design.
    #[tokio::test]
    async fn detached_read_outside_stops_on_the_first_error_and_never_poisons_main() {
        let cwd = tempfile::tempdir().expect("cwd");
        let outside = tempfile::tempdir().expect("outside");
        let secret = outside.path().join("secret.txt");
        fs::write(&secret, "secret").expect("write outside file");
        let (session, registration) = faux_session(cwd.path(), "detached-escape");
        let application =
            publish_test_application_sandboxed(&session, cwd.path(), "workspace-write").await;

        let (fork, tools) = application
            .fork_side_chat_with_active_tools()
            .await
            .expect("fork side chat");
        let agent = crate::Application::create_side_chat_agent(&fork, tools);
        registration.set_responses(vec![
            read_call("escape-1", secret.to_string_lossy().as_ref()),
            // Tripwires: consumed only if the first workspace escape fails to
            // terminate the detached turn.
            bash_call("tripwire-bash", "printf 'tripwire-ran'"),
            settle(),
        ]);

        agent
            .prompt("read the secret")
            .await
            .expect("the detached turn must settle at the workspace-escape stop");

        let messages = agent.state().await.messages;
        let texts = tool_result_texts(&messages);
        let stop = texts
            .iter()
            .find(|text| text.contains("path_outside_workspace"))
            .expect("the failing read must carry the workspace-escape stop");
        assert!(
            stop.contains("/read") && stop.contains("--add-dir <project-root>"),
            "the stop message must name the host-level remedy, got: {stop}"
        );
        assert!(
            !stop.contains(outside.path().to_string_lossy().as_ref())
                && !stop.contains("secret.txt"),
            "the stop must never echo the rejected location, got: {stop}"
        );
        assert!(
            tool_call_names(&messages).iter().all(|name| name != "bash"),
            "the bash tripwire must never execute after the first-escape stop: {:?}",
            tool_call_names(&messages)
        );
        assert!(
            !assistant_texts(&messages)
                .iter()
                .any(|text| text.contains("settled")),
            "the settle tripwire must stay unconsumed, got: {:?}",
            assistant_texts(&messages)
        );

        // The fork's stop never touches Main's doom-loop state.
        {
            let tracker = session.inner.shared.doom_loop.lock();
            assert!(
                tracker.triggered_message.is_none() && tracker.current.is_none(),
                "the fork's workspace-escape stop must not pin Main's doom-loop tracker"
            );
        }
        // …and Main still runs and settles normally afterwards.
        registration.set_responses(vec![settle()]);
        let main_run = session
            .run("main turn after the fork stop", Vec::new())
            .await
            .expect("Main must be unaffected by the fork's doom loop");
        assert!(
            main_run.text.contains("settled"),
            "{}",
            main_run.text
        );
        registration.unregister();
    }

    /// Hook-level pin: once the detached fork's first workspace escape stops
    /// the turn, a same-batch bash sibling — even a successful one — also
    /// terminates with the pinned stop message, so the batch cannot escape
    /// the stop via a different tool.
    #[tokio::test]
    async fn detached_bash_tripwire_terminates_with_the_pinned_stop() {
        let cwd = tempfile::tempdir().expect("cwd");
        let (session, registration) = faux_session(cwd.path(), "detached-tripwire");
        let application = publish_test_application(&session, cwd.path()).await;
        let (fork, _tools) = application
            .fork_side_chat_with_active_tools()
            .await
            .expect("fork side chat");
        let fork_after = fork
            .after_tool_call
            .expect("fork must capture an after hook");
        let escape = pi_agent::AgentToolResult::text(
            "path_outside_workspace: <absolute path> is outside authorized workspace roots",
        );

        let first = fork_after(detached_after_context(
            1,
            "read",
            serde_json::json!({ "path": "/etc/shadow" }),
            escape,
            true,
        ))
        .await
        .expect("first workspace-escape outcome");
        assert_eq!(
            first.terminate,
            Some(true),
            "the detached fork must stop on the FIRST workspace escape"
        );

        let tripwire = fork_after(detached_after_context(
            1,
            "bash",
            serde_json::json!({ "command": "cat /etc/shadow" }),
            pi_agent::AgentToolResult::text("contents"),
            false,
        ))
        .await
        .expect("bash tripwire outcome");
        assert_eq!(
            tripwire.terminate,
            Some(true),
            "the same-batch bash tripwire must terminate with the stop"
        );
        assert_eq!(
            result_text(&tripwire),
            result_text(&first),
            "the tripwire must consume the pinned stop message"
        );

        let tracker = session.inner.shared.doom_loop.lock();
        assert!(
            tracker.triggered_message.is_none() && tracker.current.is_none(),
            "the detached stop must never pin Main's doom-loop tracker"
        );
        drop(tracker);
        registration.unregister();
    }

    /// The fork's isolated doom-loop state is per-turn: after a turn ended at
    /// the workspace-escape stop, the NEXT detached turn starts with clean
    /// state instead of dying on the previous turn's pinned stop.
    #[tokio::test]
    async fn detached_doom_loop_state_resets_when_the_next_turn_starts() {
        let cwd = tempfile::tempdir().expect("cwd");
        let (session, registration) = faux_session(cwd.path(), "detached-reset");
        let application = publish_test_application(&session, cwd.path()).await;
        let (fork, _tools) = application
            .fork_side_chat_with_active_tools()
            .await
            .expect("fork side chat");
        let fork_after = fork
            .after_tool_call
            .expect("fork must capture an after hook");

        let first = fork_after(detached_after_context(
            1,
            "read",
            serde_json::json!({ "path": "/etc/shadow" }),
            pi_agent::AgentToolResult::text(
                "path_outside_workspace: <absolute path> is outside authorized workspace roots",
            ),
            true,
        ))
        .await
        .expect("first turn's workspace-escape outcome");
        assert_eq!(first.terminate, Some(true));

        // The next detached turn (a new user prompt) must observe clean
        // doom-loop state: a successful read neither terminates nor carries
        // the previous turn's pinned message.
        let fresh = fork_after(detached_after_context(
            2,
            "read",
            serde_json::json!({ "path": "notes.txt" }),
            pi_agent::AgentToolResult::text("hello world"),
            false,
        ))
        .await
        .expect("next turn's first tool outcome");
        assert_ne!(
            fresh.terminate,
            Some(true),
            "a new detached turn must not inherit the previous turn's stop"
        );
        assert!(
            result_text(&fresh).is_empty(),
            "a fresh turn's successful tool result must not be replaced, got: {}",
            result_text(&fresh)
        );
        registration.unregister();
    }

    /// Workspace-escape classification matches the exact structured prefix
    /// (and the legacy phrase, narrowly start-anchored). Unrelated error text
    /// that merely CONTAINS the marker — docs, notes, echoed prior stops —
    /// stays recoverable: the first-error stop must never fire for it.
    #[test]
    fn workspace_escape_classification_requires_the_anchored_form() {
        let structured =
            "path_outside_workspace: <absolute path> is outside authorized workspace roots; use a repo-relative path inside the current workspace/additional roots, or relaunch/select the project with --add-dir <project-root>";
        assert_eq!(
            classify_tool_error(structured),
            ToolErrorClass::WorkspaceEscape,
            "the exact structured rejection must classify as a workspace escape"
        );
        assert_eq!(
            classify_tool_error("path_outside_workspace: <absolute path> is outside authorized workspace roots"),
            ToolErrorClass::WorkspaceEscape
        );
        assert_eq!(
            classify_tool_error("path escapes workspace: /etc/shadow"),
            ToolErrorClass::WorkspaceEscape,
            "the legacy free-form rejection must stay classified when it leads the message"
        );
        assert_eq!(
            classify_tool_error("path escapes workspace"),
            ToolErrorClass::WorkspaceEscape
        );
        // Confined-authority rejection (executor PolicyFs, workspace-write).
        assert_eq!(
            classify_tool_error(
                "filesystem capability does not cover the requested path; add it with --add-dir or sandbox.readOnlyRoots"
            ),
            ToolErrorClass::WorkspaceEscape,
            "the executor's capability rejection must classify as a workspace escape"
        );
        // A prose mention of the capability phrase elsewhere stays recoverable.
        assert_eq!(
            classify_tool_error(
                "the docs explain when a filesystem capability does not cover a request"
            ),
            ToolErrorClass::Recoverable
        );
        assert_eq!(
            classify_tool_error(
                "filesystem capability does not cover this URI and the scheme is unsupported"
            ),
            ToolErrorClass::Recoverable,
            "a stem-similar non-executor rejection must stay recoverable"
        );

    }

    /// Direct classification table: the structured prefix and the
    /// start-anchored legacy phrase classify as workspace escapes; substring
    /// mentions and non-structured forms stay recoverable.
    #[test]
    fn workspace_escape_classification_negative_cases() {
        assert_eq!(
            classify_tool_error(
                "the previous turn's read failed with path_outside_workspace: /etc/shadow, but this call recovered"
            ),
            ToolErrorClass::Recoverable,
            "an unrelated error that merely mentions the marker must stay recoverable"
        );
        assert_eq!(
            classify_tool_error(
                "see path_outside_workspace: in the docs for how rejections are reported"
            ),
            ToolErrorClass::Recoverable
        );
        assert_eq!(
            classify_tool_error(
                "the handbook explains when path escapes workspace rejections are reported"
            ),
            ToolErrorClass::Recoverable
        );
        assert_eq!(
            classify_tool_error("path_outside_workspace"),
            ToolErrorClass::Recoverable,
            "the bare marker without the structured `:` form is not the executor's rejection"
        );
        // Deterministic classification is unaffected by the tightening.
        assert_eq!(
            classify_tool_error("invalid bash arguments: missing required field command"),
            ToolErrorClass::Deterministic
        );
        assert_eq!(classify_tool_error("connection refused"), ToolErrorClass::Recoverable);
    }

}
