use crate::code_review::{
    DiffFile, DiffHunk, DiffLineKind, FileContextBlobs, FileDiff, FileDiffPage, HunkIdentity,
    MAX_CONTEXT_EXPAND_LINES, MAX_FILE_PAGE_LINES, MAX_FILE_RENDER_LINES, ReviewPagingState,
    ReviewScope, ReviewSnapshot, load_review_snapshot_capture,
};
use crate::code_review_panel::{
    AutoReviewStart, CodeReviewController, CommentSubmitResult, HunkReviewResult, MAX_COMMENT_BYTES,
    MAX_PENDING_COMMENTS, ReviewCommentRole, ReviewStopOperation, fork_model_display,
};
use crate::{
    args::Cli,
    extension_ui::{ExtensionUiAdapter, ExtensionUiEvent},
    modes::json::write_json_line,
    session_run,
};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use pi_agent::{QueueMode, ThinkingLevel};
use pi_ai::{ContentBlock, Message, Model};
use pi_coding::{
    Application, ApplicationEvent, ApplicationState, GoalState, GoalUsageDelta, LoopCreateRequest,
    LoopUpdateRequest, ProcessId, ProcessKey, ProcessSignal, ProcessSpawnSpec, ProcessTerminalSize,
    SessionAlreadyProcessing, StreamingBehavior, TodoChildAddTaskRequest, TodoChildCreateRequest,
    TodoOp, TodoPhase, TranscriptUnavailable, TranscriptRow, validate_review_language,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{self, Write},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex as StdMutex},
};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{Semaphore, mpsc};
pub(crate) const MAX_RPC_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_WS_MESSAGE_BYTES: usize = 24 * 1024 * 1024;
pub(crate) const MAX_WS_OUTBOUND_MESSAGE_BYTES: usize = 240 * 1024 * 1024;
pub(crate) const MAX_FORK_MESSAGES_RESPONSE_BYTES: usize = 256 * 1024;

/// Exact live-runtime identity. Continuity, subscribe state, and unload
/// matching use both components; neither is a capability.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuntimeIdentity {
    pub session_id: String,
    pub runtime_incarnation: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimeUnloadReason {
    Lru,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum SubscriptionCutReason {
    ColdFull,
    WarmContinuity,
    Resync,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResyncRequiredReason {
    Overflow,
    Gap,
    Lag,
    Budget,
}

impl RuntimeIdentity {
    pub(crate) fn new(session_id: impl Into<String>, runtime_incarnation: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            runtime_incarnation: runtime_incarnation.into(),
        }
    }
}

/// Recursive size walk for replay/window admission. Avoids a second
/// `to_vec` on every live event; completed snapshots and outbound frames
/// still charge `serde_json::to_string` length.
pub(crate) fn estimate_value_bytes(value: &Value) -> usize {
    match value {
        Value::Null | Value::Bool(_) => 8,
        Value::Number(_) => 16,
        Value::String(text) => text.len().saturating_add(8),
        Value::Array(items) => items
            .iter()
            .map(estimate_value_bytes)
            .fold(8usize, usize::saturating_add),
        Value::Object(map) => map.iter().fold(8usize, |total, (key, child)| {
            total
                .saturating_add(key.len())
                .saturating_add(estimate_value_bytes(child))
        }),
    }
}

const JSONL_CHANNEL_CAPACITY: usize = 8;
pub(crate) const MAX_CONCURRENT_COMMANDS: usize = 16;
/// Default transcript lines for `agent_history` when `lines` is omitted.
/// Bounded by the core `pi_coding::MAX_HISTORY_LINES` cap.
const DEFAULT_RPC_HISTORY_LINES: usize = 80;

/// Deserialize an OPTIONAL sequence field leniently for legacy/foreign/null
/// payloads: an explicit `null` (a common client shape that always carries
/// the key) decodes to the same empty `Vec` as an absent field, a real array
/// decodes normally, and any non-array scalar/object is rejected — so a
/// required-array contract is never silently widened. Apply ONLY to fields
/// that already carry `#[serde(default)]` (absent ⇒ empty); required arrays
/// keep plain `Vec` deserialization so `null` stays a hard error there.
fn null_default_vec<'de, D, T>(deserializer: D) -> std::result::Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}
/// Deserialize an OPTIONAL scalar field fail-closed about explicit `null`:
/// a missing field (via `#[serde(default)]`) stays `None`, a real value
/// decodes to `Some`, and an explicit `null` is a hard parse error instead
/// of silently collapsing into "absent". Use where `null` and absence must
/// stay distinct (e.g. an operand whose presence selects a review scope).
fn non_null_option<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Ok(Some(T::deserialize(deserializer)?))
}
/// Strict `conversationId` shape: a non-UUID is a malformed request, rejected at parse time.
fn deserialize_conversation_id<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    if is_uuid(&raw) {
        Ok(raw)
    } else {
        Err(serde::de::Error::custom(format!(
            "conversationId must be a UUID, got {raw:?}"
        )))
    }
}
fn selection_preserving_null<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(Value::deserialize(deserializer)?))
}

/// Canonical 8-4-4-4-12 hexadecimal UUID shape (any case).
fn is_uuid(value: &str) -> bool {
    let mut parts = value.split('-');
    for expected_len in [8, 4, 4, 4, 12] {
        match parts.next() {
            Some(part)
                if part.len() == expected_len
                    && part.chars().all(|c| c.is_ascii_hexdigit()) => {}
            _ => return false,
        }
    }
    parts.next().is_none()
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub enum SubscribeHandoffV1 {
    #[serde(rename = "subscribe-handoff-v1")]
    SubscribeHandoffV1,
}

/// Canonical source of a negotiated switch's clicked catalog row. Wire values
/// arrive from `session_list` rows, so the historical labels (`native`,
/// `primary`, `grok-hyper`, `grok/hyper`) normalize onto the strict set the
/// Web control plane uses: `rpi | pi | omp | codex | grok`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CatalogSwitchSource {
    Rpi,
    Pi,
    Omp,
    Codex,
    Grok,
}

impl CatalogSwitchSource {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "rpi" | "native" | "primary" | "nativepi" | "native_pi" => Some(Self::Rpi),
            "pi" => Some(Self::Pi),
            "omp" => Some(Self::Omp),
            "codex" => Some(Self::Codex),
            "grok" | "grok-hyper" | "grok/hyper" | "hyper" => Some(Self::Grok),
            _ => None,
        }
    }

    /// Catalog authority the tuple resolves against.
    pub(crate) const fn session_kind(self) -> pi_coding::SessionSourceKind {
        match self {
            Self::Rpi => pi_coding::SessionSourceKind::Rpi,
            Self::Pi => pi_coding::SessionSourceKind::Pi,
            Self::Omp => pi_coding::SessionSourceKind::Omp,
            Self::Codex => pi_coding::SessionSourceKind::Codex,
            Self::Grok => pi_coding::SessionSourceKind::Grok,
        }
    }

    /// Strict wire form echoed back in the handoff.
    pub(crate) const fn wire(self) -> &'static str {
        match self {
            Self::Rpi => "rpi",
            Self::Pi => "pi",
            Self::Omp => "omp",
            Self::Codex => "codex",
            Self::Grok => "grok",
        }
    }
}

/// Native/foreign classification of the clicked catalog row, matching the
/// `session_list` wire projection (`status.is_native()`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CatalogSwitchStatus {
    Native,
    Foreign,
}

impl CatalogSwitchStatus {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "native" => Some(Self::Native),
            "foreign" => Some(Self::Foreign),
            _ => None,
        }
    }

    pub(crate) const fn wire(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Foreign => "foreign",
        }
    }
}

/// Validated catalog identity of a negotiated Web session switch. The three
/// wire fields (`catalogSource`, `catalogSessionId`, `catalogStatus`) travel
/// together or not at all; construction is the only way in, so every holder
/// carries a normalized, fully-populated tuple. It is request-scoped: it is
/// carried alongside the runtime to the handoff response, never stored.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CatalogSwitchIdentity {
    source: CatalogSwitchSource,
    session_id: String,
    status: CatalogSwitchStatus,
}

impl CatalogSwitchIdentity {
    /// Strict construction: both enums parse from their wire vocabularies and
    /// the session id must be non-empty.
    pub(crate) fn parse(
        source: &str,
        session_id: &str,
        status: &str,
    ) -> Option<CatalogSwitchIdentity> {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return None;
        }
        Some(Self {
            source: CatalogSwitchSource::parse(source)?,
            session_id: session_id.to_owned(),
            status: CatalogSwitchStatus::parse(status)?,
        })
    }

    pub(crate) const fn source(&self) -> CatalogSwitchSource {
        self.source
    }

    pub(crate) const fn status(&self) -> CatalogSwitchStatus {
        self.status
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Native rows must resolve to the exact catalog session id on the opened
    /// runtime; foreign rows import into a distinct native identity.
    pub(crate) const fn is_native(&self) -> bool {
        matches!(self.status, CatalogSwitchStatus::Native)
    }
}

/// Why a `switch_session` catalog tuple failed validation. The message is the
/// entire user-visible text: bounded, fixed, and free of any echoed input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SwitchCatalogIdentityError {
    /// Exactly one or two of the three negotiated fields were provided.
    Incomplete,
    /// A provided field failed strict validation or the id was empty.
    Invalid,
}

impl SwitchCatalogIdentityError {
    pub(crate) const fn message(self) -> &'static str {
        match self {
            Self::Incomplete => {
                "catalogSource, catalogSessionId, and catalogStatus must be provided together"
            }
            Self::Invalid => "catalog identity is invalid",
        }
    }

    /// Stable machine code attached to negotiated switch failures.
    pub(crate) const fn code(self) -> &'static str {
        "invalid_request"
    }
}

/// Validated catalog tuple of a `switch_session`, when the client sent one.
/// All three fields are required together; absent keeps the legacy path-only
/// internal flow. Wire aliases (`native`, `primary`, `grok-hyper`,
/// `grok/hyper`) normalize here so the backend and the echoed handoff always
/// use the strict canonical set.
pub(crate) fn switch_catalog_identity(
    catalog_source: Option<&str>,
    catalog_session_id: Option<&str>,
    catalog_status: Option<&str>,
) -> Result<Option<CatalogSwitchIdentity>, SwitchCatalogIdentityError> {
    match (catalog_source, catalog_session_id, catalog_status) {
        (None, None, None) => Ok(None),
        (Some(source), Some(session_id), Some(status)) => CatalogSwitchIdentity::parse(
            source,
            session_id,
            status,
        )
        .map(Some)
        .ok_or(SwitchCatalogIdentityError::Invalid),
        _ => Err(SwitchCatalogIdentityError::Incomplete),
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RpcCommand {
    Prompt {
        #[serde(default)]
        id: Option<String>,
        message: String,
        #[serde(default, deserialize_with = "null_default_vec")]
        images: Vec<ContentBlock>,
        #[serde(default, rename = "streamingBehavior")]
        streaming_behavior: Option<StreamingBehavior>,
    },
    Steer {
        #[serde(default)]
        id: Option<String>,
        message: String,
        #[serde(default, deserialize_with = "null_default_vec")]
        images: Vec<ContentBlock>,
    },
    FollowUp {
        #[serde(default)]
        id: Option<String>,
        message: String,
        #[serde(default, deserialize_with = "null_default_vec")]
        images: Vec<ContentBlock>,
    },
    Abort {
        #[serde(default)]
        id: Option<String>,
    },
    NewSession {
        #[serde(default)]
        id: Option<String>,
        #[serde(default, rename = "parentSession")]
        parent_session: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
    },
    GetState {
        #[serde(default)]
        id: Option<String>,
    },
    SetModel {
        #[serde(default)]
        id: Option<String>,
        provider: String,
        #[serde(rename = "modelId")]
        model_id: String,
    },
    CycleModel {
        #[serde(default)]
        id: Option<String>,
    },
    GetAvailableModels {
        #[serde(default)]
        id: Option<String>,
    },
    SetThinkingLevel {
        #[serde(default)]
        id: Option<String>,
        level: ThinkingLevel,
    },
    CycleThinkingLevel {
        #[serde(default)]
        id: Option<String>,
    },
    GetAvailableThinkingLevels {
        #[serde(default)]
        id: Option<String>,
    },
    SetSteeringMode {
        #[serde(default)]
        id: Option<String>,
        mode: QueueMode,
    },
    SetFollowUpMode {
        #[serde(default)]
        id: Option<String>,
        mode: QueueMode,
    },
    Compact {
        #[serde(default)]
        id: Option<String>,
        #[serde(default, rename = "customInstructions")]
        custom_instructions: Option<String>,
    },
    SetAutoCompaction {
        #[serde(default)]
        id: Option<String>,
        enabled: bool,
    },
    SetAutoRetry {
        #[serde(default)]
        id: Option<String>,
        enabled: bool,
    },
    AbortRetry {
        #[serde(default)]
        id: Option<String>,
    },
    Bash {
        #[serde(default)]
        id: Option<String>,
        command: String,
        #[serde(default, rename = "excludeFromContext")]
        exclude_from_context: Option<bool>,
    },
    AbortBash {
        #[serde(default)]
        id: Option<String>,
    },
    GetSessionStats {
        #[serde(default)]
        id: Option<String>,
    },
    /// List the unified resume catalog (native + enabled foreign sessions).
    /// `scope` defaults to `current`; `all_projects` removes cwd filtering and,
    /// for the Web listener, scans either the active profile's default native
    /// tree or the exact configured custom root.
    /// Wire shape: `{ "type": "session_list", "id"?: string, "scope"?: "current"|"all_projects" }`.
    SessionList {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        scope: RpcSessionListScope,
    },
    ExportHtml {
        #[serde(default)]
        id: Option<String>,
        #[serde(default, rename = "outputPath")]
        output_path: Option<String>,
        #[serde(default, rename = "sessionPath")]
        session_path: Option<String>,
    },
    SwitchSession {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "sessionPath")]
        session_path: String,
        /// Negotiated catalog identity of the clicked row
        /// (`responseMode: "subscribe-handoff-v1"`). All three fields are
        /// required together and validated against the exact current catalog
        /// row before any import or spawn; absent keeps the legacy path-only
        /// flow.
        #[serde(default, rename = "catalogSource")]
        catalog_source: Option<String>,
        #[serde(default, rename = "catalogSessionId")]
        catalog_session_id: Option<String>,
        #[serde(default, rename = "catalogStatus")]
        catalog_status: Option<String>,
        /// Explicit opt-in for the lightweight Web subscribe handoff. Missing
        /// preserves the legacy full snapshot on every transport. Request JSON:
        /// `{"type":"switch_session","sessionPath":"…","responseMode":"subscribe-handoff-v1"}`.
        #[serde(default, rename = "responseMode")]
        response_mode: Option<SubscribeHandoffV1>,
    },
    /// OMP-parity `session.handoff()`: summarize the CURRENT session into a
    /// handoff document, create a NEW session in the SAME cwd whose first
    /// record is the handoff message, and answer with the same
    /// subscribe-handoff-v1 switch envelope the web switch consumes (the web
    /// switches like a catalog switch and renders the handoff divider).
    /// Request JSON:
    /// `{"type":"session_handoff","customInstructions":"…","responseMode":"subscribe-handoff-v1"}`.
    SessionHandoff {
        #[serde(default)]
        id: Option<String>,
        /// Operator instructions carried verbatim into the handoff document.
        #[serde(default, rename = "customInstructions")]
        custom_instructions: Option<String>,
        #[serde(default, rename = "responseMode")]
        response_mode: Option<SubscribeHandoffV1>,
    },
    Fork {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "entryId")]
        entry_id: String,
        #[serde(default, rename = "sessionPath")]
        session_path: Option<String>,
    },
    Clone {
        #[serde(default)]
        id: Option<String>,
        #[serde(default, rename = "sessionPath")]
        session_path: Option<String>,
    },
    GetForkMessages {
        #[serde(default)]
        id: Option<String>,
        #[serde(default, rename = "sessionPath")]
        session_path: Option<String>,
        #[serde(default)]
        cursor: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    GetEntries {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        since: Option<String>,
    },
    GetTree {
        #[serde(default)]
        id: Option<String>,
    },
    GetLastAssistantText {
        #[serde(default)]
        id: Option<String>,
    },
    SetSessionName {
        #[serde(default)]
        id: Option<String>,
        name: String,
        #[serde(default, rename = "sessionPath")]
        session_path: Option<String>,
    },
    /// Fork the latest nonempty user message of a saved session by path.
    /// Wire: `{ "type": "session_fork_latest", "id"?: string, "sessionPath": string }`.
    SessionForkLatest {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "sessionPath")]
        session_path: String,
    },
    /// Archive a saved native session. Rejects the current session and busy
    /// loaded secondaries; idle loaded secondaries are closed then archived.
    /// Wire: `{ "type": "session_delete", "id"?: string, "sessionPath": string }`.
    SessionDelete {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "sessionPath")]
        session_path: String,
    },
    /// Restore a session archived by `session_delete` within its undo window.
    /// Wire: `{ "type": "session_restore", "id"?: string, "undoToken": string }`.
    SessionRestore {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "undoToken")]
        undo_token: String,
    },
    /// Render a saved session's full context as a bounded, sanitized,
    /// pasteable text artifact. Manager-level on the Web control plane; the
    /// selected session is prepared read-only and never activated.
    /// Wire: `{ "type": "session_copy_context", "id"?: string, "sessionPath": string }`.
    SessionCopyContext {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "sessionPath")]
        session_path: String,
    },
    /// Wire: `{ "type": "session_move", "id"?: string, "sessionPath": string, "targetCwd": string }`.
    SessionMove {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "sessionPath")]
        session_path: String,
        #[serde(rename = "targetCwd")]
        target_cwd: String,
    },
    GetMessages {
        #[serde(default)]
        id: Option<String>,
    },
    GetCommands {
        #[serde(default)]
        id: Option<String>,
    },
    SetTodos {
        #[serde(default)]
        id: Option<String>,
        #[serde(default, rename = "workflowId")]
        workflow_id: Option<String>,
        phases: Vec<TodoPhase>,
    },
    TodoOp {
        #[serde(default)]
        id: Option<String>,
        #[serde(flatten)]
        op: TodoOp,
    },
    LoopCreate {
        #[serde(default)]
        id: Option<String>,
        #[serde(flatten)]
        request: LoopCreateRequest,
    },
    LoopUpdate {
        #[serde(default)]
        id: Option<String>,
        #[serde(flatten)]
        request: LoopUpdateRequest,
    },
    LoopList {
        #[serde(default)]
        id: Option<String>,
    },
    LoopDelete {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "taskId")]
        task_id: String,
    },
    LoopCancel {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "taskId")]
        task_id: String,
    },
    ProcessSpawn {
        #[serde(default)]
        id: Option<String>,
        spec: ProcessSpawnSpec,
    },
    ProcessList {
        #[serde(default)]
        id: Option<String>,
    },
    ProcessDescribe {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "processId")]
        process_id: ProcessId,
    },
    ProcessLogs {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "processId")]
        process_id: ProcessId,
        #[serde(default)]
        cursor: Option<u64>,
        #[serde(default, rename = "limitBytes")]
        limit_bytes: Option<usize>,
    },
    ProcessWrite {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "processId")]
        process_id: ProcessId,
        #[serde(rename = "dataBase64")]
        data_base64: String,
    },
    ProcessKeys {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "processId")]
        process_id: ProcessId,
        keys: Vec<ProcessKey>,
    },
    ProcessResize {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "processId")]
        process_id: ProcessId,
        cols: u16,
        rows: u16,
    },
    ProcessSignal {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "processId")]
        process_id: ProcessId,
        signal: ProcessSignal,
    },
    ProcessStop {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "processId")]
        process_id: ProcessId,
    },
    ProcessWait {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "processId")]
        process_id: ProcessId,
        #[serde(default, rename = "timeoutMs")]
        timeout_ms: Option<u64>,
    },
    GoalCreate {
        #[serde(default)]
        id: Option<String>,
        objective: String,
        #[serde(default, rename = "tokenBudget")]
        token_budget: Option<u64>,
        /// TUI parity switch: when true, mirrors `/goal create` in
        /// goal_commands.rs — create the goal AND start (or queue) its first
        /// work turn (`activate_goal`), resolving with the activation outcome
        /// instead of the goal. Default false keeps the mutation-only
        /// contract the Web Goal panel's form uses (it never starts work).
        #[serde(default)]
        activate: bool,
    },
    GoalGet {
        #[serde(default)]
        id: Option<String>,
    },
    GoalPause {
        #[serde(default)]
        id: Option<String>,
    },
    GoalResume {
        #[serde(default)]
        id: Option<String>,
        /// TUI parity switch: when true, mirrors `/goal resume` in
        /// goal_commands.rs — resume the paused goal AND schedule exactly one
        /// continuation turn (`resume_goal_work`), resolving with the
        /// activation outcome instead of the goal. Default false keeps the
        /// mutation-only contract the Web Goal panel's Resume button uses.
        #[serde(default)]
        activate: bool,
    },
    GoalComplete {
        #[serde(default)]
        id: Option<String>,
    },
    GoalDrop {
        #[serde(default)]
        id: Option<String>,
    },
    GoalUpdateUsage {
        #[serde(default)]
        id: Option<String>,
        tokens: u64,
        #[serde(default, rename = "activeTimeSeconds")]
        active_time_seconds: u64,
    },
    GoalPin {
        #[serde(default)]
        id: Option<String>,
        text: String,
    },
    GoalUnpin {
        #[serde(default)]
        id: Option<String>,
        index: usize,
    },
    /// Replay the goal journal (every goal event on the active session branch).
    /// Wire shape: `{ "type": "goal_journal", "id"?: string }`.
    GoalJournal {
        #[serde(default)]
        id: Option<String>,
    },
    /// Return the redacted schema catalog, effective values, provenance, and paths.
    /// Wire shape: `{ "type": "settings_inspect", "id"?: string }`.
    SettingsInspect {
        #[serde(default)]
        id: Option<String>,
    },
    /// Search schema keys, descriptions, and categories.
    /// Wire shape: `{ "type": "settings_search", "query": string, "id"?: string }`.
    SettingsSearch {
        #[serde(default)]
        id: Option<String>,
        query: String,
    },
    /// Open a server-held atomic draft in `global` or `project` scope.
    /// Returns a generated `draftId` used by all mutating commands.
    SettingsOpenDraft {
        #[serde(default)]
        id: Option<String>,
        scope: pi_coding::SettingsScope,
    },
    SettingsGetDraft {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "draftId")]
        draft_id: String,
    },
    SettingsSet {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "draftId")]
        draft_id: String,
        key: String,
        value: Value,
    },
    SettingsReset {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "draftId")]
        draft_id: String,
        key: String,
    },
    SettingsValidate {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "draftId")]
        draft_id: String,
    },
    SettingsApply {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "draftId")]
        draft_id: String,
    },
    SettingsCancel {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "draftId")]
        draft_id: String,
    },
    WorkflowCreate {
        #[serde(default)]
        id: Option<String>,
        name: String,
        objective: String,
    },
    WorkflowList {
        #[serde(default)]
        id: Option<String>,
    },
    WorkflowGet {
        #[serde(default)]
        id: Option<String>,
        #[serde(default, rename = "workflowId")]
        workflow_id: Option<String>,
        #[serde(default)]
        name: Option<String>,
    },
    WorkflowDetail {
        #[serde(default)]
        id: Option<String>,
        #[serde(default, rename = "workflowId")]
        workflow_id: Option<String>,
        #[serde(default)]
        name: Option<String>,
    },
    WorkflowPause {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "workflowId")]
        workflow_id: String,
    },
    WorkflowResume {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "workflowId")]
        workflow_id: String,
    },
    WorkflowCancel {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "workflowId")]
        workflow_id: String,
    },
    WorkflowIntegrate {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "workflowId")]
        workflow_id: String,
    },
    WorkflowRemove {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "workflowId")]
        workflow_id: String,
    },
    /// Send a message from the workflow child's main agent to a recipient in
    /// that exact workflowId+generation runtime. Wire:
    /// `{ "type": "workflow_hub_send", "id"?: string, "workflowId": string,
    ///    "generation": number, "recipient": string, "body": string, "replyTo"?: string }`.
    WorkflowHubSend {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "workflowId")]
        workflow_id: String,
        generation: u64,
        recipient: String,
        body: String,
        #[serde(default, rename = "replyTo")]
        reply_to: Option<String>,
    },
    /// Open a new named side-chat tab (the `/btw new <name>` mirror). Each
    /// tab is a detached parallel agent forked from the main conversation.
    /// Returns the full side-chat snapshot.
    SideChatNew {
        #[serde(default)]
        id: Option<String>,
        name: String,
    },
    /// Switch the active side-chat tab by name.
    SideChatSwitch {
        #[serde(default)]
        id: Option<String>,
        name: String,
    },
    /// Close a side-chat tab by name (defaults to the active tab). Closing
    /// the last tab replaces it with a fresh `default` tab.
    SideChatClose {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        name: Option<String>,
    },
    /// Submit a prompt to the active side-chat tab. Never touches the main
    /// session/transcript.
    SideChatPrompt {
        #[serde(default)]
        id: Option<String>,
        message: String,
        #[serde(default, deserialize_with = "null_default_vec")]
        images: Vec<ContentBlock>,
    },
    /// Snapshot every side-chat tab: names, streaming state, and per-tab
    /// transcript rows.
    SideChatList {
        #[serde(default)]
        id: Option<String>,
    },
    SideChatAbort {
        #[serde(default)]
        id: Option<String>,
    },
    /// Deterministic archive compaction with no LLM call (`/snapcompact`).
    /// Returns the same A→B token report as `compact`.
    #[serde(rename = "snapcompact")]
    SnapCompact {
        #[serde(default)]
        id: Option<String>,
    },
    /// Roll the session back to before an entry (`/rewind <entry-index>` or
    /// `/rewind <checkpoint-name>`); the dropped tail is archived to a
    /// `.rewind-*.jsonl` sidecar. List rewind targets via `get_entries`.
    Rewind {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        index: Option<usize>,
        #[serde(default)]
        checkpoint: Option<String>,
    },
    /// Render the `/handoff` envelope (`prose: false`, default) or the
    /// envelope plus one bounded summarizer paragraph (`prose: true`).
    Handoff {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        prose: bool,
    },
    /// View queued steering/follow-up prompts (`/queue`).
    QueueList {
        #[serde(default)]
        id: Option<String>,
    },
    /// Cancel queued prompts. `scope` defaults to all queues (`/queue cancel`).
    /// `steering` drains only the steering tray; `index` removes one steering
    /// row from the current snapshot.
    QueueCancel {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        scope: QueueCancelScope,
        #[serde(default)]
        index: Option<usize>,
    },
    /// Combine remaining steering prompts and inject them immediately.
    QueueSendNow {
        #[serde(default)]
        id: Option<String>,
    },
    /// Snapshot the orchestration runtime for the Subagents panel: jobs,
    /// agents, and delivered messages. `enabled` reports whether the session
    /// has orchestration attached (otherwise the lists are empty).
    JobList {
        #[serde(default)]
        id: Option<String>,
    },
    /// Spawn one or more orchestration child jobs. Wire args mirror the `task`
    /// tool exactly: `{"task": "..."}` for a single spawn, or
    /// `{"context": "...", "tasks": [...]}` for a batch.
    TaskSpawn {
        #[serde(default)]
        id: Option<String>,
        args: Value,
    },
    /// Cancel orchestration jobs by job id or agent id.
    JobCancel {
        #[serde(default)]
        id: Option<String>,
        #[serde(default, rename = "jobIds", deserialize_with = "null_default_vec")]
        job_ids: Vec<String>,
    },
    /// Send a message from the main agent to a subagent (`hub send` mirror).
    HubSend {
        #[serde(default)]
        id: Option<String>,
        to: String,
        body: String,
        #[serde(default, rename = "replyTo")]
        reply_to: Option<String>,
    },
    /// Fetch one orchestration job snapshot (settled `result.output` is the
    /// delivered yield payload).
    JobOutput {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "jobId")]
        job_id: String,
    },
    /// Fetch one child agent's recent transcript (`hub read_history` mirror).
    /// `lines` defaults to `DEFAULT_RPC_HISTORY_LINES` and must lie within
    /// the core `1..=MAX_HISTORY_LINES` bound; the returned `text` is
    /// secret-redacted and byte-capped. Works while the job is queued/running
    /// (live transcript) and after settle. Never exposes filesystem paths.
    /// Wire shape:
    /// `{ "type": "agent_history", "id"?: string, "agentId": string, "lines"?: number }`.
    AgentHistory {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "agentId")]
        agent_id: String,
        #[serde(default)]
        lines: Option<usize>,
    },
    /// Start an encrypted room bound to the selected recorded session.
    CollabStart {
        #[serde(default)]
        id: Option<String>,
        #[serde(default, rename = "baseUrl")]
        base_url: Option<String>,
    },
    /// Inspect active encrypted collaboration rooms.
    CollabStatus {
        #[serde(default)]
        id: Option<String>,
        #[serde(default, rename = "roomId")]
        room_id: Option<String>,
    },
    /// Stop one collaboration room and disconnect its participants.
    CollabStop {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "roomId")]
        room_id: String,
    },
    /// Close an idle manager-owned session on the Web control plane. The
    /// target is selected by the top-level `sessionId`; the primary TUI
    /// runtime and busy secondaries are rejected without cancelling work.
    /// Wire shape: `{ "type": "close_session", "id"?: string, "sessionId": string }`.
    CloseSession {
        #[serde(default)]
        id: Option<String>,
    },
    /// Exchange a WebRTC SDP offer for a Codex Live realtime call by proxying
    /// `POST {realtimeBaseUrl}/v1/realtime/calls` on the backend (avoids CORS
    /// and keeps the API key server-side). The JSON body carries the offer and
    /// frameless session configuration: empty instructions, configured output
    /// voice, and client delegation. `session.model` and `audio.input` remain
    /// absent so the configured proxy can inject its authoritative upstream
    /// model and AVAS accepts the session data. The upstream answer is a bare
    /// SDP body with the call id in the `Location` header. Returns
    /// `{ "sdp": "<answer>", "callId": "<call id>" }` for the browser.
    /// Wire shape: `{ "type": "realtime_create_call", "id"?: string, "sdpOffer": string }`.
    RealtimeCreateCall {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "sdpOffer")]
        sdp_offer: String,
    },
    /// Notify the backend that the Codex Live realtime session ended. The
    /// browser tears down its own `RTCPeerConnection` and `oai-events` data
    /// channel; this command is an acknowledgment so the RPC owner can release
    /// any server-side bookkeeping. Returns `{ "stopped": true }`.
    /// Wire shape: `{ "type": "realtime_stop", "id"?: string }`.
    RealtimeStop {
        #[serde(default)]
        id: Option<String>,
    },
    /// Transcribe a browser-recorded WAV through the backend's configured STT
    /// endpoint. The browser sends ONLY the bounded base64 audio and its MIME
    /// type; the endpoint URL and bearer key are read from the server-held
    /// `live.stt*` settings (never accepted from the client, never exposed to
    /// the frontend). The decoded audio must be a RIFF/WAVE PCM16 stream
    /// within the byte cap and the MIME type must be on the allowlist before
    /// any HTTP request is made. Returns `{ "text": "<transcript>" }`; errors
    /// are bounded and redacted (the STT client scrubs server-echoed secrets).
    /// Wire shape:
    /// `{ "type": "stt_transcribe", "id"?: string, "audioBase64": string, "mimeType": string }`.
    SttTranscribe {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "audioBase64")]
        audio_base64: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    /// Open a bounded Git diff snapshot for code review, forking a review
    /// agent with the owning Main session's exact active tool set and
    /// governed permissions.
    ///
    /// Wire shapes:
    /// - `{ "type": "code_review_open" }` — HEAD→working tree
    /// - `{ "type": "code_review_open", "commit": "<revision>" }` — one
    ///   pinned commit: parent→commit (a root commit diffs from the empty
    ///   tree, a merge from its first parent), resolved once and pinned to
    ///   that hash for later refreshes. Mutually exclusive with `from`,
    ///   `to`, and `includeWorkingTree` (explicit `false` counts as
    ///   supplied); an empty commit is rejected.
    /// - `{ "type": "code_review_open", "from": "<base>", "includeWorkingTree": true }`
    ///   — `<base>`→working tree (committed since base + staged + unstaged +
    ///   bounded untracked). `to` and `commit` must be omitted.
    /// - `{ "type": "code_review_open", "from": "<from>", "to": "<to>" }` —
    ///   exact commit-to-commit comparison. `includeWorkingTree` must be
    ///   absent/false and `commit` must be absent.
    /// A lone `from` without `includeWorkingTree` is rejected so partial
    /// from/to is never overloaded.
    ///
    /// `null` handling is exact and fail-closed for the scope-selecting
    /// operands: an explicit `"commit": null` or
    /// `"includeWorkingTree": null` is a parse-time rejection, never a
    /// silent collapse into "absent". `from`/`to` keep the legacy
    /// null-means-absent baseline: an explicit `null` decodes exactly as
    /// if the operand were omitted (so `{ "commit": "x", "from": null }`
    /// is a pure commit scope).
    /// Returns the full review snapshot plus controller thread/streaming state.
    CodeReviewOpen {
        #[serde(default)]
        id: Option<String>,
        #[serde(default, deserialize_with = "non_null_option")]
        commit: Option<String>,
        #[serde(default)]
        from: Option<String>,
        #[serde(default)]
        to: Option<String>,
        #[serde(default, rename = "includeWorkingTree", deserialize_with = "non_null_option")]
        include_working_tree: Option<bool>,
    },
    /// List recent commits (newest first, first-parent history) for the
    /// `/commit` picker. The optional `limit` (default 100) is clamped by
    /// the core lister to 1..=100. Read-only: no stage, checkout, or
    /// worktree write; the open review session is untouched.
    /// Wire shape: `{ "type": "code_review_commits", "id"?: string, "limit"?: u32 }`.
    /// Response: `{ "commits": [{ "commit": string, "parentCommit": string | null, "parentCount": u32, "subject": string, "author": string, "authorDate": string }], "truncated": bool }`
    /// — `parentCommit` is the first parent (null on a root commit, whose
    /// `parentCount` is 0) and `authorDate` is git's strict ISO 8601 `%aI`
    /// timestamp.
    CodeReviewCommits {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        limit: Option<u32>,
    },
    /// Re-project the currently open review snapshot and controller state.
    /// Errors when no review is open. Wire shape:
    /// `{ "type": "code_review_snapshot", "id"?: string }`.
    CodeReviewSnapshot {
        #[serde(default)]
        id: Option<String>,
    },
    /// Reload the review snapshot (re-run the bounded git acquisition) and
    /// reconcile existing threads onto the new hunk identities.
    /// Wire shape: `{ "type": "code_review_refresh", "id"?: string }`.
    CodeReviewRefresh {
        #[serde(default)]
        id: Option<String>,
    },
    /// Attach a comment to a specific hunk in the current snapshot. The
    /// full hunk identity (`snapshotId`, `path`, old/new ranges, and
    /// `contentHash`) must match the current snapshot exactly, else the
    /// comment is rejected as stale. Optional `images` are image content
    /// blocks (max 8; mime image/png|jpeg|webp|gif; 2 MiB decoded per image;
    /// 3.5 MiB total wire) validated before the comment is accepted; the
    /// blocks ride the review prompt before its text block.
    /// Wire shape:
    /// `{ "type": "code_review_comment", "id"?: string, "snapshotId": string, "path": string, "oldStart": u32, "oldCount": u32, "newStart": u32, "newCount": u32, "contentHash": string, "comment": string, "images"?: [{ "type": "image", "data": string, "mimeType": string }] }`.
    CodeReviewComment {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "snapshotId")]
        snapshot_id: String,
        path: String,
        #[serde(rename = "oldStart")]
        old_start: u32,
        #[serde(rename = "oldCount")]
        old_count: u32,
        #[serde(rename = "newStart")]
        new_start: u32,
        #[serde(rename = "newCount")]
        new_count: u32,
        #[serde(rename = "contentHash")]
        content_hash: String,
        comment: String,
        #[serde(default, deserialize_with = "null_default_vec")]
        images: Vec<ContentBlock>,
    },
    /// Swap the open review's reply model. `modelId` resolves against the
    /// session's public model catalog (the same source the main composer's
    /// model list uses) by its `provider/model-id` identity; an unknown id
    /// fails closed with a typed error.
    /// Connection material (api/provider/base_url/headers) always comes from
    /// the catalog, never from the wire. The swap affects comment replies
    /// that start afterwards; in-flight replies are unaffected.
    /// Wire shape:
    /// `{ "type": "code_review_set_model", "id"?: string, "modelId": string }`.
    /// Response: `{ "ok": true, "model": "<display name>" }`.
    CodeReviewSetModel {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "modelId")]
        model_id: String,
    },
    /// Start one Auto Review batch over the open review: every comment-ready
    /// hunk of the open snapshot (in snapshot order, capped at
    /// `MAX_AUTO_REVIEW_HUNKS`) receives an auto comment through the existing
    /// per-hunk comment pipeline, streaming a reply into its own thread. The
    /// optional `modelId` resolves exactly like `code_review_set_model`
    /// (catalog, fail-closed); absent keeps the fork model. A second batch
    /// while one is live or pending is a typed reject.
    /// Wire shape:
    /// `{ "type": "code_review_auto_review", "id"?: string, "modelId"?: string, "language"?: string }`.
    /// Response: `{ "ok": true, "state": "triaging", "batchId": N }` — the
    /// immediate start ACK; the batch state/counts then arrive on snapshots.
    CodeReviewAutoReview {
        #[serde(default)]
        id: Option<String>,
        #[serde(default, rename = "modelId")]
        model_id: Option<String>,
        #[serde(default)]
        language: Option<String>,
    },
    /// Run one focused Highlight call over the open snapshot: a single model
    /// call (never per-hunk) on the review fork agent that returns at most 20
    /// hunk identities with category. The result is transient — it resets
    /// on the next snapshot and is never persisted.
    /// Wire shape: `{ "type": "code_review_highlight", "id"?: string }`.
    /// Response: `{ "ok": true, "state": "running", "highlightGeneration": N, "reviewFindings": [...] }`.
    CodeReviewHighlight {
        #[serde(default)]
        id: Option<String>,
    },
    /// Start one Code Review TLDR over the current pinned snapshot. Optional
    /// `language` resolves like Auto Review. The controller owns the single
    /// operation slot; this command only starts it.
    /// Wire: `{ "type": "code_review_tldr", "id"?: string, "language"?: string }`.
    /// ACK: `{ "ok": true, "state": "running", "tldrGeneration": N }`.
    CodeReviewTldr {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        language: Option<String>,
    },
    /// Stop one in-flight review generation (`autoReview`, `highlight`, or `tldr`)
    /// without closing the review or cancelling a manual hunk reply.
    /// Wire shape: `{ "type": "code_review_stop", "id"?: string, "operation": "autoReview"|"highlight"|"tldr" }`.
    /// ACK: `{ "ok": true, "operation": ... }` — ok even when the review
    /// vanished during the unlocked abort (the retired agents clean up by
    /// their own drop); `operation` echoes the targeted one.
    CodeReviewStop {
        #[serde(default)]
        id: Option<String>,
        operation: ReviewStopOperation,
    },
    /// Abort the in-flight review agent turn for a specific hunk. The full
    /// hunk identity must match the current snapshot exactly (same fields as
    /// `code_review_comment`); a stale identity is rejected before any abort.
    /// Wire shape:
    /// `{ "type": "code_review_abort", "id"?: string, "snapshotId": string, "path": string, "oldStart": u32, "oldCount": u32, "newStart": u32, "newCount": u32, "contentHash": string }`.
    CodeReviewAbort {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "snapshotId")]
        snapshot_id: String,
        path: String,
        #[serde(rename = "oldStart")]
        old_start: u32,
        #[serde(rename = "oldCount")]
        old_count: u32,
        #[serde(rename = "newStart")]
        new_start: u32,
        #[serde(rename = "newCount")]
        new_count: u32,
        #[serde(rename = "contentHash")]
        content_hash: String,
    },
    /// Close the open code review: shut down the review agent and drop the
    /// snapshot/threads. Wire shape:
    /// `{ "type": "code_review_close", "id"?: string, "reviewId"?: string }`.
    /// A `reviewId` that differs from the open controller's identity is a
    /// no-op (`{ "closed": false, "staleReview": true }`): a retired panel's
    /// cleanup can never kill a controller opened after it, and neither a
    /// stale nor a missing-session close bumps the authority, so an
    /// in-flight open/refresh keeps winning. A stamped close invalidates
    /// the pending capture only while that review still owns the installed
    /// generation; a close of a review already replaced by a reserved open
    /// shuts down the old controller and leaves the newer capture valid. A
    /// legacy unstamped close always invalidates the pending capture. With
    /// nothing open the close is the typed `code_review_not_open`
    /// rejection.
    CodeReviewClose {
        #[serde(default)]
        id: Option<String>,
        #[serde(default, rename = "reviewId")]
        review_id: Option<String>,
    },
    /// Load one bounded page of a single file's full diff for the currently
    /// open review. The `snapshotId` must match the live snapshot (else the
    /// request is rejected as stale), `path` must be one of the snapshot's
    /// files (containment), and `cursor` is a 0-based line offset clamped to
    /// the loaded diff's length. The backend re-runs the same fixed-argv,
    /// isolated-sandbox, bounded git diff scoped to the file's pathspec
    /// (with rename provenance) and serves pages from an in-memory cache that
    /// is invalidated on open/refresh. Read-only; never mutates the repo.
    /// Every catalogued path is accepted, including on-demand placeholders
    /// that a globally truncated snapshot could not carry as hunks.
    /// Wire shape:
    /// `{ "type": "code_review_file_diff", "id"?: string, "snapshotId": string, "path": string, "cursor"?: usize, "maxLines"?: usize }`.
    CodeReviewFileDiff {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "snapshotId")]
        snapshot_id: String,
        path: String,
        #[serde(default)]
        cursor: usize,
        #[serde(default)]
        max_lines: Option<usize>,
    },
    /// Load a bounded window of unchanged file context for the open review.
    /// `oldStart`/`newStart` are 1-based; 0 means that side is absent (added
    /// or deleted). `count` 0 returns line counts only. Snapshot identity and
    /// path containment match `code_review_file_diff`.
    /// Wire:
    /// `{ "type":"code_review_file_context","id"?:string,"snapshotId":string,"path":string,"oldStart"?:u32,"newStart"?:u32,"count"?:u32 }`.
    CodeReviewFileContext {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "snapshotId")]
        snapshot_id: String,
        path: String,
        #[serde(default, rename = "oldStart")]
        old_start: u32,
        #[serde(default, rename = "newStart")]
        new_start: u32,
        #[serde(default)]
        count: Option<u32>,
    },
    /// List one workspace-relative directory through the captured filesystem
    /// policy. Wire: `{ "type":"file_explorer_list", "id"?:string, "path"?:string }`.
    FileExplorerList {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        path: Option<String>,
    },
    /// Search workspace files and directories by name or relative path.
    /// Wire: `{ "type":"file_explorer_search", "id"?:string, "query":string }`.
    FileExplorerSearch {
        #[serde(default)]
        id: Option<String>,
        query: String,
    },
    /// Read one bounded page of a workspace-relative text file.
    /// Wire: `{ "type":"file_explorer_read", "id"?:string, "path":string, "cursor"?:number, "maxLines"?:number }`.
    FileExplorerRead {
        #[serde(default)]
        id: Option<String>,
        path: String,
        #[serde(default)]
        cursor: usize,
        #[serde(default, rename = "maxLines")]
        max_lines: Option<usize>,
    },
    /// List worktree changes relative to HEAD.
    FileExplorerChanges {
        #[serde(default)]
        id: Option<String>,
    },
    /// Read one bounded page of a per-file unified worktree diff.
    FileExplorerDiff {
        #[serde(default)]
        id: Option<String>,
        path: String,
        #[serde(default)]
        cursor: usize,
        #[serde(default, rename = "maxLines")]
        max_lines: Option<usize>,
    },
    /// Attribute one inclusive 1-based source selection.
    /// Wire: `{ "type":"file_explorer_blame", "id"?:string, "path":string, "startLine":number, "endLine":number }`.
    FileExplorerBlame {
        #[serde(default)]
        id: Option<String>,
        path: String,
        #[serde(rename = "startLine")]
        start_line: usize,
        #[serde(rename = "endLine")]
        end_line: usize,
    },
    /// Delete a workspace file or directory (recursive).
    /// Wire: `{ "type": "file_explorer_delete", "id"?: string, "path": string }`.
    FileExplorerDelete {
        #[serde(default)]
        id: Option<String>,
        path: String,
    },
    /// Stage one bounded workspace file for the existing one-shot download
    /// route. The listen session manager owns the staged capability.
    FileExplorerStageDownload {
        #[serde(default)]
        id: Option<String>,
        path: String,
    },
    /// One-hop dependency map for one workspace-relative path. Blocking
    /// generation runs on the existing explorer `spawn_blocking` boundary
    /// and calls `crate::dependency_map::dependency_map`. No RPC-owned map
    /// state. Wire: `{ "type": "file_explorer_map", "id"?: string, "path": string }`.
    FileExplorerMap {
        #[serde(default)]
        id: Option<String>,
        path: String,
    },
    /// Complete one user-typed path for the frontend path fields (session
    /// cwd, project cwd). Lists ONE parent directory through the captured
    /// filesystem policy: relative inputs resolve against the current
    /// session cwd, absolute inputs are accepted only beneath an authorized
    /// workspace root (cwd or an additional root), and creation paths are
    /// still validated upstream. Returns
    /// `{ base, prefix, entries: [{ name, path, kind }], truncated }` where
    /// `base` is the normalized parent in the input's own form domain and
    /// each `entry.path` is `base/name` in that same domain. The optional
    /// top-level `sessionId` runtime selector is stripped by the transport
    /// before deserialization and is not part of this variant.
    /// Wire: `{ "type":"path_complete", "id"?:string, "path":string, "kind":"directory"|"file"|"any", "limit"?:number }`.
    PathComplete {
        #[serde(default)]
        id: Option<String>,
        path: String,
        kind: crate::file_explorer::CompletionKind,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// Wire: `{ "type":"file_conversation_open", "id"?:string, "conversationId":string, "path":string }`.
    FileConversationOpen {
        #[serde(default)]
        id: Option<String>,
        #[serde(
            rename = "conversationId",
            deserialize_with = "deserialize_conversation_id"
        )]
        conversation_id: String,
        path: String,
    },
    /// Wire: `{ "type":"file_conversation_prompt", "id"?:string, "conversationId":string, "message":string, "selection"?:{"startLine":number,"endLine":number} }`.
    /// Absent `selection` means none; an explicit `null` is a typed `file_conversation_selection_invalid` rejection.
    FileConversationPrompt {
        #[serde(default)]
        id: Option<String>,
        #[serde(
            rename = "conversationId",
            deserialize_with = "deserialize_conversation_id"
        )]
        conversation_id: String,
        message: String,
        #[serde(default, deserialize_with = "selection_preserving_null")]
        selection: Option<Value>,
    },
    /// Wire: `{ "type":"file_conversation_snapshot", "id"?:string, "conversationId":string }`.
    FileConversationSnapshot {
        #[serde(default)]
        id: Option<String>,
        #[serde(
            rename = "conversationId",
            deserialize_with = "deserialize_conversation_id"
        )]
        conversation_id: String,
    },
    /// Wire: `{ "type":"file_conversation_abort", "id"?:string, "conversationId":string }`.
    FileConversationAbort {
        #[serde(default)]
        id: Option<String>,
        #[serde(
            rename = "conversationId",
            deserialize_with = "deserialize_conversation_id"
        )]
        conversation_id: String,
    },
    /// Wire: `{ "type":"file_conversation_close", "id"?:string, "conversationId":string }`.
    FileConversationClose {
        #[serde(default)]
        id: Option<String>,
        #[serde(
            rename = "conversationId",
            deserialize_with = "deserialize_conversation_id"
        )]
        conversation_id: String,
    },
    /// Render a loaded skill's frontmatter summary (the `/skill <name>`
    /// mirror). Rejects an empty or unknown name. Returns `{ name, summary }`.
    /// Wire shape: `{ "type": "skill", "id"?: string, "name": string }`.
    Skill {
        #[serde(default)]
        id: Option<String>,
        name: String,
    },
    /// Invoke one trusted installed extension command (TUI `/name` and `/run`).
    /// Wire: `{ "type": "run", "id"?: string, "name": string, "args"?: string }`.
    Run {
        #[serde(default)]
        id: Option<String>,
        name: String,
        #[serde(default)]
        args: String,
    },
    /// List loaded persistent personas (the bare `/persona` mirror) with
    /// bounded contract summaries and memory/session state counts. Never
    /// exposes filesystem paths. Returns
    /// `{ "enabled": bool, "personas": [PersonaRow] }` where a row is
    /// `{ name, description, source, trusted, preferred, contractSummary,
    ///    memoryEntries, sessionCount, stateError }` (`stateError` is the
    /// fixed literal `"unreadable"` or null — never path text).
    /// Wire shape: `{ "type": "persona_list", "id"?: string }`.
    PersonaList {
        #[serde(default)]
        id: Option<String>,
    },
    /// Show one persona's definition (the `/persona <name>` mirror): the
    /// same row fields as `persona_list` plus `content` (the raw
    /// `persona.md`, bounded to `MAX_PERSONA_RPC_CONTENT_BYTES`) and
    /// `contentTruncated`. Unknown or non-persona names fail closed.
    /// Wire shape: `{ "type": "persona_get", "id"?: string, "name": string }`.
    PersonaGet {
        #[serde(default)]
        id: Option<String>,
        name: String,
    },
    /// Create a persona definition from content (the `/persona new <name>`
    /// editor-free mirror): validates the frontmatter name matches `name`,
    /// writes atomically under the user persona scope, and live-reloads the
    /// catalog so the persona is discoverable on the next read. Returns
    /// `{ "name": name, "created": true, "message": text }`.
    /// Wire shape:
    /// `{ "type": "persona_create", "id"?: string, "name": string, "content": string }`.
    PersonaCreate {
        #[serde(default)]
        id: Option<String>,
        name: String,
        content: String,
    },
    /// Overwrite an existing persona definition (the `/persona edit <name>`
    /// editor-free mirror). The committed content must declare the SAME name
    /// as the target (rename is rejected), and the file must be a regular
    /// non-symlink `persona.md` inside the persona scope. Returns
    /// `{ "name": name, "edited": true, "message": text }`.
    /// Wire shape:
    /// `{ "type": "persona_edit", "id"?: string, "name": string, "content": string }`.
    PersonaEdit {
        #[serde(default)]
        id: Option<String>,
        name: String,
        content: String,
    },
    /// Remove a persona DEFINITION only, keeping `memory/` and `sessions/`
    /// under the persona root (the `/persona remove <name> --yes` mirror).
    /// Requires an explicit `"confirm": true` on the wire — anything else is
    /// rejected before any filesystem mutation (fail-closed, mirroring the
    /// CLI `--yes` gate). Returns `{ "name": name, "removed": true,
    /// "message": text }`.
    /// Wire shape:
    /// `{ "type": "persona_remove", "id"?: string, "name": string, "confirm": bool }`.
    PersonaRemove {
        #[serde(default)]
        id: Option<String>,
        name: String,
        #[serde(default)]
        confirm: bool,
    },
    /// Purge a persona ENTIRELY: delete the whole persona root including
    /// `persona.md`, `memory/`, and `sessions/` (the
    /// `/persona remove <name> --purge --yes` mirror). Requires
    /// `"confirm": true`; anything else is rejected before any filesystem
    /// mutation. Returns `{ "name": name, "purged": true, "message": text }`.
    /// Wire shape:
    /// `{ "type": "persona_purge", "id"?: string, "name": string, "confirm": bool }`.
    PersonaPurge {
        #[serde(default)]
        id: Option<String>,
        name: String,
        #[serde(default)]
        confirm: bool,
    },
    /// Prefer a persona for unnamed task spawns (the
    /// `/persona <name> --select` mirror). The persona must be trusted and
    /// enabled. Returns `{ "name": name, "preferred": true, "message": text }`.
    /// Wire shape: `{ "type": "persona_select", "id"?: string, "name": string }`.
    PersonaSelect {
        #[serde(default)]
        id: Option<String>,
        name: String,
    },
    /// Clear the preferred persona (the `/persona --clear` mirror). Returns
    /// `{ "preferred": null, "message": text }`.
    /// Wire shape: `{ "type": "persona_clear", "id"?: string }`.
    PersonaClear {
        #[serde(default)]
        id: Option<String>,
    },
    /// Report the currently preferred agent/persona (the `/persona --current`
    /// mirror). Returns `{ "name": string | null, "message": text }`.
    /// Wire shape: `{ "type": "persona_current", "id"?: string }`.
    PersonaCurrent {
        #[serde(default)]
        id: Option<String>,
    },
    TodoChildCreate {
        #[serde(default)]
        id: Option<String>,
        #[serde(flatten)]
        request: TodoChildCreateRequest,
    },
    TodoChildList {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "parentSessionId")]
        parent_session_id: String,
    },
    TodoChildSelect {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "parentSessionId")]
        parent_session_id: String,
        #[serde(rename = "childSessionId")]
        child_session_id: String,
    },
    TodoChildSend {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "parentSessionId")]
        parent_session_id: String,
        #[serde(default, rename = "childSessionId")]
        child_session_id: Option<String>,
        message: String,
    },
    TodoChildStatus {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "parentSessionId")]
        parent_session_id: String,
        #[serde(default, rename = "childSessionId")]
        child_session_id: Option<String>,
    },
    TodoChildStop {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "parentSessionId")]
        parent_session_id: String,
        #[serde(default, rename = "childSessionId")]
        child_session_id: Option<String>,
    },
    TodoChildPause {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "parentSessionId")]
        parent_session_id: String,
        #[serde(default, rename = "childSessionId")]
        child_session_id: Option<String>,
    },
    TodoChildResume {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "parentSessionId")]
        parent_session_id: String,
        #[serde(default, rename = "childSessionId")]
        child_session_id: Option<String>,
    },
    TodoChildCancel {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "parentSessionId")]
        parent_session_id: String,
        #[serde(default, rename = "childSessionId")]
        child_session_id: Option<String>,
    },
    TodoChildArchive {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "parentSessionId")]
        parent_session_id: String,
        #[serde(rename = "childSessionId")]
        child_session_id: String,
    },
    TodoChildRemove {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "parentSessionId")]
        parent_session_id: String,
        #[serde(rename = "childSessionId")]
        child_session_id: String,
    },
    TodoChildAddTask {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "parentSessionId")]
        parent_session_id: String,
        #[serde(rename = "childSessionId")]
        child_session_id: String,
        #[serde(flatten)]
        request: TodoChildAddTaskRequest,
    },
    TodoChildDetail {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "parentSessionId")]
        parent_session_id: String,
        #[serde(rename = "childSessionId")]
        child_session_id: String,
    },
    TodoChildHubSend {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "parentSessionId")]
        parent_session_id: String,
        #[serde(rename = "childSessionId")]
        child_session_id: String,
        to: String,
        body: String,
        #[serde(default, rename = "replyTo")]
        reply_to: Option<String>,
    },
    /// Server-side persistence for the /chat LAN mapping document.
    /// Wire: `{ "type": "chat_mapping_get", "id"?: string }`.
    ChatMappingGet {
        #[serde(default)]
        id: Option<String>,
    },
    /// Wire: `{ "type": "chat_mapping_put", "id"?: string, "document": object,
    /// "baseRevision"?: number }`. A stale base returns `conflict: true` with
    /// the server copy instead of writing.
    ChatMappingPut {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "document")]
        document: Value,
        #[serde(default, rename = "baseRevision")]
        base_revision: Option<u64>,
    },
    TeamList {
        #[serde(default)]
        id: Option<String>,
    },
    TeamCreate {
        #[serde(default)]
        id: Option<String>,
        name: String,
        #[serde(default, rename = "projectLabel")]
        project_label: Option<String>,
        #[serde(default, rename = "projectCwd")]
        project_cwd: Option<String>,
    },
    TeamRemove {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "teamId")]
        team_id: String,
    },
    TeamJoin {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "teamId")]
        team_id: String,
        origin: String,
        /// Runner credential; consumed by the registry, never echoed back.
        token: String,
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        role: Option<String>,
    },
    TeamLeave {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "teamId")]
        team_id: String,
        #[serde(rename = "runnerId")]
        runner_id: String,
    },
    TeamStatus {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "teamId")]
        team_id: String,
    },
    TeamRun {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "teamId")]
        team_id: String,
        #[serde(rename = "runnerId")]
        runner_id: String,
        command: crate::team::TeamRunnerCommand,
    },
    TeamBroadcast {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "teamId")]
        team_id: String,
        command: crate::team::TeamRunnerCommand,
    },
    GetDiscoveredRunners {
        #[serde(default)]
        id: Option<String>,
    },
    TmuxListSessions {
        #[serde(default)]
        id: Option<String>,
    },
    TmuxPaneSnapshot {
        #[serde(default)]
        id: Option<String>,
        session: String,
        #[serde(default)]
        lines: Option<u32>,
    },
    TmuxSendKeys {
        #[serde(default)]
        id: Option<String>,
        session: String,
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        key: Option<String>,
    },
    TmuxSessionCreate {
        #[serde(default)]
        id: Option<String>,
        session: String,
    },
    TmuxSessionDelete {
        #[serde(default)]
        id: Option<String>,
        session: String,
    },
    TmuxTerminalAttach {
        #[serde(default)]
        id: Option<String>,
        session: String,
        cols: u32,
        rows: u32,
        #[serde(default, rename = "fullHistory")]
        full_history: bool,
        #[serde(default, rename = "cachedHistoryKey")]
        cached_history_key: Option<String>,
    },
    TmuxTerminalInput {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "terminalId")]
        terminal_id: String,
        data: String,
    },
    TmuxTerminalResize {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "terminalId")]
        terminal_id: String,
        cols: u32,
        rows: u32,
        #[serde(default, rename = "fitWindow")]
        fit_window: bool,
    },
    TmuxTerminalDetach {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "terminalId")]
        terminal_id: String,
    },
}
impl RpcCommand {
    /// Validated catalog tuple of a `switch_session`, when the client sent
    /// one. All three fields are required together; absent keeps the legacy
    /// path-only internal flow. Wire aliases (`native`, `primary`,
    /// `grok-hyper`, `grok/hyper`) normalize to the strict canonical set.
    pub(crate) fn switch_catalog_identity(
        &self,
    ) -> Result<Option<CatalogSwitchIdentity>, SwitchCatalogIdentityError> {
        match self {
            Self::SwitchSession {
                catalog_source,
                catalog_session_id,
                catalog_status,
                ..
            } => switch_catalog_identity(
                catalog_source.as_deref(),
                catalog_session_id.as_deref(),
                catalog_status.as_deref(),
            ),
            _ => Ok(None),
        }
    }
    pub(crate) fn id(&self) -> Option<String> {
        match self {
            Self::Prompt { id, .. }
            | Self::Steer { id, .. }
            | Self::FollowUp { id, .. }
            | Self::Abort { id }
            | Self::NewSession { id, .. }
            | Self::GetState { id }
            | Self::SetModel { id, .. }
            | Self::CycleModel { id }
            | Self::GetAvailableModels { id }
            | Self::GetAvailableThinkingLevels { id }
            | Self::SetThinkingLevel { id, .. }
            | Self::CycleThinkingLevel { id }
            | Self::SetSteeringMode { id, .. }
            | Self::SetFollowUpMode { id, .. }
            | Self::Compact { id, .. }
            | Self::SetAutoCompaction { id, .. }
            | Self::SetAutoRetry { id, .. }
            | Self::AbortRetry { id }
            | Self::Bash { id, .. }
            | Self::AbortBash { id }
            | Self::GetSessionStats { id }
            | Self::SessionList { id, .. }
            | Self::ExportHtml { id, .. }
            | Self::SwitchSession { id, .. }
            | Self::Fork { id, .. }
            | Self::Clone { id, .. }
            | Self::GetForkMessages { id, .. }
            | Self::GetEntries { id, .. }
            | Self::GetTree { id }
            | Self::GetLastAssistantText { id }
            | Self::SetSessionName { id, .. }
            | Self::SessionForkLatest { id, .. }
            | Self::SessionHandoff { id, .. }
            | Self::SessionMove { id, .. }
            | Self::SessionDelete { id, .. }
            | Self::SessionRestore { id, .. }
            | Self::SessionCopyContext { id, .. }
            | Self::GetMessages { id }
            | Self::GetCommands { id }
            | Self::SetTodos { id, .. }
            | Self::TodoOp { id, .. }
            | Self::LoopCreate { id, .. }
            | Self::LoopUpdate { id, .. }
            | Self::LoopList { id }
            | Self::LoopDelete { id, .. }
            | Self::LoopCancel { id, .. }
            | Self::ProcessSpawn { id, .. }
            | Self::ProcessList { id }
            | Self::ProcessDescribe { id, .. }
            | Self::ProcessLogs { id, .. }
            | Self::ProcessWrite { id, .. }
            | Self::ChatMappingGet { id }
            | Self::ChatMappingPut { id, .. }
            | Self::GoalJournal { id }
            | Self::ProcessKeys { id, .. }
            | Self::ProcessResize { id, .. }
            | Self::ProcessSignal { id, .. }
            | Self::ProcessStop { id, .. }
            | Self::GoalCreate { id, .. }
            | Self::GoalGet { id }
            | Self::GoalPause { id }
            | Self::GoalResume { id, .. }
            | Self::GoalComplete { id }
            | Self::GoalDrop { id }
            | Self::GoalUpdateUsage { id, .. }
            | Self::GoalPin { id, .. }
            | Self::GoalUnpin { id, .. }
            | Self::GoalJournal { id }
            | Self::SettingsInspect { id }
            | Self::SettingsSearch { id, .. }
            | Self::SettingsOpenDraft { id, .. }
            | Self::SettingsGetDraft { id, .. }
            | Self::SettingsSet { id, .. }
            | Self::SettingsReset { id, .. }
            | Self::SettingsValidate { id, .. }
            | Self::SettingsApply { id, .. }
            | Self::SettingsCancel { id, .. }
            | Self::ProcessWait { id, .. }
            | Self::WorkflowCreate { id, .. }
            | Self::WorkflowList { id }
            | Self::WorkflowGet { id, .. }
            | Self::WorkflowDetail { id, .. }
            | Self::WorkflowPause { id, .. }
            | Self::WorkflowResume { id, .. }
            | Self::WorkflowCancel { id, .. }
            | Self::WorkflowIntegrate { id, .. }
            | Self::WorkflowRemove { id, .. }
            | Self::WorkflowHubSend { id, .. }
            | Self::SideChatNew { id, .. }
            | Self::SideChatSwitch { id, .. }
            | Self::SideChatClose { id, .. }
            | Self::SideChatPrompt { id, .. }
            | Self::SideChatList { id }
            | Self::SideChatAbort { id }
            | Self::SnapCompact { id }
            | Self::Rewind { id, .. }
            | Self::Handoff { id, .. }
            | Self::QueueList { id }
            | Self::QueueCancel { id, .. }
            | Self::QueueSendNow { id }
            | Self::JobList { id }
            | Self::TaskSpawn { id, .. }
            | Self::JobCancel { id, .. }
            | Self::HubSend { id, .. }
            | Self::JobOutput { id, .. }
            | Self::AgentHistory { id, .. }
            | Self::CollabStart { id, .. }
            | Self::CollabStatus { id, .. }
            | Self::CollabStop { id, .. }
            | Self::RealtimeCreateCall { id, .. }
            | Self::RealtimeStop { id }
            | Self::SttTranscribe { id, .. }
            | Self::CodeReviewOpen { id, .. }
            | Self::CodeReviewCommits { id, .. }
            | Self::CodeReviewSnapshot { id }
            | Self::CodeReviewRefresh { id }
            | Self::CodeReviewComment { id, .. }
            | Self::CodeReviewSetModel { id, .. }
            | Self::CodeReviewAutoReview { id, .. }
            | Self::CodeReviewHighlight { id, .. }
            | Self::CodeReviewTldr { id, .. }
            | Self::CodeReviewStop { id, .. }
            | Self::CodeReviewAbort { id, .. }
            | Self::CodeReviewClose { id, .. }
            | Self::CodeReviewFileDiff { id, .. }
            | Self::CodeReviewFileContext { id, .. }
            | Self::FileExplorerList { id, .. }
            | Self::FileExplorerSearch { id, .. }
            | Self::FileExplorerRead { id, .. }
            | Self::FileExplorerChanges { id }
            | Self::FileExplorerDiff { id, .. }
            | Self::FileExplorerBlame { id, .. }
            | Self::FileExplorerDelete { id, .. }
            | Self::FileExplorerStageDownload { id, .. }
            | Self::FileExplorerMap { id, .. }
            | Self::PathComplete { id, .. }
            | Self::FileConversationOpen { id, .. }
            | Self::FileConversationPrompt { id, .. }
            | Self::FileConversationSnapshot { id, .. }
            | Self::FileConversationAbort { id, .. }
            | Self::FileConversationClose { id, .. }
            | Self::CloseSession { id }
            | Self::Skill { id, .. }
            | Self::Run { id, .. }
            | Self::PersonaList { id }
            | Self::PersonaGet { id, .. }
            | Self::PersonaCreate { id, .. }
            | Self::PersonaEdit { id, .. }
            | Self::PersonaRemove { id, .. }
            | Self::PersonaPurge { id, .. }
            | Self::PersonaSelect { id, .. }
            | Self::PersonaClear { id }
            | Self::PersonaCurrent { id }
            | Self::TodoChildCreate { id, .. }
            | Self::TodoChildList { id, .. }
            | Self::TodoChildSelect { id, .. }
            | Self::TodoChildSend { id, .. }
            | Self::TodoChildStatus { id, .. }
            | Self::TodoChildStop { id, .. }
            | Self::TodoChildPause { id, .. }
            | Self::TodoChildResume { id, .. }
            | Self::TodoChildCancel { id, .. }
            | Self::TodoChildArchive { id, .. }
            | Self::TodoChildRemove { id, .. }
            | Self::TodoChildAddTask { id, .. }
            | Self::TodoChildDetail { id, .. }
            | Self::TodoChildHubSend { id, .. }
            | Self::TeamList { id }
            | Self::TeamCreate { id, .. }
            | Self::TeamRemove { id, .. }
            | Self::TeamJoin { id, .. }
            | Self::TeamLeave { id, .. }
            | Self::TeamStatus { id, .. }
            | Self::TeamRun { id, .. }
            | Self::TeamBroadcast { id, .. }
            | Self::GetDiscoveredRunners { id }
            | Self::TmuxListSessions { id }
            | Self::TmuxPaneSnapshot { id, .. }
            | Self::TmuxSendKeys { id, .. }
            | Self::TmuxSessionCreate { id, .. }
            | Self::TmuxSessionDelete { id, .. }
            | Self::TmuxTerminalAttach { id, .. }
            | Self::TmuxTerminalInput { id, .. }
            | Self::TmuxTerminalResize { id, .. }
            | Self::TmuxTerminalDetach { id, .. }
            | Self::ChatMappingGet { id }
            | Self::ChatMappingPut { id, .. } => id.clone(),
        }
    }
    pub(crate) const fn command_name(&self) -> &'static str {
        match self {
            Self::Prompt { .. } => "prompt",
            Self::Steer { .. } => "steer",
            Self::FollowUp { .. } => "follow_up",
            Self::Abort { .. } => "abort",
            Self::NewSession { .. } => "new_session",
            Self::GetState { .. } => "get_state",
            Self::SetModel { .. } => "set_model",
            Self::CycleModel { .. } => "cycle_model",
            Self::GetAvailableModels { .. } => "get_available_models",
            Self::GetAvailableThinkingLevels { .. } => "get_available_thinking_levels",
            Self::SetThinkingLevel { .. } => "set_thinking_level",
            Self::CycleThinkingLevel { .. } => "cycle_thinking_level",
            Self::SetSteeringMode { .. } => "set_steering_mode",
            Self::SetFollowUpMode { .. } => "set_follow_up_mode",
            Self::Compact { .. } => "compact",
            Self::SetAutoCompaction { .. } => "set_auto_compaction",
            Self::SetAutoRetry { .. } => "set_auto_retry",
            Self::AbortRetry { .. } => "abort_retry",
            Self::Bash { .. } => "bash",
            Self::AbortBash { .. } => "abort_bash",
            Self::GetSessionStats { .. } => "get_session_stats",
            Self::SessionList { .. } => "session_list",
            Self::ExportHtml { .. } => "export_html",
            Self::SwitchSession { .. } => "switch_session",
            Self::Fork { .. } => "fork",
            Self::Clone { .. } => "clone",
            Self::GetForkMessages { .. } => "get_fork_messages",
            Self::GetEntries { .. } => "get_entries",
            Self::GetTree { .. } => "get_tree",
            Self::GetLastAssistantText { .. } => "get_last_assistant_text",
            Self::SetSessionName { .. } => "set_session_name",
            Self::SessionForkLatest { .. } => "session_fork_latest",
            Self::SessionDelete { .. } => "session_delete",
            Self::SessionRestore { .. } => "session_restore",
            Self::SessionHandoff { .. } => "session_handoff",
            Self::SessionCopyContext { .. } => "session_copy_context",
            Self::SessionMove { .. } => "session_move",
            Self::GetMessages { .. } => "get_messages",
            Self::GetCommands { .. } => "get_commands",
            Self::SetTodos { .. } => "set_todos",
            Self::TodoOp { .. } => "todo_op",
            Self::LoopCreate { .. } => "loop_create",
            Self::LoopUpdate { .. } => "loop_update",
            Self::LoopList { .. } => "loop_list",
            Self::LoopDelete { .. } => "loop_delete",
            Self::LoopCancel { .. } => "loop_cancel",
            Self::ProcessSpawn { .. } => "process_spawn",
            Self::ProcessList { .. } => "process_list",
            Self::ProcessDescribe { .. } => "process_describe",
            Self::ProcessLogs { .. } => "process_logs",
            Self::ProcessWrite { .. } => "process_write",
            Self::ProcessKeys { .. } => "process_keys",
            Self::ProcessResize { .. } => "process_resize",
            Self::ChatMappingGet { .. } => "chat_mapping_get",
            Self::ChatMappingPut { .. } => "chat_mapping_put",
            Self::ProcessSignal { .. } => "process_signal",
            Self::ProcessStop { .. } => "process_stop",
            Self::ProcessWait { .. } => "process_wait",
            Self::GoalCreate { .. } => "goal_create",
            Self::GoalGet { .. } => "goal_get",
            Self::GoalPause { .. } => "goal_pause",
            Self::GoalResume { .. } => "goal_resume",
            Self::GoalComplete { .. } => "goal_complete",
            Self::GoalDrop { .. } => "goal_drop",
            Self::GoalUpdateUsage { .. } => "goal_update_usage",
            Self::GoalPin { .. } => "goal_pin",
            Self::GoalUnpin { .. } => "goal_unpin",
            Self::GoalJournal { .. } => "goal_journal",
            Self::SettingsInspect { .. } => "settings_inspect",
            Self::SettingsSearch { .. } => "settings_search",
            Self::SettingsOpenDraft { .. } => "settings_open_draft",
            Self::SettingsGetDraft { .. } => "settings_get_draft",
            Self::SettingsSet { .. } => "settings_set",
            Self::SettingsReset { .. } => "settings_reset",
            Self::SettingsValidate { .. } => "settings_validate",
            Self::SettingsApply { .. } => "settings_apply",
            Self::SettingsCancel { .. } => "settings_cancel",
            Self::WorkflowCreate { .. } => "workflow_create",
            Self::WorkflowList { .. } => "workflow_list",
            Self::WorkflowGet { .. } => "workflow_get",
            Self::WorkflowDetail { .. } => "workflow_detail",
            Self::WorkflowPause { .. } => "workflow_pause",
            Self::WorkflowResume { .. } => "workflow_resume",
            Self::WorkflowCancel { .. } => "workflow_cancel",
            Self::WorkflowIntegrate { .. } => "workflow_integrate",
            Self::WorkflowRemove { .. } => "workflow_remove",
            Self::WorkflowHubSend { .. } => "workflow_hub_send",
            Self::SideChatNew { .. } => "side_chat_new",
            Self::SideChatSwitch { .. } => "side_chat_switch",
            Self::SideChatClose { .. } => "side_chat_close",
            Self::SideChatPrompt { .. } => "side_chat_prompt",
            Self::SideChatList { .. } => "side_chat_list",
            Self::SideChatAbort { .. } => "side_chat_abort",
            Self::SnapCompact { .. } => "snapcompact",
            Self::Rewind { .. } => "rewind",
            Self::Handoff { .. } => "handoff",
            Self::QueueList { .. } => "queue_list",
            Self::QueueCancel { .. } => "queue_cancel",
            Self::QueueSendNow { .. } => "queue_send_now",
            Self::JobList { .. } => "job_list",
            Self::TaskSpawn { .. } => "task_spawn",
            Self::JobCancel { .. } => "job_cancel",
            Self::HubSend { .. } => "hub_send",
            Self::JobOutput { .. } => "job_output",
            Self::AgentHistory { .. } => "agent_history",
            Self::CollabStart { .. } => "collab_start",
            Self::CollabStatus { .. } => "collab_status",
            Self::CollabStop { .. } => "collab_stop",
            Self::CloseSession { .. } => "close_session",
            Self::RealtimeCreateCall { .. } => "realtime_create_call",
            Self::RealtimeStop { .. } => "realtime_stop",
            Self::SttTranscribe { .. } => "stt_transcribe",
            Self::CodeReviewOpen { .. } => "code_review_open",
            Self::CodeReviewCommits { .. } => "code_review_commits",
            Self::CodeReviewSnapshot { .. } => "code_review_snapshot",
            Self::CodeReviewRefresh { .. } => "code_review_refresh",
            Self::CodeReviewComment { .. } => "code_review_comment",
            Self::CodeReviewAbort { .. } => "code_review_abort",
            Self::CodeReviewAutoReview { .. } => "code_review_auto_review",
            Self::CodeReviewHighlight { .. } => "code_review_highlight",
            Self::CodeReviewTldr { .. } => "code_review_tldr",
            Self::CodeReviewStop { .. } => "code_review_stop",
            Self::CodeReviewSetModel { .. } => "code_review_set_model",
            Self::CodeReviewClose { .. } => "code_review_close",
            Self::CodeReviewFileDiff { .. } => "code_review_file_diff",
            Self::CodeReviewFileContext { .. } => "code_review_file_context",
            Self::FileExplorerList { .. } => "file_explorer_list",
            Self::FileExplorerSearch { .. } => "file_explorer_search",
            Self::FileExplorerRead { .. } => "file_explorer_read",
            Self::FileExplorerChanges { .. } => "file_explorer_changes",
            Self::FileExplorerDiff { .. } => "file_explorer_diff",
            Self::FileExplorerBlame { .. } => "file_explorer_blame",
            Self::FileExplorerDelete { .. } => "file_explorer_delete",
            Self::FileExplorerStageDownload { .. } => "file_explorer_stage_download",
            Self::FileExplorerMap { .. } => "file_explorer_map",
            Self::PathComplete { .. } => "path_complete",
            Self::FileConversationOpen { .. } => "file_conversation_open",
            Self::FileConversationPrompt { .. } => "file_conversation_prompt",
            Self::FileConversationSnapshot { .. } => "file_conversation_snapshot",
            Self::FileConversationAbort { .. } => "file_conversation_abort",
            Self::FileConversationClose { .. } => "file_conversation_close",
            Self::Skill { .. } => "skill",
            Self::Run { .. } => "run",
            Self::PersonaList { .. } => "persona_list",
            Self::PersonaGet { .. } => "persona_get",
            Self::PersonaCreate { .. } => "persona_create",
            Self::PersonaEdit { .. } => "persona_edit",
            Self::PersonaRemove { .. } => "persona_remove",
            Self::PersonaPurge { .. } => "persona_purge",
            Self::PersonaSelect { .. } => "persona_select",
            Self::PersonaClear { .. } => "persona_clear",
            Self::PersonaCurrent { .. } => "persona_current",
            Self::TodoChildCreate { .. } => "todo_child_create",
            Self::TodoChildList { .. } => "todo_child_list",
            Self::TodoChildSelect { .. } => "todo_child_select",
            Self::TodoChildSend { .. } => "todo_child_send",
            Self::TodoChildStatus { .. } => "todo_child_status",
            Self::TodoChildStop { .. } => "todo_child_stop",
            Self::TodoChildPause { .. } => "todo_child_pause",
            Self::TodoChildResume { .. } => "todo_child_resume",
            Self::TodoChildCancel { .. } => "todo_child_cancel",
            Self::TodoChildArchive { .. } => "todo_child_archive",
            Self::TodoChildRemove { .. } => "todo_child_remove",
            Self::TodoChildAddTask { .. } => "todo_child_add_task",
            Self::TodoChildDetail { .. } => "todo_child_detail",
            Self::TodoChildHubSend { .. } => "todo_child_hub_send",
            Self::TeamList { .. } => "team_list",
            Self::TeamCreate { .. } => "team_create",
            Self::TeamRemove { .. } => "team_remove",
            Self::TeamJoin { .. } => "team_join",
            Self::TeamLeave { .. } => "team_leave",
            Self::TeamStatus { .. } => "team_status",
            Self::TeamRun { .. } => "team_run",
            Self::TeamBroadcast { .. } => "team_broadcast",
            Self::GetDiscoveredRunners { .. } => "get_discovered_runners",
            Self::TmuxListSessions { .. } => "tmux_list_sessions",
            Self::TmuxPaneSnapshot { .. } => "tmux_pane_snapshot",
            Self::TmuxSendKeys { .. } => "tmux_send_keys",
            Self::TmuxSessionCreate { .. } => "tmux_session_create",
            Self::TmuxSessionDelete { .. } => "tmux_session_delete",
            Self::TmuxTerminalAttach { .. } => "tmux_terminal_attach",
            Self::TmuxTerminalInput { .. } => "tmux_terminal_input",
            Self::TmuxTerminalResize { .. } => "tmux_terminal_resize",
            Self::TmuxTerminalDetach { .. } => "tmux_terminal_detach",
            Self::ChatMappingGet { .. } => "chat_mapping_get",
            Self::ChatMappingPut { .. } => "chat_mapping_put",
        }
    }

    pub(crate) const fn is_collab_lifecycle(&self) -> bool {
        matches!(
            self,
            Self::CollabStart { .. } | Self::CollabStatus { .. } | Self::CollabStop { .. }
        )
    }

    pub(crate) const fn is_listen_local(&self) -> bool {
        self.is_collab_lifecycle()
            || matches!(
                self,
                Self::GetDiscoveredRunners { .. }
                    | Self::TmuxListSessions { .. }
                    | Self::TmuxPaneSnapshot { .. }
                    | Self::TmuxSendKeys { .. }
                    | Self::TmuxSessionCreate { .. }
                    | Self::TmuxSessionDelete { .. }
                    | Self::TmuxTerminalAttach { .. }
                    | Self::TmuxTerminalInput { .. }
                    | Self::TmuxTerminalResize { .. }
                    | Self::TmuxTerminalDetach { .. }
            )
    }
}

impl RpcCommand {
    pub(crate) const fn runs_inline(&self) -> bool {
        matches!(
            self,
            Self::Abort { .. }
                | Self::AbortRetry { .. }
                // Steer/follow_up append to the pending queue in O(1); the
                // stdio gate must exempt them exactly like the Web bypass
                // (bypasses_command_slots) so a saturated command list can
                // never silently drop the user's steering.
                | Self::Steer { .. }
                | Self::FollowUp { .. }
                | Self::AbortBash { .. }
                | Self::LoopCancel { .. }
                | Self::ProcessSignal { .. }
                | Self::ProcessStop { .. }
                | Self::SessionList { .. }
                | Self::SettingsInspect { .. }
                | Self::SettingsSearch { .. }
                | Self::SettingsOpenDraft { .. }
                | Self::SettingsGetDraft { .. }
                | Self::SettingsSet { .. }
                | Self::SettingsReset { .. }
                | Self::SettingsValidate { .. }
                | Self::SettingsApply { .. }
                | Self::SettingsCancel { .. }
                | Self::WorkflowCreate { .. }
                | Self::WorkflowList { .. }
                | Self::WorkflowGet { .. }
                | Self::WorkflowDetail { .. }
                | Self::WorkflowPause { .. }
                | Self::WorkflowResume { .. }
                | Self::WorkflowCancel { .. }
                | Self::WorkflowIntegrate { .. }
                | Self::WorkflowRemove { .. }
                | Self::WorkflowHubSend { .. }
                | Self::SideChatNew { .. }
                | Self::SideChatSwitch { .. }
                | Self::SideChatClose { .. }
                | Self::SideChatPrompt { .. }
                | Self::SideChatList { .. }
                | Self::SideChatAbort { .. }
                | Self::QueueList { .. }
                | Self::QueueCancel { .. }
                | Self::QueueSendNow { .. }
                | Self::CollabStart { .. }
                | Self::CollabStatus { .. }
                | Self::CollabStop { .. }
                | Self::GetDiscoveredRunners { .. }
                | Self::CloseSession { .. }
                | Self::SessionDelete { .. }
                | Self::SessionRestore { .. }
                | Self::SessionForkLatest { .. }
                | Self::CodeReviewAutoReview { .. }
                | Self::CodeReviewHighlight { .. }
                | Self::CodeReviewTldr { .. }
                | Self::CodeReviewStop { .. }
                | Self::CodeReviewComment { .. }
                | Self::CodeReviewSetModel { .. }
                | Self::CodeReviewSnapshot { .. }
                | Self::CodeReviewAbort { .. }
                | Self::CodeReviewClose { .. }
                | Self::TodoChildCreate { .. }
                | Self::TodoChildList { .. }
                | Self::TodoChildSelect { .. }
                | Self::TodoChildSend { .. }
                | Self::TodoChildStatus { .. }
                | Self::TodoChildStop { .. }
                | Self::TodoChildPause { .. }
                | Self::TodoChildResume { .. }
                | Self::TodoChildArchive { .. }
                | Self::TodoChildRemove { .. }
                | Self::TodoChildAddTask { .. }
                | Self::TodoChildDetail { .. }
                | Self::TodoChildHubSend { .. }
                | Self::FileConversationOpen { .. }
                | Self::FileConversationPrompt { .. }
                | Self::FileConversationSnapshot { .. }
                | Self::FileConversationAbort { .. }
                | Self::FileConversationClose { .. }
        )
    }

    pub(crate) const fn bypasses_command_slots(&self) -> bool {
        matches!(
            self,
            Self::Abort { .. }
                | Self::AbortRetry { .. }
                | Self::AbortBash { .. }
                // Queue user operations append to or drain the pending queue
                // in O(1) and must never die on slot saturation (observed: a
                // long process_wait + watchdog probes filled all 16 slots and
                // silently dropped the user's steering).
                | Self::Steer { .. }
                | Self::FollowUp { .. }
                | Self::QueueCancel { .. }
                | Self::QueueSendNow { .. }
                | Self::LoopCancel { .. }
                | Self::ProcessSignal { .. }
                | Self::ProcessStop { .. }
                | Self::CloseSession { .. }
                | Self::SessionDelete { .. }
                | Self::SessionRestore { .. }
                | Self::RealtimeStop { .. }
                | Self::CodeReviewAbort { .. }
                | Self::CodeReviewStop { .. }
                | Self::CodeReviewClose { .. }
                | Self::SideChatAbort { .. }
                | Self::TodoChildList { .. }
                | Self::TodoChildStatus { .. }
                | Self::TodoChildSelect { .. }
                | Self::TodoChildDetail { .. }
                | Self::TmuxTerminalInput { .. }
                | Self::TmuxTerminalResize { .. }
                | Self::TmuxTerminalDetach { .. }
                | Self::TmuxSessionCreate { .. }
                | Self::TmuxSessionDelete { .. }
                | Self::FileConversationAbort { .. }
                | Self::FileConversationClose { .. }
        )
    }

    /// Read-only session-control commands must remain responsive while an
    /// ordinary command is awaiting external I/O. Interactive user-control
    /// commands (prompt / set_model / set_thinking_level) are also admitted
    /// directly: turn admission (`turn_admission` / the turn gate) serializes
    /// turn starts, and `set_model` is proven safe beside a live turn
    /// (applies to the next turn only), so none of them may starve behind one
    /// long-running ordinary command at the FIFO head.
    pub(crate) const fn bypasses_ordinary_fifo(&self) -> bool {
        matches!(
            self,
            Self::GetState { .. }
                | Self::GetSessionStats { .. }
                | Self::SessionList { .. }
                | Self::GetForkMessages { .. }
                | Self::Prompt { .. }
                | Self::SetModel { .. }
                | Self::SetThinkingLevel { .. }
                | Self::TodoChildList { .. }
                | Self::FileConversationOpen { .. }
                | Self::FileConversationPrompt { .. }
                | Self::FileConversationSnapshot { .. }
                | Self::FileConversationAbort { .. }
                | Self::FileConversationClose { .. }
                | Self::TodoChildStatus { .. }
                | Self::TodoChildDetail { .. }
        )
    }
}
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename = "extension_ui_response")]
pub(crate) struct RpcExtensionUiResponse {
    id: String,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    confirmed: Option<bool>,
    #[serde(default)]
    cancelled: Option<bool>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueCancelScope {
    #[default]
    All,
    Steering,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RpcSessionListScope {
    #[default]
    Current,
    AllProjects,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]

pub struct RpcSessionState {
    pub model: Option<Model>,
    pub active_turn_model: Option<Model>,
    pub thinking_level: ThinkingLevel,
    pub is_streaming: bool,
    pub is_compacting: bool,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub session_file: Option<String>,
    pub session_id: Option<String>,
    pub session_name: Option<String>,
    pub cwd: String,
    pub auto_compaction_enabled: bool,
    pub message_count: usize,
    pub pending_message_count: usize,
    #[serde(default)]
    pub skipped_idless: usize,
    #[serde(default)]
    pub undecodable_messages: usize,
    #[serde(default)]
    pub queued_steering: Vec<String>,
    pub todo_phases: Vec<TodoPhase>,
    pub todo_execution_status: pi_coding::TodoDagExecutionStatus,
    pub todo_failure_reason: Option<pi_coding::TodoDagFailureReason>,
    pub goal: GoalState,
    pub runtime_settings: pi_coding::RuntimeSettingsState,
}
impl RpcSessionState {
    pub(crate) fn from_application(
        s: ApplicationState,
        active_turn_model: Option<Model>,
        runtime_settings: pi_coding::RuntimeSettingsState,
        cwd: &Path,
        session_id_override: Option<String>,
    ) -> Self {
        Self {
            model: s.model.map(public_model),
            active_turn_model: active_turn_model.map(public_model),
            thinking_level: s.thinking_level,
            is_streaming: s.is_streaming,
            is_compacting: s.is_compacting,
            steering_mode: s.steering_mode,
            follow_up_mode: s.follow_up_mode,
            session_file: s.session_file,
            session_id: session_id_override.or(s.session_id),
            session_name: s.session_name,
            cwd: cwd.to_string_lossy().into_owned(),
            auto_compaction_enabled: s.auto_compaction_enabled,
            message_count: s.message_count,
            pending_message_count: s.pending_message_count,
            skipped_idless: s.skipped_idless,
            undecodable_messages: s.undecodable_messages,
            queued_steering: s.queued_steering,
            todo_phases: s.todo_phases,
            todo_execution_status: s.todo_execution_status,
            todo_failure_reason: s.todo_failure_reason,
            goal: s.goal,
            runtime_settings,
        }
    }
}
/// One session row of the `session_list` response — the RPC-visible subset of
/// the resume catalog's `ResumeSelectorRow` (search/message corpora are
/// internal and never leave the process).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSessionListRow {
    pub source: &'static str,
    pub session_id: String,
    pub name: Option<String>,
    pub cwd: String,
    pub display_time: String,
    pub modified_epoch: f64,
    pub summary: String,
    pub path: String,
    pub size: u64,
    pub message_count: Option<usize>,
    /// `native` | `foreign` | `already_imported`. An `already_imported` row
    /// is a foreign source whose native imported copy also exists (both stay
    /// listed — product decision); the sidebar badges it and resolves the
    /// copy via `nativePath`.
    pub status: String,
    /// The native imported copy's path, carried only by `already_imported`
    /// rows so the frontend can resolve/badge the import.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_path: Option<String>,
    /// The native imported copy's authoritative session id. Present only with
    /// `already_imported`; unlike the file name, this is the runtime identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
    /// True when the row is an unnamed, tiny, native Pi session whose cwd
    /// sits lexically under the OS temp root (historical test-harness
    /// shape). The Web sidebar hides such rows by default but keeps them
    /// searchable, and loaded/active sessions always stay visible; this is a
    /// recoverable view signal, never a backend deletion. Computed for the
    /// Web AllProjects scope; Current-scope rows always report false (the
    /// current workspace is by definition in active use).
    pub temporary: bool,
    pub agent_session: bool,
    pub bootstrap_only: bool,
    pub session_kind: &'static str,
}

impl RpcSessionListRow {
    pub(crate) fn from_resume_row(row: crate::resume_catalog::ResumeSelectorRow, temporary: bool) -> Self {
        let (status, native_path, native_session_id) = match &row.status {
            pi_coding::CatalogRowStatus::Native => ("native", None, None),
            pi_coding::CatalogRowStatus::Foreign => ("foreign", None, None),
            pi_coding::CatalogRowStatus::AlreadyImported {
                native_id,
                native_path,
            } => (
                "already_imported",
                Some(native_path.to_string_lossy().into_owned()),
                Some(native_id.clone()),
            ),
        };
        Self {
            source: row.source.as_str(),
            session_id: row.session_id,
            name: row.name,
            cwd: row.cwd.to_string_lossy().into_owned(),
            display_time: row.display_time,
            modified_epoch: row.modified_epoch,
            summary: row.summary,
            path: row.path.to_string_lossy().into_owned(),
            size: row.size,
            message_count: row.message_count,
            status: status.to_owned(),
            native_path,
            native_session_id,
            temporary,
            agent_session: row.agent_session,
            bootstrap_only: row.bootstrap_only,
            session_kind: row.session_kind.header_value(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RpcResponse {
    pub id: Option<String>,
    pub record_type: &'static str,
    pub command: String,
    pub session_id: Option<String>,
    pub success: bool,
    pub data: Option<Value>,
    pub error: Option<String>,
    /// Pre-validated JSON for `data`, bypassing the recursive `Value` tree.
    /// When present, `data` must be `None`; the manual `Serialize` impl emits
    /// this value verbatim as `"data"` without recursive `Value` construction.
    pub raw_data: Option<Box<serde_json::value::RawValue>>,
}

impl Serialize for RpcResponse {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        if let Some(id) = &self.id {
            map.serialize_entry("id", id)?;
        }
        map.serialize_entry("type", self.record_type)?;
        map.serialize_entry("command", &self.command)?;
        if let Some(session_id) = &self.session_id {
            map.serialize_entry("sessionId", session_id)?;
        }
        map.serialize_entry("success", &self.success)?;
        if self.raw_data.is_some() && self.data.is_some() {
            return Err(serde::ser::Error::custom(
                "RPC response cannot contain both typed and raw data",
            ));
        }
        if let Some(raw) = &self.raw_data {
            map.serialize_entry("data", raw)?;
        } else if let Some(data) = &self.data {
            map.serialize_entry("data", data)?;
        }
        if let Some(error) = &self.error {
            map.serialize_entry("error", error)?;
        }
        map.end()
    }
}

impl RpcResponse {
    pub(crate) fn success(
        id: Option<String>,
        command: impl Into<String>,
        data: Option<Value>,
    ) -> Self {
        Self {
            id,
            record_type: "response",
            command: command.into(),
            session_id: None,
            success: true,
            data,
            error: None,
            raw_data: None,
        }
    }
    pub(crate) fn success_raw(
        id: Option<String>,
        command: impl Into<String>,
        raw_json: String,
    ) -> serde_json::Result<Self> {
        Ok(Self {
            id,
            record_type: "response",
            command: command.into(),
            session_id: None,
            success: true,
            data: None,
            error: None,
            raw_data: Some(serde_json::value::RawValue::from_string(raw_json)?),
        })
    }
    pub fn failure(
        id: Option<String>,
        command: impl Into<String>,
        error: impl Into<String>,
    ) -> Self {
        Self {
            id,
            record_type: "response",
            command: command.into(),
            session_id: None,
            success: false,
            data: None,
            error: Some(error.into()),
            raw_data: None,
        }
    }

    pub(crate) fn with_error_code(mut self, code: &'static str) -> Self {
        self.data = Some(json!({ "code": code }));
        self
    }

    pub(crate) fn slot_overload(
        id: Option<String>,
        command: impl Into<String>,
        error: impl Into<String>,
    ) -> Self {
        Self::failure(id, command, error).with_error_code("busy_capacity")
    }

    pub(crate) fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }
    pub(crate) fn is_transient_slot_overload(&self) -> bool {
        !self.success
            && self.data.as_ref().is_some_and(|data| {
                data.get("code").and_then(|code| code.as_str()) == Some("busy_capacity")
            })
    }
}
#[derive(Clone, Debug, Serialize)]
pub(crate) struct RpcExtensionUiRequest {
    #[serde(rename = "type")]
    record_type: &'static str,
    id: String,
    /// Owning session of the extension that produced this request; injected
    /// by the session runtime manager's fan-in forwarder.
    #[serde(rename = "sessionId", skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<String>,
    #[serde(rename = "runtimeIncarnation", skip_serializing_if = "Option::is_none")]
    pub(crate) runtime_incarnation: Option<String>,
    #[serde(flatten)]
    request: Value,
}

impl RpcExtensionUiRequest {
    /// Error-notice frame for projection/lag failures, tagged with the owning
    /// session when known.
    pub(crate) fn error_notice(session_id: Option<String>, message: String) -> Self {
        Self {
            record_type: "extension_ui_request",
            id: uuid::Uuid::now_v7().to_string(),
            session_id,
            runtime_incarnation: None,
            request: json!({
                "method": "notify",
                "message": message,
                "notifyType": "error",
            }),
        }
    }

    pub(crate) fn error_notice_with_identity(
        identity: Option<RuntimeIdentity>,
        message: String,
    ) -> Self {
        Self {
            record_type: "extension_ui_request",
            id: uuid::Uuid::now_v7().to_string(),
            session_id: identity.as_ref().map(|identity| identity.session_id.clone()),
            runtime_incarnation: identity.map(|identity| identity.runtime_incarnation),
            request: json!({
                "method": "notify",
                "message": message,
                "notifyType": "error",
            }),
        }
    }
}
#[derive(Clone, Debug)]
enum JsonlFrame {
    Line(Vec<u8>),
    Oversized,
    Unterminated,
}
#[derive(Clone, Debug)]
pub(crate) enum RpcInput {
    Command {
        command: RpcCommand,
        /// Optional top-level `sessionId` selecting a runtime on the Web
        /// control plane. Absent targets the initial runtime.
        session_id: Option<String>,
    },
    ExtensionUiResponse(RpcExtensionUiResponse),
}

#[derive(Clone)]
pub struct RpcDispatcher {
    application: Application,
    settings: crate::settings_rpc::SettingsRpcState,
    workflows: crate::workflow_rpc::WorkflowRpcState,
    command_slots: Arc<Semaphore>,
    side_chat: SideChatRpcState,
    code_review: CodeReviewRpcState,
    file_conversations: FileConversationRpcState,
    session_storage: Option<crate::session_run::SessionStorage>,
}

pub(crate) async fn handle_tmux_command(command: RpcCommand) -> RpcResponse {
    let id = command.id();
    let name = command.command_name();
    let result = tokio::task::spawn_blocking(move || execute_tmux_command(command)).await;
    match result {
        Ok(Ok(data)) => RpcResponse::success(id, name, Some(data)),
        Ok(Err(error)) => RpcResponse::failure(id, name, error),
        Err(error) if error.is_cancelled() => {
            RpcResponse::failure(id, name, "tmux operation was cancelled")
        }
        Err(_) => RpcResponse::failure(id, name, "tmux operation failed"),
    }
}

fn execute_tmux_command(command: RpcCommand) -> Result<Value, String> {
    match command {
        RpcCommand::TmuxListSessions { .. } => {
            crate::modes::tmux::list_sessions().and_then(|data| {
                serde_json::to_value(data)
                    .map_err(|error| format!("tmux list-sessions serialize failed: {error}"))
            })
        }
        RpcCommand::TmuxPaneSnapshot {
            session, lines, ..
        } => crate::modes::tmux::pane_snapshot(&session, lines).and_then(|data| {
            serde_json::to_value(data)
                .map_err(|error| format!("tmux pane snapshot serialize failed: {error}"))
        }),
        RpcCommand::TmuxSendKeys {
            session, text, key, ..
        } => crate::modes::tmux::send_keys(&session, text.as_deref(), key.as_deref()).and_then(
            |data| {
                serde_json::to_value(data)
                    .map_err(|error| format!("tmux send-keys serialize failed: {error}"))
            },
        ),
        RpcCommand::TmuxSessionCreate { session, .. } => {
            crate::modes::tmux::create_session(&session).and_then(|data| {
                serde_json::to_value(data)
                    .map_err(|error| format!("tmux session create serialize failed: {error}"))
            })
        }
        RpcCommand::TmuxSessionDelete { session, .. } => {
            crate::modes::tmux::delete_session(&session).and_then(|data| {
                serde_json::to_value(data)
                    .map_err(|error| format!("tmux session delete serialize failed: {error}"))
            })
        }
        _ => Err("tmux command routing mismatch".to_owned()),
    }
}

impl RpcDispatcher {
    #[must_use]
    pub fn new(application: Application) -> Self {
        Self::new_with_session_storage(application, None)
    }

    #[must_use]
    pub(crate) fn new_with_session_storage(
        application: Application,
        session_storage: Option<crate::session_run::SessionStorage>,
    ) -> Self {
        Self {
            settings: crate::settings_rpc::SettingsRpcState::default(),
            workflows: crate::workflow_rpc::WorkflowRpcState::for_application(&application),
            application,
            command_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_COMMANDS)),
            side_chat: SideChatRpcState::default(),
            code_review: CodeReviewRpcState::default(),
            file_conversations: FileConversationRpcState::default(),
            session_storage,
        }
    }

    pub fn application(&self) -> &Application {
        &self.application
    }
    /// Dispatch a command with this dispatcher's full state. Side-chat and
    /// code-review commands live outside the settings/workflows handler
    /// because their controller state is owned by the RPC session, not by the
    /// application.
    pub async fn dispatch_inner(&self, command: RpcCommand) -> RpcResponse {
        if matches!(
            command,
            RpcCommand::TmuxListSessions { .. }
                | RpcCommand::TmuxPaneSnapshot { .. }
                | RpcCommand::TmuxSendKeys { .. }
                | RpcCommand::TmuxSessionCreate { .. }
                | RpcCommand::TmuxSessionDelete { .. }
        ) {
            return handle_tmux_command(command).await;
        }
        if matches!(
            command,
            RpcCommand::TmuxTerminalAttach { .. }
                | RpcCommand::TmuxTerminalInput { .. }
                | RpcCommand::TmuxTerminalResize { .. }
                | RpcCommand::TmuxTerminalDetach { .. }
        ) {
            return RpcResponse::failure(
                command.id(),
                command.command_name(),
                "tmux terminal commands require the listen control plane",
            );
        }
        if matches!(
            command,
            RpcCommand::SideChatNew { .. }
                | RpcCommand::SideChatSwitch { .. }
                | RpcCommand::SideChatClose { .. }
                | RpcCommand::SideChatPrompt { .. }
                | RpcCommand::SideChatList { .. }
                | RpcCommand::SideChatAbort { .. }
        ) {
            return handle_side_chat_command(&self.application, &self.side_chat, command).await;
        }
        if matches!(
            command,
            RpcCommand::CodeReviewOpen { .. }
                | RpcCommand::CodeReviewCommits { .. }
                | RpcCommand::CodeReviewSnapshot { .. }
                | RpcCommand::CodeReviewRefresh { .. }
                | RpcCommand::CodeReviewAutoReview { .. }
                | RpcCommand::CodeReviewHighlight { .. }
                | RpcCommand::CodeReviewTldr { .. }
                | RpcCommand::CodeReviewStop { .. }
                | RpcCommand::CodeReviewComment { .. }
                | RpcCommand::CodeReviewSetModel { .. }
                | RpcCommand::CodeReviewAbort { .. }
                | RpcCommand::CodeReviewClose { .. }
                | RpcCommand::CodeReviewFileDiff { .. }
                | RpcCommand::CodeReviewFileContext { .. }
        ) {
            return handle_code_review_command(&self.application, &self.code_review, command).await;
        }
        if matches!(
            command,
            RpcCommand::FileExplorerList { .. }
                | RpcCommand::FileExplorerSearch { .. }
                | RpcCommand::FileExplorerRead { .. }
                | RpcCommand::FileExplorerChanges { .. }
                | RpcCommand::FileExplorerDiff { .. }
                | RpcCommand::FileExplorerBlame { .. }
                | RpcCommand::FileExplorerDelete { .. }
                | RpcCommand::FileExplorerMap { .. }
                | RpcCommand::PathComplete { .. }
        ) {
            return handle_file_explorer_command(&self.application, command).await;
        }
        if matches!(
            command,
            RpcCommand::FileConversationOpen { .. }
                | RpcCommand::FileConversationPrompt { .. }
                | RpcCommand::FileConversationSnapshot { .. }
                | RpcCommand::FileConversationAbort { .. }
                | RpcCommand::FileConversationClose { .. }
        ) {
            return handle_file_conversation_command(
                &self.application,
                &self.file_conversations,
                command,
            )
            .await;
        }
        // stdio runtime-owner replacement (new_session/switch_session/fork/clone):
        // hold the collection mutex across the whole dispatch, tearing the
        // conversations down first so no open can capture an owner or fork mid-swap.
        if matches!(
            command,
            RpcCommand::NewSession { .. }
                | RpcCommand::SwitchSession { .. }
                | RpcCommand::Fork { .. }
                | RpcCommand::Clone { .. }
        ) {
            let mut file_conversation_fence = self.file_conversations.lock().await;
            file_conversation_fence.shutdown().await;
            return handle_command_with_session_storage(
                &self.application,
                &self.settings,
                &self.workflows,
                self.session_storage.as_ref(),
                command,
            )
            .await;
        }
        handle_command_with_session_storage(
            &self.application,
            &self.settings,
            &self.workflows,
            self.session_storage.as_ref(),
            command,
        )
        .await
    }

    pub(crate) async fn dispatch(&self, command: RpcCommand) -> RpcResponse {
        if command.bypasses_command_slots() {
            return self.dispatch_inner(command).await;
        }
        let id = command.id();
        let name = command.command_name();
        let Ok(_slot) = self.command_slots.clone().try_acquire_owned() else {
            return RpcResponse::slot_overload(
                id,
                name,
                format!("too many concurrent RPC commands (limit {MAX_CONCURRENT_COMMANDS})"),
            );
        };
        self.dispatch_inner(command).await
    }

    #[cfg(test)]
    pub(crate) fn acquire_all_command_slots_for_test(
        &self,
    ) -> Vec<tokio::sync::OwnedSemaphorePermit> {
        (0..MAX_CONCURRENT_COMMANDS)
            .map(|_| {
                self.command_slots
                    .clone()
                    .try_acquire_owned()
                    .expect("command slot")
            })
            .collect()
    }

    /// Shut down the RPC-owned side-chat tab container (session close /
    /// listener shutdown).
    pub(crate) async fn shutdown_side_chat(&self) {
        self.side_chat.shutdown().await;
    }

    /// Whether the RPC-owned side-chat container has a tab currently
    /// streaming a reply (part of the conservative close busy check).
    pub(crate) async fn side_chat_busy(&self) -> bool {
        self.side_chat.is_streaming().await
    }

    /// Shut down the RPC-owned code-review controller (session close /
    /// listener shutdown).
    pub(crate) async fn shutdown_code_review(&self) {
        self.code_review.shutdown().await;
    }

    /// Whether the RPC-owned code-review controller still owns cancellable
    /// work (part of the conservative close busy check).
    pub(crate) async fn code_review_busy(&self) -> bool {
        self.code_review.is_busy().await
    }

    pub(crate) async fn file_conversation_busy(
        &self,
    ) -> Result<bool, crate::file_conversation::FileConversationError> {
        let mut engine = self.file_conversations.lock().await;
        FileConversationRpcState::busy_with(&mut engine, &self.application).await
    }

    pub(crate) async fn lock_file_conversations(
        &self,
    ) -> tokio::sync::MutexGuard<'_, crate::file_conversation::FileConversationEngine> {
        self.file_conversations.lock().await
    }

    pub(crate) async fn shutdown_file_conversations(&self) {
        self.file_conversations.lock().await.shutdown().await;
    }
}

/// RPC-owned side-chat controller state.
///
/// The `/btw` tabs are TUI-internal controller state (fork + agent + per-tab
/// transcript), so the RPC session owns a lazily-created [`SideChatTabs`]
/// container. Every side-chat command serializes through one mutex; prompts
/// never touch the main session, mirroring the TUI surface exactly.
#[derive(Clone, Default)]
pub(crate) struct SideChatRpcState {
    tabs: Arc<tokio::sync::Mutex<Option<crate::side_chat::SideChatTabs>>>,
}

impl SideChatRpcState {
    /// Lock the tab container, lazily creating the legacy `default` tab on
    /// first use, and run a synchronous closure over it.
    async fn with_tabs<T>(
        &self,
        app: &Application,
        f: impl FnOnce(&mut crate::side_chat::SideChatTabs) -> Result<T>,
    ) -> Result<T> {
        let mut guard = self.lock_tabs(app).await?;
        f(guard.as_mut().expect("side-chat tabs initialized"))
    }

    /// Lock the tab container, lazily creating the legacy `default` tab on
    /// first use. Async callers (fork/close) operate on the guard directly
    /// and must drop it before re-entering [`SideChatRpcState::snapshot`].
    async fn lock_tabs(
        &self,
        app: &Application,
    ) -> Result<tokio::sync::MutexGuard<'_, Option<crate::side_chat::SideChatTabs>>> {
        let mut guard = self.tabs.lock().await;
        if guard.is_none() {
            *guard = Some(crate::side_chat::SideChatTabs::new_default(app).await?);
        }
        Ok(guard)
    }

    /// Snapshot every tab after draining pending agent events, so a streamed
    /// reply that finished between polls is reflected immediately.
    async fn snapshot(&self, app: &Application) -> Result<Value> {
        self.with_tabs(app, |tabs| {
            tabs.poll_events();
            Ok(side_chat_snapshot(tabs))
        })
        .await
    }

    /// Shut down every tab's agent/subscription (RPC session exit).
    async fn shutdown(&self) {
        let mut guard = self.tabs.lock().await;
        if let Some(tabs) = guard.as_mut() {
            tabs.shutdown().await;
        }
    }

    /// Whether any tab is currently streaming a reply (close busy check).
    /// Drain completion events first: the controller's streaming flag is
    /// event-driven, so inspecting it without polling can leave an idle tab
    /// permanently classified as busy after its task has finished.
    async fn is_streaming(&self) -> bool {
        let mut guard = self.tabs.lock().await;
        let Some(tabs) = guard.as_mut() else {
            return false;
        };
        tabs.poll_events();
        tabs.tabs().any(|(_, controller)| controller.is_streaming())
    }
}

#[derive(Clone, Default)]
pub(crate) struct FileConversationRpcState {
    engine: Arc<tokio::sync::Mutex<crate::file_conversation::FileConversationEngine>>,
}

impl FileConversationRpcState {
    pub(crate) async fn lock(
        &self,
    ) -> tokio::sync::MutexGuard<'_, crate::file_conversation::FileConversationEngine> {
        self.engine.lock().await
    }
    pub(crate) async fn busy_with(
        engine: &mut crate::file_conversation::FileConversationEngine,
        application: &Application,
    ) -> Result<bool, crate::file_conversation::FileConversationError> {
        engine.is_busy(application).await
    }
}

/// Strict selection: absent means none; an explicit `null` or non-conforming object
/// is a typed `file_conversation_selection_invalid` rejection (unknown fields ignored).
fn parse_file_conversation_selection(
    selection: &Option<Value>,
) -> Result<Option<crate::file_conversation::FileConversationLineRange>,
    crate::file_conversation::FileConversationError>
{
    use crate::file_conversation::{FileConversationError, FileConversationLineRange};
    let Some(value) = selection else {
        return Ok(None);
    };
    if value.is_null() {
        return Err(FileConversationError::SelectionInvalid {
            start_line: 0,
            end_line: 0,
        });
    }
    let start_line = value.get("startLine").and_then(Value::as_u64);
    let end_line = value.get("endLine").and_then(Value::as_u64);
    let (Some(start_line), Some(end_line)) = (start_line, end_line) else {
        return Err(FileConversationError::SelectionInvalid {
            start_line: 0,
            end_line: 0,
        });
    };
    if start_line == 0 || end_line < start_line {
        return Err(FileConversationError::SelectionInvalid {
            start_line: usize::try_from(start_line).unwrap_or(usize::MAX),
            end_line: usize::try_from(end_line).unwrap_or(usize::MAX),
        });
    }
    Ok(Some(FileConversationLineRange {
        start_line: usize::try_from(start_line).unwrap_or(usize::MAX),
        end_line: usize::try_from(end_line).unwrap_or(usize::MAX),
    }))
}

async fn handle_file_conversation_command(
    app: &Application,
    state: &FileConversationRpcState,
    c: RpcCommand,
) -> RpcResponse {
    let id = c.id();
    let name = c.command_name();
    let conversation_id = match &c {
        RpcCommand::FileConversationOpen { conversation_id, .. }
        | RpcCommand::FileConversationPrompt { conversation_id, .. }
        | RpcCommand::FileConversationSnapshot { conversation_id, .. }
        | RpcCommand::FileConversationAbort { conversation_id, .. }
        | RpcCommand::FileConversationClose { conversation_id, .. } => conversation_id.clone(),
        _ => return RpcResponse::failure(id, name, "file_conversation command routing mismatch"),
    };
    let mut engine = state.lock().await;
    let result = match c {
        RpcCommand::FileConversationOpen { path, .. } => engine
            .open(app, &conversation_id, &path)
            .await
            .map(|snapshot| snapshot.to_wire_json()),
        RpcCommand::FileConversationPrompt {
            message, selection, ..
        } => match parse_file_conversation_selection(&selection) {
            Ok(selection) => engine
                .prompt(app, &conversation_id, &message, selection)
                .await
                .map(|ack| ack.to_wire_json()),
            Err(error) => Err(error),
        },
        RpcCommand::FileConversationSnapshot { .. } => engine
            .snapshot(app, &conversation_id)
            .await
            .map(|snapshot| snapshot.to_wire_json()),
        RpcCommand::FileConversationAbort { .. } => engine
            .abort(app, &conversation_id)
            .await
            .map(|snapshot| snapshot.to_wire_json()),
        RpcCommand::FileConversationClose { .. } => engine.close(app, &conversation_id).await,
        _ => return RpcResponse::failure(id, name, "file_conversation command routing mismatch"),
    };
    match result {
        Ok(data) => RpcResponse::success(id, name, Some(data)),
        Err(error) => {
            let mut response = RpcResponse::failure(id, name, error.message());
            response.data = Some(error.failure_data(&conversation_id));
            response
        }
    }
}

/// Build the wire snapshot of the whole tab container.
fn side_chat_snapshot(tabs: &crate::side_chat::SideChatTabs) -> Value {
    let tab_values = tabs
        .tabs()
        .map(|(name, controller)| {
            let entries = controller
                .entries()
                .iter()
                .map(|entry| entry.to_wire_json())
                .collect::<Vec<_>>();
            json!({
                "name": name,
                "streaming": controller.is_streaming(),
                "status": controller.status(),
                "entries": entries,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "active": tabs.active_name(),
        "maxTabs": crate::side_chat::SideChatTabs::max_tabs(),
        "tabs": tab_values,
    })
}

/// Dispatch a side-chat RPC command against the RPC-owned tab container.
async fn handle_side_chat_command(
    app: &Application,
    side_chat: &SideChatRpcState,
    c: RpcCommand,
) -> RpcResponse {
    let id = c.id();
    let name = c.command_name();
    let result: Result<Value> = async {
        match c {
            RpcCommand::SideChatList { .. } => side_chat.snapshot(app).await,
            RpcCommand::SideChatNew { name, .. } => {
                {
                    let mut guard = side_chat.lock_tabs(app).await?;
                    guard
                        .as_mut()
                        .expect("side-chat tabs initialized")
                        .new_tab(app, &name)
                        .await?;
                }
                side_chat.snapshot(app).await
            }
            RpcCommand::SideChatSwitch { name, .. } => {
                side_chat
                    .with_tabs(app, |tabs| tabs.switch_to(&name))
                    .await?;
                side_chat.snapshot(app).await
            }
            RpcCommand::SideChatClose { name, .. } => {
                let target = match &name {
                    Some(name) => name.clone(),
                    None => {
                        side_chat
                            .with_tabs(app, |tabs| Ok(tabs.active_name().to_owned()))
                            .await?
                    }
                };
                {
                    let mut guard = side_chat.lock_tabs(app).await?;
                    guard
                        .as_mut()
                        .expect("side-chat tabs initialized")
                        .close_tab(app, &target)
                        .await?;
                }
                side_chat.snapshot(app).await
            }
            RpcCommand::SideChatPrompt { message, images, .. } => {
                let accepted = side_chat
                    .with_tabs(app, |tabs| {
                        if tabs.is_streaming() {
                            return Ok(false);
                        }
                        tabs.submit_prompt(&message, images);
                        Ok(true)
                    })
                    .await?;
                let mut snapshot = side_chat.snapshot(app).await?;
                if let Some(object) = snapshot.as_object_mut() {
                    object.insert("accepted".to_owned(), json!(accepted));
                    object.insert("busy".to_owned(), json!(!accepted));
                }
                Ok(snapshot)
            }
            RpcCommand::SideChatAbort { .. } => {
                {
                    let mut guard = side_chat.lock_tabs(app).await?;
                    guard
                        .as_mut()
                        .expect("side-chat tabs initialized")
                        .abort_streaming()
                        .await;
                }
                side_chat.snapshot(app).await
            }
            _ => unreachable!("dispatch_inner only routes side_chat_* commands here"),
        }
    }
    .await;
    match result {
        Ok(data) => RpcResponse::success(id, name, Some(data)),
        Err(e) => RpcResponse::failure(id, name, e.to_string()),
    }
}
const FILE_DIFF_CACHE_MAX_ENTRIES: usize = 32;
const FILE_DIFF_CACHE_MAX_BYTES: usize = 32 * 1024 * 1024;

const FILE_DIFF_CACHE_ENTRY_OVERHEAD_BYTES: usize = 64;

/// Weighted LRU capped by entry count and byte budget; an oversize diff is never stored (served uncached).
struct FileDiffCache {
    entries: HashMap<String, FileDiffCacheEntry>,
    bytes: usize,
    next_recency: u64,
    max_entries: usize,
    max_bytes: usize,
}

struct FileDiffCacheEntry {
    recency: u64,
    bytes: usize,
    diff: Arc<FileDiff>,
}

impl Default for FileDiffCache {
    fn default() -> Self {
        Self::new(FILE_DIFF_CACHE_MAX_ENTRIES, FILE_DIFF_CACHE_MAX_BYTES)
    }
}

impl FileDiffCache {
    fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            bytes: 0,
            next_recency: 0,
            max_entries,
            max_bytes,
        }
    }

    fn get(&mut self, path: &str) -> Option<Arc<FileDiff>> {
        let entry = self.entries.get_mut(path)?;
        self.next_recency = self.next_recency.wrapping_add(1);
        entry.recency = self.next_recency;
        Some(entry.diff.clone())
    }

    fn peek(&self, path: &str) -> Option<&Arc<FileDiff>> {
        self.entries.get(path).map(|entry| &entry.diff)
    }

    fn put(&mut self, path: String, diff: Arc<FileDiff>) -> bool {
        let bytes =
            diff.estimated_heap_bytes() + path.capacity() + FILE_DIFF_CACHE_ENTRY_OVERHEAD_BYTES;
        if bytes > self.max_bytes {
            return false;
        }
        if let Some(old) = self.entries.remove(&path) {
            self.bytes = self.bytes.saturating_sub(old.bytes);
        }
        self.next_recency = self.next_recency.wrapping_add(1);
        self.entries.insert(
            path,
            FileDiffCacheEntry {
                recency: self.next_recency,
                bytes,
                diff,
            },
        );
        self.bytes += bytes;
        while self.entries.len() > self.max_entries || self.bytes > self.max_bytes {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.recency)
                .map(|(path, _)| path.clone())
            else {
                break;
            };
            if let Some(entry) = self.entries.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(entry.bytes);
            }
        }
        true
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
        self.next_recency = 0;
    }
}

/// RPC-owned code-review controller + snapshot state.
///
/// `/code-review` is fullscreen controller state (Main-equivalent governed
/// tools + per-hunk threads), so the RPC session owns a lazily-created
/// [`CodeReviewController`] paired with the last loaded [`ReviewSnapshot`].
/// Controller mutations serialize through one mutex; blocking Git capture
/// runs outside it so snapshot and teardown observe the authoritative state.
/// State is per-`RpcDispatcher` (per-session), so two Web sessions never
/// share a review controller.
struct CodeReviewSession {
    controller: CodeReviewController,
    snapshot: ReviewSnapshot,
    paging: ReviewPagingState,
    file_diff_cache: FileDiffCache,
    file_diff_inflight: HashMap<String, Arc<FileDiffLoadFlight>>,
    file_context_cache: HashMap<String, Arc<FileContextBlobs>>,
    comment_authority: CommentAuthority,
}

/// Retained-record bound per open review; an overflowing path fails closed
/// ([`CommentAuthority::record`]) with every served page hunk complete=false.
const MAX_COMMENT_AUTHORITY_HUNKS: usize = 4_096;

#[derive(Debug)]
struct HunkAuthorityRecord {
    old_start: u32,
    old_count: u32,
    new_start: u32,
    new_count: u32,
    content_hash: String,
    comment_ready: bool,
}

#[derive(Default)]
struct CommentAuthority {
    records: HashMap<String, Vec<HunkAuthorityRecord>>,
    overflow: std::collections::HashSet<String>,
    total: usize,
}

impl CommentAuthority {
    fn record(&mut self, path: &str, diff: &FileDiff) -> bool {
        if self.overflow.contains(path) {
            return false;
        }
        let old = self.records.remove(path);
        if let Some(old) = &old {
            self.total -= old.len();
        }
        if self.total + diff.hunks.len() > MAX_COMMENT_AUTHORITY_HUNKS {
            self.overflow.insert(path.to_owned());
            return false;
        }
        let records: Vec<HunkAuthorityRecord> = diff
            .hunks
            .iter()
            .enumerate()
            .map(|(index, hunk)| HunkAuthorityRecord {
                old_start: hunk.old_start,
                old_count: hunk.old_count,
                new_start: hunk.new_start,
                new_count: hunk.new_count,
                content_hash: HunkIdentity::new("", path, hunk).content_hash,
                comment_ready: diff.hunk_is_comment_ready(index),
            })
            .collect();
        self.total += records.len();
        self.records.insert(path.to_owned(), records);
        true
    }

    fn lookup(&self, path: &str) -> Option<&[HunkAuthorityRecord]> {
        self.records.get(path).map(Vec::as_slice)
    }

    fn overflowed(&self, path: &str) -> bool {
        self.overflow.contains(path)
    }
}

struct ServedFileDiff {
    diff: Arc<FileDiff>,
    authority_complete: bool,
}

/// Per-path single-flight slot: the owner publishes exactly one terminal
/// outcome that every queued joiner consumes instead of re-running the load.
struct FileDiffLoadFlight {
    slot: tokio::sync::Mutex<()>,
    outcome: tokio::sync::OnceCell<anyhow::Result<ServedFileDiff>>,
}
#[derive(Default)]
struct CodeReviewState {
    session: Option<CodeReviewSession>,
    /// Pending-capture token. Open/refresh reserve the next value before
    /// capture. A later open/refresh, an unstamped close, session shutdown,
    /// or a stamped close that still owns `installed_authority` advances it
    /// so that capture cannot install. A stamped close of a review whose
    /// generation is no longer installed leaves this token alone.
    authority: u64,
    /// Generation owned by the controller currently in `session`. A pending
    /// open reserves a newer `authority` without taking this generation
    /// until cutover, so closing the still-installed review cannot cancel it.
    installed_authority: u64,
}

#[cfg(test)]
struct ReviewCaptureGate {
    captured: tokio::sync::Notify,
    released: std::sync::Mutex<bool>,
    release: std::sync::Condvar,
}

#[cfg(test)]
impl ReviewCaptureGate {
    fn hold_capture(&self) {
        self.captured.notify_one();
        let mut released = self
            .released
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while !*released {
            released = self
                .release
                .wait(released)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    fn release(&self) {
        *self
            .released
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
        self.release.notify_all();
    }
}

#[cfg(test)]
struct FileDiffLoadGate {
    loaded: tokio::sync::Barrier,
    release: tokio::sync::Barrier,
}

#[derive(Clone, Default)]
pub(crate) struct CodeReviewRpcState {
    state: Arc<tokio::sync::Mutex<CodeReviewState>>,
    #[cfg(test)]
    review_capture_gate: Arc<tokio::sync::Mutex<Option<Arc<ReviewCaptureGate>>>>,
    #[cfg(test)]
    file_diff_load_gate: Arc<tokio::sync::Mutex<Option<Arc<FileDiffLoadGate>>>>,
    #[cfg(test)]
    file_diff_load_count: Arc<std::sync::atomic::AtomicUsize>,
}

impl CodeReviewRpcState {
    /// Open a bounded HEAD→working-tree, base→working-tree, two-revision, or
    /// pinned-commit diff snapshot and fork a fresh review agent with the
    /// owning Main session's exact active governed tools. Any previously
    /// open review remains authoritative during capture and shuts down
    /// after the validated cutover.
    async fn open(
        &self,
        app: &Application,
        commit: Option<String>,
        from: Option<String>,
        to: Option<String>,
        include_working_tree: Option<bool>,
    ) -> Result<Value> {
        let scope = match (commit, from, to, include_working_tree) {
            (Some(commit), None, None, None) => {
                if commit.trim().is_empty() {
                    bail!("code_review_open `commit` must be a non-empty revision");
                }
                // The capture resolves the revision once and fills the
                // label; refreshes reuse that resolved hash.
                ReviewScope::Commit {
                    commit,
                    label: String::new(),
                }
            }
            (Some(_), _, _, _) => bail!(
                "code_review_open commit excludes from, to, and includeWorkingTree"
            ),
            (None, None, None, None | Some(false)) => ReviewScope::WorkingTree,
            (None, Some(base), None, Some(true)) => ReviewScope::BaseToWorkingTree { base },
            (None, Some(from), Some(to), None | Some(false)) => {
                ReviewScope::Revisions { from, to }
            }
            (None, Some(_), None, _) | (None, None, Some(_), _) => bail!(
                "code_review_open requires `from` and `to` together, `from` with includeWorkingTree, or neither"
            ),
            (None, Some(_), Some(_), _) | (None, None, None, Some(true)) => bail!(
                "code_review_open includeWorkingTree requires `from` alone (no `to`)"
            ),
        };
        // Reserve the pending-capture token before capture while leaving the
        // previous session installed and responsive. A later open/refresh, an
        // unstamped close, shutdown, or a close of the review that still owns
        // the installed generation makes this capture stale. Closing the
        // review this reservation replaces does not.
        let authority = {
            let mut state = self.state.lock().await;
            state.authority = state.authority.wrapping_add(1);
            state.authority
        };
        let cwd = app.session().cwd().to_path_buf();
        #[cfg(test)]
        let capture_gate = self.review_capture_gate.lock().await.clone();
        // Git acquisition is blocking and must run off the async runtime.
        let (snapshot, paging) = tokio::task::spawn_blocking(move || {
            let captured = load_review_snapshot_capture(&cwd, scope);
            #[cfg(test)]
            if let Some(gate) = capture_gate {
                gate.hold_capture();
            }
            captured
        })
        .await
        .map_err(|e| anyhow!("code review snapshot load failed: {e}"))?;
        let mut controller = Some(CodeReviewController::fork_from(app).await?);
        controller
            .as_mut()
            .expect("fresh controller")
            .reconcile_snapshot(&snapshot)
            .await?;
        let cutover = {
            let mut state = self.state.lock().await;
            if state.authority != authority {
                None
            } else {
                state.installed_authority = authority;
                let old = state.session.replace(CodeReviewSession {
                    controller: controller.take().expect("fresh controller"),
                    snapshot,
                    paging,
                    file_diff_cache: FileDiffCache::default(),
                    file_diff_inflight: HashMap::new(),
                    file_context_cache: HashMap::new(),
                    comment_authority: CommentAuthority::default(),
                });
                let session = state.session.as_ref().expect("code review session stored");
                Some((
                    old,
                    code_review_projection(&session.snapshot, &session.controller),
                ))
            }
        };
        let Some((old, projection)) = cutover else {
            controller.expect("stale fresh controller").shutdown().await;
            bail!("code review open was superseded while its snapshot was loading");
        };
        if let Some(mut old) = old {
            old.controller.shutdown().await;
        }
        Ok(projection)
    }

    /// List recent commits for the `/commit` picker: a read-only bounded
    /// git log that never touches review controller state. The git call
    /// runs on the blocking pool like every other code-review git
    /// acquisition.
    async fn commits(&self, app: &Application, limit: Option<u32>) -> Result<Value> {
        let cwd = app.session().cwd().to_path_buf();
        let listed = tokio::task::spawn_blocking(move || {
            crate::code_review::list_recent_commits(&cwd, limit.unwrap_or(100) as usize)
        })
        .await
        .map_err(|error| anyhow!("code review commit list failed: {error}"))?
        .map_err(anyhow::Error::msg)?;
        Ok(json!({
            "commits": listed
                .commits
                .iter()
                .map(|entry| {
                    json!({
                        "commit": entry.commit,
                        "parentCommit": entry.parent_commit,
                        "parentCount": entry.parent_count,
                        "subject": entry.subject,
                        "author": entry.author,
                        "authorDate": entry.author_date,
                    })
                })
                .collect::<Vec<_>>(),
            "truncated": listed.truncated,
        }))
    }

    /// Re-project the currently open review snapshot and controller state.
    async fn snapshot(&self) -> Result<Value> {
        let mut state = self.state.lock().await;
        let Some(session) = state.session.as_mut() else {
            return Err(CodeReviewRpcError::ReviewNotOpen.into());
        };
        session.controller.poll_events();
        Ok(code_review_projection(
            &session.snapshot,
            &session.controller,
        ))
    }

    /// Reload the review snapshot (re-run the bounded git acquisition) and
    /// reconcile existing threads onto the new hunk identities.
    async fn refresh(&self, app: &Application) -> Result<Value> {
        // First pass: reserve the pending-capture token and the installed
        // review's generation, then release the mutex before the blocking
        // Git work. The previous snapshot stays authoritative until cutover.
        // A close of that same review, a later capture, or shutdown
        // invalidates this reservation; a close of any other review does not.
        let (authority, installed_authority, expected_snapshot_id, scope) = {
            let mut state = self.state.lock().await;
            let Some(session) = state.session.as_ref() else {
                return Err(CodeReviewRpcError::ReviewNotOpen.into());
            };
            let expected_snapshot_id = session.snapshot.snapshot_id.clone();
            let scope = session.snapshot.scope.clone();
            state.authority = state.authority.wrapping_add(1);
            (
                state.authority,
                state.installed_authority,
                expected_snapshot_id,
                scope,
            )
        };
        let cwd = app.session().cwd().to_path_buf();
        #[cfg(test)]
        let capture_gate = self.review_capture_gate.lock().await.clone();
        let (snapshot, paging) = tokio::task::spawn_blocking(move || {
            let captured = load_review_snapshot_capture(&cwd, scope);
            #[cfg(test)]
            if let Some(gate) = capture_gate {
                gate.hold_capture();
            }
            captured
        })
        .await
        .map_err(|e| anyhow!("code review snapshot reload failed: {e}"))?;
        let mut state = self.state.lock().await;
        if state.authority != authority || state.installed_authority != installed_authority {
            bail!("code review refresh was superseded while its snapshot was loading");
        }
        let Some(session) = state.session.as_ref() else {
            bail!("code review refresh targets a closed review");
        };
        if session.snapshot.snapshot_id != expected_snapshot_id {
            bail!("code review refresh targets a replaced review");
        }
        state.installed_authority = authority;
        let session = state
            .session
            .as_mut()
            .expect("refresh session checked before claiming its generation");
        session.controller.poll_events();
        session.controller.reconcile_snapshot(&snapshot).await?;
        session.snapshot = snapshot;
        session.paging = paging;
        session.file_diff_cache.clear();
        session.file_diff_inflight.clear();
        session.file_context_cache.clear();
        session.comment_authority = CommentAuthority::default();
        Ok(code_review_projection(
            &session.snapshot,
            &session.controller,
        ))
    }

    async fn comment(
        &self,
        app: &Application,
        snapshot_id: &str,
        path: &str,
        old_start: u32,
        old_count: u32,
        new_start: u32,
        new_count: u32,
        content_hash: &str,
        comment: &str,
        images: Vec<ContentBlock>,
    ) -> Result<Value> {
        // Fail closed before any state lookup: a violating image never
        // reaches the controller or its prompt.
        let images = validate_comment_images(&images)?;
        let normalized = crate::code_review::normalize_repo_path(path);
        {
            let mut state = self.state.lock().await;
            let Some(session) = state.session.as_mut() else {
                return Err(CodeReviewRpcError::ReviewNotOpen.into());
            };
            session.controller.poll_events();
            let target = resolve_comment_target(
                &session.snapshot,
                Some(&session.file_diff_cache),
                Some(&session.comment_authority),
                snapshot_id,
                &normalized,
                old_start,
                old_count,
                new_start,
                new_count,
                content_hash,
            )?;
            if !matches!(target, ResolvedHunk::Authority { .. }) {
                let result = match target {
                    ResolvedHunk::Snapshot { file, hunk_index } => session
                        .controller
                        .submit_comment_with_images(
                            &session.snapshot,
                            file,
                            hunk_index,
                            comment,
                            images.clone(),
                        ),
                    ResolvedHunk::Loaded {
                        file,
                        diff,
                        hunk_index,
                    } => session.controller.submit_loaded_comment_with_images(
                        &session.snapshot,
                        file,
                        &diff,
                        hunk_index,
                        comment,
                        images.clone(),
                    ),
                    ResolvedHunk::Authority { .. } => unreachable!("matched above"),
                };
                comment_submit_result(result)?;
                return Ok(code_review_projection(
                    &session.snapshot,
                    &session.controller,
                ));
            }
        }
        let served = self.load_file_diff(app, snapshot_id, &normalized).await?;
        let mut state = self.state.lock().await;
        let Some(session) = state.session.as_mut() else {
            return Err(CodeReviewRpcError::ReviewNotOpen.into());
        };
        session.controller.poll_events();
        if session.snapshot.snapshot_id != snapshot_id {
            bail!("comment targets a stale snapshot; refresh the code review");
        }
        let file = session
            .snapshot
            .files
            .iter()
            .find(|file| file.path == normalized)
            .ok_or_else(|| anyhow!("comment targets an unknown file {normalized:?}"))?;
        let hunk_index = served
            .diff
            .hunks
            .iter()
            .position(|hunk| {
                hunk.old_start == old_start
                    && hunk.old_count == old_count
                    && hunk.new_start == new_start
                    && hunk.new_count == new_count
            })
            .ok_or_else(|| {
                anyhow!("comment targets an unknown hunk in the loaded diff {normalized:?}")
            })?;
        if !served.diff.hunk_is_comment_ready(hunk_index) {
            bail!(
                "comment targets an incomplete hunk in {normalized:?}; the per-file body was cut by the size limit"
            );
        }
        let identity = HunkIdentity::new(snapshot_id, &file.path, &served.diff.hunks[hunk_index]);
        if identity.content_hash != content_hash {
            bail!("comment targets a stale hunk; the diff content has changed");
        }
        let result = session.controller.submit_loaded_comment_with_images(
            &session.snapshot,
            file,
            &served.diff,
            hunk_index,
            comment,
            images,
        );
        comment_submit_result(result)?;
        Ok(code_review_projection(
            &session.snapshot,
            &session.controller,
        ))
    }
    /// Resolve `modelId` against the session's public model catalog and swap
    /// the open review's fork model. `modelId` is the `provider/model-id`
    /// identity the wire's option values carry. Fail closed on unknown ids
    /// and catalog load failures; the raw catalog entry
    /// (api/provider/base_url/headers) supplies all connection material —
    /// the wire only ever names the model, never a display label.
    async fn set_model(&self, model_id: &str) -> Result<Value> {
        let catalog = available_models().await?;
        let listed = resolve_catalog_model(&catalog, model_id)
            .ok_or_else(|| CodeReviewRpcError::UnknownModel(model_id.to_owned()))?;
        let model = pi_ai::get_model(&listed.provider, &listed.id).ok_or_else(|| {
            CodeReviewRpcError::UnknownModel(format!("{}/{}", listed.provider, listed.id))
        })?;
        let display = fork_model_display(&model);
        {
            let mut state = self.state.lock().await;
            let session = state
                .session
                .as_mut()
                .ok_or(CodeReviewRpcError::ReviewNotOpen)?;
            session.controller.set_model(model);
        }
        Ok(json!({
            "ok": true,
            "model": display,
        }))
    }

    /// Start one Auto Review batch (see
    /// [`RpcCommand::CodeReviewAutoReview`]): model resolution and batch
    /// accounting live on the controller; unknown model ids and a running
    /// batch surface as typed errors.
    async fn auto_review(&self, model_id: Option<&str>, language: Option<&str>) -> Result<Value> {
        let mut state = self.state.lock().await;
        let Some(session) = state.session.as_mut() else {
            return Err(CodeReviewRpcError::ReviewNotOpen.into());
        };
        session.controller.poll_events();
        match session
            .controller.start_auto_review(&session.snapshot, model_id, language)
            .await?
        {
            AutoReviewStart::Started { .. } => {
                let batch_id = session.controller.auto_review_batch_id();
                Ok(json!({
                    "ok": true,
                    "state": "triaging",
                    "batchId": batch_id,
                }))
            }
            AutoReviewStart::AlreadyRunning => Err(CodeReviewRpcError::AutoRunning.into()),
            AutoReviewStart::UnknownModel(model_id) => {
                Err(CodeReviewRpcError::UnknownModel(model_id).into())
            }
            AutoReviewStart::UnsupportedSchema => Err(CodeReviewRpcError::UnsupportedSchema.into()),
        }
    }

    /// Run one Highlight call (see [`RpcCommand::CodeReviewHighlight`]).
    /// Returns immediately: the accepted start ACK carries the running state
    /// plus the projected `highlightGeneration` identity; the single model
    /// call runs on a detached fork agent and its result arrives on later
    /// snapshots. No review pipeline state is held across the RPC.
    async fn highlight(&self) -> Result<Value> {
        let mut state = self.state.lock().await;
        let Some(session) = state.session.as_mut() else {
            return Err(CodeReviewRpcError::ReviewNotOpen.into());
        };
        session.controller.poll_events();
        match session.controller.start_highlight(&session.snapshot) {
            crate::code_review_panel::HighlightStart::Started => Ok(json!({
                "ok": true,
                "state": "running",
                "highlightGeneration": session.controller.highlight_batch_id(),
                "reviewFindings": review_findings_projection(&session.controller),
            })),
            crate::code_review_panel::HighlightStart::AlreadyRunning => {
                Err(CodeReviewRpcError::HighlightRunning.into())
            }
        }
    }
    /// Start one TLDR over the pinned snapshot (see [`RpcCommand::CodeReviewTldr`]).
    /// Returns the running ACK only. Snapshot fields stay on the existing
    /// review projection; this method holds no TLDR state.
    async fn tldr(&self, language: &str) -> Result<Value> {
        let mut state = self.state.lock().await;
        let Some(session) = state.session.as_mut() else {
            return Err(CodeReviewRpcError::ReviewNotOpen.into());
        };
        session.controller.poll_events();
        match session.controller.start_tldr(&session.snapshot, Some(language)) {
            Ok(generation) => Ok(json!({
                "ok": true,
                "state": "running",
                "tldrGeneration": generation,
            })),
            Err(error) if error.downcast_ref::<crate::code_review_panel::TldrError>()
                == Some(&crate::code_review_panel::TldrError::AlreadyRunning) =>
            {
                Err(CodeReviewRpcError::TldrRunning.into())
            }
            Err(error) => Err(error),
        }
    }

    async fn stop(&self, operation: ReviewStopOperation) -> Result<Value> {
        let (mut pending, review_id) = {
            let mut state = self.state.lock().await;
            let Some(session) = state.session.as_mut() else {
                return Err(CodeReviewRpcError::ReviewNotOpen.into());
            };
            session.controller.poll_events();
            (
                session.controller.begin_stop_operation(operation),
                session.controller.review_id().to_owned(),
            )
        };
        if let Some(pending) = pending.as_mut() {
            // The agent aborts/waits run with the state mutex released so
            // other code-review commands stay responsive during a held abort.
            pending.abort().await;
        }
        {
            let mut state = self.state.lock().await;
            if let Some(session) = state.session.as_mut() {
                // A refresh/reopen replaced the controller while the abort
                // ran: the extracted agents belong to the retired review and
                // are cleaned by the pending's drop, never finalized here.
                if session.controller.review_id() == review_id
                    && let Some(pending) = pending
                {
                    session.controller.finish_stop_operation(pending).await;
                }
                session.controller.poll_events();
            }
        }
        Ok(json!({
            "ok": true,
            "operation": operation,
        }))
    }

    async fn abort(
        &self,
        snapshot_id: &str,
        path: &str,
        old_start: u32,
        old_count: u32,
        new_start: u32,
        new_count: u32,
        content_hash: &str,
    ) -> Result<Value> {
        let normalized = crate::code_review::normalize_repo_path(path);
        let mut state = self.state.lock().await;
        let Some(session) = state.session.as_mut() else {
            return Err(CodeReviewRpcError::ReviewNotOpen.into());
        };
        session.controller.poll_events();
        let target = resolve_comment_target(
            &session.snapshot,
            Some(&session.file_diff_cache),
            Some(&session.comment_authority),
            snapshot_id,
            &normalized,
            old_start,
            old_count,
            new_start,
            new_count,
            content_hash,
        )?;
        let identity = match &target {
            ResolvedHunk::Snapshot { file, hunk_index } => {
                session.snapshot.hunk_identity(file, &file.hunks[*hunk_index])
            }
            ResolvedHunk::Loaded {
                file,
                diff,
                hunk_index,
            } => HunkIdentity::new(
                &session.snapshot.snapshot_id,
                &file.path,
                &diff.hunks[*hunk_index],
            ),
            ResolvedHunk::Authority { file, record } => HunkIdentity {
                snapshot_id: snapshot_id.to_owned(),
                path: file.path.clone(),
                old_start: record.old_start,
                old_count: record.old_count,
                new_start: record.new_start,
                new_count: record.new_count,
                content_hash: record.content_hash.clone(),
            },
        };
        session.controller.abort_hunk(&identity).await;
        session.controller.poll_events();
        Ok(code_review_projection(
            &session.snapshot,
            &session.controller,
        ))
    }
    async fn close(&self, review_id: Option<&str>) -> Result<Value> {
        let mut session = {
            let mut state = self.state.lock().await;
            // Nothing to close: a typed no-open reject that never bumps the
            // authority, so an in-flight open/refresh keeps winning. A
            // stamped close may only take the controller whose identity it
            // observed; a mismatch is a no-op reject.
            let Some(session) = state.session.as_ref() else {
                return Err(CodeReviewRpcError::ReviewNotOpen.into());
            };
            if review_id.is_some_and(|expected| session.controller.review_id() != expected) {
                return Ok(json!({"closed": false, "staleReview": true}));
            }
            // Unstamped close, and a stamped close that still owns the
            // installed generation, invalidate the pending capture. A stamp
            // for a review already replaced by a reserved open shuts that
            // controller down without touching the newer token.
            if review_id.is_none() || state.installed_authority == state.authority {
                state.authority = state.authority.wrapping_add(1);
            }
            state
                .session
                .take()
                .ok_or(CodeReviewRpcError::ReviewNotOpen)?
        };
        session.controller.shutdown().await;
        Ok(json!({"closed": true}))
    }

    /// Shut down any open review controller (RPC session exit / listener
    /// shutdown). Never errors: a missing review is a no-op.
    async fn shutdown(&self) {
        let session = {
            let mut state = self.state.lock().await;
            state.authority = state.authority.wrapping_add(1);
            state.installed_authority = state.authority;
            state.session.take()
        };
        if let Some(mut session) = session {
            session.controller.shutdown().await;
        }
    }

    /// Whether a review still owns cancellable work (close/LRU busy check).
    /// A terminal response may remain owner-held until its task is joined.
    async fn is_busy(&self) -> bool {
        let mut state = self.state.lock().await;
        let Some(session) = state.session.as_mut() else {
            return false;
        };
        session.controller.poll_events();
        session.controller.is_busy()
    }

    async fn file_diff(
        &self,
        app: &Application,
        snapshot_id: &str,
        path: &str,
        cursor: usize,
        max_lines: Option<usize>,
    ) -> Result<Value> {
        let served = self.load_file_diff(app, snapshot_id, path).await?;
        let max = max_lines.unwrap_or(MAX_FILE_PAGE_LINES);
        let mut page = served
            .diff
            .slice_page(snapshot_id, cursor, max)
            .map_err(anyhow::Error::msg)?;
        if !served.authority_complete {
            for hunk in &mut page.hunks {
                hunk.complete = false;
            }
        }
        Ok(file_diff_projection(&page))
    }

    async fn file_context(
        &self,
        app: &Application,
        snapshot_id: &str,
        path: &str,
        old_start: u32,
        new_start: u32,
        count: Option<u32>,
    ) -> Result<Value> {
        let normalized = crate::code_review::normalize_repo_path(path);
        // Spec bound: one expand click loads at most MAX_CONTEXT_EXPAND_LINES
        // rows; the cumulative whole-file budget is enforced by the render cap
        // on top of these bounded requests, never by a single oversized call.
        let count = count
            .unwrap_or(MAX_CONTEXT_EXPAND_LINES)
            .min(MAX_CONTEXT_EXPAND_LINES);
        let (file, paging, cached) = {
            let mut state = self.state.lock().await;
            let Some(session) = state.session.as_mut() else {
                return Err(CodeReviewRpcError::ReviewNotOpen.into());
            };
            if session.snapshot.snapshot_id != snapshot_id {
                bail!("code_review_file_context targets a stale snapshot; refresh the code review");
            }
            let file = session
                .snapshot
                .files
                .iter()
                .find(|f| f.path == normalized)
                .ok_or_else(|| anyhow!("code_review_file_context targets an unknown file {path:?}"))?
                .clone();
            if file.binary || file.status == crate::code_review::FileStatus::Binary {
                bail!("binary files cannot expand context");
            }
            let cached = session.file_context_cache.get(&normalized).cloned();
            (
                file,
                session.paging.clone(),
                cached,
            )
        };
        let blobs = if let Some(blobs) = cached {
            blobs
        } else {
            let cwd = app.session().cwd().to_path_buf();
            let loaded = tokio::task::spawn_blocking(move || {
                FileContextBlobs::load(&cwd, &paging, &file)
            })
            .await
            .map_err(|e| anyhow!("code review file context load failed: {e}"))?
            .map_err(anyhow::Error::msg)?;
            let arc = Arc::new(loaded);
            let mut state = self.state.lock().await;
            let Some(session) = state.session.as_mut() else {
                bail!("code_review_file_context targets a stale snapshot; refresh the code review");
            };
            if session.snapshot.snapshot_id != snapshot_id {
                bail!("code_review_file_context targets a stale snapshot; refresh the code review");
            }
            session
                .file_context_cache
                .entry(normalized.clone())
                .or_insert_with(|| arc.clone());
            session
                .file_context_cache
                .get(&normalized)
                .cloned()
                .unwrap_or(arc)
        };
        let slice = blobs
            .slice(old_start, new_start, count)
            .map_err(anyhow::Error::msg)?;
        Ok(file_context_projection(snapshot_id, &normalized, &slice))
    }

    async fn load_file_diff(
        &self,
        app: &Application,
        snapshot_id: &str,
        path: &str,
    ) -> Result<ServedFileDiff> {
        let normalized = crate::code_review::normalize_repo_path(path);
        {
            let mut state = self.state.lock().await;
            if let Some(session) = state.session.as_mut() {
                if session.snapshot.snapshot_id == snapshot_id {
                    if let Some(cached) = session.file_diff_cache.get(&normalized) {
                        return Ok(ServedFileDiff {
                            diff: cached,
                            authority_complete: !session.comment_authority.overflowed(&normalized),
                        });
                    }
                }
            }
        }
        let (file_clone, scope, paging, flight) = {
            let mut state = self.state.lock().await;
            let Some(session) = state.session.as_mut() else {
                return Err(CodeReviewRpcError::ReviewNotOpen.into());
            };
            if session.snapshot.snapshot_id != snapshot_id {
                bail!("code_review_file_diff targets a stale snapshot; refresh the code review");
            }
            let file = session
                .snapshot
                .files
                .iter()
                .find(|f| f.path == normalized)
                .ok_or_else(|| anyhow!("code_review_file_diff targets an unknown file {path:?}"))?
                .clone();
            if let Some(cached) = session.file_diff_cache.get(&normalized) {
                return Ok(ServedFileDiff {
                    diff: cached,
                    authority_complete: !session.comment_authority.overflowed(&normalized),
                });
            }
            let flight = match session.file_diff_inflight.get(&normalized) {
                Some(flight) => flight.clone(),
                None => {
                    let flight = Arc::new(FileDiffLoadFlight {
                        slot: tokio::sync::Mutex::new(()),
                        outcome: tokio::sync::OnceCell::new(),
                    });
                    session
                        .file_diff_inflight
                        .insert(normalized.clone(), flight.clone());
                    flight
                }
            };
            (file, session.snapshot.scope.clone(), session.paging.clone(), flight)
        };

        let _owner = flight.slot.lock().await;
        #[cfg(test)]
        let load_gate = self.file_diff_load_gate.lock().await.clone();
        let fast = {
            let mut state = self.state.lock().await;
            let Some(session) = state.session.as_mut() else {
                bail!("code_review_file_diff targets a stale snapshot; refresh the code review");
            };
            if session.snapshot.snapshot_id != snapshot_id {
                bail!("code_review_file_diff targets a stale snapshot; refresh the code review");
            }
            if let Some(cached) = session.file_diff_cache.get(&normalized) {
                Some(Ok(ServedFileDiff {
                    diff: cached,
                    authority_complete: !session.comment_authority.overflowed(&normalized),
                }))
            } else {
                flight.outcome.get().map(|shared| {
                    shared
                        .as_ref()
                        .map(|served| ServedFileDiff {
                            diff: Arc::clone(&served.diff),
                            authority_complete: served.authority_complete,
                        })
                        .map_err(|e| anyhow!("{e}"))
                })
            }
        };
        if let Some(served) = fast {
            return served;
        }

        let cwd = app.session().cwd().to_path_buf();
        let diff = tokio::task::spawn_blocking(move || {
            FileDiff::load_snapshot_file(&cwd, &scope, &paging, &file_clone)
        })
        .await
        .map_err(|e| anyhow!("code review file diff load failed: {e}"))?;
        // Gate waits must run with no state lock held: a racing refresh needs
        // that mutex to reach the release barrier.
        #[cfg(test)]
        if let Some(gate) = &load_gate {
            self.file_diff_load_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            gate.loaded.wait().await;
            gate.release.wait().await;
        }

        let served = {
            let mut state = self.state.lock().await;
            let Some(session) = state.session.as_mut() else {
                bail!("code_review_file_diff targets a stale snapshot; refresh the code review");
            };
            if session.snapshot.snapshot_id != snapshot_id {
                bail!("code_review_file_diff targets a stale snapshot; refresh the code review");
            }
            let arc = Arc::new(diff);
            let authority_complete = if arc.error.is_none() {
                session.file_diff_cache.put(normalized.clone(), arc.clone());
                session.comment_authority.record(&normalized, arc.as_ref())
            } else {
                true
            };
            let _ = flight.outcome.set(Ok(ServedFileDiff {
                diff: arc.clone(),
                authority_complete,
            }));
            if session
                .file_diff_inflight
                .get(&normalized)
                .is_some_and(|entry| Arc::ptr_eq(entry, &flight))
            {
                session.file_diff_inflight.remove(&normalized);
            }
            ServedFileDiff {
                diff: arc,
                authority_complete,
            }
        };
        Ok(served)
    }
}

#[derive(Debug)]
enum ResolvedHunk<'a> {
    Snapshot {
        file: &'a DiffFile,
        hunk_index: usize,
    },
    Loaded {
        file: &'a DiffFile,
        diff: Arc<FileDiff>,
        hunk_index: usize,
    },
    Authority {
        file: &'a DiffFile,
        record: &'a HunkAuthorityRecord,
    },
}

fn resolve_comment_target<'a>(
    snapshot: &'a ReviewSnapshot,
    file_diff_cache: Option<&'a FileDiffCache>,
    comment_authority: Option<&'a CommentAuthority>,
    snapshot_id: &str,
    path: &str,
    old_start: u32,
    old_count: u32,
    new_start: u32,
    new_count: u32,
    content_hash: &str,
) -> Result<ResolvedHunk<'a>> {
    if snapshot.snapshot_id != snapshot_id {
        bail!("comment targets a stale snapshot; refresh the code review");
    }
    let file = snapshot
        .files
        .iter()
        .find(|file| file.path == path)
        .ok_or_else(|| anyhow!("comment targets an unknown file {path:?}"))?;
    if let Some(authority) = comment_authority {
        if authority.overflowed(path) {
            bail!(
                "comment targets a hunk in {path:?} beyond the retained comment authority bound; refresh the code review"
            );
        }
    }
    if let Some(loaded) = file_diff_cache.and_then(|cache| cache.peek(path)) {
        if loaded.error.is_none() {
            let hunk_index = loaded
                .hunks
                .iter()
                .position(|hunk| {
                    hunk.old_start == old_start
                        && hunk.old_count == old_count
                        && hunk.new_start == new_start
                        && hunk.new_count == new_count
                })
                .ok_or_else(|| {
                    anyhow!("comment targets an unknown hunk in the loaded diff {path:?}")
                })?;
            if !loaded.hunk_is_comment_ready(hunk_index) {
                bail!(
                    "comment targets an incomplete hunk in {path:?}; the per-file body was cut by the size limit"
                );
            }
            let identity = HunkIdentity::new(snapshot_id, &file.path, &loaded.hunks[hunk_index]);
            if identity.content_hash != content_hash {
                bail!("comment targets a stale hunk; the diff content has changed");
            }
            return Ok(ResolvedHunk::Loaded {
                file,
                diff: loaded.clone(),
                hunk_index,
            });
        }
    }
    if let Some(authority) = comment_authority {
        if let Some(records) = authority.lookup(path) {
            let record = records
                .iter()
                .find(|record| {
                    record.old_start == old_start
                        && record.old_count == old_count
                        && record.new_start == new_start
                        && record.new_count == new_count
                })
                .ok_or_else(|| {
                    anyhow!("comment targets an unknown hunk in the loaded diff {path:?}")
                })?;
            if !record.comment_ready {
                bail!(
                    "comment targets an incomplete hunk in {path:?}; the per-file body was cut by the size limit"
                );
            }
            if record.content_hash != content_hash {
                bail!("comment targets a stale hunk; the diff content has changed");
            }
            return Ok(ResolvedHunk::Authority { file, record });
        }
    }
    let hunk_index = file
        .hunks
        .iter()
        .position(|hunk| {
            hunk.old_start == old_start
                && hunk.old_count == old_count
                && hunk.new_start == new_start
                && hunk.new_count == new_count
        })
        .ok_or_else(|| anyhow!("comment targets an unknown hunk in {path:?}"))?;
    if !file.hunk_is_comment_ready(hunk_index) {
        bail!("comment targets an incomplete hunk in {path:?}; the diff body was truncated");
    }
    let identity = snapshot.hunk_identity(file, &file.hunks[hunk_index]);
    if identity.content_hash != content_hash {
        bail!("comment targets a stale hunk; the diff content has changed");
    }
    Ok(ResolvedHunk::Snapshot { file, hunk_index })
}

fn comment_submit_result(result: CommentSubmitResult) -> Result<()> {
    match result {
        CommentSubmitResult::Accepted => Ok(()),
        CommentSubmitResult::Empty => {
            bail!("code review comment was not accepted (comment is empty)")
        }
        CommentSubmitResult::QueueFull => bail!(
            "code review comment was not accepted (this hunk has {MAX_PENDING_COMMENTS} replies queued; wait for a reply to settle or abort the review)"
        ),
        CommentSubmitResult::TooLarge => bail!(
            "code review comment was not accepted (it exceeds the {MAX_COMMENT_BYTES}-byte limit; shorten the comment and resubmit)"
        ),
        CommentSubmitResult::IncompleteHunk => bail!(
            "code review comment was not accepted (the snapshot hunk is incomplete; load the full file diff first)"
        ),
        CommentSubmitResult::Failed => bail!(
            "code review comment was not accepted (the review agent could not be started)"
        ),
    }
}

/// Bounded image attachment budget for review comments. Per image: base64
/// must decode and stay within the decoded cap. In total: the base64 wire
/// footprint stays under 3.5 MiB, comfortably inside the 4 MiB RPC frame
/// ([`MAX_RPC_MESSAGE_BYTES`]).
const MAX_COMMENT_IMAGES: usize = 8;
const MAX_COMMENT_IMAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_COMMENT_IMAGES_WIRE_BYTES: usize =
    3 * 1024 * 1024 + 512 * 1024;

fn resolve_review_language(explicit: Option<&str>, default: Option<&str>) -> Result<String> {
    let tag = explicit.or(default).unwrap_or(crate::code_review_panel::DEFAULT_REVIEW_LANGUAGE);
    validate_review_language(tag)
        .map(str::to_owned)
        .map_err(|_| CodeReviewRpcError::InvalidLanguage.into())
}

/// Resolve a review model against the public catalog by its `provider/model-id`
/// identity — exactly the value the wire's option values carry. The first
/// `/` separates provider from the id (model ids may themselves contain
/// slashes); a bare id or a display label never resolves.
pub(crate) fn resolve_catalog_model<'a>(
    catalog: &'a [pi_ai::Model],
    model_id: &str,
) -> Option<&'a pi_ai::Model> {
    let (provider, id) = model_id.split_once('/')?;
    catalog
        .iter()
        .find(|model| model.provider == provider && model.id == id)
}

/// Typed code-review RPC rejection categories.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CodeReviewRpcError {
    ReviewNotOpen,
    UnknownModel(String),
    AutoRunning,
    HighlightRunning,
    TldrRunning,
    UnsupportedSchema,
    TooManyImages,
    UnsupportedImageType,
    InvalidImageData,
    ImagesTooLarge,
    InvalidLanguage,
}

impl CodeReviewRpcError {
    fn code(&self) -> &'static str {
        match self {
            Self::ReviewNotOpen => "code_review_not_open",
            Self::UnknownModel(_) => "code_review_unknown_model",
            Self::AutoRunning => "code_review_auto_running",
            Self::HighlightRunning => "code_review_highlight_running",
            Self::TldrRunning => "code_review_tldr_running",
            Self::UnsupportedSchema => "code_review_unsupported_schema",
            Self::InvalidLanguage => "code_review_invalid_language",
            Self::TooManyImages => "code_review_too_many_images",
            Self::UnsupportedImageType => "code_review_unsupported_image_type",
            Self::InvalidImageData => "code_review_invalid_image_data",
            Self::ImagesTooLarge => "code_review_images_too_large",
        }
    }
}

impl std::fmt::Display for CodeReviewRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReviewNotOpen => f.write_str("no open code review; call code_review_open first"),
            Self::UnknownModel(_) => write!(f, "code review model switch was not accepted (unknown model id)"),
            Self::InvalidLanguage => f.write_str("Review language must be 1–32 printable ASCII characters"),
            Self::AutoRunning => write!(
                f,
                "code review auto review was not accepted (a batch is already running; wait for it to settle or abort its hunks)"
            ),
            Self::HighlightRunning => f.write_str(
                "code review highlight was not accepted (a highlight call is already running; wait for it to settle)",
            ),
            Self::TldrRunning => f.write_str(
                "code review TLDR was not accepted (a TLDR call is already running; wait for it to settle)",
            ),
            Self::UnsupportedSchema => f.write_str(
                "code review auto review requires a model with strict JSON schema output",
            ),
            Self::TooManyImages => write!(
                f,
                "code review comment was not accepted (more than {MAX_COMMENT_IMAGES} images)"
            ),
            Self::UnsupportedImageType => write!(
                f,
                "code review comment was not accepted (image mime must be image/png, image/jpeg, image/webp, or image/gif)"
            ),
            Self::InvalidImageData => write!(
                f,
                "code review comment was not accepted (an image is not valid base64 or exceeds the {MAX_COMMENT_IMAGE_BYTES}-byte decoded limit)"
            ),
            Self::ImagesTooLarge => write!(
                f,
                "code review comment was not accepted (images exceed the {MAX_COMMENT_IMAGES_WIRE_BYTES}-byte total wire limit)"
            ),
        }
    }
}

/// Validate comment image attachments against the typed budget. Only image
/// content blocks are accepted; every image must decode as standard base64
/// and stay within the per-image decoded cap, and the combined base64 wire
/// footprint must stay within the total cap.
fn validate_comment_images(images: &[ContentBlock]) -> Result<Vec<ContentBlock>> {
    if images.len() > MAX_COMMENT_IMAGES {
        return Err(CodeReviewRpcError::TooManyImages.into());
    }
    let mut validated = Vec::with_capacity(images.len());
    let mut total_wire = 0usize;
    for image in images {
        let ContentBlock::Image { data, mime_type } = image else {
            return Err(CodeReviewRpcError::UnsupportedImageType.into());
        };
        if !matches!(
            mime_type.as_str(),
            "image/png" | "image/jpeg" | "image/webp" | "image/gif"
        ) {
            return Err(CodeReviewRpcError::UnsupportedImageType.into());
        }
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(data.as_bytes())
            .map_err(|_| CodeReviewRpcError::InvalidImageData)?;
        if decoded.len() > MAX_COMMENT_IMAGE_BYTES {
            return Err(CodeReviewRpcError::InvalidImageData.into());
        }
        total_wire = total_wire.saturating_add(data.len());
        if total_wire > MAX_COMMENT_IMAGES_WIRE_BYTES {
            return Err(CodeReviewRpcError::ImagesTooLarge.into());
        }
        validated.push(image.clone());
    }
    Ok(validated)
}

impl std::error::Error for CodeReviewRpcError {}

/// Build the wire projection of a review snapshot + controller. The absolute
/// repository `root` is NEVER included — only repo-relative paths leave the
/// process. Output stays bounded: git acquisition caps the combined patch at
/// [`crate::code_review::MAX_DIFF_BYTES`] and the changed-file catalog at
/// [`crate::code_review::MAX_CATALOG_BYTES`]; files the truncated patch could
/// not carry project as on-demand placeholders instead of vanishing.
fn code_review_projection(snapshot: &ReviewSnapshot, controller: &CodeReviewController) -> Value {
    let files = snapshot
        .files
        .iter()
        .map(|file| {
            let hunks = file
                .hunks
                .iter()
                .map(|hunk| {
                    let identity = snapshot.hunk_identity(file, hunk);
                    let lines = hunk
                        .lines
                        .iter()
                        .map(|line| {
                            let mut obj = json!({
                                "kind": diff_line_kind_str(line.kind),
                                "text": line.text,
                            });
                            if let Some(old_no) = line.old_no {
                                obj["oldNo"] = json!(old_no);
                            }
                            if let Some(new_no) = line.new_no {
                                obj["newNo"] = json!(new_no);
                            }
                            obj
                        })
                        .collect::<Vec<_>>();
                    json!({
                        "header": hunk.header,
                        "oldStart": hunk.old_start,
                        "oldCount": hunk.old_count,
                        "newStart": hunk.new_start,
                        "newCount": hunk.new_count,
                        "contentHash": identity.content_hash,
                        "lines": lines,
                    })
                })
                .collect::<Vec<_>>();
            let mut file_obj = json!({
                "path": file.path,
                "status": file.status.label(),
                "binary": file.binary,
                "insertions": file.displayed_change_stats().0,
                "deletions": file.displayed_change_stats().1,
                "bodyState": file.body_state(),
                "hunks": hunks,
            });
            if let Some(previous_path) = &file.previous_path {
                file_obj["previousPath"] = json!(previous_path);
            }
            if let Some(message) = &file.message {
                file_obj["message"] = json!(message);
            }
            file_obj
        })
        .collect::<Vec<_>>();

    let threads = controller
        .threads()
        .values()
        .map(|thread| {
            let mut thread_obj = json!({
                "identity": hunk_identity_projection(&thread.identity),
                "comments": thread.comments.iter().map(|comment| {
                    let mut obj = json!({
                        "role": review_role_str(comment.role),
                        "text": comment.text,
                        "partial": comment.partial,
                    });
                    if comment.auto {
                        obj["auto"] = json!(true);
                    }
                    if let Some(model) = comment.model.as_deref() {
                        obj["model"] = json!(model);
                    }
                    if let Some(thinking) = comment
                        .thinking
                        .as_deref()
                        .filter(|thinking| !thinking.is_empty())
                    {
                        obj["thinking"] = json!(thinking);
                    }
                    obj
                }).collect::<Vec<_>>(),
                "streamingText": thread.streaming_text,
                "streamingThinking": thread.streaming_thinking,
                "error": thread.error,
                "stale": thread.stale,
                "isStreaming": thread.reply_status.is_live(),
                "isCancellable": controller.is_hunk_cancellable(&thread.identity),
                "model": thread.model,
                "generation": thread.generation,
                "replyId": thread.reply_id,
                "replyStatus": thread.reply_status.as_str(),
                "toolCalls": thread.tool_calls.iter().map(|tool| {
                    json!({
                        "id": tool.id,
                        "name": tool.name,
                        "status": tool.status.as_str(),
                    })
                }).collect::<Vec<_>>(),
            });
            thread_obj
        })
        .collect::<Vec<_>>();

    json!({
        "comparisonLabel": snapshot.comparison_label(),
        "snapshotId": snapshot.snapshot_id,
        "reviewId": controller.review_id(),
        "generation": controller.generation(),
        "revision": controller.revision(),
        "highlightState": controller.highlight_state().as_str(),
        "reviewFindings": review_findings_projection(controller),
        "highlightError": controller.highlight_error().map(ToString::to_string),
        "highlightGeneration": controller.highlight_batch_id(),
        "tldrState": controller.tldr_state().as_str(),
        "tldrGeneration": controller.tldr_generation(),
        "tldrSnapshotId": controller.tldr_snapshot_id(),
        "tldrError": controller.tldr_error(),
        "tldrMarkdown": controller.tldr_markdown(),
        "truncated": snapshot.truncated,
        "totalsComplete": snapshot.summary_complete,
        "error": snapshot.error,
        "totalInsertions": snapshot.total_insertions(),
        "totalDeletions": snapshot.total_deletions(),
        "files": files,
        "threads": threads,
        "isStreaming": controller.is_streaming(),
        "internalBusy": controller.is_busy(),
        "activeCount": controller.active_count(),
        "autoReviewState": controller.auto_review_state().as_str(),
        "reviewLanguageDefault": controller.review_language_default(),
        "reviewModelId": controller.fork_model_id(),
        "reviewModelProvider": controller.fork_model_provider(),
        "reviewModelLabel": controller.fork_model_display_name(),
        "autoReviewError": controller
            .auto_review_error()
            .map(auto_review_error_projection),
        "autoReviewBatchId": controller.auto_review_batch_id(),
        "autoReviewCounts": {
            "submitted": controller.auto_review_counts().submitted,
            "completed": controller.auto_review_counts().completed,
            "failed": controller.auto_review_counts().failed,
        },
    })
}

/// Typed wire shape of the latest Auto Review failure: a stable machine code
/// plus the bounded display copy. `cancelled` is the exact user-Stop terminal
/// category (the Web treats it as the only quiet terminal); every other code
/// is a business failure with a toast owner.
fn auto_review_error_projection(
    error: &crate::code_review_panel::AutoReviewError,
) -> Value {
    let code = match error {
        crate::code_review_panel::AutoReviewError::Cancelled => "cancelled",
        crate::code_review_panel::AutoReviewError::InvalidTriage => "invalid-triage",
        crate::code_review_panel::AutoReviewError::UnsupportedSchema => "unsupported-schema",
        crate::code_review_panel::AutoReviewError::PromptTooLarge => "prompt-too-large",
        crate::code_review_panel::AutoReviewError::LaunchRejected => "launch-rejected",
        crate::code_review_panel::AutoReviewError::Provider { category, .. } => category.as_str(),
    };
    json!({
        "message": error.to_string(),
        "code": code,
    })
}

fn review_findings_projection(controller: &CodeReviewController) -> Value {
    json!(controller.review_findings().iter().map(|(identity, findings)| json!({
        "identity": hunk_identity_projection(identity),
        "findings": findings.iter().map(|finding| json!({
            "category": finding.category.as_str(),
            "side": finding.side.as_str(),
            "startLine": finding.start_line,
            "endLine": finding.end_line,
            "summary": finding.summary,
            "evidence": finding.evidence,
        })).collect::<Vec<_>>(),
    })).collect::<Vec<_>>())
}

/// Wire projection of a [`crate::code_review::HunkIdentity`].
fn hunk_identity_projection(identity: &crate::code_review::HunkIdentity) -> Value {
    json!({
        "snapshotId": identity.snapshot_id,
        "path": identity.path,
        "oldStart": identity.old_start,
        "oldCount": identity.old_count,
        "newStart": identity.new_start,
        "newCount": identity.new_count,
        "contentHash": identity.content_hash,
    })
}

fn diff_line_kind_str(kind: DiffLineKind) -> &'static str {
    match kind {
        DiffLineKind::Context => "context",
        DiffLineKind::Addition => "addition",
        DiffLineKind::Deletion => "deletion",
        DiffLineKind::Meta => "meta",
    }
}

fn file_diff_projection(page: &FileDiffPage) -> Value {
    let line_projection = |line: &crate::code_review::DiffLine| {
        let mut obj = json!({
            "kind": diff_line_kind_str(line.kind),
            "text": line.text,
        });
        if let Some(old_no) = line.old_no {
            obj["oldNo"] = json!(old_no);
        }
        if let Some(new_no) = line.new_no {
            obj["newNo"] = json!(new_no);
        }
        obj
    };
    let hunks = page
        .hunks
        .iter()
        .map(|hunk| {
            json!({
                "index": hunk.index,
                "header": hunk.header,
                "oldStart": hunk.old_start,
                "oldCount": hunk.old_count,
                "newStart": hunk.new_start,
                "newCount": hunk.new_count,
                "contentHash": hunk.content_hash,
                "totalLines": hunk.total_lines,
                "complete": hunk.complete,
                "lineStart": hunk.line_start,
                "lines": hunk.lines.iter().map(line_projection).collect::<Vec<_>>(),
            })
        })
        .collect::<Vec<_>>();
    let mut out = json!({
        "snapshotId": page.snapshot_id,
        "path": page.path,
        "binary": page.binary,
        "status": page.status.label(),
        "hunks": hunks,
        "cursor": page.cursor,
        "hasMore": page.has_more,
        "totalLines": page.total_lines,
        "hunkCount": page.hunk_count,
        "byteCapped": page.byte_capped,
    });
    if let Some(previous_path) = &page.previous_path {
        out["previousPath"] = json!(previous_path);
    }
    if let Some(next_cursor) = page.next_cursor {
        out["nextCursor"] = json!(next_cursor);
    }
    out
}

fn file_context_projection(
    snapshot_id: &str,
    path: &str,
    slice: &crate::code_review::FileContextSlice,
) -> Value {
    let lines: Vec<Value> = slice
        .lines
        .iter()
        .map(|line| {
            let mut obj = json!({
                "kind": diff_line_kind_str(line.kind),
                "text": line.text,
            });
            if let Some(old_no) = line.old_no {
                obj["oldNo"] = json!(old_no);
            }
            if let Some(new_no) = line.new_no {
                obj["newNo"] = json!(new_no);
            }
            obj
        })
        .collect();
    json!({
        "snapshotId": snapshot_id,
        "path": path,
        "oldLineCount": slice.old_line_count,
        "newLineCount": slice.new_line_count,
        "binary": slice.binary,
        "lines": lines,
    })
}

fn review_role_str(role: ReviewCommentRole) -> &'static str {
    match role {
        ReviewCommentRole::User => "user",
        ReviewCommentRole::Assistant => "assistant",
        ReviewCommentRole::System => "system",
    }
}

/// Dispatch a code-review RPC command against the RPC-owned controller.
async fn handle_code_review_command(
    app: &Application,
    code_review: &CodeReviewRpcState,
    c: RpcCommand,
) -> RpcResponse {
    let id = c.id();
    let name = c.command_name();
    let result: Result<Value> = async {
        match c {
            RpcCommand::CodeReviewOpen {
                commit,
                from,
                to,
                include_working_tree,
                ..
            } => {
                code_review
                    .open(app, commit, from, to, include_working_tree)
                    .await
            }
            RpcCommand::CodeReviewCommits { limit, .. } => code_review.commits(app, limit).await,
            RpcCommand::CodeReviewSnapshot { .. } => code_review.snapshot().await,
            RpcCommand::CodeReviewRefresh { .. } => code_review.refresh(app).await,
            RpcCommand::CodeReviewComment {
                snapshot_id,
                path,
                old_start,
                old_count,
                new_start,
                new_count,
                content_hash,
                comment,
                images,
                ..
            } => {
                code_review
                    .comment(
                        app,
                        &snapshot_id,
                        &path,
                        old_start,
                        old_count,
                        new_start,
                        new_count,
                        &content_hash,
                        &comment,
                        images,
                    )
                    .await
            }
            RpcCommand::CodeReviewSetModel { model_id, .. } => {
                code_review.set_model(&model_id).await
            }
            RpcCommand::CodeReviewAbort {
                snapshot_id,
                path,
                old_start,
                old_count,
                new_start,
                new_count,
                content_hash,
                ..
            } => {
                code_review
                    .abort(
                        &snapshot_id,
                        &path,
                        old_start,
                        old_count,
                        new_start,
                        new_count,
                        &content_hash,
                    )
                    .await
            }
            RpcCommand::CodeReviewHighlight { .. } => code_review.highlight().await,
            RpcCommand::CodeReviewTldr { language, .. } => {
                let default = app.settings_manager()?.settings().review_language;
                let language = resolve_review_language(language.as_deref(), default.as_deref())?;
                code_review.tldr(&language).await
            }
            RpcCommand::CodeReviewStop { operation, .. } => code_review.stop(operation).await,
            RpcCommand::CodeReviewClose { review_id, .. } => code_review.close(review_id.as_deref()).await,
            RpcCommand::CodeReviewAutoReview { model_id, language, .. } => {
                let default = app.settings_manager()?.settings().review_language;
                let language = resolve_review_language(language.as_deref(), default.as_deref())?;
                code_review.auto_review(model_id.as_deref(), Some(&language)).await
            }
            RpcCommand::CodeReviewFileDiff {
                snapshot_id,
                path,
                cursor,
                max_lines,
                ..
            } => {
                code_review
                    .file_diff(app, &snapshot_id, &path, cursor, max_lines)
                    .await
            }
            RpcCommand::CodeReviewFileContext {
                snapshot_id,
                path,
                old_start,
                new_start,
                count,
                ..
            } => {
                code_review
                    .file_context(app, &snapshot_id, &path, old_start, new_start, count)
                    .await
            }
            _ => unreachable!("dispatch_inner only routes code_review_* commands here"),
        }
    }
    .await;
    match result {
        Ok(data) => RpcResponse::success(id, name, Some(data)),
        Err(error) => {
            let mut response = RpcResponse::failure(id, name, error.to_string());
            if let Some(typed) = error.downcast_ref::<CodeReviewRpcError>() {
                response.data = Some(json!({ "code": typed.code() }));
            } else if error.downcast_ref::<crate::code_review_panel::AutoReviewError>().is_some() {
                response.data = Some(json!({ "code": "code_review_auto_failed" }));
            }
            response
        }
    }
}

async fn handle_file_explorer_command(app: &Application, c: RpcCommand) -> RpcResponse {
    let id = c.id();
    let name = c.command_name();
    let app = app.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<Value> {
        let value = match c {
            RpcCommand::FileExplorerList { path, .. } => {
                serde_json::to_value(crate::file_explorer::list(
                    &app,
                    path.as_deref().unwrap_or(""),
                )?)?
            }
            RpcCommand::FileExplorerSearch { query, .. } => {
                serde_json::to_value(crate::file_explorer::search(&app, &query)?)?
            }
            RpcCommand::FileExplorerRead {
                path,
                cursor,
                max_lines,
                ..
            } => serde_json::to_value(crate::file_explorer::read(
                &app, &path, cursor, max_lines,
            )?)?,
            RpcCommand::FileExplorerChanges { .. } => {
                serde_json::to_value(crate::file_explorer::changes(&app)?)?
            }
            RpcCommand::FileExplorerDiff {
                path,
                cursor,
                max_lines,
                ..
            } => serde_json::to_value(crate::file_explorer::diff(
                &app, &path, cursor, max_lines,
            )?)?,
            RpcCommand::FileExplorerBlame {
                path,
                start_line,
                end_line,
                ..
            } => serde_json::to_value(crate::file_explorer::blame(
                &app, &path, start_line, end_line,
            )?)?,
            RpcCommand::FileExplorerDelete { path, .. } => {
                serde_json::to_value(crate::file_explorer::delete_path(
                    &app, &path,
                )?)?
            }
            RpcCommand::FileExplorerMap { path, .. } => {
                serde_json::to_value(crate::dependency_map::dependency_map(&app, &path)?)?
            }
            RpcCommand::PathComplete {
                path,
                kind,
                limit,
                ..
            } => serde_json::to_value(crate::file_explorer::complete_path(
                &app, &path, kind, limit,
            )?)?,
            _ => unreachable!("dispatch_inner only routes file_explorer read commands here"),
        };
        Ok(value)
    })
    .await;
    match result {
        Ok(Ok(data)) => RpcResponse::success(id, name, Some(data)),
        Ok(Err(error)) => {
            let mut response = RpcResponse::failure(id, name, error.to_string());
            if let Some(typed) = error.downcast_ref::<crate::file_explorer::FileExplorerError>() {
                response.data = Some(json!({ "code": typed }));
            }
            response
        }
        Err(error) if error.is_cancelled() => {
            RpcResponse::failure(id, name, "file explorer operation was cancelled")
        }
        Err(_) => RpcResponse::failure(id, name, "file explorer operation failed"),
    }
}

pub(crate) fn project_application_event(event: ApplicationEvent) -> Result<Value> {
    match event {
        ApplicationEvent::Agent(pi_agent::AgentEvent::MessageStart { message })
        | ApplicationEvent::AgentPublished {
            event: pi_agent::AgentEvent::MessageStart { message },
            ..
        } => Ok(serde_json::to_value(pi_agent::AgentEvent::MessageStart {
            message: public_message(message),
        })?),
        ApplicationEvent::Agent(pi_agent::AgentEvent::MessageEnd { message })
        | ApplicationEvent::AgentPublished {
            event: pi_agent::AgentEvent::MessageEnd { message },
            ..
        } => Ok(serde_json::to_value(pi_agent::AgentEvent::MessageEnd {
            message: public_message(message),
        })?),
        ApplicationEvent::Workflow(event) => Ok(serde_json::to_value(
            crate::workflow_rpc::project_workflow_event(&event),
        )?),
        // The multi-session manager reserves the top-level `sessionId` for the
        // OWNING/source runtime, so the forked TARGET identity is renamed
        // `forkedSessionId` in the projected payload.
        ApplicationEvent::SessionForked(event) => {
            let mut value = serde_json::to_value(event)?;
            if let Some(object) = value.as_object_mut() {
                if let Some(session_id) = object.remove("sessionId") {
                    object.insert("forkedSessionId".to_owned(), session_id);
                }
                object.insert("type".to_owned(), json!("session_forked"));
            }
            Ok(value)
        }
        event => Ok(serde_json::to_value(event)?),
    }
}

pub(crate) fn project_extension_ui_event(
    event: ExtensionUiEvent,
) -> Result<Option<RpcExtensionUiRequest>> {
    ui_event_request(event)
}

pub(crate) fn accept_extension_ui_response(
    adapter: &ExtensionUiAdapter,
    response: RpcExtensionUiResponse,
) -> Result<()> {
    handle_ui_response(adapter, response)
}

pub async fn run(cli: &Cli) -> Result<()> {
    let session_run::RunSession {
        application,
        extension_ui,
        todo_child_service,
        ..
    } = session_run::build_session(cli).await?;
    let result = run_with_io_and_extension_ui_and_todo(
        application,
        extension_ui.expect("RPC has UI adapter"),
        tokio::io::stdin(),
        io::stdout(),
        Some(todo_child_service),
    )
    .await;
    result
}
pub async fn run_with_io<R, W>(application: Application, input: R, output: W) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: Write + Send + 'static,
{
    run_with_io_and_extension_ui(application, ExtensionUiAdapter::default(), input, output).await
}
pub async fn run_with_io_and_extension_ui<R, W>(
    application: Application,
    extension_ui: ExtensionUiAdapter,
    input: R,
    output: W,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: Write + Send + 'static,
{
    run_with_io_and_extension_ui_and_todo(application, extension_ui, input, output, None).await
}

async fn run_with_io_and_extension_ui_and_todo<R, W>(
    application: Application,
    extension_ui: ExtensionUiAdapter,
    input: R,
    output: W,
    todo_child_service: Option<Arc<pi_coding::TodoChildService>>,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: Write + Send + 'static,
{
    extension_ui.set_canonical_queries_supported(false);
    let mut events = application.subscribe();
    let mut ui_events = extension_ui.subscribe();
    let (lines_tx, mut lines_rx) = mpsc::channel(JSONL_CHANNEL_CAPACITY);
    let reader = tokio::spawn(read_jsonl(input, lines_tx));
    let output = Arc::new(StdMutex::new(output));
    let dispatcher = RpcDispatcher::new(application.clone());
    let result = async {
        let mut commands = tokio::task::JoinSet::new();
        let mut input_open = true;
        loop {
            if !input_open && commands.is_empty() {
                break;
            }
            tokio::select! {
                line = lines_rx.recv(), if input_open => {
                    match line {
                        Some(JsonlFrame::Line(line)) => match parse_input(&line) {
                            Ok(RpcInput::Command { command, session_id }) => {
                                if let Some(session_id) = session_id {
                                    let response = RpcResponse::failure(
                                        command.id(),
                                        command.command_name(),
                                        format!(
                                            "sessionId is only supported on the Web control plane (unknown session {session_id})"
                                        ),
                                    );
                                    write_shared_json(&output, &response)?;
                                    continue;
                                }
                                if command.runs_inline() {
                                    let response = dispatcher.dispatch_inner(command).await;
                                    write_shared_json(&output, &response)?;
                                } else if commands.len() >= MAX_CONCURRENT_COMMANDS {
                                    let response = RpcResponse::slot_overload(
                                        command.id(),
                                        command.command_name(),
                                        format!("too many concurrent RPC commands (limit {MAX_CONCURRENT_COMMANDS})"),
                                    );
                                    write_shared_json(&output, &response)?;
                                } else {
                                    let dispatcher = dispatcher.clone();
                                    let output = output.clone();
                                    commands.spawn(async move {
                                        let response = dispatcher.dispatch_inner(command).await;
                                        write_shared_json(&output, &response)
                                    });
                                }
                            }
                            Ok(RpcInput::ExtensionUiResponse(response)) => {
                                if let Err(error) = handle_ui_response(&extension_ui, response) {
                                    write_shared_json(
                                        &output,
                                        &RpcResponse::failure(None, "extension_ui_response", error.to_string()),
                                    )?;
                                }
                            }
                            Err(response) => write_shared_json(&output, &response)?,
                        },
                        Some(JsonlFrame::Oversized) => write_shared_json(
                            &output,
                            &RpcResponse::failure(
                                None,
                                "parse",
                                format!("RPC frame exceeds {MAX_RPC_MESSAGE_BYTES} bytes"),
                            ),
                        )?,
                        Some(JsonlFrame::Unterminated) => write_shared_json(
                            &output,
                            &RpcResponse::failure(None, "parse", "RPC frame must end with LF"),
                        )?,
                        None => input_open = false,
                    }
                }
                completed = commands.join_next(), if !commands.is_empty() => {
                    match completed {
                        Some(Ok(result)) => result?,
                        Some(Err(error)) => return Err(anyhow!("RPC command task failed: {error}")),
                        None => {}
                    }
                }
                event = events.recv() => match event {
                    Ok(event) => write_shared_json(&output, &project_application_event(event)?)?,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => write_shared_json(
                        &output,
                        &RpcResponse::failure(None, "events", format!("application event stream lagged by {count} records")),
                    )?,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                event = ui_events.recv() => match event {
                    Ok(event) => {
                        if let Some(request) = project_extension_ui_event(event)? {
                            write_shared_json(&output, &request)?;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => write_shared_json(
                        &output,
                        &RpcResponse::failure(None, "extension_ui", format!("extension UI event stream lagged by {count} records")),
                    )?,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
            }
        }
        reader.await.context("joining JSONL reader")??;
        dispatcher.side_chat.shutdown().await;
        Ok(())
    }
    .await;
    // Unconditional file-conversation teardown: owned controllers must never outlive the process exit path.
    dispatcher.shutdown_file_conversations().await;
    let shutdown = if let Some(service) = todo_child_service {
        service
            .shutdown()
            .await
            .context("shutting down Todo child service")
    } else {
        Ok(())
    };
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
fn write_shared_json<W: Write, T: Serialize>(output: &Arc<StdMutex<W>>, value: &T) -> Result<()> {
    let mut output = output
        .lock()
        .map_err(|_| anyhow!("RPC stdout lock was poisoned"))?;
    write_json_line(&mut *output, value)
}
async fn read_jsonl<R: AsyncRead + Unpin>(
    mut input: R,
    lines: mpsc::Sender<JsonlFrame>,
) -> Result<()> {
    let mut buffer = Vec::new();
    let mut oversized = false;
    let mut chunk = [0_u8; 8192];
    loop {
        let count = input.read(&mut chunk).await.context("reading RPC stdin")?;
        if count == 0 {
            if oversized {
                let _ = lines.send(JsonlFrame::Oversized).await;
            } else if !buffer.is_empty() {
                let _ = lines.send(JsonlFrame::Unterminated).await;
            }
            return Ok(());
        }
        for &byte in &chunk[..count] {
            if byte == b'\n' {
                let frame = if oversized {
                    oversized = false;
                    JsonlFrame::Oversized
                } else {
                    trim_cr(&mut buffer);
                    JsonlFrame::Line(std::mem::take(&mut buffer))
                };
                if lines.send(frame).await.is_err() {
                    return Ok(());
                }
            } else if !oversized {
                if buffer.len() == MAX_RPC_MESSAGE_BYTES {
                    buffer.clear();
                    oversized = true;
                } else {
                    buffer.push(byte);
                }
            }
        }
    }
}
fn trim_cr(line: &mut Vec<u8>) {
    if line.last() == Some(&b'\r') {
        line.pop();
    }
}
fn rpc_command_from_workflow(command: crate::workflow_rpc::WorkflowRpcCommand) -> RpcCommand {
    match command {
        crate::workflow_rpc::WorkflowRpcCommand::WorkflowCreate {
            id,
            name,
            objective,
        } => RpcCommand::WorkflowCreate {
            id,
            name,
            objective,
        },
        crate::workflow_rpc::WorkflowRpcCommand::WorkflowList { id } => {
            RpcCommand::WorkflowList { id }
        }
        crate::workflow_rpc::WorkflowRpcCommand::WorkflowGet {
            id,
            workflow_id,
            name,
        } => RpcCommand::WorkflowGet {
            id,
            workflow_id,
            name,
        },
        crate::workflow_rpc::WorkflowRpcCommand::WorkflowDetail {
            id,
            workflow_id,
            name,
        } => RpcCommand::WorkflowDetail {
            id,
            workflow_id,
            name,
        },
        crate::workflow_rpc::WorkflowRpcCommand::WorkflowPause { id, workflow_id } => {
            RpcCommand::WorkflowPause { id, workflow_id }
        }
        crate::workflow_rpc::WorkflowRpcCommand::WorkflowResume { id, workflow_id } => {
            RpcCommand::WorkflowResume { id, workflow_id }
        }
        crate::workflow_rpc::WorkflowRpcCommand::WorkflowCancel { id, workflow_id } => {
            RpcCommand::WorkflowCancel { id, workflow_id }
        }
        crate::workflow_rpc::WorkflowRpcCommand::WorkflowIntegrate { id, workflow_id } => {
            RpcCommand::WorkflowIntegrate { id, workflow_id }
        }
        crate::workflow_rpc::WorkflowRpcCommand::WorkflowRemove { id, workflow_id } => {
            RpcCommand::WorkflowRemove { id, workflow_id }
        }
    }
}
/// Field-aware context for a command-frame deserialization error that hits a
/// sequence (`Vec`) field. Serde's `from_value` error does not name the
/// failing field, so the parse boundary maps the error back to the command
/// variant whose sequence field is unambiguous (each command below exposes
/// exactly one non-optional sequence). OPTIONAL sequence fields
/// (`images`, `jobIds`) already route through [`null_default_vec`] and never
/// reach this point, so a remaining `expected a sequence` error can only
/// come from a REQUIRED array — which must stay rejected with a message that
/// names the field so the frontend can render an actionable protocol error.
/// No payload contents are ever echoed.
fn sequence_field_hint(command: &str, error: &serde_json::Error) -> Option<String> {
    if !error.to_string().contains("expected a sequence") {
        return None;
    }
    let field = match command {
        "set_todos" => Some("phases"),
        "process_keys" => Some("keys"),
        "process_spawn" => Some("spec.argv"),
        _ => None,
    }?;
    Some(format!(
        "field `{field}` must be an array — send an array or omit it if the field is optional"
    ))
}

fn tag_parse_failure(response: RpcResponse, session_id: Option<&str>) -> RpcResponse {
    match session_id {
        Some(session_id) => response.with_session_id(session_id),
        None => response,
    }
}

pub(crate) fn parse_input(line: &[u8]) -> std::result::Result<RpcInput, RpcResponse> {
    let mut value: Value = serde_json::from_slice(line).map_err(|e| {
        RpcResponse::failure(None, "parse", format!("Failed to parse command: {e}"))
    })?;
    let id = value.get("id").and_then(Value::as_str).map(str::to_owned);
    // Optional top-level sessionId selects a runtime on the Web control
    // plane. It is stripped BEFORE command deserialization so no variant
    // carries it (the workflow commands use deny_unknown_fields schemas).
    let session_id = match value.get("sessionId") {
        None => None,
        Some(Value::String(session_id)) => Some(session_id.clone()),
        Some(_) => {
            return Err(RpcResponse::failure(
                id,
                "parse",
                "sessionId must be a string",
            ));
        }
    };
    if session_id.is_some()
        && let Some(object) = value.as_object_mut()
    {
        object.remove("sessionId");
    }
    let Some(command) = value.get("type").and_then(Value::as_str).map(str::to_owned) else {
        return Err(tag_parse_failure(
            RpcResponse::failure(
                id,
                "parse",
                "Failed to parse command: missing string field `type`",
            ),
            session_id.as_deref(),
        ));
    };
    if command == "extension_ui_response" {
        return serde_json::from_value(value)
            .map(RpcInput::ExtensionUiResponse)
            .map_err(|e| {
                tag_parse_failure(
                    RpcResponse::failure(
                        id,
                        command,
                        format!("Invalid extension UI response: {e}"),
                    ),
                    session_id.as_deref(),
                )
            });
    }
    // workflow_hub_send is Application-routed (not WorkflowRpcCommand) but
    // still uses a deny_unknown_fields camelCase wire schema.
    if command == "workflow_hub_send" {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WorkflowHubSendWire {
            #[serde(default)]
            id: Option<String>,
            #[serde(rename = "type")]
            _type: String,
            #[serde(rename = "workflowId")]
            workflow_id: String,
            generation: u64,
            recipient: String,
            body: String,
            #[serde(default, rename = "replyTo")]
            reply_to: Option<String>,
        }
        return match serde_json::from_value::<WorkflowHubSendWire>(value) {
            Ok(wire) => Ok(RpcInput::Command {
                command: RpcCommand::WorkflowHubSend {
                    id: wire.id,
                    workflow_id: wire.workflow_id,
                    generation: wire.generation,
                    recipient: wire.recipient,
                    body: wire.body,
                    reply_to: wire.reply_to,
                },
                session_id,
            }),
            Err(error) => Err(tag_parse_failure(
                RpcResponse::failure(id, command, format!("Invalid command: {error}")),
                session_id.as_deref(),
            )),
        };
    }
    // Workflow commands use a deny_unknown_fields wire schema.
    if crate::workflow_rpc::WorkflowRpcCommand::is_workflow_type(&command) {
        return match crate::workflow_rpc::parse_workflow_command(value) {
            Ok(workflow) => Ok(RpcInput::Command {
                command: rpc_command_from_workflow(workflow),
                session_id,
            }),
            Err(error) => Err(tag_parse_failure(
                RpcResponse::failure(id, command, format!("Invalid command: {error}")),
                session_id.as_deref(),
            )),
        };
    }
    serde_json::from_value(value)
        .map(|command| RpcInput::Command {
            command,
            session_id: session_id.clone(),
        })
        .map_err(|e| {
            let message = if e.to_string().starts_with("unknown variant") {
                format!("Unknown command: {command}")
            } else if let Some(hint) = sequence_field_hint(&command, &e) {
                format!("Invalid command {command}: {hint}")
            } else {
                format!("Invalid command: {e}")
            };
            tag_parse_failure(
                RpcResponse::failure(id, command, message),
                session_id.as_deref(),
            )
        })
}
fn handle_ui_response(adapter: &ExtensionUiAdapter, r: RpcExtensionUiResponse) -> Result<()> {
    if r.cancelled == Some(true) {
        return adapter.cancel(&r.id);
    }
    if let Some(v) = r.confirmed {
        return adapter.respond_confirmed(&r.id, v);
    }
    if let Some(v) = r.value {
        return adapter.respond_value(&r.id, v);
    }
    bail!("extension UI response must contain value, confirmed, or cancelled: true")
}
fn ui_event_request(event: ExtensionUiEvent) -> Result<Option<RpcExtensionUiRequest>> {
    let id = uuid::Uuid::new_v4().to_string();
    let request = match event {
        ExtensionUiEvent::InteractionRequested { interaction } => {
            let mut projected = ui_request(interaction.id, interaction.request)?;
            if let Some(object) = projected.request.as_object_mut() {
                object.insert(
                    "extensionId".to_owned(),
                    json!(interaction.context.instance.extension_id),
                );
                object.insert(
                    "generation".to_owned(),
                    json!(interaction.context.instance.generation),
                );
            }
            return Ok(Some(projected));
        }
        ExtensionUiEvent::Notification { notification } => json!({
            "method": "notify",
            "message": notification.message,
            "notifyType": match notification.level {
                pi_coding::UiNotificationLevel::Info => "info",
                pi_coding::UiNotificationLevel::Warning => "warning",
                pi_coding::UiNotificationLevel::Error => "error",
            },
            "extensionId": notification.instance.extension_id,
            "generation": notification.instance.generation,
        }),
        ExtensionUiEvent::StatusChanged { item } => json!({
            "method": "setStatus",
            "statusKey": item.key,
            "statusText": item.text,
            "extensionId": item.instance.extension_id,
            "generation": item.instance.generation,
        }),
        ExtensionUiEvent::StatusCleared { instance, key } => json!({
            "method": "setStatus",
            "statusKey": key,
            "statusText": Value::Null,
            "extensionId": instance.extension_id,
            "generation": instance.generation,
        }),
        ExtensionUiEvent::WidgetChanged { item } => json!({
            "method": "setWidget",
            "widgetKey": item.key,
            "widgetLines": item.lines,
            "widgetPlacement": match item.placement {
                pi_coding::UiWidgetPlacement::AboveEditor => "aboveEditor",
                pi_coding::UiWidgetPlacement::BelowEditor => "belowEditor",
            },
            "extensionId": item.instance.extension_id,
            "generation": item.instance.generation,
        }),
        ExtensionUiEvent::WidgetCleared { instance, key } => json!({
            "method": "setWidget",
            "widgetKey": key,
            "widgetLines": Value::Null,
            "widgetPlacement": "aboveEditor",
            "extensionId": instance.extension_id,
            "generation": instance.generation,
        }),
        ExtensionUiEvent::TitleChanged { instance, title } => json!({
            "method": "setTitle",
            "title": title,
            "extensionId": instance.extension_id,
            "generation": instance.generation,
        }),
        ExtensionUiEvent::EditorTextChanged { instance, text } => json!({
            "method": "set_editor_text",
            "text": text,
            "extensionId": instance.extension_id,
            "generation": instance.generation,
        }),
        ExtensionUiEvent::WorkingMessageChanged { instance, message } => json!({
            "method": "set_working_message",
            "message": message,
            "extensionId": instance.extension_id,
            "generation": instance.generation,
        }),
        ExtensionUiEvent::WorkingVisibilityChanged { instance, visible } => json!({
            "method": "set_working_visible",
            "visible": visible,
            "extensionId": instance.extension_id,
            "generation": instance.generation,
        }),
        ExtensionUiEvent::WorkingIndicatorChanged { instance, options } => json!({
            "method": "set_working_indicator",
            "options": options,
            "extensionId": instance.extension_id,
            "generation": instance.generation,
        }),
        ExtensionUiEvent::HiddenThinkingLabelChanged { instance, label } => json!({
            "method": "set_hidden_thinking_label",
            "label": label,
            "extensionId": instance.extension_id,
            "generation": instance.generation,
        }),
        ExtensionUiEvent::ThemeChanged { instance, name } => json!({
            "method": "set_theme",
            "name": name,
            "extensionId": instance.extension_id,
            "generation": instance.generation,
        }),
        ExtensionUiEvent::ToolsExpandedChanged { instance, expanded } => json!({
            "method": "set_tools_expanded",
            "expanded": expanded,
            "extensionId": instance.extension_id,
            "generation": instance.generation,
        }),
        ExtensionUiEvent::ExtensionCleared { instance } => json!({
            "method": "extension_cleared",
            "extensionId": instance.extension_id,
            "generation": instance.generation,
        }),
        ExtensionUiEvent::OverlayOpenRequested {
            instance,
            id,
            title,
            rows,
            non_capturing,
            input,
        } => json!({
            "method": "overlay_open",
            "overlayId": id,
            "title": title,
            "rows": rows,
            "nonCapturing": non_capturing,
            "input": input,
            "extensionId": instance.extension_id,
            "generation": instance.generation,
        }),
        ExtensionUiEvent::OverlayRowsChanged { instance, id, rows } => json!({
            "method": "overlay_rows_changed",
            "overlayId": id,
            "rows": rows,
            "extensionId": instance.extension_id,
            "generation": instance.generation,
        }),
    };
    Ok(Some(RpcExtensionUiRequest {
        record_type: "extension_ui_request",
        id,
        session_id: None,
        runtime_incarnation: None,
        request,
    }))
}

fn ui_request(id: String, request: pi_coding::ExtensionUiRequest) -> Result<RpcExtensionUiRequest> {
    use pi_coding::{ExtensionUiRequest, UiNotificationLevel, UiWidgetPlacement};
    let request = match request {
        ExtensionUiRequest::Select { title, options } => json!({
            "method": "select", "title": title,
            "options": options.into_iter().map(|option| option.value).collect::<Vec<_>>(),
        }),
        ExtensionUiRequest::Confirm { title, message } => {
            json!({ "method": "confirm", "title": title, "message": message })
        }
        ExtensionUiRequest::Input {
            title, placeholder, ..
        } => json!({ "method": "input", "title": title, "placeholder": placeholder }),
        ExtensionUiRequest::Editor { title, prefill } => {
            json!({ "method": "editor", "title": title, "prefill": prefill })
        }
        ExtensionUiRequest::Notify { message, level } => json!({
            "method": "notify", "message": message,
            "notifyType": match level { UiNotificationLevel::Info => "info", UiNotificationLevel::Warning => "warning", UiNotificationLevel::Error => "error" },
        }),
        ExtensionUiRequest::Status { key, text } => {
            json!({ "method": "setStatus", "statusKey": key, "statusText": text })
        }
        ExtensionUiRequest::Widget {
            key,
            lines,
            placement,
        } => json!({
            "method": "setWidget", "widgetKey": key, "widgetLines": lines,
            "widgetPlacement": match placement { UiWidgetPlacement::AboveEditor => "aboveEditor", UiWidgetPlacement::BelowEditor => "belowEditor" },
        }),
        ExtensionUiRequest::Title { title } => json!({ "method": "setTitle", "title": title }),
        ExtensionUiRequest::SetEditorText { text } => {
            json!({ "method": "set_editor_text", "text": text })
        }
        ExtensionUiRequest::GetEditorText => {
            bail!(
                "getEditorText requires canonical host editor state and is unsupported by the RPC shadow adapter"
            )
        }
        ExtensionUiRequest::PasteToEditor { text } => {
            json!({ "method": "paste_to_editor", "text": text })
        }
        ExtensionUiRequest::SetWorkingMessage { message } => {
            json!({ "method": "set_working_message", "message": message })
        }
        ExtensionUiRequest::SetWorkingVisible { visible } => {
            json!({ "method": "set_working_visible", "visible": visible })
        }
        ExtensionUiRequest::SetWorkingIndicator { options } => {
            json!({ "method": "set_working_indicator", "options": options })
        }
        ExtensionUiRequest::SetHiddenThinkingLabel { label } => {
            json!({ "method": "set_hidden_thinking_label", "label": label })
        }
        ExtensionUiRequest::GetAllThemes => {
            bail!(
                "getAllThemes requires canonical host theme state and is unsupported by the RPC shadow adapter"
            )
        }
        ExtensionUiRequest::GetTheme { .. } => {
            bail!(
                "getTheme requires canonical host theme state and is unsupported by the RPC shadow adapter"
            )
        }
        ExtensionUiRequest::SetTheme { name } => {
            json!({ "method": "set_theme", "name": name })
        }
        ExtensionUiRequest::GetToolsExpanded => {
            bail!(
                "getToolsExpanded requires canonical host tool expansion state and is unsupported by the RPC shadow adapter"
            )
        }
        ExtensionUiRequest::SetToolsExpanded { expanded } => {
            json!({ "method": "set_tools_expanded", "expanded": expanded })
        }
        ExtensionUiRequest::OverlaySetRows { id, rows } => {
            json!({ "method": "overlay_set_rows", "overlayId": id, "rows": rows })
        }
        ExtensionUiRequest::OverlayOpen {
            id,
            non_capturing,
            input,
            ..
        } => {
            json!({
                "method": "overlay_open",
                "overlayId": id,
                "nonCapturing": non_capturing,
                "input": input,
            })
        }
    };
    Ok(RpcExtensionUiRequest {
        record_type: "extension_ui_request",
        id,
        session_id: None,
        runtime_incarnation: None,
        request,
    })
}
fn workflow_hub_send_response_data(
    workflow_id: String,
    generation: u64,
    recipient: String,
    receipts: Vec<pi_coding::DeliveryReceipt>,
) -> Value {
    let receipts = receipts
        .into_iter()
        .map(|receipt| {
            json!({
                "to": receipt.to,
                "requested": receipt.requested,
                "outcome": receipt.outcome,
                "messageId": receipt.message_id,
                "error": receipt.error,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "workflowId": workflow_id,
        "generation": generation,
        "recipient": recipient,
        "receipts": receipts,
    })
}

async fn handle_command(
    app: &Application,
    settings: &crate::settings_rpc::SettingsRpcState,
    workflows: &crate::workflow_rpc::WorkflowRpcState,
    command: RpcCommand,
) -> RpcResponse {
    handle_command_with_session_storage(app, settings, workflows, None, command).await
}

async fn handle_command_with_session_storage(
    app: &Application,
    settings: &crate::settings_rpc::SettingsRpcState,
    workflows: &crate::workflow_rpc::WorkflowRpcState,
    session_storage: Option<&crate::session_run::SessionStorage>,
    c: RpcCommand,
) -> RpcResponse {
    let id = c.id();
    let name = c.command_name();
    if matches!(c, RpcCommand::GetTree { .. }) {
        let raw = app
            .session_tree()
            .and_then(|tree| tree.to_json_string().map_err(Into::into));
        return match raw.and_then(|json| {
            RpcResponse::success_raw(id.clone(), name.clone(), json).map_err(Into::into)
        }) {
            Ok(response) => response,
            Err(error) => RpcResponse::failure(
                id,
                name,
                format!("Failed to serialize session tree: {error}"),
            ),
        };
    }
    if matches!(c, RpcCommand::GetForkMessages { .. }) {
        return match Box::pin(handle_command_inner(app, settings, workflows, session_storage, c)).await {
            Ok(data) => bounded_fork_messages_response(id, name, None, data),
            Err(error) => RpcResponse::failure(id, name, error.to_string()),
        };
    }

    match Box::pin(handle_command_inner(app, settings, workflows, session_storage, c)).await {
        Ok(data) => RpcResponse::success(id, name, data),
        Err(e) => {
            let mut response = RpcResponse::failure(id, name, e.to_string());
            attach_handle_command_error_code(&e, &mut response);
            response
        }
    }
}

fn attach_handle_command_error_code(error: &anyhow::Error, response: &mut RpcResponse) {
    if error.downcast_ref::<TranscriptUnavailable>().is_some() {
        response.data = Some(json!({ "code": "transcript_unavailable" }));
        return;
    }
    if let Some(typed) = error.downcast_ref::<SessionAlreadyProcessing>() {
        response.data = Some(json!({ "code": typed.code() }));
    }
}

pub(crate) fn bounded_fork_messages_response(
    id: Option<String>,
    name: impl Into<String>,
    session_id: Option<String>,
    data: Option<Value>,
) -> RpcResponse {
    let name = name.into();
    let attach_session = |response: RpcResponse, owner: &Option<String>| match owner {
        Some(owner) => response.with_session_id(owner.clone()),
        None => response,
    };
    let response = attach_session(
        RpcResponse::success(id.clone(), name.clone(), data),
        &session_id,
    );
    if serde_json::to_vec(&response)
        .is_ok_and(|encoded| encoded.len() <= MAX_FORK_MESSAGES_RESPONSE_BYTES)
    {
        return response;
    }
    let bounded_id = id.filter(|value| value.len() <= 4096);
    let bounded_session_id = session_id.filter(|value| value.len() <= 4096);
    attach_session(
        RpcResponse::failure(
            bounded_id,
            name,
            "fork message page exceeds the response byte limit",
        ),
        &bounded_session_id,
    )
}
/// Upper bound for persona definition content returned over RPC. The core
/// discovery bound (`MAX_AGENT_DEFINITION_BYTES`, 256 KiB) already caps every
/// discoverable `persona.md`; the wire cap matches it so a get never returns
/// more than the core would ever load, and the truncation flag keeps the
/// client honest about partial content.
const MAX_PERSONA_RPC_CONTENT_BYTES: usize = 256 * 1024;

/// One path-free persona row for the Web catalog (the `/persona` list
/// mirror). State-count errors collapse to the fixed literal `"unreadable"`
/// so no filesystem path text ever crosses the wire.
fn persona_row_json(definition: &pi_coding::AgentDefinition, preferred: Option<&str>) -> Value {
    let counts = crate::agents_panel::persona_state_counts(definition.persona_root());
    let source = match definition.source {
        pi_coding::AgentDefinitionSource::Project => "project",
        pi_coding::AgentDefinitionSource::User => "user",
        pi_coding::AgentDefinitionSource::Bundled => "bundled",
    };
    json!({
        "name": definition.name,
        "description": definition.description,
        "source": source,
        "trusted": definition.trusted,
        "preferred": preferred == Some(definition.name.as_str()),
        "contractSummary": crate::agents_panel::persona_contract_summary(definition),
        "memoryEntries": counts.memory_entries,
        "sessionCount": counts.transcript_count,
        "stateError": counts.error.as_ref().map(|_| "unreadable"),
    })
}

/// Loaded persona definitions (persona-kind only, discovery order) from the
/// session's resource snapshot; `None` when the session has no resource
/// manager.
fn persona_definitions(app: &Application) -> Option<Vec<pi_coding::AgentDefinition>> {
    app.resource_snapshot().map(|snapshot| {
        snapshot
            .agents
            .iter()
            .filter(|definition| definition.is_persona())
            .cloned()
            .collect::<Vec<_>>()
    })
}

/// Test seam for the `SessionList` off-worker contract.
///
/// The `SessionList` handler records, keyed by the command's RPC `id`, the
/// id of the thread that actually runs `load_resume_catalog` from inside its
/// `spawn_blocking` closure. Each regression test passes its own unique `id`
/// and reads back its own entry, so parallel `SessionList` tests can never
/// overwrite one another's record — a synchronous inline revert for one test
/// cannot be masked by a concurrent test's `spawn_blocking` write. This is
/// `cfg(test)`-gated, so it is compiled out of every production build and
/// touches no wire shape, no public API, no type contract, and no runtime
/// behavior — it only records a `ThreadId` while the test binary is running.
#[cfg(test)]
static SESSION_LIST_LOADER_THREADS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, std::thread::ThreadId>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(test)]
const SESSION_LIST_PANIC_SEAM_KEY: &str = "session-list-worker-panic";

fn session_catalog_join_error(error: tokio::task::JoinError) -> anyhow::Error {
    if error.is_cancelled() {
        anyhow!("session catalog scan was cancelled")
    } else if error.is_panic() {
        anyhow!("session catalog scan worker panicked")
    } else {
        anyhow!("session catalog scan worker failed")
    }
}

async fn handle_command_inner(
    app: &Application,
    settings: &crate::settings_rpc::SettingsRpcState,
    workflows: &crate::workflow_rpc::WorkflowRpcState,
    session_storage: Option<&crate::session_run::SessionStorage>,
    c: RpcCommand,
) -> Result<Option<Value>> {
    match c {
        RpcCommand::GoalCreate {
            objective,
            token_budget,
            activate,
            ..
        } => {
            // TUI parity: with `activate` the create mirrors `/goal create`
            // (goal_commands.rs `execute_interactive_goal_command`) — create
            // AND start/queue the first work turn, resolving with the
            // activation outcome; without it the mutation-only contract the
            // Web Goal panel uses is preserved, resolving with the goal.
            let value = if activate {
                serde_json::to_value(app.activate_goal(objective, token_budget).await?)?
            } else {
                serde_json::to_value(app.goal_create(objective, token_budget)?)?
            };
            Ok(Some(value))
        }
        RpcCommand::GoalGet { .. } => Ok(Some(serde_json::to_value(app.goal_state())?)),
        RpcCommand::GoalPause { .. } => Ok(Some(serde_json::to_value(app.goal_pause()?)?)),
        RpcCommand::GoalResume { activate, .. } => {
            // TUI parity: with `activate` the resume mirrors `/goal resume` —
            // resume AND schedule exactly one continuation turn
            // (`resume_goal_work`), resolving with the activation outcome;
            // without it the mutation-only contract is preserved.
            let value = if activate {
                serde_json::to_value(app.resume_goal_work().await?)?
            } else {
                serde_json::to_value(app.goal_resume()?)?
            };
            Ok(Some(value))
        }
        RpcCommand::GoalComplete { .. } => Ok(Some(serde_json::to_value(app.goal_complete()?)?)),
        RpcCommand::GoalDrop { .. } => Ok(Some(serde_json::to_value(app.goal_drop()?)?)),
        RpcCommand::GoalUpdateUsage {
            tokens,
            active_time_seconds,
            ..
        } => Ok(Some(serde_json::to_value(app.goal_update_usage(
            GoalUsageDelta::new(tokens, active_time_seconds),
        )?)?)),
        RpcCommand::GoalPin { text, .. } => Ok(Some(serde_json::to_value(app.goal_pin(text)?)?)),
        RpcCommand::GoalUnpin { index, .. } => {
            Ok(Some(serde_json::to_value(app.goal_unpin(index)?)?))
        }
        RpcCommand::GoalJournal { .. } => Ok(Some(serde_json::to_value(app.goal_journal()?)?)),
        RpcCommand::SettingsInspect { .. } => Ok(Some(serde_json::to_value(
            crate::settings_rpc::SettingsRpcState::inspect(app)
                .ok_or_else(|| anyhow!("session has no resource manager"))?,
        )?)),
        RpcCommand::SettingsSearch { query, .. } => Ok(Some(serde_json::to_value(
            crate::settings_rpc::SettingsRpcState::search(app, &query)?,
        )?)),
        RpcCommand::SettingsOpenDraft { scope, .. } => {
            Ok(Some(serde_json::to_value(settings.open(app, scope)?)?))
        }
        RpcCommand::SettingsGetDraft { draft_id, .. } => {
            Ok(Some(serde_json::to_value(settings.get(&draft_id)?)?))
        }
        RpcCommand::SettingsSet {
            draft_id,
            key,
            value,
            ..
        } => Ok(Some(serde_json::to_value(
            settings.set(&draft_id, &key, value)?,
        )?)),
        RpcCommand::SettingsReset { draft_id, key, .. } => Ok(Some(serde_json::to_value(
            settings.reset(&draft_id, &key)?,
        )?)),
        RpcCommand::SettingsValidate { draft_id, .. } => {
            Ok(Some(serde_json::to_value(settings.validate(&draft_id)?)?))
        }
        RpcCommand::SettingsApply { draft_id, .. } => Ok(Some(serde_json::to_value(
            settings.apply(app, &draft_id).await?,
        )?)),
        RpcCommand::SettingsCancel { draft_id, .. } => {
            settings.cancel(&draft_id)?;
            Ok(Some(json!({"cancelled":true})))
        }
        RpcCommand::Prompt {
            message,
            images,
            streaming_behavior,
            ..
        } => Ok(app
            .prompt(message, images, streaming_behavior)
            .await?
            .map(|accepted| serde_json::to_value(accepted))
            .transpose()?),
        RpcCommand::Steer {
            message, images, ..
        } => {
            app.steer(message, images).await?;
            Ok(None)
        }
        RpcCommand::FollowUp {
            message, images, ..
        } => {
            app.follow_up(message, images).await?;
            Ok(None)
        }
        RpcCommand::Abort { .. } => {
            app.abort().await;
            Ok(None)
        }
        RpcCommand::NewSession {
            parent_session,
            cwd,
            ..
        } => {
            if let Some(cwd) = cwd.as_deref().map(str::trim).filter(|cwd| !cwd.is_empty()) {
                let path = Path::new(cwd);
                if !path.is_absolute() {
                    bail!("new session cwd must be absolute");
                }
                pi_coding::inspect_operator_directory(path)
                    .context("validating new session cwd")?;
                bail!("new_session cwd is only supported by the multi-session Web runtime");
            }
            let outcome = app
                .new_session_with_parent(parent_session.as_deref().map(Path::new))
                .await?;
            if !outcome.cancelled {
                crate::session_run::rebind_workflows_for_active_session(&app)
                    .await
                    .context("rebinding workflow storage after new session")?;
            }
            Ok(Some(json!({"cancelled":outcome.cancelled})))
        }
        RpcCommand::GetState { .. } => Ok(Some(serde_json::to_value(
            RpcSessionState::from_application(
                app.state().await,
                app.active_turn_model(),
                app.runtime_settings_state(),
                app.session().cwd(),
                None,
            ),
        )?)),
        RpcCommand::SetModel {
            provider, model_id, ..
        } => Ok(Some(serde_json::to_value(
            set_model(app, &provider, &model_id).await?,
        )?)),
        RpcCommand::CycleModel { .. } => cycle_model(app).await,
        RpcCommand::GetAvailableModels { .. } => {
            Ok(Some(json!({"models":available_models().await?})))
        }
        RpcCommand::SetThinkingLevel { level, .. } => {
            let change = app.set_thinking_level(level);
            Ok(Some(json!({
                "requested": change.requested,
                "level": change.effective,
                "clamped": change.clamped,
                "message": change.message,
            })))
        }
        RpcCommand::CycleThinkingLevel { .. } => {
            let s = app.state().await;
            let Some(m) = s.model.as_ref() else {
                return Ok(Some(Value::Null));
            };
            let levels = available_thinking_levels(m);
            if levels.len() <= 1 {
                return Ok(Some(Value::Null));
            }
            let index = levels
                .iter()
                .position(|l| *l == s.thinking_level)
                .unwrap_or(0);
            let level = levels[(index + 1) % levels.len()];
            let change = app.set_thinking_level(level);
            Ok(Some(json!({
                "requested": change.requested,
                "level": change.effective,
                "clamped": change.clamped,
                "message": change.message,
            })))
        }
        RpcCommand::GetAvailableThinkingLevels { .. } => Ok(Some(
            json!({"levels":app.state().await.model.as_ref().map_or_else(||vec![ThinkingLevel::Off],available_thinking_levels)}),
        )),
        RpcCommand::SetSteeringMode { mode, .. } => {
            app.set_steering_mode(mode).await;
            Ok(None)
        }
        RpcCommand::SetFollowUpMode { mode, .. } => {
            app.set_follow_up_mode(mode).await;
            Ok(None)
        }
        RpcCommand::Compact {
            custom_instructions,
            ..
        } => Ok(Some(serde_json::to_value(
            app.compact(custom_instructions.as_deref()).await?,
        )?)),
        RpcCommand::SetAutoCompaction { enabled, .. } => {
            app.set_auto_compaction_enabled(enabled);
            Ok(None)
        }
        RpcCommand::SetAutoRetry { enabled, .. } => {
            app.set_auto_retry_enabled(enabled);
            Ok(None)
        }
        RpcCommand::AbortRetry { .. } => {
            app.abort_retry();
            Ok(None)
        }
        RpcCommand::Bash {
            id,
            command,
            exclude_from_context,
        } => Ok(Some(serde_json::to_value(
            app.execute_bash_with_id(command, exclude_from_context.unwrap_or(false), id)
                .await?,
        )?)),
        RpcCommand::AbortBash { .. } => {
            app.abort_bash();
            Ok(None)
        }
        RpcCommand::GetSessionStats { .. } => Ok(Some(serde_json::to_value(app.session_stats())?)),
        RpcCommand::SessionList { id: _id, scope, .. } => {
            let session = app.session();
            let catalog = match scope {
                RpcSessionListScope::Current => pi_coding::SessionCatalog::from_env()?
                    .with_native_session_root(session.session_dir()),
                RpcSessionListScope::AllProjects => session_storage
                    .ok_or_else(|| {
                        anyhow!(
                            "all-project session catalog is unavailable outside the Web listener"
                        )
                    })?
                    .catalog()?,
            };
            let cwd_scope =
                matches!(scope, RpcSessionListScope::Current).then(|| session.cwd().to_path_buf());
            let sources = match scope {
                RpcSessionListScope::Current => {
                    crate::resume_catalog::effective_resume_sources(app)
                }
                RpcSessionListScope::AllProjects => crate::resume_catalog::web_resume_sources(app),
            };
            let request = crate::resume_catalog::ResumeCatalogRequest {
                sources,
                cwd_scope,
                ..crate::resume_catalog::ResumeCatalogRequest::default()
            };
            #[cfg(test)]
            let loader_key = _id.clone().unwrap_or_default();
            let rows = tokio::task::spawn_blocking(move || {
                #[cfg(test)]
                {
                    SESSION_LIST_LOADER_THREADS
                        .lock()
                        .expect("loader thread seam")
                        .insert(loader_key.clone(), std::thread::current().id());
                    if loader_key == SESSION_LIST_PANIC_SEAM_KEY {
                        panic!("session catalog worker panic seam");
                    }
                }
                crate::resume_catalog::load_resume_catalog(&catalog, &request)
            })
            .await
            .map_err(session_catalog_join_error)??;
            let rows = match scope {
                RpcSessionListScope::Current => rows
                    .rows
                    .into_iter()
                    .map(|row| (row, false))
                    .collect::<Vec<_>>(),
                RpcSessionListScope::AllProjects => {
                    // Imported foreign rows STAY VISIBLE next to their native
                    // copy (product decision): the row carries its
                    // AlreadyImported status + nativePath so the sidebar can
                    // badge it; no coalescing drop.
                    let unfiltered = rows.rows;
                    let mut rows = crate::resume_catalog::filter_web_noise_rows(unfiltered.clone());
                    // A brand-new session persists header-only (0 messages,
                    // <10KiB), so the noise filter would hide the CURRENT
                    // session's row after a reload. Re-union the live
                    // recorder's row after filtering; the recorder identity
                    // matches CatalogRow::session_id (see the live_name
                    // overlay below).
                    if let Some(live_id) = session.recorder_info().map(|(id, _)| id)
                        && !rows.iter().any(|row| row.session_id == live_id)
                        && let Some(recorder_row) =
                            unfiltered.iter().find(|row| row.session_id == live_id)
                    {
                        rows.push(recorder_row.clone());
                    }
                    let (regular, temporary) =
                        crate::resume_catalog::partition_web_noise_rows(rows);
                    regular
                        .into_iter()
                        .map(|row| (row, false))
                        .chain(temporary.into_iter().map(|row| (row, true)))
                        .collect::<Vec<_>>()
                }
            };
            // The catalog reads persisted session files; the live recorder's
            // name is authoritative and may not have flushed yet, so overlay
            // it only onto the row for the same physical recorder.
            let live_name = session.session_name();
            let live_session_id = session.recorder_info().map(|(id, _)| id);
            let sessions = rows
                .into_iter()
                .map(|(row, temporary)| {
                    let mut row = RpcSessionListRow::from_resume_row(row, temporary);
                    if live_name.is_some()
                        && live_session_id
                            .as_ref()
                            .is_some_and(|id| id == &row.session_id)
                    {
                        row.name = live_name.clone();
                    }
                    row
                })
                .collect::<Vec<_>>();
            Ok(Some(json!({"sessions": sessions})))
        }
        RpcCommand::ExportHtml {
            output_path,
            session_path,
            ..
        } => {
            if session_path.is_some() {
                bail!("sessionPath export is available only through the Web listener");
            }
            Ok(Some(
                json!({"path":app.export_html(output_path.as_deref().map(Path::new))?.to_string_lossy()}),
            ))
        }
        RpcCommand::SwitchSession {
            session_path,
            catalog_source,
            catalog_session_id,
            catalog_status,
            ..
        } => {
            // The stdio transport has no negotiated handoff; a malformed or
            // partial catalog tuple must fail closed instead of being
            // silently ignored.
            switch_catalog_identity(
                catalog_source.as_deref(),
                catalog_session_id.as_deref(),
                catalog_status.as_deref(),
            )
            .map_err(|error| anyhow!(error.message()))?;
            let catalog = pi_coding::SessionCatalog::from_env()?
                .with_native_session_root(app.session().session_dir());
            let prepared = catalog
                .prepare_native_resume(&session_path)
                .map_err(anyhow::Error::new)?;
            let outcome = app.switch_prepared_session(prepared).await?;
            if !outcome.cancelled {
                crate::session_run::rebind_workflows_for_active_session(&app)
                    .await
                    .context("rebinding workflow storage after session switch")?;
            }
            Ok(Some(json!({"cancelled":outcome.cancelled})))
        }
        RpcCommand::Fork {
            entry_id,
            session_path,
            ..
        } => {
            if let Some(session_path) = session_path {
                Ok(Some(stdio_branch_saved_session(
                    app,
                    session_storage,
                    &session_path,
                    Some(entry_id.as_str()),
                )?))
            } else {
                let outcome = app.fork_session(&entry_id).await?;
                if !outcome.cancelled {
                    crate::session_run::rebind_workflows_for_active_session(&app)
                        .await
                        .context("rebinding workflow storage after session fork")?;
                }
                Ok(Some(json!({
                    "text": outcome.text,
                    "cancelled": outcome.cancelled,
                })))
            }
        }
        RpcCommand::Clone { session_path, .. } => {
            if let Some(session_path) = session_path {
                Ok(Some(stdio_branch_saved_session(
                    app,
                    session_storage,
                    &session_path,
                    None,
                )?))
            } else {
                let outcome = app.clone_session().await?;
                if !outcome.cancelled {
                    crate::session_run::rebind_workflows_for_active_session(&app)
                        .await
                        .context("rebinding workflow storage after session clone")?;
                }
                Ok(Some(json!({"cancelled":outcome.cancelled})))
            }
        }
        RpcCommand::GetForkMessages {
            session_path,
            cursor,
            limit,
            ..
        } => {
            let page = if let Some(session_path) = session_path {
                let prepared = stdio_prepare_saved_session(app, &session_path)?;
                fork_messages_from_tree(prepared.tree(), cursor.as_deref(), limit)?
            } else {
                app.fork_messages(cursor.as_deref(), limit)?
            };
            Ok(Some(serde_json::to_value(page)?))
        }
        RpcCommand::GetEntries { since, .. } => {
            let mut entries = app.session_entries(since.as_deref())?;
            for entry in &mut entries.entries {
                if entry.custom_type.as_deref() == Some(pi_coding::LOOP_SCHEDULED_MESSAGE_TYPE) {
                    let details = entry.details.as_ref();
                    let task_id = details
                        .and_then(|value| value.get("taskId"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let schedule = details
                        .and_then(|value| value.get("schedule"))
                        .and_then(Value::as_str)
                        .unwrap_or("scheduled");
                    let prompt = details
                        .and_then(|value| value.get("prompt"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    entry.custom_type = Some(format!("Loop {task_id} · {schedule}"));
                    entry.content = Some(prompt.into());
                    entry.display = Some(true);
                }
            }
            Ok(Some(serde_json::to_value(entries)?))
        }
        RpcCommand::GetTree { .. } => bail!("GetTree must use the raw response path"),
        RpcCommand::GetLastAssistantText { .. } => {
            Ok(Some(json!({"text":app.last_assistant_text()})))
        }
        RpcCommand::SetSessionName {
            name, session_path, ..
        } => {
            if name.trim().is_empty() {
                bail!("Session name cannot be empty")
            }
            if let Some(session_path) = session_path {
                stdio_rename_saved_session(app, &session_path, name.trim())?;
            } else {
                app.set_session_name(name.trim())?;
            }
            Ok(None)
        }
        RpcCommand::SessionForkLatest { session_path, .. } => Ok(Some(stdio_branch_saved_session(
            app,
            session_storage,
            &session_path,
            Some("__latest__"),
        )?)),
        RpcCommand::SessionDelete { session_path, .. } => {
            Ok(Some(stdio_delete_saved_session(app, &session_path)?))
        }
        RpcCommand::SessionRestore { .. } => {
            bail!("session_restore is only supported on the Web control plane")
        }
        RpcCommand::SessionCopyContext { .. } => {
            bail!("session_copy_context is available only through the Web listener")
        }
        RpcCommand::SessionHandoff { .. } => {
            bail!("session_handoff is available only through the multi-session Web runtime")
        }
        RpcCommand::SessionMove { .. } => {
            bail!("session_move is available only through the multi-session Web runtime")
        }
        RpcCommand::GetMessages { .. } => {
            let rows = app.session().authoritative_transcript()?;
            Ok(Some(json!({ "messages": public_transcript_rows(rows) })))
        }
        RpcCommand::GetCommands { .. } => {
            // Project the Application's collision-free executable catalog —
            // builtins plus trusted dynamic prompt/skill/extension commands —
            // so the Web picker can surface loaded skill candidates without a
            // second hardcoded list. Collision/trust rules reuse
            // `interactive_commands::executable_catalog` (builtins win, the
            // first dynamic name wins, conflicting entries are dropped with a
            // diagnostic). Each skill entry carries a stable `skillName`
            // (the bare name behind the `skill:<name>` wire name) so the Web
            // composer can compose `/skill <name>` without re-parsing.
            let (catalog, _diagnostics) = crate::interactive_commands::executable_catalog(app);
            let commands = catalog
                .into_iter()
                .map(|command| {
                    let builtin = crate::interactive_commands::builtin(&command.name);
                    let skill_name =
                        if command.source == crate::interactive_commands::CommandSource::Skill {
                            command.name.strip_prefix("skill:").map(str::to_owned)
                        } else {
                            None
                        };
                    json!({
                        "name": command.name,
                        "description": command.description,
                        "source": command_source_str(command.source),
                        "argumentHint": builtin.and_then(|command| command.argument_hint),
                        "requiresArguments": builtin.is_some_and(|command| command.requires_arguments),
                        "skillName": skill_name,
                    })
                })
                .collect::<Vec<_>>();
            Ok(Some(json!({"commands":commands})))
        }
        RpcCommand::Run { name, args, .. } => {
            let name = name.trim();
            if name.is_empty() {
                bail!("run command name is required");
            }
            let value =
                crate::interactive_commands::invoke_extension_command(app, name, args).await?;
            Ok(Some(json!({ "name": name, "value": value })))
        }
        RpcCommand::Skill { name, .. } => {
            let name = name.trim();
            if name.is_empty() {
                bail!("skill name is required");
            }
            let summary = crate::interactive_commands::skill_frontmatter_summary(app, name)
                .ok_or_else(|| anyhow!("unknown skill '{name}'"))?;
            Ok(Some(json!({ "name": name, "summary": summary })))
        }
        RpcCommand::PersonaList { .. } => {
            let Some(personas) = persona_definitions(app) else {
                return Ok(Some(json!({ "enabled": false, "personas": [] })));
            };
            let preferred = crate::interactive_commands::current_preferred_agent(app);
            let rows = personas
                .iter()
                .map(|definition| persona_row_json(definition, preferred.as_deref()))
                .collect::<Vec<_>>();
            Ok(Some(json!({ "enabled": true, "personas": rows })))
        }
        RpcCommand::PersonaGet { name, .. } => {
            let Some(personas) = persona_definitions(app) else {
                bail!("persona catalog is unavailable in this session");
            };
            let preferred = crate::interactive_commands::current_preferred_agent(app);
            let definition = personas
                .iter()
                .find(|definition| definition.name == name)
                .ok_or_else(|| {
                    anyhow!("unknown persona {name:?}; persona_list lists available personas")
                })?;
            let content = crate::interactive_commands::persona_editor_seed(
                app,
                &name,
                crate::interactive_commands::PersonaEditKind::Edit,
            )
            .with_context(|| format!("reading persona {name:?} definition"))?;
            // Byte-cap on a UTF-8 char boundary so truncation never splits a
            // codepoint (the core bound is 256 KiB of bytes). Walk back at
            // most 3 continuation bytes — `floor_char_boundary` is not
            // available on the pinned toolchain.
            let (content, content_truncated) = if content.len() > MAX_PERSONA_RPC_CONTENT_BYTES {
                let mut end = MAX_PERSONA_RPC_CONTENT_BYTES;
                while end > 0 && !content.is_char_boundary(end) {
                    end -= 1;
                }
                (content[..end].to_owned(), true)
            } else {
                (content, false)
            };
            let mut row = persona_row_json(definition, preferred.as_deref());
            if let Some(object) = row.as_object_mut() {
                object.insert("content".to_owned(), json!(content));
                object.insert("contentTruncated".to_owned(), json!(content_truncated));
            }
            Ok(Some(row))
        }
        RpcCommand::PersonaCreate { name, content, .. } => {
            let message = crate::interactive_commands::commit_persona_definition(
                app,
                &name,
                &content,
                crate::interactive_commands::PersonaEditKind::New,
            )
            .await
            // The commit chain is path-free by construction (relative probe
            // paths only, persona FS contexts carry fixed labels), so the
            // FULL chain — including the frontmatter validation root cause —
            // is safe to surface on the wire.
            .map_err(|error| anyhow!("{}", format!("{error:#}")))?;
            Ok(Some(
                json!({ "name": name, "created": true, "message": message }),
            ))
        }
        RpcCommand::PersonaEdit { name, content, .. } => {
            let message = crate::interactive_commands::commit_persona_definition(
                app,
                &name,
                &content,
                crate::interactive_commands::PersonaEditKind::Edit,
            )
            .await
            .map_err(|error| anyhow!("{}", format!("{error:#}")))?;
            Ok(Some(
                json!({ "name": name, "edited": true, "message": message }),
            ))
        }
        RpcCommand::PersonaRemove { name, confirm, .. } => {
            if !confirm {
                bail!(
                    "persona_remove requires confirm: true (mirrors /persona remove <name> --yes); refusing without confirmation"
                );
            }
            let message = crate::interactive_commands::execute_interactive_persona_command(
                app,
                crate::interactive_commands::InteractivePersonaCommand::Remove {
                    name: name.clone(),
                },
            )
            .await?;
            Ok(Some(
                json!({ "name": name, "removed": true, "message": message }),
            ))
        }
        RpcCommand::PersonaPurge { name, confirm, .. } => {
            if !confirm {
                bail!(
                    "persona_purge requires confirm: true (mirrors /persona remove <name> --purge --yes); refusing without confirmation"
                );
            }
            let message = crate::interactive_commands::execute_interactive_persona_command(
                app,
                crate::interactive_commands::InteractivePersonaCommand::Purge {
                    name: name.clone(),
                },
            )
            .await?;
            Ok(Some(
                json!({ "name": name, "purged": true, "message": message }),
            ))
        }
        RpcCommand::PersonaSelect { name, .. } => {
            let message = crate::interactive_commands::execute_interactive_persona_command(
                app,
                crate::interactive_commands::InteractivePersonaCommand::Select {
                    name: name.clone(),
                },
            )
            .await?;
            Ok(Some(
                json!({ "name": name, "preferred": true, "message": message }),
            ))
        }
        RpcCommand::PersonaClear { .. } => {
            let message = crate::interactive_commands::execute_interactive_persona_command(
                app,
                crate::interactive_commands::InteractivePersonaCommand::Clear,
            )
            .await?;
            Ok(Some(
                json!({ "preferred": serde_json::Value::Null, "message": message }),
            ))
        }
        RpcCommand::PersonaCurrent { .. } => {
            let message = crate::interactive_commands::execute_interactive_persona_command(
                app,
                crate::interactive_commands::InteractivePersonaCommand::Current,
            )
            .await?;
            let name = crate::interactive_commands::current_preferred_agent(app);
            Ok(Some(json!({ "name": name, "message": message })))
        }
        RpcCommand::SetTodos {
            workflow_id,
            phases,
            ..
        } => {
            let result = match workflow_id {
                Some(workflow_id) => {
                    app.set_workflow_todos(&pi_coding::WorkflowId::new(workflow_id), phases)?
                }
                None => app.set_todos(phases)?,
            };
            Ok(Some(
                json!({"phases":result.phases,"completedTasks":result.completed_tasks,"summary":result.summary}),
            ))
        }
        RpcCommand::TodoOp { op, .. } => {
            // Mirror the per-op todo vocabulary of the agent `todo` tool
            // (pi_coding::TodoOp) over RPC so web clients can mutate the DAG
            // atomically (append/done/start/drop/rm/dependencies) instead of
            // round-tripping the full phase list through set_todos. The
            // application publishes `todo_updated` for every successful op.
            let result = app.apply_todo(op)?;
            Ok(Some(
                json!({"phases":result.phases,"completedTasks":result.completed_tasks,"summary":result.summary}),
            ))
        }
        RpcCommand::LoopCreate { request, .. } => {
            Ok(Some(serde_json::to_value(app.loop_create(request).await?)?))
        }
        RpcCommand::LoopUpdate { request, .. } => {
            Ok(Some(serde_json::to_value(app.loop_update(request).await?)?))
        }
        RpcCommand::LoopList { .. } => Ok(Some(serde_json::to_value(app.loop_list().await?)?)),
        RpcCommand::LoopDelete { task_id, .. } => Ok(Some(serde_json::to_value(
            app.loop_delete(&task_id).await?,
        )?)),
        RpcCommand::LoopCancel { task_id, .. } => Ok(Some(serde_json::to_value(
            app.loop_cancel(&task_id).await?,
        )?)),
        RpcCommand::ProcessSpawn { spec, .. } => {
            Ok(Some(serde_json::to_value(app.process_spawn(spec).await?)?))
        }
        RpcCommand::ProcessList { .. } => Ok(Some(serde_json::to_value(app.process_list())?)),
        RpcCommand::ProcessDescribe { process_id, .. } => Ok(Some(serde_json::to_value(
            app.process_describe(&process_id)?,
        )?)),
        RpcCommand::ProcessLogs {
            process_id,
            cursor,
            limit_bytes,
            ..
        } => Ok(Some(serde_json::to_value(
            app.process_logs(&process_id, cursor.unwrap_or(0), limit_bytes, false, None)
                .await?,
        )?)),
        RpcCommand::ProcessWrite {
            process_id,
            data_base64,
            ..
        } => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data_base64)
                .context("decoding process dataBase64")?;
            app.process_write(&process_id, bytes, false).await?;
            Ok(Some(Value::Null))
        }
        RpcCommand::ProcessKeys {
            process_id, keys, ..
        } => {
            app.process_send_keys(&process_id, &keys).await?;
            Ok(Some(Value::Null))
        }
        RpcCommand::ProcessResize {
            process_id,
            cols,
            rows,
            ..
        } => {
            app.process_resize(&process_id, ProcessTerminalSize { cols, rows })?;
            Ok(Some(Value::Null))
        }
        RpcCommand::ProcessSignal {
            process_id, signal, ..
        } => {
            app.process_signal(&process_id, signal)?;
            Ok(Some(Value::Null))
        }
        RpcCommand::ProcessStop { process_id, .. } => Ok(Some(serde_json::to_value(
            app.process_stop(&process_id, None).await?,
        )?)),
        RpcCommand::ProcessWait {
            process_id,
            timeout_ms,
            ..
        } => Ok(Some(serde_json::to_value(
            app.process_wait(
                &process_id,
                timeout_ms.map(std::time::Duration::from_millis),
            )
            .await?,
        )?)),
        RpcCommand::WorkflowCreate {
            id,
            name,
            objective,
        } => Ok(Some(
            workflows
                .dispatch(crate::workflow_rpc::WorkflowRpcCommand::WorkflowCreate {
                    id,
                    name,
                    objective,
                })
                .await?,
        )),
        RpcCommand::WorkflowList { id } => Ok(Some(
            workflows
                .dispatch(crate::workflow_rpc::WorkflowRpcCommand::WorkflowList { id })
                .await?,
        )),
        RpcCommand::WorkflowGet {
            id,
            workflow_id,
            name,
        } => Ok(Some(
            workflows
                .dispatch(crate::workflow_rpc::WorkflowRpcCommand::WorkflowGet {
                    id,
                    workflow_id,
                    name,
                })
                .await?,
        )),
        RpcCommand::WorkflowDetail {
            id,
            workflow_id,
            name,
        } => Ok(Some(
            workflows
                .dispatch(crate::workflow_rpc::WorkflowRpcCommand::WorkflowDetail {
                    id,
                    workflow_id,
                    name,
                })
                .await?,
        )),
        RpcCommand::WorkflowPause { id, workflow_id } => Ok(Some(
            workflows
                .dispatch(crate::workflow_rpc::WorkflowRpcCommand::WorkflowPause {
                    id,
                    workflow_id,
                })
                .await?,
        )),
        RpcCommand::WorkflowResume { id, workflow_id } => Ok(Some(
            workflows
                .dispatch(crate::workflow_rpc::WorkflowRpcCommand::WorkflowResume {
                    id,
                    workflow_id,
                })
                .await?,
        )),
        RpcCommand::WorkflowCancel { id, workflow_id } => Ok(Some(
            workflows
                .dispatch(crate::workflow_rpc::WorkflowRpcCommand::WorkflowCancel {
                    id,
                    workflow_id,
                })
                .await?,
        )),
        RpcCommand::WorkflowIntegrate { id, workflow_id } => Ok(Some(
            workflows
                .dispatch(crate::workflow_rpc::WorkflowRpcCommand::WorkflowIntegrate {
                    id,
                    workflow_id,
                })
                .await?,
        )),
        RpcCommand::WorkflowRemove { id, workflow_id } => Ok(Some(
            workflows
                .dispatch(crate::workflow_rpc::WorkflowRpcCommand::WorkflowRemove {
                    id,
                    workflow_id,
                })
                .await?,
        )),
        RpcCommand::WorkflowHubSend {
            workflow_id,
            generation,
            recipient,
            body,
            reply_to,
            ..
        } => {
            let receipts =
                app.workflow_hub_send(&workflow_id, generation, &recipient, &body, reply_to)?;
            Ok(Some(workflow_hub_send_response_data(
                workflow_id,
                generation,
                recipient,
                receipts,
            )))
        }
        RpcCommand::SnapCompact { .. } => {
            Ok(Some(serde_json::to_value(app.compact_snap().await?)?))
        }
        RpcCommand::Rewind {
            index, checkpoint, ..
        } => {
            let target = match (index, checkpoint) {
                (Some(_), Some(_)) => {
                    bail!("rewind accepts either `index` or `checkpoint`, not both")
                }
                (Some(index), None) => pi_coding::RewindTarget::Index(index),
                (None, Some(checkpoint)) => pi_coding::RewindTarget::Checkpoint(checkpoint),
                (None, None) => bail!(
                    "rewind requires an `index` (entry index) or a `checkpoint` name; list rewind targets via get_entries"
                ),
            };
            Ok(Some(serde_json::to_value(app.rewind(target).await?)?))
        }
        RpcCommand::Handoff { prose, .. } => {
            let handoff = if prose {
                app.generate_handoff_with_prose().await?
            } else {
                app.generate_handoff()?
            };
            Ok(Some(json!({"text": handoff.render(), "prose": prose})))
        }
        RpcCommand::QueueList { .. } => {
            let (steering, follow_up) = app.queued_messages().await;
            let steering = queued_message_texts(steering);
            let follow_up = queued_message_texts(follow_up);
            Ok(Some(json!({
                "steering": steering,
                "followUp": follow_up,
                "total": steering.len() + follow_up.len(),
            })))
        }
        RpcCommand::QueueCancel { scope, index, .. } => match (index, scope) {
            (Some(index), _) => {
                let cancelled = if app.remove_steering_at(index).await.is_some() {
                    1
                } else {
                    0
                };
                Ok(Some(json!({"cancelled": cancelled, "index": index})))
            }
            (None, QueueCancelScope::Steering) => {
                let drained = app.drain_steering_messages().await;
                Ok(Some(json!({"cancelled": drained.len()})))
            }
            (None, QueueCancelScope::All) => {
                let (steering, follow_up) = app.drain_queued_messages().await;
                Ok(Some(json!({"cancelled": steering.len() + follow_up.len()})))
            }
        },
        RpcCommand::QueueSendNow { .. } => {
            let result = app.send_queued_steering_now().await?;
            Ok(Some(serde_json::to_value(result)?))
        }
        RpcCommand::JobList { .. } => {
            let Some(runtime) = app.orchestration_runtime() else {
                return Ok(Some(json!({
                    "enabled": false,
                    "groupId": null,
                    "jobs": [],
                    "agents": [],
                    "messages": [],
                    "catalog": [],
                })));
            };
            let jobs = runtime.jobs(None);
            let agents = runtime.list(runtime.main_agent_id());
            let messages = runtime.delivered_messages();
            let catalog = runtime
                .enabled_agents()
                .into_iter()
                .map(|agent| {
                    json!({
                        "name": agent.name,
                        "description": agent.description,
                    })
                })
                .collect::<Vec<_>>();
            Ok(Some(json!({
                "enabled": true,
                "groupId": runtime.group_id(),
                "jobs": jobs,
                "agents": agents,
                "messages": messages,
                "catalog": catalog,
            })))
        }
        RpcCommand::TaskSpawn { args, .. } => {
            let runtime = app
                .orchestration_runtime()
                .ok_or_else(|| anyhow!("orchestration is not enabled in this session"))?;
            let parameters: pi_coding::TaskParameters = serde_json::from_value(args)?;
            let items = parameters.into_items(&runtime)?;
            let parent = runtime.main_agent_id().to_owned();
            let spawns = runtime.spawn_tasks(&parent, 0, items)?;
            Ok(Some(json!({"spawns": spawns})))
        }
        RpcCommand::JobCancel { job_ids, .. } => {
            let runtime = app
                .orchestration_runtime()
                .ok_or_else(|| anyhow!("orchestration is not enabled in this session"))?;
            let cancelled = runtime.cancel_jobs_result(&job_ids)?;
            Ok(Some(json!({"cancelled": cancelled})))
        }
        RpcCommand::HubSend {
            to, body, reply_to, ..
        } => {
            let runtime = app
                .orchestration_runtime()
                .ok_or_else(|| anyhow!("orchestration is not enabled in this session"))?;
            let from = runtime.main_agent_id().to_owned();
            let receipts = runtime.send(&from, &to, &body, reply_to);
            Ok(Some(json!({"receipts": receipts})))
        }
        RpcCommand::JobOutput { job_id, .. } => {
            let runtime = app
                .orchestration_runtime()
                .ok_or_else(|| anyhow!("orchestration is not enabled in this session"))?;
            let job = runtime
                .jobs(Some(std::slice::from_ref(&job_id)))
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("no orchestration job with id {job_id:?}"))?;
            Ok(Some(json!({"job": job})))
        }
        RpcCommand::AgentHistory {
            agent_id, lines, ..
        } => {
            let runtime = app
                .orchestration_runtime()
                .ok_or_else(|| anyhow!("orchestration is not enabled in this session"))?;
            let lines = lines.unwrap_or(DEFAULT_RPC_HISTORY_LINES);
            if !(1..=pi_coding::MAX_HISTORY_LINES).contains(&lines) {
                bail!(
                    "lines must be between 1 and {}",
                    pi_coding::MAX_HISTORY_LINES
                );
            }
            let text = runtime.read_child_history(&agent_id, lines)?;
            Ok(Some(json!({"agentId": agent_id, "text": text})))
        }
        RpcCommand::TodoChildCreate { request, .. } => Ok(Some(serde_json::to_value(
            app.todo_child_create(request).await?,
        )?)),
        RpcCommand::TodoChildList {
            parent_session_id, ..
        } => Ok(Some(serde_json::to_value(
            app.todo_child_list(Some(&parent_session_id)).await?,
        )?)),
        RpcCommand::TodoChildSelect {
            parent_session_id,
            child_session_id,
            ..
        } => {
            app.todo_child_select(Some(&parent_session_id), &child_session_id)
                .await?;
            Ok(Some(json!({})))
        }
        RpcCommand::TodoChildSend {
            parent_session_id,
            child_session_id,
            message,
            ..
        } => Ok(Some(serde_json::to_value(
            app.todo_child_send(
                Some(&parent_session_id),
                child_session_id.as_deref(),
                message,
            )
            .await?,
        )?)),
        RpcCommand::TodoChildStatus {
            parent_session_id,
            child_session_id,
            ..
        } => Ok(Some(serde_json::to_value(
            app.todo_child_status(Some(&parent_session_id), child_session_id.as_deref())
                .await?,
        )?)),
        RpcCommand::TodoChildStop {
            parent_session_id,
            child_session_id,
            ..
        } => Ok(Some(serde_json::to_value(
            app.todo_child_stop(Some(&parent_session_id), child_session_id.as_deref())
                .await?,
        )?)),
        RpcCommand::TodoChildPause {
            parent_session_id,
            child_session_id,
            ..
        } => Ok(Some(serde_json::to_value(
            app.todo_child_pause(Some(&parent_session_id), child_session_id.as_deref())
                .await?,
        )?)),
        RpcCommand::TodoChildResume {
            parent_session_id,
            child_session_id,
            ..
        } => Ok(Some(serde_json::to_value(
            app.todo_child_resume(Some(&parent_session_id), child_session_id.as_deref())
                .await?,
        )?)),
        RpcCommand::TodoChildCancel {
            parent_session_id,
            child_session_id,
            ..
        } => Ok(Some(serde_json::to_value(
            app.todo_child_cancel(Some(&parent_session_id), child_session_id.as_deref())
                .await?,
        )?)),
        RpcCommand::TodoChildArchive {
            parent_session_id,
            child_session_id,
            ..
        } => {
            app.todo_child_archive(Some(&parent_session_id), &child_session_id)
                .await?;
            Ok(Some(json!({})))
        }
        RpcCommand::TodoChildRemove {
            parent_session_id,
            child_session_id,
            ..
        } => {
            app.todo_child_remove(Some(&parent_session_id), &child_session_id)
                .await?;
            Ok(Some(json!({})))
        }
        RpcCommand::TodoChildAddTask {
            parent_session_id,
            child_session_id,
            request,
            ..
        } => Ok(Some(serde_json::to_value(
            app.todo_child_add_task(Some(&parent_session_id), &child_session_id, request)
                .await?,
        )?)),
        RpcCommand::TodoChildDetail {
            parent_session_id,
            child_session_id,
            ..
        } => Ok(Some(serde_json::to_value(
            app.todo_child_detail(&parent_session_id, &child_session_id)
                .await?,
        )?)),
        RpcCommand::TodoChildHubSend {
            parent_session_id,
            child_session_id,
            to,
            body,
            reply_to,
            ..
        } => Ok(Some(serde_json::to_value(
            app.todo_child_hub_send(&parent_session_id, &child_session_id, &to, &body, reply_to)
                .await?,
        )?)),
        RpcCommand::ChatMappingGet { .. } => {
            Ok(Some(crate::chat_sync::handle_get(&pi_coding::agent_dir_path())?))
        }
        RpcCommand::ChatMappingPut { document, base_revision, .. } => {
            let wire = crate::chat_sync::ChatMappingPutWire {
                document: document.clone(),
                base_revision,
            };
            Ok(Some(crate::chat_sync::handle_put(&pi_coding::agent_dir_path(), &wire)?))
        }
        RpcCommand::TeamList { .. } => {
            let registry = crate::team::TeamRegistry::open(&pi_coding::agent_dir_path())?;
            Ok(Some(json!({ "teams": registry.list() })))
        }
        RpcCommand::TeamCreate {
            name,
            project_label,
            project_cwd,
            ..
        } => {
            let registry = crate::team::TeamRegistry::open(&pi_coding::agent_dir_path())?;
            let team = registry.create_team(
                &name,
                project_label.as_deref().unwrap_or(&name),
                project_cwd.as_deref().unwrap_or(""),
            )?;
            Ok(Some(json!({ "team": team })))
        }
        RpcCommand::TeamRemove { team_id, .. } => {
            let registry = crate::team::TeamRegistry::open(&pi_coding::agent_dir_path())?;
            registry.remove_team(&team_id)?;
            Ok(Some(json!({ "removed": true })))
        }
        RpcCommand::TeamJoin {
            team_id,
            origin,
            token,
            label,
            role,
            ..
        } => {
            let registry = crate::team::TeamRegistry::open(&pi_coding::agent_dir_path())?;
            let member = registry.join(
                &team_id,
                &origin,
                &token,
                label.as_deref().unwrap_or("runner"),
                role.as_deref(),
            )?;
            Ok(Some(json!({ "member": member })))
        }
        RpcCommand::TeamLeave {
            team_id, runner_id, ..
        } => {
            let registry = crate::team::TeamRegistry::open(&pi_coding::agent_dir_path())?;
            registry.leave(&team_id, &runner_id)?;
            Ok(Some(json!({ "left": true })))
        }
        RpcCommand::TeamStatus { team_id, .. } => {
            let registry = crate::team::TeamRegistry::open(&pi_coding::agent_dir_path())?;
            let team = registry.team(&team_id)?;
            let mut members = Vec::new();
            for (member, probe) in crate::team::team_status(&registry, &team).await {
                let row = match probe {
                    Ok(state) => json!({
                        "member": member,
                        "online": true,
                        "sessionId": state.session_id,
                        "cwd": state.cwd,
                        "sessionCount": state.session_count,
                    }),
                    Err(error) => json!({
                        "member": member,
                        "online": false,
                        "error": format!("{error:#}"),
                    }),
                };
                members.push(row);
            }
            Ok(Some(json!({ "members": members })))
        }
        RpcCommand::TeamRun {
            team_id,
            runner_id,
            command,
            ..
        } => {
            let registry = crate::team::TeamRegistry::open(&pi_coding::agent_dir_path())?;
            let (member, token) = registry.member_with_token(&team_id, &runner_id)?;
            let command_id = format!("team-{}", uuid::Uuid::now_v7());
            let data =
                crate::team::run_runner_command(&member.origin, &token, &command, &command_id)
                    .await?;
            Ok(Some(json!({ "data": data })))
        }
        RpcCommand::TeamBroadcast {
            team_id, command, ..
        } => {
            let registry = crate::team::TeamRegistry::open(&pi_coding::agent_dir_path())?;
            let team = registry.team(&team_id)?;
            let mut results = Vec::new();
            for member in &team.members {
                let token = registry.runner_token(&team_id, &member.runner_id)?;
                let command_id = format!("team-{}", uuid::Uuid::now_v7());
                let row = match crate::team::run_runner_command(
                    &member.origin,
                    &token,
                    &command,
                    &command_id,
                )
                .await
                {
                    Ok(data) => json!({ "runnerId": member.runner_id, "ok": true, "data": data }),
                    Err(error) => {
                        json!({ "runnerId": member.runner_id, "ok": false, "error": format!("{error:#}") })
                    }
                };
                results.push(row);
            }
            Ok(Some(json!({ "results": results })))
        }
        // Side-chat commands never reach the settings/workflows handler:
        // RpcDispatcher::dispatch_inner routes them to
        // handle_side_chat_command with the RPC-owned tab container.
        RpcCommand::SideChatNew { .. }
        | RpcCommand::SideChatSwitch { .. }
        | RpcCommand::SideChatClose { .. }
        | RpcCommand::SideChatPrompt { .. }
        | RpcCommand::SideChatList { .. }
        | RpcCommand::SideChatAbort { .. } => {
            unreachable!("side_chat_* commands must be routed by dispatch_inner")
        }
        // Code-review commands are RPC-session controller state and are
        // routed by dispatch_inner to handle_code_review_command; the
        // settings/workflows handler never sees them.
        RpcCommand::CodeReviewOpen { .. }
        | RpcCommand::CodeReviewCommits { .. }
        | RpcCommand::CodeReviewSnapshot { .. }
        | RpcCommand::CodeReviewRefresh { .. }
        | RpcCommand::CodeReviewComment { .. }
        | RpcCommand::CodeReviewSetModel { .. }
        | RpcCommand::CodeReviewAutoReview { .. }
        | RpcCommand::CodeReviewHighlight { .. }
        | RpcCommand::CodeReviewTldr { .. }
        | RpcCommand::CodeReviewStop { .. }
        | RpcCommand::CodeReviewAbort { .. }
        | RpcCommand::CodeReviewClose { .. }
        | RpcCommand::CodeReviewFileDiff { .. }
        | RpcCommand::CodeReviewFileContext { .. } => {
            unreachable!("code_review_* commands must be routed by dispatch_inner")
        }
        RpcCommand::FileExplorerList { .. }
        | RpcCommand::FileExplorerSearch { .. }
        | RpcCommand::FileExplorerRead { .. }
        | RpcCommand::FileExplorerChanges { .. }
        | RpcCommand::FileExplorerDiff { .. }
        | RpcCommand::FileExplorerBlame { .. }
        | RpcCommand::FileExplorerDelete { .. }
        | RpcCommand::FileExplorerMap { .. }
        | RpcCommand::PathComplete { .. } => {
            unreachable!("file_explorer commands must be routed by dispatch_inner")
        }
        RpcCommand::FileConversationOpen { .. }
        | RpcCommand::FileConversationPrompt { .. }
        | RpcCommand::FileConversationSnapshot { .. }
        | RpcCommand::FileConversationAbort { .. }
        | RpcCommand::FileConversationClose { .. } => {
            bail!("file_conversation commands must be routed by dispatch_inner")
        }
        RpcCommand::TmuxListSessions { .. }
        | RpcCommand::TmuxPaneSnapshot { .. }
        | RpcCommand::TmuxSendKeys { .. }
        | RpcCommand::TmuxSessionCreate { .. }
        | RpcCommand::TmuxSessionDelete { .. }
        | RpcCommand::TmuxTerminalAttach { .. }
        | RpcCommand::TmuxTerminalInput { .. }
        | RpcCommand::TmuxTerminalResize { .. }
        | RpcCommand::TmuxTerminalDetach { .. } => {
            unreachable!("tmux commands must be routed by dispatch_inner")
        }
        RpcCommand::FileExplorerStageDownload { .. } => {
            bail!("file downloads require the listen control plane")
        }
        // The listen transport intercepts collaboration lifecycle commands
        // and LAN discovery; stdio RPC has no room registry or mDNS view.
        RpcCommand::CollabStart { .. }
        | RpcCommand::CollabStatus { .. }
        | RpcCommand::CollabStop { .. } => {
            bail!("collaboration room commands require the listen control plane")
        }
        RpcCommand::GetDiscoveredRunners { .. } => {
            bail!("get_discovered_runners requires the listen control plane")
        }
        // Codex Live realtime proxy commands. The browser owns WebRTC and the
        // `oai-events` data channel; these only relay signaling/session HTTP
        // calls to CLIProxyAPI with the server-held API key (never exposed to
        // the frontend).
        RpcCommand::RealtimeCreateCall { sdp_offer, .. } => {
            let live = app.runtime_settings().live.clone();
            let executor = app.http_executor(pi_coding::HostHttpClass::RealtimeApi);
            Ok(Some(
                realtime_create_call_with_executor(&live, &sdp_offer, executor).await?,
            ))
        }
        RpcCommand::RealtimeStop { .. } => Ok(Some(json!({"stopped": true}))),
        // STT voice proxy (hold-to-talk): the browser sends ONLY the bounded
        // recording; the endpoint URL and bearer key stay in the server-held
        // live settings (never accepted from the client, never exposed to the
        // frontend).
        RpcCommand::SttTranscribe {
            audio_base64,
            mime_type,
            ..
        } => {
            let live = app.runtime_settings().live.clone();
            let executor = app.http_executor(pi_coding::HostHttpClass::LiveStt);
            Ok(Some(
                stt_transcribe_with_executor(&live, &audio_base64, &mime_type, executor).await?,
            ))
        }
        // The Web control plane's session runtime manager intercepts
        // close_session before any dispatcher sees it; stdio RPC has one
        // Application and nothing to close.
        RpcCommand::CloseSession { .. } => {
            bail!("close_session is only supported on the Web control plane")
        }
    }
}

// ---------------------------------------------------------------------------
// Codex Live realtime proxy (CLIProxyAPI)
//
// The web frontend drives WebRTC directly, but all HTTP signaling goes through
// these RPC commands so the CLIProxyAPI access key never leaves the backend.
// ---------------------------------------------------------------------------

/// Bounded timeout for the realtime proxy round trips; a hung CLIProxyAPI
/// must not wedge the RPC dispatcher indefinitely.
const REALTIME_PROXY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Maximum bytes accepted for the create-call SDP offer (browser → proxy)
/// and for the upstream SDP answer (proxy → browser): a real SDP is a few
/// KiB, so the cap keeps both directions' memory bounded against an
/// oversized peer.
const REALTIME_BODY_LIMIT: usize = 256 * 1024;

/// Character cap for redacted diagnostic text in transport/status/body
/// errors (applied by the shared `pi_coding::redact::redact_bounded` after
/// redaction); an untrusted upstream echo can never blow up the error.
const REALTIME_DIAGNOSTIC_LIMIT: usize = 300;

/// Builds `{base}/v1{path}` for a realtime endpoint, de-duplicating a
/// trailing `/v1` on the configured base (Hyper parity, mirroring
/// `pi_coding::live::transcriptions_url`). Only the canonical base PATH is
/// interpolated: anything from the first `?`/`#` onward is dropped, so a
/// query/fragment can never swallow the appended API path or leak a secret
/// into the request URL. (Validation rejects such bases upstream; stripping
/// here is defense in depth.)
fn realtime_endpoint(base: &str, path: &str) -> String {
    let base = base.split(['?', '#']).next().unwrap_or("");
    let base = base.trim().trim_end_matches('/');
    let base = base.strip_suffix("/v1").unwrap_or(base);
    format!("{base}/v1{path}")
}

/// Validates that `settings` can drive the realtime proxy commands, mirroring
/// `pi_coding::live::validate_live_settings`: the CLIProxyAPI base URL and
/// access key must be configured, and plaintext bearer credentials are
/// rejected unless `allowInsecure` is set. The base URL must be a clean
/// `scheme://authority[/path]`: userinfo (username/password) and
/// query/fragment are rejected because they can carry secrets and a query
/// would swallow the appended API path. Errors never echo the raw configured
/// value — a parse failure uses fixed text, and only the (bounded) scheme is
/// named when it is unsupported.
fn validate_realtime_proxy(settings: &pi_coding::LiveRuntimeSettings) -> Result<()> {
    if !settings.enabled {
        bail!(
            "Live voice is disabled — set `Settings.live.enabled = true` (or run `/settings set live.enabled true`)"
        );
    }
    if settings.realtime_base_url.trim().is_empty() {
        bail!(
            "Realtime voice is not configured — set `Settings.live.realtimeBaseUrl` to your CLIProxyAPI base URL (e.g. http://host:port)"
        );
    }
    if settings.realtime_api_key.trim().is_empty() {
        bail!(
            "Realtime voice is not configured — set `Settings.live.realtimeApiKey` (the CLIProxyAPI access key) in settings.json; it is secret and cannot be written through /settings"
        );
    }
    // Fixed text — the raw configured value (which may carry a secret in its
    // userinfo/query) is never echoed into the error.
    let parsed = reqwest::Url::parse(settings.realtime_base_url.trim())
        .context("`Settings.live.realtimeBaseUrl` is not a valid URL")?;
    // Userinfo and query/fragment are never accepted: they can carry secrets,
    // and a query would swallow the appended API path.
    if !parsed.username().is_empty() || parsed.password().is_some() {
        bail!("`Settings.live.realtimeBaseUrl` must not contain a username or password");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        bail!("`Settings.live.realtimeBaseUrl` must not contain a query or fragment");
    }
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if settings.allow_insecure => Ok(()),
        "http" => bail!(
            "Refusing to send realtime bearer credentials over plaintext: `Settings.live.realtimeBaseUrl` uses http:// but `Settings.live.allowInsecure` is false. Use https://, or set `Settings.live.allowInsecure = true` for a loopback/self-hosted CLIProxyAPI"
        ),
        other => bail!(
            "Unsupported `Settings.live.realtimeBaseUrl` scheme `{other}://` — use https:// (or http:// with `Settings.live.allowInsecure = true`)"
        ),
    }
}

/// Parses a realtime call id from the last path segment of a `Location`
/// header. Accepts a non-empty `rtc_`-prefixed suffix or a standard UUID;
/// rejects anything else. Errors NEVER echo the raw `Location` value: it may
/// carry a signed routing/upstream token in its query or fragment, and
/// non-UTF-8 bytes must not be Debug-printed either. Missing header,
/// non-UTF-8, and empty-path cases use fixed text; an invalid id error
/// carries only the derived path segment, length-bounded.
fn parse_realtime_call_id(location: Option<&reqwest::header::HeaderValue>) -> Result<String> {
    let header = location.context(
        "realtime_create_call: /v1/realtime/calls response has no `Location` header (call id)",
    )?;
    let raw = header
        .to_str()
        .context("realtime_create_call: `Location` header is not valid UTF-8 (call id)")?;
    // The call id is the last non-empty path segment of the path part only —
    // everything from the first `?`/`#` onward (query/fragment) is dropped
    // before deriving, and is never echoed.
    let path = raw.split(['?', '#']).next().unwrap_or("");
    let segment = path
        .rsplit('/')
        .find(|s| !s.is_empty())
        .context("realtime_create_call: `Location` has no path segment to derive a call id")?;
    let valid = if let Some(suffix) = segment.strip_prefix("rtc_") {
        !suffix.is_empty()
    } else {
        uuid::Uuid::parse_str(segment).is_ok()
    };
    if !valid {
        // Echo only the derived segment, truncated to a bounded length —
        // never the raw Location or its query/fragment.
        let bounded = segment.chars().take(120).collect::<String>();
        bail!(
            "realtime_create_call: `Location` call id `{bounded}` is neither a non-empty `rtc_` id nor a standard UUID"
        );
    }
    Ok(segment.to_owned())
}

/// Redacts credential-like material from a diagnostic string before it can
/// be echoed. The configured API key and the base/endpoint URLs are replaced
/// exactly first (so a reqwest error that embeds the request URL never
/// surfaces it), then the shared
/// [`pi_coding::redact::redact_bounded`] is applied — it covers the
/// established credential patterns (`Authorization: Bearer …`, case-
/// insensitive `bearer`, `token=`/`access_token=`, and the `name=value`
/// forms such as `API_KEY: …` or `Token : …`) and truncates the result to
/// `REALTIME_DIAGNOSTIC_LIMIT` characters on char boundaries.
fn redact_realtime_diagnostics(
    settings: &pi_coding::LiveRuntimeSettings,
    url: &str,
    raw: &str,
) -> String {
    let mut out = raw.to_owned();
    let key = settings.realtime_api_key.trim();
    if !key.is_empty() {
        out = out.replace(key, "[REDACTED]");
    }
    // Most specific first: the full endpoint URL, then the bare base.
    if !url.is_empty() {
        out = out.replace(url, "[endpoint]");
    }
    let base = settings.realtime_base_url.trim();
    if !base.is_empty() {
        out = out.replace(base, "[endpoint]");
    }
    pi_coding::redact::redact_bounded(&out, REALTIME_DIAGNOSTIC_LIMIT)
}

/// Reads a response body bounded to `REALTIME_BODY_LIMIT` bytes, rejecting an
/// oversized `Content-Length` before reading anything and bailing as soon as
/// the cap is exceeded mid-stream. Read errors use fixed context — the
/// upstream may echo credentials, so no raw body ever lands in them.


/// Proxies `POST {realtimeBaseUrl}/v1/realtime/calls` as a JSON POST carrying
/// the SDP offer and frameless session configuration. The create-call
/// session carries empty instructions, the configured output voice, and
/// client delegation. It omits `model` so the proxy can inject its
/// authoritative upstream model. Returns the bare SDP answer and call id as
/// `{"sdp": ..., "callId": ...}` for the browser's `RTCPeerConnection` and
/// `oai-events` data channel. Transport, status, and body errors use a fixed
/// endpoint name and redact credential-like content; the configured base/URL
/// and API key never surface, and both the SDP offer and the upstream answer
/// are bounded to `REALTIME_BODY_LIMIT`.
/// `audio.input` is intentionally absent: AVAS rejects it as an unknown
/// create-call session parameter.
async fn realtime_create_call_with_executor(
    settings: &pi_coding::LiveRuntimeSettings,
    sdp_offer: &str,
    executor: pi_ai::SharedHttpExecutor,
) -> Result<Value> {
    validate_realtime_proxy(settings)?;
    // The offer is bounded (and non-empty) so an oversized browser cannot
    // push unbounded memory into the request.
    if sdp_offer.trim().is_empty() {
        bail!("realtime_create_call: SDP offer must not be empty");
    }
    if sdp_offer.len() > REALTIME_BODY_LIMIT {
        bail!(
            "realtime_create_call: SDP offer exceeds the {} KiB bound",
            REALTIME_BODY_LIMIT / 1024
        );
    }
    let url = realtime_endpoint(&settings.realtime_base_url, "/realtime/calls");
    let body = serde_json::to_vec(&json!({
        "sdp": sdp_offer,
        "session": {
            "instructions": "",
            "audio": { "output": { "voice": settings.voice } },
            "delegation": { "type": "client" },
        },
    }))?;
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_str(&format!("Bearer {}", settings.realtime_api_key.trim()))?,
    );
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/sdp"),
    );
    headers.insert(
        http::HeaderName::from_static("openai-alpha"),
        http::HeaderValue::from_static("quicksilver=v2"),
    );
    let request = pi_ai::request_from_url(
        pi_ai::HttpMethod::Post,
        &url,
        &headers,
        body,
        &pi_ai::StreamOptions {
            timeout_ms: Some(REALTIME_PROXY_TIMEOUT.as_millis() as u64),
            ..Default::default()
        },
        REALTIME_BODY_LIMIT as u64,
    )?;
    let response = executor.execute(request).await.map_err(|_| {
        anyhow!("realtime_create_call: POST to the realtime create-call endpoint failed")
    })?;
    // Capture the Location header before consuming the body — the call id
    // lives there, not in the (bare SDP) response body.
    let location = response.headers().get(http::header::LOCATION).cloned();
    let status = response.status();
    let response_body = pi_ai::collect_response_body(response)
        .await
        .with_context(|| {
            format!(
                "realtime_create_call: upstream response exceeds the {} KiB bound",
                REALTIME_BODY_LIMIT / 1024
            )
        })?;
    if !status.is_success() {
        let detail =
            redact_realtime_diagnostics(settings, &url, &String::from_utf8_lossy(&response_body));
        bail!(
            "realtime_create_call: the realtime create-call endpoint returned {status}: {detail}"
        );
    }
    let sdp = String::from_utf8(response_body)
        .context("realtime_create_call: upstream SDP answer is not valid UTF-8")?;
    if sdp.trim().is_empty() {
        bail!("realtime_create_call: /v1/realtime/calls returned an empty SDP answer body");
    }
    let call_id = parse_realtime_call_id(location.as_ref())?;
    Ok(json!({ "sdp": sdp, "callId": call_id }))
}

// ---------------------------------------------------------------------------
// STT proxy (hold-to-talk voice)
//
// The web frontend records via MediaRecorder, converts the capture to WAV
// (blobToWav), and POSTs ONLY the bounded base64 audio + MIME type over the
// RPC; the endpoint URL and bearer key are read from the server-held live
// settings (`live.sttBaseUrl`/`live.sttApiKey`), so no credential or endpoint
// ever crosses the browser wire.
// ---------------------------------------------------------------------------

/// Decoded-size cap for browser-recorded STT audio: a 44-byte RIFF/WAVE
/// header plus [`pi_coding::live::MAX_RECORDING_SECONDS`] of
/// [`pi_coding::live::SAMPLE_RATE`] mono 16-bit PCM. Bounds the base64 wire
/// payload well under the transport frame limit and the forwarded STT
/// request.
const STT_MAX_AUDIO_BYTES: usize = 44
    + pi_coding::live::SAMPLE_RATE as usize * 2 * pi_coding::live::MAX_RECORDING_SECONDS as usize;

/// MIME allowlist for the `stt_transcribe` RPC. The browser converts the
/// MediaRecorder container to WAV before sending (blobToWav), so raw
/// webm/mp4 captures never cross the wire.
const STT_ALLOWED_MIME_TYPES: [&str; 1] = ["audio/wav"];

/// Validates the browser's STT payload (MIME allowlist, bounded base64,
/// strict RIFF/WAVE PCM16 parse) and proxies the transcription through
/// [`pi_coding::live::SttClient`] with the server-held `live.*` settings.
/// Returns `{ "text": ... }`. Errors are bounded and never echo the bearer
/// key: the size/MIME/WAV rejections carry no payload contents, and the STT
/// client redacts server-echoed secrets. The mode gate (TUI-only vs Web
/// realtime) is a UI concern; this RPC is an explicit STT request, so it
/// validates as `stt` and lets `SttClient` enforce the security-relevant
/// checks (enabled, base URL, key, scheme).
async fn stt_transcribe_with_executor(
    settings: &pi_coding::LiveRuntimeSettings,
    audio_base64: &str,
    mime_type: &str,
    executor: pi_ai::SharedHttpExecutor,
) -> Result<Value> {
    let mime = mime_type.trim();
    if !STT_ALLOWED_MIME_TYPES
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(mime))
    {
        // Echo at most a bounded prefix so a hostile client cannot project an
        // arbitrarily long string into the error surface.
        let shown = mime_type.chars().take(120).collect::<String>();
        bail!(
            "stt_transcribe: unsupported audio MIME type `{shown}` — only audio/wav (the browser converts the recording first) is accepted"
        );
    }
    // Reject a payload that cannot possibly fit before allocating: base64 of
    // a decoded size at the cap is `ceil(cap / 3) * 4` characters.
    let max_base64_len = STT_MAX_AUDIO_BYTES.div_ceil(3) * 4;
    if audio_base64.len() > max_base64_len {
        bail!(
            "stt_transcribe: audio payload of {} base64 chars exceeds the {STT_MAX_AUDIO_BYTES}-byte WAV cap",
            audio_base64.len()
        );
    }
    let wav = base64::engine::general_purpose::STANDARD
        .decode(audio_base64)
        .context("stt_transcribe: audioBase64 is not valid base64")?;
    if wav.len() > STT_MAX_AUDIO_BYTES {
        bail!(
            "stt_transcribe: decoded audio of {} bytes exceeds the {STT_MAX_AUDIO_BYTES}-byte WAV cap",
            wav.len()
        );
    }
    let capture = pi_coding::live::parse_wav_capture(&wav)?;
    let mut validated = settings.clone();
    validated.mode = "stt".to_owned();
    let stt = pi_coding::live::SttClient::new_with_executor(executor);
    let text = stt.transcribe(&validated, &capture).await?;
    Ok(json!({ "text": text }))
}

pub(crate) fn user_message_text(message: &Message) -> Option<String> {
    let Message::User(user) = message else {
        return None;
    };
    let text: String = user
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    (!text.trim().is_empty()).then_some(text)
}

pub(crate) fn fork_messages_from_tree(
    tree: &pi_coding::SessionTree,
    cursor: Option<&str>,
    limit: Option<usize>,
) -> Result<pi_coding::ForkMessagePage> {
    tree.fork_messages_page(cursor, limit)
}

pub(crate) fn last_nonempty_user_entry_id(tree: &pi_coding::SessionTree) -> Result<String> {
    tree.branch(tree.active_leaf_id.as_deref())
        .into_iter()
        .rev()
        .find_map(|entry| user_message_text(entry.message.as_ref()?).map(|_| entry.id.clone()))
        .ok_or_else(|| anyhow!("session has no user message to fork from"))
}

fn stdio_prepare_saved_session(
    app: &Application,
    session_path: &str,
) -> Result<pi_coding::PreparedSessionResume> {
    let catalog = pi_coding::SessionCatalog::from_env()?
        .with_native_session_root(app.session().session_dir());
    catalog
        .prepare_native_resume(session_path)
        .map_err(anyhow::Error::new)
}

fn stdio_is_current_session(
    app: &Application,
    prepared: &pi_coding::PreparedSessionResume,
) -> bool {
    if app.session().session_file_identity() == Some(prepared.identity()) {
        return true;
    }
    matches!(
        app.session().recorder_info(),
        Some((id, _)) if id == prepared.tree().header.id
    )
}

fn stdio_target_session_dir(
    app: &Application,
    session_storage: Option<&crate::session_run::SessionStorage>,
    recorded_cwd: &Path,
) -> Result<PathBuf> {
    if let Some(storage) = session_storage {
        let session = app.session();
        let cwd = if recorded_cwd.as_os_str().is_empty() {
            session.cwd()
        } else {
            recorded_cwd
        };
        storage.session_dir_for(cwd)
    } else {
        Ok(app.session().session_dir())
    }
}

fn stdio_branch_saved_session(
    app: &Application,
    session_storage: Option<&crate::session_run::SessionStorage>,
    session_path: &str,
    entry_id: Option<&str>,
) -> Result<Value> {
    let prepared = stdio_prepare_saved_session(app, session_path)?;
    if stdio_is_current_session(app, &prepared) {
        bail!(
            "cannot fork or clone the current session by path; omit sessionPath or switch away first"
        );
    }
    let leaf_id = match entry_id {
        Some("__latest__") => last_nonempty_user_entry_id(prepared.tree())?,
        Some(entry_id) => {
            let selected = prepared
                .tree()
                .entries
                .iter()
                .find(|entry| entry.id == entry_id)
                .ok_or_else(|| anyhow!("Invalid entry ID for forking"))?;
            if selected
                .message
                .as_ref()
                .and_then(user_message_text)
                .is_none()
            {
                bail!("Invalid entry ID for forking");
            }
            entry_id.to_owned()
        }
        None => prepared
            .tree()
            .active_leaf_id
            .clone()
            .ok_or_else(|| anyhow!("cannot clone session: no current entry selected"))?,
    };
    let session_dir = stdio_target_session_dir(app, session_storage, prepared.target_cwd())?;
    let source_path = prepared.path().to_path_buf();
    let recorder = if entry_id.is_some() {
        pi_coding::fork_prepared_session_at_user(prepared, &leaf_id, Some(&session_dir))?
    } else {
        pi_coding::create_branched_session_from_prepared(prepared, &leaf_id, Some(&session_dir))?
    };
    recorder.persist_now()?;
    Ok(json!({
        "sessionId": recorder.id(),
        "path": recorder.path().to_string_lossy(),
        "parentSession": source_path.to_string_lossy(),
        "cancelled": false,
    }))
}

fn stdio_rename_saved_session(app: &Application, session_path: &str, name: &str) -> Result<()> {
    let prepared = stdio_prepare_saved_session(app, session_path)?;
    if stdio_is_current_session(app, &prepared) {
        app.set_session_name(name)?;
        return Ok(());
    }
    let path = prepared.path().to_path_buf();
    drop(prepared);
    let recorder = pi_coding::resume_session(&path)?;
    recorder.record_session_name(name)?;
    recorder.close()?;
    Ok(())
}

fn stdio_delete_saved_session(app: &Application, session_path: &str) -> Result<Value> {
    let prepared = stdio_prepare_saved_session(app, session_path)?;
    if stdio_is_current_session(app, &prepared) {
        bail!("cannot delete the current session; switch away/close first");
    }
    let path = prepared.path().to_path_buf();
    drop(prepared);
    let outcome = pi_coding::archive_saved_session(&path)?;
    Ok(json!({
        "deleted": true,
        "sessionId": outcome.session_id(),
        "path": outcome.original_path().to_string_lossy(),
    }))
}

pub(crate) fn public_message(message: Message) -> Message {
    let Message::Custom(custom) = message else {
        return message;
    };
    let Some(loop_message) = pi_coding::loop_message_view(&custom) else {
        return Message::Custom(custom);
    };
    Message::Custom(pi_ai::CustomMessage {
        custom_type: format!("Loop {} · {}", loop_message.task_id, loop_message.schedule),
        content: loop_message.prompt.into(),
        display: true,
        details: custom.details.clone(),
        timestamp: custom.timestamp,
    })
}

pub(crate) fn public_transcript_rows(rows: Vec<TranscriptRow>) -> Vec<Value> {
    rows.into_iter()
        .map(|row| {
            let message = public_message(row.message);
            match row.id {
                Some(id) => json!({ "id": id, "message": message }),
                None => json!({ "message": message }),
            }
        })
        .collect()
}

fn public_model(mut model: Model) -> Model {
    model.headers = None;
    model
}

/// Text of queued user prompts for the `/queue` view. Steering and follow-up
/// messages are always user messages; other message kinds are skipped,
/// mirroring the TUI's `message_text` projection.
fn queued_message_texts(messages: Vec<Message>) -> Vec<String> {
    messages
        .into_iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(
                user.content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect()
}
pub(crate) async fn available_models() -> Result<Vec<Model>> {
    crate::models_config::load_custom_models()?;
    let mut providers = pi_ai::get_providers();
    providers.sort();
    let mut models = Vec::new();
    for provider in providers {
        let mut available = pi_ai::get_models(&provider);
        available.sort_by(|a, b| a.id.cmp(&b.id));
        models.extend(
            available
                .into_iter()
                .filter(crate::models_config::is_public_model),
        )
    }
    models.dedup_by(|a, b| a.provider == b.provider && a.id == b.id);
    models = crate::models_config::filter_models_for_resolved_auth_async(models, None).await;
    Ok(models.into_iter().map(public_model).collect())
}
async fn set_model(app: &Application, provider: &str, model_id: &str) -> Result<Model> {
    crate::models_config::load_custom_models()?;
    let model = pi_ai::get_model(provider, model_id)
        .ok_or_else(|| anyhow!("Model not found: {provider}/{model_id}"))?;
    crate::models_config::resolve_available_model_request_auth_async(&model, None, None).await?;
    app.set_model_with_resolved_auth(model.clone()).await?;
    Ok(public_model(model))
}
async fn cycle_model(app: &Application) -> Result<Option<Value>> {
    let models = available_models().await?;
    if models.len() <= 1 {
        return Ok(Some(Value::Null));
    }
    let state = app.state().await;
    let current = state
        .model
        .as_ref()
        .and_then(|a| {
            models
                .iter()
                .position(|m| m.provider == a.provider && m.id == a.id)
        })
        .unwrap_or(0);
    let next = &models[(current + 1) % models.len()];
    let model = set_model(app, &next.provider, &next.id).await?;
    Ok(Some(
        json!({"model":model,"thinkingLevel":app.state().await.thinking_level,"isScoped":false}),
    ))
}
/// Wire name for the source backing an executable command.
fn command_source_str(source: crate::interactive_commands::CommandSource) -> &'static str {
    match source {
        crate::interactive_commands::CommandSource::Builtin => "builtin",
        crate::interactive_commands::CommandSource::Prompt => "prompt",
        crate::interactive_commands::CommandSource::Skill => "skill",
        crate::interactive_commands::CommandSource::Extension => "extension",
    }
}

fn available_thinking_levels(model: &Model) -> Vec<ThinkingLevel> {
    pi_ai::supported_thinking_levels(model)
        .into_iter()
        .filter_map(|l| match l {
            "off" => Some(ThinkingLevel::Off),
            "minimal" => Some(ThinkingLevel::Minimal),
            "low" => Some(ThinkingLevel::Low),
            "medium" => Some(ThinkingLevel::Medium),
            "high" => Some(ThinkingLevel::High),
            "xhigh" => Some(ThinkingLevel::Xhigh),
            "max" => Some(ThinkingLevel::Max),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prompt_busy_error_attaches_session_processing_code() {
        let mut response = RpcResponse::failure(None, "prompt", SessionAlreadyProcessing.to_string());
        attach_handle_command_error_code(&anyhow!(SessionAlreadyProcessing), &mut response);
        assert_eq!(response.data, Some(json!({"code": "session_processing"})));
        assert_eq!(
            response.error.as_deref(),
            Some("session is already processing; choose steer or followUp")
        );
        let mut other = RpcResponse::failure(None, "prompt", "other");
        attach_handle_command_error_code(&anyhow!("busy with no typed code"), &mut other);
        assert_eq!(other.data, None);
    }

    #[test]
    fn side_chat_entry_wire_carries_tool_and_thinking_fields() {
        use crate::side_chat::{SideChatEntry, SideChatEntryKind, SideChatRole};
        let tool = SideChatEntry {
            id: "sc-1".into(),
            kind: SideChatEntryKind::Tool,
            role: SideChatRole::Tool,
            text: "[bash] ok".into(),
            tool_name: Some("bash".into()),
            tool_args: Some(json!({"command": "ls"})),
            output: Some("ok".into()),
            thinking: None,
            is_error: false,
            is_partial: false,
        };
        let tool_wire = tool.to_wire_json();
        assert_eq!(tool_wire["id"], "sc-1");
        assert_eq!(tool_wire["kind"], "tool");
        assert_eq!(tool_wire["toolName"], "bash");
        assert_eq!(tool_wire["toolArgs"]["command"], "ls");
        assert_eq!(tool_wire["output"], "ok");
        assert_eq!(tool_wire["isError"], false);
        assert_eq!(tool_wire["isPartial"], false);

        let thinking = SideChatEntry {
            id: "sc-2".into(),
            kind: SideChatEntryKind::Thinking,
            role: SideChatRole::Assistant,
            text: "plan".into(),
            tool_name: None,
            tool_args: None,
            output: None,
            thinking: Some("plan".into()),
            is_error: false,
            is_partial: true,
        };
        let thinking_wire = thinking.to_wire_json();
        assert_eq!(thinking_wire["id"], "sc-2");
        assert_eq!(thinking_wire["kind"], "thinking");
        assert_eq!(thinking_wire["thinking"], "plan");
        assert_eq!(thinking_wire["isPartial"], true);

        let live = SideChatEntry {
            id: "sc-3".into(),
            kind: SideChatEntryKind::Assistant,
            role: SideChatRole::Assistant,
            text: "Hel".into(),
            tool_name: None,
            tool_args: None,
            output: None,
            thinking: None,
            is_error: false,
            is_partial: true,
        };
        let live_wire = live.to_wire_json();
        assert_eq!(live_wire["id"], "sc-3");
        assert_eq!(live_wire["kind"], "assistant");
        assert_eq!(live_wire["text"], "Hel");
        assert_eq!(live_wire["isPartial"], true);
    }

    #[test]
    fn review_language_resolution_and_validation() {
        assert_eq!(resolve_review_language(Some(" zh-CN "), Some("en")).unwrap(), "zh-CN".to_owned());
        assert_eq!(resolve_review_language(None, Some("en")).unwrap(), "en".to_owned());
        assert_eq!(resolve_review_language(None, None).unwrap(), "zh".to_owned());
        for invalid in ["", "  ", "en\nUS", "中文", "abcdefghijklmnopqrstuvwxyz1234567"] {
            let error = resolve_review_language(Some(invalid), Some("en")).unwrap_err();
            assert_eq!(error.downcast_ref::<CodeReviewRpcError>(), Some(&CodeReviewRpcError::InvalidLanguage));
        }
    }

    #[test]
    fn code_review_stop_parses_typed_operation() {
        fn parse_command(value: Value) -> RpcCommand {
            match parse_input(&serde_json::to_vec(&value).unwrap()).expect("command") {
                RpcInput::Command { command, .. } => command,
                RpcInput::ExtensionUiResponse(_) => panic!("expected command"),
            }
        }
        assert!(matches!(
            parse_command(json!({"type":"code_review_stop","operation":"autoReview"})),
            RpcCommand::CodeReviewStop { operation: ReviewStopOperation::AutoReview, .. }
        ));
        assert!(matches!(
            parse_command(json!({"type":"code_review_stop","operation":"highlight"})),
            RpcCommand::CodeReviewStop { operation: ReviewStopOperation::Highlight, .. }
        ));
        assert!(matches!(
            parse_command(json!({"type":"code_review_stop","operation":"tldr"})),
            RpcCommand::CodeReviewStop { operation: ReviewStopOperation::Tldr, .. }
        ));
        assert!(parse_input(&serde_json::to_vec(&json!({"type":"code_review_stop","operation":"abort"})).unwrap()).is_err());
    }

    /// Test-only wrapper that routes the realtime create-call through the
    /// same injected-executor production path used by the RPC dispatcher.
    async fn realtime_create_call(
        settings: &pi_coding::LiveRuntimeSettings,
        sdp_offer: &str,
    ) -> Result<Value> {
        realtime_create_call_with_executor(settings, sdp_offer, test_http_executor()).await
    }

    /// Test-only wrapper that routes STT transcription through the same
    /// injected-executor production path used by the RPC dispatcher.
    async fn stt_transcribe(
        settings: &pi_coding::LiveRuntimeSettings,
        audio_base64: &str,
        mime_type: &str,
    ) -> Result<Value> {
        stt_transcribe_with_executor(settings, audio_base64, mime_type, test_http_executor()).await
    }

    #[derive(Clone, Copy)]
    struct TestHttpExecutor;

    impl pi_ai::HttpExecutor for TestHttpExecutor {
        fn execute<'a>(
            &'a self,
            request: pi_ai::HttpRequest,
        ) -> futures_util::future::BoxFuture<'a, anyhow::Result<pi_ai::HttpResponse>> {
            async move {
                let parts = request.into_parts();
                let scheme = match parts.authority.scheme() {
                    pi_ai::ResolvedHttpScheme::Http => "http",
                    pi_ai::ResolvedHttpScheme::Https => "https",
                };
                let host = match parts.authority.host() {
                    pi_ai::ResolvedHttpHost::Dns(name) => name.clone(),
                    pi_ai::ResolvedHttpHost::Ip(address) => address.to_string(),
                };
                let url = format!(
                    "{scheme}://{host}:{}{}",
                    parts.authority.port(),
                    parts.path_and_query
                );
                let method = match parts.method {
                    pi_ai::HttpMethod::Get => reqwest::Method::GET,
                    pi_ai::HttpMethod::Post => reqwest::Method::POST,
                    pi_ai::HttpMethod::Put => reqwest::Method::PUT,
                    pi_ai::HttpMethod::Patch => reqwest::Method::PATCH,
                    pi_ai::HttpMethod::Delete => reqwest::Method::DELETE,
                };
                let client = reqwest::Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()?;
                let headers = parts.headers.into_iter().collect::<http::HeaderMap>();
                let response = client
                    .request(method, url)
                    .headers(headers)
                    .body(parts.body)
                    .send()
                    .await?;
                let status = response.status();
                let headers = response.headers().clone();
                let body = response
                    .bytes_stream()
                    .map(|chunk| chunk.map(|bytes| bytes.to_vec()).map_err(Into::into));
                Ok(pi_ai::HttpResponse::new(status, headers, Box::pin(body)))
            }
            .boxed()
        }
    }

    fn test_http_executor() -> pi_ai::SharedHttpExecutor {
        std::sync::Arc::new(TestHttpExecutor)
    }

    fn disabled_session(options: pi_coding::SessionOptions) -> Result<pi_coding::Session> {
        pi_coding::Session::new_with_additional_tools_filtered_and_discovery(
            options,
            Vec::new(),
            pi_coding::ToolSelection::default(),
            pi_coding::ResourceDiscovery::Disabled,
        )
    }

    fn faux_model() -> Model {
        pi_ai::providers::register_default_faux();
        pi_ai::get_model("faux", "faux-1").expect("registered faux model")
    }

    fn isolated_resources(cwd: &Path, agent_dir: &Path) -> pi_coding::ResourceManager {
        std::fs::create_dir_all(agent_dir).expect("agent dir");
        if !agent_dir.join("settings.json").exists() {
            std::fs::write(
                agent_dir.join("settings.json"),
                r#"{"sandbox":{"mode":"danger-full-access"}}"#,
            )
            .expect("isolated sandbox settings");
        }
        let mut options = pi_coding::ResourceManagerOptions::new(cwd);
        options.agent_dir = agent_dir.to_path_buf();
        options.project_trust_override = Some(true);
        options.disable_extensions = true;
        options.disable_skills = true;
        options.disable_prompt_templates = true;
        options.disable_themes = true;
        options.disable_context_files = true;
        pi_coding::ResourceManager::new(options).expect("resources")
    }

    async fn isolated_application(session: pi_coding::Session) -> Application {
        let agent = tempfile::tempdir().expect("agent");
        if let Some(compaction) = session.compaction_settings() {
            std::fs::write(
                agent.path().join("settings.json"),
                format!(
                    r#"{{"sandbox":{{"mode":"danger-full-access"}},"compaction":{{"enabled":{},"reserveTokens":{},"keepRecentTokens":{},"snapKeepTurns":{}}}}}"#,
                    compaction.enabled,
                    compaction.reserve_tokens,
                    compaction.keep_recent_tokens,
                    compaction.snap_keep_turns,
                ),
            )
            .expect("isolated compaction settings");
        }
        if session.recorder_info().is_none() {
            let session_dir = session.cwd().join(".pi-test-sessions");
            std::fs::create_dir_all(&session_dir).expect("session dir");
            session.set_session_dir(session_dir);
        }
        let resources = isolated_resources(session.cwd(), agent.path());
        session
            .attach_resources(resources)
            .await
            .expect("attach resources");
        Application::new(session).await
    }
    use futures_util::{FutureExt as _, StreamExt as _};
    fn settings_state() -> crate::settings_rpc::SettingsRpcState {
        crate::settings_rpc::SettingsRpcState::default()
    }

    fn bash_execution_event_fixture() -> ApplicationEvent {
        ApplicationEvent::Session(pi_coding::SessionEvent::BashExecutionEnd {
            message: pi_ai::BashExecutionMessage {
                command: "echo ok".to_owned(),
                output: "ok".to_owned(),
                exit_code: Some(0),
                cancelled: false,
                truncated: false,
                full_output_path: None,
                timestamp: 1,
                exclude_from_context: None,
            },
        })
    }

    #[test]
    fn bash_execution_end_projects_web_event_shape() {
        let projected = project_application_event(bash_execution_event_fixture()).unwrap();
        assert_eq!(projected["type"], "bash_execution_end");
        assert_eq!(projected["message"]["command"], "echo ok");
        assert_eq!(projected["message"]["output"], "ok");
        assert_eq!(projected["message"]["exitCode"], 0);
    }

    #[test]
    fn queue_update_projects_web_event_shape() {
        let projected = project_application_event(ApplicationEvent::Session(
            pi_coding::SessionEvent::QueueUpdate {
                steering: vec![pi_ai::Message::user_text("steer now", 1)],
                follow_up: Vec::new(),
            },
        ))
        .unwrap();
        assert_eq!(projected["type"], "queue_update");
        assert_eq!(projected["steering"][0]["role"], "user");
        assert_eq!(projected["steering"][0]["content"][0]["text"], "steer now");
        assert_eq!(projected["followUp"].as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn process_exited_projects_web_event_shape() {
        let process = serde_json::from_value(json!({
            "id": "proc-exit",
            "ownerId": "owner",
            "label": "sleep",
            "state": "exited",
            "tty": false,
            "startedAtMs": 1,
            "exitedAtMs": 2,
            "exitCode": 0,
            "outputStartCursor": 0,
            "outputCursor": 0,
        }))
        .expect("process info");
        let projected = project_application_event(ApplicationEvent::Process(
            pi_coding::ProcessEvent::ProcessExited { process },
        ))
        .expect("project process exited");
        assert_eq!(projected["type"], "process_exited");
        assert_eq!(projected["process"]["id"], "proc-exit");
        assert_eq!(projected["process"]["state"], "exited");
    }

    #[test]
    fn outer_turn_lifecycle_projects_authoritative_events() {
        let started = project_application_event(ApplicationEvent::TurnStarted {
            turn_id: "turn-1".to_owned(),
            model: Some(Model {
                id: "initial".to_owned(),
                provider: "fixture".to_owned(),
                ..Model::default()
            }),
        })
        .expect("project started");
        assert_eq!(started["type"], "turn_started");
        assert_eq!(started["turnId"], "turn-1");
        assert_eq!(started["model"]["id"], "initial");

        let terminal = project_application_event(ApplicationEvent::TurnTerminal {
            turn_id: "turn-1".to_owned(),
            status: pi_coding::TurnTerminalStatus::Succeeded,
            message: None,
        })
        .expect("project terminal");
        assert_eq!(terminal["type"], "turn_terminal");
        assert_eq!(terminal["turnId"], "turn-1");
        assert_eq!(terminal["status"], "succeeded");
    }

    fn workflows_state() -> crate::workflow_rpc::WorkflowRpcState {
        let state = crate::workflow_rpc::WorkflowRpcState::new();
        state.set_memory_host();
        state
    }

    #[test]
    fn workflow_application_events_use_public_rpc_projection() {
        let workflow_id = pi_coding::WorkflowId::new("workflow-wire");
        let event = pi_coding::WorkflowEvent::StatusChanged {
            workflow_id: workflow_id.clone(),
            generation: 7,
            status: pi_coding::WorkflowStatus::Running,
        };
        let projected = crate::workflow_rpc::project_workflow_event(&event);
        assert!(matches!(
            &projected,
            crate::workflow_rpc::WorkflowWireEvent::WorkflowStatusChanged {
                workflow_id: projected_id,
                generation: 7,
                status: pi_coding::WorkflowStatus::Running,
                name: None,
            } if projected_id == &workflow_id
        ));
        let wire = serde_json::to_value(projected).expect("serialize projected event");
        assert_eq!(wire["type"], "workflow_status_changed");
        assert!(wire.get("snapshot").is_none());
    }

    #[test]
    fn all_command_fixtures_deserialize() {
        let fixtures = [
            json!({"type":"prompt","message":"x","streamingBehavior":"followUp"}),
            json!({"type":"steer","message":"x"}),
            json!({"type":"follow_up","message":"x"}),
            json!({"type":"abort"}),
            json!({"type":"new_session","parentSession":"p","cwd":"<workspace>/project-a"}),
            json!({"type":"get_state"}),
            json!({"type":"set_model","provider":"p","modelId":"m"}),
            json!({"type":"cycle_model"}),
            json!({"type":"get_available_models"}),
            json!({"type":"set_thinking_level","level":"high"}),
            json!({"type":"cycle_thinking_level"}),
            json!({"type":"get_available_thinking_levels"}),
            json!({"type":"set_steering_mode","mode":"all"}),
            json!({"type":"set_follow_up_mode","mode":"one-at-a-time"}),
            json!({"type":"compact","customInstructions":"c"}),
            json!({"type":"set_auto_compaction","enabled":true}),
            json!({"type":"set_auto_retry","enabled":true}),
            json!({"type":"abort_retry"}),
            json!({"type":"bash","command":"pwd","excludeFromContext":true}),
            json!({"type":"abort_bash"}),
            json!({"type":"get_session_stats"}),
            json!({"type":"session_list"}),
            json!({"type":"export_html","outputPath":"o"}),
            json!({"type":"export_html","sessionPath":"s"}),
            json!({"type":"switch_session","sessionPath":"s"}),
            json!({"type":"fork","entryId":"e"}),
            json!({"type":"clone"}),
            json!({"type":"get_fork_messages"}),
            json!({"type":"get_entries","since":"e"}),
            json!({"type":"get_tree"}),
            json!({"type":"get_last_assistant_text"}),
            json!({"type":"set_session_name","name":"n"}),
            json!({"type":"get_messages"}),
            json!({"type":"set_todos","workflowId":"wf-1","phases":[]}),
            json!({"type":"todo_op","op":"append","phase":"Plan","items":["ship it"]}),
            json!({"type":"todo_op","op":"done","task":"task-x"}),
            json!({"type":"todo_op","op":"update_dependencies","task":"task-x","dependsOn":["task-y"]}),
            json!({"type":"loop_create","interval":"5m","prompt":"check","fireImmediately":true,"durable":false}),
            json!({"type":"loop_update","taskId":"loop-1","interval":"10m","prompt":"check again"}),
            json!({"type":"loop_list"}),
            json!({"type":"loop_delete","taskId":"loop-1"}),
            json!({"type":"loop_cancel","taskId":"loop-1"}),
            json!({"type":"process_spawn","spec":{"argv":["printf","ok"],"cwd":".","env":{},"tty":false}}),
            json!({"type":"process_list"}),
            json!({"type":"process_describe","processId":"00000000-0000-7000-8000-000000000000"}),
            json!({"type":"process_logs","processId":"00000000-0000-7000-8000-000000000000","cursor":0,"limitBytes":1024}),
            json!({"type":"process_write","processId":"00000000-0000-7000-8000-000000000000","dataBase64":"b2s="}),
            json!({"type":"process_keys","processId":"00000000-0000-7000-8000-000000000000","keys":["ENTER","CTRL_C"]}),
            json!({"type":"process_resize","processId":"00000000-0000-7000-8000-000000000000","cols":80,"rows":24}),
            json!({"type":"process_signal","processId":"00000000-0000-7000-8000-000000000000","signal":"SIGTERM"}),
            json!({"type":"process_stop","processId":"00000000-0000-7000-8000-000000000000"}),
            json!({"type":"process_wait","processId":"00000000-0000-7000-8000-000000000000","timeoutMs":500}),
            json!({"type":"goal_create","objective":"ship","tokenBudget":100}),
            json!({"type":"goal_get"}),
            json!({"type":"goal_pause"}),
            json!({"type":"goal_resume"}),
            json!({"type":"goal_complete"}),
            json!({"type":"goal_drop"}),
            json!({"type":"goal_update_usage","tokens":5,"activeTimeSeconds":1}),
            json!({"type":"settings_inspect"}),
            json!({"type":"settings_search","query":"retry"}),
            json!({"type":"settings_open_draft","scope":"global"}),
            json!({"type":"settings_get_draft","draftId":"draft"}),
            json!({"type":"settings_set","draftId":"draft","key":"compaction.enabled","value":false}),
            json!({"type":"settings_reset","draftId":"draft","key":"theme"}),
            json!({"type":"settings_validate","draftId":"draft"}),
            json!({"type":"settings_apply","draftId":"draft"}),
            json!({"type":"settings_cancel","draftId":"draft"}),
            json!({"type":"workflow_create","name":"ship","objective":"land multi-workflow"}),
            json!({"type":"workflow_list"}),
            json!({"type":"workflow_get","workflowId":"wf-1"}),
            json!({"type":"workflow_get","name":"ship"}),
            json!({"type":"workflow_detail","workflowId":"wf-1"}),
            json!({"type":"workflow_detail","name":"ship"}),
            json!({"type":"workflow_pause","workflowId":"wf-1"}),
            json!({"type":"workflow_resume","workflowId":"wf-1"}),
            json!({"type":"workflow_cancel","workflowId":"wf-1"}),
            json!({"type":"workflow_integrate","workflowId":"wf-1"}),
            json!({"type":"workflow_remove","workflowId":"wf-1"}),
            json!({"type":"workflow_hub_send","workflowId":"wf-1","generation":1,"recipient":"writer","body":"ping"}),
            json!({"type":"workflow_hub_send","id":"whs-1","workflowId":"wf-1","generation":1,"recipient":"writer","body":"ping","replyTo":"msg-1"}),
            json!({"type":"side_chat_new","name":"research"}),
            json!({"type":"side_chat_switch","name":"research"}),
            json!({"type":"side_chat_close"}),
            json!({"type":"side_chat_close","name":"research"}),
            json!({"type":"side_chat_prompt","message":"summarize"}),
            json!({"type":"side_chat_prompt","message":"summarize","images":[{"type":"image","data":"aW1n","mimeType":"image/png"}]}),
            json!({"type":"side_chat_list"}),
            json!({"type":"side_chat_abort"}),
            json!({"type":"snapcompact"}),
            json!({"type":"rewind","index":4}),
            json!({"type":"rewind","checkpoint":"milestone"}),
            json!({"type":"handoff"}),
            json!({"type":"handoff","prose":true}),
            json!({"type":"queue_list"}),
            json!({"type":"queue_cancel"}),
            json!({"type":"queue_cancel","scope":"steering","index":0}),
            json!({"type":"queue_send_now"}),
            json!({"type":"job_list"}),
            json!({"type":"task_spawn","args":{"task":"inspect source"}}),
            json!({"type":"task_spawn","args":{"context":"shared","tasks":[{"task":"one"}]}}),
            json!({"type":"job_cancel","jobIds":["job-1"]}),
            json!({"type":"hub_send","to":"writer","body":"ping"}),
            json!({"type":"job_output","jobId":"job-1"}),
            json!({"type":"agent_history","agentId":"writer"}),
            json!({"type":"agent_history","agentId":"writer","lines":80}),
            json!({"type":"code_review_open"}),
            json!({"type":"code_review_open","from":"HEAD","to":"feature"}),
            json!({"type":"code_review_open","from":"HEAD","includeWorkingTree":true}),
            json!({"type":"code_review_open","commit":"abc123"}),
            json!({"type":"code_review_open","commit":"abc123","includeWorkingTree":false}),
            json!({"type":"code_review_commits"}),
            json!({"type":"code_review_commits","limit":5}),
            json!({"type":"code_review_snapshot"}),
            json!({"type":"code_review_refresh"}),
            json!({"type":"code_review_comment","snapshotId":"s1","path":"a.rs","oldStart":1,"oldCount":1,"newStart":1,"newCount":1,"contentHash":"h","comment":"looks good"}),
            json!({"type":"code_review_comment","snapshotId":"s1","path":"a.rs","oldStart":1,"oldCount":1,"newStart":1,"newCount":1,"contentHash":"h","comment":"see this","images":[{"type":"image","data":"aW1n","mimeType":"image/png"}]}),
            json!({"type":"code_review_set_model","modelId":"grok-4"}),
            json!({"type":"code_review_auto_review"}),
            json!({"type":"code_review_auto_review","modelId":"grok-4"}),
            json!({"type":"code_review_highlight"}),
            json!({"type":"code_review_tldr"}),
            json!({"type":"code_review_tldr","id":"tldr-1","language":"en"}),
            json!({"type":"code_review_stop","operation":"autoReview"}),
            json!({"type":"code_review_stop","operation":"highlight"}),
            json!({"type":"code_review_stop","operation":"tldr"}),
            json!({"type":"code_review_abort","snapshotId":"s1","path":"a.rs","oldStart":1,"oldCount":1,"newStart":1,"newCount":1,"contentHash":"h"}),
            json!({"type":"code_review_close"}),
            json!({"type":"code_review_file_diff","snapshotId":"s1","path":"a.rs"}),
            json!({"type":"code_review_file_diff","snapshotId":"s1","path":"a.rs","cursor":100,"maxLines":50}),
            json!({"type":"code_review_file_context","snapshotId":"s1","path":"a.rs","oldStart":1,"newStart":1,"count":20}),
            json!({"type":"file_explorer_list"}),
            json!({"type":"file_explorer_list","path":"src"}),
            json!({"type":"file_explorer_search","query":"widget"}),
            json!({"type":"file_explorer_read","path":"src/lib.rs"}),
            json!({"type":"file_explorer_read","path":"src/lib.rs","cursor":400,"maxLines":2000}),
            json!({"type":"file_explorer_changes"}),
            json!({"type":"file_explorer_diff","path":"src/lib.rs"}),
            json!({"type":"file_explorer_blame","path":"src/lib.rs","startLine":10,"endLine":20}),
            json!({"type":"file_explorer_stage_download","path":"src/lib.rs"}),
            json!({"type":"file_explorer_map","id":"map-1","path":"src/lib.rs"}),
            json!({"type":"path_complete","path":"src/","kind":"any"}),
            json!({"type":"path_complete","path":"src/ma","kind":"directory","limit":20}),
            json!({"type":"skill","name":"research"}),
            json!({"type":"run","name":"extension-only"}),
            json!({"type":"run","name":"extension-only","args":"hello"}),
            json!({"type":"persona_list"}),
            json!({"type":"persona_get","name":"mentor"}),
            json!({"type":"persona_create","name":"guide","content":"---\nname: guide\ndescription: g\n---\nprompt"}),
            json!({"type":"persona_edit","name":"mentor","content":"---\nname: mentor\ndescription: m\n---\nprompt"}),
            json!({"type":"persona_remove","name":"mentor","confirm":true}),
            json!({"type":"persona_purge","name":"mentor","confirm":true}),
            json!({"type":"persona_select","name":"mentor"}),
            json!({"type":"persona_clear"}),
            json!({"type":"persona_current"}),
            json!({"type":"get_discovered_runners"}),
        ];
        for f in fixtures {
            assert!(
                matches!(
                    parse_input(&serde_json::to_vec(&f).unwrap()),
                    Ok(RpcInput::Command { .. })
                ),
                "{f}"
            );
        }
    }

    #[test]
    fn path_complete_rejects_missing_kind_on_wire() {
        // Wire contract: `kind` is required (`"directory"|"file"|"any"`);
        // it is never defaulted for the client.
        let missing_kind = json!({"type":"path_complete","path":"src/"});
        assert!(
            parse_input(&serde_json::to_vec(&missing_kind).unwrap()).is_err(),
            "path_complete without kind must fail closed"
        );
    }

    #[test]
    fn file_explorer_blame_requires_exact_path_and_range() {
        let parsed = parse_input(&serde_json::to_vec(&json!({
            "type": "file_explorer_blame",
            "id": "blame-1",
            "path": "src/lib.rs",
            "startLine": 10,
            "endLine": 20,
            "sessionId": "owner-session"
        })).unwrap())
        .expect("file_explorer_blame");
        let RpcInput::Command { command, session_id } = parsed else {
            panic!("expected a command");
        };
        assert_eq!(session_id.as_deref(), Some("owner-session"));
        assert_eq!(command.command_name(), "file_explorer_blame");
        assert_eq!(command.id().as_deref(), Some("blame-1"));
        assert!(!command.runs_inline());
        assert!(matches!(
            command,
            RpcCommand::FileExplorerBlame {
                path,
                start_line: 10,
                end_line: 20,
                ..
            } if path == "src/lib.rs"
        ));
        for fixture in [
            json!({"type":"file_explorer_blame","path":"src/lib.rs","endLine":20}),
            json!({"type":"file_explorer_blame","path":"src/lib.rs","startLine":10}),
            json!({"type":"file_explorer_blame","startLine":10,"endLine":20}),
        ] {
            assert!(
                parse_input(&serde_json::to_vec(&fixture).unwrap()).is_err(),
                "incomplete blame command must fail closed: {fixture}"
            );
        }
    }
    #[test]
    fn code_review_tldr_parses_optional_language_and_runs_inline() {
        let parsed = parse_input(
            &serde_json::to_vec(&json!({
                "type": "code_review_tldr",
                "id": "tldr-1",
                "language": "en"
            }))
            .unwrap(),
        )
        .expect("code_review_tldr");
        let RpcInput::Command { command, .. } = parsed else {
            panic!("expected a command");
        };
        assert_eq!(command.command_name(), "code_review_tldr");
        assert_eq!(command.id().as_deref(), Some("tldr-1"));
        assert!(command.runs_inline());
        assert!(!command.bypasses_command_slots());
        assert!(matches!(
            command,
            RpcCommand::CodeReviewTldr { language: Some(language), .. } if language == "en"
        ));
        let absent = parse_input(&serde_json::to_vec(&json!({"type":"code_review_tldr"})).unwrap())
            .expect("language defaults to absent");
        let RpcInput::Command { command, .. } = absent else {
            panic!("expected a command");
        };
        assert!(matches!(
            command,
            RpcCommand::CodeReviewTldr { language: None, .. }
        ));
    }

    #[test]
    fn file_explorer_map_requires_path_and_blocks_on_a_slot() {
        let parsed = parse_input(
            &serde_json::to_vec(&json!({
                "type": "file_explorer_map",
                "id": "map-1",
                "path": "src/lib.rs",
                "sessionId": "owner-session"
            }))
            .unwrap(),
        )
        .expect("file_explorer_map");
        let RpcInput::Command { command, session_id } = parsed else {
            panic!("expected a command");
        };
        assert_eq!(session_id.as_deref(), Some("owner-session"));
        assert_eq!(command.command_name(), "file_explorer_map");
        assert_eq!(command.id().as_deref(), Some("map-1"));
        assert!(!command.runs_inline());
        assert!(!command.bypasses_command_slots());
        assert!(matches!(
            command,
            RpcCommand::FileExplorerMap { path, .. } if path == "src/lib.rs"
        ));
        assert!(
            parse_input(&serde_json::to_vec(&json!({"type":"file_explorer_map"})).unwrap()).is_err(),
            "file_explorer_map without path must fail closed"
        );
    }
    #[test]
    fn path_complete_accepts_and_strips_top_level_session_id() {
        let owned = json!({
            "type":"path_complete",
            "path":"src/ma",
            "kind":"directory",
            "limit":20,
            "sessionId":"owner-session"
        });
        let parsed = parse_input(&serde_json::to_vec(&owned).unwrap())
            .expect("path_complete with a top-level sessionId must parse");
        let RpcInput::Command { command, session_id } = parsed else {
            panic!("expected a command: {parsed:?}");
        };
        assert_eq!(session_id.as_deref(), Some("owner-session"));
        assert!(matches!(
            command,
            RpcCommand::PathComplete { path, kind: crate::file_explorer::CompletionKind::Directory, limit: Some(20), .. }
                if path == "src/ma"
        ));
    }
    #[test]
    fn switch_session_response_mode_is_explicitly_negotiated() {
        let negotiated: RpcCommand = serde_json::from_value(json!({
            "type": "switch_session",
            "id": "warm",
            "sessionPath": "session.jsonl",
            "responseMode": "subscribe-handoff-v1"
        }))
        .expect("negotiated switch parses");
        assert!(matches!(
            negotiated,
            RpcCommand::SwitchSession {
                response_mode: Some(SubscribeHandoffV1::SubscribeHandoffV1),
                ..
            }
        ));
        let legacy: RpcCommand = serde_json::from_value(json!({
            "type": "switch_session",
            "sessionPath": "session.jsonl"
        }))
        .expect("legacy switch parses");
        assert!(matches!(
            legacy,
            RpcCommand::SwitchSession {
                response_mode: None,
                ..
            }
        ));
    }

    #[test]
    fn switch_session_catalog_tuple_parses_and_normalizes_aliases() {
        let command = parse_input(
            br#"{"type":"switch_session","id":"tuple","sessionPath":"row.jsonl","responseMode":"subscribe-handoff-v1","catalogSource":"primary","catalogSessionId":" row-id ","catalogStatus":"native"}"#,
        )
        .ok()
        .and_then(|input| match input {
            RpcInput::Command { command, .. } => Some(command),
            _ => None,
        })
        .expect("negotiated switch with catalog tuple parses");
        let identity = command
            .switch_catalog_identity()
            .expect("valid tuple")
            .expect("tuple present");
        assert_eq!(identity.source(), CatalogSwitchSource::Rpi);
        assert_eq!(identity.status(), CatalogSwitchStatus::Native);
        assert_eq!(identity.session_id(), "row-id");
        assert!(identity.is_native());

        let hyper = CatalogSwitchIdentity::parse("grok-hyper", "grok-id", "foreign")
            .expect("grok alias normalizes");
        assert_eq!(hyper.source(), CatalogSwitchSource::Grok);
        assert_eq!(hyper.source().wire(), "grok");
        let slash = CatalogSwitchIdentity::parse("grok/hyper", "grok-id", "foreign")
            .expect("grok/hyper alias normalizes");
        assert_eq!(slash.source(), CatalogSwitchSource::Grok);
        assert!(!slash.is_native());
    }

    #[test]
    fn switch_session_catalog_tuple_requires_all_three_fields() {
        let command: RpcCommand = serde_json::from_value(json!({
            "type": "switch_session",
            "sessionPath": "row.jsonl",
            "catalogSource": "codex",
            "catalogSessionId": "codex-id"
        }))
        .expect("partial tuple still deserializes");
        let error = command
            .switch_catalog_identity()
            .expect_err("partial tuple must fail closed");
        assert_eq!(error, SwitchCatalogIdentityError::Incomplete);
        assert_eq!(error.code(), "invalid_request");
        assert!(!error.message().contains("codex"));
    }

    #[test]
    fn switch_session_catalog_tuple_rejects_unknown_values() {
        for (source, status) in [
            ("claude", "native"),
            ("codex", "imported"),
            ("codex", ""),
            ("", "foreign"),
        ] {
            assert_eq!(
                switch_catalog_identity(Some(source), Some("id"), Some(status)),
                Err(SwitchCatalogIdentityError::Invalid),
                "{source}/{status} must be rejected"
            );
        }
        assert_eq!(
            switch_catalog_identity(Some("codex"), Some("   "), Some("foreign")),
            Err(SwitchCatalogIdentityError::Invalid),
            "empty catalog session id must be rejected"
        );
        // Absent tuple keeps the legacy path-only flow.
        assert_eq!(switch_catalog_identity(None, None, None), Ok(None));
    }

    #[test]
    fn export_html_session_path_is_optional_camel_case() {
        let selected = parse_input(
            br#"{"type":"export_html","id":"export-selected","sessionPath":"row-token"}"#,
        )
        .expect("selected export command");
        assert!(matches!(
            selected,
            RpcInput::Command {
                command: RpcCommand::ExportHtml {
                    id: Some(id),
                    output_path: None,
                    session_path: Some(session_path),
                },
                ..
            } if id == "export-selected" && session_path == "row-token"
        ));

        let legacy = parse_input(br#"{"type":"export_html","outputPath":"out.html"}"#)
            .expect("legacy export command");
        assert!(matches!(
            legacy,
            RpcInput::Command {
                command: RpcCommand::ExportHtml {
                    output_path: Some(output_path),
                    session_path: None,
                    ..
                },
                ..
            } if output_path == "out.html"
        ));
    }

    #[test]
    fn session_copy_context_requires_session_path_and_parses_id() {
        let selected = parse_input(
            br#"{"type":"session_copy_context","id":"copy-selected","sessionPath":"row-token"}"#,
        )
        .expect("selected copy-context command");
        assert!(matches!(
            selected,
            RpcInput::Command {
                command: RpcCommand::SessionCopyContext {
                    id: Some(id),
                    session_path,
                },
                session_id: None,
            } if id == "copy-selected" && session_path == "row-token"
        ));

        let bare = parse_input(br#"{"type":"session_copy_context","sessionPath":"s"}"#)
            .expect("bare copy-context command");
        assert!(matches!(
            bare,
            RpcInput::Command {
                command: RpcCommand::SessionCopyContext {
                    id: None,
                    session_path,
                },
                ..
            } if session_path == "s"
        ));

        let missing = parse_input(br#"{"type":"session_copy_context"}"#)
            .expect_err("missing sessionPath must be rejected");
        assert!(!missing.success);
        assert_eq!(missing.command, "session_copy_context");
        assert!(
            missing
                .error
                .as_deref()
                .is_some_and(|error| error.contains("sessionPath")),
            "{missing:?}"
        );
    }

    #[tokio::test]
    async fn session_copy_context_name_and_direct_reject() {
        let parsed = parse_input(br#"{"type":"session_copy_context","sessionPath":"s"}"#)
            .expect("copy-context command");
        let RpcInput::Command { command, .. } = parsed else {
            panic!("expected RPC command");
        };
        assert_eq!(command.command_name(), "session_copy_context");
        assert_eq!(command.id(), None);

        let app = build_todo_app("faux-rpc-copy-context-reject", "faux-rpc-copy-context-reject-api")
            .await;
        let response = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::SessionCopyContext {
                id: Some("copy-direct".into()),
                session_path: "row-token".into(),
            },
        )
        .await;
        assert!(!response.success, "{response:?}");
        assert_eq!(response.command, "session_copy_context");
        assert_eq!(
            response.error.as_deref(),
            Some("session_copy_context is available only through the Web listener")
        );
        assert!(
            !response
                .error
                .as_deref()
                .is_some_and(|error| error.contains("row-token")),
            "no requested path may leak into the rejection: {response:?}"
        );
        app.cleanup().await;
    }

    #[test]
    fn session_move_requires_session_path_and_target_cwd() {
        let selected = parse_input(
            br#"{"type":"session_move","id":"move-selected","sessionPath":"row-token","targetCwd":"/tmp/target"}"#,
        )
        .expect("selected session_move command");
        assert!(matches!(
            selected,
            RpcInput::Command {
                command: RpcCommand::SessionMove {
                    id: Some(id),
                    session_path,
                    target_cwd,
                },
                session_id: None,
            } if id == "move-selected" && session_path == "row-token" && target_cwd == "/tmp/target"
        ));

        let bare =
            parse_input(br#"{"type":"session_move","sessionPath":"s","targetCwd":"/tmp/t"}"#)
                .expect("bare session_move command");
        assert!(matches!(
            bare,
            RpcInput::Command {
                command: RpcCommand::SessionMove {
                    id: None,
                    session_path,
                    target_cwd,
                },
                ..
            } if session_path == "s" && target_cwd == "/tmp/t"
        ));

        let missing_path = parse_input(br#"{"type":"session_move","targetCwd":"/tmp/t"}"#)
            .expect_err("missing sessionPath must be rejected");
        assert!(!missing_path.success);
        assert_eq!(missing_path.command, "session_move");
        assert!(
            missing_path
                .error
                .as_deref()
                .is_some_and(|error| error.contains("sessionPath")),
            "{missing_path:?}"
        );

        let missing_cwd = parse_input(br#"{"type":"session_move","sessionPath":"s"}"#)
            .expect_err("missing targetCwd must be rejected");
        assert!(!missing_cwd.success);
        assert_eq!(missing_cwd.command, "session_move");
        assert!(
            missing_cwd
                .error
                .as_deref()
                .is_some_and(|error| error.contains("targetCwd")),
            "{missing_cwd:?}"
        );
    }

    #[tokio::test]
    async fn session_move_name_and_direct_reject() {
        let parsed =
            parse_input(br#"{"type":"session_move","sessionPath":"s","targetCwd":"/tmp/t"}"#)
                .expect("session_move command");
        let RpcInput::Command { command, .. } = parsed else {
            panic!("expected RPC command");
        };
        assert_eq!(command.command_name(), "session_move");
        assert_eq!(command.id(), None);

        let app = build_todo_app("faux-rpc-session-move-reject", "faux-rpc-session-move-reject-api")
            .await;
        let response = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::SessionMove {
                id: Some("move-direct".into()),
                session_path: "row-token".into(),
                target_cwd: "/tmp/target".into(),
            },
        )
        .await;
        assert!(!response.success, "{response:?}");
        assert_eq!(response.command, "session_move");
        assert_eq!(
            response.error.as_deref(),
            Some("session_move is available only through the multi-session Web runtime")
        );
        assert!(
            !response
                .error
                .as_deref()
                .is_some_and(|error| error.contains("row-token") || error.contains("/tmp/target")),
            "no requested path may leak into the rejection: {response:?}"
        );
        app.cleanup().await;
    }

    fn new_session_cwd_parses_as_optional_camel_case_string() {
        let explicit = parse_input(
            br#"{"type":"new_session","id":"new-explicit","cwd":"<workspace>/project-a"}"#,
        )
        .expect("explicit cwd");
        assert!(matches!(
            explicit,
            RpcInput::Command {
                command: RpcCommand::NewSession {
                    id: Some(id),
                    parent_session: None,
                    cwd: Some(cwd),
                },
                session_id: None,
            } if id == "new-explicit" && cwd == "<workspace>/project-a"
        ));

        let inherited = parse_input(br#"{"type":"new_session"}"#).expect("inherited cwd");
        assert!(matches!(
            inherited,
            RpcInput::Command {
                command: RpcCommand::NewSession { cwd: None, .. },
                ..
            }
        ));

        let null_cwd = parse_input(br#"{"type":"new_session","cwd":null}"#)
            .expect("null cwd preserves inheritance");
        assert!(matches!(
            null_cwd,
            RpcInput::Command {
                command: RpcCommand::NewSession { cwd: None, .. },
                ..
            }
        ));

        let invalid =
            parse_input(br#"{"type":"new_session","cwd":7}"#).expect_err("non-string cwd rejected");
        assert_eq!(invalid.command, "new_session");
        assert!(
            invalid
                .error
                .as_deref()
                .is_some_and(|error| error.contains("Invalid command"))
        );
    }

    #[test]
    fn persona_commands_parse_and_confirm_defaults_false() {
        // persona_remove/persona_purge default `confirm` to false so an
        // omitted confirmation can never deserialize into a destructive op
        // (fail-closed; the handler rejects without an explicit true).
        for raw in [
            &br#"{"type":"persona_remove","name":"mentor"}"#[..],
            &br#"{"type":"persona_purge","name":"mentor"}"#[..],
        ] {
            match parse_input(raw).expect("parse") {
                RpcInput::Command { command, .. } => {
                    let confirmed = match command {
                        RpcCommand::PersonaRemove { confirm, .. }
                        | RpcCommand::PersonaPurge { confirm, .. } => confirm,
                        other => panic!("expected destructive persona command, got {other:?}"),
                    };
                    assert!(!confirmed, "confirm must default to false");
                }
                RpcInput::ExtensionUiResponse(_) => panic!("expected RPC command"),
            }
        }
        let ok = parse_input(br#"{"type":"persona_remove","name":"mentor","confirm":true}"#)
            .expect("confirmed remove");
        assert!(matches!(
            ok,
            RpcInput::Command {
                command: RpcCommand::PersonaRemove { name, confirm: true, .. },
                ..
            } if name == "mentor"
        ));
    }

    #[test]
    fn agent_history_parses_agent_id_and_optional_lines() {
        // lines is optional and omitted requests keep the handler default.
        let defaulted = parse_input(br#"{"type":"agent_history","agentId":"writer"}"#)
            .expect("agent history without lines");
        assert!(matches!(
            defaulted,
            RpcInput::Command {
                command: RpcCommand::AgentHistory { agent_id, lines: None, .. },
                ..
            } if agent_id == "writer"
        ));
        let with_lines = parse_input(br#"{"type":"agent_history","agentId":"writer","lines":120}"#)
            .expect("agent history with lines");
        assert!(matches!(
            with_lines,
            RpcInput::Command {
                command: RpcCommand::AgentHistory { agent_id, lines: Some(120), .. },
                ..
            } if agent_id == "writer"
        ));
        // agentId is required: a payload without it must not deserialize.
        let missing = parse_input(br#"{"type":"agent_history"}"#);
        assert!(
            missing
                .as_ref()
                .err()
                .is_some_and(|response| !response.success && response.command == "agent_history"),
            "agentId must be required: {missing:?}"
        );
    }
    #[test]
    fn session_list_scope_defaults_current_and_accepts_all_projects() {
        let current = parse_input(br#"{"type":"session_list"}"#).expect("current scope");
        assert!(matches!(
            current,
            RpcInput::Command {
                command: RpcCommand::SessionList {
                    scope: RpcSessionListScope::Current,
                    ..
                },
                ..
            }
        ));
        let all = parse_input(br#"{"type":"session_list","scope":"all_projects"}"#)
            .expect("all projects scope");
        assert!(matches!(
            all,
            RpcInput::Command {
                command: RpcCommand::SessionList {
                    scope: RpcSessionListScope::AllProjects,
                    ..
                },
                ..
            }
        ));
    }

    /// Regression for the intermittent Web frontend error
    /// `invalid type: null, expected a sequence`. RPC commands carry several
    /// OPTIONAL sequence fields (`#[serde(default)]` ⇒ absent = empty). A
    /// client that always serializes the key but emits `null` when there is
    /// nothing to send (a common foreign/legacy shape) used to trip serde's
    /// `Vec` deserializer on the explicit `null` — `#[serde(default)]` only
    /// applies when the field is ABSENT, not when it is present-and-null —
    /// so the whole frame was rejected. The fix routes those optional
    /// sequences through [`null_default_vec`] (null ⇒ empty, array ⇒ vec,
    /// anything else still errors). REQUIRED arrays keep plain `Vec`
    /// deserialization so `null` stays a hard error there.
    #[test]
    fn optional_sequence_fields_accept_null_as_empty() {
        // The exact pre-fix failure mode on the same field type: a plain
        // `Vec<ContentBlock>` deserialized from an explicit `null` errors
        // with the reported message. This is the contract the fix works
        // around without weakening required arrays.
        let pre_fix = serde_json::from_value::<Vec<ContentBlock>>(Value::Null)
            .expect_err("plain Vec rejects null");
        assert!(
            pre_fix.to_string().contains("invalid type: null"),
            "pre-fix error must mention null: {pre_fix}"
        );
        assert!(
            pre_fix.to_string().contains("expected a sequence"),
            "pre-fix error must mention a sequence: {pre_fix}"
        );

        let parse_command = |value: Value| match parse_input(
            &serde_json::to_vec(&value).expect("serialize command fixture"),
        )
        .expect("parse command fixture")
        {
            RpcInput::Command { command, .. } => command,
            RpcInput::ExtensionUiResponse(_) => panic!("expected RPC command"),
        };

        // OPTIONAL sequence fields: explicit null decodes to the same empty
        // Vec as an absent field (prompt/steer/follow_up images, job_cancel
        // jobIds). Absent and empty-array already worked; null now matches.
        let prompt_null = parse_command(json!({"type":"prompt","message":"hi","images":null}));
        let RpcCommand::Prompt { images, .. } = prompt_null else {
            panic!("fixture must deserialize as prompt");
        };
        assert!(images.is_empty(), "null images -> empty");

        let prompt_absent = parse_command(json!({"type":"prompt","message":"hi"}));
        let RpcCommand::Prompt {
            images: absent_images,
            ..
        } = prompt_absent
        else {
            panic!("absent fixture must deserialize as prompt");
        };
        assert!(absent_images.is_empty(), "absent images -> empty");

        let steer_null = parse_command(json!({"type":"steer","message":"hi","images":null}));
        let RpcCommand::Steer {
            images: steer_images,
            ..
        } = steer_null
        else {
            panic!("fixture must deserialize as steer");
        };
        assert!(steer_images.is_empty(), "null steer images -> empty");

        let follow_up_null =
            parse_command(json!({"type":"follow_up","message":"hi","images":null}));
        let RpcCommand::FollowUp {
            images: follow_up_images,
            ..
        } = follow_up_null
        else {
            panic!("fixture must deserialize as follow_up");
        };
        assert!(
            follow_up_images.is_empty(),
            "null follow_up images -> empty"
        );

        let job_cancel_null = parse_command(json!({"type":"job_cancel","jobIds":null}));
        let RpcCommand::JobCancel { job_ids, .. } = job_cancel_null else {
            panic!("fixture must deserialize as job_cancel");
        };
        assert!(job_ids.is_empty(), "null jobIds -> empty");

        // Normal array behavior is unchanged: a real array decodes verbatim.
        let job_cancel_array =
            parse_command(json!({"type":"job_cancel","jobIds":["job-1","job-2"]}));
        let RpcCommand::JobCancel {
            job_ids: array_ids, ..
        } = job_cancel_array
        else {
            panic!("array fixture must deserialize as job_cancel");
        };
        assert_eq!(array_ids, vec!["job-1".to_owned(), "job-2".to_owned()]);

        // OPTIONAL fields still reject malformed non-array, non-null values:
        // a scalar/object is not a valid sequence and must not be silently
        // coerced to empty (that would widen the contract).
        let scalar_err = parse_input(br#"{"type":"job_cancel","jobIds":"job-1"}"#)
            .expect_err("scalar jobIds must be rejected");
        assert!(
            scalar_err
                .error
                .as_deref()
                .unwrap_or("")
                .contains("Invalid command"),
            "scalar rejection must surface as a parse failure: {scalar_err:?}"
        );
        let object_err = parse_input(br#"{"type":"job_cancel","jobIds":{"a":1}}"#)
            .expect_err("object jobIds must be rejected");
        assert!(
            object_err
                .error
                .as_deref()
                .unwrap_or("")
                .contains("Invalid command"),
            "object rejection must surface as a parse failure: {object_err:?}"
        );

        // REQUIRED sequence fields stay strict: `null` is NOT semantically
        // valid for a required array and must remain a hard error — and the
        // parse boundary must name the field so the frontend can render an
        // actionable protocol error.
        let set_todos_null = parse_input(br#"{"type":"set_todos","phases":null}"#)
            .expect_err("null phases rejected");
        let set_todos_message = set_todos_null.error.as_deref().unwrap_or("");
        assert!(
            set_todos_message.contains("Invalid command set_todos"),
            "required phases null must be a field-aware parse error: {set_todos_message}"
        );
        assert!(
            set_todos_message.contains("phases") && set_todos_message.contains("must be an array"),
            "required phases null must name the field: {set_todos_message}"
        );
        let process_keys_null = parse_input(
            br#"{"type":"process_keys","processId":"00000000-0000-7000-8000-000000000000","keys":null}"#,
        )
        .expect_err("null keys rejected");
        let process_keys_message = process_keys_null.error.as_deref().unwrap_or("");
        assert!(
            process_keys_message.contains("Invalid command process_keys")
                && process_keys_message.contains("`keys`")
                && process_keys_message.contains("must be an array"),
            "required keys null must name the field: {process_keys_message}"
        );
        // A scalar/object on a required array is still rejected too.
        let process_keys_scalar = parse_input(
            br#"{"type":"process_keys","processId":"00000000-0000-7000-8000-000000000000","keys":"a"}"#,
        )
        .expect_err("scalar keys rejected");
        let process_keys_scalar_message = process_keys_scalar.error.as_deref().unwrap_or("");
        assert!(
            process_keys_scalar_message.contains("`keys`")
                && process_keys_scalar_message.contains("must be an array"),
            "required keys scalar must be a field-aware rejection: {process_keys_scalar_message}"
        );
        // A valid required array still parses normally.
        let set_todos_array =
            parse_input(br#"{"type":"set_todos","phases":[{"name":"Plan","tasks":[]}]}"#)
                .expect("valid set_todos phases must parse");
        assert!(matches!(
            set_todos_array,
            RpcInput::Command { command: RpcCommand::SetTodos { phases, .. }, .. } if phases.len() == 1
        ));
    }

    #[test]
    fn command_slot_bypass_is_limited_to_recovery_commands() {
        let parse_command = |value: Value| match parse_input(
            &serde_json::to_vec(&value).expect("serialize command fixture"),
        )
        .expect("parse command fixture")
        {
            RpcInput::Command { command, .. } => command,
            RpcInput::ExtensionUiResponse(_) => panic!("expected RPC command"),
        };

        for fixture in [
            json!({"type":"abort"}),
            json!({"type":"abort_retry"}),
            json!({"type":"abort_bash"}),
            json!({"type":"steer","message":"keep going"}),
            json!({"type":"follow_up","message":"after this"}),
            json!({"type":"queue_cancel","scope":"steering"}),
            json!({"type":"queue_send_now"}),
            json!({"type":"loop_cancel","taskId":"loop-1"}),
            json!({"type":"process_signal","processId":"00000000-0000-7000-8000-000000000000","signal":"SIGTERM"}),
            json!({"type":"process_stop","processId":"00000000-0000-7000-8000-000000000000"}),
            json!({"type":"code_review_abort","snapshotId":"s1","path":"a.rs","oldStart":1,"oldCount":1,"newStart":1,"newCount":1,"contentHash":"h"}),
            json!({"type":"code_review_stop","operation":"highlight"}),
            json!({"type":"code_review_close"}),
        ] {
            let command = parse_command(fixture.clone());
            assert!(
                command.bypasses_command_slots(),
                "recovery command must bypass saturated slots: {fixture}"
            );
        }

        for fixture in [
            json!({"type":"workflow_create","name":"ship","objective":"bounded"}),
            json!({"type":"process_wait","processId":"00000000-0000-7000-8000-000000000000"}),
            json!({"type":"code_review_open"}),
            json!({"type":"code_review_open","from":"HEAD","to":"feature"}),
            json!({"type":"code_review_open","from":"HEAD","includeWorkingTree":true}),
            json!({"type":"code_review_refresh"}),
            json!({"type":"code_review_commits"}),
        ] {
            let command = parse_command(fixture.clone());
            assert!(
                !command.bypasses_command_slots(),
                "ordinary awaited command must remain bounded: {fixture}"
            );
        }

        let settings = parse_command(json!({"type":"settings_apply","draftId":"draft"}));
        let workflow = parse_command(json!({"type":"workflow_pause","workflowId":"wf-1"}));
        assert!(settings.runs_inline() && workflow.runs_inline());

        // Code-review snapshot/comment/abort/close run inline (controller state
        // reads and safety/teardown); open/refresh/commits take a slot (spawn_blocking git).
        let snapshot = parse_command(json!({"type":"code_review_snapshot"}));
        let comment = parse_command(
            json!({"type":"code_review_comment","snapshotId":"s","path":"a","oldStart":1,"oldCount":1,"newStart":1,"newCount":1,"contentHash":"h","comment":"x"}),
        );
        let abort = parse_command(
            json!({"type":"code_review_abort","snapshotId":"s","path":"a","oldStart":1,"oldCount":1,"newStart":1,"newCount":1,"contentHash":"h"}),
        );
        let stop = parse_command(json!({"type":"code_review_stop","operation":"autoReview"}));
        let close = parse_command(json!({"type":"code_review_close"}));
        let open = parse_command(json!({"type":"code_review_open"}));
        let refresh = parse_command(json!({"type":"code_review_refresh"}));
        let commits = parse_command(json!({"type":"code_review_commits"}));
        let file_diff =
            parse_command(json!({"type":"code_review_file_diff","snapshotId":"s","path":"a"}));
        let explorer_read =
            parse_command(json!({"type":"file_explorer_read","path":"src/lib.rs"}));
        let explorer_diff =
            parse_command(json!({"type":"file_explorer_diff","path":"src/lib.rs"}));
        let explorer_blame = parse_command(
            json!({"type":"file_explorer_blame","path":"src/lib.rs","startLine":10,"endLine":20}),
        );
        let explorer_stage = parse_command(
            json!({"type":"file_explorer_stage_download","path":"src/lib.rs"}),
        );
        let tldr = parse_command(json!({"type":"code_review_tldr","language":"en"}));
        let explorer_map = parse_command(json!({"type":"file_explorer_map","path":"src/lib.rs"}));
        let tmux_list = parse_command(json!({"type":"tmux_list_sessions"}));
        let tmux_snap = parse_command(json!({"type":"tmux_pane_snapshot","session":"s"}));
        let tmux_send = parse_command(json!({
            "type":"tmux_send_keys",
            "session":"s",
            "text":"x"
        }));
        let tmux_create = parse_command(json!({
            "type":"tmux_session_create",
            "session":"s"
        }));
        let tmux_delete = parse_command(json!({
            "type":"tmux_session_delete",
            "session":"s"
        }));
        let tmux_attach = parse_command(json!({
            "type":"tmux_terminal_attach",
            "session":"s",
            "cols":80,
            "rows":24
        }));
        let tmux_input = parse_command(json!({
            "type":"tmux_terminal_input",
            "terminalId":"t",
            "data":"YQ=="
        }));
        let tmux_resize = parse_command(json!({
            "type":"tmux_terminal_resize",
            "terminalId":"t",
            "cols":100,
            "rows":30
        }));
        let tmux_detach = parse_command(json!({
            "type":"tmux_terminal_detach",
            "terminalId":"t"
        }));
        assert!(
            snapshot.runs_inline()
                && comment.runs_inline()
                && abort.runs_inline()
                && stop.runs_inline()
                && close.runs_inline()
                && tldr.runs_inline()
        );
        // file_diff re-runs scoped git via spawn_blocking: it takes a slot like
        // open/refresh (NOT inline, NOT a bypass).
        assert!(
            !open.runs_inline()
                && !refresh.runs_inline()
                && !file_diff.runs_inline()
                && !commits.runs_inline()
                && !explorer_read.runs_inline()
                && !explorer_diff.runs_inline()
                && !explorer_blame.runs_inline()
                && !explorer_stage.runs_inline()
                && !explorer_map.runs_inline()
                && !tmux_list.runs_inline()
                && !tmux_snap.runs_inline()
                && !tmux_send.runs_inline()
                && !tmux_create.runs_inline()
                && !tmux_delete.runs_inline()
                && !tmux_attach.runs_inline()
                && !tmux_input.runs_inline()
                && !tmux_resize.runs_inline()
                && !tmux_detach.runs_inline()
        );
        assert!(
            !open.bypasses_command_slots()
                && !refresh.bypasses_command_slots()
                && !snapshot.bypasses_command_slots()
                && !file_diff.bypasses_command_slots()
                && !commits.bypasses_command_slots()
                && !tldr.bypasses_command_slots()
                && !explorer_map.bypasses_command_slots()
        );

        // The stdio gate (runs_inline) must exempt steer/follow_up exactly
        // like the Web bypass: a saturated command list must never drop the
        // user's steering with "too many concurrent RPC commands".
        let steer = parse_command(json!({"type":"steer","message":"fix it"}));
        let follow_up = parse_command(json!({"type":"follow_up","message":"next"}));
        assert!(
            steer.runs_inline() && follow_up.runs_inline(),
            "steer/follow_up must run inline on the stdio gate"
        );

        // Interactive user-control commands bypass the ordinary FIFO so one
        // long-running ordinary command cannot starve them; read-only
        // get_messages still queues behind the ordinary FIFO.
        let prompt = parse_command(json!({"type":"prompt","message":"go"}));
        let set_model =
            parse_command(json!({"type":"set_model","provider":"p","modelId":"m"}));
        let set_thinking = parse_command(json!({"type":"set_thinking_level","level":"high"}));
        let get_messages = parse_command(json!({"type":"get_messages"}));
        assert!(
            prompt.bypasses_ordinary_fifo()
                && set_model.bypasses_ordinary_fifo()
                && set_thinking.bypasses_ordinary_fifo(),
            "interactive user-control commands must not starve behind the ordinary FIFO"
        );
        assert!(!get_messages.bypasses_ordinary_fifo());
        let overload = RpcResponse::slot_overload(
            Some("c1".into()),
            "get_state",
            format!("too many concurrent RPC commands (limit {MAX_CONCURRENT_COMMANDS})"),
        );
        assert_eq!(overload.data, Some(json!({ "code": "busy_capacity" })));
        assert!(overload.is_transient_slot_overload());
        let prose_only = RpcResponse::failure(
            Some("c2".into()),
            "get_state",
            format!("too many concurrent RPC commands (limit {MAX_CONCURRENT_COMMANDS})"),
        );
        assert!(!prose_only.is_transient_slot_overload());
    }
    fn public_models_never_serialize_headers() {

        let secret = "Bearer rpc-secret-must-not-leak";
        let model = Model {
            headers: Some(std::collections::HashMap::from([
                ("Authorization".to_owned(), secret.to_owned()),
                ("X-Probe-Secret".to_owned(), "probe-value".to_owned()),
            ])),
            ..Model::default()
        };
        let encoded = serde_json::to_string(&public_model(model)).unwrap();
        assert!(!encoded.contains(secret));
        assert!(!encoded.contains("probe-value"));
        assert_eq!(
            serde_json::from_str::<Value>(&encoded).unwrap()["headers"],
            Value::Null
        );
    }
    #[test]
    fn resolve_catalog_model_matches_provider_identity_not_labels() {
        let catalog = vec![
            Model {
                id: "m1".into(),
                provider: "p1".into(),
                name: "Label One".into(),
                ..Default::default()
            },
            Model {
                id: "m1".into(),
                provider: "p2".into(),
                name: "Label Two".into(),
                ..Default::default()
            },
            Model {
                id: "m2".into(),
                provider: "p1".into(),
                ..Default::default()
            },
            Model {
                id: "m/3".into(),
                provider: "p1".into(),
                ..Default::default()
            },
        ];
        assert_eq!(
            resolve_catalog_model(&catalog, "p2/m1").map(|model| model.provider.as_str()),
            Some("p2"),
            "qualified provider/id identity resolves across duplicate ids"
        );
        assert_eq!(
            resolve_catalog_model(&catalog, "p1/m2").map(|model| model.id.as_str()),
            Some("m2"),
        );
        assert_eq!(
            resolve_catalog_model(&catalog, "p1/m/3").map(|model| model.id.as_str()),
            Some("m/3"),
            "the first slash separates; a slash-containing id suffix is preserved"
        );
        assert!(
            resolve_catalog_model(&catalog, "m2").is_none(),
            "a bare id never resolves; the wire carries provider/id"
        );
        assert!(
            resolve_catalog_model(&catalog, "m1").is_none(),
            "an ambiguous bare id must fail closed"
        );
        assert!(
            resolve_catalog_model(&catalog, "Label One").is_none(),
            "display labels never resolve"
        );
        assert!(resolve_catalog_model(&catalog, "p1/nope").is_none());
    }

    #[tokio::test]
    async fn code_review_stale_close_cannot_supersede_inflight_open() {
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(cwd.path().join("review.txt"), "base\n").expect("write baseline");
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "baseline"]).status.success());
        std::fs::write(cwd.path().join("review.txt"), "first\n").expect("write first change");

        let app = build_code_review_app(cwd.path()).await;
        let code_review = CodeReviewRpcState::default();
        let initial = code_review
            .open(&app, None, None, None, None)
            .await
            .expect("initial open");
        let gate = Arc::new(ReviewCaptureGate {
            captured: tokio::sync::Notify::new(),
            released: std::sync::Mutex::new(false),
            release: std::sync::Condvar::new(),
        });
        *code_review.review_capture_gate.lock().await = Some(gate.clone());
        std::fs::write(cwd.path().join("review.txt"), "second\n").expect("write second change");
        let inflight = tokio::spawn({
            let app = app.clone();
            let state = code_review.clone();
            async move { state.open(&app, None, None, None, None).await }
        });
        gate.captured.notified().await;
        let stale = code_review
            .close(Some("retired-review-identity"))
            .await
            .expect("stale close returns ok");
        assert_eq!(stale["closed"], false, "{stale}");
        assert_eq!(stale["staleReview"], true, "{stale}");
        gate.release();
        let opened = inflight
            .await
            .expect("open task")
            .expect("in-flight open must not be superseded by a stale close");
        assert_ne!(
            opened["snapshotId"], initial["snapshotId"],
            "the gated open must cut over to its own capture"
        );
        assert!(code_review.snapshot().await.is_ok());
        // The complementary case: a matching (unstamped) close during a gated
        // open bumps the authority, so that capture fails its cutover and no
        // controller remains.
        let gate = Arc::new(ReviewCaptureGate {
            captured: tokio::sync::Notify::new(),
            released: std::sync::Mutex::new(false),
            release: std::sync::Condvar::new(),
        });
        *code_review.review_capture_gate.lock().await = Some(gate.clone());
        std::fs::write(cwd.path().join("review.txt"), "third\n").expect("write third change");
        let inflight = tokio::spawn({
            let app = app.clone();
            let state = code_review.clone();
            async move { state.open(&app, None, None, None, None).await }
        });
        gate.captured.notified().await;
        let closed = code_review
            .close(None)
            .await
            .expect("matching close closes the live controller");
        assert_eq!(closed["closed"], true, "{closed:?}");
        gate.release();
        let superseded = inflight
            .await
            .expect("open task")
            .expect_err("a matching close must supersede the gated open capture");
        assert!(superseded.to_string().contains("superseded"), "{superseded:#}");
        assert!(
            code_review.snapshot().await.is_err(),
            "the matching close left no controller to cut over to"
        );
        app.cleanup().await;
    }
    /// Cleanup close of review A after open B has reserved, and before B
    /// cuts over, must not invalidate that capture. Once B is installed, the
    /// same retired stamp cannot close it. Unstamped close still supersedes
    /// a reserved open; see the complementary case above.
    #[tokio::test]
    async fn code_review_close_of_retired_review_cannot_supersede_reserved_open() {
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(cwd.path().join("review.txt"), "base\n").expect("write baseline");
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "baseline"]).status.success());
        std::fs::write(cwd.path().join("review.txt"), "first\n").expect("write first change");

        let app = build_code_review_app(cwd.path()).await;
        let code_review = CodeReviewRpcState::default();
        let opened_a = code_review
            .open(&app, None, None, None, None)
            .await
            .expect("open A");
        let review_a = opened_a["reviewId"]
            .as_str()
            .expect("review A id")
            .to_owned();
        let gate = Arc::new(ReviewCaptureGate {
            captured: tokio::sync::Notify::new(),
            released: std::sync::Mutex::new(false),
            release: std::sync::Condvar::new(),
        });
        *code_review.review_capture_gate.lock().await = Some(gate.clone());
        std::fs::write(cwd.path().join("review.txt"), "second\n").expect("write second change");
        let inflight_b = tokio::spawn({
            let app = app.clone();
            let state = code_review.clone();
            async move { state.open(&app, None, None, None, None).await }
        });
        gate.captured.notified().await;
        let retired = code_review
            .close(Some(&review_a))
            .await
            .expect("closing the installed review A while B is reserved");
        assert_eq!(
            retired["closed"], true,
            "the stamp still names the installed controller: {retired}"
        );
        assert!(retired.get("staleReview").is_none(), "{retired}");
        gate.release();
        *code_review.review_capture_gate.lock().await = None;
        let opened_b = inflight_b
            .await
            .expect("open B task")
            .expect("close of retired review A must not supersede open B reserved before cutover");
        let review_b = opened_b["reviewId"]
            .as_str()
            .expect("review B id")
            .to_owned();
        assert_ne!(review_b, review_a, "open B must install its own controller");
        assert_ne!(
            opened_b["snapshotId"], opened_a["snapshotId"],
            "open B must cut over to its own capture"
        );
        assert_eq!(
            code_review.snapshot().await.expect("B is open")["reviewId"],
            review_b,
            "the reserved open must be the live controller"
        );
        let stale = code_review
            .close(Some(&review_a))
            .await
            .expect("stale close of A after B cut over");
        assert_eq!(stale["closed"], false, "{stale}");
        assert_eq!(stale["staleReview"], true, "{stale}");
        assert_eq!(
            code_review.snapshot().await.expect("B survived stale close")["reviewId"],
            review_b,
            "a stale close of A must not close B"
        );
        let closed_b = code_review
            .close(Some(&review_b))
            .await
            .expect("matching close of B");
        assert_eq!(closed_b["closed"], true, "{closed_b:?}");
        assert!(
            code_review.snapshot().await.is_err(),
            "a close stamped with the live review must still close it"
        );
        app.cleanup().await;
    }

    #[tokio::test]
    async fn code_review_stale_close_cannot_supersede_inflight_refresh() {
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(cwd.path().join("review.txt"), "base\n").expect("write baseline");
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "baseline"]).status.success());
        std::fs::write(cwd.path().join("review.txt"), "changed\n").expect("write change");

        let app = build_code_review_app(cwd.path()).await;
        let code_review = CodeReviewRpcState::default();
        let initial = code_review
            .open(&app, None, None, None, None)
            .await
            .expect("initial open");
        let review_id = initial["reviewId"].as_str().expect("review id").to_owned();
        let gate = Arc::new(ReviewCaptureGate {
            captured: tokio::sync::Notify::new(),
            released: std::sync::Mutex::new(false),
            release: std::sync::Condvar::new(),
        });
        *code_review.review_capture_gate.lock().await = Some(gate.clone());
        let refresh = tokio::spawn({
            let app = app.clone();
            let state = code_review.clone();
            async move { state.refresh(&app).await }
        });
        gate.captured.notified().await;
        let stale = code_review
            .close(Some("retired-review-identity"))
            .await
            .expect("stale close returns ok");
        assert_eq!(stale["closed"], false, "{stale}");
        assert_eq!(stale["staleReview"], true, "{stale}");
        gate.release();
        let refreshed = refresh
            .await
            .expect("refresh task")
            .expect("in-flight refresh must not be superseded by a stale close");
        assert_eq!(
            refreshed["reviewId"], review_id,
            "refresh keeps the same controller identity"
        );
        assert!(code_review.snapshot().await.is_ok());
        // The complementary case: a matching (unstamped) close during a gated
        // refresh bumps the authority, so that capture fails its cutover and
        // the controller is gone.
        let gate = Arc::new(ReviewCaptureGate {
            captured: tokio::sync::Notify::new(),
            released: std::sync::Mutex::new(false),
            release: std::sync::Condvar::new(),
        });
        *code_review.review_capture_gate.lock().await = Some(gate.clone());
        let inflight = tokio::spawn({
            let app = app.clone();
            let state = code_review.clone();
            async move { state.refresh(&app).await }
        });
        gate.captured.notified().await;
        let closed = code_review
            .close(None)
            .await
            .expect("matching close closes the live controller");
        assert_eq!(closed["closed"], true, "{closed:?}");
        gate.release();
        let superseded = inflight
            .await
            .expect("refresh task")
            .expect_err("a matching close must supersede the gated refresh capture");
        assert!(superseded.to_string().contains("superseded"), "{superseded:#}");
        assert!(
            code_review.snapshot().await.is_err(),
            "the matching close left no controller to cut over to"
        );
        app.cleanup().await;
    }

    #[test]
    fn process_write_rejects_malformed_base64() {
        let command: RpcCommand = serde_json::from_value(json!({
            "type":"process_write",
            "processId":"00000000-0000-7000-8000-000000000000",
            "dataBase64":"***"
        }))
        .unwrap();
        let RpcCommand::ProcessWrite { data_base64, .. } = command else {
            panic!("fixture must deserialize as process_write")
        };
        assert!(
            base64::engine::general_purpose::STANDARD
                .decode(data_base64)
                .is_err()
        );
    }
    #[test]
    fn runtime_settings_state_serializes_camel_case() {
        let value = serde_json::to_value(
            pi_coding::Settings::default()
                .runtime_settings()
                .unwrap()
                .state(),
        )
        .unwrap();
        assert!(value.get("autoRetry").is_some());
        assert!(value.get("processToolEnabled").is_some());
        assert!(value.get("auto_retry").is_none());
    }
    #[test]
    fn response_event_are_json() {
        let r = RpcResponse::success(
            Some("i".into()),
            "get_tree",
            Some(json!({"tree":[],"leafId":null,"activeLeafId":null})),
        );
        let v: Value = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(v["type"], "response");
        let e = RpcExtensionUiRequest {
            record_type: "extension_ui_request",
            id: "u".into(),
            session_id: None,
            runtime_incarnation: None,
            request: json!({"method":"confirm"}),
        };
        assert!(serde_json::from_str::<Value>(&serde_json::to_string(&e).unwrap()).is_ok());
    }

    #[test]
    fn extension_working_indicator_preserves_structured_options() {
        let request = ui_request(
            "working".to_owned(),
            pi_coding::ExtensionUiRequest::SetWorkingIndicator {
                options: Some(pi_coding::WorkingIndicatorOptions {
                    frames: Some(vec!["·".to_owned(), "●".to_owned()]),
                    interval_ms: Some(120),
                }),
            },
        )
        .unwrap();
        assert_eq!(request.request["method"], "set_working_indicator");
        assert_eq!(request.request["options"]["frames"], json!(["·", "●"]));
        assert_eq!(request.request["options"]["intervalMs"], 120);
    }

    #[tokio::test]
    async fn rpc_shadow_queries_fail_instead_of_reporting_canonical_state() {
        let adapter = ExtensionUiAdapter::new();
        adapter.set_canonical_queries_supported(false);
        let context = pi_coding::ExtensionUiContext {
            instance: pi_coding::ExtensionInstanceId {
                extension_id: "rpc-query".to_owned(),
                generation: 1,
            },
            mode: pi_coding::ExtensionMode::Rpc,
        };
        for request in [
            pi_coding::ExtensionUiRequest::GetEditorText,
            pi_coding::ExtensionUiRequest::GetAllThemes,
            pi_coding::ExtensionUiRequest::GetTheme {
                name: "dark".to_owned(),
            },
            pi_coding::ExtensionUiRequest::GetToolsExpanded,
        ] {
            let error = pi_coding::ExtensionUiHost::request(
                &adapter,
                context.clone(),
                request,
                pi_coding::ExtensionCancellation::new(),
            )
            .await
            .expect_err("shadow-only queries must be explicit unsupported errors");
            assert!(error.to_string().contains("canonical host state"));
        }
    }

    #[test]
    fn rpc_shadow_serializer_rejects_canonical_theme_queries() {
        for request in [
            pi_coding::ExtensionUiRequest::GetEditorText,
            pi_coding::ExtensionUiRequest::GetAllThemes,
            pi_coding::ExtensionUiRequest::GetTheme {
                name: "dark".to_owned(),
            },
            pi_coding::ExtensionUiRequest::GetToolsExpanded,
        ] {
            let error = ui_request("shadow".to_owned(), request)
                .expect_err("canonical query serializers must fail closed");
            assert!(
                error
                    .to_string()
                    .contains("unsupported by the RPC shadow adapter"),
                "{error:#}"
            );
        }
    }

    #[test]
    fn rpc_projection_emits_owner_identity_and_extension_cleared() {
        let status = ui_event_request(ExtensionUiEvent::StatusChanged {
            item: crate::extension_ui::ExtensionStatusItem {
                instance: pi_coding::ExtensionInstanceId {
                    extension_id: "owner-a".to_owned(),
                    generation: 1,
                },
                key: "phase".to_owned(),
                text: "running".to_owned(),
            },
        })
        .expect("status projection")
        .expect("status event should project");
        assert_eq!(status.request["method"], "setStatus");
        assert_eq!(status.request["statusKey"], "phase");
        assert_eq!(status.request["statusText"], "running");
        assert_eq!(status.request["extensionId"], "owner-a");
        assert_eq!(status.request["generation"], 1);

        let widget = ui_event_request(ExtensionUiEvent::WidgetChanged {
            item: crate::extension_ui::ExtensionWidgetItem {
                instance: pi_coding::ExtensionInstanceId {
                    extension_id: "owner-b".to_owned(),
                    generation: 2,
                },
                key: "panel".to_owned(),
                lines: vec!["line".to_owned()],
                placement: pi_coding::UiWidgetPlacement::AboveEditor,
            },
        })
        .expect("widget projection")
        .expect("widget event should project");
        assert_eq!(widget.request["method"], "setWidget");
        assert_eq!(widget.request["widgetKey"], "panel");
        assert_eq!(widget.request["extensionId"], "owner-b");
        assert_eq!(widget.request["generation"], 2);

        let cleared = ui_event_request(ExtensionUiEvent::ExtensionCleared {
            instance: pi_coding::ExtensionInstanceId {
                extension_id: "owner-a".to_owned(),
                generation: 1,
            },
        })
        .expect("clear projection")
        .expect("ExtensionCleared must project a canonical cleanup event");
        assert_eq!(cleared.request["method"], "extension_cleared");
        assert_eq!(cleared.request["extensionId"], "owner-a");
        assert_eq!(cleared.request["generation"], 1);
    }

    #[test]
    fn rpc_interaction_request_carries_owner_identity_for_cleanup() {
        let projected = ui_event_request(ExtensionUiEvent::InteractionRequested {
            interaction: crate::extension_ui::ExtensionUiInteraction {
                id: "dialog-1".to_owned(),
                context: pi_coding::ExtensionUiContext {
                    instance: pi_coding::ExtensionInstanceId {
                        extension_id: "owner-dialog".to_owned(),
                        generation: 7,
                    },
                    mode: pi_coding::ExtensionMode::Rpc,
                },
                request: pi_coding::ExtensionUiRequest::Confirm {
                    title: "Continue?".to_owned(),
                    message: "Apply change".to_owned(),
                },
            },
        })
        .expect("interaction projection")
        .expect("interaction event should project");
        assert_eq!(projected.id, "dialog-1");
        assert_eq!(projected.request["method"], "confirm");
        assert_eq!(projected.request["extensionId"], "owner-dialog");
        assert_eq!(projected.request["generation"], 7);

        let cleared = ui_event_request(ExtensionUiEvent::ExtensionCleared {
            instance: pi_coding::ExtensionInstanceId {
                extension_id: "owner-dialog".to_owned(),
                generation: 7,
            },
        })
        .expect("clear projection")
        .expect("ExtensionCleared must project");
        assert_eq!(cleared.request["method"], "extension_cleared");
        assert_eq!(cleared.request["extensionId"], "owner-dialog");
        assert_eq!(cleared.request["generation"], 7);
    }
    #[tokio::test]
    async fn malformed_then_valid_recovers() {
        let (mut w, r) = tokio::io::duplex(256);
        let (tx, mut rx) = mpsc::channel(4);
        let task = tokio::spawn(read_jsonl(r, tx));
        use tokio::io::AsyncWriteExt;
        w.write_all(b"{bad}\n{\"type\":\"get_state\",\"id\":\"ok\"}\n")
            .await
            .unwrap();
        drop(w);
        let JsonlFrame::Line(first) = rx.recv().await.unwrap() else {
            panic!()
        };
        assert!(parse_input(&first).is_err());
        let JsonlFrame::Line(second) = rx.recv().await.unwrap() else {
            panic!()
        };
        let Ok(RpcInput::Command { command: c, .. }) = parse_input(&second) else {
            panic!()
        };
        assert_eq!(c.id().as_deref(), Some("ok"));
        task.await.unwrap().unwrap();
    }
    #[tokio::test]
    async fn malformed_after_todo_frame_recovers_with_id() {
        let (mut w, r) = tokio::io::duplex(512);
        let (tx, mut rx) = mpsc::channel(4);
        let task = tokio::spawn(read_jsonl(r, tx));
        use tokio::io::AsyncWriteExt;
        w.write_all(b"{\"type\":\"set_todos\",\"id\":\"t1\",\"phases\":[]}\n{\"type\":\"bogus\",\"id\":\"x1\"}\n").await.unwrap();
        drop(w);
        let JsonlFrame::Line(first) = rx.recv().await.unwrap() else {
            panic!()
        };
        let Ok(RpcInput::Command {
            command: RpcCommand::SetTodos { phases, .. },
            ..
        }) = parse_input(&first)
        else {
            panic!("first frame must parse to SetTodos")
        };
        assert!(phases.is_empty());
        let JsonlFrame::Line(second) = rx.recv().await.unwrap() else {
            panic!()
        };
        let Err(resp) = parse_input(&second) else {
            panic!("malformed frame must error")
        };
        assert_eq!(resp.id.as_deref(), Some("x1"));
        assert_eq!(resp.command, "bogus");
        assert!(!resp.success);
        task.await.unwrap().unwrap();
    }
    #[tokio::test]
    async fn oversized_frame_recovers_at_next_lf() {
        let (mut w, r) = tokio::io::duplex(8192);
        let (tx, mut rx) = mpsc::channel(4);
        let task = tokio::spawn(read_jsonl(r, tx));
        use tokio::io::AsyncWriteExt;
        w.write_all(&vec![b'x'; MAX_RPC_MESSAGE_BYTES + 1])
            .await
            .unwrap();
        w.write_all(b"\n{\"type\":\"get_state\",\"id\":\"after-limit\"}\n")
            .await
            .unwrap();
        drop(w);
        assert!(matches!(rx.recv().await, Some(JsonlFrame::Oversized)));
        let Some(JsonlFrame::Line(valid)) = rx.recv().await else {
            panic!()
        };
        let Ok(RpcInput::Command { command, .. }) = parse_input(&valid) else {
            panic!()
        };
        assert_eq!(command.id().as_deref(), Some("after-limit"));
        task.await.unwrap().unwrap();
    }
    #[tokio::test]
    async fn unterminated_final_frame_is_rejected() {
        let (mut w, r) = tokio::io::duplex(256);
        let (tx, mut rx) = mpsc::channel(2);
        let task = tokio::spawn(read_jsonl(r, tx));
        use tokio::io::AsyncWriteExt;
        w.write_all(b"{\"type\":\"get_state\"}").await.unwrap();
        drop(w);
        assert!(matches!(rx.recv().await, Some(JsonlFrame::Unterminated)));
        task.await.unwrap().unwrap();
    }
    #[test]
    fn todo_events_serialize_with_expected_type() {
        use pi_coding::{TodoCompletionTransition, TodoItem, TodoPhase, TodoStatus};
        let phases = vec![TodoPhase {
            name: "Plan".into(),
            tasks: vec![TodoItem {
                id: "task-x".into(),
                content: "x".into(),
                status: TodoStatus::InProgress,
                depends_on: Vec::new(),
                ready: true,
                waiting_on: Vec::new(),
                agent: None,
            }],
        }];
        let updated = pi_coding::ApplicationEvent::TodoUpdated {
            phases: phases.clone(),
            completed_tasks: vec![TodoCompletionTransition {
                phase: "Plan".into(),
                content: "x".into(),
            }],
            execution_status: pi_coding::TodoDagExecutionStatus::Failed,
            failure_reason: Some(pi_coding::TodoDagFailureReason::Reconcile {
                message: "retry DAG execution".into(),
            }),
        };
        let v = project_application_event(updated).expect("project todo update");
        assert_eq!(v["type"], "todo_updated");
        assert_eq!(v["phases"][0]["name"], "Plan");
        assert_eq!(v["phases"][0]["tasks"][0]["status"], "in_progress");
        assert_eq!(v["phases"][0]["tasks"][0]["id"], "task-x");
        assert_eq!(v["phases"][0]["tasks"][0]["dependsOn"], json!([]));
        assert_eq!(v["phases"][0]["tasks"][0]["ready"], true);
        assert_eq!(v["phases"][0]["tasks"][0]["waitingOn"], json!([]));
        assert_eq!(v["completed_tasks"][0]["phase"], "Plan");
        assert_eq!(v["executionStatus"], "failed");
        assert_eq!(v["failureReason"]["kind"], "reconcile");
        assert_eq!(v["failureReason"]["message"], "retry DAG execution");
        let reminder = pi_coding::ApplicationEvent::TodoReminder {
            phases,
            execution_status: pi_coding::TodoDagExecutionStatus::Dormant,
            failure_reason: None,
        };
        let v = project_application_event(reminder).expect("project todo reminder");
        assert_eq!(v["type"], "todo_reminder");
        assert_eq!(v["phases"][0]["name"], "Plan");
    }
    async fn build_todo_app(model_id: &str, api: &str) -> Application {
        use pi_ai::providers::{FauxProviderOptions, FauxResponse, register_faux_provider};

        let mut model = Model::default();
        model.id = model_id.into();
        model.name = model_id.into();
        model.api = api.into();
        model.provider = "faux".into();
        model.base_url = "http://localhost:0".into();
        let registration = register_faux_provider(FauxProviderOptions {
            api: api.into(),
            provider: "faux".into(),
            models: vec![model.clone()],
            chunk_size: 4,
        });
        registration.set_responses(vec![FauxResponse::text("ok")]);
        let cwd = tempfile::tempdir().expect("tempdir");
        let session = disabled_session(pi_coding::SessionOptions {
            model,
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "faux".into(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        })
        .expect("session");
        let application = isolated_application(session).await;
        registration.unregister();
        application
    }
    async fn build_compact_app(
        model_id: &str,
        responses: Vec<pi_ai::providers::FauxResponse>,
    ) -> Application {
        let mut model = faux_model();
        model.id = model_id.into();
        model.name = model_id.into();
        let responses = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from(
            responses,
        )));
        let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context, _options| {
            let responses = responses.clone();
            Box::pin(async move {
                let response = responses
                    .lock()
                    .expect("response queue")
                    .pop_front()
                    .expect("queued response");
                let mut message = pi_ai::AssistantMessage::pending(&model);
                message.content = response.content;
                message.stop_reason = response.stop_reason;
                message.error_message = response.error_message;
                let stream = pi_ai::new_assistant_message_event_stream();
                let event = if message.stop_reason == pi_ai::StopReason::Error {
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
                stream.push(event).await;
                stream.end(Some(message)).await;
                stream
            })
        });
        let cwd = tempfile::tempdir().expect("tempdir");
        let session = disabled_session(pi_coding::SessionOptions {

            model,
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "faux".into(),
            compaction: Some(pi_coding::CompactionSettings {
                enabled: true,
                reserve_tokens: 20,
                keep_recent_tokens: 4,
                snap_keep_turns: 10,
            }),
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(stream_fn),
            auth_resolver: None,
            auth_manager: None,
        })
        .expect("session");
        let mut assistant = pi_ai::AssistantMessage::pending(&session.model().expect("model"));
        assistant.content = vec![ContentBlock::text("old answer")];
        assistant.stop_reason = pi_ai::StopReason::Stop;
        assistant.timestamp = 2;
        let padding = "old request ".repeat(1_000);
        let mut history = Vec::new();
        for turn in 0..4 {
            history.push(Message::user_text(
                format!("{turn}: {padding}"),
                turn * 2 + 1,
            ));
            let mut prior = assistant.clone();
            prior.timestamp = turn * 2 + 2;
            history.push(Message::Assistant(prior));
        }
        history.push(Message::user_text("recent request", 10));
        session.load_history(history).await.expect("load history");
        isolated_application(session).await
    }
    #[test]
    fn fork_message_response_enforces_full_wire_bound() {
        let normal = bounded_fork_messages_response(
            Some("request-1".into()),
            "get_fork_messages",
            Some("session-1".into()),
            Some(json!({
                "messages": [{ "entryId": "entry-1", "text": "preview" }],
                "truncated": false,
                "total": 1,
            })),
        );
        assert!(normal.success);
        assert!(serde_json::to_vec(&normal).expect("normal response").len() <= MAX_FORK_MESSAGES_RESPONSE_BYTES);

        let oversized = bounded_fork_messages_response(
            Some("request-2".into()),
            "get_fork_messages",
            Some("session-2".into()),
            Some(json!({ "messages": [{ "entryId": "entry-2", "text": "x".repeat(MAX_FORK_MESSAGES_RESPONSE_BYTES) }] })),
        );
        assert!(!oversized.success);
        assert_eq!(oversized.id.as_deref(), Some("request-2"));
        assert_eq!(oversized.session_id.as_deref(), Some("session-2"));
        assert!(serde_json::to_vec(&oversized).expect("bounded failure").len() <= MAX_FORK_MESSAGES_RESPONSE_BYTES);
    }

    #[tokio::test]
    async fn compact_rpc_uses_application_path_and_returns_sanitized_result() {
        let app = build_compact_app(
            "faux-rpc-compact",
            vec![pi_ai::providers::FauxResponse::text("rpc checkpoint")],
        )
        .await;
        let response = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::Compact {
                id: Some("compact-1".into()),
                custom_instructions: Some("preserve decisions".into()),
            },
        )
        .await;

        assert!(response.success, "compact failed: {:?}", response.error);
        assert_eq!(response.id.as_deref(), Some("compact-1"));
        assert_eq!(response.command, "compact");
        assert_eq!(response.error, None);
        let data = response.data.expect("compaction result");
        assert!(
            data["summary"]
                .as_str()
                .is_some_and(|summary| summary.ends_with("rpc checkpoint"))
        );
        assert!(data.get("apiKey").is_none());
        assert!(matches!(
            app.messages().first(),
            Some(Message::CompactionSummary(_))
        ));

        app.cleanup().await;
    }

    #[tokio::test]
    async fn compact_rpc_reports_actionable_error_without_leaking_result_data() {
        let app = build_compact_app(
            "faux-rpc-compact-error",
            vec![pi_ai::providers::FauxResponse::error("summary rejected")],
        )
        .await;
        let response = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::Compact {
                id: Some("compact-error".into()),
                custom_instructions: None,
            },
        )
        .await;

        assert!(!response.success);
        assert_eq!(response.id.as_deref(), Some("compact-error"));
        assert_eq!(response.command, "compact");
        assert!(response.data.is_none());
        assert!(
            response
                .error
                .as_deref()
                .is_some_and(|error| error.contains("summary rejected"))
        );

        app.cleanup().await;
    }

    struct LifecycleExtensionHost;

    struct LifecycleExtensionTransport {
        frames: StdMutex<std::collections::VecDeque<Option<pi_coding::ExtensionFrame>>>,
        ready: tokio::sync::Notify,
    }

    impl LifecycleExtensionTransport {
        fn new() -> Self {
            Self {
                frames: StdMutex::new(std::collections::VecDeque::new()),
                ready: tokio::sync::Notify::new(),
            }
        }

        fn push(&self, frame: pi_coding::ExtensionFrame) {
            self.frames
                .lock()
                .expect("extension frame queue")
                .push_back(Some(frame));
            self.ready.notify_one();
        }
    }

    impl pi_coding::ExtensionTransport for LifecycleExtensionTransport {
        fn send(
            &self,
            frame: &pi_coding::ExtensionHostFrame,
        ) -> pi_coding::ExtensionFuture<'_, Result<()>> {
            match frame {
                pi_coding::ExtensionHostFrame::Hello { .. } => {
                    self.push(pi_coding::ExtensionFrame::Hello {
                        protocol_version: pi_coding::EXTENSION_PROTOCOL_VERSION,
                        manifest: pi_coding::ExtensionCapabilityManifest {
                            id: "rpc-lifecycle".to_owned(),
                            name: "RPC lifecycle fixture".to_owned(),
                            version: "1.0.0".to_owned(),
                            capabilities: std::collections::BTreeSet::from([
                                pi_coding::ExtensionCapability::EventHooks,
                            ]),
                            ui_capabilities: std::collections::BTreeSet::new(),
                        },
                    });
                }
                pi_coding::ExtensionHostFrame::Request { id, request, .. } => {
                    if matches!(request, pi_coding::ExtensionHostRequest::Load) {
                        for event in ["session_before_switch", "session_before_fork"] {
                            self.push(pi_coding::ExtensionFrame::Register {
                                registration: pi_coding::ExtensionRegistration::EventHook {
                                    hook: pi_coding::ExtensionEventHookDescriptor {
                                        event: event.to_owned(),
                                    },
                                },
                            });
                        }
                    }
                    let value = match request {
                        pi_coding::ExtensionHostRequest::Invoke {
                            invocation: pi_coding::ExtensionInvocation::Event { event },
                            ..
                        } if event.name == "session_before_switch" => json!({"cancel":true}),
                        pi_coding::ExtensionHostRequest::Invoke {
                            invocation: pi_coding::ExtensionInvocation::Event { event },
                            ..
                        } if event.name == "session_before_fork" => json!({"cancel":true}),
                        _ => Value::Null,
                    };
                    self.push(pi_coding::ExtensionFrame::Response {
                        id: id.clone(),
                        result: pi_coding::ProtocolResult::Success { value },
                    });
                }
                pi_coding::ExtensionHostFrame::Shutdown { .. } => {
                    self.frames
                        .lock()
                        .expect("extension frame queue")
                        .push_back(None);
                    self.ready.notify_one();
                }
                pi_coding::ExtensionHostFrame::Response { .. }
                | pi_coding::ExtensionHostFrame::Cancel { .. } => {}
            }
            Box::pin(async { Ok(()) })
        }

        fn receive(
            &self,
        ) -> pi_coding::ExtensionFuture<'_, Result<Option<pi_coding::ExtensionFrame>>> {
            Box::pin(async move {
                loop {
                    let notified = self.ready.notified();
                    if let Some(frame) = self
                        .frames
                        .lock()
                        .expect("extension frame queue")
                        .pop_front()
                    {
                        return Ok(frame);
                    }
                    notified.await;
                }
            })
        }

        fn terminate(&self) -> pi_coding::ExtensionFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }

        fn diagnostic_context(&self) -> String {
            "RPC lifecycle fixture".to_owned()
        }
    }

    impl pi_coding::ExtensionHost for LifecycleExtensionHost {
        fn launch(
            &self,
            _launch: pi_coding::ExtensionLaunch,
        ) -> pi_coding::ExtensionFuture<'_, Result<Arc<dyn pi_coding::ExtensionTransport>>>
        {
            Box::pin(async {
                Ok(Arc::new(LifecycleExtensionTransport::new())
                    as Arc<dyn pi_coding::ExtensionTransport>)
            })
        }
    }

    async fn build_lifecycle_app(cancel: bool) -> (tempfile::TempDir, Application) {
        let root = tempfile::tempdir().expect("lifecycle root");
        let cwd = root.path().join("workspace");
        let agent_dir = root.path().join("agent");
        std::fs::create_dir_all(&cwd).expect("lifecycle cwd");
        std::fs::create_dir_all(&agent_dir).expect("lifecycle agent dir");
        let session = disabled_session(pi_coding::SessionOptions {
            model: faux_model(),
            cwd: cwd.clone(),
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
            auth_manager: None,
        })
        .expect("lifecycle session");
        let recorder = pi_coding::start_session_in(&cwd, None, None, Some(&cwd), None, None)
            .expect("record lifecycle session");
        recorder
            .record_message(&Message::user_text("fork text", 1))
            .expect("record fork message");
        recorder.persist_now().expect("persist lifecycle session");
        session.set_session_dir(cwd.clone());
        session.record(recorder).expect("attach lifecycle recorder");
        if !cancel {
            return (root, isolated_application(session).await);
        }
        let extension_path = cwd.join("fixture-extension");
        std::fs::write(&extension_path, "fixture").expect("lifecycle extension executable");
        let manifest_path = cwd.join(pi_coding::PROCESS_EXTENSION_MANIFEST_FILE);
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&json!({
                "schemaVersion": 1,
                "id": "rpc-lifecycle",
                "runtime": "process",
                "executable": "fixture-extension",
                "capabilities": ["event_hooks"]
            }))
            .expect("lifecycle extension manifest"),
        )
        .expect("write lifecycle extension manifest");
        let mut resource_options = pi_coding::ResourceManagerOptions::new(&cwd);
        resource_options.agent_dir = agent_dir;
        std::fs::write(
            resource_options.agent_dir.join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )
        .expect("isolated sandbox settings");
        resource_options.project_trust_override = Some(true);
        resource_options.disable_skills = true;
        resource_options.disable_prompt_templates = true;
        resource_options.disable_themes = true;
        resource_options.disable_context_files = true;
        resource_options.explicit_extension_paths = vec![manifest_path];
        session
            .attach_resources(pi_coding::ResourceManager::new(resource_options).expect("resources"))
            .await
            .expect("attach resources");
        let permissions = pi_coding::ExtensionPermissionSet {
            capabilities: std::collections::BTreeSet::from([
                pi_coding::ExtensionCapability::EventHooks,
            ]),
            ui_capabilities: std::collections::BTreeSet::new(),
        };
        let runtime = pi_coding::ExtensionRuntime::new(
            Arc::new(LifecycleExtensionHost),
            None,
            pi_coding::ExtensionRuntimeOptions::default(),
        );
        (
            root,
            Application::new_with_extensions(session, runtime, permissions).await,
        )
    }

    #[tokio::test]
    async fn lifecycle_rpc_cancellation_is_successful_and_preserves_session() {
        let (_cwd, app) = build_lifecycle_app(true).await;
        let before = app.state().await;
        let messages = app.messages();
        let entry_id = app
            .fork_messages(None, None)
            .expect("fork messages")
            .messages[0]
            .entry_id
            .clone();
        let switch_path = before.session_file.clone().expect("session file");
        for (command, expected) in [
            (
                RpcCommand::NewSession {
                    id: Some("new".into()),
                    parent_session: None,
                    cwd: None,
                },
                json!({"cancelled":true}),
            ),
            (
                RpcCommand::SwitchSession {
                    id: Some("switch".into()),
                    session_path: switch_path.clone(),
                    catalog_source: None,
                    catalog_session_id: None,
                    catalog_status: None,
                    response_mode: None,
                },
                json!({"cancelled":true}),
            ),
            (
                RpcCommand::Fork {
                    id: Some("fork".into()),
                    entry_id: entry_id.clone(),
                    session_path: None,
                },
                json!({"text":"","cancelled":true}),
            ),
            (
                RpcCommand::Clone {
                    id: Some("clone".into()),
                    session_path: None,
                },
                json!({"cancelled":true}),
            ),
        ] {
            let response =
                handle_command(&app, &settings_state(), &workflows_state(), command).await;
            assert!(response.success, "{response:?}");
            assert_eq!(response.data, Some(expected));
            let after = app.state().await;
            assert_eq!(after.session_id, before.session_id);
            assert_eq!(after.session_file, before.session_file);
            assert_eq!(app.messages(), messages);
        }
        app.cleanup().await;
    }

    #[tokio::test]
    async fn new_and_switch_rpc_return_false_when_not_cancelled() {
        let (_cwd, app) = build_lifecycle_app(false).await;
        let switch_path = app.state().await.session_file.expect("session file");
        let switched = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::SwitchSession {
                id: Some("switch".into()),
                session_path: switch_path,
                catalog_source: None,
                catalog_session_id: None,
                catalog_status: None,
                response_mode: None,
            },
        )
        .await;
        assert!(switched.success, "{switched:?}");
        assert_eq!(switched.data, Some(json!({"cancelled":false})));

        let created = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::NewSession {
                id: Some("new".into()),
                parent_session: None,
                cwd: None,
            },
        )
        .await;
        assert!(created.success, "{created:?}");
        assert_eq!(created.data, Some(json!({"cancelled":false})));

        let empty = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::NewSession {
                id: Some("new-empty".into()),
                parent_session: None,
                cwd: Some("   ".into()),
            },
        )
        .await;
        assert!(empty.success, "{empty:?}");
        app.cleanup().await;
    }

    #[tokio::test]
    async fn standalone_new_session_cwd_fails_closed_without_mutating_source() {
        let (_cwd, app) = build_lifecycle_app(false).await;
        let before = app.state().await;
        let messages = app.messages();
        let target = tempfile::tempdir().expect("target cwd");

        let response = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::NewSession {
                id: Some("new-target".into()),
                parent_session: None,
                cwd: Some(target.path().to_string_lossy().into_owned()),
            },
        )
        .await;
        assert!(!response.success);
        assert!(
            response
                .error
                .as_deref()
                .is_some_and(|error| error.contains("multi-session Web runtime"))
        );
        let after = app.state().await;
        assert_eq!(after.session_id, before.session_id);
        assert_eq!(after.session_file, before.session_file);
        assert_eq!(app.messages(), messages);
        app.cleanup().await;
    }

    #[tokio::test]
    async fn fork_rpc_returns_selected_text_and_false_when_not_cancelled() {
        let (_cwd, app) = build_lifecycle_app(false).await;
        let entry_id = app
            .fork_messages(None, None)
            .expect("fork messages")
            .messages[0]
            .entry_id
            .clone();
        let response = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::Fork {
                id: Some("fork".into()),
                entry_id,
                session_path: None,
            },
        )
        .await;
        assert!(response.success, "{response:?}");
        assert_eq!(
            response.data,
            Some(json!({"text":"fork text","cancelled":false}))
        );
        app.cleanup().await;
    }

    #[tokio::test]
    async fn clone_rpc_clones_recorded_session_branch() {
        let app = build_todo_app("faux-rpc-clone", "faux-rpc-clone-api").await;
        let session_dir = tempfile::tempdir().expect("session dir");
        let recorder = pi_coding::start_session_in(
            session_dir.path(),
            None,
            None,
            Some(session_dir.path()),
            None,
            None,
        )
        .expect("start recorded session");
        recorder
            .record_message(&Message::user_text("clone me", 1))
            .expect("record clone source entry");
        recorder
            .persist_now()
            .expect("persist clone source session");
        app.session()
            .set_session_dir(session_dir.path().to_path_buf());
        app.session().record(recorder).expect("attach recorder");
        let source_file = app.state().await.session_file.expect("source session file");

        let response = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::Clone {
                id: Some("clone-1".into()),
                session_path: None,
            },
        )
        .await;

        assert!(response.success, "clone failed: {:?}", response.error);
        assert_eq!(response.id.as_deref(), Some("clone-1"));
        assert_eq!(response.data, Some(json!({"cancelled":false})));
        let cloned_file = app.state().await.session_file.expect("cloned session file");
        assert_ne!(cloned_file, source_file);
        assert!(app.messages().iter().any(|message| {
            matches!(message, Message::User(user) if user.content.iter().any(|block| matches!(block, ContentBlock::Text { text, .. } if text == "clone me")))
        }));
        app.cleanup().await;
    }
    #[tokio::test]
    async fn session_list_rpc_lists_cwd_scoped_native_sessions_and_round_trips_rename() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session_dir = tempfile::tempdir().expect("session dir");
        let session = disabled_session(pi_coding::SessionOptions {
            model: faux_model(),
            cwd: cwd.path().to_path_buf(),
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
            auth_manager: None,
        })
        .expect("session");
        session.set_session_dir(session_dir.path().to_path_buf());
        let recorder = pi_coding::start_session_in(
            cwd.path(),
            None,
            None,
            Some(session_dir.path()),
            None,
            None,
        )
        .expect("record session");
        recorder
            .record_message(&Message::user_text("hello session list", 1))
            .expect("record message");
        recorder.persist_now().expect("persist session");
        session.record(recorder).expect("attach recorder");
        let app = isolated_application(session).await;

        // A second recorder under a different cwd must be excluded by the
        // catalog's cwd scope (mirrors the TUI `/sessions` panel).
        let other_cwd = tempfile::tempdir().expect("other cwd");
        let other_session_dir = tempfile::tempdir().expect("other session dir");
        let other = pi_coding::start_session_in(
            other_cwd.path(),
            None,
            None,
            Some(other_session_dir.path()),
            None,
            None,
        )
        .expect("other recorder");
        other
            .record_message(&Message::user_text("other cwd", 1))
            .expect("record other");
        other.persist_now().expect("persist other");

        let state = app.state().await;
        let session_id = state.session_id.expect("session id");
        let response = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::SessionList {
                id: Some("list".into()),
                scope: RpcSessionListScope::Current,
            },
        )
        .await;
        assert!(response.success, "{response:?}");
        assert_eq!(response.id.as_deref(), Some("list"));
        assert_eq!(response.command, "session_list");
        let data = response.data.expect("session list data");
        let sessions = data["sessions"].as_array().expect("sessions array");
        let row = sessions
            .iter()
            .find(|row| row["sessionId"].as_str() == Some(session_id.as_str()))
            .expect("current session row");
        assert_eq!(row["source"], "rpi");
        assert_eq!(row["status"], "native");
        assert_eq!(row["cwd"], cwd.path().to_string_lossy().as_ref());
        assert_eq!(row["messageCount"], 1);
        assert!(row["summary"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(row["modifiedEpoch"].is_f64());
        assert!(row["displayTime"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(
            row["path"].as_str().is_some_and(|p| p.ends_with(".jsonl")),
            "{row:?}"
        );
        assert!(row["name"].is_null());
        assert!(
            !sessions
                .iter()
                .any(|row| row["cwd"] == other_cwd.path().to_string_lossy().as_ref()),
            "other-cwd session must be invisible: {sessions:?}"
        );

        // Rename round-trips into the catalog so the web panel can re-list.
        let renamed = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::SetSessionName {
                id: Some("rename".into()),
                name: "web demo".into(),
                session_path: None,
            },
        )
        .await;
        assert!(renamed.success, "{renamed:?}");
        let relisted = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::SessionList {
                id: Some("list2".into()),
                scope: RpcSessionListScope::Current,
            },
        )
        .await;
        assert!(relisted.success, "{relisted:?}");
        let sessions = relisted.data.expect("session list")["sessions"]
            .as_array()
            .expect("sessions array")
            .clone();
        let renamed_row = sessions
            .iter()
            .find(|row| row["sessionId"].as_str() == Some(session_id.as_str()))
            .expect("renamed row");
        assert_eq!(renamed_row["name"], "web demo");
        app.cleanup().await;
    }

    #[tokio::test]
    async fn session_list_all_projects_uses_typed_default_tree_storage() {
        // Mirror the production default tree (`<agent>/sessions`): the typed
        // DefaultTree catalog maps the native root to the native agent dir by
        // stripping the trailing `sessions` component, so the fixture must use
        // the production-shaped root rather than a bare directory.
        let agent = tempfile::tempdir().expect("agent dir");
        let root = agent.path().join("sessions");
        let project_a = tempfile::tempdir().expect("project A");
        let project_b = tempfile::tempdir().expect("project B");
        let storage = crate::session_run::SessionStorage::DefaultTree { native_root: root };
        let dir_a = storage.session_dir_for(project_a.path()).expect("A dir");
        let dir_b = storage.session_dir_for(project_b.path()).expect("B dir");
        for (cwd, dir, message) in [
            (project_a.path(), dir_a.as_path(), "project A"),
            (project_b.path(), dir_b.as_path(), "project B"),
        ] {
            let recorder = pi_coding::start_session_in(cwd, None, None, Some(dir), None, None)
                .expect("record session");
            recorder
                .record_message(&Message::user_text(message, 1))
                .expect("message");
            recorder.persist_now().expect("persist");
        }
        let session = disabled_session(pi_coding::SessionOptions {
            model: faux_model(),
            cwd: project_a.path().to_path_buf(),
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
            auth_manager: None,
        })
        .expect("session");
        session.set_session_dir(dir_a);
        let app = isolated_application(session).await;
        let response = handle_command_with_session_storage(
            &app,
            &settings_state(),
            &workflows_state(),
            Some(&storage),
            RpcCommand::SessionList {
                id: None,
                scope: RpcSessionListScope::AllProjects,
            },
        )
        .await;
        assert!(response.success, "{response:?}");
        let rows = response.data.expect("catalog")["sessions"]
            .as_array()
            .expect("rows")
            .clone();
        // Both recorded sessions are unnamed, tiny native Pi sessions in temp
        // workspaces: the AllProjects pipeline keeps them (they carry real
        // messages) and flags them `temporary` — never backend-deleted. (The
        // shared catalog may also carry the ambient HOME's rows, so assert
        // presence of the two seeded rows, not the total count.)
        let row_a = rows
            .iter()
            .find(|row| row["cwd"] == project_a.path().to_string_lossy().as_ref())
            .unwrap_or_else(|| panic!("project A row missing: {rows:?}"));
        let row_b = rows
            .iter()
            .find(|row| row["cwd"] == project_b.path().to_string_lossy().as_ref())
            .unwrap_or_else(|| panic!("project B row missing: {rows:?}"));
        assert_eq!(
            row_a["temporary"], true,
            "temp-workspace row must be marked temporary: {row_a}"
        );
        assert_eq!(
            row_b["temporary"], true,
            "temp-workspace row must be marked temporary: {row_b}"
        );
        app.cleanup().await;
    }

    #[tokio::test]
    async fn session_list_all_projects_loads_deep_omp_record_on_spawn_blocking_thread() {
        // Unique RPC id keys this test's own off-worker seam entry, so a
        // concurrent SessionList test (which records under its own id) can
        // never overwrite or mask this test's record — the seam is
        // per-invocation, not global last-writer.
        const SEAM_KEY: &str = "off-worker-deep-omp";
        SESSION_LIST_LOADER_THREADS
            .lock()
            .expect("loader thread seam")
            .remove(SEAM_KEY);
        let home = tempfile::tempdir().expect("hermetic catalog home");
        let catalog = pi_coding::SessionCatalog::new(home.path());
        let omp_root = catalog.root_for(pi_coding::SessionSourceKind::Omp).path;
        std::fs::create_dir_all(omp_root.join("--rpc--")).expect("OMP project dir");
        // Depth 120 stays under serde_json's default recursion limit (128) yet
        // exercises a deeply-nested diagnostic through the real production
        // catalog parse path. The fixture must load, not crash.
        let nested = format!("{}null{}", "[".repeat(120), "]".repeat(120));
        let omp_path = omp_root.join("--rpc--").join("deep-session.jsonl");
        std::fs::write(
            &omp_path,
            format!(
                concat!(
                    r#"{{"type":"session","version":3,"id":"rpc-deep-omp","timestamp":"2026-01-01T00:00:00Z","cwd":"{cwd}"}}"#,
                    "\n",
                    r#"{{"type":"message","id":"u","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{{"role":"user","content":"deep rpc prompt"}},"diagnostic":{nested}}}"#,
                    "\n"
                ),
                cwd = home.path().join("workspace").display(),
                nested = nested,
            ),
        )
        .expect("deep OMP fixture");
        let storage = crate::session_run::SessionStorage::Hermetic {
            sessions_home: home.path().to_path_buf(),
        };
        let cwd = tempfile::tempdir().expect("app cwd");
        let session = disabled_session(pi_coding::SessionOptions {
            model: faux_model(),
            cwd: cwd.path().to_path_buf(),
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
            auth_manager: None,
        })
        .expect("session");
        let app = isolated_application(session).await;

        // Under the default current-thread `#[tokio::test]` runtime, the test
        // body, the `.await` on `handle_command_with_session_storage`, and the
        // call into `spawn_blocking` all run on this same OS thread — the
        // async worker. `spawn_blocking` dispatches the closure onto a Tokio
        // blocking-pool thread, which is always a distinct `ThreadId`. So
        // capturing the worker id here and comparing it against the seam
        // deterministically distinguishes `spawn_blocking` from a synchronous
        // inline call (which would record this same thread id). No sleep, no
        // timing: the join on `spawn_blocking` happens-before the read below.
        let async_worker = std::thread::current().id();

        let response = handle_command_with_session_storage(
            &app,
            &settings_state(),
            &workflows_state(),
            Some(&storage),
            RpcCommand::SessionList {
                id: Some(SEAM_KEY.to_owned()),
                scope: RpcSessionListScope::AllProjects,
            },
        )
        .await;
        assert!(response.success, "{response:?}");
        let rows = response.data.expect("catalog")["sessions"]
            .as_array()
            .expect("rows")
            .clone();
        let row = rows
            .iter()
            .find(|row| row["sessionId"] == "rpc-deep-omp")
            .expect("deep OMP session row");
        assert_eq!(row["summary"], "deep rpc prompt");
        assert_eq!(row["messageCount"], 1);

        // Off-worker contract: `load_resume_catalog` must have executed on a
        // `spawn_blocking` thread, never on the async worker driving this test.
        // Read under this test's unique `SEAM_KEY` so a concurrent SessionList
        // test cannot overwrite the entry. Fails if `spawn_blocking` is
        // replaced by a synchronous inline call (loader == async_worker), and
        // fails if the seam write is removed entirely (no entry under the key
        // after the `remove` above).
        let loader = match SESSION_LIST_LOADER_THREADS
            .lock()
            .expect("loader thread seam")
            .get(SEAM_KEY)
            .copied()
        {
            Some(id) => id,
            None => {
                panic!("load_resume_catalog did not record its loader thread under `{SEAM_KEY}`")
            }
        };
        assert_ne!(
            loader, async_worker,
            "load_resume_catalog must run off the async worker via spawn_blocking, \
             not inline on the Tokio worker thread"
        );
        app.cleanup().await;
    }

    #[tokio::test]
    async fn session_list_worker_panic_is_not_reported_as_cancellation() {
        let home = tempfile::tempdir().expect("hermetic catalog home");
        let storage = crate::session_run::SessionStorage::Hermetic {
            sessions_home: home.path().to_path_buf(),
        };
        let cwd = tempfile::tempdir().expect("app cwd");
        let session = disabled_session(pi_coding::SessionOptions {
            model: faux_model(),
            cwd: cwd.path().to_path_buf(),
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
            auth_manager: None,
        })
        .expect("session");
        let app = isolated_application(session).await;

        let response = handle_command_with_session_storage(
            &app,
            &settings_state(),
            &workflows_state(),
            Some(&storage),
            RpcCommand::SessionList {
                id: Some(SESSION_LIST_PANIC_SEAM_KEY.to_owned()),
                scope: RpcSessionListScope::AllProjects,
            },
        )
        .await;

        assert!(!response.success, "worker panic must fail the request");
        assert_eq!(
            response.error.as_deref(),
            Some("session catalog scan worker panicked")
        );
        app.cleanup().await;
    }

    #[tokio::test]
    async fn goal_rpc_mutations_and_malformed_recovery_share_application_state() {
        let app = build_todo_app("faux-rpc-goal", "faux-rpc-goal-api").await;
        let settings = settings_state();
        let malformed = parse_input(br#"{"type":"goal_create","objective":12}"#)
            .expect_err("malformed goal command");
        assert!(!malformed.success);
        assert_eq!(malformed.command, "goal_create");

        let created = handle_command(
            &app,
            &settings,
            &workflows_state(),
            RpcCommand::GoalCreate {
                id: Some("create".into()),
                objective: "ship safely".into(),
                token_budget: Some(10),
                activate: false,
            },
        )
        .await;
        assert!(created.success, "{created:?}");
        assert_eq!(created.data.expect("goal")["objective"], "ship safely");

        let charged = handle_command(
            &app,
            &settings,
            &workflows_state(),
            RpcCommand::GoalUpdateUsage {
                id: Some("usage".into()),
                tokens: 10,
                active_time_seconds: 2,
            },
        )
        .await;
        assert!(charged.success, "{charged:?}");
        let state = handle_command(
            &app,
            &settings,
            &workflows_state(),
            RpcCommand::GoalGet {
                id: Some("get".into()),
            },
        )
        .await;
        let data = state.data.expect("goal state");
        assert_eq!(data["current"]["lifecycle"], "paused");
        assert_eq!(data["current"]["pauseReason"], "budget_exhausted");

        let after_malformed = handle_command(
            &app,
            &settings,
            &workflows_state(),
            RpcCommand::GetState {
                id: Some("state".into()),
            },
        )
        .await;
        assert!(after_malformed.success);
        assert_eq!(
            after_malformed.data.expect("application state")["goal"]["current"]["usage"]["tokensUsed"],
            10
        );
    }

    #[tokio::test]
    async fn goal_activate_flag_mirrors_tui_create_and_resume_while_default_stays_mutation_only() {
        let settings = settings_state();

        // Wire-level parse: `activate` is an OPTIONAL field; absent (the Web
        // Goal panel contract) and present (the composer /goal create parity)
        // frames both parse.
        let command = match parse_input(br#"{"type":"goal_create","objective":"ship with work","tokenBudget":10,"activate":true}"#)
            .expect("activate:true goal_create frame must parse")
        {
            RpcInput::Command { command, .. } => command,
            RpcInput::ExtensionUiResponse(_) => panic!("expected goal_create command"),
        };
        assert!(matches!(
            command,
            RpcCommand::GoalCreate { activate: true, .. }
        ));

        // Default (activate absent/false) keeps the mutation-only contract:
        // the response is the GOAL wire, not an outcome.
        let app = build_todo_app("faux-rpc-goal-plain", "faux-rpc-goal-plain-api").await;
        let plain = handle_command(
            &app,
            &settings,
            &workflows_state(),
            RpcCommand::GoalCreate {
                id: Some("plain".into()),
                objective: "mutation only".into(),
                token_budget: None,
                activate: false,
            },
        )
        .await;
        assert!(plain.success, "{plain:?}");
        let goal = plain.data.expect("goal");
        assert_eq!(goal["objective"], "mutation only");
        assert!(
            goal.get("id").is_some(),
            "mutation-only create resolves with the Goal wire"
        );
        app.cleanup().await;

        // activate:true mirrors TUI /goal create — create + start work.
        let app = build_todo_app("faux-rpc-goal-activate", "faux-rpc-goal-activate-api").await;
        let created = handle_command(&app, &settings, &workflows_state(), command).await;
        assert!(created.success, "{created:?}");
        let outcome = created.data.expect("activation outcome");
        assert!(
            outcome == "started" || outcome == "queued",
            "unexpected activation outcome: {outcome:?}"
        );
        let state = handle_command(
            &app,
            &settings,
            &workflows_state(),
            RpcCommand::GoalGet {
                id: Some("get".into()),
            },
        )
        .await;
        let data = state.data.expect("goal state");
        assert_eq!(data["current"]["objective"], "ship with work");
        assert_eq!(data["current"]["tokenBudget"], 10);
        assert_eq!(data["current"]["lifecycle"], "active");

        // activate:true resume mirrors TUI /goal resume — resume + one turn.
        assert!(
            handle_command(
                &app,
                &settings,
                &workflows_state(),
                RpcCommand::GoalPause {
                    id: Some("pause".into()),
                },
            )
            .await
            .success
        );
        let resumed = handle_command(
            &app,
            &settings,
            &workflows_state(),
            RpcCommand::GoalResume {
                id: Some("resume".into()),
                activate: true,
            },
        )
        .await;
        assert!(resumed.success, "{resumed:?}");
        let outcome = resumed.data.expect("activation outcome");
        assert!(
            outcome == "started" || outcome == "queued",
            "unexpected activation outcome: {outcome:?}"
        );
        app.cleanup().await;
    }

    #[tokio::test]
    async fn goal_pin_unpin_and_journal_rpc_replay_recorder_backed_session() {
        let cwd = tempfile::tempdir().expect("goal journal cwd");
        let session = disabled_session(pi_coding::SessionOptions {
            model: faux_model(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "test".into(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        })
        .expect("goal journal session");
        let recorder = pi_coding::start_session_in(
            cwd.path(),
            None,
            None,
            Some(cwd.path()),
            Some("goal-rpc-journal"),
            None,
        )
        .expect("record goal journal session");
        session.set_session_dir(cwd.path().to_path_buf());
        session.record(recorder).expect("attach recorder");
        let app = isolated_application(session).await;
        let settings = settings_state();

        let created = handle_command(
            &app,
            &settings,
            &workflows_state(),
            RpcCommand::GoalCreate {
                id: Some("create".into()),
                objective: "ship cleanly".into(),
                token_budget: Some(20),
                activate: false,
            },
        )
        .await;
        assert!(created.success, "{created:?}");

        let pinned = handle_command(
            &app,
            &settings,
            &workflows_state(),
            RpcCommand::GoalPin {
                id: Some("pin".into()),
                text: "stay calm".into(),
            },
        )
        .await;
        assert!(pinned.success, "{pinned:?}");
        assert_eq!(pinned.data.expect("goal")["pins"][0], "stay calm");

        let malformed =
            parse_input(br#"{"type":"goal_pin","text":7}"#).expect_err("malformed goal_pin");
        assert!(!malformed.success);
        assert_eq!(malformed.command, "goal_pin");

        let pinned_again = handle_command(
            &app,
            &settings,
            &workflows_state(),
            RpcCommand::GoalPin {
                id: Some("pin2".into()),
                text: "double-check".into(),
            },
        )
        .await;
        assert!(pinned_again.success, "{pinned_again:?}");

        let unpinned = handle_command(
            &app,
            &settings,
            &workflows_state(),
            RpcCommand::GoalUnpin {
                id: Some("unpin".into()),
                index: 0,
            },
        )
        .await;
        assert!(unpinned.success, "{unpinned:?}");
        assert_eq!(
            unpinned.data.expect("goal")["pins"],
            json!(["double-check"])
        );

        let bad_unpin = handle_command(
            &app,
            &settings,
            &workflows_state(),
            RpcCommand::GoalUnpin {
                id: Some("bad-unpin".into()),
                index: 9,
            },
        )
        .await;
        assert!(!bad_unpin.success, "out-of-range unpin must fail cleanly");

        let paused = handle_command(
            &app,
            &settings,
            &workflows_state(),
            RpcCommand::GoalPause {
                id: Some("pause".into()),
            },
        )
        .await;
        assert!(paused.success, "{paused:?}");
        assert_eq!(paused.data.expect("goal")["lifecycle"], "paused");

        let journal = handle_command(
            &app,
            &settings,
            &workflows_state(),
            RpcCommand::GoalJournal {
                id: Some("journal".into()),
            },
        )
        .await;
        assert!(journal.success, "{journal:?}");
        let entries = journal.data.expect("goal journal");
        let kinds: Vec<&str> = entries
            .as_array()
            .expect("journal array")
            .iter()
            .map(|entry| entry["kind"]["type"].as_str().expect("kind type"))
            .collect();
        assert_eq!(
            kinds,
            [
                "created",
                "pins_updated",
                "pins_updated",
                "pins_updated",
                "paused"
            ]
        );
        assert_eq!(entries[1]["kind"]["pins"], json!(["stay calm"]));
        assert_eq!(
            entries[2]["kind"]["pins"],
            json!(["stay calm", "double-check"])
        );
        assert_eq!(entries[3]["kind"]["pins"], json!(["double-check"]));
        assert_eq!(entries[4]["kind"]["reason"], "manual");
        // Every journal entry carries the resulting goal snapshot.
        assert_eq!(entries[4]["goal"]["objective"], "ship cleanly");
        assert_eq!(entries[4]["goal"]["usage"]["tokensUsed"], 0);
        assert_eq!(entries[4]["goal"]["pins"], json!(["double-check"]));

        let state = handle_command(
            &app,
            &settings,
            &workflows_state(),
            RpcCommand::GetState {
                id: Some("state".into()),
            },
        )
        .await;
        assert!(state.success);
        let state_data = state.data.as_ref().expect("state");
        assert_eq!(state_data["goal"]["current"]["lifecycle"], "paused");
        assert_eq!(
            state_data["goal"]["current"]["pins"],
            json!(["double-check"])
        );

        app.cleanup().await;
    }

    async fn build_settings_app() -> (tempfile::TempDir, Application) {
        let agent = tempfile::tempdir().expect("agent");
        std::fs::write(
            agent.path().join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )
        .expect("isolated sandbox settings");
        let cwd = tempfile::tempdir().expect("cwd");
        let mut resource_options = pi_coding::ResourceManagerOptions::new(cwd.path());
        resource_options.agent_dir = agent.path().to_path_buf();
        resource_options.project_trust_override = Some(true);
        resource_options.disable_extensions = true;
        resource_options.disable_skills = true;
        resource_options.disable_prompt_templates = true;
        resource_options.disable_themes = true;
        resource_options.disable_context_files = true;
        let resources = pi_coding::ResourceManager::new(resource_options).expect("resources");
        let session = disabled_session(pi_coding::SessionOptions {
            model: faux_model(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "test".into(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        })
        .expect("session");
        session
            .attach_resources(resources)
            .await
            .expect("attach resources");
        let session_dir = cwd.path().join("sessions");
        std::fs::create_dir_all(&session_dir).expect("settings session dir");
        session.set_session_dir(session_dir);
        (agent, Application::new(session).await)
    }

    #[tokio::test]
    async fn settings_rpc_open_set_validate_apply_and_malformed_recovery() {
        let (agent, app) = build_settings_app().await;
        let settings = settings_state();
        let opened = handle_command(
            &app,
            &settings,
            &workflows_state(),
            RpcCommand::SettingsOpenDraft {
                id: Some("open".into()),
                scope: pi_coding::SettingsScope::Global,
            },
        )
        .await;
        assert!(opened.success);
        let draft_id = opened.data.expect("draft data")["draftId"]
            .as_str()
            .expect("draft id")
            .to_owned();
        let invalid = handle_command(
            &app,
            &settings,
            &workflows_state(),
            RpcCommand::SettingsSet {
                id: Some("invalid".into()),
                draft_id: draft_id.clone(),
                key: "compaction.enabled".into(),
                value: json!("false"),
            },
        )
        .await;
        assert!(!invalid.success);
        let valid = handle_command(
            &app,
            &settings,
            &workflows_state(),
            RpcCommand::SettingsSet {
                id: Some("set".into()),
                draft_id: draft_id.clone(),
                key: "compaction.enabled".into(),
                value: json!(false),
            },
        )
        .await;
        assert!(valid.success);
        assert!(
            handle_command(
                &app,
                &settings,
                &workflows_state(),
                RpcCommand::SettingsValidate {
                    id: Some("validate".into()),
                    draft_id: draft_id.clone(),
                },
            )
            .await
            .success
        );
        let applied = handle_command(
            &app,
            &settings,
            &workflows_state(),
            RpcCommand::SettingsApply {
                id: Some("apply".into()),
                draft_id,
            },
        )
        .await;
        assert!(applied.success);
        assert_eq!(applied.data.expect("outcome")["appliedLive"], true);
        let saved: Value = serde_json::from_slice(
            &std::fs::read(agent.path().join("settings.json")).expect("settings"),
        )
        .expect("json");
        assert_eq!(saved["compaction"]["enabled"], false);

        let Err(malformed) = parse_input(
            br#"{"type":"settings_set","id":"bad","draftId":7,"key":"theme","value":"x"}"#,
        ) else {
            panic!("malformed settings command must fail")
        };
        assert_eq!(malformed.id.as_deref(), Some("bad"));
        assert_eq!(malformed.command, "settings_set");
        assert!(matches!(
            parse_input(br#"{"type":"settings_inspect","id":"after"}"#),
            Ok(RpcInput::Command { command: RpcCommand::SettingsInspect { id }, .. }) if id.as_deref() == Some("after")
        ));
    }

    #[tokio::test]
    async fn settings_rpc_exposes_exact_sandbox_schema() {
        let (_agent, app) = build_settings_app().await;
        let response = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::SettingsInspect {
                id: Some("sandbox-schema".into()),
            },
        )
        .await;
        assert!(response.success, "{response:?}");
        let data = response.data.expect("catalog");
        let keys = data["values"]
            .as_array()
            .expect("values")
            .iter()
            .filter_map(|value| value["definition"]["key"].as_str())
            .collect::<Vec<_>>();
        assert!(keys.contains(&"sandbox.mode"));
        assert!(keys.contains(&"sandbox.hostHttp"));
        assert!(!keys.contains(&"sandbox.enabled"));
        assert!(!keys.contains(&"orchestration.sandboxed"));
    }

    #[tokio::test]
    async fn set_todos_round_trips_through_application() {
        use pi_coding::{TodoItem, TodoPhase, TodoStatus};
        let app = build_todo_app("faux-rpc-todo-set", "faux-rpc-todo-set-api").await;
        let phases = vec![TodoPhase {
            name: "Plan".into(),
            tasks: vec![
                TodoItem {
                    id: "task-root".into(),
                    content: "root".into(),
                    status: TodoStatus::InProgress,
                    depends_on: Vec::new(),
                    ready: true,
                    waiting_on: Vec::new(),
                    agent: None,
                },
                TodoItem {
                    id: "task-do".into(),
                    content: "do".into(),
                    status: TodoStatus::Pending,
                    depends_on: vec!["task-root".into()],
                    ready: false,
                    waiting_on: Vec::new(),
                    agent: None,
                },
            ],
        }];
        let r = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::SetTodos {
                id: Some("t1".into()),
                workflow_id: None,
                phases,
            },
        )
        .await;
        assert!(r.success);
        assert_eq!(r.id.as_deref(), Some("t1"));
        assert_eq!(r.command, "set_todos");
        let d = r.data.expect("set_todos returns data");
        assert_eq!(d["phases"][0]["name"], "Plan");
        assert_eq!(d["phases"][0]["tasks"][1]["content"], "do");
        assert_eq!(d["phases"][0]["tasks"][1]["status"], "pending");
        assert_eq!(d["phases"][0]["tasks"][1]["id"], "task-do");
        assert_eq!(
            d["phases"][0]["tasks"][1]["dependsOn"],
            json!(["task-root"])
        );
        assert_eq!(d["phases"][0]["tasks"][1]["ready"], false);
        assert_eq!(
            d["phases"][0]["tasks"][1]["waitingOn"][0]["taskId"],
            "task-root"
        );
    }
    #[tokio::test]
    async fn todo_op_rpc_appends_completes_and_reopens_through_application() {
        let app = build_todo_app("faux-rpc-todo-op", "faux-rpc-todo-op-api").await;
        let append = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::TodoOp {
                id: Some("t1".into()),
                op: pi_coding::TodoOp::Append {
                    phase: "Plan".into(),
                    items: vec!["wire task".into()],
                },
            },
        )
        .await;
        assert!(append.success, "append must succeed: {:?}", append.error);
        assert_eq!(append.id.as_deref(), Some("t1"));
        assert_eq!(append.command, "todo_op");
        let d = append.data.expect("todo_op returns data");
        assert_eq!(d["phases"][0]["name"], "Plan");
        assert_eq!(d["phases"][0]["tasks"][0]["content"], "wire task");
        assert_eq!(d["phases"][0]["tasks"][0]["status"], "in_progress");
        let task_id = d["phases"][0]["tasks"][0]["id"]
            .as_str()
            .expect("assigned task id")
            .to_owned();
        assert!(
            task_id.starts_with("task-"),
            "append assigns a task id: {task_id}"
        );

        // Complete by id; the response carries the transition and new status.
        let done = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::TodoOp {
                id: Some("t2".into()),
                op: pi_coding::TodoOp::Done {
                    task: Some(task_id.clone()),
                    phase: None,
                },
            },
        )
        .await;
        assert!(done.success, "done must succeed: {:?}", done.error);
        let done_data = done.data.expect("done data");
        assert_eq!(done_data["phases"][0]["tasks"][0]["status"], "completed");
        assert_eq!(done_data["completedTasks"][0]["content"], "wire task");

        // Reopen via start (resets to in_progress when unblocked).
        let reopen = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::TodoOp {
                id: Some("t3".into()),
                op: pi_coding::TodoOp::Start {
                    task: task_id.clone(),
                },
            },
        )
        .await;
        assert!(reopen.success, "start must succeed: {:?}", reopen.error);
        let reopen_data = reopen.data.expect("reopen data");
        assert_eq!(
            reopen_data["phases"][0]["tasks"][0]["status"],
            "in_progress"
        );

        // Starting a task that is waiting on a dependency is rejected without
        // mutating application state: append a second task, wire it to the
        // first (which is still in progress), then try to start it.
        let other = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::TodoOp {
                id: Some("t4".into()),
                op: pi_coding::TodoOp::Append {
                    phase: "Plan".into(),
                    items: vec!["dependent".into()],
                },
            },
        )
        .await;
        assert!(other.success);
        let dependent_id = other.data.expect("other data")["phases"][0]["tasks"][1]["id"]
            .as_str()
            .expect("dependent id")
            .to_owned();
        let link = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::TodoOp {
                id: Some("t5".into()),
                op: pi_coding::TodoOp::AddDependency {
                    task: dependent_id.clone(),
                    depends_on: vec![task_id],
                },
            },
        )
        .await;
        assert!(
            link.success,
            "add_dependency must succeed: {:?}",
            link.error
        );
        let waiting = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::TodoOp {
                id: Some("t6".into()),
                op: pi_coding::TodoOp::Start { task: dependent_id },
            },
        )
        .await;
        assert!(!waiting.success, "start on a waiting task must fail");
        assert!(
            waiting
                .error
                .as_deref()
                .is_some_and(|error| error.contains("waiting on"))
        );
        let state = app.todo_state();
        assert_eq!(
            state.phases[0].tasks[1].status,
            pi_coding::TodoStatus::Pending,
            "failed op must not mutate application todo state"
        );
    }
    #[tokio::test]
    async fn set_todos_with_workflow_id_fails_without_mutating_parent_when_missing() {
        let app = build_todo_app(
            "faux-rpc-workflow-todo-missing",
            "faux-rpc-workflow-todo-missing-api",
        )
        .await;
        let phases = vec![pi_coding::TodoPhase {
            name: "Workflow".into(),
            tasks: vec![pi_coding::TodoItem {
                id: "scoped".into(),
                content: "scoped".into(),
                status: pi_coding::TodoStatus::Pending,
                depends_on: Vec::new(),
                ready: true,
                waiting_on: Vec::new(),
                agent: None,
            }],
        }];
        let response = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::SetTodos {
                id: Some("scoped".into()),
                workflow_id: Some("missing".into()),
                phases,
            },
        )
        .await;
        assert!(!response.success);
        assert_eq!(
            response.error.as_deref(),
            Some("application workflow manager is not configured")
        );
        assert!(
            app.todo_state().phases.is_empty(),
            "missing workflow must not fall back to parent"
        );
    }
    #[tokio::test]
    async fn get_state_exposes_todo_phases() {
        use pi_coding::{TodoItem, TodoPhase, TodoStatus};
        let app = build_todo_app("faux-rpc-todo-get", "faux-rpc-todo-get-api").await;
        app.set_todos(vec![TodoPhase {
            name: "Plan".into(),
            tasks: vec![TodoItem {
                id: "task-x".into(),
                content: "x".into(),
                status: TodoStatus::InProgress,
                depends_on: Vec::new(),
                ready: true,
                waiting_on: Vec::new(),
                agent: None,
            }],
        }])
        .expect("set_todos");
        let r = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::GetState {
                id: Some("s1".into()),
            },
        )
        .await;
        assert!(r.success);
        let d = r.data.expect("state data");
        assert_eq!(d["todoPhases"][0]["tasks"][0]["id"], "task-x");
        assert_eq!(d["todoPhases"][0]["tasks"][0]["dependsOn"], json!([]));
        assert_eq!(d["todoPhases"][0]["tasks"][0]["ready"], true);
        assert_eq!(d["todoPhases"][0]["name"], "Plan");
        assert_eq!(d["todoPhases"][0]["tasks"][0]["status"], "in_progress");
        assert_eq!(d["todoPhases"][0]["tasks"][0]["waitingOn"], json!([]));
        assert_eq!(d["todoExecutionStatus"], "dormant");
        assert_eq!(d["todoFailureReason"], Value::Null);
    }

    #[test]
    fn workflow_unknown_fields_fail_closed_via_parse_input() {
        let err = parse_input(
            br#"{"type":"workflow_create","name":"ship","objective":"x","extraField":true}"#,
        )
        .expect_err("unknown fields must fail");
        assert!(!err.success);
        assert_eq!(err.command, "workflow_create");
        assert!(
            err.error
                .as_deref()
                .is_some_and(|e| e.contains("unknown field") || e.contains("extraField")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn workflow_rpc_create_list_get_and_event_redaction() {
        let app = build_todo_app("faux-rpc-workflow", "faux-rpc-workflow-api").await;
        let settings = settings_state();
        let workflows = workflows_state();

        let created = handle_command(
            &app,
            &settings,
            &workflows,
            RpcCommand::WorkflowCreate {
                id: Some("wc1".into()),
                name: "ship".into(),
                objective: "land multi-workflow foundation".into(),
            },
        )
        .await;
        assert!(created.success, "{created:?}");
        assert_eq!(created.command, "workflow_create");
        let data = created.data.as_ref().expect("create data").clone();
        assert_eq!(data["name"], "ship");
        assert_eq!(data["status"], "queued");
        assert_eq!(data["generation"], 1);
        let workflow_id = data["workflowId"].as_str().expect("workflowId").to_owned();
        let worktree = data["worktree"].as_str().expect("worktree label");
        assert!(
            !worktree.starts_with('/'),
            "worktree must not be absolute: {worktree}"
        );
        let encoded = serde_json::to_string(&data).unwrap();
        assert!(!crate::workflow_rpc::wire_json_leaks_absolute_path(
            &encoded
        ));

        let listed = handle_command(
            &app,
            &settings,
            &workflows,
            RpcCommand::WorkflowList {
                id: Some("wl1".into()),
            },
        )
        .await;
        assert!(listed.success, "{listed:?}");
        assert_eq!(
            listed.data.as_ref().unwrap()["workflows"][0]["workflowId"],
            workflow_id
        );

        let got = handle_command(
            &app,
            &settings,
            &workflows,
            RpcCommand::WorkflowGet {
                id: Some("wg1".into()),
                workflow_id: Some(workflow_id.clone()),
                name: None,
            },
        )
        .await;
        assert!(got.success, "{got:?}");
        assert_eq!(
            got.data.as_ref().unwrap()["objective"],
            "land multi-workflow foundation"
        );

        // Host-side absolute path redacted on event wire projection.
        let host = workflows
            .with_host(|host| host.get(Some(&workflow_id), None))
            .expect("host get");
        assert!(
            host.worktree_label
                .as_deref()
                .is_some_and(|p| std::path::Path::new(p).is_absolute()),
            "host keeps absolute path internally for redaction"
        );
        let event =
            crate::workflow_rpc::project_workflow_event(&pi_coding::WorkflowEvent::Updated {
                snapshot: host.clone(),
            });
        let event_json = serde_json::to_value(&event).unwrap();
        assert_eq!(event_json["type"], "workflow_updated");
        assert_eq!(event_json["workflowId"], workflow_id);
        assert_eq!(event_json["generation"], host.generation);
        let event_encoded = serde_json::to_string(&event).unwrap();
        assert!(!crate::workflow_rpc::wire_json_leaks_absolute_path(
            &event_encoded
        ));

        // LF framing: response + event are one JSON object each.
        let mut buffer = Vec::new();
        crate::modes::json::write_json_line(&mut buffer, &created).unwrap();
        crate::modes::json::write_json_line(&mut buffer, &event).unwrap();
        let text = String::from_utf8(buffer).unwrap();
        assert!(!text.contains('\u{1b}'));
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            let _: Value = serde_json::from_str(line).expect("JSON object per LF");
        }

        app.cleanup().await;
    }

    #[tokio::test]
    async fn workflow_rpc_full_lifecycle_dispatches_pause_resume_cancel_and_remove() {
        let app = build_todo_app(
            "faux-rpc-workflow-lifecycle",
            "faux-rpc-workflow-lifecycle-api",
        )
        .await;
        let settings = settings_state();
        let workflows = workflows_state();
        let handle = |command: RpcCommand| handle_command(&app, &settings, &workflows, command);

        // create -> queued
        let created = handle(RpcCommand::WorkflowCreate {
            id: Some("lifecycle-create".into()),
            name: "ship".into(),
            objective: "land multi-workflow lifecycle".into(),
        })
        .await;
        assert!(created.success, "{created:?}");
        let workflow_id = created.data.as_ref().expect("create data")["workflowId"]
            .as_str()
            .expect("workflowId")
            .to_owned();

        // queued -> paused -> resumed (running) -> cancelled -> removed
        let paused = handle(RpcCommand::WorkflowPause {
            id: Some("lifecycle-pause".into()),
            workflow_id: workflow_id.clone(),
        })
        .await;
        assert!(paused.success, "{paused:?}");
        assert_eq!(paused.data.as_ref().unwrap()["status"], "paused");
        assert!(
            paused.data.as_ref().unwrap()["generation"]
                .as_u64()
                .unwrap()
                >= 2,
            "pause must bump the generation"
        );

        let resumed = handle(RpcCommand::WorkflowResume {
            id: Some("lifecycle-resume".into()),
            workflow_id: workflow_id.clone(),
        })
        .await;
        assert!(resumed.success, "{resumed:?}");
        assert_eq!(resumed.data.as_ref().unwrap()["status"], "running");

        // integrate is only legal on terminal/conflicted workflows; on a live
        // workflow the RPC layer reports the structured failure.
        let integrate = handle(RpcCommand::WorkflowIntegrate {
            id: Some("lifecycle-integrate".into()),
            workflow_id: workflow_id.clone(),
        })
        .await;
        assert!(!integrate.success);
        assert_eq!(integrate.command, "workflow_integrate");
        assert_eq!(integrate.id.as_deref(), Some("lifecycle-integrate"));
        assert!(
            integrate
                .error
                .as_deref()
                .is_some_and(|error| error.contains("cannot integrate")),
            "{integrate:?}"
        );

        let cancelled = handle(RpcCommand::WorkflowCancel {
            id: Some("lifecycle-cancel".into()),
            workflow_id: workflow_id.clone(),
        })
        .await;
        assert!(cancelled.success, "{cancelled:?}");
        assert_eq!(cancelled.data.as_ref().unwrap()["status"], "cancelled");

        let removed = handle(RpcCommand::WorkflowRemove {
            id: Some("lifecycle-remove".into()),
            workflow_id: workflow_id.clone(),
        })
        .await;
        assert!(removed.success, "{removed:?}");
        assert_eq!(removed.data.as_ref().unwrap()["workflowId"], workflow_id);

        let listed = handle(RpcCommand::WorkflowList {
            id: Some("lifecycle-list".into()),
        })
        .await;
        assert!(listed.success, "{listed:?}");
        assert!(
            listed.data.as_ref().unwrap()["workflows"]
                .as_array()
                .expect("workflows array")
                .is_empty(),
            "removed workflow must be gone from the list"
        );

        app.cleanup().await;
    }

    #[tokio::test]
    async fn workflow_detail_dispatches_panel_projection_through_rpc() {
        let app = build_todo_app("faux-rpc-workflow-detail", "faux-rpc-workflow-detail-api").await;
        let settings = settings_state();
        let workflows = workflows_state();

        let created = handle_command(
            &app,
            &settings,
            &workflows,
            RpcCommand::WorkflowCreate {
                id: Some("detail-create".into()),
                name: "ship".into(),
                objective: "land the detail projection".into(),
            },
        )
        .await;
        assert!(created.success, "{created:?}");
        let workflow_id = created.data.as_ref().expect("create data")["workflowId"]
            .as_str()
            .expect("workflowId")
            .to_owned();

        let detail = handle_command(
            &app,
            &settings,
            &workflows,
            RpcCommand::WorkflowDetail {
                id: Some("detail-get".into()),
                workflow_id: Some(workflow_id.clone()),
                name: None,
            },
        )
        .await;
        assert!(detail.success, "{detail:?}");
        assert_eq!(detail.command, "workflow_detail");
        let data = detail.data.as_ref().expect("detail data");
        assert_eq!(data["id"], workflow_id);
        assert_eq!(data["name"], "ship");
        assert_eq!(data["objective"], "land the detail projection");
        assert_eq!(data["status"], "queued");
        let worktree = data["worktree"]["label"].as_str().expect("worktree label");
        assert!(
            !Path::new(worktree).is_absolute(),
            "worktree must not be absolute: {worktree}"
        );
        let encoded = serde_json::to_string(data).unwrap();
        assert!(
            !crate::workflow_rpc::wire_json_leaks_absolute_path(&encoded),
            "absolute path leaked: {encoded}"
        );

        // A missing selector fails closed with the command name intact.
        let missing = handle_command(
            &app,
            &settings,
            &workflows,
            RpcCommand::WorkflowDetail {
                id: Some("detail-missing".into()),
                workflow_id: None,
                name: None,
            },
        )
        .await;
        assert!(!missing.success, "{missing:?}");
        assert_eq!(missing.command, "workflow_detail");
        assert!(
            missing
                .error
                .as_deref()
                .is_some_and(|error| error.contains("workflowId or name")),
            "{missing:?}"
        );

        app.cleanup().await;
    }

    #[test]
    fn workflow_hub_send_parses_camel_case_and_rejects_unknown_fields() {
        match parse_input(
            br#"{"type":"workflow_hub_send","id":"whs-1","workflowId":"wf-1","generation":7,"recipient":"writer","body":"ping","replyTo":"msg-1"}"#,
        )
        .expect("parse")
        {
            RpcInput::Command {
                command: RpcCommand::WorkflowHubSend {
                    id,
                    workflow_id,
                    generation,
                    recipient,
                    body,
                    reply_to,
                },
                ..
            } => {
                assert_eq!(id.as_deref(), Some("whs-1"));
                assert_eq!(workflow_id, "wf-1");
                assert_eq!(generation, 7);
                assert_eq!(recipient, "writer");
                assert_eq!(body, "ping");
                assert_eq!(reply_to.as_deref(), Some("msg-1"));
            }
            other => panic!("expected WorkflowHubSend, got {other:?}"),
        }

        let err = parse_input(
            br#"{"type":"workflow_hub_send","workflowId":"wf-1","generation":1,"recipient":"writer","body":"ping","from":"attacker"}"#,
        )
        .expect_err("arbitrary from must fail closed");
        assert!(!err.success);
        assert_eq!(err.command, "workflow_hub_send");
        assert!(
            err.error
                .as_deref()
                .is_some_and(|error| error.contains("unknown field") || error.contains("from")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn workflow_hub_send_rejects_unknown_stale_broadcast_and_empty_body() {
        let app = build_todo_app("faux-rpc-workflow-hub", "faux-rpc-workflow-hub-api").await;
        let settings = settings_state();
        let workflows = workflows_state();
        let handle = |command: RpcCommand| handle_command(&app, &settings, &workflows, command);

        let created = handle(RpcCommand::WorkflowCreate {
            id: Some("hub-create".into()),
            name: "ship".into(),
            objective: "land hub send".into(),
        })
        .await;
        assert!(created.success, "{created:?}");
        let workflow_id = created.data.as_ref().expect("create data")["workflowId"]
            .as_str()
            .expect("workflowId")
            .to_owned();
        let generation = created.data.as_ref().unwrap()["generation"]
            .as_u64()
            .expect("generation");

        let unknown = handle(RpcCommand::WorkflowHubSend {
            id: Some("hub-unknown".into()),
            workflow_id: "missing-workflow".into(),
            generation,
            recipient: "writer".into(),
            body: "ping".into(),
            reply_to: None,
        })
        .await;
        assert!(!unknown.success, "{unknown:?}");
        assert_eq!(unknown.command, "workflow_hub_send");
        assert_eq!(unknown.id.as_deref(), Some("hub-unknown"));
        assert!(
            unknown.error.as_deref().is_some_and(|error| {
                error.contains("not found") || error.contains("not configured")
            }),
            "{unknown:?}"
        );

        let stale = handle(RpcCommand::WorkflowHubSend {
            id: Some("hub-stale".into()),
            workflow_id: workflow_id.clone(),
            generation: generation.saturating_add(1),
            recipient: "writer".into(),
            body: "ping".into(),
            reply_to: None,
        })
        .await;
        assert!(!stale.success, "{stale:?}");
        assert_eq!(stale.command, "workflow_hub_send");
        assert!(
            stale
                .error
                .as_deref()
                .is_some_and(|error| error.contains("stale")
                    || error.contains("not found")
                    || error.contains("not configured")),
            "{stale:?}"
        );

        let broadcast = handle(RpcCommand::WorkflowHubSend {
            id: Some("hub-broadcast".into()),
            workflow_id: workflow_id.clone(),
            generation,
            recipient: "all".into(),
            body: "ping".into(),
            reply_to: None,
        })
        .await;
        assert!(!broadcast.success, "{broadcast:?}");
        assert_eq!(broadcast.command, "workflow_hub_send");
        assert!(
            broadcast
                .error
                .as_deref()
                .is_some_and(|error| error.contains("broadcast")),
            "{broadcast:?}"
        );

        let empty = handle(RpcCommand::WorkflowHubSend {
            id: Some("hub-empty".into()),
            workflow_id,
            generation,
            recipient: "writer".into(),
            body: "   ".into(),
            reply_to: None,
        })
        .await;
        assert!(!empty.success, "{empty:?}");
        assert_eq!(empty.command, "workflow_hub_send");
        assert!(
            empty
                .error
                .as_deref()
                .is_some_and(|error| error.contains("body")),
            "{empty:?}"
        );

        app.cleanup().await;
    }

    #[tokio::test]
    async fn workflow_hub_send_runtime_unavailable_is_a_command_failure() {
        let app = build_todo_app(
            "faux-rpc-workflow-hub-unavailable",
            "faux-rpc-workflow-hub-unavailable-api",
        )
        .await;
        let settings = settings_state();
        let workflows = workflows_state();
        let created = handle_command(
            &app,
            &settings,
            &workflows,
            RpcCommand::WorkflowCreate {
                id: Some("hub-unavailable-create".into()),
                name: "ship".into(),
                objective: "resident send".into(),
            },
        )
        .await;
        assert!(created.success, "{created:?}");
        let workflow_id = created.data.as_ref().unwrap()["workflowId"]
            .as_str()
            .expect("workflowId")
            .to_owned();
        let generation = created.data.as_ref().unwrap()["generation"]
            .as_u64()
            .expect("generation");

        let sent = handle_command(
            &app,
            &settings,
            &workflows,
            RpcCommand::WorkflowHubSend {
                id: Some("hub-unavailable".into()),
                workflow_id,
                generation,
                recipient: "Main".into(),
                body: "status?".into(),
                reply_to: None,
            },
        )
        .await;
        assert!(!sent.success, "{sent:?}");
        assert_eq!(sent.command, "workflow_hub_send");
        assert_eq!(sent.id.as_deref(), Some("hub-unavailable"));
        assert!(
            sent.error.as_deref().is_some_and(|error| {
                error.contains("unavailable") || error.contains("not configured")
            }),
            "{sent:?}"
        );
        assert!(sent.data.is_none(), "{sent:?}");
        app.cleanup().await;
    }

    #[test]
    fn workflow_hub_send_wire_requires_clients_to_check_each_receipt() {
        let data = workflow_hub_send_response_data(
            "workflow-1".into(),
            7,
            "worker".into(),
            vec![pi_coding::DeliveryReceipt {
                to: "worker".into(),
                outcome: pi_coding::DeliveryOutcome::Failed,
                requested: None,
                message_id: Some("message-1".into()),
                error: Some("orchestration mailbox is full".into()),
            }],
        );
        let response = RpcResponse::success(
            Some("hub-failed-receipt".into()),
            "workflow_hub_send",
            Some(data),
        );
        assert!(
            response.success,
            "accepted dispatch is a successful RPC response"
        );
        assert!(response.error.is_none());
        let receipt = &response.data.as_ref().unwrap()["receipts"][0];
        assert_eq!(receipt["to"], "worker");
        assert!(receipt["requested"].is_null(), "{receipt}");
        assert_eq!(receipt["outcome"], "failed");
        assert_eq!(receipt["messageId"], "message-1");
        assert_eq!(receipt["error"], "orchestration mailbox is full");
    }

    struct CatalogExtensionHost;

    struct CatalogExtensionTransport {
        frames: StdMutex<std::collections::VecDeque<Option<pi_coding::ExtensionFrame>>>,
        ready: tokio::sync::Notify,
    }

    impl CatalogExtensionTransport {
        fn new() -> Self {
            Self {
                frames: StdMutex::new(std::collections::VecDeque::new()),
                ready: tokio::sync::Notify::new(),
            }
        }

        fn push(&self, frame: pi_coding::ExtensionFrame) {
            self.frames
                .lock()
                .expect("extension frame queue")
                .push_back(Some(frame));
            self.ready.notify_one();
        }
    }

    impl pi_coding::ExtensionTransport for CatalogExtensionTransport {
        fn send(
            &self,
            frame: &pi_coding::ExtensionHostFrame,
        ) -> pi_coding::ExtensionFuture<'_, Result<()>> {
            match frame {
                pi_coding::ExtensionHostFrame::Hello { .. } => {
                    self.push(pi_coding::ExtensionFrame::Hello {
                        protocol_version: pi_coding::EXTENSION_PROTOCOL_VERSION,
                        manifest: pi_coding::ExtensionCapabilityManifest {
                            id: "rpc-catalog".to_owned(),
                            name: "RPC catalog fixture".to_owned(),
                            version: "1.0.0".to_owned(),
                            capabilities: std::collections::BTreeSet::from([
                                pi_coding::ExtensionCapability::Commands,
                            ]),
                            ui_capabilities: std::collections::BTreeSet::new(),
                        },
                    });
                }
                pi_coding::ExtensionHostFrame::Request { id, request, .. } => {
                    if matches!(request, pi_coding::ExtensionHostRequest::Load) {
                        for (name, description) in [
                            ("extension-only", "Extension only command"),
                            ("shared", "Extension collision"),
                            ("help", "Builtin collision"),
                        ] {
                            self.push(pi_coding::ExtensionFrame::Register {
                                registration: pi_coding::ExtensionRegistration::Command {
                                    command: pi_coding::ExtensionCommandDescriptor {
                                        name: name.to_owned(),
                                        description: Some(description.to_owned()),
                                    },
                                },
                            });
                        }
                    }
                    let value = match request {
                        pi_coding::ExtensionHostRequest::Invoke {
                            invocation:
                                pi_coding::ExtensionInvocation::Command { name, .. },
                            ..
                        } if name == "extension-only" => serde_json::Value::String("pong".to_owned()),
                        _ => serde_json::Value::Null,
                    };
                    self.push(pi_coding::ExtensionFrame::Response {
                        id: id.clone(),
                        result: pi_coding::ProtocolResult::Success { value },
                    });
                }
                pi_coding::ExtensionHostFrame::Shutdown { .. } => {
                    self.frames
                        .lock()
                        .expect("extension frame queue")
                        .push_back(None);
                    self.ready.notify_one();
                }
                pi_coding::ExtensionHostFrame::Response { .. }
                | pi_coding::ExtensionHostFrame::Cancel { .. } => {}
            }
            Box::pin(async { Ok(()) })
        }

        fn receive(
            &self,
        ) -> pi_coding::ExtensionFuture<'_, Result<Option<pi_coding::ExtensionFrame>>> {
            Box::pin(async move {
                loop {
                    let notified = self.ready.notified();
                    if let Some(frame) = self
                        .frames
                        .lock()
                        .expect("extension frame queue")
                        .pop_front()
                    {
                        return Ok(frame);
                    }
                    notified.await;
                }
            })
        }

        fn terminate(&self) -> pi_coding::ExtensionFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }

        fn diagnostic_context(&self) -> String {
            "RPC catalog fixture".to_owned()
        }
    }

    impl pi_coding::ExtensionHost for CatalogExtensionHost {
        fn launch(
            &self,
            _launch: pi_coding::ExtensionLaunch,
        ) -> pi_coding::ExtensionFuture<'_, Result<Arc<dyn pi_coding::ExtensionTransport>>>
        {
            Box::pin(async {
                Ok(Arc::new(CatalogExtensionTransport::new())
                    as Arc<dyn pi_coding::ExtensionTransport>)
            })
        }
    }

    async fn build_command_catalog_app() -> (tempfile::TempDir, tempfile::TempDir, Application) {
        let cwd = tempfile::tempdir().expect("catalog cwd");
        let agent_dir = tempfile::tempdir().expect("catalog agent dir");
        let prompt_dir = cwd.path().join(".rpi/prompts");
        std::fs::create_dir_all(&prompt_dir).expect("prompt directory");
        for (name, description) in [
            ("help", "Conflicts with builtin help"),
            ("shared", "Wins the dynamic collision"),
            ("prompt-only", "Prompt only command"),
        ] {
            std::fs::write(
                prompt_dir.join(format!("{name}.md")),
                format!("---\ndescription: {description}\n---\nprompt body\n"),
            )
            .expect("prompt template");
        }
        let skill_dir = cwd.path().join(".rpi/skills/catalog-skill");
        std::fs::create_dir_all(&skill_dir).expect("skill directory");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: catalog-skill\ndescription: Skill only command\n---\nskill body\n",
        )
        .expect("skill definition");
        let extension_path = cwd.path().join("fixture-extension");
        std::fs::write(&extension_path, "fixture").expect("catalog extension executable");
        let manifest_path = cwd.path().join(pi_coding::PROCESS_EXTENSION_MANIFEST_FILE);
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&json!({
                "schemaVersion": 1,
                "id": "rpc-catalog",
                "runtime": "process",
                "executable": "fixture-extension",
                "capabilities": ["commands"]
            }))
            .expect("catalog extension manifest"),
        )
        .expect("write catalog extension manifest");

        let session = disabled_session(pi_coding::SessionOptions {
            model: faux_model(),
            cwd: cwd.path().to_path_buf(),
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
            auth_manager: None,
        })
        .expect("catalog session");
        let mut resource_options = pi_coding::ResourceManagerOptions::new(cwd.path());
        resource_options.agent_dir = agent_dir.path().to_path_buf();
        std::fs::create_dir_all(resource_options.agent_dir.join("agents"))
            .expect("catalog global agents dir");
        std::fs::create_dir_all(cwd.path().join(".rpi/agents"))
            .expect("catalog project agents dir");
        std::fs::write(
            resource_options.agent_dir.join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )
        .expect("isolated sandbox settings");
        resource_options.project_trust_override = Some(true);
        resource_options.disable_context_files = true;
        resource_options.disable_themes = true;
        resource_options.explicit_extension_paths = vec![manifest_path];
        session
            .attach_resources(pi_coding::ResourceManager::new(resource_options).expect("resources"))
            .await
            .expect("attach resources");
        let session_dir = cwd.path().join("sessions");
        std::fs::create_dir_all(&session_dir).expect("catalog session dir");
        session.set_session_dir(session_dir);

        let permissions = pi_coding::ExtensionPermissionSet {
            capabilities: std::collections::BTreeSet::from([
                pi_coding::ExtensionCapability::Commands,
            ]),
            ui_capabilities: std::collections::BTreeSet::new(),
        };
        let runtime = pi_coding::ExtensionRuntime::new(
            Arc::new(CatalogExtensionHost),
            None,
            pi_coding::ExtensionRuntimeOptions::default(),
        );
        (
            cwd,
            agent_dir,
            Application::new_with_extensions(session, runtime, permissions).await,
        )
    }

    #[tokio::test]
    async fn get_commands_projects_executable_catalog_with_loaded_skills() {
        let (_cwd, _agent_dir, app) = build_command_catalog_app().await;
        let response = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::GetCommands {
                id: Some("catalog".to_owned()),
            },
        )
        .await;
        assert!(response.success, "{response:?}");
        let commands = response.data.expect("get_commands data")["commands"]
            .as_array()
            .expect("commands array")
            .clone();
        let names = commands
            .iter()
            .map(|command| command["name"].as_str().expect("command name"))
            .collect::<Vec<_>>();

        // The primary builtin surface stays projected (skill is a builtin with
        // argument metadata so the Web picker can show `/skill` as a parent).
        let skill_builtin = commands
            .iter()
            .find(|command| command["name"] == "skill" && command["source"] == "builtin")
            .expect("builtin /skill is projected");
        assert_eq!(skill_builtin["argumentHint"], "<name>");
        assert_eq!(skill_builtin["requiresArguments"], true);
        assert!(
            skill_builtin["skillName"].is_null(),
            "builtin skill must not carry a skillName"
        );

        // The real loaded skill projects as a `skill:<name>` wire entry with a
        // stable bare `skillName` so the Web composer can compose `/skill <name>`.
        let skill_entry = commands
            .iter()
            .find(|command| command["name"] == "skill:catalog-skill")
            .expect("loaded skill:catalog-skill is projected");
        assert_eq!(skill_entry["source"], "skill");
        assert_eq!(skill_entry["skillName"], "catalog-skill");
        assert_eq!(
            skill_entry["description"], "Skill only command",
            "skill description must come from the loaded frontmatter"
        );

        // Collision-free: the conflicting `help` prompt and `help`/`shared`
        // extension duplicates are excluded — only the builtin `help` and the
        // first dynamic `shared` (the prompt) remain.
        assert_eq!(
            commands
                .iter()
                .filter(|command| command["name"] == "help")
                .count(),
            1,
            "conflicting help prompt must be dropped, builtin help kept"
        );
        assert_eq!(
            commands
                .iter()
                .filter(|command| command["name"] == "help")
                .next()
                .map(|command| command["source"].as_str().unwrap_or("")),
            Some("builtin")
        );
        assert_eq!(
            commands
                .iter()
                .filter(|command| command["name"] == "shared")
                .count(),
            1,
            "shared prompt/extension collision must resolve to a single entry"
        );
        let shared = commands
            .iter()
            .find(|command| command["name"] == "shared")
            .expect("shared winner present");
        assert_eq!(
            shared["source"], "prompt",
            "prompt wins the dynamic collision"
        );

        // Dynamic prompt + extension commands are projected on the wire (the
        // Web picker filters its own executable surface) — but conflicts never
        // duplicate a name.
        assert!(names.contains(&"prompt-only"));
        assert!(names.contains(&"extension-only"));
        // Every entry carries a source from the closed set; non-skill entries
        // project `skillName: null` so the wire shape stays stable.
        for command in &commands {
            let source = command["source"].as_str().expect("source string");
            assert!(
                matches!(source, "builtin" | "prompt" | "skill" | "extension"),
                "unexpected source {source:?} for {}",
                command["name"]
            );
            if source != "skill" {
                assert!(
                    command["skillName"].is_null(),
                    "non-skill {} must not carry skillName",
                    command["name"]
                );
            }
        }
        app.cleanup().await;
    }

    #[test]
    fn command_source_str_maps_every_variant_exhaustively() {
        use crate::interactive_commands::CommandSource;
        assert_eq!(command_source_str(CommandSource::Builtin), "builtin");
        assert_eq!(command_source_str(CommandSource::Prompt), "prompt");
        assert_eq!(command_source_str(CommandSource::Skill), "skill");
        assert_eq!(command_source_str(CommandSource::Extension), "extension");
    }

    #[tokio::test]
    async fn get_commands_includes_workflow_and_builtins_source() {
        let app = build_todo_app("faux-rpc-workflow-cmds", "faux-rpc-workflow-cmds-api").await;
        let r = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::GetCommands {
                id: Some("cmds".into()),
            },
        )
        .await;
        assert!(r.success, "{r:?}");
        let commands = r.data.expect("commands")["commands"]
            .as_array()
            .cloned()
            .expect("commands array");
        assert_eq!(
            commands
                .iter()
                .filter(|command| command["name"] == "todo")
                .count(),
            1,
            "expected exactly one todo command in {commands:?}"
        );
        assert!(
            !commands
                .iter()
                .any(|command| command["name"] == "todo-child"),
            "todo-child must not be advertised in {commands:?}"
        );
        let todo = commands
            .iter()
            .find(|command| command["name"] == "todo")
            .expect("unified todo command");
        assert_eq!(
            todo["argumentHint"],
            "[list|new [label]|select <id>|send [id] <message>|add <id> [options] <task>|status [id]|pause [id]|resume [id]|stop [id]|cancel [id]|archive <id>|start <task>|done <task>|drop <task>|clear|<markdown>]"
        );
        assert!(
            commands.iter().any(|command| command["name"] == "workflow"),
            "expected workflow in {commands:?}"
        );
        // Every command carries a lowercase source string.
        for cmd in &commands {
            let src = cmd["source"].as_str().expect("source is a string");
            assert!(
                matches!(src, "builtin" | "prompt" | "skill" | "extension"),
                "unexpected source {src:?} for command {}",
                cmd["name"]
            );
        }
        // At least one builtin is present (e.g. help).
        assert!(
            commands.iter().any(|c| c["source"] == "builtin"),
            "expected at least one builtin command in {commands:?}"
        );
        app.cleanup().await;
    }

    /// Session whose provider stream replies with a fixed assistant message —
    /// used for the side-chat round trip (the fork inherits this stream).
    fn reply_stream(reply: &'static str) -> pi_agent::StreamFn {
        Arc::new(move |model, _context, _options| {
            Box::pin(async move {
                let stream = pi_ai::new_assistant_message_event_stream();
                let producer = stream.clone();
                tokio::spawn(async move {
                    let mut message = pi_ai::AssistantMessage::pending(&model);
                    message.content.push(ContentBlock::text(reply));
                    message.stop_reason = pi_ai::StopReason::Stop;
                    producer.end(Some(message)).await;
                });
                stream
            })
        })
    }

    async fn build_side_chat_app(
        model_id: &str,
        stream_fn: pi_agent::StreamFn,
    ) -> (tempfile::TempDir, tempfile::TempDir, Application) {
        let mut model = faux_model();
        model.id = model_id.into();
        model.name = model_id.into();
        let cwd = tempfile::tempdir().expect("tempdir");
        let session = disabled_session(pi_coding::SessionOptions {
            model,
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "faux".into(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(stream_fn),
            auth_resolver: None,
            auth_manager: None,
        })
        .expect("session");
        let session_dir = cwd.path().join(".pi-test-sessions");
        std::fs::create_dir_all(&session_dir).expect("session dir");
        session.set_session_dir(session_dir);
        let agent = tempfile::tempdir().expect("agent");
        session
            .attach_resources(isolated_resources(cwd.path(), agent.path()))
            .await
            .expect("attach resources");
        let app = Application::new(session).await;
        (cwd, agent, app)
    }

    /// Build an Application whose session cwd is `cwd` and whose provider stream
    /// replies with a fixed assistant message. Used by code-review RPC tests:
    /// `CodeReviewController::fork_from` forks the side-chat conversation (no
    /// provider call), and comment submissions resolve through `reply_stream`.
    async fn build_code_review_app(cwd: &Path) -> Application {
        build_code_review_app_with_stream(cwd, reply_stream("review reply")).await
    }

    async fn build_code_review_app_with_stream(cwd: &Path, stream_fn: pi_agent::StreamFn) -> Application {
        let mut model = faux_model();
        model.id = "faux-rpc-code-review".into();
        model.name = "faux-rpc-code-review".into();
        let agent_root = Arc::new(tempfile::tempdir().expect("review agent root"));
        let retained_root = agent_root.clone();
        let stream_fn: pi_agent::StreamFn = Arc::new(move |model, context, options| {
            let retained_root = retained_root.clone();
            let response = stream_fn(model, context, options);
            Box::pin(async move {
                let response = response.await;
                drop(retained_root);
                response
            })
        });
        let session = disabled_session(pi_coding::SessionOptions {
            model,
            cwd: cwd.to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "faux".into(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(stream_fn),
            auth_resolver: None,
            auth_manager: None,
        })
        .expect("code-review session");
        let resources = isolated_resources(cwd, agent_root.path());
        let session_dir = cwd.join(".pi-test-sessions");
        std::fs::create_dir_all(&session_dir).expect("review session dir");
        session.set_session_dir(session_dir);
        session.attach_resources(resources).await.expect("review resources");
        Application::new(session).await
    }

    async fn code_review_file_diff_cache_paths(
        code_review: &CodeReviewRpcState,
    ) -> Vec<String> {
        let state = code_review.state.lock().await;
        let Some(session) = state.session.as_ref() else {
            return Vec::new();
        };
        let mut paths: Vec<String> =
            session.file_diff_cache.entries.keys().cloned().collect();
        paths.sort();
        paths
    }

    fn file_diff_page_text(page: &Value) -> String {
        page["hunks"]
            .as_array()
            .expect("page hunks")
            .iter()
            .flat_map(|hunk| hunk["lines"].as_array().expect("hunk lines"))
            .filter_map(|line| line["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn synthetic_file_diff(path: &str, text_len: usize) -> Arc<FileDiff> {
        use crate::code_review::{DiffLine, FileStatus};
        Arc::new(FileDiff {
            path: path.into(),
            previous_path: None,
            status: FileStatus::Modified,
            binary: false,
            hunks: vec![DiffHunk {
                header: "@@ -1 +1,2 @@".into(),
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 2,
                lines: vec![DiffLine {
                    kind: DiffLineKind::Addition,
                    old_no: None,
                    new_no: Some(2),
                    text: "x".repeat(text_len),
                }],
            }],
            byte_capped: false,
            error: None,
        })
    }

    #[test]
    fn file_diff_cache_evicts_lru_at_entry_cap_and_refreshes_on_hit() {
        let mut cache = FileDiffCache::new(2, usize::MAX);
        cache.put("a".into(), synthetic_file_diff("a", 10));
        cache.put("b".into(), synthetic_file_diff("b", 10));
        assert!(cache.get("a").is_some());
        cache.put("c".into(), synthetic_file_diff("c", 10));
        assert!(
            cache.peek("a").is_some(),
            "recently hit entry survives eviction"
        );
        assert!(cache.peek("c").is_some(), "newest entry survives eviction");
        assert!(
            cache.peek("b").is_none(),
            "least-recently-used entry is evicted"
        );
    }

    #[test]
    fn file_diff_cache_bounds_bytes_and_never_caches_oversize_entries() {
        let small = synthetic_file_diff("small", 10);
        let large = synthetic_file_diff("large", 5000);
        assert!(large.estimated_heap_bytes() > 5000);
        assert!(
            large.estimated_heap_bytes() >= small.estimated_heap_bytes() + 4990,
            "line text bytes must dominate the estimate"
        );

        let small_estimate = small.estimated_heap_bytes();
        let large_estimate = large.estimated_heap_bytes();
        assert!(small_estimate > 0);
        assert!(
            small_estimate * 2 < large_estimate,
            "fixture text sizes must dominate structural overhead"
        );

        let mut cache = FileDiffCache::new(usize::MAX, small_estimate * 2 - 1);
        cache.put("large".into(), synthetic_file_diff("large", 5000));
        assert!(
            cache.peek("large").is_none(),
            "oversize single value must be returned uncached"
        );
        assert_eq!(cache.entries.len(), 0);
        assert_eq!(cache.bytes, 0);

        cache.put("s1".into(), synthetic_file_diff("s1", 10));
        cache.put("s2".into(), synthetic_file_diff("s2", 10));
        assert_eq!(cache.entries.len(), 1, "byte cap evicts down to one entry");
        assert!(
            cache.bytes <= small_estimate * 2 - 1,
            "cache must stay within its byte cap: {}",
            cache.bytes
        );
        assert!(cache.peek("s2").is_some(), "most recent entry survives");
        assert!(cache.peek("s1").is_none(), "LRU entry evicted on byte cap");
    }

    #[test]
    fn file_diff_cache_entry_weight_charges_key_and_documented_overhead_exactly() {
        let key = "k0".to_owned();
        let diff = synthetic_file_diff(&key, 16);
        let weight =
            diff.estimated_heap_bytes() + key.capacity() + FILE_DIFF_CACHE_ENTRY_OVERHEAD_BYTES;

        let mut cache = FileDiffCache::new(usize::MAX, weight);
        cache.put(key, diff);
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.bytes, weight, "one entry weighs key + overhead + diff");

        cache.put("k1".into(), synthetic_file_diff("k1", 16));
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.bytes, weight);
        assert!(cache.peek("k1").is_some());
        assert!(cache.peek("k0").is_none(), "LRU entry evicted at the exact cap");

        let mut two = FileDiffCache::new(usize::MAX, 2 * weight);
        two.put("k0".into(), synthetic_file_diff("k0", 16));
        two.put("k1".into(), synthetic_file_diff("k1", 16));
        assert_eq!(two.entries.len(), 2);
        assert_eq!(two.bytes, 2 * weight);
        two.put("k2".into(), synthetic_file_diff("k2", 16));
        assert_eq!(two.entries.len(), 2);
        assert_eq!(two.bytes, 2 * weight);
        assert!(two.peek("k2").is_some());
        assert!(two.peek("k1").is_some());
        assert!(two.peek("k0").is_none());
    }

    #[test]
    fn file_diff_cache_replacement_charges_the_retained_key_exactly() {
        let diff = synthetic_file_diff("dup", 16);
        let small = "dup".to_owned();
        let mut big = String::with_capacity(4096);
        big.push_str("dup");
        let small_capacity = small.capacity();
        let small_weight =
            diff.estimated_heap_bytes() + small.capacity() + FILE_DIFF_CACHE_ENTRY_OVERHEAD_BYTES;
        let big_weight =
            diff.estimated_heap_bytes() + big.capacity() + FILE_DIFF_CACHE_ENTRY_OVERHEAD_BYTES;
        let mut cache = FileDiffCache::new(usize::MAX, big_weight.max(small_weight) * 2);

        cache.put(small, diff.clone());
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.bytes, small_weight);
        assert_eq!(
            cache.entries.get_key_value("dup").map(|(key, _)| key.capacity()),
            Some(small_capacity),
            "the map retains exactly the key it charged"
        );

        cache.put(big, synthetic_file_diff("dup", 16));
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(
            cache.entries.get_key_value("dup").map(|(key, _)| key.capacity()),
            Some(4096),
            "replacement retains exactly the new key"
        );
        assert_eq!(cache.bytes, big_weight);

        cache.put("dup".to_owned(), synthetic_file_diff("dup", 16));
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(
            cache.entries.get_key_value("dup").map(|(key, _)| key.capacity()),
            Some(small_capacity),
            "replacement retains exactly the new key in the reverse direction"
        );
        assert_eq!(cache.bytes, small_weight);
    }

    #[tokio::test]
    async fn code_review_file_diff_cache_clears_on_refresh() {
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(cwd.path().join("single.txt"), "base\n").expect("write base");
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "init"]).status.success());
        std::fs::write(cwd.path().join("single.txt"), "base\nchanged\n").expect("write changed");
        assert!(git(&["add", "single.txt"]).status.success());

        let app = build_code_review_app(cwd.path()).await;
        let code_review = CodeReviewRpcState::default();
        let opened = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen {
                id: None,
                commit: None,
                from: None,
                to: None,
                include_working_tree: None,
            },
        )
        .await;
        assert!(opened.success, "{opened:?}");
        let snapshot_id = opened.data.as_ref().expect("open data")["snapshotId"]
            .as_str()
            .expect("snapshot id")
            .to_owned();

        let loaded = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewFileDiff {
                id: None,
                snapshot_id: snapshot_id.clone(),
                path: "single.txt".into(),
                cursor: 0,
                max_lines: Some(25),
            },
        )
        .await;
        assert!(loaded.success, "{loaded:?}");
        assert_eq!(
            code_review_file_diff_cache_paths(&code_review).await,
            vec!["single.txt".to_owned()],
            "successful load must be cached"
        );

        std::fs::write(cwd.path().join("single.txt"), "base\nchanged2\n").expect("write changed2");
        assert!(git(&["add", "single.txt"]).status.success());
        let refreshed = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewRefresh { id: None },
        )
        .await;
        assert!(refreshed.success, "{refreshed:?}");
        assert!(
            code_review_file_diff_cache_paths(&code_review).await.is_empty(),
            "refresh must clear the file-diff cache"
        );

        let closed = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewClose { id: None, review_id: None },
        )
        .await;
        assert!(closed.success);
        app.cleanup().await;
    }

    #[tokio::test]
    async fn code_review_refresh_pages_refreshed_pinned_authority() {
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(cwd.path().join("tracked.txt"), "v1\n").expect("write base");
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "init"]).status.success());
        std::fs::write(cwd.path().join("untracked.txt"), "u1\n").expect("write untracked");

        let app = build_code_review_app(cwd.path()).await;
        let code_review = CodeReviewRpcState::default();
        let opened = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen {
                id: None,
                commit: None,
                from: None,
                to: None,
                include_working_tree: None,
            },
        )
        .await;
        assert!(opened.success, "{opened:?}");
        let snapshot_id = opened.data.as_ref().expect("open data")["snapshotId"]
            .as_str()
            .expect("snapshot id")
            .to_owned();

        let open_page = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewFileDiff {
                id: None,
                snapshot_id: snapshot_id.clone(),
                path: "untracked.txt".into(),
                cursor: 0,
                max_lines: Some(25),
            },
        )
        .await;
        assert!(open_page.success, "{open_page:?}");
        let open_text = file_diff_page_text(open_page.data.as_ref().expect("open page"));
        assert!(open_text.contains("u1"), "{open_text:?}");
        assert!(!open_text.contains("u2"), "{open_text:?}");

        std::fs::write(cwd.path().join("tracked.txt"), "v2\n").expect("mutate tracked");
        assert!(git(&["checkout", "-q", "-b", "feature"]).status.success());
        std::fs::write(cwd.path().join("untracked.txt"), "u2\n").expect("rewrite untracked");
        let refreshed = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewRefresh { id: None },
        )
        .await;
        assert!(refreshed.success, "{refreshed:?}");
        let refreshed_id = refreshed.data.as_ref().expect("refresh data")["snapshotId"]
            .as_str()
            .expect("snapshot id")
            .to_owned();
        assert_ne!(
            refreshed_id, snapshot_id,
            "mutation + untracked rewrite must flip the snapshot identity"
        );

        let refreshed_page = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewFileDiff {
                id: None,
                snapshot_id: refreshed_id.clone(),
                path: "untracked.txt".into(),
                cursor: 0,
                max_lines: Some(25),
            },
        )
        .await;
        assert!(refreshed_page.success, "{refreshed_page:?}");
        let page = refreshed_page.data.as_ref().expect("refreshed page");
        assert_eq!(page["snapshotId"], refreshed_id);
        let text = file_diff_page_text(page);
        assert!(
            text.contains("u2"),
            "post-refresh paging must serve the refreshed untracked pin: {text:?}"
        );
        assert!(
            !text.contains("u1"),
            "the open-time pin must not survive the refresh: {text:?}"
        );

        let tracked_page = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewFileDiff {
                id: None,
                snapshot_id: refreshed_id.clone(),
                path: "tracked.txt".into(),
                cursor: 0,
                max_lines: Some(25),
            },
        )
        .await;
        assert!(tracked_page.success, "{tracked_page:?}");
        let tracked_text =
            file_diff_page_text(tracked_page.data.as_ref().expect("tracked page"));
        assert!(
            tracked_text.contains("v2"),
            "post-refresh paging must serve the refreshed tracked state: {tracked_text:?}"
        );

        let closed = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewClose { id: None, review_id: None },
        )
        .await;
        assert!(closed.success);
        app.cleanup().await;
    }

    #[tokio::test]
    async fn code_review_file_diff_cache_evicts_lru_and_reloads_evicted_file() {
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"]).status.success();
        git(&["config", "user.name", "Test"]).status.success();
        for index in 0..40 {
            std::fs::write(cwd.path().join(format!("f{index:02}.txt")), "base\n")
                .expect("write base");
        }
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "init"]).status.success());
        for index in 0..40 {
            std::fs::write(
                cwd.path().join(format!("f{index:02}.txt")),
                format!("base\nchanged-{index}\n"),
            )
            .expect("write changed");
        }
        assert!(git(&["add", "."]).status.success());

        let app = build_code_review_app(cwd.path()).await;
        let code_review = CodeReviewRpcState::default();
        let opened = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen {
                id: None,
                commit: None,
                from: None,
                to: None,
                include_working_tree: None,
            },
        )
        .await;
        assert!(opened.success, "{opened:?}");
        let snapshot_id = opened.data.as_ref().expect("open data")["snapshotId"]
            .as_str()
            .expect("snapshot id")
            .to_owned();
        let files = opened.data.as_ref().expect("open data")["files"]
            .as_array()
            .expect("files")
            .clone();
        assert!(
            files.len() > FILE_DIFF_CACHE_MAX_ENTRIES,
            "fixture must visit more files than the entry cap"
        );

        for file in &files {
            let path = file["path"].as_str().expect("path").to_owned();
            let response = handle_code_review_command(
                &app,
                &code_review,
                RpcCommand::CodeReviewFileDiff {
                    id: None,
                    snapshot_id: snapshot_id.clone(),
                    path,
                    cursor: 0,
                    max_lines: Some(25),
                },
            )
            .await;
            assert!(response.success, "{response:?}");
        }

        let cached = code_review_file_diff_cache_paths(&code_review).await;
        assert!(
            cached.len() <= FILE_DIFF_CACHE_MAX_ENTRIES,
            "cache must be bounded by the entry cap: {} entries",
            cached.len()
        );
        let first_path = files[0]["path"].as_str().expect("path").to_owned();
        let last_path = files[files.len() - 1]["path"].as_str().expect("path").to_owned();
        assert!(
            !cached.iter().any(|path| *path == first_path),
            "first visited (LRU) path must be evicted: {cached:?}"
        );
        assert!(
            cached.iter().any(|path| *path == last_path),
            "most recently used path must survive: {cached:?}"
        );

        let reloaded = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewFileDiff {
                id: None,
                snapshot_id: snapshot_id.clone(),
                path: first_path.clone(),
                cursor: 0,
                max_lines: Some(25),
            },
        )
        .await;
        assert!(reloaded.success, "{reloaded:?}");
        let page = reloaded.data.expect("page data");
        let hunk = &page["hunks"].as_array().expect("hunks")[0];
        let identity = (
            hunk["oldStart"].as_u64().expect("oldStart") as u32,
            hunk["oldCount"].as_u64().expect("oldCount") as u32,
            hunk["newStart"].as_u64().expect("newStart") as u32,
            hunk["newCount"].as_u64().expect("newCount") as u32,
            hunk["contentHash"].as_str().expect("hash").to_owned(),
        );

        let commented = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewComment {
                id: Some("comment-after-eviction".into()),
                snapshot_id,
                path: first_path,
                old_start: identity.0,
                old_count: identity.1,
                new_start: identity.2,
                new_count: identity.3,
                content_hash: identity.4,
                comment: "after eviction reload".into(),
                images: Vec::new(),
            },
        )
        .await;
        assert!(commented.success, "{commented:?}");
        let thread = commented.data.as_ref().expect("comment data")["threads"]
            .as_array()
            .expect("threads")
            .first()
            .expect("thread")
            .clone();
        assert_eq!(thread["comments"].as_array().expect("comments").len(), 1);

        let closed = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewClose { id: None, review_id: None },
        )
        .await;
        assert!(closed.success);
        app.cleanup().await;
    }

    #[tokio::test]
    async fn code_review_comment_resolves_cached_then_evicted_via_authority() {
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"]).status.success();
        git(&["config", "user.name", "Test"]).status.success();
        for index in 0..40 {
            std::fs::write(cwd.path().join(format!("f{index:02}.txt")), "base\n")
                .expect("write base");
        }
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "init"]).status.success());
        for index in 0..40 {
            std::fs::write(
                cwd.path().join(format!("f{index:02}.txt")),
                format!("base\nchanged-{index}\n"),
            )
            .expect("write changed");
        }
        assert!(git(&["add", "."]).status.success());

        let app = build_code_review_app(cwd.path()).await;
        let code_review = CodeReviewRpcState::default();
        let opened = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen {
                id: None,
                commit: None,
                from: None,
                to: None,
                include_working_tree: None,
            },
        )
        .await;
        assert!(opened.success, "{opened:?}");
        let snapshot_id = opened.data.as_ref().expect("open data")["snapshotId"]
            .as_str()
            .expect("snapshot id")
            .to_owned();
        let files = opened.data.as_ref().expect("open data")["files"]
            .as_array()
            .expect("files")
            .clone();
        let first_path = files[0]["path"].as_str().expect("path").to_owned();

        let first_page = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewFileDiff {
                id: None,
                snapshot_id: snapshot_id.clone(),
                path: first_path.clone(),
                cursor: 0,
                max_lines: Some(25),
            },
        )
        .await;
        assert!(first_page.success, "{first_page:?}");
        let page = first_page.data.expect("page data");
        let hunk = &page["hunks"].as_array().expect("hunks")[0];
        assert_eq!(hunk["complete"], true, "cached load serves complete hunks");
        let identity = (
            hunk["oldStart"].as_u64().expect("oldStart") as u32,
            hunk["oldCount"].as_u64().expect("oldCount") as u32,
            hunk["newStart"].as_u64().expect("newStart") as u32,
            hunk["newCount"].as_u64().expect("newCount") as u32,
            hunk["contentHash"].as_str().expect("hash").to_owned(),
        );

        for file in &files {
            let path = file["path"].as_str().expect("path").to_owned();
            let response = handle_code_review_command(
                &app,
                &code_review,
                RpcCommand::CodeReviewFileDiff {
                    id: None,
                    snapshot_id: snapshot_id.clone(),
                    path,
                    cursor: 0,
                    max_lines: Some(25),
                },
            )
            .await;
            assert!(response.success, "{response:?}");
        }

        let cached = code_review_file_diff_cache_paths(&code_review).await;
        assert!(
            !cached.iter().any(|path| *path == first_path),
            "the loaded entry must be evicted before the comment: {cached:?}"
        );
        {
            let state = code_review.state.lock().await;
            let session = state.session.as_ref().expect("open session");
            let records = session
                .comment_authority
                .lookup(&first_path)
                .expect("authority retained the cached-then-evicted identities");
            assert!(
                records.iter().any(|record| record.content_hash == identity.4),
                "authority must cover the identity captured while cached"
            );
        }

        let commented = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewComment {
                id: Some("comment-cached-then-evicted".into()),
                snapshot_id,
                path: first_path,
                old_start: identity.0,
                old_count: identity.1,
                new_start: identity.2,
                new_count: identity.3,
                content_hash: identity.4,
                comment: "resolved via retained authority".into(),
                images: Vec::new(),
            },
        )
        .await;
        assert!(
            commented.success,
            "comment must resolve via retained authority without a prior refetch: {commented:?}"
        );
        let thread = commented.data.as_ref().expect("comment data")["threads"]
            .as_array()
            .expect("threads")
            .first()
            .expect("thread")
            .clone();
        assert_eq!(thread["comments"].as_array().expect("comments").len(), 1);

        let closed = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewClose { id: None, review_id: None },
        )
        .await;
        assert!(closed.success);
        app.cleanup().await;
    }

    #[tokio::test]
    async fn file_explorer_rpc_lists_reads_pages_and_reports_worktree_changes() {
        let cwd = tempfile::tempdir().expect("file explorer cwd");
        std::fs::create_dir(cwd.path().join("dir")).expect("directory");
        std::fs::create_dir(cwd.path().join(".git-hidden-fixture")).expect("dot directory");
        std::fs::write(cwd.path().join("dir/nested.txt"), "nested\n").expect("nested file");
        std::fs::write(cwd.path().join("z.txt"), "one\ntwo\nthree\n").expect("text file");
        std::fs::write(cwd.path().join("binary.bin"), b"text\0binary").expect("binary file");
        std::fs::create_dir_all(cwd.path().join("removed/deep")).expect("deleted directory");
        std::fs::write(cwd.path().join("removed/deep/gone.txt"), "gone\n")
            .expect("deleted fixture");
        for index in 0..505 {
            std::fs::write(cwd.path().join(format!("cap-{index:03}.txt")), "x")
                .expect("cap fixture");
        }
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "baseline"]).status.success());
        std::fs::remove_dir_all(cwd.path().join("removed")).expect("delete tracked tree");
        std::fs::write(cwd.path().join("z.txt"), "one\nTWO\nthree\n").expect("modify text");
        std::fs::write(cwd.path().join("new.txt"), "new\nbody\n").expect("untracked file");

        let app = build_code_review_app(cwd.path()).await;
        let list = handle_file_explorer_command(
            &app,
            RpcCommand::FileExplorerList {
                id: Some("list".into()),
                path: None,
            },
        )
        .await;
        assert!(list.success, "{list:?}");
        let list_data = list.data.expect("list data");
        let entries = list_data["entries"].as_array().expect("entries");
        assert_eq!(entries[0]["kind"], "dir", "dirs sort before files: {entries:?}");
        assert!(!entries.iter().any(|entry| entry["name"] == ".git"));
        assert_eq!(entries.len(), 511);
        assert_eq!(list_data["truncated"], false);

        let first = handle_file_explorer_command(
            &app,
            RpcCommand::FileExplorerRead {
                id: None,
                path: "z.txt".into(),
                cursor: 0,
                max_lines: Some(2),
            },
        )
        .await;
        assert!(first.success, "{first:?}");
        let first_data = first.data.expect("first page");
        assert_eq!(first_data["lines"], json!(["one", "TWO"]));
        assert_eq!(first_data["nextCursor"], 2);
        assert_eq!(first_data["truncated"], true);
        let final_page = handle_file_explorer_command(
            &app,
            RpcCommand::FileExplorerRead {
                id: None,
                path: "z.txt".into(),
                cursor: 2,
                max_lines: Some(2),
            },
        )
        .await;
        let final_data = final_page.data.expect("final page");
        assert_eq!(final_data["lines"], json!(["three"]));
        assert!(final_data.get("nextCursor").is_none());
        assert_eq!(final_data["truncated"], false);

        let changes = handle_file_explorer_command(
            &app,
            RpcCommand::FileExplorerChanges { id: None },
        )
        .await;
        assert!(changes.success, "{changes:?}");
        let changes_data = changes.data.expect("changes");
        assert_eq!(changes_data["isRepo"], true);
        assert!(changes_data["files"].as_array().expect("files").iter().any(|file| {
            file["path"] == "z.txt" && file["status"] == "modified"
        }));
        assert!(changes_data["files"].as_array().expect("files").iter().any(|file| {
            file["path"] == "new.txt" && file["status"] == "added"
        }));
        assert!(changes_data["files"].as_array().expect("files").iter().any(|file| {
            file["path"] == "removed/deep/gone.txt" && file["status"] == "deleted"
        }));

        for path in ["z.txt", "new.txt", "removed/deep/gone.txt"] {
            let diff = handle_file_explorer_command(
                &app,
                RpcCommand::FileExplorerDiff {
                    id: None,
                    path: path.into(),
                    cursor: 0,
                    max_lines: None,
                },
            )
            .await;
            assert!(diff.success, "{path}: {diff:?}");
            let lines = diff.data.expect("diff")["lines"]
                .as_array()
                .expect("diff lines")
                .clone();
            assert!(lines.iter().any(|line| line.as_str().is_some_and(|line| line.starts_with("@@"))), "{path}: {lines:?}");
            if path == "new.txt" {
                assert!(lines.iter().any(|line| line == "+new"), "{lines:?}");
            }
            if path == "removed/deep/gone.txt" {
                assert!(lines.iter().any(|line| line == "-gone"), "{lines:?}");
            }
        }

        let binary = handle_file_explorer_command(
            &app,
            RpcCommand::FileExplorerRead {
                id: None,
                path: "binary.bin".into(),
                cursor: 0,
                max_lines: None,
            },
        )
        .await;
        assert_eq!(binary.error.as_deref(), Some("not a text file"));
        app.cleanup().await;
    }

    #[tokio::test]
    async fn file_explorer_rpc_rejects_unsafe_paths_and_non_files_without_leaking_paths() {
        let cwd = tempfile::tempdir().expect("file explorer cwd");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret.txt"), "secret\n").expect("outside file");
        std::fs::create_dir(cwd.path().join("dir")).expect("directory");
        std::fs::write(cwd.path().join("file.txt"), "safe\n").expect("safe file");
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), cwd.path().join("escape"))
            .expect("symlink escape");
        let app = build_code_review_app(cwd.path()).await;

        for path in [
            "../secret.txt".to_owned(),
            outside.path().join("secret.txt").to_string_lossy().into_owned(),
            "dir".to_owned(),
            #[cfg(unix)]
            "escape/secret.txt".to_owned(),
        ] {
            let response = handle_file_explorer_command(
                &app,
                RpcCommand::FileExplorerRead {
                    id: None,
                    path: path.clone(),
                    cursor: 0,
                    max_lines: None,
                },
            )
            .await;
            assert!(!response.success, "{response:?}");
            let error = response.error.expect("bounded error");
            if path == "dir" {
                assert_eq!(error, "not a file");
            }
            assert!(matches!(
                error.as_str(),
                "path is outside the workspace"
                    | "not a file"
                    | "file is unavailable"
                    | "file is a symlink"
            ), "{error}");
            assert!(!error.contains(cwd.path().to_string_lossy().as_ref()), "{error}");
            assert!(!error.contains(outside.path().to_string_lossy().as_ref()), "{error}");
        }

        for path in [
            "../outside".to_owned(),
            outside.path().to_string_lossy().into_owned(),
            "file.txt".to_owned(),
            #[cfg(unix)]
            "escape".to_owned(),
        ] {
            let response = handle_file_explorer_command(
                &app,
                RpcCommand::FileExplorerList {
                    id: None,
                    path: Some(path),
                },
            )
            .await;
            assert!(!response.success, "{response:?}");
            let error = response.error.expect("list error");
            assert!(matches!(
                error.as_str(),
                "path is outside the workspace" | "not a directory" | "directory is unavailable"
            ), "{error}");
            assert!(!error.contains(cwd.path().to_string_lossy().as_ref()), "{error}");
            assert!(!error.contains(outside.path().to_string_lossy().as_ref()), "{error}");
        }
        app.cleanup().await;
    }

    #[tokio::test]
    async fn file_explorer_changes_non_repo_returns_false_and_empty_files() {
        let cwd = tempfile::tempdir().expect("non-repo cwd");
        std::fs::write(cwd.path().join("plain.txt"), "plain\n").expect("plain file");
        let app = build_code_review_app(cwd.path()).await;
        let response = handle_file_explorer_command(
            &app,
            RpcCommand::FileExplorerChanges { id: None },
        )
        .await;
        assert!(response.success, "{response:?}");
        let data = response.data.expect("changes response");
        assert_eq!(data, json!({"files": [], "isRepo": false, "truncated": false}));
        app.cleanup().await;
    }

    #[tokio::test]
    async fn file_explorer_rpc_attaches_typed_error_codes_independently_of_prose() {
        let cwd = tempfile::tempdir().expect("file explorer cwd");
        std::fs::write(
            cwd.path().join("long.txt"),
            format!("{}", "z".repeat(crate::file_explorer::MAX_PAGE_BYTES + 1)),
        )
        .expect("oversized line");
        std::fs::write(cwd.path().join("binary.bin"), b"text\0binary").expect("binary file");
        std::fs::create_dir_all(cwd.path().join("removed/deep")).expect("deleted directory");
        std::fs::write(cwd.path().join("removed/deep/gone.txt"), "gone\n")
            .expect("deleted fixture");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "baseline"]).status.success());
        std::fs::remove_dir_all(cwd.path().join("removed")).expect("delete tracked tree");

        let app = build_code_review_app(cwd.path()).await;

        let too_large = handle_file_explorer_command(
            &app,
            RpcCommand::FileExplorerRead {
                id: None,
                path: "long.txt".into(),
                cursor: 0,
                max_lines: None,
            },
        )
        .await;
        assert!(!too_large.success, "{too_large:?}");
        assert_eq!(
            too_large.error.as_deref(),
            Some("line exceeds response limit")
        );
        assert_eq!(too_large.data, Some(json!({"code": "file_line_too_large"})));

        let missing = handle_file_explorer_command(
            &app,
            RpcCommand::FileExplorerRead {
                id: None,
                path: "removed/deep/gone.txt".into(),
                cursor: 0,
                max_lines: None,
            },
        )
        .await;
        assert!(!missing.success, "{missing:?}");
        assert_eq!(missing.error.as_deref(), Some("file is unavailable"));
        assert_eq!(missing.data, Some(json!({"code": "file_unavailable"})));

        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().expect("outside");
            std::fs::write(outside.path().join("secret.txt"), "secret").expect("secret");
            std::os::unix::fs::symlink(
                outside.path().join("secret.txt"),
                cwd.path().join("escape.txt"),
            )
            .expect("symlink");
            let symlink = handle_file_explorer_command(
                &app,
                RpcCommand::FileExplorerRead {
                    id: None,
                    path: "escape.txt".into(),
                    cursor: 0,
                    max_lines: None,
                },
            )
            .await;
            assert!(!symlink.success, "{symlink:?}");
            assert_eq!(symlink.error.as_deref(), Some("file is a symlink"));
            assert_eq!(symlink.data, Some(json!({"code": "file_symlink"})));
        }

        let binary = handle_file_explorer_command(
            &app,
            RpcCommand::FileExplorerRead {
                id: None,
                path: "binary.bin".into(),
                cursor: 0,
                max_lines: None,
            },
        )
        .await;
        assert!(!binary.success, "{binary:?}");
        assert_eq!(binary.error.as_deref(), Some("not a text file"));
        assert!(binary.data.is_none(), "{binary:?}");

        app.cleanup().await;
    }

    #[tokio::test]
    async fn file_explorer_read_resolves_unique_basename_from_transcript_prose() {
        // A basename-only citation (`init.rs:495` in the audit findings)
        // misses at the workspace root yet names a real file deeper in the
        // tree: the unique-basename fallback reads it and echoes the real
        // workspace-relative path. Two REAL homonyms in eligible subtrees
        // keep the stable missing-file error (the request never guesses),
        // a file only under the skipped `target/` subtree stays missing,
        // and a deeper missing path never triggers the basename walk.
        let cwd = tempfile::tempdir().expect("file explorer cwd");
        std::fs::create_dir_all(cwd.path().join("psy_node_common/src/realm/processor/db"))
            .expect("nested dirs");
        std::fs::write(cwd.path().join("psy_node_common/src/realm/processor/db/init.rs"), "unique body\n")
            .expect("unique file");
        std::fs::write(cwd.path().join("gone.rs"), "eligible competing file\n").expect("root gone.rs");
        std::fs::create_dir_all(cwd.path().join("crates/alpha/src")).expect("alpha dirs");
        std::fs::create_dir_all(cwd.path().join("crates/beta/src")).expect("beta dirs");
        std::fs::write(cwd.path().join("crates/alpha/src/homonym.rs"), "alpha body\n")
            .expect("alpha homonym");
        std::fs::write(cwd.path().join("crates/beta/src/homonym.rs"), "beta body\n")
            .expect("beta homonym");
        std::fs::create_dir_all(cwd.path().join("target/debug")).expect("target dir");
        std::fs::write(cwd.path().join("target/debug/target_only.rs"), "build artifact\n")
            .expect("target file");
        let app = build_code_review_app(cwd.path()).await;
        let read = |path: &'static str, max_lines: Option<usize>| {
            let app = app.clone();
            async move {
                handle_file_explorer_command(
                    &app,
                    RpcCommand::FileExplorerRead {
                        id: None,
                        path: path.into(),
                        cursor: 0,
                        max_lines,
                    },
                )
                .await
            }
        };

        let unique = read("init.rs", Some(5)).await;
        assert!(unique.success, "{unique:?}");
        let data = unique.data.expect("unique read data");
        assert_eq!(data["path"], "psy_node_common/src/realm/processor/db/init.rs");
        assert_eq!(data["lines"], json!(["unique body"]));

        let homonym = read("homonym.rs", Some(5)).await;
        assert!(!homonym.success, "{homonym:?}");
        assert_eq!(homonym.error.as_deref(), Some("file is unavailable"));
        assert_eq!(homonym.data, Some(json!({"code": "file_unavailable"})));

        let target_only = read("target_only.rs", Some(5)).await;
        assert!(!target_only.success, "{target_only:?}");
        assert_eq!(target_only.error.as_deref(), Some("file is unavailable"));

        let deeper_missing = read("realm/edge/gone.rs", Some(5)).await;
        assert!(!deeper_missing.success, "{deeper_missing:?}");
        assert_eq!(deeper_missing.error.as_deref(), Some("file is unavailable"));

        app.cleanup().await;
    }

    #[tokio::test]
    async fn side_chat_rpc_round_trip_creates_prompts_switches_and_closes_tabs() {
        let (_cwd, _agent, app) =
            build_side_chat_app("faux-rpc-side", reply_stream("side reply")).await;
        let side_chat = SideChatRpcState::default();
        let dispatch = |command: RpcCommand| handle_side_chat_command(&app, &side_chat, command);
        let listed = dispatch(RpcCommand::SideChatList {
            id: Some("list-1".into()),
        })
        .await;
        assert!(listed.success, "{listed:?}");
        assert_eq!(listed.command, "side_chat_list");
        let data = listed.data.expect("snapshot");
        assert_eq!(data["active"], "default", "{data}");
        assert_eq!(data["tabs"].as_array().expect("tabs").len(), 1);

        let created = dispatch(RpcCommand::SideChatNew {
            id: Some("new-1".into()),
            name: "research".into(),
        })
        .await;
        assert!(created.success, "{created:?}");
        assert_eq!(created.data.expect("snapshot")["active"], "research");

        let prompted = dispatch(RpcCommand::SideChatPrompt {
            id: Some("prompt-1".into()),
            message: "side hello".into(),
            images: vec![],
        })
        .await;
        assert!(prompted.success, "{prompted:?}");
        let data = prompted.data.expect("snapshot");
        assert_eq!(data["accepted"], json!(true), "{data}");
        assert_eq!(data["busy"], json!(false));

        let mut saw_reply = false;
        for _ in 0..100 {
            let listed = dispatch(RpcCommand::SideChatList { id: None }).await;
            let data = listed.data.expect("snapshot");
            let active = data["tabs"]
                .as_array()
                .expect("tabs")
                .iter()
                .find(|tab| tab["name"] == "research")
                .expect("research tab");
            if active["entries"]
                .as_array()
                .expect("entries")
                .iter()
                .any(|entry| {
                    entry["role"] == "assistant"
                        && entry["text"]
                            .as_str()
                            .is_some_and(|text| text.contains("side reply"))
                })
            {
                saw_reply = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            saw_reply,
            "side-chat assistant reply must appear in the snapshot"
        );

        assert!(
            app.messages()
                .iter()
                .all(|message| !format!("{message:?}").contains("side hello")),
            "side-chat prompt must never enter the main transcript"
        );

        let switched = dispatch(RpcCommand::SideChatSwitch {
            id: None,
            name: "default".into(),
        })
        .await;
        assert!(switched.success, "{switched:?}");
        assert_eq!(switched.data.expect("snapshot")["active"], "default");

        let closed = dispatch(RpcCommand::SideChatClose {
            id: None,
            name: Some("research".into()),
        })
        .await;
        assert!(closed.success, "{closed:?}");
        let data = closed.data.expect("snapshot");
        assert_eq!(data["active"], "default");
        let names = data["tabs"]
            .as_array()
            .expect("tabs")
            .iter()
            .map(|tab| tab["name"].as_str().expect("tab name"))
            .collect::<Vec<_>>();
        assert!(!names.contains(&"research"), "{names:?}");

        side_chat.shutdown().await;
        app.cleanup().await;
    }

    #[tokio::test]
    async fn side_chat_rpc_rejects_busy_prompt_and_unknown_switch() {
        let release = Arc::new(tokio::sync::Notify::new());
        let gated_stream: pi_agent::StreamFn = {
            let release = release.clone();
            Arc::new(move |model, _context, _options| {
                let release = release.clone();
                Box::pin(async move {
                    let stream = pi_ai::new_assistant_message_event_stream();
                    let producer = stream.clone();
                    tokio::spawn(async move {
                        release.notified().await;
                        let mut message = pi_ai::AssistantMessage::pending(&model);
                        message.content.push(ContentBlock::text("side reply"));
                        message.stop_reason = pi_ai::StopReason::Stop;
                        producer.end(Some(message)).await;
                    });
                    stream
                })
            })
        };
        let (_cwd, _agent, app) = build_side_chat_app("faux-rpc-side-busy", gated_stream).await;
        let side_chat = SideChatRpcState::default();
        let dispatch = |command: RpcCommand| handle_side_chat_command(&app, &side_chat, command);

        let switched = dispatch(RpcCommand::SideChatSwitch {
            id: Some("missing-switch".into()),
            name: "nope".into(),
        })
        .await;
        assert!(!switched.success, "{switched:?}");
        assert_eq!(switched.command, "side_chat_switch");
        assert_eq!(switched.id.as_deref(), Some("missing-switch"));
        assert_eq!(
            switched.error.as_deref(),
            Some(r#"no side tab named "nope""#),
            "{switched:?}"
        );

        let invalid = dispatch(RpcCommand::SideChatNew {
            id: None,
            name: "new".into(), // reserved /btw word
        })
        .await;
        assert!(!invalid.success, "{invalid:?}");
        assert!(
            invalid
                .error
                .as_deref()
                .is_some_and(|e| e.contains("reserved"))
        );

        let first = dispatch(RpcCommand::SideChatPrompt {
            id: Some("prompt-first".into()),
            message: "first prompt".into(),
            images: vec![],
        })
        .await;
        assert!(first.success, "{first:?}");
        let first_data = first.data.expect("first prompt snapshot");
        assert_eq!(first_data["accepted"], json!(true), "{first_data}");
        assert_eq!(first_data["busy"], json!(false), "{first_data}");

        let busy = dispatch(RpcCommand::SideChatPrompt {
            id: Some("prompt-busy".into()),
            message: "must not be submitted".into(),
            images: vec![],
        })
        .await;
        assert!(busy.success, "{busy:?}");
        let busy_data = busy.data.expect("busy prompt snapshot");
        assert_eq!(busy_data["accepted"], json!(false), "{busy_data}");
        assert_eq!(busy_data["busy"], json!(true), "{busy_data}");
        let default = busy_data["tabs"]
            .as_array()
            .expect("tabs")
            .iter()
            .find(|tab| tab["name"] == "default")
            .expect("default tab");
        assert!(
            default["entries"]
                .as_array()
                .expect("entries")
                .iter()
                .all(|entry| entry["text"] != "must not be submitted"),
            "busy prompt must not enter the side transcript: {default}"
        );

        let aborted = dispatch(RpcCommand::SideChatAbort {
            id: Some("prompt-abort".into()),
        })
        .await;
        assert!(aborted.success, "{aborted:?}");

        let second = dispatch(RpcCommand::SideChatPrompt {
            id: Some("prompt-second".into()),
            message: "second prompt".into(),
            images: vec![],
        })
        .await;
        assert!(second.success, "{second:?}");
        let second_data = second.data.expect("second prompt snapshot");
        assert_eq!(second_data["accepted"], json!(true), "{second_data}");
        assert_eq!(second_data["busy"], json!(false), "{second_data}");

        release.notify_one();
        side_chat.shutdown().await;
        app.cleanup().await;
    }

    #[tokio::test]
    async fn skill_rpc_returns_frontmatter_summary_and_rejects_unknown() {
        let (_cwd, _agent_dir, app) = build_command_catalog_app().await;
        let ok = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::Skill {
                id: Some("skill-ok".into()),
                name: "catalog-skill".into(),
            },
        )
        .await;
        assert!(ok.success, "{ok:?}");
        assert_eq!(ok.command, "skill");
        let data = ok.data.expect("skill data");
        assert_eq!(data["name"], "catalog-skill");
        let summary = data["summary"].as_str().expect("summary text");
        assert!(summary.contains("name: catalog-skill"), "{summary}");
        assert!(
            summary.contains("description: Skill only command"),
            "{summary}"
        );

        let empty = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::Skill {
                id: Some("skill-empty".into()),
                name: "   ".into(),
            },
        )
        .await;
        assert!(!empty.success, "{empty:?}");
        assert!(
            empty
                .error
                .as_deref()
                .is_some_and(|error| error.contains("required")),
            "{empty:?}"
        );

        let unknown = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::Skill {
                id: Some("skill-unknown".into()),
                name: "nope".into(),
            },
        )
        .await;
        assert!(!unknown.success, "{unknown:?}");
        assert!(
            unknown
                .error
                .as_deref()
                .is_some_and(|error| error.contains("unknown skill")),
            "{unknown:?}"
        );
        app.cleanup().await;
    }

    #[tokio::test]
    async fn run_rpc_invokes_trusted_extension_command_and_rejects_unknown() {
        let (_cwd, _agent_dir, app) = build_command_catalog_app().await;
        let ok = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::Run {
                id: Some("run-ok".into()),
                name: "extension-only".into(),
                args: String::new(),
            },
        )
        .await;
        assert!(ok.success, "{ok:?}");
        assert_eq!(ok.command, "run");
        let data = ok.data.expect("run data");
        assert_eq!(data["name"], "extension-only");
        assert_eq!(data["value"], "pong");

        let empty = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::Run {
                id: Some("run-empty".into()),
                name: "   ".into(),
                args: String::new(),
            },
        )
        .await;
        assert!(!empty.success, "{empty:?}");
        assert!(
            empty
                .error
                .as_deref()
                .is_some_and(|error| error.contains("required")),
            "{empty:?}"
        );

        let unknown = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::Run {
                id: Some("run-unknown".into()),
                name: "nope".into(),
                args: String::new(),
            },
        )
        .await;
        assert!(!unknown.success, "{unknown:?}");
        assert!(
            unknown
                .error
                .as_deref()
                .is_some_and(|error| error.contains("unknown or untrusted")),
            "{unknown:?}"
        );
        app.cleanup().await;
    }

    #[tokio::test]
    async fn get_commands_projects_skill_argument_metadata() {
        let (_cwd, _agent_dir, app) = build_command_catalog_app().await;
        let response = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::GetCommands {
                id: Some("catalog-meta".into()),
            },
        )
        .await;
        assert!(response.success, "{response:?}");
        let commands = response.data.expect("data")["commands"]
            .as_array()
            .expect("commands")
            .clone();
        let skill = commands
            .iter()
            .find(|command| command["name"] == "skill")
            .expect("skill is a visible primary command");
        assert_eq!(skill["source"], "builtin");
        assert_eq!(skill["argumentHint"], "<name>");
        assert_eq!(skill["requiresArguments"], true);
        let code_review = commands
            .iter()
            .find(|command| command["name"] == "code-review")
            .expect("code-review is a visible primary command");
        assert_eq!(code_review["argumentHint"], "[<base> | <from> <to>]");
        assert_eq!(code_review["requiresArguments"], false);
        app.cleanup().await;
    }

    #[tokio::test]
    async fn code_review_open_on_non_git_cwd_surfaces_error_without_root() {
        let cwd = tempfile::tempdir().expect("non-git cwd");
        let app = build_code_review_app(cwd.path()).await;
        let code_review = CodeReviewRpcState::default();
        let response = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen {
                id: Some("open-non-git".into()),
                commit: None,
                from: None,
                to: None,
                include_working_tree: None,
            },
        )
        .await;
        assert!(response.success, "{response:?}");
        assert_eq!(response.command, "code_review_open");
        let data = response.data.expect("open data");
        let encoded = serde_json::to_string(&data).unwrap();
        assert!(
            !encoded.contains("\"root\""),
            "root must not be exposed: {encoded}"
        );
        assert!(data["files"].as_array().expect("files").is_empty());
        assert!(
            data["error"]
                .as_str()
                .is_some_and(|error| !error.is_empty()),
            "non-git open must surface an error payload: {data}"
        );
        assert_eq!(data["comparisonLabel"], "HEAD → working tree");
        assert_eq!(data["isStreaming"], false);
        assert_eq!(data["activeCount"], 0);
        assert_eq!(data["threads"].as_array().expect("threads").len(), 0);

        let snap = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewSnapshot {
                id: Some("snap-non-git".into()),
            },
        )
        .await;
        assert!(snap.success, "{snap:?}");
        assert!(snap.data.expect("snap data")["error"].as_str().is_some());

        let closed = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewClose { id: Some("close-non-git".into()), review_id: None },
        )
        .await;
        assert!(closed.success, "{closed:?}");
        assert_eq!(closed.data.expect("close data")["closed"], true);

        let after = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewSnapshot {
                id: Some("snap-after-close".into()),
            },
        )
        .await;
        assert!(!after.success, "{after:?}");
        assert_eq!(
            after.data.as_ref().and_then(|data| data.get("code")).and_then(|code| code.as_str()),
            Some("code_review_not_open"),
            "a snapshot of a closed review is the typed no-open rejection: {after:?}"
        );
        assert_eq!(
            after.error.as_deref(),
            Some("no open code review; call code_review_open first"),
        );
        app.cleanup().await;
    }

    #[tokio::test]
    async fn code_review_open_on_real_git_working_tree_lists_changed_files() {
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(
            git(&["init", "-q"]).status.success(),
            "git init must succeed"
        );
        git(&["config", "user.email", "test@example.com"])
            .status
            .success();
        git(&["config", "user.name", "Test"]).status.success();
        std::fs::write(cwd.path().join("README.md"), "initial\n").expect("write");

        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "init"]).status.success());
        std::fs::write(cwd.path().join("README.md"), "initial\nchanged\n").expect("write");
        std::fs::write(cwd.path().join("new.txt"), "new file\n").expect("write");
        assert!(git(&["add", "."]).status.success());

        let app = build_code_review_app(cwd.path()).await;
        let code_review = CodeReviewRpcState::default();
        let response = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen {
                id: Some("open-git".into()),
                commit: None,
                from: None,
                to: None,
                include_working_tree: None,
            },
        )
        .await;
        assert!(response.success, "{response:?}");
        let data = response.data.expect("open data");
        assert!(
            data["error"].is_null(),
            "no error expected on a real repo: {data}"
        );
        let encoded = serde_json::to_string(&data).unwrap();
        assert!(
            !encoded.contains("\"root\""),
            "root must not be exposed: {encoded}"
        );
        let files = data["files"].as_array().expect("files");
        let paths: Vec<&str> = files
            .iter()
            .map(|file| file["path"].as_str().expect("path"))
            .collect();
        assert!(
            paths.contains(&"README.md"),
            "modified file must appear: {paths:?}"
        );
        assert!(
            paths.contains(&"new.txt"),
            "added file must appear: {paths:?}"
        );

        let readme = files
            .iter()
            .find(|file| file["path"] == "README.md")
            .expect("readme");
        let hunks = readme["hunks"].as_array().expect("hunks");
        assert!(!hunks.is_empty(), "modified file must have a hunk");
        let hunk = &hunks[0];
        assert!(
            hunk["contentHash"]
                .as_str()
                .is_some_and(|hash| !hash.is_empty()),
            "hunk must carry a content hash"
        );
        assert!(hunk["oldStart"].as_u64().is_some());
        assert!(hunk["newStart"].as_u64().is_some());
        assert!(
            hunk["lines"]
                .as_array()
                .expect("lines")
                .iter()
                .any(|line| line["kind"] == "addition"),
            "hunk must include at least one addition line"
        );

        assert_eq!(data["comparisonLabel"], "HEAD → working tree");
        assert!(
            data["totalInsertions"].as_u64().expect("insertions") >= 2,
            "expected at least two inserted lines: {data}"
        );
        assert_eq!(data["isStreaming"], false);

        let half = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen {
                id: Some("open-half".into()),
                commit: None,
                from: Some("HEAD".into()),
                to: None,
                include_working_tree: None,
            },
        )
        .await;
        assert!(!half.success, "{half:?}");
        assert!(
            half.error.as_deref().is_some_and(|e| e.contains("from")
                && e.contains("to")
                && e.contains("includeWorkingTree")),
            "{half:?}"
        );

        let base_to_tree = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen {
                id: Some("open-base".into()),
                commit: None,
                from: Some("HEAD".into()),
                to: None,
                include_working_tree: Some(true),
            },
        )
        .await;
        assert!(base_to_tree.success, "{base_to_tree:?}");
        let base_data = base_to_tree.data.expect("base-to-tree data");
        assert_eq!(base_data["comparisonLabel"], "HEAD → working tree");
        assert!(base_data["error"].is_null(), "{base_data}");
        assert!(
            base_data["files"]
                .as_array()
                .expect("files")
                .iter()
                .any(|file| file["path"] == "README.md"),
            "HEAD with includeWorkingTree must keep working-tree files: {base_data}"
        );

        let closed = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewClose { id: Some("close-git".into()), review_id: None },
        )
        .await;
        assert!(closed.success, "{closed:?}");
        assert_eq!(closed.data.expect("close data")["closed"], true);
        app.cleanup().await;
    }

    #[test]
    fn code_review_open_commit_and_commits_wire_decode() {
        let parse_command = |value: Value| match parse_input(
            &serde_json::to_vec(&value).expect("serialize command fixture"),
        )
        .expect("parse command fixture")
        {
            RpcInput::Command { command, .. } => command,
            RpcInput::ExtensionUiResponse(_) => panic!("expected RPC command"),
        };
        match parse_command(json!({"type":"code_review_open","commit":"abc123"})) {
            RpcCommand::CodeReviewOpen {
                commit,
                from,
                to,
                include_working_tree,
                ..
            } => {
                assert_eq!(commit.as_deref(), Some("abc123"));
                assert!(from.is_none() && to.is_none());
                assert_eq!(include_working_tree, None);
            }
            other => panic!("expected code_review_open: {other:?}"),
        }
        // An explicit `includeWorkingTree: false` must stay distinguishable
        // from an absent operand: it counts as supplied for exclusivity.
        match parse_command(json!({"type":"code_review_open","includeWorkingTree":false})) {
            RpcCommand::CodeReviewOpen {
                include_working_tree,
                ..
            } => {
                assert_eq!(include_working_tree, Some(false));
            }
            other => panic!("expected code_review_open: {other:?}"),
        }
        match parse_command(json!({"type":"code_review_commits"})) {
            RpcCommand::CodeReviewCommits { limit, .. } => assert_eq!(limit, None),
            other => panic!("expected code_review_commits: {other:?}"),
        }
        match parse_command(json!({"type":"code_review_commits","limit":5})) {
            RpcCommand::CodeReviewCommits { limit, .. } => assert_eq!(limit, Some(5)),
            other => panic!("expected code_review_commits: {other:?}"),
        }
        // Null is fail-closed on the scope-selecting operands: an explicit
        // `null` is a parse-time rejection, never "absent".
        for fixture in [
            r#"{"type":"code_review_open","commit":null}"#,
            r#"{"type":"code_review_open","includeWorkingTree":null}"#,
        ] {
            let rejected = parse_input(fixture.as_bytes())
                .expect_err("an explicit null operand must be rejected");
            assert!(
                rejected
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("invalid type")),
                "explicit null must fail closed: {rejected:?}"
            );
        }
        // `from`/`to` keep the legacy null-means-absent baseline: mixing a
        // commit with `"from": null` is still a pure commit scope.
        match parse_command(json!({"type":"code_review_open","commit":"abc123","from":null})) {
            RpcCommand::CodeReviewOpen { commit, from, .. } => {
                assert_eq!(commit.as_deref(), Some("abc123"));
                assert!(from.is_none());
            }
            other => panic!("expected code_review_open: {other:?}"),
        }
    }

    #[tokio::test]
    async fn code_review_open_commit_scope_rejects_conflated_operands() {
        let cwd = tempfile::tempdir().expect("cwd");
        let app = build_code_review_app(cwd.path()).await;
        let code_review = CodeReviewRpcState::default();
        for (commit, from, to, include_working_tree) in [
            (Some("HEAD".to_owned()), Some("HEAD".to_owned()), None, None),
            (Some("HEAD".to_owned()), None, Some("main".to_owned()), None),
            (Some("HEAD".to_owned()), None, None, Some(true)),
            (Some("HEAD".to_owned()), None, None, Some(false)),
        ] {
            let response = handle_code_review_command(
                &app,
                &code_review,
                RpcCommand::CodeReviewOpen {
                    id: Some("open-conflated".into()),
                    commit,
                    from,
                    to,
                    include_working_tree,
                },
            )
            .await;
            assert!(!response.success, "{response:?}");
            assert!(
                response
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("commit") && error.contains("excludes")),
                "conflated operands must be rejected: {response:?}"
            );
        }
        let empty = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen {
                id: Some("open-empty".into()),
                commit: Some("   ".to_owned()),
                from: None,
                to: None,
                include_working_tree: None,
            },
        )
        .await;
        assert!(!empty.success, "{empty:?}");
        assert!(
            empty
                .error
                .as_deref()
                .is_some_and(|error| error.contains("non-empty")),
            "an empty commit must be rejected: {empty:?}"
        );
        app.cleanup().await;
    }

    #[tokio::test]
    async fn code_review_open_commit_pins_root_commit_on_real_git_repo() {
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"]).status.success();
        git(&["config", "user.name", "Test"]).status.success();
        std::fs::write(cwd.path().join("README.md"), "root\n").expect("write");
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "root commit"]).status.success());
        let full_hash = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
            .expect("rev-parse utf8")
            .trim()
            .to_owned();

        let app = build_code_review_app(cwd.path()).await;
        let code_review = CodeReviewRpcState::default();
        let response = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen {
                id: Some("open-root".into()),
                commit: Some("HEAD".into()),
                from: None,
                to: None,
                include_working_tree: None,
            },
        )
        .await;
        assert!(response.success, "{response:?}");
        let data = response.data.expect("open data");
        assert!(
            data["error"].is_null(),
            "a root commit must open without a scope error: {data}"
        );
        // The label names the empty-tree comparison and pins the RESOLVED
        // object id, not the requested "HEAD" ref.
        let label = data["comparisonLabel"].as_str().expect("comparison label");
        let pinned = label
            .strip_prefix("empty tree → ")
            .expect("root commit label names the empty tree");
        assert!(
            full_hash.starts_with(pinned),
            "label must pin the resolved hash: {label} vs {full_hash}"
        );
        let files = data["files"].as_array().expect("files");
        assert!(
            files.iter().any(|file| file["path"] == "README.md"),
            "the root commit diff must list its committed file: {files:?}"
        );

        // Pinning: a follow-up snapshot reuses the resolved scope.
        let snap = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewSnapshot {
                id: Some("snap-root".into()),
            },
        )
        .await;
        assert!(snap.success, "{snap:?}");
        assert_eq!(
            snap.data.expect("snapshot data")["comparisonLabel"],
            data["comparisonLabel"]
        );
        app.cleanup().await;
    }

    #[tokio::test]
    async fn code_review_commits_lists_recent_commits_on_real_git_repo() {
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"]).status.success();
        git(&["config", "user.name", "Test"]).status.success();
        std::fs::write(cwd.path().join("README.md"), "base\n").expect("write");
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "root commit"]).status.success());
        std::fs::write(cwd.path().join("README.md"), "base\nchanged\n").expect("write");
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "second commit"]).status.success());
        let head_hash = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
            .expect("rev-parse utf8")
            .trim()
            .to_owned();
        let root_hash = String::from_utf8(git(&["rev-parse", "HEAD~1"]).stdout)
            .expect("rev-parse utf8")
            .trim()
            .to_owned();

        let app = build_code_review_app(cwd.path()).await;
        let code_review = CodeReviewRpcState::default();
        let response = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewCommits {
                id: Some("commits-1".into()),
                limit: Some(1),
            },
        )
        .await;
        assert!(response.success, "{response:?}");
        let data = response.data.expect("commits data");
        let rows = data["commits"].as_array().expect("commits array");
        assert_eq!(rows.len(), 1, "limit 1 returns one row: {data}");
        assert_eq!(
            data["truncated"],
            true,
            "an extra row existed past the limit"
        );
        assert_eq!(rows[0]["commit"], head_hash.as_str());
        assert_eq!(rows[0]["parentCommit"], root_hash.as_str());
        assert_eq!(rows[0]["parentCount"], 1);
        assert_eq!(rows[0]["subject"], "second commit");
        assert_eq!(rows[0]["author"], "Test");
        let author_date = rows[0]["authorDate"].as_str().expect("authorDate");
        assert!(
            author_date.contains('T'),
            "authorDate must be a strict ISO 8601 string: {author_date}"
        );

        let full = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewCommits {
                id: Some("commits-all".into()),
                limit: None,
            },
        )
        .await;
        assert!(full.success, "{full:?}");
        let full_data = full.data.expect("full list data");
        let full_rows = full_data["commits"].as_array().expect("commits array");
        assert_eq!(
            full_rows.len(),
            2,
            "the default limit lists both commits: {full_data}"
        );
        assert_eq!(full_data["truncated"], false);
        assert_eq!(full_rows[1]["commit"], root_hash.as_str());
        assert!(
            full_rows[1]["parentCommit"].is_null(),
            "a root commit has no first parent: {full_rows:?}"
        );
        assert_eq!(full_rows[1]["parentCount"], 0);
        assert_eq!(full_rows[1]["subject"], "root commit");
        app.cleanup().await;
    }

    /// Per-hunk provider stream that blocks each reply on the semaphore so an
    /// auto review batch stays provably running between dispatches.
    fn gated_rpc_review_stream(gate: Arc<tokio::sync::Semaphore>) -> pi_agent::StreamFn {
        Arc::new(move |model, context, _options| {
            let gate = gate.clone();
            let prompt = review_request_text(&context);
            let triage = prompt.strip_prefix("Classify every hunk").map(|text| {
                text.lines()
                    .filter_map(|line| line.strip_prefix("Hunk: "))
                    .collect::<Vec<_>>()
                    .join(",")
            });
            Box::pin(async move {
                let stream = pi_ai::new_assistant_message_event_stream();
                let producer = stream.clone();
                tokio::spawn(async move {
                    let reply = if let Some(core) = triage {
                        let tokens: Vec<&str> = if core.is_empty() {
                            Vec::new()
                        } else {
                            core.split(',').collect()
                        };
                        serde_json::json!({"core": tokens, "skipped": 0}).to_string()
                    } else {
                        gate.acquire().await.expect("release permit").forget();
                        findings_reply_for_request(&prompt, &[])
                    };
                    let mut message = pi_ai::AssistantMessage::pending(&model);
                    message.content.push(pi_ai::ContentBlock::text(reply));
                    message.stop_reason = pi_ai::StopReason::Stop;
                    producer.end(Some(message)).await;
                });
                stream
            })
        })
    }

    #[tokio::test]
    async fn code_review_auto_review_submits_and_second_batch_rejects() {
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"]).status.success();
        git(&["config", "user.name", "Test"]).status.success();
        std::fs::write(cwd.path().join("README.md"), "initial\n").expect("write");
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "init"]).status.success());
        std::fs::write(cwd.path().join("README.md"), "initial\nchanged\n").expect("write");
        assert!(git(&["add", "."]).status.success());

        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let app = build_code_review_app_with_stream(cwd.path(), gated_rpc_review_stream(gate.clone())).await;
        let code_review = CodeReviewRpcState::default();
        let opened = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen {
                id: Some("open-auto".into()),
                commit: None,
                from: None,
                to: None,
                include_working_tree: None,
            },
        )
        .await;
        assert!(opened.success, "{opened:?}");
        {
            let mut state = code_review.state.lock().await;
            let session = state.session.as_mut().expect("open review");
            let mut model = session.controller.fork_model().clone();
            model.api = pi_ai::API_OPENAI_COMPLETIONS.to_owned();
            session.controller.set_model(model);
            session.controller.start_auto_review(&session.snapshot, None, None).await.expect("triage accepted");
            let projection = code_review_projection(&session.snapshot, &session.controller);
            assert_eq!(projection["autoReviewState"], "triaging");
            assert_eq!(projection["reviewLanguageDefault"], "zh");
            assert!(projection["threads"].as_array().expect("threads").is_empty());
            session.controller.abort().await;
        }

        let auto = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewAutoReview {
                id: Some("auto-run".into()),
                model_id: None,
                language: None,
            },
        )
        .await;
        assert!(auto.success, "{auto:?}");
        let data = auto.data.expect("auto data");
        assert_eq!(data["ok"], true);
        assert_eq!(data["state"], "triaging");
        assert!(data["batchId"].as_u64().is_some(), "{data}");

        let second = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewAutoReview {
                id: Some("auto-second".into()),
                model_id: None,
                language: None,
            },
        )
        .await;
        assert!(!second.success, "{second:?}");
        assert_eq!(
            second.data.as_ref().and_then(|data| data["code"].as_str()),
            Some("code_review_auto_running"),
            "{second:?}"
        );

        let unknown = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewAutoReview {
                id: Some("auto-unknown".into()),
                model_id: Some("no-such-provider/definitely-not-a-model".into()),
                language: None,
            },
        )
        .await;
        assert!(!unknown.success, "{unknown:?}");
        assert_eq!(
            unknown.data.as_ref().and_then(|data| data["code"].as_str()),
            Some("code_review_unknown_model"),
            "{unknown:?}"
        );

        let snapshot = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewSnapshot {
                id: Some("snap-auto".into()),
            },
        )
        .await;
        let mut data = snapshot.data.expect("snapshot data");
        for _ in 0..100 {
            if data["autoReviewState"] == "running" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let snapshot = handle_code_review_command(
                &app,
                &code_review,
                RpcCommand::CodeReviewSnapshot {
                    id: Some("snap-auto".into()),
                },
            )
            .await;
            data = snapshot.data.expect("snapshot data");
        }
        assert_eq!(data["autoReviewState"], "running", "{data}");
        let submitted = data["autoReviewCounts"]["submitted"].as_u64().expect("submitted");
        assert!(submitted >= 1, "the changed hunk is submitted: {data}");
        assert_eq!(data["autoReviewCounts"]["failed"], 0);
        let threads = data["threads"].as_array().expect("threads");
        assert!(
            threads
                .iter()
                .any(|thread| thread["comments"]
                    .as_array()
                    .expect("comments")
                    .iter()
                    .any(|comment| comment["auto"] == true)),
            "auto cards must be marked on the wire: {data}"
        );
        assert!(
            threads
                .iter()
                .all(|thread| thread.get("autoReview").is_none()),
            "threads carry no finding verdict: {data}"
        );

        gate.add_permits(submitted.try_into().expect("submitted count"));
        let snapshot = loop {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let snapshot = handle_code_review_command(
                &app,
                &code_review,
                RpcCommand::CodeReviewSnapshot {
                    id: Some("snap-auto".into()),
                },
            )
            .await;
            let data = snapshot.data.expect("snapshot data");
            if data["autoReviewState"] == "done" {
                break data;
            }
        };
        assert_eq!(snapshot["autoReviewCounts"]["completed"], submitted, "{snapshot}");
        assert_eq!(snapshot["autoReviewCounts"]["failed"], 0);

        let closed = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewClose { id: Some("close-auto".into()), review_id: None },
        )
        .await;
        assert!(closed.success, "{closed:?}");
        app.cleanup().await;
    }

    #[tokio::test]
    async fn code_review_auto_review_rejects_oversized_triage_without_started_status() {
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"]).status.success();
        git(&["config", "user.name", "Test"]).status.success();
        std::fs::write(cwd.path().join("README.md"), "initial\n").expect("write");
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "init"]).status.success());
        std::fs::write(cwd.path().join("README.md"), "initial\nchanged\n").expect("write");
        assert!(git(&["add", "."]).status.success());

        let app = build_code_review_app_with_stream(cwd.path(), highlight_rpc_review_stream(Vec::new())).await;
        let code_review = CodeReviewRpcState::default();
        let opened = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen {
                id: Some("open-oversize".into()),
                from: None,
                commit: None,
                to: None,
                include_working_tree: None,
            },
        )
        .await;
        assert!(opened.success, "{opened:?}");
        {
            let mut state = code_review.state.lock().await;
            let session = state.session.as_mut().expect("open review");
            let mut model = session.controller.fork_model().clone();
            model.api = pi_ai::API_OPENAI_COMPLETIONS.to_owned();
            session.controller.set_model(model);
        }
        let model_before = {
            let state = code_review.state.lock().await;
            let session = state.session.as_ref().expect("open review");
            fork_model_display(session.controller.fork_model())
        };
        {
            let mut state = code_review.state.lock().await;
            let session = state.session.as_mut().expect("open review");
            let mut snapshot = session.snapshot.clone();
            if let Some(file) = snapshot.files.first_mut() {
                if let Some(hunk) = file.hunks.first_mut() {
                    hunk.lines.push(crate::code_review::DiffLine {
                        kind: crate::code_review::DiffLineKind::Addition,
                        old_no: None,
                        new_no: Some(99),
                        text: "x".repeat(crate::code_review::MAX_DIFF_BYTES),
                    });
                    hunk.new_count += 1;
                }
            }
            session.snapshot = snapshot;
        }
        let rejected = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewAutoReview {
                id: Some("auto-oversize".into()),
                model_id: None,
                language: None,
            },
        )
        .await;
        assert!(
            !rejected.success,
            "oversized triage must be rejected, never report started: {rejected:?}"
        );
        assert_eq!(
            rejected.data.as_ref().and_then(|data| data["code"].as_str()),
            Some("code_review_auto_failed"),
            "{rejected:?}"
        );
        let error = rejected.error.as_deref().expect("typed error message");
        assert!(error.contains("exceeds"), "{error}");
        {
            let mut state = code_review.state.lock().await;
            let session = state.session.as_mut().expect("open review");
            let projection = code_review_projection(&session.snapshot, &session.controller);
            assert_eq!(projection["autoReviewState"], "idle", "{projection}");
            assert!(projection["threads"].as_array().expect("threads").is_empty());
            assert_eq!(
                fork_model_display(session.controller.fork_model()),
                model_before,
                "a rejected triage must not mutate the selected model"
            );
        }
        app.cleanup().await;
    }

    /// Stream whose per-hunk prompt future blocks synchronously inside its
    /// first poll for `hold`, so an abort's task-handle join is provably held
    /// while other RPC commands must still acquire the state mutex.
    fn blocking_abort_rpc_stream(hold: std::time::Duration) -> pi_agent::StreamFn {
        Arc::new(move |model, context, _options| {
            let prompt = review_request_text(&context);
            let triage = prompt.strip_prefix("Classify every hunk").map(|text| {
                text.lines()
                    .filter_map(|line| line.strip_prefix("Hunk: "))
                    .collect::<Vec<_>>()
                    .join(",")
            });
            Box::pin(async move {
                if triage.is_none() {
                    std::thread::sleep(hold);
                }
                let stream = pi_ai::new_assistant_message_event_stream();
                let producer = stream.clone();
                tokio::spawn(async move {
                    let reply = if let Some(core) = triage {
                        let tokens: Vec<&str> = if core.is_empty() {
                            Vec::new()
                        } else {
                            core.split(',').collect()
                        };
                        serde_json::json!({"core": tokens, "skipped": 0}).to_string()
                    } else {
                        findings_reply_for_request(&prompt, &[])
                    };
                    let mut message = pi_ai::AssistantMessage::pending(&model);
                    message.content.push(pi_ai::ContentBlock::text(reply));
                    message.stop_reason = pi_ai::StopReason::Stop;
                    producer.end(Some(message)).await;
                });
                stream
            })
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn code_review_stop_releases_state_mutex_while_aborting_agents() {
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"]).status.success();
        git(&["config", "user.name", "Test"]).status.success();
        std::fs::write(cwd.path().join("README.md"), "initial\n").expect("write");
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "init"]).status.success());
        std::fs::write(cwd.path().join("README.md"), "initial\nchanged\n").expect("write");
        assert!(git(&["add", "."]).status.success());

        let app = build_code_review_app_with_stream(cwd.path(), blocking_abort_rpc_stream(std::time::Duration::from_secs(3))).await;
        let code_review = CodeReviewRpcState::default();
        let opened = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen {
                id: Some("open-held".into()),
                from: None,
                commit: None,
                to: None,
                include_working_tree: None,
            },
        )
        .await;
        assert!(opened.success, "{opened:?}");
        {
            let mut state = code_review.state.lock().await;
            let session = state.session.as_mut().expect("open review");
            let mut model = session.controller.fork_model().clone();
            model.api = pi_ai::API_OPENAI_COMPLETIONS.to_owned();
            session.controller.set_model(model);
        }
        let auto = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewAutoReview {
                id: Some("auto-held".into()),
                model_id: None,
                language: None,
            },
        )
        .await;
        assert!(auto.success, "{auto:?}");
        let auto_data = auto.data.expect("auto data");
        assert_eq!(auto_data["state"], "triaging", "{auto_data}");
        let mut data = auto_data;
        for _ in 0..100 {
            let snapshot = handle_code_review_command(
                &app,
                &code_review,
                RpcCommand::CodeReviewSnapshot {
                    id: Some("poll-running".into()),
                },
            )
            .await;
            data = snapshot.data.expect("snapshot data");
            if data["autoReviewState"] == "running" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(data["autoReviewState"], "running", "{data}");
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let stop_code_review = code_review.clone();
        let stop_app = app.clone();
        let stop_task = tokio::spawn(async move {
            handle_code_review_command(
                &stop_app,
                &stop_code_review,
                RpcCommand::CodeReviewStop {
                    id: Some("stop-held".into()),
                    operation: ReviewStopOperation::AutoReview,
                },
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let mut prepare_seen = false;
        for _ in 0..20 {
            let poll = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                handle_code_review_command(
                    &app,
                    &code_review,
                    RpcCommand::CodeReviewSnapshot {
                        id: Some("poll-prepare".into()),
                    },
                ),
            )
            .await;
            if let Ok(response) = poll {
                let poll_data = response.data.expect("snapshot data");
                if poll_data["autoReviewState"] == "failed" {
                    prepare_seen = true;
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            prepare_seen,
            "stop prepare must mutate state and release the mutex while the abort is held"
        );
        assert!(
            !stop_task.is_finished(),
            "the stop must still be inside its held abort"
        );
        let probe = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            handle_code_review_command(
                &app,
                &code_review,
                RpcCommand::CodeReviewSnapshot {
                    id: Some("probe-during-abort".into()),
                },
            ),
        )
        .await;
        let Ok(probe_response) = probe else {
            panic!("a snapshot command must acquire the state mutex during the held abort");
        };
        assert!(probe_response.success, "{probe_response:?}");
        let stopped = tokio::time::timeout(std::time::Duration::from_secs(15), stop_task)
            .await
            .expect("stop settles after the blocking stream poll returns")
            .expect("stop task joins");
        assert!(stopped.success, "{stopped:?}");
        let final_projection = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewSnapshot {
                id: Some("final-held".into()),
            },
        )
        .await;
        let final_data = final_projection.data.expect("final snapshot");
        assert_eq!(final_data["autoReviewState"], "failed", "{final_data}");
        app.cleanup().await;
    }

    /// Provider stream that answers the single Highlight prompt with findings
    /// JSON whose identities are copied from that request. Every other prompt
    /// gets a plain non-JSON reply.
    fn highlight_rpc_review_stream(findings: Vec<serde_json::Value>) -> pi_agent::StreamFn {
        Arc::new(move |model, context, _options| {
            let findings = findings.clone();
            let prompt = review_request_text(&context);
            Box::pin(async move {
                let stream = pi_ai::new_assistant_message_event_stream();
                let producer = stream.clone();
                tokio::spawn(async move {
                    let reply = if prompt.contains("Requested hunks:") {
                        findings_reply_for_request(&prompt, &findings)
                    } else {
                        "non-highlight reply".to_owned()
                    };
                    let mut message = pi_ai::AssistantMessage::pending(&model);
                    message.content.push(pi_ai::ContentBlock::text(reply));
                    message.stop_reason = pi_ai::StopReason::Stop;
                    producer.end(Some(message)).await;
                });
                stream
            })
        })
    }

    fn review_request_text(context: &pi_ai::Context) -> String {
        context
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                pi_ai::Message::User(user) => user.content.iter().find_map(|block| match block {
                    pi_ai::ContentBlock::Text { text, .. } => Some(text.clone()),
                    _ => None,
                }),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// One findings document naming every identity captured from the request.
    /// `finding_for` is keyed by request path; paths without an entry are clean.
    fn findings_reply_for_request(prompt: &str, finding_for: &[serde_json::Value]) -> String {
        let hunks = prompt
            .lines()
            .filter_map(|line| {
                let rest = line.strip_prefix("Hunk identity: ")?;
                let mut fields = std::collections::HashMap::new();
                for field in rest.split_whitespace() {
                    let (key, value) = field.split_once('=')?;
                    fields.insert(key, value);
                }
                let path = fields.get("path")?.to_owned();
                let mut finding = finding_for
                    .iter()
                    .find(|finding| finding["path"].as_str() == Some(path))
                    .cloned();
                if let Some(finding) = finding.as_mut().and_then(serde_json::Value::as_object_mut) {
                    finding.remove("path");
                }
                let findings = finding.into_iter().collect::<Vec<_>>();
                Some(json!({
                    "identity": {
                        "snapshotId": fields.get("snapshotId")?.to_owned(),
                        "path": path,
                        "oldStart": fields.get("oldStart")?.parse::<u32>().ok()?,
                        "oldCount": fields.get("oldCount")?.parse::<u32>().ok()?,
                        "newStart": fields.get("newStart")?.parse::<u32>().ok()?,
                        "newCount": fields.get("newCount")?.parse::<u32>().ok()?,
                        "contentHash": fields.get("contentHash")?.to_owned(),
                    },
                    "findings": findings,
                }))
            })
            .collect::<Vec<_>>();
        json!({"hunks": hunks}).to_string()
    }

    #[tokio::test]
    async fn code_review_highlight_returns_summary_and_projection_fields() {
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"]).status.success();
        git(&["config", "user.name", "Test"]).status.success();
        std::fs::write(cwd.path().join("README.md"), "initial\n").expect("write");
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "init"]).status.success());
        std::fs::write(cwd.path().join("README.md"), "initial\nchanged\n").expect("write");
        assert!(git(&["add", "."]).status.success());

        let app = build_code_review_app_with_stream(
            cwd.path(),
            highlight_rpc_review_stream(vec![json!({
                "path": "README.md",
                "category": "naming",
                "side": "new",
                "startLine": 2,
                "endLine": 2,
                "summary": "changed name",
                "evidence": "changed",
            })]),
        )
        .await;
        let code_review = CodeReviewRpcState::default();

        let missing = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewHighlight { id: None },
        )
        .await;
        assert!(!missing.success, "{missing:?}");

        let opened = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen {
                id: Some("open-hl".into()),
                from: None,
                commit: None,
                to: None,
                include_working_tree: None,
            },
        )
        .await;
        assert!(opened.success, "{opened:?}");

        let first = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewHighlight { id: Some("hl-1".into()) },
        )
        .await;
        assert!(first.success, "{first:?}");
        let data = first.data.expect("highlight data");
        assert_eq!(data["ok"], true);
        assert_eq!(data["state"], "running");

        let mut data = serde_json::Value::Null;
        for _ in 0..100 {
            let snapshot = handle_code_review_command(
                &app,
                &code_review,
                RpcCommand::CodeReviewSnapshot {
                    id: Some("snap-hl".into()),
                },
            )
            .await;
            data = snapshot.data.expect("snapshot data");
            if data["highlightState"] == "done" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(data["highlightState"], "done", "{data}");
        let findings = data["reviewFindings"].as_array().expect("reviewFindings");
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0]["identity"]["path"], "README.md");
        assert_eq!(findings[0]["identity"]["snapshotId"], data["snapshotId"]);
        assert_eq!(findings[0]["findings"][0]["category"], "naming");
        assert_eq!(findings[0]["findings"][0]["side"], "new");
        assert_eq!(findings[0]["findings"][0]["startLine"], 2);
        assert!(data.get("highlightHunks").is_none());
        assert!(data.get("autoReview").is_none());
        assert!(data["highlightError"].is_null());

        // A settled highlight may be rerun: one call at a time, sequentially.
        let rerun = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewHighlight { id: Some("hl-2".into()) },
        )
        .await;
        assert!(rerun.success, "{rerun:?}");

        let closed = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewClose { id: Some("close-hl".into()), review_id: None },
        )
        .await;
        assert!(closed.success, "{closed:?}");
        app.cleanup().await;
    }

    /// Provider stream whose Highlight reply blocks on `gate` (so a call can
    /// be held provably Running); every other prompt replies immediately.
    fn gated_highlight_rpc_stream(gate: Arc<tokio::sync::Semaphore>) -> pi_agent::StreamFn {
        Arc::new(move |model, context, _options| {
            let gate = gate.clone();
            Box::pin(async move {
                let is_highlight = context.messages.iter().any(|message| match message {
                    pi_ai::Message::User(user) => user.content.iter().any(|block| matches!(
                        block,
                        pi_ai::ContentBlock::Text { text, .. } if text.contains("Requested hunks:")
                    )),
                    _ => false,
                });
                let stream = pi_ai::new_assistant_message_event_stream();
                let producer = stream.clone();
                tokio::spawn(async move {
                    let reply = if is_highlight {
                        gate.acquire().await.expect("release permit").forget();
                        "definitely not json".to_owned()
                    } else {
                        "non-highlight reply".to_owned()
                    };
                    let mut message = pi_ai::AssistantMessage::pending(&model);
                    message.content.push(pi_ai::ContentBlock::text(reply));
                    message.stop_reason = pi_ai::StopReason::Stop;
                    producer.end(Some(message)).await;
                });
                stream
            })
        })
    }

    #[tokio::test]
    async fn code_review_highlight_rejects_concurrent_call_and_invalid_reply() {
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"]).status.success();
        git(&["config", "user.name", "Test"]).status.success();
        std::fs::write(cwd.path().join("README.md"), "initial\n").expect("write");
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "init"]).status.success());
        std::fs::write(cwd.path().join("README.md"), "initial\nchanged\n").expect("write");
        assert!(git(&["add", "."]).status.success());

        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let app = build_code_review_app_with_stream(cwd.path(), gated_highlight_rpc_stream(gate.clone()))
            .await;
        let code_review = CodeReviewRpcState::default();
        let opened = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen {
                id: Some("open-hl-bad".into()),
                from: None,
                commit: None,
                to: None,
                include_working_tree: None,
            },
        )
        .await;
        assert!(opened.success, "{opened:?}");

        {
            let mut state = code_review.state.lock().await;
            let session = state.session.as_mut().expect("open review");
            assert_eq!(
                session.controller.start_highlight(&session.snapshot),
                crate::code_review_panel::HighlightStart::Started
            );
            assert_eq!(
                session.controller.highlight_state(),
                crate::code_review_panel::HighlightState::Running
            );
        }
        let busy = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewHighlight { id: Some("hl-busy".into()) },
        )
        .await;
        assert!(!busy.success, "{busy:?}");
        assert_eq!(
            busy.data.as_ref().and_then(|data| data["code"].as_str()),
            Some("code_review_highlight_running"),
            "{busy:?}"
        );

        // Release the held call; its plain reply fails the typed parse.
        gate.add_permits(1);
        loop {
            let settled = {
                let mut state = code_review.state.lock().await;
                let session = state.session.as_mut().expect("open review");
                session.controller.poll_events();
                session.controller.highlight_state()
            };
            if settled == crate::code_review_panel::HighlightState::Failed {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let invalid = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewHighlight { id: Some("hl-invalid".into()) },
        )
        .await;
        assert!(invalid.success, "{invalid:?}");
        {
            let state = code_review.state.lock().await;
            assert_eq!(
                state.session.as_ref().expect("open review").controller.highlight_state(),
                crate::code_review_panel::HighlightState::Running,
                "retried highlight must enter Running before its gated reply"
            );
        }
        // The gate only exists to hold the first call provably Running for
        // the concurrency check; release the second held call so its plain
        // reply reaches the typed parse and fails closed.
        gate.add_permits(1);
        let mut failed = false;
        for _ in 0..100 {
            let settled = {
                let mut state = code_review.state.lock().await;
                let session = state.session.as_mut().expect("open review");
                session.controller.poll_events();
                session.controller.highlight_state()
            };
            if settled == crate::code_review_panel::HighlightState::Failed {
                failed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(failed, "invalid highlight must fail closed");
        {
            let state = code_review.state.lock().await;
            let session = state.session.as_ref().expect("open review");
            assert!(session.controller.review_findings().is_empty());
        }
        app.cleanup().await;
    }
    #[tokio::test]
    async fn code_review_tldr_projects_generation_and_stop_keeps_last_good() {
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"]).status.success();
        git(&["config", "user.name", "Test"]).status.success();
        std::fs::write(cwd.path().join("README.md"), "initial\n").expect("write");
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "init"]).status.success());
        std::fs::write(cwd.path().join("README.md"), "initial\nchanged\n").expect("write");

        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let app = build_code_review_app_with_stream(
            cwd.path(),
            gated_tldr_rpc_stream(gate.clone(), "# Overview\n\nREADME changed.\n"),
        )
        .await;
        let code_review = CodeReviewRpcState::default();
        let missing = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewTldr { id: None, language: None },
        )
        .await;
        assert!(!missing.success, "{missing:?}");
        assert_eq!(
            missing.data.as_ref().and_then(|data| data["code"].as_str()),
            Some("code_review_not_open"),
            "{missing:?}"
        );

        let opened = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen {
                id: Some("open-tldr".into()),
                commit: None,
                from: None,
                to: None,
                include_working_tree: None,
            },
        )
        .await;
        assert!(opened.success, "{opened:?}");
        let snapshot_id = opened.data.expect("open data")["snapshotId"]
            .as_str()
            .expect("snapshot id")
            .to_owned();
        let idle = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewSnapshot { id: Some("tldr-idle".into()) },
        )
        .await;
        let idle = idle.data.expect("idle snapshot");
        assert_eq!(idle["tldrState"], "idle");
        assert_eq!(idle["tldrGeneration"], 0);
        assert!(idle["tldrSnapshotId"].is_null());
        assert!(idle["tldrError"].is_null());
        assert_eq!(idle["tldrMarkdown"], "");

        let started = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewTldr {
                id: Some("tldr-1".into()),
                language: Some("en".into()),
            },
        )
        .await;
        assert!(started.success, "{started:?}");
        let started = started.data.expect("tldr ack");
        assert_eq!(started["ok"], true);
        assert_eq!(started["state"], "running");
        let generation = started["tldrGeneration"].as_u64().expect("generation");
        assert_eq!(generation, 1);

        let busy = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewTldr { id: Some("tldr-busy".into()), language: None },
        )
        .await;
        assert!(!busy.success, "{busy:?}");
        assert_eq!(
            busy.data.as_ref().and_then(|data| data["code"].as_str()),
            Some("code_review_tldr_running"),
            "{busy:?}"
        );

        gate.add_permits(1);
        let mut done = serde_json::Value::Null;
        for _ in 0..100 {
            let snapshot = handle_code_review_command(
                &app,
                &code_review,
                RpcCommand::CodeReviewSnapshot { id: Some("tldr-done".into()) },
            )
            .await;
            done = snapshot.data.expect("done snapshot");
            if done["tldrState"] == "done" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(done["tldrState"], "done", "{done}");
        assert_eq!(done["tldrGeneration"], generation);
        assert_eq!(done["tldrSnapshotId"], snapshot_id);
        assert!(done["tldrError"].is_null());
        assert_eq!(
            done["tldrMarkdown"].as_str().unwrap_or("").trim(),
            "# Overview\n\nREADME changed.\n".trim(),
        );

        let stopped_start = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewTldr { id: Some("tldr-2".into()), language: None },
        )
        .await;
        assert!(stopped_start.success, "{stopped_start:?}");
        let stopped_generation = stopped_start.data.expect("second ack")["tldrGeneration"]
            .as_u64()
            .expect("second generation");
        assert!(stopped_generation > generation);
        let stopped = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewStop {
                id: Some("stop-tldr".into()),
                operation: ReviewStopOperation::Tldr,
            },
        )
        .await;
        assert!(stopped.success, "{stopped:?}");
        assert_eq!(stopped.data.expect("stop ack")["operation"], "tldr");
        let after = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewSnapshot { id: Some("tldr-stopped".into()) },
        )
        .await;
        let after = after.data.expect("stopped snapshot");
        assert_eq!(after["tldrState"], "done", "{after}");
        assert!(after["tldrGeneration"].as_u64().expect("generation") > stopped_generation);
        assert_eq!(
            after["tldrMarkdown"].as_str().unwrap_or("").trim(),
            "# Overview\n\nREADME changed.\n".trim(),
        );
        assert!(after["tldrError"].is_null());
        app.cleanup().await;
    }

    fn gated_tldr_rpc_stream(
        gate: Arc<tokio::sync::Semaphore>,
        markdown: &'static str,
    ) -> pi_agent::StreamFn {
        Arc::new(move |model, context, _options| {
            let gate = gate.clone();
            let prompt = review_request_text(&context);
            Box::pin(async move {
                let stream = pi_ai::new_assistant_message_event_stream();
                let producer = stream.clone();
                tokio::spawn(async move {
                    let reply = if prompt.contains("comparisonLabel:") {
                        gate.acquire().await.expect("release permit").forget();
                        markdown.to_owned()
                    } else {
                        "non-tldr reply".to_owned()
                    };
                    let mut message = pi_ai::AssistantMessage::pending(&model);
                    message.content.push(pi_ai::ContentBlock::text(reply));
                    message.stop_reason = pi_ai::StopReason::Stop;
                    producer.end(Some(message)).await;
                });
                stream
            })
        })
    }
    #[tokio::test]
    async fn code_review_refresh_capture_keeps_snapshot_and_abort_responsive() {
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(cwd.path().join("review.txt"), "base\n").expect("write baseline");
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "baseline"]).status.success());
        std::fs::write(cwd.path().join("review.txt"), "base\nchanged\n").expect("write change");

        let app = build_code_review_app(cwd.path()).await;
        let code_review = CodeReviewRpcState::default();
        let opened = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen {
                id: None,
                commit: None,
                from: None,
                to: None,
                include_working_tree: None,
            },
        )
        .await;
        assert!(opened.success, "{opened:?}");
        let opened_data = opened.data.expect("open data");
        let snapshot_id = opened_data["snapshotId"]
            .as_str()
            .expect("snapshot id")
            .to_owned();
        let hunk = &opened_data["files"][0]["hunks"][0];
        let abort = RpcCommand::CodeReviewAbort {
            id: Some("abort-during-refresh".into()),
            snapshot_id: snapshot_id.clone(),
            path: opened_data["files"][0]["path"]
                .as_str()
                .expect("path")
                .to_owned(),
            old_start: hunk["oldStart"].as_u64().expect("old start") as u32,
            old_count: hunk["oldCount"].as_u64().expect("old count") as u32,
            new_start: hunk["newStart"].as_u64().expect("new start") as u32,
            new_count: hunk["newCount"].as_u64().expect("new count") as u32,
            content_hash: hunk["contentHash"]
                .as_str()
                .expect("content hash")
                .to_owned(),
        };
        let gate = Arc::new(ReviewCaptureGate {
            captured: tokio::sync::Notify::new(),
            released: std::sync::Mutex::new(false),
            release: std::sync::Condvar::new(),
        });
        *code_review.review_capture_gate.lock().await = Some(gate.clone());
        let refresh_state = code_review.clone();
        let refresh = tokio::spawn({
            let app = app.clone();
            async move {
                handle_code_review_command(
                    &app,
                    &refresh_state,
                    RpcCommand::CodeReviewRefresh {
                        id: Some("held-refresh".into()),
                    },
                )
                .await
            }
        });
        gate.captured.notified().await;

        let snapshot = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            handle_code_review_command(
                &app,
                &code_review,
                RpcCommand::CodeReviewSnapshot {
                    id: Some("snapshot-during-refresh".into()),
                },
            ),
        )
        .await
        .expect("snapshot pinned behind Git capture");
        assert!(snapshot.success, "{snapshot:?}");
        assert_eq!(
            snapshot.data.expect("snapshot data")["snapshotId"],
            snapshot_id
        );
        let aborted = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            handle_code_review_command(&app, &code_review, abort),
        )
        .await
        .expect("abort pinned behind Git capture");
        assert!(aborted.success, "{aborted:?}");

        gate.release();
        let refreshed = refresh.await.expect("refresh task");
        assert!(refreshed.success, "{refreshed:?}");
        *code_review.review_capture_gate.lock().await = None;
        assert!(code_review.close(None).await.is_ok());
        app.cleanup().await;
    }

    #[tokio::test]
    async fn code_review_delayed_refresh_cannot_resurrect_closed_state() {
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(cwd.path().join("review.txt"), "base\n").expect("write baseline");
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "baseline"]).status.success());
        std::fs::write(cwd.path().join("review.txt"), "changed\n").expect("write change");

        let app = build_code_review_app(cwd.path()).await;
        let code_review = CodeReviewRpcState::default();
        assert!(code_review.open(&app, None, None, None, None).await.is_ok());
        let gate = Arc::new(ReviewCaptureGate {
            captured: tokio::sync::Notify::new(),
            released: std::sync::Mutex::new(false),
            release: std::sync::Condvar::new(),
        });
        *code_review.review_capture_gate.lock().await = Some(gate.clone());
        let refresh_state = code_review.clone();
        let refresh = tokio::spawn({
            let app = app.clone();
            async move { refresh_state.refresh(&app).await }
        });
        gate.captured.notified().await;
        let closed =
            tokio::time::timeout(std::time::Duration::from_millis(250), code_review.close(None))
                .await
                .expect("close pinned behind Git capture");
        assert!(closed.is_ok(), "{closed:?}");
        gate.release();
        let stale = refresh
            .await
            .expect("refresh task")
            .expect_err("stale refresh resurrected state");
        assert!(stale.to_string().contains("superseded"), "{stale:#}");
        assert!(
            code_review.snapshot().await.is_err(),
            "closed state was resurrected"
        );
        app.cleanup().await;
    }

    #[tokio::test]
    async fn code_review_delayed_open_cannot_overwrite_newer_open() {
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(cwd.path().join("review.txt"), "base\n").expect("write baseline");
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "baseline"]).status.success());
        std::fs::write(cwd.path().join("review.txt"), "first\n").expect("write first change");

        let app = build_code_review_app(cwd.path()).await;
        let code_review = CodeReviewRpcState::default();
        let initial = code_review
            .open(&app, None, None, None, None)
            .await
            .expect("initial open");
        let initial_id = initial["snapshotId"]
            .as_str()
            .expect("initial snapshot id")
            .to_owned();
        let gate = Arc::new(ReviewCaptureGate {
            captured: tokio::sync::Notify::new(),
            released: std::sync::Mutex::new(false),
            release: std::sync::Condvar::new(),
        });
        *code_review.review_capture_gate.lock().await = Some(gate.clone());
        let stale_state = code_review.clone();
        let stale_open = tokio::spawn({
            let app = app.clone();
            async move { stale_state.open(&app, None, None, None, None).await }
        });
        gate.captured.notified().await;
        *code_review.review_capture_gate.lock().await = None;
        let during_open = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            code_review.snapshot(),
        )
        .await
        .expect("snapshot pinned behind replacement open capture")
        .expect("previous review remains authoritative");
        assert_eq!(during_open["snapshotId"], initial_id);
        std::fs::write(cwd.path().join("review.txt"), "second\n").expect("write second change");
        let newer = code_review
            .open(&app, None, None, None, None)
            .await
            .expect("newer open");
        let newer_id = newer["snapshotId"]
            .as_str()
            .expect("newer snapshot id")
            .to_owned();
        gate.release();
        let stale = stale_open
            .await
            .expect("stale open task")
            .expect_err("stale open overwrote newer state");
        assert!(stale.to_string().contains("superseded"), "{stale:#}");
        assert_eq!(
            code_review.snapshot().await.expect("live snapshot")["snapshotId"],
            newer_id,
        );
        assert!(code_review.close(None).await.is_ok());
        app.cleanup().await;
    }

    #[tokio::test]
    async fn code_review_stale_close_cannot_kill_newer_controller() {
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(cwd.path().join("review.txt"), "base\n").expect("write baseline");
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "baseline"]).status.success());
        std::fs::write(cwd.path().join("review.txt"), "changed\n").expect("write change");

        let app = build_code_review_app(cwd.path()).await;
        let code_review = CodeReviewRpcState::default();
        code_review
            .open(&app, None, None, None, None)
            .await
            .expect("open");
        let review_id = code_review
            .snapshot()
            .await
            .expect("snapshot")["reviewId"]
            .as_str()
            .expect("review id")
            .to_owned();
        let stale = code_review
            .close(Some("retired-review-identity"))
            .await
            .expect("stale close ok");
        assert_eq!(stale["closed"], false, "{stale}");
        assert_eq!(stale["staleReview"], true, "{stale}");
        assert_eq!(
            code_review.snapshot().await.expect("controller survived")["reviewId"],
            review_id,
        );
        let closed = code_review.close(Some(&review_id)).await.expect("real close");
        assert_eq!(closed["closed"], true, "{closed:?}");
        assert!(
            code_review.snapshot().await.is_err(),
            "the matching close still closes the controller"
        );
        // A repeat stamped close after the review is gone is the typed no-open
        // rejection, never a silent success or an authority bump.
        let repeat = code_review
            .close(Some(&review_id))
            .await
            .expect_err("repeat close must reject");
        assert_eq!(
            repeat.downcast_ref::<CodeReviewRpcError>(),
            Some(&CodeReviewRpcError::ReviewNotOpen),
            "{repeat:#}"
        );
        app.cleanup().await;
    }

    #[tokio::test]
    async fn code_review_file_diff_paginates_single_file_and_rejects_stale_requests() {
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"])
            .status
            .success();
        git(&["config", "user.name", "Test"]).status.success();
        std::fs::write(cwd.path().join("big.txt"), "base\n").expect("write");
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "init"]).status.success());
        let changed = "changed-line\n".repeat(5000);
        std::fs::write(cwd.path().join("big.txt"), &changed).expect("write");
        assert!(git(&["add", "."]).status.success());

        let app = build_code_review_app(cwd.path()).await;
        let code_review = CodeReviewRpcState::default();
        let opened = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen {
                id: None,
                commit: None,
                from: None,
                to: None,
                include_working_tree: None,
            },
        )
        .await;
        assert!(opened.success, "{opened:?}");
        let snapshot_id = opened.data.as_ref().expect("open data")["snapshotId"]
            .as_str()
            .expect("snapshot id")
            .to_owned();

        let unknown = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewFileDiff {
                id: None,
                snapshot_id: snapshot_id.clone(),
                path: "does/not/exist.rs".into(),
                cursor: 0,
                max_lines: None,
            },
        )
        .await;
        assert!(!unknown.success, "{unknown:?}");
        assert!(
            unknown
                .error
                .as_deref()
                .is_some_and(|e| e.contains("unknown file"))
        );

        let traversal = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewFileDiff {
                id: None,
                snapshot_id: snapshot_id.clone(),
                path: "../etc/passwd".into(),
                cursor: 0,
                max_lines: None,
            },
        )
        .await;
        assert!(!traversal.success, "{traversal:?}");

        let first = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewFileDiff {
                id: None,
                snapshot_id: snapshot_id.clone(),
                path: "big.txt".into(),
                cursor: 0,
                max_lines: Some(50),
            },
        )
        .await;
        assert!(first.success, "{first:?}");
        let page = first.data.expect("page data");
        assert_eq!(page["path"], "big.txt");
        assert_eq!(page["cursor"], 0);
        let hunks = page["hunks"].as_array().expect("hunks");
        assert!(
            !hunks.is_empty(),
            "page must carry hunk descriptors: {page}"
        );
        let page_lines: usize = hunks
            .iter()
            .map(|hunk| hunk["lines"].as_array().expect("hunk lines").len())
            .sum();
        assert_eq!(page_lines, 50);
        assert!(hunks.iter().all(|hunk| hunk["complete"] == true), "{page}");
        assert_eq!(
            hunks[0]["contentHash"].as_str().expect("hash").len(),
            64,
            "descriptor must carry the full sha256 content hash"
        );
        assert_eq!(page["hasMore"], true);
        let next = page["nextCursor"].as_u64().expect("next cursor");
        assert_eq!(next, 50);
        let total = page["totalLines"].as_u64().expect("total lines");
        assert!(
            total > 4000,
            "the big file should exceed the render cap: {total}"
        );
        let hunk_count = page["hunkCount"].as_u64().expect("hunk count");
        assert!(hunk_count >= 1);

        let hunk = &hunks[0];
        let identity = (
            hunk["oldStart"].as_u64().expect("oldStart") as u32,
            hunk["oldCount"].as_u64().expect("oldCount") as u32,
            hunk["newStart"].as_u64().expect("newStart") as u32,
            hunk["newCount"].as_u64().expect("newCount") as u32,
            hunk["contentHash"].as_str().expect("hash").to_owned(),
        );
        let commented = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewComment {
                id: Some("comment-loaded".into()),
                snapshot_id: snapshot_id.clone(),
                path: "big.txt".into(),
                old_start: identity.0,
                old_count: identity.1,
                new_start: identity.2,
                new_count: identity.3,
                content_hash: identity.4.clone(),
                comment: "loaded hunk comment".into(),
                images: Vec::new(),
            },
        )
        .await;
        assert!(commented.success, "{commented:?}");
        let thread = commented.data.as_ref().expect("comment data")["threads"]
            .as_array()
            .expect("threads")
            .first()
            .expect("thread")
            .clone();
        assert_eq!(thread["identity"]["path"], "big.txt");
        assert_eq!(
            thread["identity"]["contentHash"].as_str().expect("hash"),
            identity.4
        );
        assert_eq!(
            thread["comments"].as_array().expect("comments").len(),
            1,
            "the accepted comment must be queued on the thread"
        );
        assert_eq!(thread["comments"][0]["text"], "loaded hunk comment");
        let second = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewComment {
                id: Some("comment-loaded-2".into()),
                snapshot_id: snapshot_id.clone(),
                path: "big.txt".into(),
                old_start: identity.0,
                old_count: identity.1,
                new_start: identity.2,
                new_count: identity.3,
                content_hash: identity.4.clone(),
                comment: "queued follow-up".into(),
                images: Vec::new(),
            },
        )
        .await;
        assert!(second.success, "{second:?}");
        let thread = second.data.as_ref().expect("comment data")["threads"]
            .as_array()
            .expect("threads")
            .first()
            .expect("thread")
            .clone();
        assert_eq!(
            thread["comments"].as_array().expect("comments").len(),
            2,
            "the second comment must queue on the same loaded hunk thread"
        );
        let wrong_hash = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewComment {
                id: Some("comment-wrong-hash".into()),
                snapshot_id: snapshot_id.clone(),
                path: "big.txt".into(),
                old_start: identity.0,
                old_count: identity.1,
                new_start: identity.2,
                new_count: identity.3,
                content_hash: "deadbeef".into(),
                comment: "stale".into(),
                images: Vec::new(),
            },
        )
        .await;
        assert!(!wrong_hash.success, "{wrong_hash:?}");
        assert!(
            wrong_hash
                .error
                .as_deref()
                .is_some_and(|e| e.contains("stale hunk"))
        );
        let wrong_range = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewComment {
                id: Some("comment-wrong-range".into()),
                snapshot_id: snapshot_id.clone(),
                path: "big.txt".into(),
                old_start: 999,
                old_count: 1,
                new_start: 1,
                new_count: 1,
                content_hash: identity.4.clone(),
                comment: "stale".into(),
                images: Vec::new(),
            },
        )
        .await;
        assert!(!wrong_range.success, "{wrong_range:?}");
        assert!(
            wrong_range
                .error
                .as_deref()
                .is_some_and(|e| e.contains("unknown hunk"))
        );
        let aborted = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewAbort {
                id: Some("abort-loaded".into()),
                snapshot_id: snapshot_id.clone(),
                path: "big.txt".into(),
                old_start: identity.0,
                old_count: identity.1,
                new_start: identity.2,
                new_count: identity.3,
                content_hash: identity.4.clone(),
            },
        )
        .await;
        assert!(aborted.success, "{aborted:?}");

        let stale = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewFileDiff {
                id: None,
                snapshot_id: "wrong-snapshot-id".to_owned(),
                path: "big.txt".into(),
                cursor: 0,
                max_lines: None,
            },
        )
        .await;
        assert!(!stale.success, "{stale:?}");
        assert!(
            stale
                .error
                .as_deref()
                .is_some_and(|e| e.contains("stale snapshot"))
        );

        let over_cursor = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewFileDiff {
                id: None,
                snapshot_id: snapshot_id.clone(),
                path: "big.txt".into(),
                cursor: total as usize + 10,
                max_lines: None,
            },
        )
        .await;
        assert!(!over_cursor.success, "{over_cursor:?}");
        assert!(
            over_cursor
                .error
                .as_deref()
                .is_some_and(|e| e.contains("out of range"))
        );

        let changed2 = "changed-line\n".repeat(6000);
        std::fs::write(cwd.path().join("big.txt"), &changed2).expect("write big2");
        assert!(git(&["add", "big.txt"]).status.success());

        let refreshed = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewRefresh { id: None },
        )
        .await;
        assert!(refreshed.success, "{refreshed:?}");
        let new_snapshot_id = refreshed.data.as_ref().expect("refresh data")["snapshotId"]
            .as_str()
            .expect("snapshot id")
            .to_owned();
        let stale_after_refresh = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewFileDiff {
                id: None,
                snapshot_id: snapshot_id,
                path: "big.txt".into(),
                cursor: 0,
                max_lines: None,
            },
        )
        .await;
        assert!(!stale_after_refresh.success, "{stale_after_refresh:?}");
        let ok_after = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewFileDiff {
                id: None,
                snapshot_id: new_snapshot_id,
                path: "big.txt".into(),
                cursor: 0,
                max_lines: Some(10),
            },
        )
        .await;
        assert!(ok_after.success, "{ok_after:?}");

        let closed = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewClose { id: None, review_id: None },
        )
        .await;
        assert!(closed.success);
        app.cleanup().await;
    }

    #[tokio::test]
    async fn code_review_file_context_expands_above_hunk_and_rejects_stale_snapshot() {
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"])
            .status
            .success();
        git(&["config", "user.name", "Test"]).status.success();
        let mut body = String::new();
        for n in 1..=50 {
            body.push_str(&format!("line-{n}\n"));
        }
        std::fs::write(cwd.path().join("wide.rs"), &body).expect("write");
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "init"]).status.success());
        let edited = body.replace("line-40\n", "line-40-edit\n");
        std::fs::write(cwd.path().join("wide.rs"), &edited).expect("edit");

        let app = build_code_review_app(cwd.path()).await;
        let code_review = CodeReviewRpcState::default();
        let opened = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen {
                id: None,
                commit: None,
                from: None,
                to: None,
                include_working_tree: None,
            },
        )
        .await;
        assert!(opened.success, "{opened:?}");
        let snapshot_id = opened.data.as_ref().expect("open data")["snapshotId"]
            .as_str()
            .expect("snapshot id")
            .to_owned();
        let files = opened.data.as_ref().expect("open data")["files"]
            .as_array()
            .expect("files");
        let file = files
            .iter()
            .find(|file| file["path"] == "wide.rs")
            .expect("wide.rs");
        let hunks = file["hunks"].as_array().expect("hunks");
        assert!(!hunks.is_empty(), "{file}");
        let new_start = hunks[0]["newStart"].as_u64().expect("newStart");
        assert!(new_start > 1, "hunk must sit below file start: {file}");
        let hash_before = hunks[0]["contentHash"].as_str().expect("hash").to_owned();

        std::fs::write(cwd.path().join("wide.rs"), "LIVE-WORKTREE-SHOULD-NOT-APPEAR\n")
            .expect("overwrite live worktree");

        let stale = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewFileContext {
                id: None,
                snapshot_id: "wrong-snapshot-id".to_owned(),
                path: "wide.rs".into(),
                old_start: 1,
                new_start: 1,
                count: Some(20),
            },
        )
        .await;
        assert!(!stale.success, "{stale:?}");
        assert!(
            stale
                .error
                .as_deref()
                .is_some_and(|e| e.contains("stale snapshot")),
            "{stale:?}"
        );

        let expanded = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewFileContext {
                id: None,
                snapshot_id: snapshot_id.clone(),
                path: "wide.rs".into(),
                old_start: 20,
                new_start: 20,
                count: Some(20),
            },
        )
        .await;
        assert!(expanded.success, "{expanded:?}");
        let page = expanded.data.expect("context data");
        assert_eq!(page["snapshotId"], snapshot_id);
        assert_eq!(page["path"], "wide.rs");
        let lines = page["lines"].as_array().expect("lines");
        assert_eq!(lines.len(), 20, "{page}");
        assert_eq!(lines[0]["kind"], "context");
        assert_eq!(lines[0]["newNo"], 20);
        assert_eq!(lines[0]["text"], "line-20");
        assert!(
            !lines.iter().any(|line| line["text"]
                .as_str()
                .is_some_and(|text| text.contains("LIVE-WORKTREE"))),
            "{page}"
        );
        let after = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewSnapshot { id: None },
        )
        .await;
        assert!(after.success, "{after:?}");
        let after_hunks = after.data.as_ref().expect("snapshot")["files"]
            .as_array()
            .expect("files")
            .iter()
            .find(|file| file["path"] == "wide.rs")
            .expect("wide.rs")["hunks"]
            .as_array()
            .expect("hunks");
        assert_eq!(
            after_hunks[0]["contentHash"].as_str().expect("hash"),
            hash_before
        );

        let meta = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewFileContext {
                id: None,
                snapshot_id: snapshot_id.clone(),
                path: "wide.rs".into(),
                old_start: 0,
                new_start: 0,
                count: Some(0),
            },
        )
        .await;
        assert!(meta.success, "{meta:?}");
        let meta_page = meta.data.expect("count-0 data");
        assert_eq!(meta_page["oldLineCount"], 50);
        assert_eq!(meta_page["newLineCount"], 50);
        assert_eq!(
            meta_page["lines"].as_array().expect("lines").len(),
            0,
            "{meta_page}"
        );

        let closed = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewClose { id: None, review_id: None },
        )
        .await;
        assert!(closed.success);
        app.cleanup().await;
    }

    #[tokio::test]
    async fn code_review_served_uncached_diff_keeps_comment_and_abort_authority() {
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"])
            .status
            .success();
        git(&["config", "user.name", "Test"]).status.success();
        std::fs::write(cwd.path().join("big.txt"), "base\n").expect("write");
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "init"]).status.success());
        let changed = "changed-line\n".repeat(5000);
        std::fs::write(cwd.path().join("big.txt"), &changed).expect("write");
        assert!(git(&["add", "."]).status.success());

        let app = build_code_review_app(cwd.path()).await;
        let code_review = CodeReviewRpcState::default();
        let opened = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen {
                id: None,
                commit: None,
                from: None,
                to: None,
                include_working_tree: None,
            },
        )
        .await;
        assert!(opened.success, "{opened:?}");
        let snapshot_id = opened.data.as_ref().expect("open data")["snapshotId"]
            .as_str()
            .expect("snapshot id")
            .to_owned();

        {
            let mut state = code_review.state.lock().await;
            let session = state.session.as_mut().expect("open review");
            session.file_diff_cache = FileDiffCache::new(FILE_DIFF_CACHE_MAX_ENTRIES, 1);
        }

        let first = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewFileDiff {
                id: None,
                snapshot_id: snapshot_id.clone(),
                path: "big.txt".into(),
                cursor: 0,
                max_lines: Some(50),
            },
        )
        .await;
        assert!(first.success, "{first:?}");
        let page = first.data.expect("page data");
        let hunks = page["hunks"].as_array().expect("hunks").clone();
        assert!(!hunks.is_empty(), "page carries hunk descriptors: {page}");
        assert!(
            hunks.iter().all(|hunk| hunk["complete"] == true),
            "retained authority lets the page expose comment-ready identities: {page}"
        );
        let hunk = &hunks[0];
        let identity = (
            hunk["oldStart"].as_u64().expect("oldStart") as u32,
            hunk["oldCount"].as_u64().expect("oldCount") as u32,
            hunk["newStart"].as_u64().expect("newStart") as u32,
            hunk["newCount"].as_u64().expect("newCount") as u32,
            hunk["contentHash"].as_str().expect("hash").to_owned(),
        );

        {
            let state = code_review.state.lock().await;
            let session = state.session.as_ref().expect("open review");
            assert!(
                session.file_diff_cache.peek("big.txt").is_none(),
                "the body cache rejected the diff"
            );
            let records = session
                .comment_authority
                .lookup("big.txt")
                .expect("authority retained the served identities");
            assert_eq!(records.len() as u64, page["hunkCount"].as_u64().expect("hunk count"));
            assert_eq!(session.comment_authority.total, records.len());
            assert!(!session.comment_authority.overflowed("big.txt"));
            assert_eq!(
                records[0].content_hash, identity.4,
                "record digest matches the page projection"
            );
        }

        let commented = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewComment {
                id: Some("comment-uncached".into()),
                snapshot_id: snapshot_id.clone(),
                path: "big.txt".into(),
                old_start: identity.0,
                old_count: identity.1,
                new_start: identity.2,
                new_count: identity.3,
                content_hash: identity.4.clone(),
                comment: "served-uncached comment".into(),
                images: Vec::new(),
            },
        )
        .await;
        assert!(commented.success, "{commented:?}");
        let thread = commented.data.as_ref().expect("comment data")["threads"]
            .as_array()
            .expect("threads")
            .first()
            .expect("thread")
            .clone();
        assert_eq!(thread["identity"]["path"], "big.txt");
        assert_eq!(
            thread["identity"]["contentHash"].as_str().expect("thread hash"),
            identity.4.as_str(),
            "the thread carries the served page's exact identity"
        );

        let aborted = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewAbort {
                id: None,
                snapshot_id: snapshot_id.clone(),
                path: "big.txt".into(),
                old_start: identity.0,
                old_count: identity.1,
                new_start: identity.2,
                new_count: identity.3,
                content_hash: identity.4.clone(),
            },
        )
        .await;
        assert!(aborted.success, "{aborted:?}");

        // Snapshot identity is content-derived: refresh only yields a new id
        // once the captured comparison actually changed.
        std::fs::write(cwd.path().join("big.txt"), format!("{changed}more\n"))
            .expect("mutate before refresh");
        assert!(git(&["add", "big.txt"]).status.success());

        let refreshed = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewRefresh { id: None },
        )
        .await;
        assert!(refreshed.success, "{refreshed:?}");
        {
            let state = code_review.state.lock().await;
            let session = state.session.as_ref().expect("open review");
            assert_ne!(session.snapshot.snapshot_id, snapshot_id);
            assert!(
                session.comment_authority.records.is_empty()
                    && session.comment_authority.total == 0
                    && session.comment_authority.overflow.is_empty(),
                "refresh clears the comment authority with the session"
            );
        }

        let closed = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewClose { id: None, review_id: None },
        )
        .await;
        assert!(closed.success, "{closed:?}");
        {
            let state = code_review.state.lock().await;
            assert!(state.session.is_none(), "close drops the session");
        }
        app.cleanup().await;
    }

    #[tokio::test]
    async fn code_review_authority_overflow_pages_fail_closed_and_reject_comments() {
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"])
            .status
            .success();
        git(&["config", "user.name", "Test"]).status.success();
        std::fs::write(cwd.path().join("big.txt"), "base\n").expect("write");
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "init"]).status.success());
        let changed = "changed-line\n".repeat(5000);
        std::fs::write(cwd.path().join("big.txt"), &changed).expect("write");
        assert!(git(&["add", "."]).status.success());

        let app = build_code_review_app(cwd.path()).await;
        let code_review = CodeReviewRpcState::default();
        let opened = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen {
                id: None,
                from: None,
                commit: None,
                to: None,
                include_working_tree: None,
            },
        )
        .await;
        assert!(opened.success, "{opened:?}");
        let snapshot_id = opened.data.as_ref().expect("open data")["snapshotId"]
            .as_str()
            .expect("snapshot id")
            .to_owned();

        {
            use crate::code_review::{DiffLine, FileStatus};
            let hunk = |text: &str| DiffHunk {
                header: "@@ -1,1 +1,1 @@".into(),
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
                lines: vec![DiffLine {
                    kind: DiffLineKind::Addition,
                    old_no: None,
                    new_no: Some(1),
                    text: text.to_owned(),
                }],
            };
            let filler = |texts: &[&str]| FileDiff {
                path: "filler".into(),
                previous_path: None,
                status: FileStatus::Modified,
                binary: false,
                hunks: texts.iter().map(|text| hunk(text)).collect(),
                byte_capped: false,
                error: None,
            };
            let mut state = code_review.state.lock().await;
            let session = state.session.as_mut().expect("open review");
            session.file_diff_cache = FileDiffCache::new(FILE_DIFF_CACHE_MAX_ENTRIES, 1);
            let mut batch = 0usize;
            while session.comment_authority.total + 256 <= MAX_COMMENT_AUTHORITY_HUNKS {
                let texts: Vec<String> = (0..256)
                    .map(|i| format!("hunk {}", batch * 256 + i))
                    .collect();
                let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
                assert!(session
                    .comment_authority
                    .record(&format!("filler{batch}.txt"), &filler(&texts)));
                batch += 1;
            }
            assert_eq!(session.comment_authority.total, MAX_COMMENT_AUTHORITY_HUNKS);
        }

        let first = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewFileDiff {
                id: None,
                snapshot_id: snapshot_id.clone(),
                path: "big.txt".into(),
                cursor: 0,
                max_lines: Some(50),
            },
        )
        .await;
        assert!(first.success, "{first:?}");
        let page = first.data.expect("page data");
        let hunks = page["hunks"].as_array().expect("hunks").clone();
        assert!(!hunks.is_empty());
        assert!(
            hunks.iter().all(|hunk| hunk["complete"] == false),
            "an authority-overflowed path serves complete=false on every hunk: {page}"
        );
        let hunk = &hunks[0];

        let rejected = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewComment {
                id: Some("overflow-comment".into()),
                snapshot_id: snapshot_id.clone(),
                path: "big.txt".into(),
                old_start: hunk["oldStart"].as_u64().expect("oldStart") as u32,
                old_count: hunk["oldCount"].as_u64().expect("oldCount") as u32,
                new_start: hunk["newStart"].as_u64().expect("newStart") as u32,
                new_count: hunk["newCount"].as_u64().expect("newCount") as u32,
                content_hash: hunk["contentHash"].as_str().expect("hash").to_owned(),
                comment: "must be rejected".into(),
                images: Vec::new(),
            },
        )
        .await;
        assert!(!rejected.success, "{rejected:?}");
        assert!(
            rejected
                .error
                .as_deref()
                .is_some_and(|e| e.contains("comment authority bound")),
            "rejection names the retained authority bound: {rejected:?}"
        );

        let abort_rejected = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewAbort {
                id: None,
                snapshot_id,
                path: "big.txt".into(),
                old_start: hunk["oldStart"].as_u64().expect("oldStart") as u32,
                old_count: hunk["oldCount"].as_u64().expect("oldCount") as u32,
                new_start: hunk["newStart"].as_u64().expect("newStart") as u32,
                new_count: hunk["newCount"].as_u64().expect("newCount") as u32,
                content_hash: hunk["contentHash"].as_str().expect("hash").to_owned(),
            },
        )
        .await;
        assert!(!abort_rejected.success, "{abort_rejected:?}");
        assert!(
            abort_rejected
                .error
                .as_deref()
                .is_some_and(|e| e.contains("comment authority bound")),
            "{abort_rejected:?}"
        );

        let closed = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewClose { id: None, review_id: None },
        )
        .await;
        assert!(closed.success, "{closed:?}");
        app.cleanup().await;
    }

    #[tokio::test]
    async fn code_review_authority_overflow_denies_cached_diff_pages_and_comments() {
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"]).status.success();
        git(&["config", "user.name", "Test"]).status.success();
        std::fs::write(cwd.path().join("big.txt"), "base\n").expect("write");
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "init"]).status.success());
        let changed = "changed-line\n".repeat(5000);
        std::fs::write(cwd.path().join("big.txt"), &changed).expect("write");
        assert!(git(&["add", "."]).status.success());

        let app = build_code_review_app(cwd.path()).await;
        let code_review = CodeReviewRpcState::default();
        let opened = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen {
                id: None,
                from: None,
                commit: None,
                to: None,
                include_working_tree: None,
            },
        )
        .await;
        assert!(opened.success, "{opened:?}");
        let snapshot_id = opened.data.as_ref().expect("open data")["snapshotId"]
            .as_str()
            .expect("snapshot id")
            .to_owned();

        {
            use crate::code_review::{DiffLine, FileStatus};
            let hunk = |text: &str| DiffHunk {
                header: "@@ -1,1 +1,1 @@".into(),
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
                lines: vec![DiffLine {
                    kind: DiffLineKind::Addition,
                    old_no: None,
                    new_no: Some(1),
                    text: text.to_owned(),
                }],
            };
            let filler = |texts: &[&str]| FileDiff {
                path: "filler".into(),
                previous_path: None,
                status: FileStatus::Modified,
                binary: false,
                hunks: texts.iter().map(|text| hunk(text)).collect(),
                byte_capped: false,
                error: None,
            };
            let mut state = code_review.state.lock().await;
            let session = state.session.as_mut().expect("open review");
            let mut batch = 0usize;
            while session.comment_authority.total + 256 <= MAX_COMMENT_AUTHORITY_HUNKS {
                let texts: Vec<String> = (0..256)
                    .map(|i| format!("hunk {}", batch * 256 + i))
                    .collect();
                let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
                assert!(session
                    .comment_authority
                    .record(&format!("filler{batch}.txt"), &filler(&texts)));
                batch += 1;
            }
            assert_eq!(session.comment_authority.total, MAX_COMMENT_AUTHORITY_HUNKS);
        }

        let page = |cursor: usize| {
            let app = &app;
            let code_review = &code_review;
            let snapshot_id = &snapshot_id;
            async move {
                handle_code_review_command(
                    app,
                    code_review,
                    RpcCommand::CodeReviewFileDiff {
                        id: None,
                        snapshot_id: snapshot_id.clone(),
                        path: "big.txt".into(),
                        cursor,
                        max_lines: Some(50),
                    },
                )
                .await
            }
        };
        let first = page(0).await;
        assert!(first.success, "{first:?}");
        let first_page = first.data.expect("page data");
        let hunks = first_page["hunks"].as_array().expect("hunks").clone();
        assert!(!hunks.is_empty());
        assert!(
            hunks.iter().all(|hunk| hunk["complete"] == false),
            "the overflowed path serves complete=false on the first (loading) pass: {first_page}"
        );

        let second = page(0).await;
        assert!(second.success, "{second:?}");
        let second_page = second.data.expect("page data");
        let cached_hunks = second_page["hunks"].as_array().expect("hunks").clone();
        assert_eq!(second_page["cursor"], 0);
        assert!(
            cached_hunks.iter().all(|hunk| hunk["complete"] == false),
            "a cache hit must NOT restore complete=true for an authority-denied path: {second_page}"
        );
        {
            let state = code_review.state.lock().await;
            let session = state.session.as_ref().expect("open review");
            assert!(
                session.file_diff_cache.peek("big.txt").is_some(),
                "the diff body stayed cached across the two requests"
            );
            assert!(session.comment_authority.overflowed("big.txt"));
        }

        let hunk = &cached_hunks[0];
        let rejected = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewComment {
                id: Some("overflow-cached-comment".into()),
                snapshot_id,
                path: "big.txt".into(),
                old_start: hunk["oldStart"].as_u64().expect("oldStart") as u32,
                old_count: hunk["oldCount"].as_u64().expect("oldCount") as u32,
                new_start: hunk["newStart"].as_u64().expect("newStart") as u32,
                new_count: hunk["newCount"].as_u64().expect("newCount") as u32,
                content_hash: hunk["contentHash"].as_str().expect("hash").to_owned(),
                comment: "must fail closed even cached".into(),
                images: Vec::new(),
            },
        )
        .await;
        assert!(
            !rejected.success,
            "the resolver must fail closed for an overflowed path even on a cache hit: {rejected:?}"
        );
        assert!(
            rejected
                .error
                .as_deref()
                .is_some_and(|e| e.contains("comment authority bound")),
            "rejection names the retained authority bound: {rejected:?}"
        );

        let closed = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewClose { id: None, review_id: None },
        )
        .await;
        assert!(closed.success, "{closed:?}");
        app.cleanup().await;
    }

    #[test]
    fn file_diff_page_projection_serializes_within_wire_cap() {
        // Quote-heavy lines expand under JSON escaping. Keep line count under
        // MAX_FILE_PAGE_LINES so paging is forced by the byte cap, not the
        // line cap. Control bytes cannot be used: sanitize_display maps them
        // to spaces and collapses the escaped cost.
        let hostile_line = "\"".repeat(2500);
        let lines: usize = 300;
        let mut patch = format!(
            "diff --git a/hostile.rs b/hostile.rs\n--- a/hostile.rs\n+++ b/hostile.rs\n@@ -0,0 +1,{lines} @@\n"
        );
        for i in 0..lines {
            patch.push_str(&format!("+{i}{hostile_line}\n"));
        }
        let files = crate::code_review::parse_unified_diff(&patch);
        assert_eq!(files.len(), 1);
        let diff = crate::code_review::FileDiff {
            path: "hostile.rs".into(),
            previous_path: None,
            status: crate::code_review::FileStatus::Modified,
            binary: false,
            hunks: files[0].hunks.clone(),
            byte_capped: false,
            error: None,
        };
        let mut cursor = 0usize;
        let mut pages = 0usize;
        while let Ok(page) = diff.slice_page("snap-1", cursor, MAX_FILE_PAGE_LINES) {
            let wire = serde_json::to_string(&file_diff_projection(&page))
                .expect("projection serializes");
            assert!(
                wire.len() <= crate::code_review::MAX_FILE_PAGE_BYTES,
                "page {} serialized to {} bytes: {wire}",
                page.cursor,
                wire.len()
            );
            pages += 1;
            match page.next_cursor {
                Some(next) => cursor = next,
                None => break,
            }
            assert!(pages <= 100, "paging did not terminate");
        }
        assert!(pages > 1, "hostile lines must be byte-bound: {pages} pages");

        let oversized_line: String = "\\\"".repeat(crate::code_review::MAX_DIFF_LINE_TEXT_BYTES / 2);
        let mut patch = format!(
            "diff --git a/huge.rs b/huge.rs\n--- a/huge.rs\n+++ b/huge.rs\n@@ -0,0 +1,2 @@\n"
        );
        for i in 0..2 {
            patch.push_str(&format!("+{i}{oversized_line}\n"));
        }
        let files = crate::code_review::parse_unified_diff(&patch);
        let diff = crate::code_review::FileDiff {
            path: "huge.rs".into(),
            previous_path: None,
            status: crate::code_review::FileStatus::Modified,
            binary: false,
            hunks: files[0].hunks.clone(),
            byte_capped: false,
            error: None,
        };
        let page = diff.slice_page("snap-1", 0, MAX_FILE_PAGE_LINES).expect("page");
        assert_eq!(page.hunks[0].lines.len(), 1, "one-line progress exception");
        let wire = serde_json::to_string(&file_diff_projection(&page)).expect("projection serializes");
        assert!(
            wire.len() <= crate::code_review::MAX_FILE_PAGE_BYTES,
            "one-line exception page serialized to {} bytes",
            wire.len()
        );
    }

    #[tokio::test]
    async fn code_review_file_diff_rejects_load_completed_during_refresh() {
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"])
            .status
            .success();
        git(&["config", "user.name", "Test"]).status.success();
        std::fs::write(cwd.path().join("race.txt"), "base\n").expect("write baseline");
        assert!(git(&["add", "race.txt"]).status.success());
        assert!(git(&["commit", "-q", "-m", "baseline"]).status.success());
        std::fs::write(cwd.path().join("race.txt"), "base\nopened\n").expect("write opened");

        let app = build_code_review_app(cwd.path()).await;
        let code_review = CodeReviewRpcState::default();
        let opened = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen {
                id: None,
                from: None,
                commit: None,
                to: None,
                include_working_tree: None,
            },
        )
        .await;
        assert!(opened.success, "{opened:?}");
        let old_snapshot_id = opened.data.as_ref().expect("open data")["snapshotId"]
            .as_str()
            .expect("snapshot id")
            .to_owned();

        let gate = Arc::new(FileDiffLoadGate {
            loaded: tokio::sync::Barrier::new(2),
            release: tokio::sync::Barrier::new(2),
        });
        *code_review.file_diff_load_gate.lock().await = Some(gate.clone());
        let load_state = code_review.clone();
        let load_app = &app;
        let load_id = old_snapshot_id.clone();
        let load = async move {
            handle_code_review_command(
                load_app,
                &load_state,
                RpcCommand::CodeReviewFileDiff {
                    id: None,
                    snapshot_id: load_id,
                    path: "race.txt".into(),
                    cursor: 0,
                    max_lines: Some(50),
                },
            )
            .await
        };
        let race = async {
            gate.loaded.wait().await;
            std::fs::write(cwd.path().join("race.txt"), "base\nrefreshed\n")
                .expect("write refreshed");
            let refreshed = handle_code_review_command(
                &app,
                &code_review,
                RpcCommand::CodeReviewRefresh { id: None },
            )
            .await;
            assert!(refreshed.success, "{refreshed:?}");
            gate.release.wait().await;
            refreshed
        };
        let (loaded, refreshed) = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            async { tokio::join!(load, race) },
        )
        .await
        .expect("load held a state lock across the gate barriers and deadlocked the racing refresh");
        assert!(!loaded.success, "in-flight old page was served: {loaded:?}");
        assert!(
            loaded
                .error
                .as_deref()
                .is_some_and(|error| error.contains("stale snapshot"))
        );
        assert_ne!(
            refreshed.data.as_ref().expect("refresh data")["snapshotId"],
            old_snapshot_id
        );

        assert!(
            code_review_file_diff_cache_paths(&code_review)
                .await
                .is_empty(),
            "a load that completed under a superseded snapshot must not be cached"
        );

        *code_review.file_diff_load_gate.lock().await = None;
        let closed = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewClose { id: None, review_id: None },
        )
        .await;
        assert!(closed.success);
        app.cleanup().await;
    }


    #[tokio::test]
    async fn code_review_file_diff_coalesces_duplicate_concurrent_loads() {
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"])
            .status
            .success();
        git(&["config", "user.name", "Test"]).status.success();
        std::fs::write(cwd.path().join("race.txt"), "base\n").expect("write baseline");
        assert!(git(&["add", "race.txt"]).status.success());
        assert!(git(&["commit", "-q", "-m", "baseline"]).status.success());
        std::fs::write(cwd.path().join("race.txt"), "base\nopened\n").expect("write opened");

        let app = build_code_review_app(cwd.path()).await;
        let code_review = CodeReviewRpcState::default();
        let opened = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen {
                id: None,
                from: None,
                commit: None,
                to: None,
                include_working_tree: None,
            },
        )
        .await;
        assert!(opened.success, "{opened:?}");
        let snapshot_id = opened.data.as_ref().expect("open data")["snapshotId"]
            .as_str()
            .expect("snapshot id")
            .to_owned();

        let gate = Arc::new(FileDiffLoadGate {
            loaded: tokio::sync::Barrier::new(2),
            release: tokio::sync::Barrier::new(2),
        });
        *code_review.file_diff_load_gate.lock().await = Some(gate.clone());
        let make_request = || {
            let code_review = code_review.clone();
            let snapshot_id = snapshot_id.clone();
            let app = &app;
            async move {
                handle_code_review_command(
                    app,
                    &code_review,
                    RpcCommand::CodeReviewFileDiff {
                        id: None,
                        snapshot_id,
                        path: "race.txt".into(),
                        cursor: 0,
                        max_lines: Some(50),
                    },
                )
                .await
            }
        };
        let race = {
            let gate = gate.clone();
            async move {
                gate.loaded.wait().await;
                gate.release.wait().await;
            }
        };
        let joined = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            async { tokio::join!(make_request(), make_request(), race) },
        )
        .await
        .expect("single-flight violated: a duplicate request ran its own physical load and deadlocked on the load gate");
        let (first, second, ()) = joined;
        assert!(first.success, "{first:?}");
        assert!(second.success, "{second:?}");
        assert_eq!(
            first.data, second.data,
            "both concurrent requests must slice the SAME loaded FileDiff"
        );
        assert_eq!(
            code_review
                .file_diff_load_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly ONE physical git load must serve both concurrent requests"
        );

        *code_review.file_diff_load_gate.lock().await = None;
        let closed = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewClose { id: None, review_id: None },
        )
        .await;
        assert!(closed.success);
        app.cleanup().await;
    }

    #[tokio::test]
    async fn code_review_file_diff_shares_uncacheable_outcome_among_waiters() {
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"])
            .status
            .success();
        git(&["config", "user.name", "Test"]).status.success();
        std::fs::write(cwd.path().join("race.txt"), "base\n").expect("write baseline");
        assert!(git(&["add", "race.txt"]).status.success());
        assert!(git(&["commit", "-q", "-m", "baseline"]).status.success());
        // A single oversize untracked file provides the deterministic
        // uncacheable outcome: larger than the per-file diff cap, it is
        // catalogued as an on-demand placeholder whose body is never pinned,
        // so its per-file load fails identically for every caller and must
        // never enter the file-diff cache.
        let oversize = cwd.path().join("oversize.txt");
        std::fs::File::create(&oversize)
            .expect("create oversize file")
            .set_len(crate::code_review::MAX_FILE_DIFF_BYTES as u64 + 1)
            .expect("grow oversize file");

        let app = build_code_review_app(cwd.path()).await;
        let code_review = CodeReviewRpcState::default();
        let opened = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen {
                id: None,
                from: None,
                commit: None,
                to: None,
                include_working_tree: None,
            },
        )
        .await;
        assert!(opened.success, "{opened:?}");
        let snapshot_id = opened.data.as_ref().expect("open data")["snapshotId"]
            .as_str()
            .expect("snapshot id")
            .to_owned();

        let gate = Arc::new(FileDiffLoadGate {
            loaded: tokio::sync::Barrier::new(2),
            release: tokio::sync::Barrier::new(2),
        });
        *code_review.file_diff_load_gate.lock().await = Some(gate.clone());
        let make_request = || {
            let code_review = code_review.clone();
            let snapshot_id = snapshot_id.clone();
            let app = &app;
            async move {
                handle_code_review_command(
                    app,
                    &code_review,
                    RpcCommand::CodeReviewFileDiff {
                        id: None,
                        snapshot_id,
                        path: "oversize.txt".into(),
                        cursor: 0,
                        max_lines: Some(50),
                    },
                )
                .await
            }
        };
        let race = {
            let gate = gate.clone();
            async move {
                gate.loaded.wait().await;
                gate.release.wait().await;
            }
        };
        let joined = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            async { tokio::join!(make_request(), make_request(), race) },
        )
        .await
        .expect("single-flight violated: a duplicate request ran its own physical load and deadlocked on the load gate");
        let (first, second, ()) = joined;
        assert!(!first.success, "{first:?}");
        assert!(
            first
                .error
                .as_deref()
                .is_some_and(|error| error.contains("untracked file body was not captured")),
            "the shared outcome must be the deterministic unpinned-body failure: {first:?}"
        );
        assert!(!second.success, "{second:?}");
        assert_eq!(
            first.error, second.error,
            "both concurrent requests must consume the SAME shared terminal outcome"
        );
        assert_eq!(
            code_review
                .file_diff_load_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly ONE physical file-diff load must serve both concurrent requests even when the outcome is not cacheable"
        );
        assert!(
            code_review_file_diff_cache_paths(&code_review)
                .await
                .is_empty(),
            "an error outcome must not enter the file-diff cache",
        );

        *code_review.file_diff_load_gate.lock().await = None;
        let closed = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewClose { id: None, review_id: None },
        )
        .await;
        assert!(closed.success);
        app.cleanup().await;
    }

    #[tokio::test]
    async fn code_review_file_diff_serves_every_file_after_global_truncation() {
        // A repo whose combined HEAD→working-tree diff exceeds the 2 MiB
        // snapshot cap: each of two big files has a ~2.6 MiB diff, so the
        // first (path-sorted) file consumes the whole cap and the second big
        // file plus the later files are guaranteed on-demand placeholders.
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", cwd.path().to_str().expect("cwd str")])
                .args(args)
                .output()
                .expect("git invocation")
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "test@example.com"])
            .status
            .success();
        git(&["config", "user.name", "Test"]).status.success();
        std::fs::write(cwd.path().join("big-a.txt"), "base\n").expect("write big-a");
        std::fs::write(cwd.path().join("big-b.txt"), "base\n").expect("write big-b");
        std::fs::write(
            cwd.path().join("rename-old.txt"),
            "alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta\n",
        )
        .expect("write rename source");
        std::fs::write(cwd.path().join("zz-later.txt"), "base\n").expect("write later");
        assert!(git(&["add", "-A"]).status.success());
        assert!(git(&["commit", "-q", "-m", "baseline"]).status.success());
        let changed = "changed-line\n".repeat(200_000);
        std::fs::write(cwd.path().join("big-a.txt"), &changed).expect("write big-a changed");
        std::fs::write(cwd.path().join("big-b.txt"), &changed).expect("write big-b changed");
        std::fs::rename(
            cwd.path().join("rename-old.txt"),
            cwd.path().join("rename-new.txt"),
        )
        .expect("rename");
        std::fs::write(
            cwd.path().join("rename-new.txt"),
            "alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta\nrenamed\n",
        )
        .expect("write rename destination");
        std::fs::write(cwd.path().join("zz-later.txt"), "base\nchanged\n")
            .expect("write later changed");
        assert!(git(&["add", "-A"]).status.success());

        let app = build_code_review_app(cwd.path()).await;
        let code_review = CodeReviewRpcState::default();
        let opened = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen {
                id: None,
                from: None,
                commit: None,
                to: None,
                include_working_tree: None,
            },
        )
        .await;
        assert!(opened.success, "{opened:?}");
        let data = opened.data.expect("open data");
        assert_eq!(
            data["truncated"], true,
            "snapshot must be globally truncated: {data}"
        );
        assert_eq!(data["totalsComplete"], true, "{data}");
        assert_eq!(data["totalInsertions"], 400_002, "{data}");
        // git ground truth: each big file replaces its single baseline line
        // (-1 each), while the rename and the zz-later append add lines
        // without deleting any — two deletions in total.
        assert_eq!(data["totalDeletions"], 2, "{data}");
        let files = data["files"].as_array().expect("files");
        let paths: Vec<&str> = files
            .iter()
            .map(|file| file["path"].as_str().expect("path"))
            .collect();
        for expected in ["big-a.txt", "big-b.txt", "rename-new.txt", "zz-later.txt"] {
            assert!(
                paths.contains(&expected),
                "changed file missing from truncated projection: {paths:?}"
            );
        }
        let snapshot_id = data["snapshotId"].as_str().expect("snapshot id").to_owned();

        // The path-sorted late file is a guaranteed placeholder. Mutate it
        // after open but before its first FileDiff: the old snapshot must page
        // the prepared index, never the newer worktree bytes.
        let placeholder = files
            .iter()
            .find(|file| file["path"] == "zz-later.txt")
            .expect("late placeholder");
        assert_eq!(placeholder["bodyState"], "onDemand");
        assert!(placeholder["hunks"].as_array().expect("hunks").is_empty());
        let placeholder_path = placeholder["path"]
            .as_str()
            .expect("placeholder path")
            .to_owned();
        assert_eq!(
            placeholder["message"],
            "diff omitted: combined diff truncated; loaded on demand"
        );
        std::fs::write(cwd.path().join("zz-later.txt"), "base\nnewer-after-open\n")
            .expect("mutate after open");
        let placeholder_diff = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewFileDiff {
                id: None,
                snapshot_id: snapshot_id.clone(),
                path: placeholder_path,
                cursor: 0,
                max_lines: Some(50),
            },
        )
        .await;
        assert!(placeholder_diff.success, "{placeholder_diff:?}");
        let placeholder_page = placeholder_diff.data.expect("placeholder page");
        assert!(placeholder_page["totalLines"].as_u64().expect("total") > 0);
        let placeholder_hunks = placeholder_page["hunks"].as_array().expect("hunks");
        assert!(!placeholder_hunks.is_empty(), "{placeholder_page}");
        let placeholder_lines: usize = placeholder_hunks
            .iter()
            .map(|hunk| hunk["lines"].as_array().expect("hunk lines").len())
            .sum();
        assert!(placeholder_lines > 0);
        let old_page_text = placeholder_hunks
            .iter()
            .flat_map(|hunk| hunk["lines"].as_array().expect("hunk lines"))
            .filter_map(|line| line["text"].as_str())
            .collect::<Vec<_>>();
        assert!(
            old_page_text.contains(&"changed"),
            "old page lost open-time content: {old_page_text:?}"
        );
        assert!(
            !old_page_text.contains(&"newer-after-open"),
            "old page read post-open worktree content: {old_page_text:?}"
        );

        // Even a real identity derived from the newer worktree cannot be
        // authorized under the old snapshot id: the cached old FileDiff is
        // authoritative for comment resolution.
        let (newer_snapshot, newer_paging) =
            load_review_snapshot_capture(cwd.path(), ReviewScope::WorkingTree);
        let newer_file = newer_snapshot
            .files
            .iter()
            .find(|file| file.path == "zz-later.txt")
            .expect("newer file");
        let newer_diff = FileDiff::load_snapshot_file(
            cwd.path(),
            &ReviewScope::WorkingTree,
            &newer_paging,
            newer_file,
        );
        let newer_hunk = newer_diff.hunks.first().expect("newer hunk");
        let newer_identity = HunkIdentity::new(&snapshot_id, "zz-later.txt", newer_hunk);
        let forged_newer_comment = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewComment {
                id: None,
                snapshot_id: snapshot_id.clone(),
                path: "zz-later.txt".into(),
                old_start: newer_identity.old_start,
                old_count: newer_identity.old_count,
                new_start: newer_identity.new_start,
                new_count: newer_identity.new_count,
                content_hash: newer_identity.content_hash,
                comment: "must not authorize newer bytes".into(),
                images: Vec::new(),
            },
        )
        .await;
        assert!(!forged_newer_comment.success, "{forged_newer_comment:?}");
        assert!(
            forged_newer_comment
                .error
                .as_deref()
                .is_some_and(|error| error.contains("stale hunk")),
            "{forged_newer_comment:?}"
        );

        // Every catalogued path in the truncated snapshot serves a bounded
        // first page — containment must accept placeholders like any file.
        for file in files {
            let path = file["path"].as_str().expect("path").to_owned();
            let response = handle_code_review_command(
                &app,
                &code_review,
                RpcCommand::CodeReviewFileDiff {
                    id: None,
                    snapshot_id: snapshot_id.clone(),
                    path,
                    cursor: 0,
                    max_lines: Some(25),
                },
            )
            .await;
            assert!(response.success, "{response:?}");
            let page = response.data.expect("page data");
            let page_lines: usize = page["hunks"]
                .as_array()
                .expect("hunks")
                .iter()
                .map(|hunk| hunk["lines"].as_array().expect("hunk lines").len())
                .sum();
            assert!(
                page_lines > 0,
                "file diff must be readable after truncation: {page}"
            );
        }

        let refreshed = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewRefresh { id: None },
        )
        .await;
        assert!(refreshed.success, "{refreshed:?}");
        let refreshed_id = refreshed.data.as_ref().expect("refresh data")["snapshotId"]
            .as_str()
            .expect("refreshed id")
            .to_owned();
        assert_ne!(refreshed_id, snapshot_id);
        let refreshed_page = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewFileDiff {
                id: None,
                snapshot_id: refreshed_id,
                path: "zz-later.txt".into(),
                cursor: 0,
                max_lines: Some(50),
            },
        )
        .await;
        assert!(refreshed_page.success, "{refreshed_page:?}");
        let refreshed_page = refreshed_page.data.expect("refreshed page");
        let refreshed_text = refreshed_page["hunks"]
            .as_array()
            .expect("hunks")
            .iter()
            .flat_map(|hunk| hunk["lines"].as_array().expect("hunk lines"))
            .filter_map(|line| line["text"].as_str())
            .collect::<Vec<_>>();
        assert!(
            refreshed_text.contains(&"newer-after-open"),
            "{refreshed_text:?}"
        );
        assert!(!refreshed_text.contains(&"changed"), "{refreshed_text:?}");

        let closed = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewClose { id: None, review_id: None },
        )
        .await;
        assert!(closed.success);
        app.cleanup().await;
    }

    #[test]
    fn resolve_comment_target_rejects_stale_identity() {
        use crate::code_review::{DiffLine, FileStatus, ReviewScope};
        let file = DiffFile {
            path: "src/lib.rs".into(),
            previous_path: None,
            status: FileStatus::Modified,
            binary: false,
            insertions: 1,
            deletions: 1,
            hunks: vec![DiffHunk {
                header: "@@ -1,1 +1,1 @@".into(),
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
                lines: vec![DiffLine {
                    kind: DiffLineKind::Context,
                    old_no: Some(1),
                    new_no: Some(1),
                    text: "old".into(),
                }],
            }],
            truncated: false,
            message: None,
        };
        let snapshot = ReviewSnapshot {
            root: PathBuf::from("/tmp/fake-root"),
            scope: ReviewScope::WorkingTree,
            snapshot_id: "snap-1".into(),
            files: vec![file],
            truncated: false,
            total_insertions: 0,
            total_deletions: 0,
            summary_complete: true,
            error: None,
        };
        let identity = snapshot.hunk_identity(&snapshot.files[0], &snapshot.files[0].hunks[0]);

        // A fully matching identity resolves to the (file, hunk).
        let resolved = resolve_comment_target(
            &snapshot,
            None,
            None as Option<&CommentAuthority>,
            &identity.snapshot_id,
            "src/lib.rs",
            1,
            1,
            1,
            1,
            &identity.content_hash,
        )
        .expect("matching identity resolves");
        match resolved {
            ResolvedHunk::Snapshot { file, hunk_index } => {
                assert_eq!(file.path, "src/lib.rs");
                assert_eq!(file.hunks[hunk_index].old_start, 1);
            }
            ResolvedHunk::Loaded { .. } | ResolvedHunk::Authority { .. } => {
                panic!("no cache: must resolve against the snapshot")
            }
        }

        // Stale snapshot id is rejected.
        let err = resolve_comment_target(
            &snapshot,
            None,
            None as Option<&CommentAuthority>,
            "other-snapshot",
            "src/lib.rs",
            1,
            1,
            1,
            1,
            &identity.content_hash,
        )
        .unwrap_err();
        assert!(err.to_string().contains("stale snapshot"), "{err}");

        // Unknown path is rejected.
        let err = resolve_comment_target(
            &snapshot,
            None,
            None as Option<&CommentAuthority>,
            &identity.snapshot_id,
            "missing.rs",
            1,
            1,
            1,
            1,
            &identity.content_hash,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown file"), "{err}");

        // Wrong hunk range is rejected.
        let err = resolve_comment_target(
            &snapshot,
            None,
            None as Option<&CommentAuthority>,
            &identity.snapshot_id,
            "src/lib.rs",
            5,
            1,
            1,
            1,
            &identity.content_hash,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown hunk"), "{err}");

        // Stale content hash (diff content changed) is rejected.
        let err = resolve_comment_target(
            &snapshot,
            None,
            None as Option<&CommentAuthority>,
            &identity.snapshot_id,
            "src/lib.rs",
            1,
            1,
            1,
            1,
            "wrong-hash",
        )
        .unwrap_err();
        assert!(err.to_string().contains("stale hunk"), "{err}");
    }

    #[test]
    fn resolve_comment_target_uses_cached_loaded_hunks_authoritatively() {
        use crate::code_review::{DiffLine, FileStatus, ReviewScope};
        use std::sync::Arc;
        // A globally-truncated placeholder file: the snapshot carries NO hunks
        // (the frontend loads them per-file). The cached FileDiff is the only
        // valid hunk source; snapshot identities must never resolve.
        let snapshot_file = DiffFile {
            path: "placeholder.txt".into(),
            previous_path: None,
            status: FileStatus::Modified,
            binary: false,
            insertions: 0,
            deletions: 0,
            hunks: Vec::new(),
            truncated: true,
            message: Some("diff omitted: combined diff truncated; loaded on demand".into()),
        };
        let snapshot = ReviewSnapshot {
            root: PathBuf::from("/tmp/fake-root"),
            scope: ReviewScope::WorkingTree,
            snapshot_id: "snap-1".into(),
            files: vec![snapshot_file],
            truncated: true,
            total_insertions: 0,
            total_deletions: 0,
            summary_complete: true,
            error: None,
        };
        let loaded_hunk = DiffHunk {
            header: "@@ -1,2 +1,2 @@".into(),
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
            lines: vec![
                DiffLine {
                    kind: DiffLineKind::Deletion,
                    old_no: Some(1),
                    new_no: None,
                    text: "later base".into(),
                },
                DiffLine {
                    kind: DiffLineKind::Addition,
                    old_no: None,
                    new_no: Some(1),
                    text: "later changed".into(),
                },
            ],
        };
        let loaded = FileDiff {
            path: "placeholder.txt".into(),
            previous_path: None,
            status: FileStatus::Modified,
            binary: false,
            hunks: vec![loaded_hunk.clone()],
            byte_capped: false,
            error: None,
        };
        let mut cache = FileDiffCache::default();
        cache.put("placeholder.txt".into(), Arc::new(loaded));

        // The loaded hunk identity resolves to the cached hunk, not the
        // snapshot's (empty) placeholder set.
        let identity = HunkIdentity::new("snap-1", "placeholder.txt", &loaded_hunk);
        let resolved = resolve_comment_target(
            &snapshot,
            Some(&cache),
            None as Option<&CommentAuthority>,
            &identity.snapshot_id,
            "placeholder.txt",
            identity.old_start,
            identity.old_count,
            identity.new_start,
            identity.new_count,
            &identity.content_hash,
        )
        .expect("cached loaded hunk resolves");
        match resolved {
            ResolvedHunk::Loaded {
                file,
                diff,
                hunk_index,
            } => {
                assert_eq!(file.path, "placeholder.txt");
                assert_eq!(diff.hunks[hunk_index], loaded_hunk);
            }
            ResolvedHunk::Snapshot { .. } | ResolvedHunk::Authority { .. } => {
                panic!("cached path must resolve via the cache")
            }
        }

        // Wrong hash for the cached hunk is rejected (never falls back).
        let err = resolve_comment_target(
            &snapshot,
            Some(&cache),
            None as Option<&CommentAuthority>,
            &identity.snapshot_id,
            "placeholder.txt",
            identity.old_start,
            identity.old_count,
            identity.new_start,
            identity.new_count,
            "wrong-hash",
        )
        .unwrap_err();
        assert!(err.to_string().contains("stale hunk"), "{err}");

        // Wrong range for the cached path is rejected as an unknown hunk.
        let err = resolve_comment_target(
            &snapshot,
            Some(&cache),
            None as Option<&CommentAuthority>,
            &identity.snapshot_id,
            "placeholder.txt",
            42,
            1,
            1,
            1,
            &identity.content_hash,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown hunk"), "{err}");

        // A byte-capped INCOMPLETE cached hunk is rejected: its identity is
        // not a valid comment target (content cut by the size limit).
        let partial_hunk = DiffHunk {
            header: "@@ -1,3 +1,3 @@".into(),
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 3,
            lines: vec![
                DiffLine {
                    kind: DiffLineKind::Context,
                    old_no: Some(1),
                    new_no: Some(1),
                    text: "a".into(),
                },
                DiffLine {
                    kind: DiffLineKind::Deletion,
                    old_no: Some(2),
                    new_no: None,
                    text: "b".into(),
                },
            ],
        };
        assert!(!partial_hunk.is_complete());
        let mut capped_cache = FileDiffCache::default();
        let capped_diff = Arc::new(FileDiff {
            path: "placeholder.txt".into(),
            previous_path: None,
            status: FileStatus::Modified,
            binary: false,
            hunks: vec![partial_hunk.clone()],
            byte_capped: true,
            error: None,
        });
        capped_cache.put("placeholder.txt".into(), capped_diff);
        let partial_identity = HunkIdentity::new("snap-1", "placeholder.txt", &partial_hunk);
        let err = resolve_comment_target(
            &snapshot,
            Some(&capped_cache),
            None as Option<&CommentAuthority>,
            &partial_identity.snapshot_id,
            "placeholder.txt",
            partial_identity.old_start,
            partial_identity.old_count,
            partial_identity.new_start,
            partial_identity.new_count,
            &partial_identity.content_hash,
        )
        .unwrap_err();
        assert!(err.to_string().contains("incomplete hunk"), "{err}");

        let counts_ok_hunk = DiffHunk {
            header: "@@ -1,1 +1,1 @@".into(),
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
            lines: vec![DiffLine {
                kind: DiffLineKind::Context,
                old_no: Some(1),
                new_no: Some(1),
                text: "no-eol tail".into(),
            }],
        };
        assert!(counts_ok_hunk.is_complete());
        let mut meta_cut_cache = FileDiffCache::default();
        meta_cut_cache.put(
            "placeholder.txt".into(),
            Arc::new(FileDiff {
                path: "placeholder.txt".into(),
                previous_path: None,
                status: FileStatus::Modified,
                binary: false,
                hunks: vec![counts_ok_hunk.clone()],
                byte_capped: true,
                error: None,
            }),
        );
        let meta_cut_identity = HunkIdentity::new("snap-1", "placeholder.txt", &counts_ok_hunk);
        let err = resolve_comment_target(
            &snapshot,
            Some(&meta_cut_cache),
            None as Option<&CommentAuthority>,
            &meta_cut_identity.snapshot_id,
            "placeholder.txt",
            meta_cut_identity.old_start,
            meta_cut_identity.old_count,
            meta_cut_identity.new_start,
            meta_cut_identity.new_count,
            &meta_cut_identity.content_hash,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("incomplete hunk"),
            "a counts-satisfied byte-capped final hunk must still be rejected: {err}"
        );

        // A snapshot with a COMPLETE hunk still resolves against the snapshot
        // when no cache exists (the pre-load window for non-truncated files).
        let mut complete_snapshot = snapshot.clone();
        complete_snapshot.files[0].hunks = vec![loaded_hunk.clone()];
        complete_snapshot.files[0].truncated = false;
        let resolved = resolve_comment_target(
            &complete_snapshot,
            None,
            None as Option<&CommentAuthority>,
            &identity.snapshot_id,
            "placeholder.txt",
            identity.old_start,
            identity.old_count,
            identity.new_start,
            identity.new_count,
            &identity.content_hash,
        )
        .expect("complete snapshot hunk resolves without a cache");
        assert!(matches!(resolved, ResolvedHunk::Snapshot { .. }));

        // A globally truncated snapshot's PARTIAL last hunk is rejected even
        // without a cache (its identity is incomplete).
        let mut partial_snapshot = snapshot.clone();
        partial_snapshot.files[0].hunks = vec![partial_hunk.clone()];
        let partial_identity = HunkIdentity::new("snap-1", "placeholder.txt", &partial_hunk);
        let err = resolve_comment_target(
            &partial_snapshot,
            None,
            None as Option<&CommentAuthority>,
            &partial_identity.snapshot_id,
            "placeholder.txt",
            partial_identity.old_start,
            partial_identity.old_count,
            partial_identity.new_start,
            partial_identity.new_count,
            &partial_identity.content_hash,
        )
        .unwrap_err();
        assert!(err.to_string().contains("incomplete hunk"), "{err}");
    }

    #[test]
    fn resolve_comment_target_resolves_served_uncached_authority() {
        use crate::code_review::{DiffLine, FileStatus, ReviewScope};
        let snapshot_file = DiffFile {
            path: "placeholder.txt".into(),
            previous_path: None,
            status: FileStatus::Modified,
            binary: false,
            insertions: 0,
            deletions: 0,
            hunks: Vec::new(),
            truncated: true,
            message: Some("diff omitted: combined diff truncated; loaded on demand".into()),
        };
        let snapshot = ReviewSnapshot {
            root: PathBuf::from("/tmp/fake-root"),
            scope: ReviewScope::WorkingTree,
            snapshot_id: "snap-1".into(),
            files: vec![snapshot_file],
            truncated: true,
            total_insertions: 0,
            total_deletions: 0,
            summary_complete: true,
            error: None,
        };
        let loaded_hunk = DiffHunk {
            header: "@@ -1,2 +1,2 @@".into(),
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
            lines: vec![
                DiffLine {
                    kind: DiffLineKind::Deletion,
                    old_no: Some(1),
                    new_no: None,
                    text: "later base".into(),
                },
                DiffLine {
                    kind: DiffLineKind::Addition,
                    old_no: None,
                    new_no: Some(1),
                    text: "later changed".into(),
                },
            ],
        };
        let loaded = FileDiff {
            path: "placeholder.txt".into(),
            previous_path: None,
            status: FileStatus::Modified,
            binary: false,
            hunks: vec![loaded_hunk.clone()],
            byte_capped: false,
            error: None,
        };
        let mut authority = CommentAuthority::default();
        assert!(authority.record("placeholder.txt", &loaded));
        assert!(authority.lookup("placeholder.txt").is_some());
        let identity = HunkIdentity::new("snap-1", "placeholder.txt", &loaded_hunk);

        let resolved = resolve_comment_target(
            &snapshot,
            None,
            Some(&authority),
            &identity.snapshot_id,
            "placeholder.txt",
            identity.old_start,
            identity.old_count,
            identity.new_start,
            identity.new_count,
            &identity.content_hash,
        )
        .expect("served-uncached identity resolves");
        match resolved {
            ResolvedHunk::Authority { file, record } => {
                assert_eq!(file.path, "placeholder.txt");
                assert!(record.comment_ready);
                assert_eq!(record.old_start, identity.old_start);
                assert_eq!(record.content_hash, identity.content_hash);
            }
            ResolvedHunk::Snapshot { .. } | ResolvedHunk::Loaded { .. } => {
                panic!("uncached path must resolve via the authority map")
            }
        }

        let err = resolve_comment_target(
            &snapshot,
            None,
            Some(&authority),
            &identity.snapshot_id,
            "placeholder.txt",
            identity.old_start,
            identity.old_count,
            identity.new_start,
            identity.new_count,
            "deadbeef",
        )
        .unwrap_err();
        assert!(err.to_string().contains("stale hunk"), "{err}");

        let err = resolve_comment_target(
            &snapshot,
            None,
            Some(&authority),
            &identity.snapshot_id,
            "placeholder.txt",
            999,
            identity.old_count,
            999,
            identity.new_count,
            &identity.content_hash,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown hunk"), "{err}");

        let mut partial_hunk = loaded_hunk.clone();
        partial_hunk.lines.pop();
        partial_hunk.new_count = 2;
        partial_hunk.header = "@@ -1,2 +1,2 @@".into();
        let partial = FileDiff {
            path: "placeholder.txt".into(),
            previous_path: None,
            status: FileStatus::Modified,
            binary: false,
            hunks: vec![partial_hunk.clone()],
            byte_capped: true,
            error: None,
        };
        let mut capped_authority = CommentAuthority::default();
        assert!(capped_authority.record("placeholder.txt", &partial));
        let partial_identity = HunkIdentity::new("snap-1", "placeholder.txt", &partial_hunk);
        let err = resolve_comment_target(
            &snapshot,
            None,
            Some(&capped_authority),
            &partial_identity.snapshot_id,
            "placeholder.txt",
            partial_identity.old_start,
            partial_identity.old_count,
            partial_identity.new_start,
            partial_identity.new_count,
            &partial_identity.content_hash,
        )
        .unwrap_err();
        assert!(err.to_string().contains("incomplete hunk"), "{err}");

        let mut overflow = CommentAuthority::default();
        overflow.overflow.insert("placeholder.txt".to_owned());
        let err = resolve_comment_target(
            &snapshot,
            None,
            Some(&overflow),
            &identity.snapshot_id,
            "placeholder.txt",
            identity.old_start,
            identity.old_count,
            identity.new_start,
            identity.new_count,
            &identity.content_hash,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("comment authority bound"),
            "{err}"
        );
    }

    #[test]
    fn comment_authority_record_replaces_and_bounds_retention() {
        use crate::code_review::{DiffLine, FileStatus};
        let hunk = |text: &str| DiffHunk {
            header: "@@ -1,1 +1,1 @@".into(),
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
            lines: vec![DiffLine {
                kind: DiffLineKind::Addition,
                old_no: None,
                new_no: Some(1),
                text: text.to_owned(),
            }],
        };
        let diff = |texts: &[&str]| FileDiff {
            path: "a.txt".into(),
            previous_path: None,
            status: FileStatus::Modified,
            binary: false,
            hunks: texts.iter().map(|text| hunk(text)).collect(),
            byte_capped: false,
            error: None,
        };
        let mut authority = CommentAuthority::default();

        assert!(authority.record("a.txt", &diff(&["one", "two"])));
        assert_eq!(authority.total, 2);
        assert!(authority.record("a.txt", &diff(&["one"])));
        assert_eq!(authority.total, 1, "reload replaces, never double-charges");
        let mut authority = CommentAuthority::default();
        let texts: Vec<String> = (0..MAX_COMMENT_AUTHORITY_HUNKS + 1)
            .map(|i| format!("hunk {i}"))
            .collect();
        let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
        assert!(!authority.record("big.txt", &diff(&texts)));
        assert_eq!(
            authority.total, 0,
            "a dropped path's budget is never stranded: total counts only retained records"
        );
        assert!(authority.overflowed("big.txt"));
        assert!(authority.lookup("big.txt").is_none());
        assert!(authority.record("small.txt", &diff(&["one"])));
        assert_eq!(authority.total, 1);
    }

    #[test]
    fn code_review_abort_requires_full_hunk_identity_on_wire() {
        // Clean cutover: bare abort (no identity) no longer deserializes.
        let bare = parse_input(br#"{"type":"code_review_abort"}"#);
        assert!(
            bare.as_ref().err().is_some_and(|response| {
                !response.success && response.command == "code_review_abort"
            }),
            "code_review_abort must require full hunk identity: {bare:?}"
        );

        // Full identity matches code_review_comment's identity fields.
        let full = parse_input(
            br#"{"type":"code_review_abort","snapshotId":"s1","path":"a.rs","oldStart":1,"oldCount":1,"newStart":2,"newCount":3,"contentHash":"hash"}"#,
        )
        .expect("full identity abort");
        match full {
            RpcInput::Command {
                command:
                    RpcCommand::CodeReviewAbort {
                        snapshot_id,
                        path,
                        old_start,
                        old_count,
                        new_start,
                        new_count,
                        content_hash,
                        ..
                    },
                ..
            } => {
                assert_eq!(snapshot_id, "s1");
                assert_eq!(path, "a.rs");
                assert_eq!(old_start, 1);
                assert_eq!(old_count, 1);
                assert_eq!(new_start, 2);
                assert_eq!(new_count, 3);
                assert_eq!(content_hash, "hash");
            }
            other => panic!("expected CodeReviewAbort, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn code_review_projection_exposes_active_count_and_thread_model_fields() {
        let cwd = tempfile::tempdir().expect("git cwd");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(cwd.path())
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .status()
                .expect("git")
        };
        assert!(git(&["init", "-q"]).success());
        assert!(git(&["config", "user.email", "t@example.com"]).success());
        assert!(git(&["config", "user.name", "t"]).success());
        std::fs::write(cwd.path().join("a.rs"), "fn main() {}\n").expect("write");
        assert!(git(&["add", "a.rs"]).success());
        assert!(git(&["commit", "-q", "-m", "init"]).success());
        std::fs::write(cwd.path().join("a.rs"), "fn main() { println!(\"x\"); }\n").expect("write");

        let app = build_code_review_app(cwd.path()).await;
        let code_review = CodeReviewRpcState::default();
        let opened = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen {
                id: Some("open-proj".into()),
                from: None,
                commit: None,
                to: None,
                include_working_tree: None,
            },
        )
        .await;
        assert!(opened.success, "{opened:?}");
        let data = opened.data.expect("open data");
        // Aggregate streaming + bounded active count; activeHunk is gone.
        assert_eq!(data["internalBusy"], false);
        assert_eq!(data["isStreaming"], false);
        assert_eq!(data["activeCount"], 0);
        assert!(
            data.get("activeHunk").is_none(),
            "activeHunk must be removed: {data}"
        );
        assert!(data["threads"].as_array().expect("threads").is_empty());

        // Snapshot re-projects the same shape.
        let snap = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewSnapshot {
                id: Some("snap-proj".into()),
            },
        )
        .await;
        assert!(snap.success, "{snap:?}");
        let snap_data = snap.data.expect("snap");
        assert_eq!(snap_data["internalBusy"], false);
        assert_eq!(snap_data["activeCount"], 0);
        assert!(snap_data.get("activeHunk").is_none());

        // Abort without a matching identity is rejected as stale/unknown.
        let abort = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewAbort {
                id: Some("abort-stale".into()),
                snapshot_id: "wrong".into(),
                path: "a.rs".into(),
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
                content_hash: "h".into(),
            },
        )
        .await;
        assert!(!abort.success, "{abort:?}");
        assert!(
            abort
                .error
                .as_deref()
                .is_some_and(|error| error.contains("stale snapshot")),
            "{abort:?}"
        );

        let closed = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewClose { id: Some("close-proj".into()), review_id: None },
        )
        .await;
        assert!(closed.success, "{closed:?}");
        app.cleanup().await;
    }

    /// Session with dense history so snap compact has something to archive
    /// (mirrors the pi-coding snap_compact_tests fixture: 12 large turns,
    /// keep 2, never calls the provider).
    async fn build_snapcompact_app(model_id: &str) -> Application {
        let mut model = faux_model();
        model.id = model_id.into();
        model.name = model_id.into();
        let cwd = tempfile::tempdir().expect("tempdir");
        let session = disabled_session(pi_coding::SessionOptions {
            model,
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "faux".into(),
            compaction: Some(pi_coding::CompactionSettings {
                enabled: true,
                reserve_tokens: 20,
                keep_recent_tokens: 4,
                snap_keep_turns: 2,
            }),
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(reply_stream("unused snap reply")),
            auth_resolver: None,
            auth_manager: None,
        })
        .expect("session");
        let padding = "x".repeat(200);
        let mut history = Vec::new();
        for turn in 0..12 {
            history.push(Message::user_text(
                format!("ask number {turn}: {padding}"),
                turn as i64 * 2,
            ));
            let mut assistant = pi_ai::AssistantMessage::pending(&Model::default());
            assistant.content = vec![ContentBlock::text(format!("answer {turn}: {padding}"))];
            assistant.stop_reason = pi_ai::StopReason::Stop;
            assistant.timestamp = turn as i64 * 2 + 1;
            history.push(Message::Assistant(assistant));
        }
        session
            .load_history(history)
            .await
            .expect("load dense history");
        isolated_application(session).await
    }

    #[tokio::test]
    async fn snapcompact_rpc_reports_a_to_b_tokens_without_provider() {
        let app = build_snapcompact_app("faux-rpc-snapcompact").await;
        let response = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::SnapCompact {
                id: Some("snap-1".into()),
            },
        )
        .await;
        assert!(response.success, "{response:?}");
        assert_eq!(response.command, "snapcompact");
        let data = response.data.expect("compaction result");
        let before = data["tokensBefore"].as_i64().expect("tokensBefore");
        let after = data["estimatedTokensAfter"]
            .as_i64()
            .expect("estimatedTokensAfter");
        assert!(
            after < before,
            "snapcompact must shrink the context: {before} -> {after}"
        );
        assert!(
            data["firstKeptEntryId"]
                .as_str()
                .is_some_and(|id| !id.is_empty()),
            "snapcompact must report the first kept entry: {data}"
        );
        app.cleanup().await;
    }

    #[tokio::test]
    async fn rewind_rpc_lists_requires_one_target_and_rolls_back() {
        let app = build_todo_app("faux-rpc-rewind", "faux-rpc-rewind-api").await;
        let session_dir = tempfile::tempdir().expect("session dir");
        let recorder = pi_coding::start_session_in(
            session_dir.path(),
            None,
            None,
            Some(session_dir.path()),
            None,
            None,
        )
        .expect("start recorded session");
        for (index, text) in ["first", "second", "third", "fourth", "fifth"]
            .iter()
            .enumerate()
        {
            recorder
                .record_message(&Message::user_text(*text, index as i64))
                .expect("record message");
        }
        recorder.persist_now().expect("persist");
        app.session()
            .set_session_dir(session_dir.path().to_path_buf());
        app.session().record(recorder).expect("attach recorder");

        let settings = settings_state();
        let workflows = workflows_state();
        let handle = |command: RpcCommand| handle_command(&app, &settings, &workflows, command);

        // The web flow: list entries via get_entries, then rewind to a target.
        let entries = handle(RpcCommand::GetEntries {
            id: Some("entries-1".into()),
            since: None,
        })
        .await;
        assert!(entries.success, "{entries:?}");
        let count = entries.data.expect("entries")["entries"]
            .as_array()
            .expect("entries array")
            .len();
        assert!(count >= 3, "fixture must have history to rewind: {count}");

        let rewinded = handle(RpcCommand::Rewind {
            id: Some("rewind-1".into()),
            index: Some(count - 1),
            checkpoint: None,
        })
        .await;
        assert!(rewinded.success, "{rewinded:?}");
        let data = rewinded.data.expect("rewind outcome");
        assert_eq!(data["retainedEntries"], count - 1);
        assert_eq!(data["droppedEntries"], 1);
        assert!(
            data["archivePath"]
                .as_str()
                .is_some_and(|path| path.contains(".rewind-")),
            "{data}"
        );

        // A checkpoint rewind reports the resolved name.
        app.set_checkpoint("milestone").expect("checkpoint");
        let checkpointed = handle(RpcCommand::Rewind {
            id: Some("rewind-ckpt".into()),
            index: None,
            checkpoint: Some("milestone".into()),
        })
        .await;
        assert!(checkpointed.success, "{checkpointed:?}");
        assert_eq!(
            checkpointed.data.expect("rewind")["checkpoint"],
            "milestone"
        );

        // Out-of-range index must fail cleanly.
        let out_of_range = handle(RpcCommand::Rewind {
            id: Some("rewind-oob".into()),
            index: Some(count + 1000),
            checkpoint: None,
        })
        .await;
        assert!(!out_of_range.success, "{out_of_range:?}");

        app.cleanup().await;
    }

    #[tokio::test]
    async fn handoff_rpc_renders_envelope_without_provider_call() {
        let app = build_todo_app("faux-rpc-handoff", "faux-rpc-handoff-api").await;
        let response = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::Handoff {
                id: Some("handoff-1".into()),
                prose: false,
            },
        )
        .await;
        assert!(response.success, "{response:?}");
        assert_eq!(response.command, "handoff");
        let data = response.data.expect("handoff");
        assert_eq!(data["prose"], json!(false));
        assert!(
            data["text"]
                .as_str()
                .is_some_and(|text| text.starts_with("# Handoff")),
            "{data}"
        );
        app.cleanup().await;
    }

    #[tokio::test]
    async fn queue_rpc_lists_and_cancels_pending_prompts() {
        let app = build_todo_app("faux-rpc-queue", "faux-rpc-queue-api").await;
        app.set_steering_mode(QueueMode::All).await;
        app.set_follow_up_mode(QueueMode::All).await;
        app.steer("first steer".to_owned(), Vec::new())
            .await
            .expect("queue steer");
        app.follow_up("second follow".to_owned(), Vec::new())
            .await
            .expect("queue follow-up");

        let listed = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::QueueList {
                id: Some("queue-1".into()),
            },
        )
        .await;
        assert!(listed.success, "{listed:?}");
        let data = listed.data.expect("queue");
        assert_eq!(data["total"], 2);
        assert_eq!(data["steering"][0], "first steer");
        assert_eq!(data["followUp"][0], "second follow");

        let cancelled = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::QueueCancel {
                id: Some("queue-2".into()),
                scope: QueueCancelScope::All,
                index: None,
            },
        )
        .await;
        assert!(cancelled.success, "{cancelled:?}");
        assert_eq!(cancelled.data.expect("cancelled")["cancelled"], 2);
        let (steering, follow_up) = app.queued_messages().await;
        assert!(
            steering.is_empty() && follow_up.is_empty(),
            "queue must be fully drained after queue_cancel"
        );

        let empty = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::QueueList {
                id: Some("queue-3".into()),
            },
        )
        .await;
        assert!(empty.success, "{empty:?}");
        assert_eq!(empty.data.expect("queue")["total"], 0);
        app.cleanup().await;
    }

    #[tokio::test]
    async fn queue_rpc_send_now_combines_steering_and_cancel_one_leaves_follow_up() {
        let app = build_todo_app("faux-rpc-queue-send", "faux-rpc-queue-send-api").await;
        app.set_steering_mode(QueueMode::All).await;
        app.set_follow_up_mode(QueueMode::All).await;
        app.steer("first".to_owned(), Vec::new())
            .await
            .expect("steer first");
        app.steer("second".to_owned(), Vec::new())
            .await
            .expect("steer second");
        app.follow_up("later".to_owned(), Vec::new())
            .await
            .expect("follow-up");

        let removed = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::QueueCancel {
                id: Some("drop-0".into()),
                scope: QueueCancelScope::Steering,
                index: Some(0),
            },
        )
        .await;
        assert!(removed.success, "{removed:?}");
        assert_eq!(removed.data.expect("removed")["cancelled"], 1);
        let (steering, follow_up) = app.queued_messages().await;
        assert_eq!(pi_agent::queued_user_previews(&steering), ["second"]);
        assert_eq!(pi_agent::queued_user_previews(&follow_up), ["later"]);

        let cancelled = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::QueueCancel {
                id: Some("steer-only".into()),
                scope: QueueCancelScope::Steering,
                index: None,
            },
        )
        .await;
        assert!(cancelled.success, "{cancelled:?}");
        assert_eq!(cancelled.data.expect("cancelled")["cancelled"], 1);
        let (steering, follow_up) = app.queued_messages().await;
        assert!(steering.is_empty(), "steering-only cancel");
        assert_eq!(pi_agent::queued_user_previews(&follow_up), ["later"]);
        app.drain_queued_messages().await;

        app.steer("alpha".to_owned(), Vec::new())
            .await
            .expect("steer alpha");
        app.steer("beta".to_owned(), Vec::new())
            .await
            .expect("steer beta");
        let sent = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::QueueSendNow {
                id: Some("send-now".into()),
            },
        )
        .await;
        assert!(sent.success, "{sent:?}");
        let data = sent.data.expect("send now");
        assert_eq!(data["sent"], 2);
        assert_eq!(data["combined"], true);
        let (steering, follow_up) = app.queued_messages().await;
        assert!(steering.is_empty(), "send now drains steering");
        assert!(follow_up.is_empty(), "no follow-up on this send");
        app.cleanup().await;
    }

    #[tokio::test]
    async fn queue_cancel_missing_index_is_idempotent_success() {
        let app = build_todo_app(
            "faux-rpc-queue-missing",
            "faux-rpc-queue-missing-api",
        )
        .await;
        let missing = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::QueueCancel {
                id: Some("missing-0".into()),
                scope: QueueCancelScope::Steering,
                index: Some(0),
            },
        )
        .await;
        assert!(missing.success, "{missing:?}");
        let missing_data = missing.data.expect("missing");
        assert_eq!(missing_data["cancelled"], 0);
        assert_eq!(missing_data["index"], 0);
        app.steer("only".to_owned(), Vec::new())
            .await
            .expect("steer");
        let first = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::QueueCancel {
                id: Some("drop-only".into()),
                scope: QueueCancelScope::Steering,
                index: Some(0),
            },
        )
        .await;
        assert!(first.success, "{first:?}");
        assert_eq!(first.data.expect("first")["cancelled"], 1);
        let again = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::QueueCancel {
                id: Some("drop-again".into()),
                scope: QueueCancelScope::Steering,
                index: Some(0),
            },
        )
        .await;
        assert!(again.success, "{again:?}");
        assert_eq!(again.data.expect("again")["cancelled"], 0);
        let (steering, follow_up) = app.queued_messages().await;
        assert!(steering.is_empty() && follow_up.is_empty());
        app.cleanup().await;
    }


    // ------------------------------------------------------------------
    // D93: Subagents panel RPC — job_list / task_spawn / job_cancel /
    // hub_send / job_output.
    // ------------------------------------------------------------------

    /// Session wired to an instantly-settling faux stream (one empty turn).
    fn subagents_session(cwd: &Path) -> pi_coding::Session {
        use pi_ai::{AssistantMessage, AssistantMessageEvent, StopReason};
        let stream_fn: pi_agent::StreamFn = Arc::new(|model, _context, _options| {
            async move {
                let events = pi_ai::new_assistant_message_event_stream();
                let writer = events.clone();
                tokio::spawn(async move {
                    let mut message = AssistantMessage::pending(&model);
                    message.stop_reason = StopReason::Stop;
                    writer
                        .push(AssistantMessageEvent::Done {
                            reason: StopReason::Stop,
                            message: message.clone(),
                        })
                        .await;
                    writer.end(Some(message)).await;
                });
                events
            }
            .boxed()
        });
        disabled_session(pi_coding::SessionOptions {
            model: faux_model(),
            cwd: cwd.to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "subagents".to_owned(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(stream_fn),
            auth_resolver: None,
            auth_manager: None,
        })
        .expect("subagents session")
    }

    /// Application with orchestration attached and two trusted agents.
    async fn subagents_app() -> (tempfile::TempDir, Application) {
        subagents_app_with_child_delay(std::time::Duration::ZERO).await
    }

    /// [`subagents_app`] with an artificial delay before the child's first
    /// turn finishes, so a spawned job stays `running` long enough to observe
    /// its live transcript.
    async fn subagents_app_with_child_delay(
        child_delay: std::time::Duration,
    ) -> (tempfile::TempDir, Application) {
        use pi_coding::{
            AgentCatalog, AgentDefinition, AgentDefinitionSource, OrchestrationConfig,
        };
        let root = tempfile::tempdir().expect("tempdir");
        let child_cwd = root.path().to_path_buf();
        let agent = |name: &str, description: &str| AgentDefinition {
            name: name.to_owned(),
            description: description.to_owned(),
            system_prompt: "prompt".to_owned(),
            tools: Some(Vec::new()),
            autoload_skills: Vec::new(),
            model: None,
            thinking_level: Some(ThinkingLevel::Off),
            max_turns: None,
            max_tool_calls: None,
            timeout_secs: None,
            disallowed_tools: Vec::new(),
            capability_ceiling: None,
            source: AgentDefinitionSource::Bundled,
            path: None,
            trusted: true,
            kind: pi_coding::AgentDefinitionKind::Agent,
            personality: None,
            soft_budget: None,
            knowledge_roots: Vec::new(),
            user_proxy: false,
        };
        let factory: pi_coding::ChildSessionFactory = Arc::new(move |request| {
            let child_cwd = child_cwd.clone();
            let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context, _options| {
                use pi_ai::{AssistantMessage, AssistantMessageEvent, StopReason};
                async move {
                    let events = pi_ai::new_assistant_message_event_stream();
                    let writer = events.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(child_delay).await;
                        let mut message = AssistantMessage::pending(&model);
                        message.stop_reason = StopReason::Stop;
                        writer
                            .push(AssistantMessageEvent::Done {
                                reason: StopReason::Stop,
                                message: message.clone(),
                            })
                            .await;
                        writer.end(Some(message)).await;
                    });
                    events
                }
                .boxed()
            });
            Box::pin(async move {
                disabled_session(pi_coding::SessionOptions {
                    model: request.model,
                    cwd: child_cwd,
                    system_prompt: request.system_prompt,
                    thinking_level: request.thinking_level.unwrap_or(ThinkingLevel::Off),
                    api_key: "subagents-child".to_owned(),
                    compaction: None,
                    stream_options: Default::default(),
                    tools: Some(request.orchestration_tools),
                    before_tool_call: None,
                    after_tool_call: None,
                    stream_fn: Some(stream_fn),
                    auth_resolver: None,
                    auth_manager: None,
                })
            })
        });
        let mut config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![
                agent("writer", "Write assigned content"),
                agent("researcher", "Research and study assigned topics"),
            ]),
            root.path(),
        );
        config.default_agent = "writer".to_owned();
        config.parent_model = Model::default();
        let runtime =
            pi_coding::OrchestrationRuntime::new(config, factory).expect("orchestration runtime");
        let workspace = root.path().join("workspace");
        let agent_dir = root.path().join("agent");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::create_dir_all(&agent_dir).expect("agent dir");
        let session = subagents_session(&workspace);
        session
            .attach_resources(isolated_resources(&workspace, &agent_dir))
            .await
            .expect("attach resources");
        let session_dir = root.path().join("sessions");
        std::fs::create_dir_all(&session_dir).expect("session dir");
        session.set_session_dir(session_dir);
        session.start_new_recording().expect("start recording");
        let application = Application::new_with_orchestration(session, runtime).await;
        (root, application)
    }

    async fn subagents_handle(app: &Application, command: RpcCommand) -> RpcResponse {
        handle_command(app, &settings_state(), &workflows_state(), command).await
    }

    #[tokio::test]
    async fn job_list_reports_disabled_without_orchestration() {
        let app = build_todo_app("faux-rpc-jobs", "faux-rpc-jobs-api").await;
        let response = subagents_handle(&app, RpcCommand::JobList { id: None }).await;
        assert!(response.success, "{response:?}");
        let data = response.data.expect("job list");
        assert_eq!(data["enabled"], false);
        assert_eq!(data["jobs"], json!([]));
        app.cleanup().await;
    }

    #[tokio::test]
    async fn task_spawn_job_list_cancel_output_and_hub_round_trip() {
        let (_root, app) = subagents_app().await;

        // job_list before any spawn: enabled with an empty job list and the
        // agent catalog exposed for the spawn form.
        let listed = subagents_handle(&app, RpcCommand::JobList { id: None }).await;
        assert!(listed.success, "{listed:?}");
        let list_data = listed.data.expect("job list");
        assert_eq!(list_data["enabled"], true);
        assert_eq!(
            list_data["groupId"],
            json!(app.orchestration_runtime().expect("runtime").group_id())
        );
        assert_eq!(list_data["jobs"], json!([]));
        let catalog = list_data["catalog"]
            .as_array()
            .expect("catalog")
            .iter()
            .filter_map(|agent| agent.get("name").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert!(
            catalog.iter().any(|name| *name == "writer"),
            "catalog must include the writer agent: {catalog:?}"
        );

        // task_spawn single-spawn wire shape mirrors the `task` tool.
        let spawned = subagents_handle(
            &app,
            RpcCommand::TaskSpawn {
                id: None,
                args: json!({"task": "write the release notes", "agent": "writer"}),
            },
        )
        .await;
        assert!(spawned.success, "{spawned:?}");
        let spawns = spawned.data.expect("spawns")["spawns"]
            .as_array()
            .expect("spawns array")
            .clone();
        assert_eq!(spawns.len(), 1);
        let job_id = spawns[0]["jobId"].as_str().expect("job id").to_owned();
        let agent_id = spawns[0]["agentId"].as_str().expect("agent id").to_owned();
        assert!(!job_id.is_empty() && !agent_id.is_empty());

        // job_list now exposes the queued job with its description.
        let listed_after = subagents_handle(&app, RpcCommand::JobList { id: None }).await;
        let jobs = listed_after.data.expect("job list")["jobs"]
            .as_array()
            .expect("jobs array")
            .clone();
        assert_eq!(jobs.len(), 1, "{jobs:?}");
        assert_eq!(jobs[0]["id"], json!(job_id));
        assert_eq!(jobs[0]["agentId"], json!(agent_id));
        assert_eq!(jobs[0]["agent"], json!("writer"));
        assert_eq!(jobs[0]["status"], json!("queued"));
        assert_eq!(jobs[0]["description"], json!("write the release notes"));

        // job_output returns the settled job snapshot (the child run settles
        // instantly on the faux stream; status may already be completed).
        let output = subagents_handle(
            &app,
            RpcCommand::JobOutput {
                id: None,
                job_id: job_id.clone(),
            },
        )
        .await;
        assert!(output.success, "{output:?}");
        assert_eq!(output.data.expect("job")["job"]["id"], json!(job_id));

        // Unknown job ids fail structurally with a clear error.
        let missing = subagents_handle(
            &app,
            RpcCommand::JobOutput {
                id: None,
                job_id: "no-such-job".to_owned(),
            },
        )
        .await;
        assert!(!missing.success);
        assert!(
            missing
                .error
                .as_deref()
                .is_some_and(|error| error.contains("no orchestration job")),
            "{missing:?}"
        );

        // hub_send delivers to the spawned agent (receipt reports the outcome).
        let sent = subagents_handle(
            &app,
            RpcCommand::HubSend {
                id: None,
                to: agent_id.clone(),
                body: "status report?".to_owned(),
                reply_to: None,
            },
        )
        .await;
        assert!(sent.success, "{sent:?}");
        let receipts = sent.data.expect("receipts")["receipts"]
            .as_array()
            .expect("receipts array")
            .clone();
        assert_eq!(receipts.len(), 1, "{receipts:?}");
        assert_eq!(receipts[0]["to"], json!(agent_id));
        assert!(
            receipts[0]["outcome"]
                .as_str()
                .is_some_and(|outcome| matches!(outcome, "queued" | "woken" | "revived")),
            "delivery must succeed: {receipts:?}"
        );

        // job_cancel cancels by job id. The job settles asynchronously in the
        // run loop (it may already have completed on the faux stream), so
        // poll the wire until the snapshot reaches a settled status.
        let cancelled = subagents_handle(
            &app,
            RpcCommand::JobCancel {
                id: None,
                job_ids: vec![job_id.clone()],
            },
        )
        .await;
        assert!(cancelled.success, "{cancelled:?}");
        let settled = loop {
            let snapshot = subagents_handle(&app, RpcCommand::JobList { id: None }).await;
            let jobs = snapshot.data.expect("job list")["jobs"]
                .as_array()
                .expect("jobs array")
                .clone();
            assert_eq!(jobs.len(), 1, "{jobs:?}");
            let status = jobs[0]["status"].as_str().unwrap_or("");
            if matches!(status, "completed" | "failed" | "cancelled") {
                break jobs[0].clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };
        assert_eq!(settled["id"], json!(job_id));
        assert!(
            matches!(settled["status"].as_str(), Some("cancelled" | "completed")),
            "job must settle to cancelled or completed: {settled:?}"
        );
        app.cleanup().await;
    }

    #[tokio::test]
    async fn agent_history_fetches_running_child_transcript() {
        let (root, app) =
            subagents_app_with_child_delay(std::time::Duration::from_millis(400)).await;

        let spawned = subagents_handle(
            &app,
            RpcCommand::TaskSpawn {
                id: None,
                args: json!({"task": "write the release notes", "agent": "writer"}),
            },
        )
        .await;
        assert!(spawned.success, "{spawned:?}");
        let spawns = spawned.data.expect("spawns")["spawns"]
            .as_array()
            .expect("spawns array")
            .clone();
        let agent_id = spawns[0]["agentId"].as_str().expect("agent id").to_owned();

        // The transcript must be readable from the live session while the job
        // is still running — never gated on the settle-time snapshot.
        let mut saw_running = false;
        let mut running_fetch: Option<RpcResponse> = None;
        loop {
            let listed = subagents_handle(&app, RpcCommand::JobList { id: None }).await;
            let jobs = listed.data.expect("job list")["jobs"]
                .as_array()
                .expect("jobs array")
                .clone();
            assert_eq!(jobs.len(), 1, "{jobs:?}");
            let status = jobs[0]["status"].as_str().unwrap_or("").to_owned();
            if status == "running" {
                saw_running = true;
            }
            if saw_running {
                let fetched = subagents_handle(
                    &app,
                    RpcCommand::AgentHistory {
                        id: None,
                        agent_id: agent_id.clone(),
                        lines: None,
                    },
                )
                .await;
                if fetched.success
                    && fetched.data.as_ref().is_some_and(|data| {
                        data["text"].as_str().is_some_and(|text| !text.is_empty())
                    })
                {
                    running_fetch = Some(fetched);
                    break;
                }
            }
            assert!(
                !matches!(status.as_str(), "completed" | "failed" | "cancelled"),
                "job settled before the running transcript became fetchable: {jobs:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let fetched = running_fetch.expect("running fetch");
        let data = fetched.data.expect("history data");
        assert_eq!(data["agentId"], json!(agent_id));
        let text = data["text"].as_str().expect("history text");
        assert!(!text.is_empty() && text.lines().count() <= 80, "{text:?}");
        assert!(text.contains("write the release notes"), "{text:?}");
        let serialized = serde_json::to_string(&data).expect("serialize history");
        assert!(
            !serialized.contains(root.path().to_string_lossy().as_ref()),
            "no filesystem paths may leak: {serialized}"
        );

        // The same fetch keeps working after the job settles.
        loop {
            let listed = subagents_handle(&app, RpcCommand::JobList { id: None }).await;
            let jobs = listed.data.expect("job list")["jobs"]
                .as_array()
                .expect("jobs array")
                .clone();
            let status = jobs[0]["status"].as_str().unwrap_or("");
            if matches!(status, "completed" | "failed" | "cancelled") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let settled = subagents_handle(
            &app,
            RpcCommand::AgentHistory {
                id: None,
                agent_id,
                lines: Some(20),
            },
        )
        .await;
        assert!(settled.success, "{settled:?}");
        let settled_data = settled.data.expect("settled history");
        assert_eq!(settled_data["agentId"], data["agentId"]);
        assert!(
            settled_data["text"]
                .as_str()
                .is_some_and(|text| text.lines().count() <= 20),
            "{settled_data:?}"
        );
        app.cleanup().await;
    }

    #[tokio::test]
    async fn agent_history_rejects_unknown_agent_and_invalid_lines() {
        let (_root, app) = subagents_app().await;

        // Unknown agent ids fail actionably, naming the offending id.
        let missing = subagents_handle(
            &app,
            RpcCommand::AgentHistory {
                id: None,
                agent_id: "no-such-agent".to_owned(),
                lines: None,
            },
        )
        .await;
        assert!(!missing.success, "{missing:?}");
        assert!(
            missing
                .error
                .as_deref()
                .is_some_and(|error| error.contains("no-such-agent")),
            "{missing:?}"
        );

        // Line counts outside the core bound fail before any history read.
        for bad_lines in [0, pi_coding::MAX_HISTORY_LINES + 1] {
            let response = subagents_handle(
                &app,
                RpcCommand::AgentHistory {
                    id: None,
                    agent_id: "writer".to_owned(),
                    lines: Some(bad_lines),
                },
            )
            .await;
            assert!(!response.success, "{response:?}");
            assert!(
                response.error.as_deref().is_some_and(|error| {
                    error.contains("lines")
                        && error.contains(&pi_coding::MAX_HISTORY_LINES.to_string())
                }),
                "{response:?}"
            );
        }
        app.cleanup().await;
    }

    #[tokio::test]
    async fn task_spawn_batch_requires_context_and_rejects_unknown_agents() {
        let (_root, app) = subagents_app().await;

        // Batch without context is a caller error (mirrors the task tool).
        let no_context = subagents_handle(
            &app,
            RpcCommand::TaskSpawn {
                id: None,
                args: json!({"tasks": [{"task": "one"}]}),
            },
        )
        .await;
        assert!(!no_context.success, "{no_context:?}");
        assert!(
            no_context
                .error
                .as_deref()
                .is_some_and(|error| error.contains("context")),
            "{no_context:?}"
        );

        // Batch with context fans out one job per item.
        let batch = subagents_handle(
            &app,
            RpcCommand::TaskSpawn {
                id: None,
                args: json!({
                    "context": "ship the release",
                    "tasks": [
                        {"name": "A", "agent": "writer", "task": "draft notes"},
                        {"name": "B", "agent": "researcher", "task": "verify claims"},
                    ],
                }),
            },
        )
        .await;
        assert!(batch.success, "{batch:?}");
        let spawns = batch.data.expect("spawns")["spawns"]
            .as_array()
            .expect("spawns array")
            .clone();
        assert_eq!(spawns.len(), 2, "{spawns:?}");
        let listed = subagents_handle(&app, RpcCommand::JobList { id: None }).await;
        let jobs = listed.data.expect("job list")["jobs"]
            .as_array()
            .expect("jobs array")
            .clone();
        assert_eq!(jobs.len(), 2, "{jobs:?}");

        // Unknown agent names fail the spawn with an actionable error.
        let unknown = subagents_handle(
            &app,
            RpcCommand::TaskSpawn {
                id: None,
                args: json!({"task": "inspect", "agent": "ghost"}),
            },
        )
        .await;
        assert!(!unknown.success, "{unknown:?}");
        assert!(
            unknown
                .error
                .as_deref()
                .is_some_and(|error| error.contains("ghost")),
            "{unknown:?}"
        );
        app.cleanup().await;
    }

    // ------------------------------------------------------------------
    // Persistent persona RPC — persona_list / persona_get / persona_create /
    // persona_edit / persona_remove / persona_purge / persona_select /
    // persona_clear / persona_current, plus task_spawn targeting a persona
    // agent name. Apps mirror the real listen runtime: a ResourceManager
    // discovers durable personas from a temp agent dir, the session is
    // recording (persona spawns require a durable parent), and the
    // orchestration runtime catalog is built from the resource snapshot.
    // ------------------------------------------------------------------

    /// Seeds `personas/<name>/persona.md` (plus memory/sessions when
    /// `seed_state`) under the agent dir, returning the persona root.
    fn seed_persona(
        agent_dir: &std::path::Path,
        name: &str,
        seed_state: bool,
    ) -> std::path::PathBuf {
        let root = agent_dir.join("personas").join(name);
        std::fs::create_dir_all(&root).expect("persona root");
        std::fs::write(
            root.join("persona.md"),
            format!("---\nname: {name}\ndescription: durable {name}\ntools: [read, grep]\n---\n{name} prompt"),
        )
        .expect("persona.md");
        if seed_state {
            let memory = root.join("memory");
            std::fs::create_dir_all(&memory).expect("memory dir");
            std::fs::write(
                memory.join("entries.jsonl"),
                "{\"id\":\"a\",\"content\":\"persona-memory-note\",\"tags\":[],\"ts\":1,\"session\":\"s\"}\n",
            )
            .expect("entries");
            let sessions = root.join("sessions");
            std::fs::create_dir_all(&sessions).expect("sessions dir");
            std::fs::write(sessions.join("Mentor.jsonl"), "{}\n").expect("archive");
        }
        root
    }

    /// Application with a resource-discovered persona, a recording session,
    /// and an orchestration runtime cataloged from the resource snapshot.
    /// Returns (temp root, app, provider registration, persona root).
    async fn persona_rpc_app(
        seed_state: bool,
    ) -> (
        tempfile::TempDir,
        Application,
        pi_ai::providers::FauxProviderRegistration,
        std::path::PathBuf,
    ) {
        use pi_ai::StopReason;
        use pi_ai::providers::{FauxProviderOptions, FauxResponse, register_faux_provider};
        use pi_coding::{
            AgentCatalog, OrchestrationConfig, OrchestrationRuntime, ResourceManager,
            ResourceManagerOptions, Session, SessionOptions,
        };
        let root = tempfile::tempdir().expect("root");
        let agent_dir = root.path().join("agent");
        std::fs::create_dir_all(&agent_dir).expect("agent dir");
        let persona_root = seed_persona(&agent_dir, "mentor", seed_state);
        let cwd = root.path().join("project");
        std::fs::create_dir_all(&cwd).expect("cwd");
        std::fs::create_dir_all(cwd.join(".rpi").join("artifacts")).expect("artifacts");

        let suffix = uuid::Uuid::now_v7().simple().to_string();
        let api = format!("persona-rpc-api-{suffix}");
        let provider = format!("persona-rpc-provider-{suffix}");
        let model = Model {
            id: format!("persona-rpc-model-{suffix}"),
            name: "Persona RPC Model".to_owned(),
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
        registration.set_responses(vec![FauxResponse {
            content: vec![pi_ai::ContentBlock::text("done")],
            stop_reason: StopReason::Stop,
            error_message: None,
        }]);

        std::fs::write(
            root.path().join("agent").join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )
        .expect("isolated sandbox settings");
        let mut options = ResourceManagerOptions::new(&cwd);
        options.agent_dir = agent_dir;
        options.disable_extensions = true;
        options.disable_skills = true;
        options.disable_prompt_templates = true;
        options.disable_themes = true;
        options.disable_context_files = true;
        let resources = ResourceManager::new(options).expect("resources");
        let session = disabled_session(SessionOptions {
            model: model.clone(),
            cwd: cwd.clone(),
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
        .expect("session");
        let session_root = root.path().join("session-root");
        session.set_session_dir(session_root);
        session.start_new_recording().expect("start recording");
        session
            .attach_resources(resources.clone())
            .await
            .expect("attach resources");

        // Orchestration catalog mirrors the resource snapshot and is staged
        // in the same published generation as the recorded parent session.
        let snapshot = resources.snapshot();
        let child_cwd = cwd.clone();
        let factory: pi_coding::ChildSessionFactory = Arc::new(move |request| {
            let child_cwd = child_cwd.clone();
            let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context, _options| {
                use pi_ai::{AssistantMessage, AssistantMessageEvent, StopReason};
                async move {
                    let events = pi_ai::new_assistant_message_event_stream();
                    let writer = events.clone();
                    tokio::spawn(async move {
                        let mut message = AssistantMessage::pending(&model);
                        message.stop_reason = StopReason::Stop;
                        writer
                            .push(AssistantMessageEvent::Done {
                                reason: StopReason::Stop,
                                message: message.clone(),
                            })
                            .await;
                        writer.end(Some(message)).await;
                    });
                    events
                }
                .boxed()
            });
            Box::pin(async move {
                disabled_session(pi_coding::SessionOptions {
                    model: request.model,
                    cwd: child_cwd,
                    system_prompt: request.system_prompt,
                    thinking_level: request.thinking_level.unwrap_or(ThinkingLevel::Off),
                    api_key: "persona-rpc-child".to_owned(),
                    compaction: None,
                    stream_options: Default::default(),
                    tools: Some(request.orchestration_tools),
                    before_tool_call: None,
                    after_tool_call: None,
                    stream_fn: Some(stream_fn),
                    auth_resolver: None,
                    auth_manager: None,
                })
            })
        });
        let mut config = OrchestrationConfig::new(
            AgentCatalog::from_agents(snapshot.agents.clone()),
            cwd.join(".rpi").join("artifacts"),
        );
        config.parent_model = model;
        let runtime = OrchestrationRuntime::new(config, factory).expect("orchestration runtime");
        let application = Application::from_runtime_candidate(
            pi_coding::ApplicationRuntimeCandidate::new(session).with_orchestration(runtime),
        )
        .await
        .expect("publish persona application");
        (root, application, registration, persona_root)
    }

    #[tokio::test]
    async fn persona_rpc_list_discloses_seeded_persona_with_state_and_no_paths() {
        let (_root, app, _registration, persona_root) = persona_rpc_app(true).await;

        let response = subagents_handle(&app, RpcCommand::PersonaList { id: None }).await;
        assert!(response.success, "{response:?}");
        let data = response.data.expect("persona list");
        assert_eq!(data["enabled"], true);
        let rows = data["personas"].as_array().expect("personas array");
        assert_eq!(rows.len(), 1, "{rows:?}");
        let row = &rows[0];
        assert_eq!(row["name"], json!("mentor"));
        assert_eq!(row["description"], json!("durable mentor"));
        assert_eq!(row["source"], json!("user"));
        assert_eq!(row["trusted"], json!(true));
        assert_eq!(row["preferred"], json!(false));
        assert!(
            row["contractSummary"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
        );
        assert_eq!(row["memoryEntries"], json!(1));
        assert_eq!(row["sessionCount"], json!(1));
        assert_eq!(row["stateError"], json!(null));
        // No absolute paths may cross the wire (the fixture home lives under
        // the temp root; persona roots must stay server-side).
        let serialized = serde_json::to_string(&data).expect("serialize");
        assert!(
            !serialized.contains(persona_root.to_string_lossy().as_ref()),
            "persona_list leaked the persona root path: {serialized}"
        );
        assert!(
            !serialized.contains(
                std::env::current_dir()
                    .expect("cwd")
                    .to_string_lossy()
                    .as_ref()
            ),
            "persona_list leaked an absolute path: {serialized}"
        );
        app.cleanup().await;
    }

    #[tokio::test]
    async fn persona_rpc_get_returns_bounded_content_and_rejects_unknown() {
        let (_root, app, _registration, persona_root) = persona_rpc_app(false).await;

        let got = subagents_handle(
            &app,
            RpcCommand::PersonaGet {
                id: None,
                name: "mentor".to_owned(),
            },
        )
        .await;
        assert!(got.success, "{got:?}");
        let data = got.data.expect("persona get");
        assert_eq!(data["name"], json!("mentor"));
        assert_eq!(data["contentTruncated"], json!(false));
        let content = data["content"].as_str().expect("content");
        assert!(content.contains("durable mentor"), "{content}");
        assert!(
            !content.contains(persona_root.to_string_lossy().as_ref()),
            "persona_get leaked the persona root path"
        );

        let missing = subagents_handle(
            &app,
            RpcCommand::PersonaGet {
                id: None,
                name: "ghost".to_owned(),
            },
        )
        .await;
        assert!(!missing.success, "{missing:?}");
        assert!(
            missing
                .error
                .as_deref()
                .is_some_and(|error| error.contains("ghost")),
            "{missing:?}"
        );
        app.cleanup().await;
    }

    #[tokio::test]
    async fn persona_rpc_create_makes_catalog_discoverable_and_edit_rejects_name_mismatch() {
        let (_root, app, _registration, _persona_root) = persona_rpc_app(false).await;

        // Config save -> catalog discoverable: after persona_create the very
        // next persona_list includes the new persona (commit live-reloads).
        let created = subagents_handle(
            &app,
            RpcCommand::PersonaCreate {
                id: None,
                name: "guide".to_owned(),
                content: "---\nname: guide\ndescription: guided assistant\n---\nguide prompt\n"
                    .to_owned(),
            },
        )
        .await;
        assert!(created.success, "{created:?}");
        assert_eq!(created.data.expect("created")["created"], json!(true));
        let listed = subagents_handle(&app, RpcCommand::PersonaList { id: None }).await;
        let list_data = listed.data.expect("list");
        let rows = list_data["personas"].as_array().expect("rows");
        let names = rows
            .iter()
            .filter_map(|row| row.get("name").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert!(
            names.contains(&"guide") && names.contains(&"mentor"),
            "catalog must discover the created persona: {names:?}"
        );

        // A duplicate create is rejected (name already in use).
        let duplicate = subagents_handle(
            &app,
            RpcCommand::PersonaCreate {
                id: None,
                name: "guide".to_owned(),
                content: "---\nname: guide\ndescription: again\n---\nprompt".to_owned(),
            },
        )
        .await;
        assert!(!duplicate.success, "{duplicate:?}");
        assert!(
            duplicate
                .error
                .as_deref()
                .is_some_and(|error| error.contains("already in use")),
            "{duplicate:?}"
        );

        // Edit with a mismatched frontmatter name is rejected: the committed
        // content must declare the target name (no silent renames).
        let mismatch = subagents_handle(
            &app,
            RpcCommand::PersonaEdit {
                id: None,
                name: "guide".to_owned(),
                content: "---\nname: other\ndescription: renamed\n---\nprompt".to_owned(),
            },
        )
        .await;
        assert!(!mismatch.success, "{mismatch:?}");
        assert!(
            mismatch
                .error
                .as_deref()
                .is_some_and(|error| error.contains("must match the target name")),
            "{mismatch:?}"
        );

        // A matching-name edit commits and is visible afterwards.
        let edited = subagents_handle(
            &app,
            RpcCommand::PersonaEdit {
                id: None,
                name: "guide".to_owned(),
                content: "---\nname: guide\ndescription: refined guide\n---\nrefined prompt"
                    .to_owned(),
            },
        )
        .await;
        assert!(edited.success, "{edited:?}");
        let got = subagents_handle(
            &app,
            RpcCommand::PersonaGet {
                id: None,
                name: "guide".to_owned(),
            },
        )
        .await;
        assert!(got.success, "{got:?}");
        assert_eq!(
            got.data.expect("get")["description"],
            json!("refined guide")
        );
        app.cleanup().await;
    }

    #[tokio::test]
    async fn persona_rpc_remove_keeps_state_and_purge_deletes_root_with_confirmation_gates() {
        let (_root, app, _registration, persona_root) = persona_rpc_app(true).await;
        let memory = persona_root.join("memory").join("entries.jsonl");
        let sessions = persona_root.join("sessions").join("Mentor.jsonl");
        let persona_md = persona_root.join("persona.md");

        // Destructive ops without `confirm: true` are rejected before any
        // filesystem mutation (fail-closed mirror of the CLI --yes gate).
        for command in [
            RpcCommand::PersonaRemove {
                id: None,
                name: "mentor".to_owned(),
                confirm: false,
            },
            RpcCommand::PersonaPurge {
                id: None,
                name: "mentor".to_owned(),
                confirm: false,
            },
        ] {
            let response = subagents_handle(&app, command).await;
            assert!(!response.success, "{response:?}");
            assert!(
                response
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("confirm: true")),
                "{response:?}"
            );
        }
        assert!(
            persona_md.exists(),
            "unconfirmed op must not touch the disk"
        );

        // Unknown personas fail closed even with confirmation.
        let unknown = subagents_handle(
            &app,
            RpcCommand::PersonaRemove {
                id: None,
                name: "ghost".to_owned(),
                confirm: true,
            },
        )
        .await;
        assert!(!unknown.success, "{unknown:?}");

        // Remove with confirmation deletes persona.md but KEEPS memory and
        // sessions under the persona root, and the catalog drops the persona.
        let removed = subagents_handle(
            &app,
            RpcCommand::PersonaRemove {
                id: None,
                name: "mentor".to_owned(),
                confirm: true,
            },
        )
        .await;
        assert!(removed.success, "{removed:?}");
        assert_eq!(removed.data.expect("removed")["removed"], json!(true));
        assert!(!persona_md.exists(), "remove must delete persona.md");
        assert!(memory.exists(), "remove must keep memory/entries.jsonl");
        assert!(sessions.exists(), "remove must keep sessions archives");
        let listed = subagents_handle(&app, RpcCommand::PersonaList { id: None }).await;
        assert_eq!(
            listed.data.expect("list")["personas"]
                .as_array()
                .expect("rows")
                .len(),
            0,
            "removed persona must leave the catalog"
        );

        // A removed persona cannot be edited (no definition to resolve).
        let edit_after_remove = subagents_handle(
            &app,
            RpcCommand::PersonaEdit {
                id: None,
                name: "mentor".to_owned(),
                content: "---\nname: mentor\ndescription: back\n---\nprompt".to_owned(),
            },
        )
        .await;
        assert!(!edit_after_remove.success, "{edit_after_remove:?}");

        // Re-create with the same name (the kept state becomes visible again)
        // so purge can be exercised on a persona with memory.
        let recreated = subagents_handle(
            &app,
            RpcCommand::PersonaCreate {
                id: None,
                name: "mentor".to_owned(),
                content: "---\nname: mentor\ndescription: recreated\n---\nprompt".to_owned(),
            },
        )
        .await;
        assert!(recreated.success, "{recreated:?}");
        assert!(memory.exists(), "recreate must reuse the kept memory dir");
        assert!(persona_md.exists(), "recreate must write persona.md");

        // Purge with confirmation deletes the WHOLE root (definition + state).
        let purged = subagents_handle(
            &app,
            RpcCommand::PersonaPurge {
                id: None,
                name: "mentor".to_owned(),
                confirm: true,
            },
        )
        .await;
        assert!(purged.success, "{purged:?}");
        assert_eq!(purged.data.expect("purged")["purged"], json!(true));
        assert!(!persona_root.exists(), "purge must delete the persona root");
        let listed_after = subagents_handle(&app, RpcCommand::PersonaList { id: None }).await;
        assert_eq!(
            listed_after.data.expect("list")["personas"]
                .as_array()
                .expect("rows")
                .len(),
            0
        );
        app.cleanup().await;
    }

    #[tokio::test]
    async fn persona_rpc_select_clear_current_and_task_spawn_target_persona() {
        let (_root, app, _registration, _persona_root) = persona_rpc_app(false).await;

        // persona_current reports no selection initially.
        let initial = subagents_handle(&app, RpcCommand::PersonaCurrent { id: None }).await;
        assert!(initial.success, "{initial:?}");
        assert_eq!(initial.data.expect("current")["name"], json!(null));

        // Select persists the preference; the list marks the persona.
        let selected = subagents_handle(
            &app,
            RpcCommand::PersonaSelect {
                id: None,
                name: "mentor".to_owned(),
            },
        )
        .await;
        assert!(selected.success, "{selected:?}");
        assert_eq!(selected.data.expect("select")["preferred"], json!(true));
        let listed = subagents_handle(&app, RpcCommand::PersonaList { id: None }).await;
        let row = &listed.data.expect("list")["personas"][0];
        assert_eq!(row["preferred"], json!(true), "{row:?}");
        let current = subagents_handle(&app, RpcCommand::PersonaCurrent { id: None }).await;
        assert_eq!(current.data.expect("current")["name"], json!("mentor"));

        // Selecting an unknown persona fails closed.
        let unknown = subagents_handle(
            &app,
            RpcCommand::PersonaSelect {
                id: None,
                name: "ghost".to_owned(),
            },
        )
        .await;
        assert!(!unknown.success, "{unknown:?}");

        // Clear drops the preference.
        let cleared = subagents_handle(&app, RpcCommand::PersonaClear { id: None }).await;
        assert!(cleared.success, "{cleared:?}");
        let after_clear = subagents_handle(&app, RpcCommand::PersonaCurrent { id: None }).await;
        assert_eq!(after_clear.data.expect("current")["name"], json!(null));

        // The Web Run button calls task_spawn with `agent` = the persona
        // name: the spawned job must point at the persona agent.
        let spawned = subagents_handle(
            &app,
            RpcCommand::TaskSpawn {
                id: None,
                args: json!({"task": "audit the release notes", "agent": "mentor"}),
            },
        )
        .await;
        assert!(spawned.success, "{spawned:?}");
        let spawns = spawned.data.expect("spawns")["spawns"]
            .as_array()
            .expect("spawns array")
            .clone();
        assert_eq!(spawns.len(), 1, "{spawns:?}");
        assert_eq!(spawns[0]["agent"], json!("mentor"), "{spawns:?}");
        let listed_jobs = subagents_handle(&app, RpcCommand::JobList { id: None }).await;
        let job_list_data = listed_jobs.data.expect("job list");
        let jobs = job_list_data["jobs"]
            .as_array()
            .expect("jobs array")
            .clone();
        assert_eq!(jobs.len(), 1, "{jobs:?}");
        assert_eq!(jobs[0]["agent"], json!("mentor"), "{jobs:?}");
        assert!(
            jobs[0]["agentId"].as_str().is_some_and(|id| !id.is_empty()),
            "spawn must carry a stable agent id: {jobs:?}"
        );
        let serialized = serde_json::to_string(&job_list_data).expect("serialize");
        assert!(
            !serialized.to_lowercase().contains("user-mock-key")
                && !serialized.to_lowercase().contains("api_key"),
            "no credentials may cross the wire: {serialized}"
        );
        app.cleanup().await;
    }

    #[tokio::test]
    async fn persona_rpc_failure_errors_never_embed_absolute_paths() {
        let (root, app, _registration, persona_root) = persona_rpc_app(false).await;

        // Sabotage the definition AFTER discovery: the snapshot still lists
        // the persona, but persona_get's definition read fails — the error
        // surfaced to the Web must carry the operation + persona name only,
        // never the filesystem path.
        std::fs::remove_file(persona_root.join("persona.md")).expect("remove persona.md");
        let got = subagents_handle(
            &app,
            RpcCommand::PersonaGet {
                id: None,
                name: "mentor".to_owned(),
            },
        )
        .await;
        assert!(!got.success, "{got:?}");
        let serialized = serde_json::to_string(&got).expect("serialize");
        assert!(
            !serialized.contains(root.path().to_string_lossy().as_ref()),
            "persona_get failure must not leak the temp root: {serialized}"
        );
        assert!(
            !serialized.contains(
                std::env::current_dir()
                    .expect("cwd")
                    .to_string_lossy()
                    .as_ref()
            ),
            "persona_get failure must not leak an absolute path: {serialized}"
        );
        assert!(
            serialized.contains("mentor"),
            "the error must stay actionable with the persona name: {serialized}"
        );

        // Destructive ops on an unknown persona fail closed without paths.
        let unknown = subagents_handle(
            &app,
            RpcCommand::PersonaPurge {
                id: None,
                name: "ghost".to_owned(),
                confirm: true,
            },
        )
        .await;
        assert!(!unknown.success, "{unknown:?}");
        let serialized_unknown = serde_json::to_string(&unknown).expect("serialize");
        assert!(
            !serialized_unknown.contains(root.path().to_string_lossy().as_ref()),
            "unknown persona error must not leak paths: {serialized_unknown}"
        );

        // Invalid create content fails validation without paths.
        let invalid = subagents_handle(
            &app,
            RpcCommand::PersonaCreate {
                id: None,
                name: "bad".to_owned(),
                content: "no frontmatter here".to_owned(),
            },
        )
        .await;
        assert!(!invalid.success, "{invalid:?}");
        let serialized_invalid = serde_json::to_string(&invalid).expect("serialize");
        assert!(
            !serialized_invalid.contains(root.path().to_string_lossy().as_ref()),
            "invalid content error must not leak paths: {serialized_invalid}"
        );
        app.cleanup().await;
    }

    #[tokio::test]
    async fn persona_rpc_create_reports_reload_failure_instead_of_fake_success() {
        let (root, app, _registration, _persona_root) = persona_rpc_app(false).await;
        // Sabotage the catalog AFTER startup: an oversized persona definition
        // (> 256 KiB) makes the reload inside commit_persona_definition fail
        // deterministically. The persona.md write lands, but the RPC must
        // report a REAL failure (never created:true) with a fixed, path-free
        // message so the Web keeps the editor draft instead of loading a
        // stale catalog.
        let bad_root = root.path().join("agent").join("personas").join("bad");
        std::fs::create_dir_all(&bad_root).expect("bad persona dir");
        let mut oversized = vec![b'x'; 256 * 1024 + 1];
        oversized[..4].copy_from_slice(b"---\n");
        std::fs::write(bad_root.join("persona.md"), oversized).expect("oversized persona.md");

        let response = subagents_handle(
            &app,
            RpcCommand::PersonaCreate {
                id: None,
                name: "scribe".to_owned(),
                content: "---\nname: scribe\ndescription: scribing\n---\nprompt\n".to_owned(),
            },
        )
        .await;
        assert!(
            !response.success,
            "reload failure must not fake success: {response:?}"
        );
        let error = response.error.as_deref().expect("error");
        assert!(
            error.contains("reload or restart required") && error.contains("scribe"),
            "{error}"
        );
        assert!(
            !error.contains(root.path().to_string_lossy().as_ref()),
            "reload-failure wire error must stay path-free: {error}"
        );
        let serialized = serde_json::to_string(&response).expect("serialize");
        assert!(
            !serialized.contains(root.path().to_string_lossy().as_ref()),
            "no temp root may cross the wire: {serialized}"
        );
        app.cleanup().await;
    }

    #[tokio::test]
    async fn persona_rpc_published_empty_catalog_fails_closed_for_unknown_personas() {
        // A published session with no persona definitions reports an empty
        // catalog, and read commands fail instead of fabricating rows.
        let app = build_todo_app("faux-rpc-persona-none", "faux-rpc-persona-none-api").await;
        let listed = subagents_handle(&app, RpcCommand::PersonaList { id: None }).await;
        assert!(listed.success, "{listed:?}");
        let data = listed.data.expect("list");
        assert_eq!(data["enabled"], json!(true));
        assert_eq!(data["personas"], json!([]));
        let got = subagents_handle(
            &app,
            RpcCommand::PersonaGet {
                id: None,
                name: "mentor".to_owned(),
            },
        )
        .await;
        assert!(!got.success, "{got:?}");
        app.cleanup().await;
    }

    #[tokio::test]
    async fn orchestration_mutations_fail_cleanly_without_runtime() {
        let app = build_todo_app("faux-rpc-jobs2", "faux-rpc-jobs2-api").await;
        for (command, needle) in [
            (
                RpcCommand::TaskSpawn {
                    id: None,
                    args: json!({"task": "x"}),
                },
                "orchestration is not enabled",
            ),
            (
                RpcCommand::JobCancel {
                    id: None,
                    job_ids: vec!["j".into()],
                },
                "orchestration is not enabled",
            ),
            (
                RpcCommand::HubSend {
                    id: None,
                    to: "writer".into(),
                    body: "hi".into(),
                    reply_to: None,
                },
                "orchestration is not enabled",
            ),
            (
                RpcCommand::JobOutput {
                    id: None,
                    job_id: "j".into(),
                },
                "orchestration is not enabled",
            ),
            (
                RpcCommand::AgentHistory {
                    id: None,
                    agent_id: "writer".into(),
                    lines: None,
                },
                "orchestration is not enabled",
            ),
        ] {
            let response = subagents_handle(&app, command).await;
            assert!(!response.success, "{response:?}");
            assert!(
                response
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains(needle)),
                "{response:?}"
            );
        }
        app.cleanup().await;
    }
    // ---- Codex Live realtime proxy: local TCP mock regression tests ----
    //
    // A loopback TCP server speaks just enough HTTP/1.1 to stand in for
    // CLIProxyAPI, so the create-call contract is exercised with no real
    // network and no real credentials. The bearer below is a test fixture,
    // never a live secret.
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    const REALTIME_TEST_KEY: &str = "test-bearer-not-a-real-secret";
    const REALTIME_TEST_SDP_OFFER: &str = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n";

    fn realtime_test_settings(base_url: &str) -> pi_coding::LiveRuntimeSettings {
        pi_coding::LiveRuntimeSettings {
            enabled: true,
            mode: "realtime".to_owned(),
            stt_base_url: String::new(),
            stt_api_key: String::new(),
            stt_model: String::new(),
            realtime_base_url: base_url.to_owned(),
            realtime_api_key: REALTIME_TEST_KEY.to_owned(),
            // The create-call session intentionally omits this configured
            // model; the web data channel forwards it in `session.update`.
            realtime_model: "gpt-5.6-sol".to_owned(),
            voice: "sol".to_owned(),
            language: None,
            allow_insecure: true,
        }
    }

    #[test]
    fn realtime_proxy_requires_live_enabled() {
        let mut settings = realtime_test_settings("http://127.0.0.1:1");
        settings.enabled = false;
        let error = validate_realtime_proxy(&settings)
            .expect_err("disabled realtime must fail closed")
            .to_string();
        assert!(error.contains("Settings.live.enabled"), "{error}");
        assert!(!error.contains(REALTIME_TEST_KEY), "{error}");
    }

    #[test]
    fn validate_realtime_proxy_rejects_userinfo_without_echoing_secrets() {
        // A base URL carrying userinfo must fail closed with fixed text —
        // neither the username nor the password may surface in the error.
        let settings = realtime_test_settings("http://alice:SUPER-SECRET-PASSWORD@127.0.0.1:1/v1");
        let error = validate_realtime_proxy(&settings)
            .expect_err("userinfo base URL must fail closed")
            .to_string();
        assert!(error.contains("username or password"), "{error}");
        assert!(!error.contains("alice"), "{error}");
        assert!(!error.contains("SUPER-SECRET-PASSWORD"), "{error}");
        assert!(!error.contains(REALTIME_TEST_KEY), "{error}");
    }

    #[test]
    fn validate_realtime_proxy_rejects_query_without_echoing_token() {
        // A query/fragment (potential routing secret) must fail closed with
        // fixed text and never be echoed.
        let settings =
            realtime_test_settings("http://127.0.0.1:1/v1?token=SUPER-SECRET-QUERY-TOKEN#frag");
        let error = validate_realtime_proxy(&settings)
            .expect_err("query base URL must fail closed")
            .to_string();
        assert!(error.contains("query or fragment"), "{error}");
        assert!(!error.contains("SUPER-SECRET-QUERY-TOKEN"), "{error}");
        assert!(!error.contains("token="), "{error}");
        assert!(!error.contains(REALTIME_TEST_KEY), "{error}");
    }

    #[test]
    fn realtime_endpoint_never_carries_query_or_fragment_into_url() {
        // Defense in depth: even if a non-canonical base slipped through, the
        // built URL must never contain its query/fragment (which could carry
        // a secret) — only the canonical base path is interpolated.
        let url = realtime_endpoint(
            "http://127.0.0.1:1/base?token=SUPER-SECRET-QUERY#frag=secret",
            "/realtime/calls",
        );
        assert_eq!(url, "http://127.0.0.1:1/base/v1/realtime/calls");
        assert!(!url.contains("SUPER-SECRET-QUERY"), "{url}");
        assert!(!url.contains("secret"), "{url}");
    }

    #[tokio::test]
    async fn realtime_create_call_rejects_empty_offer() {
        let settings = realtime_test_settings("http://127.0.0.1:1");
        let err = realtime_create_call(&settings, "   \r\n\t ")
            .await
            .expect_err("empty offer must error");
        assert!(err.to_string().contains("SDP offer"), "{}", err);
    }

    #[tokio::test]
    async fn realtime_create_call_rejects_oversized_offer() {
        let settings = realtime_test_settings("http://127.0.0.1:1");
        let huge = "v=0\r\n".repeat(REALTIME_BODY_LIMIT / 5 + 1);
        let err = realtime_create_call(&settings, &huge)
            .await
            .expect_err("oversized offer must error");
        let msg = err.to_string();
        assert!(msg.contains("SDP offer"), "{msg}");
        assert!(msg.contains("256 KiB"), "{msg}");
        assert!(!msg.contains(REALTIME_TEST_KEY), "{msg}");
    }

    #[tokio::test]
    async fn realtime_create_call_network_error_uses_fixed_endpoint_name() {
        // Bind a listener and immediately drop it so nothing is listening:
        // the POST fails at connect, and the error must not echo the raw URL.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        drop(listener);
        let settings = realtime_test_settings(&format!("http://127.0.0.1:{port}"));
        let err = realtime_create_call(&settings, REALTIME_TEST_SDP_OFFER)
            .await
            .expect_err("connection refused must error")
            .to_string();
        assert!(err.contains("realtime_create_call"), "{err}");
        assert!(!err.contains(&format!("127.0.0.1:{port}")), "{err}");
        assert!(!err.contains(REALTIME_TEST_KEY), "{err}");
        assert!(!err.contains("Bearer"), "{err}");
    }

    #[tokio::test]
    async fn realtime_create_call_redacts_credentials_in_non_2xx_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let settings = realtime_test_settings(&format!("http://127.0.0.1:{port}"));

        let call =
            tokio::spawn(
                async move { realtime_create_call(&settings, REALTIME_TEST_SDP_OFFER).await },
            );

        let (mut sock, _addr) = listener.accept().await.expect("accept");
        let _req = read_http_request(&mut sock).await;
        // The upstream echoes credential-looking material back in every
        // established form (Authorization header, mixed-case, colon/space and
        // `=` separators, query-style values, and the bare configured key);
        // the shared redaction must scrub all of them from the error.
        let body = format!(
            "Authorization: Bearer {0}\nAPI_KEY: {0}\nToken : {0}\ntoken={0}\napiKey={1}\n{1}",
            "SUPER-SECRET-UPSTREAM-TOKEN", REALTIME_TEST_KEY
        );
        write_http_response(
            &mut sock,
            "HTTP/1.1 500 Internal Server Error",
            &[("Content-Type".to_owned(), "text/plain".to_owned())],
            body.as_bytes(),
        )
        .await;

        let err = call
            .await
            .expect("task join")
            .expect_err("non-2xx must error");
        let msg = err.to_string();
        assert!(msg.contains("500"), "{msg}");
        // The shared contract: every secret VALUE is scrubbed, the redaction
        // marker is present, and the truncated body stays bounded.
        assert!(!msg.contains("SUPER-SECRET-UPSTREAM-TOKEN"), "{msg}");
        assert!(!msg.contains(REALTIME_TEST_KEY), "{msg}");
        assert!(!msg.contains("Bearer"), "{msg}");
        assert!(msg.contains("[REDACTED]"), "{msg}");
    }

    #[tokio::test]
    async fn realtime_create_call_rejects_oversized_response_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let settings = realtime_test_settings(&format!("http://127.0.0.1:{port}"));

        let call =
            tokio::spawn(
                async move { realtime_create_call(&settings, REALTIME_TEST_SDP_OFFER).await },
            );

        let (mut sock, _addr) = listener.accept().await.expect("accept");
        let _req = read_http_request(&mut sock).await;
        // Claim an oversized answer via Content-Length only: the pre-check
        // must reject it on the header, before any body bytes are read.
        let header = format!(
            "HTTP/1.1 200 OK\r\nLocation: /v1/realtime/calls/rtc_big\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            REALTIME_BODY_LIMIT + 1
        );
        sock.write_all(header.as_bytes())
            .await
            .expect("write oversized headers");
        drop(sock);

        let err = call
            .await
            .expect("task join")
            .expect_err("oversized response body must error");
        let msg = err.to_string();
        assert!(msg.contains("256 KiB"), "{msg}");
        assert!(msg.contains("bound"), "{msg}");
        assert!(!msg.contains(REALTIME_TEST_KEY), "{msg}");
    }

    struct MockRequest {
        method: String,
        path: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl MockRequest {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }
    }

    /// Decodes the captured create-call request body as the contract JSON
    /// `{sdp, session}` object.
    fn json_request_body(body: &[u8]) -> Value {
        serde_json::from_slice(body).expect("JSON request body")
    }

    /// Reads one HTTP/1.1 request (request line + headers + exactly
    /// `Content-Length` body bytes) from `reader`.
    async fn read_http_request<R: tokio::io::AsyncRead + Unpin>(reader: &mut R) -> MockRequest {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        let header_end = loop {
            let n = reader
                .read(&mut chunk)
                .await
                .expect("read request bytes before connection close");
            if n == 0 {
                panic!("connection closed before full HTTP request was received");
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(idx) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break idx + 4;
            }
        };
        let header_block =
            std::str::from_utf8(&buf[..header_end]).expect("ascii request header block");
        let mut lines = header_block.split("\r\n");
        let request_line = lines.next().expect("request line");
        let mut parts = request_line.split_whitespace();
        let method = parts.next().expect("method").to_owned();
        let path = parts.next().expect("path").to_owned();
        let mut headers = Vec::new();
        let mut content_length = 0usize;
        for line in lines {
            if line.is_empty() {
                continue;
            }
            if let Some((k, v)) = line.split_once(':') {
                let key = k.trim();
                let value = v.trim();
                if key.eq_ignore_ascii_case("content-length") {
                    content_length = value.parse().expect("content-length");
                }
                headers.push((key.to_owned(), value.to_owned()));
            }
        }
        // Keep reading until the full body (per Content-Length) is buffered.
        while buf.len() < header_end + content_length {
            let n = reader.read(&mut chunk).await.expect("read request body");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let body = buf[header_end..header_end + content_length].to_vec();
        MockRequest {
            method,
            path,
            headers,
            body,
        }
    }

    async fn write_http_response(
        socket: &mut tokio::net::TcpStream,
        status_line: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) {
        let mut out = format!("{status_line}\r\n");
        for (k, v) in headers {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        out.push_str(&format!("Content-Length: {}\r\n", body.len()));
        out.push_str("Connection: close\r\n\r\n");
        socket
            .write_all(out.as_bytes())
            .await
            .expect("write response headers");
        socket.write_all(body).await.expect("write response body");
        socket.flush().await.expect("flush response");
    }

    #[tokio::test]
    async fn realtime_create_call_posts_session_and_returns_sdp_call_id() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let settings = realtime_test_settings(&format!("http://127.0.0.1:{port}"));

        let call =
            tokio::spawn(
                async move { realtime_create_call(&settings, REALTIME_TEST_SDP_OFFER).await },
            );

        let (mut sock, _addr) = listener.accept().await.expect("accept");
        let req = read_http_request(&mut sock).await;
        // Method + path: POST /v1/realtime/calls.
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/v1/realtime/calls");
        // Bearer header carries only the test fake value.
        assert_eq!(
            req.header("authorization").unwrap(),
            format!("Bearer {REALTIME_TEST_KEY}")
        );
        // AVAS rejects the create-call request when this exact alpha contract
        // header is absent or carries a different value.
        assert_eq!(req.header("openai-alpha"), Some("quicksilver=v2"));
        // The create-call contract is a JSON POST with exactly `sdp` and the
        // required frameless v3 `session` object. The proxy owns model injection.
        let content_type = req.header("content-type").expect("content-type");
        assert_eq!(
            content_type, "application/json",
            "content-type: {content_type:?}"
        );
        let request_body = json_request_body(&req.body);
        let mut body_keys = request_body
            .as_object()
            .expect("request body must be an object")
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        body_keys.sort();
        assert_eq!(
            body_keys,
            ["sdp", "session"],
            "create-call body must contain sdp and session: {request_body}"
        );
        assert_eq!(request_body["sdp"], REALTIME_TEST_SDP_OFFER);
        assert_eq!(request_body["session"]["instructions"], "");
        assert!(request_body["session"].get("model").is_none());
        assert!(request_body["session"].get("type").is_none());
        assert!(request_body["session"]["audio"].get("input").is_none());
        assert_eq!(request_body["session"]["audio"]["output"]["voice"], "sol");
        assert_eq!(request_body["session"]["delegation"]["type"], "client");

        let sdp_answer = "v=0\r\no=- 2 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n";
        write_http_response(
            &mut sock,
            "HTTP/1.1 201 Created",
            &[
                (
                    "Location".to_owned(),
                    "/v1/realtime/calls/rtc_alpha-001".to_owned(),
                ),
                ("Content-Type".to_owned(), "application/sdp".to_owned()),
            ],
            sdp_answer.as_bytes(),
        )
        .await;

        let result = call.await.expect("task join").expect("create call ok");
        assert_eq!(result["sdp"], sdp_answer);
        assert_eq!(result["callId"], "rtc_alpha-001");
    }

    /// Loopback stand-in for the current create-call contract: a complete
    /// object-valued `session` without the rejected `audio.input` field
    /// succeeds; missing or malformed session data fails with 400. Returns
    /// the parsed request and response status line.
    async fn serve_realtime_create_call(listener: &TcpListener) -> (MockRequest, String) {
        let (mut sock, _addr) = listener.accept().await.expect("accept");
        let req = read_http_request(&mut sock).await;
        let request_body = json_request_body(&req.body);
        let session = &request_body["session"];
        let session_is_valid = session["instructions"] == ""
            && session.get("model").is_none()
            && session.get("type").is_none()
            && session["audio"].get("input").is_none()
            && session["audio"]["output"]["voice"]
                .as_str()
                .is_some_and(|voice| !voice.trim().is_empty())
            && session["delegation"]["type"] == "client";
        let (status_line, headers, body): (&str, Vec<(String, String)>, Vec<u8>) =
            if session_is_valid {
                (
                    "HTTP/1.1 201 Created",
                    vec![
                        (
                            "Location".to_owned(),
                            "/v1/realtime/calls/rtc_alpha-001".to_owned(),
                        ),
                        ("Content-Type".to_owned(), "application/sdp".to_owned()),
                    ],
                    b"v=0\r\no=- 2 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n".to_vec(),
                )
            } else {
                (
                    "HTTP/1.1 400 Bad Request",
                    vec![("Content-Type".to_owned(), "text/plain".to_owned())],
                    b"create-call requires an object-valued session".to_vec(),
                )
            };
        write_http_response(&mut sock, status_line, &headers, &body).await;
        (req, status_line.to_owned())
    }

    /// Reads just the status line of an HTTP response (the caller only needs
    /// the status for the mock-fidelity leg).
    async fn read_http_status_line<R: tokio::io::AsyncRead + Unpin>(reader: &mut R) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let n = reader.read(&mut chunk).await.expect("read response bytes");
            assert!(n > 0, "mock closed before the status line");
            buf.extend_from_slice(&chunk[..n]);
            if let Some(idx) = buf.windows(2).position(|w| w == b"\r\n") {
                return String::from_utf8(buf[..idx].to_vec()).expect("ascii status line");
            }
            assert!(buf.len() < 64 * 1024, "mock response head unbounded");
        }
    }

    #[tokio::test]
    async fn realtime_create_call_requires_complete_session_object() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let settings = realtime_test_settings(&format!("http://127.0.0.1:{port}"));

        // Leg 1: the real client must satisfy the current session contract.
        let call =
            tokio::spawn(
                async move { realtime_create_call(&settings, REALTIME_TEST_SDP_OFFER).await },
            );
        let (req, status_line) = serve_realtime_create_call(&listener).await;
        let request_body = json_request_body(&req.body);
        assert_eq!(request_body["session"]["instructions"], "");
        assert!(request_body["session"].get("model").is_none());
        assert!(request_body["session"].get("type").is_none());
        assert!(request_body["session"]["audio"].get("input").is_none());
        assert_eq!(request_body["session"]["audio"]["output"]["voice"], "sol");
        assert_eq!(request_body["sdp"], REALTIME_TEST_SDP_OFFER);
        assert!(status_line.starts_with("HTTP/1.1 201"), "{status_line}");
        let response = call
            .await
            .expect("task join")
            .expect("session-bearing create call must succeed");
        assert_eq!(response["callId"], "rtc_alpha-001");

        // Leg 2: mock fidelity — the legacy request without session must fail.
        let old_shape = json!({ "sdp": REALTIME_TEST_SDP_OFFER });
        let mut client = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        let raw = format!(
            "POST /v1/realtime/calls HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            old_shape.to_string().len(),
            old_shape
        );
        client
            .write_all(raw.as_bytes())
            .await
            .expect("write legacy request without session");
        let (_req, status_line) = serve_realtime_create_call(&listener).await;
        let rejected = read_http_status_line(&mut client).await;
        assert_eq!(status_line, "HTTP/1.1 400 Bad Request", "{status_line}");
        assert_eq!(rejected, "HTTP/1.1 400 Bad Request", "{rejected}");
    }

    #[tokio::test]
    async fn realtime_create_call_accepts_uuid_location() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let settings = realtime_test_settings(&format!("http://127.0.0.1:{port}"));
        let uuid = "550e8400-e29b-41d4-a716-446655440000";

        let call =
            tokio::spawn(
                async move { realtime_create_call(&settings, REALTIME_TEST_SDP_OFFER).await },
            );

        let (mut sock, _addr) = listener.accept().await.expect("accept");
        let _req = read_http_request(&mut sock).await;
        let sdp_answer = "v=0\r\no=- 9 9 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n";
        write_http_response(
            &mut sock,
            "HTTP/1.1 200 OK",
            &[(
                "Location".to_owned(),
                format!("https://proxy.example/v1/realtime/calls/{uuid}"),
            )],
            sdp_answer.as_bytes(),
        )
        .await;

        let result = call.await.expect("task join").expect("create call ok");
        assert_eq!(result["callId"], uuid);
        assert_eq!(result["sdp"], sdp_answer);
    }

    #[tokio::test]
    async fn realtime_create_call_rejects_missing_location() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let settings = realtime_test_settings(&format!("http://127.0.0.1:{port}"));

        let call =
            tokio::spawn(
                async move { realtime_create_call(&settings, REALTIME_TEST_SDP_OFFER).await },
            );

        let (mut sock, _addr) = listener.accept().await.expect("accept");
        let _req = read_http_request(&mut sock).await;
        write_http_response(
            &mut sock,
            "HTTP/1.1 201 Created",
            &[("Content-Type".to_owned(), "application/sdp".to_owned())],
            b"v=0\r\no=- 3 3 IN IP4 127.0.0.1\r\n",
        )
        .await;

        let err = call
            .await
            .expect("task join")
            .expect_err("missing Location must error");
        let msg = err.to_string();
        assert!(msg.contains("no `Location` header"), "{msg}");
        // Must not echo auth material.
        assert!(!msg.contains(REALTIME_TEST_KEY), "{msg}");
        assert!(!msg.contains("Bearer"), "{msg}");
    }

    #[tokio::test]
    async fn realtime_create_call_rejects_illegal_location() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let settings = realtime_test_settings(&format!("http://127.0.0.1:{port}"));

        let call =
            tokio::spawn(
                async move { realtime_create_call(&settings, REALTIME_TEST_SDP_OFFER).await },
            );

        let (mut sock, _addr) = listener.accept().await.expect("accept");
        let _req = read_http_request(&mut sock).await;
        write_http_response(
            &mut sock,
            "HTTP/1.1 201 Created",
            &[
                (
                    "Location".to_owned(),
                    "/v1/realtime/calls/not-a-valid-id".to_owned(),
                ),
                ("Content-Type".to_owned(), "application/sdp".to_owned()),
            ],
            b"v=0\r\no=- 4 4 IN IP4 127.0.0.1\r\n",
        )
        .await;

        let err = call
            .await
            .expect("task join")
            .expect_err("illegal Location must error");
        let msg = err.to_string();
        assert!(msg.contains("not-a-valid-id"), "{msg}");
        assert!(msg.contains("Location"), "{msg}");
        assert!(!msg.contains(REALTIME_TEST_KEY), "{msg}");
        assert!(!msg.contains("Bearer"), "{msg}");
    }

    #[tokio::test]
    async fn realtime_create_call_location_error_never_echoes_query_token() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let settings = realtime_test_settings(&format!("http://127.0.0.1:{port}"));

        let call =
            tokio::spawn(
                async move { realtime_create_call(&settings, REALTIME_TEST_SDP_OFFER).await },
            );

        let (mut sock, _addr) = listener.accept().await.expect("accept");
        let _req = read_http_request(&mut sock).await;
        // A Location carrying a query token must fail on the derived path
        // segment without ever echoing the raw Location, its query, or the
        // fragment — the token is server-side routing material.
        write_http_response(
            &mut sock,
            "HTTP/1.1 201 Created",
            &[
                (
                    "Location".to_owned(),
                    "/v1/realtime/calls/not-a-valid-id?token=SUPER-SECRET-QUERY-TOKEN#frag=also-secret".to_owned(),
                ),
                ("Content-Type".to_owned(), "application/sdp".to_owned()),
            ],
            b"v=0\r\no=- 4 4 IN IP4 127.0.0.1\r\n",
        )
        .await;

        let err = call
            .await
            .expect("task join")
            .expect_err("illegal Location must error");
        let msg = err.to_string();
        // The derived path segment is the only Location-derived content.
        assert!(msg.contains("not-a-valid-id"), "{msg}");
        assert!(msg.contains("Location"), "{msg}");
        // Query/fragment secrets never surface in the error.
        assert!(!msg.contains("SUPER-SECRET-QUERY-TOKEN"), "{msg}");
        assert!(!msg.contains("also-secret"), "{msg}");
        assert!(!msg.contains("token="), "{msg}");
        assert!(!msg.contains(REALTIME_TEST_KEY), "{msg}");
        assert!(!msg.contains("Bearer"), "{msg}");
    }

    #[test]
    fn parse_realtime_call_id_non_utf8_location_uses_fixed_text() {
        // Non-UTF-8 Location bytes must not be Debug-echoed: only fixed text.
        let bad = reqwest::header::HeaderValue::from_bytes(b"/v1/realtime/calls/\xff\xfe")
            .expect("HeaderValue accepts high bytes");
        let err = parse_realtime_call_id(Some(&bad))
            .expect_err("non-UTF-8 Location must error")
            .to_string();
        assert!(err.contains("not valid UTF-8"), "{err}");
        assert!(!err.contains("\\xff"), "{err}");
        assert!(!err.contains("\\xfe"), "{err}");
        assert!(!err.contains(REALTIME_TEST_KEY), "{err}");
    }

    #[tokio::test]
    async fn realtime_create_call_rejects_empty_sdp() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let settings = realtime_test_settings(&format!("http://127.0.0.1:{port}"));

        let call =
            tokio::spawn(
                async move { realtime_create_call(&settings, REALTIME_TEST_SDP_OFFER).await },
            );

        let (mut sock, _addr) = listener.accept().await.expect("accept");
        let _req = read_http_request(&mut sock).await;
        // Whitespace-only body must be rejected as an empty SDP answer.
        write_http_response(
            &mut sock,
            "HTTP/1.1 201 Created",
            &[
                (
                    "Location".to_owned(),
                    "/v1/realtime/calls/rtc_ok".to_owned(),
                ),
                ("Content-Type".to_owned(), "application/sdp".to_owned()),
            ],
            b"   \r\n  \t ",
        )
        .await;

        let err = call
            .await
            .expect("task join")
            .expect_err("empty SDP must error");
        assert!(err.to_string().contains("empty SDP"), "{}", err);
    }

    #[tokio::test]
    async fn realtime_create_call_surfaces_truncated_non_2xx_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let settings = realtime_test_settings(&format!("http://127.0.0.1:{port}"));

        let call =
            tokio::spawn(
                async move { realtime_create_call(&settings, REALTIME_TEST_SDP_OFFER).await },
            );

        let (mut sock, _addr) = listener.accept().await.expect("accept");
        let _req = read_http_request(&mut sock).await;
        let long_body = "X".repeat(2000);
        write_http_response(
            &mut sock,
            "HTTP/1.1 503 Service Unavailable",
            &[("Content-Type".to_owned(), "text/plain".to_owned())],
            long_body.as_bytes(),
        )
        .await;

        let err = call
            .await
            .expect("task join")
            .expect_err("non-2xx must error");
        let msg = err.to_string();
        assert!(msg.contains("503"), "{msg}");
        // Body is truncated to 300 chars in the error.
        assert!(msg.contains(&"X".repeat(300)), "{msg}");
        assert!(!msg.contains(&"X".repeat(301)), "{msg}");
        // Auth must not leak into the error.
        assert!(!msg.contains(REALTIME_TEST_KEY), "{msg}");
        assert!(!msg.contains("Bearer"), "{msg}");
    }

    /// A realistic ICE-gathered offer (the shape the browser POSTs after the
    /// `waitForIceGatheringComplete` fix): it carries `a=candidate` lines and a
    /// trailing `a=end-of-candidates`. The proxy MUST forward the SDP part
    /// byte-for-byte so the upstream's answer can actually connect — the
    /// long-standing silent failure was the browser posting a candidate-less
    /// offer. This test pins the contract with a gather-complete offer
    /// containing host-candidate lines.
    #[tokio::test]
    async fn realtime_create_call_forwards_ice_gathered_sdp_verbatim() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let settings = realtime_test_settings(&format!("http://127.0.0.1:{port}"));
        // A gather-complete offer: host candidate + end-of-candidates marker.
        // This is what pc.localDescription.sdp looks like after ICE gathering
        // resolves on a loopback/LAN (host candidates only).
        let gathered_offer = "v=0\r\no=- 42 2 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
             a=mid:0\r\na=rtpmap:111 opus/48000/2\r\n\
             a=candidate:1 1 udp 2122252543 127.0.0.1 50000 typ host generation 0 ufrag abcd\r\n\
             a=candidate:1 2 udp 2122252543 127.0.0.1 50000 typ host generation 0 ufrag abcd\r\n\
             a=end-of-candidates\r\n";

        let offer_for_task = gathered_offer.to_owned();
        let call =
            tokio::spawn(async move { realtime_create_call(&settings, &offer_for_task).await });

        let (mut sock, _addr) = listener.accept().await.expect("accept");
        let req = read_http_request(&mut sock).await;
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/v1/realtime/calls");
        assert_eq!(req.header("openai-alpha"), Some("quicksilver=v2"));
        let content_type = req.header("content-type").expect("content-type");
        assert_eq!(
            content_type, "application/json",
            "content-type: {content_type:?}"
        );
        let request_body = json_request_body(&req.body);
        // The SDP is forwarded VERBATIM, including every a=candidate line and
        // the end-of-candidates marker — no rewriting, no candidate stripping.
        // The browser's gathered offer is what the upstream sees.
        let sdp = request_body["sdp"].as_str().expect("sdp string");
        assert_eq!(
            sdp, gathered_offer,
            "gathered offer must be forwarded byte-for-byte"
        );
        assert!(
            sdp.contains("a=candidate:"),
            "offer must carry ICE candidates: {sdp}"
        );
        assert!(
            sdp.contains("a=end-of-candidates"),
            "offer must mark gather complete: {sdp}"
        );
        // Frameless session metadata accompanies the gathered offer without a model.
        assert_eq!(request_body["session"]["instructions"], "");
        assert!(request_body["session"].get("model").is_none());
        assert!(request_body["session"].get("type").is_none());
        assert!(request_body["session"]["audio"].get("input").is_none());
        assert_eq!(request_body["session"]["audio"]["output"]["voice"], "sol");
        assert_eq!(request_body["session"]["delegation"]["type"], "client");

        // Reply with a bare SDP answer + Location; the proxy returns both.
        let sdp_answer =
            "v=0\r\no=- 7 7 IN IP4 127.0.0.1\r\ns=-\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n";
        write_http_response(
            &mut sock,
            "HTTP/1.1 201 Created",
            &[
                (
                    "Location".to_owned(),
                    "/v1/realtime/calls/rtc_gather_001".to_owned(),
                ),
                ("Content-Type".to_owned(), "application/sdp".to_owned()),
            ],
            sdp_answer.as_bytes(),
        )
        .await;
        let result = call
            .await
            .expect("task join")
            .expect("realtime_create_call ok");
        assert_eq!(result["sdp"], sdp_answer);
        assert_eq!(result["callId"], "rtc_gather_001");
        // Auth never leaks into the persisted call id or SDP.
        assert!(
            !result.to_string().contains(REALTIME_TEST_KEY),
            "{}",
            result
        );
    }

    // ---- STT voice proxy (stt_transcribe): bounded WAV-only contract ----
    //
    // The browser sends ONLY base64 WAV audio + a MIME type; the endpoint
    // URL and bearer key come from the server-held live settings. These
    // tests exercise the payload bounds, the strict WAV parse, and the full
    // forward path against a loopback TCP mock speaking the OpenAI-compatible
    // transcriptions endpoint. The bearer below is a fixed non-sensitive
    // placeholder, never a live secret.

    const STT_TEST_KEY: &str = "stt-test-bearer-not-a-real-secret";

    fn stt_test_settings(base_url: &str) -> pi_coding::LiveRuntimeSettings {
        pi_coding::LiveRuntimeSettings {
            enabled: true,
            mode: "stt".to_owned(),
            stt_base_url: base_url.to_owned(),
            stt_api_key: STT_TEST_KEY.to_owned(),
            stt_model: "whisper-1".to_owned(),
            realtime_base_url: String::new(),
            realtime_api_key: String::new(),
            realtime_model: "gpt-realtime-1.5".to_owned(),
            voice: "sol".to_owned(),
            language: None,
            allow_insecure: true,
        }
    }

    /// A small valid mono PCM16 WAV (0.5 s of silence at 16 kHz).
    fn stt_test_wav() -> Vec<u8> {
        pi_coding::live::encode_wav(&vec![0i16; 8_000], 16_000, 1)
    }

    fn stt_b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[tokio::test]
    async fn stt_transcribe_forwards_wav_and_returns_transcript() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let settings = stt_test_settings(&format!("http://127.0.0.1:{port}"));
        let wav = stt_test_wav();

        // "Audio/WAV": the MIME allowlist is case-insensitive.
        let call =
            tokio::spawn(
                async move { stt_transcribe(&settings, &stt_b64(&wav), "Audio/WAV").await },
            );

        let (mut sock, _addr) = listener.accept().await.expect("accept");
        let req = read_http_request(&mut sock).await;
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/v1/audio/transcriptions");
        assert_eq!(
            req.header("authorization").unwrap(),
            format!("Bearer {STT_TEST_KEY}")
        );
        // The backend's SttClient forwards the WAV as a multipart form
        // ({file: audio/wav, model}) with the server-held key.
        let content_type = req.header("content-type").expect("content-type");
        assert!(
            content_type.starts_with("multipart/form-data"),
            "content-type: {content_type:?}"
        );
        let body = String::from_utf8_lossy(&req.body);
        assert!(
            body.contains("RIFF"),
            "wav bytes embedded in the multipart body"
        );
        assert!(body.contains("audio/wav"), "{body}");
        assert!(body.contains("name=\"file\""), "{body}");
        assert!(body.contains("name=\"model\""), "{body}");
        assert!(body.contains("whisper-1"), "{body}");

        write_http_response(
            &mut sock,
            "HTTP/1.1 200 OK",
            &[("Content-Type".to_owned(), "application/json".to_owned())],
            b"{\"text\":\"stt hello from the mock\"}",
        )
        .await;

        let result = call.await.expect("task join").expect("transcribe ok");
        assert_eq!(result["text"], "stt hello from the mock");
        // The response carries ONLY the transcript — never settings, keys,
        // or the audio bytes.
        assert_eq!(
            result.as_object().map(|object| object.len()),
            Some(1),
            "{result}"
        );
    }

    #[tokio::test]
    async fn stt_transcribe_rejects_non_wav_mime() {
        let settings = stt_test_settings("http://127.0.0.1:1");
        let wav = stt_test_wav();
        for mime in [
            "audio/webm",
            "audio/webm;codecs=opus",
            "audio/mp4",
            "application/octet-stream",
            "text/plain",
            "",
        ] {
            let err = stt_transcribe(&settings, &stt_b64(&wav), mime)
                .await
                .expect_err("non-wav MIME must be rejected")
                .to_string();
            assert!(err.contains("unsupported audio MIME type"), "{err}");
            assert!(!err.contains(STT_TEST_KEY), "{err}");
            assert!(!err.contains("Bearer"), "{err}");
        }
        // A hostile long MIME is echoed only as a bounded prefix.
        let long_mime = format!("{}/{}", "a".repeat(300), "b".repeat(300));
        let err = stt_transcribe(&settings, &stt_b64(&wav), &long_mime)
            .await
            .expect_err("long MIME must be rejected")
            .to_string();
        assert!(err.contains("unsupported audio MIME type"), "{err}");
        assert!(!err.contains(&long_mime), "full MIME echoed: {err}");
    }

    #[tokio::test]
    async fn stt_transcribe_rejects_oversized_audio() {
        let settings = stt_test_settings("http://127.0.0.1:1");
        // One byte over the cap: rejected at the decoded-size bound (the
        // base64 char pre-check cannot distinguish cap+1 bytes from cap
        // bytes, so the decoded check is the binding one).
        let oversize = vec![0u8; STT_MAX_AUDIO_BYTES + 1];
        let err = stt_transcribe(&settings, &stt_b64(&oversize), "audio/wav")
            .await
            .expect_err("oversized audio must be rejected")
            .to_string();
        assert!(err.contains("exceeds the"), "{err}");
        assert!(err.contains(&STT_MAX_AUDIO_BYTES.to_string()), "{err}");
        assert!(!err.contains(STT_TEST_KEY), "{err}");

        // A base64 string far beyond any plausible WAV is rejected by the
        // char-length pre-check without decoding.
        let huge = "A".repeat(STT_MAX_AUDIO_BYTES.div_ceil(3) * 4 + 1);
        let err = stt_transcribe(&settings, &huge, "audio/wav")
            .await
            .expect_err("oversized base64 must be rejected")
            .to_string();
        assert!(err.contains("exceeds the"), "{err}");
        assert!(!err.contains(STT_TEST_KEY), "{err}");
    }

    #[tokio::test]
    async fn stt_transcribe_rejects_malformed_wav_and_base64() {
        let settings = stt_test_settings("http://127.0.0.1:1");

        // Non-WAV bytes pass the size bound but fail the strict RIFF parse.
        let err = stt_transcribe(&settings, &stt_b64(b"definitely not a wav"), "audio/wav")
            .await
            .expect_err("non-WAV bytes must be rejected")
            .to_string();
        assert!(err.contains("RIFF/WAVE"), "{err}");
        assert!(!err.contains(STT_TEST_KEY), "{err}");

        // Malformed base64 fails before any parse.
        let err = stt_transcribe(&settings, "!!!not-base64!!!", "audio/wav")
            .await
            .expect_err("malformed base64 must be rejected")
            .to_string();
        assert!(err.contains("not valid base64"), "{err}");
        assert!(!err.contains(STT_TEST_KEY), "{err}");

        // Non-PCM WAV (format 2) is rejected by the strict parse.
        let mut wav = stt_test_wav();
        wav[20..22].copy_from_slice(&2u16.to_le_bytes());
        let err = stt_transcribe(&settings, &stt_b64(&wav), "audio/wav")
            .await
            .expect_err("non-PCM WAV must be rejected")
            .to_string();
        assert!(err.contains("not PCM"), "{err}");
        assert!(!err.contains(STT_TEST_KEY), "{err}");
    }

    #[tokio::test]
    async fn stt_transcribe_rejects_unconfigured_live() {
        let mut settings = stt_test_settings("http://127.0.0.1:1");
        let wav = stt_test_wav();
        settings.enabled = false;
        let err = stt_transcribe(&settings, &stt_b64(&wav), "audio/wav")
            .await
            .expect_err("disabled live must fail closed")
            .to_string();
        assert!(err.contains("Settings.live.enabled"), "{err}");
        assert!(!err.contains(STT_TEST_KEY), "{err}");
        assert!(!err.contains("Bearer"), "{err}");

        settings.enabled = true;
        settings.stt_base_url.clear();
        let err = stt_transcribe(&settings, &stt_b64(&wav), "audio/wav")
            .await
            .expect_err("unconfigured live must fail closed")
            .to_string();
        assert!(err.contains("Settings.live.sttBaseUrl"), "{err}");
        assert!(!err.contains(STT_TEST_KEY), "{err}");

        settings.stt_base_url = "http://127.0.0.1:1".to_owned();
        settings.allow_insecure = false;
        let err = stt_transcribe(&settings, &stt_b64(&wav), "audio/wav")
            .await
            .expect_err("plaintext live must fail closed")
            .to_string();
        assert!(err.contains("allowInsecure"), "{err}");
        assert!(!err.contains(STT_TEST_KEY), "{err}");
    }

    /// Stack overflow regression: execute the actual `GetTree` handler against
    /// a 20,001-entry recorded session and serialize the returned response on
    /// a 2 MiB stack (the Tokio worker default). The command path must bypass
    /// `serde_json::to_value`, preserve the nested contract, and emit every
    /// entry in parent-child order without recursion-limit errors or abort.
    #[test]
    fn get_tree_rpc_serializes_deep_chain_without_stack_overflow() {
        const ENTRY_COUNT: usize = 20_001;
        const WORKER_STACK: usize = 2 * 1024 * 1024;
        let join_result = std::thread::Builder::new()
            .stack_size(WORKER_STACK)
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("test runtime");
                runtime.block_on(async move {
                    let root = tempfile::tempdir().expect("deep session root");
                    let cwd = root.path().join("workspace");
                    let session_dir = root.path().join("sessions");
                    std::fs::create_dir_all(&cwd).expect("workspace");
                    std::fs::create_dir_all(&session_dir).expect("session dir");
                    let recorder = pi_coding::start_session_in(
                        &cwd,
                        None,
                        None,
                        Some(&session_dir),
                        Some("deep-get-tree"),
                        None,
                    )
                    .expect("start session");
                    let mut entry_ids = Vec::with_capacity(ENTRY_COUNT);
                    for _ in 0..ENTRY_COUNT {
                        entry_ids.push(
                            recorder
                                .record_message(&Message::user_text("msg", 0))
                                .expect("record chain entry"),
                        );
                    }
                    recorder.persist_now().expect("persist chain");
                    let path = recorder.path();
                    recorder.close().expect("close writer");
                    let recorder = pi_coding::resume_session(&path).expect("resume deep session");
                    let session = disabled_session(pi_coding::SessionOptions {
                        model: faux_model(),
                        cwd: cwd.clone(),
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
                        auth_manager: None,
                    })
                    .expect("deep session");
                    session.set_session_dir(session_dir);
                    session.record(recorder).expect("attach recorder");
                    let app = isolated_application(session).await;
                    let response = handle_command(
                        &app,
                        &settings_state(),
                        &workflows_state(),
                        RpcCommand::GetTree {
                            id: Some("tree-deep".into()),
                        },
                    )
                    .await;
                    assert!(response.success, "{response:?}");
                    assert!(response.data.is_none(), "deep tree must bypass Value");
                    assert!(response.raw_data.is_some(), "deep tree must use raw data");
                    let mut buffer = Vec::new();
                    crate::modes::json::write_json_line(&mut buffer, &response)
                        .expect("write actual GetTree JSONL response");
                    let encoded = String::from_utf8(buffer).expect("UTF-8 JSONL response");
                    (encoded, entry_ids)
                })
            })
            .expect("worker-stack thread")
            .join();
        assert!(
            join_result.is_ok(),
            "GetTree must not overflow {} stack",
            WORKER_STACK
        );
        let (encoded, entry_ids) = join_result.unwrap();
        assert!(encoded.contains(r#""type":"response""#));
        assert!(encoded.contains(r#""command":"get_tree""#));
        assert!(encoded.contains(r#""id":"tree-deep""#));
        assert!(encoded.ends_with('\n'), "RPC response must use LF framing");
        assert_eq!(encoded.matches(r#""entry":{"#).count(), ENTRY_COUNT);
        assert_eq!(encoded.matches(r#""children":[{"#).count(), ENTRY_COUNT - 1);
        assert_eq!(encoded.matches(r#""children":[]"#).count(), 1);
        let mut cursor = 0;
        for (index, id) in entry_ids.iter().enumerate() {
            let parent = index.checked_sub(1).map_or_else(
                || "null".to_owned(),
                |parent| serde_json::to_string(&entry_ids[parent]).expect("parent id"),
            );
            let expected = format!(
                r#""entry":{{"type":"message","id":{},"parentId":{parent},"timestamp":"#,
                serde_json::to_string(id).expect("entry id"),
            );
            let offset = encoded[cursor..]
                .find(&expected)
                .unwrap_or_else(|| panic!("missing ordered entry {index}"));
            cursor += offset + expected.len();
        }
        let leaf = serde_json::to_string(entry_ids.last().expect("leaf id")).expect("leaf id");
        assert!(encoded.contains(&format!(r#""leafId":{leaf}"#)));
        assert!(encoded.contains(&format!(r#""activeLeafId":{leaf}"#)));
    }

    #[test]
    fn session_list_row_wire_carries_agent_and_bootstrap_flags() {
        let row = crate::resume_catalog::ResumeSelectorRow {
            source: pi_coding::SessionSourceKind::Codex,
            source_badge: "codex",
            session_id: "wire-agent".to_owned(),
            cwd: PathBuf::from("/home/u/.slock/agents/agent-7"),
            modified_epoch: 1.0,
            display_time: "2026-01-01 00:00".to_owned(),
            summary: "<user_info>bootstrap</user_info>".to_owned(),
            path: PathBuf::from("/sessions/rollout-agent.jsonl"),
            size: 1,
            message_count: Some(1),
            name: None,
            status: pi_coding::CatalogRowStatus::Foreign,
            search_text: String::new(),
            message_blob: String::new(),
            has_parent_session: false,
            session_kind: pi_coding::SessionKind::WorkflowSupervisor,
            agent_session: true,
            bootstrap_only: true,
        };
        let wire = serde_json::to_value(RpcSessionListRow::from_resume_row(row, false))
            .expect("serialize session_list row");
        assert_eq!(wire["agentSession"], true, "{wire}");
        assert_eq!(wire["bootstrapOnly"], true, "{wire}");
        assert_eq!(wire["sessionKind"], "workflowSupervisor", "{wire}");

        let plain = crate::resume_catalog::ResumeSelectorRow {
            source: pi_coding::SessionSourceKind::NativePi,
            source_badge: "rpi",
            session_id: "wire-user".to_owned(),
            cwd: PathBuf::from("/home/u/Projects/app"),
            modified_epoch: 2.0,
            display_time: "2026-01-02 00:00".to_owned(),
            summary: "fix the login bug".to_owned(),
            path: PathBuf::from("/sessions/user.jsonl"),
            size: 2,
            message_count: Some(2),
            name: None,
            status: pi_coding::CatalogRowStatus::Native,
            search_text: String::new(),
            message_blob: String::new(),
            has_parent_session: false,
            session_kind: pi_coding::SessionKind::User,
            agent_session: false,
            bootstrap_only: false,
        };
        let wire = serde_json::to_value(RpcSessionListRow::from_resume_row(plain, false))
            .expect("serialize user row");
        assert_eq!(wire["agentSession"], false, "{wire}");
        assert_eq!(wire["bootstrapOnly"], false, "{wire}");
        assert_eq!(wire["sessionKind"], "user", "{wire}");
    }
    #[test]
    fn session_list_grok_row_wire_source_is_canonical_grok() {
        let row = crate::resume_catalog::ResumeSelectorRow {
            source: pi_coding::SessionSourceKind::Grok,
            source_badge: "grok/hyper",
            session_id: "grok-switch".to_owned(),
            cwd: PathBuf::from("/ws/pi-rs"),
            modified_epoch: 3.0,
            display_time: "2026-01-03 00:00".to_owned(),
            summary: "grok session".to_owned(),
            path: PathBuf::from("/sessions/grok.jsonl"),
            size: 3,
            message_count: Some(3),
            name: None,
            status: pi_coding::CatalogRowStatus::Foreign,
            search_text: String::new(),
            message_blob: String::new(),
            has_parent_session: false,
            session_kind: pi_coding::SessionKind::User,
            agent_session: false,
            bootstrap_only: false,
        };
        let wire = serde_json::to_value(RpcSessionListRow::from_resume_row(row, false))
            .expect("serialize grok row");
        // The Web switch authority accepts only the byte-exact canonical token
        // `grok`; the picker label `grok/hyper` would make every Grok row click
        // inert (no switch_session frame ever sent).
        assert_eq!(wire["source"], "grok", "{wire}");
        assert_ne!(wire["source"], "grok/hyper", "{wire}");
    }
    #[test]
    fn session_list_already_imported_row_carries_authoritative_native_id() {
        let row = crate::resume_catalog::ResumeSelectorRow {
            source: pi_coding::SessionSourceKind::Grok,
            source_badge: "grok/hyper",
            session_id: "grok-source".to_owned(),
            cwd: PathBuf::from("<workspace>/project"),
            modified_epoch: 4.0,
            display_time: "2026-01-04 00:00".to_owned(),
            summary: "grok session".to_owned(),
            path: PathBuf::from("<workspace>/grok/summary.json"),
            size: 4,
            message_count: Some(1),
            name: None,
            status: pi_coding::CatalogRowStatus::AlreadyImported {
                native_id: "native-header-id".to_owned(),
                native_path: PathBuf::from("<workspace>/sessions/import_storage-id.jsonl"),
            },
            search_text: String::new(),
            message_blob: String::new(),
            has_parent_session: false,
            session_kind: pi_coding::SessionKind::User,
            agent_session: false,
            bootstrap_only: false,
        };
        let wire = serde_json::to_value(RpcSessionListRow::from_resume_row(row, false))
            .expect("serialize imported row");
        assert_eq!(wire["nativeSessionId"], "native-header-id");
        assert_eq!(wire["nativePath"], "<workspace>/sessions/import_storage-id.jsonl");
    }
}
