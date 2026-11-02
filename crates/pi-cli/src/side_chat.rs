//! Built-in `/btw` side-chat controller.
//!
//! Owns a detached parallel agent forked from the main conversation, with its
//! own transcript, editor, streaming state, tool mode, and cleanup. Side I/O
//! never enters the main transcript, status history, or structured stdout.

use std::collections::{HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use parking_lot::Mutex;
use pi_agent::{Agent, AgentEvent, AgentTool, ThinkingLevel, ToolCapability};
use pi_ai::{AssistantMessageEvent, ContentBlock, Message, Model};
use pi_coding::{
    Application, ApplicationRuntimeCandidate, ResourceDiscovery, Session, SessionOptions,
    SideChatFork, SideChatMainPeek, ToolSelection, WorkspaceRoots, create_peek_main_tool,
    filter_tools_by_capabilities, tools_include_mutation,
};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Default side-chat tool mode is read-only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SideChatToolMode {
    #[default]
    ReadOnly,
    Edit,
}

impl SideChatToolMode {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Edit => "edit",
        }
    }

    #[must_use]
    pub const fn is_edit(self) -> bool {
        matches!(self, Self::Edit)
    }
}

/// Renderable transcript row local to the side panel.
#[derive(Clone, Debug)]
pub struct SideChatEntry {
    pub id: String,
    pub kind: SideChatEntryKind,
    pub role: SideChatRole,
    pub text: String,
    pub tool_name: Option<String>,
    pub tool_args: Option<Value>,
    pub output: Option<String>,
    pub thinking: Option<String>,
    pub is_error: bool,
    pub is_partial: bool,
}

impl Default for SideChatEntry {
    fn default() -> Self {
        Self {
            id: String::new(),
            kind: SideChatEntryKind::System,
            role: SideChatRole::System,
            text: String::new(),
            tool_name: None,
            tool_args: None,
            output: None,
            thinking: None,
            is_error: false,
            is_partial: false,
        }
    }
}

impl SideChatEntry {
    fn from_role(id: String, role: SideChatRole, text: String) -> Self {
        Self {
            id,
            kind: SideChatEntryKind::from_role(role),
            role,
            text,
            ..Self::default()
        }
    }

    /// Wire object for `side_chat_list` / new / switch / close / prompt.
    #[must_use]
    pub fn to_wire_json(&self) -> Value {
        json!({
            "id": self.id,
            "kind": match self.kind {
                SideChatEntryKind::User => "user",
                SideChatEntryKind::Assistant => "assistant",
                SideChatEntryKind::Tool => "tool",
                SideChatEntryKind::System => "system",
                SideChatEntryKind::Thinking => "thinking",
            },
            "role": match self.role {
                SideChatRole::User => "user",
                SideChatRole::Assistant => "assistant",
                SideChatRole::Tool => "tool",
                SideChatRole::System => "system",
            },
            "text": self.text,
            "toolName": self.tool_name,
            "toolArgs": self.tool_args,
            "output": self.output,
            "thinking": self.thinking,
            "isError": self.is_error,
            "isPartial": self.is_partial,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SideChatEntryKind {
    User,
    Assistant,
    Tool,
    System,
    Thinking,
}

impl SideChatEntryKind {
    fn from_role(role: SideChatRole) -> Self {
        match role {
            SideChatRole::User => Self::User,
            SideChatRole::Assistant => Self::Assistant,
            SideChatRole::Tool => Self::Tool,
            SideChatRole::System => Self::System,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SideChatRole {
    User,
    Assistant,
    Tool,
    System,
}

/// Key/paste handling outcome for the side-chat overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SideChatAction {
    /// Event consumed; keep overlay open.
    Handled,
    /// Hide the overlay but keep the controller alive for reopen.
    CloseOverlay,
    /// Event not handled by side chat.
    Ignored,
}

/// Async follow-up requested after a key is accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SideChatAsyncRequest {
    None,
    ToggleTools,
    Abort,
    Refork,
    Clear,
}

/// Best-effort tracker of main-agent Write tool path arguments.
///
/// Only records explicit `path` args on Write-capable tools. Never parses shell
/// command strings. This is advisory overlap signal, not precise file locking.
#[derive(Clone, Debug, Default)]
pub struct FileActivityTracker {
    written: HashSet<PathBuf>,
}

impl FileActivityTracker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn track_path(&mut self, path: impl Into<PathBuf>) {
        let path = path.into();
        if !path.as_os_str().is_empty() {
            self.written.insert(path);
        }
    }

    #[must_use]
    pub fn has_any_writes(&self) -> bool {
        !self.written.is_empty()
    }

    #[must_use]
    pub fn has_written(&self, path: &Path) -> bool {
        self.written.contains(path)
    }

    /// Observe a main-agent tool start. Only Write-capable tools contribute paths
    /// from explicit `path` arguments (never bash command text).
    pub fn observe_tool_start(&mut self, capability: ToolCapability, arguments: &Value) {
        if !matches!(capability, ToolCapability::Write) {
            return;
        }
        if let Some(path) = arguments
            .get("path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|path| !path.is_empty())
        {
            self.track_path(PathBuf::from(path));
        }
    }

    #[must_use]
    pub fn written_paths(&self) -> Vec<PathBuf> {
        let mut paths = self.written.iter().cloned().collect::<Vec<_>>();
        paths.sort();
        paths
    }
}

#[derive(Clone, Debug)]
enum SideChatInternalEvent {
    Agent(AgentEvent),
    PromptFailed(String),
}

/// Persistent side-chat session controller.
///
/// Closing the overlay does not drop this value; TUI exit must call [`SideChatController::shutdown`].
pub struct SideChatController {
    agent: Agent,
    fork: SideChatFork,
    /// Parent prompt plus the one standard side-chat suffix, captured before
    /// any tool contract. Tool changes rebuild from this instead of scanning
    /// the live prompt.
    side_prompt_base: String,
    tool_mode: SideChatToolMode,
    entries: Vec<SideChatEntry>,
    editor_lines: Vec<String>,
    editor_row: usize,
    editor_column: usize,
    streaming_text: String,
    is_streaming: bool,
    status: String,
    show_edit_warning: bool,
    scroll: usize,
    file_tracker: Arc<Mutex<FileActivityTracker>>,
    event_tx: mpsc::UnboundedSender<SideChatInternalEvent>,
    event_rx: mpsc::UnboundedReceiver<SideChatInternalEvent>,
    prompt_task: Option<JoinHandle<()>>,
    /// Subscription keep-alive (dropped on shutdown).
    _subscription: Option<pi_agent::Subscription>,
    main_application: Application,
    /// Published isolated application whose generation governs every side tool.
    side_application: Application,
    /// Private recording namespace retained for the isolated application lifetime.
    side_session_dir: PathBuf,
    warned_first_mutation: bool,
    /// In-flight tool calls awaiting finalization, keyed by tool call id to
    /// the index of their streamed partial transcript row. Bounded by the
    /// number of concurrently executing tools; cleared when finalized or when
    /// the transcript is reset.
    pending_tool_calls: HashMap<String, usize>,
    /// Tool call ids that already produced a final transcript row via
    /// `ToolExecutionEnd`. The trailing `MessageEnd::ToolResult` echo is
    /// suppressed for these. Bounded by removal on consume and cleared on
    /// turn boundaries / transcript reset.
    finalized_tool_calls: HashSet<String>,
    entry_seq: u64,
    pending_thinking_index: Option<usize>,
    pending_assistant_index: Option<usize>,
    /// One submitted turn already published its display row. The next
    /// `MessageEnd::User` is that turn's model prompt and is not a second
    /// display row, even when the texts differ. Cleared when consumed or when
    /// the turn/transcript ends.
    pending_user_echo: bool,
}

impl SideChatController {
    /// Fork a new side-chat agent from the main application.
    pub async fn fork_from(application: &Application) -> Result<Self> {
        let fork = application.fork_side_chat().await?;
        Self::from_fork(application, fork).await
    }

    async fn from_fork(application: &Application, fork: SideChatFork) -> Result<Self> {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let file_tracker = Arc::new(Mutex::new(FileActivityTracker::new()));
        let (side_application, side_session_dir) =
            Self::build_isolated_application(application, &fork).await?;
        let tools = match Self::build_tools(
            application,
            &side_application,
            &fork,
            SideChatToolMode::ReadOnly,
            file_tracker.clone(),
        ) {
            Ok(tools) => tools,
            Err(error) => {
                side_application.cleanup().await;
                let _ = std::fs::remove_dir_all(&side_session_dir);
                return Err(error);
            }
        };
        let agent = Application::create_side_chat_agent(&fork, Vec::new());
        let side_prompt_base = agent.state().await.system_prompt;
        install_side_tools(&agent, &side_prompt_base, tools).await;
        let subscription_tx = event_tx.clone();
        let subscription = agent
            .subscribe_simple(move |event| {
                let tx = subscription_tx.clone();
                async move {
                    let _ = tx.send(SideChatInternalEvent::Agent(event));
                    Ok(())
                }
            })
            .await;
        Ok(Self {
            agent,
            fork,
            side_prompt_base,
            tool_mode: SideChatToolMode::ReadOnly,
            entries: Vec::new(),
            editor_lines: vec![String::new()],
            editor_row: 0,
            editor_column: 0,
            streaming_text: String::new(),
            is_streaming: false,
            status: "Side chat · read-only tools · Ctrl+T edit mode · Esc close".to_owned(),
            show_edit_warning: false,
            scroll: 0,
            file_tracker,
            event_tx,
            event_rx,
            prompt_task: None,
            _subscription: Some(subscription),
            main_application: application.clone(),
            side_application,
            side_session_dir,
            warned_first_mutation: false,
            pending_tool_calls: HashMap::new(),
            finalized_tool_calls: HashSet::new(),
            entry_seq: 0,
            pending_thinking_index: None,
            pending_assistant_index: None,
            pending_user_echo: false,
        })
    }

    async fn build_isolated_application(
        application: &Application,
        fork: &SideChatFork,
    ) -> Result<(Application, PathBuf)> {
        let main_session = application.session();
        let child_options = main_session.child_session_options_snapshot();
        let side_session_dir =
            std::env::temp_dir().join(format!("rpi-side-chat-{}", uuid::Uuid::now_v7().simple()));
        std::fs::create_dir(&side_session_dir)?;
        let result = async {
            let resources = main_session
                .resource_manager()
                .ok_or_else(|| anyhow!("main application has no published resource manager"))?
                .rebuild_for_cwd(&fork.cwd)?;
            let workspace = child_options.workspace;
            let session = Session::new_with_additional_tools_filtered_discovery_workspace_and_uri(
                SessionOptions {
                    model: fork.model.clone(),
                    cwd: fork.cwd.clone(),
                    system_prompt: fork.system_prompt.clone(),
                    thinking_level: fork.thinking_level,
                    api_key: fork.api_key.clone(),
                    compaction: None,
                    stream_options: fork.stream_options.clone(),
                    tools: None,
                    before_tool_call: None,
                    after_tool_call: None,
                    stream_fn: Some(fork.stream_fn.clone()),
                    auth_resolver: fork.auth_resolver.clone(),
                    auth_manager: child_options.auth_manager,
                },
                Vec::new(),
                ToolSelection::default(),
                ResourceDiscovery::Disabled,
                workspace,
                None,
            )?;
            session.set_session_dir(side_session_dir.clone());
            session.set_permission_rules(fork.permission_rules.clone());
            session.attach_resources(resources).await?;
            Application::from_runtime_candidate(ApplicationRuntimeCandidate::new(session)).await
        }
        .await;
        match result {
            Ok(application) => Ok((application, side_session_dir)),
            Err(error) => {
                let _ = std::fs::remove_dir_all(&side_session_dir);
                Err(error)
            }
        }
    }

    fn build_tools(
        application: &Application,
        side_application: &Application,
        fork: &SideChatFork,
        mode: SideChatToolMode,
        tracker: Arc<Mutex<FileActivityTracker>>,
    ) -> Result<Vec<AgentTool>> {
        let mut tools = side_application
            .get_all_tools()
            .into_iter()
            .filter(|tool| {
                !matches!(
                    tool.name.as_str(),
                    "todo" | "goal" | "task" | "hub" | "process" | "extension"
                )
            })
            .collect::<Vec<_>>();
        if mode == SideChatToolMode::Edit {
            tools = wrap_mutation_tools_with_warning(tools, tracker);
        }
        if mode == SideChatToolMode::ReadOnly {
            tools = filter_tools_by_capabilities(tools, &[ToolCapability::Read]);
        }
        let leaf_id = fork.leaf_id.clone();
        let fork_message_count = fork.messages.len();
        let main = application.clone();
        tools.push(create_peek_main_tool(move |since_fork, since| {
            if let Some(since) = since {
                return main.peek_main_history(Some(&since));
            }
            if since_fork {
                if let Some(leaf_id) = leaf_id.as_deref() {
                    return main.peek_main_history(Some(leaf_id));
                }
                let mut peek = main.peek_main_history(None)?;
                peek.messages = peek.messages.into_iter().skip(fork_message_count).collect();
                return Ok(peek);
            }
            main.peek_main_history(None)
        }));
        if mode == SideChatToolMode::ReadOnly && tools_include_mutation(&tools) {
            return Err(anyhow!(
                "side-chat read-only tool set unexpectedly contains Write/Exec capabilities"
            ));
        }
        Ok(tools)
    }

    #[must_use]
    pub fn tool_mode(&self) -> SideChatToolMode {
        self.tool_mode
    }

    #[must_use]
    pub fn is_streaming(&self) -> bool {
        self.is_streaming
    }

    #[must_use]
    pub fn show_edit_warning(&self) -> bool {
        self.show_edit_warning
    }

    #[must_use]
    pub fn status(&self) -> &str {
        &self.status
    }

    #[must_use]
    pub fn entries(&self) -> &[SideChatEntry] {
        &self.entries
    }

    fn alloc_entry_id(&mut self) -> String {
        self.entry_seq += 1;
        format!("sc-{}", self.entry_seq)
    }

    fn push_entry(&mut self, mut entry: SideChatEntry) -> usize {
        if entry.id.is_empty() {
            entry.id = self.alloc_entry_id();
        }
        self.entries.push(entry);
        self.entries.len() - 1
    }

    fn push_role(&mut self, role: SideChatRole, text: String, is_error: bool, is_partial: bool) -> usize {
        let id = self.alloc_entry_id();
        let mut entry = SideChatEntry::from_role(id, role, text);
        entry.is_error = is_error;
        entry.is_partial = is_partial;
        self.push_entry(entry)
    }

    fn finalize_thinking_entry(&mut self, thinking: String) {
        if thinking.is_empty() {
            if let Some(index) = self.pending_thinking_index
                && let Some(entry) = self.entries.get_mut(index)
            {
                entry.is_partial = false;
            }
            return;
        }
        if let Some(index) = self.pending_thinking_index {
            if let Some(entry) = self.entries.get_mut(index) {
                entry.thinking = Some(thinking.clone());
                entry.text = thinking;
                entry.is_partial = false;
            }
            return;
        }
        let id = self.alloc_entry_id();
        let index = self.push_entry(SideChatEntry {
            id,
            kind: SideChatEntryKind::Thinking,
            role: SideChatRole::Assistant,
            text: thinking.clone(),
            thinking: Some(thinking),
            is_partial: false,
            ..SideChatEntry::default()
        });
        self.pending_thinking_index = Some(index);
    }

    fn settle_streaming_assistant(&mut self, text: String, is_partial: bool) {
        self.streaming_text.clear();
        match self.pending_assistant_index.take() {
            Some(index) => {
                if let Some(entry) = self.entries.get_mut(index) {
                    entry.text = text;
                    entry.is_partial = is_partial;
                }
            }
            None if !text.is_empty() => {
                self.push_role(SideChatRole::Assistant, text, false, is_partial);
            }
            None => {}
        }
    }

    fn append_streaming_text(&mut self, delta: &str) {
        self.streaming_text.push_str(delta);
        if let Some(index) = self.pending_assistant_index {
            if let Some(entry) = self.entries.get_mut(index) {
                entry.text = self.streaming_text.clone();
                entry.is_partial = true;
            }
            return;
        }
        let id = self.alloc_entry_id();
        let index = self.push_entry(SideChatEntry {
            id,
            kind: SideChatEntryKind::Assistant,
            role: SideChatRole::Assistant,
            text: self.streaming_text.clone(),
            is_partial: true,
            ..SideChatEntry::default()
        });
        self.pending_assistant_index = Some(index);
    }

    #[must_use]
    pub fn streaming_text(&self) -> &str {
        &self.streaming_text
    }

    #[must_use]
    pub fn editor_text(&self) -> String {
        self.editor_lines.join("\n")
    }

    #[must_use]
    pub fn editor_lines(&self) -> &[String] {
        &self.editor_lines
    }

    #[must_use]
    pub fn editor_cursor(&self) -> (usize, usize) {
        (self.editor_row, self.editor_column)
    }

    #[must_use]
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    #[must_use]
    pub fn fork_leaf_id(&self) -> Option<&str> {
        self.fork.leaf_id.as_deref()
    }

    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.fork.cwd
    }

    #[must_use]
    pub fn model(&self) -> &Model {
        &self.fork.model
    }

    #[must_use]
    pub fn thinking_level(&self) -> ThinkingLevel {
        self.fork.thinking_level
    }

    /// Drain agent events into local transcript/streaming state.
    /// Side events must never be forwarded to main.
    pub fn poll_events(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.event_rx.try_recv() {
            changed = true;
            match event {
                SideChatInternalEvent::Agent(event) => self.apply_agent_event(event),
                SideChatInternalEvent::PromptFailed(message) => {
                    self.is_streaming = false;
                    self.streaming_text.clear();
                    self.pending_user_echo = false;
                    self.push_role(SideChatRole::System, message, true, false);
                    self.status = "Side chat error".to_owned();
                }
            }
        }
        if let Some(handle) = self.prompt_task.as_ref()
            && handle.is_finished()
        {
            self.prompt_task = None;
        }
        changed
    }

    pub(crate) fn apply_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::AgentStart => {
                self.is_streaming = true;
                self.streaming_text.clear();
                self.pending_assistant_index = None;
                self.status = "Side chat thinking…".to_owned();
            }
            AgentEvent::TurnStart => {
                self.pending_tool_calls.clear();
                self.finalized_tool_calls.clear();
                self.pending_thinking_index = None;
            }
            AgentEvent::MessageUpdate {
                assistant_message_event: AssistantMessageEvent::TextDelta { delta, .. },
                ..
            } => {
                self.append_streaming_text(&delta);
            }
            AgentEvent::MessageUpdate {
                assistant_message_event: AssistantMessageEvent::ThinkingStart { .. },
                ..
            } => {
                if self.pending_thinking_index.is_some() {
                    return;
                }
                let id = self.alloc_entry_id();
                let index = self.push_entry(SideChatEntry {
                    id,
                    kind: SideChatEntryKind::Thinking,
                    role: SideChatRole::Assistant,
                    text: String::new(),
                    thinking: Some(String::new()),
                    is_partial: true,
                    ..SideChatEntry::default()
                });
                self.pending_thinking_index = Some(index);
            }
            AgentEvent::MessageUpdate {
                assistant_message_event: AssistantMessageEvent::ThinkingDelta { delta, .. },
                ..
            } => {
                if self.pending_thinking_index.is_none() {
                    let id = self.alloc_entry_id();
                    let index = self.push_entry(SideChatEntry {
                        id,
                        kind: SideChatEntryKind::Thinking,
                        role: SideChatRole::Assistant,
                        text: String::new(),
                        thinking: Some(String::new()),
                        is_partial: true,
                        ..SideChatEntry::default()
                    });
                    self.pending_thinking_index = Some(index);
                }
                if let Some(index) = self.pending_thinking_index
                    && let Some(entry) = self.entries.get_mut(index)
                {
                    entry.thinking.get_or_insert_with(String::new).push_str(&delta);
                    entry.text = entry.thinking.clone().unwrap_or_default();
                }
            }
            AgentEvent::MessageUpdate {
                assistant_message_event: AssistantMessageEvent::ThinkingEnd { content, .. },
                ..
            } => {
                self.finalize_thinking_entry(content);
            }
            AgentEvent::MessageEnd { message } => {
                match message {
                    Message::User(user) => {
                        if !std::mem::take(&mut self.pending_user_echo) {
                            let text = content_text(&user.content);
                            self.push_role(SideChatRole::User, text, false, false);
                        }
                    }
                    Message::Assistant(assistant) => {
                        let thinking = content_thinking(&assistant.content);
                        self.finalize_thinking_entry(thinking);
                        self.pending_thinking_index = None;
                        let text = content_text(&assistant.content);
                        let streamed = if text.is_empty() {
                            std::mem::take(&mut self.streaming_text)
                        } else {
                            self.streaming_text.clear();
                            text
                        };
                        if streamed.is_empty() {
                            if let Some(index) = self.pending_assistant_index.take()
                                && let Some(entry) = self.entries.get_mut(index)
                            {
                                entry.is_partial = false;
                            }
                        } else {
                            self.settle_streaming_assistant(streamed, false);
                        }
                    }
                    Message::ToolResult(result) => {
                        if !self.finalized_tool_calls.remove(&result.tool_call_id) {
                            let output = content_text(&result.content);
                            let id = self.alloc_entry_id();
                            self.push_entry(SideChatEntry {
                                id,
                                kind: SideChatEntryKind::Tool,
                                role: SideChatRole::Tool,
                                text: format!("[{}] {output}", result.tool_name),
                                tool_name: Some(result.tool_name),
                                output: Some(output),
                                is_error: result.is_error,
                                ..SideChatEntry::default()
                            });
                        }
                    }
                    _ => {}
                }
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                arguments,
            } => {
                if self.tool_mode.is_edit() && !self.warned_first_mutation {
                    let capability = self
                        .side_application
                        .get_tool_definition(&tool_name)
                        .map(|tool| tool.capability)
                        .unwrap_or(ToolCapability::Read);
                    if matches!(capability, ToolCapability::Write | ToolCapability::Exec) {
                        self.warned_first_mutation = true;
                        self.push_role(
                            SideChatRole::System,
                            "⚠ Edit mode: Write/Exec tools can conflict with the main agent. Coordinate file changes carefully.".to_owned(),
                            true,
                            false,
                        );
                    }
                }
                let summary = tool_start_summary(&tool_name, &arguments);
                let id = self.alloc_entry_id();
                let index = self.push_entry(SideChatEntry {
                    id,
                    kind: SideChatEntryKind::Tool,
                    role: SideChatRole::Tool,
                    text: summary,
                    tool_name: Some(tool_name),
                    tool_args: Some(arguments),
                    is_partial: true,
                    ..SideChatEntry::default()
                });
                self.pending_tool_calls.insert(tool_call_id, index);
            }
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                partial_result,
                ..
            } => {
                if let Some(index) = self.pending_tool_calls.get(&tool_call_id).copied()
                    && let Some(entry) = self.entries.get_mut(index)
                    && entry.role == SideChatRole::Tool
                    && entry.is_partial
                {
                    let output = content_text(&partial_result.content);
                    entry.text = format!("[{tool_name}] {output}");
                    entry.output = Some(output);
                    if entry.tool_name.is_none() {
                        entry.tool_name = Some(tool_name);
                    }
                }
            }
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => {
                let output = content_text(&result.content);
                let finalized_in_place = self
                    .pending_tool_calls
                    .remove(&tool_call_id)
                    .and_then(|index| self.entries.get_mut(index))
                    .is_some_and(|entry| {
                        if entry.role != SideChatRole::Tool || !entry.is_partial {
                            return false;
                        }
                        entry.text = format!("[{tool_name}] {output}");
                        entry.tool_name = Some(tool_name.clone());
                        entry.output = Some(output.clone());
                        entry.is_error = is_error;
                        entry.is_partial = false;
                        true
                    });
                if !finalized_in_place {
                    let id = self.alloc_entry_id();
                    self.push_entry(SideChatEntry {
                        id,
                        kind: SideChatEntryKind::Tool,
                        role: SideChatRole::Tool,
                        text: format!("[{tool_name}] {output}"),
                        tool_name: Some(tool_name),
                        output: Some(output),
                        is_error,
                        ..SideChatEntry::default()
                    });
                }
                self.finalized_tool_calls.insert(tool_call_id);
            }
            AgentEvent::AgentEnd { .. } => {
                self.is_streaming = false;
                self.pending_user_echo = false;
                if !self.streaming_text.is_empty() {
                    let text = std::mem::take(&mut self.streaming_text);
                    self.settle_streaming_assistant(text, false);
                } else if let Some(index) = self.pending_assistant_index.take()
                    && let Some(entry) = self.entries.get_mut(index)
                {
                    entry.is_partial = false;
                }
                if let Some(index) = self.pending_thinking_index.take()
                    && let Some(entry) = self.entries.get_mut(index)
                {
                    entry.is_partial = false;
                }
                self.pending_tool_calls.clear();
                self.finalized_tool_calls.clear();
                self.status = format!(
                    "Side chat · {} tools · Ctrl+T toggle · Esc close",
                    self.tool_mode.label()
                );
            }
            _ => {}
        }
    }

    /// Observe main application agent events for advisory file-overlap tracking.
    pub fn observe_main_agent_event(&self, event: &AgentEvent) {
        if let AgentEvent::ToolExecutionStart {
            tool_name,
            arguments,
            ..
        } = event
        {
            let capability = self
                .main_application
                .get_tool_definition(tool_name)
                .map(|tool| tool.capability)
                .unwrap_or(ToolCapability::Read);
            self.file_tracker
                .lock()
                .observe_tool_start(capability, arguments);
        }
    }

    /// Submit a side-chat prompt. Never writes to main session/transcript.
    ///
    /// The transcript row and the model prompt are the same text. Callers
    /// that show different text use [`Self::submit_displayed_prompt`].
    pub fn submit_prompt(&mut self, prompt: impl Into<String>, images: Vec<ContentBlock>) {
        self.submit_displayed_prompt(prompt, images, None);
    }

    /// Submit one prompt. `display` is the transcript row; `None` shows the
    /// model text. The model prompt is captured before the row is published
    /// and is never rewritten afterward.
    pub fn submit_displayed_prompt(
        &mut self,
        prompt: impl Into<String>,
        images: Vec<ContentBlock>,
        display: Option<&str>,
    ) {
        let prompt = prompt.into();
        let trimmed = prompt.trim();
        let images: Vec<ContentBlock> = images
            .into_iter()
            .filter(|block| matches!(block, ContentBlock::Image { .. }))
            .collect();
        if (trimmed.is_empty() && images.is_empty()) || self.is_streaming {
            return;
        }
        let shown = display.unwrap_or(trimmed);
        self.push_role(SideChatRole::User, shown.to_owned(), false, false);
        self.clear_editor();
        self.is_streaming = true;
        self.pending_user_echo = true;
        self.streaming_text.clear();
        self.pending_assistant_index = None;
        self.status = "Side chat thinking…".to_owned();
        let agent = self.agent.clone();
        let tx = self.event_tx.clone();
        let prompt = trimmed.to_owned();
        self.prompt_task = Some(tokio::spawn(async move {
            let result = if images.is_empty() {
                agent.prompt(prompt).await
            } else {
                let mut content = Vec::new();
                if !prompt.is_empty() {
                    content.push(ContentBlock::text(prompt));
                }
                content.extend(images);
                agent
                    .prompt_messages(vec![Message::User(pi_ai::UserMessage {
                        content,
                        timestamp: pi_ai::now_millis(),
                    })])
                    .await
            };
            if let Err(error) = result {
                let _ = tx.send(SideChatInternalEvent::PromptFailed(format!(
                    "Side chat failed: {error:#}"
                )));
            }
        }));
    }

    /// Explicit Ctrl+T toggle between read-only and edit tool sets.
    pub async fn toggle_tool_mode(&mut self) -> Result<()> {
        if self.is_streaming || self.agent.state().await.is_streaming {
            self.status =
                "Side chat is streaming; abort the current turn before changing tool mode"
                    .to_owned();
            return Ok(());
        }
        let next = match self.tool_mode {
            SideChatToolMode::ReadOnly => SideChatToolMode::Edit,
            SideChatToolMode::Edit => SideChatToolMode::ReadOnly,
        };
        let tools = Self::build_tools(
            &self.main_application,
            &self.side_application,
            &self.fork,
            next,
            self.file_tracker.clone(),
        )?;
        install_side_tools(&self.agent, &self.side_prompt_base, tools).await;
        self.tool_mode = next;
        self.show_edit_warning = next.is_edit();
        self.warned_first_mutation = false;
        let main_writes = self.file_tracker.lock().written_paths();
        self.status = if next.is_edit() {
            "EDIT MODE · write/exec enabled · file overlap risk with main agent · Ctrl+T for read-only"
                .to_owned()
        } else {
            "Side chat · read-only tools · Ctrl+T edit mode · Esc close".to_owned()
        };
        let warning = if next.is_edit() {
            if main_writes.is_empty() {
                "⚠ Edit mode enabled. Write/Exec tools are active and may conflict with the main agent. Coordinate carefully.".to_owned()
            } else {
                format!(
                    "⚠ Edit mode enabled. Write/Exec tools are active. Main agent has written {} path(s) this session (advisory): {}. Coordinate carefully.",
                    main_writes.len(),
                    main_writes
                        .iter()
                        .take(5)
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        } else {
            "Read-only mode restored. Write/Exec tools disabled.".to_owned()
        };
        self.push_role(SideChatRole::System, warning, next.is_edit(), false);
        Ok(())
    }

    /// Abort the active turn, await full agent idleness, and consume queued
    /// terminal events before any reset/refork can expose the next generation.
    pub async fn abort_streaming(&mut self) {
        let was_streaming = self.is_streaming || self.agent.state().await.is_streaming;
        if was_streaming {
            self.agent.abort().await;
        }
        if let Some(handle) = self.prompt_task.take() {
            handle.abort();
            let _ = handle.await;
        }
        self.agent.wait_for_idle().await;
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                SideChatInternalEvent::Agent(AgentEvent::MessageUpdate {
                    assistant_message_event: AssistantMessageEvent::TextDelta { delta, .. },
                    ..
                }) => self.streaming_text.push_str(&delta),
                SideChatInternalEvent::Agent(AgentEvent::MessageEnd {
                    message: Message::Assistant(assistant),
                }) if self.streaming_text.is_empty() => {
                    self.streaming_text = content_text(&assistant.content);
                }
                SideChatInternalEvent::Agent(_) | SideChatInternalEvent::PromptFailed(_) => {}
            }
        }
        self.is_streaming = false;
        self.pending_user_echo = false;
        if was_streaming {
            if !self.streaming_text.is_empty() {
                let text = std::mem::take(&mut self.streaming_text);
                self.settle_streaming_assistant(text, true);
            } else {
                self.pending_assistant_index = None;
            }
            self.status = "Side chat aborted".to_owned();
        }
    }

    /// Full cleanup for TUI exit. Overlay close must NOT call this.
    pub async fn shutdown(&mut self) {
        self.abort_streaming().await;
        self._subscription = None;
        while self.event_rx.try_recv().is_ok() {}
        self.side_application.cleanup().await;
        let _ = std::fs::remove_dir_all(&self.side_session_dir);
    }

    /// Refork from the current main leaf, discarding side transcript.
    pub async fn refork_from_main(&mut self) -> Result<()> {
        self.abort_streaming().await;
        let fork = self.main_application.fork_side_chat().await?;
        let (side_application, side_session_dir) =
            Self::build_isolated_application(&self.main_application, &fork).await?;
        let tools = match Self::build_tools(
            &self.main_application,
            &side_application,
            &fork,
            self.tool_mode,
            self.file_tracker.clone(),
        ) {
            Ok(tools) => tools,
            Err(error) => {
                side_application.cleanup().await;
                let _ = std::fs::remove_dir_all(&side_session_dir);
                return Err(error);
            }
        };
        let agent = Application::create_side_chat_agent(&fork, Vec::new());
        let side_prompt_base = agent.state().await.system_prompt;
        install_side_tools(&agent, &side_prompt_base, tools).await;
        let subscription_tx = self.event_tx.clone();
        let subscription = agent
            .subscribe_simple(move |event| {
                let tx = subscription_tx.clone();
                async move {
                    let _ = tx.send(SideChatInternalEvent::Agent(event));
                    Ok(())
                }
            })
            .await;
        self.agent = agent;
        let previous_side_application =
            std::mem::replace(&mut self.side_application, side_application);
        self.fork = fork;
        self.side_prompt_base = side_prompt_base;
        self.entries.clear();
        self.streaming_text.clear();
        self.is_streaming = false;
        let previous_side_session_dir =
            std::mem::replace(&mut self.side_session_dir, side_session_dir);
        self.pending_tool_calls.clear();
        self.finalized_tool_calls.clear();
        self.pending_thinking_index = None;
        self.pending_assistant_index = None;
        self.pending_user_echo = false;
        self._subscription = Some(subscription);
        previous_side_application.cleanup().await;
        self.status = format!(
            "Side chat reforked · {} tools · Esc close",
            self.tool_mode.label()
        );
        let _ = std::fs::remove_dir_all(previous_side_session_dir);
        Ok(())
    }

    /// Clear side transcript/messages but keep the agent and mode.
    pub async fn clear_conversation(&mut self) -> Result<()> {
        self.abort_streaming().await;
        self.agent.reset().await;
        self.agent.set_messages(self.fork.messages.clone()).await;
        self.entries.clear();
        self.streaming_text.clear();
        self.pending_tool_calls.clear();
        self.finalized_tool_calls.clear();
        self.pending_thinking_index = None;
        self.pending_assistant_index = None;
        self.pending_user_echo = false;
        self.status = "Side chat cleared".to_owned();
        Ok(())
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> SideChatAction {
        if key.kind == KeyEventKind::Release {
            return SideChatAction::Ignored;
        }
        match key.code {
            KeyCode::Esc => {
                if self.is_streaming {
                    SideChatAction::Handled
                } else {
                    SideChatAction::CloseOverlay
                }
            }
            KeyCode::Enter
                if !key.modifiers.intersects(
                    KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL,
                ) =>
            {
                let text = self.editor_text();
                self.submit_prompt(text, Vec::new());
                SideChatAction::Handled
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.insert_newline();
                SideChatAction::Handled
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.insert_newline();
                SideChatAction::Handled
            }
            KeyCode::Backspace => {
                self.backspace();
                SideChatAction::Handled
            }
            KeyCode::Delete => {
                self.delete();
                SideChatAction::Handled
            }
            KeyCode::Left => {
                self.move_left();
                SideChatAction::Handled
            }
            KeyCode::Right => {
                self.move_right();
                SideChatAction::Handled
            }
            KeyCode::Up => {
                self.scroll = self.scroll.saturating_add(1);
                SideChatAction::Handled
            }
            KeyCode::Down => {
                self.scroll = self.scroll.saturating_sub(1);
                SideChatAction::Handled
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_add(10);
                SideChatAction::Handled
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(10);
                SideChatAction::Handled
            }
            KeyCode::Home => {
                self.editor_column = 0;
                SideChatAction::Handled
            }
            KeyCode::End => {
                self.editor_column = self.editor_lines[self.editor_row].len();
                SideChatAction::Handled
            }
            KeyCode::Char('t')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                if self.is_streaming {
                    self.status =
                        "Side chat is streaming; abort the current turn before changing tool mode"
                            .to_owned();
                }
                SideChatAction::Handled
            }
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::ALT) => {
                SideChatAction::Handled
            }
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::ALT) => {
                SideChatAction::Handled
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.insert_char(character);
                SideChatAction::Handled
            }
            _ => SideChatAction::Ignored,
        }
    }

    /// Async side-effect required after a handled key, if any.
    #[must_use]
    pub fn key_needs_async(&self, key: KeyEvent) -> SideChatAsyncRequest {
        if key.kind == KeyEventKind::Release {
            return SideChatAsyncRequest::None;
        }
        if key.code == KeyCode::Char('t')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            return if self.is_streaming {
                SideChatAsyncRequest::None
            } else {
                SideChatAsyncRequest::ToggleTools
            };
        }
        if key.code == KeyCode::Esc && self.is_streaming {
            return SideChatAsyncRequest::Abort;
        }
        if key.code == KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::ALT) {
            return SideChatAsyncRequest::Refork;
        }
        if key.code == KeyCode::Char('n') && key.modifiers.contains(KeyModifiers::ALT) {
            return SideChatAsyncRequest::Clear;
        }
        SideChatAsyncRequest::None
    }

    pub fn handle_paste(&mut self, payload: &str) {
        if payload.is_empty() {
            return;
        }
        let normalized = payload.replace("\r\n", "\n").replace('\r', "\n");
        self.insert_text(&normalized);
        self.status = format!("Pasted {} bytes into side chat", payload.len());
    }

    fn clear_editor(&mut self) {
        self.editor_lines = vec![String::new()];
        self.editor_row = 0;
        self.editor_column = 0;
    }

    fn insert_char(&mut self, character: char) {
        self.editor_lines[self.editor_row].insert(self.editor_column, character);
        self.editor_column += character.len_utf8();
    }

    fn insert_text(&mut self, text: &str) {
        for character in text.chars() {
            if character == '\n' {
                self.insert_newline();
            } else {
                self.insert_char(character);
            }
        }
    }

    fn insert_newline(&mut self) {
        let tail = self.editor_lines[self.editor_row].split_off(self.editor_column);
        self.editor_row += 1;
        self.editor_column = 0;
        self.editor_lines.insert(self.editor_row, tail);
    }

    fn backspace(&mut self) {
        if self.editor_column > 0 {
            let column = self.editor_column;
            let line = &mut self.editor_lines[self.editor_row];
            let prev = line[..column]
                .chars()
                .next_back()
                .map(|ch| ch.len_utf8())
                .unwrap_or(0);
            if prev > 0 {
                let start = column - prev;
                line.replace_range(start..column, "");
                self.editor_column = start;
            }
        } else if self.editor_row > 0 {
            let current = self.editor_lines.remove(self.editor_row);
            self.editor_row -= 1;
            self.editor_column = self.editor_lines[self.editor_row].len();
            self.editor_lines[self.editor_row].push_str(&current);
        }
    }

    fn delete(&mut self) {
        let line = &mut self.editor_lines[self.editor_row];
        if self.editor_column < line.len() {
            let next = line[self.editor_column..]
                .chars()
                .next()
                .map(|ch| ch.len_utf8())
                .unwrap_or(0);
            if next > 0 {
                let end = self.editor_column + next;
                line.replace_range(self.editor_column..end, "");
            }
        } else if self.editor_row + 1 < self.editor_lines.len() {
            let next = self.editor_lines.remove(self.editor_row + 1);
            self.editor_lines[self.editor_row].push_str(&next);
        }
    }

    fn move_left(&mut self) {
        if self.editor_column > 0 {
            let prev = self.editor_lines[self.editor_row][..self.editor_column]
                .chars()
                .next_back()
                .map(|ch| ch.len_utf8())
                .unwrap_or(0);
            self.editor_column -= prev;
        } else if self.editor_row > 0 {
            self.editor_row -= 1;
            self.editor_column = self.editor_lines[self.editor_row].len();
        }
    }

    fn move_right(&mut self) {
        let line = &self.editor_lines[self.editor_row];
        if self.editor_column < line.len() {
            let next = line[self.editor_column..]
                .chars()
                .next()
                .map(|ch| ch.len_utf8())
                .unwrap_or(0);
            self.editor_column += next;
        } else if self.editor_row + 1 < self.editor_lines.len() {
            self.editor_row += 1;
            self.editor_column = 0;
        }
    }

    pub async fn tool_names(&self) -> Vec<String> {
        self.agent
            .state()
            .await
            .tools
            .into_iter()
            .map(|tool| tool.name)
            .collect()
    }

    pub async fn tool_capabilities(&self) -> Vec<(String, ToolCapability)> {
        self.agent
            .state()
            .await
            .tools
            .into_iter()
            .map(|tool| (tool.name, tool.capability))
            .collect()
    }

    pub async fn agent_messages(&self) -> Vec<Message> {
        self.agent.state().await.messages
    }

    #[cfg(test)]
    pub(crate) async fn wait_for_prompt_idle(&mut self) -> Result<()> {
        let Some(task) = self.prompt_task.take() else {
            return Err(anyhow!("side-chat prompt task was not submitted"));
        };
        task.await.map_err(|error| {
            anyhow!("side-chat prompt task failed to complete: {error}")
        })
    }



    pub fn peek_main(&self, since_fork: bool) -> Result<SideChatMainPeek> {
        if since_fork {
            if let Some(leaf_id) = self.fork.leaf_id.as_deref() {
                return self.main_application.peek_main_history(Some(leaf_id));
            }
            let mut peek = self.main_application.peek_main_history(None)?;
            peek.messages = peek
                .messages
                .into_iter()
                .skip(self.fork.messages.len())
                .collect();
            return Ok(peek);
        }
        self.main_application.peek_main_history(None)
    }
}

/// Maximum number of named side-chat tabs in one [`SideChatTabs`] container.
pub const MAX_SIDE_CHAT_TABS: usize = 8;

/// Name of the tab created for a legacy single `/btw` session. Old sessions
/// remain readable under this default tab.
pub const DEFAULT_SIDE_CHAT_TAB: &str = "default";

/// `/btw` command words that cannot be used as tab names.
const RESERVED_SIDE_CHAT_TAB_WORDS: [&str; 3] = ["new", "list", "close"];

/// One named tab: a full side-chat session record (fork + agent + transcript).
struct SideChatTab {
    name: String,
    controller: SideChatController,
}

/// Multi-tab container for parallel `/btw` side-chat sessions.
///
/// Each tab owns its own [`SideChatController`] — its own fork, agent,
/// transcript, editor, scroll, tool mode, and streaming state. Switching tabs
/// only moves the `active` index: it never forks, never calls a model, and
/// never touches the main session. The main conversation is deliberately not
/// a tab; tabs are parallel conversations only.
///
/// The container always holds at least one tab. `new_default` (used by the
/// first `/btw` open) creates the legacy single-session tab named
/// [`DEFAULT_SIDE_CHAT_TAB`], so old `/btw` behavior is preserved unchanged.
/// Closing the last tab replaces it with a fresh default tab.
///
/// Read/write accessors delegate to the active tab via [`Deref`]/[`DerefMut`];
/// container-wide concerns (`poll_events`, `observe_main_agent_event`,
/// `shutdown`) cover every tab so a background tab keeps streaming after a
/// switch.
pub struct SideChatTabs {
    tabs: Vec<SideChatTab>,
    active: usize,
}

impl SideChatTabs {
    /// Hard bound on open tabs.
    #[must_use]
    pub const fn max_tabs() -> usize {
        MAX_SIDE_CHAT_TABS
    }

    /// Validate a tab name: 1..=32 ASCII word characters (`[A-Za-z0-9_]`),
    /// and not a reserved `/btw` command word (`new`, `list`, `close`).
    pub fn validate_tab_name(name: &str) -> Result<()> {
        if name.is_empty() {
            return Err(anyhow!("tab name must not be empty"));
        }
        if name.len() > 32 {
            return Err(anyhow!(
                "tab name {name:?} exceeds 32 characters ({} given)",
                name.len()
            ));
        }
        if !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            return Err(anyhow!(
                "tab name {name:?} must use only letters, digits, and underscores"
            ));
        }
        if RESERVED_SIDE_CHAT_TAB_WORDS.contains(&name) {
            return Err(anyhow!(
                "tab name {name:?} is reserved for /btw commands (new, list, close)"
            ));
        }
        Ok(())
    }

    /// Create the container with a single default tab — the legacy single
    /// `/btw` session, readable exactly as before.
    pub async fn new_default(application: &Application) -> Result<Self> {
        let controller = SideChatController::fork_from(application).await?;
        Ok(Self::from_single(DEFAULT_SIDE_CHAT_TAB, controller))
    }

    /// Wrap one existing controller as a single-tab container. Used by tests
    /// and embedders that already own a controller.
    #[must_use]
    pub fn from_single(name: impl Into<String>, controller: SideChatController) -> Self {
        Self {
            tabs: vec![SideChatTab {
                name: name.into(),
                controller,
            }],
            active: 0,
        }
    }

    /// Number of open tabs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    /// Name of the active tab.
    #[must_use]
    pub fn active_name(&self) -> &str {
        &self.tabs[self.active].name
    }

    /// Names of all open tabs, in order.
    #[must_use]
    pub fn tab_names(&self) -> Vec<&str> {
        self.tabs.iter().map(|tab| tab.name.as_str()).collect()
    }

    /// Iterate (name, controller) pairs for every open tab, in order. The
    /// active tab is not special-cased; callers compare against
    /// [`SideChatTabs::active_name`]. Used by embedders (RPC/web) to snapshot
    /// every parallel session, including tabs that are streaming in the
    /// background after a switch.
    pub fn tabs(&self) -> impl Iterator<Item = (&str, &SideChatController)> {
        self.tabs
            .iter()
            .map(|tab| (tab.name.as_str(), &tab.controller))
    }

    /// True when a tab with this name exists.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.tabs.iter().any(|tab| tab.name == name)
    }

    /// Fork a new named tab and make it active. Validates the name, enforces
    /// the [`MAX_SIDE_CHAT_TABS`] bound, and forks a fresh side agent — no
    /// model call happens until the user submits a prompt.
    pub async fn new_tab(&mut self, application: &Application, name: &str) -> Result<()> {
        Self::validate_tab_name(name)?;
        if self.contains(name) {
            return Err(anyhow!("a side tab named {name:?} is already open"));
        }
        if self.tabs.len() >= MAX_SIDE_CHAT_TABS {
            return Err(anyhow!("maximum of {MAX_SIDE_CHAT_TABS} side tabs reached"));
        }
        let controller = SideChatController::fork_from(application).await?;
        self.tabs.push(SideChatTab {
            name: name.to_owned(),
            controller,
        });
        self.active = self.tabs.len() - 1;
        Ok(())
    }

    /// Switch to an existing tab by name. Instant: no fork, no model call.
    pub fn switch_to(&mut self, name: &str) -> Result<()> {
        let Some(index) = self.tabs.iter().position(|tab| tab.name == name) else {
            return Err(anyhow!("no side tab named {name:?}"));
        };
        self.active = index;
        Ok(())
    }

    /// Close a tab by name, shutting down its agent. When the last tab is
    /// closed it is replaced by a fresh default tab so the container never
    /// drops to zero tabs (the legacy single `/btw` session is always one
    /// `/btw` away). The active index moves to a surviving neighbor.
    pub async fn close_tab(&mut self, application: &Application, name: &str) -> Result<()> {
        let Some(index) = self.tabs.iter().position(|tab| tab.name == name) else {
            return Err(anyhow!("no side tab named {name:?}"));
        };
        if self.tabs.len() == 1 {
            // Create the replacement first so the ≥1-tab invariant holds even
            // if the fork fails (the close then aborts and keeps the old tab).
            let controller = SideChatController::fork_from(application).await?;
            self.tabs.push(SideChatTab {
                name: DEFAULT_SIDE_CHAT_TAB.to_owned(),
                controller,
            });
            self.active = 1;
            let mut removed = self.tabs.remove(index);
            removed.controller.shutdown().await;
            self.active = 0;
            return Ok(());
        }
        let mut removed = self.tabs.remove(index);
        removed.controller.shutdown().await;
        if index < self.active {
            self.active -= 1;
        } else if index == self.active {
            self.active = self.active.min(self.tabs.len() - 1);
        }
        Ok(())
    }

    /// Poll agent events for every tab (a background tab may still be
    /// streaming after a switch). Returns true when any tab changed.
    pub fn poll_events(&mut self) -> bool {
        let mut changed = false;
        for tab in &mut self.tabs {
            changed |= tab.controller.poll_events();
        }
        changed
    }

    /// Observe main agent events for every tab's advisory file tracker.
    pub fn observe_main_agent_event(&self, event: &AgentEvent) {
        for tab in &self.tabs {
            tab.controller.observe_main_agent_event(event);
        }
    }

    /// Shut down every tab's agent/subscription. TUI exit must call this.
    pub async fn shutdown(&mut self) {
        for tab in &mut self.tabs {
            tab.controller.shutdown().await;
        }
    }
}

impl Deref for SideChatTabs {
    type Target = SideChatController;

    fn deref(&self) -> &Self::Target {
        &self.tabs[self.active].controller
    }
}

impl DerefMut for SideChatTabs {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.tabs[self.active].controller
    }
}
async fn install_side_tools(agent: &Agent, base: &str, tools: Vec<AgentTool>) {
    let prompt = format!("{base}{}", side_tool_contract(&tools));
    agent.set_tools_and_system_prompt(tools, prompt).await;
}

fn side_tool_contract(tools: &[AgentTool]) -> String {
    let names = tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let shell_available = tools.iter().any(|tool| tool.name == "bash");
    let shell = if shell_available {
        "A shell tool is installed; call only a shell name present in this list."
    } else {
        "No shell tool is installed. Never call bash or any other shell name."
    };
    format!(
        "\n\n{SIDE_TOOL_CONTRACT_HEADER}\nAvailable tools: {names}. \
Use only these tools. Inherited tool inventories, examples, and shell guidance do not grant tools that are absent from this list. \
{shell} \
Investigate files with the available read and search tools. \
If none of them can perform the requested operation, report that limitation instead of attempting an absent tool."
    )
}

const SIDE_TOOL_CONTRACT_HEADER: &str = "# Side chat tools";



fn content_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn content_thinking(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn tool_start_summary(name: &str, arguments: &Value) -> String {
    if let Some(path) = arguments.get("path").and_then(Value::as_str) {
        return format!("[{name}] {path}…");
    }
    format!("[{name}] …")
}

/// Wrap Write/Exec tools with a visible advisory warning on first use.
/// Does not parse shell strings or claim precise overlap detection.
fn wrap_mutation_tools_with_warning(
    tools: Vec<AgentTool>,
    tracker: Arc<Mutex<FileActivityTracker>>,
) -> Vec<AgentTool> {
    let warned = Arc::new(Mutex::new(false));
    tools
        .into_iter()
        .map(|tool| {
            if !matches!(
                tool.capability,
                ToolCapability::Write | ToolCapability::Exec
            ) {
                return tool;
            }
            let capability = tool.capability;
            let execute = tool.execute.clone();
            let tracker = tracker.clone();
            let warned = warned.clone();
            let name = tool.name.clone();
            let description = tool.description.clone();
            let parameters = tool.parameters.clone();
            let label = tool.label.clone();
            let execution_mode = tool.execution_mode;
            let prepare_arguments = tool.prepare_arguments.clone();
            let prompt_guidelines = tool.prompt_guidelines.clone();
            let constrained_sampling = tool.constrained_sampling.clone();
            let mut wrapped = AgentTool::new(name, description, parameters, move |context| {
                let execute = execute.clone();
                let tracker = tracker.clone();
                let warned = warned.clone();
                async move {
                    let mut prefix = None;
                    {
                        let mut already = warned.lock();
                        if !*already {
                            *already = true;
                            let known = tracker.lock().written_paths();
                            prefix = Some(if known.is_empty() {
                                "⚠ Side-chat edit mode: this Write/Exec call may overlap with the main agent.\n".to_owned()
                            } else {
                                format!(
                                    "⚠ Side-chat edit mode: main agent path activity (advisory): {}.\n",
                                    known
                                        .iter()
                                        .take(5)
                                        .map(|path| path.display().to_string())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                )
                            });
                        }
                    }
                    let mut result = (execute)(context).await?;
                    if let Some(warning) = prefix {
                        if let Some(ContentBlock::Text { text, .. }) = result.content.first_mut() {
                            *text = format!("{warning}{text}");
                        } else {
                            result.content.insert(0, ContentBlock::text(warning));
                        }
                    }
                    Ok(result)
                }
            })
            .with_capability(capability)
            .with_label(label)
            .with_execution_mode(execution_mode);
            if let Some(prepare) = prepare_arguments {
                wrapped.prepare_arguments = Some(prepare);
            }
            wrapped.prompt_guidelines = prompt_guidelines;
            wrapped.constrained_sampling = constrained_sampling;
            wrapped
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_agent::{AgentToolResult, ToolCallContext};
    use pi_ai::providers::{
        FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
    };
    use pi_ai::{ContentBlock, Model, ToolResultMessage};

    struct SideChatTestApplication {
        application: Application,
        _cwd: Option<tempfile::TempDir>,
        _agent: tempfile::TempDir,
        _session: tempfile::TempDir,
        _resources: pi_coding::ResourceManager,
        faux: FauxProviderRegistration,
    }

    impl Deref for SideChatTestApplication {
        type Target = Application;

        fn deref(&self) -> &Self::Target {
            &self.application
        }
    }
    impl SideChatTestApplication {
        async fn load_history(&self, messages: Vec<Message>) -> anyhow::Result<()> {
            self.application.session().load_history(messages).await
        }
    }

    async fn test_application(cwd: &Path) -> SideChatTestApplication {
        test_application_with_roots(cwd, Vec::<PathBuf>::new()).await
    }

    async fn test_application_with_roots(
        cwd: &Path,
        additional: Vec<PathBuf>,
    ) -> SideChatTestApplication {
        test_application_with(cwd, additional, false).await
    }

    /// Parent `stream_fn` is the registered faux provider, so a queued response
    /// completes a real `submit_displayed_prompt` turn.
    async fn test_application_live_faux(cwd: &Path) -> SideChatTestApplication {
        test_application_with(cwd, Vec::new(), true).await
    }

    async fn test_application_with(
        cwd: &Path,
        additional: Vec<PathBuf>,
        live_faux: bool,
    ) -> SideChatTestApplication {
        let suffix = uuid::Uuid::now_v7().simple().to_string();
        let model = Model {
            id: format!("side-chat-{suffix}"),
            name: "Side chat fixture".to_owned(),
            api: format!("side-chat-api-{suffix}"),
            provider: format!("side-chat-provider-{suffix}"),
            base_url: "http://localhost:0".to_owned(),
            ..Model::default()
        };
        let faux = register_faux_provider(FauxProviderOptions {
            api: model.api.clone(),
            provider: model.provider.clone(),
            models: vec![model.clone()],
            chunk_size: 4,
        });
        let stream_fn: pi_agent::StreamFn = if live_faux {
            pi_ai::get_api_provider(&model.api)
                .expect("registered side-chat faux provider")
                .stream_simple
        } else {
            Arc::new(|_, _, _| {
                Box::pin(async { pi_ai::new_assistant_message_event_stream() })
            })
        };
        let workspace = pi_coding::WorkspaceRoots::new(cwd, additional).expect("workspace");
        let session =
            pi_coding::Session::new_with_additional_tools_filtered_discovery_workspace_and_uri(
                pi_coding::SessionOptions {
                    model,
                    cwd: cwd.to_path_buf(),
                    system_prompt: "main system\nUse bash for file operations like ls, rg, find."
                        .to_owned(),
                    thinking_level: ThinkingLevel::Off,
                    api_key: "faux".to_owned(),
                    compaction: None,
                    stream_options: Default::default(),
                    tools: Some(Vec::new()),
                    before_tool_call: None,
                    after_tool_call: None,
                    stream_fn: Some(stream_fn),
                    auth_resolver: None,
                    auth_manager: None,
                },
                Vec::new(),
                Default::default(),
                pi_coding::ResourceDiscovery::Disabled,
                workspace,
                None,
            )
            .expect("session");
        let agent = tempfile::tempdir().expect("agent dir");
        std::fs::write(
            agent.path().join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )
        .expect("sandbox settings");
        let session_dir = tempfile::tempdir().expect("session dir");
        session.set_session_dir(session_dir.path().to_path_buf());
        let mut options = pi_coding::ResourceManagerOptions::new(cwd);
        options.agent_dir = agent.path().to_path_buf();
        options.project_trust_override = Some(true);
        options.disable_extensions = true;
        options.disable_skills = true;
        options.disable_prompt_templates = true;
        options.disable_themes = true;
        options.disable_context_files = true;
        let resources = pi_coding::ResourceManager::new(options).expect("resources");
        session
            .attach_resources(resources.clone())
            .await
            .expect("attach resources");
        let application = Application::from_runtime_candidate(
            pi_coding::ApplicationRuntimeCandidate::new(session),
        )
        .await
        .expect("publish application");
        SideChatTestApplication {
            _cwd: None,
            application,
            _agent: agent,
            _session: session_dir,
            _resources: resources,
            faux,
        }
    }

    #[tokio::test]
    async fn fork_does_not_mutate_main() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_application(cwd.path()).await;
        session
            .load_history(vec![Message::user_text("main hello", 1)])
            .await
            .expect("history");
        let application = session;
        let before_state = application.state().await;
        let before_messages = application.messages();

        let mut side = SideChatController::fork_from(&application)
            .await
            .expect("fork");
        side.submit_prompt("side only", Vec::new());
        let after_state = application.state().await;
        assert_eq!(after_state.session_id, before_state.session_id);
        assert_eq!(after_state.session_file, before_state.session_file);
        assert_eq!(application.messages(), before_messages);
        assert_eq!(side.entries().len(), 1);
        assert_eq!(side.entries()[0].role, SideChatRole::User);
        assert_eq!(side.entries()[0].text, "side only");
        side.shutdown().await;
    }

    #[tokio::test]
    async fn independent_transcripts_and_events() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_application(cwd.path()).await;
        session
            .load_history(vec![Message::user_text("shared", 1)])
            .await
            .expect("history");
        let application = session;
        let mut side = SideChatController::fork_from(&application)
            .await
            .expect("fork");
        side.submit_prompt("side turn", Vec::new());
        assert!(
            application.messages().iter().all(|message| match message {
                Message::User(user) => content_text(&user.content) != "side turn",
                _ => true,
            }),
            "side prompt must not enter main messages"
        );
        assert!(
            side.entries()
                .iter()
                .any(|entry| entry.role == SideChatRole::User && entry.text == "side turn")
        );
        side.shutdown().await;
    }

    #[tokio::test]
    async fn reopen_persistence_keeps_controller_state() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_application(cwd.path()).await;
        let application = session;
        let mut side = SideChatController::fork_from(&application)
            .await
            .expect("fork");
        side.submit_prompt("remember me", Vec::new());
        let entries_before = side.entries().len();
        assert_eq!(side.entries().len(), entries_before);
        assert_eq!(side.entries()[0].text, "remember me");
        side.shutdown().await;
    }

    /// Native Ask wire for one turn whose display text and model prompt differ.
    ///
    /// The fixture is the captured snapshot (`src/lib.rs` lines 1-1): the
    /// transcript keeps the typed question and the assistant reply, and does
    /// not display the composed model prompt. The agent still receives that
    /// prompt. Identity is the submitted display versus `MessageEnd::User`,
    /// not a `FILE CONTEXT` parse.
    #[tokio::test]
    async fn submit_displayed_prompt_with_distinct_model_text_shows_one_user_and_assistant() {
        const DISPLAY: &str = "Explain this selected line";
        const MODEL: &str = "Explain this selected line\n\n--- FILE CONTEXT: src/lib.rs \u{00b7} lines 1-1 ---\npub fn answer() -> u32 {\n--- END FILE CONTEXT ---";
        const ASSISTANT: &str = "The selected file context is src/lib.rs \u{00b7} lines 1-1.";

        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_application_live_faux(cwd.path()).await;
        session
            .faux
            .set_responses(vec![FauxResponse::text(ASSISTANT)]);
        let mut side = SideChatController::fork_from(&session)
            .await
            .expect("fork");
        side.submit_displayed_prompt(MODEL, Vec::new(), Some(DISPLAY));
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            side.wait_for_prompt_idle(),
        )
        .await
        .expect("displayed prompt timed out")
        .expect("displayed prompt");
        side.poll_events();

        let wires: Vec<Value> = side
            .entries()
            .iter()
            .map(SideChatEntry::to_wire_json)
            .collect();
        let without_ids: Vec<Value> = wires
            .iter()
            .map(|wire| {
                let mut wire = wire.clone();
                wire.as_object_mut()
                    .expect("wire object")
                    .remove("id");
                wire
            })
            .collect();
        assert_eq!(
            without_ids,
            vec![
                json!({
                    "kind": "user",
                    "role": "user",
                    "text": DISPLAY,
                    "toolName": null,
                    "toolArgs": null,
                    "output": null,
                    "thinking": null,
                    "isError": false,
                    "isPartial": false,
                }),
                json!({
                    "kind": "assistant",
                    "role": "assistant",
                    "text": ASSISTANT,
                    "toolName": null,
                    "toolArgs": null,
                    "output": null,
                    "thinking": null,
                    "isError": false,
                    "isPartial": false,
                }),
            ],
            "one display user row and the assistant; model context must not be a wire row: {wires:?}"
        );
        assert!(
            wires.iter().all(|wire| {
                wire["id"]
                    .as_str()
                    .is_some_and(|id| !id.is_empty())
            }),
            "wire ids must stay populated: {wires:?}"
        );
        assert_ne!(wires[0]["id"], wires[1]["id"]);

        let captured = side.agent_messages().await;
        let model_text = captured
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::User(user) => Some(content_text(&user.content)),
                _ => None,
            })
            .expect("model user message");
        assert_eq!(
            model_text, MODEL,
            "the agent must still receive the composed prompt"
        );
        side.shutdown().await;
    }

    fn user_event(text: &str) -> AgentEvent {
        AgentEvent::MessageEnd {
            message: Message::user_text(text, 1),
        }
    }

    fn user_texts(side: &SideChatController) -> Vec<&str> {
        side.entries()
            .iter()
            .filter(|entry| entry.role == SideChatRole::User)
            .map(|entry| entry.text.as_str())
            .collect()
    }

    /// The pending flag consumes exactly one user echo. A later user event
    /// with the same text is a real row, not another echo of the display.
    #[tokio::test]
    async fn later_identical_user_message_is_not_dropped() {
        let (_cwd, mut side) = fresh_side().await;
        side.submit_displayed_prompt("composed model prompt", Vec::new(), Some("same line"));
        side.apply_agent_event(AgentEvent::AgentStart);
        side.apply_agent_event(AgentEvent::TurnStart);
        side.apply_agent_event(user_event("composed model prompt"));
        assert_eq!(user_texts(&side), vec!["same line"]);

        side.apply_agent_event(user_event("same line"));
        assert_eq!(
            user_texts(&side),
            vec!["same line", "same line"],
            "a user event after the pending echo is consumed must stay"
        );
        side.shutdown().await;
    }

    /// Each terminal event the controller applies retires the pending echo,
    /// so a following user event with the same text is kept. Seed controller
    /// state directly: submitting to `fresh_side` starts a non-terminating
    /// stream, and synthetic terminal events do not finish that prompt task.
    #[tokio::test]
    async fn terminal_events_clear_pending_user_echo() {
        let (_cwd, mut side) = fresh_side().await;
        side.push_role(SideChatRole::User, "shown".to_owned(), false, false);
        side.is_streaming = true;
        side.pending_user_echo = true;
        side.apply_agent_event(AgentEvent::AgentEnd { messages: Vec::new() });
        assert!(!side.pending_user_echo);
        assert!(!side.is_streaming);
        side.apply_agent_event(user_event("shown"));
        assert_eq!(user_texts(&side), vec!["shown", "shown"]);

        side.push_role(SideChatRole::User, "again".to_owned(), false, false);
        side.is_streaming = true;
        side.pending_user_echo = true;
        side.event_tx
            .send(SideChatInternalEvent::PromptFailed(
                "Side chat failed: boom".to_owned(),
            ))
            .expect("queue prompt failure");
        assert!(side.poll_events());
        assert!(!side.pending_user_echo);
        assert!(!side.is_streaming);
        side.apply_agent_event(user_event("again"));
        assert_eq!(user_texts(&side), vec!["shown", "shown", "again", "again"]);
        assert!(side.prompt_task.is_none());
        side.shutdown().await;
    }

    #[tokio::test]
    async fn read_only_default_lacks_write_exec() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_application(cwd.path()).await;
        let application = session;
        let side = SideChatController::fork_from(&application)
            .await
            .expect("fork");
        let caps = side.tool_capabilities().await;
        assert!(
            caps.iter().all(|(_, cap)| *cap == ToolCapability::Read),
            "default tools must be Read-only by capability: {caps:?}"
        );
        assert!(caps.iter().any(|(name, _)| name == "peek_main"));
        assert!(
            !caps
                .iter()
                .any(|(name, _)| name == "write" || name == "bash")
        );
    }

    fn tool_context(arguments: Value) -> ToolCallContext {
        let (controller, abort) = pi_agent::AbortController::new();
        std::mem::forget(controller);
        ToolCallContext {
            tool_call_id: "boundary".to_owned(),
            arguments,
            on_update: Arc::new(|_: AgentToolResult| {}),
            abort,
            model: None,
            approval_token: None,
        }
    }

    fn tool_text(result: &AgentToolResult) -> String {
        match result.content.first() {
            Some(ContentBlock::Text { text, .. }) => text.clone(),
            _ => String::new(),
        }
    }

    #[tokio::test]
    async fn additional_authorized_root_is_readable_and_exec_stays_absent() {
        let cwd = tempfile::tempdir().expect("cwd");
        let authorized = tempfile::tempdir().expect("authorized root");
        let outside = tempfile::tempdir().expect("outside root");
        let sentinel = "side-chat-authorized-sentinel-7f3a";
        std::fs::write(authorized.path().join("only-here.txt"), format!("{sentinel}\n"))
            .expect("authorized sentinel");
        std::fs::write(outside.path().join("secret.txt"), "outside-sentinel\n").expect("outside");
        let session = test_application_with_roots(
            cwd.path(),
            vec![authorized.path().to_path_buf()],
        )
        .await;
        let side = SideChatController::fork_from(&session)
            .await
            .expect("fork");
        let child_roots = side
            .side_application
            .session()
            .workspace_roots()
            .roots()
            .to_vec();
        let parent_roots = session.session().workspace_roots().roots().to_vec();
        assert_eq!(child_roots, parent_roots);
        assert!(
            child_roots.iter().any(|root| root == authorized.path()),
            "child must retain the authorized additional root: {child_roots:?}"
        );

        let tools = side.agent.state().await.tools;
        assert!(tools.iter().all(|tool| tool.capability == ToolCapability::Read));
        assert!(tools.iter().any(|tool| tool.name == "grep"));
        assert!(tools.iter().any(|tool| tool.name == "read"));
        assert!(!tools.iter().any(|tool| {
            tool.name == "bash"
                || matches!(tool.capability, ToolCapability::Write | ToolCapability::Exec)
        }));
        let prompt = side.agent.state().await.system_prompt;
        assert!(
            prompt.contains("Use bash for file operations"),
            "inherited shell guidance must remain so the contract has something to override: {prompt}"
        );
        let contract = prompt
            .rsplit_once(SIDE_TOOL_CONTRACT_HEADER)
            .map(|(_, section)| section)
            .expect("final tool contract");
        assert!(contract.contains("grep") && contract.contains("read"), "{contract}");
        assert!(
            contract.contains("Never call bash") && contract.contains("No shell tool is installed"),
            "{contract}"
        );
        let inventory = contract
            .split("Available tools:")
            .nth(1)
            .and_then(|section| section.split('.').next())
            .unwrap_or("");
        assert!(
            !inventory.split(',').any(|name| name.trim() == "bash"),
            "contract inventory must not advertise bash: {inventory}"
        );

        let grep = tools.iter().find(|tool| tool.name == "grep").expect("grep");
        let found = (grep.execute)(tool_context(json!({
            "pattern": sentinel,
            "path": authorized.path(),
        })))
        .await
        .expect("authorized grep");
        let found_text = tool_text(&found);
        assert!(
            found_text.contains(sentinel),
            "grep must read the additional authorized root, got: {found_text}"
        );
        let denied = (grep.execute)(tool_context(json!({
            "pattern": "outside-sentinel",
            "path": outside.path(),
        })))
        .await
        .expect_err("outside root must stay denied")
        .to_string();
        assert!(
            denied.starts_with("path_outside_workspace:"),
            "{denied}"
        );
    }


    #[tokio::test]
    async fn mode_toggle_shows_warning_and_restores_readonly() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_application(cwd.path()).await;
        let application = session;
        let mut side = SideChatController::fork_from(&application)
            .await
            .expect("fork");
        assert!(!side.show_edit_warning());
        side.toggle_tool_mode().await.expect("toggle edit");
        assert!(side.tool_mode().is_edit());
        assert!(side.show_edit_warning());
        assert!(side.status().to_ascii_lowercase().contains("edit"));
        assert!(side.entries().iter().any(|entry| {
            entry.text.contains("Edit mode")
                || entry.text.contains("overlap")
                || entry.text.contains("Write/Exec")
        }));
        let caps = side.tool_capabilities().await;
        assert!(
            caps.iter()
                .any(|(_, cap)| matches!(cap, ToolCapability::Write | ToolCapability::Exec)),
            "edit mode must enable mutation capabilities: {caps:?}"
        );
        side.toggle_tool_mode().await.expect("toggle readonly");
        assert!(!side.tool_mode().is_edit());
        assert!(!side.show_edit_warning());
        let caps = side.tool_capabilities().await;
        assert!(caps.iter().all(|(_, cap)| *cap == ToolCapability::Read));
        side.shutdown().await;
    }

    #[tokio::test]
    async fn peek_main_returns_main_history_read_only() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_application(cwd.path()).await;
        session
            .load_history(vec![
                Message::user_text("alpha", 1),
                Message::user_text("beta", 2),
            ])
            .await
            .expect("history");
        let application = session;
        let side = SideChatController::fork_from(&application)
            .await
            .expect("fork");
        let before = application.messages();
        let peek = side.peek_main(false).expect("peek");
        assert!(peek.messages.len() >= 2);
        assert_eq!(application.messages(), before);
    }

    #[tokio::test]
    async fn side_cleanup_on_shutdown() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_application(cwd.path()).await;
        let application = session;
        let mut side = SideChatController::fork_from(&application)
            .await
            .expect("fork");
        side.submit_prompt("running", Vec::new());
        side.shutdown().await;
        assert!(!side.is_streaming());
    }

    #[tokio::test]
    async fn paste_isolation_stays_in_side_editor() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_application(cwd.path()).await;
        let application = session;
        let mut side = SideChatController::fork_from(&application)
            .await
            .expect("fork");
        let before = application.messages();
        side.handle_paste("pasted\nside\nonly");
        assert_eq!(side.editor_text(), "pasted\nside\nonly");
        assert_eq!(application.messages(), before);
        assert!(side.entries().is_empty());
        side.shutdown().await;
    }

    #[test]
    fn file_tracker_uses_path_args_not_shell_strings() {
        let mut tracker = FileActivityTracker::new();
        tracker.observe_tool_start(
            ToolCapability::Write,
            &serde_json::json!({"path": "src/main.rs"}),
        );
        assert!(tracker.has_written(Path::new("src/main.rs")));
        tracker.observe_tool_start(
            ToolCapability::Exec,
            &serde_json::json!({"command": "echo src/other.rs > src/other.rs"}),
        );
        assert!(!tracker.has_written(Path::new("src/other.rs")));
    }

    #[test]
    fn command_registration_contract_name() {
        assert_eq!(SideChatToolMode::ReadOnly.label(), "read-only");
        assert_eq!(SideChatToolMode::Edit.label(), "edit");
    }

    #[tokio::test]
    async fn tabs_default_opens_legacy_single_session() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_application(cwd.path()).await;
        let application = session;
        let mut tabs = SideChatTabs::new_default(&application)
            .await
            .expect("default container");
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs.active_name(), DEFAULT_SIDE_CHAT_TAB);
        assert_eq!(tabs.tab_names(), vec![DEFAULT_SIDE_CHAT_TAB]);
        assert!(tabs.entries().is_empty(), "fresh default tab is empty");

        // Legacy `/btw <prompt>` behavior: submit goes to the active tab.
        tabs.submit_prompt("legacy prompt", Vec::new());
        assert_eq!(tabs.entries().len(), 1);
        assert_eq!(tabs.entries()[0].role, SideChatRole::User);
        assert_eq!(tabs.entries()[0].text, "legacy prompt");
        tabs.shutdown().await;
        assert!(!tabs.is_streaming());
    }

    #[tokio::test]
    async fn tabs_write_switch_round_trip_preserves_transcripts() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_application(cwd.path()).await;
        let application = session;
        let mut tabs = SideChatTabs::new_default(&application)
            .await
            .expect("default container");

        // Write in the default tab.
        tabs.submit_prompt("draft in default", Vec::new());
        assert_eq!(tabs.active_name(), DEFAULT_SIDE_CHAT_TAB);
        let default_entries = tabs
            .entries()
            .iter()
            .map(|entry| entry.text.clone())
            .collect::<Vec<_>>();
        assert_eq!(default_entries, vec!["draft in default"]);

        // New tab is active and starts empty; write there too.
        tabs.new_tab(&application, "alpha")
            .await
            .expect("alpha tab");
        assert_eq!(tabs.active_name(), "alpha");
        assert!(tabs.entries().is_empty());
        tabs.submit_prompt("draft in alpha", Vec::new());
        let alpha_entries = tabs
            .entries()
            .iter()
            .map(|entry| entry.text.clone())
            .collect::<Vec<_>>();
        assert_eq!(alpha_entries, vec!["draft in alpha"]);

        // Switch back to default: its transcript is preserved in its own record.
        tabs.switch_to(DEFAULT_SIDE_CHAT_TAB)
            .expect("switch to default");
        assert_eq!(tabs.active_name(), DEFAULT_SIDE_CHAT_TAB);
        let default_again = tabs
            .entries()
            .iter()
            .map(|entry| entry.text.clone())
            .collect::<Vec<_>>();
        assert_eq!(default_again, vec!["draft in default"]);
        assert_eq!(
            tabs.entries().len(),
            1,
            "switching must not mix transcripts"
        );

        // And back to alpha.
        tabs.switch_to("alpha").expect("switch to alpha");
        let alpha_again = tabs
            .entries()
            .iter()
            .map(|entry| entry.text.clone())
            .collect::<Vec<_>>();
        assert_eq!(alpha_again, vec!["draft in alpha"]);

        tabs.shutdown().await;
    }

    #[tokio::test]
    async fn tabs_list_and_close() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_application(cwd.path()).await;
        let application = session;
        let mut tabs = SideChatTabs::new_default(&application)
            .await
            .expect("default container");
        tabs.new_tab(&application, "alpha").await.expect("alpha");
        tabs.new_tab(&application, "beta").await.expect("beta");
        tabs.switch_to(DEFAULT_SIDE_CHAT_TAB)
            .expect("switch to default");

        // list
        assert_eq!(tabs.tab_names(), vec!["default", "alpha", "beta"]);
        assert!(tabs.contains("alpha"));
        assert!(!tabs.contains("nope"));
        assert_eq!(tabs.active_name(), "default");

        // close an inactive tab: active stays put
        tabs.close_tab(&application, "alpha")
            .await
            .expect("close alpha");
        assert_eq!(tabs.tab_names(), vec!["default", "beta"]);
        assert_eq!(tabs.active_name(), "default");

        // close the active tab: active moves to the neighbor
        tabs.close_tab(&application, "default")
            .await
            .expect("close default");
        assert_eq!(tabs.tab_names(), vec!["beta"]);
        assert_eq!(tabs.active_name(), "beta");

        // close unknown: error, no mutation
        let error = tabs
            .close_tab(&application, "missing")
            .await
            .expect_err("unknown tab");
        assert!(error.to_string().contains("no side tab named"));
        assert_eq!(tabs.tab_names(), vec!["beta"]);

        tabs.shutdown().await;
    }

    #[tokio::test]
    async fn tabs_max_bound_rejects_overflow() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_application(cwd.path()).await;
        let application = session;
        let mut tabs = SideChatTabs::new_default(&application)
            .await
            .expect("default container");
        for index in 1..MAX_SIDE_CHAT_TABS {
            tabs.new_tab(&application, &format!("tab{index}"))
                .await
                .expect("tab within bound");
        }
        assert_eq!(tabs.len(), MAX_SIDE_CHAT_TABS);
        assert_eq!(tabs.active_name(), format!("tab{}", MAX_SIDE_CHAT_TABS - 1));
        let error = tabs
            .new_tab(&application, "overflow")
            .await
            .expect_err("bound must reject");
        assert!(
            error.to_string().contains("maximum"),
            "bound error must explain the limit: {error}"
        );
        assert_eq!(
            tabs.len(),
            MAX_SIDE_CHAT_TABS,
            "bound must not mutate state"
        );

        // Duplicate names are rejected too.
        let duplicate = tabs
            .new_tab(&application, "tab1")
            .await
            .expect_err("duplicate must be rejected");
        assert!(duplicate.to_string().contains("already open"));
        tabs.shutdown().await;
    }

    #[tokio::test]
    async fn tabs_close_last_replaces_with_fresh_default() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_application(cwd.path()).await;
        let application = session;
        let mut tabs = SideChatTabs::new_default(&application)
            .await
            .expect("default container");
        tabs.submit_prompt("old default content", Vec::new());
        assert_eq!(tabs.entries().len(), 1);

        tabs.close_tab(&application, DEFAULT_SIDE_CHAT_TAB)
            .await
            .expect("close last tab");
        assert_eq!(tabs.len(), 1, "container must never drop to zero tabs");
        assert_eq!(tabs.active_name(), DEFAULT_SIDE_CHAT_TAB);
        assert!(
            tabs.entries().is_empty(),
            "replacement default tab must be a fresh session"
        );
        tabs.shutdown().await;
    }

    #[test]
    fn tabs_name_validation() {
        for bad in [
            "",
            "has space",
            "tab/name",
            "tab-name",
            "new",
            "list",
            "close",
        ] {
            assert!(
                SideChatTabs::validate_tab_name(bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
        let too_long = "a".repeat(33);
        assert!(SideChatTabs::validate_tab_name(&too_long).is_err());
        let max_len = "b".repeat(32);
        assert!(SideChatTabs::validate_tab_name(&max_len).is_ok());
        for good in ["a", "tab_1", "Z9", DEFAULT_SIDE_CHAT_TAB, "Tab"] {
            assert!(
                SideChatTabs::validate_tab_name(good).is_ok(),
                "{good:?} must be accepted"
            );
        }
    }

    #[tokio::test]
    async fn tabs_poll_events_and_observe_cover_every_tab() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_application(cwd.path()).await;
        let application = session;
        let mut tabs = SideChatTabs::new_default(&application)
            .await
            .expect("default container");
        tabs.new_tab(&application, "alpha").await.expect("alpha");

        // Polling is container-wide: no panic with multiple live tabs, and a
        // background tab still drains its events.
        let _ = tabs.poll_events();
        tabs.observe_main_agent_event(&AgentEvent::AgentStart);
        tabs.shutdown().await;
    }

    async fn fresh_side() -> (SideChatTestApplication, SideChatController) {
        let cwd = tempfile::tempdir().expect("cwd");
        let mut application = test_application(cwd.path()).await;
        application._cwd = Some(cwd);
        let side = SideChatController::fork_from(&application)
            .await
            .expect("fork");
        (application, side)
    }

    fn tool_result_message(id: &str, name: &str, text: &str, is_error: bool) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: id.to_owned(),
            tool_name: name.to_owned(),
            content: vec![ContentBlock::text(text)],
            usage: None,
            details: None,
            added_tool_names: Vec::new(),
            is_error,
            timestamp: 0,
        })
    }

    fn tool_rows(side: &SideChatController) -> Vec<&SideChatEntry> {
        side.entries()
            .iter()
            .filter(|entry| entry.role == SideChatRole::Tool)
            .collect()
    }

    /// Regression: the real event order (start/update/end/message-end) must
    /// finalize exactly one tool row per call with the final result text and
    /// error status, retaining streamed partials until finalization.
    #[tokio::test]
    async fn tool_call_event_order_yields_single_final_row() {
        let (_cwd, mut side) = fresh_side().await;
        side.apply_agent_event(AgentEvent::AgentStart);
        side.apply_agent_event(AgentEvent::TurnStart);
        side.apply_agent_event(AgentEvent::ToolExecutionStart {
            tool_call_id: "call-1".into(),
            tool_name: "read".into(),
            arguments: serde_json::json!({"path": "src/foo.rs"}),
        });
        // Streamed partial content is retained on the in-flight row.
        let partial_rows = tool_rows(&side);
        assert_eq!(partial_rows.len(), 1);
        assert!(partial_rows[0].is_partial);
        assert_eq!(partial_rows[0].text, "[read] src/foo.rs…");
        side.apply_agent_event(AgentEvent::ToolExecutionUpdate {
            tool_call_id: "call-1".into(),
            tool_name: "read".into(),
            arguments: serde_json::json!({}),
            partial_result: AgentToolResult::text("partial…"),
        });
        assert_eq!(tool_rows(&side)[0].text, "[read] partial…");
        assert!(tool_rows(&side)[0].is_partial);
        side.apply_agent_event(AgentEvent::ToolExecutionEnd {
            tool_call_id: "call-1".into(),
            tool_name: "read".into(),
            result: AgentToolResult::text("hello world"),
            is_error: false,
        });
        // Trailing MessageEnd::ToolResult echo must be suppressed.
        side.apply_agent_event(AgentEvent::MessageEnd {
            message: tool_result_message("call-1", "read", "hello world", false),
        });
        let rows = tool_rows(&side);
        assert_eq!(
            rows.len(),
            1,
            "exactly one final row per tool call: {rows:?}"
        );
        assert!(!rows[0].is_partial);
        assert_eq!(rows[0].text, "[read] hello world");
        assert!(!rows[0].is_error);
        // Dedup state is bounded: cleared at turn/agent boundaries.
        assert!(side.finalized_tool_calls.is_empty());
        assert!(side.pending_tool_calls.is_empty());
    }

    /// Parallel tool calls can complete out of order. Two starts followed by
    /// reversed ends and message-ends must keep each result with its correct
    /// tool name and produce exactly one row per call.
    #[tokio::test]
    async fn parallel_tool_calls_keep_identity_out_of_order() {
        let (_cwd, mut side) = fresh_side().await;
        side.apply_agent_event(AgentEvent::AgentStart);
        side.apply_agent_event(AgentEvent::TurnStart);
        side.apply_agent_event(AgentEvent::ToolExecutionStart {
            tool_call_id: "call-a".into(),
            tool_name: "read".into(),
            arguments: serde_json::json!({"path": "a.rs"}),
        });
        side.apply_agent_event(AgentEvent::ToolExecutionStart {
            tool_call_id: "call-b".into(),
            tool_name: "grep".into(),
            arguments: serde_json::json!({"pattern": "todo"}),
        });
        // Ends arrive reversed (call-b finalizes first).
        side.apply_agent_event(AgentEvent::ToolExecutionEnd {
            tool_call_id: "call-b".into(),
            tool_name: "grep".into(),
            result: AgentToolResult::text("result-b"),
            is_error: false,
        });
        side.apply_agent_event(AgentEvent::ToolExecutionEnd {
            tool_call_id: "call-a".into(),
            tool_name: "read".into(),
            result: AgentToolResult::text("result-a"),
            is_error: false,
        });
        // Trailing message-ends also reversed.
        side.apply_agent_event(AgentEvent::MessageEnd {
            message: tool_result_message("call-b", "grep", "result-b", false),
        });
        side.apply_agent_event(AgentEvent::MessageEnd {
            message: tool_result_message("call-a", "read", "result-a", false),
        });
        let rows = tool_rows(&side);
        assert_eq!(rows.len(), 2, "one row per call: {rows:?}");
        let by_text: Vec<&str> = rows.iter().map(|row| row.text.as_str()).collect();
        assert!(by_text.contains(&"[read] result-a"));
        assert!(by_text.contains(&"[grep] result-b"));
        for row in rows {
            assert!(!row.is_partial);
            assert!(!row.is_error);
        }
    }

    /// Final `is_error=true` must upsert the matching pending row by
    /// `tool_call_id` (not nearest-row), replace streamed partial text, leave
    /// that row terminal-error, and never contaminate a sibling call or spawn
    /// a duplicate final row when MessageEnd echoes the result.
    #[tokio::test]
    async fn tool_call_final_error_upserts_matching_row_by_id() {
        let (_cwd, mut side) = fresh_side().await;
        side.apply_agent_event(AgentEvent::AgentStart);
        side.apply_agent_event(AgentEvent::TurnStart);

        // Error call starts first (with a streamed partial); success call
        // starts second so it is the most recent pending row. A broken
        // reducer that finalizes the last/nearest row would hit call-ok, not
        // call-err — only ID-based association reaches the right row.
        side.apply_agent_event(AgentEvent::ToolExecutionStart {
            tool_call_id: "call-err".into(),
            tool_name: "bash".into(),
            arguments: serde_json::json!({"command": "false"}),
        });
        side.apply_agent_event(AgentEvent::ToolExecutionUpdate {
            tool_call_id: "call-err".into(),
            tool_name: "bash".into(),
            arguments: serde_json::json!({}),
            partial_result: AgentToolResult::text("running…"),
        });
        side.apply_agent_event(AgentEvent::ToolExecutionStart {
            tool_call_id: "call-ok".into(),
            tool_name: "read".into(),
            arguments: serde_json::json!({"path": "ok.rs"}),
        });
        assert_eq!(tool_rows(&side).len(), 2);
        assert!(
            tool_rows(&side)
                .iter()
                .any(|row| row.is_partial && row.text == "[bash] running…")
        );

        // Finalize the error call while the sibling is still partial/running.
        side.apply_agent_event(AgentEvent::ToolExecutionEnd {
            tool_call_id: "call-err".into(),
            tool_name: "bash".into(),
            result: AgentToolResult::text("exit 1: permission denied"),
            is_error: true,
        });
        // Trailing MessageEnd echo must not create a second error row.
        side.apply_agent_event(AgentEvent::MessageEnd {
            message: tool_result_message("call-err", "bash", "exit 1: permission denied", true),
        });

        let rows = tool_rows(&side);
        assert_eq!(
            rows.len(),
            2,
            "exactly one row per call (ok still pending + err final): {rows:?}"
        );

        let err = rows
            .iter()
            .find(|row| row.text.contains("exit 1: permission denied"))
            .expect("final error text must replace the streamed partial");
        assert_eq!(err.text, "[bash] exit 1: permission denied");
        assert!(
            err.is_error,
            "is_error=true must mark the matching row error"
        );
        assert!(
            !err.is_partial,
            "final error row must not remain partial/running"
        );

        let ok = rows
            .iter()
            .find(|row| row.text.contains("ok.rs") || row.text.starts_with("[read]"))
            .expect("sibling row must remain associated with call-ok");
        assert!(
            !ok.is_error,
            "error flag must not cross-associate onto the sibling row"
        );
        assert!(ok.is_partial, "unfinished sibling must stay partial");

        // Complete sibling successfully — proves identity stayed keyed by id.
        side.apply_agent_event(AgentEvent::ToolExecutionEnd {
            tool_call_id: "call-ok".into(),
            tool_name: "read".into(),
            result: AgentToolResult::text("ok-body"),
            is_error: false,
        });
        side.apply_agent_event(AgentEvent::MessageEnd {
            message: tool_result_message("call-ok", "read", "ok-body", false),
        });

        let final_rows = tool_rows(&side);
        assert_eq!(
            final_rows.len(),
            2,
            "no duplicate final rows: {final_rows:?}"
        );

        let err = final_rows
            .iter()
            .find(|row| row.text == "[bash] exit 1: permission denied")
            .expect("error row preserved after sibling finalizes");
        assert!(err.is_error);
        assert!(!err.is_partial);

        let ok = final_rows
            .iter()
            .find(|row| row.text == "[read] ok-body")
            .expect("ok row finalized by tool_call_id");
        assert!(!ok.is_error);
        assert!(!ok.is_partial);
    }

    /// A tool result that arrives without any streamed execution events
    /// (MessageEnd-only) must still be recorded as the sole final row.
    #[tokio::test]
    async fn message_end_only_tool_result_is_recorded() {
        let (_cwd, mut side) = fresh_side().await;
        side.apply_agent_event(AgentEvent::AgentStart);
        side.apply_agent_event(AgentEvent::TurnStart);
        side.apply_agent_event(AgentEvent::MessageEnd {
            message: tool_result_message("solo", "read", "fallback result", false),
        });
        let rows = tool_rows(&side);
        assert_eq!(rows.len(), 1, "fallback must record one row: {rows:?}");
        assert_eq!(rows[0].text, "[read] fallback result");
        assert!(!rows[0].is_error);
        assert!(!rows[0].is_partial);
        assert_eq!(rows[0].tool_name.as_deref(), Some("read"));
        assert_eq!(rows[0].output.as_deref(), Some("fallback result"));
        let wire = rows[0].to_wire_json();
        assert_eq!(wire["kind"], "tool");
        assert_eq!(wire["toolName"], "read");
        assert_eq!(wire["output"], "fallback result");
        assert_eq!(wire["id"], rows[0].id);
    }

    #[tokio::test]
    async fn tool_start_captures_args_and_thinking_projects_fold_fields() {
        let (_cwd, mut side) = fresh_side().await;
        side.apply_agent_event(AgentEvent::AgentStart);
        side.apply_agent_event(AgentEvent::TurnStart);
        side.apply_agent_event(AgentEvent::ToolExecutionStart {
            tool_call_id: "call-bash".into(),
            tool_name: "bash".into(),
            arguments: json!({"command": "ls"}),
        });
        side.apply_agent_event(AgentEvent::ToolExecutionEnd {
            tool_call_id: "call-bash".into(),
            tool_name: "bash".into(),
            result: AgentToolResult::text("ok"),
            is_error: false,
        });
        let tool = tool_rows(&side)[0];
        assert_eq!(tool.kind, SideChatEntryKind::Tool);
        assert_eq!(tool.tool_name.as_deref(), Some("bash"));
        assert_eq!(tool.tool_args, Some(json!({"command": "ls"})));
        assert_eq!(tool.output.as_deref(), Some("ok"));
        let tool_wire = tool.to_wire_json();
        assert_eq!(tool_wire["kind"], "tool");
        assert_eq!(tool_wire["toolName"], "bash");
        assert_eq!(tool_wire["toolArgs"]["command"], "ls");
        assert_eq!(tool_wire["output"], "ok");
        assert!(tool_wire["id"].as_str().is_some_and(|id| !id.is_empty()));

        let mut partial = pi_ai::AssistantMessage::pending(side.model());
        side.apply_agent_event(AgentEvent::MessageUpdate {
            message: Message::Assistant(partial.clone()),
            assistant_message_event: AssistantMessageEvent::ThinkingStart {
                content_index: 0,
                partial: partial.clone(),
            },
        });
        side.apply_agent_event(AgentEvent::MessageUpdate {
            message: Message::Assistant(partial.clone()),
            assistant_message_event: AssistantMessageEvent::ThinkingDelta {
                content_index: 0,
                delta: "consider the queue".into(),
                partial: partial.clone(),
            },
        });
        side.apply_agent_event(AgentEvent::MessageUpdate {
            message: Message::Assistant(partial.clone()),
            assistant_message_event: AssistantMessageEvent::ThinkingEnd {
                content_index: 0,
                content: "consider the queue".into(),
                partial: partial.clone(),
            },
        });
        partial.content.push(ContentBlock::thinking("consider the queue"));
        partial.content.push(ContentBlock::text("done"));
        side.apply_agent_event(AgentEvent::MessageEnd {
            message: Message::Assistant(partial),
        });
        let thinking = side
            .entries()
            .iter()
            .find(|entry| entry.kind == SideChatEntryKind::Thinking)
            .expect("thinking row");
        assert_eq!(thinking.thinking.as_deref(), Some("consider the queue"));
        assert!(!thinking.is_partial);
        let thinking_wire = thinking.to_wire_json();
        assert_eq!(thinking_wire["kind"], "thinking");
        assert_eq!(thinking_wire["thinking"], "consider the queue");
        assert!(
            thinking_wire["id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
        let assistant = side
            .entries()
            .iter()
            .find(|entry| entry.kind == SideChatEntryKind::Assistant)
            .expect("assistant row");
        assert_eq!(assistant.text, "done");
    }

    #[tokio::test]
    async fn thinking_end_then_text_delta_then_message_end_keeps_one_thinking_row() {
        let (_cwd, mut side) = fresh_side().await;
        side.apply_agent_event(AgentEvent::AgentStart);
        side.apply_agent_event(AgentEvent::TurnStart);
        let mut partial = pi_ai::AssistantMessage::pending(side.model());
        side.apply_agent_event(AgentEvent::MessageUpdate {
            message: Message::Assistant(partial.clone()),
            assistant_message_event: AssistantMessageEvent::ThinkingStart {
                content_index: 0,
                partial: partial.clone(),
            },
        });
        side.apply_agent_event(AgentEvent::MessageUpdate {
            message: Message::Assistant(partial.clone()),
            assistant_message_event: AssistantMessageEvent::ThinkingDelta {
                content_index: 0,
                delta: "REASONBLOCKQ".into(),
                partial: partial.clone(),
            },
        });
        side.apply_agent_event(AgentEvent::MessageUpdate {
            message: Message::Assistant(partial.clone()),
            assistant_message_event: AssistantMessageEvent::ThinkingEnd {
                content_index: 0,
                content: "REASONBLOCKQ".into(),
                partial: partial.clone(),
            },
        });
        let thinking_after_end: Vec<_> = side
            .entries()
            .iter()
            .filter(|entry| entry.kind == SideChatEntryKind::Thinking)
            .collect();
        assert_eq!(thinking_after_end.len(), 1);
        let thinking_id = thinking_after_end[0].id.clone();
        assert!(!thinking_after_end[0].is_partial);

        side.apply_agent_event(AgentEvent::MessageUpdate {
            message: Message::Assistant(partial.clone()),
            assistant_message_event: AssistantMessageEvent::TextDelta {
                content_index: 1,
                delta: "LIVEANSWERZX".into(),
                partial: partial.clone(),
            },
        });
        let thinking_after_delta: Vec<_> = side
            .entries()
            .iter()
            .filter(|entry| entry.kind == SideChatEntryKind::Thinking)
            .collect();
        assert_eq!(thinking_after_delta.len(), 1);
        assert_eq!(thinking_after_delta[0].id, thinking_id);
        let live_assistants: Vec<_> = side
            .entries()
            .iter()
            .filter(|entry| entry.kind == SideChatEntryKind::Assistant)
            .collect();
        assert_eq!(live_assistants.len(), 1);
        let assistant_id = live_assistants[0].id.clone();
        assert!(live_assistants[0].is_partial);
        assert_eq!(live_assistants[0].text, "LIVEANSWERZX");
        assert_eq!(side.streaming_text(), "LIVEANSWERZX");

        partial.content.push(ContentBlock::thinking("REASONBLOCKQ"));
        partial.content.push(ContentBlock::text("LIVEANSWERZX"));
        side.apply_agent_event(AgentEvent::MessageEnd {
            message: Message::Assistant(partial),
        });
        let thinking_settled: Vec<_> = side
            .entries()
            .iter()
            .filter(|entry| entry.kind == SideChatEntryKind::Thinking)
            .collect();
        assert_eq!(thinking_settled.len(), 1);
        assert_eq!(thinking_settled[0].id, thinking_id);
        assert_eq!(thinking_settled[0].thinking.as_deref(), Some("REASONBLOCKQ"));
        assert!(!thinking_settled[0].is_partial);
        let assistant_settled: Vec<_> = side
            .entries()
            .iter()
            .filter(|entry| entry.kind == SideChatEntryKind::Assistant)
            .collect();
        assert_eq!(assistant_settled.len(), 1);
        assert_eq!(assistant_settled[0].id, assistant_id);
        assert_eq!(assistant_settled[0].text, "LIVEANSWERZX");
        assert!(!assistant_settled[0].is_partial);
        assert!(side.streaming_text().is_empty());
        let thinking_wire = thinking_settled[0].to_wire_json();
        assert_eq!(thinking_wire["id"], thinking_id);
        assert_eq!(thinking_wire["kind"], "thinking");
        assert_eq!(thinking_wire["thinking"], "REASONBLOCKQ");
        assert_eq!(thinking_wire["isPartial"], false);
        let assistant_wire = assistant_settled[0].to_wire_json();
        assert_eq!(assistant_wire["id"], assistant_id);
        assert_eq!(assistant_wire["kind"], "assistant");
        assert_eq!(assistant_wire["text"], "LIVEANSWERZX");
        assert_eq!(assistant_wire["isPartial"], false);
    }

    #[tokio::test]
    async fn text_delta_projects_partial_then_settles_same_id() {
        let (_cwd, mut side) = fresh_side().await;
        side.apply_agent_event(AgentEvent::AgentStart);
        side.apply_agent_event(AgentEvent::TurnStart);
        let mut partial = pi_ai::AssistantMessage::pending(side.model());
        side.apply_agent_event(AgentEvent::MessageUpdate {
            message: Message::Assistant(partial.clone()),
            assistant_message_event: AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "Hel".into(),
                partial: partial.clone(),
            },
        });
        let live = side
            .entries()
            .iter()
            .filter(|entry| entry.kind == SideChatEntryKind::Assistant)
            .collect::<Vec<_>>();
        assert_eq!(live.len(), 1, "live buffer must be one identifiable row");
        assert_eq!(live[0].text, "Hel");
        assert!(live[0].is_partial);
        let live_id = live[0].id.clone();
        let live_wire = live[0].to_wire_json();
        assert_eq!(live_wire["id"], live_id);
        assert_eq!(live_wire["kind"], "assistant");
        assert_eq!(live_wire["text"], "Hel");
        assert_eq!(live_wire["isPartial"], true);
        assert_eq!(side.streaming_text(), "Hel");

        side.apply_agent_event(AgentEvent::MessageUpdate {
            message: Message::Assistant(partial.clone()),
            assistant_message_event: AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "lo".into(),
                partial: partial.clone(),
            },
        });
        let mid = side
            .entries()
            .iter()
            .find(|entry| entry.id == live_id)
            .expect("same id while streaming");
        assert_eq!(mid.text, "Hello");
        assert!(mid.is_partial);
        assert_eq!(
            side.entries()
                .iter()
                .filter(|entry| entry.kind == SideChatEntryKind::Assistant)
                .count(),
            1
        );

        partial.content.push(ContentBlock::text("Hello"));
        side.apply_agent_event(AgentEvent::MessageEnd {
            message: Message::Assistant(partial),
        });
        let settled: Vec<_> = side
            .entries()
            .iter()
            .filter(|entry| entry.kind == SideChatEntryKind::Assistant)
            .collect();
        assert_eq!(settled.len(), 1, "MessageEnd must not append a second row");
        assert_eq!(settled[0].id, live_id);
        assert_eq!(settled[0].text, "Hello");
        assert!(!settled[0].is_partial);
        let settled_wire = settled[0].to_wire_json();
        assert_eq!(settled_wire["id"], live_id);
        assert_eq!(settled_wire["isPartial"], false);
        assert_eq!(settled_wire["text"], "Hello");
        assert!(side.streaming_text().is_empty());
    }
}
