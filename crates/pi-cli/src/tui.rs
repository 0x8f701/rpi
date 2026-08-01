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
    event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode, window_size},
};
use futures_util::StreamExt;
use pi_agent::{AgentEvent, ThinkingLevel};
use pi_ai::{AssistantMessageEvent, ContentBlock, Message, Model};
use pi_coding::{
    Application, ApplicationEvent, CONFIG_DIR_NAME, DoubleEscapeAction, ExtensionUiRequest,
    LoopEvent, LoopTask, Session, TodoPhase, TodoStatus, UiNotificationLevel, UiSelectOption,
    UiWidgetPlacement,
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
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
use crate::keybindings::{Action, KeyBindingsManager};
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
use crate::tree_panel::{TreePanel, TreePanelMode};

const MAX_TRANSCRIPT_LINES: usize = 4_000;
const MAX_COMPLETIONS: usize = 7;


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
    if let Some(prompt) = initial_prompts.first() {
        let expanded = crate::file_args::expand_prompt(prompt, &state.cwd_path)?;
        state.push_lines("You", prompt.clone(), state.themes.theme().accent);
        application.prompt(expanded.prompt, expanded.images, None).await?;
        for prompt in initial_prompts.into_iter().skip(1) {
            let expanded = crate::file_args::expand_prompt(&prompt, &state.cwd_path)?;
            application.follow_up(expanded.prompt, expanded.images).await;
        }
    }
    let mut update_notice = Some(Box::pin(crate::self_update::startup_notice()));
    let mut theme_watch = tokio::time::interval(std::time::Duration::from_millis(250));

    loop {
        terminal.draw(|frame, images| render(frame, &state, images))?;
        tokio::select! {
            terminal_event = input.next() => {
                let Some(terminal_event) = terminal_event else {
                    state.cancel_extension_dialogs();
                    return Ok(());
                };
                match terminal_event? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        if handle_key(&application, &mut state, key, &mut terminal).await? {
                            state.cancel_extension_dialogs();
                            return Ok(());
                        }
                    }
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            }
            application_event = events.recv() => {
                match application_event {
                    Ok(event) => state.apply(event),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => state.push_status(format!("UI skipped {count} stale events"), true),
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
            _ = theme_watch.tick() => { state.poll_theme_reload(); state.reconcile_extension_dialog(); }
            _ = shutdown.recv() => {
                // SIGTERM/SIGHUP (Unix): restore the terminal and exit
                // cleanly. The signal handler itself only signals tokio's
                // self-pipe (async-signal-safe); all terminal IO happens
                // here in normal async context, never in the handler.
                state.cancel_extension_dialogs();
                restore_terminal();
                return Ok(());
            }
        }
    }
}

/// Whether the TUI currently owns the terminal (raw mode + alternate screen
/// + hidden cursor). Set on enter/re-enter, cleared on restore. RPC/JSON and
/// print modes never set this, so terminal restoration is a no-op for them —
/// structured-output modes never acquire a TUI guard.
static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Restore the terminal to cooked mode + the main screen exactly once per
/// active TUI epoch. The atomic swap is the idempotency latch: the first
/// caller (Drop, the panic hook, or the async signal-exit branch) wins and
/// performs the restoration; every later caller observes `false` and does
/// nothing. Safe to call concurrently from the panic hook and Drop.
fn restore_terminal() {
    if TUI_ACTIVE.swap(false, Ordering::SeqCst) {
        let mut stdout = io::stdout();
        // Clear inlined Kitty graphics before leaving the alt screen so a
        // panic/signal-driven cleanup doesn't strand image state. Detection
        // is env-only (non-blocking — safe in the panic path); non-Kitty
        // terminals ignore the APC escape. Runs exactly once via the
        // TUI_ACTIVE latch, before disable_raw_mode / Show / LeaveAlternateScreen.
        if crate::terminal_images::detect_protocol(
            &crate::terminal_images::TerminalEnvironment::current(),
        ) == Some(crate::terminal_images::TerminalImageProtocol::Kitty) {
            let _ = stdout.write_all(crate::terminal_images::KITTY_DELETE_ALL);
        }
        let _ = disable_raw_mode();
        let _ = execute!(stdout, Show, LeaveAlternateScreen);
        let _ = stdout.flush();
    }
}

/// Install a process-wide panic hook that restores the terminal before the
/// panic message is printed, so a panicking TUI never strands the user in raw
/// mode / alternate screen. Idempotent. No-op for non-TUI paths: `TUI_ACTIVE`
/// is false there, so the hook forwards to the previous hook unchanged.
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
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, Hide) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        let terminal = match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => terminal,
            Err(error) => {
                let mut stdout = io::stdout();
                let _ = execute!(stdout, Show, LeaveAlternateScreen);
                let _ = disable_raw_mode();
                return Err(error.into());
            }
        };
        TUI_ACTIVE.store(true, Ordering::SeqCst);
        Ok(Self { terminal, images: TerminalImageRenderer::default() })
    }

    fn draw(
        &mut self,
        render: impl FnOnce(&mut ratatui::Frame<'_>, &mut TerminalImageRenderer) -> ImageDrawPlan,
    ) -> Result<()> {
        let mut plan = None;
        self.terminal.draw(|frame| plan = Some(render(frame, &mut self.images)))?;
        let plan = plan.expect("render closure always produces an image plan");
        self.images.present(self.terminal.backend_mut(), plan.identity, &plan.placements)?;
        Ok(())
    }

    /// Temporarily yield the terminal (cooked mode + main screen + visible
    /// cursor) for a sub-operation such as `/login`, `/logout`, or an external
    /// editor. Pairs with [`reacquire_from_shell`]. Marks the TUI inactive so a
    /// concurrent restore (panic/signal) is a no-op while the terminal is
    /// already yielded — the cursor is shown and the alternate screen left
    /// exactly once across normal/panic/suspend paths.
    fn yield_to_shell(&mut self) -> Result<()> {
        self.images.cleanup(self.terminal.backend_mut())?;
        // Clear inlined Kitty graphics before yielding to the cooked shell so
        // a sub-operation (external editor/viewer) and the shell don't see
        // stale image state. Same env-only Kitty gate as restore_terminal;
        // flushed by the subsequent execute!(...). Runs once per yield.
        if crate::terminal_images::detect_protocol(
            &crate::terminal_images::TerminalEnvironment::current(),
        ) == Some(crate::terminal_images::TerminalImageProtocol::Kitty) {
            let _ = self
                .terminal
                .backend_mut()
                .write_all(crate::terminal_images::KITTY_DELETE_ALL);
        }
        disable_raw_mode()?;
        execute!(self.terminal.backend_mut(), Show, LeaveAlternateScreen)?;
        TUI_ACTIVE.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// Re-enter raw mode + alternate screen after [`yield_to_shell`].
    fn reacquire_from_shell(&mut self) -> Result<()> {
        enable_raw_mode()?;
        execute!(self.terminal.backend_mut(), EnterAlternateScreen, Hide)?;
        self.terminal.clear()?;
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
        let normalized = if text.contains('\r') {
            Cow::Owned(text.replace("\r\n", "\n").replace('\r', "\n"))
        } else {
            Cow::Borrowed(text)
        };
        self.record_undo();
        self.break_action_chain();
        self.insert_text_internal(&normalized);
    }

    fn insert_text_internal(&mut self, text: &str) -> EditorPosition {
        let tail = self.lines[self.row].split_off(self.column);
        let mut pieces = text.split('\n').peekable();
        let first = pieces.next().unwrap_or_default();
        self.lines[self.row].push_str(first);
        self.column += first.len();
        if pieces.peek().is_none() {
            self.lines[self.row].push_str(&tail);
            return EditorPosition {
                row: self.row,
                column: self.column,
            };
        }
        while let Some(piece) = pieces.next() {
            self.row += 1;
            if pieces.peek().is_none() {
                self.column = piece.len();
                self.lines.insert(self.row, format!("{piece}{tail}"));
            } else {
                self.lines.insert(self.row, piece.to_owned());
            }
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
}

#[derive(Clone)]
struct TranscriptEntry {
    kind: TranscriptKind,
    content: Vec<ContentBlock>,
    tool_name: Option<String>,
    is_error: bool,
    is_partial: bool,
}

struct TuiState {
    transcript: Vec<TranscriptEntry>,
    editor: EditorState,
    streaming_text: String,
    streaming_thinking: String,
    is_streaming: bool,
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
    tree_panel: Option<TreePanel>,
    scoped_models: Option<Vec<Model>>,
    session_selector: Option<SavedSessionSelector>,
    scoped_model_selector: Option<ScopedModelSelector>,
    todo_phases: Vec<TodoPhase>,
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
            editor: EditorState::new(),
            streaming_text: String::new(),
            streaming_thinking: String::new(),
            is_streaming: false,
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
            tree_panel: None,
            scoped_models: initial_scoped_models,
            session_selector: None,
            scoped_model_selector: None,
            todo_phases: application.todo_state().phases,
            active_loops: std::collections::BTreeMap::new(),
        };
        for message in session.history() {
            state.push_message(message);
        }
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
            ExtensionUiEvent::ExtensionCleared { instance } => {
                if self
                    .extension_dialog
                    .as_ref()
                    .is_some_and(|dialog| dialog.instance() == &instance)
                {
                    self.extension_dialog = None;
                }
            }
            ExtensionUiEvent::Notification { .. }
            | ExtensionUiEvent::StatusChanged { .. }
            | ExtensionUiEvent::StatusCleared { .. }
            | ExtensionUiEvent::WidgetChanged { .. }
            | ExtensionUiEvent::WidgetCleared { .. }
            | ExtensionUiEvent::TitleChanged { .. } => {}
        }
    }

    fn reconcile_extension_dialog(&mut self) {
        let Some(dialog) = &self.extension_dialog else {
            return;
        };
        if !self
            .extension_ui
            .pending_interactions()
            .iter()
            .any(|pending| pending.id == dialog.interaction.id)
        {
            self.extension_dialog = None;
        }
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

    fn apply(&mut self, event: ApplicationEvent) {
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
            ApplicationEvent::Agent(AgentEvent::ToolExecutionStart {
                tool_name,
                arguments,
                ..
            }) => self.push_tool(tool_name, arguments, Vec::new(), false, true),
            ApplicationEvent::Agent(AgentEvent::ToolExecutionUpdate {
                tool_name,
                arguments,
                partial_result,
                ..
            }) => self.push_tool(tool_name, arguments, partial_result.content, false, true),
            ApplicationEvent::Agent(AgentEvent::ToolExecutionEnd {
                tool_name,
                result,
                is_error,
                ..
            }) => self.push_tool(
                tool_name,
                serde_json::Value::Null,
                result.content,
                is_error,
                false,
            ),
            ApplicationEvent::Agent(AgentEvent::MessageEnd { message }) => {
                if matches!(message, Message::Assistant(_)) {
                    self.streaming_text.clear();
                    self.streaming_thinking.clear();
                }
                self.push_message(message);
            }
            ApplicationEvent::Session(pi_coding::SessionEvent::BashExecutionEnd { message }) => {
                self.push_message(Message::BashExecution(message));
                self.status = "Ready".to_owned();
            }
            ApplicationEvent::Session(pi_coding::SessionEvent::BashExecutionUpdate { .. }) => {}
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

    fn push_message(&mut self, message: Message) {
        match message {
            Message::User(message) => self.push_entry(TranscriptEntry {
                kind: TranscriptKind::User,
                content: message.content,
                tool_name: None,
                is_error: false,
                is_partial: false,
            }),
            Message::Assistant(message) => self.push_entry(TranscriptEntry {
                kind: TranscriptKind::Assistant,
                content: message.content,
                tool_name: None,
                is_error: false,
                is_partial: false,
            }),
            Message::ToolResult(message) => self.push_entry(TranscriptEntry {
                kind: TranscriptKind::Tool,
                content: message.content,
                tool_name: Some(message.tool_name),
                is_error: message.is_error,
                is_partial: false,
            }),
            Message::BashExecution(message) => self.push_entry(TranscriptEntry {
                kind: TranscriptKind::Tool,
                content: vec![ContentBlock::text(pi_ai::bash_execution_to_text(&message))],
                tool_name: Some("Bash".to_owned()),
                is_error: message.cancelled || message.exit_code.is_some_and(|code| code != 0),
                is_partial: false,
            }),
            Message::Custom(message) => {
                if message.display {
                    self.push_entry(TranscriptEntry {
                        kind: TranscriptKind::Custom,
                        content: message.content.into_blocks(),
                        tool_name: Some(message.custom_type),
                        is_error: false,
                        is_partial: false,
                    });
                }
            }
            Message::BranchSummary(message) => self.push_entry(TranscriptEntry {
                kind: TranscriptKind::System,
                content: vec![ContentBlock::text(message.summary)],
                tool_name: Some("Branch summary".to_owned()),
                is_error: false,
                is_partial: false,
            }),
            Message::CompactionSummary(message) => self.push_entry(TranscriptEntry {
                kind: TranscriptKind::System,
                content: vec![ContentBlock::text(message.summary)],
                tool_name: Some("Compaction summary".to_owned()),
                is_error: false,
                is_partial: false,
            }),
        }
    }

    fn push_lines(&mut self, label: &str, text: String, _color: Color) {
        self.push_entry(TranscriptEntry {
            kind: if label == "You" {
                TranscriptKind::User
            } else {
                TranscriptKind::System
            },
            content: vec![ContentBlock::text(text)],
            tool_name: None,
            is_error: false,
            is_partial: false,
        });
    }

    fn push_status(&mut self, text: String, is_error: bool) {
        self.push_entry(TranscriptEntry {
            kind: TranscriptKind::System,
            content: vec![ContentBlock::text(text)],
            tool_name: None,
            is_error,
            is_partial: false,
        });
    }

    fn push_tool(
        &mut self,
        tool_name: String,
        arguments: serde_json::Value,
        mut content: Vec<ContentBlock>,
        is_error: bool,
        is_partial: bool,
    ) {
        self.transcript.retain(|entry| {
            !(entry.kind == TranscriptKind::Tool
                && entry.is_partial
                && entry.tool_name.as_deref() == Some(&tool_name))
        });
        let summary = compact_arguments(&arguments);
        if !summary.is_empty() {
            content.insert(0, ContentBlock::text(summary));
        }
        self.push_entry(TranscriptEntry {
            kind: TranscriptKind::Tool,
            content,
            tool_name: Some(tool_name),
            is_error,
            is_partial,
        });
    }

    fn push_entry(&mut self, entry: TranscriptEntry) {
        self.transcript.push(entry);
        self.trim_transcript();
        self.follow_transcript();
    }

    fn trim_transcript(&mut self) {
        if self.transcript.len() > MAX_TRANSCRIPT_LINES {
            let excess = self.transcript.len() - MAX_TRANSCRIPT_LINES;
            self.transcript.drain(..excess);
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
    }

    fn toggle_thinking(&mut self) {
        self.show_thinking = !self.show_thinking;
    }

    fn poll_theme_reload(&mut self) {
        let reload = self.themes.reload_if_changed();
        for diagnostic in reload.diagnostics {
            self.push_status(format!("Theme reload ignored: {diagnostic}"), true);
        }
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
                    Ok(Some(ClipboardContent::Text(text))) => {
                        self.editor.insert_text(&text);
                        self.status = "Pasted clipboard text".to_owned();
                        self.refresh_completions();
                    }
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
        self.tree_panel = None;
        self.scoped_model_selector = None;
        self.session_selector = Some(SavedSessionSelector::new(
            pi_coding::list_sessions(&self.cwd_path),
            current,
        ));
    }

    async fn open_scoped_models_panel(&mut self) {
        self.panel = None;
        self.tree_panel = None;
        self.session_selector = None;
        self.scoped_model_selector = Some(ScopedModelSelector::new(
            available_models().await,
            self.scoped_models.clone(),
        ));
    }

    async fn open_settings_panel(&mut self, application: &Application) {
        let state = application.state().await;
        self.panel = Some(SelectorPanel {
            title: "Settings".to_owned(),
            help: "Enter change · Esc close".to_owned(),
            items: vec![
                PanelItem {
                    label: "Thinking level".to_owned(),
                    description: crate::output::thinking_level_str(state.thinking_level).to_owned(),
                    value: PanelValue::SettingsThinking,
                    checked: false,
                },
                PanelItem {
                    label: "Theme".to_owned(),
                    description: self.themes.active_name().to_owned(),
                    value: PanelValue::SettingsTheme,
                    checked: false,
                },
                PanelItem {
                    label: "Automatic compaction".to_owned(),
                    description: if state.auto_compaction_enabled {
                        "enabled"
                    } else {
                        "disabled"
                    }
                    .to_owned(),
                    value: PanelValue::SettingsAutoCompact,
                    checked: state.auto_compaction_enabled,
                },
            ],
            selected: 0,
            query: String::new(),
        });
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
                        state.transcript.clear();
                        for message in application.messages() {
                            state.push_message(message);
                        }
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
                            state.transcript.clear();
                            for message in application.messages() {
                                state.push_message(message);
                            }
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
                        match application.switch_session(&path).await {
                            Ok(()) => {
                                keep_open = false;
                                state.transcript.clear();
                                for message in application.messages() {
                                    state.push_message(message);
                                }
                                state.refresh_todo_display(application);
                                state.status = format!("Resumed {}", path.display());
                            }
                            Err(error) => state
                                .push_status(format!("Failed to resume session: {error:#}"), true),
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
                        Ok(()) => {
                            state.model = reference;
                            state.panel = None;
                            state.status = format!("Model: {}", state.model);
                        }
                        Err(error) => {
                            state.push_status(format!("Cannot switch model: {error:#}"), true)
                        }
                    }
                }
                PanelValue::Thinking(level) => {
                    application.set_thinking_level(level);
                    state.panel = None;
                    state.status =
                        format!("Thinking: {}", crate::output::thinking_level_str(level));
                }
                PanelValue::SettingsThinking => state.open_thinking_panel(application).await,
                PanelValue::SettingsTheme => {
                    state.themes.cycle(1);
                    state.status = format!("Theme: {}", state.themes.active_name());
                    state.open_settings_panel(application).await;
                }
                PanelValue::SettingsAutoCompact => {
                    let enabled = !application.state().await.auto_compaction_enabled;
                    application.set_auto_compaction_enabled(enabled);
                    state.status = format!(
                        "Automatic compaction {}",
                        if enabled { "enabled" } else { "disabled" }
                    );
                    state.open_settings_panel(application).await;
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
    if let Some(exit) = handle_tree_panel_key(application, state, key).await? {
        return Ok(exit);
    }
    if let Some(exit) = handle_session_selector_key(application, state, key).await? {
        return Ok(exit);
    }
    if let Some(exit) = handle_scoped_model_selector_key(application, state, key)? {
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
            let expanded = match crate::file_args::expand_prompt(&prompt, &state.cwd_path) {
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
                        state.transcript.clear();
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
    application.set_thinking_level(next);
    state.status = format!(
        "Thinking level: {}",
        crate::output::thinking_level_str(next)
    );
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
        Ok(()) => {
            state.model = reference;
            state.status = format!("Switched to {}", state.model);
        }
        Err(error) => state.push_status(format!("Cannot switch model: {error:#}"), true),
    }
}
async fn submit(
    application: &Application,
    state: &mut TuiState,
    terminal: &mut TerminalGuard,
) -> Result<bool> {
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
        match name {
            "quit" | "exit" => return Ok(true),
            "copy" => state.start_copy(application),
            "new" if !state.is_streaming => match application.new_session().await {
                Ok(()) => {
                    state.transcript.clear();
                    state.clear_todo_display();
                    state.status = "Started a new session".to_owned();
                }
                Err(error) => state.push_status(format!("Failed to start new session: {error:#}"), true),
            },
            "settings" => state.open_settings_panel(application).await,
            "model" if arg.is_none() => state.open_model_panel(application).await,
            "model" => {
                let spec = arg.expect("guarded");
                match crate::commands::resolve_model_spec(spec) {
                    Ok((model, _)) => {
                        let reference = format!("{}/{}", model.provider, model.id);
                        match application.set_model_with_resolved_auth(model).await {
                            Ok(()) => {
                                state.model = reference;
                                state.status = format!("Model: {}", state.model);
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
            "resume-codex" => match arg {
                Some(input) => match crate::commands::import_codex_for_resume(input) {
                    Ok(path) => match application.switch_session(&path).await {
                        Ok(()) => {
                            state.transcript.clear();
                            for message in application.messages() { state.push_message(message); }
                            state.refresh_todo_display(application);
                            state.status = format!("Imported and resumed {}", path.display());
                        }
                        Err(error) => state.push_status(format!("Imported Codex session could not be resumed: {error:#}"), true),
                    },
                    Err(error) => state.push_status(format!("Codex import failed: {error:#}"), true),
                },
                None => state.push_status("Usage: /resume-codex <path|id>".to_owned(), true),
            },
            "scoped-models" => state.open_scoped_models_panel().await,
            "resume" if arg.is_none() => state.open_session_panel(application).await,
            "fork" => state.open_fork_panel(application),
            "tree" => state.open_tree_panel(application),
            "trust" => state.open_trust_panel(application),
            "resume" => {
                let path = Path::new(arg.expect("guarded"));
                match application.switch_session(path).await {
                    Ok(()) => {
                        state.transcript.clear();
                        for message in application.messages() { state.push_message(message); }
                        state.refresh_todo_display(application);
                        state.status = format!("Resumed {}", path.display());
                    }
                    Err(error) => state.push_status(format!("Failed to resume session: {error:#}"), true),
                }
            }
            "clone" if !state.is_streaming => match application.clone_session().await {
                Ok(()) => state.status = "Cloned current session branch".to_owned(),
                Err(error) => state.push_status(format!("Failed to clone session: {error:#}"), true),
            }
            "loop" => {
                let parsed = pi_coding::parse_loop_args(arg.unwrap_or_default());
                let Some(interval) = parsed.interval else {
                    state.push_status(pi_coding::loop_usage_message().to_owned(), true);
                    return Ok(false);
                };
                match application
                    .loop_create(pi_coding::LoopCreateRequest::immediate(interval, parsed.prompt))
                    .await
                {
                    Ok(task) => state.status = format!(
                        "Loop {} scheduled {} · expires {}",
                        task.id,
                        task.human_schedule(),
                        task.expires_at.to_rfc3339()
                    ),
                    Err(error) => state.push_status(format!("Failed to schedule loop: {error}"), true),
                }
            }
            "loops" => match application.loop_list().await {
                Ok(tasks) if tasks.is_empty() => state.push_status("No active loops".to_owned(), false),
                Ok(tasks) => {
                    let listing = tasks
                        .iter()
                        .map(|task| format!(
                            "{}  {}  next {}  {}",
                            task.id,
                            task.human_schedule(),
                            task.next_fire_at().to_rfc3339(),
                            task.prompt
                        ))
                        .collect::<Vec<_>>()
                        .join("\n");
                    state.push_lines("Loops", listing, state.themes.theme().accent);
                }
                Err(error) => state.push_status(format!("Failed to list loops: {error}"), true),
            },
            "loop-cancel" => match arg {
                Some(task_id) => match application.loop_cancel(task_id).await {
                    Ok(true) => state.status = format!("Cancelled loop {task_id}"),
                    Ok(false) => state.push_status(format!("No active loop with id {task_id}"), true),
                    Err(error) => state.push_status(format!("Failed to cancel loop: {error}"), true),
                },
                None => state.push_status("Usage: /loop-cancel <id>".to_owned(), true),
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
                            state.transcript.clear();
                            for message in application.messages() { state.push_message(message); }
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
    let expanded = match crate::file_args::expand_prompt(&prompt, &state.cwd_path) {
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
    if let Err(error) = application.prompt(expanded.prompt, attachments, None).await {
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
    state.pending_attachments.clear();
    state.cancel_file_completion();
    state.editor.clear();
    state.completions.clear();
    state.completion_query = None;
    Ok(false)
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
    let attachment_rows = usize::from(!state.pending_attachments.is_empty());
    let editor_rows = state.editor.lines.len().saturating_add(attachment_rows);
    let input_height = u16::try_from(editor_rows.saturating_add(2))
        .unwrap_or(u16::MAX)
        .clamp(3, 10);
    let completion_height = u16::try_from(state.completions.items.len())
        .unwrap_or(u16::MAX)
        .min(u16::try_from(MAX_COMPLETIONS).unwrap_or(u16::MAX));
    let todo_lines = render_todo_panel_lines(&state.todo_phases, theme);
    let todo_height = u16::try_from(todo_lines.len()).unwrap_or(u16::MAX).min(8);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(todo_height),
            Constraint::Length(above_height),
            Constraint::Length(completion_height),
            Constraint::Length(input_height),
            Constraint::Length(below_height),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let title = extension.title.as_deref().unwrap_or("pi (rs)");
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {} ", clean_terminal_text(title)),
                Style::default()
                    .fg(theme.text)
                    .bg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    " {} · {}",
                    clean_terminal_text(&state.model),
                    clean_terminal_text(&state.cwd)
                ),
                Style::default().fg(theme.text),
            ),
        ])),
        sections[0],
    );

    let cell_size = window_size().ok().and_then(|size| {
        (size.columns > 0 && size.rows > 0 && size.width > 0 && size.height > 0).then_some(
            TerminalCellSize {
                width_pixels: size.width / size.columns,
                height_pixels: size.height / size.rows,
            },
        )
    }).unwrap_or_default();
    let mut image_candidates = Vec::new();
    let mut image_context = TranscriptImageContext {
        renderer: images,
        candidates: &mut image_candidates,
        config: ImageDisplayConfig {
            show_images: state.show_images,
            width_cells: state.image_width_cells,
        },
        viewport_columns: sections[1].width.saturating_sub(2),
        viewport_rows: sections[1].height.saturating_sub(2),
        cell_size,
    };
    let mut transcript = render_transcript_lines(state, theme, &mut image_context);
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
                is_error: false,
                is_partial: true,
            },
            state.show_thinking,
            state.expand_tools,
            theme,
            None,
        );
    }
    let transcript_height = usize::from(sections[1].height.saturating_sub(2));
    state.transcript_page_rows.set(transcript_height.max(1));
    let transcript_width = sections[1].width.saturating_sub(2).max(1);
    let total_rows = wrapped_line_count(&transcript, transcript_width);
    let paragraph = Paragraph::new(Text::from(transcript.clone()))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.border))
                .title(if state.transcript_scroll == 0 {
                    " Conversation ".to_owned()
                } else {
                    format!(
                        " Conversation · {} rows above latest ",
                        state.transcript_scroll
                    )
                }),
        )
        .style(Style::default().fg(theme.text))
        .wrap(Wrap { trim: false });
    let bottom = total_rows.saturating_sub(transcript_height);
    let scroll = bottom.saturating_sub(state.transcript_scroll.min(bottom));
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
                        sections[1].x.saturating_add(1),
                        sections[1].y.saturating_add(1).saturating_add(
                            u16::try_from(visible_row).unwrap_or(u16::MAX),
                        ),
                    ))
            })
            .collect()
    };
    frame.render_widget(
        paragraph.scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)),
        sections[1],
    );

    if todo_height > 0 {
        let open = todo_open_count(&state.todo_phases);
        let title = format!(" Tasks · {open} open ");
        frame.render_widget(
            Paragraph::new(Text::from(todo_lines))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(theme.border))
                        .title(title),
                )
                .style(Style::default().fg(theme.text))
                .wrap(Wrap { trim: false }),
            sections[2],
        );
    }
    if above_height > 0 {
        frame.render_widget(Paragraph::new(above), sections[3]);
    }
    if !state.completions.items.is_empty() {
        let lines = state
            .completions
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let style = if index == state.completions.selected {
                    Style::default().fg(theme.text).bg(theme.selected_bg)
                } else {
                    Style::default().fg(theme.muted)
                };
                Line::from(Span::styled(
                    format!(
                        " {}  {}",
                        clean_terminal_text(&item.label),
                        clean_terminal_text(&item.description)
                    ),
                    style,
                ))
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), sections[4]);
    }

    let editor_text = if state.pending_attachments.is_empty() {
        clean_terminal_text(&state.editor.lines.join("\n"))
    } else {
        clean_terminal_text(&format!(
            "[{} image attachment{}]\n{}",
            state.pending_attachments.len(),
            if state.pending_attachments.len() == 1 {
                ""
            } else {
                "s"
            },
            state.editor.lines.join("\n")
        ))
    };
    frame.render_widget(
        Paragraph::new(editor_text)
            .style(Style::default().fg(theme.text))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if state.is_streaming {
                        theme.border_accent
                    } else {
                        theme.border
                    }))
                    .title(if state.is_streaming {
                        " Queue "
                    } else {
                        " Message "
                    }),
            )
            .wrap(Wrap { trim: false }),
        sections[5],
    );
    let cursor_x = sections[5]
        .x
        .saturating_add(1)
        .saturating_add(display_width(
            &state.editor.lines[state.editor.row][..state.editor.column],
        ));
    let cursor_y = sections[5]
        .y
        .saturating_add(1)
        .saturating_add(u16::from(!state.pending_attachments.is_empty()))
        .saturating_add(u16::try_from(state.editor.row).unwrap_or(u16::MAX));
    if state.extension_dialog.is_none()
        && cursor_x < sections[5].right().saturating_sub(1)
        && cursor_y < sections[5].bottom().saturating_sub(1)
    {
        frame.set_cursor_position((cursor_x, cursor_y));
    }
    if below_height > 0 {
        frame.render_widget(Paragraph::new(below), sections[6]);
    }
    let mut status = extension
        .statuses
        .iter()
        .map(|item| clean_terminal_text(&item.text))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>();
    if let Some(notification) = extension.notifications.last() {
        status.push(clean_terminal_text(&notification.message));
    }
    if status.is_empty() {
        status.push(clean_terminal_text(&state.status));
    }
    if !state.active_loops.is_empty() {
        let count = state.active_loops.len();
        status.push(format!(
            "{count} loop{} active",
            if count == 1 { "" } else { "s" }
        ));
    }
    frame.render_widget(
        Paragraph::new(status.join(" · ")).style(Style::default().fg(theme.dim)),
        sections[7],
    );
    if let Some(panel) = &state.panel {
        render_selector_panel(frame, panel, theme);
    }
    if let Some(panel) = &state.tree_panel {
        render_tree_panel(frame, panel, theme);
    }
    if let Some(selector) = &state.session_selector {
        render_saved_session_selector(frame, selector, theme);
    }
    if let Some(selector) = &state.scoped_model_selector {
        render_scoped_model_selector(frame, selector, theme);
    }
    if let Some(dialog) = &state.extension_dialog {
        render_extension_dialog(frame, dialog, theme);
    }
    ImageDrawPlan {
        identity: ImageFrameIdentity {
            viewport_width: sections[1].width,
            viewport_height: sections[1].height,
            theme_hash,
            message_hash,
        },
        placements,
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
    image_context: &mut TranscriptImageContext<'_>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for entry in &state.transcript {
        render_transcript_entry_inner(
            &mut lines,
            entry,
            state.show_thinking,
            state.expand_tools,
            theme,
            Some(image_context),
        );
    }
    lines
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
) {
    render_transcript_entry_inner(lines, entry, show_thinking, expand_tools, theme, None);
}

fn render_transcript_entry_inner(
    lines: &mut Vec<Line<'static>>,
    entry: &TranscriptEntry,
    show_thinking: bool,
    expand_tools: bool,
    theme: Theme,
    mut image_context: Option<&mut TranscriptImageContext<'_>>,
) {
    let (label, label_color, background) = match entry.kind {
        TranscriptKind::User => ("You", theme.accent, Some(theme.user_message_bg)),
        TranscriptKind::Assistant => ("Assistant", theme.success, None),
        TranscriptKind::System => (
            entry
                .tool_name
                .as_deref()
                .unwrap_or(if entry.is_error { "Error" } else { "System" }),
            if entry.is_error {
                theme.error
            } else {
                theme.custom_message_label
            },
            Some(theme.custom_message_bg),
        ),
        TranscriptKind::Custom => (
            entry.tool_name.as_deref().unwrap_or("Custom"),
            theme.custom_message_label,
            Some(theme.custom_message_bg),
        ),
        TranscriptKind::Tool => (
            entry.tool_name.as_deref().unwrap_or("Tool"),
            if entry.is_error {
                theme.error
            } else {
                theme.tool_title
            },
            Some(if entry.is_partial {
                theme.tool_pending_bg
            } else if entry.is_error {
                theme.tool_error_bg
            } else {
                theme.tool_success_bg
            }),
        ),
    };
    let status = if entry.kind == TranscriptKind::Tool {
        if entry.is_partial {
            " …"
        } else if entry.is_error {
            " error"
        } else {
            " done"
        }
    } else {
        ""
    };
    lines.push(Line::from(Span::styled(
        format!("{}{status}", clean_terminal_text(label)),
        Style::default()
            .fg(label_color)
            .add_modifier(Modifier::BOLD),
    )));
    let mut visible_blocks = 0_usize;
    for block in &entry.content {
        match block {
            ContentBlock::Text { text, .. } => {
                if entry.kind == TranscriptKind::Tool && !expand_tools && visible_blocks > 0 {
                    continue;
                }
                let base = match entry.kind {
                    TranscriptKind::User => theme.user_message_text,
                    TranscriptKind::Tool => theme.tool_output,
                    TranscriptKind::System => {
                        if entry.is_error {
                            theme.error
                        } else {
                            theme.custom_message_text
                        }
                    }
                    TranscriptKind::Custom => theme.custom_message_text,
                    TranscriptKind::Assistant => theme.text,
                };
                let mut rendered = if entry.kind == TranscriptKind::Tool {
                    render_tool_text(&clean_terminal_text(text), theme)
                } else {
                    render_markdown(&clean_terminal_text(text), theme, base)
                };
                if entry.kind == TranscriptKind::Tool {
                    apply_tool_line_styles(&mut rendered, theme, background);
                    if !expand_tools && rendered.len() > 1 {
                        rendered.truncate(1);
                        rendered.push(Line::from(Span::styled(
                            "  Ctrl+O to expand",
                            Style::default()
                                .fg(theme.dim)
                                .bg(background.unwrap_or(Color::Reset)),
                        )));
                    }
                } else if let Some(background) = background {
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
                if *redacted || thinking.trim().is_empty() {
                    continue;
                }
                if show_thinking {
                    lines.push(Line::from(Span::styled(
                        "Reasoning",
                        Style::default()
                            .fg(theme.thinking_text)
                            .add_modifier(Modifier::ITALIC),
                    )));
                    let mut rendered =
                        render_markdown(&clean_terminal_text(thinking), theme, theme.thinking_text);
                    for line in &mut rendered {
                        line.style = line.style.add_modifier(Modifier::ITALIC);
                    }
                    lines.extend(rendered);
                } else {
                    lines.push(Line::from(Span::styled(
                        "Reasoning hidden · Ctrl+T to show",
                        Style::default()
                            .fg(theme.thinking_text)
                            .add_modifier(Modifier::ITALIC),
                    )));
                }
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
    lines.push(Line::default());
}

fn render_saved_session_selector(
    frame: &mut ratatui::Frame<'_>,
    selector: &SavedSessionSelector,
    theme: Theme,
) {
    let visible = selector.visible_sessions();
    let height = u16::try_from(visible.len().saturating_add(6))
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
        let path = if selector.show_path() {
            format!(" · {}", session.path.display())
        } else {
            String::new()
        };
        let text = format!(
            "{marker} {} · {} messages{path}",
            session_display_name(session),
            session.messages
        );
        let style = if index == selector.selected() {
            Style::default().fg(theme.text).bg(theme.selected_bg)
        } else {
            Style::default().fg(if session.name.is_some() {
                theme.warning
            } else {
                theme.text
            })
        };
        lines.push(Line::from(Span::styled(clean_terminal_text(&text), style)));
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
            transcript: Vec::new(),
            editor: EditorState::new(),
            streaming_text: String::new(),
            streaming_thinking: String::new(),
            is_streaming: false,
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
            tree_panel: None,
            scoped_models: None,
            session_selector: None,
            scoped_model_selector: None,
            todo_phases: Vec::new(),
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
            transcript: Vec::new(),
            editor: EditorState::new(),
            streaming_text: String::new(),
            streaming_thinking: String::new(),
            is_streaming: false,
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
            tree_panel: None,
            scoped_models: None,
            session_selector: None,
            scoped_model_selector: None,
            todo_phases: Vec::new(),
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
    fn markdown_render_distinguishes_heading_code_syntax_link_and_quote() {
        let lines = render_markdown(
            "# Heading\n\n[docs](https://example.test) and `inline`\n> quote\n```rust\nfn main() { let value = \"ok\"; }\n```",
            crate::theme::DARK,
            crate::theme::DARK.text,
        );
        let styles = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.style.fg))
            .collect::<Vec<_>>();
        assert!(styles.contains(&Some(crate::theme::DARK.md_heading)));
        assert!(styles.contains(&Some(crate::theme::DARK.md_link)));
        assert!(styles.contains(&Some(crate::theme::DARK.md_code)));
        assert!(styles.contains(&Some(crate::theme::DARK.md_quote)));
        assert!(styles.contains(&Some(crate::theme::DARK.syntax_keyword)));
        assert!(styles.contains(&Some(crate::theme::DARK.syntax_string)));
    }

    #[test]
    fn custom_transcript_uses_custom_theme_roles() {
        let entry = TranscriptEntry {
            kind: TranscriptKind::Custom,
            content: vec![ContentBlock::text("extension notice")],
            tool_name: Some("release-note".to_owned()),
            is_error: false,
            is_partial: false,
        };
        let mut lines = Vec::new();
        render_transcript_entry(&mut lines, &entry, true, true, crate::theme::DARK);
        assert_eq!(lines[0].spans[0].content, "release-note");
        assert_eq!(
            lines[0].spans[0].style.fg,
            Some(crate::theme::DARK.custom_message_label)
        );
        assert!(
            lines
                .iter()
                .skip(1)
                .flat_map(|line| &line.spans)
                .any(|span| {
                    span.style.fg == Some(crate::theme::DARK.custom_message_text)
                        && span.style.bg == Some(crate::theme::DARK.custom_message_bg)
                })
        );
    }

    #[test]
    fn tool_diff_and_reasoning_render_with_semantic_roles_and_no_ansi() {
        let entry = TranscriptEntry {
            kind: TranscriptKind::Tool,
            content: vec![
                ContentBlock::text("\u{1b}[31m@@ -1 +1 @@\u{1b}[0m\n- old\n+ new"),
                ContentBlock::thinking("private analysis"),
            ],
            tool_name: Some("edit".to_owned()),
            is_error: false,
            is_partial: false,
        };
        let mut lines = Vec::new();
        render_transcript_entry(&mut lines, &entry, true, true, crate::theme::DARK);
        let spans = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .collect::<Vec<_>>();
        assert!(
            spans
                .iter()
                .any(|span| span.style.fg == Some(crate::theme::DARK.tool_diff_removed))
        );
        assert!(
            spans
                .iter()
                .any(|span| span.style.fg == Some(crate::theme::DARK.tool_diff_added))
        );
        assert!(
            spans
                .iter()
                .any(|span| span.style.fg == Some(crate::theme::DARK.thinking_text))
        );
        assert!(spans.iter().all(|span| !span.content.contains('\u{1b}')));
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
            transcript: Vec::new(),
            editor: EditorState::new(),
            streaming_text: String::new(),
            streaming_thinking: String::new(),
            is_streaming: false,
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
            tree_panel: None,
            scoped_models: None,
            session_selector: None,
            scoped_model_selector: None,
            todo_phases: phases,
            active_loops: std::collections::BTreeMap::new(),
        }
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
}
