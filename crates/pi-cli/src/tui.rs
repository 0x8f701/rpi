use std::borrow::Cow;
use std::cell::Cell;
use std::collections::hash_map::DefaultHasher;
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
    terminal::{disable_raw_mode, enable_raw_mode, window_size},
};
use futures_util::StreamExt;
use pi_agent::{AgentEvent, ThinkingLevel};
use pi_ai::{AssistantMessageEvent, ContentBlock, Message, Model};
use pi_coding::{
    Application, ApplicationEvent, CONFIG_DIR_NAME, DoubleEscapeAction, ExtensionUiRequest,
    LoopEvent, LoopTask, Session, StreamingBehavior, TodoPhase, TodoStatus, ToolCallViewStatus,
    UiNotificationLevel, UiSelectOption, UiWidgetPlacement,
};
use ratatui::{
    Terminal,
    TerminalOptions, Viewport,
    backend::CrosstermBackend,
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
    BUILTIN_COMMANDS, CommandSource, InteractiveCommand,
    executable_catalog as interactive_commands, expand_resource_command,
};
use crate::job_card_adapter::{
    JobCardPresentationAdapter, JobCardRowRole, JobCardRows,
};
use crate::keybindings::{Action, KeyBindingsManager};
use crate::agents_panel::{AgentsPanel, AgentsPanelAction};
use crate::markdown::ratatui::{
    MarkdownRatatuiStyles, render_ratatui_markdown, render_ratatui_markdown_streaming,
};
use crate::process_commands::{ProcessPanel, ProcessPanelAction, render_process_panel};
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
    ToolCardPresentationAdapter, ToolCardRowRole, ToolCardRows,
};
use crate::tree_panel::{TreePanel, TreePanelMode};
use crate::settings_panel::{SettingsControl, SettingsPanel};

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
                        if let Some(first) = raw_paste_character(key) {
                            let mut payload = first.to_string();
                            let mut deferred = None;
                            let mut rejected = false;
                            loop {
                                match tokio::time::timeout(
                                    std::time::Duration::from_millis(10),
                                    input.next(),
                                )
                                .await
                                {
                                    Ok(Some(Ok(Event::Key(next))))
                                        if next.kind != KeyEventKind::Release =>
                                    {
                                        if let Some(character) = raw_paste_character(next) {
                                            if !rejected
                                                && payload.len() + character.len_utf8()
                                                    <= MAX_PASTE_BYTES
                                            {
                                                payload.push(character);
                                            } else {
                                                rejected = true;
                                                payload.clear();
                                                state.status = format!(
                                                    "Paste rejected: input exceeds the {} MiB limit",
                                                    MAX_PASTE_BYTES / (1024 * 1024)
                                                );
                                            }
                                        } else {
                                            deferred = Some(next);
                                            break;
                                        }
                                    }
                                    Ok(Some(Ok(Event::Paste(text)))) => {
                                        if !rejected && payload.len() + text.len() <= MAX_PASTE_BYTES {
                                            payload.push_str(&text);
                                        } else {
                                            rejected = true;
                                            payload.clear();
                                            state.status = format!(
                                                "Paste rejected: input exceeds the {} MiB limit",
                                                MAX_PASTE_BYTES / (1024 * 1024)
                                            );
                                        }
                                    }
                                    Ok(Some(Ok(_))) => break,
                                    Ok(Some(Err(error))) => return Err(error.into()),
                                    Ok(None) | Err(_) => break,
                                }
                            }
                            if !rejected {
                                if payload.chars().count() > 1 {
                                    handle_paste(&mut state, &payload);
                                } else if handle_key(&application, &mut state, key, &mut terminal).await? {
                                    state.cancel_extension_dialogs();
                                    return Ok(());
                                }
                            }
                            if let Some(key) = deferred
                                && handle_key(&application, &mut state, key, &mut terminal).await?
                            {
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
                    Ok(event) => state.apply(event),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                        state.refresh_job_projection(&application);
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
            _ = animation.tick(), if state.is_streaming => { state.animation_frame = state.animation_frame.wrapping_add(1); }
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
    execute!(writer, EnableBracketedPaste, Hide)
}

fn release_terminal(writer: &mut impl Write) -> io::Result<()> {
    execute!(writer, DisableBracketedPaste, Show)
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
        })
    }

    fn commit_settled(&mut self, state: &mut TuiState) -> Result<()> {
        self.terminal.autoresize()?;
        let size = self.terminal.size()?;
        let transcript_rows = transcript_region_height(state, size.height);
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
        state.finish_commit(entries.len());
        Ok(())
    }

    fn resize_live_viewport(&mut self, _state: &TuiState) -> Result<()> {
        self.terminal.autoresize()?;
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

    /// Temporarily yield raw input and the cursor to a shell operation. The
    /// inline viewport stays on the normal screen and committed scrollback is
    /// never cleared.
    /// Clear only ratatui's current inline viewport. `Terminal::clear` maps an
    /// inline viewport to ClearType::AfterCursor from the viewport origin, so
    /// committed normal-screen rows above it remain durable scrollback.
    fn clear_live_viewport(&mut self) -> Result<()> {
        self.terminal.clear()?;
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
        self.record_undo();
        self.break_action_chain();
        self.lines[self.row].insert(self.column, character);
        self.column += character.len_utf8();
    }

    fn insert_newline(&mut self) {
        self.record_undo();
        self.break_action_chain();
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
        self.record_undo();
        self.break_action_chain();
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
    job_card: Option<JobCardRows>,
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
    streaming_text: String,
    streaming_thinking: String,
    thinking_level: ThinkingLevel,
    is_streaming: bool,
    animation_frame: usize,
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
    expand_tools: bool,
    transcript_scroll: usize,
    transcript_page_rows: Cell<usize>,
    show_images: bool,
    image_width_cells: u16,
    status: String,
    model: String,
    cwd: String,
    completions: CompletionState,
    themes: ThemeManager,
    keybindings: KeyBindingsManager,
    cwd_path: PathBuf,
    pending_attachments: Vec<ContentBlock>,
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
    agents_panel: Option<AgentsPanel>,
    scoped_models: Option<Vec<Model>>,
    session_selector: Option<SavedSessionSelector>,
    scoped_model_selector: Option<ScopedModelSelector>,
    todo_phases: Vec<TodoPhase>,
    /// Extension-owned working indicator text; authoritative for host queries.
    extension_working_message: Option<String>,
    extension_working_visible: bool,
    extension_hidden_thinking_label: Option<String>,
    extension_title: Option<String>,
    active_loops: std::collections::BTreeMap<String, LoopTask>,
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
            thinking_level: session.thinking_level(),
            streaming_text: String::new(),
            streaming_thinking: String::new(),
            is_streaming: false,
            animation_frame: 0,
            pending_user_echo: false,
            show_thinking: runtime_settings.show_thinking,
            double_escape_action: runtime_settings.double_escape_action,
            last_escape: None,
            expand_tools: false,
            transcript_scroll: 0,
            transcript_page_rows: Cell::new(1),
            show_images: runtime_settings.show_images,
            image_width_cells: runtime_settings.image_width_cells,
            status: "Enter submit · Shift+Enter/Ctrl+J newline · Esc abort · Ctrl+D quit"
                .to_owned(),
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
            agents_panel: None,
            scoped_models: initial_scoped_models,
            session_selector: None,
            scoped_model_selector: None,
            todo_phases: application.todo_state().phases,
            extension_working_message: None,
            extension_working_visible: false,
            extension_hidden_thinking_label: None,
            extension_title: None,
            active_loops: std::collections::BTreeMap::new(),
        };
        state.sync_extension_host_bindings();
        for message in session.history() {
            state.push_message(message);
        }
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
        self.tool_cards.apply_agent_event(event);
        let tool_call_id = match event {
            AgentEvent::ToolExecutionStart { tool_call_id, .. }
            | AgentEvent::ToolExecutionUpdate { tool_call_id, .. }
            | AgentEvent::ToolExecutionEnd { tool_call_id, .. } => Some(tool_call_id.as_str()),
            AgentEvent::MessageEnd {
                message: Message::ToolResult(result),
            } => Some(result.tool_call_id.as_str()),
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

    fn apply_orchestration_event(&mut self, event: pi_coding::OrchestrationEvent) {
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
        let mut cards = self.job_cards.cards_in_source_order();
        if let (Some(card), Some(aggregate)) = (cards.last_mut(), self.job_cards.aggregate_row()) {
            card.rows.push(aggregate);
        }
        for card in cards {
            let job_id = card.job_id.clone();
            let is_partial = matches!(
                card.job_status,
                pi_coding::JobStatus::Queued | pi_coding::JobStatus::Running
            );
            let is_error = card.job_status == pi_coding::JobStatus::Failed;
            let entry = TranscriptEntry {
                kind: TranscriptKind::Job,
                content: Vec::new(),
                tool_name: None,
                tool_card: None,
                job_card: Some(card),
                is_error,
                is_partial,
            };
            if let Some(existing) = self.transcript.iter_mut().find(|entry| {
                entry
                    .job_card
                    .as_ref()
                    .is_some_and(|card| card.job_id == job_id)
            }) {
                *existing = entry;
            } else {
                self.transcript.push(entry);
            }
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
                    && pi_coding::loop_message_view(custom).is_some()
                {
                    self.push_loop_message(custom);
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
            ApplicationEvent::Loop(event) => self.apply_loop_event(event),
            _ => {}
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

    /// Refresh the todo panel from canonical state. Display-only: never
    /// mutates the canonical todo state.
    fn refresh_todo_display(&mut self, application: &Application) {
        self.todo_phases = application.todo_state().phases;
    }

    /// Clear the todo panel display without touching canonical state. Used on
    /// session lifecycle transitions where the canonical list is reset.
    fn clear_todo_display(&mut self) {
        self.todo_phases.clear();
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
        self.committed_entries = 0;
        self.transcript_scroll = 0;
        self.transcript_page_rows.set(1);
        self.streaming_text.clear();
        self.streaming_thinking.clear();
        self.is_streaming = false;
        self.pending_user_echo = false;
        self.reset_tool_projection();
        self.job_cards.clear();
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
        self.push_entry(TranscriptEntry {
            kind: TranscriptKind::System,
            content: vec![ContentBlock::text(text)],
            tool_name: None,
            tool_card: None,
            job_card: None,
            is_error,
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
        let file_prefix =
            file_search::current_at_prefix(&self.editor.lines[row], self.editor.column);
        if let Some(prefix) = file_prefix {
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
        let text = self.editor.text();
        let Some(prefix) = text.strip_prefix('/') else {
            self.completions.clear();
            return;
        };
        if prefix.contains(char::is_whitespace) {
            self.completions.clear();
            return;
        }
        let mut items = self
            .commands
            .iter()
            .filter(|command| fuzzy_match(&command.name, prefix))
            .map(|command| CompletionItem {
                value: format!("/{}", command.name),
                label: format!("/{}", command.name),
                description: command.description.clone(),
                is_directory: false,
            })
            .collect::<Vec<_>>();
        items.truncate(MAX_COMPLETIONS);
        self.completions.items = items;
        self.completions.context = Some(CompletionContext::Slash);
        self.completions.selected = self
            .completions
            .selected
            .min(self.completions.items.len().saturating_sub(1));
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
                        self.pending_attachments.push(image.into_content_block());
                        self.status = format!(
                            "Attached {mime_type} ({} KiB) · {} pending",
                            size.div_ceil(1024),
                            self.pending_attachments.len()
                        );
                    }
                    Ok(Some(ClipboardContent::Text(text))) => self.handle_paste(&text),
                    Ok(None) => {
                        self.push_status("Clipboard is empty".to_owned(), true);
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
        self.status = "Reading clipboard".to_owned();
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
        let Some(prefix) = text.strip_prefix('/') else { return false; };
        if prefix.is_empty() || prefix.contains(char::is_whitespace) { return false; }
        let matches = self.commands.iter().filter(|command| command.name.starts_with(prefix)).collect::<Vec<_>>();
        if matches.len() != 1 || matches[0].name == prefix { return false; }
        self.editor.set_text(&format!("/{}", matches[0].name));
        self.refresh_completions();
        true
    }

    fn accept_completion(&mut self) {
        let Some(item) = self.completions.selected().cloned() else {
            return;
        };
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
            "xhigh" | "max" => Some(ThinkingLevel::Xhigh),
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
            _ => {}
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
            }
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
                    }
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
                                    state.refresh_todo_display(application);
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
                _ => {}
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
            KeyCode::Esc => keep_open = false,
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
            _ => {}
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
        KeyCode::Esc => state.panel = None,
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
                PanelValue::Session(_) | PanelValue::ScopedModel(_) => {}
            }
        }
        _ => {}
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
        Some(ProcessPanelAction::Close) => return Ok(true),
        Some(ProcessPanelAction::Open(id)) => match application.process_logs(&id, 0, None, false, None).await {
            Ok(logs) => panel.set_logs(&id, &logs),
            Err(error) => panel.fail(format!("Cannot read process output: {error:#}")),
        },
        Some(ProcessPanelAction::SendText { id, text }) => {
            if let Err(error) = application.process_write(&id, text.into_bytes(), false).await { panel.fail(format!("Cannot send input: {error:#}")); }
        }
        Some(ProcessPanelAction::SendKeys { id, keys }) => {
            if let Err(error) = application.process_send_keys(&id, &keys).await { panel.fail(format!("Cannot send key: {error:#}")); }
        }
        Some(ProcessPanelAction::Resize { id, size }) => {
            if let Err(error) = application.process_resize(&id, size) { panel.fail(format!("Cannot resize terminal: {error:#}")); }
        }
        Some(ProcessPanelAction::Signal { id, signal }) => {
            if let Err(error) = application.process_signal(&id, signal) { panel.fail(format!("Cannot signal process: {error:#}")); }
        }
        Some(ProcessPanelAction::Stop(id)) => match application.process_stop(&id, None).await {
            Ok(process) => panel.update_process(process),
            Err(error) => panel.fail(format!("Cannot stop process: {error:#}")),
        },
        None => {}
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

fn handle_paste(state: &mut TuiState, payload: &str) {
    if handle_extension_dialog_paste(state, payload) {
        return;
    }
    state.handle_paste(payload);
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

    // The completion menu intercepts navigation/accept/abort while open; any
    // other action falls through to normal dispatch (e.g. typing narrows it).
    if !state.completions.items.is_empty() {
        if let Some(action) = action {
            match action {
                Action::EditorSubmit if matches!(state.completions.context, Some(CompletionContext::Slash)) => {
                    state.accept_completion();
                    return Ok(false);
                }
                Action::AcceptCompletion => {
                    state.accept_completion();
                    return Ok(false);
                }
                Action::Abort => {
                    state.cancel_file_completion();
                    state.completion_query = None;
                    state.completions.clear();
                    return Ok(false);
                }
                Action::EditorUp => {
                    state.completions.selected = state.completions.selected.saturating_sub(1);
                    return Ok(false);
                }
                Action::EditorDown => {
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
        Action::EditorUp => state.editor.move_up(),
        Action::EditorDown => state.editor.move_down(),
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
        Action::ClearEditor => state.editor.clear(),
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
            let mut attachments = state.pending_attachments.clone();
            attachments.extend(file_images.iter().cloned());
            if state.is_streaming {
                application.follow_up(expanded.prompt, attachments).await;
                state.status = "Queued follow-up".to_owned();
            } else if let Err(error) = application.prompt(expanded.prompt, attachments, None).await
            {
                state.pending_attachments.extend(file_images);
                state.push_status(format!("Prompt was not accepted: {error}"), true);
                return Ok(false);
            }
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
        "xhigh" | "max" => pi_agent::ThinkingLevel::Xhigh,
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

fn dispatch_goal_command(
    application: &Application,
    state: &mut TuiState,
    argument: Option<&str>,
) -> bool {
    let command = match crate::goal_commands::parse_interactive_goal_command(argument) {
        Ok(command) => command,
        Err(error) => {
            state.status = format!("{error:#}");
            return false;
        }
    };
    match crate::goal_commands::execute_interactive_goal_command(application, command) {
        Ok(output) => {
            state.status = output;
            true
        }
        Err(error) => {
            state.status = format!("Goal command failed: {error:#}");
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
        }
        return Ok(false);
    }
    if state.pending_attachments.is_empty()
        && let Some(command) = prompt.strip_prefix('/')
    {
        let mut parts = command.trim().splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or("");
        let arg = parts.next().map(str::trim).filter(|s| !s.is_empty());
        if arg.is_none()
            && let Some(command) = BUILTIN_COMMANDS.iter().find(|command| command.name == name)
            && command.argument_hint.is_some_and(|hint| hint.contains('<'))
        {
            state.status = format!("Usage: /{} {}", command.name, command.argument_hint.unwrap_or_default());
            return Ok(false);
        }
        match name {
            "quit" | "exit" => return Ok(true),
            "copy" => state.start_copy(application),
            "new" if !state.is_streaming => match application.new_session().await {
                Ok(()) => {
                    state.replace_transcript_from_application(application);
                    state.clear_todo_display();
                    state.status = "Started a new session".to_owned();
                }
                Err(error) => state.push_status(format!("Failed to start new session: {error:#}"), true),
            },
            "settings" => state.open_settings_panel(application),
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
                            state.refresh_todo_display(application);
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
                    let markdown = pi_coding::todo_phases_to_markdown(&application.todo_state().phases);
                    state.push_lines("Todo", markdown, state.themes.theme().accent);
                }
            },
            "changelog" => state.push_lines("Changelog", include_str!("../../../CHANGELOG.md").to_owned(), state.themes.theme().accent),
            "hotkeys" => state.push_lines(
                "Hotkeys",
                "Enter submit · Shift+Enter/Ctrl+J newline · Esc abort · Ctrl+D quit · Ctrl+L model selector · Ctrl+T thinking · Ctrl+O thinking visibility · Alt+Up/Down cycle model · Ctrl+R tools · Ctrl+V paste · Ctrl+S external editor".to_owned(),
                state.themes.theme().accent,
            ),
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
                            state.refresh_todo_display(application);
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
                let help = state.commands.iter().map(|command| format!("/{:<18} {}", command.name, command.description)).collect::<Vec<_>>().join("\n");
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
            "goal" => {
                if !dispatch_goal_command(application, state, arg) {
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
                            Ok(()) => state.push_lines("You", expanded, state.themes.theme().accent),
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
    let mut attachments = state.pending_attachments.clone();
    attachments.extend(file_images.iter().cloned());
    let attachment_count = attachments.len();
    let streaming_behavior = streaming_submit_behavior(state.is_streaming);
    if let Err(error) = application
        .prompt(expanded.prompt, attachments, streaming_behavior)
        .await
    {
        state.pending_attachments.extend(file_images);
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
    state.pending_attachments.clear();
    state.cancel_file_completion();
    state.editor.clear();
    state.completions.clear();
    state.completion_query = None;
    Ok(false)
}

fn live_viewport_height(state: &TuiState, width: u16, terminal_height: u16) -> u16 {
    let theme = state.themes.theme();
    let extension = state.extension_ui.snapshot();
    let above = extension_widget_lines(&extension, UiWidgetPlacement::AboveEditor, theme);
    let below = extension_widget_lines(&extension, UiWidgetPlacement::BelowEditor, theme);
    let attachment_rows = usize::from(!state.pending_attachments.is_empty());
    let editor_rows = state.editor.lines.len().saturating_add(attachment_rows);
    let input_height = u16::try_from(editor_rows.saturating_add(1))
        .unwrap_or(u16::MAX)
        .clamp(2, 9);
    let completion_height = u16::try_from(state.completions.items.len())
        .unwrap_or(u16::MAX)
        .min(u16::try_from(MAX_COMPLETIONS).unwrap_or(u16::MAX));
    let todo_height = u16::try_from(render_todo_panel_lines(&state.todo_phases, theme).len())
        .unwrap_or(u16::MAX)
        .min(8);
    let mut live_lines = Vec::new();
    for entry in &state.transcript[state.committed_entries..] {
        render_transcript_entry(
            &mut live_lines,
            entry,
            state.show_thinking,
            state.expand_tools,
            theme,
            width.max(1),
        );
    }
    if !state.streaming_thinking.is_empty() || !state.streaming_text.is_empty() {
        let mut content = Vec::new();
        if !state.streaming_thinking.is_empty() {
            content.push(ContentBlock::thinking(state.streaming_thinking.clone()));
        }
        if !state.streaming_text.is_empty() {
            content.push(ContentBlock::text(state.streaming_text.clone()));
        }
        render_transcript_entry(
            &mut live_lines,
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
            width.max(1),
        );
    }
    let transcript_height = u16::try_from(wrapped_line_count(&live_lines, width.max(1)))
        .unwrap_or(u16::MAX)
        .min(8);
    let overlays_open = state.panel.is_some()
        || state.tree_panel.is_some()
        || state.process_panel.is_some()
        || state.agents_panel.is_some()
        || state.session_selector.is_some()
        || state.scoped_model_selector.is_some()
        || state.extension_dialog.is_some();
    let overlay_height = if overlays_open { terminal_height.min(16) } else { 0 };
    transcript_height
        .saturating_add(todo_height)
        .saturating_add(u16::try_from(above.len()).unwrap_or(u16::MAX).min(6))
        .saturating_add(completion_height)
        .saturating_add(input_height)
        .saturating_add(u16::try_from(below.len()).unwrap_or(u16::MAX).min(6))
        .saturating_add(1)
        .max(overlay_height)
        .clamp(3, terminal_height.max(3))
        .min(if width < 40 { terminal_height } else { terminal_height.min(16) })
}

fn transcript_region_height(state: &TuiState, terminal_height: u16) -> u16 {
    let theme = state.themes.theme();
    let extension = state.extension_ui.snapshot();
    let above_height = u16::try_from(
        extension_widget_lines(&extension, UiWidgetPlacement::AboveEditor, theme).len(),
    )
    .unwrap_or(u16::MAX)
    .min(6);
    let below_height = u16::try_from(
        extension_widget_lines(&extension, UiWidgetPlacement::BelowEditor, theme).len(),
    )
    .unwrap_or(u16::MAX)
    .min(6);
    let input_height = if state.editor.lines.len() <= 1 && state.pending_attachments.is_empty() {
        2
    } else {
        u16::try_from(state.editor.lines.len().saturating_add(2))
            .unwrap_or(u16::MAX)
            .clamp(3, 10)
    };
    let completion_height = u16::try_from(state.completions.items.len())
        .unwrap_or(u16::MAX)
        .min(u16::try_from(MAX_COMPLETIONS).unwrap_or(u16::MAX));
    let todo_height = u16::try_from(render_todo_panel_lines(&state.todo_phases, theme).len())
        .unwrap_or(u16::MAX)
        .min(8);
    terminal_height
        .saturating_sub(todo_height)
        .saturating_sub(above_height)
        .saturating_sub(completion_height)
        .saturating_sub(input_height)
        .saturating_sub(below_height)
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
    let content_rows = max_rows.saturating_sub(2).max(1);
    let (editor_lines, _) = visible_editor_lines(state, inner.saturating_sub(3), content_rows);
    composer_border_lines_with_editor(state, width, theme, editor_lines)
}


fn composer_border_lines(state: &TuiState, width: u16, theme: Theme) -> Vec<Line<'static>> {
    let inner = usize::from(width.saturating_sub(2));
    let total_rows = state.editor.lines.iter().map(|line| wrapped_row_count(&clean_terminal_text(line), inner.saturating_sub(3))).sum::<usize>();
    let (editor_lines, _) = visible_editor_lines(state, inner.saturating_sub(3), total_rows.max(1));
    composer_border_lines_with_editor(state, width, theme, editor_lines)
}

fn composer_border_lines_with_editor(state: &TuiState, width: u16, theme: Theme, editor_lines: Vec<String>) -> Vec<Line<'static>> {
    let inner = usize::from(width.saturating_sub(2));
    let model = clean_terminal_text(&state.model);
    let cwd = compact_cwd(&state.cwd);
    let thinking = state.effective_thinking_state().label;
    let status = if state.is_streaming {
        const FRAMES: &[&str] = &["◐", "◓", "◑", "◒"];
        format!("working {} ▶──", FRAMES[state.animation_frame % FRAMES.len()])
    } else {
        "▶──".to_owned()
    };
    let header_width = usize::from(display_width("── π  > ⬢ "))
        + usize::from(display_width(&model))
        + usize::from(display_width(&format!(" · ◑ {thinking} > 📁 ")))
        + usize::from(display_width(&cwd))
        + usize::from(display_width(" > ⟲ "))
        + usize::from(display_width(&status));
    let mut lines = if header_width <= inner {
        let top_fill = "─".repeat(inner - header_width);
        vec![Line::from(vec![
            Span::styled("╭── ", Style::default().fg(theme.muted).bg(theme.user_message_bg)),
            Span::styled("π", Style::default().fg(theme.accent).bg(theme.user_message_bg).add_modifier(Modifier::BOLD)),
            Span::styled("  > ⬢ ", Style::default().fg(theme.border).bg(theme.user_message_bg)),
            Span::styled(model, Style::default().fg(theme.accent).bg(theme.user_message_bg)),
            Span::styled(format!(" · ◑ {thinking} > 📁 "), Style::default().fg(theme.accent).bg(theme.user_message_bg)),
            Span::styled(cwd, Style::default().fg(theme.syntax_variable).bg(theme.user_message_bg)),
            Span::styled(" > ⟲ ", Style::default().fg(theme.border).bg(theme.user_message_bg)),
            Span::styled(status, Style::default().fg(theme.muted).bg(theme.user_message_bg)),
            Span::styled(top_fill, Style::default().fg(theme.muted).bg(theme.user_message_bg)),
            Span::styled("╮", Style::default().fg(theme.muted).bg(theme.user_message_bg)),
        ])]
    } else {
        vec![Line::from(vec![
            Span::styled("╭── ", Style::default().fg(theme.muted).bg(theme.user_message_bg)),
            Span::styled("π", Style::default().fg(theme.accent).bg(theme.user_message_bg).add_modifier(Modifier::BOLD)),
            Span::styled(" ", Style::default().bg(theme.user_message_bg)),
            Span::styled("─".repeat(inner.saturating_sub(5)), Style::default().fg(theme.muted).bg(theme.user_message_bg)),
            Span::styled("╮", Style::default().fg(theme.muted).bg(theme.user_message_bg)),
        ])]
    };
    let editor_lines = editor_lines;
    if editor_lines.len() <= 1 && state.pending_attachments.is_empty() && state.completions.items.is_empty() {
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
    if !state.completions.items.is_empty() && editor_lines.len() <= 1 {
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
    lines.push(Line::from(Span::styled(format!("╰{}╯", "─".repeat(inner)), Style::default().fg(theme.border_muted))));
    lines
}

fn render_welcome_lines(state: &TuiState, theme: Theme) -> Vec<Line<'static>> {
    if !state.transcript.is_empty() || !state.streaming_text.is_empty() || !state.streaming_thinking.is_empty() {
        return Vec::new();
    }
    let recent = pi_coding::list_sessions(&state.cwd_path).into_iter().take(3).collect::<Vec<_>>();
    let mut lines = vec![
        Line::default(),
        Line::from(Span::styled("  π  pi-rs", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD))),
        Line::from(Span::styled(format!("  {}", clean_terminal_text(&state.model)), Style::default().fg(theme.muted))),
        Line::default(),
        Line::from(Span::styled("  Start typing to begin · /help for commands · @file to attach context", Style::default().fg(theme.text))),
    ];
    if !recent.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled("  Recent sessions", Style::default().fg(theme.muted).add_modifier(Modifier::BOLD))));
        for session in recent {
            let label = session.name.as_deref().unwrap_or(&session.id);
            lines.push(Line::from(Span::styled(format!("  • {}", clean_terminal_text(label)), Style::default().fg(theme.text))));
        }
    }
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
    let above_height = u16::try_from(above.len()).unwrap_or(u16::MAX).min(6);
    let below_height = u16::try_from(below.len()).unwrap_or(u16::MAX).min(6);
    let completion_height = u16::try_from(state.completions.items.len())
        .unwrap_or(u16::MAX)
        .min(u16::try_from(MAX_COMPLETIONS).unwrap_or(u16::MAX));
    let todo_lines = render_todo_panel_lines(&state.todo_phases, theme);
    let todo_height = u16::try_from(todo_lines.len()).unwrap_or(u16::MAX).min(8);
    let composer_height = u16::try_from(
        state
            .editor
            .lines
            .iter()
            .map(|line| wrapped_row_count(&clean_terminal_text(line), usize::from(frame.area().width.saturating_sub(5))))
            .sum::<usize>()
            .saturating_add(2),
    )
    .unwrap_or(u16::MAX);
    let reserved = todo_height.saturating_add(above_height).saturating_add(below_height).saturating_add(completion_height);
    let max_composer_height = frame.area().height.saturating_sub(reserved).max(2);
    let input_height = composer_height.min(max_composer_height);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(todo_height),
            Constraint::Length(above_height),
            Constraint::Length(input_height),
            Constraint::Length(completion_height),
            Constraint::Length(below_height),
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
    let overlays_open = state.panel.is_some()
        || state.tree_panel.is_some()
        || state.process_panel.is_some()
        || state.settings_panel.is_some()
        || state.agents_panel.is_some()
        || state.session_selector.is_some()
        || state.scoped_model_selector.is_some()
        || state.extension_dialog.is_some();
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
    if todo_height > 0 {
        frame.render_widget(
            Paragraph::new(Text::from(todo_lines))
                .style(Style::default().fg(theme.text))
                .wrap(Wrap { trim: false }),
            sections[1],
        );
    }
    if above_height > 0 {
        frame.render_widget(Paragraph::new(above), sections[2]);
    }
    let composer_lines = composer_border_lines_bounded(state, sections[3].width, theme, usize::from(sections[3].height));
    frame.render_widget(Paragraph::new(composer_lines), sections[3]);
    if !state.completions.items.is_empty() {
        let lines = state.completions.items.iter().enumerate().map(|(index, item)| {
            let selected = index == state.completions.selected;
            Line::from(vec![
                Span::styled(if selected { "❯ " } else { "  " }, Style::default().fg(if selected { theme.accent } else { theme.dim })),
                Span::styled(clean_terminal_text(&item.label), Style::default().fg(if selected { theme.text } else { theme.muted }).add_modifier(if selected { Modifier::BOLD } else { Modifier::empty() })),
                Span::styled(format!("  {}", clean_terminal_text(&item.description)), Style::default().fg(theme.muted)),
            ])
        }).collect::<Vec<_>>();
        let completion_area = Rect { x: sections[4].x.saturating_add(2), y: sections[4].y, width: sections[4].width.saturating_sub(3), height: sections[4].height };
        frame.render_widget(Paragraph::new(lines), completion_area);
    }
    let editor_width = usize::from(sections[3].width.saturating_sub(5));
    let (_, cursor_column) = editor_wrapped_position(state, editor_width);
    let (_, visible_cursor_row) = visible_editor_lines(state, editor_width, usize::from(sections[3].height.saturating_sub(2)).max(1));
    let cursor_x = sections[3]
        .x
        .saturating_add(if state.completions.items.is_empty() && state.editor.lines.len() <= 1 { 3 } else { 2 })
        .saturating_add(cursor_column);
    let cursor_y = sections[3].y.saturating_add(1).saturating_add(u16::try_from(visible_cursor_row).unwrap_or(u16::MAX));
    if state.extension_dialog.is_none()
        && state.process_panel.is_none()
        && state.settings_panel.is_none()
        && state.agents_panel.is_none()
        && cursor_x < sections[3].right().saturating_sub(1)
        && cursor_y < sections[3].bottom()
    {
        frame.set_cursor_position((cursor_x, cursor_y));
    }
    if below_height > 0 {
        frame.render_widget(Paragraph::new(below), sections[5]);
    }
    if let Some(panel) = &state.settings_panel { render_settings_panel(frame, panel, state.settings_value_input.as_ref(), theme); }
    if let Some(panel) = &state.panel { render_selector_panel(frame, panel, theme); }
    if let Some(panel) = &state.tree_panel { render_tree_panel(frame, panel, theme); }
    if let Some(panel) = &state.process_panel { render_process_panel(frame, panel, theme); }
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
        render_transcript_entry_inner(
            &mut lines,
            entry,
            state.show_thinking,
            state.expand_tools,
            theme,
            width,
            Some(image_context),
        );
    }
    lines
}

fn render_job_card(lines: &mut Vec<Line<'static>>, card: &JobCardRows, theme: Theme) {
    let background = match card.job_status {
        pi_coding::JobStatus::Queued | pi_coding::JobStatus::Running => theme.tool_pending_bg,
        pi_coding::JobStatus::Completed => theme.tool_success_bg,
        pi_coding::JobStatus::Failed | pi_coding::JobStatus::Cancelled => theme.tool_error_bg,
    };
    for row in &card.rows {
        let (prefix, color, modifier) = match row.role {
            JobCardRowRole::Title => ("Task ", job_status_color(card.job_status, theme), Modifier::BOLD),
            JobCardRowRole::Description => ("  ", theme.text, Modifier::empty()),
            JobCardRowRole::Timing => ("  ", theme.muted, Modifier::empty()),
            JobCardRowRole::Usage => ("  ", theme.dim, Modifier::empty()),
            JobCardRowRole::Result => ("  ↳ ", theme.tool_output, Modifier::empty()),
            JobCardRowRole::Error => ("  ! ", theme.error, Modifier::empty()),
            JobCardRowRole::Reference => ("  · ", theme.md_link_url, Modifier::empty()),
            JobCardRowRole::Aggregate => ("", theme.muted, Modifier::ITALIC),
        };
        for (index, text) in clean_terminal_text(&row.text).lines().enumerate() {
            let current_prefix = if index == 0 { prefix } else { "    " };
            lines.push(Line::from(vec![
                Span::styled(current_prefix.to_owned(), Style::default().fg(color).bg(background)),
                Span::styled(text.to_owned(), Style::default().fg(color).bg(background).add_modifier(modifier)),
            ]));
        }
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

/// Count tasks that are neither completed nor abandoned across all phases.
fn todo_open_count(phases: &[TodoPhase]) -> usize {
    phases
        .iter()
        .flat_map(|phase| &phase.tasks)
        .filter(|task| !matches!(&task.status, TodoStatus::Completed | TodoStatus::Abandoned))
        .count()
}

/// Build the compact phase/task panel lines. Each phase is a bold header; each
/// task is a marker + content line, with a distinct color per status:
/// pending (dim), in-progress (accent), completed (success), abandoned (muted).
fn render_todo_panel_lines(phases: &[TodoPhase], theme: Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for phase in phases {
        lines.push(Line::from(Span::styled(
            clean_terminal_text(&phase.name),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )));
        for task in &phase.tasks {
            let (marker, color) = todo_status_marker(&task.status, theme);
            lines.push(Line::from(vec![
                Span::styled(format!(" {marker} "), Style::default().fg(color)),
                Span::styled(
                    clean_terminal_text(&task.content),
                    Style::default().fg(theme.text),
                ),
            ]));
        }
    }
    lines
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
    if card.tool_name.eq_ignore_ascii_case("bash") {
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
        let title = if card.arguments_summary.is_empty() { format!("{marker} {}", card.tool_name) } else { format!("{marker} {} {}", card.tool_name, card.arguments_summary) };
        push_tool_box_row(lines, &title, theme.tool_title, border, inner);
    }
    for row in &card.rows {
        if row.role == ToolCardRowRole::Command { continue; }
        let color = match row.role {
            ToolCardRowRole::Command => theme.tool_title,
            ToolCardRowRole::Content => theme.tool_output,
            ToolCardRowRole::Details => theme.dim,
            ToolCardRowRole::Status => if card.is_error { theme.error } else { theme.muted },
            ToolCardRowRole::Error => theme.error,
        };
        for text in clean_terminal_text(&row.text).lines() { push_tool_box_row(lines, text, color, border, inner); }
    }
    if card.truncated { push_tool_box_row(lines, &format!("… {} more lines ⟦Ctrl+O: Expand⟧", card.omitted_content_lines), theme.dim, border, inner); }
    lines.push(Line::from(Span::styled(format!("╰{}╯", "─".repeat(inner)), Style::default().fg(border))));
    lines.push(Line::default());
}

fn push_tool_separator(lines: &mut Vec<Line<'static>>, label: &str, border: Color, inner: usize) {
    let fill = "─".repeat(inner.saturating_sub(label.chars().count().saturating_add(2)));
    lines.push(Line::from(vec![Span::styled("├──", Style::default().fg(border)), Span::styled(label.to_owned(), Style::default().fg(border)), Span::styled(fill, Style::default().fg(border)), Span::styled("┤", Style::default().fg(border))]));
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

fn render_transcript_entry_inner(
    lines: &mut Vec<Line<'static>>,
    entry: &TranscriptEntry,
    show_thinking: bool,
    expand_tools: bool,
    theme: Theme,
    width: u16,
    mut image_context: Option<&mut TranscriptImageContext<'_>>,
) {
    if entry.kind == TranscriptKind::Job {
        if let Some(card) = &entry.job_card {
            render_job_card(lines, card, theme);
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
    for block in &entry.content {
        match block {
            ContentBlock::Text { text, .. } => {
                if text.trim().is_empty() {
                    continue;
                }
                let base = match entry.kind {
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
                    for line in &mut rendered {
                        line.spans.insert(
                            0,
                            Span::styled("  ", Style::default().fg(theme.accent)),
                        );
                    }
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
    let sort = match selector.sort() {
        SessionSort::Newest => "newest",
        SessionSort::Name => "name",
    };
    let named = if selector.named_only() {
        "named"
    } else {
        "all"
    };
    let mut lines = vec![
        Line::from(Span::styled(
            "Resume Session",
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!(
                "Ctrl+N {named} · Ctrl+P path · Ctrl+S sort:{sort} · Ctrl+R rename · Ctrl+D delete"
            ),
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


fn render_agents_panel(frame: &mut ratatui::Frame<'_>, panel: &AgentsPanel, theme: Theme) {
    let lines_data = panel.view_lines();
    let height = u16::try_from(lines_data.len().saturating_add(5))
        .unwrap_or(u16::MAX)
        .clamp(8, 24);
    let area = centered_rect(
        frame.area().width.saturating_sub(4).min(110).max(40),
        height,
        frame.area(),
    );
    frame.render_widget(Clear, area);
    let dirty = if panel.dirty() { " · unsaved" } else { "" };
    let mut lines = vec![
        Line::from(Span::styled(
            format!("{}{dirty}", panel.title()),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            panel.help().to_owned(),
            Style::default().fg(theme.dim),
        )),
    ];
    for row in lines_data {
        let style = if row.selected {
            Style::default().fg(theme.text).bg(theme.selected_bg)
        } else {
            Style::default().fg(theme.text)
        };
        lines.push(Line::from(Span::styled(clean_terminal_text(&row.text), style)));
    }
    if let Some(selected) = panel.selected_row() {
        lines.push(Line::from(Span::styled(
            clean_terminal_text(&selected.description),
            Style::default().fg(theme.dim),
        )));
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
            format!(
                "Enter toggle · Ctrl+A all · Ctrl+X clear · Ctrl+P provider · Alt+↑/↓ reorder · Ctrl+S save{dirty}"
            ),
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
        lines.push(Line::from(Span::styled(
            "↑↓ move  Enter select  Esc close",
            Style::default().fg(theme.dim),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "↑↓ move  ← fold  → unfold  Enter select  Alt+D/T/U/L/A filters  Alt+Shift+L label  Esc close",
            Style::default().fg(theme.dim),
        )));
        lines.push(Line::from(vec![
            Span::styled("Type to search: ", Style::default().fg(theme.dim)),
            Span::styled(clean_terminal_text(&panel.query), Style::default().fg(theme.text)),
        ]));
    }
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

fn render_selector_panel(frame: &mut ratatui::Frame<'_>, panel: &SelectorPanel, theme: Theme) {
    let visible = panel.visible_indices();
    let height = u16::try_from(visible.len().saturating_add(4))
        .unwrap_or(u16::MAX)
        .clamp(5, 20);
    let width = frame.area().width.saturating_sub(4).min(76).max(20);
    let area = centered_rect(width, height, frame.area());
    let mut lines = vec![Line::from(vec![
        Span::styled("Filter: ", Style::default().fg(theme.dim)),
        Span::styled(
            clean_terminal_text(&panel.query),
            Style::default().fg(theme.text),
        ),
    ])];
    for (visible_index, item_index) in visible.into_iter().enumerate() {
        let item = &panel.items[item_index];
        let marker = if item.checked { "✓" } else { " " };
        let style = if visible_index == panel.selected {
            Style::default().fg(theme.text).bg(theme.selected_bg)
        } else {
            Style::default().fg(theme.text)
        };
        lines.push(Line::from(Span::styled(
            format!(
                " {marker} {}  {}",
                clean_terminal_text(&item.label),
                clean_terminal_text(&item.description)
            ),
            style,
        )));
    }
    lines.push(Line::from(Span::styled(
        clean_terminal_text(&panel.help),
        Style::default().fg(theme.dim),
    )));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
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
        editor.insert_char('界');
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
            raw_paste_character(KeyEvent::new(KeyCode::Char('界'), KeyModifiers::NONE)),
            Some('界')
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
    fn paste_event_normalizes_multiline_crlf_and_preserves_cjk() {
        let mut state = todo_test_state(Vec::new());
        handle_paste(&mut state, "first\r\n界🙂\rthird");
        assert_eq!(state.editor.text(), "first\n界🙂\nthird");
        assert_eq!((state.editor.row, state.editor.column), (2, "third".len()));
        assert!(state.transcript.is_empty(), "pasting must not submit a message");
    }

    #[test]
    fn paste_event_inserts_7608_plus_characters_once_and_undoes_once() {
        let mut state = todo_test_state(Vec::new());
        let payload = "x".repeat(8_193);
        handle_paste(&mut state, &payload);
        assert_eq!(state.editor.text(), payload);
        assert_eq!(state.editor.undo.len(), 1);
        state.editor.undo();
        assert!(state.editor.is_empty());
    }

    #[test]
    fn oversize_paste_is_rejected_without_mutating_the_draft() {
        let mut state = todo_test_state(Vec::new());
        state.editor.set_text("keep");
        let undo_entries = state.editor.undo.len();
        handle_paste(&mut state, &"x".repeat(MAX_PASTE_BYTES + 1));
        assert_eq!(state.editor.text(), "keep");
        assert_eq!(state.editor.undo.len(), undo_entries);
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
        editor.insert_text("one 界界, two\n次 line");
        editor.move_word_left();
        assert_eq!((editor.row, editor.column), (1, "次 ".len()));
        editor.delete_word_backward();
        assert_eq!(editor.text(), "one 界界, two\nline");
        editor.move_word_left();
        editor.delete_word_backward();
        assert_eq!(editor.text(), "one 界界, \nline");
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
        editor.insert_text("a界\nβ界c");
        editor.move_home();
        editor.begin_jump(JumpDirection::Backward);
        assert!(editor.jump_to_char('界'));
        assert_eq!((editor.row, editor.column), (0, 1));
        editor.begin_jump(JumpDirection::Forward);
        assert!(editor.jump_to_char('界'));
        assert_eq!((editor.row, editor.column), (1, 'β'.len_utf8()));
        assert!(editor.lines[editor.row].is_char_boundary(editor.column));
    }

    #[test]
    fn editor_histories_are_bounded() {
        let mut editor = EditorState::new();
        for _ in 0..(MAX_UNDO_HISTORY + 5) {
            editor.insert_char('x');
        }
        assert_eq!(editor.undo.len(), MAX_UNDO_HISTORY);
        editor.break_action_chain();
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
    fn slash_completion_fuzzy_matches_and_accepts_selection() {
        let (background_tx, _background_rx) = mpsc::unbounded_channel();
        let mut state = TuiState {
            tool_cards: ToolCardPresentationAdapter::new(),
            job_cards: JobCardPresentationAdapter::new(),
            transcript: Vec::new(),
            committed_entries: 0,
            editor: EditorState::new(),
            streaming_text: String::new(),
            streaming_thinking: String::new(),
            thinking_level: ThinkingLevel::Off,
            is_streaming: false,
            animation_frame: 0,
            pending_user_echo: false,
            show_thinking: true,
            double_escape_action: DoubleEscapeAction::Tree,
            last_escape: None,
            expand_tools: false,
            transcript_scroll: 0,
            transcript_page_rows: Cell::new(1),
            show_images: true,
            image_width_cells: 50,
            status: String::new(),
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
            settings_value_input: None,
            tree_panel: None,
            process_panel: None,
            agents_panel: None,
            scoped_models: None,
            session_selector: None,
            scoped_model_selector: None,
            todo_phases: Vec::new(),
            extension_working_message: None,
            extension_working_visible: false,
            extension_hidden_thinking_label: None,
            extension_title: None,
            active_loops: std::collections::BTreeMap::new(),
        };
        state.editor.insert_char('/');
        state.editor.insert_char('h');
        state.editor.insert_char('p');
        state.refresh_completions();
        assert_eq!(state.completions.items[0].value, "/help");
        state.accept_completion();
        assert_eq!(state.editor.text(), "/help");
        assert!(state.completions.items.is_empty());
    }

    #[test]
    fn slash_completion_includes_dynamic_commands_with_descriptions() {
        let (background_tx, _background_rx) = mpsc::unbounded_channel();
        let mut state = TuiState {
            tool_cards: ToolCardPresentationAdapter::new(),
            job_cards: JobCardPresentationAdapter::new(),
            transcript: Vec::new(),
            committed_entries: 0,
            editor: EditorState::new(),
            streaming_text: String::new(),
            streaming_thinking: String::new(),
            thinking_level: ThinkingLevel::Off,
            is_streaming: false,
            animation_frame: 0,
            pending_user_echo: false,
            show_thinking: true,
            double_escape_action: DoubleEscapeAction::Tree,
            last_escape: None,
            expand_tools: false,
            transcript_scroll: 0,
            transcript_page_rows: Cell::new(1),
            show_images: true,
            image_width_cells: 50,
            status: String::new(),
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
            commands: vec![InteractiveCommand {
                name: "skill:release".to_owned(),
                description: "Prepare a release".to_owned(),
                source: CommandSource::Skill,
            }],
            panel: None,
            settings_panel: None,
            settings_value_input: None,
            tree_panel: None,
            process_panel: None,
            agents_panel: None,
            scoped_models: None,
            session_selector: None,
            scoped_model_selector: None,
            todo_phases: Vec::new(),
            extension_working_message: None,
            extension_working_visible: false,
            extension_hidden_thinking_label: None,
            extension_title: None,
            active_loops: std::collections::BTreeMap::new(),
        };
        state.editor.insert_text("/srl");
        state.refresh_completions();
        assert_eq!(state.completions.items[0].value, "/skill:release");
        assert_eq!(state.completions.items[0].description, "Prepare a release");
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
        let source = "# Heading\n\n1. ordered\n   - [x] nested\n\n| 名称 | 状態 |\n| --- | ---: |\n| 東京 | ✅ |\n\n[docs](https://example.test)\n\n```rust\nlet place = \"東京\";\n```\n\n```mermaid\nflowchart LR\nA --> B\n```\n\n```mermaid\nsequenceDiagram\nA->>B: fallback\n```";
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
    fn streaming_assistant_matches_shared_tail_semantics_without_prefix_duplication() {
        let width = 32;
        let source = "# Stable\n\nmutable tail\n\n| 名称 | 状態 |\n| --- | --- |\n| 東京 | ✅ |";
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
        assert!(plain.iter().any(|line| line.contains("東京")));
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
        assert!(lines[0].spans[0].content.starts_with("  "));
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

        let mut bash = todo_test_state(Vec::new());
        let body = (1..=30).map(|line| line.to_string()).collect::<Vec<_>>().join("\n");
        bash.apply(ApplicationEvent::Agent(AgentEvent::ToolExecutionStart { tool_call_id: "bash".to_owned(), tool_name: "bash".to_owned(), arguments: serde_json::json!({"command": "seq 1 30"}) }));
        bash.apply(ApplicationEvent::Agent(AgentEvent::ToolExecutionEnd { tool_call_id: "bash".to_owned(), tool_name: "bash".to_owned(), result: AgentToolResult::text(body), is_error: false }));
        let entry = bash.transcript.last().unwrap();
        let mut compact = Vec::new();
        render_transcript_entry(&mut compact, entry, true, false, crate::theme::DARK, 80);
        let compact_text = compact.iter().flat_map(|line| &line.spans).map(|span| span.content.as_ref()).collect::<String>();
        assert!(compact_text.contains("$ seq 1 30"));
        assert!(compact_text.contains("… 11 more lines ⟦Ctrl+O: Expand⟧"));
        assert!(!compact_text.contains("bash done"));
        let mut expanded = Vec::new();
        render_transcript_entry(&mut expanded, entry, true, true, crate::theme::DARK, 80);
        let expanded_text = expanded.iter().flat_map(|line| &line.spans).map(|span| span.content.as_ref()).collect::<String>();
        assert!(expanded_text.contains("1"));
        assert!(!expanded_text.contains("Ctrl+O: Expand"));
        assert!(compact.iter().filter(|line| line.spans.iter().map(|span| span.content.as_ref()).collect::<String>().starts_with('╭')).count() == 1);
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

    #[tokio::test]
    async fn goal_dispatch_avoids_model_turn_and_preserves_editor_on_usage_error() {
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
        state.editor.set_text("/goal create --tokens nope keep this");
        assert!(!dispatch_goal_command(
            &application,
            &mut state,
            Some("create --tokens nope keep this"),
        ));
        assert_eq!(state.editor.text(), "/goal create --tokens nope keep this");
        assert!(state.status.contains("positive integer"));
        assert!(session.history().is_empty());

        assert!(dispatch_goal_command(
            &application,
            &mut state,
            Some("create --tokens 20 ship cleanly"),
        ));
        assert!(state.status.contains("active · 0/20 tokens · ship cleanly"));
        assert!(session.history().is_empty(), "goal command must not run the agent");
        application.cleanup().await;
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
        let lines = vec![Line::raw("界界界"), Line::default()];
        assert_eq!(wrapped_line_count(&lines, 4), 3);
    }

    #[test]
    fn compact_arguments_truncates_on_character_boundaries() {
        let argument = "界".repeat(80);
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
            streaming_text: String::new(),
            streaming_thinking: String::new(),
            thinking_level: ThinkingLevel::Off,
            is_streaming: false,
            animation_frame: 0,
            pending_user_echo: false,
            show_thinking: true,
            double_escape_action: DoubleEscapeAction::Tree,
            last_escape: None,
            expand_tools: false,
            transcript_scroll: 0,
            transcript_page_rows: Cell::new(1),
            show_images: true,
            image_width_cells: 50,
            status: String::new(),
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
            settings_value_input: None,
            tree_panel: None,
            process_panel: None,
            agents_panel: None,
            scoped_models: None,
            session_selector: None,
            scoped_model_selector: None,
            todo_phases: phases,
            extension_working_message: None,
            extension_working_visible: false,
            extension_hidden_thinking_label: None,
            extension_title: None,
            active_loops: std::collections::BTreeMap::new(),
        }
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
        assert_eq!(card.job_status, pi_coding::JobStatus::Completed);
        assert_eq!(card.agent_status, Some(pi_coding::AgentStatus::Parked));
        assert!(card.rows[0].text.contains("completed · parked"));

        let mut rendered = Vec::new();
        render_job_card(&mut rendered, card, crate::theme::DARK);
        let text = rendered
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("inspect the workspace"));
        assert!(text.contains("completed · parked"));
    }

    #[test]
    fn todo_panel_renders_all_four_status_markers() {
        use pi_coding::{TodoItem, TodoPhase, TodoStatus};
        let theme = crate::theme::DARK;
        let phases = vec![TodoPhase {
            name: "Plan".to_owned(),
            tasks: vec![
                TodoItem {
                    content: "pending".to_owned(),
                    status: TodoStatus::Pending,
                },
                TodoItem {
                    content: "active".to_owned(),
                    status: TodoStatus::InProgress,
                },
                TodoItem {
                    content: "done".to_owned(),
                    status: TodoStatus::Completed,
                },
                TodoItem {
                    content: "dropped".to_owned(),
                    status: TodoStatus::Abandoned,
                },
            ],
        }];
        let lines = render_todo_panel_lines(&phases, theme);
        // one bold phase header + one line per task
        assert_eq!(lines.len(), 5);
        let markers: Vec<&str> = lines
            .iter()
            .skip(1)
            .map(|line| {
                line.spans
                    .first()
                    .map(|span| span.content.as_ref())
                    .unwrap_or("")
            })
            .collect();
        assert_eq!(markers, vec![" ○ ", " ► ", " ✓ ", " ✗ "]);
        let colors: Vec<Option<Color>> = lines
            .iter()
            .skip(1)
            .map(|line| line.spans.first().and_then(|span| span.style.fg))
            .collect();
        assert_eq!(
            colors,
            vec![
                Some(theme.dim),
                Some(theme.accent),
                Some(theme.success),
                Some(theme.muted)
            ]
        );
    }

    #[test]
    fn todo_open_count_excludes_terminal_statuses() {
        use pi_coding::{TodoItem, TodoPhase, TodoStatus};
        let phases = vec![TodoPhase {
            name: "P".to_owned(),
            tasks: vec![
                TodoItem {
                    content: "a".to_owned(),
                    status: TodoStatus::Pending,
                },
                TodoItem {
                    content: "b".to_owned(),
                    status: TodoStatus::InProgress,
                },
                TodoItem {
                    content: "c".to_owned(),
                    status: TodoStatus::Completed,
                },
                TodoItem {
                    content: "d".to_owned(),
                    status: TodoStatus::Abandoned,
                },
            ],
        }];
        assert_eq!(todo_open_count(&phases), 2);
    }

    #[test]
    fn clear_todo_display_empties_panel_without_canonical_mutation() {
        use pi_coding::{TodoItem, TodoPhase, TodoStatus};
        let phases = vec![TodoPhase {
            name: "Plan".to_owned(),
            tasks: vec![TodoItem {
                content: "x".to_owned(),
                status: TodoStatus::InProgress,
            }],
        }];
        let mut state = todo_test_state(phases);
        assert!(!state.todo_phases.is_empty());
        // clear_todo_display takes no Application handle, so it cannot reach
        // canonical state — it only resets the display buffer.
        state.clear_todo_display();
        assert!(state.todo_phases.is_empty());
        assert_eq!(
            render_todo_panel_lines(&state.todo_phases, crate::theme::DARK).len(),
            0
        );
    }

    #[test]
    fn todo_updated_and_reminder_refresh_display_only() {
        use pi_coding::{TodoCompletionTransition, TodoItem, TodoPhase, TodoStatus};
        let phases = vec![TodoPhase {
            name: "Plan".to_owned(),
            tasks: vec![TodoItem {
                content: "x".to_owned(),
                status: TodoStatus::InProgress,
            }],
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
        assert_eq!(live_viewport_height(&state, 80, 24), 4);

        state.panel = Some(SelectorPanel {
            title: "Models".to_owned(),
            help: String::new(),
            items: Vec::new(),
            selected: 0,
            query: String::new(),
        });
        assert_eq!(live_viewport_height(&state, 80, 24), 16);
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
        state.editor.set_text("界🙂abcdefghij界🙂abcdefghij界🙂abcdefghij");
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
        state.editor.set_text("/lo"); assert!(!state.accept_unambiguous_command_prefix()); assert_eq!(state.editor.text(), "/lo");
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
    fn animation_changes_only_while_streaming() {
        let mut state = todo_test_state(Vec::new());
        state.is_streaming = true;
        let first = composer_border_lines(&state, 90, crate::theme::DARK)[0].spans.iter().map(|span| span.content.as_ref()).collect::<String>();
        state.animation_frame = 1;
        let second = composer_border_lines(&state, 90, crate::theme::DARK)[0].spans.iter().map(|span| span.content.as_ref()).collect::<String>();
        assert_ne!(first, second);
        state.is_streaming = false;
        let idle_first = composer_border_lines(&state, 90, crate::theme::DARK)[0].spans.iter().map(|span| span.content.as_ref()).collect::<String>();
        state.animation_frame = 3;
        let idle_second = composer_border_lines(&state, 90, crate::theme::DARK)[0].spans.iter().map(|span| span.content.as_ref()).collect::<String>();
        assert_eq!(idle_first, idle_second);
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
        state.cwd = "/home/test/Downloads".to_owned();
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
    fn bounded_composer_materializes_only_visible_rows_for_large_and_cjk_pastes() {
        let mut state = todo_test_state(Vec::new());
        state.editor.set_text(&format!("{}{}", "x".repeat(7_608), "界".repeat(200)));
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
}
