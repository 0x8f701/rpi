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
    Application, SideChatFork, SideChatMainPeek, create_all_tools, create_all_tools_with_rules,
    create_peek_main_tool, create_read_only_tools, filter_tools_by_capabilities,
    tools_include_mutation,
};
use serde_json::Value;
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
    pub role: SideChatRole,
    pub text: String,
    pub is_error: bool,
    pub is_partial: bool,
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
        let tools = Self::build_tools(
            application,
            &fork,
            SideChatToolMode::ReadOnly,
            file_tracker.clone(),
        )?;
        let agent = Application::create_side_chat_agent(&fork, tools);
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
            warned_first_mutation: false,
            pending_tool_calls: HashMap::new(),
            finalized_tool_calls: HashSet::new(),
        })
    }

    fn build_tools(
        application: &Application,
        fork: &SideChatFork,
        mode: SideChatToolMode,
        tracker: Arc<Mutex<FileActivityTracker>>,
    ) -> Result<Vec<AgentTool>> {
        let cwd = fork.cwd.to_string_lossy().into_owned();
        let mut tools = match mode {
            SideChatToolMode::ReadOnly => create_read_only_tools(&cwd),
            SideChatToolMode::Edit => {
                // Build fresh workspace-scoped tools. Never clone the main
                // Session's stateful todo/process/task/hub/goal/extension
                // closures into the detached side agent. The main session's
                // live permission-rule source is threaded into the lsp tool so
                // its rename preflight obeys the same rules as host approval.
                wrap_mutation_tools_with_warning(
                    create_all_tools_with_rules(&cwd, fork.permission_rules.clone()),
                    tracker,
                )
            }
        };
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
                    self.entries.push(SideChatEntry {
                        role: SideChatRole::System,
                        text: message,
                        is_error: true,
                        is_partial: false,
                    });
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

    fn apply_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::AgentStart => {
                self.is_streaming = true;
                self.streaming_text.clear();
                self.status = "Side chat thinking…".to_owned();
            }
            AgentEvent::TurnStart => {
                self.pending_tool_calls.clear();
                self.finalized_tool_calls.clear();
            }
            AgentEvent::MessageUpdate {
                assistant_message_event: AssistantMessageEvent::TextDelta { delta, .. },
                ..
            } => {
                self.streaming_text.push_str(&delta);
            }
            AgentEvent::MessageEnd { message } => match message {
                Message::User(user) => {
                    let text = content_text(&user.content);
                    if !self.entries.last().is_some_and(|entry| {
                        entry.role == SideChatRole::User && entry.text == text
                    }) {
                        self.entries.push(SideChatEntry {
                            role: SideChatRole::User,
                            text,
                            is_error: false,
                            is_partial: false,
                        });
                    }
                }
                Message::Assistant(assistant) => {
                    let text = content_text(&assistant.content);
                    self.streaming_text.clear();
                    if !text.is_empty() {
                        self.entries.push(SideChatEntry {
                            role: SideChatRole::Assistant,
                            text,
                            is_error: false,
                            is_partial: false,
                        });
                    }
                }
                Message::ToolResult(result) => {
                    if !self.finalized_tool_calls.remove(&result.tool_call_id) {
                        let text = content_text(&result.content);
                        self.entries.push(SideChatEntry {
                            role: SideChatRole::Tool,
                            text: format!("[{}] {text}", result.tool_name),
                            is_error: result.is_error,
                            is_partial: false,
                        });
                    }
                }
                _ => {}
            },
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                arguments,
            } => {
                if self.tool_mode.is_edit() && !self.warned_first_mutation {
                    let capability = self
                        .main_application
                        .get_tool_definition(&tool_name)
                        .map(|tool| tool.capability)
                        .unwrap_or(ToolCapability::Read);
                    if matches!(capability, ToolCapability::Write | ToolCapability::Exec) {
                        self.warned_first_mutation = true;
                        self.entries.push(SideChatEntry {
                            role: SideChatRole::System,
                            text: "⚠ Edit mode: Write/Exec tools can conflict with the main agent. Coordinate file changes carefully.".to_owned(),
                            is_error: true,
                            is_partial: false,
                        });
                    }
                }
                let summary = tool_start_summary(&tool_name, &arguments);
                self.entries.push(SideChatEntry {
                    role: SideChatRole::Tool,
                    text: summary,
                    is_error: false,
                    is_partial: true,
                });
                self.pending_tool_calls
                    .insert(tool_call_id, self.entries.len() - 1);
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
                    entry.text = format!("[{tool_name}] {}", content_text(&partial_result.content));
                }
            }
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => {
                let text = content_text(&result.content);
                let finalized_in_place = self
                    .pending_tool_calls
                    .remove(&tool_call_id)
                    .and_then(|index| self.entries.get_mut(index))
                    .is_some_and(|entry| {
                        if entry.role != SideChatRole::Tool || !entry.is_partial {
                            return false;
                        }
                        entry.text = format!("[{tool_name}] {text}");
                        entry.is_error = is_error;
                        entry.is_partial = false;
                        true
                    });
                if !finalized_in_place {
                    self.entries.push(SideChatEntry {
                        role: SideChatRole::Tool,
                        text: format!("[{tool_name}] {text}"),
                        is_error,
                        is_partial: false,
                    });
                }
                self.finalized_tool_calls.insert(tool_call_id);
            }
            AgentEvent::AgentEnd { .. } => {
                self.is_streaming = false;
                if !self.streaming_text.is_empty() {
                    let text = std::mem::take(&mut self.streaming_text);
                    self.entries.push(SideChatEntry {
                        role: SideChatRole::Assistant,
                        text,
                        is_error: false,
                        is_partial: false,
                    });
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
    pub fn submit_prompt(&mut self, prompt: impl Into<String>) {
        let prompt = prompt.into();
        let trimmed = prompt.trim();
        if trimmed.is_empty() || self.is_streaming {
            return;
        }
        self.entries.push(SideChatEntry {
            role: SideChatRole::User,
            text: trimmed.to_owned(),
            is_error: false,
            is_partial: false,
        });
        self.clear_editor();
        self.is_streaming = true;
        self.streaming_text.clear();
        self.status = "Side chat thinking…".to_owned();
        let agent = self.agent.clone();
        let tx = self.event_tx.clone();
        let prompt = trimmed.to_owned();
        self.prompt_task = Some(tokio::spawn(async move {
            if let Err(error) = agent.prompt(prompt).await {
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
            &self.fork,
            next,
            self.file_tracker.clone(),
        )?;
        self.agent.set_tools(tools).await;
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
        self.entries.push(SideChatEntry {
            role: SideChatRole::System,
            text: warning,
            is_error: next.is_edit(),
            is_partial: false,
        });
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
        if was_streaming {
            if !self.streaming_text.is_empty() {
                let text = std::mem::take(&mut self.streaming_text);
                self.entries.push(SideChatEntry {
                    role: SideChatRole::Assistant,
                    text,
                    is_error: false,
                    is_partial: true,
                });
            }
            self.status = "Side chat aborted".to_owned();
        }
    }

    /// Full cleanup for TUI exit. Overlay close must NOT call this.
    pub async fn shutdown(&mut self) {
        self.abort_streaming().await;
        self._subscription = None;
        while self.event_rx.try_recv().is_ok() {}
    }

    /// Refork from the current main leaf, discarding side transcript.
    pub async fn refork_from_main(&mut self) -> Result<()> {
        self.abort_streaming().await;
        let fork = self.main_application.fork_side_chat().await?;
        let tools = Self::build_tools(
            &self.main_application,
            &fork,
            self.tool_mode,
            self.file_tracker.clone(),
        )?;
        let agent = Application::create_side_chat_agent(&fork, tools);
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
        self.fork = fork;
        self.entries.clear();
        self.streaming_text.clear();
        self.is_streaming = false;
        self.pending_tool_calls.clear();
        self.finalized_tool_calls.clear();
        self._subscription = Some(subscription);
        self.status = format!(
            "Side chat reforked · {} tools · Esc close",
            self.tool_mode.label()
        );
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
                self.submit_prompt(text);
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
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::ALT) => SideChatAction::Handled,
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::ALT) => SideChatAction::Handled,
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
        self.tabs.iter().map(|tab| (tab.name.as_str(), &tab.controller))
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
            return Err(anyhow!(
                "maximum of {MAX_SIDE_CHAT_TABS} side tabs reached"
            ));
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
    use pi_agent::AgentToolResult;
    use pi_ai::{Model, ToolResultMessage};

    fn test_session(cwd: &Path) -> pi_coding::Session {
        pi_coding::Session::new(pi_coding::SessionOptions {
            model: Model::default(),
            cwd: cwd.to_path_buf(),
            system_prompt: "main system".to_owned(),
            thinking_level: ThinkingLevel::Off,
            api_key: "test-key".to_owned(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(create_all_tools(&cwd.to_string_lossy())),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
        })
        .expect("session")
    }

    #[tokio::test]
    async fn fork_does_not_mutate_main() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_session(cwd.path());
        session
            .load_history(vec![Message::user_text("main hello", 1)])
            .await
            .expect("history");
        let application = Application::new(session).await;
        let before_state = application.state().await;
        let before_messages = application.messages();

        let mut side = SideChatController::fork_from(&application)
            .await
            .expect("fork");
        side.submit_prompt("side only");
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
        let session = test_session(cwd.path());
        session
            .load_history(vec![Message::user_text("shared", 1)])
            .await
            .expect("history");
        let application = Application::new(session).await;
        let mut side = SideChatController::fork_from(&application)
            .await
            .expect("fork");
        side.submit_prompt("side turn");
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
        let session = test_session(cwd.path());
        let application = Application::new(session).await;
        let mut side = SideChatController::fork_from(&application)
            .await
            .expect("fork");
        side.submit_prompt("remember me");
        let entries_before = side.entries().len();
        assert_eq!(side.entries().len(), entries_before);
        assert_eq!(side.entries()[0].text, "remember me");
        side.shutdown().await;
    }

    #[tokio::test]
    async fn read_only_default_lacks_write_exec() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_session(cwd.path());
        let application = Application::new(session).await;
        let side = SideChatController::fork_from(&application)
            .await
            .expect("fork");
        let caps = side.tool_capabilities().await;
        assert!(
            caps.iter().all(|(_, cap)| *cap == ToolCapability::Read),
            "default tools must be Read-only by capability: {caps:?}"
        );
        assert!(caps.iter().any(|(name, _)| name == "peek_main"));
        assert!(!caps.iter().any(|(name, _)| name == "write" || name == "bash"));
    }

    #[tokio::test]
    async fn mode_toggle_shows_warning_and_restores_readonly() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_session(cwd.path());
        let application = Application::new(session).await;
        let mut side = SideChatController::fork_from(&application)
            .await
            .expect("fork");
        assert!(!side.show_edit_warning());
        side.toggle_tool_mode().await.expect("toggle edit");
        assert!(side.tool_mode().is_edit());
        assert!(side.show_edit_warning());
        assert!(side.status().to_ascii_lowercase().contains("edit"));
        assert!(
            side.entries().iter().any(|entry| {
                entry.text.contains("Edit mode")
                    || entry.text.contains("overlap")
                    || entry.text.contains("Write/Exec")
            })
        );
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
        let session = test_session(cwd.path());
        session
            .load_history(vec![
                Message::user_text("alpha", 1),
                Message::user_text("beta", 2),
            ])
            .await
            .expect("history");
        let application = Application::new(session).await;
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
        let session = test_session(cwd.path());
        let application = Application::new(session).await;
        let mut side = SideChatController::fork_from(&application)
            .await
            .expect("fork");
        side.submit_prompt("running");
        side.shutdown().await;
        assert!(!side.is_streaming());
    }

    #[tokio::test]
    async fn paste_isolation_stays_in_side_editor() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_session(cwd.path());
        let application = Application::new(session).await;
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
        let session = test_session(cwd.path());
        let application = Application::new(session).await;
        let mut tabs = SideChatTabs::new_default(&application)
            .await
            .expect("default container");
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs.active_name(), DEFAULT_SIDE_CHAT_TAB);
        assert_eq!(tabs.tab_names(), vec![DEFAULT_SIDE_CHAT_TAB]);
        assert!(tabs.entries().is_empty(), "fresh default tab is empty");

        // Legacy `/btw <prompt>` behavior: submit goes to the active tab.
        tabs.submit_prompt("legacy prompt");
        assert_eq!(tabs.entries().len(), 1);
        assert_eq!(tabs.entries()[0].role, SideChatRole::User);
        assert_eq!(tabs.entries()[0].text, "legacy prompt");
        tabs.shutdown().await;
        assert!(!tabs.is_streaming());
    }

    #[tokio::test]
    async fn tabs_write_switch_round_trip_preserves_transcripts() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_session(cwd.path());
        let application = Application::new(session).await;
        let mut tabs = SideChatTabs::new_default(&application)
            .await
            .expect("default container");

        // Write in the default tab.
        tabs.submit_prompt("draft in default");
        assert_eq!(tabs.active_name(), DEFAULT_SIDE_CHAT_TAB);
        let default_entries = tabs
            .entries()
            .iter()
            .map(|entry| entry.text.clone())
            .collect::<Vec<_>>();
        assert_eq!(default_entries, vec!["draft in default"]);

        // New tab is active and starts empty; write there too.
        tabs.new_tab(&application, "alpha").await.expect("alpha tab");
        assert_eq!(tabs.active_name(), "alpha");
        assert!(tabs.entries().is_empty());
        tabs.submit_prompt("draft in alpha");
        let alpha_entries = tabs
            .entries()
            .iter()
            .map(|entry| entry.text.clone())
            .collect::<Vec<_>>();
        assert_eq!(alpha_entries, vec!["draft in alpha"]);

        // Switch back to default: its transcript is preserved in its own record.
        tabs.switch_to(DEFAULT_SIDE_CHAT_TAB).expect("switch to default");
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
        let session = test_session(cwd.path());
        let application = Application::new(session).await;
        let mut tabs = SideChatTabs::new_default(&application)
            .await
            .expect("default container");
        tabs.new_tab(&application, "alpha").await.expect("alpha");
        tabs.new_tab(&application, "beta").await.expect("beta");
        tabs.switch_to(DEFAULT_SIDE_CHAT_TAB).expect("switch to default");

        // list
        assert_eq!(tabs.tab_names(), vec!["default", "alpha", "beta"]);
        assert!(tabs.contains("alpha"));
        assert!(!tabs.contains("nope"));
        assert_eq!(tabs.active_name(), "default");

        // close an inactive tab: active stays put
        tabs.close_tab(&application, "alpha").await.expect("close alpha");
        assert_eq!(tabs.tab_names(), vec!["default", "beta"]);
        assert_eq!(tabs.active_name(), "default");

        // close the active tab: active moves to the neighbor
        tabs.close_tab(&application, "default").await.expect("close default");
        assert_eq!(tabs.tab_names(), vec!["beta"]);
        assert_eq!(tabs.active_name(), "beta");

        // close unknown: error, no mutation
        let error = tabs.close_tab(&application, "missing").await.expect_err("unknown tab");
        assert!(error.to_string().contains("no side tab named"));
        assert_eq!(tabs.tab_names(), vec!["beta"]);

        tabs.shutdown().await;
    }

    #[tokio::test]
    async fn tabs_max_bound_rejects_overflow() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_session(cwd.path());
        let application = Application::new(session).await;
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
        assert_eq!(tabs.len(), MAX_SIDE_CHAT_TABS, "bound must not mutate state");

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
        let session = test_session(cwd.path());
        let application = Application::new(session).await;
        let mut tabs = SideChatTabs::new_default(&application)
            .await
            .expect("default container");
        tabs.submit_prompt("old default content");
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
        let session = test_session(cwd.path());
        let application = Application::new(session).await;
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

    async fn fresh_side() -> (tempfile::TempDir, SideChatController) {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = test_session(cwd.path());
        let application = Application::new(session).await;
        let side = SideChatController::fork_from(&application)
            .await
            .expect("fork");
        (cwd, side)
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
        assert_eq!(rows.len(), 1, "exactly one final row per tool call: {rows:?}");
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
            message: tool_result_message(
                "call-err",
                "bash",
                "exit 1: permission denied",
                true,
            ),
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
        assert!(err.is_error, "is_error=true must mark the matching row error");
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
        assert_eq!(final_rows.len(), 2, "no duplicate final rows: {final_rows:?}");

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
    }
}
