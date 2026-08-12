use crate::code_review::{
    DiffFile, DiffHunk, DiffLineKind, FileDiff, FileDiffPage, ReviewScope, ReviewSnapshot,
    MAX_FILE_PAGE_LINES, load_review_snapshot_for,
};
use crate::code_review_panel::{CodeReviewController, ReviewCommentRole};
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
    StreamingBehavior, TodoOp, TodoPhase,
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
use tokio::sync::{mpsc, Semaphore};
pub(crate) const MAX_RPC_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
const JSONL_CHANNEL_CAPACITY: usize = 8;
pub(crate) const MAX_CONCURRENT_COMMANDS: usize = 16;
/// Default transcript lines for `agent_history` when `lines` is omitted.
/// Bounded by the core `pi_coding::MAX_HISTORY_LINES` cap.
const DEFAULT_RPC_HISTORY_LINES: usize = 80;

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RpcCommand {
    Prompt {
        #[serde(default)]
        id: Option<String>,
        message: String,
        #[serde(default)]
        images: Vec<ContentBlock>,
        #[serde(default, rename = "streamingBehavior")]
        streaming_behavior: Option<StreamingBehavior>,
    },
    Steer {
        #[serde(default)]
        id: Option<String>,
        message: String,
        #[serde(default)]
        images: Vec<ContentBlock>,
    },
    FollowUp {
        #[serde(default)]
        id: Option<String>,
        message: String,
        #[serde(default)]
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
    },
    SwitchSession {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "sessionPath")]
        session_path: String,
    },
    Fork {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "entryId")]
        entry_id: String,
    },
    Clone {
        #[serde(default)]
        id: Option<String>,
    },
    GetForkMessages {
        #[serde(default)]
        id: Option<String>,
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
    },
    /// Snapshot every side-chat tab: names, streaming state, and per-tab
    /// transcript rows.
    SideChatList {
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
    /// Cancel (drain) every queued steering/follow-up prompt (`/queue cancel`).
    QueueCancel {
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
        #[serde(default, rename = "jobIds")]
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
    /// and keeps the API key server-side). The POST body is
    /// `{ "sdp": offer, "session": { type, audio: { input, output } } }` —
    /// the create-call session is the Quicksilver shape WITHOUT `model`, which
    /// Codex realtime rejects with 400 (`Field session.model is not allowed`).
    /// The configured `model` is delivered over the `oai-events` data
    /// channel's `session.update` instead, where it IS accepted. The upstream
    /// answer is a bare SDP body with the call id in the `Location` header. Returns
    /// `{ "sdp": "<answer>", "callId": "<call id>" }` for the browser's
    /// `RTCPeerConnection` and the `oai-events` data channel.
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
    /// Open a bounded HEAD→working-tree or two-revision Git diff snapshot
    /// for code review, forking a read-only review agent. `from`/`to` must
    /// both be present (two-revision) or both absent (working tree); any
    /// other combination is rejected. Returns the full review snapshot plus
    /// controller thread/streaming state.
    /// Wire shape: `{ "type": "code_review_open", "id"?: string, "from"?: string, "to"?: string }`.
    CodeReviewOpen {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        from: Option<String>,
        #[serde(default)]
        to: Option<String>,
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
    /// comment is rejected as stale.
    /// Wire shape:
    /// `{ "type": "code_review_comment", "id"?: string, "snapshotId": string, "path": string, "oldStart": u32, "oldCount": u32, "newStart": u32, "newCount": u32, "contentHash": string, "comment": string }`.
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
    },
    /// Abort the in-flight review agent turn for the active hunk.
    /// Wire shape: `{ "type": "code_review_abort", "id"?: string }`.
    CodeReviewAbort {
        #[serde(default)]
        id: Option<String>,
    },
    /// Close the open code review: shut down the review agent and drop the
    /// snapshot/threads. Wire shape:
    /// `{ "type": "code_review_close", "id"?: string }`.
    CodeReviewClose {
        #[serde(default)]
        id: Option<String>,
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
    /// Render a loaded skill's frontmatter summary (the `/skill <name>`
    /// mirror). Rejects an empty or unknown name. Returns `{ name, summary }`.
    /// Wire shape: `{ "type": "skill", "id"?: string, "name": string }`.
    Skill {
        #[serde(default)]
        id: Option<String>,
        name: String,
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
}
impl RpcCommand {
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
            | Self::SetThinkingLevel { id, .. }
            | Self::CycleThinkingLevel { id }
            | Self::GetAvailableThinkingLevels { id }
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
            | Self::Clone { id }
            | Self::GetForkMessages { id }
            | Self::GetEntries { id, .. }
            | Self::GetTree { id }
            | Self::GetLastAssistantText { id }
            | Self::SetSessionName { id, .. }
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
            | Self::ProcessKeys { id, .. }
            | Self::ProcessResize { id, .. }
            | Self::ProcessSignal { id, .. }
            | Self::ProcessStop { id, .. }
            | Self::GoalCreate { id, .. }
            | Self::GoalGet { id }
            | Self::GoalPause { id }
            | Self::GoalResume { id }
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
            | Self::SideChatNew { id, .. }
            | Self::SideChatSwitch { id, .. }
            | Self::SideChatClose { id, .. }
            | Self::SideChatPrompt { id, .. }
            | Self::SideChatList { id }
            | Self::SnapCompact { id }
            | Self::Rewind { id, .. }
            | Self::Handoff { id, .. }
            | Self::QueueList { id }
            | Self::QueueCancel { id }
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
            | Self::CodeReviewSnapshot { id }
            | Self::CodeReviewRefresh { id }
            | Self::CodeReviewComment { id, .. }
            | Self::CodeReviewAbort { id }
            | Self::CodeReviewClose { id }
            | Self::CodeReviewFileDiff { id, .. }
            | Self::CloseSession { id }
            | Self::Skill { id, .. }
            | Self::PersonaList { id }
            | Self::PersonaGet { id, .. }
            | Self::PersonaCreate { id, .. }
            | Self::PersonaEdit { id, .. }
            | Self::PersonaRemove { id, .. }
            | Self::PersonaPurge { id, .. }
            | Self::PersonaSelect { id, .. }
            | Self::PersonaClear { id }
            | Self::PersonaCurrent { id } => id.clone(),
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
            Self::SetThinkingLevel { .. } => "set_thinking_level",
            Self::CycleThinkingLevel { .. } => "cycle_thinking_level",
            Self::GetAvailableThinkingLevels { .. } => "get_available_thinking_levels",
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
            Self::SideChatNew { .. } => "side_chat_new",
            Self::SideChatSwitch { .. } => "side_chat_switch",
            Self::SideChatClose { .. } => "side_chat_close",
            Self::SideChatPrompt { .. } => "side_chat_prompt",
            Self::SideChatList { .. } => "side_chat_list",
            Self::SnapCompact { .. } => "snapcompact",
            Self::Rewind { .. } => "rewind",
            Self::Handoff { .. } => "handoff",
            Self::QueueList { .. } => "queue_list",
            Self::QueueCancel { .. } => "queue_cancel",
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
            Self::CodeReviewSnapshot { .. } => "code_review_snapshot",
            Self::CodeReviewRefresh { .. } => "code_review_refresh",
            Self::CodeReviewComment { .. } => "code_review_comment",
            Self::CodeReviewAbort { .. } => "code_review_abort",
            Self::CodeReviewClose { .. } => "code_review_close",
            Self::CodeReviewFileDiff { .. } => "code_review_file_diff",
            Self::Skill { .. } => "skill",
            Self::PersonaList { .. } => "persona_list",
            Self::PersonaGet { .. } => "persona_get",
            Self::PersonaCreate { .. } => "persona_create",
            Self::PersonaEdit { .. } => "persona_edit",
            Self::PersonaRemove { .. } => "persona_remove",
            Self::PersonaPurge { .. } => "persona_purge",
            Self::PersonaSelect { .. } => "persona_select",
            Self::PersonaClear { .. } => "persona_clear",
            Self::PersonaCurrent { .. } => "persona_current",
        }
    }

    pub(crate) const fn is_collab_lifecycle(&self) -> bool {
        matches!(
            self,
            Self::CollabStart { .. } | Self::CollabStatus { .. } | Self::CollabStop { .. }
        )
    }
}

impl RpcCommand {
    pub(crate) const fn runs_inline(&self) -> bool {
        matches!(
            self,
            Self::Abort { .. }
                | Self::AbortRetry { .. }
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
                | Self::SideChatNew { .. }
                | Self::SideChatSwitch { .. }
                | Self::SideChatClose { .. }
                | Self::SideChatPrompt { .. }
                | Self::SideChatList { .. }
                | Self::QueueList { .. }
                | Self::QueueCancel { .. }
                | Self::CollabStart { .. }
                | Self::CollabStatus { .. }
                | Self::CollabStop { .. }
                | Self::CloseSession { .. }
                | Self::RealtimeStop { .. }
                | Self::CodeReviewSnapshot { .. }
                | Self::CodeReviewComment { .. }
                | Self::CodeReviewAbort { .. }
                | Self::CodeReviewClose { .. }
        )
    }

    pub(crate) const fn bypasses_command_slots(&self) -> bool {
        matches!(
            self,
            Self::Abort { .. }
                | Self::AbortRetry { .. }
                | Self::AbortBash { .. }
                | Self::LoopCancel { .. }
                | Self::ProcessSignal { .. }
                | Self::ProcessStop { .. }
                | Self::CloseSession { .. }
                | Self::RealtimeStop { .. }
                | Self::CodeReviewAbort { .. }
                | Self::CodeReviewClose { .. }
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
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
    pub todo_phases: Vec<TodoPhase>,
    pub goal: GoalState,
    pub runtime_settings: pi_coding::RuntimeSettingsState,
}
impl RpcSessionState {
    pub(crate) fn from_application(
        s: ApplicationState,
        runtime_settings: pi_coding::RuntimeSettingsState,
        cwd: &Path,
    ) -> Self {
        Self {
            model: s.model.map(public_model),
            thinking_level: s.thinking_level,
            is_streaming: s.is_streaming,
            is_compacting: s.is_compacting,
            steering_mode: s.steering_mode,
            follow_up_mode: s.follow_up_mode,
            session_file: s.session_file,
            session_id: s.session_id,
            session_name: s.session_name,
            cwd: cwd.to_string_lossy().into_owned(),
            auto_compaction_enabled: s.auto_compaction_enabled,
            message_count: s.message_count,
            pending_message_count: s.pending_message_count,
            todo_phases: s.todo_phases,
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
    pub status: String,
    /// True when the row is an unnamed, tiny, native Pi session whose cwd
    /// sits lexically under the OS temp root (historical test-harness
    /// shape). The Web sidebar hides such rows by default but keeps them
    /// searchable, and loaded/active sessions always stay visible; this is a
    /// recoverable view signal, never a backend deletion. Computed for the
    /// Web AllProjects scope; Current-scope rows always report false (the
    /// current workspace is by definition in active use).
    pub temporary: bool,
}

impl RpcSessionListRow {
    fn from_resume_row(row: crate::resume_catalog::ResumeSelectorRow, temporary: bool) -> Self {
        Self {
            source: row.source.label(),
            session_id: row.session_id,
            name: row.name,
            cwd: row.cwd.to_string_lossy().into_owned(),
            display_time: row.display_time,
            modified_epoch: row.modified_epoch,
            summary: row.summary,
            path: row.path.to_string_lossy().into_owned(),
            size: row.size,
            message_count: row.message_count,
            status: row.status.is_native().then_some("native").unwrap_or("foreign").to_owned(),
            temporary,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct RpcResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub record_type: &'static str,
    pub command: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
impl RpcResponse {
    pub(crate) fn success(id: Option<String>, command: impl Into<String>, data: Option<Value>) -> Self {
        Self {
            id,
            record_type: "response",
            command: command.into(),
            success: true,
            data,
            error: None,
        }
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
            success: false,
            data: None,
            error: Some(error.into()),
        }
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
    #[serde(flatten)]
    request: Value,
}

impl RpcExtensionUiRequest {
    /// Error-notice frame for projection/lag failures, tagged with the owning
    /// session when known.
    pub(crate) fn error_notice(session_id: Option<String>, message: String) -> Self {
        Self {
            record_type: "extension_ui_request",
            id: uuid::Uuid::new_v4().to_string(),
            session_id,
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
pub(crate) struct RpcDispatcher {
    application: Application,
    settings: crate::settings_rpc::SettingsRpcState,
    workflows: crate::workflow_rpc::WorkflowRpcState,
    command_slots: Arc<Semaphore>,
    side_chat: SideChatRpcState,
    code_review: CodeReviewRpcState,
    session_storage: Option<crate::session_run::SessionStorage>,
}

impl RpcDispatcher {
    #[must_use]
    pub(crate) fn new(application: Application) -> Self {
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
            session_storage,
        }
    }

    pub(crate) fn application(&self) -> &Application {
        &self.application
    }
    /// Dispatch a command with this dispatcher's full state. Side-chat and
    /// code-review commands live outside the settings/workflows handler
    /// because their controller state is owned by the RPC session, not by the
    /// application.
    pub(crate) async fn dispatch_inner(&self, command: RpcCommand) -> RpcResponse {
        if matches!(
            command,
            RpcCommand::SideChatNew { .. }
                | RpcCommand::SideChatSwitch { .. }
                | RpcCommand::SideChatClose { .. }
                | RpcCommand::SideChatPrompt { .. }
                | RpcCommand::SideChatList { .. }
        ) {
            return handle_side_chat_command(&self.application, &self.side_chat, command).await;
        }
        if matches!(
            command,
            RpcCommand::CodeReviewOpen { .. }
                | RpcCommand::CodeReviewSnapshot { .. }
                | RpcCommand::CodeReviewRefresh { .. }
                | RpcCommand::CodeReviewComment { .. }
                | RpcCommand::CodeReviewAbort { .. }
                | RpcCommand::CodeReviewClose { .. }
                | RpcCommand::CodeReviewFileDiff { .. }
        ) {
            return handle_code_review_command(&self.application, &self.code_review, command).await;
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
            return RpcResponse::failure(
                id,
                name,
                format!("too many concurrent RPC commands (limit {MAX_CONCURRENT_COMMANDS})"),
            );
        };
        self.dispatch_inner(command).await
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

    /// Whether the RPC-owned code-review controller has an in-flight review
    /// turn (part of the conservative close busy check).
    pub(crate) async fn code_review_busy(&self) -> bool {
        self.code_review.is_streaming().await
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

/// Build the wire snapshot of the whole tab container.
fn side_chat_snapshot(tabs: &crate::side_chat::SideChatTabs) -> Value {
    use crate::side_chat::SideChatRole;
    let tab_values = tabs
        .tabs()
        .map(|(name, controller)| {
            let entries = controller
                .entries()
                .iter()
                .map(|entry| {
                    json!({
                        "role": match entry.role {
                            SideChatRole::User => "user",
                            SideChatRole::Assistant => "assistant",
                            SideChatRole::Tool => "tool",
                            SideChatRole::System => "system",
                        },
                        "text": entry.text,
                        "isError": entry.is_error,
                        "isPartial": entry.is_partial,
                    })
                })
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
                side_chat.with_tabs(app, |tabs| tabs.switch_to(&name)).await?;
                side_chat.snapshot(app).await
            }
            RpcCommand::SideChatClose { name, .. } => {
                let target = match &name {
                    Some(name) => name.clone(),
                    None => side_chat
                        .with_tabs(app, |tabs| Ok(tabs.active_name().to_owned()))
                        .await?,
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
            RpcCommand::SideChatPrompt { message, .. } => {
                let accepted = side_chat
                    .with_tabs(app, |tabs| {
                        if tabs.is_streaming() {
                            return Ok(false);
                        }
                        tabs.submit_prompt(&message);
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
            _ => unreachable!("dispatch_inner only routes side_chat_* commands here"),
        }
    }
    .await;
    match result {
        Ok(data) => RpcResponse::success(id, name, Some(data)),
        Err(e) => RpcResponse::failure(id, name, e.to_string()),
    }
}
/// RPC-owned code-review controller + snapshot state.
///
/// `/code-review` is fullscreen TUI controller state (forked read-only agent
/// + per-hunk threads), so the RPC session owns a lazily-created
/// [`CodeReviewController`] paired with the last loaded [`ReviewSnapshot`].
/// Every code-review command serializes through one mutex; the review agent
/// never touches the main session, mirroring the TUI surface exactly. State
/// is per-`RpcDispatcher` (per-session), so two Web sessions never share a
/// review controller.
struct CodeReviewSession {
    controller: CodeReviewController,
    snapshot: ReviewSnapshot,
    /// Per-path single-file diff cache for the paging RPC. Built lazily on
    /// first `code_review_file_diff` for a path; cleared on open/refresh so a
    /// stale snapshot never serves cached pages from an older capture.
    file_diff_cache: HashMap<String, Arc<FileDiff>>,
}

#[derive(Clone, Default)]
pub(crate) struct CodeReviewRpcState {
    session: Arc<tokio::sync::Mutex<Option<CodeReviewSession>>>,
}

impl CodeReviewRpcState {
    /// Open a bounded HEAD→working-tree or two-revision diff snapshot and
    /// fork a fresh read-only review agent. Any previously open review is
    /// shut down first (clean cutover, no leaked controller). `from`/`to`
    /// must both be present or both absent.
    async fn open(
        &self,
        app: &Application,
        from: Option<String>,
        to: Option<String>,
    ) -> Result<Value> {
        let scope = match (from, to) {
            (None, None) => ReviewScope::WorkingTree,
            (Some(from), Some(to)) => ReviewScope::Revisions { from, to },
            _ => bail!("code_review_open requires `from` and `to` together, or neither"),
        };
        let mut guard = self.session.lock().await;
        let cwd = app.session().cwd().to_path_buf();
        // Git acquisition is blocking and must run off the async runtime.
        let snapshot = tokio::task::spawn_blocking(move || load_review_snapshot_for(&cwd, scope))
            .await
            .map_err(|e| anyhow!("code review snapshot load failed: {e}"))?;
        let mut controller = CodeReviewController::fork_from(app).await?;
        controller.reconcile_snapshot(&snapshot);
        if let Some(mut old) = guard.take() {
            old.controller.shutdown().await;
        }
        *guard = Some(CodeReviewSession {
            controller,
            snapshot,
            file_diff_cache: HashMap::new(),
        });
        let session = guard.as_ref().expect("code review session stored");
        Ok(code_review_projection(&session.snapshot, &session.controller))
    }

    /// Re-project the currently open review snapshot and controller state.
    async fn snapshot(&self) -> Result<Value> {
        let mut guard = self.session.lock().await;
        let Some(session) = guard.as_mut() else {
            bail!("no open code review; call code_review_open first");
        };
        session.controller.poll_events();
        Ok(code_review_projection(&session.snapshot, &session.controller))
    }

    /// Reload the review snapshot (re-run the bounded git acquisition) and
    /// reconcile existing threads onto the new hunk identities.
    async fn refresh(&self, app: &Application) -> Result<Value> {
        let mut guard = self.session.lock().await;
        let Some(session) = guard.as_mut() else {
            bail!("no open code review; call code_review_open first");
        };
        let scope = session.snapshot.scope.clone();
        let cwd = app.session().cwd().to_path_buf();
        let snapshot = tokio::task::spawn_blocking(move || load_review_snapshot_for(&cwd, scope))
            .await
            .map_err(|e| anyhow!("code review snapshot reload failed: {e}"))?;
        session.controller.poll_events();
        session.controller.reconcile_snapshot(&snapshot);
        session.snapshot = snapshot;
        session.file_diff_cache.clear();
        Ok(code_review_projection(&session.snapshot, &session.controller))
    }

    /// Attach a comment to a specific hunk. The full hunk identity must match
    /// the current snapshot exactly; a stale snapshot/path/range/contentHash
    /// is rejected before the review agent is ever prompted.
    async fn comment(
        &self,
        snapshot_id: &str,
        path: &str,
        old_start: u32,
        old_count: u32,
        new_start: u32,
        new_count: u32,
        content_hash: &str,
        comment: &str,
    ) -> Result<Value> {
        let mut guard = self.session.lock().await;
        let Some(session) = guard.as_mut() else {
            bail!("no open code review; call code_review_open first");
        };
        session.controller.poll_events();
        let (file, hunk) = resolve_comment_target(
            &session.snapshot,
            snapshot_id,
            path,
            old_start,
            old_count,
            new_start,
            new_count,
            content_hash,
        )?;
        let accepted = session
            .controller
            .submit_comment(&session.snapshot, file, hunk, comment);
        if !accepted {
            bail!("code review comment was not accepted (a turn may be streaming or the comment is empty)");
        }
        Ok(code_review_projection(&session.snapshot, &session.controller))
    }

    /// Abort the in-flight review agent turn for the active hunk.
    async fn abort(&self) -> Result<Value> {
        let mut guard = self.session.lock().await;
        let Some(session) = guard.as_mut() else {
            bail!("no open code review; call code_review_open first");
        };
        session.controller.abort().await;
        session.controller.poll_events();
        Ok(code_review_projection(&session.snapshot, &session.controller))
    }

    /// Close the open review: shut down the review agent and drop state.
    async fn close(&self) -> Result<Value> {
        let mut guard = self.session.lock().await;
        let Some(mut session) = guard.take() else {
            bail!("no open code review; call code_review_open first");
        };
        session.controller.shutdown().await;
        Ok(json!({"closed": true}))
    }

    /// Shut down any open review controller (RPC session exit / listener
    /// shutdown). Never errors: a missing review is a no-op.
    async fn shutdown(&self) {
        let mut guard = self.session.lock().await;
        if let Some(mut session) = guard.take() {
            session.controller.shutdown().await;
        }
    }

    /// Whether a review turn is in flight (close busy check). Drains pending
    /// events first so a just-finished turn is not misclassified as busy.
    async fn is_streaming(&self) -> bool {
        let mut guard = self.session.lock().await;
        let Some(session) = guard.as_mut() else {
            return false;
        };
        session.controller.poll_events();
        session.controller.is_streaming()
    }
    /// Load one bounded page of a single file's full diff. The `snapshotId`
    /// must match the live snapshot (stale guard), `path` must be one of the
    /// snapshot's files (containment), and `cursor` is a 0-based line offset.
    /// The full per-file diff is loaded off the async runtime on first request
    /// for a path and cached on the session; subsequent pages slice from the
    /// cache without re-running git. The cache is cleared on open/refresh. If
    /// the snapshot is refreshed while a load is in flight, the loaded diff is
    /// discarded (not cached) and the caller sees a stale-snapshot error.
    async fn file_diff(
        &self,
        app: &Application,
        snapshot_id: &str,
        path: &str,
        cursor: usize,
        max_lines: Option<usize>,
    ) -> Result<Value> {
        let normalized = crate::code_review::normalize_repo_path(path);
        // First pass: validate snapshot id + path containment under the lock
        // and return a cache hit if present. The lock is released before the
        // spawn_blocking git call so snapshot polls are not blocked.
        let (file_clone, scope, cached) = {
            let mut guard = self.session.lock().await;
            let Some(session) = guard.as_mut() else {
                bail!("no open code review; call code_review_open first");
            };
            if session.snapshot.snapshot_id != snapshot_id {
                bail!("code_review_file_diff targets a stale snapshot; refresh the code review");
            }
            let file = session
                .snapshot
                .files
                .iter()
                .find(|f| f.path == normalized)
                .ok_or_else(|| anyhow!("code_review_file_diff targets an unknown file {path:?}"))?;
            (
                file.clone(),
                session.snapshot.scope.clone(),
                session.file_diff_cache.get(&normalized).cloned(),
            )
        };

        let arc = if let Some(cached) = cached {
            cached
        } else {
            let cwd = app.session().cwd().to_path_buf();
            let file_for_load = file_clone.clone();
            let diff = tokio::task::spawn_blocking(move || FileDiff::load(&cwd, &scope, &file_for_load))
                .await
                .map_err(|e| anyhow!("code review file diff load failed: {e}"))?;
            let arc = Arc::new(diff);
            // Re-lock to store; discard if the snapshot changed meanwhile so a
            // stale capture never poisons the cache.
            let mut guard = self.session.lock().await;
            if let Some(session) = guard.as_mut() {
                if session.snapshot.snapshot_id == snapshot_id {
                    session.file_diff_cache.insert(normalized.clone(), arc.clone());
                }
            }
            arc
        };

        let max = max_lines.unwrap_or(MAX_FILE_PAGE_LINES);
        let page = arc.slice_page(snapshot_id, cursor, max).map_err(anyhow::Error::msg)?;
        Ok(file_diff_projection(&page))
    }
}

/// Validate that a comment request targets a real hunk in the current
/// snapshot with a matching identity. Returns the resolved `(file, hunk)` so
/// the caller can submit the comment. A mismatched `snapshotId`, unknown
/// path/range, or changed `contentHash` is rejected as stale — this is the
/// deterministic core (no agent, no network) exercised by unit tests.
fn resolve_comment_target<'a>(
    snapshot: &'a ReviewSnapshot,
    snapshot_id: &str,
    path: &str,
    old_start: u32,
    old_count: u32,
    new_start: u32,
    new_count: u32,
    content_hash: &str,
) -> Result<(&'a DiffFile, &'a DiffHunk)> {
    if snapshot.snapshot_id != snapshot_id {
        bail!("comment targets a stale snapshot; refresh the code review");
    }
    let file = snapshot
        .files
        .iter()
        .find(|file| file.path == path)
        .ok_or_else(|| anyhow!("comment targets an unknown file {path:?}"))?;
    let hunk = file
        .hunks
        .iter()
        .find(|hunk| {
            hunk.old_start == old_start
                && hunk.old_count == old_count
                && hunk.new_start == new_start
                && hunk.new_count == new_count
        })
        .ok_or_else(|| anyhow!("comment targets an unknown hunk in {path:?}"))?;
    let identity = snapshot.hunk_identity(file, hunk);
    if identity.content_hash != content_hash {
        bail!("comment targets a stale hunk; the diff content has changed");
    }
    Ok((file, hunk))
}

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
                "insertions": file.insertions,
                "deletions": file.deletions,
                "truncated": file.truncated,
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
            json!({
                "identity": hunk_identity_projection(&thread.identity),
                "comments": thread.comments.iter().map(|comment| json!({
                    "role": review_role_str(comment.role),
                    "text": comment.text,
                    "partial": comment.partial,
                })).collect::<Vec<_>>(),
                "streamingText": thread.streaming_text,
                "error": thread.error,
                "stale": thread.stale,
            })
        })
        .collect::<Vec<_>>();

    json!({
        "comparisonLabel": snapshot.comparison_label(),
        "snapshotId": snapshot.snapshot_id,
        "truncated": snapshot.truncated,
        "error": snapshot.error,
        "totalInsertions": snapshot.total_insertions(),
        "totalDeletions": snapshot.total_deletions(),
        "files": files,
        "threads": threads,
        "isStreaming": controller.is_streaming(),
        "activeHunk": controller.active_hunk().map(hunk_identity_projection),
    })
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

/// Wire projection of a single [`FileDiffPage`]. Lines preserve their order
/// across pages; `nextCursor` is absent on the final page. Output stays well
/// under the WS frame limit — the page is bounded by line count and bytes in
/// [`FileDiff::slice_page`].
fn file_diff_projection(page: &FileDiffPage) -> Value {
    let lines = page
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
    let mut out = json!({
        "snapshotId": page.snapshot_id,
        "path": page.path,
        "binary": page.binary,
        "status": page.status.label(),
        "lines": lines,
        "cursor": page.cursor,
        "hasMore": page.has_more,
        "totalLines": page.total_lines,
        "truncated": page.truncated,
    });
    if let Some(previous_path) = &page.previous_path {
        out["previousPath"] = json!(previous_path);
    }
    if let Some(next_cursor) = page.next_cursor {
        out["nextCursor"] = json!(next_cursor);
    }
    out
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
            RpcCommand::CodeReviewOpen { from, to, .. } => {
                code_review.open(app, from, to).await
            }
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
                ..
            } => {
                code_review
                    .comment(
                        &snapshot_id,
                        &path,
                        old_start,
                        old_count,
                        new_start,
                        new_count,
                        &content_hash,
                        &comment,
                    )
                    .await
            }
            RpcCommand::CodeReviewAbort { .. } => code_review.abort().await,
            RpcCommand::CodeReviewClose { .. } => code_review.close().await,
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
            _ => unreachable!("dispatch_inner only routes code_review_* commands here"),
        }
    }
    .await;
    match result {
        Ok(data) => RpcResponse::success(id, name, Some(data)),
        Err(e) => RpcResponse::failure(id, name, e.to_string()),
    }
}

pub(crate) fn project_application_event(event: ApplicationEvent) -> Result<Value> {
    match event {
        ApplicationEvent::Agent(pi_agent::AgentEvent::MessageStart { message }) => {
            Ok(serde_json::to_value(ApplicationEvent::Agent(
                pi_agent::AgentEvent::MessageStart {
                    message: public_message(message),
                },
            ))?)
        }
        ApplicationEvent::Agent(pi_agent::AgentEvent::MessageEnd { message }) => {
            Ok(serde_json::to_value(ApplicationEvent::Agent(
                pi_agent::AgentEvent::MessageEnd {
                    message: public_message(message),
                },
            ))?)
        }
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
        ..
    } = session_run::build_session(cli).await?;
    run_with_io_and_extension_ui(
        application,
        extension_ui.expect("RPC has UI adapter"),
        tokio::io::stdin(),
        io::stdout(),
    )
    .await
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
    extension_ui.set_canonical_queries_supported(false);
    let mut events = application.subscribe();
    let mut ui_events = extension_ui.subscribe();
    let (lines_tx, mut lines_rx) = mpsc::channel(JSONL_CHANNEL_CAPACITY);
    let reader = tokio::spawn(read_jsonl(input, lines_tx));
    let output = Arc::new(StdMutex::new(output));
    let dispatcher = RpcDispatcher::new(application.clone());
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
                            // The stdio RPC transport keeps single-Application
                            // semantics: sessionId routing exists only on the
                            // Web control plane manager.
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
                                let response = RpcResponse::failure(
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
    application.cleanup().await;
    Ok(())
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
            ))
        }
    };
    if session_id.is_some()
        && let Some(object) = value.as_object_mut()
    {
        object.remove("sessionId");
    }
    let Some(command) = value.get("type").and_then(Value::as_str).map(str::to_owned) else {
        return Err(RpcResponse::failure(
            id,
            "parse",
            "Failed to parse command: missing string field `type`",
        ));
    };
    if command == "extension_ui_response" {
        return serde_json::from_value(value)
            .map(RpcInput::ExtensionUiResponse)
            .map_err(|e| {
                RpcResponse::failure(id, command, format!("Invalid extension UI response: {e}"))
            });
    }
    // Workflow commands use a deny_unknown_fields wire schema.
    if crate::workflow_rpc::WorkflowRpcCommand::is_workflow_type(&command) {
        return match crate::workflow_rpc::parse_workflow_command(value) {
            Ok(workflow) => Ok(RpcInput::Command {
                command: rpc_command_from_workflow(workflow),
                session_id,
            }),
            Err(error) => Err(RpcResponse::failure(
                id,
                command,
                format!("Invalid command: {error}"),
            )),
        };
    }
    serde_json::from_value(value)
        .map(|command| RpcInput::Command { command, session_id })
        .map_err(|e| {
            let message = if e.to_string().starts_with("unknown variant") {
                format!("Unknown command: {command}")
            } else {
                format!("Invalid command: {e}")
            };
            RpcResponse::failure(id, command, message)
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
        ExtensionUiEvent::OverlayRowsChanged {
            instance,
            id,
            rows,
        } => json!({
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
        request,
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
    match handle_command_inner(app, settings, workflows, session_storage, c).await {
        Ok(data) => RpcResponse::success(id, name, data),
        Err(e) => RpcResponse::failure(id, name, e.to_string()),
    }
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
            ..
        } => Ok(Some(serde_json::to_value(
            app.goal_create(objective, token_budget)?,
        )?)),
        RpcCommand::GoalGet { .. } => Ok(Some(serde_json::to_value(app.goal_state())?)),
        RpcCommand::GoalPause { .. } => Ok(Some(serde_json::to_value(app.goal_pause()?)?)),
        RpcCommand::GoalResume { .. } => Ok(Some(serde_json::to_value(app.goal_resume()?)?)),
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
        } => {
            app.prompt(message, images, streaming_behavior).await?;
            Ok(None)
        }
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
        RpcCommand::NewSession { parent_session, .. } => {
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
                app.runtime_settings_state(),
                app.session().cwd(),
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
        RpcCommand::SessionList { scope, .. } => {
            let session = app.session();
            let catalog = match scope {
                RpcSessionListScope::Current => pi_coding::SessionCatalog::from_env()?
                    .with_native_session_root(session.session_dir()),
                RpcSessionListScope::AllProjects => session_storage
                    .ok_or_else(|| anyhow!("all-project session catalog is unavailable outside the Web listener"))?
                    .catalog()?,
            };
            let cwd_scope = matches!(scope, RpcSessionListScope::Current)
                .then(|| session.cwd().to_path_buf());
            let rows = crate::resume_catalog::load_resume_catalog(
                &catalog,
                &crate::resume_catalog::ResumeCatalogRequest {
                    sources: match scope {
                        RpcSessionListScope::Current => {
                            crate::resume_catalog::effective_resume_sources(app)
                        }
                        RpcSessionListScope::AllProjects => {
                            crate::resume_catalog::web_resume_sources(app)
                        }
                    },
                    cwd_scope,
                    ..crate::resume_catalog::ResumeCatalogRequest::default()
                },
            )?;
            let rows = match scope {
                RpcSessionListScope::Current => rows
                    .rows
                    .into_iter()
                    .map(|row| (row, false))
                    .collect::<Vec<_>>(),
                RpcSessionListScope::AllProjects => {
                    let rows = crate::resume_catalog::filter_web_noise_rows(
                        crate::resume_catalog::coalesce_web_import_rows(rows.rows),
                    );
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
            let live_identity = session.recorder_info().map(|(id, path)| {
                (id, crate::modes::session_runtime_manager::canonical_session_path(&path))
            });
            let sessions = rows
                .into_iter()
                .map(|(row, temporary)| {
                    let mut row = RpcSessionListRow::from_resume_row(row, temporary);
                    let row_path = crate::modes::session_runtime_manager::canonical_session_path(
                        Path::new(&row.path),
                    );
                    if live_name.is_some()
                        && live_identity.as_ref().is_some_and(|(id, path)| {
                            id == &row.session_id && path == &row_path
                        })
                    {
                        row.name = live_name.clone();
                    }
                    row
                })
                .collect::<Vec<_>>();
            Ok(Some(json!({"sessions": sessions})))
        }
        RpcCommand::ExportHtml { output_path, .. } => Ok(Some(
            json!({"path":app.export_html(output_path.as_deref().map(Path::new))?.to_string_lossy()}),
        )),
        RpcCommand::SwitchSession { session_path, .. } => {
            let outcome = app.switch_session(Path::new(&session_path)).await?;
            if !outcome.cancelled {
                crate::session_run::rebind_workflows_for_active_session(&app)
                    .await
                    .context("rebinding workflow storage after session switch")?;
            }
            Ok(Some(json!({"cancelled":outcome.cancelled})))
        }
        RpcCommand::Fork { entry_id, .. } => {
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
        RpcCommand::Clone { .. } => {
            let outcome = app.clone_session().await?;
            if !outcome.cancelled {
                crate::session_run::rebind_workflows_for_active_session(&app)
                    .await
                    .context("rebinding workflow storage after session clone")?;
            }
            Ok(Some(json!({"cancelled":outcome.cancelled})))
        }
        RpcCommand::GetForkMessages { .. } => Ok(Some(json!({"messages":app.fork_messages()?}))),
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
        RpcCommand::GetTree { .. } => Ok(Some(serde_json::to_value(app.session_tree()?)?)),
        RpcCommand::GetLastAssistantText { .. } => {
            Ok(Some(json!({"text":app.last_assistant_text()})))
        }
        RpcCommand::SetSessionName { name, .. } => {
            if name.trim().is_empty() {
                bail!("Session name cannot be empty")
            }
            app.set_session_name(name.trim())?;
            Ok(None)
        }
        RpcCommand::GetMessages { .. } => {
            let messages: Vec<Message> = app.messages().into_iter().map(public_message).collect();
            Ok(Some(json!({"messages":messages})))
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
            Ok(Some(json!({ "name": name, "created": true, "message": message })))
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
            Ok(Some(json!({ "name": name, "edited": true, "message": message })))
        }
        RpcCommand::PersonaRemove { name, confirm, .. } => {
            if !confirm {
                bail!(
                    "persona_remove requires confirm: true (mirrors /persona remove <name> --yes); refusing without confirmation"
                );
            }
            let message = crate::interactive_commands::execute_interactive_persona_command(
                app,
                crate::interactive_commands::InteractivePersonaCommand::Remove { name: name.clone() },
            )
            .await?;
            Ok(Some(json!({ "name": name, "removed": true, "message": message })))
        }
        RpcCommand::PersonaPurge { name, confirm, .. } => {
            if !confirm {
                bail!(
                    "persona_purge requires confirm: true (mirrors /persona remove <name> --purge --yes); refusing without confirmation"
                );
            }
            let message = crate::interactive_commands::execute_interactive_persona_command(
                app,
                crate::interactive_commands::InteractivePersonaCommand::Purge { name: name.clone() },
            )
            .await?;
            Ok(Some(json!({ "name": name, "purged": true, "message": message })))
        }
        RpcCommand::PersonaSelect { name, .. } => {
            let message = crate::interactive_commands::execute_interactive_persona_command(
                app,
                crate::interactive_commands::InteractivePersonaCommand::Select { name: name.clone() },
            )
            .await?;
            Ok(Some(json!({ "name": name, "preferred": true, "message": message })))
        }
        RpcCommand::PersonaClear { .. } => {
            let message = crate::interactive_commands::execute_interactive_persona_command(
                app,
                crate::interactive_commands::InteractivePersonaCommand::Clear,
            )
            .await?;
            Ok(Some(json!({ "preferred": serde_json::Value::Null, "message": message })))
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
        RpcCommand::SetTodos { workflow_id, phases, .. } => {
            let result = match workflow_id {
                Some(workflow_id) => app.set_workflow_todos(&pi_coding::WorkflowId::new(workflow_id), phases)?,
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
        } => Ok(Some(workflows.dispatch(
            crate::workflow_rpc::WorkflowRpcCommand::WorkflowCreate {
                id,
                name,
                objective,
            },
        ).await?)),
        RpcCommand::WorkflowList { id } => Ok(Some(workflows.dispatch(
            crate::workflow_rpc::WorkflowRpcCommand::WorkflowList { id },
        ).await?)),
        RpcCommand::WorkflowGet {
            id,
            workflow_id,
            name,
        } => Ok(Some(workflows.dispatch(
            crate::workflow_rpc::WorkflowRpcCommand::WorkflowGet {
                id,
                workflow_id,
                name,
            },
        ).await?)),
        RpcCommand::WorkflowDetail {
            id,
            workflow_id,
            name,
        } => Ok(Some(workflows.dispatch(
            crate::workflow_rpc::WorkflowRpcCommand::WorkflowDetail {
                id,
                workflow_id,
                name,
            },
        ).await?)),
        RpcCommand::WorkflowPause { id, workflow_id } => Ok(Some(workflows.dispatch(
            crate::workflow_rpc::WorkflowRpcCommand::WorkflowPause { id, workflow_id },
        ).await?)),
        RpcCommand::WorkflowResume { id, workflow_id } => Ok(Some(workflows.dispatch(
            crate::workflow_rpc::WorkflowRpcCommand::WorkflowResume { id, workflow_id },
        ).await?)),
        RpcCommand::WorkflowCancel { id, workflow_id } => Ok(Some(workflows.dispatch(
            crate::workflow_rpc::WorkflowRpcCommand::WorkflowCancel { id, workflow_id },
        ).await?)),
        RpcCommand::WorkflowIntegrate { id, workflow_id } => Ok(Some(workflows.dispatch(
            crate::workflow_rpc::WorkflowRpcCommand::WorkflowIntegrate { id, workflow_id },
        ).await?)),
        RpcCommand::WorkflowRemove { id, workflow_id } => Ok(Some(workflows.dispatch(
            crate::workflow_rpc::WorkflowRpcCommand::WorkflowRemove { id, workflow_id },
        ).await?)),
        RpcCommand::SnapCompact { .. } => Ok(Some(serde_json::to_value(
            app.compact_snap().await?,
        )?)),
        RpcCommand::Rewind {
            index,
            checkpoint,
            ..
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
                app.generate_handoff()
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
        RpcCommand::QueueCancel { .. } => {
            let (steering, follow_up) = app.drain_queued_messages().await;
            Ok(Some(json!({"cancelled": steering.len() + follow_up.len()})))
        }
        RpcCommand::JobList { .. } => {
            let Some(runtime) = app.orchestration_runtime() else {
                return Ok(Some(json!({
                    "enabled": false,
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
        RpcCommand::AgentHistory { agent_id, lines, .. } => {
            let runtime = app
                .orchestration_runtime()
                .ok_or_else(|| anyhow!("orchestration is not enabled in this session"))?;
            let lines = lines.unwrap_or(DEFAULT_RPC_HISTORY_LINES);
            if !(1..=pi_coding::MAX_HISTORY_LINES).contains(&lines) {
                bail!("lines must be between 1 and {}", pi_coding::MAX_HISTORY_LINES);
            }
            let text = runtime.read_child_history(&agent_id, lines)?;
            Ok(Some(json!({"agentId": agent_id, "text": text})))
        }
        // Side-chat commands never reach the settings/workflows handler:
        // RpcDispatcher::dispatch_inner routes them to
        // handle_side_chat_command with the RPC-owned tab container.
        RpcCommand::SideChatNew { .. }
        | RpcCommand::SideChatSwitch { .. }
        | RpcCommand::SideChatClose { .. }
        | RpcCommand::SideChatPrompt { .. }
        | RpcCommand::SideChatList { .. } => {
            unreachable!("side_chat_* commands must be routed by dispatch_inner")
        }
        // Code-review commands are RPC-session controller state and are
        // routed by dispatch_inner to handle_code_review_command; the
        // settings/workflows handler never sees them.
        RpcCommand::CodeReviewOpen { .. }
        | RpcCommand::CodeReviewSnapshot { .. }
        | RpcCommand::CodeReviewRefresh { .. }
        | RpcCommand::CodeReviewComment { .. }
        | RpcCommand::CodeReviewAbort { .. }
        | RpcCommand::CodeReviewClose { .. }
        | RpcCommand::CodeReviewFileDiff { .. } => {
            unreachable!("code_review_* commands must be routed by dispatch_inner")
        }
        // The listen transport intercepts collaboration lifecycle commands;
        // stdio RPC has no room registry or reachable listener origin.
        RpcCommand::CollabStart { .. }
        | RpcCommand::CollabStatus { .. }
        | RpcCommand::CollabStop { .. } => {
            bail!("collaboration room commands require the listen control plane")
        }
        // Codex Live realtime proxy commands. The browser owns WebRTC and the
        // `oai-events` data channel; these only relay signaling/session HTTP
        // calls to CLIProxyAPI with the server-held API key (never exposed to
        // the frontend).
        RpcCommand::RealtimeCreateCall { sdp_offer, .. } => {
            let live = app.runtime_settings().live.clone();
            Ok(Some(realtime_create_call(&live, &sdp_offer).await?))
        }
        RpcCommand::RealtimeStop { .. } => Ok(Some(json!({"stopped": true}))),
        // STT voice proxy (hold-to-talk): the browser sends ONLY the bounded
        // recording; the endpoint URL and bearer key stay in the server-held
        // live settings (never accepted from the client, never exposed to the
        // frontend).
        RpcCommand::SttTranscribe { audio_base64, mime_type, .. } => {
            let live = app.runtime_settings().live.clone();
            Ok(Some(stt_transcribe(&live, &audio_base64, &mime_type).await?))
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
        bail!(
            "`Settings.live.realtimeBaseUrl` must not contain a username or password"
        );
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        bail!(
            "`Settings.live.realtimeBaseUrl` must not contain a query or fragment"
        );
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

/// Builds the CLIProxyAPI realtime session payload — the `session` object sent
/// in the create-call POST. Codex realtime rejects `session.model` with 400
/// (`Field session.model is not allowed for this Codex realtime session`), so
/// the create-call session is the Quicksilver shape WITHOUT `model`:
/// `{ type, audio: { input: { format: { type, rate } }, output: { voice } } }`.
/// This is NOT the same object the web side sends as `session.update`: the
/// data-channel shape (web `buildRealtimeSessionConfig`) additionally carries
/// the configured `realtimeModel`, which is accepted there. `voice` keeps its
/// correct nesting under `audio.output.voice` and is passed through unchanged
/// (custom aliases are not hard-rejected here; the upstream surfaces any
/// mismatch as a diagnostic error).
fn realtime_session_payload(settings: &pi_coding::LiveRuntimeSettings) -> Value {
    json!({
        "type": "quicksilver",
        "audio": {
            "input": {
                "format": {
                    "type": "audio/pcm",
                    "rate": 24000,
                },
            },
            "output": {
                "voice": settings.voice,
            },
        },
    })
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
async fn read_bounded_response_body(response: &mut reqwest::Response) -> Result<Vec<u8>> {
    if let Some(len) = response.content_length() {
        if len > REALTIME_BODY_LIMIT as u64 {
            bail!(
                "realtime_create_call: upstream response body exceeds the {} KiB bound",
                REALTIME_BODY_LIMIT / 1024
            );
        }
    }
    let mut buf = Vec::new();
    loop {
        let chunk = response
            .chunk()
            .await
            .context("realtime_create_call: reading upstream response body failed")?;
        let Some(chunk) = chunk else {
            break;
        };
        // checked_add keeps the bound arithmetic overflow-safe even in
        // theory (the cap is far below usize::MAX in practice).
        let total = buf
            .len()
            .checked_add(chunk.len())
            .context("realtime_create_call: upstream response body length overflow")?;
        if total > REALTIME_BODY_LIMIT {
            bail!(
                "realtime_create_call: upstream response body exceeds the {} KiB bound",
                REALTIME_BODY_LIMIT / 1024
            );
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Proxies `POST {realtimeBaseUrl}/v1/realtime/calls` as a JSON POST whose
/// body is exactly `{ "sdp": <offer>, "session": <create-call session> }`
/// with `Content-Type: application/json`. The create-call session is the
/// Quicksilver shape WITHOUT `model` — Codex realtime rejects `session.model`
/// with 400 — so the configured model is NOT sent here; it rides the web
/// `session.update` frame over the `oai-events` data channel instead (where
/// the model field IS accepted). Returns the bare SDP answer and call id as
/// `{"sdp": ..., "callId": ...}` for the browser's
/// `RTCPeerConnection` and `oai-events` data channel. Transport, status, and
/// body errors use a fixed endpoint name and redact credential-like content;
/// the configured base/URL and API key never surface, and both the SDP offer
/// and the upstream answer are bounded to `REALTIME_BODY_LIMIT`.
async fn realtime_create_call(
    settings: &pi_coding::LiveRuntimeSettings,
    sdp_offer: &str,
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
    let client = reqwest::Client::builder()
        .timeout(REALTIME_PROXY_TIMEOUT)
        .build()
        .context("building realtime proxy client")?;
    // Exact wire body: `{sdp, session}` and nothing else. The session object
    // carries only the quicksilver create-call fields (type + audio, voice
    // nested under audio.output) — never `model`, which Codex rejects; the
    // configured model is sent via the data-channel session.update instead.
    let body = json!({
        "sdp": sdp_offer,
        "session": realtime_session_payload(settings),
    });
    let mut response = match client
        .post(&url)
        .bearer_auth(settings.realtime_api_key.trim())
        .header("OpenAI-Alpha", "quicksilver=v2")
        .json(&body)
        .send()
        .await
    {
        Ok(response) => response,
        Err(err) => {
            // Fixed endpoint name; reqwest embeds the request URL in its own
            // error text, so the full chain is redacted before surfacing.
            let detail = redact_realtime_diagnostics(settings, &url, &format!("{err}"));
            bail!("realtime_create_call: POST to the realtime create-call endpoint failed: {detail}");
        }
    };
    // Capture the Location header before consuming the body — the call id
    // lives there, not in the (bare SDP) response body.
    let location = response.headers().get(reqwest::header::LOCATION).cloned();
    let status = response.status();
    let response_body = read_bounded_response_body(&mut response).await?;
    if !status.is_success() {
        let detail = redact_realtime_diagnostics(
            settings,
            &url,
            &String::from_utf8_lossy(&response_body),
        );
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
async fn stt_transcribe(
    settings: &pi_coding::LiveRuntimeSettings,
    audio_base64: &str,
    mime_type: &str,
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
    let stt = pi_coding::live::SttClient::new().context("stt_transcribe: building STT client")?;
    let mut validated = settings.clone();
    validated.mode = "stt".to_owned();
    let text = stt.transcribe(&validated, &capture).await?;
    Ok(json!({ "text": text }))
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
async fn available_models() -> Result<Vec<Model>> {
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
                .filter(crate::models_config::has_configured_auth),
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
    use futures_util::FutureExt as _;
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
            json!({"type":"new_session","parentSession":"p"}),
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
            json!({"type":"switch_session","sessionPath":"s"}),
            json!({"type":"fork","entryId":"e"}),
            json!({"type":"clone"}),
            json!({"type":"get_fork_messages"}),
            json!({"type":"get_entries","since":"e"}),
            json!({"type":"get_tree"}),
            json!({"type":"get_last_assistant_text"}),
            json!({"type":"set_session_name","name":"n"}),
            json!({"type":"get_messages"}),
            json!({"type":"get_commands"}),
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
            json!({"type":"side_chat_new","name":"research"}),
            json!({"type":"side_chat_switch","name":"research"}),
            json!({"type":"side_chat_close"}),
            json!({"type":"side_chat_close","name":"research"}),
            json!({"type":"side_chat_prompt","message":"summarize"}),
            json!({"type":"side_chat_list"}),
            json!({"type":"snapcompact"}),
            json!({"type":"rewind","index":4}),
            json!({"type":"rewind","checkpoint":"milestone"}),
            json!({"type":"handoff"}),
            json!({"type":"handoff","prose":true}),
            json!({"type":"queue_list"}),
            json!({"type":"queue_cancel"}),
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
            json!({"type":"code_review_snapshot"}),
            json!({"type":"code_review_refresh"}),
            json!({"type":"code_review_comment","snapshotId":"s1","path":"a.rs","oldStart":1,"oldCount":1,"newStart":1,"newCount":1,"contentHash":"h","comment":"looks good"}),
            json!({"type":"code_review_abort"}),
            json!({"type":"code_review_close"}),
            json!({"type":"code_review_file_diff","snapshotId":"s1","path":"a.rs"}),
            json!({"type":"code_review_file_diff","snapshotId":"s1","path":"a.rs","cursor":100,"maxLines":50}),
            json!({"type":"skill","name":"research"}),
            json!({"type":"persona_list"}),
            json!({"type":"persona_get","name":"mentor"}),
            json!({"type":"persona_create","name":"guide","content":"---\nname: guide\ndescription: g\n---\nprompt"}),
            json!({"type":"persona_edit","name":"mentor","content":"---\nname: mentor\ndescription: m\n---\nprompt"}),
            json!({"type":"persona_remove","name":"mentor","confirm":true}),
            json!({"type":"persona_purge","name":"mentor","confirm":true}),
            json!({"type":"persona_select","name":"mentor"}),
            json!({"type":"persona_clear"}),
            json!({"type":"persona_current"}),
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
                command: RpcCommand::SessionList { scope: RpcSessionListScope::Current, .. },
                ..
            }
        ));
        let all = parse_input(br#"{"type":"session_list","scope":"all_projects"}"#)
            .expect("all projects scope");
        assert!(matches!(
            all,
            RpcInput::Command {
                command: RpcCommand::SessionList { scope: RpcSessionListScope::AllProjects, .. },
                ..
            }
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
            json!({"type":"loop_cancel","taskId":"loop-1"}),
            json!({"type":"process_signal","processId":"00000000-0000-7000-8000-000000000000","signal":"SIGTERM"}),
            json!({"type":"process_stop","processId":"00000000-0000-7000-8000-000000000000"}),
            json!({"type":"code_review_abort"}),
            json!({"type":"code_review_close"}),
        ] {
            let command = parse_command(fixture.clone());
            assert!(
                command.bypasses_command_slots(),
                "recovery command must bypass saturated slots: {fixture}"
            );
        }

        for fixture in [
            json!({"type":"settings_apply","draftId":"draft"}),
            json!({"type":"workflow_create","name":"ship","objective":"bounded"}),
            json!({"type":"process_wait","processId":"00000000-0000-7000-8000-000000000000"}),
            json!({"type":"code_review_open"}),
            json!({"type":"code_review_open","from":"HEAD","to":"feature"}),
            json!({"type":"code_review_refresh"}),
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
        // reads and safety/teardown); open/refresh take a slot (spawn_blocking git).
        let snapshot = parse_command(json!({"type":"code_review_snapshot"}));
        let comment = parse_command(json!({"type":"code_review_comment","snapshotId":"s","path":"a","oldStart":1,"oldCount":1,"newStart":1,"newCount":1,"contentHash":"h","comment":"x"}));
        let abort = parse_command(json!({"type":"code_review_abort"}));
        let close = parse_command(json!({"type":"code_review_close"}));
        let open = parse_command(json!({"type":"code_review_open"}));
        let refresh = parse_command(json!({"type":"code_review_refresh"}));
        let file_diff = parse_command(json!({"type":"code_review_file_diff","snapshotId":"s","path":"a"}));
        assert!(snapshot.runs_inline() && comment.runs_inline() && abort.runs_inline() && close.runs_inline());
        // file_diff re-runs scoped git via spawn_blocking: it takes a slot like
        // open/refresh (NOT inline, NOT a bypass).
        assert!(!open.runs_inline() && !refresh.runs_inline() && !file_diff.runs_inline());
        assert!(abort.bypasses_command_slots() && close.bypasses_command_slots());
        assert!(!open.bypasses_command_slots() && !refresh.bypasses_command_slots() && !snapshot.bypasses_command_slots() && !file_diff.bypasses_command_slots());
    }
    #[test]
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
        let Ok(RpcInput::Command { command: RpcCommand::SetTodos { phases, .. }, .. }) = parse_input(&first) else {
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
                blocked_by: Vec::new(),
                agent: None,
            }],
        }];
        let updated = pi_coding::ApplicationEvent::TodoUpdated {
            phases: phases.clone(),
            completed_tasks: vec![TodoCompletionTransition {
                phase: "Plan".into(),
                content: "x".into(),
            }],
        };
        let v: Value = serde_json::from_str(&serde_json::to_string(&updated).unwrap()).unwrap();
        assert_eq!(v["type"], "todo_updated");
        assert_eq!(v["phases"][0]["name"], "Plan");
        assert_eq!(v["phases"][0]["tasks"][0]["status"], "in_progress");
        assert_eq!(v["phases"][0]["tasks"][0]["id"], "task-x");
        assert_eq!(v["phases"][0]["tasks"][0]["dependsOn"], json!([]));
        assert_eq!(v["phases"][0]["tasks"][0]["ready"], true);
        assert_eq!(v["phases"][0]["tasks"][0]["blockedBy"], json!([]));
        assert_eq!(v["completed_tasks"][0]["phase"], "Plan");
        let reminder = pi_coding::ApplicationEvent::TodoReminder { phases };
        let v: Value = serde_json::from_str(&serde_json::to_string(&reminder).unwrap()).unwrap();
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
        let reg = register_faux_provider(FauxProviderOptions {
            api: api.into(),
            provider: "faux".into(),
            models: vec![model.clone()],
            chunk_size: 4,
        });
        reg.set_responses(vec![FauxResponse::text("ok")]);
        let cwd = tempfile::tempdir().expect("tempdir");
        let session = pi_coding::Session::new(pi_coding::SessionOptions {
            model,
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "faux".into(),
            compaction: None,
            stream_options: Default::default(),
            tools: None,
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
        })
        .expect("session");
        let app = Application::new(session).await;
        reg.unregister();
        app
    }
    async fn build_compact_app(
        model_id: &str,
        responses: Vec<pi_ai::providers::FauxResponse>,
    ) -> Application {
        let mut model = Model::default();
        model.id = model_id.into();
        model.name = model_id.into();
        model.api = format!("{model_id}-api");
        model.provider = "faux".into();
        model.base_url = "http://localhost:0".into();
        let responses = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from(responses)));
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
        let session = pi_coding::Session::new(pi_coding::SessionOptions {
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
        })
        .expect("session");
        let mut assistant = pi_ai::AssistantMessage::pending(&session.model().expect("model"));
        assistant.content = vec![ContentBlock::text("old answer")];
        assistant.stop_reason = pi_ai::StopReason::Stop;
        assistant.timestamp = 2;
        session
            .load_history(vec![
                Message::user_text("old request ".repeat(20), 1),
                Message::Assistant(assistant),
                Message::user_text("recent request", 3),
            ])
            .await
            .expect("load history");
        Application::new(session).await
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
        assert!(data["summary"].as_str().is_some_and(|summary| summary.ends_with("rpc checkpoint")));
        assert!(data.get("apiKey").is_none());
        assert!(matches!(app.messages().first(), Some(Message::CompactionSummary(_))));

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
        assert!(response.error.as_deref().is_some_and(|error| error.contains("summary rejected")));

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
            self.frames.lock().expect("extension frame queue").push_back(Some(frame));
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
                            invocation:
                                pi_coding::ExtensionInvocation::Event { event },
                            ..
                        } if event.name == "session_before_switch" => json!({"cancel":true}),
                        pi_coding::ExtensionHostRequest::Invoke {
                            invocation:
                                pi_coding::ExtensionInvocation::Event { event },
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
                    self.frames.lock().expect("extension frame queue").push_back(None);
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
                    if let Some(frame) = self.frames.lock().expect("extension frame queue").pop_front() {
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
        ) -> pi_coding::ExtensionFuture<'_, Result<Arc<dyn pi_coding::ExtensionTransport>>> {
            Box::pin(async {
                Ok(Arc::new(LifecycleExtensionTransport::new())
                    as Arc<dyn pi_coding::ExtensionTransport>)
            })
        }
    }

    async fn build_lifecycle_app(cancel: bool) -> (tempfile::TempDir, Application) {
        let cwd = tempfile::tempdir().expect("lifecycle cwd");
        let session = pi_coding::Session::new(pi_coding::SessionOptions {
            model: Model::default(),
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
        })
        .expect("lifecycle session");
        let recorder = pi_coding::start_session_in(
            cwd.path(),
            None,
            None,
            Some(cwd.path()),
            None,
            None,
        )
        .expect("record lifecycle session");
        recorder
            .record_message(&Message::user_text("fork text", 1))
            .expect("record fork message");
        recorder.persist_now().expect("persist lifecycle session");
        session.set_session_dir(cwd.path().to_path_buf());
        session.record(recorder).expect("attach lifecycle recorder");
        if !cancel {
            return (cwd, Application::new(session).await);
        }
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
        let report = runtime
            .load(vec![pi_coding::ExtensionSpec::new(
                "rpc-lifecycle",
                cwd.path().join("fixture-extension"),
                cwd.path(),
                pi_coding::ExtensionOrigin::Project,
                true,
                permissions.clone(),
            )])
            .await;
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        (
            cwd,
            Application::new_with_extensions(session, runtime, permissions).await,
        )
    }

    #[tokio::test]
    async fn lifecycle_rpc_cancellation_is_successful_and_preserves_session() {
        let (_cwd, app) = build_lifecycle_app(true).await;
        let before = app.state().await;
        let messages = app.messages();
        let entry_id = app.fork_messages().expect("fork messages")[0].entry_id.clone();
        let switch_path = before.session_file.clone().expect("session file");
        for (command, expected) in [
            (RpcCommand::NewSession { id: Some("new".into()), parent_session: None }, json!({"cancelled":true})),
            (RpcCommand::SwitchSession { id: Some("switch".into()), session_path: switch_path.clone() }, json!({"cancelled":true})),
            (RpcCommand::Fork { id: Some("fork".into()), entry_id: entry_id.clone() }, json!({"text":"","cancelled":true})),
            (RpcCommand::Clone { id: Some("clone".into()) }, json!({"cancelled":true})),
        ] {
            let response = handle_command(&app, &settings_state(), &workflows_state(), command).await;
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
            RpcCommand::SwitchSession { id: Some("switch".into()), session_path: switch_path },
        )
        .await;
        assert!(switched.success, "{switched:?}");
        assert_eq!(switched.data, Some(json!({"cancelled":false})));

        let created = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::NewSession { id: Some("new".into()), parent_session: None },
        )
        .await;
        assert!(created.success, "{created:?}");
        assert_eq!(created.data, Some(json!({"cancelled":false})));
        app.cleanup().await;
    }

    #[tokio::test]
    async fn fork_rpc_returns_selected_text_and_false_when_not_cancelled() {
        let (_cwd, app) = build_lifecycle_app(false).await;
        let entry_id = app.fork_messages().expect("fork messages")[0].entry_id.clone();
        let response = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::Fork { id: Some("fork".into()), entry_id },
        )
        .await;
        assert!(response.success, "{response:?}");
        assert_eq!(response.data, Some(json!({"text":"fork text","cancelled":false})));
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
        app.session().set_session_dir(session_dir.path().to_path_buf());
        app.session().record(recorder).expect("attach recorder");
        let source_file = app.state().await.session_file.expect("source session file");

        let response = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::Clone {
                id: Some("clone-1".into()),
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
        let session = pi_coding::Session::new(pi_coding::SessionOptions {
            model: Model::default(),
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
        let app = Application::new(session).await;

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
            RpcCommand::SessionList { id: Some("list".into()), scope: RpcSessionListScope::Current },
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
        assert_eq!(row["source"], "pi");
        assert_eq!(row["status"], "native");
        assert_eq!(row["cwd"], cwd.path().to_string_lossy().as_ref());
        assert_eq!(row["messageCount"], 1);
        assert!(row["summary"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(row["modifiedEpoch"].is_f64());
        assert!(row["displayTime"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(
            row["path"]
                .as_str()
                .is_some_and(|p| p.ends_with(".jsonl")),
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
            RpcCommand::SetSessionName { id: Some("rename".into()), name: "web demo".into() },
        )
        .await;
        assert!(renamed.success, "{renamed:?}");
        let relisted = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::SessionList { id: Some("list2".into()), scope: RpcSessionListScope::Current },
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
        let storage = crate::session_run::SessionStorage::DefaultTree {
            native_root: root,
        };
        let dir_a = storage.session_dir_for(project_a.path()).expect("A dir");
        let dir_b = storage.session_dir_for(project_b.path()).expect("B dir");
        for (cwd, dir, message) in [
            (project_a.path(), dir_a.as_path(), "project A"),
            (project_b.path(), dir_b.as_path(), "project B"),
        ] {
            let recorder = pi_coding::start_session_in(cwd, None, None, Some(dir), None, None)
                .expect("record session");
            recorder.record_message(&Message::user_text(message, 1)).expect("message");
            recorder.persist_now().expect("persist");
        }
        let session = pi_coding::Session::new(pi_coding::SessionOptions {
            model: Model::default(), cwd: project_a.path().to_path_buf(), system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off, api_key: "test".to_owned(), compaction: None,
            stream_options: Default::default(), tools: Some(Vec::new()), before_tool_call: None,
            after_tool_call: None, stream_fn: None, auth_resolver: None,
        }).expect("session");
        session.set_session_dir(dir_a);
        let app = Application::new(session).await;
        let response = handle_command_with_session_storage(
            &app, &settings_state(), &workflows_state(), Some(&storage),
            RpcCommand::SessionList { id: None, scope: RpcSessionListScope::AllProjects },
        ).await;
        assert!(response.success, "{response:?}");
        let rows = response.data.expect("catalog")["sessions"].as_array().expect("rows").clone();
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
        assert_eq!(row_a["temporary"], true, "temp-workspace row must be marked temporary: {row_a}");
        assert_eq!(row_b["temporary"], true, "temp-workspace row must be marked temporary: {row_b}");
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
    async fn goal_pin_unpin_and_journal_rpc_replay_recorder_backed_session() {
        let cwd = tempfile::tempdir().expect("goal journal cwd");
        let session = pi_coding::Session::new(pi_coding::SessionOptions {
            model: Model::default(),
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
        let app = Application::new(session).await;
        let settings = settings_state();

        let created = handle_command(
            &app,
            &settings,
            &workflows_state(),
            RpcCommand::GoalCreate {
                id: Some("create".into()),
                objective: "ship cleanly".into(),
                token_budget: Some(20),
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

        let malformed = parse_input(br#"{"type":"goal_pin","text":7}"#)
            .expect_err("malformed goal_pin");
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
        assert_eq!(unpinned.data.expect("goal")["pins"], json!(["double-check"]));

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
            ["created", "pins_updated", "pins_updated", "pins_updated", "paused"]
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
        assert_eq!(
            state_data["goal"]["current"]["lifecycle"],
            "paused"
        );
        assert_eq!(
            state_data["goal"]["current"]["pins"],
            json!(["double-check"])
        );

        app.cleanup().await;
    }

    async fn build_settings_app() -> (tempfile::TempDir, Application) {
        let agent = tempfile::tempdir().expect("agent");
        let cwd = tempfile::tempdir().expect("cwd");
        let mut resource_options = pi_coding::ResourceManagerOptions::new(cwd.path());
        resource_options.agent_dir = agent.path().to_path_buf();
        resource_options.project_trust_override = Some(true);
        let resources = pi_coding::ResourceManager::new(resource_options).expect("resources");
        let session = pi_coding::Session::new(pi_coding::SessionOptions {
            model: Model::default(),
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
        })
        .expect("session");
        session
            .attach_resources(resources)
            .await
            .expect("attach resources");
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
    async fn set_todos_round_trips_through_application() {
        use pi_coding::{TodoItem, TodoPhase, TodoStatus};
        let app = build_todo_app("faux-rpc-todo-set", "faux-rpc-todo-set-api").await;
        let phases = vec![TodoPhase {
            name: "Plan".into(),
            tasks: vec![
                TodoItem { id: "task-root".into(), content: "root".into(), status: TodoStatus::InProgress, depends_on: Vec::new(), ready: true, blocked_by: Vec::new(), agent: None },
                TodoItem { id: "task-do".into(), content: "do".into(), status: TodoStatus::Pending, depends_on: vec!["task-root".into()], ready: false, blocked_by: Vec::new(), agent: None },
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
        assert_eq!(d["phases"][0]["tasks"][1]["dependsOn"], json!(["task-root"]));
        assert_eq!(d["phases"][0]["tasks"][1]["ready"], false);
        assert_eq!(d["phases"][0]["tasks"][1]["blockedBy"][0]["taskId"], "task-root");
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
        assert!(task_id.starts_with("task-"), "append assigns a task id: {task_id}");

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
        assert_eq!(reopen_data["phases"][0]["tasks"][0]["status"], "in_progress");

        // A blocked start is rejected and the application state is untouched:
        // append a second task, wire it as a dependency of the first (which is
        // still in_progress), then try to start it.
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
        assert!(link.success, "add_dependency must succeed: {:?}", link.error);
        let blocked = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::TodoOp {
                id: Some("t6".into()),
                op: pi_coding::TodoOp::Start { task: dependent_id },
            },
        )
        .await;
        assert!(!blocked.success, "start on a blocked task must fail");
        assert!(blocked.error.as_deref().is_some_and(|e| e.contains("blocked")));
        let state = app.todo_state();
        assert_eq!(
            state.phases[0].tasks[1].status,
            pi_coding::TodoStatus::Pending,
            "failed op must not mutate application todo state"
        );
    }
    #[tokio::test]
    async fn set_todos_with_workflow_id_fails_without_mutating_parent_when_missing() {
        let app = build_todo_app("faux-rpc-workflow-todo-missing", "faux-rpc-workflow-todo-missing-api").await;
        let phases = vec![pi_coding::TodoPhase {
            name: "Workflow".into(),
            tasks: vec![pi_coding::TodoItem {
                id: "scoped".into(), content: "scoped".into(), status: pi_coding::TodoStatus::Pending,
                depends_on: Vec::new(), ready: true, blocked_by: Vec::new(), agent: None,
            }],
        }];
        let response = handle_command(
            &app,
            &settings_state(),
            &workflows_state(),
            RpcCommand::SetTodos { id: Some("scoped".into()), workflow_id: Some("missing".into()), phases },
        ).await;
        assert!(!response.success);
        assert_eq!(response.error.as_deref(), Some("application workflow manager is not configured"));
        assert!(app.todo_state().phases.is_empty(), "missing workflow must not fall back to parent");
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
                blocked_by: Vec::new(),
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
        assert!(!crate::workflow_rpc::wire_json_leaks_absolute_path(&encoded));

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
        assert_eq!(listed.data.as_ref().unwrap()["workflows"][0]["workflowId"], workflow_id);

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
        assert_eq!(got.data.as_ref().unwrap()["objective"], "land multi-workflow foundation");

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
        let event = crate::workflow_rpc::project_workflow_event(
            &pi_coding::WorkflowEvent::Updated {
                snapshot: host.clone(),
            },
        );
        let event_json = serde_json::to_value(&event).unwrap();
        assert_eq!(event_json["type"], "workflow_updated");
        assert_eq!(event_json["workflowId"], workflow_id);
        assert_eq!(event_json["generation"], host.generation);
        let event_encoded = serde_json::to_string(&event).unwrap();
        assert!(!crate::workflow_rpc::wire_json_leaks_absolute_path(&event_encoded));

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
        let app = build_todo_app("faux-rpc-workflow-lifecycle", "faux-rpc-workflow-lifecycle-api")
            .await;
        let settings = settings_state();
        let workflows = workflows_state();
        let handle =
            |command: RpcCommand| handle_command(&app, &settings, &workflows, command);

        // create -> queued
        let created = handle(RpcCommand::WorkflowCreate {
            id: Some("lifecycle-create".into()),
            name: "ship".into(),
            objective: "land multi-workflow lifecycle".into(),
        })
        .await;
        assert!(created.success, "{created:?}");
        let workflow_id = created
            .data
            .as_ref()
            .expect("create data")["workflowId"]
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
            paused.data.as_ref().unwrap()["generation"].as_u64().unwrap() >= 2,
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
            listed
                .data
                .as_ref()
                .unwrap()["workflows"]
                .as_array()
                .expect("workflows array")
                .is_empty(),
            "removed workflow must be gone from the list"
        );

        app.cleanup().await;
    }

    #[tokio::test]
    async fn workflow_detail_dispatches_panel_projection_through_rpc() {
        let app = build_todo_app("faux-rpc-workflow-detail", "faux-rpc-workflow-detail-api")
            .await;
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
        let workflow_id = created
            .data
            .as_ref()
            .expect("create data")["workflowId"]
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
            self.frames.lock().expect("extension frame queue").push_back(Some(frame));
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
                    self.push(pi_coding::ExtensionFrame::Response {
                        id: id.clone(),
                        result: pi_coding::ProtocolResult::Success { value: Value::Null },
                    });
                }
                pi_coding::ExtensionHostFrame::Shutdown { .. } => {
                    self.frames.lock().expect("extension frame queue").push_back(None);
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
                    if let Some(frame) = self.frames.lock().expect("extension frame queue").pop_front() {
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
        ) -> pi_coding::ExtensionFuture<'_, Result<Arc<dyn pi_coding::ExtensionTransport>>> {
            Box::pin(async {
                Ok(Arc::new(CatalogExtensionTransport::new())
                    as Arc<dyn pi_coding::ExtensionTransport>)
            })
        }
    }

    async fn build_command_catalog_app() -> (tempfile::TempDir, Application) {
        let cwd = tempfile::tempdir().expect("catalog cwd");
        let prompt_dir = cwd.path().join(".pi/prompts");
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
        let skill_dir = cwd.path().join(".pi/skills/catalog-skill");
        std::fs::create_dir_all(&skill_dir).expect("skill directory");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: catalog-skill\ndescription: Skill only command\n---\nskill body\n",
        )
        .expect("skill definition");

        let session = pi_coding::Session::new(pi_coding::SessionOptions {
            model: Model::default(),
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
        })
        .expect("catalog session");
        let mut resource_options = pi_coding::ResourceManagerOptions::new(cwd.path());
        resource_options.project_trust_override = Some(true);
        session
            .attach_resources(pi_coding::ResourceManager::new(resource_options).expect("resources"))
            .await
            .expect("attach resources");

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
        let spec = pi_coding::ExtensionSpec::new(
            "rpc-catalog",
            cwd.path().join("fixture-extension"),
            cwd.path(),
            pi_coding::ExtensionOrigin::Project,
            true,
            permissions.clone(),
        );
        let report = runtime.load(vec![spec]).await;
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        (
            cwd,
            Application::new_with_extensions(session, runtime, permissions).await,
        )
    }

    #[tokio::test]
    async fn get_commands_projects_executable_catalog_with_loaded_skills() {
        let (_cwd, app) = build_command_catalog_app().await;
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
        assert_eq!(shared["source"], "prompt", "prompt wins the dynamic collision");

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
        assert!(
            commands.iter().any(|c| c["name"] == "workflow"),
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

    async fn build_side_chat_app(model_id: &str) -> Application {
        let mut model = Model::default();
        model.id = model_id.into();
        model.name = model_id.into();
        model.api = format!("{model_id}-api");
        model.provider = "faux".into();
        model.base_url = "http://localhost:0".into();
        let cwd = tempfile::tempdir().expect("tempdir");
        let session = pi_coding::Session::new(pi_coding::SessionOptions {
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
            stream_fn: Some(reply_stream("side reply")),
            auth_resolver: None,
        })
        .expect("session");
        Application::new(session).await
    }

    /// Build an Application whose session cwd is `cwd` and whose provider stream
    /// replies with a fixed assistant message. Used by code-review RPC tests:
    /// `CodeReviewController::fork_from` forks the side-chat conversation (no
    /// provider call), and comment submissions resolve through `reply_stream`.
    async fn build_code_review_app(cwd: &Path) -> Application {
        let mut model = Model::default();
        model.id = "faux-rpc-code-review".into();
        model.name = "faux-rpc-code-review".into();
        model.api = "faux-rpc-code-review-api".into();
        model.provider = "faux".into();
        model.base_url = "http://localhost:0".into();
        let session = pi_coding::Session::new(pi_coding::SessionOptions {
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
            stream_fn: Some(reply_stream("review reply")),
            auth_resolver: None,
        })
        .expect("code-review session");
        Application::new(session).await
    }

    #[tokio::test]
    async fn side_chat_rpc_round_trip_creates_prompts_switches_and_closes_tabs() {
        let app = build_side_chat_app("faux-rpc-side").await;
        let side_chat = SideChatRpcState::default();
        let dispatch =
            |command: RpcCommand| handle_side_chat_command(&app, &side_chat, command);

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
        })
        .await;
        assert!(prompted.success, "{prompted:?}");
        let data = prompted.data.expect("snapshot");
        assert_eq!(data["accepted"], json!(true), "{data}");
        assert_eq!(data["busy"], json!(false));

        // Poll until the assistant reply lands (the prompt task runs in the
        // background; snapshots drain controller events before serializing).
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
        assert!(saw_reply, "side-chat assistant reply must appear in the snapshot");

        // The side transcript must not have leaked into the main session.
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
        let app = build_side_chat_app("faux-rpc-side-busy").await;
        let side_chat = SideChatRpcState::default();
        let dispatch =
            |command: RpcCommand| handle_side_chat_command(&app, &side_chat, command);

        let switched = dispatch(RpcCommand::SideChatSwitch {
            id: None,
            name: "nope".into(),
        })
        .await;
        assert!(!switched.success);
        assert!(switched.error.as_deref().is_some_and(|e| e.contains("nope")));

        let invalid = dispatch(RpcCommand::SideChatNew {
            id: None,
            name: "new".into(), // reserved /btw word
        })
        .await;
        assert!(!invalid.success, "{invalid:?}");
        assert!(invalid
            .error
            .as_deref()
            .is_some_and(|e| e.contains("reserved")));

        side_chat.shutdown().await;
        app.cleanup().await;
    }

    #[tokio::test]
    async fn skill_rpc_returns_frontmatter_summary_and_rejects_unknown() {
        let (_cwd, app) = build_command_catalog_app().await;
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
        assert!(summary.contains("description: Skill only command"), "{summary}");

        // Empty / whitespace name is rejected with a typed error.
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

        // Unknown skill is rejected (never sent to the model).
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
    async fn get_commands_projects_skill_argument_metadata() {
        let (_cwd, app) = build_command_catalog_app().await;
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
        // code-review stays primary with an optional argument hint.
        let code_review = commands
            .iter()
            .find(|command| command["name"] == "code-review")
            .expect("code-review is a visible primary command");
        assert_eq!(code_review["argumentHint"], "[<from> <to>]");
        assert_eq!(code_review["requiresArguments"], false);
        app.cleanup().await;
    }

    #[tokio::test]
    async fn code_review_open_on_non_git_cwd_surfaces_error_without_root() {
        let cwd = tempfile::tempdir().expect("non-git cwd");
        // Intentionally NOT a git repository.
        let app = build_code_review_app(cwd.path()).await;
        let code_review = CodeReviewRpcState::default();
        let response = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen {
                id: Some("open-non-git".into()),
                from: None,
                to: None,
            },
        )
        .await;
        assert!(response.success, "{response:?}");
        assert_eq!(response.command, "code_review_open");
        let data = response.data.expect("open data");
        // The absolute repository root must never be exposed on the wire.
        let encoded = serde_json::to_string(&data).unwrap();
        assert!(!encoded.contains("\"root\""), "root must not be exposed: {encoded}");
        assert!(data["files"].as_array().expect("files").is_empty());
        assert!(
            data["error"].as_str().is_some_and(|error| !error.is_empty()),
            "non-git open must surface an error payload: {data}"
        );
        assert_eq!(data["comparisonLabel"], "HEAD → working tree");
        assert_eq!(data["isStreaming"], false);
        assert_eq!(data["activeHunk"], Value::Null);
        assert_eq!(data["threads"].as_array().expect("threads").len(), 0);

        // A bare snapshot re-projects the same error state.
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

        // Close tears down the review controller.
        let closed = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewClose {
                id: Some("close-non-git".into()),
            },
        )
        .await;
        assert!(closed.success, "{closed:?}");
        assert_eq!(closed.data.expect("close data")["closed"], true);

        // After close, snapshot errors (no open review).
        let after = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewSnapshot {
                id: Some("snap-after-close".into()),
            },
        )
        .await;
        assert!(!after.success, "{after:?}");
        assert!(after.error.as_deref().is_some_and(|e| e.contains("no open code review")));
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
        git(&["config", "user.email", "test@example.com"]).status.success();
        git(&["config", "user.name", "Test"]).status.success();
        std::fs::write(cwd.path().join("README.md"), "initial\n").expect("write");
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "init"]).status.success());
        // Working-tree changes: modify README.md and add new.txt.
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
                from: None,
                to: None,
            },
        )
        .await;
        assert!(response.success, "{response:?}");
        let data = response.data.expect("open data");
        assert!(data["error"].is_null(), "no error expected on a real repo: {data}");
        let encoded = serde_json::to_string(&data).unwrap();
        assert!(!encoded.contains("\"root\""), "root must not be exposed: {encoded}");
        let files = data["files"].as_array().expect("files");
        let paths: Vec<&str> = files
            .iter()
            .map(|file| file["path"].as_str().expect("path"))
            .collect();
        assert!(paths.contains(&"README.md"), "modified file must appear: {paths:?}");
        assert!(paths.contains(&"new.txt"), "added file must appear: {paths:?}");

        // Hunks carry identity + typed lines.
        let readme = files
            .iter()
            .find(|file| file["path"] == "README.md")
            .expect("readme");
        let hunks = readme["hunks"].as_array().expect("hunks");
        assert!(!hunks.is_empty(), "modified file must have a hunk");
        let hunk = &hunks[0];
        assert!(
            hunk["contentHash"].as_str().is_some_and(|hash| !hash.is_empty()),
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

        // Open rejects from+to mismatch (one without the other).
        let half = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen {
                id: Some("open-half".into()),
                from: Some("HEAD".into()),
                to: None,
            },
        )
        .await;
        assert!(!half.success, "{half:?}");
        assert!(
            half.error
                .as_deref()
                .is_some_and(|e| e.contains("from") && e.contains("to")),
            "{half:?}"
        );

        // Close cleans up the review controller.
        let closed = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewClose {
                id: Some("close-git".into()),
            },
        )
        .await;
        assert!(closed.success, "{closed:?}");
        assert_eq!(closed.data.expect("close data")["closed"], true);
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
            RpcCommand::CodeReviewOpen { id: None, from: None, to: None },
        )
        .await;
        assert!(opened.success, "{opened:?}");
        let snapshot_id = opened.data.as_ref().expect("open data")["snapshotId"]
            .as_str()
            .expect("snapshot id")
            .to_owned();

        // Unknown path is rejected (containment) before git ever runs.
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
        assert!(unknown.error.as_deref().is_some_and(|e| e.contains("unknown file")));

        // A traversal path is rejected as unknown (normalize collapses "..").
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

        // First page loads the big file's full diff and returns a bounded page.
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
        assert_eq!(page["lines"].as_array().expect("lines").len(), 50);
        assert_eq!(page["hasMore"], true);
        let next = page["nextCursor"].as_u64().expect("next cursor");
        assert_eq!(next, 50);
        let total = page["totalLines"].as_u64().expect("total lines");
        assert!(total > 4000, "the big file should exceed the render cap: {total}");

        // A stale snapshot id is rejected.
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
        assert!(stale.error.as_deref().is_some_and(|e| e.contains("stale snapshot")));

        // A cursor past the end is rejected by the slicer.
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
        assert!(over_cursor.error.as_deref().is_some_and(|e| e.contains("out of range")));

        // Mutate the working tree before refresh so the snapshot identity
        // (a digest of the diff bytes) actually changes; an unchanged refresh
        // is idempotent and reuses the same snapshot id by design.
        let changed2 = "changed-line\n".repeat(6000);
        std::fs::write(cwd.path().join("big.txt"), &changed2).expect("write big2");
        assert!(git(&["add", "big.txt"]).status.success());

        // Refresh invalidates the cache: the old snapshot id is now stale.
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
        // The new snapshot id still serves pages.
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
            RpcCommand::CodeReviewClose { id: None },
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
        git(&["config", "user.email", "test@example.com"]).status.success();
        git(&["config", "user.name", "Test"]).status.success();
        std::fs::write(cwd.path().join("big-a.txt"), "base\n").expect("write big-a");
        std::fs::write(cwd.path().join("big-b.txt"), "base\n").expect("write big-b");
        std::fs::write(cwd.path().join("rename-old.txt"), "alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta\n")
            .expect("write rename source");
        std::fs::write(cwd.path().join("zz-later.txt"), "base\n").expect("write later");
        assert!(git(&["add", "-A"]).status.success());
        assert!(git(&["commit", "-q", "-m", "baseline"]).status.success());
        let changed = "changed-line\n".repeat(200_000);
        std::fs::write(cwd.path().join("big-a.txt"), &changed).expect("write big-a changed");
        std::fs::write(cwd.path().join("big-b.txt"), &changed).expect("write big-b changed");
        std::fs::rename(cwd.path().join("rename-old.txt"), cwd.path().join("rename-new.txt"))
            .expect("rename");
        std::fs::write(
            cwd.path().join("rename-new.txt"),
            "alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta\nrenamed\n",
        )
        .expect("write rename destination");
        std::fs::write(cwd.path().join("zz-later.txt"), "base\nchanged\n").expect("write later changed");
        assert!(git(&["add", "-A"]).status.success());

        let app = build_code_review_app(cwd.path()).await;
        let code_review = CodeReviewRpcState::default();
        let opened = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewOpen { id: None, from: None, to: None },
        )
        .await;
        assert!(opened.success, "{opened:?}");
        let data = opened.data.expect("open data");
        assert_eq!(data["truncated"], true, "snapshot must be globally truncated: {data}");
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

        // A placeholder (empty hunks + truncated) must be present and must
        // serve its first page through code_review_file_diff.
        let placeholder = files
            .iter()
            .find(|file| {
                file["truncated"] == true && file["hunks"].as_array().expect("hunks").is_empty()
            })
            .expect("truncated snapshot must expose a placeholder");
        let placeholder_path = placeholder["path"].as_str().expect("placeholder path").to_owned();
        assert_eq!(placeholder["message"], "diff omitted: combined diff truncated; loaded on demand");
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
        assert_eq!(placeholder_page["lines"].as_array().expect("lines").len(), 50);

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
            assert!(
                page["lines"].as_array().expect("lines").len() > 0,
                "file diff must be readable after truncation: {page}"
            );
        }

        let closed = handle_code_review_command(
            &app,
            &code_review,
            RpcCommand::CodeReviewClose { id: None },
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
            error: None,
        };
        let identity = snapshot.hunk_identity(&snapshot.files[0], &snapshot.files[0].hunks[0]);

        // A fully matching identity resolves to the (file, hunk).
        let (resolved_file, resolved_hunk) = resolve_comment_target(
            &snapshot,
            &identity.snapshot_id,
            "src/lib.rs",
            1,
            1,
            1,
            1,
            &identity.content_hash,
        )
        .expect("matching identity resolves");
        assert_eq!(resolved_file.path, "src/lib.rs");
        assert_eq!(resolved_hunk.old_start, 1);

        // Stale snapshot id is rejected.
        let err = resolve_comment_target(
            &snapshot,
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

    /// Session with dense history so snap compact has something to archive
    /// (mirrors the pi-coding snap_compact_tests fixture: 12 large turns,
    /// keep 2, never calls the provider).
    async fn build_snapcompact_app(model_id: &str) -> Application {
        let mut model = Model::default();
        model.id = model_id.into();
        model.name = model_id.into();
        model.api = format!("{model_id}-api");
        model.provider = "faux".into();
        model.base_url = "http://localhost:0".into();
        let cwd = tempfile::tempdir().expect("tempdir");
        let session = pi_coding::Session::new(pi_coding::SessionOptions {
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
        Application::new(session).await
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
        app.session().set_session_dir(session_dir.path().to_path_buf());
        app.session().record(recorder).expect("attach recorder");

        let settings = settings_state();
        let workflows = workflows_state();
        let handle = |command: RpcCommand| handle_command(&app, &settings, &workflows, command);

        // Bare rewind must refuse and point at get_entries.
        let bare = handle(RpcCommand::Rewind {
            id: Some("rewind-bare".into()),
            index: None,
            checkpoint: None,
        })
        .await;
        assert!(!bare.success, "{bare:?}");
        assert!(bare
            .error
            .as_deref()
            .is_some_and(|e| e.contains("get_entries") && e.contains("index")));

        // Both targets must be rejected.
        let both = handle(RpcCommand::Rewind {
            id: Some("rewind-both".into()),
            index: Some(2),
            checkpoint: Some("c".into()),
        })
        .await;
        assert!(!both.success);

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
        assert_eq!(checkpointed.data.expect("rewind")["checkpoint"], "milestone");

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
        app.steer("first steer".to_owned(), Vec::new()).await.expect("queue steer");
        app.follow_up("second follow".to_owned(), Vec::new()).await.expect("queue follow-up");

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

    // ------------------------------------------------------------------
    // D93: Subagents panel RPC — job_list / task_spawn / job_cancel /
    // hub_send / job_output.
    // ------------------------------------------------------------------

    /// Session wired to an instantly-settling faux stream (one empty turn).
    fn subagents_session() -> pi_coding::Session {
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
        pi_coding::Session::new(pi_coding::SessionOptions {
            model: Model::default(),
            cwd: std::env::current_dir().expect("cwd"),
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
        use pi_coding::{AgentCatalog, AgentDefinition, AgentDefinitionSource, OrchestrationConfig};
        let root = tempfile::tempdir().expect("tempdir");
        let agent = |name: &str, description: &str| AgentDefinition { name: name.to_owned(),
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
        path: None, trusted: true, kind: pi_coding::AgentDefinitionKind::Agent, personality: None, soft_budget: None };
        let factory: pi_coding::ChildSessionFactory = Arc::new(move |request| {
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
                pi_coding::Session::new(pi_coding::SessionOptions {
                    model: request.model,
                    cwd: std::env::current_dir().expect("cwd"),
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
        let application =
            Application::new_with_orchestration(subagents_session(), runtime).await;
        (root, application)
    }

    async fn subagents_handle(
        app: &Application,
        command: RpcCommand,
    ) -> RpcResponse {
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
        let spawns = spawned
            .data
            .expect("spawns")["spawns"]
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
    fn seed_persona(agent_dir: &std::path::Path, name: &str, seed_state: bool) -> std::path::PathBuf {
        let root = agent_dir.join("personas").join(name);
        std::fs::create_dir_all(&root).expect("persona root");
        std::fs::write(
            root.join("persona.md"),
            format!("---\nname: {name}\ndescription: durable {name}\n---\n{name} prompt"),
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
    async fn persona_rpc_app(seed_state: bool) -> (tempfile::TempDir, Application, pi_ai::providers::FauxProviderRegistration, std::path::PathBuf) {
        use pi_ai::providers::{FauxProviderOptions, FauxResponse, register_faux_provider};
        use pi_ai::StopReason;
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
        std::fs::create_dir_all(cwd.join(".pi").join("artifacts")).expect("artifacts");

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

        let mut options = ResourceManagerOptions::new(&cwd);
        options.agent_dir = agent_dir;
        options.disable_extensions = true;
        options.disable_skills = true;
        options.disable_prompt_templates = true;
        options.disable_themes = true;
        options.disable_context_files = true;
        let resources = ResourceManager::new(options).expect("resources");
        let session = Session::new(SessionOptions {
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
        })
        .expect("session");
        let session_root = root.path().join("session-root");
        std::fs::create_dir_all(&session_root).expect("session root");
        session.set_session_dir(session_root);
        session.start_new_recording().expect("start recording");
        session
            .attach_resources(resources)
            .await
            .expect("attach resources");
        let application = Application::new(session).await;

        // Orchestration catalog mirrors the resource snapshot (the real flow
        // builds it from snapshot.agents at startup and rebuilds on reload).
        let snapshot = application
            .resource_snapshot()
            .expect("resource snapshot after attach");
        let factory: pi_coding::ChildSessionFactory = Arc::new(move |request| {
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
                pi_coding::Session::new(pi_coding::SessionOptions {
                    model: request.model,
                    cwd: std::env::current_dir().expect("cwd"),
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
                })
            })
        });
        let mut config = OrchestrationConfig::new(
            AgentCatalog::from_agents(snapshot.agents.clone()),
            cwd.join(".pi").join("artifacts"),
        );
        config.parent_model = model;
        let runtime = OrchestrationRuntime::new(config, factory).expect("orchestration runtime");
        application
            .attach_orchestration(runtime)
            .expect("attach orchestration");
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
        assert!(row["contractSummary"].as_str().is_some_and(|s| !s.is_empty()));
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
            !serialized.contains(std::env::current_dir().expect("cwd").to_string_lossy().as_ref()),
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
                content: "---\nname: guide\ndescription: guided assistant\n---\nguide prompt\n".to_owned(),
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
                content: "---\nname: guide\ndescription: refined guide\n---\nrefined prompt".to_owned(),
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
        assert!(persona_md.exists(), "unconfirmed op must not touch the disk");

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
            listed.data.expect("list")["personas"].as_array().expect("rows").len(),
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
            listed_after.data.expect("list")["personas"].as_array().expect("rows").len(),
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
    async fn persona_rpc_natural_language_prompt_routes_to_persona() {
        // The natural-language form `让 mentor 审查这次修改` with NO explicit
        // agent routes through the existing orchestration selector to the
        // PERSONA agent (durably-bound runtime; no frontend heuristic).
        let (_root, app, _registration, _persona_root) = persona_rpc_app(false).await;
        let runtime = app.orchestration_runtime().expect("runtime");
        let spawned = runtime
            .spawn_from_natural_language("Main", 0, "让 mentor 审查这次修改")
            .expect("spawn from natural language")
            .expect("delegation must produce spawns");
        assert_eq!(spawned.len(), 1, "{spawned:?}");
        assert_eq!(spawned[0].agent, "mentor", "{spawned:?}");
        assert_eq!(spawned[0].agent_id, "mentor", "{spawned:?}");
        let listed = subagents_handle(&app, RpcCommand::JobList { id: None }).await;
        let jobs = listed.data.expect("job list")["jobs"]
            .as_array()
            .expect("jobs array")
            .clone();
        assert_eq!(jobs.len(), 1, "{jobs:?}");
        assert_eq!(jobs[0]["agent"], json!("mentor"), "{jobs:?}");
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
            !serialized.contains(std::env::current_dir().expect("cwd").to_string_lossy().as_ref()),
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
        assert!(!response.success, "reload failure must not fake success: {response:?}");
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
    async fn persona_rpc_fails_closed_without_resource_manager() {
        // A session without a ResourceManager has no persona catalog: list
        // reports disabled, and read/mutate commands fail instead of
        // fabricating rows.
        let app = build_todo_app("faux-rpc-persona-none", "faux-rpc-persona-none-api").await;
        let listed = subagents_handle(&app, RpcCommand::PersonaList { id: None }).await;
        assert!(listed.success, "{listed:?}");
        assert_eq!(listed.data.expect("list")["enabled"], json!(false));
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
    const REALTIME_TEST_SDP_OFFER: &str =
        "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n";

    fn realtime_test_settings(base_url: &str) -> pi_coding::LiveRuntimeSettings {
        pi_coding::LiveRuntimeSettings {
            enabled: true,
            mode: "realtime".to_owned(),
            stt_base_url: String::new(),
            stt_api_key: String::new(),
            stt_model: String::new(),
            realtime_base_url: base_url.to_owned(),
            realtime_api_key: REALTIME_TEST_KEY.to_owned(),
            // A custom alias (not the upstream's gpt-realtime-1.5) keeps the
            // fixture distinct from any hardcoded upstream value. The create-call
            // session omits `model` entirely (Codex rejects it with 400); the
            // web `session.update` data-channel shape forwards the configured
            // label verbatim instead.
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
        let settings =
            realtime_test_settings("http://alice:SUPER-SECRET-PASSWORD@127.0.0.1:1/v1");
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

        let call = tokio::spawn(async move {
            realtime_create_call(&settings, REALTIME_TEST_SDP_OFFER).await
        });

        let (mut sock, _addr) = listener.accept().await.expect("accept");
        let _req = read_http_request(&mut sock).await;
        // The upstream echoes credential-looking material back in every
        // established form (Authorization header, mixed-case, colon/space and
        // `=` separators, query-style values, and the bare configured key);
        // the shared redaction must scrub all of them from the error.
        let body = format!(
            "Authorization: Bearer {0}\nAPI_KEY: {0}\nToken : {0}\ntoken={0}\napiKey={1}\n{1}",
            "SUPER-SECRET-UPSTREAM-TOKEN",
            REALTIME_TEST_KEY
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

        let call = tokio::spawn(async move {
            realtime_create_call(&settings, REALTIME_TEST_SDP_OFFER).await
        });

        let (mut sock, _addr) = listener.accept().await.expect("accept");
        let _req = read_http_request(&mut sock).await;
        // Claim an oversized answer via Content-Length only: the pre-check
        // must reject it on the header, before any body bytes are read.
        let header = format!(
            "HTTP/1.1 200 OK\r\nLocation: /v1/realtime/calls/rtc_big\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            REALTIME_BODY_LIMIT + 1
        );
        sock.write_all(header.as_bytes()).await.expect("write oversized headers");
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
    async fn realtime_create_call_posts_session_payload_and_returns_sdp_call_id() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let settings = realtime_test_settings(&format!("http://127.0.0.1:{port}"));

        let call = tokio::spawn(async move {
            realtime_create_call(&settings, REALTIME_TEST_SDP_OFFER).await
        });

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
        // The create-call contract is a JSON POST: `Content-Type:
        // application/json` and a body that is EXACTLY `{sdp, session}` — no
        // extra fields, no multipart. The session object is the Quicksilver
        // create-call shape: `{type, audio}` with `voice` nested under
        // `audio.output` — `model` is deliberately absent because Codex
        // realtime rejects `session.model` with 400 (the configured model
        // rides the data-channel `session.update` instead, where it is
        // accepted).
        let content_type = req.header("content-type").expect("content-type");
        assert_eq!(content_type, "application/json", "content-type: {content_type:?}");
        let request_body = json_request_body(&req.body);
        let mut body_keys = request_body
            .as_object()
            .expect("request body must be an object")
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        body_keys.sort();
        assert_eq!(
            body_keys, ["sdp", "session"],
            "create-call body must be exactly {{sdp, session}}: {request_body}"
        );
        assert_eq!(request_body["sdp"], REALTIME_TEST_SDP_OFFER);
        let session = &request_body["session"];
        assert!(session.is_object(), "session must be an object: {session}");
        // Exact create-call session shape: `{audio, type}` and nothing else.
        let mut session_keys = session
            .as_object()
            .expect("session must be an object")
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        session_keys.sort();
        assert_eq!(
            session_keys, ["audio", "type"],
            "create-call session must be exactly {{audio, type}}: {session}"
        );
        assert_eq!(session["type"], "quicksilver");
        // The forbidden `model` field must never reach the create-call POST —
        // Codex rejects it with 400.
        assert!(
            session.get("model").is_none(),
            "create-call session must not carry `model`: {session}"
        );
        assert_eq!(session["audio"]["input"]["format"]["type"], "audio/pcm");
        assert_eq!(session["audio"]["input"]["format"]["rate"], 24000);
        assert_eq!(session["audio"]["output"]["voice"], "sol");

        let sdp_answer = "v=0\r\no=- 2 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n";
        write_http_response(
            &mut sock,
            "HTTP/1.1 201 Created",
            &[
                ("Location".to_owned(), "/v1/realtime/calls/rtc_alpha-001".to_owned()),
                ("Content-Type".to_owned(), "application/sdp".to_owned()),
            ],
            sdp_answer.as_bytes(),
        )
        .await;

        let result = call.await.expect("task join").expect("create call ok");
        assert_eq!(result["sdp"], sdp_answer);
        assert_eq!(result["callId"], "rtc_alpha-001");
    }

    /// Loopback CLIProxyAPI stand-in implementing the reported Codex
    /// quicksilver create-call contract: a `session` that carries `model` is
    /// rejected with 400 `Field session.model is not allowed for this Codex
    /// realtime session`; one without `model` is accepted with 201 +
    /// `Location` + a bare SDP body. Returns the parsed request and the status
    /// line sent back.
    async fn serve_realtime_create_call(listener: &TcpListener) -> (MockRequest, String) {
        let (mut sock, _addr) = listener.accept().await.expect("accept");
        let req = read_http_request(&mut sock).await;
        let request_body = json_request_body(&req.body);
        let session = &request_body["session"];
        let (status_line, headers, body): (&str, Vec<(String, String)>, Vec<u8>) =
            if session.get("model").is_some() {
                (
                    "HTTP/1.1 400 Bad Request",
                    vec![("Content-Type".to_owned(), "text/plain".to_owned())],
                    b"Field session.model is not allowed for this Codex realtime session".to_vec(),
                )
            } else {
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
    async fn realtime_create_call_omits_model_forbidden_by_codex_create_call() {
        // Codex realtime (CLIProxyAPI quicksilver) rejects create-call
        // requests whose `session` carries `model` with 400 "Field
        // session.model is not allowed for this Codex realtime session". The
        // loopback mock reproduces exactly that discriminator: a session WITH
        // `model` → 400, one WITHOUT → 201. The fixed client must land on the
        // 2xx path, proving the create-call session omits `model` — the
        // configured model rides the data-channel `session.update` instead.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let settings = realtime_test_settings(&format!("http://127.0.0.1:{port}"));

        // Leg 1: the real client. If `realtime_session_payload` ever sends
        // `model` again, the mock 400s the request and this leg fails.
        let call = tokio::spawn(async move {
            realtime_create_call(&settings, REALTIME_TEST_SDP_OFFER).await
        });
        let (req, status_line) = serve_realtime_create_call(&listener).await;
        let request_body = json_request_body(&req.body);
        let session = &request_body["session"];
        assert!(
            session.get("model").is_none(),
            "create-call session must not carry `model` (Codex rejects it): {session}"
        );
        assert!(status_line.starts_with("HTTP/1.1 201"), "{status_line}");
        let result = call
            .await
            .expect("task join")
            .expect("model-free create call must succeed");
        assert_eq!(result["callId"], "rtc_alpha-001");

        // Leg 2: mock fidelity — the pre-fix shape (session WITH `model`) must
        // be rejected with the reported 400, proving the discriminator above
        // is the real upstream contract and leg 1's pass is not a tautology
        // (a mock that always 2xx'd would let a regressed client slip through).
        let old_shape = json!({
            "sdp": REALTIME_TEST_SDP_OFFER,
            "session": {
                "type": "quicksilver",
                "model": "gpt-5.6-sol",
                "audio": {
                    "input": { "format": { "type": "audio/pcm", "rate": 24000 } },
                    "output": { "voice": "sol" },
                },
            },
        });
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
            .expect("write pre-fix shape request");
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

        let call = tokio::spawn(async move {
            realtime_create_call(&settings, REALTIME_TEST_SDP_OFFER).await
        });

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

        let call = tokio::spawn(async move {
            realtime_create_call(&settings, REALTIME_TEST_SDP_OFFER).await
        });

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

        let call = tokio::spawn(async move {
            realtime_create_call(&settings, REALTIME_TEST_SDP_OFFER).await
        });

        let (mut sock, _addr) = listener.accept().await.expect("accept");
        let _req = read_http_request(&mut sock).await;
        write_http_response(
            &mut sock,
            "HTTP/1.1 201 Created",
            &[
                ("Location".to_owned(), "/v1/realtime/calls/not-a-valid-id".to_owned()),
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

        let call = tokio::spawn(async move {
            realtime_create_call(&settings, REALTIME_TEST_SDP_OFFER).await
        });

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

        let call = tokio::spawn(async move {
            realtime_create_call(&settings, REALTIME_TEST_SDP_OFFER).await
        });

        let (mut sock, _addr) = listener.accept().await.expect("accept");
        let _req = read_http_request(&mut sock).await;
        // Whitespace-only body must be rejected as an empty SDP answer.
        write_http_response(
            &mut sock,
            "HTTP/1.1 201 Created",
            &[
                ("Location".to_owned(), "/v1/realtime/calls/rtc_ok".to_owned()),
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

        let call = tokio::spawn(async move {
            realtime_create_call(&settings, REALTIME_TEST_SDP_OFFER).await
        });

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
        let gathered_offer =
            "v=0\r\no=- 42 2 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
             a=mid:0\r\na=rtpmap:111 opus/48000/2\r\n\
             a=candidate:1 1 udp 2122252543 127.0.0.1 50000 typ host generation 0 ufrag abcd\r\n\
             a=candidate:1 2 udp 2122252543 127.0.0.1 50000 typ host generation 0 ufrag abcd\r\n\
             a=end-of-candidates\r\n";

        let offer_for_task = gathered_offer.to_owned();
        let call = tokio::spawn(async move {
            realtime_create_call(&settings, &offer_for_task).await
        });

        let (mut sock, _addr) = listener.accept().await.expect("accept");
        let req = read_http_request(&mut sock).await;
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/v1/realtime/calls");
        assert_eq!(req.header("openai-alpha"), Some("quicksilver=v2"));
        let content_type = req.header("content-type").expect("content-type");
        assert_eq!(content_type, "application/json", "content-type: {content_type:?}");
        let request_body = json_request_body(&req.body);
        // The SDP is forwarded VERBATIM, including every a=candidate line and
        // the end-of-candidates marker — no rewriting, no candidate stripping.
        // The browser's gathered offer is what the upstream sees.
        let sdp = request_body["sdp"].as_str().expect("sdp string");
        assert_eq!(sdp, gathered_offer, "gathered offer must be forwarded byte-for-byte");
        assert!(sdp.contains("a=candidate:"), "offer must carry ICE candidates: {sdp}");
        assert!(sdp.contains("a=end-of-candidates"), "offer must mark gather complete: {sdp}");
        // The session object is unchanged by the ICE fix.
        let session = &request_body["session"];
        assert_eq!(session["type"], "quicksilver");
        assert_eq!(session["audio"]["input"]["format"]["rate"], 24000);

        // Reply with a bare SDP answer + Location; the proxy returns both.
        let sdp_answer = "v=0\r\no=- 7 7 IN IP4 127.0.0.1\r\ns=-\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n";
        write_http_response(
            &mut sock,
            "HTTP/1.1 201 Created",
            &[
                ("Location".to_owned(), "/v1/realtime/calls/rtc_gather_001".to_owned()),
                ("Content-Type".to_owned(), "application/sdp".to_owned()),
            ],
            sdp_answer.as_bytes(),
        )
        .await;
        let result = call.await.expect("task join").expect("realtime_create_call ok");
        assert_eq!(result["sdp"], sdp_answer);
        assert_eq!(result["callId"], "rtc_gather_001");
        // Auth never leaks into the persisted call id or SDP.
        assert!(!result.to_string().contains(REALTIME_TEST_KEY), "{}", result);
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
        let call = tokio::spawn(async move {
            stt_transcribe(&settings, &stt_b64(&wav), "Audio/WAV").await
        });

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
        assert!(body.contains("RIFF"), "wav bytes embedded in the multipart body");
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
}
