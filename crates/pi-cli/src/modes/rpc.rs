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
    StreamingBehavior, TodoPhase,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io::{self, Write},
    path::Path,
    sync::{Arc, Mutex as StdMutex},
};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::mpsc;
const MAX_JSONL_FRAME_BYTES: usize = 4 * 1024 * 1024;
const JSONL_CHANNEL_CAPACITY: usize = 8;
const MAX_CONCURRENT_COMMANDS: usize = 16;

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
}
impl RpcCommand {
    fn id(&self) -> Option<String> {
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
            | Self::WorkflowPause { id, .. }
            | Self::WorkflowResume { id, .. }
            | Self::WorkflowCancel { id, .. }
            | Self::WorkflowIntegrate { id, .. }
            | Self::WorkflowRemove { id, .. } => id.clone(),
        }
    }
    const fn command_name(&self) -> &'static str {
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
            Self::WorkflowPause { .. } => "workflow_pause",
            Self::WorkflowResume { .. } => "workflow_resume",
            Self::WorkflowCancel { .. } => "workflow_cancel",
            Self::WorkflowIntegrate { .. } => "workflow_integrate",
            Self::WorkflowRemove { .. } => "workflow_remove",
        }
    }
}
impl RpcCommand {
    const fn runs_inline(&self) -> bool {
        matches!(
            self,
            Self::Abort { .. }
                | Self::AbortRetry { .. }
                | Self::AbortBash { .. }
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
                | Self::WorkflowPause { .. }
                | Self::WorkflowResume { .. }
                | Self::WorkflowCancel { .. }
                | Self::WorkflowIntegrate { .. }
                | Self::WorkflowRemove { .. }
        )
    }
}
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename = "extension_ui_response")]
struct RpcExtensionUiResponse {
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
    pub auto_compaction_enabled: bool,
    pub message_count: usize,
    pub pending_message_count: usize,
    pub todo_phases: Vec<TodoPhase>,
    pub goal: GoalState,
    pub runtime_settings: pi_coding::RuntimeSettingsState,
}
impl RpcSessionState {
    fn from_application(
        s: ApplicationState,
        runtime_settings: pi_coding::RuntimeSettingsState,
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
            auto_compaction_enabled: s.auto_compaction_enabled,
            message_count: s.message_count,
            pending_message_count: s.pending_message_count,
            todo_phases: s.todo_phases,
            goal: s.goal,
            runtime_settings,
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
    fn success(id: Option<String>, command: impl Into<String>, data: Option<Value>) -> Self {
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
struct RpcExtensionUiRequest {
    #[serde(rename = "type")]
    record_type: &'static str,
    id: String,
    #[serde(flatten)]
    request: Value,
}
#[derive(Clone, Debug)]
enum JsonlFrame {
    Line(Vec<u8>),
    Oversized,
    Unterminated,
}
#[derive(Clone, Debug)]
enum RpcInput {
    Command(RpcCommand),
    ExtensionUiResponse(RpcExtensionUiResponse),
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
    let settings = crate::settings_rpc::SettingsRpcState::default();
    // Application-owned manager bind (no independent manager open / set_host).
    let workflows = crate::workflow_rpc::WorkflowRpcState::for_application(&application);
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
                        Ok(RpcInput::Command(command)) if command.runs_inline() => {
                            let response =
                                handle_command(&application, &settings, &workflows, command).await;
                            write_shared_json(&output, &response)?;
                        }
                        Ok(RpcInput::Command(command)) if commands.len() >= MAX_CONCURRENT_COMMANDS => {
                            let response = RpcResponse::failure(
                                command.id(),
                                command.command_name(),
                                format!("too many concurrent RPC commands (limit {MAX_CONCURRENT_COMMANDS})"),
                            );
                            write_shared_json(&output, &response)?;
                        }
                        Ok(RpcInput::Command(command)) => {
                            let application = application.clone();
                            let settings = settings.clone();
                            let workflows = workflows.clone();
                            let output = output.clone();
                            commands.spawn(async move {
                                let response =
                                    handle_command(&application, &settings, &workflows, command)
                                        .await;
                                write_shared_json(&output, &response)
                            });
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
                            format!("RPC frame exceeds {MAX_JSONL_FRAME_BYTES} bytes"),
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
                Ok(ApplicationEvent::Agent(pi_agent::AgentEvent::MessageStart { message })) => {
                    write_shared_json(&output, &ApplicationEvent::Agent(pi_agent::AgentEvent::MessageStart { message: public_message(message) }))?
                }
                Ok(ApplicationEvent::Agent(pi_agent::AgentEvent::MessageEnd { message })) => {
                    write_shared_json(&output, &ApplicationEvent::Agent(pi_agent::AgentEvent::MessageEnd { message: public_message(message) }))?
                }
                Ok(ApplicationEvent::Workflow(event)) => {
                    let public = crate::workflow_rpc::project_workflow_event(&event);
                    write_shared_json(&output, &public)?
                }
                Ok(event) => write_shared_json(&output, &event)?,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => write_shared_json(
                    &output,
                    &RpcResponse::failure(None, "events", format!("application event stream lagged by {count} records")),
                )?,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            event = ui_events.recv() => match event {
                Ok(event) => {
                    if let Some(request) = ui_event_request(event)? {
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
                if buffer.len() == MAX_JSONL_FRAME_BYTES {
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
fn parse_input(line: &[u8]) -> std::result::Result<RpcInput, RpcResponse> {
    let value: Value = serde_json::from_slice(line).map_err(|e| {
        RpcResponse::failure(None, "parse", format!("Failed to parse command: {e}"))
    })?;
    let id = value.get("id").and_then(Value::as_str).map(str::to_owned);
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
            Ok(workflow) => Ok(RpcInput::Command(rpc_command_from_workflow(workflow))),
            Err(error) => Err(RpcResponse::failure(
                id,
                command,
                format!("Invalid command: {error}"),
            )),
        };
    }
    serde_json::from_value(value)
        .map(RpcInput::Command)
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
    };
    Ok(Some(RpcExtensionUiRequest {
        record_type: "extension_ui_request",
        id,
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
    };
    Ok(RpcExtensionUiRequest {
        record_type: "extension_ui_request",
        id,
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
            app.new_session_with_parent(parent_session.as_deref().map(Path::new))
                .await?;
            Ok(Some(json!({"cancelled":false})))
        }
        RpcCommand::GetState { .. } => Ok(Some(serde_json::to_value(
            RpcSessionState::from_application(app.state().await, app.runtime_settings_state()),
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
        RpcCommand::ExportHtml { output_path, .. } => Ok(Some(
            json!({"path":app.export_html(output_path.as_deref().map(Path::new))?.to_string_lossy()}),
        )),
        RpcCommand::SwitchSession { session_path, .. } => {
            app.switch_session(Path::new(&session_path)).await?;
            Ok(Some(json!({"cancelled":false})))
        }
        RpcCommand::Fork { entry_id, .. } => {
            app.fork_session(&entry_id).await?;
            Ok(Some(json!({"cancelled":false})))
        }
        RpcCommand::Clone { .. } => {
            app.clone_session().await?;
            Ok(Some(json!({"cancelled":false})))
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
                        "source": "builtin",
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
    }
}
fn public_message(message: Message) -> Message {
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
            json!({"type":"loop_create","interval":"5m","prompt":"check","fireImmediately":true,"durable":false}),
            json!({"type":"loop_update","taskId":"loop-1","interval":"10m","prompt":"check again"}),
            json!({"type":"loop_list"}),
            json!({"type":"loop_delete","taskId":"loop-1"}),
            json!({"type":"loop_cancel","taskId":"loop-1"}),
            json!({"type":"process_spawn","spec":{"argv":["printf","ok"],"cwd":"/tmp","env":{},"tty":false}}),
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
            json!({"type":"workflow_pause","workflowId":"wf-1"}),
            json!({"type":"workflow_resume","workflowId":"wf-1"}),
            json!({"type":"workflow_cancel","workflowId":"wf-1"}),
            json!({"type":"workflow_integrate","workflowId":"wf-1"}),
            json!({"type":"workflow_remove","workflowId":"wf-1"}),
        ];
        for f in fixtures {
            assert!(
                matches!(
                    parse_input(&serde_json::to_vec(&f).unwrap()),
                    Ok(RpcInput::Command(_))
                ),
                "{f}"
            );
        }
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
        let Ok(RpcInput::Command(c)) = parse_input(&second) else {
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
        let Ok(RpcInput::Command(RpcCommand::SetTodos { phases, .. })) = parse_input(&first) else {
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
        w.write_all(&vec![b'x'; MAX_JSONL_FRAME_BYTES + 1])
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
        let Ok(RpcInput::Command(command)) = parse_input(&valid) else {
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
            Ok(RpcInput::Command(RpcCommand::SettingsInspect { id })) if id.as_deref() == Some("after")
        ));
    }

    #[tokio::test]
    async fn set_todos_round_trips_through_application() {
        use pi_coding::{TodoItem, TodoPhase, TodoStatus};
        let app = build_todo_app("faux-rpc-todo-set", "faux-rpc-todo-set-api").await;
        let phases = vec![TodoPhase {
            name: "Plan".into(),
            tasks: vec![
                TodoItem { id: "task-root".into(), content: "root".into(), status: TodoStatus::InProgress, depends_on: Vec::new(), ready: true, blocked_by: Vec::new() },
                TodoItem { id: "task-do".into(), content: "do".into(), status: TodoStatus::Pending, depends_on: vec!["task-root".into()], ready: false, blocked_by: Vec::new() },
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
    async fn set_todos_with_workflow_id_fails_without_mutating_parent_when_missing() {
        let app = build_todo_app("faux-rpc-workflow-todo-missing", "faux-rpc-workflow-todo-missing-api").await;
        let phases = vec![pi_coding::TodoPhase {
            name: "Workflow".into(),
            tasks: vec![pi_coding::TodoItem {
                id: "scoped".into(), content: "scoped".into(), status: pi_coding::TodoStatus::Pending,
                depends_on: Vec::new(), ready: true, blocked_by: Vec::new(),
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
    async fn get_commands_includes_workflow() {
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
        app.cleanup().await;
    }
}
