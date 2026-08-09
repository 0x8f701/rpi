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
    io::{self, Write},
    path::Path,
    pin::Pin,
    sync::{Arc, Mutex as StdMutex},
};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{mpsc, Semaphore};
pub(crate) const MAX_RPC_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
const JSONL_CHANNEL_CAPACITY: usize = 8;
pub(crate) const MAX_CONCURRENT_COMMANDS: usize = 16;

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
    /// List the unified resume catalog (native + enabled foreign sessions),
    /// scoped to the session working directory, mirroring the TUI `/sessions`
    /// panel. Wire shape: `{ "type": "session_list", "id"?: string }`.
    SessionList {
        #[serde(default)]
        id: Option<String>,
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
            | Self::SessionList { id }
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
            | Self::CollabStart { id, .. }
            | Self::CollabStatus { id, .. }
            | Self::CollabStop { id, .. }
            | Self::CloseSession { id } => id.clone(),
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
            Self::CollabStart { .. } => "collab_start",
            Self::CollabStatus { .. } => "collab_status",
            Self::CollabStop { .. } => "collab_stop",
            Self::CloseSession { .. } => "close_session",
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
                | Self::CollabStop { .. }
                | Self::CloseSession { .. }
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
}

impl RpcSessionListRow {
    fn from_resume_row(row: crate::resume_catalog::ResumeSelectorRow) -> Self {
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
}

impl RpcDispatcher {
    #[must_use]
    pub(crate) fn new(application: Application) -> Self {
        Self {
            settings: crate::settings_rpc::SettingsRpcState::default(),
            workflows: crate::workflow_rpc::WorkflowRpcState::for_application(&application),
            application,
            command_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_COMMANDS)),
            side_chat: SideChatRpcState::default(),
        }
    }

    pub(crate) fn application(&self) -> &Application {
        &self.application
    }

    /// Dispatch a command with this dispatcher's full state. Side-chat
    /// commands live outside the settings/workflows handler because the tabs
    /// are controller state owned by the RPC session, not by the application.
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
        handle_command(
            &self.application,
            &self.settings,
            &self.workflows,
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
    c: RpcCommand,
) -> RpcResponse {
    let id = c.id();
    let name = c.command_name();
    match handle_command_inner(app, settings, workflows, c).await {
        Ok(data) => RpcResponse::success(id, name, data),
        Err(e) => RpcResponse::failure(id, name, e.to_string()),
    }
}
async fn handle_command_inner(
    app: &Application,
    settings: &crate::settings_rpc::SettingsRpcState,
    workflows: &crate::workflow_rpc::WorkflowRpcState,
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
            app.steer(message, images).await;
            Ok(None)
        }
        RpcCommand::FollowUp {
            message, images, ..
        } => {
            app.follow_up(message, images).await;
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
        RpcCommand::SessionList { .. } => {
            let session = app.session();
            let catalog = pi_coding::SessionCatalog::from_env()?
                .with_native_session_root(session.session_dir());
            let rows = crate::resume_catalog::load_resume_catalog(
                &catalog,
                &crate::resume_catalog::ResumeCatalogRequest {
                    sources: crate::resume_catalog::effective_resume_sources(app),
                    cwd_scope: Some(session.cwd().to_path_buf()),
                    ..crate::resume_catalog::ResumeCatalogRequest::default()
                },
            )?;
            // The catalog reads persisted session files; the live recorder's
            // name is authoritative and may not have flushed yet (a fresh
            // session with no assistant turn keeps appends in memory), so
            // overlay it onto the current session's row.
            let live_name = session.session_name();
            let live_id = session.recorder_info().map(|(id, _)| id);
            let sessions = rows
                .rows
                .into_iter()
                .map(|row| {
                    let mut row = RpcSessionListRow::from_resume_row(row);
                    if live_name.is_some() && Some(row.session_id.as_str()) == live_id.as_deref() {
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
            let commands = crate::interactive_commands::visible_catalog()
                .into_iter()
                .map(|command| {
                    json!({
                        "name": command.name,
                        "description": command.description,
                        "source": command_source_str(command.source),
                    })
                })
                .collect::<Vec<_>>();
            Ok(Some(json!({"commands":commands})))
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
        // The listen transport intercepts collaboration lifecycle commands;
        // stdio RPC has no room registry or reachable listener origin.
        RpcCommand::CollabStart { .. }
        | RpcCommand::CollabStatus { .. }
        | RpcCommand::CollabStop { .. } => {
            bail!("collaboration room commands require the listen control plane")
        }
        // The Web control plane's session runtime manager intercepts
        // close_session before any dispatcher sees it; stdio RPC has one
        // Application and nothing to close.
        RpcCommand::CloseSession { .. } => {
            bail!("close_session is only supported on the Web control plane")
        }
    }
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
            json!({"type":"workflow_pause","workflowId":"wf-1"}),
            json!({"type":"workflow_integrate","workflowId":"wf-1"}),
            json!({"type":"process_wait","processId":"00000000-0000-7000-8000-000000000000"}),
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
            RpcCommand::SessionList { id: Some("list".into()) },
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
            RpcCommand::SessionList { id: Some("list2".into()) },
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
    async fn get_commands_exactly_projects_ordered_primary_catalog() {
        let (_cwd, app) = build_command_catalog_app().await;
        let expected = crate::interactive_commands::visible_catalog()
            .iter()
            .map(|command| {
                json!({
                    "name": command.name,
                    "description": command.description,
                    "source": command_source_str(command.source),
                })
            })
            .collect::<Vec<_>>();
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
        let data = response.data.expect("get_commands data");
        assert_eq!(data, json!({ "commands": expected }));

        let commands = data["commands"].as_array().expect("commands array");
        let names = commands
            .iter()
            .map(|command| command["name"].as_str().expect("command name"))
            .collect::<Vec<_>>();
        assert_eq!(names, crate::interactive_commands::PRIMARY_COMMAND_NAMES);
        assert!(commands.iter().all(|command| command["source"] == "builtin"));
        assert!(!names.contains(&"prompt-only"));
        assert!(!names.contains(&"skill:catalog-skill"));
        assert!(!names.contains(&"extension-only"));
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
        app.steer("first steer".to_owned(), Vec::new()).await;
        app.follow_up("second follow".to_owned(), Vec::new()).await;

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
        let factory: pi_coding::ChildSessionFactory = Arc::new(|request| {
            let stream_fn: pi_agent::StreamFn = Arc::new(|model, _context, _options| {
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
}
