use std::borrow::Cow;
use std::cell::Cell;
use std::collections::{hash_map::DefaultHasher, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use crossterm::{
    cursor::{Hide, Show},
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent,
        KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{DisableLineWrap, EnableLineWrap, disable_raw_mode, enable_raw_mode, window_size},
};
use futures_util::{FutureExt, StreamExt};
use pi_agent::{AgentEvent, ThinkingLevel};
use pi_ai::{AssistantMessageEvent, ContentBlock, Message, Model};
use pi_coding::{
    Application, ApplicationEvent, CONFIG_DIR_NAME, DoubleEscapeAction, ExtensionUiRequest,
    GoalLifecycle, GoalState, LoopEvent, LoopTask, Session, StreamingBehavior, TodoItem, TodoPhase,
    TodoStatus, ToolCallViewStatus, UiNotificationLevel, UiSelectOption, UiWidgetPlacement,
};
use ratatui::{
    Terminal,
    TerminalOptions, Viewport,
    backend::{Backend, ClearType, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    widgets::Widget,
};
#[cfg(unix)]
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use unicode_width::UnicodeWidthChar;

use crate::clipboard::{self, ClipboardContent};
use crate::extension_ui::{ExtensionUiAdapter, ExtensionUiEvent, ExtensionUiInteraction};
use crate::file_search::{self, AtPrefix};
use crate::interactive_commands::{
    BUILTIN_COMMANDS, CommandSource, InteractiveCommand, builtin,
    executable_catalog as interactive_commands, expand_resource_command, requires_arguments,
    usage, visible_catalog,
};
use crate::job_card_adapter::{
    JobCardPresentationAdapter, JobCardRowRole, JobCardRows, TaskCardRows,
};
use crate::orchestration_message::{
    orchestration_irc_view, orchestration_irc_view_from_mailbox, OrchestrationIrcView,
};
use crate::keybindings::{Action, KeyBindingsManager};
use crate::agents_panel::{AgentsPanel, AgentsPanelAction};
use crate::markdown::ratatui::{
    MarkdownRatatuiStyles, render_ratatui_markdown, render_ratatui_markdown_streaming,
    syntax_spans_unpadded as markdown_syntax_spans,
};
use crate::process_commands::{
    ProcessKeyResult, ProcessPanel, ProcessPanelAction, render_process_panel,
};
use crate::terminal_images::{
    ImageDisplayConfig, ImageFrameIdentity, ImageLayout, ImagePlacement, TerminalCellSize,
    TerminalImageRenderer,
};
use crate::saved_session_selector::{
    SavedSessionSelector, SessionSelectorMode, SessionSelectorRequest, SessionSort,
    session_display_name,
};
use crate::scoped_model_selector::{ScopedModelSelection, ScopedModelSelector};
use crate::theme::{Theme, ThemeManager};
use crate::tool_card_adapter::{
    ToolCardPresentationAdapter, ToolCardRowRole, ToolCardRows, task_delegation_request,
};
use crate::tree_panel::{TreePanel, TreePanelMode};
use crate::settings_panel::{SettingsControl, SettingsPanel};
use crate::workflow_panel::{WorkflowIntentKind, WorkflowPanel, WorkflowPanelResult, WorkflowPanelSnapshot, compact_workflow_status, render_workflow_panel};

const MAX_TRANSCRIPT_LINES: usize = 4_000;
const MAX_COMPLETIONS: usize = 7;
const MAX_PASTE_BYTES: usize = 1024 * 1024;


#[derive(Clone)]
enum PanelValue {
    Model(Model),
    Thinking(ThinkingLevel),
    Session(PathBuf),
    ScopedModel(Model),
    Trust(pi_coding::TrustDecision),
    SettingsThinking,
    SettingsTheme,
    SettingsAutoCompact,
    GoalCreate,
    GoalShow,
    GoalPause,
    GoalResume,
    GoalComplete,
    GoalDrop,
}

#[derive(Clone)]
struct PanelItem {
    label: String,
    description: String,
    value: PanelValue,
    checked: bool,
}

struct SelectorPanel {
    title: String,
    help: String,
    items: Vec<PanelItem>,
    selected: usize,
    query: String,
}

impl SelectorPanel {
    fn visible_indices(&self) -> Vec<usize> {
        self.items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                fuzzy_match(&item.label, &self.query) || fuzzy_match(&item.description, &self.query)
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn selected_item(&self) -> Option<&PanelItem> {
        self.visible_indices()
            .get(self.selected)
            .and_then(|index| self.items.get(*index))
    }
}

enum ExtensionDialogKind {
    Select {
        options: Vec<UiSelectOption>,
        selected: usize,
    },
    Confirm {
        message: String,
        confirmed: bool,
    },
    Input {
        placeholder: Option<String>,
        editor: EditorState,
    },
    Editor {
        editor: EditorState,
    },
}

struct ExtensionDialog {
    interaction: ExtensionUiInteraction,
    title: String,
    kind: ExtensionDialogKind,
}

impl ExtensionDialog {
    fn new(interaction: ExtensionUiInteraction) -> Self {
        let (title, kind) = match &interaction.request {
            ExtensionUiRequest::Select { title, options } => (
                title.clone(),
                ExtensionDialogKind::Select {
                    options: options.clone(),
                    selected: 0,
                },
            ),
            ExtensionUiRequest::Confirm { title, message } => (
                title.clone(),
                ExtensionDialogKind::Confirm {
                    message: message.clone(),
                    confirmed: true,
                },
            ),
            ExtensionUiRequest::Input {
                title,
                placeholder,
                value,
            } => {
                let mut editor = EditorState::new();
                if let Some(value) = value {
                    editor.set_text(value);
                }
                (
                    title.clone(),
                    ExtensionDialogKind::Input {
                        placeholder: placeholder.clone(),
                        editor,
                    },
                )
            }
            ExtensionUiRequest::Editor { title, prefill } => {
                let mut editor = EditorState::new();
                if let Some(prefill) = prefill {
                    editor.set_text(prefill);
                }
                (title.clone(), ExtensionDialogKind::Editor { editor })
            }
            _ => unreachable!("only interactive requests become dialogs"),
        };
        Self {
            interaction,
            title,
            kind,
        }
    }

    fn instance(&self) -> &pi_coding::ExtensionInstanceId {
        &self.interaction.context.instance
    }
}


#[derive(Clone)]
struct CompletionItem {
    value: String,
    label: String,
    description: String,
    is_directory: bool,
}


#[derive(Clone, Debug)]
enum CompletionContext {
    Slash,
    File {
        generation: u64,
        row: usize,
        prefix: AtPrefix,
    },
}

#[derive(Default)]
struct CompletionState {
    items: Vec<CompletionItem>,
    selected: usize,
    context: Option<CompletionContext>,
}

impl CompletionState {
    fn selected(&self) -> Option<&CompletionItem> {
        self.items.get(self.selected)
    }

    fn clear(&mut self) {
        self.items.clear();
        self.selected = 0;
        self.context = None;
    }

    /// Selected-centered / tail-following window into the full match list.
    /// Navigation still walks every item; only the painted rows are capped.
    fn visible_window(&self, max_rows: usize) -> (usize, &[CompletionItem]) {
        if self.items.is_empty() || max_rows == 0 {
            return (0, &[]);
        }
        let max_rows = max_rows.min(self.items.len());
        let half = max_rows / 2;
        let mut start = self.selected.saturating_sub(half);
        if start + max_rows > self.items.len() {
            start = self.items.len() - max_rows;
        }
        (start, &self.items[start..start + max_rows])
    }
}

enum BackgroundEvent {
    FileCompletion {
        generation: u64,
        row: usize,
        prefix: AtPrefix,
        result: std::result::Result<Vec<file_search::FileMatch>, String>,
    },
    ClipboardRead(std::result::Result<Option<ClipboardContent>, String>),
    ClipboardWrite(std::result::Result<(), String>),
}
struct TranscriptImageCandidate {
    line_index: usize,
    layout: ImageLayout,
    data: String,
    mime_type: String,
}

struct TranscriptImageContext<'renderer> {
    renderer: &'renderer mut TerminalImageRenderer,
    candidates: &'renderer mut Vec<TranscriptImageCandidate>,
    config: ImageDisplayConfig,
    viewport_columns: u16,
    viewport_rows: u16,
    cell_size: TerminalCellSize,
}

struct ImageDrawPlan {
    identity: ImageFrameIdentity,
    placements: Vec<ImagePlacement>,
}

pub async fn interactive(
    application: Application,
    _extension_ui: ExtensionUiAdapter,
    initial_scoped_models: Option<Vec<Model>>,
    initial_prompts: Vec<String>,
) -> Result<()> {
    install_panic_hook();
    // Install the shutdown-signal streams before entering the terminal so a
    // SIGTERM/SIGHUP delivered during setup is recorded by tokio's signal
    // registry and drains on the first select! poll.
    let mut shutdown = ShutdownSignals::new()?;
    let mut terminal = TerminalGuard::enter()?;
    // Test-only panic injection: never set during normal operation.
    if std::env::var_os("PI_TEST_PANIC_AFTER_ENTER").is_some() {
        panic!("pi-test: panic after TUI enter");
    }
    let mut input = EventStream::new();
    let mut events = application.subscribe();
    let mut extension_events = _extension_ui.subscribe();
    let (background_tx, mut background_rx) = mpsc::unbounded_channel();
    let mut state = TuiState::new(
        &application,
        _extension_ui,
        background_tx,
        initial_scoped_models,
    );
    let session = application.session();
    if let Some(prompt) = initial_prompts.first() {
        let expanded = crate::file_args::expand_prompt_in_workspace(
            prompt,
            session.workspace_roots(),
        )?;
        state.push_lines("You", prompt.clone(), state.themes.theme().accent);
        application.prompt(expanded.prompt, expanded.images, None).await?;
        for prompt in initial_prompts.into_iter().skip(1) {
            let expanded = crate::file_args::expand_prompt_in_workspace(
                &prompt,
                session.workspace_roots(),
            )?;
            application.follow_up(expanded.prompt, expanded.images).await;
        }
    }
    let mut update_notice = Some(Box::pin(crate::self_update::startup_notice()));
    let mut theme_watch = tokio::time::interval(std::time::Duration::from_millis(250));
    let mut animation = tokio::time::interval(std::time::Duration::from_millis(120));

    loop {
        terminal.commit_settled(&mut state)?;
        terminal.draw(&state, |frame, images| render(frame, &state, images))?;
        tokio::select! {
            terminal_event = input.next() => {
                let Some(terminal_event) = terminal_event else {
                    state.cancel_extension_dialogs();
                    return Ok(());
                };
                match terminal_event? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        if is_raw_multiline_paste_start(key) {
                            // Ordinary printable keys never wait. Only an unmodified Enter
                            // probes events already buffered by the terminal, which is enough
                            // to recognize an unmarked multiline paste without a timer.
                            let mut keys = vec![key];
                            let mut payload = "\n".to_owned();
                            let mut deferred = None;
                            let mut rejected = false;
                            let mut input_closed = false;
                            loop {
                                match input.next().now_or_never() {
                                    Some(Some(Ok(Event::Key(next))))
                                        if next.kind != KeyEventKind::Release =>
                                    {
                                        if let Some(character) = raw_paste_character(next) {
                                            if payload.len() + character.len_utf8()
                                                <= MAX_PASTE_BYTES
                                            {
                                                keys.push(next);
                                                payload.push(character);
                                            } else {
                                                // Stop at the first byte beyond the cap. The
                                                // event that crossed it is still dispatched as
                                                // a key, and later input remains in EventStream.
                                                rejected = true;
                                                deferred = Some(Event::Key(next));
                                                state.status = format!(
                                                    "Paste rejected: input exceeds the {} MiB limit",
                                                    MAX_PASTE_BYTES / (1024 * 1024)
                                                );
                                                break;
                                            }
                                        } else {
                                            deferred = Some(Event::Key(next));
                                            break;
                                        }
                                    }
                                    Some(Some(Ok(event))) => {
                                        // Bracketed paste is always handled directly rather
                                        // than merged into an unmarked candidate.
                                        deferred = Some(event);
                                        break;
                                    }
                                    Some(Some(Err(error))) => return Err(error.into()),
                                    Some(None) => {
                                        input_closed = true;
                                        break;
                                    }
                                    None => break,
                                }
                            }
                            if !rejected {
                                match classify_raw_input_burst(&payload) {
                                    RawInputDisposition::Paste => {
                                        handle_paste(&mut state, &payload);
                                    }
                                    RawInputDisposition::Keys => {
                                        for replay in keys {
                                            if handle_key(
                                                &application,
                                                &mut state,
                                                replay,
                                                &mut terminal,
                                            )
                                            .await?
                                            {
                                                state.cancel_extension_dialogs();
                                                return Ok(());
                                            }
                                        }
                                    }
                                }
                            }
                            if let Some(event) = deferred {
                                match event {
                                    Event::Key(next) if next.kind != KeyEventKind::Release => {
                                        if handle_key(
                                            &application,
                                            &mut state,
                                            next,
                                            &mut terminal,
                                        )
                                        .await?
                                        {
                                            state.cancel_extension_dialogs();
                                            return Ok(());
                                        }
                                    }
                                    Event::Paste(text) => handle_paste(&mut state, &text),
                                    _ => {}
                                }
                            }
                            if input_closed {
                                state.cancel_extension_dialogs();
                                return Ok(());
                            }
                        } else if handle_key(&application, &mut state, key, &mut terminal).await? {
                            state.cancel_extension_dialogs();
                            return Ok(());
                        }
                        state.sync_extension_host_bindings();
                    }
                    Event::Paste(payload) => {
                        handle_paste(&mut state, &payload);
                        state.sync_extension_host_bindings();
                    }
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            }
            application_event = events.recv() => {
                match application_event {
                    Ok(ApplicationEvent::RuntimeChanged { epoch }) => {
                        state.replace_transcript_from_application(&application);
                        state.refresh_job_projection(&application);
                        state.todo_phases = application.todo_state().phases;
                        state.cwd_path = application.session().cwd().to_path_buf();
                        state.apply_runtime_settings(&application).await;
                        state.status = format!("Switched application runtime generation {epoch}");
                    }
                    Ok(ApplicationEvent::Workflow(event)) => state.apply_workflow_event(&application, event),
                    Ok(event) => state.apply(event),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                        state.refresh_job_projection(&application);
                        state.workflow_snapshots = application
                            .workflow_list()
                            .iter()
                            .map(|snapshot| TuiState::project_workflow_snapshot(&application, snapshot))
                            .collect();
                        if let Some(panel) = &mut state.workflow_panel {
                            panel.replace(state.workflow_snapshots.clone());
                        }
                        state.push_status(format!("UI skipped {count} stale events"), true);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => { state.cancel_extension_dialogs(); return Ok(()); }
                }
            }
            extension_event = extension_events.recv() => {
                match extension_event {
                    Ok(event) => state.apply_extension_ui(event),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                        state.push_status(format!("Extension UI skipped {count} stale events"), true);
                        state.reconcile_extension_dialog();
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => { state.cancel_extension_dialogs(); return Ok(()); }
                }
            }
            background_event = background_rx.recv() => {
                let Some(background_event) = background_event else {
                    state.cancel_extension_dialogs();
                    return Ok(());
                };
                state.apply_background(background_event);
                state.sync_extension_host_bindings();
            }
            notice = async {
                update_notice
                    .as_mut()
                    .expect("guarded update notice future")
                    .as_mut()
                    .await
            }, if update_notice.is_some() => {
                update_notice = None;
                if let Some(notice) = notice {
                    state.push_status(notice, false);
                }
            }
            _ = animation.tick(), if state.has_active_animation() => { state.animation_frame = state.animation_frame.wrapping_add(1); }
            _ = theme_watch.tick() => {
                let changed = state.poll_theme_reload() | state.reconcile_extension_dialog();
                if !changed {
                    continue;
                }
            }
            _ = shutdown.recv() => {
                // Return through TerminalGuard::drop so the live inline
                // viewport is cleared before cooked mode and the cursor are
                // restored. Committed rows above it remain untouched.
                state.cancel_extension_dialogs();
                return Ok(());
            }
        }
    }
}

/// Whether the inline TUI currently owns raw mode and the hidden cursor.
/// Structured-output modes never acquire a guard, so restoration is a no-op
/// for them. The normal screen is deliberately retained for scrollback.
static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);
static INSTALLED: AtomicBool = AtomicBool::new(false);

fn acquire_terminal(writer: &mut impl Write) -> io::Result<()> {
    execute!(writer, DisableLineWrap, EnableBracketedPaste, Hide)
}

fn release_terminal(writer: &mut impl Write) -> io::Result<()> {
    execute!(writer, DisableBracketedPaste, EnableLineWrap, Show)
}

/// Restore cooked input and the visible cursor exactly once per active TUI
/// epoch. This never clears the normal screen, so committed transcript output
/// remains in terminal and tmux scrollback after exit, panic, or a signal.
fn restore_terminal() {
    if TUI_ACTIVE.swap(false, Ordering::SeqCst) {
        let mut stdout = io::stdout();
        if crate::terminal_images::detect_protocol(
            &crate::terminal_images::TerminalEnvironment::current(),
        ) == Some(crate::terminal_images::TerminalImageProtocol::Kitty) {
            let _ = stdout.write_all(crate::terminal_images::KITTY_DELETE_ALL);
        }
        let _ = disable_raw_mode();
        let _ = release_terminal(&mut stdout);
        let _ = stdout.flush();
    }
}

/// Install a process-wide panic hook that restores cooked mode and the cursor
/// before the panic is printed. The normal-screen transcript is left intact.
pub fn install_panic_hook() {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        prev(info);
    }));
}


struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    images: TerminalImageRenderer,
    /// Previous `page_overlay_open` sample. Used to clear the live region exactly
    /// once on overlay dismiss so transient page pixels cannot later be promoted
    /// into native scrollback by `insert_before`.
    page_overlay_was_open: bool,
}

impl TerminalGuard {
    const MIN_VIEWPORT_HEIGHT: u16 = 3;

    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = acquire_terminal(&mut stdout) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        // Stable full-terminal inline height. Ratatui bakes `Viewport::Inline`
        // height at construction and reconstructing mid-session issues a CPR
        // that fails on bare PTYs ("cursor position could not be read").
        // Overlay durability is handled by skipping commit while open and
        // clearing the live region on dismiss — not by shrinking the viewport.
        let height = crossterm::terminal::size()
            .map(|(_, rows)| rows.max(Self::MIN_VIEWPORT_HEIGHT))
            .unwrap_or(24);
        let terminal = match Terminal::with_options(
            CrosstermBackend::new(stdout),
            TerminalOptions {
                viewport: Viewport::Inline(height),
            },
        ) {
            Ok(terminal) => terminal,
            Err(error) => {
                let mut stdout = io::stdout();
                let _ = release_terminal(&mut stdout);
                let _ = disable_raw_mode();
                return Err(error.into());
            }
        };
        TUI_ACTIVE.store(true, Ordering::SeqCst);
        Ok(Self {
            terminal,
            images: TerminalImageRenderer::default(),
            page_overlay_was_open: false,
        })
    }

    fn commit_settled(&mut self, state: &mut TuiState) -> Result<()> {
        // Synchronize overlay lifecycle before any insert_before call. A key
        // dismissal is observed at the top of the next loop, before draw();
        // clearing here prevents the prior overlay frame entering scrollback.
        self.resize_live_viewport(state)?;
        if page_overlay_open(state) {
            return Ok(());
        }
        let size = self.terminal.size()?;
        let transcript_rows = transcript_region_height(state, size.width.max(1), size.height);
        let entries = state.overflow_commit_batch(size.width.max(1), transcript_rows);
        if entries.is_empty() {
            return Ok(());
        }
        let theme = state.themes.theme();
        let mut lines = Vec::new();
        for entry in &entries {
            render_transcript_entry(
                &mut lines,
                entry,
                state.show_thinking,
                state.expand_tools,
                theme,
                size.width.max(1),
            );
        }
        let height = u16::try_from(wrapped_line_count(&lines, size.width.max(1)))
            .unwrap_or(u16::MAX)
            .max(1);
        self.terminal.insert_before(height, |buffer| {
            Paragraph::new(Text::from(lines))
                .style(Style::default().fg(theme.text))
                .wrap(Wrap { trim: false })
                .render(buffer.area, buffer);
        })?;
        // `insert_before` writes directly to the backend, while ratatui keeps
        // the prior frame buffer for its next differential draw. The following
        // draw can therefore skip composer rows overwritten by the committed
        // transcript. Clear row-by-row before invalidating the back buffer:
        // this forces a complete redraw without promoting mutable live rows
        // into scrollback.
        self.clear_live_viewport()?;
        state.finish_commit(entries.len());
        Ok(())
    }

    fn resize_live_viewport(&mut self, state: &TuiState) -> Result<()> {
        self.terminal.autoresize()?;
        let open = page_overlay_open(state);
        if self.page_overlay_was_open && !open {
            self.clear_live_viewport()?;
        }
        self.page_overlay_was_open = open;
        Ok(())
    }

    fn draw(
        &mut self,
        state: &TuiState,
        render: impl FnOnce(&mut ratatui::Frame<'_>, &mut TerminalImageRenderer) -> ImageDrawPlan,
    ) -> Result<()> {
        self.resize_live_viewport(state)?;
        let mut plan = None;
        self.terminal
            .draw(|frame| plan = Some(render(frame, &mut self.images)))?;
        let plan = plan.expect("render closure always produces an image plan");
        self.images.present(
            self.terminal.backend_mut(),
            plan.identity,
            &plan.placements,
        )?;
        Ok(())
    }

    /// Clear the inline viewport without asking tmux to preserve the erased
    /// full-screen frame in history. A home-positioned ED sequence promotes
    /// every visible row to tmux scrollback before clearing; clearing each row
    /// first keeps transient settings/workflow pages out of retained history.
    fn clear_live_viewport(&mut self) -> Result<()> {
        clear_inline_viewport(&mut self.terminal)?;
        Ok(())
    }
    fn yield_to_shell(&mut self) -> Result<()> {
        self.clear_live_viewport()?;
        self.images.cleanup(self.terminal.backend_mut())?;
        if crate::terminal_images::detect_protocol(
            &crate::terminal_images::TerminalEnvironment::current(),
        ) == Some(crate::terminal_images::TerminalImageProtocol::Kitty) {
            let _ = self
                .terminal
                .backend_mut()
                .write_all(crate::terminal_images::KITTY_DELETE_ALL);
        }
        disable_raw_mode()?;
        release_terminal(self.terminal.backend_mut())?;
        TUI_ACTIVE.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn reacquire_from_shell(&mut self) -> Result<()> {
        enable_raw_mode()?;
        acquire_terminal(self.terminal.backend_mut())?;
        self.images = TerminalImageRenderer::default();
        self.page_overlay_was_open = false;
        TUI_ACTIVE.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn suspend<F, Fut, T>(&mut self, operation: F) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        self.yield_to_shell()?;
        let result = operation().await;
        self.reacquire_from_shell()?;
        result
    }
}

fn clear_inline_viewport<B: Backend>(terminal: &mut Terminal<B>) -> io::Result<()> {
    let area = terminal.get_frame().area();
    let backend = terminal.backend_mut();
    for y in area.top()..area.bottom() {
        backend.set_cursor_position((area.left(), y))?;
        backend.clear_region(ClearType::CurrentLine)?;
    }
    Backend::flush(backend)?;
    terminal.clear()
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Best effort: erase only mutable composer/status/overlay rows. Never
        // clear the normal screen or terminal scrollback.
        let _ = self.terminal.clear();
        restore_terminal();
    }
}

/// Unix shutdown-signal fan-in (SIGTERM + SIGHUP). The signal handler is
/// tokio's self-pipe writer (async-signal-safe); it performs no terminal IO.
/// [`ShutdownSignals::recv`] resolves in normal async context, where the
/// caller restores the terminal. On non-Unix targets there are no such
/// signals, so `recv` never resolves and the branch is inert.
#[cfg(unix)]
struct ShutdownSignals {
    sigterm: Signal,
    sighup: Signal,
}

#[cfg(unix)]
impl ShutdownSignals {
    fn new() -> Result<Self> {
        Ok(Self {
            sigterm: signal(SignalKind::terminate())?,
            sighup: signal(SignalKind::hangup())?,
        })
    }

    async fn recv(&mut self) {
        tokio::select! {
            _ = self.sigterm.recv() => {}
            _ = self.sighup.recv() => {}
        }
    }
}

#[cfg(not(unix))]
struct ShutdownSignals;

#[cfg(not(unix))]
impl ShutdownSignals {
    fn new() -> Result<Self> {
        Ok(Self)
    }

    async fn recv(&mut self) {
        std::future::pending::<()>().await;
    }
}

const MAX_UNDO_HISTORY: usize = 100;
const MAX_YANK_HISTORY: usize = 32;

#[derive(Clone)]
struct EditorSnapshot {
    lines: Vec<String>,
    row: usize,
    column: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct EditorPosition {
    row: usize,
    column: usize,
}

#[derive(Clone, Copy)]
struct YankSpan {
    start: EditorPosition,
    end: EditorPosition,
    ring_offset: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditorActionKind {
    Other,
    Insert,
    Kill,
    Yank,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum JumpDirection {
    Forward,
    Backward,
}

struct EditorState {
    lines: Vec<String>,
    row: usize,
    column: usize,
    undo: Vec<EditorSnapshot>,
    kill_ring: Vec<String>,
    last_action: EditorActionKind,
    last_yank: Option<YankSpan>,
    jump_direction: Option<JumpDirection>,
}

impl Default for EditorState {
    fn default() -> Self {
        Self::new()
    }
}

impl EditorState {
    fn new() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            column: 0,
            undo: Vec::new(),
            kill_ring: Vec::new(),
            last_action: EditorActionKind::Other,
            last_yank: None,
            jump_direction: None,
        }
    }

    fn text(&self) -> String {
        self.lines.join("\n")
    }
    fn is_empty(&self) -> bool {
        self.lines.iter().all(String::is_empty)
    }
    fn snapshot(&self) -> EditorSnapshot {
        EditorSnapshot {
            lines: self.lines.clone(),
            row: self.row,
            column: self.column,
        }
    }

    fn record_undo(&mut self) {
        if self.undo.len() == MAX_UNDO_HISTORY {
            self.undo.remove(0);
        }
        self.undo.push(self.snapshot());
    }

    fn record_insert_undo(&mut self) {
        if self.last_action != EditorActionKind::Insert {
            self.record_undo();
        }
        self.last_action = EditorActionKind::Insert;
        self.last_yank = None;
    }

    fn break_insert_chain(&mut self) {
        if self.last_action == EditorActionKind::Insert {
            self.last_action = EditorActionKind::Other;
        }
    }

    fn break_action_chain(&mut self) {
        self.last_action = EditorActionKind::Other;
        self.last_yank = None;
    }

    fn clear(&mut self) {
        if self.is_empty() && self.lines.len() == 1 {
            self.break_action_chain();
            return;
        }
        self.record_undo();
        self.lines = vec![String::new()];
        self.row = 0;
        self.column = 0;
        self.break_action_chain();
    }

    fn set_text(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        if self.text() == normalized {
            return;
        }
        self.record_undo();
        self.lines = normalized.split('\n').map(str::to_owned).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.row = self.lines.len() - 1;
        self.column = self.lines[self.row].len();
        self.break_action_chain();
    }

    fn insert_char(&mut self, character: char) {
        self.record_insert_undo();
        self.lines[self.row].insert(self.column, character);
        self.column += character.len_utf8();
    }

    fn insert_newline(&mut self) {
        self.record_insert_undo();
        let tail = self.lines[self.row].split_off(self.column);
        self.row += 1;
        self.column = 0;
        self.lines.insert(self.row, tail);
    }

    fn insert_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let normalized = normalize_newlines(text);
        self.record_insert_undo();
        self.insert_text_internal(&normalized);
    }

    fn insert_text_internal(&mut self, text: &str) -> EditorPosition {
        let original_row = self.row;
        let tail = self.lines[original_row].split_off(self.column);
        let mut pieces = text.split('\n');
        let first = pieces.next().unwrap_or_default();
        self.lines[original_row].push_str(first);

        let mut inserted = pieces.map(str::to_owned).collect::<Vec<_>>();
        if inserted.is_empty() {
            self.column += first.len();
            self.lines[original_row].push_str(&tail);
        } else {
            self.row = original_row + inserted.len();
            self.column = inserted.last().map_or(0, String::len);
            inserted
                .last_mut()
                .expect("multiline insertion has a last line")
                .push_str(&tail);
            self.lines
                .splice(original_row + 1..original_row + 1, inserted);
        }
        EditorPosition {
            row: self.row,
            column: self.column,
        }
    }

    fn replace_range(
        &mut self,
        row: usize,
        start: usize,
        end: usize,
        value: &str,
        is_directory: bool,
    ) {
        let Some(line) = self.lines.get(row) else {
            return;
        };
        if start > end
            || end > line.len()
            || !line.is_char_boundary(start)
            || !line.is_char_boundary(end)
        {
            return;
        }
        self.record_undo();
        self.break_action_chain();
        let line = &mut self.lines[row];
        let mut replace_end = end;
        if value.ends_with('"') && line[end..].starts_with('"') {
            replace_end += 1;
        }
        let suffix = if is_directory { "" } else { " " };
        line.replace_range(start..replace_end, &format!("{value}{suffix}"));
        self.row = row;
        self.column = start
            + if is_directory && value.ends_with('"') {
                value.len().saturating_sub(1)
            } else {
                value.len() + suffix.len()
            };
    }

    fn backspace(&mut self) {
        if self.column > 0 {
            self.record_undo();
            self.break_action_chain();
            let previous = previous_char_boundary(&self.lines[self.row], self.column);
            self.lines[self.row].replace_range(previous..self.column, "");
            self.column = previous;
        } else if self.row > 0 {
            self.record_undo();
            self.break_action_chain();
            let current = self.lines.remove(self.row);
            self.row -= 1;
            self.column = self.lines[self.row].len();
            self.lines[self.row].push_str(&current);
        }
    }

    fn delete(&mut self) {
        if self.column < self.lines[self.row].len() {
            self.record_undo();
            self.break_action_chain();
            let next = next_char_boundary(&self.lines[self.row], self.column);
            self.lines[self.row].replace_range(self.column..next, "");
        } else if self.row + 1 < self.lines.len() {
            self.record_undo();
            self.break_action_chain();
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    fn move_left(&mut self) {
        self.break_action_chain();
        if self.column > 0 {
            self.column = previous_char_boundary(&self.lines[self.row], self.column);
        } else if self.row > 0 {
            self.row -= 1;
            self.column = self.lines[self.row].len();
        }
    }
    fn move_right(&mut self) {
        self.break_action_chain();
        if self.column < self.lines[self.row].len() {
            self.column = next_char_boundary(&self.lines[self.row], self.column);
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.column = 0;
        }
    }
    fn move_up(&mut self) {
        self.break_action_chain();
        if self.row > 0 {
            self.row -= 1;
            self.column = floor_char_boundary(&self.lines[self.row], self.column);
        }
    }
    fn move_down(&mut self) {
        self.break_action_chain();
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.column = floor_char_boundary(&self.lines[self.row], self.column);
        }
    }
    fn at_first_line(&self) -> bool {
        self.row == 0
    }

    fn at_last_line(&self) -> bool {
        self.row + 1 >= self.lines.len()
    }

    fn move_home(&mut self) {
        self.break_action_chain();
        self.column = 0;
    }
    fn move_end(&mut self) {
        self.break_action_chain();
        self.column = self.lines[self.row].len();
    }

    fn move_word_left(&mut self) {
        self.break_action_chain();
        if self.column == 0 {
            if self.row > 0 {
                self.row -= 1;
                self.column = self.lines[self.row].len();
            }
        } else {
            self.column = word_boundary_backward(&self.lines[self.row], self.column);
        }
    }

    fn move_word_right(&mut self) {
        self.break_action_chain();
        if self.column == self.lines[self.row].len() {
            if self.row + 1 < self.lines.len() {
                self.row += 1;
                self.column = 0;
            }
        } else {
            self.column = word_boundary_forward(&self.lines[self.row], self.column);
        }
    }

    fn begin_jump(&mut self, direction: JumpDirection) {
        self.break_action_chain();
        self.jump_direction = Some(direction);
    }
    fn cancel_jump(&mut self) {
        self.jump_direction = None;
    }

    fn jump_to_char(&mut self, character: char) -> bool {
        let Some(direction) = self.jump_direction.take() else {
            return false;
        };
        self.break_action_chain();
        match direction {
            JumpDirection::Forward => {
                for row in self.row..self.lines.len() {
                    let line = &self.lines[row];
                    let start = if row == self.row {
                        next_char_boundary(line, self.column)
                    } else {
                        0
                    };
                    if let Some(offset) = line[start..].find(character) {
                        self.row = row;
                        self.column = start + offset;
                        return true;
                    }
                }
            }
            JumpDirection::Backward => {
                for row in (0..=self.row).rev() {
                    let line = &self.lines[row];
                    let end = if row == self.row {
                        self.column
                    } else {
                        line.len()
                    };
                    if let Some(index) = line[..end].rfind(character) {
                        self.row = row;
                        self.column = index;
                        return true;
                    }
                }
            }
        }
        false
    }

    fn push_kill(&mut self, text: String, prepend: bool, accumulate: bool) {
        if text.is_empty() {
            return;
        }
        if accumulate && !self.kill_ring.is_empty() {
            let entry = self.kill_ring.last_mut().expect("kill ring checked");
            if prepend {
                entry.insert_str(0, &text);
            } else {
                entry.push_str(&text);
            }
        } else {
            if self.kill_ring.len() == MAX_YANK_HISTORY {
                self.kill_ring.remove(0);
            }
            self.kill_ring.push(text);
        }
        self.last_action = EditorActionKind::Kill;
        self.last_yank = None;
    }

    fn delete_word_backward(&mut self) {
        if self.column == 0 {
            if self.row == 0 {
                return;
            }
            self.record_undo();
            let accumulate = self.last_action == EditorActionKind::Kill;
            let current = self.lines.remove(self.row);
            self.row -= 1;
            self.column = self.lines[self.row].len();
            self.lines[self.row].push_str(&current);
            self.push_kill("\n".to_owned(), true, accumulate);
            return;
        }
        self.record_undo();
        let accumulate = self.last_action == EditorActionKind::Kill;
        let start = word_boundary_backward(&self.lines[self.row], self.column);
        let killed = self.lines[self.row][start..self.column].to_owned();
        self.lines[self.row].replace_range(start..self.column, "");
        self.column = start;
        self.push_kill(killed, true, accumulate);
    }

    fn delete_word_forward(&mut self) {
        if self.column == self.lines[self.row].len() {
            if self.row + 1 == self.lines.len() {
                return;
            }
            self.record_undo();
            let accumulate = self.last_action == EditorActionKind::Kill;
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
            self.push_kill("\n".to_owned(), false, accumulate);
            return;
        }
        self.record_undo();
        let accumulate = self.last_action == EditorActionKind::Kill;
        let end = word_boundary_forward(&self.lines[self.row], self.column);
        let killed = self.lines[self.row][self.column..end].to_owned();
        self.lines[self.row].replace_range(self.column..end, "");
        self.push_kill(killed, false, accumulate);
    }

    fn delete_to_line_start(&mut self) {
        if self.column == 0 {
            self.delete_word_backward();
            return;
        }
        self.record_undo();
        let accumulate = self.last_action == EditorActionKind::Kill;
        let killed = self.lines[self.row][..self.column].to_owned();
        self.lines[self.row].replace_range(..self.column, "");
        self.column = 0;
        self.push_kill(killed, true, accumulate);
    }

    fn delete_to_line_end(&mut self) {
        if self.column == self.lines[self.row].len() {
            self.delete_word_forward();
            return;
        }
        self.record_undo();
        let accumulate = self.last_action == EditorActionKind::Kill;
        let killed = self.lines[self.row][self.column..].to_owned();
        self.lines[self.row].truncate(self.column);
        self.push_kill(killed, false, accumulate);
    }

    fn yank(&mut self) {
        let Some(text) = self.kill_ring.last().cloned() else {
            return;
        };
        self.record_undo();
        let start = EditorPosition {
            row: self.row,
            column: self.column,
        };
        let end = self.insert_text_internal(&text);
        self.last_action = EditorActionKind::Yank;
        self.last_yank = Some(YankSpan {
            start,
            end,
            ring_offset: 0,
        });
    }

    fn yank_pop(&mut self) {
        let Some(span) = self.last_yank else {
            return;
        };
        if self.last_action != EditorActionKind::Yank || self.kill_ring.len() < 2 {
            return;
        }
        self.record_undo();
        self.delete_span(span.start, span.end);
        let ring_offset = (span.ring_offset + 1) % self.kill_ring.len();
        let index = self.kill_ring.len() - 1 - ring_offset;
        let text = self.kill_ring[index].clone();
        let end = self.insert_text_internal(&text);
        self.last_action = EditorActionKind::Yank;
        self.last_yank = Some(YankSpan {
            start: span.start,
            end,
            ring_offset,
        });
    }

    fn delete_span(&mut self, start: EditorPosition, end: EditorPosition) {
        if start.row == end.row {
            self.lines[start.row].replace_range(start.column..end.column, "");
        } else {
            let prefix = self.lines[start.row][..start.column].to_owned();
            let suffix = self.lines[end.row][end.column..].to_owned();
            self.lines
                .splice(start.row..=end.row, [format!("{prefix}{suffix}")]);
        }
        self.row = start.row;
        self.column = start.column;
    }

    fn undo(&mut self) {
        let Some(snapshot) = self.undo.pop() else {
            return;
        };
        self.lines = snapshot.lines;
        self.row = snapshot.row;
        self.column = snapshot.column;
        self.break_action_chain();
        self.jump_direction = None;
    }
}

fn previous_char_boundary(text: &str, cursor: usize) -> usize {
    text[..cursor]
        .char_indices()
        .last()
        .map_or(0, |(index, _)| index)
}

fn next_char_boundary(text: &str, cursor: usize) -> usize {
    text[cursor..]
        .char_indices()
        .nth(1)
        .map_or(text.len(), |(offset, _)| cursor + offset)
}

fn word_class(character: char) -> u8 {
    if character.is_whitespace() {
        0
    } else if character.is_alphanumeric() || character == '_' {
        1
    } else {
        2
    }
}

fn word_boundary_backward(text: &str, cursor: usize) -> usize {
    let mut position = cursor;
    while position > 0 {
        let previous = previous_char_boundary(text, position);
        let character = text[previous..position]
            .chars()
            .next()
            .expect("character boundary");
        if !character.is_whitespace() {
            break;
        }
        position = previous;
    }
    if position == 0 {
        return 0;
    }
    let previous = previous_char_boundary(text, position);
    let class = word_class(
        text[previous..position]
            .chars()
            .next()
            .expect("character boundary"),
    );
    position = previous;
    while position > 0 {
        let previous = previous_char_boundary(text, position);
        let character = text[previous..position]
            .chars()
            .next()
            .expect("character boundary");
        if word_class(character) != class {
            break;
        }
        position = previous;
    }
    position
}

fn word_boundary_forward(text: &str, cursor: usize) -> usize {
    let mut position = cursor;
    while position < text.len() {
        let next = next_char_boundary(text, position);
        let character = text[position..next]
            .chars()
            .next()
            .expect("character boundary");
        if !character.is_whitespace() {
            break;
        }
        position = next;
    }
    if position == text.len() {
        return position;
    }
    let next = next_char_boundary(text, position);
    let class = word_class(
        text[position..next]
            .chars()
            .next()
            .expect("character boundary"),
    );
    position = next;
    while position < text.len() {
        let next = next_char_boundary(text, position);
        let character = text[position..next]
            .chars()
            .next()
            .expect("character boundary");
        if word_class(character) != class {
            break;
        }
        position = next;
    }
    position
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TranscriptKind {
    User,
    Assistant,
    System,
    Custom,
    Tool,
    Job,
}

#[derive(Clone)]
struct ToolTranscript {
    compact: ToolCardRows,
    expanded: ToolCardRows,
}

#[derive(Clone)]
struct TranscriptEntry {
    kind: TranscriptKind,
    content: Vec<ContentBlock>,
    tool_name: Option<String>,
    tool_card: Option<ToolTranscript>,
    job_card: Option<TaskCardRows>,
    is_error: bool,
    is_partial: bool,
}

fn tool_transcript_entry(compact: ToolCardRows, expanded: ToolCardRows) -> TranscriptEntry {
    let is_error = compact.is_error;
    let is_partial = compact.is_partial;
    TranscriptEntry {
        kind: TranscriptKind::Tool,
        content: Vec::new(),
        tool_name: Some(compact.tool_name.clone()),
        tool_card: Some(ToolTranscript { compact, expanded }),
        job_card: None,
        is_error,
        is_partial,
    }
}

struct EffectiveThinkingState<'a> {
    level: ThinkingLevel,
    show_thinking: bool,
    label: Cow<'a, str>,
}

struct TuiState {
    transcript: Vec<TranscriptEntry>,
    tool_cards: ToolCardPresentationAdapter,
    job_cards: JobCardPresentationAdapter,
    committed_entries: usize,
    editor: EditorState,
    /// Submitted prompt drafts newest-last. Seeded from session user messages
    /// and extended once per accepted composer submission.
    prompt_history: Vec<String>,
    /// `None` while editing the live draft; `Some(i)` while browsing history.
    prompt_history_index: Option<usize>,
    /// Draft text captured when Up first leaves the live composer for history.
    prompt_history_draft: Option<String>,
    streaming_text: String,
    streaming_thinking: String,
    thinking_level: ThinkingLevel,
    is_streaming: bool,
    animation_frame: usize,
    is_compacting: bool,
    /// True after the TUI echoes a submitted user prompt immediately (before
    /// the agent emits its `MessageEnd` for the persisted `Message::User`).
    /// Consumed by the next `Message::User` `MessageEnd`, which replaces the
    /// immediate-display entry with the canonical persisted content instead
    /// of appending a second "You" row. Loops and follow-ups have no immediate
    /// display, so their `MessageEnd` falls through to `push_message` and
    /// renders exactly once.
    pending_user_echo: bool,
    show_thinking: bool,
    double_escape_action: DoubleEscapeAction,
    last_escape: Option<std::time::Instant>,
    /// Timestamp of the most recent idle Ctrl-C (`Action::ClearEditor`) that
    /// armed the OMP double-press exit ladder. Cleared on unrelated input.
    last_ctrl_c: Option<std::time::Instant>,
    expand_tools: bool,
    transcript_scroll: usize,
    transcript_page_rows: Cell<usize>,
    show_images: bool,
    image_width_cells: u16,
    status: String,
    /// Ephemeral input/runtime error rendered immediately above the composer.
    /// It is never copied into the transcript or native terminal scrollback.
    composer_error: Option<String>,
    model: String,
    cwd: String,
    completions: CompletionState,
    themes: ThemeManager,
    keybindings: KeyBindingsManager,
    cwd_path: PathBuf,
    pending_attachments: Vec<PendingAttachment>,
    extension_ui: ExtensionUiAdapter,
    extension_dialog: Option<ExtensionDialog>,
    background_tx: mpsc::UnboundedSender<BackgroundEvent>,
    completion_generation: u64,
    completion_query: Option<(usize, AtPrefix)>,
    completion_cancel: Option<CancellationToken>,
    clipboard_read_busy: bool,
    clipboard_write_busy: bool,
    commands: Vec<InteractiveCommand>,
    panel: Option<SelectorPanel>,
    settings_panel: Option<SettingsPanel>,
    settings_value_input: Option<(String, String)>,
    tree_panel: Option<TreePanel>,
    process_panel: Option<ProcessPanel>,
    workflow_panel: Option<WorkflowPanel>,
    agents_panel: Option<AgentsPanel>,
    scoped_models: Option<Vec<Model>>,
    session_selector: Option<SavedSessionSelector>,
    scoped_model_selector: Option<ScopedModelSelector>,
    todo_phases: Vec<TodoPhase>,
    workflow_snapshots: Vec<WorkflowPanelSnapshot>,
    /// Extension-owned working indicator text; authoritative for host queries.
    extension_working_message: Option<String>,
    extension_working_visible: bool,
    extension_hidden_thinking_label: Option<String>,
    extension_title: Option<String>,
    goal_state: GoalState,
    active_loops: std::collections::BTreeMap<String, LoopTask>,
    /// Dedupes live MessageDelivered projections and session CustomMessage IRC.
    seen_irc_message_ids: std::collections::HashSet<String>,
}

fn goal_status_summary(state: &GoalState) -> Option<String> {
    state.current.as_ref().map(|goal| {
        let marker = match goal.lifecycle {
            GoalLifecycle::Active => "🎯",
            GoalLifecycle::Paused => "⏸",
            GoalLifecycle::Completed => "✓",
            GoalLifecycle::Dropped => "✗",
        };
        let usage = goal.token_budget.map_or_else(
            || format!("{}", goal.usage.tokens_used),
            |budget| format!("{}/{}", goal.usage.tokens_used, budget),
        );
        format!("{marker} Goal {usage}")
    })
}
impl TuiState {
    fn new(
        application: &Application,
        extension_ui: ExtensionUiAdapter,
        background_tx: mpsc::UnboundedSender<BackgroundEvent>,
        initial_scoped_models: Option<Vec<Model>>,
    ) -> Self {
        let session = application.session();
        let model = session.model().map_or_else(
            || "no model".to_owned(),
            |model| format!("{}/{}", model.provider, model.id),
        );
        let cwd = session.cwd();
        let (theme_dirs, keybinding_files) = config_paths(cwd);
        let explicit_themes = session
            .resource_manager()
            .map(|resources| {
                resources
                    .snapshot()
                    .themes
                    .iter()
                    .map(|resource| resource.path.clone())
                    .collect()
            })
            .unwrap_or_default();
        let runtime_settings = application
            .resource_snapshot()
            .map(|snapshot| snapshot.settings.tui_runtime())
            .unwrap_or_else(|| pi_coding::Settings::default().tui_runtime());
        let mut themes = ThemeManager::load_sources(theme_dirs, explicit_themes);
        if let Some(theme) = runtime_settings.theme.as_deref() {
            let _ = themes.switch_by_name(theme);
        }
        let mut keybindings = KeyBindingsManager::load(keybinding_files);
        let inline_keybindings_error = keybindings.apply_inline(&runtime_settings.keybindings).err();
        let (commands, command_diagnostics) = interactive_commands(application);
        let mut state = Self {
            transcript: Vec::new(),
            tool_cards: ToolCardPresentationAdapter::new(),
            job_cards: JobCardPresentationAdapter::new(),
            committed_entries: 0,
            editor: EditorState::new(),
            prompt_history: Vec::new(),
            prompt_history_index: None,
            prompt_history_draft: None,
            thinking_level: session.thinking_level(),
            streaming_text: String::new(),
            streaming_thinking: String::new(),
            is_streaming: false,
            animation_frame: 0,
            is_compacting: false,
            pending_user_echo: false,
            show_thinking: runtime_settings.show_thinking,
            double_escape_action: runtime_settings.double_escape_action,
            last_escape: None,
            last_ctrl_c: None,
            expand_tools: false,
            transcript_scroll: 0,
            transcript_page_rows: Cell::new(1),
            show_images: runtime_settings.show_images,
            image_width_cells: runtime_settings.image_width_cells,
            status: "Enter submit · Shift+Enter/Ctrl+J newline · Esc abort · Ctrl+D quit"
                .to_owned(),
            composer_error: None,
            model,
            cwd: cwd.display().to_string(),
            completions: CompletionState::default(),
            themes,
            keybindings,
            cwd_path: cwd.to_path_buf(),
            pending_attachments: Vec::new(),
            extension_ui,
            extension_dialog: None,
            background_tx,
            completion_generation: 0,
            completion_query: None,
            completion_cancel: None,
            clipboard_read_busy: false,
            clipboard_write_busy: false,
            commands,
            panel: None,
            settings_panel: None,
            settings_value_input: None,
            tree_panel: None,
            process_panel: None,
            workflow_panel: None,
            agents_panel: None,
            scoped_models: initial_scoped_models,
            session_selector: None,
            scoped_model_selector: None,
            todo_phases: application.todo_state().phases,
            workflow_snapshots: application.workflow_list().iter().map(WorkflowPanelSnapshot::from).collect(),
            extension_working_message: None,
            extension_working_visible: false,
            extension_hidden_thinking_label: None,
            extension_title: None,
            active_loops: std::collections::BTreeMap::new(),
            seen_irc_message_ids: std::collections::HashSet::new(),
            goal_state: application.goal_state(),
        };
        state.sync_extension_host_bindings();
        for message in session.history() {
            state.push_message(message);
        }
        state.rebuild_prompt_history_from_messages(session.history());
        state.refresh_job_projection(application);
        let diagnostics: Vec<String> = state
            .themes
            .diagnostics()
            .iter()
            .chain(state.keybindings.diagnostics().iter())
            .cloned()
            .collect();
        for diagnostic in diagnostics {
            state.push_status(diagnostic, true);
        }
        if let Some(error) = inline_keybindings_error {
            state.push_status(format!("Invalid inline keybindings: {error}"), true);
        }
        for diagnostic in command_diagnostics {
            state.push_status(diagnostic, true);
        }
        state
    }

    async fn apply_runtime_settings(&mut self, application: &Application) {
        let settings = application.runtime_settings();
        self.show_thinking = settings.tui.show_thinking;
        self.show_images = settings.tui.show_images;
        self.image_width_cells = settings.tui.image_width_cells;
        self.double_escape_action = settings.tui.double_escape_action;
        self.last_escape = None;
        self.last_ctrl_c = None;

        if let Some(snapshot) = application.resource_snapshot() {
            let explicit_themes = snapshot
                .themes
                .iter()
                .map(|resource| resource.path.clone())
                .collect();
            self.themes = ThemeManager::load_sources(snapshot.theme_dirs.clone(), explicit_themes);
            if let Some(theme) = settings.tui.theme.as_deref()
                && let Err(error) = self.themes.switch_by_name(theme)
            {
                self.push_status(error, true);
            }
            self.keybindings = KeyBindingsManager::load(snapshot.keybinding_files.clone());
            if let Err(error) = self.keybindings.apply_inline(&settings.tui.keybindings) {
                self.push_status(format!("Invalid inline keybindings: {error}"), true);
            }
            for diagnostic in self.keybindings.diagnostics().to_vec() {
                self.push_status(diagnostic, true);
            }
        }

        self.scoped_models = match settings.tui.scoped_models.as_deref() {
            Some(patterns) => match crate::session_run::resolve_model_scope(patterns).await {
                Ok(models) => Some(models),
                Err(error) => {
                    self.push_status(format!("Invalid reloaded model scope: {error:#}"), true);
                    self.scoped_models.clone()
                }
            },
            None => None,
        };
        self.sync_extension_host_bindings();
    }

    fn apply_extension_ui(&mut self, event: ExtensionUiEvent) {
        match event {
            ExtensionUiEvent::InteractionRequested { interaction } => {
                if self.extension_dialog.is_some() {
                    let _ = self.extension_ui.cancel(&interaction.id);
                    self.push_status(
                        "Extension interaction rejected because another dialog is active"
                            .to_owned(),
                        true,
                    );
                } else {
                    self.completions.clear();
                    self.extension_dialog = Some(ExtensionDialog::new(interaction));
                }
            }
            ExtensionUiEvent::EditorTextChanged { text, .. } => {
                if !matches!(
                    self.extension_dialog.as_ref().map(|dialog| &dialog.kind),
                    Some(ExtensionDialogKind::Editor { .. } | ExtensionDialogKind::Input { .. })
                ) {
                    self.editor.set_text(&text);
                    self.refresh_completions();
                }
            }
            ExtensionUiEvent::WorkingMessageChanged { message, .. } => {
                self.extension_working_message = message.clone();
                if self.extension_working_visible {
                    if let Some(message) = message.filter(|text| !text.is_empty()) {
                        self.status = message;
                    }
                }
            }
            ExtensionUiEvent::WorkingVisibilityChanged { visible, .. } => {
                self.extension_working_visible = visible;
                if visible {
                    if let Some(message) = self
                        .extension_working_message
                        .as_ref()
                        .filter(|text| !text.is_empty())
                    {
                        self.status = message.clone();
                    }
                }
            }
            ExtensionUiEvent::WorkingIndicatorChanged { .. } => {
                // Frames/interval are retained in the adapter snapshot; the
                // compact TUI has no separate spinner surface beyond status.
            }
            ExtensionUiEvent::HiddenThinkingLabelChanged { label, .. } => {
                self.extension_hidden_thinking_label = label;
            }
            ExtensionUiEvent::ThemeChanged { name, .. } => {
                if let Err(error) = self.themes.switch_by_name(&name) {
                    self.push_status(error, true);
                } else {
                    self.status = format!("Theme: {name}");
                }
            }
            ExtensionUiEvent::ToolsExpandedChanged { expanded, .. } => {
                self.expand_tools = expanded;
            }
            ExtensionUiEvent::TitleChanged { title, .. } => {
                self.extension_title = Some(title);
            }
            ExtensionUiEvent::ExtensionCleared { instance } => {
                if self
                    .extension_dialog
                    .as_ref()
                    .is_some_and(|dialog| dialog.instance() == &instance)
                {
                    self.extension_dialog = None;
                }
                // Re-read owner-scoped adapter snapshot after cleanup so local
                // caches cannot outlive the cleared extension.
                let snapshot = self.extension_ui.snapshot();
                self.extension_working_message = snapshot.working_message;
                self.extension_working_visible = snapshot.working_visible;
                self.extension_hidden_thinking_label = snapshot.hidden_thinking_label;
                self.extension_title = snapshot.title;
                self.expand_tools = snapshot.tools_expanded;
                if let Some(name) = snapshot.active_theme {
                    let _ = self.themes.switch_by_name(&name);
                }
            }
            ExtensionUiEvent::Notification { notification } => {
                self.push_status(notification.message, matches!(notification.level, UiNotificationLevel::Error | UiNotificationLevel::Warning));
            }
            ExtensionUiEvent::StatusChanged { item } => {
                self.status = item.text;
            }
            ExtensionUiEvent::StatusCleared { .. }
            | ExtensionUiEvent::WidgetChanged { .. }
            | ExtensionUiEvent::WidgetCleared { .. } => {}
        }
        self.sync_extension_host_bindings();
    }

    /// Publishes live editor/theme/tools state into the extension adapter so
    /// canonical queries answer from the real TUI reducer, not shadow defaults.
    fn sync_extension_host_bindings(&self) {
        let themes = self
            .themes
            .names()
            .into_iter()
            .map(|name| pi_coding::ExtensionThemeDescriptor {
                name,
                path: None,
            })
            .collect();
        self.extension_ui.set_themes(themes);
        self.extension_ui
            .set_active_theme(Some(self.themes.active_name().to_owned()));
        self.extension_ui
            .set_host_editor_text(self.editor.text());
        self.extension_ui
            .set_host_tools_expanded(self.expand_tools);
    }

    fn reconcile_extension_dialog(&mut self) -> bool {
        let Some(dialog) = &self.extension_dialog else {
            return false;
        };
        if self
            .extension_ui
            .pending_interactions()
            .iter()
            .any(|pending| pending.id == dialog.interaction.id)
        {
            return false;
        }
        self.extension_dialog = None;
        true
    }

    fn cancel_extension_dialogs(&mut self) {
        if let Some(dialog) = self.extension_dialog.take() {
            let _ = self.extension_ui.cancel(&dialog.interaction.id);
        }
        for interaction in self.extension_ui.pending_interactions() {
            let _ = self.extension_ui.cancel(&interaction.id);
        }
    }

    fn finish_extension_dialog(&mut self, cancelled: bool) {
        let Some(dialog) = self.extension_dialog.take() else {
            return;
        };
        let id = dialog.interaction.id;
        let result = if cancelled {
            self.extension_ui.cancel(&id)
        } else {
            match dialog.kind {
                ExtensionDialogKind::Select { options, selected } => {
                    options.get(selected).map_or_else(
                        || self.extension_ui.cancel(&id),
                        |option| self.extension_ui.respond_value(&id, option.value.clone()),
                    )
                }
                ExtensionDialogKind::Confirm { confirmed, .. } => {
                    self.extension_ui.respond_confirmed(&id, confirmed)
                }
                ExtensionDialogKind::Input { editor, .. }
                | ExtensionDialogKind::Editor { editor } => {
                    self.extension_ui.respond_value(&id, editor.text())
                }
            }
        };
        if let Err(error) = result {
            self.push_status(format!("Extension interaction closed: {error}"), true);
        }
    }

    fn reset_tool_projection(&mut self) {
        self.tool_cards.clear();
    }

    fn apply_tool_event(&mut self, event: &AgentEvent) -> bool {
        if let AgentEvent::ToolExecutionStart { tool_name, arguments, .. } = event
            && tool_name.eq_ignore_ascii_case("task")
            && let Some(request) = task_delegation_request(arguments)
        {
            self.job_cards.set_task_request(
                request.context,
                request.children.into_iter().map(|child| (child.name, child.agent, child.task)),
            );
            return true;
        }
        if matches!(event,
            AgentEvent::ToolExecutionUpdate { tool_name, .. }
            | AgentEvent::ToolExecutionEnd { tool_name, .. }
            if tool_name.eq_ignore_ascii_case("task")
        ) || matches!(event,
            AgentEvent::MessageEnd { message: Message::ToolResult(result) }
            if result.tool_name.eq_ignore_ascii_case("task")
        ) {
            return true;
        }
        self.tool_cards.apply_agent_event(event);
        let tool_call_id = match event {
            AgentEvent::ToolExecutionStart { tool_call_id, .. }
            | AgentEvent::ToolExecutionUpdate { tool_call_id, .. }
            | AgentEvent::ToolExecutionEnd { tool_call_id, .. } => Some(tool_call_id.as_str()),
            AgentEvent::MessageEnd { message: Message::ToolResult(result) } => Some(result.tool_call_id.as_str()),
            _ => None,
        };
        if let Some(tool_call_id) = tool_call_id {
            self.upsert_tool_card(tool_call_id);
            return matches!(event, AgentEvent::MessageEnd { message: Message::ToolResult(_) });
        }
        false
    }

    fn upsert_tool_card(&mut self, tool_call_id: &str) {
        let (Some(compact), Some(expanded)) = (
            self.tool_cards.rows(tool_call_id, false),
            self.tool_cards.rows(tool_call_id, true),
        ) else {
            return;
        };
        let entry = tool_transcript_entry(compact, expanded);
        if let Some(existing) = self.transcript.iter_mut().find(|entry| {
            entry.tool_card.as_ref().is_some_and(|tool| {
                tool.compact.tool_call_id == tool_call_id
            })
        }) {
            *existing = entry;
            self.trim_transcript();
            self.follow_transcript();
        } else {
            self.push_entry(entry);
        }
    }

    fn push_bash_execution(&mut self, message: pi_ai::BashExecutionMessage) {
        let compact = ToolCardPresentationAdapter::bash_execution_rows(&message, false);
        let expanded = ToolCardPresentationAdapter::bash_execution_rows(&message, true);
        let tool_call_id = compact.tool_call_id.clone();
        let entry = tool_transcript_entry(compact, expanded);
        if let Some(existing) = self.transcript.iter_mut().find(|entry| {
            entry.tool_card.as_ref().is_some_and(|tool| {
                tool.compact.tool_call_id == tool_call_id
            })
        }) {
            *existing = entry;
            self.trim_transcript();
            self.follow_transcript();
        } else {
            self.push_entry(entry);
        }
    }

    fn push_tool_result(&mut self, message: pi_ai::ToolResultMessage) {
        let tool_call_id = message.tool_call_id.clone();
        self.tool_cards.apply_tool_result(&message);
        self.upsert_tool_card(&tool_call_id);
    }

    fn has_active_animation(&self) -> bool {
        self.is_streaming || self.is_compacting || self.job_cards.running_count() > 0
    }

    fn apply_orchestration_event(&mut self, event: pi_coding::OrchestrationEvent) {
        if let pi_coding::OrchestrationEvent::MessageDelivered { message, .. } = &event {
            let view = orchestration_irc_view_from_mailbox(
                &message.id,
                &message.from,
                &message.to,
                &message.body,
                message.reply_to.as_deref(),
            );
            self.push_irc_view(&view);
        }
        self.job_cards.apply_orchestration_event(&event);
        self.sync_job_cards();
    }

    fn refresh_job_projection(&mut self, application: &Application) {
        self.job_cards.clear();
        let Some(runtime) = application.orchestration_runtime() else {
            return;
        };
        self.job_cards.replace_snapshots(
            runtime.group_id().to_owned(),
            runtime.jobs(None),
            runtime.list(runtime.main_agent_id()),
        );
        self.sync_job_cards();
    }

    fn sync_job_cards(&mut self) {
        let Some(card) = self.job_cards.task_card() else { return };
        let group_id = card.group_id.clone();
        let is_partial = card.children.iter().any(|child| matches!(child.job_status, pi_coding::JobStatus::Queued | pi_coding::JobStatus::Running));
        let is_error = card.children.iter().any(|child| child.job_status == pi_coding::JobStatus::Failed);
        let entry = TranscriptEntry { kind: TranscriptKind::Job, content: Vec::new(), tool_name: None, tool_card: None, job_card: Some(card), is_error, is_partial };
        if let Some(existing) = self.transcript.iter_mut().find(|entry| entry.job_card.as_ref().is_some_and(|card| card.group_id == group_id)) {
            *existing = entry;
        } else {
            self.transcript.push(entry);
        }
        self.trim_transcript();
        self.follow_transcript();
    }

    fn apply(&mut self, event: ApplicationEvent) {
        if let ApplicationEvent::Agent(agent_event) = &event
            && self.apply_tool_event(agent_event)
        {
            return;
        }
        match event {
            ApplicationEvent::Agent(AgentEvent::AgentStart) => {
                self.is_streaming = true;
                self.streaming_text.clear();
                self.streaming_thinking.clear();
                self.follow_transcript();
                self.status = "Working".to_owned();
            }
            ApplicationEvent::Agent(AgentEvent::MessageUpdate {
                assistant_message_event: AssistantMessageEvent::TextDelta { delta, .. },
                ..
            }) => {
                self.streaming_text.push_str(&delta);
                self.follow_transcript();
            }
            ApplicationEvent::Agent(AgentEvent::MessageUpdate {
                assistant_message_event: AssistantMessageEvent::ThinkingDelta { delta, .. },
                ..
            }) => {
                self.streaming_thinking.push_str(&delta);
                self.follow_transcript();
            }
            ApplicationEvent::Agent(
                AgentEvent::ToolExecutionStart { .. }
                | AgentEvent::ToolExecutionUpdate { .. }
                | AgentEvent::ToolExecutionEnd { .. },
            ) => {}
            ApplicationEvent::Agent(AgentEvent::MessageEnd { message }) => {
                if matches!(message, Message::Assistant(_)) {
                    self.streaming_text.clear();
                    self.streaming_thinking.clear();
                }
                if let Message::Custom(custom) = &message
                    && (pi_coding::loop_message_view(custom).is_some()
                        || orchestration_irc_view(custom).is_some())
                {
                    if pi_coding::loop_message_view(custom).is_some() {
                        self.push_loop_message(custom);
                    } else if let Some(irc) = orchestration_irc_view(custom) {
                        self.push_irc_view(&irc);
                    }
                } else if self.pending_user_echo
                    && let Message::User(user) = message
                {
                    // The TUI already echoed this prompt on submission. The
                    // canonical `MessageEnd` only confirms Session persistence
                    // (handled by the session's history subscription, outside
                    // the TUI), so reconcile it into the immediate-display
                    // slot — upgrading to the persisted content blocks (which
                    // carry image attachments) — instead of appending a second
                    // "You" row.
                    self.pending_user_echo = false;
                    self.replace_last_user_entry(TranscriptEntry { kind: TranscriptKind::User, content: user.content, tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false });
                } else {
                    self.push_message(message);
                }
            }
            ApplicationEvent::Orchestration(event) => self.apply_orchestration_event(event),
            ApplicationEvent::Session(pi_coding::SessionEvent::BashExecutionEnd { message }) => {
                self.push_bash_execution(message);
            }
            ApplicationEvent::Session(pi_coding::SessionEvent::AutoRetryStart {
                attempt,
                max_attempts,
                delay_ms,
                ..
            }) => {
                self.status = format!("Retrying ({attempt}/{max_attempts}) in {delay_ms}ms");
            }
            ApplicationEvent::Session(pi_coding::SessionEvent::AutoRetryEnd {
                success,
                final_error,
                ..
            }) => {
                self.status = if success {
                    "Retry succeeded".to_owned()
                } else {
                    final_error.unwrap_or_else(|| "Retry ended".to_owned())
                };
            }
            ApplicationEvent::Session(pi_coding::SessionEvent::CompactionStart { .. }) => {
                self.is_compacting = true;
                self.status = "Compacting context".to_owned();
            }
            ApplicationEvent::Session(pi_coding::SessionEvent::CompactionEnd {
                aborted,
                will_retry,
                error_message,
                ..
            }) => {
                self.is_compacting = false;
                self.status = if aborted {
                    "Compaction aborted".to_owned()
                } else if will_retry {
                    "Compaction will retry".to_owned()
                } else if let Some(error) = error_message {
                    format!("Compaction failed: {error}")
                } else {
                    "Compaction complete".to_owned()
                };
            }
            ApplicationEvent::AgentSettled => {
                self.is_streaming = false;
                self.streaming_text.clear();
                self.streaming_thinking.clear();
                self.status = "Ready".to_owned();
            }
            ApplicationEvent::RunFailed { message } => {
                self.is_streaming = false;
                self.push_status(message, true);
            }
            ApplicationEvent::Exported { path } => {
                self.push_status(format!("Exported to {path}"), false);
            }
            ApplicationEvent::ShareSucceeded { url } => {
                self.push_status(format!("Shared: {url}"), false);
            }
            ApplicationEvent::ShareFailed { message } => {
                self.push_status(format!("Share failed: {message}"), true);
            }
            ApplicationEvent::TodoUpdated { phases, .. } => {
                self.todo_phases = phases;
            }
            ApplicationEvent::Workflow(_) => {}
            ApplicationEvent::TodoReminder { phases } => {
                self.todo_phases = phases;
                let open = todo_open_count(&self.todo_phases);
                self.status = format!("Todo reminder: {open} open task(s)");
            }
            ApplicationEvent::Process(event) => {
                if let Some(panel) = &mut self.process_panel {
                    panel.apply_event(event);
                }
            }
            ApplicationEvent::GoalUpdated { state, .. }
            | ApplicationEvent::GoalUsageCharged { state, .. } => {
                self.goal_state = state;
            }
            ApplicationEvent::Loop(event) => self.apply_loop_event(event),
            _ => {}
        }
    }

    fn apply_workflow_event(&mut self, application: &Application, event: pi_coding::WorkflowEvent) {
        match event {
            pi_coding::WorkflowEvent::Created { snapshot }
            | pi_coding::WorkflowEvent::Updated { snapshot } => {
                let projected = Self::project_workflow_snapshot(application, &snapshot);
                if let Some(current) = self
                    .workflow_snapshots
                    .iter_mut()
                    .find(|current| current.id == projected.id)
                {
                    if current.generation <= projected.generation {
                        *current = projected;
                    }
                } else {
                    self.workflow_snapshots.push(projected);
                }
            }
            pi_coding::WorkflowEvent::StatusChanged {
                workflow_id,
                generation,
                status,
            } => {
                if let Some(current) = self
                    .workflow_snapshots
                    .iter_mut()
                    .find(|current| current.id == workflow_id.as_str())
                    && current.generation == generation
                {
                    current.status = status;
                }
            }
            pi_coding::WorkflowEvent::Removed { workflow_id, generation } => {
                self.workflow_snapshots.retain(|current| {
                    current.id != workflow_id.as_str() || current.generation > generation
                });
            }
        }
        if let Some(panel) = &mut self.workflow_panel {
            panel.replace(self.workflow_snapshots.clone());
        }
    }

    fn apply_loop_event(&mut self, event: LoopEvent) {
        match event {
            LoopEvent::Created { task, .. } | LoopEvent::Updated { task } => {
                let schedule = task.human_schedule();
                let id = task.id.clone();
                self.active_loops.insert(id.clone(), task);
                self.status = format!("Loop {id} scheduled {schedule}");
            }
            LoopEvent::Queued {
                task_id, position, ..
            } => {
                self.status = format!("Loop {task_id} queued at position {position}");
            }
            LoopEvent::Fired { task_id, .. } => {
                self.status = format!("Loop {task_id} running");
            }
            LoopEvent::Skipped {
                task_id, reason, ..
            } => {
                self.status = format!("Loop {task_id} skipped: {reason:?}");
            }
            LoopEvent::Finished { task_id, .. } => {
                self.status = format!("Loop {task_id} finished");
            }
            LoopEvent::Failed {
                task_id, message, ..
            } => {
                self.push_status(format!("Loop {task_id} failed: {message}"), true);
            }
            LoopEvent::Removed { task_id, reason } => {
                self.active_loops.remove(&task_id);
                self.status = format!("Loop {task_id} removed: {reason:?}");
            }
            LoopEvent::SchedulerFailed { message } => {
                self.push_status(format!("Loop scheduler failed: {message}"), true);
            }
        }
    }


    /// Atomically replace every conversation-derived display buffer from the
    /// shared application after tree navigation, fork, resume, or reset.
    /// Reset ledger indices before replay so committed history from the old
    /// conversation can never index into the shorter replacement transcript.
    fn replace_transcript_from_application(&mut self, application: &Application) {
        let session = application.session();
        self.model = session.model().map_or_else(
            || "no model".to_owned(),
            |model| format!("{}/{}", model.provider, model.id),
        );
        self.thinking_level = session.thinking_level();
        self.cwd_path = session.cwd().to_path_buf();
        self.cwd = self.cwd_path.display().to_string();
        self.transcript.clear();
        self.seen_irc_message_ids.clear();
        self.committed_entries = 0;
        self.transcript_scroll = 0;
        self.transcript_page_rows.set(1);
        self.streaming_text.clear();
        self.streaming_thinking.clear();
        self.is_streaming = false;
        self.pending_user_echo = false;
        self.rebuild_prompt_history_from_messages(application.messages());
        self.reset_tool_projection();
        self.job_cards.clear();
        self.todo_phases = application.todo_state().phases;
        self.workflow_snapshots = application.workflow_list().iter().map(WorkflowPanelSnapshot::from).collect();
        for message in application.messages() {
            self.push_message(message);
        }
        self.refresh_job_projection(application);
        self.committed_entries = self.committed_entries.min(self.transcript.len());
    }

    fn push_loop_message(&mut self, message: &pi_ai::CustomMessage) {
        let Some(loop_message) = pi_coding::loop_message_view(message) else { return };
        self.push_entry(TranscriptEntry { kind: TranscriptKind::System, content: vec![ContentBlock::text(loop_message.prompt)], tool_name: Some(format!("Loop {} · {}", loop_message.task_id, loop_message.schedule)), tool_card: None, job_card: None, is_error: false, is_partial: false });
    }

    fn push_irc_view(&mut self, view: &OrchestrationIrcView<'_>) {
        if !self.seen_irc_message_ids.insert(view.id.as_ref().to_owned()) {
            return;
        }
        let from = self.job_cards.agent_display_name(view.from.as_ref());
        let to = self.job_cards.agent_display_name(view.to.as_ref());
        let label = view.label(&from, &to);
        let content = view.body_blocks();
        if content.is_empty() {
            self.push_entry(TranscriptEntry {
                kind: TranscriptKind::Custom,
                content: Vec::new(),
                tool_name: Some(label),
                tool_card: None,
                job_card: None,
                is_error: false,
                is_partial: false,
            });
            return;
        }
        self.push_entry(TranscriptEntry {
            kind: TranscriptKind::Custom,
            content,
            tool_name: Some(label),
            tool_card: None,
            job_card: None,
            is_error: false,
            is_partial: false,
        });
    }

    fn push_message(&mut self, message: Message) {
        match message {
            Message::User(message) => self.push_entry(TranscriptEntry { kind: TranscriptKind::User, content: message.content, tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false }),
            Message::Assistant(message) => {
                let content = message
                    .content
                    .into_iter()
                    .filter(|block| !matches!(block, ContentBlock::ToolCall(_)))
                    .collect::<Vec<_>>();
                if !content.is_empty() {
                    self.push_entry(TranscriptEntry { kind: TranscriptKind::Assistant, content, tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false });
                }
            }
            Message::ToolResult(message) => self.push_tool_result(message),
            Message::BashExecution(message) => self.push_bash_execution(message),
            Message::Custom(message) => {
                if pi_coding::loop_message_view(&message).is_some() {
                    self.push_loop_message(&message);
                } else if let Some(irc) = orchestration_irc_view(&message) {
                    self.push_irc_view(&irc);
                } else if message.display {
                    self.push_entry(TranscriptEntry { kind: TranscriptKind::Custom, content: message.content.into_blocks(), tool_name: Some(message.custom_type), tool_card: None, job_card: None, is_error: false, is_partial: false });
                }
            }
            Message::BranchSummary(message) => self.push_entry(TranscriptEntry { kind: TranscriptKind::System, content: vec![ContentBlock::text(message.summary)], tool_name: Some("Branch summary".to_owned()), tool_card: None, job_card: None, is_error: false, is_partial: false }),
            Message::CompactionSummary(message) => self.push_entry(TranscriptEntry { kind: TranscriptKind::System, content: vec![ContentBlock::text(message.summary)], tool_name: Some("Compaction summary".to_owned()), tool_card: None, job_card: None, is_error: false, is_partial: false }),
        }
    }

    fn push_lines(&mut self, label: &str, text: String, _color: Color) {
        let is_user_echo = label == "You";
        // The immediate "You" echo is the only transcript source for a
        // user-submitted prompt until the agent's `MessageEnd` arrives; arm
        // `pending_user_echo` so that event reconciles into this slot instead
        // of appending a duplicate "You" row.
        if is_user_echo {
            self.pending_user_echo = true;
        }
        self.push_entry(TranscriptEntry { kind: if is_user_echo {
            TranscriptKind::User
        } else {
            TranscriptKind::System
        }, content: vec![ContentBlock::text(text)], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false });
    }

    fn push_status(&mut self, text: String, is_error: bool) {
        if is_error {
            let clean = clean_terminal_text(&text);
            self.status = clean.lines().next().unwrap_or_default().trim().to_owned();
            self.composer_error = Some(text);
            return;
        }
        self.status = text.clone();
        self.push_entry(TranscriptEntry {
            kind: TranscriptKind::System,
            content: vec![ContentBlock::text(text)],
            tool_name: None,
            tool_card: None,
            job_card: None,
            is_error: false,
            is_partial: false,
        });
    }


    fn push_entry(&mut self, entry: TranscriptEntry) {
        self.transcript.push(entry);
        self.trim_transcript();
        self.follow_transcript();
    }

    fn settled_end(&self) -> usize {
        let mut end = self.committed_entries;
        while let Some(entry) = self.transcript.get(end) {
            let pending_user = self.pending_user_echo
                && end + 1 == self.transcript.len()
                && entry.kind == TranscriptKind::User;
            if entry.is_partial || pending_user {
                break;
            }
            end += 1;
        }
        end
    }

    fn settled_commit_batch(&self) -> Vec<TranscriptEntry> {
        self.transcript[self.committed_entries..self.settled_end()].to_vec()
    }

    fn overflow_commit_batch(&self, width: u16, viewport_rows: u16) -> Vec<TranscriptEntry> {
        let theme = self.themes.theme();
        let mut rendered = Vec::new();
        for entry in &self.transcript[self.committed_entries..] {
            render_transcript_entry(
                &mut rendered,
                entry,
                self.show_thinking,
                self.expand_tools,
                theme,
                width,
            );
        }
        if !self.streaming_thinking.is_empty() || !self.streaming_text.is_empty() {
            let mut content = Vec::new();
            if !self.streaming_thinking.is_empty() {
                content.push(ContentBlock::thinking(self.streaming_thinking.clone()));
            }
            if !self.streaming_text.is_empty() {
                content.push(ContentBlock::text(self.streaming_text.clone()));
            }
            render_transcript_entry(
                &mut rendered,
                &TranscriptEntry {
                    kind: TranscriptKind::Assistant,
                    content,
                    tool_name: None,
                    tool_card: None,
                    job_card: None,
                    is_error: false,
                    is_partial: true,
                },
                self.show_thinking,
                self.expand_tools,
                theme,
                width,
            );
        }
        let width = width.max(1);
        let mut rows = wrapped_line_count(&rendered, width);
        let settled_end = self.settled_end();
        let live_entries = self.transcript.len().saturating_sub(self.committed_entries);
        let mut end = self.committed_entries;
        while rows > usize::from(viewport_rows.max(1))
            && end < settled_end
            && live_entries.saturating_sub(end - self.committed_entries) > 1
        {
            let mut entry_lines = Vec::new();
            render_transcript_entry(
                &mut entry_lines,
                &self.transcript[end],
                self.show_thinking,
                self.expand_tools,
                theme,
                width,
            );
            rows = rows.saturating_sub(wrapped_line_count(&entry_lines, width));
            end += 1;
        }
        self.transcript[self.committed_entries..end].to_vec()
    }

    fn finish_commit(&mut self, count: usize) {
        self.committed_entries = self
            .committed_entries
            .saturating_add(count)
            .min(self.transcript.len());
    }

    /// Reconcile a canonical `Message::User` `MessageEnd` into the
    /// immediate-display "You" slot left by [`push_lines`] instead of pushing
    /// a second entry. The immediate echo is always the last transcript entry
    /// when the matching `MessageEnd` arrives (only `AgentStart`, which emits
    /// no transcript row, is processed in between), so this collapses the two
    /// rows into one carrying the persisted content blocks. If the slot was
    /// somehow displaced, fall back to a normal append so the user message is
    /// never lost.
    fn replace_last_user_entry(&mut self, entry: TranscriptEntry) {
        if matches!(
            self.transcript.last(),
            Some(existing) if existing.kind == TranscriptKind::User
        ) {
            if let Some(last) = self.transcript.last_mut() {
                *last = entry;
            }
            self.trim_transcript();
            self.follow_transcript();
        } else {
            self.push_entry(entry);
        }
    }

    fn trim_transcript(&mut self) {
        if self.transcript.len() > MAX_TRANSCRIPT_LINES {
            let excess = self.transcript.len() - MAX_TRANSCRIPT_LINES;
            self.transcript.drain(..excess);
            self.committed_entries = self.committed_entries.saturating_sub(excess);
        }
    }

    fn follow_transcript(&mut self) {
        self.transcript_scroll = 0;
    }

    fn page_transcript(&mut self, direction: i32) {
        let page = self.transcript_page_rows.get().max(1);
        if direction < 0 {
            self.transcript_scroll = self.transcript_scroll.saturating_add(page);
        } else {
            self.transcript_scroll = self.transcript_scroll.saturating_sub(page);
        }
    }

    fn toggle_tool_details(&mut self) {
        self.expand_tools = !self.expand_tools;
        self.sync_extension_host_bindings();
    }

    fn effective_thinking_state(&self) -> EffectiveThinkingState<'_> {
        let label = if self.show_thinking {
            Cow::Borrowed(crate::output::thinking_level_str(self.thinking_level))
        } else if let Some(label) = self
            .extension_hidden_thinking_label
            .as_deref()
            .filter(|label| !label.trim().is_empty())
        {
            Cow::Borrowed(label)
        } else {
            Cow::Owned(format!(
                "{} hidden",
                crate::output::thinking_level_str(self.thinking_level)
            ))
        };
        EffectiveThinkingState {
            level: self.thinking_level,
            show_thinking: self.show_thinking,
            label,
        }
    }

    fn toggle_thinking(&mut self) {
        self.show_thinking = !self.show_thinking;
    }

    /// Rebuild prompt history from the active session's user messages and drop
    /// any in-progress history browse/draft so session transitions cannot leak.
    fn rebuild_prompt_history_from_messages<I>(&mut self, messages: I)
    where
        I: IntoIterator<Item = Message>,
    {
        self.prompt_history.clear();
        self.prompt_history_index = None;
        self.prompt_history_draft = None;
        for message in messages {
            if let Some(text) = message_text(message) {
                self.push_prompt_history_entry(text);
            }
        }
    }

    /// Record one accepted composer draft exactly once (duplicate-suppressed).
    fn record_accepted_prompt(&mut self, draft: &str) {
        self.composer_error = None;
        if draft.trim().is_empty() {
            self.prompt_history_index = None;
            self.prompt_history_draft = None;
            return;
        }
        self.push_prompt_history_entry(draft.to_owned());
        self.prompt_history_index = None;
        self.prompt_history_draft = None;
    }

    fn push_prompt_history_entry(&mut self, text: String) {
        if self.prompt_history.last().is_some_and(|last| last == &text) {
            return;
        }
        self.prompt_history.push(text);
    }

    /// Ctrl-U: clear the whole composer and pending completion UI state.
    fn clear_composer(&mut self) {
        self.editor.clear();
        self.pending_attachments.clear();
        self.cancel_file_completion();
        self.completions.clear();
        self.completion_query = None;
        self.prompt_history_index = None;
        self.prompt_history_draft = None;
    }

    /// Up: move within multiline text first; at the first-line boundary enter
    /// prompt history (newest first, then older).
    fn history_or_move_up(&mut self) {
        if !self.editor.at_first_line() {
            self.editor.move_up();
            return;
        }
        if self.prompt_history.is_empty() {
            return;
        }
        match self.prompt_history_index {
            None => {
                self.prompt_history_draft = Some(self.editor.text());
                let index = self.prompt_history.len() - 1;
                self.prompt_history_index = Some(index);
                self.editor.set_text(&self.prompt_history[index]);
            }
            Some(0) => {}
            Some(index) => {
                let index = index - 1;
                self.prompt_history_index = Some(index);
                self.editor.set_text(&self.prompt_history[index]);
            }
        }
    }

    /// Down: move within multiline text first; at the last-line boundary walk
    /// toward newer history, restoring the saved draft past newest.
    fn history_or_move_down(&mut self) {
        if !self.editor.at_last_line() {
            self.editor.move_down();
            return;
        }
        match self.prompt_history_index {
            None => {}
            Some(index) if index + 1 < self.prompt_history.len() => {
                let index = index + 1;
                self.prompt_history_index = Some(index);
                self.editor.set_text(&self.prompt_history[index]);
            }
            Some(_) => {
                self.prompt_history_index = None;
                let draft = self.prompt_history_draft.take().unwrap_or_default();
                self.editor.set_text(&draft);
            }
        }
    }


    fn poll_theme_reload(&mut self) -> bool {
        let reload = self.themes.reload_if_changed();
        let changed = reload.changed || !reload.diagnostics.is_empty();
        for diagnostic in reload.diagnostics {
            self.push_status(format!("Theme reload ignored: {diagnostic}"), true);
        }
        if reload.changed {
            self.sync_extension_host_bindings();
        }
        changed
    }

    fn refresh_completions(&mut self) {
        let row = self.editor.row;
        let column = self.editor.column;
        let (might_complete_file, slash_text) = {
            let line = &self.editor.lines[row];
            let before_cursor = &line[..column];
            (
                before_cursor.as_bytes().contains(&b'@'),
                (row == 0 && self.editor.lines.len() == 1 && before_cursor.starts_with('/'))
                    .then(|| before_cursor.to_owned()),
            )
        };
        let might_complete_slash = slash_text.is_some();

        if !might_complete_file && !might_complete_slash {
            if self.completion_query.is_some() || self.completion_cancel.is_some() {
                self.cancel_file_completion();
                self.completion_query = None;
            }
            if !self.completions.items.is_empty() || self.completions.context.is_some() {
                self.completions.clear();
            }
            return;
        }

        if might_complete_file
            && let Some(prefix) =
                file_search::current_at_prefix(&self.editor.lines[row], column)
        {
            if self.completion_query.as_ref() == Some(&(row, prefix.clone())) {
                return;
            }
            self.cancel_file_completion();
            self.completion_generation = self.completion_generation.wrapping_add(1);
            let generation = self.completion_generation;
            let cancellation = CancellationToken::new();
            self.completion_query = Some((row, prefix.clone()));
            self.completion_cancel = Some(cancellation.clone());
            self.completions.clear();

            let cwd = self.cwd_path.clone();
            let query = prefix.query.clone();
            let tx = self.background_tx.clone();
            tokio::spawn(async move {
                let result = file_search::search(cwd, query, MAX_COMPLETIONS, cancellation)
                    .await
                    .map_err(|error| error.to_string());
                let _ = tx.send(BackgroundEvent::FileCompletion {
                    generation,
                    row,
                    prefix,
                    result,
                });
            });
            return;
        }

        self.cancel_file_completion();
        self.completion_query = None;
        if !might_complete_slash {
            self.completions.clear();
            return;
        }
        let text = slash_text.expect("slash completion checked");
        let prefix = &text[1..];
        if prefix.contains(char::is_whitespace) {
            self.completions.clear();
            return;
        }
        let commands = if prefix.starts_with("skill:") {
            self.commands
                .iter()
                .filter(|command| command.source == CommandSource::Skill)
                .cloned()
                .collect::<Vec<_>>()
        } else {
            visible_catalog()
        };
        let items = commands
            .into_iter()
            .filter(|command| fuzzy_match(&command.name, prefix))
            .map(|command| CompletionItem {
                value: format!("/{}", command.name),
                label: format!("/{}", command.name),
                description: command.description.clone(),
                is_directory: false,
            })
            .collect::<Vec<_>>();
        // Keep every fuzzy match so `/settings` (and anything past the first
        // MAX_COMPLETIONS alphabetical hits) remains selectable. The popup
        // windows to MAX_COMPLETIONS rows only at render time.
        self.completions.items = items;
        self.completions.context = Some(CompletionContext::Slash);
        // Prefer an exact full-text hit so typing `/ps` highlights `/ps`, not an
        // earlier fuzzy neighbor like `/loops`.
        self.completions.selected = exact_completion_index(&self.completions.items, &text)
            .unwrap_or_else(|| {
                self.completions
                    .selected
                    .min(self.completions.items.len().saturating_sub(1))
            });
    }

    fn cancel_file_completion(&mut self) {
        if let Some(cancellation) = self.completion_cancel.take() {
            cancellation.cancel();
        }
    }

    fn apply_background(&mut self, event: BackgroundEvent) {
        match event {
            BackgroundEvent::FileCompletion {
                generation,
                row,
                prefix,
                result,
            } => {
                let still_current = generation == self.completion_generation
                    && self.completion_query.as_ref() == Some(&(row, prefix.clone()))
                    && self.editor.row == row
                    && file_search::current_at_prefix(&self.editor.lines[row], self.editor.column)
                        .as_ref()
                        == Some(&prefix);
                if !still_current {
                    return;
                }
                self.completion_cancel = None;
                match result {
                    Ok(matches) => {
                        self.completions.items = matches
                            .into_iter()
                            .map(|item| CompletionItem {
                                value: item.value,
                                label: item.label,
                                description: if item.is_directory {
                                    "directory".to_owned()
                                } else {
                                    "file".to_owned()
                                },
                                is_directory: item.is_directory,
                            })
                            .collect();
                        self.completions.selected = 0;
                        self.completions.context = Some(CompletionContext::File {
                            generation,
                            row,
                            prefix,
                        });
                        if self.completions.items.is_empty() {
                            self.completions.context = None;
                        }
                    }
                    Err(error) => {
                        self.completions.clear();
                        self.push_status(format!("File completion failed: {error}"), true);
                    }
                }
            }
            BackgroundEvent::ClipboardRead(result) => {
                self.clipboard_read_busy = false;
                match result {
                    Ok(Some(ClipboardContent::Image(image))) => {
                        let mime_type = image.mime_type.clone();
                        let size = image.bytes.len();
                        self.pending_attachments
                            .push(PendingAttachment::from_clipboard_image(image));
                        self.status = format!(
                            "Attached {mime_type} ({} KiB) · {} pending",
                            size.div_ceil(1024),
                            self.pending_attachments.len()
                        );
                    }
                    Ok(Some(ClipboardContent::Text(text))) => {
                        // Never re-enter empty-paste → clipboard read from a
                        // clipboard text result (avoids a busy loop).
                        if text.is_empty() {
                            self.push_status(
                                "Clipboard is empty · use Alt+V to paste images".to_owned(),
                                true,
                            );
                        } else {
                            self.insert_pasted_text(&text);
                        }
                    }
                    Ok(None) => {
                        self.push_status(
                            "Clipboard is empty · use Alt+V to paste images".to_owned(),
                            true,
                        );
                    }
                    Err(error) => {
                        self.push_status(format!("Clipboard paste failed: {error}"), true);
                    }
                }
            }
            BackgroundEvent::ClipboardWrite(result) => {
                self.clipboard_write_busy = false;
                match result {
                    Ok(()) => self.status = "Copied last assistant message".to_owned(),
                    Err(error) => self.push_status(format!("Clipboard copy failed: {error}"), true),
                }
            }
        }
    }

    fn handle_paste(&mut self, payload: &str) {
        if payload.len() > MAX_PASTE_BYTES {
            self.status = format!(
                "Paste rejected: {} bytes exceeds the {} MiB limit",
                payload.len(),
                MAX_PASTE_BYTES / (1024 * 1024)
            );
            return;
        }
        // Terminals often intercept Ctrl+V as bracketed paste. Image-only
        // clipboards yield an empty text payload; fall through to the async
        // image-capable clipboard reader instead of silently no-oping.
        if payload.is_empty() {
            self.start_clipboard_read();
            return;
        }
        self.insert_pasted_text(payload);
    }

    fn insert_pasted_text(&mut self, payload: &str) {
        if payload.is_empty() {
            return;
        }
        self.editor.insert_text(payload);
        self.status = format!("Pasted {} bytes", payload.len());
        self.refresh_completions();
    }

    fn start_clipboard_read(&mut self) {
        if self.clipboard_read_busy {
            self.status = "Clipboard paste already in progress".to_owned();
            return;
        }
        self.clipboard_read_busy = true;
        // Alt+V is the reliable image-paste chord; Ctrl+V is best-effort
        // because many terminals convert it into bracketed text paste.
        self.status = "Reading clipboard · Alt+V pastes images".to_owned();
        let tx = self.background_tx.clone();
        tokio::spawn(async move {
            let result = clipboard::read().await.map_err(|error| error.to_string());
            let _ = tx.send(BackgroundEvent::ClipboardRead(result));
        });
    }

    fn start_copy(&mut self, application: &Application) {
        if self.clipboard_write_busy {
            self.status = "Clipboard copy already in progress".to_owned();
            return;
        }
        let text = application.last_assistant_text().unwrap_or_default();
        if text.is_empty() {
            self.push_status("No assistant message to copy".to_owned(), true);
            return;
        }
        self.clipboard_write_busy = true;
        self.status = "Copying last assistant message".to_owned();
        let tx = self.background_tx.clone();
        tokio::spawn(async move {
            let result = clipboard::write_text(&text)
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(BackgroundEvent::ClipboardWrite(result));
        });
    }

    fn accept_unambiguous_command_prefix(&mut self) -> bool {
        let text = self.editor.text();
        let Some(prefix) = text.strip_prefix('/') else {
            return false;
        };
        if prefix.is_empty() || prefix.contains(char::is_whitespace) {
            return false;
        }
        let matches = visible_catalog()
            .into_iter()
            .filter(|command| command.name.starts_with(prefix))
            .collect::<Vec<_>>();
        if matches.len() != 1 || matches[0].name == prefix {
            return false;
        }
        self.editor.set_text(&format!("/{}", matches[0].name));
        self.refresh_completions();
        true
    }

    fn accept_completion(&mut self) {
        let Some(item) = self.completions.selected().cloned() else {
            return;
        };
        // Exact slash drafts already equal the selected value (`/goal`, `/ps`).
        // Consume the accept once by dismissing the menu without rewriting the
        // editor — a second apply would look like duplication (`/goall`) when a
        // later key/path also inserts.
        if matches!(self.completions.context, Some(CompletionContext::Slash))
            && item.value == self.editor.text()
        {
            self.completions.clear();
            self.completion_query = None;
            return;
        }
        let refresh_files = match self.completions.context.clone() {
            Some(CompletionContext::Slash) => {
                self.editor.lines.clear();
                self.editor.lines.push(item.value);
                self.editor.row = 0;
                self.editor.column = self.editor.lines[0].len();
                false
            }
            Some(CompletionContext::File {
                generation,
                row,
                prefix,
            }) if generation == self.completion_generation => {
                self.editor.replace_range(
                    row,
                    prefix.start,
                    prefix.end,
                    &item.value,
                    item.is_directory,
                );
                true
            }
            _ => return,
        };
        self.completions.clear();
        self.completion_query = None;
        if refresh_files {
            self.refresh_completions();
        }
    }

    async fn open_model_panel(&mut self, application: &Application) {
        let current = application.state().await.model;
        self.panel = Some(SelectorPanel {
            title: "Select model".to_owned(),
            help: "Type to filter · Enter select · Esc cancel".to_owned(),
            items: available_models().await
                .into_iter()
                .map(|model| PanelItem {
                    checked: current.as_ref().is_some_and(|selected| {
                        selected.provider == model.provider && selected.id == model.id
                    }),
                    label: format!("{}/{}", model.provider, model.id),
                    description: model.name.clone(),
                    value: PanelValue::Model(model),
                })
                .collect(),
            selected: 0,
            query: String::new(),
        });
    }

    async fn open_thinking_panel(&mut self, application: &Application) {
        let state = application.state().await;
        self.panel = Some(SelectorPanel {
            title: "Select thinking level".to_owned(),
            help: "Enter select · Esc cancel".to_owned(),
            items: state
                .model
                .as_ref()
                .map_or_else(|| vec![ThinkingLevel::Off], available_thinking_levels)
                .into_iter()
                .map(|level| PanelItem {
                    label: crate::output::thinking_level_str(level).to_owned(),
                    description: String::new(),
                    value: PanelValue::Thinking(level),
                    checked: level == state.thinking_level,
                })
                .collect(),
            selected: 0,
            query: String::new(),
        });
    }

    async fn open_session_panel(&mut self, application: &Application) {
        let current = application.state().await.session_file.map(PathBuf::from);
        self.panel = None;
        self.agents_panel = None;
        self.tree_panel = None;
        self.scoped_model_selector = None;
        self.session_selector = Some(SavedSessionSelector::new(
            pi_coding::list_sessions(&self.cwd_path),
            current,
        ));
    }

    async fn open_scoped_models_panel(&mut self) {
        self.panel = None;
        self.agents_panel = None;
        self.tree_panel = None;
        self.session_selector = None;
        self.scoped_model_selector = Some(ScopedModelSelector::new(
            available_models().await,
            self.scoped_models.clone(),
        ));
    }

    fn open_settings_panel(&mut self, application: &Application) {
        self.panel = None;
        self.tree_panel = None;
        self.process_panel = None;
        self.agents_panel = None;
        self.session_selector = None;
        self.scoped_model_selector = None;
        match SettingsPanel::from_application(application, pi_coding::SettingsScope::Global) {
            Ok(panel) => self.settings_panel = Some(panel),
            Err(error) => self.push_status(format!("Cannot open settings: {error:#}"), true),
        }
    }

    fn project_workflow_snapshot(application: &Application, snapshot: &pi_coding::WorkflowSnapshot) -> WorkflowPanelSnapshot {
        application.workflow_detail(&snapshot.workflow_id, snapshot.generation).map_or_else(
            |_| WorkflowPanelSnapshot::from(snapshot),
            |detail| WorkflowPanelSnapshot::from_runtime_detail(&detail, snapshot),
        )
    }

    fn open_workflow_panel(&mut self, application: &Application) {
        self.panel = None;
        self.settings_panel = None;
        self.tree_panel = None;
        self.process_panel = None;
        self.agents_panel = None;
        self.session_selector = None;
        self.scoped_model_selector = None;
        self.cancel_file_completion();
        self.editor.clear();
        let workflows = application.workflow_list().iter().map(|snapshot| Self::project_workflow_snapshot(application, snapshot)).collect();
        self.workflow_panel = Some(WorkflowPanel::new(workflows));
    }


    async fn open_agents_panel(&mut self, application: &Application) {
        self.panel = None;
        self.tree_panel = None;
        self.process_panel = None;
        self.session_selector = None;
        self.scoped_model_selector = None;
        match AgentsPanel::from_application(application, available_models().await) {
            Ok(panel) => {
                self.agents_panel = Some(panel);
            }
            Err(error) => self.push_status(format!("Cannot open agents panel: {error:#}"), true),
        }
    }

    fn open_tree_panel(&mut self, application: &Application) {
        self.open_session_tree_panel(application, TreePanelMode::Navigate);
    }

    fn open_fork_panel(&mut self, application: &Application) {
        self.open_session_tree_panel(application, TreePanelMode::Fork);
    }

    fn open_session_tree_panel(&mut self, application: &Application, mode: TreePanelMode) {
        match application.session_tree() {
            Ok(tree) => {
                self.panel = None;
                self.agents_panel = None;
                self.process_panel = None;
                self.tree_panel = Some(TreePanel::new(tree, mode));
            }
            Err(error) => self.push_status(format!("Cannot load session tree: {error:#}"), true),
        }
    }

    fn open_trust_panel(&mut self, application: &Application) {
        let current = application
            .resource_snapshot()
            .map(|snapshot| snapshot.trust.decision);
        self.panel = Some(SelectorPanel {
            title: "Project trust".to_owned(),
            help: "Enter save and reload · Esc cancel".to_owned(),
            items: [
                (
                    "Trusted",
                    "Load project resources and executable extensions",
                    pi_coding::TrustDecision::Trusted,
                ),
                (
                    "Untrusted",
                    "Ignore project resources and extensions",
                    pi_coding::TrustDecision::Untrusted,
                ),
                (
                    "Ask",
                    "Clear the saved decision",
                    pi_coding::TrustDecision::Ask,
                ),
            ]
            .into_iter()
            .map(|(label, description, decision)| PanelItem {
                label: label.to_owned(),
                description: description.to_owned(),
                value: PanelValue::Trust(decision),
                checked: current == Some(decision),
            })
            .collect(),
            selected: 0,
            query: String::new(),
        });
    }

    fn open_goal_panel(&mut self, application: &Application) {
        self.panel = None;
        self.settings_panel = None;
        self.tree_panel = None;
        self.process_panel = None;
        self.agents_panel = None;
        self.session_selector = None;
        self.cancel_file_completion();
        self.editor.clear();
        self.scoped_model_selector = None;
        self.goal_state = application.goal_state();
        let items = match self.goal_state.current.as_ref() {
            None => vec![
                PanelItem {
                    label: "Create goal".to_owned(),
                    description: "Enter an objective and optional token budget".to_owned(),
                    value: PanelValue::GoalCreate,
                    checked: true,
                },
                PanelItem {
                    label: "Show details".to_owned(),
                    description: "Confirm that no goal is active".to_owned(),
                    value: PanelValue::GoalShow,
                    checked: false,
                },
            ],
            Some(goal) => {
                let mut items = vec![PanelItem {
                    label: "Show details".to_owned(),
                    description: crate::goal_commands::format_goal_state(&self.goal_state),
                    value: PanelValue::GoalShow,
                    checked: true,
                }];
                match goal.lifecycle {
                    GoalLifecycle::Active => items.push(PanelItem {
                        label: "Pause".to_owned(),
                        description: "Pause work without dropping the objective".to_owned(),
                        value: PanelValue::GoalPause,
                        checked: false,
                    }),
                    GoalLifecycle::Paused => items.push(PanelItem {
                        label: "Resume".to_owned(),
                        description: "Continue work toward this objective".to_owned(),
                        value: PanelValue::GoalResume,
                        checked: false,
                    }),
                    GoalLifecycle::Completed | GoalLifecycle::Dropped => {}
                }
                if !goal.lifecycle.is_terminal() {
                    items.push(PanelItem {
                        label: "Complete".to_owned(),
                        description: "Mark the objective complete".to_owned(),
                        value: PanelValue::GoalComplete,
                        checked: false,
                    });
                    items.push(PanelItem {
                        label: "Drop".to_owned(),
                        description: "Permanently stop pursuing this goal".to_owned(),
                        value: PanelValue::GoalDrop,
                        checked: false,
                    });
                }
                items
            }
        };
        self.completions.clear();
        self.completion_query = None;
        self.panel = Some(SelectorPanel {
            title: "Goal".to_owned(),
            help: "↑/↓ move · Enter select · Esc cancel".to_owned(),
            items,
            selected: 0,
            query: String::new(),
        });
    }
}

async fn available_models() -> Vec<Model> {
    let mut models = pi_ai::get_providers()
        .into_iter()
        .flat_map(|provider| pi_ai::get_models(&provider))
        .filter(crate::models_config::has_configured_auth)
        .collect::<Vec<_>>();
    models.sort_by(|left, right| (&left.provider, &left.id).cmp(&(&right.provider, &right.id)));
    crate::models_config::filter_models_for_resolved_auth_async(models, None).await
}

fn available_thinking_levels(model: &Model) -> Vec<ThinkingLevel> {
    pi_ai::supported_thinking_levels(model)
        .into_iter()
        .filter_map(|level| match level {
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

async fn handle_tree_panel_key(
    application: &Application,
    state: &mut TuiState,
    key: KeyEvent,
) -> Result<Option<bool>> {
    let Some(mut panel) = state.tree_panel.take() else {
        return Ok(None);
    };
    if panel.mode == TreePanelMode::Navigate && panel.label_input.is_some() {
        match key.code {
            KeyCode::Esc => panel.cancel_label_edit(),
            KeyCode::Backspace => panel.backspace_label(),
            KeyCode::Enter => {
                if let Some((target_id, label)) = panel.finish_label_edit() {
                    match application.set_session_label(&target_id, label.as_deref()) {
                        Ok(()) => {
                            state.open_session_tree_panel(application, panel.mode);
                            return Ok(Some(false));
                        }
                        Err(error) => state.push_status(format!("Failed to update label: {error:#}"), true),
                    }
                }
            }
            KeyCode::Char(character) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => panel.insert_label_char(character),
            _ => state.status = overlay_unknown_key_status(key),
        }
        state.tree_panel = Some(panel);
        return Ok(Some(false));
    }
    let action = state.keybindings.resolve_in(
        &key,
        &[
            Action::EditorUp,
            Action::EditorDown,
            Action::EditorLeft,
            Action::EditorRight,
            Action::EditorPageUp,
            Action::EditorPageDown,
            Action::TreeFoldOrUp,
            Action::TreeUnfoldOrDown,
            Action::TreeEditLabel,
            Action::TreeToggleLabelTimestamp,
            Action::TreeFilterDefault,
            Action::TreeFilterNoTools,
            Action::TreeFilterUserOnly,
            Action::TreeFilterLabeledOnly,
            Action::TreeFilterAll,
            Action::TreeFilterCycleForward,
            Action::TreeFilterCycleBackward,
        ],
    );
    match key.code {
        KeyCode::Esc => {
            if panel.mode == TreePanelMode::Navigate && panel.clear_search_or_folds() {
                state.tree_panel = Some(panel);
            } else {
                state.status = "Ready".to_owned();
            }
        }
        KeyCode::Char('q')
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            state.status = "Ready".to_owned();
        }
        KeyCode::Backspace if panel.mode == TreePanelMode::Navigate => {
            panel.backspace_search();
            state.tree_panel = Some(panel);
        }
        KeyCode::Enter => {
            let Some(selected) = panel.selected_entry().cloned() else {
                state.tree_panel = Some(panel);
                return Ok(Some(false));
            };
            match panel.mode {
                TreePanelMode::Navigate => match application
                    .navigate_tree(&selected.id, pi_coding::NavigateTreeOptions::default())
                    .await
                {
                    Ok(result) => {
                        state.replace_transcript_from_application(application);
                        if let Some(text) = result.editor_text {
                            state.editor.set_text(&text);
                        }
                        state.status = if result.changed {
                            "Navigated session tree".to_owned()
                        } else {
                            "Already at this point".to_owned()
                        };
                    }
                    Err(error) => {
                        state.push_status(
                            format!("Failed to navigate session tree: {error:#}"),
                            true,
                        );
                        state.tree_panel = Some(panel);
                    }
                },
                TreePanelMode::Fork => {
                    if !panel.selected_is_forkable() {
                        state.push_status(
                            "Select a user or custom message to fork".to_owned(),
                            true,
                        );
                        state.tree_panel = Some(panel);
                        return Ok(Some(false));
                    }
                    match application.fork_session(&selected.id).await {
                        Ok(prompt) => {
                            state.replace_transcript_from_application(application);
                            state.editor.set_text(&prompt);
                            state.status =
                                "Forked session; edit and submit the selected prompt".to_owned();
                        }
                        Err(error) => {
                            state.push_status(format!("Failed to fork session: {error:#}"), true);
                            state.tree_panel = Some(panel);
                        }
                    }
                }
            }
        }
        KeyCode::Char(character)
            if panel.mode == TreePanelMode::Navigate
                && action.is_none()
                && !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            panel.insert_search_char(character);
            state.tree_panel = Some(panel);
        }
        _ => {
            if let Some(action) = action {
                panel.apply_action(action, 12);
            } else {
                state.status = overlay_unknown_key_status(key);
            }
            state.tree_panel = Some(panel);
        }
    }
    Ok(Some(false))
}

async fn handle_session_selector_key(
    application: &Application,
    state: &mut TuiState,
    key: KeyEvent,
) -> Result<Option<bool>> {
    let Some(mut selector) = state.session_selector.take() else {
        return Ok(None);
    };
    let action = state.keybindings.resolve_in(
        &key,
        &[
            Action::SessionToggleNamedFilter,
            Action::SessionTogglePath,
            Action::SessionToggleSort,
            Action::SessionRename,
            Action::SessionDelete,
            Action::SessionDeleteNoninvasive,
        ],
    );
    let mut keep_open = true;
    match action {
        Some(Action::SessionToggleNamedFilter) => selector.toggle_named_filter(),
        Some(Action::SessionTogglePath) => selector.toggle_path(),
        Some(Action::SessionToggleSort) => selector.toggle_sort(),
        Some(Action::SessionRename) => selector.begin_rename(),
        Some(Action::SessionDelete) => {
            if !selector.begin_delete(false) {
                state.status = "Cannot delete the currently active session".to_owned();
            }
        }
        Some(Action::SessionDeleteNoninvasive) => {
            selector.begin_delete(true);
        }
        _ => {
            match key.code {
                KeyCode::Esc => {
                    if !selector.cancel_mode() {
                        keep_open = false;
                        state.status = "Ready".to_owned();
                    }
                }
                KeyCode::Char('q')
                    if matches!(selector.mode(), SessionSelectorMode::List)
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    keep_open = false;
                    state.status = "Ready".to_owned();
                }
                KeyCode::Up => selector.move_selection(-1),
                KeyCode::Down => selector.move_selection(1),
                KeyCode::Backspace => match selector.mode() {
                    SessionSelectorMode::Rename { .. } => selector.rename_pop(),
                    SessionSelectorMode::List => selector.pop_query(),
                    SessionSelectorMode::ConfirmDelete { .. } => {}
                },
                KeyCode::Enter => match selector.confirm() {
                    SessionSelectorRequest::None => {}
                    SessionSelectorRequest::Resume(path) => {
                        match pi_coding::SessionCatalog::from_env() {
                            Ok(catalog) => match crate::resume_catalog::switch_resume_selection(
                                application,
                                &catalog,
                                &crate::resume_catalog::ResumeSelectionRequest::Input(
                                    path.to_string_lossy().into_owned(),
                                ),
                                Some(&state.cwd_path),
                            )
                            .await
                            {
                                Ok(result) => {
                                    keep_open = false;
                                    state.replace_transcript_from_application(application);
                                    state.status = format!("Resumed {}", result.path.display());
                                }
                                Err(error) => state.push_status(
                                    format!("Failed to resume session: {error:#}"),
                                    true,
                                ),
                            },
                            Err(error) => state.push_status(
                                format!("Failed to resume session: {error:#}"),
                                true,
                            ),
                        }
                    }
                    SessionSelectorRequest::Rename { path, name } => {
                        match pi_coding::rename_saved_session(&state.cwd_path, &path, &name) {
                            Ok(_) => {
                                selector.reload(pi_coding::list_sessions(&state.cwd_path));
                                state.status = format!("Renamed session to {name}");
                            }
                            Err(error) => state
                                .push_status(format!("Failed to rename session: {error:#}"), true),
                        }
                    }
                    SessionSelectorRequest::Delete(path) => {
                        match pi_coding::delete_saved_session(&state.cwd_path, &path) {
                            Ok(()) => {
                                selector.reload(pi_coding::list_sessions(&state.cwd_path));
                                state.status = "Session deleted".to_owned();
                            }
                            Err(error) => state
                                .push_status(format!("Failed to delete session: {error:#}"), true),
                        }
                    }
                },
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    match selector.mode() {
                        SessionSelectorMode::Rename { .. } => selector.rename_push(character),
                        SessionSelectorMode::List => selector.push_query(character),
                        SessionSelectorMode::ConfirmDelete { .. } => {}
                    }
                }
                _ => state.status = overlay_unknown_key_status(key),
            }
        }
    }
    if keep_open {
        state.session_selector = Some(selector);
    }
    Ok(Some(false))
}

fn handle_scoped_model_selector_key(
    application: &Application,
    state: &mut TuiState,
    key: KeyEvent,
) -> Result<Option<bool>> {
    let Some(mut selector) = state.scoped_model_selector.take() else {
        return Ok(None);
    };
    let action = state.keybindings.resolve_in(
        &key,
        &[
            Action::ModelsSave,
            Action::ModelsEnableAll,
            Action::ModelsClearAll,
            Action::ModelsToggleProvider,
            Action::ModelsReorderUp,
            Action::ModelsReorderDown,
        ],
    );
    let mut keep_open = true;
    let mut selection_changed = false;
    match action {
        Some(Action::ModelsSave) => {
            let patterns = selector.persisted_patterns();
            let result = application
                .session()
                .resource_manager()
                .ok_or_else(|| anyhow::anyhow!("session has no settings manager"))?
                .settings_manager()
                .update_global(|settings| settings.enabled_models = patterns);
            match result {
                Ok(()) => {
                    selector.mark_saved();
                    state.status = "Saved model selection".to_owned();
                }
                Err(error) => {
                    state.push_status(format!("Failed to save model selection: {error:#}"), true)
                }
            }
        }
        Some(Action::ModelsEnableAll) => {
            selector.enable_all();
            selection_changed = true;
        }
        Some(Action::ModelsClearAll) => {
            selector.clear_all();
            selection_changed = true;
        }
        Some(Action::ModelsToggleProvider) => {
            selector.toggle_provider();
            selection_changed = true;
        }
        Some(Action::ModelsReorderUp) => {
            selector.reorder_selected(-1);
            selection_changed = true;
        }
        Some(Action::ModelsReorderDown) => {
            selector.reorder_selected(1);
            selection_changed = true;
        }
        _ => match key.code {
            KeyCode::Esc => {
                keep_open = false;
                state.status = "Ready".to_owned();
            }
            KeyCode::Char('q')
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                keep_open = false;
                state.status = "Ready".to_owned();
            }
            KeyCode::Up => selector.move_selection(-1),
            KeyCode::Down => selector.move_selection(1),
            KeyCode::Backspace => selector.pop_query(),
            KeyCode::Enter => {
                selector.toggle_selected();
                selection_changed = true;
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                selector.push_query(character)
            }
            _ => state.status = overlay_unknown_key_status(key),
        },
    }
    if selection_changed {
        state.scoped_models = selector.scoped_models();
        state.status = match selector.selection() {
            ScopedModelSelection::All => "All models enabled for cycling".to_owned(),
            ScopedModelSelection::Explicit(_) => {
                format!("{} scoped model(s)", selector.enabled_count())
            }
        };
    }
    if keep_open {
        state.scoped_model_selector = Some(selector);
    }
    Ok(Some(false))
}

async fn handle_settings_panel_key(application: &Application, state: &mut TuiState, key: KeyEvent) -> Result<Option<bool>> {
    let Some(mut panel) = state.settings_panel.take() else { return Ok(None); };
    if let Some((setting_key, mut value)) = state.settings_value_input.take() {
        match key.code {
            KeyCode::Esc => {}
            KeyCode::Backspace => { value.pop(); state.settings_value_input = Some((setting_key, value)); }
            KeyCode::Enter => { let parsed = serde_json::from_str::<serde_json::Value>(&value).unwrap_or_else(|_| serde_json::Value::String(value.clone())); if let Err(error) = panel.set_value(&setting_key, parsed) { state.status = format!("Invalid setting value: {error:#}"); state.settings_value_input = Some((setting_key, value)); } }
            KeyCode::Char(character) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => { value.push(character); state.settings_value_input = Some((setting_key, value)); }
            _ => state.settings_value_input = Some((setting_key, value)),
        }
        state.settings_panel = Some(panel); return Ok(Some(false));
    }
    match key.code {
        KeyCode::Esc => { panel.cancel()?; return Ok(Some(false)); }
        KeyCode::Up => panel.move_previous()?, KeyCode::Down => panel.move_next()?,
        KeyCode::Left => panel.previous_category(), KeyCode::Right | KeyCode::Tab => panel.next_category(), KeyCode::BackTab => panel.previous_category(),
        KeyCode::Backspace => { let mut search = panel.search().to_owned(); search.pop(); panel.set_search(search); }
        KeyCode::Delete => if let Some(row) = panel.selected()? { if let Err(error) = panel.reset(&row.key) { state.status = format!("Cannot reset setting: {error:#}"); } },
        KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => if let Err(error) = panel.set_scope(pi_coding::SettingsScope::Global) { state.status = format!("Cannot change settings scope: {error:#}"); },
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => if let Err(error) = panel.set_scope(pi_coding::SettingsScope::Project) { state.status = format!("Cannot change settings scope: {error:#}"); },
        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => match panel.validate() {
            Err(error) => state.status = format!("Settings validation failed: {error:#}"),
            Ok(()) => match panel.apply(application).await {
                Ok(outcome) => { if outcome.applied_live || outcome.reloaded { state.apply_runtime_settings(application).await; } state.status = if outcome.restart_required { "Settings saved; restart required".to_owned() } else { "Settings applied".to_owned() }; }
                Err(error) => state.status = format!("Settings apply failed: {error:#}"),
            },
        },
        KeyCode::Enter => if let Some(row) = panel.selected()? { if row.writable && row.blocked_reason.is_none() { match row.control {
            SettingsControl::Boolean { value } => panel.set_boolean(&row.key, !value.unwrap_or(false))?,
            SettingsControl::Enum { value, options } => if !options.is_empty() { let next = value.as_ref().and_then(|current| options.iter().position(|option| option == current)).map_or(0, |index| (index + 1) % options.len()); panel.set_enum(&row.key, options[next].clone())?; },
            SettingsControl::Secret { .. } => state.status = "Secret settings are managed through auth storage".to_owned(),
            _ => { let initial = row.scope_value.as_ref().unwrap_or(&row.effective_value); state.settings_value_input = Some((row.key, match initial { serde_json::Value::String(value) => value.clone(), other => other.to_string() })); }
        } } },
        KeyCode::Char(character) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => { let mut search = panel.search().to_owned(); search.push(character); panel.set_search(search); }
        _ => {}
    }
    state.settings_panel = Some(panel); Ok(Some(false))
}

async fn handle_workflow_panel_key(application: &Application, state: &mut TuiState, key: KeyEvent) -> Result<Option<bool>> {
    let Some(panel) = state.workflow_panel.as_mut() else { return Ok(None); };
    match panel.handle_key(key) {
        WorkflowPanelResult::Close => { state.workflow_panel = None; state.status = "Ready".to_owned(); }
        WorkflowPanelResult::Intent { workflow_id, kind } => {
            let id = pi_coding::WorkflowId::new(workflow_id);
            let snapshot = application.workflow_get(&id)?;
            let result = match kind {
                WorkflowIntentKind::Pause => application.workflow_pause(&id, snapshot.generation).await,
                WorkflowIntentKind::Resume => application.workflow_resume(&id, snapshot.generation).await,
                WorkflowIntentKind::Cancel => application.workflow_cancel(&id, snapshot.generation).await,
                WorkflowIntentKind::Integrate => application.workflow_integrate(&id, snapshot.generation).await,
            };
            match result {
                Ok(updated) => { application.workflow_select(Some(&id))?; panel.replace(application.workflow_list().iter().map(|snapshot| TuiState::project_workflow_snapshot(application, snapshot)).collect()); state.status = format!("Workflow {}", updated.status.as_str()); }
                Err(error) => state.push_status(format!("Workflow action failed: {error:#}"), true),
            }
        }
        WorkflowPanelResult::Unknown => state.status = overlay_unknown_key_status(key),
        WorkflowPanelResult::Handled => if let Some(selected) = panel.selected_workflow() { application.workflow_select(Some(&pi_coding::WorkflowId::new(selected.id.clone())))?; },
    }
    Ok(Some(false))
}

async fn handle_panel_key(
    application: &Application,
    state: &mut TuiState,
    key: KeyEvent,
) -> Result<Option<bool>> {
    let Some(panel) = state.panel.as_mut() else {
        return Ok(None);
    };
    let visible_count = panel.visible_indices().len();
    match key.code {
        KeyCode::Esc => {
            state.panel = None;
            state.status = "Ready".to_owned();
        }
        KeyCode::Char('q')
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            state.panel = None;
            state.status = "Ready".to_owned();
        }
        KeyCode::Up => panel.selected = panel.selected.saturating_sub(1),
        KeyCode::Down => panel.selected = (panel.selected + 1).min(visible_count.saturating_sub(1)),
        KeyCode::Backspace => {
            panel.query.pop();
            panel.selected = 0;
        }
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            panel.query.push(character);
            panel.selected = 0;
        }
        KeyCode::Enter => {
            let Some(selected) = panel.selected_item().cloned() else {
                return Ok(Some(false));
            };
            match selected.value {
                PanelValue::Model(model) => {
                    let reference = format!("{}/{}", model.provider, model.id);
                    match application.set_model_with_resolved_auth(model).await {
                        Ok(change) => {
                            state.model = reference;
                            state.thinking_level = change.effective;
                            state.panel = None;
                            state.status = if change.clamped {
                                change.message
                            } else {
                                format!("Model: {}", state.model)
                            };
                        }
                        Err(error) => {
                            state.push_status(format!("Cannot switch model: {error:#}"), true)
                        }
                    }
                }
                PanelValue::Thinking(level) => {
                    let change = application.set_thinking_level(level);
                    state.thinking_level = change.effective;
                    state.panel = None;
                    state.status = change.message;
                }
                PanelValue::SettingsThinking => state.open_thinking_panel(application).await,
                PanelValue::SettingsTheme => {
                    state.themes.cycle(1);
                    state.status = format!("Theme: {}", state.themes.active_name());
                    state.open_settings_panel(application);
                }
                PanelValue::SettingsAutoCompact => {
                    let enabled = !application.state().await.auto_compaction_enabled;
                    application.set_auto_compaction_enabled(enabled);
                    state.status = format!(
                        "Automatic compaction {}",
                        if enabled { "enabled" } else { "disabled" }
                    );
                    state.open_settings_panel(application);
                }
                PanelValue::Trust(decision) => {
                    match application.set_project_trust(decision).await {
                        Ok(result) => {
                            state.panel = None;
                            let (commands, diagnostics) = interactive_commands(application);
                            state.commands = commands;
                            state.apply_runtime_settings(application).await;
                            state.status = format!(
                                "Project trust updated; reloaded generation {}",
                                result.generation
                            );
                            for diagnostic in diagnostics {
                                state.push_status(diagnostic, true);
                            }
                        }
                        Err(error) => state.push_status(
                            format!("Failed to update project trust: {error:#}"),
                            true,
                        ),
                    }
                }
                PanelValue::GoalCreate => {
                    state.panel = None;
                    state.editor.set_text("/goal create ");
                    state.status = "Enter the goal objective, then press Enter".to_owned();
                    state.refresh_completions();
                }
                PanelValue::GoalShow => {
                    state.panel = None;
                    state.goal_state = application.goal_state();
                    state.push_lines(
                        "Goal",
                        crate::goal_commands::format_goal_details(&state.goal_state),
                        state.themes.theme().accent,
                    );
                }
                PanelValue::GoalPause => {
                    state.panel = None;
                    dispatch_goal_command(application, state, Some("pause")).await;
                }
                PanelValue::GoalResume => {
                    state.panel = None;
                    dispatch_goal_command(application, state, Some("resume")).await;
                }
                PanelValue::GoalComplete => {
                    state.panel = None;
                    dispatch_goal_command(application, state, Some("complete")).await;
                }
                PanelValue::GoalDrop => {
                    state.panel = None;
                    dispatch_goal_command(application, state, Some("drop")).await;
                }
                PanelValue::Session(_) | PanelValue::ScopedModel(_) => {}
            }
        }
        _ => state.status = overlay_unknown_key_status(key),
    }
    Ok(Some(false))
}

fn handle_extension_dialog_paste(state: &mut TuiState, payload: &str) -> bool {
    let Some(dialog) = state.extension_dialog.as_mut() else {
        return false;
    };
    if payload.len() > MAX_PASTE_BYTES {
        state.status = format!(
            "Paste rejected: {} bytes exceeds the {} MiB limit",
            payload.len(),
            MAX_PASTE_BYTES / (1024 * 1024)
        );
        return true;
    }
    match &mut dialog.kind {
        ExtensionDialogKind::Input { editor, .. } | ExtensionDialogKind::Editor { editor } => {
            editor.insert_text(payload);
        }
        ExtensionDialogKind::Select { .. } | ExtensionDialogKind::Confirm { .. } => {}
    }
    true
}

fn handle_extension_dialog_key(state: &mut TuiState, key: KeyEvent) -> bool {
    if state.extension_dialog.is_none() { return false; }
    if key.code == KeyCode::Esc { state.finish_extension_dialog(true); return true; }
    let action = state.keybindings.resolve(&key);
    let mut accept = false;
    {
        let dialog = state.extension_dialog.as_mut().expect("dialog checked");
        match &mut dialog.kind {
            ExtensionDialogKind::Select { options, selected } => match key.code {
                KeyCode::Up => *selected = selected.saturating_sub(1),
                KeyCode::Down => *selected = (*selected + 1).min(options.len().saturating_sub(1)),
                KeyCode::Enter => accept = true,
                _ => {}
            },
            ExtensionDialogKind::Confirm { confirmed, .. } => match key.code {
                KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down | KeyCode::Tab => *confirmed = !*confirmed,
                KeyCode::Char('y' | 'Y') => { *confirmed = true; accept = true; }
                KeyCode::Char('n' | 'N') => { *confirmed = false; accept = true; }
                KeyCode::Enter => accept = true,
                _ => {}
            },
            ExtensionDialogKind::Input { editor, .. } | ExtensionDialogKind::Editor { editor } => match action {
                Some(Action::EditorSubmit) => accept = true,
                Some(Action::EditorNewline) => editor.insert_newline(),
                Some(Action::EditorBackspace) => editor.backspace(),
                Some(Action::EditorDelete) => editor.delete(),
                Some(Action::EditorLeft) => editor.move_left(),
                Some(Action::EditorRight) => editor.move_right(),
                Some(Action::EditorUp) => editor.move_up(),
                Some(Action::EditorDown) => editor.move_down(),
                Some(Action::EditorWordLeft) => editor.move_word_left(),
                Some(Action::EditorWordRight) => editor.move_word_right(),
                Some(Action::EditorHome) => editor.move_home(),
                Some(Action::EditorEnd) => editor.move_end(),
                Some(Action::EditorDeleteWordBackward) => editor.delete_word_backward(),
                Some(Action::EditorDeleteWordForward) => editor.delete_word_forward(),
                Some(Action::EditorDeleteToLineStart) => editor.delete_to_line_start(),
                Some(Action::EditorDeleteToLineEnd) => editor.delete_to_line_end(),
                Some(Action::EditorClear) => editor.clear(),
                Some(Action::EditorYank) => editor.yank(),
                Some(Action::EditorYankPop) => editor.yank_pop(),
                Some(Action::EditorUndo) => editor.undo(),
                None => if let KeyCode::Char(character) = key.code
                    && !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) { editor.insert_char(character); },
                _ => {}
            },
        }
    }
    if accept { state.finish_extension_dialog(false); }
    true
}

async fn handle_process_panel_key(
    application: &Application,
    state: &mut TuiState,
    key: KeyEvent,
) -> Result<bool> {
    let Some(mut panel) = state.process_panel.take() else { return Ok(false); };
    match panel.handle_key(key) {
        ProcessKeyResult::Action(ProcessPanelAction::Close) => {
            state.status = "Ready".to_owned();
            return Ok(true);
        }
        ProcessKeyResult::Action(ProcessPanelAction::Open(id)) => {
            match application.process_logs(&id, 0, None, false, None).await {
                Ok(logs) => panel.set_logs(&id, &logs),
                Err(error) => panel.fail(format!("Cannot read process output: {error:#}")),
            }
        }
        ProcessKeyResult::Action(ProcessPanelAction::SendText { id, text }) => {
            if let Err(error) = application
                .process_write(&id, text.into_bytes(), false)
                .await
            {
                panel.fail(format!("Cannot send input: {error:#}"));
            }
        }
        ProcessKeyResult::Action(ProcessPanelAction::SendKeys { id, keys }) => {
            if let Err(error) = application.process_send_keys(&id, &keys).await {
                panel.fail(format!("Cannot send key: {error:#}"));
            }
        }
        ProcessKeyResult::Action(ProcessPanelAction::Resize { id, size }) => {
            if let Err(error) = application.process_resize(&id, size) {
                panel.fail(format!("Cannot resize terminal: {error:#}"));
            }
        }
        ProcessKeyResult::Action(ProcessPanelAction::Signal { id, signal }) => {
            if let Err(error) = application.process_signal(&id, signal) {
                panel.fail(format!("Cannot signal process: {error:#}"));
            }
        }
        ProcessKeyResult::Action(ProcessPanelAction::Stop(id)) => {
            match application.process_stop(&id, None).await {
                Ok(process) => panel.update_process(process),
                Err(error) => panel.fail(format!("Cannot stop process: {error:#}")),
            }
        }
        ProcessKeyResult::Handled => {}
        ProcessKeyResult::Unknown => {
            let message = overlay_unknown_key_status(key);
            state.status = message.clone();
            panel.fail(message);
        }
    }
    state.process_panel = Some(panel);
    Ok(true)
}


async fn handle_agents_panel_key(
    application: &Application,
    state: &mut TuiState,
    key: KeyEvent,
) -> Result<Option<bool>> {
    let Some(mut panel) = state.agents_panel.take() else {
        return Ok(None);
    };
    let settings = application
        .session()
        .resource_manager()
        .map(|resources| resources.settings_manager());
    let settings_ref = settings.as_ref();
    match panel.handle_key(key, settings_ref) {
        AgentsPanelAction::Continue(status) => {
            if let Some(status) = status {
                state.status = status;
            }
            state.agents_panel = Some(panel);
        }
        AgentsPanelAction::Saved => {
            // Disk write already succeeded; only report Saved when live runtime reflects it.
            match application.reload().await {
                Ok(result) => {
                    refresh_agents_panel_from_application(application, &mut panel).await;
                    state.status = format!(
                        "Saved global agent settings · live orchestration generation {}",
                        result.generation
                    );
                    state.agents_panel = Some(panel);
                    let (commands, diagnostics) = interactive_commands(application);
                    state.commands = commands;
                    state.apply_runtime_settings(application).await;
                    for diagnostic in diagnostics {
                        state.push_status(diagnostic, true);
                    }
                }
                Err(error) => {
                    // Settings are on disk but runtime is stale — never claim Saved.
                    state.push_status(
                        format!(
                            "Agent settings written, but live reload failed: {error:#}. Run /reload."
                        ),
                        true,
                    );
                    state.agents_panel = Some(panel);
                }
            }
        }
        AgentsPanelAction::Close(status) => {
            state.status = status.unwrap_or_else(|| "Ready".to_owned());
        }
        AgentsPanelAction::Error(error) => {
            state.push_status(error, true);
            state.agents_panel = Some(panel);
        }
    }
    Ok(Some(false))
}

async fn refresh_agents_panel_from_application(application: &Application, panel: &mut AgentsPanel) {
    let models = available_models().await;
    if let Some(snapshot) = application.resource_snapshot() {
        let parent = application.session().model().unwrap_or_default();
        let global_agents = application
            .session()
            .resource_manager()
            .map(|resources| resources.settings_manager().global_settings().agents)
            .unwrap_or_default();
        panel.reload_definitions(
            snapshot.agents.clone(),
            parent,
            &global_agents,
            &snapshot.settings.agents,
            Some(models),
        );
    } else {
        panel.set_model_choices(models);
    }
}

fn is_raw_multiline_paste_start(key: KeyEvent) -> bool {
    key.code == KeyCode::Enter && key.modifiers.is_empty()
}

fn raw_paste_character(key: KeyEvent) -> Option<char> {
    if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
        return None;
    }
    match key.code {
        KeyCode::Char(character) => Some(character),
        KeyCode::Enter => Some('\n'),
        _ => None,
    }
}

/// How a nonblocking unmarked raw input candidate should be delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawInputDisposition {
    /// Insert as a single multiline paste.
    Paste,
    /// Replay each captured key through the normal key path exactly once.
    Keys,
}

/// A nonblocking drain begins only after an unmodified Enter. Classify the
/// candidate as paste once another complete logical line follows; otherwise
/// replay every event through normal key dispatch.
fn classify_raw_input_burst(payload: &str) -> RawInputDisposition {
    if is_raw_multiline_burst(payload) {
        RawInputDisposition::Paste
    } else {
        RawInputDisposition::Keys
    }
}

/// True when `text` has two completed logical line breaks (three line segments).
/// A single Enter batched with surrounding keystrokes is ambiguous and must stay
/// on the key path. CRLF counts as one logical break.
fn is_raw_multiline_burst(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut breaks = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' => {
                breaks += 1;
                if bytes.get(index + 1) == Some(&b'\n') {
                    index += 2;
                } else {
                    index += 1;
                }
                continue;
            }
            b'\n' => {
                breaks += 1;
                index += 1;
                continue;
            }
            _ => {
                if breaks >= 2 {
                    return true;
                }
                index += 1;
            }
        }
    }
    false
}

/// Apply a nonblocking unmarked candidate to the editor. Focused regressions
/// use this helper to model an already-drained terminal event burst.
fn apply_classified_burst(state: &mut TuiState, payload: &str) {
    match classify_raw_input_burst(payload) {
        RawInputDisposition::Paste => handle_paste(state, payload),
        RawInputDisposition::Keys => {
            for character in payload.chars() {
                if character == '\n' {
                    state.editor.insert_newline();
                } else {
                    state.editor.insert_char(character);
                }
                state.refresh_completions();
            }
        }
    }
}



fn exact_completion_index(items: &[CompletionItem], text: &str) -> Option<usize> {
    items.iter().position(|item| item.value == text)
}

/// True when the selected slash completion already equals the editor draft, so
/// Enter should execute rather than re-accept the same value.
fn completion_already_matches_editor(state: &TuiState) -> bool {
    state
        .completions
        .selected()
        .is_some_and(|item| item.value == state.editor.text())
}

fn handle_paste(state: &mut TuiState, payload: &str) {
    if handle_extension_dialog_paste(state, payload) {
        return;
    }
    state.handle_paste(payload);
}

fn dismiss_composer_error_on_escape(state: &mut TuiState, code: KeyCode) -> bool {
    if code != KeyCode::Esc || state.composer_error.take().is_none() {
        return false;
    }
    state.last_escape = None;
    true
}

/// Routes an incoming key through the configured keybindings to a stable action
async fn handle_key(
    application: &Application,
    state: &mut TuiState,
    key: KeyEvent,
    terminal: &mut TerminalGuard,
) -> Result<bool> {
    if handle_extension_dialog_key(state, key) {
        return Ok(false);
    }
    if handle_process_panel_key(application, state, key).await? {
        return Ok(false);
    }
    if let Some(exit) = handle_agents_panel_key(application, state, key).await? {
        return Ok(exit);
    }
    if let Some(exit) = handle_tree_panel_key(application, state, key).await? {
        return Ok(exit);
    }
    if let Some(exit) = handle_session_selector_key(application, state, key).await? {
        return Ok(exit);
    }
    if let Some(exit) = handle_scoped_model_selector_key(application, state, key)? {
        return Ok(exit);
    }
    if let Some(exit) = handle_workflow_panel_key(application, state, key).await? {
        return Ok(exit);
    }
    if let Some(exit) = handle_settings_panel_key(application, state, key).await? {
        return Ok(exit);
    }
    if let Some(exit) = handle_panel_key(application, state, key).await? {
        return Ok(exit);
    }
    if state.editor.jump_direction.is_some() {
        if let KeyCode::Char(character) = key.code
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            state.editor.jump_to_char(character);
            state.refresh_completions();
            return Ok(false);
        }
        state.editor.cancel_jump();
    }

    if dismiss_composer_error_on_escape(state, key.code) {
        return Ok(false);
    }
    if key.code == KeyCode::Esc
        && !state.is_streaming
        && !application.is_bash_running()
        && state.editor.is_empty()
        && state.pending_attachments.is_empty()
        && state.completions.items.is_empty()
    {
        let now = std::time::Instant::now();
        let double = state
            .last_escape
            .replace(now)
            .is_some_and(|previous| now.duration_since(previous) <= std::time::Duration::from_millis(500));
        if double {
            state.last_escape = None;
            match state.double_escape_action {
                DoubleEscapeAction::Tree => state.open_tree_panel(application),
                DoubleEscapeAction::Fork => state.open_fork_panel(application),
                DoubleEscapeAction::None => {}
            }
            return Ok(false);
        }
    } else if key.code != KeyCode::Esc {
        state.last_escape = None;
    }
    let action = state.keybindings.resolve(&key);
    // Unrelated input cancels a pending double-Ctrl-C exit arm. ClearEditor
    // itself manages the timer inside dispatch_action.
    if !matches!(action, Some(Action::ClearEditor)) {
        state.last_ctrl_c = None;
    }

    if action.is_some() {
        state.editor.break_insert_chain();
    }

    // The completion menu intercepts navigation/accept/abort while open; any
    // other action falls through to normal dispatch (e.g. typing narrows it).
    if !state.completions.items.is_empty() {
        if let Some(action) = action {
            match action {
                Action::EditorSubmit
                    if matches!(state.completions.context, Some(CompletionContext::Slash))
                        && !completion_already_matches_editor(state) =>
                {
                    // Partial slash draft: Tab/Enter fills the selected value.
                    // When the editor already equals the selected value (exact
                    // `/ps`), fall through so one Enter executes the command.
                    state.editor.break_insert_chain();
                    state.accept_completion();
                    return Ok(false);
                }
                Action::AcceptCompletion if !completion_already_matches_editor(state) => {
                    state.editor.break_insert_chain();
                    state.accept_completion();
                    return Ok(false);
                }
                Action::AcceptCompletion => {
                    // Exact match: dismiss without rewriting so Tab cannot
                    // re-apply `/goal` and leave a dangling insertion path.
                    state.editor.break_insert_chain();
                    state.completions.clear();
                    state.completion_query = None;
                    return Ok(false);
                }
                Action::Abort => {
                    state.editor.break_insert_chain();
                    state.cancel_file_completion();
                    state.completion_query = None;
                    state.completions.clear();
                    return Ok(false);
                }
                Action::EditorUp => {
                    state.editor.break_insert_chain();
                    state.completions.selected = state.completions.selected.saturating_sub(1);
                    return Ok(false);
                }
                Action::EditorDown => {
                    state.editor.break_insert_chain();
                    state.completions.selected = (state.completions.selected + 1)
                        .min(state.completions.items.len().saturating_sub(1));
                    return Ok(false);
                }
                _ => {}
            }
        }
    }

    let Some(action) = action else {
        // Fallback: a plain printable character with no control/alt modifier
        // inserts into the editor. Bound chords never reach here.
        if let KeyCode::Char(character) = key.code
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            state.editor.insert_char(character);
            state.refresh_completions();
        }
        return Ok(false);
    };
    dispatch_action(application, state, action, terminal).await
}

async fn dispatch_action(
    application: &Application,
    state: &mut TuiState,
    action: Action,
    terminal: &mut TerminalGuard,
) -> Result<bool> {
    match action {
        Action::EditorSubmit => {
            if submit(application, state, terminal).await? {
                return Ok(true);
            }
        }
        Action::EditorNewline => state.editor.insert_newline(),
        Action::EditorBackspace => state.editor.backspace(),
        Action::EditorDelete => state.editor.delete(),
        Action::EditorLeft => state.editor.move_left(),
        Action::EditorRight => state.editor.move_right(),
        Action::EditorUp => state.history_or_move_up(),
        Action::EditorDown => state.history_or_move_down(),
        Action::EditorWordLeft => state.editor.move_word_left(),
        Action::EditorWordRight => state.editor.move_word_right(),
        Action::EditorHome => state.editor.move_home(),
        Action::EditorEnd => state.editor.move_end(),
        Action::EditorJumpForward => state.editor.begin_jump(JumpDirection::Forward),
        Action::EditorJumpBackward => state.editor.begin_jump(JumpDirection::Backward),
        Action::EditorPageUp => state.page_transcript(-1),
        Action::EditorPageDown => state.page_transcript(1),
        Action::EditorDeleteWordBackward => state.editor.delete_word_backward(),
        Action::EditorDeleteWordForward => state.editor.delete_word_forward(),
        Action::EditorDeleteToLineStart => state.editor.delete_to_line_start(),
        Action::EditorDeleteToLineEnd => state.editor.delete_to_line_end(),
        Action::EditorClear => state.clear_composer(),
        Action::EditorYank => state.editor.yank(),
        Action::EditorYankPop => state.editor.yank_pop(),
        Action::EditorUndo => state.editor.undo(),
        Action::Abort => {
            if application.is_bash_running() {
                application.abort_bash();
                state.status = "Aborting bash command".to_owned();
            } else if state.is_streaming {
                application.abort().await;
                state.status = "Aborting".to_owned();
            } else if state.editor.is_empty() && !state.pending_attachments.is_empty() {
                state.pending_attachments.clear();
                state.status = "Removed pending image attachments".to_owned();
            } else {
                state.editor.clear();
            }
        }
        Action::ClearEditor => {
            // OMP-compatible Ctrl-C ladder:
            // 1) bash / streaming still abort on first press
            // 2) nonempty editor or pending attachments clear on first press
            // 3) idle second press within 500ms exits cleanly via normal return
            if application.is_bash_running() {
                application.abort_bash();
                state.last_ctrl_c = None;
                state.status = "Aborting bash command".to_owned();
            } else if state.is_streaming {
                application.abort().await;
                state.last_ctrl_c = None;
                state.status = "Aborting".to_owned();
            } else if handle_ctrl_c_clear_or_exit(state, std::time::Instant::now()) {
                return Ok(true);
            }
        }
        Action::Quit => {
            if state.editor.is_empty() && state.pending_attachments.is_empty() {
                if state.is_streaming {
                    application.abort().await;
                }
                return Ok(true);
            }
            state.editor.delete();
        }

        Action::AcceptCompletion => state.accept_completion(),
        Action::ThemeNext => {
            state.themes.cycle(1);
            state.push_status(format!("Theme: {}", state.themes.active_name()), false);
        }
        Action::ThemePrev => {
            state.themes.cycle(-1);
            state.push_status(format!("Theme: {}", state.themes.active_name()), false);
        }
        Action::ClipboardPaste => state.start_clipboard_read(),
        Action::CopyLastAssistant => state.start_copy(application),
        Action::ExternalEditor => open_external_editor(application, state, terminal).await?,

        Action::Suspend => suspend_process(state, terminal).await?,
        Action::ThinkingCycle => cycle_thinking(application, state).await,

        Action::ThinkingToggle => state.toggle_thinking(),
        Action::ModelCycleForward => cycle_model(application, state, 1).await,
        Action::ModelCycleBackward => cycle_model(application, state, -1).await,
        Action::ModelSelect => state.open_model_panel(application).await,
        Action::ToolsExpand => state.toggle_tool_details(),
        Action::FollowUp => {
            let prompt = state.editor.text();
            if prompt.trim().is_empty() && state.pending_attachments.is_empty() {
                return Ok(false);
            }
            let expanded = match crate::file_args::expand_prompt_in_workspace(
                &prompt,
                application.session().workspace_roots(),
            ) {
                Ok(expanded) => expanded,
                Err(error) => {
                    state.push_status(format!("Prompt was not accepted: {error:#}"), true);
                    return Ok(false);
                }
            };
            let file_images = expanded.images;
            let attachments =
                assemble_submit_attachments(&state.pending_attachments, file_images);
            if state.is_streaming {
                application.follow_up(expanded.prompt, attachments).await;
                state.status = "Queued follow-up".to_owned();
            } else if let Err(error) = application.prompt(expanded.prompt, attachments, None).await
            {
                // Clipboard attachments stay in `pending_attachments` (never drained).
                // File @images remain in the editor text and are re-expanded on retry —
                // do not push them into pending or retries will duplicate them.
                state.push_status(format!("Prompt was not accepted: {error}"), true);
                return Ok(false);
            }
            state.record_accepted_prompt(&prompt);
            state.pending_attachments.clear();
            state.editor.clear();
        }
        Action::Dequeue => {
            let (steering, follow_up) = application.drain_queued_messages().await;
            let restored = steering
                .into_iter()
                .chain(follow_up)
                .filter_map(message_text)
                .collect::<Vec<_>>();
            if restored.is_empty() {
                state.status = "No queued messages to restore".to_owned();
            } else {
                let count = restored.len();
                let queued_text = restored.join("\n\n");
                let current_text = state.editor.text();
                let combined = [queued_text.as_str(), current_text.as_str()]
                    .into_iter()
                    .filter(|text| !text.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                state.editor.set_text(&combined);
                state.status = format!(
                    "Restored {count} queued message{} to editor",
                    if count == 1 { "" } else { "s" }
                );
            }
        }
        Action::SessionNew => {
            if !state.is_streaming {
                match application.new_session().await {
                    Ok(()) => {
                        state.replace_transcript_from_application(application);
                        state.status = "Started a new session".to_owned();
                    }
                    Err(error) => {
                        state.push_status(format!("Failed to start new session: {error:#}"), true)
                    }
                }
            }
        }
        Action::SessionResume => state.open_session_panel(application).await,
        Action::SessionTree => state.open_tree_panel(application),
        Action::SessionFork => state.open_fork_panel(application),
        Action::SessionToggleNamedFilter
        | Action::SessionTogglePath
        | Action::SessionToggleSort
        | Action::SessionRename
        | Action::SessionDelete
        | Action::SessionDeleteNoninvasive
        | Action::ModelsSave
        | Action::ModelsEnableAll
        | Action::ModelsClearAll
        | Action::ModelsToggleProvider
        | Action::ModelsReorderUp
        | Action::ModelsReorderDown
        | Action::TreeFoldOrUp
        | Action::TreeUnfoldOrDown
        | Action::TreeEditLabel
        | Action::TreeToggleLabelTimestamp
        | Action::TreeFilterDefault
        | Action::TreeFilterNoTools
        | Action::TreeFilterUserOnly
        | Action::TreeFilterLabeledOnly
        | Action::TreeFilterAll
        | Action::TreeFilterCycleForward
        | Action::TreeFilterCycleBackward => unreachable!("contextual action bypassed its active selector"),
    }
    if !matches!(action, Action::ClipboardPaste | Action::CopyLastAssistant) {
        state.refresh_completions();
    }
    Ok(false)
}

/// Idle / clear branches of Ctrl-C (`Action::ClearEditor`).
///
/// Returns `true` when a second idle press within 500ms should exit the TUI
/// through the normal return path (terminal restore via `TerminalGuard::drop`).
fn handle_ctrl_c_clear_or_exit(state: &mut TuiState, now: std::time::Instant) -> bool {
    const DOUBLE_CTRL_C_MS: u64 = 500;
    if !state.editor.is_empty() || !state.pending_attachments.is_empty() {
        state.editor.clear();
        state.pending_attachments.clear();
        state.cancel_file_completion();
        state.completions.clear();
        state.completion_query = None;
        state.last_ctrl_c = Some(now);
        state.status = "Cleared · Ctrl+C again to quit".to_owned();
        return false;
    }
    let double = state.last_ctrl_c.take().is_some_and(|previous| {
        now.duration_since(previous) <= std::time::Duration::from_millis(DOUBLE_CTRL_C_MS)
    });
    if double {
        return true;
    }
    state.last_ctrl_c = Some(now);
    state.status = "Ctrl+C again to quit".to_owned();
    false
}

async fn open_external_editor(
    application: &Application,
    state: &mut TuiState,
    terminal: &mut TerminalGuard,
) -> Result<()> {
    let configured = application
        .session()
        .resource_manager()
        .and_then(|resources| {
            resources
                .settings_manager()
                .settings()
                .extra
                .get("externalEditor")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        });
    let command = configured
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("VISUAL")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "notepad".to_owned()
            } else {
                "nano".to_owned()
            }
        });
    let directory = std::env::temp_dir().join(format!("pi-editor-{}", uuid::Uuid::new_v4()));
    let path = directory.join("prompt.md");
    std::fs::create_dir(&directory)?;
    std::fs::write(&path, state.editor.text())?;
    let result = terminal
        .suspend(|| async {
            eprintln!(
                "Launching external editor: {command}\nPi will resume when the editor exits."
            );
            let mut parts = command.split_whitespace();
            let program = parts
                .next()
                .ok_or_else(|| anyhow::anyhow!("external editor command is empty"))?;
            let status = tokio::process::Command::new(program)
                .args(parts)
                .arg(&path)
                .status()
                .await?;
            if status.success() {
                Ok(())
            } else {
                Err(anyhow::anyhow!("external editor exited with {status}"))
            }
        })
        .await;
    match result {
        Ok(()) => match std::fs::read_to_string(&path) {
            Ok(text) => {
                state
                    .editor
                    .set_text(text.strip_suffix('\n').unwrap_or(&text));
                state.status = "External editor changes applied".to_owned();
            }
            Err(error) => state.push_status(
                format!("Failed to read external editor result: {error}"),
                true,
            ),
        },
        Err(error) => state.push_status(format!("External editor failed: {error:#}"), true),
    }
    let _ = std::fs::remove_dir_all(directory);
    Ok(())
}

#[cfg(unix)]
async fn suspend_process(state: &mut TuiState, terminal: &mut TerminalGuard) -> Result<()> {
    terminal
        .suspend(|| async {
            let status = tokio::process::Command::new("kill")
                .args(["-TSTP", "0"])
                .status()
                .await?;
            if status.success() {
                Ok(())
            } else {
                Err(anyhow::anyhow!("suspend signal failed with {status}"))
            }
        })
        .await?;
    state.status = "Resumed".to_owned();
    Ok(())
}

#[cfg(not(unix))]
async fn suspend_process(state: &mut TuiState, _terminal: &mut TerminalGuard) -> Result<()> {
    state.push_status("Suspend is not supported on this platform".to_owned(), true);
    Ok(())
}

fn thinking_level_from_name(name: &str) -> Option<pi_agent::ThinkingLevel> {
    Some(match name {
        "off" => pi_agent::ThinkingLevel::Off,
        "minimal" => pi_agent::ThinkingLevel::Minimal,
        "low" => pi_agent::ThinkingLevel::Low,
        "medium" => pi_agent::ThinkingLevel::Medium,
        "high" => pi_agent::ThinkingLevel::High,
        "xhigh" => pi_agent::ThinkingLevel::Xhigh,
        "max" => pi_agent::ThinkingLevel::Max,
        _ => return None,
    })
}

async fn cycle_thinking(application: &Application, state: &mut TuiState) {
    let application_state = application.state().await;
    let Some(model) = application_state.model.as_ref() else {
        state.push_status("No active model".to_owned(), true);
        return;
    };
    let levels = pi_ai::supported_thinking_levels(model)
        .into_iter()
        .filter_map(thinking_level_from_name)
        .collect::<Vec<_>>();
    if levels.len() <= 1 {
        state.push_status("Current model does not support thinking".to_owned(), true);
        return;
    }
    let current = levels
        .iter()
        .position(|level| *level == application_state.thinking_level)
        .unwrap_or(0);
    let next = levels[(current + 1) % levels.len()];
    let change = application.set_thinking_level(next);
    state.thinking_level = change.effective;
    state.status = change.message;
}

async fn cycle_model(application: &Application, state: &mut TuiState, direction: i32) {
    if let Err(error) = crate::models_config::load_custom_models() {
        state.push_status(format!("Failed to load models: {error:#}"), true);
        return;
    }
    let mut models = match &state.scoped_models {
        None => {
            let mut models = pi_ai::get_providers()
                .into_iter()
                .flat_map(|provider| pi_ai::get_models(&provider))
                .filter(crate::models_config::has_configured_auth)
                .collect::<Vec<_>>();
            models.sort_by(|left, right| {
                (&left.provider, &left.id).cmp(&(&right.provider, &right.id))
            });
            models.dedup_by(|left, right| left.provider == right.provider && left.id == right.id);
            models
        }
        Some(models) => models.clone(),
    };
    models = crate::models_config::filter_models_for_resolved_auth_async(models, None).await;
    if models.is_empty() {
        state.status = "No models enabled for cycling".to_owned();
        return;
    }
    if models.len() == 1 {
        state.status = "Only one model available".to_owned();
        return;
    }
    let current = application.state().await.model;
    let index = current
        .as_ref()
        .and_then(|active| {
            models
                .iter()
                .position(|model| model.provider == active.provider && model.id == active.id)
        })
        .unwrap_or(0);
    let next = if direction < 0 {
        index.checked_sub(1).unwrap_or(models.len() - 1)
    } else {
        (index + 1) % models.len()
    };
    let model = models.remove(next);
    let reference = format!("{}/{}", model.provider, model.id);
    match application.set_model_with_resolved_auth(model).await {
        Ok(change) => {
            state.model = reference;
            state.thinking_level = change.effective;
            state.status = if change.clamped {
                change.message
            } else {
                format!("Switched to {}", state.model)
            };
        }
        Err(error) => state.push_status(format!("Cannot switch model: {error:#}"), true),
    }
}
const fn streaming_submit_behavior(is_streaming: bool) -> Option<StreamingBehavior> {
    if is_streaming {
        Some(StreamingBehavior::Steer)
    } else {
        None
    }
}

/// Reject bare required-arg builtins with visible usage feedback and clear the draft.
/// Returns true when the submission was consumed as a usage rejection.
fn reject_missing_required_arguments(state: &mut TuiState, name: &str, arg: Option<&str>) -> bool {
    if arg.is_some() {
        return false;
    }
    let Some(command) = builtin(name) else {
        return false;
    };
    if !requires_arguments(command) {
        return false;
    }
    state.push_status(format!("Usage: {}", usage(command)), true);
    state.cancel_file_completion();
    state.editor.clear();
    state.completions.clear();
    state.completion_query = None;
    true
}

async fn dispatch_goal_command(
    application: &Application,
    state: &mut TuiState,
    argument: Option<&str>,
) -> bool {
    let command = match crate::goal_commands::parse_interactive_goal_command(argument) {
        Ok(command) => command,
        Err(error) => {
            state.push_status(format!("{error:#}"), true);
            return false;
        }
    };
    let render_details = matches!(
        command,
        crate::goal_commands::InteractiveGoalCommand::Show
            | crate::goal_commands::InteractiveGoalCommand::Create { .. }
    );
    match crate::goal_commands::execute_interactive_goal_command(application, command).await {
        Ok(output) => {
            state.composer_error = None;
            state.goal_state = application.goal_state();
            if render_details {
                // Create/inspect/show surface the OMP-style details block
                // immediately in the live transcript (not only a compact toast).
                state.push_lines(
                    "Goal",
                    crate::goal_commands::format_goal_details(&state.goal_state),
                    state.themes.theme().accent,
                );
            }
            state.status = output;
            true
        }
        Err(error) => {
            state.push_status(format!("Goal command failed: {error:#}"), true);
            false
        }
    }
}
async fn dispatch_workflow_command(
    application: &Application,
    state: &mut TuiState,
    argument: Option<&str>,
) -> bool {
    let command = match crate::workflow_commands::parse_interactive_workflow_command(argument) {
        Ok(command) => command,
        Err(error) => {
            state.push_status(format!("{error:#}"), true);
            return false;
        }
    };
    match crate::workflow_commands::execute_interactive_workflow_on_application(application, command)
        .await
    {
        Ok(crate::workflow_commands::WorkflowCommandEffect::OpenPage) => {
            state.open_workflow_panel(application);
            true
        }
        Ok(crate::workflow_commands::WorkflowCommandEffect::Message(message)) => {
            state.workflow_snapshots = application
                .workflow_list()
                .iter()
                .map(|snapshot| TuiState::project_workflow_snapshot(application, snapshot))
                .collect();
            state.composer_error = None;
            state.status = message;
            true
        }
        Err(error) => {
            state.push_status(format!("Workflow command failed: {error:#}"), true);
            false
        }
    }
}

async fn submit(
    application: &Application,
    state: &mut TuiState,
    terminal: &mut TerminalGuard,
) -> Result<bool> {
    if state.pending_attachments.is_empty() && state.accept_unambiguous_command_prefix() {
        return Ok(false);
    }
    let prompt = state.editor.text();
    if prompt.trim().is_empty() && state.pending_attachments.is_empty() {
        return Ok(false);
    }
    if state.pending_attachments.is_empty()
        && let Some((command, exclude_from_context)) = parse_bash_input(&prompt)
    {
        if application.is_bash_running() {
            state.push_status(
                "A bash command is already running. Press Esc to cancel it first.".to_owned(),
                true,
            );
            return Ok(false);
        }
        let bash_draft = prompt.clone();
        state.cancel_file_completion();
        state.editor.clear();
        state.completions.clear();
        state.completion_query = None;
        state.status = format!("Running !{command}");
        if let Err(error) = application
            .execute_bash(command.clone(), exclude_from_context)
            .await
        {
            state.push_status(format!("Bash command failed: {error:#}"), true);
        } else {
            state.record_accepted_prompt(&bash_draft);
        }
        return Ok(false);
    }
    if state.pending_attachments.is_empty()
        && let Some(command) = prompt.strip_prefix('/')
    {
        let mut parts = command.trim().splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or("");
        let arg = parts.next().map(str::trim).filter(|s| !s.is_empty());
        if reject_missing_required_arguments(state, name, arg) {
            return Ok(false);
        }
        match name {
            "quit" | "exit" => return Ok(true),
            "copy" => state.start_copy(application),
            "new" if !state.is_streaming => match application.new_session().await {
                Ok(()) => {
                    state.replace_transcript_from_application(application);
                    state.status = "Started a new session".to_owned();
                }
                Err(error) => state.push_status(format!("Failed to start new session: {error:#}"), true),
            },
            "settings" => state.open_settings_panel(application),
            "workflow" => {
                if !dispatch_workflow_command(application, state, arg).await {
                    return Ok(false);
                }
            }
            "model" if arg.is_none() => state.open_model_panel(application).await,
            "model" => {
                let spec = arg.expect("guarded");
                match crate::commands::resolve_model_spec(spec) {
                    Ok((model, _)) => {
                        let reference = format!("{}/{}", model.provider, model.id);
                        match application.set_model_with_resolved_auth(model).await {
                            Ok(change) => {
                                state.model = reference;
                                state.thinking_level = change.effective;
                                state.status = if change.clamped {
                                    change.message
                                } else {
                                    format!("Model: {}", state.model)
                                };
                            }
                            Err(error) => state.push_status(format!("Cannot switch model: {error:#}"), true),
                        }
                    }
                    Err(error) => state.push_status(format!("Cannot switch model: {error:#}"), true),
                }
            }
            "models" => {
                let filter = arg.map(str::to_ascii_lowercase);
                let listing = available_models().await.into_iter().filter_map(|model| {
                    let reference = format!("{}/{}", model.provider, model.id);
                    filter.as_ref().map_or(true, |query| reference.to_ascii_lowercase().contains(query)).then_some(reference)
                }).collect::<Vec<_>>().join("\n");
                state.push_lines("Models", if listing.is_empty() { "No available models".to_owned() } else { listing }, state.themes.theme().accent);
            }
            "sessions" => state.open_session_panel(application).await,
            "scoped-models" => state.open_scoped_models_panel().await,
            "resume" if arg.is_none() => state.open_session_panel(application).await,
            "branch" | "fork" => state.open_fork_panel(application),
            "tree" => state.open_tree_panel(application),
            "trust" => state.open_trust_panel(application),
            "resume" => {
                let input = arg.expect("guarded");
                match pi_coding::SessionCatalog::from_env() {
                    Ok(catalog) => match crate::resume_catalog::switch_resume_selection(
                        application,
                        &catalog,
                        &crate::resume_catalog::ResumeSelectionRequest::Input(input.to_owned()),
                        Some(&state.cwd_path),
                    )
                    .await
                    {
                        Ok(result) => {
                            state.replace_transcript_from_application(application);
                            state.status = format!("Resumed {}", result.path.display());
                        }
                        Err(error) => state
                            .push_status(format!("Failed to resume session: {error:#}"), true),
                    },
                    Err(error) => state
                        .push_status(format!("Failed to resume session: {error:#}"), true),
                }
            }
            "clone" if !state.is_streaming => match application.clone_session().await {
                Ok(()) => state.status = "Cloned current session branch".to_owned(),
                Err(error) => state.push_status(format!("Failed to clone session: {error:#}"), true),
            }
            "loop" | "loop-update" if state.is_streaming => {
                state.push_status(
                    format!("/{name} is unavailable while another turn is running"),
                    true,
                );
                return Ok(false);
            }
            "loop" | "loops" | "loop-update" | "loop-delete" | "loop-cancel" => {
                match crate::loop_commands::parse_interactive_loop_command(name, arg) {
                    Ok(Some(command)) => {
                        match crate::loop_commands::execute_interactive_loop_command(
                            application,
                            command,
                        )
                        .await
                        {
                            Ok(output) if name == "loops" => {
                                state.push_lines("Loops", output, state.themes.theme().accent);
                            }
                            Ok(output) => state.status = output,
                            Err(error) => {
                                state.push_status(format!("Loop command failed: {error:#}"), true);
                                return Ok(false);
                            }
                        }
                    }
                    Ok(None) => unreachable!("loop command name was matched"),
                    Err(error) => {
                        state.push_status(format!("{error:#}"), true);
                        return Ok(false);
                    }
                }
            }
            "compact" if !state.is_streaming => match application.compact(arg).await {
                Ok(result) => state.status = format!(
                    "Compacted {} → {} estimated tokens",
                    result.tokens_before,
                    result.estimated_tokens_after.unwrap_or_default()
                ),
                Err(error) => state.push_status(format!("Compaction failed: {error:#}"), true),
            }
            "name" => match arg {
                Some(name) => match application.set_session_name(name) {
                    Ok(()) => state.status = format!("Session name: {name}"),
                    Err(error) => state.push_status(format!("Failed to name session: {error:#}"), true),
                },
                None => {
                    let name = application.state().await.session_name.unwrap_or_else(|| "(unnamed)".to_owned());
                    state.push_status(format!("Session name: {name}"), false);
                }
            },
            "session" => {
                let current = application.state().await;
                state.push_status(
                    format!(
                        "Session {} · {} messages · {}",
                        current.session_id.as_deref().unwrap_or("(not recording)"),
                        current.message_count,
                        current.session_file.as_deref().unwrap_or("in memory")
                    ),
                    false,
                );
            }
            "todo" => match arg {
                Some(markdown) => match pi_coding::parse_todo_markdown(markdown) {
                    Ok(phases) => match application.set_todos(phases) {
                        Ok(result) => {
                            state.todo_phases = result.phases;
                            state.push_status(format!("Todo: {}", result.summary), false);
                        }
                        Err(error) => state.push_status(format!("Failed to set todos: {error:#}"), true),
                    },
                    Err(error) => state.push_status(format!("Invalid todo markdown: {error:#}"), true),
                },
                None => {
                    let text = format_todo_human_lines(&application.todo_state().phases);
                    state.push_lines("Todo", text, state.themes.theme().accent);
                }
            },
            "changelog" => state.push_lines("Changelog", include_str!("../../../CHANGELOG.md").to_owned(), state.themes.theme().accent),
            "hotkeys" => {
                let text = format_hotkeys_text(&state.keybindings);
                state.push_lines("Hotkeys", text, state.themes.theme().accent);
            }
            "reload" if !state.is_streaming => match application.reload().await {
                Ok(result) => {
                    let (commands, diagnostics) = interactive_commands(application);
                    state.commands = commands;
                    state.apply_runtime_settings(application).await;
                    if let Some(panel) = state.agents_panel.as_mut() {
                        refresh_agents_panel_from_application(application, panel).await;
                    }
                    state.status = format!("Reloaded resource generation {}", result.generation);
                    for diagnostic in diagnostics { state.push_status(diagnostic, true); }
                }
                Err(error) => state.push_status(format!("Reload failed: {error:#}"), true),
            },
            "export" if !state.is_streaming => {
                let output = arg.map(PathBuf::from);
                let result = if output.as_ref().is_some_and(|path| path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))) {
                    application.export_jsonl(output.as_deref())
                } else {
                    application.export_html(output.as_deref())
                };
                match result {
                    Ok(path) => state.push_status(format!("Exported {}", path.display()), false),
                    Err(error) => state.push_status(format!("Export failed: {error:#}"), true),
                }
            }
            "import" if !state.is_streaming => match arg {
                Some(input) => match pi_coding::import_session(pi_coding::SourceSessionFormat::Pi, Path::new(input)) {
                    Ok(imported) => match application.switch_session(&imported.path).await {
                        Ok(()) => {
                            state.replace_transcript_from_application(application);
                            state.status = format!("Imported and resumed {}", imported.path.display());
                        }
                        Err(error) => state.push_status(format!("Imported session could not be resumed: {error:#}"), true),
                    },
                    Err(error) => state.push_status(format!("Import failed: {error}"), true),
                },
                None => state.push_status("Usage: /import <path.jsonl>".to_owned(), true),
            }
            "share" if !state.is_streaming => {
                application.share_session();
                state.status = "Sharing...".to_owned();
            }
            "llama" if !state.is_streaming => {
                let command = arg.unwrap_or("status").to_owned();
                match crate::llama_commands::run_slash(&command).await {
                    Ok(message) => state.push_status(message, false),
                    Err(error) => state.push_status(format!("{error:#}"), true),
                }
            }
            "agents" => state.open_agents_panel(application).await,
            "ps" if arg.is_none() => {
                state.panel = None;
                state.tree_panel = None;
                state.session_selector = None;
                state.scoped_model_selector = None;
                state.agents_panel = None;
                state.process_panel = Some(ProcessPanel::new(application.process_list()));
            }
            "ps" | "process" => match crate::process_commands::parse_interactive_process_command(name, arg) {
                Ok(Some(command)) => match crate::process_commands::execute_interactive_process_command(application, command).await {
                    Ok(output) => state.push_lines("Processes", output, state.themes.theme().accent),
                    Err(error) => state.push_status(format!("Process command failed: {error:#}"), true),
                },
                Ok(None) => unreachable!("matched process command"),
                Err(error) => state.push_status(format!("{error:#}"), true),
            }
            "login" if !state.is_streaming => {
                let result = terminal
                    .suspend(|| crate::auth_commands::login(arg, true))
                    .await;
                match result {
                    Ok(info) => state.push_status(
                        format!(
                            "Logged in to {} using {}",
                            info.provider_id,
                            info.credential_type.label()
                        ),
                        false,
                    ),
                    Err(error) => state.push_status(format!("{error:#}"), true),
                }
            }
            "logout" if !state.is_streaming => {
                let result = terminal
                    .suspend(|| crate::auth_commands::logout(arg, true))
                    .await;
                match result {
                    Ok(info) => {
                        state.push_status(format!("Logged out of {}", info.provider_id), false)
                    }
                    Err(error) => state.push_status(format!("{error:#}"), true),
                }
            }
            "help" => {
                let help = visible_catalog()
                    .into_iter()
                    .map(|command| format!("/{:<18} {}", command.name, command.description))
                    .collect::<Vec<_>>()
                    .join("\n");
                state.push_lines("Commands", help, state.themes.theme().accent);
            }
            "theme" => match arg {
                None => {
                    let names = state.themes.names();
                    let current = state.themes.active_name();
                    let listing = names
                        .iter()
                        .map(|name| {
                            if name == current {
                                format!("*{name}")
                            } else {
                                name.clone()
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    state.push_status(format!("Themes: {listing}"), false);
                }
                Some("next") => {
                    state.themes.cycle(1);
                    state.push_status(format!("Theme: {}", state.themes.active_name()), false);
                }
                Some("prev") => {
                    state.themes.cycle(-1);
                    state.push_status(format!("Theme: {}", state.themes.active_name()), false);
                }
                Some(name) => match state.themes.switch_by_name(name) {
                    Ok(()) => state.push_status(format!("Theme: {name}"), false),
                    Err(error) => state.push_status(error, true),
                },
            },
            "run" => {
                match crate::interactive_commands::parse_run_invocation(arg.unwrap_or_default()) {
                    Ok((command, arguments)) => {
                        match crate::interactive_commands::invoke_extension_command(
                            application,
                            command,
                            arguments.to_owned(),
                        )
                        .await
                        {
                            Ok(value) if !value.is_null() => {
                                state.push_status(value.to_string(), false)
                            }
                            Ok(_) => state.status = format!("Ran /{command}"),
                            Err(error) => state
                                .push_status(format!("/run {command} failed: {error:#}"), true),
                        }
                    }
                    Err(error) => state.push_status(format!("{error:#}"), true),
                }
            }
            "goal" if arg.is_none() => state.open_goal_panel(application),
            "goal" => {
                if !dispatch_goal_command(application, state, arg).await {
                    return Ok(false);
                }
            }
            "chain" | "run-chain" => {
                match crate::interactive_commands::parse_chain_invocation(arg.unwrap_or_default()) {
                    Ok(steps) => {
                        match crate::interactive_commands::invoke_extension_chain(
                            application,
                            &steps,
                        )
                        .await
                        {
                            Ok(outputs) => {
                                let summary = outputs
                                    .into_iter()
                                    .map(|(name, value)| {
                                        if value.is_null() {
                                            format!("/{name}: ok")
                                        } else {
                                            format!("/{name}: {value}")
                                        }
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                state.push_lines(
                                    "Chain",
                                    summary,
                                    state.themes.theme().accent,
                                );
                            }
                            Err(error) => {
                                state.push_status(format!("/{name} failed: {error:#}"), true)
                            }
                        }
                    }
                    Err(error) => state.push_status(format!("{error:#}"), true),
                }
            }
            "" => {}
            command => {
                let source = state.commands.iter().find(|candidate| candidate.name == command).map(|candidate| candidate.source);
                match source {
                    Some(CommandSource::Prompt | CommandSource::Skill) => match expand_resource_command(application, command, arg.unwrap_or_default()) {
                        Ok(Some(expanded)) => match application.prompt(expanded.clone(), Vec::new(), None).await {
                            Ok(()) => {
                                state.record_accepted_prompt(&prompt);
                                state.push_lines("You", expanded, state.themes.theme().accent);
                            }
                            Err(error) => state.push_status(format!("Prompt was not accepted: {error}"), true),
                        },
                        Ok(None) => state.push_status(format!("Command /{command} is no longer available; try /reload"), true),
                        Err(error) => state.push_status(format!("Failed to expand /{command}: {error:#}"), true),
                    },
                    Some(CommandSource::Extension) => {
                        let result = match application.extension_runtime() {
                            Some(runtime) => runtime.invoke_command(command, arg.unwrap_or_default().to_owned(), None, None).await,
                            None => Err(anyhow::anyhow!("extension runtime is not loaded")),
                        };
                        match result {
                            Ok(value) if !value.is_null() => state.push_status(value.to_string(), false),
                            Ok(_) => state.status = format!("Ran /{command}"),
                            Err(error) => state.push_status(format!("Extension command /{command} failed: {error:#}"), true),
                        }
                    }
                    Some(CommandSource::Builtin) => {
                        let usage = crate::interactive_commands::builtin(command).map(crate::interactive_commands::usage).unwrap_or_else(|| format!("/{command}"));
                        state.push_status(format!("{usage} is unavailable while the session is busy or missing required arguments"), true);
                    }
                    None => {
                        let suggestion = crate::interactive_commands::closest_builtin(command).map_or_else(String::new, |name| format!(" Did you mean /{name}?"));
                        state.push_status(format!("Unknown command /{command}.{suggestion}"), true);
                    }
                }
            }
        }
        state.cancel_file_completion();
        state.editor.clear();
        state.completions.clear();
        state.completion_query = None;
        return Ok(false);
    }
    let expanded = match crate::file_args::expand_prompt_in_workspace(
        &prompt,
        application.session().workspace_roots(),
    ) {
        Ok(expanded) => expanded,
        Err(error) => {
            state.push_status(format!("Prompt was not accepted: {error:#}"), true);
            return Ok(false);
        }
    };
    let file_images = expanded.images;
    let attachments = assemble_submit_attachments(&state.pending_attachments, file_images);
    let attachment_count = attachments.len();
    let streaming_behavior = streaming_submit_behavior(state.is_streaming);
    if let Err(error) = application
        .prompt(expanded.prompt, attachments, streaming_behavior)
        .await
    {
        // Keep pre-submit clipboard attachments. File images are still referenced
        // by the draft text and must not be copied into pending (duplicate on retry).
        state.push_status(format!("Prompt was not accepted: {error}"), true);
        return Ok(false);
    }
    let display_prompt = if prompt.trim().is_empty() {
        format!(
            "[{attachment_count} image attachment{}]",
            if attachment_count == 1 { "" } else { "s" }
        )
    } else if attachment_count == 0 {
        prompt.clone()
    } else {
        format!(
            "{prompt}\n[{attachment_count} image attachment{}]",
            if attachment_count == 1 { "" } else { "s" }
        )
    };
    state.push_lines("You", display_prompt, state.themes.theme().accent);
    if state.is_streaming {
        state.status = "Steering current response".to_owned();
    }
    state.record_accepted_prompt(&prompt);
    state.pending_attachments.clear();
    state.cancel_file_completion();
    state.editor.clear();
    state.completions.clear();
    state.completion_query = None;
    Ok(false)
}

/// True while a modal page owns the live inline region. These frames are
/// transient: they expand the live viewport while open and must never be
/// committed through `insert_before` into native/tmux scrollback.
fn page_overlay_open(state: &TuiState) -> bool {
    state.panel.is_some()
        || state.tree_panel.is_some()
        || state.process_panel.is_some()
        || state.settings_panel.is_some()
        || state.workflow_panel.is_some()
        || state.agents_panel.is_some()
        || state.session_selector.is_some()
        || state.scoped_model_selector.is_some()
        || state.extension_dialog.is_some()
}

const MIN_COMPOSER_HEIGHT: u16 = 2;
const MAX_COMPOSER_HEIGHT: u16 = 10;
const MAX_INLINE_SUMMARY_HEIGHT: u16 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TuiLayoutHeights {
    transcript: u16,
    todo: u16,
    above: u16,
    composer: u16,
    error: u16,
    completions: u16,
    below: u16,
}

fn desired_composer_height(state: &TuiState, width: u16) -> u16 {
    let attachment_rows = pending_attachment_labels(&state.pending_attachments).len();
    if state.editor.lines.len() <= 1 && attachment_rows == 0 {
        return MIN_COMPOSER_HEIGHT;
    }
    u16::try_from(
        state
            .editor
            .lines
            .iter()
            .map(|line| wrapped_row_count(&clean_terminal_text(line), usize::from(width.saturating_sub(5))))
            .sum::<usize>()
            .saturating_add(attachment_rows)
            .saturating_add(2),
    )
    .unwrap_or(u16::MAX)
    .clamp(3, MAX_COMPOSER_HEIGHT)
}

fn tui_layout_heights(
    state: &TuiState,
    width: u16,
    terminal_height: u16,
    todo_height: u16,
    above_height: u16,
    completion_height: u16,
    below_height: u16,
) -> TuiLayoutHeights {
    let composer = desired_composer_height(state, width).min(terminal_height);
    let mut optional_budget = terminal_height
        .saturating_sub(composer)
        .saturating_sub(1);
    let error = composer_error_toast_height(state, width).min(optional_budget);
    optional_budget = optional_budget.saturating_sub(error);
    let todo = todo_height
        .min(MAX_INLINE_SUMMARY_HEIGHT)
        .min(optional_budget);
    optional_budget = optional_budget.saturating_sub(todo);
    let above = above_height.min(optional_budget);
    optional_budget = optional_budget.saturating_sub(above);
    let completions = completion_height.min(optional_budget);
    optional_budget = optional_budget.saturating_sub(completions);
    let below = below_height.min(optional_budget);
    let transcript = terminal_height
        .saturating_sub(composer)
        .saturating_sub(todo)
        .saturating_sub(above)
        .saturating_sub(error)
        .saturating_sub(completions)
        .saturating_sub(below);
    TuiLayoutHeights {
        transcript,
        todo,
        above,
        error,
        composer,
        completions,
        below,
    }
}

fn live_viewport_height(state: &TuiState, width: u16, terminal_height: u16) -> u16 {
    let theme = state.themes.theme();
    let extension = state.extension_ui.snapshot();
    let above_height = u16::try_from(extension_widget_lines(&extension, UiWidgetPlacement::AboveEditor, theme).len())
        .unwrap_or(u16::MAX)
        .min(6);
    let below_height = u16::try_from(extension_widget_lines(&extension, UiWidgetPlacement::BelowEditor, theme).len())
        .unwrap_or(u16::MAX)
        .min(6);
    let completion_height = u16::try_from(state.completions.items.len())
        .unwrap_or(u16::MAX)
        .min(u16::try_from(MAX_COMPLETIONS).unwrap_or(u16::MAX));
    let todo_height = u16::try_from(render_todo_panel_lines(
        &state.todo_phases,
        &state.job_cards.cards_in_source_order(),
        theme,
        width.max(1),
    ).len())
    .unwrap_or(u16::MAX);
    let layout = tui_layout_heights(
        state,
        width,
        terminal_height,
        todo_height,
        above_height,
        completion_height,
        below_height,
    );
    let progress_is_capped = todo_height > layout.todo;
    let mut live_lines = Vec::new();
    for entry in &state.transcript[state.committed_entries..] {
        trim_inter_entry_blank_before_user(&mut live_lines, entry);
        render_transcript_entry(&mut live_lines, entry, state.show_thinking, state.expand_tools, theme, width.max(1));
    }
    let raw_transcript = u16::try_from(wrapped_line_count(&live_lines, width.max(1))).unwrap_or(u16::MAX);
    let chrome = layout.todo
        .saturating_add(layout.above)
        .saturating_add(layout.error)
        .saturating_add(layout.composer)
        .saturating_add(layout.completions)
        .saturating_add(layout.below)
        .saturating_add(1);
    let overlay_height = if page_overlay_open(state) { terminal_height.min(16) } else { 0 };
    if progress_is_capped {
        terminal_height
    } else {
        chrome
            .saturating_add(raw_transcript.min(8).min(terminal_height.saturating_sub(chrome)))
            .max(overlay_height)
            .clamp(3, terminal_height.max(3))
    }
}


fn transcript_region_height(state: &TuiState, width: u16, terminal_height: u16) -> u16 {
    let theme = state.themes.theme();
    let extension = state.extension_ui.snapshot();
    let above_height = u16::try_from(extension_widget_lines(&extension, UiWidgetPlacement::AboveEditor, theme).len())
        .unwrap_or(u16::MAX)
        .min(6);
    let below_height = u16::try_from(extension_widget_lines(&extension, UiWidgetPlacement::BelowEditor, theme).len())
        .unwrap_or(u16::MAX)
        .min(6);
    let completion_height = u16::try_from(state.completions.items.len())
        .unwrap_or(u16::MAX)
        .min(u16::try_from(MAX_COMPLETIONS).unwrap_or(u16::MAX));
    let todo_height = u16::try_from(render_todo_panel_lines(
        &state.todo_phases,
        &state.job_cards.cards_in_source_order(),
        theme,
        width.max(1),
    ).len())
    .unwrap_or(u16::MAX);
    tui_layout_heights(
        state,
        width,
        terminal_height,
        todo_height,
        above_height,
        completion_height,
        below_height,
    )
    .transcript
    .max(1)
}

fn compact_cwd(cwd: &str) -> String {
    let home = std::env::var("HOME").ok();
    home.as_deref().and_then(|home| cwd.strip_prefix(home)).map_or_else(
        || clean_terminal_text(cwd),
        |suffix| if suffix.is_empty() { "~".to_owned() } else { format!("~{suffix}") },
    )
}

fn wrap_display_line(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = vec![String::new()];
    let mut columns = 0usize;
    for character in text.chars() {
        let character_width = character.width().unwrap_or(0);
        if columns > 0 && columns.saturating_add(character_width) > width {
            rows.push(String::new());
            columns = 0;
        }
        rows.last_mut().expect("one wrap row").push(character);
        columns = columns.saturating_add(character_width);
    }
    rows
}
fn wrapped_row_count(text: &str, width: usize) -> usize {
    let width = width.max(1);
    let mut rows = 1usize;
    let mut columns = 0usize;
    for character in text.chars() {
        let character_width = character.width().unwrap_or(0);
        if columns > 0 && columns.saturating_add(character_width) > width {
            rows += 1;
            columns = 0;
        }
        columns = columns.saturating_add(character_width);
    }
    rows
}

fn editor_wrapped_position(state: &TuiState, width: usize) -> (usize, u16) {
    let width = width.max(1);
    let prior_rows = state.editor.lines[..state.editor.row]
        .iter()
        .map(|line| wrapped_row_count(&clean_terminal_text(line), width))
        .sum::<usize>();
    let prefix = clean_terminal_text(&state.editor.lines[state.editor.row][..state.editor.column]);
    let mut row = 0usize;
    let mut columns = 0usize;
    for character in prefix.chars() {
        let character_width = character.width().unwrap_or(0);
        if columns > 0 && columns.saturating_add(character_width) > width {
            row += 1;
            columns = 0;
        }
        columns = columns.saturating_add(character_width);
    }
    (prior_rows.saturating_add(row), u16::try_from(columns).unwrap_or(u16::MAX))
}

fn visible_editor_lines(state: &TuiState, width: usize, max_rows: usize) -> (Vec<String>, usize) {
    let width = width.max(1);
    let max_rows = max_rows.max(1);
    let (cursor_row, _) = editor_wrapped_position(state, width);
    let start = cursor_row.saturating_sub(max_rows.saturating_sub(1));
    let end = start.saturating_add(max_rows);
    let mut visible = Vec::with_capacity(max_rows);
    let mut absolute_row = 0usize;
    for line in &state.editor.lines {
        for wrapped in wrap_display_line(&clean_terminal_text(line), width) {
            if absolute_row >= start && absolute_row < end {
                visible.push(wrapped);
            }
            absolute_row += 1;
            if absolute_row >= end {
                break;
            }
        }
        if absolute_row >= end {
            break;
        }
    }
    (visible, cursor_row.saturating_sub(start))
}

fn composer_border_lines_bounded(state: &TuiState, width: u16, theme: Theme, max_rows: usize) -> Vec<Line<'static>> {
    let inner = usize::from(width.saturating_sub(2));
    let attachment_rows = pending_attachment_labels(&state.pending_attachments).len();
    // Keep attachment placeholders visible inside the bounded card: editor
    // rows share the interior budget after chrome (2) and attachment rows.
    let content_rows = max_rows
        .saturating_sub(2)
        .saturating_sub(attachment_rows)
        .max(1);
    let (editor_lines, _) = visible_editor_lines(state, inner.saturating_sub(3), content_rows);
    composer_border_lines_with_editor(state, width, theme, editor_lines)
}


fn composer_border_lines(state: &TuiState, width: u16, theme: Theme) -> Vec<Line<'static>> {
    let inner = usize::from(width.saturating_sub(2));
    let total_rows = state
        .editor
        .lines
        .iter()
        .map(|line| wrapped_row_count(&clean_terminal_text(line), inner.saturating_sub(3)))
        .sum::<usize>();
    let (editor_lines, _) = visible_editor_lines(state, inner.saturating_sub(3), total_rows.max(1));
    composer_border_lines_with_editor(state, width, theme, editor_lines)
}

/// Compact thinking label for the OMP composer chrome (`med`, `off`, …).
fn composer_thinking_label(state: &TuiState) -> String {
    let effective = state.effective_thinking_state();
    let short = match effective.level {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Minimal => "min",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "med",
        ThinkingLevel::High => "high",
        ThinkingLevel::Xhigh => "xhi",
        ThinkingLevel::Max => "max",
    };
    if effective.show_thinking {
        short.to_owned()
    } else if let Some(label) = state
        .extension_hidden_thinking_label
        .as_deref()
        .filter(|label| !label.trim().is_empty())
    {
        label.to_owned()
    } else {
        format!("{short} hid")
    }
}


/// Compose the inline status segment for the OMP-style composer header.
/// Busy states keep the activity animation; idle states surface `state.status`.
fn composer_status_display(state: &TuiState, theme: Theme) -> (String, Color) {
    if state.is_compacting {
        return (
            format!(
                "compacting {} ▶──",
                ACTIVE_ANIMATION_FRAMES[state.animation_frame % ACTIVE_ANIMATION_FRAMES.len()]
            ),
            theme.muted,
        );
    }
    if state.is_streaming {
        return (
            format!(
                "working {} ▶──",
                ACTIVE_ANIMATION_FRAMES[state.animation_frame % ACTIVE_ANIMATION_FRAMES.len()]
            ),
            theme.muted,
        );
    }
    if let Some(goal) = goal_status_summary(&state.goal_state) {
        return (format!("{goal} ▶──"), theme.accent);
    }
    let text = state.status.trim();
    if text.is_empty() {
        ("▶──".to_owned(), theme.muted)
    } else {
        (format!("{text} ▶──"), theme.dim)
    }
}

fn truncate_status_text(text: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    if usize::from(display_width(text)) <= max_cols {
        return text.to_owned();
    }
    if max_cols <= 1 {
        return "…".to_owned();
    }
    let mut out = String::new();
    let mut cols = 0usize;
    let limit = max_cols.saturating_sub(1);
    for ch in text.chars() {
        let ch_cols = usize::from(display_width(&ch.to_string()));
        if cols.saturating_add(ch_cols) > limit {
            break;
        }
        out.push(ch);
        cols = cols.saturating_add(ch_cols);
    }
    out.push('…');
    out
}

const MAX_COMPOSER_ERROR_HEIGHT: usize = 5;
const COMPOSER_ERROR_DISMISSAL_HINT: &str = "Dismissed when you send your next message.";

fn composer_error_toast_height(state: &TuiState, width: u16) -> u16 {
    let Some(error) = state.composer_error.as_deref() else {
        return 0;
    };
    if width < 5 {
        return 1;
    }
    let content_width = usize::from(width.saturating_sub(4)).max(1);
    let message_rows = clean_terminal_text(error)
        .lines()
        .map(|line| wrapped_row_count(line, content_width))
        .sum::<usize>()
        .clamp(1, 2);
    u16::try_from(message_rows.saturating_add(3)).unwrap_or(MAX_COMPOSER_ERROR_HEIGHT as u16)
}

/// Render a red, width- and height-bounded error toast for the live composer.
/// These rows are ephemeral and must never enter transcript commit paths.
fn composer_error_toast_lines(
    state: &TuiState,
    width: u16,
    theme: Theme,
) -> Vec<Line<'static>> {
    let Some(error) = state.composer_error.as_deref() else {
        return Vec::new();
    };
    let width = usize::from(width);
    let border_style = Style::default().fg(theme.error).bg(theme.tool_error_bg);
    if width < 5 {
        return vec![Line::from(Span::styled(
            truncate_status_text("!", width),
            border_style.add_modifier(Modifier::BOLD),
        ))];
    }

    let inner = width.saturating_sub(2);
    let content_width = width.saturating_sub(4).max(1);
    let clean_error = clean_terminal_text(error);
    let raw_message_rows = clean_error
        .lines()
        .map(|line| wrapped_row_count(line, content_width))
        .sum::<usize>()
        .max(1);
    let mut message_rows = clean_error
        .lines()
        .flat_map(|line| wrap_display_line(line, content_width))
        .take(2)
        .collect::<Vec<_>>();
    if message_rows.is_empty() {
        message_rows.push("Unknown error".to_owned());
    }
    if raw_message_rows > message_rows.len()
        && let Some(last) = message_rows.last_mut()
    {
        *last = truncate_status_text(&format!("{last}…"), content_width);
    }

    let error_style = Style::default().fg(theme.error).bg(theme.tool_error_bg);
    let hint_style = Style::default().fg(theme.dim).bg(theme.tool_error_bg);
    let title = truncate_status_text(" Error ", inner);
    let top_fill = "─".repeat(inner.saturating_sub(usize::from(display_width(&title))));
    let mut lines = vec![Line::from(vec![
        Span::styled("╭", border_style),
        Span::styled(title, border_style.add_modifier(Modifier::BOLD)),
        Span::styled(top_fill, border_style),
        Span::styled("╮", border_style),
    ])];
    for row in message_rows {
        let row = truncate_status_text(&row, content_width);
        let fill = " ".repeat(content_width.saturating_sub(usize::from(display_width(&row))));
        lines.push(Line::from(vec![
            Span::styled("│ ", border_style),
            Span::styled(row, error_style),
            Span::styled(fill, error_style),
            Span::styled(" │", border_style),
        ]));
    }
    let hint = truncate_status_text(COMPOSER_ERROR_DISMISSAL_HINT, content_width);
    let hint_fill = " ".repeat(content_width.saturating_sub(usize::from(display_width(&hint))));
    lines.push(Line::from(vec![
        Span::styled("│ ", border_style),
        Span::styled(hint, hint_style),
        Span::styled(hint_fill, hint_style),
        Span::styled(" │", border_style),
    ]));
    lines.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(inner)),
        border_style,
    )));
    lines
}

fn composer_border_lines_with_editor(state: &TuiState, width: u16, theme: Theme, editor_lines: Vec<String>) -> Vec<Line<'static>> {
    let inner = usize::from(width.saturating_sub(2));
    let model = clean_terminal_text(&state.model);
    let cwd = compact_cwd(&state.cwd);
    let thinking = composer_thinking_label(state);
    // Inline status: activity animation while busy; otherwise the durable
    // `state.status` toast written by slash/actions/events (previously unrendered).
    // Status is truncated inside the normal chrome budget so a long toast never
    // forces the 90-column OMP header into the narrow fallback.
    let (raw_status, status_color) = composer_status_display(state, theme);
    let chrome_prefix = usize::from(display_width("── π  > ⬢ "))
        + usize::from(display_width(&model))
        + usize::from(display_width(&format!(" · ◑ {thinking} > 📁 ")))
        + usize::from(display_width(&cwd))
        + usize::from(display_width(" > ⟲ "));
    let mut lines = if chrome_prefix < inner {
        let status_budget = inner.saturating_sub(chrome_prefix);
        let status_text = truncate_status_text(&raw_status, status_budget);
        let top_fill = "─".repeat(status_budget.saturating_sub(usize::from(display_width(&status_text))));
        vec![Line::from(vec![
            Span::styled("╭── ", Style::default().fg(theme.muted).bg(theme.user_message_bg)),
            Span::styled("π", Style::default().fg(theme.accent).bg(theme.user_message_bg).add_modifier(Modifier::BOLD)),
            Span::styled("  > ⬢ ", Style::default().fg(theme.border).bg(theme.user_message_bg)),
            Span::styled(model, Style::default().fg(theme.accent).bg(theme.user_message_bg)),
            Span::styled(format!(" · ◑ {thinking} > 📁 "), Style::default().fg(theme.accent).bg(theme.user_message_bg)),
            Span::styled(cwd, Style::default().fg(theme.syntax_variable).bg(theme.user_message_bg)),
            Span::styled(" > ⟲ ", Style::default().fg(theme.border).bg(theme.user_message_bg)),
            Span::styled(status_text, Style::default().fg(status_color).bg(theme.user_message_bg)),
            Span::styled(top_fill, Style::default().fg(theme.muted).bg(theme.user_message_bg)),
            Span::styled("╮", Style::default().fg(theme.muted).bg(theme.user_message_bg)),
        ])]
    } else {
        // Narrow terminals: keep chrome + a truncated status so toasts stay visible.
        // Inner content is `── π  > ⟲ ` + status + fill; ╭/╮ sit outside `inner`.
        let fixed = usize::from(display_width("── π  > ⟲ "));
        let budget = inner.saturating_sub(fixed);
        let truncated = if budget == 0 {
            String::new()
        } else {
            truncate_status_text(&raw_status, budget)
        };
        let fill = "─".repeat(inner.saturating_sub(fixed.saturating_add(usize::from(display_width(&truncated)))));
        vec![Line::from(vec![
            Span::styled("╭── ", Style::default().fg(theme.muted).bg(theme.user_message_bg)),
            Span::styled("π", Style::default().fg(theme.accent).bg(theme.user_message_bg).add_modifier(Modifier::BOLD)),
            Span::styled("  > ⟲ ", Style::default().fg(theme.border).bg(theme.user_message_bg)),
            Span::styled(truncated, Style::default().fg(status_color).bg(theme.user_message_bg)),
            Span::styled(fill, Style::default().fg(theme.muted).bg(theme.user_message_bg)),
            Span::styled("╮", Style::default().fg(theme.muted).bg(theme.user_message_bg)),
        ])]
    };
    let editor_lines = editor_lines;
    let attachment_labels = pending_attachment_labels(&state.pending_attachments);
    if editor_lines.len() <= 1 && attachment_labels.is_empty() && state.completions.items.is_empty() {
        let input = editor_lines.first().cloned().unwrap_or_default();
        let input_width = usize::from(display_width(&input));
        let fill = "─".repeat(inner.saturating_sub(input_width.saturating_add(3)));
        lines.push(Line::from(vec![
            Span::styled("╰─ ", Style::default().fg(theme.border_muted)),
            Span::styled(input, Style::default().fg(theme.text)),
            Span::styled(format!(" {fill}╯"), Style::default().fg(theme.border_muted)),
        ]));
        return lines;
    }
    if !state.completions.items.is_empty() && editor_lines.len() <= 1 && attachment_labels.is_empty() {
        let input = editor_lines.first().cloned().unwrap_or_default();
        let input_width = usize::from(display_width(&input));
        let fill = " ".repeat(inner.saturating_sub(input_width.saturating_add(2)));
        lines.push(Line::from(vec![
            Span::styled("│  ", Style::default().fg(theme.muted).bg(theme.user_message_bg)),
            Span::styled(input, Style::default().fg(theme.text).bg(theme.user_message_bg)),
            Span::styled(fill, Style::default().bg(theme.user_message_bg)),
            Span::styled("│", Style::default().fg(theme.muted).bg(theme.user_message_bg)),
        ]));
        lines.push(Line::from(Span::styled(format!("╰{}╯", "─".repeat(inner)), Style::default().fg(theme.muted).bg(theme.user_message_bg))));
        return lines;
    }
    for line in editor_lines {
        let line_width = usize::from(display_width(&line));
        let fill = " ".repeat(inner.saturating_sub(line_width.saturating_add(1)));
        lines.push(Line::from(vec![
            Span::styled("│ ", Style::default().fg(theme.border_muted)),
            Span::styled(line, Style::default().fg(theme.text)),
            Span::raw(fill),
            Span::styled("│", Style::default().fg(theme.border_muted)),
        ]));
    }
    for label in attachment_labels {
        let line_width = usize::from(display_width(&label));
        let fill = " ".repeat(inner.saturating_sub(line_width.saturating_add(1)));
        lines.push(Line::from(vec![
            Span::styled("│ ", Style::default().fg(theme.border_muted)),
            Span::styled(label, Style::default().fg(theme.muted)),
            Span::raw(fill),
            Span::styled("│", Style::default().fg(theme.border_muted)),
        ]));
    }
    lines.push(Line::from(Span::styled(format!("╰{}╯", "─".repeat(inner)), Style::default().fg(theme.border_muted))));
    lines
}

fn render_welcome_lines(state: &TuiState, theme: Theme) -> Vec<Line<'static>> {
    if !state.transcript.is_empty() || !state.streaming_text.is_empty() || !state.streaming_thinking.is_empty() { return Vec::new(); }
    let recent = pi_coding::list_sessions(&state.cwd_path).into_iter().take(3).collect::<Vec<_>>();
    let mut lines = vec![Line::default(), Line::from(Span::styled("  rpi", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD))), Line::from(Span::styled(format!("  {}", clean_terminal_text(&state.model)), Style::default().fg(theme.muted))), Line::default(), Line::from(Span::styled("  Start typing · Alt+V paste image · /help · @file to attach context", Style::default().fg(theme.text)))];
    if !recent.is_empty() { lines.push(Line::default()); lines.push(Line::from(Span::styled("  Recent sessions", Style::default().fg(theme.muted).add_modifier(Modifier::BOLD)))); for session in recent { let label = session.name.as_deref().unwrap_or(&session.id); lines.push(Line::from(Span::styled(format!("  • {}", clean_terminal_text(label)), Style::default().fg(theme.text)))); } }
    lines
}

fn render(
    frame: &mut ratatui::Frame<'_>,
    state: &TuiState,
    images: &mut TerminalImageRenderer,
) -> ImageDrawPlan {
    let theme = state.themes.theme();
    let extension = state.extension_ui.snapshot();
    let above = extension_widget_lines(&extension, UiWidgetPlacement::AboveEditor, theme);
    let below = extension_widget_lines(&extension, UiWidgetPlacement::BelowEditor, theme);
    let error_lines = composer_error_toast_lines(state, frame.area().width, theme);
    let above_height = u16::try_from(above.len()).unwrap_or(u16::MAX).min(6);
    let below_height = u16::try_from(below.len()).unwrap_or(u16::MAX).min(6);
    let completion_height = u16::try_from(state.completions.items.len())
        .unwrap_or(u16::MAX)
        .min(u16::try_from(MAX_COMPLETIONS).unwrap_or(u16::MAX));
    let mut todo_lines = if state.workflow_snapshots.is_empty() {
        render_todo_panel_lines(&state.todo_phases, &state.job_cards.cards_in_source_order(), theme, frame.area().width.max(1))
    } else {
        vec![Line::from(Span::styled(compact_workflow_status(&state.workflow_snapshots), Style::default().fg(theme.muted).add_modifier(Modifier::BOLD)))]
    };
    let layout = tui_layout_heights(
        state,
        frame.area().width,
        frame.area().height,
        u16::try_from(todo_lines.len()).unwrap_or(u16::MAX),
        above_height,
        completion_height,
        below_height,
    );
    let todo_needs_trailing_gap = todo_lines.last().is_some_and(|line| line.spans.is_empty());
    todo_lines.truncate(usize::from(layout.todo));
    if todo_needs_trailing_gap
        && todo_lines.len() == usize::from(layout.todo)
        && todo_lines.last().is_some_and(|line| !line.spans.is_empty())
    {
        if let Some(last) = todo_lines.last_mut() {
            *last = Line::default();
        }
    }
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(layout.transcript),
            Constraint::Length(layout.todo),
            Constraint::Length(layout.above),
            Constraint::Length(layout.error),
            Constraint::Length(layout.composer),
            Constraint::Length(layout.completions),
            Constraint::Length(layout.below),
        ])
        .split(frame.area());

    let cell_size = window_size()
        .ok()
        .and_then(|size| {
            (size.columns > 0 && size.rows > 0 && size.width > 0 && size.height > 0)
                .then_some(TerminalCellSize {
                    width_pixels: size.width / size.columns,
                    height_pixels: size.height / size.rows,
                })
        })
        .unwrap_or_default();
    let mut image_candidates = Vec::new();
    let mut image_context = TranscriptImageContext {
        renderer: images,
        candidates: &mut image_candidates,
        config: ImageDisplayConfig {
            show_images: state.show_images,
            width_cells: state.image_width_cells,
        },
        viewport_columns: sections[0].width,
        viewport_rows: sections[0].height,
        cell_size,
    };
    let mut transcript =
        render_transcript_lines(state, theme, sections[0].width.max(1), &mut image_context);
    if !state.streaming_thinking.is_empty() || !state.streaming_text.is_empty() {
        let mut content = Vec::new();
        if !state.streaming_thinking.is_empty() {
            content.push(ContentBlock::thinking(state.streaming_thinking.clone()));
        }
        if !state.streaming_text.is_empty() {
            content.push(ContentBlock::text(state.streaming_text.clone()));
        }
        render_transcript_entry_inner(
            &mut transcript,
            &TranscriptEntry {
                kind: TranscriptKind::Assistant,
                content,
                tool_name: None,
                tool_card: None,
                job_card: None,
                is_error: false,
                is_partial: true,
            },
            state.show_thinking,
            state.expand_tools,
            theme,
            sections[0].width.max(1),
            state.animation_frame,
            None,
        );
    }
    if transcript.is_empty() {
        transcript = render_welcome_lines(state, theme);
    }
    let transcript_height = usize::from(sections[0].height);
    state.transcript_page_rows.set(transcript_height.max(1));
    let transcript_width = sections[0].width.max(1);
    let total_rows = wrapped_line_count(&transcript, transcript_width);
    let bottom = total_rows.saturating_sub(transcript_height);
    let scroll = bottom.saturating_sub(state.transcript_scroll.min(bottom));
    let paragraph = Paragraph::new(Text::from(transcript.clone()))
        .style(Style::default().fg(theme.text))
        .wrap(Wrap { trim: false });
    let mut message_hasher = DefaultHasher::new();
    transcript.hash(&mut message_hasher);
    state.transcript_scroll.hash(&mut message_hasher);
    let message_hash = message_hasher.finish();
    let mut theme_hasher = DefaultHasher::new();
    state.themes.active_name().hash(&mut theme_hasher);
    format!("{theme:?}").hash(&mut theme_hasher);
    let theme_hash = theme_hasher.finish();
    let overlays_open = page_overlay_open(state);
    let placements = if overlays_open {
        Vec::new()
    } else {
        image_candidates
            .into_iter()
            .filter_map(|candidate| {
                let row = wrapped_line_count(&transcript[..candidate.line_index], transcript_width);
                let visible_row = row.checked_sub(scroll)?;
                (visible_row.saturating_add(usize::from(candidate.layout.rows())) <= transcript_height)
                    .then(|| ImagePlacement::new(
                        candidate.layout,
                        candidate.data,
                        candidate.mime_type,
                        sections[0].x,
                        sections[0].y.saturating_add(u16::try_from(visible_row).unwrap_or(u16::MAX)),
                    ))
            })
            .collect()
    };
    frame.render_widget(
        paragraph.scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)),
        sections[0],
    );
    if layout.todo > 0 {
        frame.render_widget(
            Paragraph::new(Text::from(todo_lines))
                .style(Style::default().fg(theme.text))
                .wrap(Wrap { trim: false }),
            sections[1],
        );
    }
    if layout.above > 0 {
        frame.render_widget(Paragraph::new(above), sections[2]);
    }
    if layout.error > 0 {
        frame.render_widget(Paragraph::new(error_lines), sections[3]);
    }
    let composer_lines = composer_border_lines_bounded(state, sections[4].width, theme, usize::from(sections[4].height));
    frame.render_widget(Paragraph::new(composer_lines), sections[4]);
    if !state.completions.items.is_empty() {
        let (window_start, visible) = state.completions.visible_window(MAX_COMPLETIONS);
        let lines = visible
            .iter()
            .enumerate()
            .map(|(offset, item)| {
                let index = window_start + offset;
                let selected = index == state.completions.selected;
                Line::from(vec![
                    Span::styled(
                        if selected { "❯ " } else { "  " },
                        Style::default().fg(if selected { theme.accent } else { theme.dim }),
                    ),
                    Span::styled(
                        clean_terminal_text(&item.label),
                        Style::default()
                            .fg(if selected { theme.text } else { theme.muted })
                            .add_modifier(if selected {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                    Span::styled(
                        format!("  {}", clean_terminal_text(&item.description)),
                        Style::default().fg(theme.muted),
                    ),
                ])
            })
            .collect::<Vec<_>>();
        let completion_area = Rect {
            x: sections[5].x.saturating_add(2),
            y: sections[5].y,
            width: sections[5].width.saturating_sub(3),
            height: sections[5].height,
        };
        frame.render_widget(Paragraph::new(lines), completion_area);
    }
    let editor_width = usize::from(sections[4].width.saturating_sub(5));
    let (_, cursor_column) = editor_wrapped_position(state, editor_width);
    let (_, visible_cursor_row) = visible_editor_lines(state, editor_width, usize::from(sections[4].height.saturating_sub(2)).max(1));
    let cursor_x = sections[4]
        .x
        .saturating_add(if state.completions.items.is_empty() && state.editor.lines.len() <= 1 { 3 } else { 2 })
        .saturating_add(cursor_column);
    let cursor_y = sections[4].y.saturating_add(1).saturating_add(u16::try_from(visible_cursor_row).unwrap_or(u16::MAX));
    if state.extension_dialog.is_none()
        && state.process_panel.is_none()
        && state.settings_panel.is_none()
        && state.workflow_panel.is_none()
        && state.agents_panel.is_none()
        && cursor_x < sections[4].right().saturating_sub(1)
        && cursor_y < sections[4].bottom()
    {
        frame.set_cursor_position((cursor_x, cursor_y));
    }
    if layout.below > 0 {
        frame.render_widget(Paragraph::new(below), sections[6]);
    }
    if let Some(panel) = &state.settings_panel { render_settings_panel(frame, panel, state.settings_value_input.as_ref(), theme); }
    if let Some(panel) = &state.panel { render_selector_panel(frame, panel, theme); }
    if let Some(panel) = &state.tree_panel { render_tree_panel(frame, panel, theme); }
    if let Some(panel) = &state.process_panel { render_process_panel(frame, panel, theme); }
    if let Some(panel) = &state.workflow_panel { render_workflow_panel(frame, panel, theme); }
    if let Some(panel) = &state.agents_panel { render_agents_panel(frame, panel, theme); }
    if let Some(selector) = &state.session_selector { render_saved_session_selector(frame, selector, theme); }
    if let Some(selector) = &state.scoped_model_selector { render_scoped_model_selector(frame, selector, theme); }
    if let Some(dialog) = &state.extension_dialog { render_extension_dialog(frame, dialog, theme); }
    ImageDrawPlan {
        identity: ImageFrameIdentity {
            viewport_width: sections[0].width,
            viewport_height: sections[0].height,
            theme_hash,
            message_hash,
        },
        placements,
    }
}
fn render_settings_panel(frame: &mut ratatui::Frame<'_>, panel: &SettingsPanel, input: Option<&(String, String)>, theme: Theme) {
    let Ok(snapshot) = panel.snapshot() else { return; };
    let area = centered_rect(frame.area().width.saturating_sub(4).min(140).max(40), frame.area().height.saturating_sub(4).max(12), frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default().title(format!(" Settings · {:?} scope{} ", snapshot.scope, if snapshot.dirty { " · modified" } else { "" })).borders(Borders::ALL).border_style(Style::default().fg(theme.border_accent));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let category = snapshot.category.map_or_else(|| "All".to_owned(), |value| format!("{value:?}"));
    let mut lines = vec![Line::from(vec![Span::styled("Category ", Style::default().fg(theme.dim)), Span::styled(category, Style::default().fg(theme.accent)), Span::styled("  ←/→ · Scope Ctrl-G/Ctrl-P · ", Style::default().fg(theme.dim)), Span::styled(if snapshot.project_trusted { "trusted" } else { "untrusted" }, Style::default().fg(if snapshot.project_trusted { theme.success } else { theme.warning }))]), Line::from(vec![Span::styled("Search ", Style::default().fg(theme.dim)), Span::styled(if snapshot.search.is_empty() { "type to filter" } else { &snapshot.search }, Style::default().fg(theme.text))]), Line::from(Span::styled("↑/↓ select · Enter edit/toggle · Del reset · Ctrl-S apply · Esc cancel", Style::default().fg(theme.muted))), Line::from("")];
    let visible_rows = usize::from(inner.height).saturating_sub(4);
    let start = snapshot.cursor.saturating_sub(visible_rows.saturating_sub(1));
    for (index, row) in snapshot.rows.iter().enumerate().skip(start).take(visible_rows) {
        let selected = index == snapshot.cursor;
        let value = if row.redacted { "[redacted]".to_owned() } else { row.effective_value.to_string() };
        let metadata = format!("{:?} · {:?}{}", row.source, row.behavior, if row.inherited { " · inherited" } else { "" });
        let blocked = row.blocked_reason.as_deref().map_or(String::new(), |reason| format!(" · {reason}"));
        lines.push(Line::from(vec![Span::styled(if selected { "› " } else { "  " }, Style::default().fg(theme.accent)), Span::styled(clean_terminal_text(&row.key), Style::default().fg(if selected { theme.accent } else { theme.text }).add_modifier(if selected { Modifier::BOLD } else { Modifier::empty() })), Span::styled(format!(" = {value}  [{metadata}]{blocked}"), Style::default().fg(if row.blocked_reason.is_some() { theme.warning } else { theme.muted }))]));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    if let Some((key, value)) = input {
        let y = inner.bottom().saturating_sub(1);
        let prefix = format!("Edit {key}: ");
        frame.render_widget(Paragraph::new(Line::from(vec![Span::styled(prefix.clone(), Style::default().fg(theme.accent)), Span::styled(value.clone(), Style::default().fg(theme.text))])), Rect { x: inner.x, y, width: inner.width, height: 1 });
        let x = inner.x.saturating_add(display_width(&format!("{prefix}{value}")));
        if x < inner.right() { frame.set_cursor_position((x, y)); }
    }
}


fn render_extension_dialog(frame: &mut ratatui::Frame<'_>, dialog: &ExtensionDialog, theme: Theme) {
    let mut lines = Vec::new();
    let mut cursor = None;
    match &dialog.kind {
        ExtensionDialogKind::Select { options, selected } => {
            for (index, option) in options.iter().enumerate() {
                let style = if index == *selected {
                    Style::default().fg(theme.text).bg(theme.selected_bg)
                } else {
                    Style::default().fg(theme.text)
                };
                lines.push(Line::from(Span::styled(
                    format!(" {}", clean_terminal_text(&option.label)),
                    style,
                )));
                if let Some(description) = &option.description {
                    lines.push(Line::from(Span::styled(
                        format!("   {}", clean_terminal_text(description)),
                        Style::default().fg(theme.dim),
                    )));
                }
            }
            lines.push(Line::from(Span::styled(
                "↑/↓ select · Enter accept · Esc cancel",
                Style::default().fg(theme.dim),
            )));
        }
        ExtensionDialogKind::Confirm { message, confirmed } => {
            lines.extend(clean_terminal_text(message).lines().map(|line| {
                Line::from(Span::styled(
                    line.to_owned(),
                    Style::default().fg(theme.text),
                ))
            }));
            let yes = if *confirmed {
                Style::default().fg(theme.text).bg(theme.selected_bg)
            } else {
                Style::default().fg(theme.dim)
            };
            let no = if *confirmed {
                Style::default().fg(theme.dim)
            } else {
                Style::default().fg(theme.text).bg(theme.selected_bg)
            };
            lines.push(Line::from(vec![
                Span::styled(" Yes ", yes),
                Span::raw("  "),
                Span::styled(" No ", no),
            ]));
            lines.push(Line::from(Span::styled(
                "←/→ choose · Enter accept · Esc cancel",
                Style::default().fg(theme.dim),
            )));
        }
        ExtensionDialogKind::Input {
            placeholder,
            editor,
        } => {
            if editor.is_empty() {
                if let Some(placeholder) = placeholder {
                    lines.push(Line::from(Span::styled(
                        clean_terminal_text(placeholder),
                        Style::default().fg(theme.dim),
                    )));
                }
            }
            if !editor.is_empty() {
                lines.extend(editor.lines.iter().map(|line| {
                    Line::from(Span::styled(
                        clean_terminal_text(line),
                        Style::default().fg(theme.text),
                    ))
                }));
            }
            lines.push(Line::from(Span::styled(
                "Enter accept · Esc cancel",
                Style::default().fg(theme.dim),
            )));
            cursor = Some((editor.row, editor.column, editor.lines[editor.row].clone()));
        }
        ExtensionDialogKind::Editor { editor } => {
            lines.extend(editor.lines.iter().map(|line| {
                Line::from(Span::styled(
                    clean_terminal_text(line),
                    Style::default().fg(theme.text),
                ))
            }));
            lines.push(Line::from(Span::styled(
                "Shift+Enter/Ctrl+J newline · Enter accept · Esc cancel",
                Style::default().fg(theme.dim),
            )));
            cursor = Some((editor.row, editor.column, editor.lines[editor.row].clone()));
        }
    }
    let height = u16::try_from(lines.len().saturating_add(2))
        .unwrap_or(u16::MAX)
        .clamp(4, frame.area().height.saturating_sub(2).max(4));
    let area = centered_rect(
        frame.area().width.saturating_sub(4).min(76).max(20),
        height,
        frame.area(),
    );
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.border_accent))
                    .title(format!(" {} ", clean_terminal_text(&dialog.title))),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
    if let Some((row, column, line)) = cursor {
        let x = area
            .x
            .saturating_add(1)
            .saturating_add(display_width(&line[..column]));
        let y = area
            .y
            .saturating_add(1)
            .saturating_add(u16::try_from(row).unwrap_or(u16::MAX));
        if x < area.right().saturating_sub(1) && y < area.bottom().saturating_sub(1) {
            frame.set_cursor_position((x, y));
        }
    }
}

fn trim_inter_entry_blank_before_user(lines: &mut Vec<Line<'static>>, entry: &TranscriptEntry) {
    if entry.kind == TranscriptKind::User
        && lines.last().is_some_and(|line| {
            line.spans
                .iter()
                .all(|span| span.content.trim().is_empty())
        })
    {
        lines.pop();
    }
}

fn render_transcript_lines(
    state: &TuiState,
    theme: Theme,
    width: u16,
    image_context: &mut TranscriptImageContext<'_>,
) -> Vec<Line<'static>> {
    let start = if state.transcript_scroll > 0 {
        0
    } else {
        state.committed_entries.min(state.transcript.len())
    };
    let mut lines = Vec::new();
    for entry in &state.transcript[start..] {
        trim_inter_entry_blank_before_user(&mut lines, entry);
        render_transcript_entry_inner(
            &mut lines,
            entry,
            state.show_thinking,
            state.expand_tools,
            theme,
            width,
            state.animation_frame,
            Some(image_context),
        );
    }
    lines
}

fn render_job_card(lines: &mut Vec<Line<'static>>, card: &TaskCardRows, theme: Theme, animation_frame: usize, width: u16) {
    let inner = usize::from(width.saturating_sub(2)).max(1);
    let border = if card.children.iter().any(|child| child.job_status == pi_coding::JobStatus::Failed) { theme.error }
        else if card.children.iter().any(|child| child.job_status == pi_coding::JobStatus::Cancelled) { theme.warning }
        else { theme.border_accent };
    lines.push(Line::from(Span::styled(format!("╭{}╮", "─".repeat(inner)), Style::default().fg(border))));
    push_task_box_row(lines, &format!("Task {} agents", card.children.len()), theme.tool_title, border, inner, Modifier::BOLD);
    if !card.context.trim().is_empty() {
        for line in render_transcript_markdown(&card.context, theme, theme.text, u16::try_from(inner.saturating_sub(2)).unwrap_or(u16::MAX), false) {
            push_task_box_line(lines, line, border, inner);
        }
        push_task_separator(lines, " Agents ", border, inner);
    }
    for child in &card.children {
        let marker = match child.job_status {
            pi_coding::JobStatus::Queued | pi_coding::JobStatus::Running => {
                ACTIVE_ANIMATION_FRAMES[animation_frame % ACTIVE_ANIMATION_FRAMES.len()]
            }
            pi_coding::JobStatus::Completed => "✓",
            pi_coding::JobStatus::Failed => "✗",
            pi_coding::JobStatus::Cancelled => "–",
        };
        let status_color = job_status_color(child.job_status, theme);
        let lifecycle = match child.job_status {
            pi_coding::JobStatus::Queued => "queued",
            pi_coding::JobStatus::Running => "running",
            pi_coding::JobStatus::Completed => "completed",
            pi_coding::JobStatus::Failed => "failed",
            pi_coding::JobStatus::Cancelled => "cancelled",
        };
        let parked = if child.agent_status == Some(pi_coding::AgentStatus::Parked) {
            " · parked"
        } else {
            ""
        };
        let title = format!("{marker} {} ({}) · {lifecycle}{parked}", child.display_name, child.agent);
        push_task_box_row(lines, &title, status_color, border, inner, Modifier::BOLD);
        if let Some(summary) = child.summary.as_deref().or_else(|| child.rows.iter().find(|row| row.role == JobCardRowRole::Description).map(|row| row.text.as_str())) {
            push_task_box_row(lines, summary, theme.text, border, inner, Modifier::empty());
        }
        let activity = child.rows.iter().filter(|row| !matches!(row.role, JobCardRowRole::Title | JobCardRowRole::Description | JobCardRowRole::Reference)).map(|row| row.text.as_str()).collect::<Vec<_>>().join(" · ");
        if !activity.is_empty() {
            push_task_box_row(lines, &activity, status_color, border, inner, Modifier::empty());
        }
    }
    push_task_box_row(lines, &card.aggregate.text, theme.muted, border, inner, Modifier::ITALIC);
    lines.push(Line::from(Span::styled(format!("╰{}╯", "─".repeat(inner)), Style::default().fg(border))));
}

fn push_task_separator(lines: &mut Vec<Line<'static>>, label: &str, border: Color, inner: usize) {
    let fill = "─".repeat(inner.saturating_sub(display_width(label).into()));
    lines.push(Line::from(vec![Span::styled("├", Style::default().fg(border)), Span::styled(label.to_owned(), Style::default().fg(border)), Span::styled(fill, Style::default().fg(border)), Span::styled("┤", Style::default().fg(border))]));
}

fn push_task_box_row(lines: &mut Vec<Line<'static>>, text: &str, color: Color, border: Color, inner: usize, modifier: Modifier) {
    for row in wrap_display_line(&clean_terminal_text(text), inner.saturating_sub(2).max(1)) {
        push_task_box_line(lines, Line::from(Span::styled(row, Style::default().fg(color).add_modifier(modifier))), border, inner);
    }
}

fn push_task_box_line(lines: &mut Vec<Line<'static>>, line: Line<'static>, border: Color, inner: usize) {
    let used = line.width();
    let fill = " ".repeat(inner.saturating_sub(used.saturating_add(2)));
    let mut spans = vec![Span::styled("│ ", Style::default().fg(border))];
    spans.extend(line.spans);
    spans.push(Span::raw(fill));
    spans.push(Span::styled(" │", Style::default().fg(border)));
    lines.push(Line::from(spans));
}

fn job_title_prefix(status: pi_coding::JobStatus, animation_frame: usize) -> &'static str {
    match status {
        pi_coding::JobStatus::Queued | pi_coding::JobStatus::Running => {
            ACTIVE_JOB_PREFIXES[animation_frame % ACTIVE_JOB_PREFIXES.len()]
        }
        pi_coding::JobStatus::Completed
        | pi_coding::JobStatus::Failed
        | pi_coding::JobStatus::Cancelled => "Task ",
    }
}

fn job_status_color(status: pi_coding::JobStatus, theme: Theme) -> Color {
    match status {
        pi_coding::JobStatus::Queued => theme.dim,
        pi_coding::JobStatus::Running => theme.accent,
        pi_coding::JobStatus::Completed => theme.success,
        pi_coding::JobStatus::Failed => theme.error,
        pi_coding::JobStatus::Cancelled => theme.warning,
    }
}

const TODO_HUD_TASK_LIMIT: usize = 5;
const TODO_HUD_JOB_LIMIT: usize = 8;

fn selected_todo_hud_tasks<'a>(tasks: &[&'a TodoItem]) -> (Vec<&'a TodoItem>, usize, bool) {
    let open = tasks
        .iter()
        .copied()
        .filter(|task| !matches!(task.status, TodoStatus::Completed | TodoStatus::Abandoned))
        .collect::<Vec<_>>();
    let active = open
        .iter()
        .copied()
        .filter(|task| task.status == TodoStatus::InProgress)
        .collect::<Vec<_>>();
    if active.len() > TODO_HUD_TASK_LIMIT {
        let hidden = active.len().saturating_sub(TODO_HUD_TASK_LIMIT);
        return (
            active.into_iter().take(TODO_HUD_TASK_LIMIT).collect(),
            hidden,
            true,
        );
    }

    let mut selected = active;
    for task in open.iter().copied().filter(|task| task.status == TodoStatus::Pending && task.ready) {
        if selected.len() >= TODO_HUD_TASK_LIMIT {
            break;
        }
        selected.push(task);
    }
    let hidden = open.len().saturating_sub(selected.len());
    (selected, hidden, false)
}

/// Count tasks that are neither completed nor abandoned across all phases.
fn todo_open_count(phases: &[TodoPhase]) -> usize {
    phases
        .iter()
        .flat_map(|phase| &phase.tasks)
        .filter(|task| !matches!(&task.status, TodoStatus::Completed | TodoStatus::Abandoned))
        .count()
}

/// Build the compact OMP-style todo and active-subagent trees. This is a
/// display-only projection: task readiness and blockers come directly from the
/// canonical todo items, while job identity remains keyed by the adapter.
fn render_todo_panel_lines(
    phases: &[TodoPhase],
    job_cards: &[JobCardRows],
    theme: Theme,
    width: u16,
) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut lines = Vec::new();
    let tasks = phases.iter().flat_map(|phase| &phase.tasks).collect::<Vec<_>>();
    if !tasks.is_empty() {
        let completed = tasks.iter().filter(|task| task.status == TodoStatus::Completed).count();
        let active = tasks.iter().filter(|task| task.status == TodoStatus::InProgress).count();
        let next = tasks.iter().filter(|task| task.status == TodoStatus::Pending && task.ready).count();
        lines.push(bounded_single_style_line(
            format!(" Todos · {active} active · {next} next · {completed}/{}", tasks.len()),
            width,
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        ));
        let (visible_tasks, hidden_tasks, hidden_tasks_are_active) = selected_todo_hud_tasks(&tasks);
        for task in visible_tasks {
            let (marker, _) = todo_status_marker(&task.status, theme);
            let blocked = todo_blocked_suffix(task);
            let suffix = if blocked.is_empty() { String::new() } else { format!(" · {blocked}") };
            let style = if task.status == TodoStatus::InProgress {
                Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.muted)
            };
            lines.push(bounded_single_style_line(
                format!("  {marker} {}{suffix}", clean_terminal_text(&task.content)),
                width,
                style,
            ));
        }
        if hidden_tasks > 0 {
            let kind = if hidden_tasks_are_active { "active" } else { "open" };
            lines.push(bounded_single_style_line(
                format!("  … {hidden_tasks} more {kind} todos"),
                width,
                Style::default().fg(theme.dim),
            ));
        }
    }

    let active_jobs = job_cards
        .iter()
        .filter(|card| matches!(card.job_status, pi_coding::JobStatus::Queued | pi_coding::JobStatus::Running))
        .collect::<Vec<_>>();
    if !active_jobs.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.push(bounded_single_style_line(
            format!(" waiting on {} jobs", active_jobs.len()),
            width,
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        ));
        for card in active_jobs.iter().take(TODO_HUD_JOB_LIMIT) {
            let running = card.job_status == pi_coding::JobStatus::Running;
            let marker = if running { "►" } else { "○" };
            let status = if running { "running" } else { "queued" };
            let assigned_task = card.todo_task_id.as_deref().and_then(|task_id| {
                tasks.iter().find(|task| task.id == task_id).map(|task| clean_terminal_text(&task.content))
            });
            let description = card
                .rows
                .iter()
                .find(|row| row.role == JobCardRowRole::Description)
                .map(|row| clean_terminal_text(&row.text));
            let summary = assigned_task.or(description).unwrap_or_default();
            let summary = if summary.is_empty() { String::new() } else { format!(" · {summary}") };
            lines.push(bounded_single_style_line(
                format!("  {marker} {} · {status}{summary}", clean_terminal_text(&card.display_name)),
                width,
                Style::default().fg(if running { theme.accent } else { theme.dim }),
            ));
        }
        let hidden_jobs = active_jobs.len().saturating_sub(TODO_HUD_JOB_LIMIT);
        if hidden_jobs > 0 {
            lines.push(bounded_single_style_line(
                format!("  … {hidden_jobs} more active jobs"),
                width,
                Style::default().fg(theme.dim),
            ));
        }
    }
    if !lines.is_empty() {
        lines.push(Line::default());
    }
    lines
}


/// Blocked state is intentionally concise; readiness is conveyed by the lack
/// of a blocker rather than repeating `ready` on every open task.
fn todo_blocked_suffix(task: &TodoItem) -> String {
    if task.ready || matches!(task.status, TodoStatus::Completed | TodoStatus::Abandoned) {
        return String::new();
    }
    if task.blocked_by.is_empty() {
        return "blocked".to_owned();
    }
    let blockers = task
        .blocked_by
        .iter()
        .map(|reason| {
            if reason.content.is_empty() {
                reason.task_id.clone()
            } else {
                reason.content.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("blocked: {blockers}")
}

fn bounded_single_style_line(text: String, width: usize, style: Style) -> Line<'static> {
    let text = if display_width(&text) as usize > width {
        truncate_todo_line(&text, width)
    } else {
        text
    };
    Line::from(Span::styled(text, style))
}


fn truncate_todo_line(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if display_width(text) as usize <= width {
        return text.to_owned();
    }
    if width <= 1 {
        return "…".to_owned();
    }
    let mut output = String::new();
    let mut used = 0usize;
    let target = width.saturating_sub(1);
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_width > target {
            break;
        }
        output.push(ch);
        used += ch_width;
    }
    output.push('…');
    output
}

/// Human `/todo` output keeps the full canonical dependency projection.
fn todo_readiness_suffix(task: &TodoItem) -> String {
    match task.status {
        TodoStatus::Completed | TodoStatus::Abandoned => String::new(),
        TodoStatus::Pending | TodoStatus::InProgress if task.ready => "ready".to_owned(),
        TodoStatus::Pending | TodoStatus::InProgress => {
            if task.blocked_by.is_empty() {
                "blocked".to_owned()
            } else {
                let blockers = task
                    .blocked_by
                    .iter()
                    .map(|reason| {
                        if reason.content.is_empty() {
                            reason.task_id.clone()
                        } else if reason.task_id.is_empty() {
                            reason.content.clone()
                        } else {
                            format!("{}({})", reason.content, reason.task_id)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("blocked by {blockers}")
            }
        }
    }
}

/// Plain-text human projection for `/todo` (no markdown phase-sequence implication).
pub(crate) fn format_todo_human_lines(phases: &[TodoPhase]) -> String {
    if phases.is_empty() {
        return "Todo list is empty.".to_owned();
    }
    let mut lines = Vec::new();
    for phase in phases {
        lines.push(phase.name.clone());
        for task in &phase.tasks {
            let (marker, _) = todo_status_marker(&task.status, crate::theme::DARK);
            let readiness = todo_readiness_suffix(task);
            if readiness.is_empty() {
                lines.push(format!(" {marker} {}", task.content));
            } else {
                lines.push(format!(" {marker} {} · {readiness}", task.content));
            }
        }
    }
    lines.join("\n")
}

/// Map a todo status to a display glyph and theme color.
fn todo_status_marker(status: &TodoStatus, theme: Theme) -> (&'static str, Color) {
    match status {
        TodoStatus::Pending => ("○", theme.dim),
        TodoStatus::InProgress => ("►", theme.accent),
        TodoStatus::Completed => ("✓", theme.success),
        TodoStatus::Abandoned => ("✗", theme.muted),
    }
}

fn render_transcript_entry(
    lines: &mut Vec<Line<'static>>,
    entry: &TranscriptEntry,
    show_thinking: bool,
    expand_tools: bool,
    theme: Theme,
    width: u16,
) {
    render_transcript_entry_inner(
        lines,
        entry,
        show_thinking,
        expand_tools,
        theme,
        width,
        0,
        None,
    );
}

fn render_tool_card(lines: &mut Vec<Line<'static>>, tool: &ToolTranscript, expanded: bool, theme: Theme, width: u16) {
    let card = if expanded { &tool.expanded } else { &tool.compact };
    let border = match card.status {
        ToolCallViewStatus::Failed | ToolCallViewStatus::Cancelled => theme.error,
        ToolCallViewStatus::Running | ToolCallViewStatus::Streaming => theme.border_accent,
        ToolCallViewStatus::Succeeded | ToolCallViewStatus::OrphanRepaired => theme.border_muted,
    };
    let inner = usize::from(width.saturating_sub(2).max(1));
    lines.push(Line::from(Span::styled(format!("╭{}╮", "─".repeat(inner)), Style::default().fg(border))));
    let tool_title = card.rows.iter().find(|row| row.role == ToolCardRowRole::Command).map_or(card.tool_name.as_str(), |row| row.text.as_str());
    if card.tool_name.eq_ignore_ascii_case("bash") {
        push_tool_box_row(lines, tool_title, theme.tool_title, border, inner);
        push_tool_box_row(lines, &format!("$ {}", card.arguments_summary), theme.bash_mode, border, inner);
        if card.rows.iter().any(|row| row.role == ToolCardRowRole::Content) {
            push_tool_separator(lines, " Output ", border, inner);
        }
    } else {
        let marker = match card.status {
            ToolCallViewStatus::Running | ToolCallViewStatus::Streaming | ToolCallViewStatus::Succeeded => if matches!(card.tool_name.as_str(), "edit" | "write") { "✎" } else { "•" },
            ToolCallViewStatus::Failed | ToolCallViewStatus::Cancelled => "✘",
            ToolCallViewStatus::OrphanRepaired => "↻",
        };
        let title = if card.arguments_summary.is_empty() { format!("{marker} {tool_title}") } else { format!("{marker} {tool_title} {}", card.arguments_summary) };
        push_tool_box_row(lines, &title, theme.tool_title, border, inner);
    }
    let code_styles = markdown_ratatui_styles(theme, theme.tool_output);
    for row in &card.rows {
        if row.role == ToolCardRowRole::Command { continue; }
        let color = match row.role {
            ToolCardRowRole::Command => theme.tool_title,
            ToolCardRowRole::Content => theme.tool_output,
            ToolCardRowRole::Details => theme.dim,
            ToolCardRowRole::Status => if card.is_error { theme.error } else { theme.muted },
            ToolCardRowRole::Error => theme.error,
        };
        for text in clean_terminal_text(&row.text).lines() {
            if row.role == ToolCardRowRole::Content
                && let Some(language) = card.code_language.as_deref()
            {
                for line in wrap_styled_line(
                    Line::from(markdown_syntax_spans(text, Some(language), code_styles)),
                    inner.saturating_sub(2).max(1),
                ) {
                    push_tool_box_line(lines, line, border, inner);
                }
            } else {
                push_tool_box_row(lines, text, color, border, inner);
            }
        }
    }
    if card.truncated { push_tool_box_row(lines, &format!("… {} more lines ⟦Ctrl+O: Expand⟧", card.omitted_content_lines), theme.dim, border, inner); }
    lines.push(Line::from(Span::styled(format!("╰{}╯", "─".repeat(inner)), Style::default().fg(border))));
    lines.push(Line::default());
}

fn push_tool_separator(lines: &mut Vec<Line<'static>>, label: &str, border: Color, inner: usize) {
    let fill = "─".repeat(inner.saturating_sub(label.chars().count().saturating_add(2)));
    lines.push(Line::from(vec![Span::styled("├──", Style::default().fg(border)), Span::styled(label.to_owned(), Style::default().fg(border)), Span::styled(fill, Style::default().fg(border)), Span::styled("┤", Style::default().fg(border))]));
}

fn wrap_styled_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut rows = vec![Vec::new()];
    let mut columns = 0usize;
    for span in line.spans {
        let style = span.style;
        let mut chunk = String::new();
        for character in span.content.chars() {
            let character_width = character.width().unwrap_or(0);
            if columns > 0 && columns.saturating_add(character_width) > width {
                if !chunk.is_empty() {
                    rows.last_mut().expect("one styled row").push(Span::styled(std::mem::take(&mut chunk), style));
                }
                rows.push(Vec::new());
                columns = 0;
            }
            chunk.push(character);
            columns = columns.saturating_add(character_width);
        }
        if !chunk.is_empty() {
            rows.last_mut().expect("one styled row").push(Span::styled(chunk, style));
        }
    }
    rows.into_iter().map(Line::from).collect()
}
fn push_tool_box_line(lines: &mut Vec<Line<'static>>, line: Line<'static>, border: Color, inner: usize) {
    let used = line.width();
    let fill = " ".repeat(inner.saturating_sub(used.saturating_add(2)));
    let mut spans = vec![Span::styled("│ ", Style::default().fg(border))];
    spans.extend(line.spans);
    spans.push(Span::raw(fill));
    spans.push(Span::styled(" │", Style::default().fg(border)));
    lines.push(Line::from(spans));
}
fn push_tool_box_row(lines: &mut Vec<Line<'static>>, text: &str, color: Color, border: Color, inner: usize) {
    for row in wrap_display_line(&clean_terminal_text(text), inner.saturating_sub(2).max(1)) {
        let used = usize::from(display_width(&row));
        let fill = " ".repeat(inner.saturating_sub(used.saturating_add(2)));
        lines.push(Line::from(vec![Span::styled("│ ", Style::default().fg(border)), Span::styled(row, Style::default().fg(color)), Span::raw(fill), Span::styled(" │", Style::default().fg(border))]));
    }
}

fn transcript_block_has_content(block: &ContentBlock) -> bool {
    match block {
        ContentBlock::Text { text, .. } => !text.trim().is_empty(),
        ContentBlock::Thinking {
            thinking, redacted, ..
        } => !redacted && !thinking.trim().is_empty(),
        ContentBlock::Image { .. } | ContentBlock::ToolCall(_) => true,
    }
}

const ACTIVE_ANIMATION_FRAMES: &[&str] = &["◐", "◓", "◑", "◒"];
const ACTIVE_JOB_PREFIXES: &[&str] = &["Task ◐ ", "Task ◓ ", "Task ◑ ", "Task ◒ "];

fn render_transcript_entry_inner(
    lines: &mut Vec<Line<'static>>,
    entry: &TranscriptEntry,
    show_thinking: bool,
    expand_tools: bool,
    theme: Theme,
    width: u16,
    animation_frame: usize,
    mut image_context: Option<&mut TranscriptImageContext<'_>>,
) {
    if entry.kind == TranscriptKind::Job {
        if let Some(card) = &entry.job_card {
            render_job_card(lines, card, theme, animation_frame, width);
        }
        return;
    }
    if entry.kind == TranscriptKind::Tool {
        if let Some(tool) = &entry.tool_card { render_tool_card(lines, tool, expand_tools, theme, width); }
        return;
    }
    if !entry.content.iter().any(|block| match block {
        ContentBlock::Thinking { .. } => show_thinking && transcript_block_has_content(block),
        _ => transcript_block_has_content(block),
    }) {
        return;
    }
    let (label, label_color, background) = match entry.kind {
        TranscriptKind::User => (None, theme.accent, Some(theme.user_message_bg)),
        TranscriptKind::Assistant => (None, theme.success, None),
        TranscriptKind::System => (
            Some(
                entry
                    .tool_name
                    .as_deref()
                    .unwrap_or(if entry.is_error { "Error" } else { "System" }),
            ),
            if entry.is_error {
                theme.error
            } else {
                theme.custom_message_label
            },
            None,
        ),
        TranscriptKind::Custom => (
            Some(entry.tool_name.as_deref().unwrap_or("Custom")),
            theme.custom_message_label,
            None,
        ),
        TranscriptKind::Tool | TranscriptKind::Job => {
            unreachable!("cards return before generic rendering")
        }
    };
    if let Some(label) = label {
        lines.push(Line::from(Span::styled(
            format!("{} ·", clean_terminal_text(label)),
            Style::default().fg(label_color),
        )));
    }
    let mut visible_blocks = 0_usize;
    let mut reasoning_labeled = false;
    let mut previous_was_thinking = false;
    for block in &entry.content {
        match block {
            ContentBlock::Text { text, .. } => {
                if text.trim().is_empty() {
                    continue;
                }
                if previous_was_thinking {
                    lines.push(Line::default());
                }
                let is_irc_reply_meta = entry.kind == TranscriptKind::Custom
                    && entry
                        .tool_name
                        .as_deref()
                        .is_some_and(|name| name.starts_with("IRC · "))
                    && text.starts_with("reply to ");
                let base = if is_irc_reply_meta {
                    theme.muted
                } else {
                    match entry.kind {
                        TranscriptKind::User => theme.user_message_text,
                        TranscriptKind::System => {
                            if entry.is_error {
                                theme.error
                            } else {
                                theme.custom_message_text
                            }
                        }
                        TranscriptKind::Custom => theme.custom_message_text,
                        TranscriptKind::Assistant => theme.text,
                        TranscriptKind::Tool | TranscriptKind::Job => {
                            unreachable!("cards return before generic text rendering")
                        }
                    }
                };
                let sanitized = clean_terminal_text(text);
                let mut rendered = if entry.kind == TranscriptKind::User {
                    render_markdown(&sanitized, theme, base)
                } else {
                    render_transcript_markdown(
                        &sanitized,
                        theme,
                        base,
                        width,
                        entry.is_partial,
                    )
                };
                if entry.kind == TranscriptKind::User {
                    rendered = render_user_card_lines(rendered, width);
                }
                if let Some(background) = background {
                    for line in &mut rendered {
                        line.style = line.style.bg(background);
                        for span in &mut line.spans {
                            span.style = span.style.bg(background);
                        }
                    }
                }
                lines.extend(rendered);
                visible_blocks += 1;
                previous_was_thinking = false;
            }
            ContentBlock::Thinking {
                thinking, redacted, ..
            } => {
                if !show_thinking || *redacted || thinking.trim().is_empty() {
                    continue;
                }
                if !reasoning_labeled {
                    lines.push(Line::from(Span::styled(
                        "thinking ·",
                        Style::default()
                            .fg(theme.dim)
                            .add_modifier(Modifier::ITALIC),
                    )));
                    reasoning_labeled = true;
                }
                let mut rendered = render_transcript_markdown(
                    &clean_terminal_text(thinking),
                    theme,
                    theme.thinking_text,
                    width,
                    entry.is_partial,
                );
                for line in &mut rendered {
                    line.style = line.style.add_modifier(Modifier::ITALIC);
                    for span in &mut line.spans {
                        span.style = span.style.add_modifier(Modifier::ITALIC);
                    }
                }
                lines.extend(rendered);
                visible_blocks += 1;
                previous_was_thinking = true;
            }
            ContentBlock::Image { data, mime_type } => {
                let layout = image_context.as_deref_mut().and_then(|context| {
                    context.renderer.layout(
                        data,
                        mime_type,
                        context.config,
                        context.viewport_columns,
                        context.viewport_rows,
                        context.cell_size,
                    )
                });
                if let (Some(layout), Some(context)) = (layout, image_context.as_deref_mut()) {
                    let line_index = lines.len();
                    lines.extend((0..layout.rows()).map(|_| Line::default()));
                    context.candidates.push(TranscriptImageCandidate {
                        line_index,
                        layout,
                        data: data.clone(),
                        mime_type: mime_type.clone(),
                    });
                } else {
                    lines.push(Line::from(Span::styled(
                        format!("[image attachment: {}]", clean_terminal_text(mime_type)),
                        Style::default().fg(theme.muted),
                    )));
                }
                visible_blocks += 1;
            }
            ContentBlock::ToolCall(call) => {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{} ", clean_terminal_text(&call.name)),
                        Style::default()
                            .fg(theme.tool_title)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        clean_terminal_text(&compact_arguments(&call.arguments)),
                        Style::default().fg(theme.tool_output),
                    ),
                ]));
                visible_blocks += 1;
            }
        }
    }
    if visible_blocks > 0 {
        lines.push(Line::default());
    }
}

fn render_user_card_lines(rendered: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let trailing_padding = usize::from(width > 1);
    let content_width = width.saturating_sub(trailing_padding).max(1);
    let mut rows = Vec::new();

    for line in rendered {
        let line_style = line.style;
        let mut row_spans = Vec::<Span<'static>>::new();
        let mut row_width = 0_usize;
        for span in line.spans {
            for character in span.content.chars() {
                let character_width = character.width().unwrap_or(0);
                if row_width > 0 && row_width.saturating_add(character_width) > content_width {
                    push_user_card_row(
                        &mut rows,
                        std::mem::take(&mut row_spans),
                        row_width,
                        width,
                        line_style,
                    );
                    row_width = 0;
                }
                if let Some(last) = row_spans.last_mut().filter(|last| last.style == span.style) {
                    last.content.to_mut().push(character);
                } else {
                    row_spans.push(Span::styled(character.to_string(), span.style));
                }
                row_width = row_width.saturating_add(character_width);
            }
        }
        push_user_card_row(
            &mut rows,
            row_spans,
            row_width,
            width,
            line_style,
        );
    }

    rows
}

fn push_user_card_row(
    rows: &mut Vec<Line<'static>>,
    content: Vec<Span<'static>>,
    content_width: usize,
    width: usize,
    line_style: Style,
) {
    let mut spans = Vec::with_capacity(content.len().saturating_add(1));
    spans.extend(content);
    let trailing_band = width.saturating_sub(content_width);
    if trailing_band > 0 {
        spans.push(Span::raw(" ".repeat(trailing_band)));
    }
    rows.push(Line::from(spans).style(line_style));
}


fn saved_session_preview_lines(
    session: &pi_coding::SessionInfo,
    marker: &str,
    show_path: bool,
) -> Vec<String> {
    let preview = clean_terminal_text(session_display_name(session));
    let path = if show_path {
        format!(" · {}", session.path.display())
    } else {
        String::new()
    };
    let mut logical_lines = preview.split('\n');
    let first = logical_lines.next().unwrap_or_default();
    let mut lines = vec![format!(
        "{marker} {first} · {} messages{path}",
        session.messages
    )];
    lines.extend(logical_lines.map(|line| format!("  {line}")));
    lines
}

fn render_saved_session_selector(
    frame: &mut ratatui::Frame<'_>,
    selector: &SavedSessionSelector,
    theme: Theme,
) {
    let visible = selector.visible_sessions();
    let visible_rows = visible
        .iter()
        .map(|session| saved_session_preview_lines(session, "", selector.show_path()).len())
        .sum::<usize>();
    let height = u16::try_from(visible_rows.saturating_add(6))
        .unwrap_or(u16::MAX)
        .clamp(8, 22);
    let area = centered_rect(
        frame.area().width.saturating_sub(4).min(110).max(30),
        height,
        frame.area(),
    );
    frame.render_widget(Clear, area);
    let mut lines = vec![
        Line::from(Span::styled(
            "Resume Session",
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            session_selector_key_hints(selector),
            Style::default().fg(theme.dim),
        )),
    ];
    match selector.mode() {
        SessionSelectorMode::Rename { value, .. } => lines.push(Line::from(vec![
            Span::styled("Rename: ", Style::default().fg(theme.warning)),
            Span::styled(
                clean_terminal_text(value),
                Style::default().fg(theme.text).bg(theme.selected_bg),
            ),
        ])),
        SessionSelectorMode::ConfirmDelete { .. } => lines.push(Line::from(Span::styled(
            "Delete session? Enter confirm · Esc cancel",
            Style::default().fg(theme.error),
        ))),
        SessionSelectorMode::List => lines.push(Line::from(vec![
            Span::styled("Filter: ", Style::default().fg(theme.dim)),
            Span::styled(
                clean_terminal_text(selector.query()),
                Style::default().fg(theme.text),
            ),
        ])),
    }
    if visible.is_empty() {
        lines.push(Line::from(Span::styled(
            "No matching sessions",
            Style::default().fg(theme.muted),
        )));
    }
    for (index, session) in visible.into_iter().enumerate() {
        let marker = if selector.is_current(session) {
            "•"
        } else {
            " "
        };
        let preview_lines =
            saved_session_preview_lines(session, marker, selector.show_path());
        let style = if index == selector.selected() {
            Style::default().fg(theme.text).bg(theme.selected_bg)
        } else {
            Style::default().fg(if session.name.is_some() {
                theme.warning
            } else {
                theme.text
            })
        };
        lines.extend(preview_lines.into_iter().map(|text| {
            Line::from(Span::styled(text, style))
        }));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.border_accent)),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}


const AGENTS_PANEL_PADDING: u16 = 1;

fn inset_rect(area: Rect, padding: u16) -> Rect {
    let inset = padding.saturating_mul(2);
    Rect {
        x: area.x.saturating_add(padding),
        y: area.y.saturating_add(padding),
        width: area.width.saturating_sub(inset),
        height: area.height.saturating_sub(inset),
    }
}

fn agents_panel_lines(panel: &AgentsPanel, theme: Theme, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let dirty = if panel.dirty() { " · unsaved" } else { "" };
    let mut lines = Vec::new();
    for text in [format!("{}{dirty}", panel.title()), panel.help().to_owned()] {
        let style = if lines.is_empty() {
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.dim)
        };
        lines.extend(
            wrap_display_line(&clean_terminal_text(&text), width)
                .into_iter()
                .map(|line| Line::from(Span::styled(line, style))),
        );
    }
    lines.push(Line::default());

    let rows = panel.view_lines();
    for (index, row) in rows.iter().enumerate() {
        let style = if row.selected {
            Style::default().fg(theme.text).bg(theme.selected_bg)
        } else {
            Style::default().fg(theme.text)
        };
        lines.extend(
            wrap_display_line(&clean_terminal_text(&row.text), width)
                .into_iter()
                .map(|line| Line::from(Span::styled(line, style))),
        );
        if index + 1 < rows.len() {
            lines.push(Line::default());
        }
    }
    if let Some(selected) = panel.selected_row() {
        lines.push(Line::default());
        lines.extend(
            wrap_display_line(&clean_terminal_text(&selected.description), width)
                .into_iter()
                .map(|line| {
                    Line::from(Span::styled(line, Style::default().fg(theme.dim)))
                }),
        );
    }
    lines
}

fn render_agents_panel(frame: &mut ratatui::Frame<'_>, panel: &AgentsPanel, theme: Theme) {
    let width = frame.area().width.saturating_sub(4).min(110).max(1);
    let content_width = width
        .saturating_sub(2)
        .saturating_sub(AGENTS_PANEL_PADDING.saturating_mul(2));
    let lines = agents_panel_lines(panel, theme, content_width);
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .saturating_add(AGENTS_PANEL_PADDING.saturating_mul(2))
        .clamp(8, 24);
    let area = centered_rect(width, height, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_accent));
    let content = inset_rect(block.inner(area), AGENTS_PANEL_PADDING);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines), content);
}

fn render_scoped_model_selector(
    frame: &mut ratatui::Frame<'_>,
    selector: &ScopedModelSelector,
    theme: Theme,
) {
    let visible = selector.visible_ids();
    let height = u16::try_from(visible.len().saturating_add(6))
        .unwrap_or(u16::MAX)
        .clamp(8, 22);
    let area = centered_rect(
        frame.area().width.saturating_sub(4).min(90).max(30),
        height,
        frame.area(),
    );
    frame.render_widget(Clear, area);
    let dirty = if selector.dirty() { " · unsaved" } else { "" };
    let mut lines = vec![
        Line::from(Span::styled(
            "Model Configuration",
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            scoped_model_key_hints().to_owned(),
            Style::default().fg(theme.dim),
        )),
        Line::from(vec![
            Span::styled("Filter: ", Style::default().fg(theme.dim)),
            Span::styled(
                clean_terminal_text(selector.query()),
                Style::default().fg(theme.text),
            ),
        ]),
    ];
    for (index, id) in visible.into_iter().enumerate() {
        let marker = if selector.is_enabled(&id) {
            "✓"
        } else {
            "✗"
        };
        let style = if index == selector.selected() {
            Style::default().fg(theme.text).bg(theme.selected_bg)
        } else {
            Style::default().fg(theme.text)
        };
        lines.push(Line::from(Span::styled(
            format!("{marker} {}", clean_terminal_text(&id)),
            style,
        )));
    }
    lines.push(Line::from(Span::styled(
        format!(
            "{} enabled · {} unavailable",
            selector.enabled_count(),
            selector.unavailable_count()
        ),
        Style::default().fg(theme.dim),
    )));
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.border_accent)),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_transcript_markdown(
    text: &str,
    theme: Theme,
    base: Color,
    width: u16,
    streaming: bool,
) -> Vec<Line<'static>> {
    let styles = markdown_ratatui_styles(theme, base);
    if streaming {
        render_ratatui_markdown_streaming(text, width, styles).lines
    } else {
        render_ratatui_markdown(text, width, styles).lines
    }
}

fn markdown_ratatui_styles(theme: Theme, base: Color) -> MarkdownRatatuiStyles {
    let text = Style::default().fg(base);
    let heading = Style::default()
        .fg(theme.md_heading)
        .add_modifier(Modifier::BOLD);
    MarkdownRatatuiStyles {
        text,
        heading_1: heading,
        heading_2: heading,
        heading_3: heading,
        heading_4: heading,
        heading_5: heading,
        heading_6: heading,
        list_marker: Style::default().fg(theme.md_list_bullet),
        quote: Style::default().fg(theme.md_quote),
        code: Style::default().fg(theme.md_code_block),
        code_fence: Style::default().fg(theme.md_code_block_border),
        inline_code: Style::default().fg(theme.md_code),
        syntax_comment: Style::default().fg(theme.syntax_comment),
        syntax_keyword: Style::default().fg(theme.syntax_keyword),
        syntax_function: Style::default().fg(theme.syntax_function),
        syntax_variable: Style::default().fg(theme.syntax_variable),
        syntax_string: Style::default().fg(theme.syntax_string),
        syntax_number: Style::default().fg(theme.syntax_number),
        syntax_type: Style::default().fg(theme.syntax_type),
        syntax_operator: Style::default().fg(theme.syntax_operator),
        syntax_punctuation: Style::default().fg(theme.syntax_punctuation),
        table_border: Style::default().fg(theme.md_code_block_border),
        table_header: Style::default()
            .fg(theme.md_heading)
            .add_modifier(Modifier::BOLD),
        table_body: text,
        mermaid_border: Style::default().fg(theme.md_code_block_border),
        mermaid_node: Style::default().fg(base),
        mermaid_edge: Style::default().fg(theme.md_list_bullet),
        diagnostic: Style::default()
            .fg(theme.warning)
            .add_modifier(Modifier::ITALIC),
        thematic_break: Style::default().fg(theme.md_hr),
    }
}

fn render_tool_text(text: &str, theme: Theme) -> Vec<Line<'static>> {
    let has_added = text
        .lines()
        .any(|line| line.starts_with('+') && !line.starts_with("+++"));
    let has_removed = text
        .lines()
        .any(|line| line.starts_with('-') && !line.starts_with("---"));
    let is_diff = text.lines().any(|line| line.starts_with("@@")) || (has_added && has_removed);
    if !is_diff {
        return render_markdown(text, theme, theme.tool_output);
    }
    text.lines()
        .map(|line| {
            let color = if line.starts_with('+') && !line.starts_with("+++") {
                theme.tool_diff_added
            } else if line.starts_with('-') && !line.starts_with("---") {
                theme.tool_diff_removed
            } else {
                theme.tool_diff_context
            };
            Line::from(Span::styled(line.to_owned(), Style::default().fg(color)))
        })
        .collect()
}

fn render_markdown(text: &str, theme: Theme, base: Color) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut fenced = false;
    let mut language = String::new();
    for raw in text.lines() {
        let line = raw.trim_end_matches('\r');
        if let Some(info) = line.trim_start().strip_prefix("```") {
            fenced = !fenced;
            if fenced {
                language = info.trim().to_owned();
            }
            lines.push(Line::from(Span::styled(
                if fenced {
                    format!(
                        "┌─ {}",
                        if language.is_empty() {
                            "code"
                        } else {
                            &language
                        }
                    )
                } else {
                    "└─".to_owned()
                },
                Style::default().fg(theme.md_code_block_border),
            )));
            continue;
        }
        if fenced {
            lines.push(Line::from(syntax_spans(line, theme)));
            continue;
        }
        let trimmed = line.trim_start();
        if let Some(heading) = trimmed
            .strip_prefix("### ")
            .or_else(|| trimmed.strip_prefix("## "))
            .or_else(|| trimmed.strip_prefix("# "))
        {
            lines.push(Line::from(Span::styled(
                heading.to_owned(),
                Style::default()
                    .fg(theme.md_heading)
                    .add_modifier(Modifier::BOLD),
            )));
        } else if let Some(quote) = trimmed.strip_prefix("> ") {
            lines.push(Line::from(vec![
                Span::styled("│ ", Style::default().fg(theme.md_quote_border)),
                Span::styled(quote.to_owned(), Style::default().fg(theme.md_quote)),
            ]));
        } else if is_markdown_rule(trimmed) {
            lines.push(Line::from(Span::styled(
                "─".repeat(trimmed.len().clamp(3, 72)),
                Style::default().fg(theme.md_hr),
            )));
        } else if let Some(item) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            let mut spans = vec![Span::styled(
                "• ",
                Style::default().fg(theme.md_list_bullet),
            )];
            spans.extend(inline_markdown_spans(item, theme, base));
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::from(inline_markdown_spans(line, theme, base)));
        }
    }
    if text.is_empty() {
        lines.push(Line::default());
    }
    lines
}

fn inline_markdown_spans(text: &str, theme: Theme, base: Color) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let next = ["`", "**", "[", "*"]
            .iter()
            .filter_map(|marker| rest.find(marker).map(|index| (index, *marker)))
            .min_by_key(|(index, _)| *index);
        let Some((index, marker)) = next else {
            spans.push(Span::styled(rest.to_owned(), Style::default().fg(base)));
            break;
        };
        if index > 0 {
            spans.push(Span::styled(
                rest[..index].to_owned(),
                Style::default().fg(base),
            ));
            rest = &rest[index..];
        }
        match marker {
            "`" => {
                if let Some(end) = rest[1..].find('`') {
                    spans.push(Span::styled(
                        rest[1..=end].to_owned(),
                        Style::default().fg(theme.md_code),
                    ));
                    rest = &rest[end + 2..];
                } else {
                    spans.push(Span::styled(rest.to_owned(), Style::default().fg(base)));
                    break;
                }
            }
            "**" => {
                if let Some(end) = rest[2..].find("**") {
                    spans.push(Span::styled(
                        rest[2..end + 2].to_owned(),
                        Style::default().fg(base).add_modifier(Modifier::BOLD),
                    ));
                    rest = &rest[end + 4..];
                } else {
                    spans.push(Span::styled(rest.to_owned(), Style::default().fg(base)));
                    break;
                }
            }
            "[" => {
                if let Some(close) = rest.find("](")
                    && let Some(end) = rest[close + 2..].find(')')
                {
                    spans.push(Span::styled(
                        rest[1..close].to_owned(),
                        Style::default()
                            .fg(theme.md_link)
                            .add_modifier(Modifier::UNDERLINED),
                    ));
                    spans.push(Span::styled(
                        format!(" ({})", &rest[close + 2..close + 2 + end]),
                        Style::default().fg(theme.md_link_url),
                    ));
                    rest = &rest[close + 3 + end..];
                } else {
                    spans.push(Span::styled("[".to_owned(), Style::default().fg(base)));
                    rest = &rest[1..];
                }
            }
            "*" => {
                if let Some(end) = rest[1..].find('*') {
                    spans.push(Span::styled(
                        rest[1..=end].to_owned(),
                        Style::default().fg(base).add_modifier(Modifier::ITALIC),
                    ));
                    rest = &rest[end + 2..];
                } else {
                    spans.push(Span::styled("*".to_owned(), Style::default().fg(base)));
                    rest = &rest[1..];
                }
            }
            _ => unreachable!(),
        }
    }
    spans
}

fn syntax_spans(line: &str, theme: Theme) -> Vec<Span<'static>> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("//") || trimmed.starts_with('#') || trimmed.starts_with("--") {
        return vec![Span::styled(
            line.to_owned(),
            Style::default().fg(theme.syntax_comment),
        )];
    }
    let keywords = [
        "as", "async", "await", "break", "const", "continue", "def", "else", "enum", "fn", "for",
        "from", "if", "impl", "import", "in", "let", "loop", "match", "mod", "mut", "pub",
        "return", "self", "static", "struct", "trait", "type", "use", "where", "while",
    ];
    let chars = line.chars().collect::<Vec<_>>();
    let mut spans = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let start = index;
        let character = chars[index];
        let color = if character.is_whitespace() {
            while index < chars.len() && chars[index].is_whitespace() {
                index += 1;
            }
            theme.syntax_punctuation
        } else if character == '"' || character == '\'' {
            let quote = character;
            index += 1;
            while index < chars.len() {
                let current = chars[index];
                index += 1;
                if current == quote {
                    break;
                }
            }
            theme.syntax_string
        } else if character.is_ascii_digit() {
            while index < chars.len()
                && (chars[index].is_ascii_hexdigit() || chars[index] == '.' || chars[index] == '_')
            {
                index += 1;
            }
            theme.syntax_number
        } else if character.is_alphanumeric() || character == '_' {
            while index < chars.len() && (chars[index].is_alphanumeric() || chars[index] == '_') {
                index += 1;
            }
            let token = chars[start..index].iter().collect::<String>();
            if keywords.contains(&token.as_str()) {
                theme.syntax_keyword
            } else if token.chars().next().is_some_and(char::is_uppercase) {
                theme.syntax_type
            } else if chars[index..].iter().find(|value| !value.is_whitespace()) == Some(&&'(') {
                theme.syntax_function
            } else {
                theme.syntax_variable
            }
        } else {
            index += 1;
            if "+-*/%=!<>|&^~".contains(character) {
                theme.syntax_operator
            } else {
                theme.syntax_punctuation
            }
        };
        spans.push(Span::styled(
            chars[start..index].iter().collect::<String>(),
            Style::default().fg(color),
        ));
    }
    spans
}

fn apply_tool_line_styles(lines: &mut [Line<'static>], theme: Theme, background: Option<Color>) {
    for line in lines {
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        let foreground = if text.starts_with('+') && !text.starts_with("+++") {
            Some(theme.tool_diff_added)
        } else if text.starts_with('-') && !text.starts_with("---") {
            Some(theme.tool_diff_removed)
        } else if text.starts_with("@@") || text.starts_with(' ') {
            Some(theme.tool_diff_context)
        } else {
            None
        };
        if let Some(foreground) = foreground {
            for span in &mut line.spans {
                span.style = span.style.fg(foreground);
            }
        }
        if let Some(background) = background {
            line.style = line.style.bg(background);
        }
    }
}

fn extension_widget_lines(
    snapshot: &crate::extension_ui::ExtensionUiSnapshot,
    placement: UiWidgetPlacement,
    theme: Theme,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for widget in snapshot
        .widgets
        .iter()
        .filter(|widget| widget.placement == placement)
    {
        for line in &widget.lines {
            lines.push(Line::from(Span::styled(
                clean_terminal_text(line),
                Style::default()
                    .fg(theme.custom_message_text)
                    .bg(theme.custom_message_bg),
            )));
        }
    }
    if placement == UiWidgetPlacement::BelowEditor {
        for notification in snapshot.notifications.iter().rev().take(3).rev() {
            let color = match notification.level {
                UiNotificationLevel::Info => theme.accent,
                UiNotificationLevel::Warning => theme.warning,
                UiNotificationLevel::Error => theme.error,
            };
            lines.push(Line::from(Span::styled(
                clean_terminal_text(&notification.message),
                Style::default().fg(color).bg(theme.custom_message_bg),
            )));
        }
    }
    lines
}

fn render_tree_panel(frame: &mut ratatui::Frame<'_>, panel: &TreePanel, theme: Theme) {
    let width = frame.area().width.saturating_sub(4).min(110).max(30);
    let rows = if panel.mode == TreePanelMode::Fork {
        panel.visible().len().saturating_mul(3)
    } else {
        panel.visible().len()
    };
    let height = u16::try_from(rows.saturating_add(8))
        .unwrap_or(u16::MAX)
        .clamp(10, frame.area().height.saturating_sub(2).max(10));
    let area = centered_rect(width, height, frame.area());
    frame.render_widget(Clear, area);
    let mut lines = vec![Line::from(Span::styled(
        clean_terminal_text(&panel.title),
        Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
    ))];
    if panel.mode == TreePanelMode::Fork {
        lines.push(Line::from(Span::styled(
            "Select a user message to copy the active path up to that point into a new session",
            Style::default().fg(theme.dim),
        )));
    } else {
        lines.push(Line::from(vec![
            Span::styled("Type to search: ", Style::default().fg(theme.dim)),
            Span::styled(clean_terminal_text(&panel.query), Style::default().fg(theme.text)),
        ]));
    }
    lines.push(Line::from(Span::styled(
        tree_panel_key_hints(panel),
        Style::default().fg(theme.dim),
    )));
    if panel.visible().is_empty() {
        lines.push(Line::from(Span::styled(
            if panel.mode == TreePanelMode::Fork { "No user messages found" } else { "No entries found" },
            Style::default().fg(theme.muted),
        )));
    } else {
        for (index, node) in panel.visible().iter().enumerate() {
            if panel.mode == TreePanelMode::Fork {
                let cursor = if node.selected { "› " } else { "  " };
                let body = node.text.strip_prefix("user: ").unwrap_or(&node.text);
                let style = if node.selected {
                    Style::default().fg(theme.text).bg(theme.selected_bg).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.text)
                };
                lines.push(Line::from(Span::styled(clean_terminal_text(&format!("{cursor}{body}")), style)));
                lines.push(Line::from(Span::styled(
                    format!("  Message {} of {}", index + 1, panel.visible().len()),
                    Style::default().fg(theme.dim),
                )));
                lines.push(Line::from(""));
                continue;
            }
            let mut prefix = String::new();
            for gutter in &node.gutters {
                prefix.push_str(if *gutter { "│  " } else { "   " });
            }
            let connector = if node.depth == 0 { "" } else if node.is_last { "└" } else { "├" };
            let branch = if node.depth == 0 { "" } else if node.foldable { if node.folded { "⊞ " } else { "⊟ " } } else { "─ " };
            prefix.push_str(connector);
            prefix.push_str(branch);
            let cursor = if node.selected { "› " } else { "  " };
            let active = if node.active { "• " } else { "  " };
            let label = node.label.as_ref().map_or_else(String::new, |label| {
                if panel.show_label_timestamps { format!("[{label} @ {}] ", node.label_timestamp.as_deref().unwrap_or("?")) } else { format!("[{label}] ") }
            });
            let style = if node.selected {
                Style::default().fg(theme.text).bg(theme.selected_bg)
            } else if node.active {
                Style::default().fg(theme.accent)
            } else {
                Style::default().fg(theme.text)
            };
            lines.push(Line::from(Span::styled(
                clean_terminal_text(&format!("{cursor}{prefix}{active}{label}{}", node.text)),
                style,
            )));
        }
    }
    if panel.mode == TreePanelMode::Navigate {
        if let Some(label) = panel.label_input.as_ref() {
            lines.push(Line::from(vec![
                Span::styled("Label (empty to remove): ", Style::default().fg(theme.warning)),
                Span::styled(clean_terminal_text(label), Style::default().fg(theme.text).bg(theme.selected_bg)),
            ]));
        }
        lines.push(Line::from(Span::styled(
            format!("({}/{}) [{}]", panel.selected.saturating_add(1).min(panel.visible().len()), panel.visible().len(), panel.filter.label()),
            Style::default().fg(theme.dim),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(theme.border_accent)))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn format_hotkeys_text(bindings: &KeyBindingsManager) -> String {
    let sections = bindings.hotkey_sections();
    if sections.is_empty() {
        return "No keybindings loaded".to_owned();
    }
    let mut parts = Vec::with_capacity(sections.len());
    for section in sections {
        let mut block = format!("## {}", section.title);
        for row in section.rows {
            block.push('\n');
            block.push_str(&row);
        }
        parts.push(block);
    }
    parts.join("\n\n")
}

fn overlay_unknown_key_status(key: KeyEvent) -> String {
    let chord = crate::keybindings::normalize_key(&key)
        .map(|chord| crate::keybindings::format_chord_display(&chord))
        .unwrap_or_else(|| format!("{key:?}"));
    format!("Unknown key {chord} · see footer hints · Esc closes · /hotkeys for full map")
}

fn tree_panel_key_hints(panel: &TreePanel) -> String {
    if panel.label_input.is_some() {
        return "Enter save label · Esc cancel".to_owned();
    }
    match panel.mode {
        TreePanelMode::Fork => "↑/↓ move · Enter fork · Esc/q close".to_owned(),
        TreePanelMode::Navigate => {
            "↑/↓ move · ← fold · → unfold · Enter select · type search · Esc/q close".to_owned()
        }
    }
}

fn session_selector_key_hints(selector: &SavedSessionSelector) -> String {
    match selector.mode() {
        SessionSelectorMode::Rename { .. } => "Type name · Enter save · Esc cancel".to_owned(),
        SessionSelectorMode::ConfirmDelete { .. } => "Enter confirm · Esc cancel".to_owned(),
        SessionSelectorMode::List => {
            "↑/↓ · Enter resume · Ctrl+N/P/S/R/D · type filter · Esc/q close".to_owned()
        }
    }
}

fn scoped_model_key_hints() -> &'static str {
    "↑/↓ · Enter toggle · Ctrl+A/X/P/S · Alt+↑/↓ reorder · type filter · Esc/q close"
}

fn selector_panel_key_hints(panel: &SelectorPanel) -> String {
    if panel.help.trim().is_empty() {
        "↑/↓ move · Enter select · type filter · Esc/q close".to_owned()
    } else if panel.help.contains("Esc") {
        panel.help.clone()
    } else {
        format!("{} · Esc/q close", panel.help)
    }
}

fn render_selector_panel(frame: &mut ratatui::Frame<'_>, panel: &SelectorPanel, theme: Theme) {
    let visible = panel.visible_indices();
    let goal_panel = panel.title == "Goal";
    let height = u16::try_from(visible.len().saturating_add(4))
        .unwrap_or(u16::MAX)
        .clamp(5, 20);
    let width = frame
        .area()
        .width
        .saturating_sub(4)
        .min(if goal_panel { 84 } else { 76 })
        .max(20);
    let area = centered_rect(width, height, frame.area());
    let inner_width = usize::from(area.width.saturating_sub(4).max(1));
    let mut lines = Vec::new();
    if !goal_panel {
        lines.push(Line::from(vec![
            Span::styled("Filter: ", Style::default().fg(theme.dim)),
            Span::styled(clean_terminal_text(&panel.query), Style::default().fg(theme.text)),
        ]));
    }
    for (visible_index, item_index) in visible.into_iter().enumerate() {
        let item = &panel.items[item_index];
        let marker = if item.checked { "✓" } else { " " };
        let style = if visible_index == panel.selected {
            Style::default().fg(theme.text).bg(theme.selected_bg)
        } else {
            Style::default().fg(theme.text)
        };
        let row = format!(
            " {marker} {}  {}",
            clean_terminal_text(&item.label),
            clean_terminal_text(&item.description)
        );
        for wrapped in wrap_display_line(&row, inner_width) {
            lines.push(Line::from(Span::styled(wrapped, style)));
        }
    }
    lines.push(Line::from(Span::styled(
        clean_terminal_text(&selector_panel_key_hints(panel)),
        Style::default().fg(theme.dim),
    )));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .padding(ratatui::widgets::Padding::horizontal(1))
                    .border_style(Style::default().fg(theme.border_accent))
                    .title(format!(" {} ", clean_terminal_text(&panel.title))),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(area.width.saturating_sub(width) / 2),
        y: area
            .y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width: width.min(area.width),
        height: height.min(area.height),
    }
}
fn is_markdown_rule(text: &str) -> bool {
    text.len() >= 3
        && text
            .chars()
            .all(|character| character == '-' || character == '*' || character == '_')
}
fn clean_terminal_text(text: &str) -> String {
    let mut clean = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            match characters.next() {
                Some('[') => {
                    for value in characters.by_ref() {
                        if ('@'..='~').contains(&value) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(value) = characters.next() {
                        if value == '\u{7}' {
                            break;
                        }
                        if value == '\u{1b}' && characters.peek() == Some(&'\\') {
                            characters.next();
                            break;
                        }
                    }
                }
                Some(_) | None => {}
            }
        } else if character == '\t' {
            clean.push_str("    ");
        } else if character == '\n' || !character.is_control() {
            clean.push(character);
        }
    }
    clean
}

fn normalize_newlines(text: &str) -> Cow<'_, str> {
    if !text.contains('\r') {
        return Cow::Borrowed(text);
    }
    let mut normalized = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\r' {
            if characters.peek() == Some(&'\n') {
                characters.next();
            }
            normalized.push('\n');
        } else {
            normalized.push(character);
        }
    }
    Cow::Owned(normalized)
}
fn wrapped_line_count(lines: &[Line<'_>], width: u16) -> usize {
    let width = usize::from(width.max(1));
    lines
        .iter()
        .map(|line| {
            let columns = line
                .spans
                .iter()
                .flat_map(|span| span.content.chars())
                .map(|character| character.width().unwrap_or(0))
                .sum::<usize>();
            columns.max(1).div_ceil(width)
        })
        .sum()
}

fn display_width(text: &str) -> u16 {
    let width = text
        .chars()
        .map(|character| character.width().unwrap_or(0))
        .sum::<usize>();
    u16::try_from(width).unwrap_or(u16::MAX)
}

fn floor_char_boundary(text: &str, requested: usize) -> usize {
    if requested >= text.len() {
        return text.len();
    }
    text.char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= requested)
        .last()
        .unwrap_or(0)
}

/// Clipboard image waiting in the composer. Bytes live only inside
/// `ContentBlock::Image`; width/height are display metadata from the guarded
/// pipeline and never appear in the model payload.
#[derive(Clone, Debug, PartialEq)]
struct PendingAttachment {
    block: ContentBlock,
    width: u32,
    height: u32,
}

impl PendingAttachment {
    fn from_clipboard_image(image: crate::clipboard::ClipboardImage) -> Self {
        let width = image.width;
        let height = image.height;
        Self {
            block: image.into_content_block(),
            width,
            height,
        }
    }

    /// OMP-compatible composer placeholder: `[Image #N, WIDTHxHEIGHT]`.
    fn label(&self, index: usize) -> String {
        format!("[Image #{}, {}x{}]", index + 1, self.width, self.height)
    }
}

/// Human-visible labels for pending image attachments. Never includes base64
/// payload bytes or absolute paths — only ordinal and decoded dimensions.
fn pending_attachment_labels(attachments: &[PendingAttachment]) -> Vec<String> {
    attachments
        .iter()
        .enumerate()
        .map(|(index, attachment)| attachment.label(index))
        .collect()
}

/// Merge clipboard pending attachments with expanded file images for one submit.
/// Pure helper so adapter tests can assert exact-once assembly without a desktop clipboard.
fn assemble_submit_attachments(
    pending: &[PendingAttachment],
    file_images: Vec<ContentBlock>,
) -> Vec<ContentBlock> {
    let mut attachments = pending
        .iter()
        .map(|attachment| attachment.block.clone())
        .collect::<Vec<_>>();
    attachments.extend(file_images);
    attachments
}

/// After a failed prompt, keep only the pre-submit pending attachments.
/// File images must not be folded into pending while the draft still contains
/// `@file` markers — expand will re-emit them on the next submit.
fn restore_pending_after_failed_submit(
    pending: &mut Vec<PendingAttachment>,
    pre_submit_pending: Vec<PendingAttachment>,
) {
    *pending = pre_submit_pending;
}

fn content_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.clone()),
            ContentBlock::Image { mime_type, .. } => {
                Some(format!("[image attachment: {mime_type}]"))
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_bash_input(text: &str) -> Option<(String, bool)> {
    let (command, exclude_from_context) = if let Some(command) = text.strip_prefix("!!") {
        (command, true)
    } else {
        (text.strip_prefix('!')?, false)
    };
    let command = command.trim();
    (!command.is_empty()).then(|| (command.to_owned(), exclude_from_context))
}

fn message_text(message: Message) -> Option<String> {
    match message {
        Message::User(message) => Some(content_text(&message.content)),
        _ => None,
    }
    .filter(|text| !text.trim().is_empty())
}

fn fuzzy_match(candidate: &str, query: &str) -> bool {
    if query.is_empty() || candidate.starts_with(query) {
        return true;
    }
    let mut candidate = candidate.chars();
    query.chars().all(|expected| {
        candidate
            .by_ref()
            .any(|actual| actual.eq_ignore_ascii_case(&expected))
    })
}

fn compact_arguments(arguments: &serde_json::Value) -> String {
    let value = ["command", "path", "pattern"]
        .into_iter()
        .find_map(|key| arguments.get(key).and_then(serde_json::Value::as_str))
        .unwrap_or_default();
    let mut characters = value.chars();
    let prefix = characters.by_ref().take(57).collect::<String>();
    if characters.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}

/// Discovers theme directories and keybinding files in global-first,
/// project-last order so project config overrides global config. Reuses the
/// shared `.pi` config dir name; the home lookup mirrors pi-coding's. When a
/// shared `ResourcePaths`/`ResourceSnapshot` API lands in pi-coding, swap this
/// out to consume it — the managers themselves only take these paths.
fn config_paths(cwd: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let global = home_dir()
        .map(|home| Path::new(&home).join(CONFIG_DIR_NAME))
        .unwrap_or_else(|| PathBuf::from(CONFIG_DIR_NAME));
    let project = cwd.join(CONFIG_DIR_NAME);
    let theme_dirs = vec![global.join("themes"), project.join("themes")];
    let keybinding_files = vec![
        global.join("keybindings.json"),
        project.join("keybindings.json"),
    ];
    (theme_dirs, keybinding_files)
}

/// Best-effort home directory (`HOME` on Unix, `USERPROFILE` on Windows).
fn home_dir() -> Option<String> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .and_then(|value| value.into_string().ok())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_handles_unicode_boundaries_and_multiline_delete() {
        let mut editor = EditorState::new();
        editor.insert_char('é');
        editor.insert_char('a');
        editor.move_left();
        editor.backspace();
        assert_eq!(editor.text(), "a");
        editor.insert_newline();
        editor.insert_char('b');
        editor.move_left();

        editor.backspace();
        assert_eq!(editor.text(), "ba");
    }

    #[test]
    fn raw_paste_burst_maps_printable_and_enter_but_not_tab_or_control_keys() {
        assert_eq!(
            raw_paste_character(KeyEvent::new(KeyCode::Char('é'), KeyModifiers::NONE)),
            Some('é')
        );
        assert_eq!(
            raw_paste_character(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some('\n')
        );
        assert_eq!(
            raw_paste_character(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            raw_paste_character(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            raw_paste_character(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            None
        );
    }

    #[test]
    fn raw_input_burst_keeps_plain_printables_on_key_path() {
        // Ordinary printable input is dispatched immediately and is never an
        // unmarked-paste candidate.
        for text in ["/work", "/settings", "ps"] {
            assert_eq!(classify_raw_input_burst(text), RawInputDisposition::Keys);
        }
        // A single Enter remains ambiguous — stay on the key path.
        assert_eq!(
            classify_raw_input_burst("hello\n"),
            RawInputDisposition::Keys
        );
        assert_eq!(
            classify_raw_input_burst("one\ntwo"),
            RawInputDisposition::Keys
        );
        // True multiline unmarked paste (≥2 breaks with trailing content).
        assert_eq!(
            classify_raw_input_burst("one\ntwo\nthree"),
            RawInputDisposition::Paste
        );
        assert_eq!(
            classify_raw_input_burst("one\r\ntwo\r\nthree"),
            RawInputDisposition::Paste
        );
    }

    #[test]
    fn slash_work_inserts_every_printable_exactly_once() {
        let mut state = todo_test_state(Vec::new());
        for character in "/work".chars() {
            apply_classified_burst(&mut state, &character.to_string());
        }
        assert_eq!(state.editor.text(), "/work");
    }

    #[test]
    fn slash_completion_uses_primary_catalog_only() {
        let mut state = todo_test_state(Vec::new());
        // Full executable catalog stays on state for dispatch/source resolution.
        state.commands = BUILTIN_COMMANDS
            .iter()
            .map(|command| InteractiveCommand {
                name: command.name.to_owned(),
                description: command.description.to_owned(),
                source: CommandSource::Builtin,
            })
            .collect();

        state.editor.set_text("/");
        state.refresh_completions();
        let values = state
            .completions
            .items
            .iter()
            .map(|item| item.value.clone())
            .collect::<Vec<_>>();
        let expected = visible_catalog()
            .into_iter()
            .map(|command| format!("/{}", command.name))
            .collect::<Vec<_>>();
        assert_eq!(values, expected);
        assert!(!values.iter().any(|value| value == "/help"));
        assert!(!values.iter().any(|value| value == "/import"));
        assert!(values.iter().any(|value| value == "/settings"));
        assert!(values.iter().any(|value| value == "/goal"));
        assert!(
            values.len() > MAX_COMPLETIONS,
            "primary catalog must keep entries beyond the visible window"
        );

        let settings_index = values
            .iter()
            .position(|value| value == "/settings")
            .expect("settings in primary catalog");
        state.completions.selected = settings_index;
        let (window_start, visible) = state.completions.visible_window(MAX_COMPLETIONS);
        assert_eq!(visible.len(), MAX_COMPLETIONS.min(values.len()));
        assert!(
            (window_start..window_start + visible.len()).contains(&settings_index)
                || visible.iter().any(|item| item.value == "/settings"),
            "selected-centered window must cover /settings"
        );

        state.accept_completion();
        assert_eq!(state.editor.text(), "/settings");
        assert!(state.completions.items.is_empty());
    }



    #[test]
    fn exact_slash_command_selects_matching_completion() {
        let mut state = todo_test_state(Vec::new());
        state.commands = vec![
            InteractiveCommand {
                name: "loops".to_owned(),
                description: "List loops".to_owned(),
                source: CommandSource::Builtin,
            },
            InteractiveCommand {
                name: "process".to_owned(),
                description: "Process ops".to_owned(),
                source: CommandSource::Builtin,
            },
            InteractiveCommand {
                name: "ps".to_owned(),
                description: "List processes".to_owned(),
                source: CommandSource::Builtin,
            },
        ];
        state.editor.set_text("/ps");
        state.refresh_completions();
        let selected = state
            .completions
            .selected()
            .expect("exact /ps must select a completion");
        assert_eq!(selected.value, "/ps");
        assert!(completion_already_matches_editor(&state));
    }

    #[test]
    fn exact_slash_enter_falls_through_without_reaccept() {
        let mut state = todo_test_state(Vec::new());
        state.commands = vec![InteractiveCommand {
            name: "ps".to_owned(),
            description: "List processes".to_owned(),
            source: CommandSource::Builtin,
        }];
        state.editor.set_text("/ps");
        state.refresh_completions();
        assert!(completion_already_matches_editor(&state));
        // One-Enter semantics: when selected already equals the draft, the
        // completion interceptor must not consume Enter as accept_completion.
        // Accept would be a no-op fill; execute path is the fall-through.
        let before = state.editor.text();
        assert_eq!(before, "/ps");
        // Simulate what would happen if accept were wrongly forced:
        state.accept_completion();
        assert_eq!(
            state.editor.text(),
            "/ps",
            "accepting an already-exact match must not rewrite the draft"
        );
        // After a real fall-through execute, completions clear via submit —
        // here we only assert the match predicate that gates fall-through.
        state.editor.set_text("/ps");
        state.refresh_completions();
        assert!(
            completion_already_matches_editor(&state),
            "exact draft must keep the fall-through gate open"
        );
        // Partial drafts still require accept (gate closed).
        state.editor.set_text("/p");
        state.refresh_completions();
        assert!(!completion_already_matches_editor(&state));
    }


    #[test]
    fn paste_event_normalizes_multiline_crlf_and_preserves_unicode() {
        let mut state = todo_test_state(Vec::new());
        handle_paste(&mut state, "first\r\né🙂\rthird");
        assert_eq!(state.editor.text(), "first\né🙂\nthird");
        assert_eq!((state.editor.row, state.editor.column), (2, "third".len()));
        assert!(state.transcript.is_empty(), "pasting must not submit a message");
    }

    #[test]
    fn multiline_paste_7608_then_typing_is_immediate_and_undo_is_grouped() {
        let mut state = todo_test_state(Vec::new());
        let payload = format!("{}\nline two\nline three", "p".repeat(7_608));
        handle_paste(&mut state, &payload);
        state.editor.insert_char('x');
        assert_eq!(state.editor.text(), format!("{payload}x"));
        assert_eq!(state.editor.undo.len(), 1);
        state.editor.undo();
        assert!(state.editor.is_empty());
    }

    #[test]
    fn oversize_paste_rejection_does_not_consume_next_key() {
        let mut state = todo_test_state(Vec::new());
        state.editor.set_text("keep");
        let undo_entries = state.editor.undo.len();
        handle_paste(&mut state, &"x".repeat(MAX_PASTE_BYTES + 1));
        state.editor.insert_char('x');
        assert_eq!(state.editor.text(), "keepx");
        assert_eq!(state.editor.undo.len(), undo_entries + 1);
        assert!(state.status.contains("Paste rejected"));
    }

    #[test]
    fn multiline_paste_stays_one_draft_until_explicit_enter_and_esc_clears_it() {
        let mut state = todo_test_state(Vec::new());
        handle_paste(&mut state, "one\ntwo\nthree");
        assert_eq!(state.editor.text(), "one\ntwo\nthree");
        assert!(state.transcript.is_empty());

        state.editor.clear();
        assert!(state.editor.is_empty(), "Esc abort clears the entire pasted draft");
    }

    #[test]
    fn editor_word_motion_and_deletion_preserve_unicode_boundaries() {
        let mut editor = EditorState::new();
        editor.insert_text("one αα, two\nβ line");
        editor.move_word_left();
        assert_eq!((editor.row, editor.column), (1, "β ".len()));
        editor.delete_word_backward();
        assert_eq!(editor.text(), "one αα, two\nline");
        editor.move_word_left();
        editor.delete_word_backward();
        assert_eq!(editor.text(), "one αα, \nline");
        assert!(editor.lines[editor.row].is_char_boundary(editor.column));
    }

    #[test]
    fn editor_kill_yank_pop_and_undo_restore_multiline_text() {
        let mut editor = EditorState::new();
        editor.insert_text("alpha βeta\nthird");
        editor.delete_to_line_start();
        editor.delete_word_backward();
        assert_eq!(editor.text(), "alpha βeta");
        editor.move_home();
        editor.delete_word_forward();
        assert_eq!(editor.text(), " βeta");
        editor.yank();
        assert_eq!(editor.text(), "alpha βeta");
        editor.yank_pop();
        assert_eq!(editor.text(), "\nthird βeta");
        editor.undo();
        assert_eq!(editor.text(), "alpha βeta");
    }

    #[test]
    fn editor_jump_searches_across_unicode_lines() {
        let mut editor = EditorState::new();
        editor.insert_text("aé\nβéc");
        editor.move_home();
        editor.begin_jump(JumpDirection::Backward);
        assert!(editor.jump_to_char('é'));
        assert_eq!((editor.row, editor.column), (0, 1));
        editor.begin_jump(JumpDirection::Forward);
        assert!(editor.jump_to_char('é'));
        assert_eq!((editor.row, editor.column), (1, 'β'.len_utf8()));
        assert!(editor.lines[editor.row].is_char_boundary(editor.column));
    }

    #[test]
    fn large_draft_contiguous_insertion_keeps_bounded_useful_undo() {
        let mut editor = EditorState::new();
        for _ in 0..7_608 {
            editor.insert_char('x');
        }
        assert_eq!(editor.undo.len(), 1, "one snapshot per insertion run");
        editor.move_left();
        editor.insert_char('y');
        assert_eq!(editor.undo.len(), 2, "navigation breaks the insertion run");
        editor.undo();
        assert_eq!(editor.text(), "x".repeat(7_608));

        for index in 0..(MAX_YANK_HISTORY + 5) {
            editor.push_kill(index.to_string(), false, false);
        }
        assert_eq!(editor.kill_ring.len(), MAX_YANK_HISTORY);
        assert_eq!(
            editor.kill_ring.last().cloned(),
            Some((MAX_YANK_HISTORY + 4).to_string())
        );
    }

    #[test]
    fn frequent_todo_and_job_events_preserve_editor_draft_and_undo() {
        let mut state = todo_test_state(Vec::new());
        for character in "large draft".chars() {
            state.editor.insert_char(character);
        }
        let before = state.editor.text();
        let undo_entries = state.editor.undo.len();
        for index in 0..256 {
            state.apply(ApplicationEvent::TodoUpdated {
                phases: Vec::new(),
                completed_tasks: Vec::new(),
            });
            state.apply(ApplicationEvent::Orchestration(
                pi_coding::OrchestrationEvent::JobUpdated {
                    group_id: "burst".to_owned(),
                    job: pi_coding::JobSnapshot {
                        id: format!("job-{index}"),
                        agent_id: format!("agent-{index}"),
                        agent: "task".to_owned(),
                        parent_id: "Main".to_owned(),
                        description: None,
                        todo_task_id: None,
                        workflow_id: None,
                        workflow_generation: None,
                        status: pi_coding::JobStatus::Running,
                        created_at: index,
                        started_at: Some(index),
                        finished_at: None,
                        result: None,
                    },
                },
            ));
        }
        assert_eq!(state.editor.text(), before);
        assert_eq!(state.editor.undo.len(), undo_entries);
        state.editor.insert_char('x');
        assert_eq!(state.editor.text(), "large draftx");
    }

    #[test]
    fn slash_completion_fuzzy_matches_and_accepts_selection() {
        let (background_tx, _background_rx) = mpsc::unbounded_channel();
        let mut state = TuiState {
            tool_cards: ToolCardPresentationAdapter::new(),
            job_cards: JobCardPresentationAdapter::new(),
            transcript: Vec::new(),
            committed_entries: 0,
            editor: EditorState::new(),
            prompt_history: Vec::new(),
            prompt_history_index: None,
            prompt_history_draft: None,
            streaming_text: String::new(),
            streaming_thinking: String::new(),
            thinking_level: ThinkingLevel::Off,
            is_streaming: false,
            animation_frame: 0,
            is_compacting: false,
            pending_user_echo: false,
            show_thinking: true,
            double_escape_action: DoubleEscapeAction::Tree,
            last_escape: None,
            last_ctrl_c: None,
            expand_tools: false,
            transcript_scroll: 0,
            transcript_page_rows: Cell::new(1),
            show_images: true,
            image_width_cells: 50,
            status: String::new(),
            composer_error: None,
            model: String::new(),
            cwd: String::new(),
            completions: CompletionState::default(),
            themes: ThemeManager::default(),
            keybindings: KeyBindingsManager::default(),
            cwd_path: PathBuf::new(),
            pending_attachments: Vec::new(),
            extension_ui: ExtensionUiAdapter::default(),
            extension_dialog: None,
            background_tx,
            completion_generation: 0,
            completion_query: None,
            completion_cancel: None,
            clipboard_read_busy: false,
            clipboard_write_busy: false,
            commands: BUILTIN_COMMANDS
                .iter()
                .map(|command| InteractiveCommand {
                    name: command.name.to_owned(),
                    description: command.description.to_owned(),
                    source: CommandSource::Builtin,
                })
                .collect(),
            panel: None,
            settings_panel: None,
            workflow_panel: None,
            settings_value_input: None,
            tree_panel: None,
            process_panel: None,
            agents_panel: None,
            scoped_models: None,
            session_selector: None,
            scoped_model_selector: None,
            todo_phases: Vec::new(),
            workflow_snapshots: Vec::new(),
            extension_working_message: None,
            extension_working_visible: false,
            extension_hidden_thinking_label: None,
            extension_title: None,
            goal_state: GoalState::default(),
            active_loops: std::collections::BTreeMap::new(),
            seen_irc_message_ids: std::collections::HashSet::new(),
        };
        state.editor.insert_char('/');
        state.editor.insert_char('m');
        state.editor.insert_char('d');
        state.refresh_completions();
        assert_eq!(state.completions.items[0].value, "/model");
        state.accept_completion();
        assert_eq!(state.editor.text(), "/model");
        assert!(state.completions.items.is_empty());
    }

    #[test]
    fn coordinate_completion_is_skill_only_and_consumes_every_printable_once() {
        assert!(
            visible_catalog()
                .iter()
                .all(|command| command.name != "coordinate" && command.name != "skill:coordinate"),
            "coordinate must not enter core command discovery"
        );

        let mut state = todo_test_state(Vec::new());
        state.commands = vec![InteractiveCommand {
            name: "skill:coordinate".to_owned(),
            description: "Coordinate work across agents".to_owned(),
            source: CommandSource::Skill,
        }];
        state.editor.insert_text("/coord");
        state.refresh_completions();
        assert!(
            !state
                .completions
                .items
                .iter()
                .any(|item| item.value == "/skill:coordinate"),
            "skills must not pollute core command completion"
        );

        state.editor.clear();
        state.completions.clear();
        apply_classified_burst(&mut state, "/");
        apply_classified_burst(&mut state, "skill:coordinate");
        assert_eq!(state.editor.text(), "/skill:coordinate");
        assert!(
            state
                .completions
                .selected()
                .is_some_and(|item| item.value == "/skill:coordinate"),
            "skill-prefixed completion must expose coordinate"
        );
        assert!(completion_already_matches_editor(&state));
        state.accept_completion();
        assert_eq!(state.editor.text(), "/skill:coordinate");
        assert!(state.completions.items.is_empty());
    }

    #[test]
    fn selector_panel_filters_and_returns_visible_selection() {
        let panel = SelectorPanel {
            title: "Models".to_owned(),
            help: String::new(),
            items: vec![
                PanelItem {
                    label: "openai/gpt".to_owned(),
                    description: "GPT".to_owned(),
                    value: PanelValue::SettingsTheme,
                    checked: false,
                },
                PanelItem {
                    label: "anthropic/claude".to_owned(),
                    description: "Claude".to_owned(),
                    value: PanelValue::SettingsThinking,
                    checked: false,
                },
            ],
            selected: 0,
            query: "acl".to_owned(),
        };
        assert_eq!(panel.visible_indices(), vec![1]);
        assert_eq!(
            panel.selected_item().map(|item| item.label.as_str()),
            Some("anthropic/claude")
        );
    }

    #[test]
    fn leading_bang_parses_foreground_bash_and_exclusion() {
        assert_eq!(
            parse_bash_input("! echo ok"),
            Some(("echo ok".to_owned(), false))
        );
        assert_eq!(
            parse_bash_input("!! printf hidden"),
            Some(("printf hidden".to_owned(), true))
        );
        assert_eq!(parse_bash_input("!   "), None);
        assert_eq!(parse_bash_input("plain message"), None);
    }

    #[test]
    fn queued_message_text_restores_only_user_text() {
        assert_eq!(
            message_text(Message::user_text("first", 1)),
            Some("first".to_owned())
        );
        assert_eq!(
            message_text(Message::BashExecution(pi_ai::BashExecutionMessage {
                command: "echo ignored".to_owned(),
                output: String::new(),
                exit_code: Some(0),
                cancelled: false,
                truncated: false,
                full_output_path: None,
                timestamp: 2,
                exclude_from_context: None,
            })),
            None
        );
    }

    #[test]
    fn assistant_markdown_matches_shared_neutral_output_for_rich_blocks() {
        let source = "# Heading\n\n1. ordered\n   - [x] nested\n\n| Name | Stat |\n| --- | ---: |\n| Tokyo | ✅ |\n\n[docs](https://example.test)\n\n```rust\nlet place = \"Tokyo\";\n```\n\n```mermaid\nflowchart LR\nA --> B\n```\n\n```mermaid\nsequenceDiagram\nA->>B: fallback\n```";
        let width = 40;
        let expected = pi_coding::markdown::render_markdown(
            source,
            &pi_coding::markdown::MarkdownRenderOptions {
                width: usize::from(width),
                ..pi_coding::markdown::MarkdownRenderOptions::default()
            },
        )
        .plain_lines();
        let entry = TranscriptEntry { kind: TranscriptKind::Assistant, content: vec![ContentBlock::text(source)], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false };
        let mut lines = Vec::new();
        render_transcript_entry(&mut lines, &entry, true, true, crate::theme::DARK, width);
        let rendered = lines[..lines.len() - 1]
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert_eq!(rendered, expected);
        assert!(rendered.iter().any(|line| line.starts_with("┌─ mermaid · flowchart")));
        assert!(rendered.iter().any(|line| line.contains("source fallback")));
        assert!(rendered.iter().all(|line| display_width(line) <= width));
        assert!(lines[0].spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert!(lines.iter().all(|line| {
            line.spans
                .iter()
                .all(|span| span.content.as_ref() != "Assistant")
        }));
    }

    #[test]
    fn screenshot_markdown_keeps_prose_default_and_cyan_sparse() {
        let source = "# Release notes\n\nOrdinary prose explains the change with **bold** and *italic* detail.\n\n- first item\n- second item\n\n[project path](https://example.test/src/lib.rs) and `cargo check`.\n\n```rust\nlet count: usize = parse(42);\n```";
        let lines = render_transcript_markdown(
            source,
            crate::theme::DARK,
            crate::theme::DARK.text,
            80,
            false,
        );
        let cyan = Some(crate::theme::DARK.md_heading);
        let plain = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(plain.contains("Ordinary prose explains the change"));
        let prose_spans = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| matches!(span.content.as_ref(), "Ordinary" | "prose" | "explains" | "change"));
        assert!(prose_spans.into_iter().all(|span| span.style.fg != cyan));
        let visible_spans = lines.iter().flat_map(|line| &line.spans).filter(|span| !span.content.trim().is_empty()).count();
        let cyan_spans = lines.iter().flat_map(|line| &line.spans).filter(|span| !span.content.trim().is_empty() && span.style.fg == cyan).count();
        assert!(cyan_spans.saturating_mul(3) < visible_spans, "cyan must remain a sparse high-salience accent");
    }

    #[test]
    fn streaming_assistant_matches_shared_tail_semantics_without_prefix_duplication() {
        let width = 32;
        let source = "# Stable\n\nmutable tail\n\n| Name | Stat |\n| --- | --- |\n| Tokyo | ✅ |";
        let rendered = render_transcript_markdown(
            source,
            crate::theme::DARK,
            crate::theme::DARK.text,
            width,
            true,
        );
        let plain = rendered
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let expected = pi_coding::markdown::render_markdown_streaming(
            source,
            &pi_coding::markdown::MarkdownRenderOptions {
                width: usize::from(width),
                ..pi_coding::markdown::MarkdownRenderOptions::default()
            },
        )
        .plain_lines();
        assert_eq!(plain, expected);
        assert_eq!(plain.iter().filter(|line| line.as_str() == "Stable").count(), 1);
        assert!(plain.iter().any(|line| line.contains("Tokyo")));
        assert!(plain.iter().all(|line| display_width(line) <= width));

        let finalized = render_transcript_markdown(
            source,
            crate::theme::DARK,
            crate::theme::DARK.text,
            width,
            false,
        );
        assert!(finalized.iter().any(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .starts_with('┌')
        }));
    }

    #[test]
    fn streaming_updates_replace_the_live_entry_without_repeating_frozen_blocks() {
        let width = 32;
        let mut state = todo_test_state(Vec::new());
        state.apply(ApplicationEvent::Agent(AgentEvent::AgentStart));
        for delta in ["# Stable\n\nmutable", " tail"] {
            let mut partial = pi_ai::AssistantMessage::pending(&Model::default());
            partial.content = vec![ContentBlock::text(format!(
                "{}{}",
                state.streaming_text, delta
            ))];
            state.apply(ApplicationEvent::Agent(AgentEvent::MessageUpdate {
                message: Message::Assistant(partial.clone()),
                assistant_message_event: AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: delta.to_owned(),
                    partial,
                },
            }));
            let rendered = render_transcript_markdown(
                &state.streaming_text,
                crate::theme::DARK,
                crate::theme::DARK.text,
                width,
                true,
            );
            let stable = rendered
                .iter()
                .filter(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>()
                        == "Stable"
                })
                .count();
            assert_eq!(stable, 1);
        }
        assert_eq!(state.streaming_text, "# Stable\n\nmutable tail");
        assert!(state.transcript.is_empty());
    }

    #[test]
    fn user_card_first_glyph_has_no_phantom_prefix_at_normal_and_narrow_widths() {
        let prompt = "Can you put it in the background?";
        let entry = TranscriptEntry { kind: TranscriptKind::User, content: vec![ContentBlock::text(prompt)], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false };

        for width in [80, 10] {
            let mut lines = Vec::new();
            render_transcript_entry(&mut lines, &entry, true, true, crate::theme::DARK, width);

            let card = &lines[..lines.len() - 1];
            let first_row = &card[0];
            let first_visible = first_row
                .spans
                .iter()
                .flat_map(|span| span.content.chars())
                .find(|character| !character.is_whitespace());
            assert_eq!(first_visible, Some('C'), "first user glyph at width {width}");
            assert!(
                first_row.spans.first().is_some_and(|span| span.content.starts_with('C')),
                "user content must begin at transcript column zero at width {width}: {first_row:?}"
            );
            assert!(card.iter().all(|line| {
                line.spans.iter().map(|span| display_width(span.content.as_ref())).sum::<u16>() == width
            }));
            assert!(card.iter().flat_map(|line| &line.spans).all(|span| {
                span.style.bg == Some(crate::theme::DARK.user_message_bg)
            }));
        }

        let unicode = TranscriptEntry { kind: TranscriptKind::User, content: vec![ContentBlock::text("🙂🙂🙂🙂🙂abc")], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false };
        let mut lines = Vec::new();
        render_transcript_entry(&mut lines, &unicode, true, true, crate::theme::DARK, 10);
        let plain = lines[..lines.len() - 1]
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>();
        assert_eq!(plain[0].trim_end(), "🙂🙂🙂🙂");
        assert_eq!(plain[1].trim_end(), "🙂abc");
    }

    #[test]
    fn compact_roles_hide_repeated_labels_and_empty_entries() {
        let assistant = TranscriptEntry { kind: TranscriptKind::Assistant, content: vec![ContentBlock::text("answer")], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false };
        let user = TranscriptEntry { kind: TranscriptKind::User, content: vec![ContentBlock::text("prompt")], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false };
        let empty = TranscriptEntry { kind: TranscriptKind::Assistant, content: vec![ContentBlock::text("  "), ContentBlock::thinking("hidden")], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false };
        let reasoning = TranscriptEntry { kind: TranscriptKind::Assistant, content: vec![ContentBlock::thinking("useful analysis"), ContentBlock::text("answer")], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false };

        let mut lines = Vec::new();
        render_transcript_entry(&mut lines, &user, true, true, crate::theme::DARK, 80);
        render_transcript_entry(&mut lines, &assistant, true, true, crate::theme::DARK, 80);
        let text = lines
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(!text.contains("You"));
        assert!(!text.contains("Assistant"));
        assert!(lines[0].spans[0].content.starts_with("prompt"));
        assert_eq!(lines[0].spans[0].style.bg, Some(crate::theme::DARK.user_message_bg));

        let before = lines.len();
        render_transcript_entry(&mut lines, &empty, false, true, crate::theme::DARK, 80);
        assert_eq!(lines.len(), before);

        let mut reasoning_lines = Vec::new();
        render_transcript_entry(&mut reasoning_lines, &reasoning, true, true, crate::theme::DARK, 80);
        let labels = reasoning_lines
            .iter()
            .flat_map(|line| &line.spans)
            .filter(|span| span.content.as_ref() == "thinking ·")
            .count();
        assert_eq!(labels, 1);
        assert!(reasoning_lines.iter().flat_map(|line| &line.spans).all(|span| {
            span.content.as_ref() != "Reasoning"
                && !span.content.contains("Reasoning hidden")
        }));
        let plain = reasoning_lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>();
        let thinking_row = plain.iter().position(|line| line == "useful analysis").unwrap();
        let answer_row = plain.iter().position(|line| line == "answer").unwrap();
        assert_eq!(answer_row, thinking_row + 2);
        assert_eq!(plain[thinking_row + 1], "");
    }

    #[test]
    fn system_and_custom_labels_are_compact_without_card_background() {
        for entry in [
            TranscriptEntry { kind: TranscriptKind::System, content: vec![ContentBlock::text("failure")], tool_name: Some("Error".to_owned()), tool_card: None, job_card: None, is_error: true, is_partial: false },
            TranscriptEntry { kind: TranscriptKind::Custom, content: vec![ContentBlock::text("notice")], tool_name: Some("release-note".to_owned()), tool_card: None, job_card: None, is_error: false, is_partial: false },
        ] {
            let mut lines = Vec::new();
            render_transcript_entry(&mut lines, &entry, true, true, crate::theme::DARK, 80);
            assert!(lines[0].spans[0].content.ends_with(" ·"));
            assert!(lines.iter().flat_map(|line| &line.spans).all(|span| {
                span.style.bg != Some(crate::theme::DARK.custom_message_bg)
            }));
        }
    }
    #[test]
    fn custom_transcript_uses_custom_theme_roles() {
        let entry = TranscriptEntry { kind: TranscriptKind::Custom, content: vec![ContentBlock::text("extension notice")], tool_name: Some("release-note".to_owned()), tool_card: None, job_card: None, is_error: false, is_partial: false };
        let mut lines = Vec::new();
        render_transcript_entry(&mut lines, &entry, true, true, crate::theme::DARK, 80);
        assert_eq!(lines[0].spans[0].content, "release-note ·");
        assert_eq!(
            lines[0].spans[0].style.fg,
            Some(crate::theme::DARK.custom_message_label)
        );
        assert!(
            lines
                .iter()
                .skip(1)
                .flat_map(|line| &line.spans)
                .any(|span| span.style.fg == Some(crate::theme::DARK.custom_message_text))
        );
        assert!(lines.iter().flat_map(|line| &line.spans).all(|span| {
            span.style.bg != Some(crate::theme::DARK.custom_message_bg)
        }));
    }

    #[test]
    fn tool_reducer_reconciles_ids_statuses_and_expanded_render() {
        use pi_agent::AgentToolResult;
        use pi_ai::{ToolResultMessage, now_millis};

        let mut state = todo_test_state(Vec::new());
        for event in [
            AgentEvent::ToolExecutionStart { tool_call_id: "a".to_owned(), tool_name: "read".to_owned(), arguments: serde_json::json!({"path": "a.rs"}) },
            AgentEvent::ToolExecutionStart { tool_call_id: "b".to_owned(), tool_name: "read".to_owned(), arguments: serde_json::json!({"path": "b.rs"}) },
            AgentEvent::ToolExecutionEnd { tool_call_id: "b".to_owned(), tool_name: "read".to_owned(), result: AgentToolResult::text("body-b"), is_error: false },
            AgentEvent::MessageEnd { message: Message::ToolResult(ToolResultMessage { tool_call_id: "b".to_owned(), tool_name: "read".to_owned(), content: vec![ContentBlock::text("body-b")], usage: None, details: None, added_tool_names: Vec::new(), is_error: false, timestamp: now_millis() }) },
            AgentEvent::ToolExecutionEnd { tool_call_id: "a".to_owned(), tool_name: "read".to_owned(), result: AgentToolResult::text("not found"), is_error: true },
        ] {
            state.apply(ApplicationEvent::Agent(event));
        }
        assert_eq!(state.transcript.iter().filter(|entry| entry.kind == TranscriptKind::Tool).count(), 2);
        assert_eq!(state.transcript.iter().filter(|entry| entry.tool_card.as_ref().is_some_and(|tool| tool.compact.tool_call_id == "b")).count(), 1);
        assert_eq!(state.transcript[0].tool_card.as_ref().unwrap().compact.status, ToolCallViewStatus::Failed);
        for entry in &state.transcript {
            let mut lines = Vec::new();
            render_transcript_entry(&mut lines, entry, true, false, crate::theme::DARK, 80);
            let rendered = lines.iter().map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect::<String>()).collect::<Vec<_>>();
            assert_eq!(rendered.iter().filter(|line| line.contains("Read")).count(), 1);
            assert!(!rendered.iter().any(|line| line.contains(" read")));
        }

        let mut bash = todo_test_state(Vec::new());
        let body = (1..=30).map(|line| line.to_string()).collect::<Vec<_>>().join("\n");
        bash.apply(ApplicationEvent::Agent(AgentEvent::ToolExecutionStart { tool_call_id: "bash".to_owned(), tool_name: "bash".to_owned(), arguments: serde_json::json!({"command": "seq 1 30"}) }));
        bash.apply(ApplicationEvent::Agent(AgentEvent::ToolExecutionEnd { tool_call_id: "bash".to_owned(), tool_name: "bash".to_owned(), result: AgentToolResult::text(body), is_error: false }));
        let entry = bash.transcript.last().unwrap();
        let mut compact = Vec::new();
        render_transcript_entry(&mut compact, entry, true, false, crate::theme::DARK, 80);
        let compact_lines = compact.iter().map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect::<String>()).collect::<Vec<_>>();
        let compact_text = compact_lines.concat();
        assert_eq!(compact_lines.iter().filter(|line| line.contains("Bash")).count(), 1);
        assert!(compact_text.contains("$ seq 1 30"));
        assert!(compact_text.contains("… 11 more lines ⟦Ctrl+O: Expand⟧"));
        assert!(!compact_text.to_ascii_lowercase().contains("bash done"));
        assert!(!compact_lines.iter().any(|line| line.contains("bash")));
        let mut expanded = Vec::new();
        render_transcript_entry(&mut expanded, entry, true, true, crate::theme::DARK, 80);
        let expanded_text = expanded.iter().flat_map(|line| &line.spans).map(|span| span.content.as_ref()).collect::<String>();
        assert!(expanded_text.contains("1"));
        assert!(!expanded_text.contains("Ctrl+O: Expand"));
        assert!(compact.iter().filter(|line| line.spans.iter().map(|span| span.content.as_ref()).collect::<String>().starts_with('╭')).count() == 1);

        for (name, arguments, output, expected) in [
            ("edit", serde_json::json!({"path": "src/lib.rs"}), "@@\n-old\n+new", "Edit"),
            ("write", serde_json::json!({"path": "src/lib.rs", "content": "new"}), "Successfully wrote", "Write"),
        ] {
            let mut tool = todo_test_state(Vec::new());
            tool.apply(ApplicationEvent::Agent(AgentEvent::ToolExecutionStart { tool_call_id: name.to_owned(), tool_name: name.to_owned(), arguments }));
            tool.apply(ApplicationEvent::Agent(AgentEvent::ToolExecutionEnd { tool_call_id: name.to_owned(), tool_name: name.to_owned(), result: AgentToolResult::text(output), is_error: false }));
            let mut rendered = Vec::new();
            render_transcript_entry(&mut rendered, tool.transcript.last().unwrap(), true, false, crate::theme::DARK, 80);
            let rendered = rendered.iter().map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect::<String>()).collect::<Vec<_>>();
            assert_eq!(rendered.iter().filter(|line| line.contains(expected)).count(), 1);
            assert!(!rendered.iter().any(|line| line.contains(&format!(" {name}"))));
        }

        let mut read = todo_test_state(Vec::new());
        read.apply(ApplicationEvent::Agent(AgentEvent::ToolExecutionStart {
            tool_call_id: "syntax-read".to_owned(),
            tool_name: "read".to_owned(),
            arguments: serde_json::json!({"path": "src/lib.rs"}),
        }));
        read.apply(ApplicationEvent::Agent(AgentEvent::ToolExecutionEnd {
            tool_call_id: "syntax-read".to_owned(),
            tool_name: "read".to_owned(),
            result: AgentToolResult::text("let count: usize = parse(42);"),
            is_error: false,
        }));
        let mut styled_read = Vec::new();
        render_transcript_entry(&mut styled_read, read.transcript.last().unwrap(), true, false, crate::theme::DARK, 80);
        let code_line = styled_read.iter().find(|line| line.spans.iter().any(|span| span.content == "let")).expect("read code line");
        let exact = code_line.spans.iter().skip(1).take(code_line.spans.len().saturating_sub(2)).map(|span| span.content.as_ref()).collect::<String>();
        assert!(exact.contains("let count: usize = parse(42);"));
        let colors = code_line.spans.iter().filter_map(|span| span.style.fg).collect::<HashSet<_>>();
        assert!(colors.len() >= 4, "read output must retain semantic syntax differentiation");
    }

    fn interaction(request: ExtensionUiRequest) -> ExtensionUiInteraction {
        ExtensionUiInteraction {
            id: "dialog".to_owned(),
            context: pi_coding::ExtensionUiContext {
                instance: pi_coding::ExtensionInstanceId {
                    extension_id: "demo".to_owned(),
                    generation: 3,
                },
                mode: pi_coding::ExtensionMode::Tui,
            },
            request,
        }
    }

    #[test]
    fn extension_dialog_select_returns_option_value_not_label() {
        let options = vec![UiSelectOption {
            value: "actual".to_owned(),
            label: "Shown".to_owned(),
            description: Some("Details".to_owned()),
        }];
        let dialog = ExtensionDialog::new(interaction(ExtensionUiRequest::Select {
            title: "Choose".to_owned(),
            options,
        }));
        match dialog.kind {
            ExtensionDialogKind::Select { options, selected } => {
                assert_eq!(selected, 0);
                assert_eq!(options[0].value, "actual");
                assert_eq!(options[0].label, "Shown");
            }
            _ => panic!("select dialog"),
        }
    }

    #[test]
    fn extension_dialog_confirm_and_input_keyboard_accept_cancel() {
        let mut confirm = ExtensionDialog::new(interaction(ExtensionUiRequest::Confirm {
            title: "Confirm".to_owned(),
            message: "Continue?".to_owned(),
        }));
        if let ExtensionDialogKind::Confirm { confirmed, .. } = &mut confirm.kind {
            *confirmed = false;
        } else {
            panic!("confirm dialog");
        }
        let mut input = ExtensionDialog::new(interaction(ExtensionUiRequest::Input {
            title: "Name".to_owned(),
            placeholder: Some("placeholder".to_owned()),
            value: Some("pre".to_owned()),
        }));
        if let ExtensionDialogKind::Input {
            placeholder,
            editor,
        } = &mut input.kind
        {
            assert_eq!(placeholder.as_deref(), Some("placeholder"));
            editor.insert_char('!');
            assert_eq!(editor.text(), "pre!");
        } else {
            panic!("input dialog");
        }
    }

    #[test]
    fn extension_dialog_editor_preserves_multiline_prefill() {
        let dialog = ExtensionDialog::new(interaction(ExtensionUiRequest::Editor {
            title: "Edit".to_owned(),
            prefill: Some("one\ntwo".to_owned()),
        }));
        match dialog.kind {
            ExtensionDialogKind::Editor { editor } => assert_eq!(editor.text(), "one\ntwo"),
            _ => panic!("editor dialog"),
        }
    }

    fn dialog_test_state(adapter: ExtensionUiAdapter) -> TuiState {
        let mut state = todo_test_state(Vec::new());
        state.extension_ui = adapter;
        state
    }

    fn tui_context(generation: u64) -> pi_coding::ExtensionUiContext {
        pi_coding::ExtensionUiContext {
            instance: pi_coding::ExtensionInstanceId { extension_id: "demo".to_owned(), generation },
            mode: pi_coding::ExtensionMode::Tui,
        }
    }

    async fn next_interaction(events: &mut tokio::sync::broadcast::Receiver<ExtensionUiEvent>) -> ExtensionUiEvent {
        tokio::time::timeout(std::time::Duration::from_secs(1), events.recv()).await.expect("extension event timeout").expect("extension event")
    }

    #[tokio::test]
    async fn extension_select_keyboard_returns_value_and_leaks_nothing() {
        use pi_coding::{ExtensionCancellation, ExtensionUiHost, ExtensionUiResponse};
        let adapter = ExtensionUiAdapter::new();
        let mut events = adapter.subscribe();
        let requester = adapter.clone();
        let task = tokio::spawn(async move { requester.request(tui_context(1), ExtensionUiRequest::Select { title: "Choose".to_owned(), options: vec![
            UiSelectOption { value: "first-value".to_owned(), label: "First label".to_owned(), description: None },
            UiSelectOption { value: "second-value".to_owned(), label: "Second label".to_owned(), description: Some("second description".to_owned()) },
        ]}, ExtensionCancellation::new()).await });
        let mut state = dialog_test_state(adapter.clone());
        state.apply_extension_ui(next_interaction(&mut events).await);
        assert!(handle_extension_dialog_key(&mut state, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
        assert!(handle_extension_dialog_key(&mut state, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        assert_eq!(task.await.unwrap().unwrap(), ExtensionUiResponse::Selected { value: Some("second-value".to_owned()) });
        assert!(adapter.pending_interactions().is_empty());
        assert!(state.extension_dialog.is_none());
    }

    #[tokio::test]
    async fn extension_confirm_escape_cancels() {
        use pi_coding::{ExtensionCancellation, ExtensionUiHost, ExtensionUiResponse};
        let adapter = ExtensionUiAdapter::new();
        let mut events = adapter.subscribe();
        let requester = adapter.clone();
        let task = tokio::spawn(async move { requester.request(tui_context(1), ExtensionUiRequest::Confirm { title: "Confirm".to_owned(), message: "Proceed?".to_owned() }, ExtensionCancellation::new()).await });
        let mut state = dialog_test_state(adapter.clone());
        state.apply_extension_ui(next_interaction(&mut events).await);
        handle_extension_dialog_key(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(task.await.unwrap().unwrap(), ExtensionUiResponse::Cancelled);
        assert!(adapter.pending_interactions().is_empty());
    }

    #[tokio::test]
    async fn extension_input_and_multiline_editor_accept_full_values() {
        use pi_coding::{ExtensionCancellation, ExtensionUiHost, ExtensionUiResponse};
        let adapter = ExtensionUiAdapter::new();
        let mut events = adapter.subscribe();
        let requester = adapter.clone();
        let input = tokio::spawn(async move { requester.request(tui_context(1), ExtensionUiRequest::Input { title: "Input".to_owned(), placeholder: Some("hint".to_owned()), value: Some("pre".to_owned()) }, ExtensionCancellation::new()).await });
        let mut state = dialog_test_state(adapter.clone());
        state.apply_extension_ui(next_interaction(&mut events).await);
        handle_extension_dialog_key(&mut state, KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE));
        handle_extension_dialog_key(&mut state, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(input.await.unwrap().unwrap(), ExtensionUiResponse::Input { value: Some("pre!".to_owned()) });

        let requester = adapter.clone();
        let editor = tokio::spawn(async move { requester.request(tui_context(1), ExtensionUiRequest::Editor { title: "Editor".to_owned(), prefill: Some("one".to_owned()) }, ExtensionCancellation::new()).await });
        state.apply_extension_ui(next_interaction(&mut events).await);
        handle_extension_dialog_key(&mut state, KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        handle_extension_dialog_key(&mut state, KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        handle_extension_dialog_key(&mut state, KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        handle_extension_dialog_key(&mut state, KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
        handle_extension_dialog_key(&mut state, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(editor.await.unwrap().unwrap(), ExtensionUiResponse::Edited { value: Some("one\ntwo".to_owned()) });
        assert!(adapter.pending_interactions().is_empty());
    }

    #[tokio::test]
    async fn extension_dialog_rejects_concurrent_request_and_reload_cancels_active() {
        use pi_coding::{ExtensionCancellation, ExtensionUiHost, ExtensionUiResponse};
        let adapter = ExtensionUiAdapter::new();
        let mut events = adapter.subscribe();
        let first_adapter = adapter.clone();
        let first = tokio::spawn(async move { first_adapter.request(tui_context(1), ExtensionUiRequest::Confirm { title: "First".to_owned(), message: "first".to_owned() }, ExtensionCancellation::new()).await });
        let mut state = dialog_test_state(adapter.clone());
        state.apply_extension_ui(next_interaction(&mut events).await);
        let second_adapter = adapter.clone();
        let second = tokio::spawn(async move { second_adapter.request(tui_context(1), ExtensionUiRequest::Input { title: "Second".to_owned(), placeholder: None, value: None }, ExtensionCancellation::new()).await });
        state.apply_extension_ui(next_interaction(&mut events).await);
        assert_eq!(second.await.unwrap().unwrap(), ExtensionUiResponse::Cancelled);
        assert_eq!(adapter.pending_interactions().len(), 1);
        adapter.clear_extension(tui_context(1).instance).await.unwrap();
        state.apply_extension_ui(next_interaction(&mut events).await);
        assert_eq!(first.await.unwrap().unwrap(), ExtensionUiResponse::Cancelled);
        assert!(state.extension_dialog.is_none());
        assert!(adapter.pending_interactions().is_empty());
    }

    #[tokio::test]
    async fn extension_set_editor_text_updates_main_editor_except_during_modal_edit() {
        use pi_coding::{ExtensionCancellation, ExtensionUiHost};
        let adapter = ExtensionUiAdapter::new();
        let mut events = adapter.subscribe();
        adapter.request(tui_context(1), ExtensionUiRequest::SetEditorText { text: "main".to_owned() }, ExtensionCancellation::new()).await.unwrap();
        let mut state = dialog_test_state(adapter.clone());
        state.apply_extension_ui(next_interaction(&mut events).await);
        assert_eq!(state.editor.text(), "main");
        state.extension_dialog = Some(ExtensionDialog::new(interaction(ExtensionUiRequest::Editor { title: "Edit".to_owned(), prefill: Some("modal".to_owned()) })));
        state.apply_extension_ui(ExtensionUiEvent::EditorTextChanged { instance: tui_context(1).instance, text: "ignored".to_owned() });
        assert_eq!(state.editor.text(), "main");
    }

    #[tokio::test]
    async fn extension_canonical_queries_and_setters_bind_tui_reducer_state() {
        use pi_coding::{ExtensionCancellation, ExtensionUiHost, ExtensionUiResponse};

        let adapter = ExtensionUiAdapter::new();
        let mut events = adapter.subscribe();
        let mut state = dialog_test_state(adapter.clone());
        state.editor.set_text("host-buffer");
        state.expand_tools = false;
        state.sync_extension_host_bindings();

        assert_eq!(
            adapter
                .request(
                    tui_context(1),
                    ExtensionUiRequest::GetEditorText,
                    ExtensionCancellation::new(),
                )
                .await
                .unwrap(),
            ExtensionUiResponse::EditorText {
                value: "host-buffer".to_owned()
            }
        );
        assert_eq!(
            adapter
                .request(
                    tui_context(1),
                    ExtensionUiRequest::GetToolsExpanded,
                    ExtensionCancellation::new(),
                )
                .await
                .unwrap(),
            ExtensionUiResponse::ToolsExpanded { expanded: false }
        );
        let themes = match adapter
            .request(
                tui_context(1),
                ExtensionUiRequest::GetAllThemes,
                ExtensionCancellation::new(),
            )
            .await
            .unwrap()
        {
            ExtensionUiResponse::Themes { themes } => themes,
            other => panic!("expected themes, got {other:?}"),
        };
        assert!(themes.iter().any(|theme| theme.name == "dark"));
        assert!(themes.iter().any(|theme| theme.name == "light"));

        adapter
            .request(
                tui_context(1),
                ExtensionUiRequest::SetEditorText {
                    text: "from-extension".to_owned(),
                },
                ExtensionCancellation::new(),
            )
            .await
            .unwrap();
        state.apply_extension_ui(next_interaction(&mut events).await);
        assert_eq!(state.editor.text(), "from-extension");

        adapter
            .request(
                tui_context(1),
                ExtensionUiRequest::SetToolsExpanded { expanded: true },
                ExtensionCancellation::new(),
            )
            .await
            .unwrap();
        state.apply_extension_ui(next_interaction(&mut events).await);
        assert!(state.expand_tools);

        adapter
            .request(
                tui_context(1),
                ExtensionUiRequest::SetWorkingMessage {
                    message: Some("building".to_owned()),
                },
                ExtensionCancellation::new(),
            )
            .await
            .unwrap();
        state.apply_extension_ui(next_interaction(&mut events).await);
        adapter
            .request(
                tui_context(1),
                ExtensionUiRequest::SetWorkingVisible { visible: true },
                ExtensionCancellation::new(),
            )
            .await
            .unwrap();
        state.apply_extension_ui(next_interaction(&mut events).await);
        assert_eq!(state.extension_working_message.as_deref(), Some("building"));
        assert!(state.extension_working_visible);
        assert_eq!(state.status, "building");

        adapter
            .request(
                tui_context(1),
                ExtensionUiRequest::SetHiddenThinkingLabel {
                    label: Some("quiet thinking".to_owned()),
                },
                ExtensionCancellation::new(),
            )
            .await
            .unwrap();
        state.apply_extension_ui(next_interaction(&mut events).await);
        assert_eq!(
            state.extension_hidden_thinking_label.as_deref(),
            Some("quiet thinking")
        );

        assert_eq!(
            adapter
                .request(
                    tui_context(1),
                    ExtensionUiRequest::SetTheme {
                        name: "light".to_owned(),
                    },
                    ExtensionCancellation::new(),
                )
                .await
                .unwrap(),
            ExtensionUiResponse::ThemeSet {
                success: true,
                error: None,
            }
        );
        state.apply_extension_ui(next_interaction(&mut events).await);
        assert_eq!(state.themes.active_name(), "light");

        assert_eq!(
            adapter
                .request(
                    tui_context(1),
                    ExtensionUiRequest::GetEditorText,
                    ExtensionCancellation::new(),
                )
                .await
                .unwrap(),
            ExtensionUiResponse::EditorText {
                value: "from-extension".to_owned()
            }
        );
        assert_eq!(
            adapter
                .request(
                    tui_context(1),
                    ExtensionUiRequest::GetToolsExpanded,
                    ExtensionCancellation::new(),
                )
                .await
                .unwrap(),
            ExtensionUiResponse::ToolsExpanded { expanded: true }
        );
    }

    #[test]
    fn typing_goal_character_by_character_consumes_each_key_once() {
        let mut state = todo_test_state(Vec::new());
        for character in "/goal".chars() {
            apply_classified_burst(&mut state, &character.to_string());
        }
        assert_eq!(state.editor.text(), "/goal");
        assert!(state.completions.selected().is_some_and(|item| item.value == "/goal"));

        // Exact accept must consume once without rewriting `/goal` into `/goall`.
        assert!(completion_already_matches_editor(&state));
        state.accept_completion();
        assert_eq!(state.editor.text(), "/goal");
        assert!(state.completions.items.is_empty());
        state.editor.insert_char('x');
        assert_eq!(state.editor.text(), "/goalx");
    }

    #[test]
    fn transcript_removes_only_inter_entry_blank_before_user() {
        let assistant = TranscriptEntry { kind: TranscriptKind::Assistant, content: vec![ContentBlock::thinking("reason"), ContentBlock::text("answer")], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false };
        let user = TranscriptEntry { kind: TranscriptKind::User, content: vec![ContentBlock::text("next prompt")], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false };
        let mut lines = Vec::new();
        render_transcript_entry(&mut lines, &assistant, true, true, crate::theme::DARK, 80);
        let answer = lines.iter().position(|line| line.spans.iter().map(|span| span.content.as_ref()).collect::<String>() == "answer").unwrap();
        assert_eq!(lines[answer - 1].spans.len(), 0, "thinking and answer retain one separator");
        trim_inter_entry_blank_before_user(&mut lines, &user);
        let before_user = lines.len();
        render_transcript_entry(&mut lines, &user, true, true, crate::theme::DARK, 80);
        assert_ne!(lines[before_user - 1].spans.len(), 0, "no full blank row precedes the user card");
        assert!(lines[before_user].spans[0].content.starts_with("next prompt"), "user content starts at transcript column zero");
    }

    #[tokio::test]
    async fn bare_goal_opens_intentional_choice_panel() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = pi_coding::Session::new(pi_coding::SessionOptions {
            model: Model::default(), cwd: cwd.path().to_path_buf(), system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off, api_key: String::new(), compaction: None,
            stream_options: Default::default(), tools: Some(Vec::new()), before_tool_call: None,
            after_tool_call: None, stream_fn: None, auth_resolver: None,
        }).expect("session");
        let application = Application::new(session).await;
        let mut state = todo_test_state(Vec::new());
        state.editor.set_text("/goal");
        state.open_goal_panel(&application);
        let panel = state.panel.as_ref().expect("goal panel");
        assert_eq!(panel.title, "Goal");
        assert!(panel.items.iter().any(|item| item.label == "Create goal"));
        assert!(panel.items.iter().any(|item| item.label == "Show details"));
        assert!(state.completions.items.is_empty());
        application.cleanup().await;
    }
    #[test]
    fn goal_panel_renders_padded_bounded_overlay() {
        use ratatui::backend::TestBackend;
        let panel = SelectorPanel {
            title: "Goal".to_owned(),
            help: "↑/↓ move · Enter select · Esc cancel".to_owned(),
            items: vec![PanelItem {
                label: "Show details".to_owned(),
                description: "active · 25/100 tokens · a deliberately long objective that wraps safely".to_owned(),
                value: PanelValue::GoalShow,
                checked: true,
            }],
            selected: 0,
            query: String::new(),
        };
        let backend = TestBackend::new(100, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_selector_panel(frame, &panel, crate::theme::DARK))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .filter(|row| !row.trim().is_empty())
            .collect::<Vec<_>>();
        assert!(rows.first().is_some_and(|row| row.contains(" Goal ")));
        assert!(rows.iter().any(|row| row.contains(" Show details")));
        assert!(rows.iter().all(|row| display_width(row) <= 100));
    }


    #[tokio::test]
    async fn goal_dispatch_starts_work_and_preserves_editor_on_usage_error() {
        let cwd = tempfile::tempdir().expect("cwd");
        let stream_fn: pi_agent::StreamFn = std::sync::Arc::new(move |model, _context, _options| {
            Box::pin(async move {
                let stream = pi_ai::new_assistant_message_event_stream();
                let producer = stream.clone();
                tokio::spawn(async move {
                    let mut message = pi_ai::AssistantMessage::pending(&model);
                    message.content.push(ContentBlock::text("done"));
                    message.stop_reason = pi_ai::StopReason::Stop;
                    producer.end(Some(message)).await;
                });
                stream
            })
        });
        let session = pi_coding::Session::new(pi_coding::SessionOptions {
            model: Model::default(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(stream_fn),
            auth_resolver: None,
        })
        .expect("session");
        let application = Application::new(session.clone()).await;
        let mut state = todo_test_state(Vec::new());
        state.editor.set_text("/goal create --tokens nope keep this");
        assert!(!dispatch_goal_command(
            &application,
            &mut state,
            Some("create --tokens nope keep this"),
        )
        .await);
        assert_eq!(state.editor.text(), "/goal create --tokens nope keep this");
        assert!(
            state
                .composer_error
                .as_deref()
                .is_some_and(|error| error.contains("positive integer")),
            "goal usage errors must remain in the composer toast"
        );
        assert!(
            state.transcript.is_empty(),
            "goal usage errors must never enter the transcript"
        );
        assert!(session.history().is_empty());

        assert!(dispatch_goal_command(
            &application,
            &mut state,
            Some("create --tokens 20 ship cleanly"),
        )
        .await);
        assert!(state.composer_error.is_none(), "accepted goal work clears the toast");
        assert!(state.status.starts_with("Goal work started · active · 0/20 tokens · ship cleanly"), "{}", state.status);
        assert_eq!(state.goal_state, application.goal_state());
        let details = state
            .transcript
            .iter()
            .rev()
            .find(|entry| {
                !entry.is_error
                    && content_text(&entry.content).contains("Objective: ship cleanly")
            })
            .map(|entry| content_text(&entry.content))
            .expect("create must push OMP-style goal details");
        assert!(details.contains("Status: active"), "{details}");
        assert!(details.contains("Tokens: 0 / 20"), "{details}");
        assert!(details.contains("Time spent:"), "{details}");

        assert!(
            dispatch_goal_command(&application, &mut state, Some("inspect")).await,
            "inspect must be read-only Show, not create"
        );
        assert_eq!(state.goal_state, application.goal_state());
        assert!(
            application.goal_state().current.is_some(),
            "inspect must not drop the active goal"
        );
        let inspect = state
            .transcript
            .last()
            .map(|entry| content_text(&entry.content))
            .expect("inspect details");
        assert!(inspect.contains("Objective: ship cleanly"), "{inspect}");
        application.wait_for_idle().await;
        assert!(session.history().iter().any(|message| matches!(message, Message::Assistant(_))), "goal create must start the agent");
        application.cleanup().await;
    }

    #[tokio::test]
    async fn goal_dispatch_reports_queued_work_while_busy() {
        let cwd = tempfile::tempdir().expect("cwd");
        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let release = CancellationToken::new();
        let stream_started = started.clone();
        let stream_release = release.clone();
        let stream_fn: pi_agent::StreamFn = std::sync::Arc::new(move |model, _context, _options| {
            let started = stream_started.clone();
            let release = stream_release.clone();
            Box::pin(async move {
                let stream = pi_ai::new_assistant_message_event_stream();
                let producer = stream.clone();
                tokio::spawn(async move {
                    started.notify_waiters();
                    release.cancelled().await;
                    let mut message = pi_ai::AssistantMessage::pending(&model);
                    message.content.push(ContentBlock::text("done"));
                    message.stop_reason = pi_ai::StopReason::Stop;
                    producer.end(Some(message)).await;
                });
                stream
            })
        });
        let session = pi_coding::Session::new(pi_coding::SessionOptions {
            model: Model::default(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(stream_fn),
            auth_resolver: None,
        })
        .expect("session");
        let application = Application::new(session).await;
        application
            .prompt("busy".to_owned(), Vec::new(), None)
            .await
            .expect("busy prompt");
        tokio::time::timeout(std::time::Duration::from_secs(2), started.notified())
            .await
            .expect("busy turn started");

        let mut state = todo_test_state(Vec::new());
        assert!(
            dispatch_goal_command(&application, &mut state, Some("create queued goal")).await
        );
        assert!(
            state.status.starts_with("Goal work queued · active · 0 tokens used · queued goal"),
            "{}",
            state.status
        );

        application.goal_pause().expect("cancel queued work");
        release.cancel();
        application.wait_for_idle().await;
        application.cleanup().await;
    }

    #[tokio::test]
    async fn bare_goal_is_optional_and_opens_choice_flow() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = pi_coding::Session::new(pi_coding::SessionOptions {
            model: Model::default(),
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
        })
        .expect("session");
        let application = Application::new(session.clone()).await;
        let mut state = todo_test_state(Vec::new());
        state.editor.set_text("/goal");

        assert!(
            !reject_missing_required_arguments(&mut state, "goal", None),
            "bare /goal must not be treated as required-arg"
        );
        state.open_goal_panel(&application);
        assert!(state.panel.as_ref().is_some_and(|panel| panel.title == "Goal"));
        assert!(session.history().is_empty());
        application.cleanup().await;
    }

    #[test]
    fn required_arg_guard_pushes_usage_and_clears_editor_completion() {
        let mut state = todo_test_state(Vec::new());
        state.editor.set_text("/import");
        state.completions.items = vec![CompletionItem {
            value: "/import".to_owned(),
            label: "/import".to_owned(),
            description: "import".to_owned(),
            is_directory: false,
        }];
        state.completions.context = Some(CompletionContext::Slash);
        state.completion_query = None;

        assert!(reject_missing_required_arguments(&mut state, "import", None));
        assert!(state.editor.is_empty(), "usage guard must clear the draft");
        assert!(
            state.completions.items.is_empty(),
            "usage guard must clear the completion popup"
        );
        assert_eq!(state.status, "Usage: /import <path.jsonl>");
        assert_eq!(
            state.composer_error.as_deref(),
            Some("Usage: /import <path.jsonl>")
        );
        assert!(
            state.transcript.is_empty(),
            "usage rejection must never enter the transcript"
        );
        let toast = composer_error_toast_lines(&state, 120, crate::theme::DARK)
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content.into_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(toast.contains("Usage: /import <path.jsonl>"), "{toast}");
        assert!(toast.contains(COMPOSER_ERROR_DISMISSAL_HINT), "{toast}");

        // Optional commands stay unguarded so bare /goal can dispatch Show.
        state.editor.set_text("/goal");
        assert!(!reject_missing_required_arguments(&mut state, "goal", None));
        assert_eq!(state.editor.text(), "/goal");
    }

    #[tokio::test]
    async fn extension_unsupported_canonical_queries_error_when_disabled() {
        use pi_coding::{ExtensionCancellation, ExtensionUiHost};

        let adapter = ExtensionUiAdapter::new();
        adapter.set_canonical_queries_supported(false);
        let mut state = dialog_test_state(adapter.clone());
        state.sync_extension_host_bindings();

        for request in [
            ExtensionUiRequest::GetEditorText,
            ExtensionUiRequest::GetAllThemes,
            ExtensionUiRequest::GetTheme {
                name: "dark".to_owned(),
            },
            ExtensionUiRequest::SetTheme {
                name: "dark".to_owned(),
            },
            ExtensionUiRequest::GetToolsExpanded,
        ] {
            let error = adapter
                .request(tui_context(1), request, ExtensionCancellation::new())
                .await
                .expect_err("disabled canonical path must fail closed");
            assert!(
                error.to_string().contains("canonical host state"),
                "{error:#}"
            );
        }
    }

    fn session_info_with_preview(preview: &str) -> pi_coding::SessionInfo {
        pi_coding::SessionInfo {
            path: PathBuf::from("/sessions/preview.jsonl"),
            id: "preview".to_owned(),
            cwd: PathBuf::from("/workspace"),
            timestamp: "2026-08-01T00:00:00Z".to_owned(),
            messages: 3,
            name: None,
            first_message: preview.to_owned(),
            all_messages_text: preview.to_owned(),
        }
    }

    #[test]
    fn saved_session_preview_preserves_logical_line_breaks() {
        let session = session_info_with_preview("first line\nsecond line\nthird line");
        assert_eq!(
            saved_session_preview_lines(&session, "•", false),
            vec![
                "• first line · 3 messages".to_owned(),
                "  second line".to_owned(),
                "  third line".to_owned(),
            ]
        );
    }

    #[test]
    fn effective_thinking_state_uses_hidden_override_and_effective_level() {
        let mut state = todo_test_state(Vec::new());
        state.thinking_level = ThinkingLevel::High;
        state.show_thinking = false;
        state.extension_hidden_thinking_label = Some("thinking quietly".to_owned());
        let effective = state.effective_thinking_state();
        assert_eq!(effective.level, ThinkingLevel::High);
        assert!(!effective.show_thinking);
        assert_eq!(effective.label, "thinking quietly");

        state.extension_hidden_thinking_label = None;
        assert_eq!(state.effective_thinking_state().label, "high hidden");
    }

    #[tokio::test]
    async fn transcript_replacement_resets_ledger_streaming_and_partial_state() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = pi_coding::Session::new(pi_coding::SessionOptions {
            model: Model::default(),
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
        })
        .expect("session");
        session
            .load_history(vec![Message::user_text("replacement", 1)])
            .await
            .expect("history");
        let application = Application::new(session).await;
        let mut state = todo_test_state(Vec::new());
        for index in 0..64 {
            state.push_message(Message::user_text(format!("old {index}"), index as i64));
        }
        state.committed_entries = state.transcript.len();
        state.transcript_scroll = 99;
        state.transcript_page_rows.set(42);
        state.streaming_text = "draft answer".to_owned();
        state.streaming_thinking = "draft reasoning".to_owned();
        state.is_streaming = true;
        state.pending_user_echo = true;

        state.replace_transcript_from_application(&application);

        assert_eq!(state.transcript.len(), 1);
        assert_eq!(state.committed_entries, 0);
        assert_eq!(state.transcript_scroll, 0);
        assert_eq!(state.transcript_page_rows.get(), 1);
        assert!(state.streaming_text.is_empty());
        assert!(state.streaming_thinking.is_empty());
        assert!(!state.is_streaming);
        assert!(!state.pending_user_echo);
        assert_eq!(state.settled_commit_batch().len(), 1);
        assert!(state.overflow_commit_batch(80, 20).is_empty());
        application.cleanup().await;
    }

    /// Contract: tree navigation, fork, keyboard-new, and resume-style session
    /// transitions call `replace_transcript_from_application`. That path must
    /// refresh `todo_phases` from the application so the Todo panel never shows
    /// a stale DAG after the underlying session changes.
    ///
    /// Regression: replace_transcript cleared conversation buffers but left
    /// `todo_phases` untouched, so a fork/new/tree switch kept the prior panel.
    #[tokio::test]
    async fn replace_transcript_refreshes_todo_phases_from_application() {
        use pi_coding::{TodoItem, TodoPhase, TodoStatus};

        let cwd = tempfile::tempdir().expect("cwd");
        let session = pi_coding::Session::new(pi_coding::SessionOptions {
            model: Model::default(),
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
        })
        .expect("session");
        // Canonical application has an empty todo list after a "new" session.
        let application = Application::new(session).await;
        assert!(
            application.todo_state().phases.is_empty(),
            "fresh application must start with empty todos"
        );

        // Display still shows a prior session's DAG (stale panel).
        let stale = vec![TodoPhase {
            name: "Stale".to_owned(),
            tasks: vec![TodoItem {
                id: "stale-1".to_owned(),
                content: "should not survive session replace".to_owned(),
                status: TodoStatus::InProgress,
                depends_on: Vec::new(),
                ready: true,
                blocked_by: Vec::new(),
            }],
        }];
        let mut state = todo_test_state(stale);
        assert_eq!(state.todo_phases.len(), 1);
        assert_eq!(state.todo_phases[0].name, "Stale");

        state.job_cards.apply_orchestration_event(&pi_coding::OrchestrationEvent::JobUpdated {
            group_id: "old-group".to_owned(),
            job: pi_coding::JobSnapshot {
                id: "old-job".to_owned(),
                agent_id: "OldAgent".to_owned(),
                agent: "task".to_owned(),
                parent_id: "Main".to_owned(),
                description: Some("old session subagent".to_owned()),
                todo_task_id: Some("stale-1".to_owned()),
                workflow_id: None,
                workflow_generation: None,
                status: pi_coding::JobStatus::Running,
                created_at: 1,
                started_at: Some(2),
                finished_at: None,
                result: None,
            },
        });
        assert_eq!(state.job_cards.cards_in_source_order().len(), 1);

        // Tree/fork/new path: replace transcript from the (empty-todo) application.
        state.replace_transcript_from_application(&application);
        assert!(
            state.job_cards.cards_in_source_order().is_empty(),
            "session replacement must clear old-session job cards"
        );

        assert!(
            state.todo_phases.is_empty(),
            "replace_transcript_from_application must refresh todo_phases from application \
             (got stale panel: {:?})",
            state.todo_phases
        );
        assert!(
            render_todo_panel_lines(
                &state.todo_phases,
                &state.job_cards.cards_in_source_order(),
                crate::theme::DARK,
                80,
            )
            .is_empty(),
            "empty replacement must leave no stale Todo panel rows"
        );

        // Subagents are an independent presentation domain: replacing a
        // session clears old job cards, while later orchestration events still
        // render even when the replacement Todo state is empty.
        state.job_cards.apply_orchestration_event(&pi_coding::OrchestrationEvent::JobUpdated {
            group_id: "replacement-group".to_owned(),
            job: pi_coding::JobSnapshot {
                id: "replacement-job".to_owned(),
                agent_id: "ReplacementAgent".to_owned(),
                agent: "task".to_owned(),
                parent_id: "Main".to_owned(),
                description: Some("independent subagent".to_owned()),
                todo_task_id: None,
                workflow_id: None,
                workflow_generation: None,
                status: pi_coding::JobStatus::Running,
                created_at: 1,
                started_at: Some(2),
                finished_at: None,
                result: None,
            },
        });
        let subagent_rows = render_todo_panel_lines(
            &state.todo_phases,
            &state.job_cards.cards_in_source_order(),
            crate::theme::DARK,
            80,
        );
        assert!(
            todo_line_texts(&subagent_rows)
                .iter()
                .any(|line| line.contains("independent subagent")),
            "Subagents must remain driven by orchestration job cards, not Todo phases"
        );
        state.job_cards.clear();

        // Symmetric case: application carries todos the display must pick up.
        application
            .set_todos(vec![TodoPhase {
                name: "Fresh".to_owned(),
                tasks: vec![TodoItem {
                    id: "fresh-1".to_owned(),
                    content: "visible after replace".to_owned(),
                    status: TodoStatus::Pending,
                    depends_on: Vec::new(),
                    ready: true,
                    blocked_by: Vec::new(),
                }],
            }])
            .expect("set_todos");
        // Simulate a display that was cleared or pointed at another session.
        state.todo_phases.clear();
        state.replace_transcript_from_application(&application);
        assert_eq!(
            state.todo_phases.len(),
            1,
            "replace must pull non-empty application todos into the panel"
        );
        assert_eq!(state.todo_phases[0].name, "Fresh");
        assert_eq!(state.todo_phases[0].tasks[0].content, "visible after replace");

        application.cleanup().await;
    }


    #[test]
    fn extension_snapshot_renders_title_status_widgets_and_editor() {
        use crate::extension_ui::{ExtensionStatusItem, ExtensionWidgetItem};
        use pi_coding::ExtensionInstanceId;

        let instance = ExtensionInstanceId {
            extension_id: "demo".to_owned(),
            generation: 1,
        };
        let snapshot = crate::extension_ui::ExtensionUiSnapshot {
            statuses: vec![ExtensionStatusItem {
                instance: instance.clone(),
                key: "state".to_owned(),
                text: "connected".to_owned(),
            }],
            widgets: vec![ExtensionWidgetItem {
                instance,
                key: "usage".to_owned(),
                lines: vec!["widget line".to_owned()],
                placement: UiWidgetPlacement::AboveEditor,
            }],
            notifications: Vec::new(),
            title: Some("Extension Title".to_owned()),
            editor_text: "extension editor".to_owned(),
            working_message: None,
            working_visible: false,
            working_indicator: None,
            hidden_thinking_label: None,
            themes: Vec::new(),
            active_theme: None,
            tools_expanded: false,
        };
        let widgets = extension_widget_lines(
            &snapshot,
            UiWidgetPlacement::AboveEditor,
            crate::theme::DARK,
        );
        assert_eq!(widgets.len(), 1);
        assert!(widgets[0].spans[0].content.contains("widget line"));
        assert_eq!(snapshot.title.as_deref(), Some("Extension Title"));
        assert_eq!(snapshot.statuses[0].text, "connected");
        assert_eq!(snapshot.editor_text, "extension editor");
    }

    #[test]
    fn wrapped_line_count_handles_wide_unicode_and_empty_rows() {
        let lines = vec![Line::raw("🙂🙂🙂"), Line::default()];
        assert_eq!(wrapped_line_count(&lines, 4), 3);
    }

    #[test]
    fn compact_arguments_truncates_on_character_boundaries() {
        let argument = "é".repeat(80);
        let compact = compact_arguments(&serde_json::json!({"path": argument}));
        assert!(compact.ends_with("..."));
        assert!(compact.is_char_boundary(compact.len()));
    }

    fn todo_test_state(phases: Vec<TodoPhase>) -> TuiState {
        let (background_tx, _background_rx) = mpsc::unbounded_channel();
        TuiState {
            tool_cards: ToolCardPresentationAdapter::new(),
            job_cards: JobCardPresentationAdapter::new(),
            transcript: Vec::new(),
            committed_entries: 0,
            editor: EditorState::new(),
            prompt_history: Vec::new(),
            prompt_history_index: None,
            prompt_history_draft: None,
            streaming_text: String::new(),
            streaming_thinking: String::new(),
            thinking_level: ThinkingLevel::Off,
            is_streaming: false,
            animation_frame: 0,
            is_compacting: false,
            pending_user_echo: false,
            show_thinking: true,
            double_escape_action: DoubleEscapeAction::Tree,
            last_escape: None,
            last_ctrl_c: None,
            expand_tools: false,
            transcript_scroll: 0,
            transcript_page_rows: Cell::new(1),
            show_images: true,
            image_width_cells: 50,
            status: String::new(),
            composer_error: None,
            model: String::new(),
            cwd: String::new(),
            completions: CompletionState::default(),
            themes: ThemeManager::default(),
            keybindings: KeyBindingsManager::default(),
            cwd_path: PathBuf::new(),
            pending_attachments: Vec::new(),
            extension_ui: ExtensionUiAdapter::default(),
            extension_dialog: None,
            background_tx,
            completion_generation: 0,
            completion_query: None,
            completion_cancel: None,
            clipboard_read_busy: false,
            clipboard_write_busy: false,
            commands: Vec::new(),
            panel: None,
            settings_panel: None,
            workflow_panel: None,
            settings_value_input: None,
            tree_panel: None,
            process_panel: None,
            agents_panel: None,
            scoped_models: None,
            session_selector: None,
            scoped_model_selector: None,
            todo_phases: phases,
            workflow_snapshots: Vec::new(),
            extension_working_message: None,
            extension_working_visible: false,
            extension_hidden_thinking_label: None,
            extension_title: None,
            goal_state: GoalState::default(),
            active_loops: std::collections::BTreeMap::new(),
            seen_irc_message_ids: std::collections::HashSet::new(),
        }
    }

    fn workflow_snapshot(generation: u64, status: pi_coding::WorkflowStatus) -> pi_coding::WorkflowSnapshot {
        pi_coding::WorkflowSnapshot {
            workflow_id: pi_coding::WorkflowId::new("workflow-generation"),
            name: "Generation gate".to_owned(),
            objective: "Ignore stale lifecycle events".to_owned(),
            status,
            created_at_ms: 1,
            updated_at_ms: 2,
            generation,
            todo: pi_coding::TodoState {
                phases: Vec::new(),
                storage: pi_coding::TodoStorage::Memory,
            },
            worktree_label: Some("workflow-generation".to_owned()),
            branch: Some("rpi/workflow/workflow-generation".to_owned()),
            supervisor_agent_id: None,
            supervisor_job_id: None,
            failure: None,
            integration: pi_coding::WorkflowIntegration::None,
        }
    }

    #[tokio::test]
    async fn stale_workflow_events_do_not_regress_tui_projection() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = pi_coding::Session::new(pi_coding::SessionOptions {
            model: Model::default(), cwd: cwd.path().to_path_buf(), system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off, api_key: String::new(), compaction: None,
            stream_options: Default::default(), tools: Some(Vec::new()), before_tool_call: None,
            after_tool_call: None, stream_fn: None, auth_resolver: None,
        }).expect("session");
        let application = Application::new(session).await;
        let mut state = todo_test_state(Vec::new());
        state.workflow_snapshots.push(WorkflowPanelSnapshot::from(&workflow_snapshot(
            2,
            pi_coding::WorkflowStatus::Running,
        )));

        state.apply_workflow_event(&application, pi_coding::WorkflowEvent::StatusChanged {
            workflow_id: pi_coding::WorkflowId::new("workflow-generation"),
            generation: 1,
            status: pi_coding::WorkflowStatus::Failed,
        });
        assert_eq!(state.workflow_snapshots[0].status, pi_coding::WorkflowStatus::Running);

        state.apply_workflow_event(&application, pi_coding::WorkflowEvent::Removed {
            workflow_id: pi_coding::WorkflowId::new("workflow-generation"),
            generation: 1,
        });
        assert_eq!(state.workflow_snapshots.len(), 1);

        state.apply_workflow_event(&application, pi_coding::WorkflowEvent::StatusChanged {
            workflow_id: pi_coding::WorkflowId::new("workflow-generation"),
            generation: 2,
            status: pi_coding::WorkflowStatus::Paused,
        });
        assert_eq!(state.workflow_snapshots[0].status, pi_coding::WorkflowStatus::Paused);
        application.cleanup().await;
    }

    #[test]
    fn job_events_update_one_inline_card_through_terminal_and_parked_states() {
        let mut state = todo_test_state(Vec::new());
        let mut job = pi_coding::JobSnapshot {
            id: "job-1".to_owned(),
            agent_id: "Child".to_owned(),
            agent: "task".to_owned(),
            parent_id: "Main".to_owned(),
            description: Some("inspect the workspace".to_owned()),
            todo_task_id: None,
            workflow_id: None,
            workflow_generation: None,
            status: pi_coding::JobStatus::Queued,
            created_at: 1_000,
            started_at: None,
            finished_at: None,
            result: None,
        };
        let event = |job| pi_coding::OrchestrationEvent::JobUpdated {
            group_id: "group".to_owned(),
            job,
        };

        state.apply(ApplicationEvent::Orchestration(event(job.clone())));
        job.status = pi_coding::JobStatus::Running;
        job.started_at = Some(1_100);
        state.apply(ApplicationEvent::Orchestration(event(job.clone())));
        state.apply(ApplicationEvent::Orchestration(
            pi_coding::OrchestrationEvent::AgentUpdated {
                group_id: "group".to_owned(),
                agent: pi_coding::AgentSnapshot {
                    id: "Child".to_owned(),
                    display_name: "task: inspect the workspace".to_owned(),
                    parent_id: Some("Main".to_owned()),
                    status: pi_coding::AgentStatus::Parked,
                    created_at: 1_000,
                    last_activity: 1_200,
                    unread: 0,
                    artifact_ref: None,
                    history_ref: None,
                },
            },
        ));
        job.status = pi_coding::JobStatus::Completed;
        job.finished_at = Some(2_100);
        let completed = event(job);
        state.apply(ApplicationEvent::Orchestration(completed.clone()));
        state.apply(ApplicationEvent::Orchestration(completed));

        let job_entries = state
            .transcript
            .iter()
            .filter(|entry| entry.kind == TranscriptKind::Job)
            .collect::<Vec<_>>();
        assert_eq!(job_entries.len(), 1, "terminal updates must replace by job id");
        assert!(!job_entries[0].is_partial);
        let card = job_entries[0].job_card.as_ref().expect("job card");
        assert_eq!(card.children[0].job_status, pi_coding::JobStatus::Completed);
        assert_eq!(card.children[0].agent_status, Some(pi_coding::AgentStatus::Parked));
        assert!(card.children[0].rows[0].text.contains("completed · parked"));

        let mut rendered = Vec::new();
        render_job_card(&mut rendered, card, crate::theme::DARK, 0, 80);
        let text = rendered
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("inspect the workspace"));
        assert!(text.contains("completed · parked"));
    }
    #[test]
    fn two_children_render_as_one_width_bounded_task_card() {
        let mut state = todo_test_state(Vec::new());
        state.apply(ApplicationEvent::Agent(AgentEvent::ToolExecutionStart {
            tool_call_id: "task-batch".to_owned(),
            tool_name: "task".to_owned(),
            arguments: serde_json::json!({
                "context": "# Goal\nShip delegation\n\n# Constraints\nKeep updates stable\n\n# Contract\nOne card",
                "tasks": [
                    {"name": "Alpha", "agent": "reviewer", "task": "Review adapter behavior"},
                    {"name": "Beta", "task": "Render narrow and wide layouts"}
                ]
            }),
        }));
        for (id, agent_id, status) in [
            ("job-alpha", "Alpha", pi_coding::JobStatus::Running),
            ("job-beta", "Beta", pi_coding::JobStatus::Queued),
        ] {
            state.apply(ApplicationEvent::Orchestration(pi_coding::OrchestrationEvent::JobUpdated {
                group_id: "group".to_owned(),
                job: pi_coding::JobSnapshot { id: id.to_owned(), agent_id: agent_id.to_owned(), agent: "task".to_owned(), parent_id: "Main".to_owned(), description: Some(format!("work for {agent_id}")), todo_task_id: None, workflow_id: None, workflow_generation: None, status, created_at: 1_000, started_at: None, finished_at: None, result: None },
            }));
        }
        let entries = state.transcript.iter().filter(|entry| entry.kind == TranscriptKind::Job).collect::<Vec<_>>();
        assert_eq!(entries.len(), 1);
        let card = entries[0].job_card.as_ref().expect("task card");
        assert_eq!(card.children.len(), 2);
        for width in [32, 96] {
            let mut rendered = Vec::new();
            render_job_card(&mut rendered, card, crate::theme::DARK, 0, width);
            assert!(rendered.iter().all(|line| line.width() <= usize::from(width)));
            let text = rendered.iter().flat_map(|line| &line.spans).map(|span| span.content.as_ref()).collect::<String>();
            assert!(text.contains("Task 2 agents"));
            assert!(text.contains("Goal"));
            assert!(text.contains("Alpha (reviewer)"));
            assert!(text.contains("Beta (task)"));
        }
    }

    #[test]
    fn todo_panel_renders_compact_active_summary() {
        use pi_coding::{TodoPhase, TodoStatus};
        let theme = crate::theme::DARK;
        let phases = vec![TodoPhase {
            name: "Plan".to_owned(),
            tasks: vec![
                todo_item("p1", "pending", TodoStatus::Pending, true, &[]),
                todo_item("p2", "active", TodoStatus::InProgress, true, &[]),
                todo_item("p3", "done", TodoStatus::Completed, false, &[]),
                todo_item("p4", "dropped", TodoStatus::Abandoned, false, &[]),
            ],
        }];
        let lines = render_todo_panel_lines(&phases, &[], theme, 80);
        let texts = todo_line_texts(&lines);
        assert_eq!(
            texts,
            vec![
                " Todos · 1 active · 1 next · 1/4",
                "  ► active",
                "  ○ pending",
                "",
            ]
        );
        assert_eq!(lines[0].spans[0].style.fg, Some(theme.accent));
        assert_eq!(lines[1].spans[0].style.fg, Some(theme.accent));
        assert!(lines[1].spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(lines[2].spans[0].style.fg, Some(theme.muted));
    }

    #[test]
    fn todo_panel_truncates_live_rows_but_human_output_keeps_dag_detail() {
        use pi_coding::{TodoBlockedReason, TodoItem, TodoPhase, TodoStatus};
        let theme = crate::theme::DARK;
        let phases = vec![
            TodoPhase {
                name: "Roots".to_owned(),
                tasks: vec![
                    todo_item("root-a", "fetch", TodoStatus::Pending, true, &[]),
                    todo_item("root-b", "compile", TodoStatus::InProgress, true, &[]),
                ],
            },
            TodoPhase {
                name: "Join".to_owned(),
                tasks: vec![TodoItem {
                    id: "join".to_owned(),
                    content: "ship a deliberately long release description".to_owned(),
                    status: TodoStatus::Pending,
                    depends_on: vec!["root-a".to_owned(), "root-b".to_owned()],
                    ready: false,
                    blocked_by: vec![
                        TodoBlockedReason { task_id: "root-a".to_owned(), content: "fetch".to_owned(), status: TodoStatus::Pending },
                        TodoBlockedReason { task_id: "root-b".to_owned(), content: "compile".to_owned(), status: TodoStatus::InProgress },
                    ],
                }],
            },
        ];
        let narrow = render_todo_panel_lines(&phases, &[], theme, 24);
        let texts = todo_line_texts(&narrow);
        assert!(texts.iter().all(|text| display_width(text) <= 24));
        assert!(texts[0].ends_with('…'));
        assert!(texts.iter().any(|text| text.contains("fetch")));
        assert!(texts.iter().any(|text| text.contains("compile")));
        assert!(texts.iter().all(|text| !text.contains("ship a deliberately")));

        let human = format_todo_human_lines(&phases);
        assert!(human.contains("fetch · ready"), "{human}");
        assert!(human.contains("compile · ready"), "{human}");
        assert!(human.contains("blocked by fetch(root-a), compile(root-b)"), "{human}");
    }


    #[test]
    fn todo_panel_renders_named_explicit_ownership_and_blank_sections() {
        let theme = crate::theme::DARK;
        let mut adapter = JobCardPresentationAdapter::new();
        for (id, agent_id, description, status, todo_task_id) in [
            ("job-secret-1", "StableScoutId", "map relevant files", pi_coding::JobStatus::Running, Some("owned")),
            ("job-secret-2", "StableWriterId", "apply focused edit", pi_coding::JobStatus::Queued, Some("missing")),
            ("job-secret-3", "StableUnownedId", "inspect without correlation", pi_coding::JobStatus::Running, None),
        ] {
            adapter.apply_orchestration_event(&pi_coding::OrchestrationEvent::JobUpdated {
                group_id: "group".to_owned(),
                job: pi_coding::JobSnapshot {
                    id: id.to_owned(),
                    agent_id: agent_id.to_owned(),
                    agent: "task".to_owned(),
                    parent_id: "Main".to_owned(),
                    description: Some(description.to_owned()),
                    todo_task_id: todo_task_id.map(str::to_owned),
                    workflow_id: None, workflow_generation: None, status,
                    created_at: 1_000,
                    started_at: (status == pi_coding::JobStatus::Running).then_some(1_100),
                    finished_at: None,
                    result: None,
                },
            });
        }
        for (id, display_name, status) in [
            ("StableScoutId", "Mira", pi_coding::AgentStatus::Running),
            ("StableWriterId", "Rowan", pi_coding::AgentStatus::Queued),
            ("StableUnownedId", "Sol", pi_coding::AgentStatus::Running),
        ] {
            adapter.apply_orchestration_event(&pi_coding::OrchestrationEvent::AgentUpdated {
                group_id: "group".to_owned(),
                agent: pi_coding::AgentSnapshot {
                    id: id.to_owned(),
                    display_name: display_name.to_owned(),
                    parent_id: Some("Main".to_owned()),
                    status,
                    created_at: 1_000,
                    last_activity: 1_100,
                    unread: 0,
                    artifact_ref: None,
                    history_ref: None,
                },
            });
        }

        let phases = vec![pi_coding::TodoPhase {
            name: "Work".to_owned(),
            tasks: vec![
                todo_item("owned", "implement correlation", TodoStatus::InProgress, true, &[]),
                todo_item("other", "preserve DAG truth", TodoStatus::Pending, true, &[]),
            ],
        }];
        let lines = render_todo_panel_lines(&phases, &adapter.cards_in_source_order(), theme, 80);
        let texts = todo_line_texts(&lines);
        let divider = texts.iter().position(String::is_empty).expect("section divider");
        assert!(texts[1].contains("implement correlation"));
        assert!(!texts[1].contains("owner:"));
        assert!(texts[2].contains("preserve DAG truth"));
        assert_eq!(texts[divider + 1], " waiting on 3 jobs");
        assert!(texts[divider + 2].contains("Mira · running · implement correlation"));
        assert!(!texts[divider + 2].contains("task:"));
        assert!(!texts[divider + 2].contains("map relevant files"));
        assert!(texts[divider + 3].contains("Rowan · queued · apply focused edit"));
        assert!(texts[divider + 4].contains("Sol · running · inspect without correlation"));
        assert_eq!(texts.last().map(String::as_str), Some(""));
        let rendered = texts.join("\n");
        assert!(!rendered.contains("StableScoutId"));
        assert!(!rendered.contains("StableWriterId"));
        assert!(!rendered.contains("job-secret"));
        assert_eq!(lines[divider + 2].spans[0].style.fg, Some(theme.accent));
        assert_eq!(lines[divider + 3].spans[0].style.fg, Some(theme.dim));
        use ratatui::backend::TestBackend;
        let mut state = todo_test_state(phases);
        state.job_cards = adapter;
        state.editor.set_text("compose here");
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut images = TerminalImageRenderer::default();
        terminal.draw(|frame| { let _ = render(frame, &state, &mut images); }).unwrap();
        let rows = terminal.backend().buffer().content.chunks(80)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>();
        let jobs_row = rows.iter().position(|row| row.contains("waiting on 3 jobs")).expect("jobs row");
        assert!(rows[jobs_row - 1].trim().is_empty());
        let composer_row = rows.iter().position(|row| row.starts_with("╭── π")).expect("composer row");
        assert!(rows[composer_row - 1].trim().is_empty());
    }

    #[test]
    fn todo_hud_bounds_dense_active_work_like_omp() {
        let tasks = (0..12)
            .map(|index| {
                todo_item(
                    &format!("task-{index}"),
                    &format!("active task {index}"),
                    TodoStatus::InProgress,
                    true,
                    &[],
                )
            })
            .collect::<Vec<_>>();
        let phases = vec![TodoPhase {
            name: "Delivery".to_owned(),
            tasks,
        }];
        let lines = render_todo_panel_lines(&phases, &[], crate::theme::DARK, 80);
        let text = todo_line_texts(&lines);
        assert_eq!(text.iter().filter(|line| line.contains("active task")).count(), TODO_HUD_TASK_LIMIT);
        assert!(text.iter().any(|line| line.contains("7 more active todos")));
    }
    #[test]
    fn subagents_only_state_renders_above_composer_without_raw_ids() {
        use ratatui::backend::TestBackend;

        let mut state = todo_test_state(Vec::new());
        for event in [
            pi_coding::OrchestrationEvent::JobUpdated {
                group_id: "group".to_owned(),
                job: pi_coding::JobSnapshot {
                    id: "01999999-secret-job-uuid".to_owned(),
                    agent_id: "opaque-agent-id".to_owned(),
                    agent: "reviewer".to_owned(),
                    parent_id: "Main".to_owned(),
                    description: Some("inspect the live state".to_owned()),
                    todo_task_id: None,
                    workflow_id: None,
                    workflow_generation: None,
                    status: pi_coding::JobStatus::Running,
                    created_at: 1_000,
                    started_at: Some(1_100),
                    finished_at: None,
                    result: None,
                },
            },
            pi_coding::OrchestrationEvent::AgentUpdated {
                group_id: "group".to_owned(),
                agent: pi_coding::AgentSnapshot {
                    id: "opaque-agent-id".to_owned(),
                    display_name: "reviewer".to_owned(),
                    parent_id: Some("Main".to_owned()),
                    status: pi_coding::AgentStatus::Running,
                    created_at: 1_000,
                    last_activity: 1_100,
                    unread: 0,
                    artifact_ref: None,
                    history_ref: None,
                },
            },
        ] {
            state.apply(ApplicationEvent::Orchestration(event));
        }
        state.editor.set_text("compose here");

        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut images = TerminalImageRenderer::default();
        terminal
            .draw(|frame| {
                let _ = render(frame, &state, &mut images);
            })
            .unwrap();
        let rows = terminal
            .backend()
            .buffer()
            .content
            .chunks(80)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>();
        assert!(rows.iter().all(|row| !row.contains("Todos ·")), "empty Todo must not render");
        let jobs_row = rows
            .iter()
            .position(|row| row.contains("waiting on 1 jobs"))
            .expect("jobs-only heading");
        assert!(rows[jobs_row + 1].contains("reviewer · running"));
        let composer_row = rows
            .iter()
            .position(|row| row.starts_with("╭── π"))
            .expect("composer row");
        assert!(jobs_row < composer_row);
        assert!(rows[composer_row - 1].trim().is_empty(), "panel and composer need a blank row");
        let rendered = rows.join("\n");
        assert!(!rendered.contains("01999999-secret-job-uuid"));
        assert!(!rendered.contains("opaque-agent-id"));
    }

    #[test]
    fn long_tool_and_active_jobs_keep_composer_visible_at_screenshot_dimensions() {
        use pi_agent::AgentToolResult;
        use ratatui::backend::{Backend, TestBackend};

        for (width, height) in [(196, 28), (80, 48)] {
            let tasks = (0..14)
                .map(|index| todo_item(&format!("task-{index}"), &format!("active task {index} with a long bounded description"), if index == 0 { TodoStatus::InProgress } else { TodoStatus::Pending }, true, &[]))
                .collect::<Vec<_>>();
            let phases = vec![TodoPhase { name: "Delivery".to_owned(), tasks }];
            let mut state = todo_test_state(phases.clone());
            state.model = "faux/faux-1".to_owned();
            state.cwd = "/tmp/portrait-project".to_owned();
            state.editor.set_text("draft prompt remains visible");
            state.expand_tools = true;
            let output = (0..160).map(|line| format!("oversized tool output row {line}"))
                .collect::<Vec<_>>().join("\n");
            state.apply(ApplicationEvent::Agent(AgentEvent::ToolExecutionStart {
                tool_call_id: "long-tool".to_owned(),
                tool_name: "bash".to_owned(),
                arguments: serde_json::json!({"command": "emit long output"}),
            }));
            state.apply(ApplicationEvent::Agent(AgentEvent::ToolExecutionEnd {
                tool_call_id: "long-tool".to_owned(),
                tool_name: "bash".to_owned(),
                result: AgentToolResult::text(output),
                is_error: false,
            }));
            for index in 0..6 {
                state.apply(ApplicationEvent::Orchestration(pi_coding::OrchestrationEvent::JobUpdated {
                    group_id: "group".to_owned(),
                    job: pi_coding::JobSnapshot {
                        id: format!("job-{index}"),
                        agent_id: format!("agent-{index}"),
                        agent: "task".to_owned(),
                        parent_id: "Main".to_owned(),
                        description: Some(format!("active assignment {index} with details that must truncate")),
                        todo_task_id: Some(format!("task-{index}")),
                        workflow_id: None,
                        workflow_generation: None,
                        status: pi_coding::JobStatus::Running,
                        created_at: 1,
                        started_at: Some(2),
                        finished_at: None,
                        result: None,
                    },
                }));
            }

            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut images = TerminalImageRenderer::default();
            for _ in 0..3 {
                state.apply(ApplicationEvent::TodoUpdated {
                    phases: phases.clone(),
                    completed_tasks: Vec::new(),
                });
                terminal.draw(|frame| { let _ = render(frame, &state, &mut images); }).unwrap();
                let rows = terminal.backend().buffer().content.chunks(usize::from(width))
                    .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
                    .collect::<Vec<_>>();
                let top = rows.iter().position(|row| row.starts_with("╭── π")).expect("composer top border");
                let bottom = rows.iter().position(|row| row.starts_with("╰─ draft prompt remains visible")).expect("composer prompt and bottom border");
                assert!(top < bottom && bottom < usize::from(height));
                assert!(rows[top].contains('π'));
                assert!(rows[bottom].ends_with('╯'));
                let cursor = terminal.backend_mut().get_cursor_position().unwrap();
                assert_eq!(usize::from(cursor.y), bottom);
                assert!(cursor.x < width);
            }
        }
    }

    fn todo_item(
        id: &str,
        content: &str,
        status: TodoStatus,
        ready: bool,
        blocked_by: &[(&str, &str, TodoStatus)],
    ) -> TodoItem {
        use pi_coding::TodoBlockedReason;
        TodoItem {
            id: id.to_owned(),
            content: content.to_owned(),
            status,
            depends_on: blocked_by
                .iter()
                .map(|(dep_id, _, _)| (*dep_id).to_owned())
                .collect(),
            ready,
            blocked_by: blocked_by
                .iter()
                .map(|(dep_id, dep_content, dep_status)| TodoBlockedReason {
                    task_id: (*dep_id).to_owned(),
                    content: (*dep_content).to_owned(),
                    status: *dep_status,
                })
                .collect(),
        }
    }

    fn todo_line_texts(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn todo_open_count_excludes_terminal_statuses() {
        use pi_coding::{TodoPhase, TodoStatus};
        let phases = vec![TodoPhase {
            name: "P".to_owned(),
            tasks: vec![
                todo_item("a", "a", TodoStatus::Pending, true, &[]),
                todo_item("b", "b", TodoStatus::InProgress, true, &[]),
                todo_item("c", "c", TodoStatus::Completed, false, &[]),
                todo_item("d", "d", TodoStatus::Abandoned, false, &[]),
            ],
        }];
        assert_eq!(todo_open_count(&phases), 2);
    }


    #[test]
    fn todo_updated_and_reminder_refresh_display_only() {
        use pi_coding::{TodoCompletionTransition, TodoPhase, TodoStatus};
        let phases = vec![TodoPhase {
            name: "Plan".to_owned(),
            tasks: vec![todo_item("x", "x", TodoStatus::InProgress, true, &[])],
        }];
        let mut state = todo_test_state(Vec::new());
        assert!(state.todo_phases.is_empty());
        state.apply(ApplicationEvent::TodoUpdated {
            phases: phases.clone(),
            completed_tasks: Vec::<TodoCompletionTransition>::new(),
        });
        assert_eq!(state.todo_phases.len(), 1);
        // A reminder republishes phases and surfaces a status, without
        // invoking any canonical mutation from the display side.
        state.apply(ApplicationEvent::TodoReminder { phases });
        assert_eq!(state.todo_phases.len(), 1);
        assert!(state.status.contains("Todo reminder"));
    }

    #[test]
    fn user_prompt_echo_renders_exactly_one_you_entry() {
        // One submitted user prompt must produce exactly one "You" transcript
        // entry. The immediate "You" echo (editor submit / startup positional
        // prompt) and the agent's `MessageEnd` for the persisted
        // `Message::User` must reconcile into a single row, not duplicate.

        // Case 1: immediate display, then the canonical MessageEnd. The
        // MessageEnd must replace the immediate-display slot (carrying the
        // persisted image block) instead of appending a second "You" row.
        let mut state = todo_test_state(Vec::new());
        state.push_lines("You", "hello".to_owned(), Color::Reset);
        assert_eq!(state.transcript.len(), 1);
        assert_eq!(state.transcript[0].kind, TranscriptKind::User);
        assert!(state.pending_user_echo);
        let canonical = Message::User(pi_ai::UserMessage {
            content: vec![
                ContentBlock::text("hello"),
                ContentBlock::Image {
                    data: "aW1n".to_owned(),
                    mime_type: "image/png".to_owned(),
                },
            ],
            timestamp: 1,
        });
        state.apply(ApplicationEvent::Agent(AgentEvent::MessageEnd {
            message: canonical,
        }));
        assert!(!state.pending_user_echo);
        assert_eq!(
            state
                .transcript
                .iter()
                .filter(|entry| entry.kind == TranscriptKind::User)
                .count(),
            1,
            "immediate-display path must keep exactly one You entry"
        );
        let entry = &state.transcript[0];
        assert_eq!(entry.kind, TranscriptKind::User);
        // Reconciled to the canonical persisted content, so the image block
        // survives (proving replacement rather than a bare skip).
        assert_eq!(entry.content.len(), 2);
        assert!(matches!(entry.content[1], ContentBlock::Image { .. }));

        // Case 2: scheduled loop turns arrive as typed hidden custom messages.
        // The TUI projects one visible Loop/System card and never a You row or
        // the internal model wrapper.
        let mut state = todo_test_state(Vec::new());
        assert!(!state.pending_user_echo);
        state.apply(ApplicationEvent::Agent(AgentEvent::MessageEnd {
            message: Message::Custom(pi_ai::CustomMessage {
                custom_type: "loop_scheduled_turn".to_owned(),
                content: "<system-reminder>internal</system-reminder>\n\nloop prompt".into(),
                display: false,
                details: Some(serde_json::json!({
                    "taskId": "abc123",
                    "prompt": "loop prompt",
                    "schedule": "every 3 seconds",
                })),
                timestamp: 2,
            }),
        }));
        assert_eq!(state.transcript.len(), 1);
        let loop_entry = &state.transcript[0];
        assert_eq!(loop_entry.kind, TranscriptKind::System);
        assert_eq!(loop_entry.tool_name.as_deref(), Some("Loop abc123 · every 3 seconds"));
        assert_eq!(content_text(&loop_entry.content), "loop prompt");
        assert!(!content_text(&loop_entry.content).contains("system-reminder"));
        assert!(!state.transcript.iter().any(|entry| entry.kind == TranscriptKind::User));
        state.push_message(Message::Custom(pi_ai::CustomMessage {
            custom_type: "loop_scheduled_turn".to_owned(),
            content: "<system-reminder>internal second</system-reminder>\n\nloop prompt".into(),
            display: false,
            details: Some(serde_json::json!({
                "taskId": "abc123",
                "prompt": "loop prompt",
                "schedule": "every 3 seconds",
            })),
            timestamp: 3,
        }));
        assert_eq!(state.transcript.len(), 2, "one public card per persisted run");
        assert!(state.transcript.iter().all(|entry| entry.kind == TranscriptKind::System));
    }

    fn orchestration_custom(
        id: &str,
        from: &str,
        to: &str,
        body: &str,
        reply_to: Option<&str>,
    ) -> Message {
        let xml = format!(
            "<orchestration-message id=\"{id}\" from=\"{from}\">\n{body}\n</orchestration-message>"
        );
        Message::Custom(pi_ai::CustomMessage {
            custom_type: pi_coding::ORCHESTRATION_MESSAGE_TYPE.to_owned(),
            content: xml.into(),
            display: true,
            details: Some(serde_json::json!({
                "id": id,
                "from": from,
                "to": to,
                "body": body,
                "replyTo": reply_to,
            })),
            timestamp: 1,
        })
    }

    #[test]
    fn orchestration_irc_renders_named_label_body_reply_and_no_raw_xml() {
        let mut state = todo_test_state(Vec::new());
        state.job_cards.apply_orchestration_event(&pi_coding::OrchestrationEvent::AgentUpdated {
            group_id: "group".to_owned(),
            agent: pi_coding::AgentSnapshot {
                id: "Scout".to_owned(),
                display_name: "task: scout workspace".to_owned(),
                parent_id: Some("Main".to_owned()),
                status: pi_coding::AgentStatus::Running,
                created_at: 1,
                last_activity: 1,
                unread: 0,
                artifact_ref: None,
                history_ref: None,
            },
        });

        // Main → named child
        state.apply(ApplicationEvent::Agent(AgentEvent::MessageEnd {
            message: orchestration_custom("m1", "Main", "Scout", "please inspect src", None),
        }));
        // child → child with reply metadata
        state.apply(ApplicationEvent::Agent(AgentEvent::MessageEnd {
            message: orchestration_custom(
                "m2",
                "Scout",
                "Main",
                "found three crates",
                Some("m1"),
            ),
        }));

        assert_eq!(state.transcript.len(), 2);
        let first = &state.transcript[0];
        assert_eq!(first.kind, TranscriptKind::Custom);
        assert_eq!(
            first.tool_name.as_deref(),
            Some("IRC · Main → task: scout workspace")
        );
        assert_eq!(content_text(&first.content), "please inspect src");
        assert!(!content_text(&first.content).contains("orchestration-message"));

        let second = &state.transcript[1];
        assert_eq!(
            second.tool_name.as_deref(),
            Some("IRC · task: scout workspace → Main")
        );
        let second_text = content_text(&second.content);
        assert!(second_text.contains("found three crates"));
        assert!(second_text.contains("reply to m1"));
        assert!(!second_text.contains("<orchestration-message"));
        assert!(!second_text.contains("Replying to message"));

        // Body on its own row beneath the label.
        let mut lines = Vec::new();
        render_transcript_entry(&mut lines, second, true, true, crate::theme::DARK, 80);
        let texts: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(texts[0].starts_with("IRC · task: scout workspace → Main"));
        assert!(texts.iter().any(|line| line.contains("found three crates")));
        assert!(texts.iter().any(|line| line.contains("reply to m1")));
        let body_idx = texts.iter().position(|line| line.contains("found three crates")).unwrap();
        let reply_idx = texts.iter().position(|line| line.contains("reply to m1")).unwrap();
        assert!(body_idx > 0, "body must be beneath the IRC label");
        assert!(reply_idx > body_idx, "reply metadata follows body");
        assert!(
            lines[reply_idx]
                .spans
                .iter()
                .any(|span| span.style.fg == Some(crate::theme::DARK.muted)),
            "reply metadata uses muted style"
        );
    }

    #[test]
    fn message_delivered_event_renders_once_and_dedupes_custom_message() {
        let mut state = todo_test_state(Vec::new());
        let message = pi_coding::MailboxMessage {
            id: "live-1".to_owned(),
            from: "Child".to_owned(),
            to: "Main".to_owned(),
            body: "status from child".to_owned(),
            timestamp: 10,
            reply_to: None,
        };
        state.apply(ApplicationEvent::Orchestration(
            pi_coding::OrchestrationEvent::MessageDelivered {
                group_id: "group".to_owned(),
                message: message.clone(),
            },
        ));
        // Same id arriving again as session CustomMessage must not duplicate.
        state.apply(ApplicationEvent::Agent(AgentEvent::MessageEnd {
            message: orchestration_custom("live-1", "Child", "Main", "status from child", None),
        }));
        assert_eq!(state.transcript.len(), 1);
        assert_eq!(
            state.transcript[0].tool_name.as_deref(),
            Some("IRC · Child → Main")
        );
        assert_eq!(content_text(&state.transcript[0].content), "status from child");
        assert!(!content_text(&state.transcript[0].content).contains("orchestration-message"));
    }


    #[test]
    fn settled_entries_commit_once_and_partial_rows_wait() {
        let mut state = todo_test_state(Vec::new());
        state.push_entry(TranscriptEntry { kind: TranscriptKind::User, content: vec![ContentBlock::text("prompt")], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false });
        state.push_entry(TranscriptEntry { kind: TranscriptKind::Assistant, content: vec![ContentBlock::text("answer")], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false });
        state.apply(ApplicationEvent::Agent(AgentEvent::ToolExecutionStart {
            tool_call_id: "partial-read".to_owned(),
            tool_name: "read".to_owned(),
            arguments: serde_json::json!({"path": "a"}),
        }));

        assert!(state.overflow_commit_batch(80, 20).is_empty());
        let overflow = state.overflow_commit_batch(80, 1);
        assert_eq!(overflow.len(), 2);
        assert_eq!(overflow[0].kind, TranscriptKind::User);
        assert_eq!(overflow[1].kind, TranscriptKind::Assistant);
        state.finish_commit(overflow.len());
        assert_eq!(state.committed_entries, 2);
        assert!(state.overflow_commit_batch(80, 20).is_empty());
    }

    #[test]
    fn finalized_user_and_assistant_stay_visible_across_draw_ticks_and_resize() {
        let mut state = todo_test_state(Vec::new());
        state.push_lines("You", "hello".to_owned(), Color::Reset);
        state.apply(ApplicationEvent::Agent(AgentEvent::MessageEnd {
            message: Message::user_text("hello", 1),
        }));
        state.apply(ApplicationEvent::Agent(AgentEvent::MessageEnd {
            message: {
                let mut message = pi_ai::AssistantMessage::pending(&Model::default());
                message.content = vec![ContentBlock::text("world")];
                message.stop_reason = pi_ai::StopReason::Stop;
                Message::Assistant(message)
            },
        }));

        assert!(state.overflow_commit_batch(163, 20).is_empty());
        assert!(state.overflow_commit_batch(163, 20).is_empty());
        assert!(state.overflow_commit_batch(90, 20).is_empty());
        assert!(state.overflow_commit_batch(163, 20).is_empty());

        assert_eq!(state.committed_entries, 0);

        let overflow = state.overflow_commit_batch(90, 1);
        assert_eq!(overflow.len(), 1);
        assert_eq!(overflow[0].kind, TranscriptKind::User);
        state.finish_commit(overflow.len());
        assert_eq!(state.committed_entries, 1);
        assert!(state.overflow_commit_batch(163, 20).is_empty());
    }

    #[test]
    fn idle_theme_tick_is_a_noop_without_binding_sync_or_redraw() {
        let mut state = todo_test_state(Vec::new());
        state.editor.set_text(&"x".repeat(7_608));
        assert!(!state.poll_theme_reload());
        assert!(!state.reconcile_extension_dialog());
        assert_eq!(state.editor.text().len(), 7_608);
    }

    #[test]
    fn live_viewport_is_compact_without_live_transcript_and_expands_for_overlays() {
        let mut state = todo_test_state(Vec::new());
        state.model = "faux/faux-1".to_owned();
        state.cwd = "/tmp/project".to_owned();
        assert_eq!(live_viewport_height(&state, 80, 24), 3);

        state.editor.set_text("one\ntwo");
        assert_eq!(live_viewport_height(&state, 80, 24), 5);

        state.panel = Some(SelectorPanel {
            title: "Models".to_owned(),
            help: String::new(),
            items: Vec::new(),
            selected: 0,
            query: String::new(),
        });
        assert_eq!(live_viewport_height(&state, 80, 24), 16);
        assert!(page_overlay_open(&state));

        state.panel = None;
        assert!(!page_overlay_open(&state));
        assert_eq!(live_viewport_height(&state, 80, 24), 5);
    }

    #[tokio::test]
    async fn settings_overlay_expands_viewport_and_blocks_commit_ledger() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent = tempfile::tempdir().expect("agent");
        std::fs::write(agent.path().join("settings.json"), "{}").expect("global settings");
        let mut options = pi_coding::ResourceManagerOptions::new(cwd.path());
        options.agent_dir = agent.path().to_path_buf();
        options.project_trust_override = Some(true);
        let resources = pi_coding::ResourceManager::new(options).expect("resources");
        let session = pi_coding::Session::new(pi_coding::SessionOptions {
            model: Model::default(),
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
        })
        .expect("session");
        session.attach_resources(resources).await.expect("attach resources");
        let application = Application::new(session).await;
        let mut state = todo_test_state(Vec::new());
        state.model = "faux/faux-1".to_owned();
        state.cwd = cwd.path().display().to_string();
        state.push_entry(TranscriptEntry {
            kind: TranscriptKind::User,
            content: vec![ContentBlock::text("keep-me-in-transcript")],
            tool_name: None,
            tool_card: None,
            job_card: None,
            is_error: false,
            is_partial: false,
        });
        state.push_entry(TranscriptEntry {
            kind: TranscriptKind::Assistant,
            content: vec![ContentBlock::text("durable assistant row")],
            tool_name: None,
            tool_card: None,
            job_card: None,
            is_error: false,
            is_partial: false,
        });

        // Open -> render contract: settings is a page overlay that expands the
        // live inline viewport and must not feed the overflow commit ledger.
        state.open_settings_panel(&application);
        assert!(state.settings_panel.is_some(), "settings panel must open");
        assert!(page_overlay_open(&state));
        assert_eq!(
            live_viewport_height(&state, 80, 24),
            16,
            "settings must expand the live viewport like other page overlays"
        );
        assert!(
            state.overflow_commit_batch(80, 1).len() >= 1,
            "settled transcript rows remain candidates while the overlay is open"
        );

        // Escape dismisses without leaving settings in the retained ledger path.
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let handled = handle_settings_panel_key(&application, &mut state, esc)
            .await
            .expect("settings key");
        assert_eq!(handled, Some(false));
        assert!(state.settings_panel.is_none(), "Escape must close settings");
        assert!(!page_overlay_open(&state));
        let dismissed_height = live_viewport_height(&state, 80, 24);
        assert!(
            dismissed_height < 16,
            "dismissed settings must collapse below the overlay expansion height, got {dismissed_height}"
        );
        assert_eq!(
            dismissed_height,
            live_viewport_height(&state, 80, 24),
            "dismissed height must be stable without the settings overlay"
        );

        // After dismiss, overflow commit may proceed for transcript only.
        let overflow = state.overflow_commit_batch(80, 1);
        assert!(
            overflow
                .iter()
                .all(|entry| matches!(entry.kind, TranscriptKind::User | TranscriptKind::Assistant)),
            "post-dismiss commit ledger must only carry transcript rows"
        );
        assert!(
            overflow.iter().any(|entry| content_text(&entry.content).contains("keep-me-in-transcript")),
            "durable transcript rows remain accessible after settings dismiss"
        );
        application.cleanup().await;
    }

    #[test]
    fn settings_overlay_render_then_dismiss_leaves_no_settings_rows() {
        use ratatui::backend::TestBackend;

        let agent = tempfile::tempdir().expect("agent");
        let cwd = tempfile::tempdir().expect("cwd");
        let manager =
            pi_coding::SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("manager");
        let panel = SettingsPanel::new(manager, pi_coding::SettingsScope::Global).expect("panel");

        // Stable full-height inline viewport (matches production enter()).
        let backend = TestBackend::new(100, 24);
        let mut terminal = ratatui::Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(24),
            },
        )
        .expect("inline terminal");

        terminal
            .draw(|frame| render_settings_panel(frame, &panel, None, crate::theme::DARK))
            .expect("draw open settings");
        let open = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol().to_owned())
            .collect::<String>();
        assert!(
            open.contains("Settings"),
            "open settings frame must paint settings chrome: {open}"
        );

        // Dismiss path: clear the live inline viewport exactly as
        // resize_live_viewport does on page_overlay open->closed edge, then
        // draw conversation chrome without the overlay.
        terminal.clear().expect("clear live viewport on dismiss");
        terminal
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new("composer ready").style(Style::default()),
                    frame.area(),
                );
            })
            .expect("draw after dismiss");

        let closed = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol().to_owned())
            .collect::<String>();
        assert!(
            !closed.contains("Settings"),
            "settings chrome must be absent after dismiss clear: {closed}"
        );
        assert!(
            !closed.contains("Ctrl-S apply"),
            "settings help rows must be absent after dismiss: {closed}"
        );
        assert!(
            closed.contains("composer ready"),
            "conversation chrome remains after dismiss"
        );
        // Overlay frames were never insert_before'd, so the native scrollback
        // ledger stays empty of settings content.
        terminal.backend().assert_scrollback_empty();
    }

    #[test]
    fn inline_insert_before_forces_complete_composer_redraw() {
        use ratatui::backend::TestBackend;

        let mut state = todo_test_state(Vec::new());
        state.editor.set_text("composer sentinel");
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(24),
            },
        )
        .expect("inline terminal");
        let mut images = TerminalImageRenderer::default();
        terminal
            .draw(|frame| {
                let _ = render(frame, &state, &mut images);
            })
            .expect("initial draw");

        terminal
            .insert_before(18, |buffer| {
                Paragraph::new("transcript row\n".repeat(18)).render(buffer.area, buffer);
            })
            .expect("insert transcript");
        clear_inline_viewport(&mut terminal).expect("invalidate overwritten viewport");
        terminal
            .draw(|frame| {
                let _ = render(frame, &state, &mut images);
            })
            .expect("redraw composer");

        let rows = terminal
            .backend()
            .buffer()
            .content
            .chunks(80)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>();
        let composer_top = rows
            .iter()
            .position(|row| row.starts_with("╭── π"))
            .expect("composer top border");
        assert!(rows[composer_top + 1].contains("composer sentinel"));
        assert!(rows[composer_top + 1].ends_with('╯'));
    }

    #[test]
    fn live_viewport_caps_progress_and_reserves_composer() {
        use pi_coding::{TodoPhase, TodoStatus};
        let tasks = (0..12)
            .map(|index| todo_item(&format!("t{index}"), &format!("task {index} keeps progress busy"), TodoStatus::Pending, true, &[]))
            .collect::<Vec<_>>();
        let mut state = todo_test_state(vec![TodoPhase { name: "Ship".to_owned(), tasks }]);
        state.editor.set_text("draft remains visible");
        let panel_rows = u16::try_from(render_todo_panel_lines(
            &state.todo_phases,
            &state.job_cards.cards_in_source_order(),
            crate::theme::DARK,
            80,
        ).len()).unwrap_or(u16::MAX);
        let layout = tui_layout_heights(&state, 80, 24, panel_rows, 0, 0, 0);
        assert_eq!(layout.composer, 2);
        assert!(layout.todo <= MAX_INLINE_SUMMARY_HEIGHT);
        assert!(layout.transcript >= 1);
        assert_eq!(transcript_region_height(&state, 80, 24), layout.transcript);
        assert!(live_viewport_height(&state, 80, 24) >= 3);
        assert!(live_viewport_height(&state, 80, 24) <= 24);
    }

    #[test]
    fn streaming_submit_behavior_is_steer() {
        assert_eq!(streaming_submit_behavior(false), None);
        assert_eq!(
            streaming_submit_behavior(true),
            Some(StreamingBehavior::Steer)
        );
    }

    #[test]
    fn pending_user_echo_and_streaming_assistant_never_duplicate_on_settle() {
        let mut state = todo_test_state(Vec::new());
        state.push_lines("You", "hello".to_owned(), Color::Reset);
        assert!(state.settled_commit_batch().is_empty());
        state.apply(ApplicationEvent::Agent(AgentEvent::MessageEnd { message: Message::user_text("hello", 1) }));
        assert_eq!(state.settled_commit_batch().len(), 1);
        state.finish_commit(1);
        state.streaming_text = "draft".to_owned();
        assert!(state.settled_commit_batch().is_empty());
        state.apply(ApplicationEvent::Agent(AgentEvent::MessageEnd { message: {
            let mut message = pi_ai::AssistantMessage::pending(&Model::default());
            message.content = vec![ContentBlock::text("final")];
            message.stop_reason = pi_ai::StopReason::Stop;
            Message::Assistant(message)
        }}));
        let batch = state.settled_commit_batch();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].kind, TranscriptKind::Assistant);
    }

    #[test]
    fn composer_wraps_unicode_input_inside_borders() {
        let mut state = todo_test_state(Vec::new());
        state.editor.set_text("🙂🙂abcdefghij🙂🙂abcdefghij🙂🙂abcdefghij");
        let lines = composer_border_lines(&state, 24, crate::theme::DARK);
        let rendered = lines.iter().map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect::<String>()).collect::<Vec<_>>();
        assert!(rendered.len() > 3);
        assert!(rendered.first().is_some_and(|line| line.starts_with('╭')));
        assert!(rendered.last().is_some_and(|line| line.starts_with('╰')));
        assert!(rendered.iter().all(|line| display_width(line) == 24));
        assert!(rendered[1..rendered.len() - 1].iter().all(|line| line.starts_with('│') && line.ends_with('│')));
    }

    #[test]
    fn command_prefix_acceptance_is_unambiguous_and_nonexecuting() {
        let mut state = todo_test_state(Vec::new());
        state.commands = vec![
            InteractiveCommand { name: "settings".to_owned(), description: String::new(), source: CommandSource::Builtin },
            InteractiveCommand { name: "branch".to_owned(), description: String::new(), source: CommandSource::Builtin },
            InteractiveCommand { name: "login".to_owned(), description: String::new(), source: CommandSource::Builtin },
            InteractiveCommand { name: "logout".to_owned(), description: String::new(), source: CommandSource::Builtin },
        ];
        state.editor.set_text("/set"); assert!(state.accept_unambiguous_command_prefix()); assert_eq!(state.editor.text(), "/settings");
        state.editor.set_text("/br"); assert!(state.accept_unambiguous_command_prefix()); assert_eq!(state.editor.text(), "/branch");
        state.editor.set_text("/lo"); assert!(state.accept_unambiguous_command_prefix()); assert_eq!(state.editor.text(), "/loop");
    }

    #[test]
    fn omp_completion_composer_is_three_rows() {
        let mut state = todo_test_state(Vec::new());
        state.editor.set_text("/branch");
        state.completions.items = vec![CompletionItem { value: "/branch".to_owned(), label: "branch".to_owned(), description: "Create a new branch from a previous message".to_owned(), is_directory: false }];
        state.completions.context = Some(CompletionContext::Slash);
        let rendered = composer_border_lines(&state, 90, crate::theme::DARK).into_iter().map(|line| line.spans.into_iter().map(|span| span.content.into_owned()).collect::<String>()).collect::<Vec<_>>();
        assert_eq!(rendered.len(), 3);
        assert!(rendered[1].starts_with("│  /branch"));
        assert!(rendered.iter().all(|line| display_width(line) == 90));
    }

    #[test]
    fn completion_acceptance_is_explicit_and_consumed_once() {
        let mut state = todo_test_state(Vec::new());
        state.commands = vec![InteractiveCommand { name: "branch".to_owned(), description: String::new(), source: CommandSource::Builtin }];
        state.editor.set_text("/br");
        state.completions.items = vec![CompletionItem { value: "/branch".to_owned(), label: "branch".to_owned(), description: String::new(), is_directory: false }];
        state.completions.context = Some(CompletionContext::Slash);
        state.editor.insert_char('a');
        assert_eq!(state.editor.text(), "/bra");
        state.refresh_completions();
        assert_eq!(state.editor.text(), "/bra");
        state.accept_completion();
        assert_eq!(state.editor.text(), "/branch");
        assert!(state.completions.items.is_empty());
        state.editor.insert_char('h');
        assert_eq!(state.editor.text(), "/branchh");
    }

    #[test]
    fn working_and_compaction_animate_only_while_active() {
        let mut state = todo_test_state(Vec::new());
        state.is_streaming = true;
        let working_first = composer_border_lines(&state, 90, crate::theme::DARK)[0].spans.iter().map(|span| span.content.as_ref()).collect::<String>();
        state.animation_frame = 1;
        let working_second = composer_border_lines(&state, 90, crate::theme::DARK)[0].spans.iter().map(|span| span.content.as_ref()).collect::<String>();
        assert_ne!(working_first, working_second);

        state.is_streaming = false;
        state.is_compacting = true;
        let compacting_first = composer_border_lines(&state, 90, crate::theme::DARK)[0].spans.iter().map(|span| span.content.as_ref()).collect::<String>();
        state.animation_frame = 2;
        let compacting_second = composer_border_lines(&state, 90, crate::theme::DARK)[0].spans.iter().map(|span| span.content.as_ref()).collect::<String>();
        assert_ne!(compacting_first, compacting_second);

        state.is_compacting = false;
        let idle_first = composer_border_lines(&state, 90, crate::theme::DARK)[0].spans.iter().map(|span| span.content.as_ref()).collect::<String>();
        state.animation_frame = 3;
        let idle_second = composer_border_lines(&state, 90, crate::theme::DARK)[0].spans.iter().map(|span| span.content.as_ref()).collect::<String>();
        assert_eq!(idle_first, idle_second);
        assert!(!state.has_active_animation());
    }

    #[test]
    fn job_animation_changes_only_for_queued_and_running_cards() {
        let mut state = todo_test_state(Vec::new());
        let mut job = pi_coding::JobSnapshot {
            id: "animated-job".to_owned(),
            agent_id: "Child".to_owned(),
            agent: "task".to_owned(),
            parent_id: "Main".to_owned(),
            description: Some("animate me".to_owned()),
            todo_task_id: None,
            workflow_id: None,
            workflow_generation: None,
            status: pi_coding::JobStatus::Queued,
            created_at: 1_000,
            started_at: None,
            finished_at: None,
            result: None,
        };
        let event = |job| ApplicationEvent::Orchestration(pi_coding::OrchestrationEvent::JobUpdated {
            group_id: "group".to_owned(),
            job,
        });
        state.apply(event(job.clone()));
        assert!(state.has_active_animation());
        let card = state.transcript[0].job_card.as_ref().expect("queued card");
        let mut first = Vec::new();
        let mut second = Vec::new();
        render_job_card(&mut first, card, crate::theme::DARK, 0, 80);
        render_job_card(&mut second, card, crate::theme::DARK, 1, 80);
        assert_ne!(first, second);

        job.status = pi_coding::JobStatus::Running;
        job.started_at = Some(1_100);
        state.apply(event(job.clone()));
        let card = state.transcript[0].job_card.as_ref().expect("running card");
        first.clear();
        second.clear();
        render_job_card(&mut first, card, crate::theme::DARK, 0, 80);
        render_job_card(&mut second, card, crate::theme::DARK, 2, 80);
        assert_ne!(first, second);

        job.status = pi_coding::JobStatus::Completed;
        job.finished_at = Some(2_100);
        state.apply(event(job));
        assert!(!state.has_active_animation());
        let card = state.transcript[0].job_card.as_ref().expect("completed card");
        first.clear();
        second.clear();
        render_job_card(&mut first, card, crate::theme::DARK, 0, 80);
        render_job_card(&mut second, card, crate::theme::DARK, 3, 80);
        assert_eq!(first, second);
    }

    #[test]
    fn page_up_renders_retained_committed_entries_without_resetting_ledger() {
        let mut state = todo_test_state(Vec::new());
        state.push_entry(TranscriptEntry { kind: TranscriptKind::User, content: vec![ContentBlock::text("old prompt")], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false });
        state.push_entry(TranscriptEntry { kind: TranscriptKind::Assistant, content: vec![ContentBlock::text("recent answer")], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false });
        state.finish_commit(1);
        state.transcript_scroll = 1;
        assert_eq!(state.committed_entries, 1);
        assert_eq!(state.transcript.len(), 2);
        assert!(state.transcript.iter().any(|entry| content_text(&entry.content).contains("old prompt")));
    }

    #[test]
    fn omp_single_line_composer_is_two_rows_with_input_in_lower_border() {
        let mut state = todo_test_state(Vec::new());
        state.model = "faux/faux-1".to_owned();
        state.cwd = "<workspace>/Downloads".to_owned();
        state.thinking_level = ThinkingLevel::Medium;
        state.editor.set_text("visible input");
        let lines = composer_border_lines(&state, 90, crate::theme::DARK);
        assert_eq!(lines.len(), 2);
        let top = lines[0].spans.iter().map(|span| span.content.as_ref()).collect::<String>();
        let bottom = lines[1].spans.iter().map(|span| span.content.as_ref()).collect::<String>();
        assert!(top.starts_with("╭── π  > ⬢ faux/faux-1 · ◑ med > 📁 "));
        assert!(top.ends_with('╮'));
        assert!(bottom.starts_with("╰─ visible input "));
        assert!(bottom.ends_with('╯'));
        assert_eq!(display_width(&top), 90);
        assert_eq!(display_width(&bottom), 90);
    }

    #[test]
    fn composer_header_renders_state_status_inline() {
        let mut state = todo_test_state(Vec::new());
        state.model = "faux/faux-1".to_owned();
        state.cwd = "<workspace>".to_owned();
        state.status = "Usage: /import <path.jsonl>".to_owned();
        state.editor.set_text("");
        let top = composer_border_lines(&state, 120, crate::theme::DARK)[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(
            top.contains("Usage: /import <path.jsonl>"),
            "composer header must surface state.status: {top}"
        );
        assert!(top.contains("▶──"), "OMP activity glyph retained: {top}");

        state.push_status("Ready".to_owned(), false);
        assert_eq!(state.status, "Ready");
        let top = composer_border_lines(&state, 120, crate::theme::DARK)[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(top.contains("Ready"), "push_status must dual-write inline toast: {top}");

        // Busy animation still wins over the toast.
        state.is_streaming = true;
        let top = composer_border_lines(&state, 120, crate::theme::DARK)[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(top.contains("working"), "streaming must prefer activity label: {top}");
        assert!(!top.contains("Ready"), "activity label replaces idle toast: {top}");
    }

    #[test]
    fn run_failure_is_ephemeral_and_never_committed() {
        let mut state = todo_test_state(Vec::new());
        state.apply(ApplicationEvent::RunFailed {
            message: "runtime exploded\nsecret detail".to_owned(),
        });

        assert_eq!(
            state.composer_error.as_deref(),
            Some("runtime exploded\nsecret detail")
        );
        assert_eq!(state.status, "runtime exploded");
        assert!(state.transcript.is_empty());
        assert!(state.settled_commit_batch().is_empty());
        assert!(state.overflow_commit_batch(80, 12).is_empty());
    }

    #[test]
    fn composer_error_clears_on_escape_and_accepted_message_only() {
        let mut state = todo_test_state(Vec::new());
        state.push_status("first rejection".to_owned(), true);

        assert!(!dismiss_composer_error_on_escape(&mut state, KeyCode::Char('x')));
        assert_eq!(state.composer_error.as_deref(), Some("first rejection"));
        state.push_status("second rejection".to_owned(), true);
        assert_eq!(state.composer_error.as_deref(), Some("second rejection"));

        state.record_accepted_prompt("accepted prompt");
        assert!(state.composer_error.is_none());

        state.push_status("dismiss me".to_owned(), true);
        state.last_escape = Some(std::time::Instant::now());
        assert!(dismiss_composer_error_on_escape(&mut state, KeyCode::Esc));
        assert!(state.composer_error.is_none());
        assert!(state.last_escape.is_none());
    }

    #[test]
    fn composer_error_toast_is_bounded_sanitized_and_truncated() {
        let mut state = todo_test_state(Vec::new());
        state.push_status(
            format!("bad \u{1b}[31mpayload\u{1b}[0m {}\nsecond row\nthird row", "🙂".repeat(40)),
            true,
        );

        let lines = composer_error_toast_lines(&state, 24, crate::theme::DARK);
        let rendered = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), usize::from(composer_error_toast_height(&state, 24)));
        assert!(lines.len() <= MAX_COMPOSER_ERROR_HEIGHT);
        assert!(rendered.iter().all(|line| display_width(line) == 24));
        assert!(rendered.iter().all(|line| !line.contains('\u{1b}')));
        assert!(rendered.iter().any(|line| line.contains('…')));
        assert!(rendered.first().is_some_and(|line| line.starts_with("╭ Error")));
        assert!(rendered.last().is_some_and(|line| line.starts_with('╰')));
        assert_eq!(lines[0].spans[0].style.fg, Some(crate::theme::DARK.error));

        state.composer_error = None;
        assert_eq!(composer_error_toast_height(&state, 24), 0);
        assert!(composer_error_toast_lines(&state, 24, crate::theme::DARK).is_empty());
    }

    #[test]
    fn composer_error_renders_immediately_above_composer() {
        use ratatui::backend::TestBackend;

        let mut state = todo_test_state(Vec::new());
        state.model = "faux/faux-1".to_owned();
        state.cwd = "/tmp/project".to_owned();
        state.editor.set_text("draft stays");
        state.push_status("runtime exploded".to_owned(), true);
        let backend = TestBackend::new(80, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut images = TerminalImageRenderer::default();
        terminal
            .draw(|frame| {
                let _ = render(frame, &state, &mut images);
            })
            .unwrap();
        let rows = terminal
            .backend()
            .buffer()
            .content
            .chunks(80)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>();
        let error_row = rows
            .iter()
            .position(|row| row.contains("runtime exploded"))
            .expect("error message row");
        let composer_row = rows
            .iter()
            .position(|row| row.contains("π") && row.contains("faux/faux-1"))
            .expect("composer top row");
        assert!(error_row < composer_row, "{rows:#?}");
        assert!(rows[composer_row - 1].starts_with('╰'), "{rows:#?}");
        assert!(
            rows[..composer_row]
                .iter()
                .any(|row| row.contains(COMPOSER_ERROR_DISMISSAL_HINT)),
            "{rows:#?}"
        );
    }

    #[test]
    fn ctrl_c_clears_editor_and_attachments_without_exiting() {
        let mut state = todo_test_state(Vec::new());
        state.editor.set_text("draft text");
        state.pending_attachments.push(PendingAttachment {
            block: ContentBlock::Image {
                data: "aW1n".to_owned(),
                mime_type: "image/png".to_owned(),
            },
            width: 1,
            height: 1,
        });
        let now = std::time::Instant::now();
        assert!(
            !handle_ctrl_c_clear_or_exit(&mut state, now),
            "first Ctrl-C with content must not exit"
        );
        assert!(state.editor.is_empty());
        assert!(state.pending_attachments.is_empty());
        assert_eq!(state.last_ctrl_c, Some(now));
        assert!(
            state.status.contains("Cleared") && state.status.contains("Ctrl+C again"),
            "clear arm should hint double-press exit: {}",
            state.status
        );
    }

    #[test]
    fn ctrl_u_clears_multiline_draft_and_pending_completion_state() {
        let mut state = todo_test_state(Vec::new());
        state.editor.set_text("line one\nline two");
        state.editor.row = 1;
        state.editor.column = 4;
        state.completions.items = vec![CompletionItem {
            label: "/model".to_owned(),
            value: "/model".to_owned(),
            description: String::new(),
            is_directory: false,
        }];
        state.completions.context = Some(CompletionContext::Slash);
        state.completion_query = Some((
            0,
            file_search::AtPrefix {
                start: 0,
                end: 1,
                query: String::new(),
            },
        ));
        state.prompt_history_index = Some(0);
        state.prompt_history_draft = Some("stale".to_owned());

        state.clear_composer();

        assert!(state.editor.is_empty(), "Ctrl-U must clear the whole composer");
        assert_eq!(state.editor.lines.len(), 1);
        assert_eq!((state.editor.row, state.editor.column), (0, 0));
        assert!(state.completions.items.is_empty());
        assert!(state.completions.context.is_none());
        assert!(state.completion_query.is_none());
        assert!(state.prompt_history_index.is_none());
        assert!(state.prompt_history_draft.is_none());
        assert_eq!(
            state.keybindings.resolve(&KeyEvent::new(
                KeyCode::Char('u'),
                KeyModifiers::CONTROL
            )),
            Some(Action::EditorClear)
        );
    }

    #[test]
    fn prompt_history_up_down_order_and_draft_restore() {
        let mut state = todo_test_state(Vec::new());
        state.record_accepted_prompt("older");
        state.record_accepted_prompt("newer");
        state.editor.set_text("live draft");

        state.history_or_move_up();
        assert_eq!(state.editor.text(), "newer");
        assert_eq!(state.prompt_history_index, Some(1));
        assert_eq!(state.prompt_history_draft.as_deref(), Some("live draft"));

        state.history_or_move_up();
        assert_eq!(state.editor.text(), "older");
        assert_eq!(state.prompt_history_index, Some(0));

        state.history_or_move_up();
        assert_eq!(state.editor.text(), "older", "oldest is a hard stop");
        assert_eq!(state.prompt_history_index, Some(0));

        state.history_or_move_down();
        assert_eq!(state.editor.text(), "newer");

        state.history_or_move_down();
        assert_eq!(state.editor.text(), "live draft");
        assert!(state.prompt_history_index.is_none());
        assert!(state.prompt_history_draft.is_none());
    }

    #[test]
    fn prompt_history_multiline_boundary_prefers_cursor_motion() {
        let mut state = todo_test_state(Vec::new());
        state.record_accepted_prompt("history entry");
        state.editor.set_text("first\nsecond\nthird");
        state.editor.row = 2;
        state.editor.column = 2;

        state.history_or_move_up();
        assert_eq!(state.editor.text(), "first\nsecond\nthird");
        assert_eq!(state.editor.row, 1);
        assert!(state.prompt_history_index.is_none());

        state.history_or_move_up();
        assert_eq!(state.editor.row, 0);
        assert!(state.prompt_history_index.is_none());

        state.history_or_move_up();
        assert_eq!(state.editor.text(), "history entry");
        assert_eq!(state.prompt_history_index, Some(0));
        assert_eq!(
            state.prompt_history_draft.as_deref(),
            Some("first\nsecond\nthird")
        );

        // Replace with a multiline history entry and ensure Down moves inside
        // it before restoring the draft.
        state.prompt_history = vec!["alpha\nbeta".to_owned()];
        state.prompt_history_index = Some(0);
        state.editor.set_text("alpha\nbeta");
        state.editor.row = 0;
        state.editor.column = 0;
        state.prompt_history_draft = Some("draft".to_owned());

        state.history_or_move_down();
        assert_eq!(state.editor.text(), "alpha\nbeta");
        assert_eq!(state.editor.row, 1);
        assert_eq!(state.prompt_history_index, Some(0));

        state.history_or_move_down();
        assert_eq!(state.editor.text(), "draft");
        assert!(state.prompt_history_index.is_none());
    }

    #[test]
    fn prompt_history_suppresses_adjacent_duplicates() {
        let mut state = todo_test_state(Vec::new());
        state.record_accepted_prompt("same");
        state.record_accepted_prompt("same");
        state.record_accepted_prompt("other");
        state.record_accepted_prompt("other");
        state.record_accepted_prompt("same");
        assert_eq!(
            state.prompt_history,
            vec!["same".to_owned(), "other".to_owned(), "same".to_owned()]
        );
    }

    #[test]
    fn prompt_history_rebuild_on_session_transition_drops_prior_draft() {
        let mut state = todo_test_state(Vec::new());
        state.record_accepted_prompt("session-a-1");
        state.record_accepted_prompt("session-a-2");
        state.editor.set_text("unsaved a draft");
        state.history_or_move_up();
        assert_eq!(state.editor.text(), "session-a-2");
        assert!(state.prompt_history_draft.is_some());

        let next_messages = vec![
            Message::User(pi_ai::UserMessage {
                content: vec![ContentBlock::text("session-b-old")],
                timestamp: 0,
            }),
            Message::User(pi_ai::UserMessage {
                content: vec![ContentBlock::text("session-b-new")],
                timestamp: 2,
            }),
        ];
        state.rebuild_prompt_history_from_messages(next_messages);

        assert_eq!(
            state.prompt_history,
            vec![
                "session-b-old".to_owned(),
                "session-b-new".to_owned()
            ]
        );
        assert!(state.prompt_history_index.is_none());
        assert!(
            state.prompt_history_draft.is_none(),
            "session transition must not leak prior draft"
        );

        state.editor.set_text("session-b draft");
        state.history_or_move_up();
        assert_eq!(state.editor.text(), "session-b-new");
        assert_eq!(
            state.prompt_history_draft.as_deref(),
            Some("session-b draft")
        );
    }


    #[test]
    fn idle_double_ctrl_c_within_500ms_exits_while_single_does_not() {
        let mut state = todo_test_state(Vec::new());
        let first = std::time::Instant::now();
        assert!(
            !handle_ctrl_c_clear_or_exit(&mut state, first),
            "first idle Ctrl-C arms the exit ladder only"
        );
        assert_eq!(state.last_ctrl_c, Some(first));
        assert!(
            state.status.contains("Ctrl+C again to quit"),
            "idle arm should set quit hint: {}",
            state.status
        );

        let second = first + std::time::Duration::from_millis(200);
        assert!(
            handle_ctrl_c_clear_or_exit(&mut state, second),
            "second idle Ctrl-C within 500ms must request clean exit"
        );
        assert!(state.last_ctrl_c.is_none());

        // A lone press after the window expires must re-arm, not exit.
        let mut state = todo_test_state(Vec::new());
        let armed = std::time::Instant::now();
        assert!(!handle_ctrl_c_clear_or_exit(&mut state, armed));
        let late = armed + std::time::Duration::from_millis(501);
        assert!(
            !handle_ctrl_c_clear_or_exit(&mut state, late),
            "Ctrl-C after 500ms must not exit"
        );
        assert_eq!(state.last_ctrl_c, Some(late));
    }

    #[test]
    fn unrelated_input_resets_double_ctrl_c_timer() {
        let mut state = todo_test_state(Vec::new());
        let armed = std::time::Instant::now();
        assert!(!handle_ctrl_c_clear_or_exit(&mut state, armed));
        assert!(state.last_ctrl_c.is_some());

        // Same policy as handle_key: any non-ClearEditor action drops the arm.
        let action = state
            .keybindings
            .resolve(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(!matches!(action, Some(Action::ClearEditor)));
        if !matches!(action, Some(Action::ClearEditor)) {
            state.last_ctrl_c = None;
        }
        assert!(state.last_ctrl_c.is_none());

        let again = armed + std::time::Duration::from_millis(100);
        assert!(
            !handle_ctrl_c_clear_or_exit(&mut state, again),
            "after reset, next Ctrl-C is a fresh first press"
        );
        assert_eq!(state.last_ctrl_c, Some(again));
    }

    #[test]
    fn default_status_documents_ctrl_c_double_quit() {
        let status = "Enter submit · Shift+Enter/Ctrl+J newline · Esc abort · Ctrl+C twice quit · Ctrl+D quit";
        assert!(status.contains("Ctrl+C twice quit"), "{status}");
        assert!(status.contains("Ctrl+D quit"), "{status}");
        assert!(
            !status.to_lowercase().contains("cannot exit"),
            "{status}"
        );
        let bindings = KeyBindingsManager::default();
        let text = format_hotkeys_text(&bindings);
        assert!(
            text.contains("Clear input / quit (twice)")
                || text.contains("Ctrl+C")
                || text.contains("clear"),
            "hotkeys text should document Ctrl+C clear/quit: {text}"
        );
    }

    fn tiny_png_bytes() -> Vec<u8> {
        use image::ImageBuffer;
        use image::{DynamicImage, ImageFormat};
        use std::io::Cursor;
        let image = DynamicImage::ImageRgb8(ImageBuffer::from_fn(2, 2, |x, y| {
            image::Rgb([(x * 40) as u8, (y * 80) as u8, 120])
        }));
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .expect("encode png");
        bytes
    }

    #[test]
    fn clipboard_png_fixture_attaches_one_image_and_preserves_draft_text() {
        let mut state = todo_test_state(Vec::new());
        state.editor.set_text("describe this");
        let image = crate::clipboard::image_from_bytes(tiny_png_bytes(), "image/png")
            .expect("fixture png");
        let expected_block = image.clone().into_content_block();
        let expected_width = image.width;
        let expected_height = image.height;
        assert!(expected_width > 0 && expected_height > 0);
        state.apply_background(BackgroundEvent::ClipboardRead(Ok(Some(
            crate::clipboard::ClipboardContent::Image(image),
        ))));

        assert_eq!(state.editor.text(), "describe this");
        assert_eq!(state.pending_attachments.len(), 1);
        assert_eq!(state.pending_attachments[0].block, expected_block);
        assert_eq!(state.pending_attachments[0].width, expected_width);
        assert_eq!(state.pending_attachments[0].height, expected_height);
        assert!(state.status.contains("Attached image/png"));
        assert!(state.status.contains("1 pending"));
        assert!(
            !state.status.contains("iVBORw0KGgo"),
            "status must not print raw/base64 image bytes"
        );

        let placeholder = format!("[Image #1, {expected_width}x{expected_height}]");
        let labels = pending_attachment_labels(&state.pending_attachments);
        assert_eq!(labels, vec![placeholder.clone()]);

        let composer = composer_border_lines(&state, 90, crate::theme::DARK);
        let rendered = composer
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(
            rendered.iter().any(|line| line.contains("describe this")),
            "draft text must remain visible: {rendered:?}"
        );
        assert!(
            rendered.iter().any(|line| line.contains(&placeholder)),
            "composer must show pending attachment label: {rendered:?}"
        );
        assert!(
            rendered.iter().all(|line| !line.contains("iVBORw0KGgo")),
            "composer must not print image bytes"
        );
        assert!(
            rendered
                .iter()
                .all(|line| !line.contains('/') || !line.contains("home")),
            "composer must not print absolute paths"
        );

        let submitted = assemble_submit_attachments(&state.pending_attachments, Vec::new());
        assert_eq!(submitted.len(), 1);
        assert_eq!(submitted[0], expected_block);
        let ContentBlock::Image { data, mime_type } = &submitted[0] else {
            panic!("expected image content block in submit payload");
        };
        assert_eq!(mime_type, "image/png");
        assert!(!data.is_empty());

        // Successful send clears pending exactly once.
        state.pending_attachments.clear();
        assert!(state.pending_attachments.is_empty());
        assert_eq!(state.editor.text(), "describe this");
    }

    #[test]
    fn multiple_clipboard_images_number_stably_in_composer() {
        let mut state = todo_test_state(Vec::new());
        let first = crate::clipboard::image_from_bytes(tiny_png_bytes(), "image/png")
            .expect("first png");
        let second = crate::clipboard::image_from_bytes(tiny_png_bytes(), "image/png")
            .expect("second png");
        let first_w = first.width;
        let first_h = first.height;
        let second_w = second.width;
        let second_h = second.height;
        state.apply_background(BackgroundEvent::ClipboardRead(Ok(Some(
            crate::clipboard::ClipboardContent::Image(first),
        ))));
        state.apply_background(BackgroundEvent::ClipboardRead(Ok(Some(
            crate::clipboard::ClipboardContent::Image(second),
        ))));

        assert_eq!(state.pending_attachments.len(), 2);
        let labels = pending_attachment_labels(&state.pending_attachments);
        assert_eq!(
            labels,
            vec![
                format!("[Image #1, {first_w}x{first_h}]"),
                format!("[Image #2, {second_w}x{second_h}]"),
            ]
        );

        let rendered = composer_border_lines(&state, 90, crate::theme::DARK)
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content.into_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(rendered.iter().any(|line| line.contains(&labels[0])));
        assert!(rendered.iter().any(|line| line.contains(&labels[1])));
        // Stable numbering: first paste stays #1 after second paste.
        let first_pos = rendered
            .iter()
            .position(|line| line.contains(&labels[0]))
            .expect("first placeholder");
        let second_pos = rendered
            .iter()
            .position(|line| line.contains(&labels[1]))
            .expect("second placeholder");
        assert!(first_pos < second_pos);
    }

    #[test]
    fn failed_submit_keeps_clipboard_attachment_without_duplicating_file_images() {
        let mut state = todo_test_state(Vec::new());
        state.editor.set_text("keep draft @shot.png");
        let clipboard_image = crate::clipboard::image_from_bytes(tiny_png_bytes(), "image/png")
            .expect("clipboard png");
        let clipboard = PendingAttachment::from_clipboard_image(clipboard_image);
        let file_image = ContentBlock::Image {
            data: "ZmlsZQ==".to_owned(),
            mime_type: "image/png".to_owned(),
        };
        state.pending_attachments.push(clipboard.clone());

        let pre_submit = state.pending_attachments.clone();
        let assembled = assemble_submit_attachments(&pre_submit, vec![file_image.clone()]);
        assert_eq!(assembled.len(), 2, "one clipboard + one file image");
        assert_eq!(assembled[0], clipboard.block);
        assert_eq!(assembled[1], file_image);

        // Simulate prompt rejection: restore only pre-submit clipboard pending.
        restore_pending_after_failed_submit(&mut state.pending_attachments, pre_submit);
        assert_eq!(state.pending_attachments, vec![clipboard.clone()]);
        assert_eq!(state.editor.text(), "keep draft @shot.png");
        let labels = pending_attachment_labels(&state.pending_attachments);
        assert_eq!(
            labels,
            vec![format!(
                "[Image #1, {}x{}]",
                clipboard.width, clipboard.height
            )]
        );

        // Retry assembly must not accumulate file images into pending.
        let retried =
            assemble_submit_attachments(&state.pending_attachments, vec![file_image.clone()]);
        assert_eq!(retried.len(), 2);
        assert_eq!(
            retried
                .iter()
                .filter(|block| matches!(block, ContentBlock::Image { .. }))
                .count(),
            2
        );
        assert_eq!(state.pending_attachments.len(), 1);
    }

    #[test]
    fn clipboard_errors_are_actionable_and_never_print_payload_bytes() {
        let mut state = todo_test_state(Vec::new());
        state.editor.set_text("draft stays");

        state.apply_background(BackgroundEvent::ClipboardRead(Err(
            "image exceeds the 20 MiB limit".to_owned(),
        )));
        assert_eq!(state.editor.text(), "draft stays");
        assert!(state.pending_attachments.is_empty());
        let oversized = state.composer_error.as_deref().expect("error toast");
        assert!(
            oversized.contains("Clipboard paste failed"),
            "actionable status missing: {oversized}"
        );
        assert!(
            oversized.contains("20 MiB"),
            "size limit missing: {oversized}"
        );
        assert!(!oversized.contains("iVBORw0KGgo"));
        assert!(state.transcript.is_empty());

        state.apply_background(BackgroundEvent::ClipboardRead(Err(
            "data is not a supported image".to_owned(),
        )));
        let malformed = state.composer_error.as_deref().expect("error toast");
        assert!(
            malformed.contains("Clipboard paste failed"),
            "actionable status missing: {malformed}"
        );
        assert!(
            malformed.contains("not a supported image"),
            "mime error missing: {malformed}"
        );
        assert!(!malformed.contains("iVBORw0KGgo"));
        assert!(state.transcript.is_empty());

        let labels = pending_attachment_labels(&[PendingAttachment {
            block: ContentBlock::Image {
                data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==".to_owned(),
                mime_type: "image/png".to_owned(),
            },
            width: 1,
            height: 1,
        }]);
        assert_eq!(labels, vec!["[Image #1, 1x1]".to_owned()]);
        assert!(labels.iter().all(|label| !label.contains("iVBORw0KGgo")));
        assert!(labels.iter().all(|label| !label.contains("image/png")));
    }

    #[test]
    fn ctrl_v_and_alt_v_bind_to_clipboard_paste() {
        let bindings = KeyBindingsManager::default();
        let ctrl_v = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL);
        let alt_v = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::ALT);
        assert_eq!(bindings.resolve(&ctrl_v), Some(Action::ClipboardPaste));
        assert_eq!(bindings.resolve(&alt_v), Some(Action::ClipboardPaste));
        // Alt+V is the discoverable image-paste chord (terminals often steal Ctrl+V).
        let label = Action::ClipboardPaste.label();
        assert!(
            label.contains("Alt+V"),
            "hotkey label must advertise Alt+V: {label}"
        );
        let hotkeys = format_hotkeys_text(&bindings);
        assert!(
            hotkeys.contains("Alt+V"),
            "/hotkeys must document Alt+V image paste: {hotkeys}"
        );
        assert!(
            hotkeys.contains("Ctrl+V") || hotkeys.contains("ctrl+v") || hotkeys.contains("Ctrl+v"),
            "/hotkeys should still list Ctrl+V best-effort: {hotkeys}"
        );
    }

    #[tokio::test]
    async fn empty_bracketed_paste_starts_clipboard_image_read() {
        // Terminals deliver image-only Ctrl+V as empty Event::Paste / bracketed paste.
        // Production spawns the clipboard read on the Tokio runtime; the test must
        // run on one or tokio::spawn panics with "no reactor running".
        let (background_tx, mut background_rx) = mpsc::unbounded_channel();
        let mut state = todo_test_state(Vec::new());
        state.background_tx = background_tx;
        state.editor.set_text("draft stays");
        handle_paste(&mut state, "");
        assert_eq!(state.editor.text(), "draft stays");
        assert!(
            state.clipboard_read_busy,
            "empty paste must arm the async clipboard reader"
        );
        assert!(
            state.status.contains("Reading clipboard"),
            "status must show clipboard progress: {}",
            state.status
        );
        assert!(
            state.status.contains("Alt+V"),
            "status must advertise the reliable image chord: {}",
            state.status
        );
        // The generated reader must report back through the same background channel
        // seam production drains; inject its completion via apply_background and
        // verify the busy flag clears. No desktop clipboard is required: platform
        // command failures still produce a terminal ClipboardRead event.
        let event = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            background_rx.recv(),
        )
        .await
        .expect("spawned clipboard read must report on the background channel")
        .expect("background channel must stay open");
        assert!(
            matches!(event, BackgroundEvent::ClipboardRead(_)),
            "event must be a ClipboardRead completion"
        );
        state.apply_background(event);
        assert!(
            !state.clipboard_read_busy,
            "injected completion must clear the busy flag"
        );
    }

    #[test]
    fn nonempty_text_paste_does_not_start_clipboard_read() {
        let mut state = todo_test_state(Vec::new());
        handle_paste(&mut state, "hello from bracketed paste");
        assert_eq!(state.editor.text(), "hello from bracketed paste");
        assert!(
            !state.clipboard_read_busy,
            "text paste must not start the image clipboard reader"
        );
        assert!(state.status.contains("Pasted"));
        assert!(state.pending_attachments.is_empty());
    }

    #[test]
    fn background_channel_clipboard_image_drains_into_composer_placeholder() {
        // Production path: background task sends ClipboardRead on background_tx;
        // the main select! loop drains via apply_background. Inject through the
        // same channel seam without a desktop clipboard.
        let (background_tx, mut background_rx) = mpsc::unbounded_channel();
        let mut state = todo_test_state(Vec::new());
        state.background_tx = background_tx.clone();
        state.editor.set_text("describe this");

        let image = crate::clipboard::image_from_bytes(tiny_png_bytes(), "image/png")
            .expect("fixture png");
        let expected_block = image.clone().into_content_block();
        let expected_width = image.width;
        let expected_height = image.height;
        let placeholder = format!("[Image #1, {expected_width}x{expected_height}]");

        background_tx
            .send(BackgroundEvent::ClipboardRead(Ok(Some(
                crate::clipboard::ClipboardContent::Image(image),
            ))))
            .expect("background channel open");

        let event = background_rx
            .try_recv()
            .expect("production drain must observe the clipboard completion");
        state.apply_background(event);

        assert!(!state.clipboard_read_busy);
        assert_eq!(state.editor.text(), "describe this");
        assert_eq!(state.pending_attachments.len(), 1);
        assert_eq!(state.pending_attachments[0].block, expected_block);
        let labels = pending_attachment_labels(&state.pending_attachments);
        assert_eq!(labels, vec![placeholder.clone()]);

        let rendered = composer_border_lines(&state, 90, crate::theme::DARK)
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content.into_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(
            rendered.iter().any(|line| line.contains(&placeholder)),
            "composer must show exact placeholder after channel drain: {rendered:?}"
        );
        assert!(
            rendered.iter().any(|line| line.contains("describe this")),
            "draft text must remain: {rendered:?}"
        );

        let submitted = assemble_submit_attachments(&state.pending_attachments, Vec::new());
        assert_eq!(submitted, vec![expected_block]);
        state.pending_attachments.clear();
        assert!(state.pending_attachments.is_empty());
    }

    #[test]
    fn welcome_line_documents_alt_v_image_paste() {
        let state = todo_test_state(Vec::new());
        let lines = render_welcome_lines(&state, crate::theme::DARK);
        let rendered = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("Alt+V"),
            "welcome must advertise Alt+V image paste: {rendered}"
        );
        assert!(rendered.contains("  rpi"), "welcome must use concise rpi branding: {rendered}");
        assert!(!rendered.contains("pi-rs"), "welcome must not expose repository branding: {rendered}");
        assert!(
            rendered.contains("paste image") || rendered.contains("Paste image"),
            "welcome must mention image paste: {rendered}"
        );
    }

    #[test]
    fn omp_multiline_composer_expands_without_losing_lines() {
        let mut state = todo_test_state(Vec::new());
        state.editor.set_text("first\nsecond");
        let lines = composer_border_lines(&state, 90, crate::theme::DARK);
        assert_eq!(lines.len(), 4);
        let rendered = lines.iter().map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect::<String>()).collect::<Vec<_>>();
        assert!(rendered[0].starts_with('╭'));
        assert!(rendered[1].contains("first"));
        assert!(rendered[2].contains("second"));
        assert!(rendered[3].starts_with('╰'));
        assert!(rendered[3].ends_with('╯'));
        assert!(rendered.iter().all(|line| display_width(line) == 90));
    }

    #[test]
    fn compact_inline_render_has_model_cwd_editor_without_dashboard_labels() {
        use ratatui::backend::TestBackend;
        let mut state = todo_test_state(Vec::new());
        state.model = "faux/faux-1".to_owned();
        state.cwd = "/tmp/project".to_owned();
        state.editor.set_text("draft prompt");
        let backend = TestBackend::new(80, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut images = TerminalImageRenderer::default();
        terminal.draw(|frame| { let _ = render(frame, &state, &mut images); }).unwrap();
        let rendered = terminal.backend().buffer().content.iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("faux/faux-1"));
        assert!(rendered.contains("/tmp/project"));
        assert!(rendered.contains("draft prompt"));
        assert!(!rendered.contains("Conversation"));
        assert!(!rendered.contains("pi (rs)"));
    }

    #[test]
    fn bounded_composer_materializes_only_visible_rows_for_large_and_wide_unicode_pastes() {
        let mut state = todo_test_state(Vec::new());
        state.editor.set_text(&format!("{}{}", "x".repeat(7_608), "🙂".repeat(200)));
        let lines = composer_border_lines_bounded(&state, 80, crate::theme::DARK, 9);
        assert!(lines.len() <= 9);
        let (_, cursor_column) = editor_wrapped_position(&state, 75);
        assert!(cursor_column < 75);
    }

    #[test]
    fn terminal_lifecycle_commands_enable_and_disable_bracketed_paste() {
        let mut bytes = Vec::new();
        acquire_terminal(&mut bytes).unwrap();
        release_terminal(&mut bytes).unwrap();
        let output = String::from_utf8(bytes).unwrap();
        assert!(!output.contains("\u{1b}[?1049h"));
        assert!(!output.contains("\u{1b}[?1049l"));
        assert!(output.contains("\u{1b}[?2004h"));
        assert!(output.contains("\u{1b}[?2004l"));
        assert!(output.contains("\u{1b}[?25l"));
        assert!(output.contains("\u{1b}[?25h"));
    }

    #[test]
    fn hotkeys_sections_cover_keymap_categories_from_catalog() {
        let bindings = KeyBindingsManager::default();
        let text = format_hotkeys_text(&bindings);
        for section in [
            "## Editor",
            "## Application",
            "## Session selector",
            "## Scoped models",
            "## Session tree",
        ] {
            assert!(text.contains(section), "missing section {section} in:\n{text}");
        }
        assert!(text.contains("Enter  —  Submit message") || text.contains("Enter"), "{text}");
        assert!(text.contains("Ctrl+D") || text.contains("Quit"), "{text}");
        assert!(text.contains("Session tree") || text.contains("Filter:"), "{text}");
        let sections = bindings.hotkey_sections();
        assert_eq!(sections.len(), 5, "{sections:?}");
        assert_eq!(sections[0].title, "Editor");
        assert_eq!(sections[1].title, "Application");
        assert_eq!(sections[2].title, "Session selector");
        assert_eq!(sections[3].title, "Scoped models");
        assert_eq!(sections[4].title, "Session tree");
        assert!(!sections[0].rows.is_empty());
    }

    #[test]
    fn overlay_key_hints_document_esc_q_and_navigation() {
        let tree = TreePanel::new(
            pi_coding::SessionTreeResult {
                tree: Vec::new(),
                leaf_id: None,
                active_leaf_id: None,
            },
            TreePanelMode::Navigate,
        );
        let tree_hints = tree_panel_key_hints(&tree);
        assert!(tree_hints.contains("Esc/q"), "{tree_hints}");
        assert!(tree_hints.contains("↑/↓") || tree_hints.contains("move"), "{tree_hints}");

        let fork = TreePanel::new(
            pi_coding::SessionTreeResult {
                tree: Vec::new(),
                leaf_id: None,
                active_leaf_id: None,
            },
            TreePanelMode::Fork,
        );
        let fork_hints = tree_panel_key_hints(&fork);
        assert!(fork_hints.contains("Esc/q"), "{fork_hints}");
        assert!(fork_hints.contains("Enter"), "{fork_hints}");

        let selector = SavedSessionSelector::new(Vec::new(), None);
        let session_hints = session_selector_key_hints(&selector);
        assert!(session_hints.contains("Esc/q"), "{session_hints}");
        assert!(session_hints.contains("Enter"), "{session_hints}");

        let model_hints = scoped_model_key_hints();
        assert!(model_hints.contains("Esc/q"), "{model_hints}");
        assert!(model_hints.contains("Enter"), "{model_hints}");

        let panel = SelectorPanel {
            title: "Models".to_owned(),
            help: "Type to filter · Enter select · Esc cancel".to_owned(),
            items: Vec::new(),
            selected: 0,
            query: String::new(),
        };
        let panel_hints = selector_panel_key_hints(&panel);
        assert!(panel_hints.contains("Esc"), "{panel_hints}");
    }

    #[test]
    fn unknown_overlay_key_keeps_status_help_text() {
        let key = KeyEvent::new(KeyCode::F(9), KeyModifiers::NONE);
        let status = overlay_unknown_key_status(key);
        assert!(status.contains("Unknown key"), "{status}");
        assert!(status.contains("/hotkeys") || status.contains("footer"), "{status}");
        assert!(status.contains("Esc"), "{status}");
    }

    #[test]
    fn process_panel_list_footer_shows_key_hints() {
        use ratatui::backend::TestBackend;
        let panel = ProcessPanel::new(Vec::new());
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_process_panel(frame, &panel, crate::theme::DARK);
            })
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Esc"), "{rendered}");
        assert!(rendered.contains("Enter") || rendered.contains("select"), "{rendered}");
    }

    #[test]
    fn tree_and_session_overlay_render_footer_key_hints() {
        use ratatui::backend::TestBackend;
        let mut state = todo_test_state(Vec::new());
        state.tree_panel = Some(TreePanel::new(
            pi_coding::SessionTreeResult {
                tree: Vec::new(),
                leaf_id: None,
                active_leaf_id: None,
            },
            TreePanelMode::Navigate,
        ));
        let backend = TestBackend::new(120, 36);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut images = TerminalImageRenderer::default();
        terminal
            .draw(|frame| {
                let _ = render(frame, &state, &mut images);
            })
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            rendered.contains("Esc/q") || rendered.contains("Esc"),
            "tree overlay missing key hints: {rendered}"
        );

        state.tree_panel = None;
        state.session_selector = Some(SavedSessionSelector::new(Vec::new(), None));
        terminal
            .draw(|frame| {
                let _ = render(frame, &state, &mut images);
            })
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            rendered.contains("Esc/q") || rendered.contains("Esc"),
            "session selector missing key hints: {rendered}"
        );
        assert!(rendered.contains("Enter") || rendered.contains("resume"), "{rendered}");
    }

    fn agents_panel_fixture() -> AgentsPanel {
        use pi_coding::{AgentDefinition, AgentDefinitionSource, AgentRuntimeSettings};

        let definition = |name: &str, tools: Vec<&str>, skills: Vec<&str>| AgentDefinition {
            name: name.to_owned(),
            description: format!("{name} handles detailed orchestration diagnostics"),
            system_prompt: "prompt".to_owned(),
            tools: Some(tools.into_iter().map(str::to_owned).collect()),
            autoload_skills: skills.into_iter().map(str::to_owned).collect(),
            model: None,
            thinking_level: Some(ThinkingLevel::Medium),
            source: AgentDefinitionSource::User,
            path: None,
            trusted: true,
        };
        let mut settings = std::collections::BTreeMap::new();
        settings.insert(
            "reviewer".to_owned(),
            AgentRuntimeSettings {
                enabled: Some(true),
                model: Some("missing/model".to_owned()),
                tools: None,
            },
        );
        AgentsPanel::new(
            vec![
                definition(
                    "reviewer",
                    vec!["read", "grep", "bash", "browser", "debug"],
                    vec!["rust", "research", "performance", "accessibility"],
                ),
                definition("worker", vec!["read", "bash"], vec!["rust"]),
            ],
            &settings,
            Model {
                provider: "openai".to_owned(),
                id: "gpt-4.1".to_owned(),
                name: "gpt-4.1".to_owned(),
                ..Model::default()
            },
            vec![Model {
                provider: "openai".to_owned(),
                id: "gpt-4.1".to_owned(),
                name: "gpt-4.1".to_owned(),
                ..Model::default()
            }],
        )
    }

    #[test]
    fn agents_panel_lines_reserve_width_and_separate_wrapped_records() {
        let panel = agents_panel_fixture();
        for width in [24, 106] {
            let lines = agents_panel_lines(&panel, crate::theme::DARK, width);
            assert!(lines.iter().all(|line| {
                let text = line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>();
                display_width(&text) <= width
            }));
            let selected = lines
                .iter()
                .enumerate()
                .filter(|(_, line)| {
                    line.spans
                        .iter()
                        .any(|span| span.style.bg == Some(crate::theme::DARK.selected_bg))
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            assert!(!selected.is_empty());
            assert!(selected.windows(2).all(|pair| pair[1] == pair[0] + 1));
            assert!(
                lines[selected[selected.len() - 1] + 1].spans.is_empty(),
                "wrapped selected record must be separated from the next record"
            );
        }
    }

    #[test]
    fn agents_panel_render_keeps_content_and_highlight_inside_padding() {
        use ratatui::backend::TestBackend;

        for (terminal_width, terminal_height) in [(156, 40), (44, 24)] {
            let panel = agents_panel_fixture();
            let backend = TestBackend::new(terminal_width, terminal_height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| render_agents_panel(frame, &panel, crate::theme::DARK))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let top = buffer
                .content
                .chunks(usize::from(terminal_width))
                .position(|row| row.iter().any(|cell| cell.symbol() == "┌"))
                .expect("agents panel top border") as u16;
            let top_row = &buffer.content[usize::from(top * terminal_width)
                ..usize::from((top + 1) * terminal_width)];
            let left = top_row
                .iter()
                .position(|cell| cell.symbol() == "┌")
                .expect("agents panel left border") as u16;
            let right = top_row
                .iter()
                .rposition(|cell| cell.symbol() == "┐")
                .expect("agents panel right border") as u16;

            let rows = buffer.content.chunks(usize::from(terminal_width));
            let content_rows = rows
                .skip(usize::from(top + 1))
                .take_while(|row| row[usize::from(left)].symbol() != "└")
                .collect::<Vec<_>>();
            assert!(content_rows[0][usize::from(left + 1)..usize::from(right)]
                .iter()
                .all(|cell| cell.symbol() == " "));
            assert!(content_rows
                .last()
                .expect("agents panel bottom padding row")
                [usize::from(left + 1)..usize::from(right)]
                .iter()
                .all(|cell| cell.symbol() == " "));
            assert!(content_rows
                .iter()
                .all(|row| row[usize::from(left + 1)].symbol() == " "));
            assert!(content_rows
                .iter()
                .all(|row| row[usize::from(right - 1)].symbol() == " "));

            let highlighted = content_rows
                .iter()
                .filter(|row| {
                    row.iter().any(|cell| cell.bg == crate::theme::DARK.selected_bg)
                })
                .collect::<Vec<_>>();
            assert!(!highlighted.is_empty());
            assert!(highlighted.iter().all(|row| {
                row[usize::from(left + 1)].bg != crate::theme::DARK.selected_bg
                    && row[usize::from(left + 2)].bg == crate::theme::DARK.selected_bg
                    && row[usize::from(right - 1)].bg != crate::theme::DARK.selected_bg
            }));
        }
    }

}
