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
    Application, ApplicationState, LoopCreateRequest, LoopUpdateRequest, ProcessId, ProcessKey,
    ProcessSignal, ProcessSpawnSpec, ProcessTerminalSize, StreamingBehavior, TodoPhase,
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
            | Self::ProcessWait { id, .. } => id.clone(),
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
        }
    }
}
impl RpcCommand {
    const fn runs_inline(&self) -> bool {
        matches!(
            self,
            Self::Abort { .. } | Self::AbortRetry { .. } | Self::AbortBash { .. }
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
    let mut events = application.subscribe();
    let mut ui_events = extension_ui.subscribe();
    let (lines_tx, mut lines_rx) = mpsc::channel(JSONL_CHANNEL_CAPACITY);
    let reader = tokio::spawn(read_jsonl(input, lines_tx));
    let output = Arc::new(StdMutex::new(output));
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
                            let response = handle_command(&application, command).await;
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
                            let output = output.clone();
                            commands.spawn(async move {
                                let response = handle_command(&application, command).await;
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
    use pi_coding::ExtensionUiRequest;
    let (id, request) = match event {
        ExtensionUiEvent::InteractionRequested { interaction } => {
            return ui_request(interaction.id, interaction.request).map(Some);
        }
        ExtensionUiEvent::Notification { notification } => (
            uuid::Uuid::new_v4().to_string(),
            ExtensionUiRequest::Notify {
                message: notification.message,
                level: notification.level,
            },
        ),
        ExtensionUiEvent::StatusChanged { item } => (
            uuid::Uuid::new_v4().to_string(),
            ExtensionUiRequest::Status {
                key: item.key,
                text: Some(item.text),
            },
        ),
        ExtensionUiEvent::StatusCleared { key, .. } => (
            uuid::Uuid::new_v4().to_string(),
            ExtensionUiRequest::Status { key, text: None },
        ),
        ExtensionUiEvent::WidgetChanged { item } => (
            uuid::Uuid::new_v4().to_string(),
            ExtensionUiRequest::Widget {
                key: item.key,
                lines: Some(item.lines),
                placement: item.placement,
            },
        ),
        ExtensionUiEvent::WidgetCleared { key, .. } => (
            uuid::Uuid::new_v4().to_string(),
            ExtensionUiRequest::Widget {
                key,
                lines: None,
                placement: pi_coding::UiWidgetPlacement::AboveEditor,
            },
        ),
        ExtensionUiEvent::TitleChanged { title, .. } => (
            uuid::Uuid::new_v4().to_string(),
            ExtensionUiRequest::Title { title },
        ),
        ExtensionUiEvent::EditorTextChanged { text, .. } => (
            uuid::Uuid::new_v4().to_string(),
            ExtensionUiRequest::SetEditorText { text },
        ),
        ExtensionUiEvent::ExtensionCleared { .. } => return Ok(None),
    };
    ui_request(id, request).map(Some)
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
    };
    Ok(RpcExtensionUiRequest {
        record_type: "extension_ui_request",
        id,
        request,
    })
}
async fn handle_command(app: &Application, c: RpcCommand) -> RpcResponse {
    let id = c.id();
    let name = c.command_name();
    match handle_command_inner(app, c).await {
        Ok(data) => RpcResponse::success(id, name, data),
        Err(e) => RpcResponse::failure(id, name, e.to_string()),
    }
}
async fn handle_command_inner(app: &Application, c: RpcCommand) -> Result<Option<Value>> {
    match c {
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
            app.set_thinking_level(level);
            Ok(None)
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
            app.set_thinking_level(level);
            Ok(Some(json!({"level":level})))
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
            let text = app.fork_session(&entry_id).await?;
            Ok(Some(json!({"text":text,"cancelled":false})))
        }
        RpcCommand::Clone { .. } => {
            app.clone_session().await?;
            Ok(Some(json!({"cancelled":false})))
        }
        RpcCommand::GetForkMessages { .. } => Ok(Some(json!({"messages":app.fork_messages()?}))),
        RpcCommand::GetEntries { since, .. } => Ok(Some(serde_json::to_value(
            app.session_entries(since.as_deref())?,
        )?)),
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
            let messages: Vec<Message> = app.messages();
            Ok(Some(json!({"messages":messages})))
        }
        RpcCommand::GetCommands { .. } => Ok(Some(json!({"commands":app.commands_catalog()}))),
        RpcCommand::SetTodos { phases, .. } => {
            let result = app.set_todos(phases)?;
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
    }
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
            "xhigh" | "max" => Some(ThinkingLevel::Xhigh),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
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
            json!({"type":"set_todos","phases":[]}),
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
                content: "x".into(),
                status: TodoStatus::InProgress,
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
    #[tokio::test]
    async fn set_todos_round_trips_through_application() {
        use pi_coding::{TodoItem, TodoPhase, TodoStatus};
        let app = build_todo_app("faux-rpc-todo-set", "faux-rpc-todo-set-api").await;
        let phases = vec![TodoPhase {
            name: "Plan".into(),
            tasks: vec![TodoItem {
                content: "do".into(),
                status: TodoStatus::InProgress,
            }],
        }];
        let r = handle_command(
            &app,
            RpcCommand::SetTodos {
                id: Some("t1".into()),
                phases,
            },
        )
        .await;
        assert!(r.success);
        assert_eq!(r.id.as_deref(), Some("t1"));
        assert_eq!(r.command, "set_todos");
        let d = r.data.expect("set_todos returns data");
        assert_eq!(d["phases"][0]["name"], "Plan");
        assert_eq!(d["phases"][0]["tasks"][0]["content"], "do");
        assert_eq!(d["phases"][0]["tasks"][0]["status"], "in_progress");
    }
    #[tokio::test]
    async fn get_state_exposes_todo_phases() {
        use pi_coding::{TodoItem, TodoPhase, TodoStatus};
        let app = build_todo_app("faux-rpc-todo-get", "faux-rpc-todo-get-api").await;
        app.set_todos(vec![TodoPhase {
            name: "Plan".into(),
            tasks: vec![TodoItem {
                content: "x".into(),
                status: TodoStatus::InProgress,
            }],
        }])
        .expect("set_todos");
        let r = handle_command(
            &app,
            RpcCommand::GetState {
                id: Some("s1".into()),
            },
        )
        .await;
        assert!(r.success);
        let d = r.data.expect("state data");
        assert_eq!(d["todoPhases"][0]["name"], "Plan");
        assert_eq!(d["todoPhases"][0]["tasks"][0]["status"], "in_progress");
    }
}
