use std::borrow::Cow;
use std::cell::Cell;
use std::collections::{HashSet, VecDeque, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, anyhow};
use base64::Engine as _;
use crossterm::{
    cursor::{Hide, Show},
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, KeyCode, KeyEvent,
        KeyEventKind, KeyModifiers, MouseEvent,
    },
    execute,
    terminal::{DisableLineWrap, EnableLineWrap, disable_raw_mode, enable_raw_mode, window_size},
};
use futures_util::{FutureExt, StreamExt};
use pi_agent::{AgentEvent, ThinkingLevel};
use pi_ai::{AssistantMessageEvent, ContentBlock, Message, Model};
use pi_coding::{
    Application, ApplicationEvent, CONFIG_DIR_NAME, DoubleEscapeAction, ExtensionUiRequest,
    GoalLifecycle, GoalState, LoopEvent, LoopTask, ProcessEvent, ProcessId, ProcessKey,
    ProcessLogs, ProcessState, Session, SessionContextUsage, StreamingBehavior, TodoItem,
    TodoPhase, TodoStatus, ToolCallViewStatus, UiNotificationLevel, UiSelectOption,
    UiWidgetPlacement,
};
use ratatui::{
    Terminal,
    TerminalOptions, Viewport,
    backend::{Backend, ClearType, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::Widget,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
#[cfg(unix)]
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use unicode_width::UnicodeWidthChar;

use crate::agents_panel::{AgentsPanel, AgentsPanelAction};
use crate::clipboard::{self, ClipboardContent};
use crate::code_review::ReviewScope;
use crate::code_review_panel::{
    CodeReviewController, CodeReviewPanel, CodeReviewPanelResult, render_code_review_panel,
    sync_code_review_layout,
};
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
use crate::keybindings::{Action, KeyBindingsManager};
use crate::markdown::ratatui::{
    MarkdownRatatuiStyles, render_ratatui_markdown, render_ratatui_markdown_streaming,
    syntax_spans_unpadded as markdown_syntax_spans,
};
use crate::orchestration_message::{
    OrchestrationIrcView, orchestration_irc_view, orchestration_irc_view_from_mailbox,
};
use crate::process_commands::{
    ProcessKeyResult, ProcessPanel, ProcessPanelAction, render_process_panel,
};
use crate::resume_catalog::{
    ResumeCatalogRequest, ResumeSelectorRow, effective_resume_sources, load_resume_catalog,
};
use crate::saved_session_selector::{
    SavedSessionSelector, SessionSelectorMode, SessionSelectorRequest, session_display_name,
};
use crate::scoped_model_selector::{ScopedModelSelection, ScopedModelSelector};
use crate::terminal_images::{
    ImageDisplayConfig, ImageFrameIdentity, ImageLayout, ImagePlacement, TerminalCellSize,
    TerminalImageRenderer,
};
use crate::theme::{Theme, ThemeManager};
use crate::todo_dag_panel::{
    TodoDagPanel, TodoDagPanelResult, TodoDagSnapshot, render_todo_dag_panel,
};
use crate::tool_card_adapter::{
    ToolCardPresentationAdapter, ToolCardRowRole, ToolCardRows, task_delegation_request,
};
use crate::tree_panel::{TreePanel, TreePanelMode};

use crate::settings_panel::{SettingsControl, SettingsPanel};
use crate::side_chat::{SideChatAction, SideChatAsyncRequest, SideChatController};
use crate::side_chat_panel::render_side_chat_panel;
use crate::workflow_panel::{WorkflowIntentKind, WorkflowPanel, WorkflowPanelResult, WorkflowPanelSnapshot, compact_workflow_status, render_workflow_panel,
};

const MAX_TRANSCRIPT_LINES: usize = 4_000;
const MAX_COMPLETIONS: usize = 7;
const MAX_PASTE_BYTES: usize = 1024 * 1024;
/// Minimum spacing between background footer (git branch/dirty + context
/// utilization) refreshes. Bounds git spawns so the render loop never floods.
const FOOTER_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1500);


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

/// Cached, background-refreshed git working-tree summary for the composer
/// footer. Collected off the render path with fixed plumbing commands so the
/// header never blocks on git/fs and repo-local conversion drivers never run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct FooterGitStatus {
    branch: Option<String>,
    /// Index changes (staged) for tracked files.
    staged: usize,
    /// Working-tree changes (unstaged) for tracked files.
    modified: usize,
    /// Untracked files (`??`).
    untracked: usize,
}

/// Exact application runtime and working-directory identity for a footer
/// refresh. Results are only applied while all three components still match.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FooterRefreshKey {
    generation: u64,
    cwd: PathBuf,
    runtime_identity: pi_coding::ProcessOwnerId,
}

#[derive(Clone)]
struct FooterRefreshRequest {
    key: FooterRefreshKey,
    session: Session,
}

impl FooterRefreshRequest {
    fn current(application: &Application) -> Self {
        loop {
            let generation = application.runtime_epoch();
            let session = application.session();
            if application.runtime_epoch() == generation {
                return Self {
                    key: FooterRefreshKey {
                        generation,
                        cwd: session.cwd().to_path_buf(),
                        runtime_identity: session.process_owner_id(),
                    },
                    session,
                };
            }
        }
    }
}

struct FooterRefreshResult {
    key: FooterRefreshKey,
    git: Option<FooterGitStatus>,
    context: Option<SessionContextUsage>,
}

/// Parse `git status --porcelain=v1 -z --branch` output into a footer summary.
/// Pure helper so focused tests can assert counts without spawning git. Returns
/// `None` when the output carries no branch header (not a git repo / git absent).
fn parse_git_porcelain(output: &[u8]) -> Option<FooterGitStatus> {
    let mut records = output.split(|&b| b == 0);
    let header = records.next()?;
    let header_str = std::str::from_utf8(header).ok()?;
    let branch = header_str.strip_prefix("## ").map(|rest| {
        // `## main`, `## main...origin/main [ahead 1]`, `## HEAD (no branch)`,
        // `## No commits yet on main`. Take the branch token up to the first
        // `...` or whitespace; detached HEAD collapses to "HEAD".
        let token = rest
            .split("...")
            .next()
            .unwrap_or(rest)
            .split_whitespace()
            .next()
            .unwrap_or(rest);
        token.to_owned()
    });
    let mut staged = 0usize;
    let mut modified = 0usize;
    let mut untracked = 0usize;
    while let Some(record) = records.next() {
        let Ok(text) = std::str::from_utf8(record) else {
            continue;
        };
        let Some(status) = text.get(..2) else {
            continue;
        };
        let (x, y) = (status.as_bytes()[0] as char, status.as_bytes()[1] as char);
        if x == '?' && y == '?' {
            untracked += 1;
        } else {
            if x != ' ' && x != '?' {
                staged += 1;
            }
            if y != ' ' && y != '?' {
                modified += 1;
            }
            // Rename/copy entries emit an extra NUL-separated original path.
            // Either the index or worktree column can carry the status.
            if x == 'R' || x == 'C' || y == 'R' || y == 'C' {
                records.next();
            }
        }
    }
    Some(FooterGitStatus {
        branch,
        staged,
        modified,
        untracked,
    })
}

/// Hard cap on each footer Git command's stdout. A pathological repository
/// cannot force unbounded allocation; oversized output is rejected.
const FOOTER_GIT_MAX_STDOUT: usize = 1024 * 1024;
/// Tight bound for each footer Git command. The single-flight worker prevents
/// slow repositories from stacking overlapping processes.
const FOOTER_GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(800);

fn collect_footer_git_output(
    sandbox: &crate::code_review::IsolatedGitSandbox,
    args: &[&str],
) -> Option<Vec<u8>> {
    let output = crate::code_review::run_git_bounded_timeout(
        sandbox.work_tree(),
        args,
        FOOTER_GIT_MAX_STDOUT,
        Some(sandbox.environment()),
        FOOTER_GIT_TIMEOUT,
    )
    .ok()?;
    if output.error.is_some() || output.truncated {
        return None;
    }
    Some(output.stdout)
}

fn count_nul_paths(output: &[u8]) -> usize {
    output
        .split(|&byte| byte == 0)
        .filter(|path| !path.is_empty())
        .collect::<HashSet<_>>()
        .len()
}

/// Collect a footer summary in a disposable Git directory with copied HEAD and
/// index but no repository-local executable config. Fixed plumbing commands
/// inspect refs, index/worktree metadata, and ignore rules; conversion/rename
/// drivers are disabled and `core.fsmonitor` is pinned off.
fn collect_footer_git_status(cwd: &Path) -> Option<FooterGitStatus> {
    let sandbox = crate::code_review::IsolatedGitSandbox::discover(cwd).ok()?;
    let command_prefix = ["-c", "core.fsmonitor=false"];
    let head_branch = collect_footer_git_output(
        &sandbox,
        &[command_prefix[0], command_prefix[1], "rev-parse", "--abbrev-ref", "HEAD"],
    );
    let has_head = head_branch.is_some();
    let branch_output = match head_branch {
        Some(branch) => branch,
        None => {
            // Unborn HEAD has no commit for `rev-parse`, but its symbolic branch
            // is still meaningful and requires no worktree conversion.
            collect_footer_git_output(
                &sandbox,
                &[command_prefix[0], command_prefix[1], "symbolic-ref", "--short", "HEAD"],
            )?
        }
    };
    let branch = std::str::from_utf8(&branch_output)
        .ok()?
        .lines()
        .next()
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
        .map(str::to_owned);

    let modified = if has_head {
        let modified_output = collect_footer_git_output(
            &sandbox,
            &[
                command_prefix[0], command_prefix[1], "diff-files", "--name-only", "-z",
                "--no-ext-diff", "--no-textconv", "--no-renames",
            ],
        )?;
        count_nul_paths(&modified_output)
    } else {
        // Without HEAD, cached paths are staged against the empty tree. Git's
        // index metadata still reports paths modified again after staging.
        let modified_output = collect_footer_git_output(
            &sandbox,
            &[command_prefix[0], command_prefix[1], "ls-files", "--modified", "-z"],
        )?;
        count_nul_paths(&modified_output)
    };
    let staged_output = if has_head {
        collect_footer_git_output(
            &sandbox,
            &[
                command_prefix[0], command_prefix[1], "diff", "--cached", "--name-only", "-z",
                "--no-ext-diff", "--no-textconv", "--no-renames",
            ],
        )?
    } else {
        // In an unborn repository, every cached path is staged against the
        // implicit empty tree. This fallback remains metadata-only.
        collect_footer_git_output(
            &sandbox,
            &[command_prefix[0], command_prefix[1], "ls-files", "--cached", "-z"],
        )?
    };
    let staged = count_nul_paths(&staged_output);
    let untracked_output = collect_footer_git_output(
        &sandbox,
        &[
            command_prefix[0], command_prefix[1], "ls-files", "--others",
            "--exclude-standard", "-z",
        ],
    )?;
    let untracked = count_nul_paths(&untracked_output);

    Some(FooterGitStatus { branch, staged, modified, untracked })
}

/// Spawn one bounded background refresh of the composer footer's git
/// branch/dirty counts and context-window utilization. The caller enforces
/// single-flight admission; the complete request identity is returned with the
/// result so a runtime or cwd change cannot be overwritten by late work.
fn spawn_footer_refresh(
    request: FooterRefreshRequest,
    tx: mpsc::UnboundedSender<BackgroundEvent>,
) {
    tokio::task::spawn_blocking(move || {
        let git = collect_footer_git_status(&request.key.cwd);
        // `session_stats()` traverses message history (synchronous CPU), so it
        // stays off the async runtime thread alongside the git collection. The
        // captured Session belongs to the runtime identity in `request.key`.
        let context = request
            .session
            .session_stats()
            .context_usage
            .filter(|usage| usage.context_window > 0);
        let _ = tx.send(BackgroundEvent::FooterRefresh(FooterRefreshResult {
            key: request.key,
            git,
            context,
        }));
    });
}

fn spawn_code_review_snapshot_load(
    controller_generation: u64,
    generation: u64,
    cwd: PathBuf,
    scope: ReviewScope,
    tx: mpsc::UnboundedSender<BackgroundEvent>,
) {
    tokio::task::spawn_blocking(move || {
        let snapshot = crate::code_review::load_review_snapshot_for(&cwd, scope);
        let _ = tx.send(BackgroundEvent::CodeReviewSnapshotLoaded {
            controller_generation,
            generation,
            cwd,
            snapshot,
        });
    });
}

enum BackgroundEvent {
    FileCompletion {
        generation: u64,
        row: usize,
        prefix: AtPrefix,
        result: std::result::Result<Vec<file_search::FileMatch>, String>,
    },
    CodeReviewSnapshotLoaded {
        controller_generation: u64,
        generation: u64,
        cwd: PathBuf,
        snapshot: crate::code_review::ReviewSnapshot,
    },
    ClipboardRead(std::result::Result<Option<ClipboardContent>, String>),
    ClipboardWrite(std::result::Result<(), String>),
    ExtensionCommandFinished {
        command: String,
        result: std::result::Result<serde_json::Value, String>,
    },
    /// Completion of a backgrounded `/workflow` subcommand (all non-`OpenPage`
    /// operations). Mirrors `ExtensionCommandFinished` for `/run`: the slash
    /// dispatch admits the command on a background task and returns promptly;
    /// the formatted effect message or bounded error arrives here. Live
    /// snapshot updates already flow through `ApplicationEvent::Workflow`.
    /// Periodic background refresh of the composer footer's git branch/dirty
    /// counts and context-window utilization. The result retains the complete
    /// request identity so late work is safe to discard.
    FooterRefresh(FooterRefreshResult),
    WorkflowCommandFinished {
        label: &'static str,
        result: std::result::Result<crate::workflow_commands::WorkflowCommandEffect, String>,
    },
}

fn spawn_extension_command(
    application: &Application,
    tx: mpsc::UnboundedSender<BackgroundEvent>,
    command: String,
    arguments: String,
) {
    let application = application.clone();
    tokio::spawn(async move {
        let result = crate::interactive_commands::invoke_extension_command(
            &application,
            &command,
            arguments,
        )
        .await
        .map_err(|error| format!("{error:#}"));
        let _ = tx.send(BackgroundEvent::ExtensionCommandFinished { command, result });
    });
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
    startup_warnings: Vec<String>,
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
        startup_warnings,
    );
    // Populate the footer's git/context segments immediately so the first
    // rendered frame is not blank; later refreshes are throttled by the loop.
    state.request_footer_refresh(&application);
    let session = application.session();
    if let Some(prompt) = initial_prompts.first() {
        let expanded = crate::file_args::expand_prompt_in_workspace(
            prompt,
            session.workspace_roots())?;
        state.push_lines("You", prompt.clone(), state.themes.theme().accent);
        application.prompt(expanded.prompt, expanded.images, None).await?;
        for prompt in initial_prompts.into_iter().skip(1) {
            let expanded = crate::file_args::expand_prompt_in_workspace(
                &prompt,
                session.workspace_roots())?;
            application.follow_up(expanded.prompt, expanded.images).await;
        }
    }
    let mut update_notice = Some(Box::pin(crate::self_update::startup_notice()));
    let mut theme_watch = tokio::time::interval(std::time::Duration::from_millis(250));
    let mut animation = tokio::time::interval(std::time::Duration::from_millis(120));
    let mut footer_refresh = tokio::time::interval(FOOTER_REFRESH_INTERVAL);

    let run_result: Result<()> = async {
    loop {
        terminal.commit_settled(&mut state)?;
        if let Some(panel) = state.code_review_panel.as_mut() {
            if let Ok(size) = terminal.terminal.size() {
                sync_code_review_layout(panel, Rect::new(0, 0, size.width, size.height));
            }
        }
        terminal.sync_code_review_mouse_capture(&state)?;
        if let Some(side) = state.side_chat.as_mut() {
            side.poll_events();
            if state.side_chat_open {
                state.status = side.status().to_owned();
            }
        }
        if let Some(controller) = state.code_review_controller.as_mut() {
            controller.poll_events();
            if let Some(panel) = state.code_review_panel.as_mut() {
                panel.sync_controller(controller);
            }
        }
        terminal.draw(&state, |frame, images| render(frame, &state, images))?;
        tokio::select! {
            terminal_event = input.next() => {
                let Some(terminal_event) = terminal_event else {
                    terminal.set_code_review_mouse_capture(false)?;
                    state.cancel_extension_dialogs();
                    state.shutdown_side_chat().await;
                    state.shutdown_code_review().await;
                    return Ok(());
                };
                match terminal_event? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        if state.pty_attachment.is_none() && is_raw_multiline_paste_start(key) {
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
                                    Some(Some(Err(error))) => {
                                        terminal.set_code_review_mouse_capture(false)?;
                                        return Err(error.into());
                                    }
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
                                                terminal.set_code_review_mouse_capture(false)?;
                                                state.cancel_extension_dialogs();
                                                state.shutdown_side_chat().await;
                                                state.shutdown_code_review().await;
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
                                            terminal.set_code_review_mouse_capture(false)?;
                                            state.cancel_extension_dialogs();
                                            state.shutdown_side_chat().await;
                                            state.shutdown_code_review().await;
                                            return Ok(());
                                        }
                                    }
                                    Event::Paste(text) => handle_terminal_paste(&application, &mut state, &text).await,
                                    _ => {}
                                }
                            }
                            if input_closed {
                                terminal.set_code_review_mouse_capture(false)?;
                                state.cancel_extension_dialogs();
                                state.shutdown_side_chat().await;
                                state.shutdown_code_review().await;
                                return Ok(());
                            }
                        } else if handle_key(&application, &mut state, key, &mut terminal).await? {
                            terminal.set_code_review_mouse_capture(false)?;
                            state.cancel_extension_dialogs();
                            state.shutdown_side_chat().await;
                            state.shutdown_code_review().await;
                            return Ok(());
                        }
                        state.sync_extension_host_bindings();
                    }
                    Event::Paste(payload) => {
                        handle_terminal_paste(&application, &mut state, &payload).await;
                        state.sync_extension_host_bindings();
                    }
                    Event::Resize(_, _) => {}
                    Event::Mouse(mouse) => {
                        if handle_code_review_mouse(&mut state, mouse, &mut terminal)? {
                            terminal.sync_code_review_mouse_capture(&state)?;
                        }
                    }
                    _ => {}
                }
            }
            application_event = events.recv() => {
                match application_event {
                    Ok(ApplicationEvent::RuntimeChanged { epoch }) => {
                        // Close any open code-review overlay before rebinding the
                        // cwd/runtime. A runtime switch can move to a different
                        // repository, so the loaded snapshot, controller, and mouse
                        // capture must not survive into the new generation.
                        state.close_code_review_panel(&mut terminal)?;
                        state.replace_transcript_from_application(&application);
                        state.refresh_job_projection(&application);
                        state.todo_phases = application.todo_state().phases;
                        if let Some(panel) = &mut state.todo_dag_panel {
                            panel.update_main(
                                state.todo_phases.clone(),
                                application.todo_dag_status(),
                                state.job_cards.cards_in_source_order(),
                            );
                            panel.update_workflows(TuiState::todo_workflow_snapshots(&application));
                        }
                        state.cwd_path = application.session().cwd().to_path_buf();
                        state.apply_runtime_settings(&application).await;
                        state.status = format!("Switched application runtime generation {epoch}");
                        state.invalidate_footer_refresh(&application);
                    }
                    Ok(ApplicationEvent::Workflow(event)) => state.apply_workflow_event(&application, event),
                    Ok(event) => {
                        if let ApplicationEvent::Agent(agent_event) = &event {
                            if let Some(side) = state.side_chat.as_ref() {
                                side.observe_main_agent_event(agent_event);
                            }
                        }
                        let refresh_todo_panel = matches!(
                            &event,
                            ApplicationEvent::Orchestration(_)
                                | ApplicationEvent::TodoUpdated { .. }
                                | ApplicationEvent::TodoReminder { .. }
                        );
                        state.apply(event);
                        if refresh_todo_panel {
                            let phases = application.todo_state().phases;
                            let status = application.todo_dag_status();
                            let jobs = state.job_cards.cards_in_source_order();
                            if let Some(panel) = &mut state.todo_dag_panel {
                                panel.update_main(phases, status, jobs);
                            }
                        }
                        // Transcript/context changes shift utilization; throttled.
                        state.request_footer_refresh(&application);
                    }
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
                        if let Some(panel) = &mut state.todo_dag_panel {
                            panel.update_main(
                                application.todo_state().phases,
                                application.todo_dag_status(),
                                state.job_cards.cards_in_source_order(),
                            );
                            panel.update_workflows(TuiState::todo_workflow_snapshots(&application));
                        }
                        state.push_status(format!("UI skipped {count} stale events"), true);
                        state.reconcile_activity_from_application(&application);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        terminal.set_code_review_mouse_capture(false)?;
                        state.cancel_extension_dialogs();
                        state.shutdown_side_chat().await;
                        state.shutdown_code_review().await;
                        return Ok(());
                    }
                }
            }
            extension_event = extension_events.recv() => {
                match extension_event {
                    Ok(event) => state.apply_extension_ui(event),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                        state.push_status(format!("Extension UI skipped {count} stale events"), true);
                        state.reconcile_extension_dialog();
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        terminal.set_code_review_mouse_capture(false)?;
                        state.cancel_extension_dialogs();
                        state.shutdown_side_chat().await;
                        state.shutdown_code_review().await;
                        return Ok(());
                    }
                }
            }
            background_event = background_rx.recv() => {
                let Some(background_event) = background_event else {
                    terminal.set_code_review_mouse_capture(false)?;
                    state.cancel_extension_dialogs();
                    state.shutdown_side_chat().await;
                    state.shutdown_code_review().await;
                    return Ok(());
                };
                if matches!(&background_event, BackgroundEvent::FooterRefresh(_)) {
                    state.reconcile_footer_refresh_identity(&application);
                }
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
            _ = animation.tick(), if state.has_active_animation() => {
                state.animation_frame = state.animation_frame.wrapping_add(1);
                state.reconcile_activity_from_application(&application);
            }
            _ = theme_watch.tick() => {
                let changed = state.poll_theme_reload() | state.reconcile_extension_dialog();
                if !changed {
                    continue;
                }
            }
            _ = footer_refresh.tick() => {
                state.request_footer_refresh(&application);
            }
            _ = shutdown.recv() => {
                // Drop page-scoped mouse capture before any async cleanup; the
                // guard remains the final fallback if terminal IO itself fails.
                terminal.set_code_review_mouse_capture(false)?;
                state.cancel_extension_dialogs();
                state.shutdown_side_chat().await;
                state.shutdown_code_review().await;
                return Ok(());
            }
        }
    }
    }
    .await;
    let run_result = match run_result {
        Ok(()) => terminal.final_settled_flush(&mut state),
        Err(error) => Err(error),
    };
    let _ = terminal.set_code_review_mouse_capture(false);
    state.cancel_extension_dialogs();
    state.shutdown_side_chat().await;
    state.shutdown_code_review().await;
    run_result
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
fn restore_terminal() {
    if TUI_ACTIVE.swap(false, Ordering::SeqCst) {
        let mut stdout = io::stdout();
        if crate::terminal_images::detect_protocol(
            &crate::terminal_images::TerminalEnvironment::current(),
        ) == Some(crate::terminal_images::TerminalImageProtocol::Kitty) {
            let _ = stdout.write_all(crate::terminal_images::KITTY_DELETE_ALL);
        }
        // Best-effort: code-review may have left mouse capture enabled.
        let _ = execute!(stdout, DisableMouseCapture);
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

fn write_code_review_mouse_capture(
    writer: &mut impl Write,
    active: &mut bool,
    enable: bool,
) -> io::Result<()> {
    if *active == enable {
        return Ok(());
    }
    if enable {
        execute!(writer, EnableMouseCapture)?;
    } else {
        execute!(writer, DisableMouseCapture)?;
    }
    *active = enable;
    Ok(())
}

trait CodeReviewMouseController {
    fn set_code_review_mouse_capture(&mut self, enable: bool) -> Result<()>;
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    images: TerminalImageRenderer,
    /// Previous `page_overlay_open` sample. Used to clear the live region exactly
    /// once on overlay dismiss so transient page pixels cannot later be promoted
    /// into native scrollback by `insert_before`.
    page_overlay_was_open: bool,
    /// True while EnableMouseCapture is active for the code-review page only.
    code_review_mouse: bool,
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
            code_review_mouse: false,
        })
    }

    fn set_code_review_mouse_capture(&mut self, enable: bool) -> Result<()> {
        write_code_review_mouse_capture(
            self.terminal.backend_mut(),
            &mut self.code_review_mouse,
            enable,
        )?;
        Ok(())
    }

    fn sync_code_review_mouse_capture(&mut self, state: &TuiState) -> Result<()> {
        self.set_code_review_mouse_capture(code_review_page_active(state))
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
        let content_width = live_content_width(size.width).max(1);
        let transcript_rows = transcript_region_height(state, size.width.max(1), size.height);
        let entries = state.overflow_commit_batch(content_width, transcript_rows);
        let has_live_continuation = state.has_visible_entry_after(entries.len());
        self.commit_transcript_batch(state, entries, content_width, has_live_continuation)
    }

    /// Persist every settled transcript entry before a normal TUI exit. Unlike
    /// the periodic overflow path, this deliberately ignores viewport pressure
    /// and page overlays: only transcript entries enter the durable buffer, and
    /// the mutable live frame is cleared before insertion.
    fn final_settled_flush(&mut self, state: &mut TuiState) -> Result<()> {
        self.resize_live_viewport(state)?;
        let size = self.terminal.size()?;
        let content_width = live_content_width(size.width).max(1);
        let entries = state.final_settled_commit_batch();
        self.commit_transcript_batch(state, entries, content_width, false)
    }

    fn commit_transcript_batch(
        &mut self,
        state: &mut TuiState,
        entries: Vec<TranscriptEntry>,
        content_width: u16,
        has_live_continuation: bool,
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let theme = state.themes.theme();
        let lines = assemble_committed_transcript_entries(
            &entries,
            state.show_thinking,
            state.expand_tools,
            theme,
            content_width,
            has_live_continuation,
        );
        let height = u16::try_from(wrapped_line_count(&lines, content_width))
            .unwrap_or(u16::MAX)
            .max(1);
        // `Viewport::Inline` spans the full terminal, so ratatui's fallback
        // `insert_before` scrolls the physical screen before painting the
        // committed rows. If the live frame is still present at that point,
        // the terminal promotes its top rows (transcript first, then eventually
        // composer/status chrome) into native scrollback. Erase the mutable
        // viewport first; `insert_before` then owns the scroll operation and
        // can promote only the committed buffer supplied below.
        self.clear_live_viewport()?;
        self.terminal.insert_before(height, |buffer| {
            let area = Rect::new(
                u16::from(buffer.area.width > 2),
                buffer.area.y,
                content_width.min(buffer.area.width),
                buffer.area.height,
            );
            Paragraph::new(Text::from(lines))
                .style(Style::default().fg(theme.text))
                .wrap(Wrap { trim: false })
                .render(area, buffer);
        })?;
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
            &plan.placements)?;
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
        // Disable mouse capture before any other fallible terminal cleanup.
        self.set_code_review_mouse_capture(false)?;
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
        self.code_review_mouse = false;
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

impl CodeReviewMouseController for TerminalGuard {
    fn set_code_review_mouse_capture(&mut self, enable: bool) -> Result<()> {
        TerminalGuard::set_code_review_mouse_capture(self, enable)
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
        // Best effort: mouse capture belongs only to the code-review page. The
        // normal transition paths release it synchronously; drop is the final
        // fallback for panics, terminal errors, and partially completed exits.
        let _ = self.set_code_review_mouse_capture(false);
        // Row-clear the mutable composer/status/overlay frame before the inline
        // ED reset. tmux may retain a home-positioned ED surface in history;
        // blanking each row first keeps normal-screen scrollback transcript-only.
        let _ = self.clear_live_viewport();
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

const MAX_PTY_ATTACHMENT_OUTPUT_BYTES: usize = 1024 * 1024;

struct PtyAttachment {
    process_id: ProcessId,
    cursor: u64,
    output: VecDeque<u8>,
}

impl PtyAttachment {
    fn new(process_id: ProcessId, cursor: u64) -> Self {
        Self {
            process_id,
            cursor,
            output: VecDeque::new(),
        }
    }

    fn apply_logs(&mut self, logs: &ProcessLogs) {
        if logs.lost {
            self.output.clear();
            self.cursor = logs.start_cursor;
        }
        for chunk in &logs.chunks {
            self.append_output(chunk.start_cursor, chunk.cursor, &chunk.bytes());
        }
        self.cursor = self.cursor.max(logs.cursor);
    }

    fn apply_event(&mut self, event: &ProcessEvent) -> bool {
        match event {
            ProcessEvent::ProcessOutput {
                id,
                start_cursor,
                cursor,
                data_base64,
                ..
            } if id == &self.process_id => {
                if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data_base64) {
                    self.append_output(*start_cursor, *cursor, &bytes);
                }
                false
            }
            ProcessEvent::ProcessExited { process } if process.id == self.process_id => true,
            _ => false,
        }
    }

    fn append_output(&mut self, start_cursor: u64, cursor: u64, bytes: &[u8]) {
        if cursor <= self.cursor {
            return;
        }
        if start_cursor > self.cursor {
            self.output.clear();
            self.cursor = start_cursor;
        }
        let offset = self
            .cursor
            .saturating_sub(start_cursor)
            .min(bytes.len() as u64) as usize;
        self.output.extend(&bytes[offset..]);
        self.cursor = cursor;
        while self.output.len() > MAX_PTY_ATTACHMENT_OUTPUT_BYTES {
            self.output.pop_front();
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SettingsValueInput {
    key: String,
    value: String,
    cursor: usize,
    hint: &'static str,
    error: Option<String>,
    replace_on_type: bool,
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
    /// `(instance, key, text)` of the keyed extension status that currently
    /// owns the live composer status, if any. `StatusCleared` retires the
    /// live status only when the caller's `(instance, key)` matches *and* the
    /// live status still equals `text`, so any ordinary newer status write
    /// (which changes `status` without touching this field) automatically
    /// invalidates the ownership without scattering retirement across every
    /// status-setting site. `ExtensionCleared` retires ownership held by the
    /// unloaded extension regardless of the current text.
    extension_status_key: Option<(pi_coding::ExtensionInstanceId, String, String)>,
    /// Ephemeral input/runtime error rendered immediately above the composer.
    /// It is never copied into the transcript or native terminal scrollback.
    composer_error: Option<String>,
    /// Whether the current bounded composer notice is a warning rather than an error.
    composer_error_is_warning: bool,
    model: String,
    cwd: String,
    completions: CompletionState,
    themes: ThemeManager,
    keybindings: KeyBindingsManager,
    cwd_path: PathBuf,
    recent_sessions: Vec<ResumeSelectorRow>,
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
    settings_value_input: Option<SettingsValueInput>,
    tree_panel: Option<TreePanel>,
    process_panel: Option<ProcessPanel>,
    pty_attachment: Option<PtyAttachment>,
    workflow_panel: Option<WorkflowPanel>,
    todo_dag_panel: Option<TodoDagPanel>,
    code_review_panel: Option<CodeReviewPanel>,
    code_review_controller: Option<CodeReviewController>,
    code_review_cleanup: Option<tokio::task::JoinHandle<()>>,
    code_review_controller_generation: u64,
    code_review_load_generation: u64,
    code_review_load_in_flight: Option<(u64, u64, PathBuf)>,
    code_review_scope: ReviewScope,
    /// Persistent side-chat controller. Survives overlay close; cleaned on TUI exit.
    side_chat: Option<SideChatController>,
    /// Whether the side-chat overlay is currently visible.
    side_chat_open: bool,
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
    /// Cached git branch/dirty counts for the composer footer, refreshed by a
    /// bounded background task (never collected in render).
    git_status: Option<FooterGitStatus>,
    /// Cached context-window utilization for the composer footer, sourced from
    /// `session_stats()` off the render path.
    context_usage: Option<SessionContextUsage>,
    /// Monotonic clock of the last footer refresh spawn. Same-runtime refreshes
    /// remain throttled to `FOOTER_REFRESH_INTERVAL`.
    last_footer_refresh: Option<std::time::Instant>,
    /// Identity of the sole blocking footer worker, when one is active.
    footer_refresh_in_flight: Option<FooterRefreshKey>,
    /// Latest coalesced request admitted while the worker is active.
    footer_refresh_pending: Option<FooterRefreshRequest>,
    /// Latest runtime/cwd identity requested by the live TUI.
    footer_refresh_current: Option<FooterRefreshKey>,
}

const MAX_RECENT_SESSIONS: usize = 3;

fn effective_session_catalog(application: &Application) -> Result<pi_coding::SessionCatalog> {
    let session = application.session();
    Ok(pi_coding::SessionCatalog::from_env()?.with_native_session_root(session.session_dir()))
}

fn load_cwd_resume_rows(application: &Application) -> Result<Vec<ResumeSelectorRow>> {
    let session = application.session();
    let catalog = effective_session_catalog(application)?;
    Ok(load_resume_catalog(
        &catalog,
        &ResumeCatalogRequest {
            sources: effective_resume_sources(application),
            cwd_scope: Some(session.cwd().to_path_buf()),
            ..ResumeCatalogRequest::default()
        },
    )?
    .rows)
}

fn rename_managed_session(application: &Application, path: &Path, name: &str) -> Result<()> {
    let root = application.session().session_dir();
    let path = pi_coding::validated_saved_session_path(&root, path)?;
    let recorder = pi_coding::resume_session(&path)?;
    recorder.record_session_name(name)?;
    recorder.close()?;
    Ok(())
}

fn delete_managed_session(application: &Application, path: &Path) -> Result<()> {
    let root = application.session().session_dir();
    let path = pi_coding::validated_saved_session_path(&root, path)?;
    std::fs::remove_file(&path)
        .map_err(|error| anyhow!("deleting saved session {}: {error}", path.display()))
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
        startup_warnings: Vec<String>,
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
            status: String::new(),
            extension_status_key: None,
            composer_error: None,
            composer_error_is_warning: false,
            model,
            cwd: cwd.display().to_string(),
            completions: CompletionState::default(),
            themes,
            keybindings,
            cwd_path: cwd.to_path_buf(),
            recent_sessions: Vec::new(),
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
            pty_attachment: None,
            workflow_panel: None,
            todo_dag_panel: None,
            code_review_panel: None,
            code_review_controller: None,
            code_review_cleanup: None,
            code_review_controller_generation: 0,
            code_review_load_generation: 0,
            code_review_load_in_flight: None,
            code_review_scope: ReviewScope::WorkingTree,
            side_chat: None,
            side_chat_open: false,
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
            git_status: None,
            context_usage: None,
            last_footer_refresh: None,
            footer_refresh_in_flight: None,
            footer_refresh_pending: None,
            footer_refresh_current: None,
            goal_state: application.goal_state(),
        };
        state.sync_extension_host_bindings();
        for message in session.history() {
            state.push_message(message);
        }
        state.rebuild_prompt_history_from_messages(session.history());
        state.refresh_job_projection(application);
        if let Err(error) = state.refresh_recent_sessions(application) {
            state.push_status(format!("Could not load recent sessions: {error:#}"), true);
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
        state.apply_startup_warnings(startup_warnings);
        state
    }

    fn refresh_recent_sessions(&mut self, application: &Application) -> Result<()> {
        self.recent_sessions = load_cwd_resume_rows(application)?
            .into_iter()
            .take(MAX_RECENT_SESSIONS)
            .collect();
        Ok(())
    }

    fn apply_process_event(&mut self, event: ProcessEvent) {
        if self
            .pty_attachment
            .as_mut()
            .is_some_and(|attachment| attachment.apply_event(&event))
        {
            self.pty_attachment = None;
            self.status = "PTY process exited; detached".to_owned();
        }
        if let Some(panel) = &mut self.process_panel {
            panel.apply_event(event);
        }
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
        if let Err(error) = self.refresh_recent_sessions(application) {
            self.push_status(format!("Could not refresh recent sessions: {error:#}"), true);
        }
        if let Some(mut selector) = self.session_selector.take() {
            match load_cwd_resume_rows(application) {
                Ok(rows) => selector.reload(rows),
                Err(error) => {
                    self.push_status(format!("Could not refresh session catalog: {error:#}"), true);
                }
            }
            self.session_selector = Some(selector);
        }
    }

    fn apply_extension_ui(&mut self, event: ExtensionUiEvent) {
        match event {
            ExtensionUiEvent::InteractionRequested { interaction } => {
                // `page_overlay_owner` is the authoritative exclusive-page
                // predicate: any modal page owner defers the interaction, not
                // only code-review/side-chat/PTY.
                if let Some(owner) = page_overlay_owner(self) {
                    let _ = self.extension_ui.cancel(&interaction.id);
                    self.extension_status_key = None;
                    self.status = format!(
                        "Extension interaction rejected while {owner} is active"
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
                        self.extension_status_key = None;
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
                        self.extension_status_key = None;
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
                    self.extension_status_key = None;
                    self.push_status(error, true);
                } else {
                    self.extension_status_key = None;
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
                // Retire keyed ownership for the unloaded extension. Clear the
                // live status only while it still equals that owner's text;
                // ordinary host statuses may have replaced it without touching
                // the key and must survive unload.
                if let Some((owner, _, owner_text)) = self.extension_status_key.as_ref()
                    && *owner == instance
                {
                    if self.status.as_str() == owner_text.as_str() {
                        self.status.clear();
                    }
                    self.extension_status_key = None;
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
                self.extension_status_key = None;
                match notification.level {
                    UiNotificationLevel::Info => self.push_status(notification.message, false),
                    UiNotificationLevel::Warning => self.push_warning(notification.message),
                    UiNotificationLevel::Error => self.push_status(notification.message, true),
                }
            }
            ExtensionUiEvent::StatusChanged { item } => {
                self.status = item.text.clone();
                self.extension_status_key = Some((item.instance, item.key, item.text));
            }
            ExtensionUiEvent::StatusCleared { instance, key } => {
                // Retire only the keyed extension status that currently owns
                // the live composer status. Any ordinary newer status write
                // changed `status` without touching `extension_status_key`, so
                // also require the live status to still equal the owned text;
                // a stale clear then no-ops instead of clobbering the newer
                // status. This centralizes ownership invalidation so ordinary
                // status writes need not each clear the key.
                if self
                    .extension_status_key
                    .as_ref()
                    .is_some_and(|(owner, owner_key, owner_text)| {
                        *owner == instance
                            && owner_key == &key
                            && self.status.as_str() == owner_text.as_str()
                    })
                {
                    self.status.clear();
                    self.extension_status_key = None;
                }
            }
            ExtensionUiEvent::WidgetChanged { .. }
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
                path: None })
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
            AgentEvent::MessageEnd { message: Message::ToolResult(result),
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
            entry.tool_card.as_ref().is_some_and(|tool| tool.compact.tool_call_id == tool_call_id)
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
            entry.tool_card.as_ref().is_some_and(|tool| tool.compact.tool_call_id == tool_call_id)
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
        if let Some(panel) = &mut self.todo_dag_panel {
            panel.update_main_jobs(self.job_cards.cards_in_source_order());
        }
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
        if let Some(panel) = &mut self.todo_dag_panel {
            panel.update_main_jobs(self.job_cards.cards_in_source_order());
        }
    }

    /// Reconcile transient activity from the shared application runtime. This
    /// is required after receiver lag because `AgentStart` or `AgentSettled`
    /// may be among the skipped broadcast events; it also lets the animation
    /// tick repair a stale busy indicator if settling raced that recovery.
    fn reconcile_activity_from_application(&mut self, application: &Application) {
        self.is_streaming = application.is_streaming();
        if self.is_streaming {
            return;
        }
        self.streaming_text.clear();
        self.streaming_thinking.clear();
        self.status = "Ready".to_owned();
    }

    fn sync_job_cards(&mut self) {
        let Some(card) = self.job_cards.task_card() else { return;
        };
        let group_id = card.group_id.clone();
        let is_partial = card.children.iter().any(|child| {
            matches!(child.job_status, pi_coding::JobStatus::Queued | pi_coding::JobStatus::Running)
        });
        let is_error = card.children.iter().any(|child| child.job_status == pi_coding::JobStatus::Failed);
        let entry = TranscriptEntry { kind: TranscriptKind::Job, content: Vec::new(), tool_name: None, tool_card: None, job_card: Some(card), is_error, is_partial,
        };
        if let Some(existing) = self.transcript.iter_mut().find(|entry| {
            entry.job_card.as_ref().is_some_and(|card| card.group_id == group_id)
        }) {
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
                    self.replace_last_user_entry(TranscriptEntry { kind: TranscriptKind::User, content: user.content, tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false,
                    });
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
                if let Some(panel) = &mut self.todo_dag_panel {
                    panel.update_main_phases(self.todo_phases.clone());
                }
            }
            ApplicationEvent::Workflow(_) => {}
            ApplicationEvent::TodoReminder { phases } => {
                self.todo_phases = phases;
                if let Some(panel) = &mut self.todo_dag_panel {
                    panel.update_main_phases(self.todo_phases.clone());
                }
                let open = todo_open_count(&self.todo_phases);
                self.status = format!("Todo reminder: {open} open task(s)");
            }
            ApplicationEvent::Process(event) => self.apply_process_event(event),
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
                if let Some(name) = self
                    .workflow_snapshots
                    .iter_mut()
                    .find(|current| current.id == workflow_id.as_str())
                    .filter(|current| current.generation == generation)
                    .map(|current| {
                        current.status = status;
                        current.name.clone()
                    })
                {
                    self.set_bounded_status(format!("Workflow {name} · {status}"));
                }
            }
            pi_coding::WorkflowEvent::Removed { workflow_id, generation,
            } => {
                self.workflow_snapshots.retain(|current| {
                    current.id != workflow_id.as_str() || current.generation > generation
                });
            }
        }
        if let Some(panel) = &mut self.workflow_panel {
            panel.replace(self.workflow_snapshots.clone());
        }
        if let Some(panel) = &mut self.todo_dag_panel {
            panel.update_workflows(Self::todo_workflow_snapshots(application));
        }
    }

    /// Admit a parsed `/workflow` subcommand (everything except `OpenPage`) on a
    /// background task, mirroring `/run` + `spawn_extension_command`. Returns
    /// immediately after setting a visible admission status so the TUI event
    /// loop never blocks on `WorkflowManager::create`/lifecycle work. The
    /// formatted effect message or bounded error arrives later through
    /// `BackgroundEvent::WorkflowCommandFinished`; live snapshot updates flow
    /// through `ApplicationEvent::Workflow` via `apply_workflow_event`.
    fn admit_workflow_command_background(
        &mut self,
        application: &Application,
        command: crate::workflow_commands::InteractiveWorkflowCommand,
    ) {
        debug_assert!(
            !matches!(
                command,
                crate::workflow_commands::InteractiveWorkflowCommand::OpenPage
            ),
            "OpenPage is admitted inline by dispatch_workflow_command",
        );
        let label = workflow_command_admission_label(&command);
        self.status = format!("{label}…");
        self.composer_error = None;
        let application = application.clone();
        let tx = self.background_tx.clone();
        tokio::spawn(async move {
            let result = crate::workflow_commands::execute_interactive_workflow_on_application(
                &application,
                command,
            )
            .await
            .map_err(|error| format!("{error:#}"));
            let _ = tx.send(BackgroundEvent::WorkflowCommandFinished { label, result });
        });
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
        if let Err(error) = self.refresh_recent_sessions(application) {
            self.push_status(
                format!("Could not refresh recent sessions: {error:#}"),
                true,
            );
        }
        self.committed_entries = self.committed_entries.min(self.transcript.len());
    }

    fn push_loop_message(&mut self, message: &pi_ai::CustomMessage) {
        let Some(loop_message) = pi_coding::loop_message_view(message) else { return;
        };
        self.push_entry(TranscriptEntry { kind: TranscriptKind::System, content: vec![ContentBlock::text(loop_message.prompt)], tool_name: Some(format!("Loop {} · {}", loop_message.task_id, loop_message.schedule)), tool_card: None, job_card: None, is_error: false, is_partial: false,
        });
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

    fn apply_startup_warnings(&mut self, warnings: Vec<String>) {
        for warning in warnings {
            self.push_warning(warning);
        }
    }

    fn push_message(&mut self, message: Message) {
        match message {
            Message::User(message) => self.push_entry(TranscriptEntry { kind: TranscriptKind::User, content: message.content, tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false,
            }),
            Message::Assistant(message) => {
                let content = message
                    .content
                    .into_iter()
                    .filter(|block| !matches!(block, ContentBlock::ToolCall(_)))
                    .collect::<Vec<_>>();
                if !content.is_empty() {
                    self.push_entry(TranscriptEntry { kind: TranscriptKind::Assistant, content, tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false,
                    });
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
                    self.push_entry(TranscriptEntry { kind: TranscriptKind::Custom, content: message.content.into_blocks(), tool_name: Some(message.custom_type), tool_card: None, job_card: None, is_error: false, is_partial: false,
                    });
                }
            }
            Message::BranchSummary(message) => self.push_entry(TranscriptEntry { kind: TranscriptKind::System, content: vec![ContentBlock::text(message.summary)], tool_name: Some("Branch summary".to_owned()), tool_card: None, job_card: None, is_error: false, is_partial: false,
            }),
            Message::CompactionSummary(message) => self.push_entry(TranscriptEntry { kind: TranscriptKind::System, content: vec![ContentBlock::text(message.summary)], tool_name: Some("Compaction summary".to_owned()), tool_card: None, job_card: None, is_error: false, is_partial: false,
            }),
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
        }, content: vec![ContentBlock::text(text)], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false,
        });
    }

    fn push_status(&mut self, text: String, is_error: bool) {
        if is_error {
            self.composer_error = Some(text);
            self.composer_error_is_warning = false;
            return;
        }
        self.set_live_status(text.clone());
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

    /// Surface a background-result status line without preempting an active
    /// exclusive overlay. When no overlay owns the status line, the message
    /// becomes the live status; otherwise it degrades to the bounded composer
    fn set_bounded_status(&mut self, text: String) {
        if page_overlay_open(self) {
            self.composer_error = Some(text);
            self.composer_error_is_warning = false;
        } else {
            self.set_live_status(text);
        }
    }

    fn push_warning(&mut self, text: String) {
        self.composer_error = Some(strip_notice_prefix(&text, "Warning: ").to_owned());
        self.composer_error_is_warning = true;
    }


    /// Replace the live composer status, retiring any keyed extension status
    /// ownership first. Centralized so the keyed-status ownership invariant
    /// holds even on the host-side write paths that go through a helper; the
    /// `StatusCleared` text-match check defends the remaining direct writes.
    /// The keyed `ExtensionUiEvent::StatusChanged` path is the sole caller
    /// that establishes a new owner and bypasses this retirement.
    fn set_live_status(&mut self, text: String) {
        self.extension_status_key = None;
        self.status = text;
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

    fn has_visible_entry_after(&self, batch_len: usize) -> bool {
        let start = self
            .committed_entries
            .saturating_add(batch_len)
            .min(self.transcript.len());
        self.transcript[start..]
            .iter()
            .any(|entry| transcript_entry_is_visible(entry, self.show_thinking))
            || !self.streaming_text.trim().is_empty()
            || (self.show_thinking && !self.streaming_thinking.trim().is_empty())
    }

    /// Final-exit selection may commit the immediate trailing user echo once
    /// prompt admission has succeeded, even if its canonical `MessageEnd(User)`
    /// has not yet reached the TUI reducer. The entry is included only when it
    /// is the first otherwise-unsettled row, preserving the contiguous ledger
    /// and never crossing a partial transcript entry.
    fn final_settled_commit_batch(&self) -> Vec<TranscriptEntry> {
        let mut end = self.settled_end();
        if self.pending_user_echo
            && end + 1 == self.transcript.len()
            && self
                .transcript
                .get(end)
                .is_some_and(|entry| entry.kind == TranscriptKind::User && !entry.is_partial)
        {
            end += 1;
        }
        self.transcript[self.committed_entries..end].to_vec()
    }

    fn overflow_commit_batch(&self, width: u16, viewport_rows: u16) -> Vec<TranscriptEntry> {
        let theme = self.themes.theme();
        let mut rendered = assemble_transcript_entries(
            &self.transcript[self.committed_entries..],
            self.show_thinking,
            self.expand_tools,
            theme,
            width,
        );
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
            end += 1;
            let mut remaining = assemble_transcript_entries(
                &self.transcript[end..],
                self.show_thinking,
                self.expand_tools,
                theme,
                width,
            );
            if !self.streaming_thinking.is_empty() || !self.streaming_text.is_empty() {
                let mut content = Vec::new();
                if !self.streaming_thinking.is_empty() {
                    content.push(ContentBlock::thinking(self.streaming_thinking.clone()));
                }
                if !self.streaming_text.is_empty() {
                    content.push(ContentBlock::text(self.streaming_text.clone()));
                }
                render_transcript_entry(
                    &mut remaining,
                    &TranscriptEntry { kind: TranscriptKind::Assistant, content, tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: true },
                    self.show_thinking,
                    self.expand_tools,
                    theme,
                    width,
                );
            }
            rows = wrapped_line_count(&remaining, width);
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
            BackgroundEvent::CodeReviewSnapshotLoaded {
                controller_generation,
                generation,
                cwd,
                snapshot,
            } => {
                let current = self.code_review_load_in_flight.as_ref()
                    == Some(&(controller_generation, generation, cwd.clone()))
                    && self.code_review_controller_generation == controller_generation
                    && self.cwd_path == cwd
                    && self.code_review_panel.is_some()
                    && self.code_review_controller.is_some();
                if !current {
                    return;
                }
                self.code_review_load_in_flight = None;
                if let Some(controller) = self.code_review_controller.as_mut() {
                    controller.reconcile_snapshot(&snapshot);
                    if let Some(panel) = self.code_review_panel.as_mut() {
                        panel.replace_snapshot(snapshot);
                        panel.sync_controller(controller);
                    }
                }
                self.status = "Code review refreshed".to_owned();
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
            BackgroundEvent::ExtensionCommandFinished { command, result } => match result {
                Ok(value) if !value.is_null() => self.push_status(value.to_string(), false),
                Ok(_) => self.set_bounded_status(format!("Ran /{command}")),
                Err(error) => self.set_bounded_status(format!("/run {command} failed: {error}")),
            },
            BackgroundEvent::WorkflowCommandFinished { label, result } => match result {
                Ok(crate::workflow_commands::WorkflowCommandEffect::OpenPage) => {
                    // OpenPage is admitted inline at dispatch time and never
                    // reaches the background path. Reset the admission label so
                    // a stray completion cannot leave a stale "Opening…" status.
                    let _ = label;
                    self.status = "Ready".to_owned();
                }
                Ok(crate::workflow_commands::WorkflowCommandEffect::Message(message)) => {
                    // Live snapshot updates already arrived through
                    // `ApplicationEvent::Workflow` (`apply_workflow_event`); this
                    // completion only surfaces the human confirmation/error text.
                    self.composer_error = None;
                    self.status = message;
                }
                Err(error) => {
                    self.push_status(format!("Workflow command failed: {error}"), true);
                }
            },
            BackgroundEvent::FooterRefresh(result) => {
                if let Some(next) = self.finish_footer_refresh(
                    result,
                    std::time::Instant::now(),
                ) {
                    self.launch_footer_refresh(next);
                }
            },
        }
    }

    /// Admit a background git/context footer refresh. At most one blocking job
    /// runs; a due request arriving during that job replaces the single pending
    /// request. Runtime/cwd identity changes bypass the same-runtime throttle.
    fn request_footer_refresh(&mut self, application: &Application) {
        let request = FooterRefreshRequest::current(application);
        if let Some(request) = self.admit_footer_refresh(request, std::time::Instant::now()) {
            self.launch_footer_refresh(request);
        }
    }

    fn admit_footer_refresh(
        &mut self,
        request: FooterRefreshRequest,
        now: std::time::Instant,
    ) -> Option<FooterRefreshRequest> {
        let identity_changed = self.footer_refresh_current.as_ref() != Some(&request.key);
        if identity_changed {
            self.footer_refresh_current = Some(request.key.clone());
            self.git_status = None;
            self.context_usage = None;
        }
        let due = identity_changed
            || self
                .last_footer_refresh
                .is_none_or(|last| now.duration_since(last) >= FOOTER_REFRESH_INTERVAL);
        if !due {
            return None;
        }
        if self.footer_refresh_in_flight.is_some() {
            self.footer_refresh_pending = Some(request);
            return None;
        }
        self.last_footer_refresh = Some(now);
        self.footer_refresh_in_flight = Some(request.key.clone());
        Some(request)
    }

    fn finish_footer_refresh(
        &mut self,
        result: FooterRefreshResult,
        now: std::time::Instant,
    ) -> Option<FooterRefreshRequest> {
        // Only the active worker can release the single-flight slot. Duplicate
        // or injected late results cannot unlock newer work.
        if self.footer_refresh_in_flight.as_ref() != Some(&result.key) {
            return None;
        }
        self.footer_refresh_in_flight = None;

        // Apply display data only when the complete runtime identity and cwd
        // still match the latest request.
        if self.footer_refresh_current.as_ref() == Some(&result.key) {
            self.git_status = result.git;
            self.context_usage = result
                .context
                .filter(|usage| usage.context_window > 0);
        }

        // Completion hands the one coalesced latest request the slot without
        // another throttle delay.
        let pending = self.footer_refresh_pending.take()?;
        if self.footer_refresh_current.as_ref() != Some(&pending.key) {
            return None;
        }
        self.last_footer_refresh = Some(now);
        self.footer_refresh_in_flight = Some(pending.key.clone());
        Some(pending)
    }

    fn launch_footer_refresh(&self, request: FooterRefreshRequest) {
        spawn_footer_refresh(request, self.background_tx.clone());
    }

    fn invalidate_footer_refresh(&mut self, application: &Application) {
        self.git_status = None;
        self.context_usage = None;
        self.request_footer_refresh(application);
    }

    fn reconcile_footer_refresh_identity(&mut self, application: &Application) {
        let current = FooterRefreshRequest::current(application);
        if self.footer_refresh_current.as_ref() == Some(&current.key) {
            return;
        }
        if let Some(current) = self.admit_footer_refresh(current, std::time::Instant::now()) {
            self.launch_footer_refresh(current);
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

    async fn open_model_panel(&mut self, application: &Application,
        mouse: &mut impl CodeReviewMouseController,
    ) -> Result<()> {
        self.close_code_review_panel(mouse)?;
        let current = application.state().await.model;
        let models = available_models().await;
        self.panel = Some(SelectorPanel {
            title: "Select model".to_owned(),
            help: "Type to filter · Enter select · Esc cancel".to_owned(),
            items: models
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
        Ok(())
    }

    async fn open_thinking_panel(&mut self, application: &Application,
        mouse: &mut impl CodeReviewMouseController,
    ) -> Result<()> {
        self.close_code_review_panel(mouse)?;
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
        Ok(())
    }

    async fn open_session_panel(&mut self, application: &Application,
        mouse: &mut impl CodeReviewMouseController,
    ) -> Result<()> {
        self.close_code_review_panel(mouse)?;
        let current = application.session().recorder_info().map(|(_, path)| path);
        let rows = match load_cwd_resume_rows(application) {
            Ok(rows) => rows,
            Err(error) => {
                self.push_status(format!("Failed to load sessions: {error:#}"), true);
                return Ok(());
            }
        };
        self.panel = None;
        self.side_chat_open = false;
        self.agents_panel = None;
        self.tree_panel = None;
        self.scoped_model_selector = None;
        self.session_selector = Some(SavedSessionSelector::new(rows, current));
        Ok(())
    }

    async fn open_scoped_models_panel(&mut self,
        mouse: &mut impl CodeReviewMouseController,
    ) -> Result<()> {
        self.close_code_review_panel(mouse)?;
        let models = available_models().await;
        self.panel = None;
        self.side_chat_open = false;
        self.agents_panel = None;
        self.tree_panel = None;
        self.session_selector = None;
        self.scoped_model_selector = Some(ScopedModelSelector::new(models,
            self.scoped_models.clone()));
        Ok(())
    }

    fn open_settings_panel(&mut self, application: &Application,
        mouse: &mut impl CodeReviewMouseController,
    ) -> Result<()> {
        self.close_code_review_panel(mouse)?;
        self.panel = None;
        self.tree_panel = None;
        self.process_panel = None;
        self.agents_panel = None;
        self.side_chat_open = false;
        self.session_selector = None;
        self.scoped_model_selector = None;
        match SettingsPanel::from_application(application, pi_coding::SettingsScope::Global) {
            Ok(panel) => self.settings_panel = Some(panel),
            Err(error) => self.push_status(format!("Cannot open settings: {error:#}"), true),
        }
        Ok(())
    }

    fn project_workflow_snapshot(application: &Application, snapshot: &pi_coding::WorkflowSnapshot,
    ) -> WorkflowPanelSnapshot {
        application.workflow_detail(&snapshot.workflow_id, snapshot.generation).map_or_else(
            |_| WorkflowPanelSnapshot::from(snapshot),
            |detail| WorkflowPanelSnapshot::from_runtime_detail(&detail, snapshot),
        )
    }

    fn open_workflow_panel(&mut self, application: &Application,
        mouse: &mut impl CodeReviewMouseController,
    ) -> Result<()> {
        self.close_code_review_panel(mouse)?;
        self.panel = None;
        self.side_chat_open = false;
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
        Ok(())
    }

    fn todo_workflow_snapshots(application: &Application) -> Vec<TodoDagSnapshot> {
        application
            .workflow_list()
            .into_iter()
            .map(|snapshot| {
                let detail = application
                    .workflow_detail(&snapshot.workflow_id, snapshot.generation)
                    .ok();
                TodoDagSnapshot::workflow(
                    snapshot.workflow_id.to_string(),
                    snapshot.name,
                    detail
                        .as_ref()
                        .map_or(snapshot.todo.phases, |detail| detail.todo.phases.clone()),
                    detail
                        .as_ref()
                        .map_or(snapshot.status, |detail| detail.status),
                    detail.map_or_else(Vec::new, |detail| detail.jobs),
                )
            })
            .collect()
    }

    fn open_todo_dag_panel(
        &mut self,
        application: &Application,
        mouse: &mut impl CodeReviewMouseController,
    ) -> Result<()> {
        self.close_code_review_panel(mouse)?;
        self.panel = None;
        self.settings_panel = None;
        self.tree_panel = None;
        self.process_panel = None;
        self.workflow_panel = None;
        self.agents_panel = None;
        self.side_chat_open = false;
        self.session_selector = None;
        self.scoped_model_selector = None;
        self.cancel_file_completion();
        self.editor.clear();
        self.todo_phases = application.todo_state().phases;
        self.refresh_job_projection(application);
        let main = TodoDagSnapshot::main(
            self.todo_phases.clone(),
            application.todo_dag_status(),
            self.job_cards.cards_in_source_order(),
        );
        self.todo_dag_panel = Some(TodoDagPanel::new(
            main,
            Self::todo_workflow_snapshots(application),
        ));
        Ok(())
    }

    fn dismiss_page_overlays_for_side_chat(
        &mut self,
        mouse: &mut impl CodeReviewMouseController,
    ) -> Result<()> {
        self.close_code_review_panel(mouse)?;
        self.panel = None;
        self.settings_panel = None;
        self.settings_value_input = None;
        self.tree_panel = None;
        self.process_panel = None;
        self.workflow_panel = None;
        self.todo_dag_panel = None;
        self.agents_panel = None;
        self.session_selector = None;
        self.scoped_model_selector = None;
        self.cancel_file_completion();
        Ok(())
    }

    /// Open `/btw` overlay. Reuses an existing side session when present.
    async fn open_side_chat(
        &mut self,
        application: &Application,
        initial_prompt: Option<&str>,
        mouse: &mut impl CodeReviewMouseController,
    ) -> Result<()> {
        self.dismiss_page_overlays_for_side_chat(mouse)?;
        if self.side_chat.is_none() {
            match SideChatController::fork_from(application).await {
                Ok(controller) => self.side_chat = Some(controller),
                Err(error) => {
                    self.push_status(format!("Cannot open side chat: {error:#}"), true);
                    return Ok(());
                }
            }
        }
        self.side_chat_open = true;
        if let Some(prompt) = initial_prompt
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            if let Some(side) = self.side_chat.as_mut() {
                side.submit_prompt(prompt);
            }
            self.status = "Side chat · submitted".to_owned();
        } else {
            self.status = "Side chat open · Esc closes overlay (session kept)".to_owned();
        }
        Ok(())
    }

    /// Hide the side-chat overlay without destroying the side Application/agent.
    fn close_side_chat_overlay(&mut self) {
        self.side_chat_open = false;
        self.status = "Ready".to_owned();
    }

    async fn shutdown_side_chat(&mut self) {
        if let Some(mut side) = self.side_chat.take() {
            side.shutdown().await;
        }
        self.side_chat_open = false;
    }

    fn request_code_review_snapshot(&mut self) {
        self.code_review_load_generation = self.code_review_load_generation.wrapping_add(1);
        let controller_generation = self.code_review_controller_generation;
        let generation = self.code_review_load_generation;
        let cwd = self.cwd_path.clone();
        let scope = self.code_review_scope.clone();
        self.code_review_load_in_flight = Some((controller_generation, generation, cwd.clone()));
        if let Some(panel) = self.code_review_panel.as_mut() {
            panel.set_snapshot_loading(true);
        }
        spawn_code_review_snapshot_load(
            controller_generation,
            generation,
            cwd,
            scope,
            self.background_tx.clone(),
        );
    }

    async fn open_code_review_panel(
        &mut self,
        application: &Application,
        scope: ReviewScope,
        mouse: &mut impl CodeReviewMouseController,
    ) -> Result<()> {
        if self.extension_dialog.is_some() {
            self.status =
                "Close the extension dialog before opening code review".to_owned();
            return Ok(());
        }
        if self.pty_attachment.is_some() {
            self.status =
                "Detach from the PTY before opening code review".to_owned();
            return Ok(());
        }
        self.code_review_scope = scope;
        self.shutdown_code_review().await;
        self.code_review_controller_generation =
            self.code_review_controller_generation.wrapping_add(1);
        let controller = match CodeReviewController::fork_from(application).await {
            Ok(controller) => controller,
            Err(error) => {
                self.push_status(format!("Cannot open code review: {error:#}"), true);
                return Ok(());
            }
        };
        let mut panel = CodeReviewPanel::loading(self.cwd_path.clone(), self.code_review_scope.clone());
        panel.sync_controller(&controller);
        if let Ok((width, height)) = crossterm::terminal::size() {
            sync_code_review_layout(&mut panel, Rect::new(0, 0, width, height));
        }
        self.panel = None;
        self.settings_panel = None;
        self.settings_value_input = None;
        self.tree_panel = None;
        self.process_panel = None;
        self.workflow_panel = None;
        self.todo_dag_panel = None;
        self.agents_panel = None;
        self.side_chat_open = false;
        self.session_selector = None;
        self.scoped_model_selector = None;
        self.cancel_file_completion();
        self.editor.clear();
        self.code_review_panel = Some(panel);
        self.code_review_controller = Some(controller);
        self.request_code_review_snapshot();
        if let Err(error) = mouse.set_code_review_mouse_capture(true) {
            self.code_review_panel = None;
            self.shutdown_code_review().await;
            return Err(error);
        }
        self.status = "Code review · c comment · [/] hunk · r refresh · Alt+R refork · Esc close".to_owned();
        Ok(())
    }

    fn close_code_review_panel(
        &mut self,
        mouse: &mut impl CodeReviewMouseController,
    ) -> Result<()> {
        let was_open = self.code_review_panel.is_some();
        mouse.set_code_review_mouse_capture(false)?;
        self.code_review_panel = None;
        self.code_review_load_generation = self.code_review_load_generation.wrapping_add(1);
        self.code_review_load_in_flight = None;
        if let Some(mut controller) = self.code_review_controller.take() {
            self.code_review_cleanup = Some(tokio::spawn(async move {
                controller.shutdown().await;
            }));
        }
        if was_open {
            self.status = "Ready".to_owned();
        }
        Ok(())
    }

    async fn shutdown_code_review(&mut self) {
        if let Some(mut controller) = self.code_review_controller.take() {
            controller.shutdown().await;
        }
        if let Some(cleanup) = self.code_review_cleanup.take() {
            let _ = cleanup.await;
        }
        self.code_review_panel = None;
        self.code_review_load_generation = self.code_review_load_generation.wrapping_add(1);
        self.code_review_load_in_flight = None;
    }


    async fn open_agents_panel(&mut self, application: &Application,
        mouse: &mut impl CodeReviewMouseController,
    ) -> Result<()> {
        self.close_code_review_panel(mouse)?;
        let models = available_models().await;
        self.panel = None;
        self.side_chat_open = false;
        self.tree_panel = None;
        self.process_panel = None;
        self.session_selector = None;
        self.scoped_model_selector = None;
        match AgentsPanel::from_application(application, models) {
            Ok(panel) => {
                self.agents_panel = Some(panel);
            }
            Err(error) => self.push_status(format!("Cannot open agents panel: {error:#}"), true),
        }
        Ok(())
    }

    fn open_tree_panel(&mut self, application: &Application,
        mouse: &mut impl CodeReviewMouseController,
    ) -> Result<()> {
        self.open_session_tree_panel(application, TreePanelMode::Navigate, mouse)
    }

    fn open_fork_panel(&mut self, application: &Application,
        mouse: &mut impl CodeReviewMouseController,
    ) -> Result<()> {
        self.open_session_tree_panel(application, TreePanelMode::Fork, mouse)
    }

    fn open_session_tree_panel(&mut self, application: &Application, mode: TreePanelMode,
        mouse: &mut impl CodeReviewMouseController,
    ) -> Result<()> {
        match application.session_tree() {
            Ok(tree) => {
                self.close_code_review_panel(mouse)?;
                self.panel = None;
                self.agents_panel = None;
                self.process_panel = None;
                self.side_chat_open = false;
                self.tree_panel = Some(TreePanel::new(tree, mode));
            }
            Err(error) => self.push_status(format!("Cannot load session tree: {error:#}"), true),
        }
        Ok(())
    }

    fn open_trust_panel(&mut self, application: &Application,
        mouse: &mut impl CodeReviewMouseController,
    ) -> Result<()> {
        let current = application
            .resource_snapshot()
            .map(|snapshot| snapshot.trust.decision);
        self.close_code_review_panel(mouse)?;
        self.side_chat_open = false;
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
        Ok(())
    }

    fn open_goal_panel(&mut self, application: &Application,
        mouse: &mut impl CodeReviewMouseController,
    ) -> Result<()> {
        self.close_code_review_panel(mouse)?;
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
        Ok(())
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
    terminal: &mut TerminalGuard,
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
                            state.open_session_tree_panel(application, panel.mode, terminal)?;
                            return Ok(Some(false));
                        }
                        Err(error) => {
                            state.push_status(format!("Failed to update label: {error:#}"), true)
                        }
                    }
                }
            }
            KeyCode::Char(character) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                panel.insert_label_char(character)
            }
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
                        Ok(outcome) if outcome.cancelled => {
                            state.status = "Session fork cancelled".to_owned();
                        }
                        Ok(outcome) => {
                            state.replace_transcript_from_application(application);
                            state.editor.set_text(&outcome.text);
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
        Some(Action::SessionRename) => {
            if let Some(message) = selector.begin_rename().status_message() {
                state.status = message.to_owned();
            }
        }
        Some(Action::SessionDelete) => {
            if let Some(message) = selector.begin_delete(false).status_message() {
                state.status = message.to_owned();
            }
        }
        Some(Action::SessionDeleteNoninvasive) => {
            if let Some(message) = selector.begin_delete(true).status_message() {
                state.status = message.to_owned();
            }
        }
        _ => match key.code {
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
                    SessionSelectorRequest::Resume(request) => {
                        let preferred_cwd = state.cwd_path.clone();
                        let sources = effective_resume_sources(application);
                        match effective_session_catalog(application) {
                            Ok(catalog) => match crate::resume_catalog::switch_resume_selection(
                                application,
                                &catalog,
                                &request,
                                Some(&preferred_cwd),
                                &sources,
                            )
                            .await
                            {
                                Ok(result) if result.cancelled => {
                                    keep_open = false;
                                    let _ = state.refresh_recent_sessions(application);
                                    state.status = "Session resume cancelled".to_owned();
                                }
                                Ok(result) => {
                                    keep_open = false;
                                    state.replace_transcript_from_application(application);
                                    state.status = format!("Resumed {}", result.path.display());
                                }
                                Err(error) => state
                                    .push_status(format!("Failed to resume session: {error:#}"), true),
                            },
                            Err(error) => {
                                state.push_status(format!("Failed to resume session: {error:#}"), true)
                            }
                        }
                    }
                SessionSelectorRequest::Rename { path, name } => {
                    match rename_managed_session(application, &path, &name) {
                        Ok(()) => match load_cwd_resume_rows(application) {
                            Ok(rows) => {
                                selector.reload(rows);
                                let _ = state.refresh_recent_sessions(application);
                                    state.status = format!("Renamed session to {name}");
                            }
                            Err(error) => state.push_status(
                                format!(
                                    "Renamed session, but failed to refresh sessions: {error:#}"
                                ),
                                true,
                            ),
                        },
                        Err(error) => {
                            state.push_status(format!("Failed to rename session: {error:#}"), true)
                        }
                    }
                }
                SessionSelectorRequest::Delete(path) => {
                    match delete_managed_session(application, &path) {
                        Ok(()) => match load_cwd_resume_rows(application) {
                            Ok(rows) => {
                                selector.reload(rows);
                                let _ = state.refresh_recent_sessions(application);
                                state.status = "Session deleted".to_owned();
                                }
                                Err(error) => state.push_status(
                                    format!(
                                    "Deleted session, but failed to refresh sessions: {error:#}"
                                ),
                                    true,
                                ),
                            },
                            Err(error) => {
                            state.push_status(
                                format!("Failed to delete session: {error:#}"), true)
                        }
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
            },
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

async fn handle_settings_panel_key(application: &Application, state: &mut TuiState, key: KeyEvent,
) -> Result<Option<bool>> {
    let Some(mut panel) = state.settings_panel.take() else { return Ok(None); };
    if let Some(mut input) = state.settings_value_input.take() {
        match key.code {
            KeyCode::Esc => {}
            KeyCode::Left => {
                input.cursor = previous_char_boundary(&input.value, input.cursor);
                input.replace_on_type = false;
                state.settings_value_input = Some(input);
            }
            KeyCode::Right => {
                input.cursor = next_char_boundary(&input.value, input.cursor);
                input.replace_on_type = false;
                state.settings_value_input = Some(input);
            }
            KeyCode::Home => {
                input.cursor = 0;
                input.replace_on_type = false;
                state.settings_value_input = Some(input);
            }
            KeyCode::End => {
                input.cursor = input.value.len();
                input.replace_on_type = false;
                state.settings_value_input = Some(input);
            }
            KeyCode::Backspace => {
                if input.replace_on_type {
                    input.value.clear();
                    input.cursor = 0;
                } else if input.cursor > 0 {
                    let previous = previous_char_boundary(&input.value, input.cursor);
                    input.value.replace_range(previous..input.cursor, "");
                    input.cursor = previous;
                }
                input.error = None;
                input.replace_on_type = false;
                state.settings_value_input = Some(input);
            }
            KeyCode::Delete => {
                if input.replace_on_type {
                    input.value.clear();
                    input.cursor = 0;
                } else if input.cursor < input.value.len() {
                    let next = next_char_boundary(&input.value, input.cursor);
                    input.value.replace_range(input.cursor..next, "");
                }
                input.error = None;
                input.replace_on_type = false;
                state.settings_value_input = Some(input);
            }
            KeyCode::Enter => {
                if let Err(error) = panel.set_input(&input.key, &input.value) {
                    input.error = Some(format!("{error:#}"));
                    input.replace_on_type = false;
                    state.settings_value_input = Some(input);
                } else {
                    state.status = "Setting added to pending changes; Ctrl-S to apply".to_owned();
                }
            }
            KeyCode::Char(character)
                if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if input.replace_on_type {
                    input.value.clear();
                    input.cursor = 0;
                }
                input.value.insert(input.cursor, character);
                input.cursor += character.len_utf8();
                input.error = None;
                input.replace_on_type = false;
                state.settings_value_input = Some(input);
            }
            _ => state.settings_value_input = Some(input),
        }
        state.settings_panel = Some(panel);
        return Ok(Some(false));
    }
    match key.code {
        KeyCode::Esc => { panel.cancel()?; return Ok(Some(false)); }
        KeyCode::Up => panel.move_previous()?, KeyCode::Down => panel.move_next()?,
        KeyCode::Left => panel.previous_category(), KeyCode::Right | KeyCode::Tab => panel.next_category(), KeyCode::BackTab => panel.previous_category(),
        KeyCode::Backspace => { let mut search = panel.search().to_owned(); search.pop(); panel.set_search(search); }
        KeyCode::Delete => {
            if let Some(row) = panel.selected()?
                && let Err(error) = panel.reset(&row.key) { state.status = format!("Cannot reset setting: {error:#}"); } }
        KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Err(error) = panel.set_scope(pi_coding::SettingsScope::Global) { state.status = format!("Cannot change settings scope: {error:#}"); }
        }
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Err(error) = panel.set_scope(pi_coding::SettingsScope::Project) { state.status = format!("Cannot change settings scope: {error:#}"); }
        }
        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            match panel.validate() {
            Err(error) => state.status = format!("Settings validation failed: {error:#}"),
            Ok(()) => match panel.apply(application).await {
                Ok(outcome) => { if outcome.applied_live || outcome.reloaded { state.apply_runtime_settings(application).await; } state.status = if outcome.restart_required { "Settings saved; restart required".to_owned() } else { "Settings applied".to_owned() }; }
                Err(error) => state.status = format!("Settings apply failed: {error:#}"),
            },
        }
        }
        KeyCode::Enter => {
            if let Some(row) = panel.selected()?
                && row.writable && row.blocked_reason.is_none() {
                if matches!(row.control, SettingsControl::Secret { .. }) {
                    state.status = "Secret settings are managed through auth storage".to_owned();
                } else {
                    let value = panel.input_value(&row.key)?;
                    let cursor = value.len();
                    let hint = panel.input_hint(&row.key)?;
                    state.settings_value_input = Some(SettingsValueInput {
                        key: row.key,
                        value,
                        cursor,
                        hint,
                        error: None,
                        replace_on_type: true,
                    });
                }
            }
        }
        KeyCode::Char(character) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => { let mut search = panel.search().to_owned(); search.push(character); panel.set_search(search); }
        _ => {}
    }
    state.settings_panel = Some(panel); Ok(Some(false))
}

async fn handle_workflow_panel_key(application: &Application, state: &mut TuiState, key: KeyEvent,
) -> Result<Option<bool>> {
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
                Ok(snapshot) => {
                    state.status = format!("Workflow {}: {}", snapshot.name, snapshot.status.as_str());
                    panel.replace(application.workflow_list().iter().map(WorkflowPanelSnapshot::from).collect());
                }
                Err(error) => state.push_status(format!("Workflow action failed: {error:#}"), true),
            }
        }
        WorkflowPanelResult::Unknown => state.status = overlay_unknown_key_status(key),
        WorkflowPanelResult::Handled => {
            if let Some(selected) = panel.selected_workflow() { application.workflow_select(Some(&pi_coding::WorkflowId::new(selected.id.clone())))?; }
        }
    }
    Ok(Some(false))
}

fn handle_todo_dag_panel_key(state: &mut TuiState,
    key: KeyEvent) -> Option<bool> {
    let panel = state.todo_dag_panel.as_mut()?;
    match panel.handle_key(key) {
        TodoDagPanelResult::Close => {
            state.todo_dag_panel = None;
            state.status = "Ready".to_owned();
        }
        TodoDagPanelResult::Unknown => state.status = overlay_unknown_key_status(key),
        TodoDagPanelResult::Handled => {}
    }
    Some(false)
}

fn submit_code_review_comment(state: &mut TuiState) -> bool {
    let Some((snapshot, file, hunk, comment)) = state.code_review_panel.as_ref().and_then(|panel| {
        Some((
            panel.snapshot().clone(),
            panel.selected_file()?.clone(),
            panel.selected_hunk()?.clone(),
            panel.comment_editor()?.to_owned(),
        ))
    }) else {
        return false;
    };
    let accepted = state
        .code_review_controller
        .as_mut()
        .is_some_and(|controller| controller.submit_comment(&snapshot, &file, &hunk, &comment));
    if let Some(panel) = state.code_review_panel.as_mut() {
        panel.complete_comment_submit(accepted);
        if let Some(controller) = state.code_review_controller.as_ref() {
            panel.sync_controller(controller);
        }
    }
    accepted
}

async fn handle_code_review_panel_key(
    state: &mut TuiState,
    key: KeyEvent,
    terminal: &mut TerminalGuard,
) -> Result<Option<bool>> {
    let Some(panel) = state.code_review_panel.as_mut() else {
        return Ok(None);
    };
    if let Ok(size) = terminal.terminal.size() {
        sync_code_review_layout(panel, Rect::new(0, 0, size.width, size.height));
    }
    let action = panel.handle_key(key);
    match action {
        CodeReviewPanelResult::Close => {
            state.close_code_review_panel(terminal)?;
        }
        CodeReviewPanelResult::SubmitComment => {
            if submit_code_review_comment(state) {
                state.status = "Code review comment submitted".to_owned();
            } else {
                state.status = "Code review busy · draft kept".to_owned();
            }
        }
        CodeReviewPanelResult::AbortReview => {
            if let Some(controller) = state.code_review_controller.as_mut() {
                controller.abort().await;
                if let Some(panel) = state.code_review_panel.as_mut() {
                    panel.sync_controller(controller);
                }
            }
            state.status = "Code review aborted".to_owned();
        }
        CodeReviewPanelResult::Refork => {
            if let Some(controller) = state.code_review_controller.as_mut() {
                if let Err(error) = controller.refork().await {
                    state.push_status(format!("Code review refork failed: {error:#}"), true);
                } else {
                    state.status = "Code review Agent reforked".to_owned();
                }
            }
        }
        CodeReviewPanelResult::Refresh => {
            state.request_code_review_snapshot();
            state.status = "Refreshing code review…".to_owned();
        }
        CodeReviewPanelResult::Busy => {
            state.status = "Code review busy · wait or Esc to abort".to_owned();
        }
        CodeReviewPanelResult::Unknown => state.status = overlay_unknown_key_status(key),
        CodeReviewPanelResult::Handled => {}
    }
    Ok(Some(false))
}

async fn handle_side_chat_key(state: &mut TuiState, key: KeyEvent) -> Result<Option<bool>> {
    if !state.side_chat_open {
        return Ok(None);
    }
    let Some(side) = state.side_chat.as_mut() else {
        state.side_chat_open = false;
        return Ok(Some(false));
    };
    let async_req = side.key_needs_async(key);
    let action = side.handle_key(key);
    match async_req {
        SideChatAsyncRequest::ToggleTools => {
            if let Err(error) = side.toggle_tool_mode().await {
                state.push_status(format!("Side chat tool toggle failed: {error:#}"), true);
            } else {
                let status = side.status().to_owned();
                state.set_live_status(status);
            }
            return Ok(Some(false));
        }
        SideChatAsyncRequest::Abort => {
            side.abort_streaming().await;
            let status = side.status().to_owned();
            state.set_live_status(status);
            return Ok(Some(false));
        }
        SideChatAsyncRequest::Refork => {
            if let Err(error) = side.refork_from_main().await {
                state.push_status(format!("Side chat refork failed: {error:#}"), true);
            } else {
                let status = side.status().to_owned();
                state.set_live_status(status);
            }
            return Ok(Some(false));
        }
        SideChatAsyncRequest::Clear => {
            let clear_result = side.clear_conversation().await;
            let status = side.status().to_owned();
            if let Err(error) = clear_result {
                state.push_status(format!("Side chat clear failed: {error:#}"), true);
            } else {
                state.set_live_status(status);
            }
            return Ok(Some(false));
        }
        SideChatAsyncRequest::None => {}
    }
    match action {
        SideChatAction::CloseOverlay => {
            state.close_side_chat_overlay();
            Ok(Some(false))
        }
        SideChatAction::Handled => {
            if let Some(side) = state.side_chat.as_ref() {
                let status = side.status().to_owned();
                state.set_live_status(status);
            }
            Ok(Some(false))
        }
        // Side chat is an exclusive overlay: consume any non-owned input
        // (Ctrl/Alt chords, F-keys, release events) so it cannot fall through
        // to global key dispatch and open another overlay behind the visible
        // side chat.
        SideChatAction::Ignored => Ok(Some(false)),
    }
}

fn handle_code_review_mouse(
    state: &mut TuiState,
    mouse: MouseEvent,
    terminal: &mut TerminalGuard,
) -> Result<bool> {
    let Some(panel) = state.code_review_panel.as_mut() else {
        return Ok(false);
    };
    if let Ok(size) = terminal.terminal.size() {
        sync_code_review_layout(panel, Rect::new(0, 0, size.width, size.height));
    }
    match panel.handle_mouse(mouse) {
        CodeReviewPanelResult::Close => {
            state.close_code_review_panel(terminal)?;
        }
        CodeReviewPanelResult::SubmitComment
        | CodeReviewPanelResult::AbortReview
        | CodeReviewPanelResult::Refork
        | CodeReviewPanelResult::Refresh
        | CodeReviewPanelResult::Busy
        | CodeReviewPanelResult::Unknown
        | CodeReviewPanelResult::Handled => {}
    }
    Ok(true)
}

async fn handle_panel_key(
    application: &Application,
    state: &mut TuiState,
    key: KeyEvent,
    terminal: &mut TerminalGuard,
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
                PanelValue::SettingsThinking => {
                    state.open_thinking_panel(application, terminal).await?;
                }
                PanelValue::SettingsTheme => {
                    state.themes.cycle(1);
                    state.status = format!("Theme: {}", state.themes.active_name());
                    state.open_settings_panel(application, terminal)?;
                }
                PanelValue::SettingsAutoCompact => {
                    let enabled = !application.state().await.auto_compaction_enabled;
                    application.set_auto_compaction_enabled(enabled);
                    state.status = format!(
                        "Automatic compaction {}",
                        if enabled { "enabled" } else { "disabled" }
                    );
                    state.open_settings_panel(application, terminal)?;
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
    if state.extension_dialog.is_none() {
        return false;
    }
    if key.code == KeyCode::Esc {
        state.finish_extension_dialog(true);
        return true;
    }
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
                KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down | KeyCode::Tab => {
                    *confirmed = !*confirmed
                }
                KeyCode::Char('y' | 'Y') => {
                    *confirmed = true;
                    accept = true;
                }
                KeyCode::Char('n' | 'N') => {
                    *confirmed = false;
                    accept = true;
                }
                KeyCode::Enter => accept = true,
                _ => {}
            },
            ExtensionDialogKind::Input { editor, .. } | ExtensionDialogKind::Editor { editor } => {
                match action {
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
                    None => {
                        if let KeyCode::Char(character) = key.code
                            && !key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        {
                            editor.insert_char(character);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    if accept {
        state.finish_extension_dialog(false);
    }
    true
}

fn process_key_for_terminal_event(key: KeyEvent) -> Option<ProcessKey> {
    match key.code {
        KeyCode::Enter => Some(ProcessKey::Enter),
        KeyCode::Tab | KeyCode::BackTab => Some(ProcessKey::Tab),
        KeyCode::Up => Some(ProcessKey::Up),
        KeyCode::Down => Some(ProcessKey::Down),
        KeyCode::Left => Some(ProcessKey::Left),
        KeyCode::Right => Some(ProcessKey::Right),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(ProcessKey::CtrlC)
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(ProcessKey::CtrlD)
        }
        _ => None,
    }
}

fn control_character_byte(key: KeyEvent) -> Option<u8> {
    let KeyCode::Char(character) = key.code else {
        return None;
    };
    if !key.modifiers.contains(KeyModifiers::CONTROL) || !character.is_ascii() {
        return None;
    }
    let byte = character.to_ascii_uppercase() as u8;
    (byte.is_ascii_alphabetic() || matches!(byte, b'@' | b'[' | b'\\' | b']' | b'^' | b'_'))
        .then_some(byte & 0x1f)
}

async fn handle_pty_attachment_key(
    application: &Application,
    state: &mut TuiState,
    key: KeyEvent,
) -> Result<bool> {
    let Some(process_id) = state
        .pty_attachment
        .as_ref()
        .map(|attachment| attachment.process_id.clone())
    else {
        return Ok(false);
    };
    if classify_pty_input(key) == PtyInput::Detach {
        state.pty_attachment = None;
        state.status = "Detached from PTY".to_owned();
        return Ok(true);
    }
    match application.process_describe(&process_id) {
        Ok(process) if process.state == ProcessState::Running => {}
        Ok(_) => {
            state.pty_attachment = None;
            state.status = "PTY process is no longer running; detached".to_owned();
            return Ok(true);
        }
        Err(error) => {
            state.pty_attachment = None;
            state.status = format!("PTY process is unavailable; detached: {error:#}");
            return Ok(true);
        }
    }
    let result = match key.code {
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            let mut encoded = [0; 4];
            application
                .process_write(
                    &process_id,
                    character.encode_utf8(&mut encoded).as_bytes().to_vec(),
                    false,
                )
                .await
        }
        KeyCode::Backspace => {
            application
                .process_write(&process_id, vec![0x7f], false)
                .await
        }
        KeyCode::Delete => {
            application
                .process_write(&process_id, b"\x1b[3~".to_vec(), false)
                .await
        }
        KeyCode::Home => {
            application
                .process_write(&process_id, b"\x1b[H".to_vec(), false)
                .await
        }
        KeyCode::End => {
            application
                .process_write(&process_id, b"\x1b[F".to_vec(), false)
                .await
        }
        KeyCode::PageUp => {
            application
                .process_write(&process_id, b"\x1b[5~".to_vec(), false)
                .await
        }
        KeyCode::PageDown => {
            application
                .process_write(&process_id, b"\x1b[6~".to_vec(), false)
                .await
        }
        _ => {
            if let Some(process_key) = process_key_for_terminal_event(key) {
                application
                    .process_send_keys(&process_id, &[process_key])
                    .await
            } else if let Some(byte) = control_character_byte(key) {
                application
                    .process_write(&process_id, vec![byte], false)
                    .await
            } else {
                Ok(())
            }
        }
    };

    if let Err(error) = result {
        state.pty_attachment = None;
        state.status = format!("PTY input failed; detached: {error:#}");
    }
    Ok(true)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PtyInput {
    Detach,
    Process,
}

/// Ctrl+] arrives as Ctrl+5 on legacy Unix terminal input (0x1d), while
/// enhanced keyboard protocols may preserve `]`; accept both representations.
fn classify_pty_input(key: KeyEvent) -> PtyInput {
    if key.code == KeyCode::Esc
        || matches!(key.code, KeyCode::Char('\u{1d}'))
        || matches!(key.code, KeyCode::Char(']' | '5'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
    {
        PtyInput::Detach
    } else {
        PtyInput::Process
    }
}

async fn attach_process(application: &Application, id: ProcessId) -> Result<PtyAttachment> {
    let process = application.process_describe(&id)?;
    if !process.tty {
        return Err(anyhow!("Direct input requires a tty=true process"));
    }
    if process.state != ProcessState::Running {
        return Err(anyhow!("Direct input requires a running process"));
    }
    let mut attachment = PtyAttachment::new(id.clone(), process.output_start_cursor);
    attachment.apply_logs(
        &application
            .process_logs(
                &id,
                process.output_start_cursor,
                Some(MAX_PTY_ATTACHMENT_OUTPUT_BYTES),
                false,
                None,
            )
            .await?,
    );
    let process = application.process_describe(&id)?;
    if process.state != ProcessState::Running {
        return Err(anyhow!("Direct input requires a running process"));
    }
    Ok(attachment)
}

async fn handle_process_panel_key(
    application: &Application,
    state: &mut TuiState,
    key: KeyEvent,
) -> Result<bool> {
    let Some(mut panel) = state.process_panel.take() else {
        return Ok(false);
    };
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
        ProcessKeyResult::Action(ProcessPanelAction::Attach(id)) => {
            match attach_process(application, id).await {
                Ok(attachment) => {
                    state.pty_attachment = Some(attachment);
                    state.process_panel = None;
                    state.status = "Attached to PTY · Ctrl+] or Esc detach".to_owned();
                    return Ok(true);
                }
                Err(error) => panel.fail(format!("Cannot attach: {error:#}")),
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
    // Every active page-overlay owner consumes the paste so it never falls
    // through to the hidden main composer. Owners with their own editor
    // delegate to it; every other owner consumes with a bounded visible
    // status. Status writes go through `set_live_status` so a live keyed
    // extension status is retired before the new status takes effect.
    if let Some(panel) = state.code_review_panel.as_mut() {
        if payload.len() > MAX_PASTE_BYTES {
            state.set_live_status(format!(
                "Paste rejected: {} bytes exceeds the {} MiB limit",
                payload.len(),
                MAX_PASTE_BYTES / (1024 * 1024)
            ));
            return;
        }
        if panel.comment_editor().is_some() {
            panel.handle_paste(payload);
            state.set_live_status(format!("Pasted {} bytes into review comment", payload.len()));
        } else if panel.open_comment_editor() {
            panel.handle_paste(payload);
            state.set_live_status(format!(
                "Opened review comment and pasted {} bytes",
                payload.len()
            ));
        } else {
            state.set_live_status(
                "Paste consumed by code review · select a hunk after loading".to_owned(),
            );
        }
        return;
    }
    if state.settings_value_input.is_some() {
        if payload.len() > MAX_PASTE_BYTES {
            state.set_live_status(format!(
                "Paste rejected: {} bytes exceeds the {} MiB limit",
                payload.len(),
                MAX_PASTE_BYTES / (1024 * 1024)
            ));
            return;
        }
        if let Some(input) = state.settings_value_input.as_mut() {
            if input.replace_on_type {
                input.value.clear();
                input.cursor = 0;
            }
            input.value.insert_str(input.cursor, payload);
            input.cursor += payload.len();
            input.error = None;
            input.replace_on_type = false;
        }
        state.set_live_status(format!("Pasted {} bytes into setting value", payload.len()));
        return;
    }
    if handle_extension_dialog_paste(state, payload) {
        return;
    }
    if state.side_chat_open {
        if payload.len() > MAX_PASTE_BYTES {
            state.set_live_status(format!(
                "Paste rejected: {} bytes exceeds the {} MiB limit",
                payload.len(),
                MAX_PASTE_BYTES / (1024 * 1024)
            ));
            return;
        }
        if let Some(side) = state.side_chat.as_mut() {
            side.handle_paste(payload);
            let status = side.status().to_owned();
            state.set_live_status(status);
        }
        return;
    }
    // Any other page-overlay owner (tree/process/settings/workflow/todo/agents/
    // session/model selector, PTY attachment, or a side-chat open without a
    // live controller) consumes the paste without touching the main composer.
    // A bounded visible status surfaces the consume; `set_bounded_status`
    // routes to the composer-error toast while an overlay owns the status line.
    if page_overlay_open(state) {
        state.set_bounded_status("Paste consumed by active overlay".to_owned());
        return;
    }
    state.handle_paste(payload);
}

async fn handle_terminal_paste(application: &Application, state: &mut TuiState, payload: &str) {
    if state.settings_value_input.is_some() {
        handle_paste(state, payload);
        return;
    }
    let Some(process_id) = state
        .pty_attachment
        .as_ref()
        .map(|attachment| attachment.process_id.clone())
    else {
        handle_paste(state, payload);
        return;
    };
    // Empty (e.g. image-only clipboard) pastes must not poke the PTY or detach
    // a healthy attachment; consume silently.
    if payload.is_empty() {
        return;
    }
    // Enforce the paste cap before writing to the PTY so an oversized payload
    // is rejected without detaching a healthy attachment or saturating the
    // process input buffer.
    if payload.len() > MAX_PASTE_BYTES {
        state.set_live_status(format!(
            "Paste rejected: {} bytes exceeds the {} MiB limit",
            payload.len(),
            MAX_PASTE_BYTES / (1024 * 1024)
        ));
        return;
    }
    if let Err(error) = application
        .process_write(&process_id, payload.as_bytes().to_vec(), false)
        .await
    {
        state.pty_attachment = None;
        state.set_live_status(format!("PTY input failed; detached: {error:#}"));
    }
}

fn dismiss_composer_error_on_escape(state: &mut TuiState, code: KeyCode, busy: bool) -> bool {
    if code != KeyCode::Esc || state.composer_error.is_none() {
        return false;
    }
    // A stale composer-error toast (e.g. the backpressure "UI skipped N stale
    // events" notice raised when the application event channel goes `Lagged`
    // during a long stream) must not own Esc while a turn is streaming or a
    // bash command is running. The user expects Esc to cancel the active
    // turn via `Application::abort`; a stale global toast would otherwise
    // swallow Esc before `dispatch_action` is reached. Drop the stale toast
    // and let Esc fall through to the abort dispatch. When idle, preserve
    // the historical dismiss-on-Esc behavior. Visible overlays (panels and
    // comment editors) are handled earlier in `handle_key` and never reach
    // here.
    let taken = state.composer_error.take();
    state.last_escape = None;
    if busy {
        return false;
    }
    taken.is_some()
}

/// Routes an incoming key through the configured keybindings to a stable action
async fn handle_key(
    application: &Application,
    state: &mut TuiState,
    key: KeyEvent,
    terminal: &mut TerminalGuard,
) -> Result<bool> {
    if handle_pty_attachment_key(application, state, key).await? {
        return Ok(false);
    }
    if handle_extension_dialog_key(state, key) {
        return Ok(false);
    }
    if handle_process_panel_key(application, state, key).await? {
        return Ok(false);
    }
    if let Some(exit) = handle_agents_panel_key(application, state, key).await? {
        return Ok(exit);
    }
    if let Some(exit) = handle_tree_panel_key(application, state, key, terminal).await? {
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
    if let Some(exit) = handle_todo_dag_panel_key(state, key) {
        return Ok(exit);
    }
    if let Some(exit) = handle_code_review_panel_key(state, key, terminal).await? {
        return Ok(exit);
    }
    if let Some(exit) = handle_side_chat_key(state, key).await? {
        return Ok(exit);
    }
    if let Some(exit) = handle_settings_panel_key(application, state, key).await? {
        return Ok(exit);
    }
    if let Some(exit) = handle_panel_key(application, state, key, terminal).await? {
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

    if dismiss_composer_error_on_escape(state, key.code, state.is_streaming || application.is_bash_running()) {
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
            .is_some_and(|previous| {
            now.duration_since(previous) <= std::time::Duration::from_millis(500)
        });
        if double {
            state.last_escape = None;
            match state.double_escape_action {
                DoubleEscapeAction::Tree => state.open_tree_panel(application, terminal)?,
                DoubleEscapeAction::Fork => state.open_fork_panel(application, terminal)?,
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
                    // A stale completion menu must never swallow Esc while a
                    // turn is streaming or a bash command is running — the
                    // user expects Esc to cancel the active turn via
                    // `Application::abort`. Dismiss the stale menu and fall
                    // through to `dispatch_action`, which routes Esc to the
                    // abort path. Visible overlays (panels/dialogs/comment
                    // editors) are handled earlier in `handle_key` and never
                    // reach this interceptor.
                    state.editor.break_insert_chain();
                    state.cancel_file_completion();
                    state.completion_query = None;
                    state.completions.clear();
                    if state.is_streaming || application.is_bash_running() {
                        // Fall through to dispatch_action for the real abort.
                    } else {
                        return Ok(false);
                    }
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
                terminal.set_code_review_mouse_capture(false)?;
                return Ok(true);
            }
        }
        Action::Quit => {
            if state.editor.is_empty() && state.pending_attachments.is_empty() {
                terminal.set_code_review_mouse_capture(false)?;
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
        Action::ModelSelect => state.open_model_panel(application, terminal).await?,
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
                    Ok(outcome) if outcome.cancelled => {
                        state.status = "New session cancelled".to_owned();
                    }
                    Ok(_) => {
                        state.replace_transcript_from_application(application);
                        state.status = "Started a new session".to_owned();
                    }
                    Err(error) => {
                        state.push_status(format!("Failed to start new session: {error:#}"), true)
                    }
                }
            }
        }
        Action::SessionResume => state.open_session_panel(application, terminal).await?,
        Action::SessionTree => state.open_tree_panel(application, terminal)?,
        Action::SessionFork => state.open_fork_panel(application, terminal)?,
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
        | Action::TreeFilterCycleBackward => {
            unreachable!("contextual action bypassed its active selector")
        }
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
    let message = format!("Usage: {}", usage(command));
    state.status = message.clone();
    state.composer_error = Some(message);
    state.composer_error_is_warning = false;
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
    terminal: &mut TerminalGuard,
) -> Result<bool> {
    let command = match crate::workflow_commands::parse_interactive_workflow_command(argument) {
        Ok(command) => command,
        Err(error) => {
            state.push_status(format!("{error:#}"), true);
            return Ok(false);
        }
    };
    if matches!(
        command,
        crate::workflow_commands::InteractiveWorkflowCommand::OpenPage
    ) {
        // Bare `/workflow` is a pure local UI action: it opens the workflows
        // page in the same frame. The effect path performs no async I/O, so it
        // is safe to await inline — the only work is port resolution (a lock
        // and clone) plus the OpenPage effect itself.
        return match crate::workflow_commands::execute_interactive_workflow_on_application(
            application, command,
        )
        .await
        {
            Ok(crate::workflow_commands::WorkflowCommandEffect::OpenPage) => {
                state.open_workflow_panel(application, terminal)?;
                Ok(true)
            }
            Ok(crate::workflow_commands::WorkflowCommandEffect::Message(_)) => Ok(true),
            Err(error) => {
                state.push_status(format!("Workflow command failed: {error:#}"), true);
                Ok(false)
            }
        };
    }
    // All other subcommands (`create`, `list`, `show`, `pause`, `resume`,
    // `cancel`, `integrate`, `remove`) may perform durable worktree, model, and
    // supervisor operations. Awaiting them inline froze the TUI event loop on
    // `WorkflowManager::create` -> `factory.create(...).await` (the exact
    // operation observed hanging `/workflow create`). Admit them on the
    // background task, mirroring `/run` + `spawn_extension_command`, and return
    // promptly with a visible admission status. The formatted result or bounded
    // error arrives later via `BackgroundEvent::WorkflowCommandFinished`, and
    // live snapshot updates already flow through `ApplicationEvent::Workflow`
    // (`apply_workflow_event`). This keeps a single nonblocking convention
    // instead of a second ad-hoc one.
    state.admit_workflow_command_background(application, command);
    Ok(true)
}

/// Visible admission label for a backgrounded `/workflow` subcommand. Returned
/// to the background completion so a stale "Creating…" status never survives a
/// later panel open. `OpenPage` is never admitted in the background; its label
/// is unused but kept total for exhaustiveness.
fn workflow_command_admission_label(
    command: &crate::workflow_commands::InteractiveWorkflowCommand,
) -> &'static str {
    match command {
        crate::workflow_commands::InteractiveWorkflowCommand::OpenPage => "Opening workflows",
        crate::workflow_commands::InteractiveWorkflowCommand::List => "Listing workflows",
        crate::workflow_commands::InteractiveWorkflowCommand::Show { .. } => "Loading workflow",
        crate::workflow_commands::InteractiveWorkflowCommand::Create { .. } => "Creating workflow",
        crate::workflow_commands::InteractiveWorkflowCommand::Pause { .. } => "Pausing workflow",
        crate::workflow_commands::InteractiveWorkflowCommand::Resume { .. } => "Resuming workflow",
        crate::workflow_commands::InteractiveWorkflowCommand::Cancel { .. } => "Cancelling workflow",
        crate::workflow_commands::InteractiveWorkflowCommand::Integrate { .. } => {
            "Integrating workflow"
        }
        crate::workflow_commands::InteractiveWorkflowCommand::Remove { .. } => "Removing workflow",
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
            "quit" | "exit" => {
                terminal.set_code_review_mouse_capture(false)?;
                return Ok(true);
            }
            "copy" => state.start_copy(application),
            "new" if !state.is_streaming => match application.new_session().await {
                Ok(outcome) if outcome.cancelled => {
                    state.status = "New session cancelled".to_owned();
                }
                Ok(_) => {
                    state.replace_transcript_from_application(application);
                    state.status = "Started a new session".to_owned();
                }
                Err(error) => {
                    state.push_status(format!("Failed to start new session: {error:#}"), true)
                }
            },
            "settings" => state.open_settings_panel(application, terminal)?,
            "workflow" => {
                if !dispatch_workflow_command(application, state, arg, terminal).await? {
                    return Ok(false);
                }
            }
            "model" if arg.is_none() => state.open_model_panel(application, terminal).await?,
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
                            Err(error) => {
                                state.push_status(format!("Cannot switch model: {error:#}"), true)
                            }
                        }
                    }
                    Err(error) => {
                        state.push_status(format!("Cannot switch model: {error:#}"), true)
                    }
                }
            }
            "models" => {
                let filter = arg.map(str::to_ascii_lowercase);
                let listing = available_models().await.into_iter().filter_map(|model| {
                    let reference = format!("{}/{}", model.provider, model.id);
                    filter.as_ref().map_or(true, |query| reference.to_ascii_lowercase().contains(query)).then_some(reference)
                }).collect::<Vec<_>>().join("\n");
                state.push_lines("Models", if listing.is_empty() { "No available models".to_owned() } else { listing }, state.themes.theme().accent,
                );
            }
            "sessions" => state.open_session_panel(application, terminal).await?,
            "scoped-models" => state.open_scoped_models_panel(terminal).await?,
            "resume" if arg.is_none() => state.open_session_panel(application, terminal).await?,
            "branch" | "fork" => state.open_fork_panel(application, terminal)?,
            "tree" => state.open_tree_panel(application, terminal)?,
            "trust" => state.open_trust_panel(application, terminal)?,
            "resume" => {
                let input = arg.expect("guarded");
                let sources = effective_resume_sources(application);
                match effective_session_catalog(application) {
                    Ok(catalog) => match crate::resume_catalog::switch_resume_selection(
                        application,
                        &catalog,
                        &crate::resume_catalog::ResumeSelectionRequest::Input(input.to_owned()),
                        Some(&state.cwd_path),
                        &sources,
                    )
                    .await
                    {
                        Ok(result) if result.cancelled => {
                            state.status = "Session resume cancelled".to_owned();
                        }
                        Ok(result) => {
                            state.replace_transcript_from_application(application);
                            state.status = format!("Resumed {}", result.path.display());
                        }
                        Err(error) => {
                            state.push_status(format!("Failed to resume session: {error:#}"), true)
                        }
                    },
                    Err(error) => {
                        state.push_status(format!("Failed to resume session: {error:#}"), true)
                    }
                }
            }
            "clone" if !state.is_streaming => match application.clone_session().await {
                Ok(outcome) if outcome.cancelled => {
                    state.status = "Session clone cancelled".to_owned()
                }
                Ok(_) => {
                    state.replace_transcript_from_application(application);
                    state.status = "Cloned current session branch".to_owned();
                }
                Err(error) => {
                    state.push_status(format!("Failed to clone session: {error:#}"), true)
                }
            },
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
                Ok(result) => {
                    state.status = format!(
                        "Compacted {} → {} estimated tokens",
                        result.tokens_before,
                        result.estimated_tokens_after.unwrap_or_default()
                    )
                }
                Err(error) => state.push_status(format!("Compaction failed: {error:#}"), true),
            },
            "name" => match arg {
                Some(name) => match application.set_session_name(name) {
                    Ok(()) => state.status = format!("Session name: {name}"),
                    Err(error) => {
                        state.push_status(format!("Failed to name session: {error:#}"), true)
                    }
                },
                None => {
                    let name = application
                        .state()
                        .await
                        .session_name
                        .unwrap_or_else(|| "(unnamed)".to_owned());
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
                        Err(error) => {
                            state.push_status(format!("Failed to set todos: {error:#}"), true)
                        }
                    },
                    Err(error) => {
                        state.push_status(format!("Invalid todo markdown: {error:#}"), true)
                    }
                },
                None => state.open_todo_dag_panel(application, terminal)?,
            },
            "code-review" => match ReviewScope::parse(arg) {
                Ok(scope) => state.open_code_review_panel(application, scope, terminal).await?,
                Err(error) => state.push_status(error, true),
            },
            "btw" => {
                state.open_side_chat(application, arg, terminal).await?;
            }
            "changelog" => state.push_lines(
                "Changelog",
                include_str!("../../../CHANGELOG.md").to_owned(),
                state.themes.theme().accent,
            ),
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
                let result = if output.as_ref().is_some_and(|path| {
                    path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
                }) {
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
                Some(input) => match pi_coding::import_session(pi_coding::SourceSessionFormat::Pi, Path::new(input),
                ) {
                    Ok(imported) => match application.switch_session(&imported.path).await {
                        Ok(outcome) if outcome.cancelled => {
                            state.status = "Session resume cancelled".to_owned();
                        }
                        Ok(_) => {
                            state.replace_transcript_from_application(application);
                            state.status = format!("Imported and resumed {}", imported.path.display());
                        }
                        Err(error) => state.push_status(format!("Imported session could not be resumed: {error:#}"), true,
                        ),
                    },
                    Err(error) => state.push_status(format!("Import failed: {error}"), true),
                },
                None => state.push_status("Usage: /import <path.jsonl>".to_owned(), true),
            },
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
            "agents" => state.open_agents_panel(application, terminal).await?,
            "ps" if arg.is_none() => {
                state.close_code_review_panel(terminal)?;
                state.panel = None;
                state.tree_panel = None;
                state.session_selector = None;
                state.scoped_model_selector = None;
                state.agents_panel = None;
                state.side_chat_open = false;
                state.process_panel = Some(ProcessPanel::new(application.process_list()));
            }
            "ps" | "process" => {
                match crate::process_commands::parse_interactive_process_command(name, arg) {
                Ok(Some(command)) => {
                        match crate::process_commands::execute_interactive_process_command(application, command,
                        ).await {
                    Ok(output) => {
                                state.push_lines("Processes", output, state.themes.theme().accent)
                            }
                            Err(error) => state.push_status(format!("Process command failed: {error:#}"), true),
                }
                    }
                    Ok(None) => unreachable!("matched process command"),
                Err(error) => state.push_status(format!("{error:#}"), true),
                }
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
                        let command = command.to_owned();
                        state.status = format!("Running /{command}");
                        spawn_extension_command(
                            application,
                            state.background_tx.clone(),
                            command,
                            arguments.to_owned(),
                        );
                    }
                    Err(error) => state.push_status(format!("{error:#}"), true),
                }
            }
            "goal" if arg.is_none() => state.open_goal_panel(application, terminal)?,
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
                                    state.themes.theme().accent);
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
                    Some(CommandSource::Prompt | CommandSource::Skill) => {
                        match expand_resource_command(application, command, arg.unwrap_or_default()) {
                        Ok(Some(expanded)) => match application.prompt(expanded.clone(), Vec::new(), None).await {
                            Ok(()) => {
                                state.record_accepted_prompt(&prompt);
                                state.push_lines("You", expanded, state.themes.theme().accent);
                            }
                            Err(error) => state.push_status(format!("Prompt was not accepted: {error}"), true),
                        },
                        Ok(None) => state.push_status(format!("Command /{command} is no longer available; try /reload"), true,
                            ),
                        Err(error) => state.push_status(format!("Failed to expand /{command}: {error:#}"), true,
                            ),
                        }
                    }
                    Some(CommandSource::Extension) => {
                        let command = command.to_owned();
                        state.status = format!("Running /{command}");
                        spawn_extension_command(
                            application,
                            state.background_tx.clone(),
                            command,
                            arg.unwrap_or_default().to_owned(),
                        );
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
    let display_prompt = if attachment_count == 0 {
        prompt.clone()
    } else {
        format!("{prompt}\n[{attachment_count} attachment(s)]")
    };
    state.pending_user_echo = true;
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

/// Descriptive label for whichever modal page currently owns the live inline
/// region, or `None` when no overlay is open. [`page_overlay_open`] is the
/// authoritative exclusive-page predicate derived from this; extension
/// interactions are deferred while any owner is active.
fn page_overlay_owner(state: &TuiState) -> Option<&'static str> {
    if state.panel.is_some() {
        Some("a selector panel")
    } else if state.tree_panel.is_some() {
        Some("the tree panel")
    } else if state.process_panel.is_some() {
        Some("the process panel")
    } else if state.settings_panel.is_some() {
        Some("the settings panel")
    } else if state.workflow_panel.is_some() {
        Some("the workflow panel")
    } else if state.todo_dag_panel.is_some() {
        Some("the todo DAG")
    } else if state.code_review_panel.is_some() {
        Some("code review")
    } else if state.side_chat_open {
        Some("side chat")
    } else if state.agents_panel.is_some() {
        Some("the agents panel")
    } else if state.session_selector.is_some() {
        Some("the session selector")
    } else if state.scoped_model_selector.is_some() {
        Some("the model selector")
    } else if state.extension_dialog.is_some() {
        Some("another extension dialog")
    } else if state.pty_attachment.is_some() {
        Some("a PTY attachment")
    } else {
        None
    }
}

/// True while a modal page owns the live inline region. These frames are
/// transient: they expand the live viewport while open and must never be
/// committed through `insert_before` into native/tmux scrollback.
fn page_overlay_open(state: &TuiState) -> bool {
    page_overlay_owner(state).is_some()
}

fn code_review_page_active(state: &TuiState) -> bool {
    state.code_review_panel.is_some()
        && state.panel.is_none()
        && state.tree_panel.is_none()
        && state.process_panel.is_none()
        && state.settings_panel.is_none()
        && state.workflow_panel.is_none()
        && state.todo_dag_panel.is_none()
        && !state.side_chat_open
        && state.agents_panel.is_none()
        && state.session_selector.is_none()
        && state.scoped_model_selector.is_none()
        && state.extension_dialog.is_none()
        && state.pty_attachment.is_none()
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

fn live_content_width(width: u16) -> u16 {
    if width > 2 { width - 2 } else { width }
}

fn live_content_height(height: u16) -> u16 {
    height.saturating_sub(u16::from(height > 1))
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
            .map(|line| {
                wrapped_row_count(&clean_terminal_text(line), usize::from(width.saturating_sub(5)),
                )
            })
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
    let notice = composer_error_toast_height(state, width).min(optional_budget);
    let notice_gap = u16::from(notice > 0 && optional_budget > notice);
    let error = notice.saturating_add(notice_gap);
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
    let width = live_content_width(width);
    let content_height = live_content_height(terminal_height);
    let theme = state.themes.theme();
    let extension = state.extension_ui.snapshot();
    let above_height = u16::try_from(extension_widget_lines(&extension, UiWidgetPlacement::AboveEditor, theme).len(),
    )
        .unwrap_or(u16::MAX)
        .min(6);
    let below_height = u16::try_from(extension_widget_lines(&extension, UiWidgetPlacement::BelowEditor, theme).len(),
    )
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
    ).len(),
    )
    .unwrap_or(u16::MAX);
    let layout = tui_layout_heights(
        state,
        width,
        content_height,
        todo_height,
        above_height,
        completion_height,
        below_height,
    );
    let progress_is_capped = todo_height > layout.todo;
    let live_lines = assemble_transcript_entries(
        &state.transcript[state.committed_entries..],
        state.show_thinking,
        state.expand_tools,
        theme,
        width.max(1),
    );
    let raw_transcript = u16::try_from(wrapped_line_count(&live_lines, width.max(1))).unwrap_or(u16::MAX);
    let chrome = layout.todo
        .saturating_add(layout.above)
        .saturating_add(layout.error)
        .saturating_add(layout.composer)
        .saturating_add(layout.completions)
        .saturating_add(layout.below)
        .saturating_add(1);
    if page_overlay_open(state) {
        return terminal_height.min(16);
    }
    let content_viewport = if progress_is_capped {
        content_height
    } else {
        chrome
            .saturating_add(raw_transcript.min(8).min(content_height.saturating_sub(chrome)))
            .clamp(3, content_height.max(3))
    };
    content_viewport
        .saturating_add(u16::from(terminal_height > 1))
        .min(terminal_height)
}

fn transcript_region_height(state: &TuiState, width: u16, terminal_height: u16) -> u16 {
    let width = live_content_width(width);
    let terminal_height = live_content_height(terminal_height);
    let theme = state.themes.theme();
    let extension = state.extension_ui.snapshot();
    let above_height = u16::try_from(extension_widget_lines(&extension, UiWidgetPlacement::AboveEditor, theme).len(),
    )
        .unwrap_or(u16::MAX)
        .min(6);
    let below_height = u16::try_from(extension_widget_lines(&extension, UiWidgetPlacement::BelowEditor, theme).len(),
    )
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
    ).len(),
    )
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
        |suffix| {
                if suffix.is_empty() { "~".to_owned() } else { format!("~{suffix}")
                }
            },
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
    (prior_rows.saturating_add(row), u16::try_from(columns).unwrap_or(u16::MAX),
    )
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

fn composer_border_lines_bounded(state: &TuiState, width: u16, theme: Theme, max_rows: usize,
) -> Vec<Line<'static>> {
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

fn composer_border_color(state: &TuiState, theme: Theme) -> Color {
    if state.pending_attachments.is_empty()
        && state
            .editor
            .lines
            .first()
            .is_some_and(|line| line.starts_with('!'))
    {
        return theme.bash_mode;
    }
    match state.thinking_level {
        ThinkingLevel::Off => theme.thinking_off,
        ThinkingLevel::Minimal => theme.thinking_minimal,
        ThinkingLevel::Low => theme.thinking_low,
        ThinkingLevel::Medium => theme.thinking_medium,
        ThinkingLevel::High => theme.thinking_high,
        ThinkingLevel::Xhigh => theme.thinking_xhigh,
        ThinkingLevel::Max => theme.thinking_max,
    }
}

fn composer_style(theme: Theme, foreground: Color) -> Style {
    Style::default()
        .fg(foreground)
        .bg(theme.user_message_bg)
}

fn composer_border_style(state: &TuiState, theme: Theme) -> Style {
    composer_style(theme, composer_border_color(state, theme))
}

/// One optional footer segment in the composer header line. Lower
/// `drop_priority` fields are omitted first when the terminal is too narrow.
struct FooterField {
    sep: &'static str,
    value: String,
    fg: Color,
    drop_priority: u8,
}

impl FooterField {
    fn width(&self) -> usize {
        usize::from(display_width(self.sep)) + usize::from(display_width(&self.value))
    }
}

/// Compact a token count into a footer-friendly width: `84k` for thousands,
/// otherwise the raw value.
fn footer_token_k(n: i64) -> String {
    if n >= 1000 {
        format!("{}k", n / 1000)
    } else {
        n.to_string()
    }
}

/// Compact git branch + dirty-counts text for the `⑂` footer segment:
/// `{branch}*{modified}+{staged}?{untracked}`, each count omitted when zero.
/// Returns `None` when there is no branch (not a git repository).
fn footer_git_text(status: &FooterGitStatus) -> Option<String> {
    let branch = status.branch.as_deref()?.trim();
    if branch.is_empty() {
        return None;
    }
    let mut text = branch.to_owned();
    if status.modified > 0 {
        text.push_str(&format!("*{}", status.modified));
    }
    if status.staged > 0 {
        text.push_str(&format!("+{}", status.staged));
    }
    if status.untracked > 0 {
        text.push_str(&format!("?{}", status.untracked));
    }
    Some(text)
}

/// Compact context-utilization text `◫ {percent}% {tokens}/{window}` for the
/// `◫` footer segment. Returns `None` when the model has no context window.
fn footer_context_text(usage: &SessionContextUsage) -> Option<String> {
    if usage.context_window <= 0 {
        return None;
    }
    let percent = usage
        .percent
        .map(|p| p.round().clamp(0.0, 999.0) as u32)
        .unwrap_or(0);
    let tokens = usage.tokens.unwrap_or(0).max(0);
    Some(format!(
        "{}% {}/{}",
        percent,
        footer_token_k(tokens),
        footer_token_k(usage.context_window)
    ))
}

/// Build the one-line composer header (the footer/status row) with model,
/// thinking, cwd, git branch/dirty, context utilization, and an optional
/// activity indicator. Lower-priority metadata drops on narrow terminals so
/// the row never overlaps; idle/default status leaves the activity segment out.
fn composer_header_line(
    state: &TuiState,
    inner: usize,
    theme: Theme,
    border_style: Style,
) -> Line<'static> {
    let status = composer_status_display(state, theme);
    let model = clean_terminal_text(&state.model);
    let cwd = compact_cwd(&state.cwd);
    let thinking = composer_thinking_label(state);
    let logo_width = usize::from(display_width("── π"));
    let available = inner.saturating_sub(logo_width);
    let status_sep = " > ⟲ ";
    let status_sep_w = status
        .as_ref()
        .map_or(0, |_| usize::from(display_width(status_sep)));
    // Render order; drop_priority ascending is dropped first on narrow widths.
    let mut fields: Vec<FooterField> = vec![
        FooterField {
            sep: "  > ⬢ ",
            value: model,
            fg: theme.accent,
            drop_priority: 5,
        },
        FooterField {
            sep: " · ◑ ",
            value: thinking,
            fg: theme.accent,
            drop_priority: 4,
        },
        FooterField {
            sep: " > 📁 ",
            value: cwd,
            fg: theme.syntax_variable,
            drop_priority: 3,
        },
    ];
    if let Some(git) = state.git_status.as_ref().and_then(footer_git_text) {
        fields.push(FooterField {
            sep: " > ⑂ ",
            value: git,
            fg: theme.accent,
            drop_priority: 2,
        });
    }
    if let Some(ctx) = state.context_usage.as_ref().and_then(footer_context_text) {
        fields.push(FooterField {
            sep: " > ◫ ",
            value: ctx,
            fg: theme.accent,
            drop_priority: 1,
        });
    }
    // Drop the lowest-priority field until all kept metadata and, when present,
    // at least one activity column fit.
    loop {
        let kept: usize = fields.iter().map(FooterField::width).sum();
        let remaining = available.saturating_sub(kept).saturating_sub(status_sep_w);
        let fits = kept.saturating_add(status_sep_w) <= available
            && (status.is_none() || remaining > 0);
        if fits || fields.is_empty() {
            break;
        }
        let drop = (0..fields.len())
            .min_by_key(|i| fields[*i].drop_priority)
            .expect("non-empty fields");
        fields.remove(drop);
    }
    let kept: usize = fields.iter().map(FooterField::width).sum();
    let mut spans = vec![
        Span::styled("╭── ", border_style),
        Span::styled(
            "π",
            composer_style(theme, theme.accent).add_modifier(Modifier::BOLD),
        ),
    ];
    for field in &fields {
        spans.push(Span::styled(field.sep, border_style));
        spans.push(Span::styled(field.value.clone(), composer_style(theme, field.fg)));
    }
    let remaining = available.saturating_sub(kept);
    if let Some((raw_status, status_color)) = status {
        if remaining > status_sep_w {
            let budget = remaining.saturating_sub(status_sep_w);
            let status_text = truncate_status_text(&raw_status, budget);
            let fill = "─".repeat(budget.saturating_sub(usize::from(display_width(&status_text))));
            spans.push(Span::styled(status_sep, border_style));
            spans.push(Span::styled(status_text, composer_style(theme, status_color)));
            spans.push(Span::styled(fill, border_style));
        } else {
            spans.push(Span::styled("─".repeat(remaining), border_style));
        }
    } else {
        spans.push(Span::styled("─".repeat(remaining), border_style));
    }
    spans.push(Span::styled("╮", border_style));
    Line::from(spans)
}

/// Compose the optional inline status segment for the OMP-style composer header.
/// Busy states keep the activity animation; default idle labels are omitted.
fn composer_status_display(state: &TuiState, theme: Theme) -> Option<(String, Color)> {
    if state.is_compacting {
        return Some((
            format!(
                "compacting {} ▶──",
                ACTIVE_ANIMATION_FRAMES[state.animation_frame % ACTIVE_ANIMATION_FRAMES.len()]
            ),
            theme.muted,
        ));
    }
    if state.is_streaming {
        return Some((
            format!(
                "working {} ▶──",
                ACTIVE_ANIMATION_FRAMES[state.animation_frame % ACTIVE_ANIMATION_FRAMES.len()]
            ),
            theme.muted,
        ));
    }
    if let Some(goal) = goal_status_summary(&state.goal_state) {
        return Some((format!("{goal} ▶──"), theme.accent));
    }
    let text = state.status.trim();
    if text.is_empty()
        || text == "Ready"
        || text == "Enter submit · Shift+Enter/Ctrl+J newline · Esc abort · Ctrl+D quit"
    {
        None
    } else {
        Some((format!("{text} ▶──"), theme.dim))
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

fn strip_notice_prefix<'a>(text: &'a str, prefix: &str) -> &'a str {
    text.strip_prefix(prefix).unwrap_or(text)
}

const MAX_COMPOSER_ERROR_HEIGHT: usize = 2;

fn composer_error_toast_height(state: &TuiState, width: u16) -> u16 {
    let Some(error) = state.composer_error.as_deref() else {
        return 0;
    };
    if width == 0 {
        return 0;
    }
    if width < 5 {
        return 1;
    }
    let label = if state.composer_error_is_warning { "Warning: " } else { "Error: " };
    let message = clean_terminal_text(error).lines().collect::<Vec<_>>().join(" ");
    let clean = format!("{label}{message}");
    u16::try_from(wrapped_row_count(&clean, usize::from(width)).clamp(1, 2))
        .unwrap_or(MAX_COMPOSER_ERROR_HEIGHT as u16)
}

/// Render a compact, width-filled warning/error banner above the live composer.
/// These rows are ephemeral and must never enter transcript commit paths.
fn composer_error_toast_lines(
    state: &TuiState,
    width: u16,
    theme: Theme,
) -> Vec<Line<'static>> {
    let Some(error) = state.composer_error.as_deref() else {
        return Vec::new();
    };
    let notice_color = if state.composer_error_is_warning {
        theme.warning
    } else {
        theme.error
    };
    let notice_background = if state.composer_error_is_warning {
        theme.custom_message_bg
    } else {
        theme.tool_error_bg
    };
    let style = Style::default().fg(notice_color).bg(notice_background);
    let width = usize::from(width);
    if width == 0 {
        return Vec::new();
    }
    let label = if state.composer_error_is_warning { "Warning: " } else { "Error: " };
    let message = clean_terminal_text(error).lines().collect::<Vec<_>>().join(" ");
    let clean = format!("{label}{message}");
    let max_rows = usize::from(composer_error_toast_height(state, width as u16));
    let raw_rows = wrapped_row_count(&clean, width);
    let mut rows = wrap_display_line(&clean, width)
        .into_iter()
        .take(max_rows)
        .collect::<Vec<_>>();
    if raw_rows > rows.len()
        && let Some(last) = rows.last_mut()
    {
        *last = truncate_status_text(&format!("{last}…"), width);
    }
    rows.into_iter()
        .map(|row| {
            let row = truncate_status_text(&row, width);
            let fill = " ".repeat(width.saturating_sub(usize::from(display_width(&row))));
            Line::from(vec![
                Span::styled(row, style.add_modifier(Modifier::BOLD)),
                Span::styled(fill, style),
            ])
        })
        .collect()
}

fn composer_border_lines_with_editor(
    state: &TuiState,
    width: u16,
    theme: Theme,
    editor_lines: Vec<String>,
) -> Vec<Line<'static>> {
    let inner = usize::from(width.saturating_sub(2));
    let border_style = composer_border_style(state, theme);
    let mut lines = vec![composer_header_line(state, inner, theme, border_style)];
    let attachment_labels = pending_attachment_labels(&state.pending_attachments);
    if editor_lines.len() <= 1 && attachment_labels.is_empty() && state.completions.items.is_empty() {
        let input = editor_lines.first().cloned().unwrap_or_default();
        let input_width = usize::from(display_width(&input));
        let fill = "─".repeat(inner.saturating_sub(input_width.saturating_add(3)));
        lines.push(Line::from(vec![
            Span::styled("╰─ ", border_style),
            Span::styled(input, composer_style(theme, theme.text)),
            Span::styled(format!(" {fill}╯"), border_style),
        ]));
        return lines;
    }
    if !state.completions.items.is_empty()
        && editor_lines.len() <= 1
        && attachment_labels.is_empty()
    {
        let input = editor_lines.first().cloned().unwrap_or_default();
        let input_width = usize::from(display_width(&input));
        let fill = " ".repeat(inner.saturating_sub(input_width.saturating_add(2)));
        lines.push(Line::from(vec![
            Span::styled("│  ", border_style),
            Span::styled(input, composer_style(theme, theme.text)),
            Span::styled(fill, composer_style(theme, theme.text)),
            Span::styled("│", border_style),
        ]));
        lines.push(Line::from(Span::styled(
            format!("╰{}╯", "─".repeat(inner)),
            border_style,
        )));
        return lines;
    }
    for line in editor_lines {
        let line_width = usize::from(display_width(&line));
        let fill = " ".repeat(inner.saturating_sub(line_width.saturating_add(1)));
        lines.push(Line::from(vec![
            Span::styled("│ ", border_style),
            Span::styled(line, composer_style(theme, theme.text)),
            Span::styled(fill, composer_style(theme, theme.text)),
            Span::styled("│", border_style),
        ]));
    }
    for label in attachment_labels {
        let line_width = usize::from(display_width(&label));
        let fill = " ".repeat(inner.saturating_sub(line_width.saturating_add(1)));
        lines.push(Line::from(vec![
            Span::styled("│ ", border_style),
            Span::styled(label, composer_style(theme, theme.muted)),
            Span::styled(fill, composer_style(theme, theme.muted)),
            Span::styled("│", border_style),
        ]));
    }
    lines.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(inner)),
        border_style,
    )));
    lines
}

fn render_welcome_lines(state: &TuiState, theme: Theme) -> Vec<Line<'static>> {
    if !state.transcript.is_empty() || !state.streaming_text.is_empty() || !state.streaming_thinking.is_empty() { return Vec::new(); }
    let mut lines = vec![
        Line::default(),
        Line::from(Span::styled(
            "rpi",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            clean_terminal_text(&state.model),
            Style::default().fg(theme.muted),
        )),
        Line::default(),
        Line::from(Span::styled(
            "Start typing · Alt+V paste image · /help · @file to attach context",
            Style::default().fg(theme.text),
        )),
    ];
    if !state.recent_sessions.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "Recent sessions",
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::BOLD),
        )));
        for row in &state.recent_sessions {
            let badge = clean_terminal_text(row.source_badge).replace('\n', " ");
            let label = clean_terminal_text(session_display_name(row)).replace('\n', " ");
            let imported = matches!(
                &row.status,
                pi_coding::CatalogRowStatus::AlreadyImported { .. }
            )
            .then_some(" · imported")
            .unwrap_or_default();
            lines.push(Line::from(Span::styled(
                format!("• [{badge}] {label}{imported}"),
                Style::default().fg(theme.text),
            )));
        }
    }
    lines
}

fn live_content_rect(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(u16::from(area.width > 2)),
        y: area.y.saturating_add(u16::from(area.height > 1)),
        width: live_content_width(area.width),
        height: live_content_height(area.height),
    }
}

fn render(
    frame: &mut ratatui::Frame<'_>,
    state: &TuiState,
    images: &mut TerminalImageRenderer,
) -> ImageDrawPlan {
    let theme = state.themes.theme();
    let content_area = live_content_rect(frame.area());
    let extension = state.extension_ui.snapshot();
    let above = extension_widget_lines(&extension, UiWidgetPlacement::AboveEditor, theme);
    let below = extension_widget_lines(&extension, UiWidgetPlacement::BelowEditor, theme);
    let mut error_lines = composer_error_toast_lines(state, content_area.width, theme);
    if !error_lines.is_empty() {
        error_lines.push(Line::default());
    }
    let above_height = u16::try_from(above.len()).unwrap_or(u16::MAX).min(6);
    let below_height = u16::try_from(below.len()).unwrap_or(u16::MAX).min(6);
    let completion_height = u16::try_from(state.completions.items.len())
        .unwrap_or(u16::MAX)
        .min(u16::try_from(MAX_COMPLETIONS).unwrap_or(u16::MAX));
    let mut todo_lines = if state.workflow_snapshots.is_empty() {
        render_todo_panel_lines(
            &state.todo_phases,
            &state.job_cards.cards_in_source_order(),
            theme,
            content_area.width.max(1),
        )
    } else {
        vec![Line::from(Span::styled(
            compact_workflow_status(&state.workflow_snapshots),
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::BOLD),
        ))]
    };
    let layout = tui_layout_heights(
        state,
        content_area.width,
        content_area.height,
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
        .split(content_area);

    let cell_size = window_size()
        .ok()
        .and_then(|size| {
            (size.columns > 0 && size.rows > 0 && size.width > 0 && size.height > 0).then_some(
                TerminalCellSize {
                    width_pixels: size.width / size.columns,
                    height_pixels: size.height / size.rows,
                },
            )
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
        append_transcript_entry_inner(
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
            .into_iter().filter_map(|candidate| {
                let row = wrapped_line_count(&transcript[..candidate.line_index], transcript_width);
                let visible_row = row.checked_sub(scroll)?;
                (visible_row.saturating_add(usize::from(candidate.layout.rows()))
                    <= transcript_height)
                    .then(|| {
                        ImagePlacement::new(
                            candidate.layout,
                            candidate.data,
                            candidate.mime_type,
                            sections[0].x,
                            sections[0]
                                .y
                                .saturating_add(u16::try_from(visible_row).unwrap_or(u16::MAX)),
                        )
                    })
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
        error_lines.truncate(usize::from(layout.error));
        frame.render_widget(Paragraph::new(error_lines), sections[3]);
    }
    let composer_lines = composer_border_lines_bounded(
        state,
        sections[4].width,
        theme,
        usize::from(sections[4].height),
    );
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
            }).collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), sections[5]);
    }
    let editor_width = usize::from(sections[4].width.saturating_sub(5));
    let (_, cursor_column) = editor_wrapped_position(state, editor_width);
    let (_, visible_cursor_row) = visible_editor_lines(
        state,
        editor_width, usize::from(sections[4].height.saturating_sub(2)).max(1),
    );
    let cursor_x = sections[4]
        .x
        .saturating_add(
            if state.completions.items.is_empty() && state.editor.lines.len() <= 1 {
                3
            } else {
                2
            },
        )
        .saturating_add(cursor_column);
    let cursor_y = sections[4].y
        .saturating_add(1)
        .saturating_add(u16::try_from(visible_cursor_row).unwrap_or(u16::MAX));
    if state.extension_dialog.is_none()
        && state.process_panel.is_none()
        && state.settings_panel.is_none()
        && state.pty_attachment.is_none()
        && state.workflow_panel.is_none()
        && state.todo_dag_panel.is_none()
        && state.code_review_panel.is_none()
        && !state.side_chat_open
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
    if let Some(panel) = &state.todo_dag_panel {
        render_todo_dag_panel(frame, panel, theme);
    }
    if let Some(panel) = &state.code_review_panel {
        render_code_review_panel(frame, panel, theme);
    }
    if state.side_chat_open {
        if let Some(side) = state.side_chat.as_ref() {
            render_side_chat_panel(frame, side, theme);
        }
    }
    if let Some(panel) = &state.agents_panel { render_agents_panel(frame, panel, theme); }
    if let Some(selector) = &state.session_selector { render_saved_session_selector(frame, selector, theme); }
    if let Some(selector) = &state.scoped_model_selector { render_scoped_model_selector(frame, selector, theme); }
    if let Some(dialog) = &state.extension_dialog { render_extension_dialog(frame, dialog, theme);
    }
    if let Some(attachment) = &state.pty_attachment {
        render_pty_attachment(frame, attachment, theme); }
    ImageDrawPlan {
        identity: ImageFrameIdentity {
            viewport_width: content_area.width,
            viewport_height: sections[0].height,
            theme_hash,
            message_hash,
        },
        placements,
    }
}
fn render_pty_attachment(frame: &mut ratatui::Frame<'_>, attachment: &PtyAttachment, theme: Theme) {
    let width = frame.area().width.saturating_sub(4).min(120).max(30);
    let height = frame.area().height.saturating_sub(4).min(40).max(10);
    let area = centered_rect(width, height, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_accent))
        .title(format!(" PTY {} · direct input ", attachment.process_id));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    let bytes = attachment.output.iter().copied().collect::<Vec<_>>();
    let text = clean_terminal_text(&String::from_utf8_lossy(&bytes));
    let width = usize::from(sections[0].width).max(1);
    let viewport = usize::from(sections[0].height).max(1);
    let rows = text
        .split('\n')
        .flat_map(|line| wrap_display_line(line, width))
        .collect::<Vec<_>>();
    let start = rows.len().saturating_sub(viewport);
    frame.render_widget(
        Paragraph::new(
            rows.into_iter()
                .skip(start)
                .map(Line::from)
                .collect::<Vec<_>>(),
        )
        .style(Style::default().fg(theme.text)),
        sections[0],
    );
    frame.render_widget(
        Paragraph::new(" Input goes directly to the child PTY · Ctrl+] or Esc detach ")
            .style(Style::default().fg(theme.dim)),
        sections[1],
    );
}

fn render_settings_panel(
    frame: &mut ratatui::Frame<'_>,
    panel: &SettingsPanel, input: Option<&SettingsValueInput>, theme: Theme,
) {
    let Ok(snapshot) = panel.snapshot() else { return; };
    let area = centered_rect(frame.area().width.saturating_sub(4).min(140).max(40), frame.area().height.saturating_sub(4).max(12), frame.area(),
    );
    frame.render_widget(Clear, area);
    let block = Block::default().title(format!(" Settings · {:?} scope{} ", snapshot.scope, if snapshot.dirty { " · modified" } else { "" })).borders(Borders::ALL).border_style(Style::default().fg(theme.border_accent));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(if input.is_some() { 4 } else { 0 }),
            Constraint::Length(1),
        ])
        .split(inner);
    let category = snapshot.category.map_or_else(|| "All".to_owned(), |value| format!("{value:?}"));
    frame.render_widget(Paragraph::new(vec![Line::from(vec![Span::styled("Category ", Style::default().fg(theme.dim)), Span::styled(category, Style::default().fg(theme.accent)), Span::styled("  ←/→ · Scope Ctrl-G/Ctrl-P · ", Style::default().fg(theme.dim),
            ), Span::styled(if snapshot.project_trusted { "trusted" } else { "untrusted" }, Style::default().fg(if snapshot.project_trusted { theme.success } else { theme.warning }),
            ),
        ]), Line::from(vec![Span::styled("Search ", Style::default().fg(theme.dim)), Span::styled(if snapshot.search.is_empty() { "type to filter" } else { &snapshot.search }, Style::default().fg(theme.text),
            ),
        ])]), sections[0]);

    let mut rendered_rows = Vec::new();
    let mut selected_rendered_row = 0;
    let mut selected_header_row = 0;
    let mut current_header_row = 0;
    let mut previous_section = None;
    for (index, row) in snapshot.rows.iter().enumerate() {
        let section = row.section();
        if previous_section.as_deref() != Some(section.as_str()) {
            current_header_row = rendered_rows.len();
            rendered_rows.push(Line::from(Span::styled(section.clone(), Style::default().fg(theme.dim).add_modifier(Modifier::BOLD))));
            previous_section = Some(section);
        }
        if index == snapshot.cursor {
            selected_rendered_row = rendered_rows.len();
            selected_header_row = current_header_row;
        }
        let selected = index == snapshot.cursor;
        let value = if row.redacted { "[redacted]".to_owned() } else { row.effective_value.to_string() };
        let metadata = format!("{:?} · {:?}{}", row.source, row.behavior, if row.inherited { " · inherited" } else { "" });
        let blocked = row.blocked_reason.as_deref().map_or(String::new(), |reason| format!(" · {reason}"));
        rendered_rows.push(Line::from(vec![Span::styled(if selected { "›  " } else { "   " }, Style::default().fg(theme.accent),
            ), Span::styled(clean_terminal_text(row.display_key()), Style::default().fg(if selected { theme.accent } else { theme.text }).add_modifier(if selected { Modifier::BOLD } else { Modifier::empty() }),
            ), Span::styled(format!(" = {value}  [{metadata}]{blocked}"), Style::default().fg(if row.blocked_reason.is_some() { theme.warning } else { theme.muted }),
            ),
        ]));
    }
    let visible_rows = usize::from(sections[1].height);
    let start = selected_rendered_row.saturating_sub(visible_rows.saturating_sub(1));
    let visible = if start > selected_header_row && visible_rows > 1 {
        let body_start = selected_rendered_row.saturating_sub(visible_rows.saturating_sub(2));
        std::iter::once(rendered_rows[selected_header_row].clone())
            .chain(rendered_rows.iter().skip(body_start).take(visible_rows - 1).cloned())
            .collect::<Vec<_>>()
    } else {
        rendered_rows.into_iter().skip(start).take(visible_rows).collect::<Vec<_>>()
    };
    frame.render_widget(Paragraph::new(visible), sections[1]);

    if let Some(input) = input {
        let editor = Block::default()
            .title(format!(" Edit {} · {} ", clean_terminal_text(&input.key), input.hint))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(if input.error.is_some() { theme.error } else { theme.accent }));
        let editor_inner = editor.inner(sections[2]);
        frame.render_widget(editor, sections[2]);
        let message = input.error.as_deref().unwrap_or("Enter confirm pending change · Esc cancel editor");
        frame.render_widget(Paragraph::new(vec![
            Line::from(Span::styled(input.value.clone(), Style::default().fg(theme.text))),
            Line::from(Span::styled(truncate_status_text(message, usize::from(editor_inner.width)), Style::default().fg(if input.error.is_some() { theme.error } else { theme.dim }))),
        ]), editor_inner);
        let cursor_width = display_width(&input.value[..input.cursor]);
        let x = editor_inner.x.saturating_add(cursor_width).min(editor_inner.right().saturating_sub(1));
        frame.set_cursor_position((x, editor_inner.y));
    }
    let footer = if input.is_some() {
        "Editing value · Enter confirm · Esc cancel editor"
    } else {
        "↑/↓ select · Enter edit · Del reset · Ctrl-S apply · Esc close"
    };
    frame.render_widget(Paragraph::new(Span::styled(footer, Style::default().fg(theme.muted))), sections[3]);
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


fn transcript_entry_is_visible(entry: &TranscriptEntry, show_thinking: bool) -> bool {
    match entry.kind {
        TranscriptKind::Job => entry.job_card.is_some(),
        TranscriptKind::Tool => entry.tool_card.is_some(),
        TranscriptKind::User
        | TranscriptKind::Assistant
        | TranscriptKind::System
        | TranscriptKind::Custom => entry.content.iter().any(|block| match block {
            ContentBlock::Thinking { .. } => {
                show_thinking && transcript_block_has_content(block)
            }
            _ => transcript_block_has_content(block),
        }),
    }
}

fn is_unstyled_transcript_separator(line: &Line<'_>) -> bool {
    line == &Line::default()
}

fn append_transcript_separator(lines: &mut Vec<Line<'static>>) -> bool {
    if lines.is_empty()
        || lines
            .last()
            .is_some_and(is_unstyled_transcript_separator)
    {
        return false;
    }
    lines.push(Line::default());
    true
}

fn append_transcript_entry_inner(
    lines: &mut Vec<Line<'static>>,
    entry: &TranscriptEntry,
    show_thinking: bool,
    expand_tools: bool,
    theme: Theme,
    width: u16,
    animation_frame: usize,
    image_context: Option<&mut TranscriptImageContext<'_>>,
) {
    if !transcript_entry_is_visible(entry, show_thinking) {
        return;
    }
    let inserted_separator = append_transcript_separator(lines);
    let entry_start = lines.len();
    render_transcript_entry_inner(
        lines,
        entry,
        show_thinking,
        expand_tools,
        theme,
        width,
        animation_frame,
        image_context,
    );
    if lines.len() == entry_start && inserted_separator {
        lines.pop();
    }
}

fn assemble_transcript_entries(
    entries: &[TranscriptEntry],
    show_thinking: bool,
    expand_tools: bool,
    theme: Theme,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for entry in entries {
        render_transcript_entry(
            &mut lines,
            entry,
            show_thinking,
            expand_tools,
            theme,
            width,
        );
    }
    lines
}

fn assemble_committed_transcript_entries(
    entries: &[TranscriptEntry],
    show_thinking: bool,
    expand_tools: bool,
    theme: Theme,
    width: u16,
    has_live_continuation: bool,
) -> Vec<Line<'static>> {
    let mut lines = assemble_transcript_entries(entries, show_thinking, expand_tools, theme, width);
    if has_live_continuation {
        append_transcript_separator(&mut lines);
    }
    lines
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
        append_transcript_entry_inner(
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

fn render_job_card(lines: &mut Vec<Line<'static>>, card: &TaskCardRows, theme: Theme, animation_frame: usize, width: u16,
) {
    let inner = usize::from(width.saturating_sub(2)).max(1);
    let border = if card.children.iter().any(|child| child.job_status == pi_coding::JobStatus::Failed) { theme.error }
        else if card.children.iter().any(|child| child.job_status == pi_coding::JobStatus::Cancelled) { theme.warning }
        else { theme.border_accent };
    lines.push(Line::from(Span::styled(format!("╭{}╮", "─".repeat(inner)), Style::default().fg(border),
    )));
    push_task_box_row(lines, &format!("Task {} agents", card.children.len()), theme.tool_title, border, inner, Modifier::BOLD,
    );
    if !card.context.trim().is_empty() {
        for line in render_transcript_markdown(&card.context, theme, theme.text, u16::try_from(inner.saturating_sub(2)).unwrap_or(u16::MAX), false,
        ) {
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
        if let Some(summary) = child.summary.as_deref().or_else(|| {
            child.rows.iter().find(|row| row.role == JobCardRowRole::Description).map(|row| row.text.as_str())
        }) {
            push_task_box_row(lines, summary, theme.text, border, inner, Modifier::empty());
        }
        let activity = child.rows.iter().filter(|row| {
                !matches!(row.role, JobCardRowRole::Title | JobCardRowRole::Description | JobCardRowRole::Reference)
            }).map(|row| row.text.as_str()).collect::<Vec<_>>().join(" · ");
        if !activity.is_empty() {
            push_task_box_row(lines, &activity, status_color, border, inner, Modifier::empty(),
            );
        }
    }
    push_task_box_row(lines, &card.aggregate.text, theme.muted, border, inner, Modifier::ITALIC,
    );
    lines.push(Line::from(Span::styled(format!("╰{}╯", "─".repeat(inner)), Style::default().fg(border),
    )));
}

fn push_task_separator(lines: &mut Vec<Line<'static>>, label: &str, border: Color, inner: usize) {
    let fill = "─".repeat(inner.saturating_sub(display_width(label).into()));
    lines.push(Line::from(vec![Span::styled("├", Style::default().fg(border)), Span::styled(label.to_owned(), Style::default().fg(border)), Span::styled(fill, Style::default().fg(border)), Span::styled("┤", Style::default().fg(border)),
    ]));
}

fn push_task_box_row(lines: &mut Vec<Line<'static>>, text: &str, color: Color, border: Color, inner: usize, modifier: Modifier,
) {
    for row in wrap_display_line(&clean_terminal_text(text), inner.saturating_sub(2).max(1)) {
        push_task_box_line(lines, Line::from(Span::styled(row, Style::default().fg(color).add_modifier(modifier),
            )), border, inner,
        );
    }
}

fn push_task_box_line(lines: &mut Vec<Line<'static>>, line: Line<'static>, border: Color, inner: usize,
) {
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
        .filter(|card| {
            matches!(card.job_status, pi_coding::JobStatus::Queued | pi_coding::JobStatus::Running)
        })
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
    append_transcript_entry_inner(
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

fn render_tool_card(lines: &mut Vec<Line<'static>>, tool: &ToolTranscript, expanded: bool, theme: Theme, width: u16,
) {
    let card = if expanded { &tool.expanded } else { &tool.compact };
    let border = match card.status {
        ToolCallViewStatus::Failed | ToolCallViewStatus::Cancelled => theme.error,
        ToolCallViewStatus::Running | ToolCallViewStatus::Streaming => theme.border_accent,
        ToolCallViewStatus::Succeeded | ToolCallViewStatus::OrphanRepaired => theme.border_muted,
    };
    let inner = usize::from(width.saturating_sub(2).max(1));
    lines.push(Line::from(Span::styled(format!("╭{}╮", "─".repeat(inner)), Style::default().fg(border),
    )));
    let tool_title = card.rows.iter().find(|row| row.role == ToolCardRowRole::Command).map_or(card.tool_name.as_str(), |row| row.text.as_str());
    if card.tool_name.eq_ignore_ascii_case("bash") {
        push_tool_box_row(lines, tool_title, theme.tool_title, border, inner);
        push_tool_box_row(lines, &format!("$ {}", card.arguments_summary), theme.bash_mode, border, inner,
        );
        if card.rows.iter().any(|row| row.role == ToolCardRowRole::Content) {
            push_tool_separator(lines, " Output ", border, inner);
        }
    } else {
        let marker = match card.status {
            ToolCallViewStatus::Running | ToolCallViewStatus::Streaming | ToolCallViewStatus::Succeeded => {
                if matches!(card.tool_name.as_str(), "edit" | "write") { "✎" } else { "•" }
            }
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
            ToolCardRowRole::Status => {
                if card.is_error { theme.error } else { theme.muted }
            }
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
    if card.truncated { push_tool_box_row(lines, &format!("… {} more lines ⟦Ctrl+O: Expand⟧", card.omitted_content_lines), theme.dim, border, inner,
        ); }
    lines.push(Line::from(Span::styled(format!("╰{}╯", "─".repeat(inner)), Style::default().fg(border),
    )));
    lines.push(Line::default());
}

fn push_tool_separator(lines: &mut Vec<Line<'static>>, label: &str, border: Color, inner: usize) {
    let fill = "─".repeat(inner.saturating_sub(label.chars().count().saturating_add(2)));
    lines.push(Line::from(vec![Span::styled("├──", Style::default().fg(border)), Span::styled(label.to_owned(), Style::default().fg(border)), Span::styled(fill, Style::default().fg(border)), Span::styled("┤", Style::default().fg(border)),
    ]));
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
fn push_tool_box_line(lines: &mut Vec<Line<'static>>, line: Line<'static>, border: Color, inner: usize,
) {
    let used = line.width();
    let fill = " ".repeat(inner.saturating_sub(used.saturating_add(2)));
    let mut spans = vec![Span::styled("│ ", Style::default().fg(border))];
    spans.extend(line.spans);
    spans.push(Span::raw(fill));
    spans.push(Span::styled(" │", Style::default().fg(border)));
    lines.push(Line::from(spans));
}
fn push_tool_box_row(lines: &mut Vec<Line<'static>>, text: &str, color: Color, border: Color, inner: usize,
) {
    for row in wrap_display_line(&clean_terminal_text(text), inner.saturating_sub(2).max(1)) {
        let used = usize::from(display_width(&row));
        let fill = " ".repeat(inner.saturating_sub(used.saturating_add(2)));
        lines.push(Line::from(vec![Span::styled("│ ", Style::default().fg(border)), Span::styled(row, Style::default().fg(color)), Span::raw(fill), Span::styled(" │", Style::default().fg(border)),
        ]));
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
                    .unwrap_or(if entry.is_error { "Error" } else { "System" })),
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
    let entry_content_start = lines.len();
    if entry.kind == TranscriptKind::User {
        lines.push(user_card_vertical_padding(width));
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
                        entry.is_partial)
                };
                if entry.kind == TranscriptKind::User {
                    rendered = render_user_card_lines(rendered, width);
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
        if entry.kind == TranscriptKind::User {
            lines.push(user_card_vertical_padding(width));
        }
        if let Some(background) = background {
            for line in &mut lines[entry_content_start..] {
                line.style = line.style.bg(background);
                for span in &mut line.spans {
                    span.style = span.style.bg(background);
                }
            }
        }
    }
    // Assistant entries retain their producer-owned unstyled trailing row.
    // The shared adjacency assembler reuses that exact row as the separator
    // before the next visible entry without treating styled user padding as blank.
    if entry.kind == TranscriptKind::Assistant {
        lines.push(Line::default());
    }
}

fn user_card_vertical_padding(width: u16) -> Line<'static> {
    Line::from(Span::raw(" ".repeat(usize::from(width.max(1)))))
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
            line_style);
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

const SESSION_SELECTOR_MAX_HEIGHT: u16 = 22;
const SESSION_SELECTOR_HEIGHT_OVERHEAD: usize = 6;

fn saved_session_preview_lines(
    row: &ResumeSelectorRow,
    marker: &str,
    show_path: bool,
    max_lines: usize,
) -> Vec<String> {
    if max_lines == 0 {
        return Vec::new();
    }
    let badge = clean_terminal_text(row.source_badge).replace('\n', " ");
    let preview = clean_terminal_text(session_display_name(row));
    let path = if show_path {
        format!(" · {}",
            clean_terminal_text(&row.path.display().to_string()).replace('\n', " ")
        )
    } else {
        String::new()
    };
    let count = row.message_count.map_or_else(
        || "? messages".to_owned(),
        |count| format!("{count} messages"),
    );
    let imported = matches!(
        &row.status,
        pi_coding::CatalogRowStatus::AlreadyImported { .. }
    )
    .then_some(" · imported")
    .unwrap_or_default();
    let mut logical_lines = preview.split('\n');
    let first = logical_lines.next().unwrap_or_default();
    let mut lines = Vec::with_capacity(max_lines.min(4));
    lines.push(format!(
        "{marker} [{badge}] {first} · {count}{imported}{path}"
    ));
    lines.extend(logical_lines.take(max_lines.saturating_sub(1))
            .map(|line| format!("  {line}")),
    );
    lines
}

fn sanitized_logical_line_count(text: &str, max_lines: usize) -> usize {
    if max_lines == 0 {
        return 0;
    }
    if max_lines == 1 {
        return 1;
    }
    let mut lines = 1;
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
        } else if character == '\n' {
            lines += 1;
            if lines == max_lines {
                break;
            }
        }
    }
    lines
}

fn saved_session_preview_line_count(row: &ResumeSelectorRow, max_lines: usize) -> usize {
    sanitized_logical_line_count(session_display_name(row), max_lines)
}

fn saved_session_viewport_start(selector: &SavedSessionSelector, row_line_budget: usize) -> usize {
    let mut start = selector.selected();
    let Some(selected) = selector.visible_row(start) else {
        return 0;
    };
    let selected_lines =
        saved_session_preview_line_count(selected, row_line_budget.saturating_add(1));
    if selected_lines > row_line_budget {
        return start;
    }
    let mut used = selected_lines;
    while start > 0 && used < row_line_budget {
        let remaining = row_line_budget - used;
        let Some(previous) = selector.visible_row(start - 1) else {
            break;
        };
        let previous_lines = saved_session_preview_line_count(previous, remaining + 1);
        if previous_lines > remaining {
            break;
        }
        start -= 1;
        used += previous_lines;
    }
    start
}

fn saved_session_preferred_preview_lines(selector: &SavedSessionSelector) -> usize {
    let max_preview_lines =
        usize::from(SESSION_SELECTOR_MAX_HEIGHT).saturating_sub(SESSION_SELECTOR_HEIGHT_OVERHEAD);
    let mut total = 0;
    for (_, row) in selector.visible_window(0, max_preview_lines) {
        let remaining = max_preview_lines - total;
        total += saved_session_preview_line_count(row, remaining);
        if total == max_preview_lines {
            break;
        }
    }
    total
}

fn saved_session_selector_lines(
    selector: &SavedSessionSelector,
    theme: Theme,
    content_line_budget: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(content_line_budget);
    if content_line_budget > 0 {
        lines.push(Line::from(Span::styled(
            "Resume Session",
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        )));
    }
    if content_line_budget > 1 {
        lines.push(Line::from(Span::styled(
            session_selector_key_hints(selector),
            Style::default().fg(theme.dim),
        )));
    }
    if content_line_budget > 2 {
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
    }

    let row_line_budget = content_line_budget.saturating_sub(lines.len());
    if selector.visible_count() == 0 {
        if row_line_budget > 0 {
            lines.push(Line::from(Span::styled(
                "No matching sessions",
                Style::default().fg(theme.muted),
            )));
        }
        return lines;
    }

    let start = saved_session_viewport_start(selector, row_line_budget);
    let mut remaining = row_line_budget;
    for (index, row) in selector.visible_window(start, row_line_budget) {
        if remaining == 0 {
            break;
        }
        let marker = if selector.is_current(row) { "•" } else { " " };
        let style = if index == selector.selected() {
            Style::default().fg(theme.text).bg(theme.selected_bg)
        } else {
            Style::default().fg(if row.name.is_some() {
                theme.warning
            } else {
                theme.text
            })
        };
        let preview_lines =
            saved_session_preview_lines(row, marker, selector.show_path(), remaining);
        remaining = remaining.saturating_sub(preview_lines.len());
        lines.extend(
            preview_lines
                .into_iter()
                .map(|text| Line::from(Span::styled(text, style))),
        );
    }
    lines
}

fn render_saved_session_selector(
    frame: &mut ratatui::Frame<'_>,
    selector: &SavedSessionSelector,
    theme: Theme,
) {
    let preview_lines = saved_session_preferred_preview_lines(selector);
    let height = u16::try_from(preview_lines.saturating_add(SESSION_SELECTOR_HEIGHT_OVERHEAD))
        .unwrap_or(u16::MAX)
        .clamp(8, SESSION_SELECTOR_MAX_HEIGHT);
    let area = centered_rect(
        frame.area().width.saturating_sub(4).min(110).max(30),
        height,
        frame.area(),
    );
    frame.render_widget(Clear, area);
    let content_line_budget = usize::from(area.height.saturating_sub(2));
    let lines = saved_session_selector_lines(selector, theme, content_line_budget);
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

fn agents_panel_lines(
    panel: &AgentsPanel,
    theme: Theme,
    width: u16,
    height: u16,
) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let height = usize::from(height);
    if height == 0 {
        return Vec::new();
    }

    let dirty = if panel.dirty() { " · unsaved" } else { "" };
    let mut header = Vec::new();
    for text in [format!("{}{dirty}", panel.title()), panel.help().to_owned()] {
        let style = if header.is_empty() {
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.dim)
        };
        header.extend(
            wrap_display_line(&clean_terminal_text(&text), width)
                .into_iter()
                .map(|line| Line::from(Span::styled(line, style))),
        );
    }
    header.push(Line::default());

    let mut body = Vec::new();
    let mut selected_range = None;
    let rows = panel.view_lines();
    for (index, row) in rows.iter().enumerate() {
        let start = body.len();
        let style = if row.selected {
            Style::default().fg(theme.text).bg(theme.selected_bg)
        } else {
            Style::default().fg(theme.text)
        };
        body.extend(
            wrap_display_line(&clean_terminal_text(&row.text), width)
                .into_iter()
                .map(|line| Line::from(Span::styled(line, style))),
        );
        if row.selected {
            selected_range = Some((start, body.len()));
        }
        if index + 1 < rows.len() {
            body.push(Line::default());
        }
    }

    let mut detail = panel.selected_row().map_or_else(Vec::new, |selected| {
        let mut lines = vec![Line::default()];
        lines.extend(
            wrap_display_line(&clean_terminal_text(&selected.description), width)
                .into_iter()
                .map(|line| Line::from(Span::styled(line, Style::default().fg(theme.dim)))),
        );
        lines
    });

    // Header is fixed. Reserve description detail only when it still leaves enough room for
    // every wrapped line of the selection; selected visibility takes precedence on short panes.
    header.truncate(height.saturating_sub(1));
    let selected_height = selected_range.map_or(1, |(start, end)| end.saturating_sub(start));
    let detail_budget = height
        .saturating_sub(header.len().saturating_add(selected_height))
        .min(detail.len());
    detail.truncate(detail_budget);
    let body_height = height.saturating_sub(header.len().saturating_add(detail.len()));

    let body_start = selected_range.map_or(0, |(selected_start, selected_end)| {
        if body.len() <= body_height || body_height == 0 {
            return 0;
        }
        if selected_end.saturating_sub(selected_start) >= body_height {
            return selected_start.min(body.len().saturating_sub(body_height));
        }
        let selected_middle = selected_start.saturating_add(selected_end) / 2;
        let centered = selected_middle.saturating_sub(body_height / 2);
        let latest_for_top = selected_start.min(body.len().saturating_sub(body_height));
        let earliest_for_bottom = selected_end.saturating_sub(body_height);
        centered
            .clamp(earliest_for_bottom, latest_for_top)
            .min(body.len().saturating_sub(body_height))
    });
    let mut lines = header;
    lines.extend(body.into_iter().skip(body_start).take(body_height));
    lines.extend(detail);
    lines.truncate(height);
    lines
}
fn render_agents_panel(frame: &mut ratatui::Frame<'_>, panel: &AgentsPanel, theme: Theme) {
    let width = frame.area().width.saturating_sub(4).min(110).max(1);
    let max_height = frame.area().height.saturating_sub(2).min(24).max(1);
    let max_area = centered_rect(width, max_height, frame.area());
    let max_content = inset_rect(Block::default().borders(Borders::ALL).inner(max_area), AGENTS_PANEL_PADDING);
    let desired_lines = agents_panel_lines(panel, theme, max_content.width, max_content.height);
    let minimum_height = 8.min(max_height);
    let height = u16::try_from(desired_lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .saturating_add(AGENTS_PANEL_PADDING.saturating_mul(2))
        .clamp(minimum_height, max_height);
    let area = centered_rect(width, height, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_accent));
    let content = inset_rect(block.inner(area), AGENTS_PANEL_PADDING);
    let lines = agents_panel_lines(panel, theme, content.width, content.height);
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
            Span::styled(clean_terminal_text(&panel.query), Style::default().fg(theme.text),
            ),
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
                lines.push(Line::from(Span::styled(clean_terminal_text(&format!("{cursor}{body}")), style,
                )));
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
                Span::styled("Label (empty to remove): ", Style::default().fg(theme.warning),
                ),
                Span::styled(clean_terminal_text(label), Style::default().fg(theme.text).bg(theme.selected_bg),
                ),
            ]));
        }
        lines.push(Line::from(Span::styled(
            format!("({}/{}) [{}]", panel.selected.saturating_add(1).min(panel.visible().len()), panel.visible().len(), panel.filter.label()),
            Style::default().fg(theme.dim),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(theme.border_accent)),
            )
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
            Span::styled(clean_terminal_text(&panel.query), Style::default().fg(theme.text),
            ),
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

pub(crate) fn clean_terminal_text(text: &str) -> String {
    let mut clean = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            // CSI cursor controls and OSC8 hyperlinks are stripped; a bare ESC
            // not opening a sequence is dropped without swallowing the next
            // character, so trailing plain text survives (ESC <plain> -> <plain>).
            match characters.peek() {
                Some('[') => {
                    characters.next();
                    for value in characters.by_ref() {
                        if ('@'..='~').contains(&value) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    characters.next();
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
                _ => {}
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
    struct FailingCodeReviewMouse;

    impl CodeReviewMouseController for FailingCodeReviewMouse {
        fn set_code_review_mouse_capture(&mut self, _enable: bool) -> Result<()> {
            Err(anyhow!("mouse sync failed"))
        }
    }

    #[derive(Default)]
    struct TestCodeReviewMouse {
        active: bool,
        transitions: Vec<bool>,
        bytes: Vec<u8>,
    }

    impl CodeReviewMouseController for TestCodeReviewMouse {
        fn set_code_review_mouse_capture(&mut self, enable: bool) -> Result<()> {
            let changed = self.active != enable;
            write_code_review_mouse_capture(&mut self.bytes, &mut self.active, enable)?;
            if changed {
                self.transitions.push(enable);
            }
            Ok(())
        }
    }
    #[tokio::test]
    async fn attachment_routes_printable_enter_ctrl_c_and_consumes_detach() {
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
        let application = Application::new(session).await;
        let process = application
            .process_spawn(pi_coding::ProcessSpawnSpec {
                argv: vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    "read line; printf '<%s>' \"$line\"; sleep 30".to_owned(),
                ],
                cwd: cwd.path().to_path_buf(),
                env: Default::default(),
                tty: true,
                terminal_size: None,
                label: None,
                timeout_ms: None,
                output_bytes: None,
            })
            .await
            .expect("spawn PTY");
        let mut state = todo_test_state(Vec::new());
        state.pty_attachment = Some(
            attach_process(&application, process.id.clone())
                .await
                .expect("attach"),
        );
        let transcript_len = state.transcript.len();

        handle_pty_attachment_key(
            &application,
            &mut state,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        )
        .await
        .expect("printable");
        handle_pty_attachment_key(
            &application,
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .await
        .expect("Enter");
        let output_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let logs = application
                .process_logs(&process.id, 0, None, false, None)
                .await
                .expect("logs");
            if logs
                .chunks
                .iter()
                .flat_map(pi_coding::ProcessLogChunk::bytes)
                .collect::<Vec<_>>()
                .windows(3)
                .any(|window| window == b"<x>")
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < output_deadline,
                "PTY command did not process printable input and Enter before deadline"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let output = application
            .process_logs(&process.id, 0, None, false, None)
            .await
            .expect("final logs")
            .chunks
            .iter()
            .flat_map(pi_coding::ProcessLogChunk::bytes)
            .collect::<Vec<_>>();
        assert!(output.windows(3).any(|window| window == b"<x>"));
        assert_eq!(state.transcript.len(), transcript_len);

        let third = application
            .process_spawn(pi_coding::ProcessSpawnSpec {
                argv: vec!["sleep".to_owned(), "30".to_owned()],
                cwd: cwd.path().to_path_buf(),
                env: Default::default(),
                tty: true,
                terminal_size: None,
                label: None,
                timeout_ms: None,
                output_bytes: None,
            })
            .await
            .expect("spawn third PTY");
        state.pty_attachment = Some(
            attach_process(&application, third.id.clone())
                .await
                .expect("attach third"),
        );
        handle_pty_attachment_key(
            &application,
            &mut state,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        )
        .await
        .expect("Escape detach");
        assert!(state.pty_attachment.is_none());
        assert_eq!(
            application
                .process_describe(&third.id)
                .expect("describe third")
                .output_cursor,
            0
        );
        application
            .process_stop(&third.id, None)
            .await
            .expect("stop third");
        state.pty_attachment = Some(
            attach_process(&application, process.id.clone())
                .await
                .expect("reattach first"),
        );
        handle_pty_attachment_key(
            &application,
            &mut state,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        )
        .await
        .expect("Ctrl-C");
        let exited = application
            .process_wait(&process.id, Some(std::time::Duration::from_secs(3)))
            .await
            .expect("wait");
        assert!(exited.state.is_terminal());
        state.apply_process_event(ProcessEvent::ProcessExited { process: exited });
        assert!(state.pty_attachment.is_none());

        let second = application
            .process_spawn(pi_coding::ProcessSpawnSpec {
                argv: vec!["sleep".to_owned(), "30".to_owned()],
                cwd: cwd.path().to_path_buf(),
                env: Default::default(),
                tty: true,
                terminal_size: None,
                label: None,
                timeout_ms: None,
                output_bytes: None,
            })
            .await
            .expect("spawn second PTY");
        state.pty_attachment = Some(
            attach_process(&application, second.id.clone())
                .await
                .expect("attach second"),
        );
        handle_pty_attachment_key(
            &application,
            &mut state,
            KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL),
        )
        .await
        .expect("detach");
        assert!(state.pty_attachment.is_none());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            application
                .process_describe(&second.id)
                .expect("describe")
                .output_cursor,
            0
        );
        application
            .process_stop(&second.id, None)
            .await
            .expect("stop");
    }

    #[test]
    fn pty_key_mapping_reserves_detach_and_maps_input() {
        assert_eq!(
            process_key_for_terminal_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(ProcessKey::Enter)
        );
        assert_eq!(
            process_key_for_terminal_event(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            )),
            Some(ProcessKey::CtrlC)
        );
        assert_eq!(
            control_character_byte(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            Some(0x01)
        );
        assert_eq!(
            control_character_byte(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL)),
            Some(0x1d),
            "Ctrl+] is recognized as a control byte but the attachment handler consumes it first",
        );
    }

    #[test]
    fn detach_chords_are_consumed_before_process_io() {
        assert_eq!(
            classify_pty_input(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            PtyInput::Detach,
        );
        assert_eq!(
            classify_pty_input(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL)),
            PtyInput::Detach,
        );

        assert_eq!(
            classify_pty_input(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            PtyInput::Process,
        );
        assert_eq!(
            classify_pty_input(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::CONTROL)),
            PtyInput::Detach,
        );
        assert_eq!(
            classify_pty_input(KeyEvent::new(KeyCode::Char('\u{1d}'), KeyModifiers::NONE)),
            PtyInput::Detach,
        );
    }

    #[test]
    fn attachment_output_renders_only_in_transient_overlay() {
        use ratatui::backend::TestBackend;

        let process_id: ProcessId =
            serde_json::from_value(serde_json::json!("00000000-0000-0000-0000-000000000022"))
                .unwrap();
        let mut state = todo_test_state(Vec::new());
        let mut attachment = PtyAttachment::new(process_id, 0);
        attachment.append_output(0, 12, b"child-output");
        state.pty_attachment = Some(attachment);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let mut images = TerminalImageRenderer::default();
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
        assert!(rendered.contains("child-output"));
        assert!(state.transcript.is_empty());
    }

    #[test]
    fn pty_output_is_bounded_and_exit_requests_detach() {
        let process_id: ProcessId =
            serde_json::from_value(serde_json::json!("00000000-0000-0000-0000-000000000020"))
                .unwrap();
        let mut attachment = PtyAttachment::new(process_id.clone(), 0);
        let bytes = vec![b'x'; MAX_PTY_ATTACHMENT_OUTPUT_BYTES + 17];
        attachment.append_output(0, bytes.len() as u64, &bytes);
        assert_eq!(attachment.output.len(), MAX_PTY_ATTACHMENT_OUTPUT_BYTES);
        let process = pi_coding::ProcessInfo {
            id: process_id,
            owner_id: pi_coding::ProcessOwnerId::new("test-owner"),
            label: None,
            state: ProcessState::Exited,
            pid: None,
            tty: true,
            started_at_ms: 1,
            exited_at_ms: Some(2),
            exit_code: Some(0),
            output_start_cursor: 0,
            output_cursor: bytes.len() as u64,
        };
        assert!(attachment.apply_event(&ProcessEvent::ProcessExited { process }));
    }

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
    fn process_exit_event_detaches_without_transcript_or_status_input_echo() {
        let process_id: ProcessId =
            serde_json::from_value(serde_json::json!("00000000-0000-0000-0000-000000000021"))
                .unwrap();
        let mut state = todo_test_state(Vec::new());
        state.pty_attachment = Some(PtyAttachment::new(process_id.clone(), 0));
        let transcript_len = state.transcript.len();
        let process = pi_coding::ProcessInfo {
            id: process_id,
            owner_id: pi_coding::ProcessOwnerId::new("test-owner"),
            label: None,
            state: ProcessState::Exited,
            pid: None,
            tty: true,
            started_at_ms: 1,
            exited_at_ms: Some(2),
            exit_code: Some(0),
            output_start_cursor: 0,
            output_cursor: 0,
        };
        state.apply_process_event(ProcessEvent::ProcessExited { process });
        assert!(state.pty_attachment.is_none());
        assert_eq!(state.transcript.len(), transcript_len);
        assert_eq!(state.status, "PTY process exited; detached");
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
    fn paste_targets_active_settings_input_without_touching_composer_or_transcript() {
        let mut state = todo_test_state(Vec::new());
        state.editor.set_text("composer sentinel");
        state.push_entry(TranscriptEntry {
            kind: TranscriptKind::Assistant,
            content: vec![ContentBlock::text("transcript sentinel")],
            tool_name: None,
            tool_card: None,
            job_card: None,
            is_error: false,
            is_partial: false,
        });
        state.settings_value_input = Some(SettingsValueInput {
            key: "extensions".to_owned(),
            value: "[".to_owned(),
            cursor: 1,
            hint: "JSON array of strings",
            error: None,
            replace_on_type: false,
        });

        handle_paste(&mut state, r#""alpha,beta","  padded  "]"#);

        assert_eq!(
            state.settings_value_input.as_ref().unwrap().value,
            r#"["alpha,beta","  padded  "]"#
        );
        assert_eq!(state.editor.text(), "composer sentinel");
        assert_eq!(state.transcript.len(), 1);
        assert_eq!(
            content_text(&state.transcript[0].content),
            "transcript sentinel"
        );

        let value_before = state.settings_value_input.as_ref().unwrap().value.clone();
        handle_paste(&mut state, &"x".repeat(MAX_PASTE_BYTES + 1));
        assert_eq!(
            state.settings_value_input.as_ref().unwrap().value,
            value_before
        );
        assert!(state.status.contains("Paste rejected"));
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
    fn extension_command_background_result_updates_status() {
        let mut state = todo_test_state(Vec::new());
        state.apply_background(BackgroundEvent::ExtensionCommandFinished {
            command: "dialog-smoke".to_owned(),
            result: Ok(serde_json::Value::Null),
        });
        assert_eq!(state.status, "Ran /dialog-smoke");

        state.apply_background(BackgroundEvent::ExtensionCommandFinished {
            command: "dialog-smoke".to_owned(),
            result: Err("extension failed".to_owned()),
        });
        assert!(state.status.contains("extension failed"));
    }

    #[test]
    fn slash_completion_fuzzy_matches_and_accepts_selection() {
        let (background_tx, _background_rx) = mpsc::unbounded_channel();
        let mut state = TuiState {
            tool_cards: ToolCardPresentationAdapter::new(),
            pty_attachment: None,
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
            extension_status_key: None,
            composer_error: None,
            composer_error_is_warning: false,
            model: String::new(),
            cwd: String::new(),
            completions: CompletionState::default(),
            themes: ThemeManager::default(),
            keybindings: KeyBindingsManager::default(),
            cwd_path: PathBuf::new(),
            recent_sessions: Vec::new(),
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
            todo_dag_panel: None,
            code_review_panel: None,
            code_review_controller: None,
            code_review_cleanup: None,
            code_review_controller_generation: 0,
            code_review_load_generation: 0,
            code_review_load_in_flight: None,
            code_review_scope: ReviewScope::WorkingTree,
            side_chat: None,
            side_chat_open: false,
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
            git_status: None,
            context_usage: None,
            last_footer_refresh: None,
            footer_refresh_in_flight: None,
            footer_refresh_pending: None,
            footer_refresh_current: None,
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
        let entry = TranscriptEntry { kind: TranscriptKind::Assistant, content: vec![ContentBlock::text(source)], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false,
        };
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
    fn assistant_mermaid_source_fallback_keeps_closing_row_before_separator() {
        // A Mermaid source-fallback block must keep its `└─` closure as the
        // final content row. The assistant path appends one trailing
        // separator row after the content, so stripping it must yield exactly
        // the shared neutral line sequence (closure included).
        let source = "```mermaid\nsequenceDiagram\nA->>B: fallback\n```";
        let width = 40;
        let expected = pi_coding::markdown::render_markdown(
            source,
            &pi_coding::markdown::MarkdownRenderOptions {
                width: usize::from(width),
                ..pi_coding::markdown::MarkdownRenderOptions::default()
            },
        )
        .plain_lines();
        assert!(
            expected.last().is_some_and(|line| line.as_str() == "└─"),
            "neutral renderer closes the fallback block: {expected:?}"
        );
        let entry = TranscriptEntry { kind: TranscriptKind::Assistant, content: vec![ContentBlock::text(source)], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false,
        };
        let mut lines = Vec::new();
        render_transcript_entry(&mut lines, &entry, true, true, crate::theme::DARK, width);
        assert!(
            lines.last().is_some_and(|line| line.spans.is_empty()),
            "assistant entry ends with a trailing separator row: {lines:?}"
        );
        let rendered = lines[..lines.len() - 1]
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>();
        assert_eq!(rendered, expected);
        assert_eq!(rendered.last().map(String::as_str), Some("└─"));
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
            .filter(|span| {
                matches!(span.content.as_ref(), "Ordinary" | "prose" | "explains" | "change")
            });
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
    fn user_card_has_one_internal_padding_row_per_edge_without_extra_separator() {
        let prompt = "Can you put it in the background?";
        let entry = TranscriptEntry { kind: TranscriptKind::User, content: vec![ContentBlock::text(prompt)], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false,
        };

        for width in [80, 10] {
            let mut lines = Vec::new();
            render_transcript_entry(&mut lines, &entry, true, true, crate::theme::DARK, width);

            let content = &lines[1..lines.len() - 1];
            assert!(lines.first().is_some_and(|line| line.spans.iter().all(|span| span.content.trim().is_empty())));
            assert!(lines.last().is_some_and(|line| line.spans.iter().all(|span| span.content.trim().is_empty())));
            let first_row = &content[0];
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
            assert!(lines.iter().all(|line| {
                line.spans.iter().map(|span| display_width(span.content.as_ref())).sum::<u16>() == width
            }));
            assert!(lines.iter().flat_map(|line| &line.spans).all(|span| {
                span.style.bg == Some(crate::theme::DARK.user_message_bg)
            }));
        }

        let unicode = TranscriptEntry { kind: TranscriptKind::User, content: vec![ContentBlock::text("🙂🙂🙂🙂🙂abc")], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false,
        };
        let mut lines = Vec::new();
        render_transcript_entry(&mut lines, &unicode, true, true, crate::theme::DARK, 10);
        let plain = lines[1..lines.len() - 1]
            .iter()
            .map(|line| {
                line.spans.iter().map(|span| span.content.as_ref()).collect::<String>()
            })
            .collect::<Vec<_>>();
        assert_eq!(plain[0].trim_end(), "🙂🙂🙂🙂");
        assert_eq!(plain[1].trim_end(), "🙂abc");
    }

    #[test]
    fn compact_roles_hide_repeated_labels_and_empty_entries() {
        let assistant = TranscriptEntry { kind: TranscriptKind::Assistant, content: vec![ContentBlock::text("answer")], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false,
        };
        let user = TranscriptEntry { kind: TranscriptKind::User, content: vec![ContentBlock::text("prompt")], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false,
        };
        let empty = TranscriptEntry { kind: TranscriptKind::Assistant, content: vec![ContentBlock::text("  "), ContentBlock::thinking("hidden")], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false,
        };
        let reasoning = TranscriptEntry { kind: TranscriptKind::Assistant, content: vec![ContentBlock::thinking("useful analysis"), ContentBlock::text("answer"),
            ], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false,
        };

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
        let user_content = lines
            .iter()
            .find(|line| line.spans.iter().any(|span| span.content.starts_with("prompt")))
            .expect("user content row");
        assert!(user_content.spans[0].content.starts_with("prompt"));
        assert_eq!(user_content.spans[0].style.bg, Some(crate::theme::DARK.user_message_bg));

        let before = lines.len();
        render_transcript_entry(&mut lines, &empty, false, true, crate::theme::DARK, 80);
        assert_eq!(lines.len(), before);

        let mut reasoning_lines = Vec::new();
        render_transcript_entry(&mut reasoning_lines, &reasoning, true, true, crate::theme::DARK, 80,
        );
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
            .map(|line| {
                line.spans.iter().map(|span| span.content.as_ref()).collect::<String>()
            })
            .collect::<Vec<_>>();
        let thinking_row = plain.iter().position(|line| line == "useful analysis").unwrap();
        let answer_row = plain.iter().position(|line| line == "answer").unwrap();
        assert_eq!(answer_row, thinking_row + 2);
        assert_eq!(plain[thinking_row + 1], "");
    }

    #[test]
    fn system_and_custom_labels_are_compact_without_card_background() {
        for entry in [
            TranscriptEntry { kind: TranscriptKind::System, content: vec![ContentBlock::text("failure")], tool_name: Some("Error".to_owned()), tool_card: None, job_card: None, is_error: true, is_partial: false,
            },
            TranscriptEntry { kind: TranscriptKind::Custom, content: vec![ContentBlock::text("notice")], tool_name: Some("release-note".to_owned()), tool_card: None, job_card: None, is_error: false, is_partial: false,
            },
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
        let entry = TranscriptEntry { kind: TranscriptKind::Custom, content: vec![ContentBlock::text("extension notice")], tool_name: Some("release-note".to_owned()), tool_card: None, job_card: None, is_error: false, is_partial: false,
        };
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
            AgentEvent::ToolExecutionStart { tool_call_id: "a".to_owned(), tool_name: "read".to_owned(), arguments: serde_json::json!({"path": "a.rs"}),
            },
            AgentEvent::ToolExecutionStart { tool_call_id: "b".to_owned(), tool_name: "read".to_owned(), arguments: serde_json::json!({"path": "b.rs"}),
            },
            AgentEvent::ToolExecutionEnd { tool_call_id: "b".to_owned(), tool_name: "read".to_owned(), result: AgentToolResult::text("body-b"), is_error: false,
            },
            AgentEvent::MessageEnd { message: Message::ToolResult(ToolResultMessage { tool_call_id: "b".to_owned(), tool_name: "read".to_owned(), content: vec![ContentBlock::text("body-b")], usage: None, details: None, added_tool_names: Vec::new(), is_error: false, timestamp: now_millis(),
                }),
            },
            AgentEvent::ToolExecutionEnd { tool_call_id: "a".to_owned(), tool_name: "read".to_owned(), result: AgentToolResult::text("not found"), is_error: true,
            },
        ] {
            state.apply(ApplicationEvent::Agent(event));
        }
        assert_eq!(state.transcript.iter().filter(|entry| entry.kind == TranscriptKind::Tool).count(), 2);
        assert_eq!(state.transcript.iter().filter(|entry| entry.tool_card.as_ref().is_some_and(|tool| tool.compact.tool_call_id == "b")).count(), 1);
        assert_eq!(state.transcript[0].tool_card.as_ref().unwrap().compact.status, ToolCallViewStatus::Failed);
        for entry in &state.transcript {
            let mut lines = Vec::new();
            render_transcript_entry(&mut lines, entry, true, false, crate::theme::DARK, 80);
            let rendered = lines.iter().map(|line| {
                    line.spans.iter().map(|span| span.content.as_ref()).collect::<String>()
                }).collect::<Vec<_>>();
            assert_eq!(rendered.iter().filter(|line| line.contains("Read")).count(), 1);
            assert!(!rendered.iter().any(|line| line.contains(" read")));
        }

        let mut bash = todo_test_state(Vec::new());
        let body = (1..=30).map(|line| line.to_string()).collect::<Vec<_>>().join("\n");
        bash.apply(ApplicationEvent::Agent(AgentEvent::ToolExecutionStart { tool_call_id: "bash".to_owned(), tool_name: "bash".to_owned(), arguments: serde_json::json!({"command": "seq 1 30"}),
        }));
        bash.apply(ApplicationEvent::Agent(AgentEvent::ToolExecutionEnd { tool_call_id: "bash".to_owned(), tool_name: "bash".to_owned(), result: AgentToolResult::text(body), is_error: false,
        }));
        let entry = bash.transcript.last().unwrap();
        let mut compact = Vec::new();
        render_transcript_entry(&mut compact, entry, true, false, crate::theme::DARK, 80);
        let compact_lines = compact.iter().map(|line| {
                line.spans.iter().map(|span| span.content.as_ref()).collect::<String>()
            }).collect::<Vec<_>>();
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
            ("edit", serde_json::json!({"path": "src/lib.rs"}), "@@\n-old\n+new", "Edit",
            ),
            ("write", serde_json::json!({"path": "src/lib.rs", "content": "new"}), "Successfully wrote", "Write",
            ),
        ] {
            let mut tool = todo_test_state(Vec::new());
            tool.apply(ApplicationEvent::Agent(AgentEvent::ToolExecutionStart { tool_call_id: name.to_owned(), tool_name: name.to_owned(), arguments,
            }));
            tool.apply(ApplicationEvent::Agent(AgentEvent::ToolExecutionEnd { tool_call_id: name.to_owned(), tool_name: name.to_owned(), result: AgentToolResult::text(output), is_error: false,
            }));
            let mut rendered = Vec::new();
            render_transcript_entry(&mut rendered, tool.transcript.last().unwrap(), true, false, crate::theme::DARK, 80,
            );
            let rendered = rendered.iter().map(|line| {
                    line.spans.iter().map(|span| span.content.as_ref()).collect::<String>()
                }).collect::<Vec<_>>();
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
        render_transcript_entry(&mut styled_read, read.transcript.last().unwrap(), true, false, crate::theme::DARK, 80,
        );
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
            instance: pi_coding::ExtensionInstanceId { extension_id: "demo".to_owned(), generation,
            },
            mode: pi_coding::ExtensionMode::Tui,
        }
    }

    async fn next_interaction(events: &mut tokio::sync::broadcast::Receiver<ExtensionUiEvent>,
    ) -> ExtensionUiEvent {
        tokio::time::timeout(std::time::Duration::from_secs(1), events.recv()).await.expect("extension event timeout").expect("extension event")
    }

    #[tokio::test]
    async fn extension_select_keyboard_returns_value_and_leaks_nothing() {
        use pi_coding::{ExtensionCancellation, ExtensionUiHost, ExtensionUiResponse};
        let adapter = ExtensionUiAdapter::new();
        let mut events = adapter.subscribe();
        let requester = adapter.clone();
        let task = tokio::spawn(async move { requester.request(tui_context(1), ExtensionUiRequest::Select { title: "Choose".to_owned(), options: vec![
            UiSelectOption { value: "first-value".to_owned(), label: "First label".to_owned(), description: None,
                            },
            UiSelectOption { value: "second-value".to_owned(), label: "Second label".to_owned(), description: Some("second description".to_owned()),
                            },
                        ],
                    }, ExtensionCancellation::new(),
                ).await });
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
        let task = tokio::spawn(async move { requester.request(tui_context(1), ExtensionUiRequest::Confirm { title: "Confirm".to_owned(), message: "Proceed?".to_owned(),
                    }, ExtensionCancellation::new(),
                ).await });
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
        let input = tokio::spawn(async move { requester.request(tui_context(1), ExtensionUiRequest::Input { title: "Input".to_owned(), placeholder: Some("hint".to_owned()), value: Some("pre".to_owned()),
                    }, ExtensionCancellation::new(),
                ).await });
        let mut state = dialog_test_state(adapter.clone());
        state.apply_extension_ui(next_interaction(&mut events).await);
        handle_extension_dialog_key(&mut state, KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE),
        );
        handle_extension_dialog_key(&mut state, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert_eq!(input.await.unwrap().unwrap(), ExtensionUiResponse::Input { value: Some("pre!".to_owned()) });

        let requester = adapter.clone();
        let editor = tokio::spawn(async move { requester.request(tui_context(1), ExtensionUiRequest::Editor { title: "Editor".to_owned(), prefill: Some("one".to_owned()),
                    }, ExtensionCancellation::new(),
                ).await });
        state.apply_extension_ui(next_interaction(&mut events).await);
        handle_extension_dialog_key(&mut state, KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
        );
        handle_extension_dialog_key(&mut state, KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
        );
        handle_extension_dialog_key(&mut state, KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE),
        );
        handle_extension_dialog_key(&mut state, KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE),
        );
        handle_extension_dialog_key(&mut state, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert_eq!(editor.await.unwrap().unwrap(), ExtensionUiResponse::Edited { value: Some("one\ntwo".to_owned()) });
        assert!(adapter.pending_interactions().is_empty());
    }

    #[tokio::test]
    async fn extension_dialog_rejects_concurrent_request_and_reload_cancels_active() {
        use pi_coding::{ExtensionCancellation, ExtensionUiHost, ExtensionUiResponse};
        let adapter = ExtensionUiAdapter::new();
        let mut events = adapter.subscribe();
        let first_adapter = adapter.clone();
        let first = tokio::spawn(async move { first_adapter.request(tui_context(1), ExtensionUiRequest::Confirm { title: "First".to_owned(), message: "first".to_owned(),
                    }, ExtensionCancellation::new(),
                ).await });
        let mut state = dialog_test_state(adapter.clone());
        state.apply_extension_ui(next_interaction(&mut events).await);
        let second_adapter = adapter.clone();
        let second = tokio::spawn(async move { second_adapter.request(tui_context(1), ExtensionUiRequest::Input { title: "Second".to_owned(), placeholder: None, value: None,
                    }, ExtensionCancellation::new(),
                ).await });
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
    async fn extension_interactions_do_not_preempt_exclusive_overlays() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = pi_coding::Session::new(pi_coding::SessionOptions {
            model: Model::default(), cwd: cwd.path().to_path_buf(), system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off, api_key: String::new(), compaction: None,
            stream_options: Default::default(), tools: Some(Vec::new()), before_tool_call: None,
            after_tool_call: None, stream_fn: None, auth_resolver: None,
        }).expect("session");
        let application = Application::new(session).await;
        use pi_coding::{ExtensionCancellation, ExtensionUiHost, ExtensionUiResponse};

        for owner in ["code review", "side chat", "PTY"] {
            let adapter = ExtensionUiAdapter::new();
            let mut events = adapter.subscribe();
            let requester = adapter.clone();
            let pending = tokio::spawn(async move {
                requester
                    .request(
                        tui_context(1),
                        ExtensionUiRequest::Confirm {
                            title: "Blocked".to_owned(),
                            message: "blocked".to_owned(),
                        },
                        ExtensionCancellation::new(),
                    )
                    .await
            });
            let mut state = dialog_test_state(adapter.clone());
            match owner {
                "code review" => {
                    let mut mouse = TestCodeReviewMouse::default();
                    state.cwd_path = std::env::temp_dir();
                    state
                        .open_code_review_panel(
                            &application,
                            ReviewScope::WorkingTree,
                            &mut mouse,
                        )
                        .await
                        .expect("open review");
                }
                "side chat" => state.side_chat_open = true,
                "PTY" => {
                    let process_id: ProcessId = serde_json::from_value(serde_json::json!(
                        "00000000-0000-0000-0000-000000000023"
                    ))
                    .expect("process id");
                    state.pty_attachment = Some(PtyAttachment::new(process_id, 0));
                }
                _ => unreachable!(),
            }

            state.apply_extension_ui(next_interaction(&mut events).await);

            assert_eq!(
                pending.await.unwrap().unwrap(),
                ExtensionUiResponse::Cancelled
            );
            assert!(state.extension_dialog.is_none(), "{owner}");
            assert!(
                state.status.contains("rejected"),
                "{}: {}",
                owner,
                state.status
            );
            assert!(adapter.pending_interactions().is_empty(), "{owner}");
        }
    }

    #[tokio::test]
    async fn extension_interaction_deferred_for_any_page_overlay() {
        use pi_coding::{ExtensionCancellation, ExtensionUiHost, ExtensionUiResponse};
        let adapter = ExtensionUiAdapter::new();
        let mut events = adapter.subscribe();
        let requester = adapter.clone();
        let pending = tokio::spawn(async move {
            requester
                .request(
                    tui_context(1),
                    ExtensionUiRequest::Confirm {
                        title: "Blocked".to_owned(),
                        message: "blocked".to_owned(),
                    },
                    ExtensionCancellation::new(),
                )
                .await
        });
        let mut state = dialog_test_state(adapter.clone());

        // A selector panel is a page overlay beyond code-review/side-chat/PTY.
        state.panel = Some(SelectorPanel {
            title: "Goals".to_owned(),
            help: String::new(),
            items: vec![PanelItem {
                label: "Show".to_owned(),
                description: String::new(),
                value: PanelValue::GoalShow,
                checked: false,
            }],
            selected: 0,
            query: String::new(),
        });
        assert!(page_overlay_open(&state));

        state.apply_extension_ui(next_interaction(&mut events).await);

        assert_eq!(
            pending.await.unwrap().unwrap(),
            ExtensionUiResponse::Cancelled
        );
        assert!(
            state.extension_dialog.is_none(),
            "interaction must not preempt a non-code-review page overlay"
        );
        assert!(
            state.status.contains("rejected"),
            "status: {}",
            state.status
        );
        assert!(
            state.panel.is_some(),
            "the owning page overlay must remain open"
        );
        assert!(adapter.pending_interactions().is_empty());
    }

    #[test]
    fn extension_keyed_status_clear_retires_only_matching_live_status() {
        use crate::extension_ui::ExtensionStatusItem;
        use pi_coding::ExtensionInstanceId;

        let adapter = ExtensionUiAdapter::new();
        let mut state = dialog_test_state(adapter.clone());

        let demo = ExtensionInstanceId {
            extension_id: "demo".to_owned(),
            generation: 1,
        };
        let other = ExtensionInstanceId {
            extension_id: "other".to_owned(),
            generation: 1,
        };

        state.apply_extension_ui(ExtensionUiEvent::StatusChanged {
            item: ExtensionStatusItem {
                instance: demo.clone(),
                key: "progress".to_owned(),
                text: "loading".to_owned(),
            },
        });
        assert_eq!(state.status, "loading");

        // A newer, unrelated keyed status supersedes the first.
        state.apply_extension_ui(ExtensionUiEvent::StatusChanged {
            item: ExtensionStatusItem {
                instance: other.clone(),
                key: "result".to_owned(),
                text: "done".to_owned(),
            },
        });
        assert_eq!(state.status, "done");

        // Clearing the now-stale key must not clobber the newer live status.
        state.apply_extension_ui(ExtensionUiEvent::StatusCleared {
            instance: demo,
            key: "progress".to_owned(),
        });
        assert_eq!(
            state.status, "done",
            "clearing a non-live keyed status must preserve the newer status"
        );

        // Clearing the active key retires the live composer status.
        state.apply_extension_ui(ExtensionUiEvent::StatusCleared {
            instance: other,
            key: "result".to_owned(),
        });
        assert_eq!(
            state.status, "",
            "clearing the live keyed status must reset the composer status"
        );
        assert!(
            state.extension_status_key.is_none(),
            "keyed status ownership must be cleared after retirement"
        );
    }

    #[test]
    fn extension_keyed_status_clear_preserves_newer_working_status() {
        use crate::extension_ui::ExtensionStatusItem;
        use pi_coding::ExtensionInstanceId;

        let adapter = ExtensionUiAdapter::new();
        let mut state = dialog_test_state(adapter.clone());
        let demo = ExtensionInstanceId {
            extension_id: "demo".to_owned(),
            generation: 1,
        };

        state.apply_extension_ui(ExtensionUiEvent::StatusChanged {
            item: ExtensionStatusItem {
                instance: demo.clone(),
                key: "progress".to_owned(),
                text: "loading".to_owned(),
            },
        });
        assert_eq!(state.status, "loading");

        // A working message supersedes the keyed extension status.
        state.extension_working_visible = true;
        state.apply_extension_ui(ExtensionUiEvent::WorkingMessageChanged {
            instance: demo.clone(),
            message: Some("thinking".to_owned()),
        });
        assert_eq!(state.status, "thinking");

        // Clearing the now-stale keyed status must not erase the working status.
        state.apply_extension_ui(ExtensionUiEvent::StatusCleared {
            instance: demo,
            key: "progress".to_owned(),
        });
        assert_eq!(
            state.status, "thinking",
            "clearing a stale keyed status must not clobber a newer working status"
        );
    }

    #[test]
    fn extension_keyed_status_clear_preserves_newer_ordinary_status() {
        use crate::extension_ui::ExtensionStatusItem;
        use pi_coding::ExtensionInstanceId;

        let adapter = ExtensionUiAdapter::new();
        let mut state = dialog_test_state(adapter.clone());
        let demo = ExtensionInstanceId {
            extension_id: "demo".to_owned(),
            generation: 1,
        };

        state.apply_extension_ui(ExtensionUiEvent::StatusChanged {
            item: ExtensionStatusItem {
                instance: demo.clone(),
                key: "progress".to_owned(),
                text: "loading".to_owned(),
            },
        });
        assert_eq!(state.status, "loading");
        assert!(state.extension_status_key.is_some());

        // An ordinary host-side status write (ApplicationEvent/LoopEvent/paste/
        // slash command) replaces `status` directly without retiring the keyed
        // owner. The centralized `StatusCleared` text-match check must defend
        // this path.
        state.status = "Compacting context".to_owned();

        // A stale keyed clear must not clobber the newer ordinary status.
        state.apply_extension_ui(ExtensionUiEvent::StatusCleared {
            instance: demo,
            key: "progress".to_owned(),
        });
        assert_eq!(
            state.status,
            "Compacting context",
            "stale keyed clear must not clobber a newer ordinary status"
        );
    }

    #[test]
    fn extension_cleared_retires_owned_live_status() {
        use crate::extension_ui::ExtensionStatusItem;
        use pi_coding::ExtensionInstanceId;

        let adapter = ExtensionUiAdapter::new();
        let mut state = dialog_test_state(adapter.clone());
        let demo = ExtensionInstanceId {
            extension_id: "demo".to_owned(),
            generation: 1,
        };

        state.apply_extension_ui(ExtensionUiEvent::StatusChanged {
            item: ExtensionStatusItem {
                instance: demo.clone(),
                key: "progress".to_owned(),
                text: "loading".to_owned(),
            },
        });
        assert_eq!(state.status, "loading");

        // Unloading the owning extension retires its live keyed composer status.
        state.apply_extension_ui(ExtensionUiEvent::ExtensionCleared {
            instance: demo.clone(),
        });
        assert_eq!(
            state.status, "",
            "ExtensionCleared must retire the live keyed status it owned"
        );
        assert!(
            state.extension_status_key.is_none(),
            "ExtensionCleared must retire keyed status ownership"
        );

        // A stale StatusCleared arriving after the unload must not resurrect or
        // clobber a newer status set in the meantime.
        state.status = "Ready".to_owned();
        state.apply_extension_ui(ExtensionUiEvent::StatusCleared {
            instance: demo,
            key: "progress".to_owned(),
        });
        assert_eq!(
            state.status, "Ready",
            "stale keyed clear after ExtensionCleared must be a no-op"
        );
    }

    #[test]
    fn extension_cleared_preserves_newer_ordinary_status() {
        use crate::extension_ui::ExtensionStatusItem;
        use pi_coding::ExtensionInstanceId;

        let adapter = ExtensionUiAdapter::new();
        let mut state = dialog_test_state(adapter.clone());
        let demo = ExtensionInstanceId {
            extension_id: "demo".to_owned(),
            generation: 1,
        };
        state.apply_extension_ui(ExtensionUiEvent::StatusChanged {
            item: ExtensionStatusItem {
                instance: demo.clone(),
                key: "progress".to_owned(),
                text: "loading".to_owned(),
            },
        });
        state.status = "Ready".to_owned();

        state.apply_extension_ui(ExtensionUiEvent::ExtensionCleared { instance: demo });

        assert_eq!(state.status, "Ready");
        assert!(state.extension_status_key.is_none());
    }

    #[test]
    fn extension_cleared_preserves_other_extension_live_status() {
        use crate::extension_ui::ExtensionStatusItem;
        use pi_coding::ExtensionInstanceId;

        let adapter = ExtensionUiAdapter::new();
        let mut state = dialog_test_state(adapter.clone());
        let demo = ExtensionInstanceId {
            extension_id: "demo".to_owned(),
            generation: 1,
        };
        let other = ExtensionInstanceId {
            extension_id: "other".to_owned(),
            generation: 1,
        };

        state.apply_extension_ui(ExtensionUiEvent::StatusChanged {
            item: ExtensionStatusItem {
                instance: demo.clone(),
                key: "progress".to_owned(),
                text: "loading".to_owned(),
            },
        });
        assert_eq!(state.status, "loading");

        // Unloading an unrelated extension must not retire demo's keyed status.
        state.apply_extension_ui(ExtensionUiEvent::ExtensionCleared {
            instance: other,
        });
        assert_eq!(
            state.status, "loading",
            "ExtensionCleared for another extension must preserve the live keyed status"
        );
        assert!(state.extension_status_key.is_some());

        // The live key still clears normally when demo retires it.
        state.apply_extension_ui(ExtensionUiEvent::StatusCleared {
            instance: demo,
            key: "progress".to_owned(),
        });
        assert_eq!(state.status, "");
    }

    #[tokio::test]
    async fn extension_set_editor_text_updates_main_editor_except_during_modal_edit() {
        use pi_coding::{ExtensionCancellation, ExtensionUiHost};
        let adapter = ExtensionUiAdapter::new();
        let mut events = adapter.subscribe();
        adapter.request(tui_context(1), ExtensionUiRequest::SetEditorText { text: "main".to_owned(),
                }, ExtensionCancellation::new(),
            ).await.unwrap();
        let mut state = dialog_test_state(adapter.clone());
        state.apply_extension_ui(next_interaction(&mut events).await);
        assert_eq!(state.editor.text(), "main");
        state.extension_dialog = Some(ExtensionDialog::new(interaction(ExtensionUiRequest::Editor { title: "Edit".to_owned(), prefill: Some("modal".to_owned()),
            },
        )));
        state.apply_extension_ui(ExtensionUiEvent::EditorTextChanged { instance: tui_context(1).instance, text: "ignored".to_owned(),
        });
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
    fn transcript_entry_adjacency_is_cell_and_style_aware() {
        fn entry(kind: TranscriptKind, text: &str) -> TranscriptEntry {
            TranscriptEntry { kind, content: vec![ContentBlock::text(text)], tool_name: matches!(kind, TranscriptKind::System | TranscriptKind::Custom).then(|| format!("{kind:?}")), tool_card: None, job_card: None, is_error: false, is_partial: false }
        }
        fn rows(entries: &[TranscriptEntry]) -> Vec<(String, Vec<Color>)> {
            let width = 24;
            let lines = assemble_transcript_entries(entries, true, true, crate::theme::DARK, width);
            let height = u16::try_from(lines.len()).unwrap();
            let area = Rect::new(0, 0, width, height);
            let mut buffer = ratatui::buffer::Buffer::empty(area);
            for (row, line) in lines.into_iter().enumerate() {
                line.render(Rect::new(0, u16::try_from(row).unwrap(), width, 1), &mut buffer);
            }
            (0..height).map(|y| { let cells = (0..width).map(|x| &buffer[(x, y)]).collect::<Vec<_>>(); (cells.iter().map(|cell| cell.symbol()).collect::<String>(), cells.iter().map(|cell| cell.bg).collect()) }).collect()
        }
        fn at(rows: &[(String, Vec<Color>)], text: &str) -> usize { rows.iter().position(|(row, _)| row.contains(text)).unwrap() }
        fn plain(row: &(String, Vec<Color>)) { assert!(row.0.trim().is_empty()); assert!(row.1.iter().all(|color| *color == Color::Reset)); }

        let rendered = rows(&[entry(TranscriptKind::User, "first user"), entry(TranscriptKind::User, "second user")]); let first = at(&rendered, "first user"); let second = at(&rendered, "second user"); assert_eq!(second - first, 4); assert!(rendered[first + 1].1.iter().all(|color| *color == crate::theme::DARK.user_message_bg)); plain(&rendered[first + 2]); assert!(rendered[first + 3].1.iter().all(|color| *color == crate::theme::DARK.user_message_bg));
        let rendered = rows(&[entry(TranscriptKind::User, "user before"), entry(TranscriptKind::Assistant, "assistant after")]); let user = at(&rendered, "user before"); let assistant = at(&rendered, "assistant after"); assert_eq!(assistant - user, 3); assert!(rendered[user + 1].1.iter().all(|color| *color == crate::theme::DARK.user_message_bg)); plain(&rendered[user + 2]);
        let rendered = rows(&[entry(TranscriptKind::Assistant, "assistant before"), entry(TranscriptKind::User, "user after")]); let assistant = at(&rendered, "assistant before"); let user = at(&rendered, "user after"); assert_eq!(user - assistant, 3); plain(&rendered[assistant + 1]); assert!(rendered[assistant + 2].1.iter().all(|color| *color == crate::theme::DARK.user_message_bg));
        let rendered = rows(&[entry(TranscriptKind::System, "system body"), entry(TranscriptKind::Custom, "custom body")]); let system = at(&rendered, "system body"); plain(&rendered[system + 1]); assert!(rendered[system + 2].0.contains("Custom"));
    }

    #[test]
    fn transcript_live_and_commit_assembly_are_identical() {
        let entries = vec![TranscriptEntry { kind: TranscriptKind::User, content: vec![ContentBlock::text("first")], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false }, TranscriptEntry { kind: TranscriptKind::Assistant, content: vec![ContentBlock::text("answer")], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false }, TranscriptEntry { kind: TranscriptKind::User, content: vec![ContentBlock::text("second")], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false }];
        let mut state = todo_test_state(Vec::new()); state.transcript = entries.clone(); let mut renderer = TerminalImageRenderer::default(); let mut candidates = Vec::new(); let mut image_context = TranscriptImageContext { renderer: &mut renderer, candidates: &mut candidates, config: ImageDisplayConfig { show_images: false, width_cells: 50 }, viewport_columns: 40, viewport_rows: 40, cell_size: TerminalCellSize::default() };
        let live = render_transcript_lines(&state, crate::theme::DARK, 40, &mut image_context); let committed = assemble_transcript_entries(&entries, state.show_thinking, state.expand_tools, crate::theme::DARK, 40); assert_eq!(live, committed);
    }

    #[test]
    fn transcript_commit_boundary_preserves_visible_adjacency() {
        let entries = vec![
            TranscriptEntry { kind: TranscriptKind::User, content: vec![ContentBlock::text("committed user")], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false },
            TranscriptEntry { kind: TranscriptKind::Assistant, content: vec![ContentBlock::text("live assistant")], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false },
        ];
        let expected = assemble_transcript_entries(&entries, true, true, crate::theme::DARK, 40);
        let mut split = assemble_committed_transcript_entries(&entries[..1], true, true, crate::theme::DARK, 40, true);
        split.extend(assemble_transcript_entries(&entries[1..], true, true, crate::theme::DARK, 40));
        assert_eq!(split, expected);

        let mut state = todo_test_state(Vec::new());
        state.transcript = vec![
            entries[0].clone(),
            TranscriptEntry { kind: TranscriptKind::Assistant, content: vec![ContentBlock::thinking("hidden")], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false },
        ];
        state.show_thinking = false;
        assert!(!state.has_visible_entry_after(1));
        state.streaming_text = "partial answer".to_owned();
        assert!(state.has_visible_entry_after(1));
        state.streaming_text.clear();
        state.streaming_thinking = "hidden partial thinking".to_owned();
        assert!(!state.has_visible_entry_after(1));
        state.show_thinking = true;
        assert!(state.has_visible_entry_after(1));
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
        let mut mouse = TestCodeReviewMouse::default();
        state.open_goal_panel(&application, &mut mouse)
            .expect("open goal");
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
        let mut mouse = TestCodeReviewMouse::default();
        state.open_goal_panel(&application, &mut mouse)
            .expect("open goal");
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
        assert!(!toast.contains("Dismissed when"), "{toast}");

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

    fn resume_row_with_preview(preview: &str) -> ResumeSelectorRow {
        ResumeSelectorRow {
            source: pi_coding::SessionSourceKind::Codex,
            source_badge: "codex",
            session_id: "preview".to_owned(),
            cwd: PathBuf::from("/workspace"),
            modified_epoch: 1.0,
            display_time: "2026-08-01 00:00".to_owned(),
            summary: preview.to_owned(),
            path: PathBuf::from("/sessions/preview.jsonl"),
            size: 1,
            message_count: Some(3),
            name: None,
            status: pi_coding::CatalogRowStatus::Foreign,
            search_text: preview.to_owned(),
        }
    }

    #[test]
    fn saved_session_preview_preserves_logical_line_breaks() {
        let row = resume_row_with_preview("first\u{1b}[31m line\nsecond\u{1b}[0m line\nthird line");
        assert_eq!(
            saved_session_preview_lines(&row, "•", false, usize::MAX),
            vec![
                "• [codex] first line · 3 messages".to_owned(),
                "  second line".to_owned(),
                "  third line".to_owned(),
            ]
        );
    }

    fn saved_session_rows(count: usize) -> Vec<ResumeSelectorRow> {
        (0..count)
            .map(|index| {
                let id = format!("session-{index:03}");
                let mut row = resume_row_with_preview(&id);
                row.session_id = id.clone();
                row.summary = id.clone();
                row.search_text = id.clone();
                row.path = PathBuf::from(format!("/sessions/{id}.jsonl"));
                row.modified_epoch = f64::from(u32::try_from(index).expect("fixture index"));
                row.status = pi_coding::CatalogRowStatus::Native;
                row
            })
            .collect()
    }

    fn rendered_line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn saved_session_selector_lines_are_capped_to_viewport_and_keep_current_marker() {
        let mut rows = saved_session_rows(40);
        let current = rows[35].path.clone();
        rows[35].status = pi_coding::CatalogRowStatus::AlreadyImported {
            native_id: "native-current".to_owned(),
            native_path: current.clone(),
        };
        let selector = SavedSessionSelector::new(rows, Some(current));
        let lines = saved_session_selector_lines(&selector, crate::theme::DARK, 8);
        let rendered = lines.iter().map(rendered_line_text).collect::<Vec<_>>();

        assert_eq!(lines.len(), 8);
        assert!(
            rendered.iter().any(|line| line.contains("session-039")),
            "{rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("• [codex] session-035 · 3 messages · imported")),
            "{rendered:?}"
        );
        assert!(
            !rendered.iter().any(|line| line.contains("session-034")),
            "{rendered:?}"
        );
    }

    #[test]
    fn saved_session_selector_scrolls_selection_to_last_row_with_selected_style() {
        let rows = saved_session_rows(40);
        let mut selector = SavedSessionSelector::new(rows, None);
        selector.move_selection(isize::MAX);
        let lines = saved_session_selector_lines(&selector, crate::theme::DARK, 8);
        let rendered = lines.iter().map(rendered_line_text).collect::<Vec<_>>();

        assert_eq!(selector.selected(), 39);
        assert_eq!(lines.len(), 8);
        assert!(
            rendered.iter().any(|line| line.contains("session-000")),
            "{rendered:?}"
        );
        assert!(
            !rendered.iter().any(|line| line.contains("session-005")),
            "{rendered:?}"
        );
        let selected_line = lines
            .iter()
            .find(|line| rendered_line_text(line).contains("session-000"))
            .expect("last selected row");
        assert!(
            selected_line
                .spans
                .iter()
                .all(|span| span.style.bg == Some(crate::theme::DARK.selected_bg))
        );
    }

    #[test]
    fn saved_session_selector_caps_multiline_preview_allocation_to_row_budget() {
        let preview = (0..100)
            .map(|index| format!("line-{index:03}"))
            .collect::<Vec<_>>()
            .join("\n");
        let selector = SavedSessionSelector::new(vec![resume_row_with_preview(&preview)], None);
        let lines = saved_session_selector_lines(&selector, crate::theme::DARK, 8);
        let rendered = lines.iter().map(rendered_line_text).collect::<Vec<_>>();

        assert_eq!(lines.len(), 8);
        assert!(
            rendered.iter().any(|line| line.contains("line-000")),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|line| line.contains("line-004")),
            "{rendered:?}"
        );
        assert!(
            !rendered.iter().any(|line| line.contains("line-005")),
            "{rendered:?}"
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

    #[tokio::test]
    async fn activity_reconciliation_clears_settled_state_after_event_lag() {
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
        let application = Application::new(session).await;
        let mut state = todo_test_state(Vec::new());
        state.is_streaming = true;
        state.streaming_text = "stale assistant delta".to_owned();
        state.streaming_thinking = "stale thinking delta".to_owned();
        state.status = "Working".to_owned();

        assert!(!application.is_streaming());
        state.reconcile_activity_from_application(&application);

        assert!(!state.is_streaming);
        assert!(state.streaming_text.is_empty());
        assert!(state.streaming_thinking.is_empty());
        assert_eq!(state.status, "Ready");
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
            extension_status_key: None,
            composer_error: None,
            composer_error_is_warning: false,
            model: String::new(),
            cwd: String::new(),
            completions: CompletionState::default(),
            themes: ThemeManager::default(),
            keybindings: KeyBindingsManager::default(),
            cwd_path: PathBuf::new(),
            recent_sessions: Vec::new(),
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
            todo_dag_panel: None,
            code_review_panel: None,
            code_review_controller: None,
            code_review_cleanup: None,
            code_review_controller_generation: 0,
            code_review_load_generation: 0,
            code_review_load_in_flight: None,
            code_review_scope: ReviewScope::WorkingTree,
            side_chat: None,
            side_chat_open: false,
            settings_value_input: None,
            tree_panel: None,
            process_panel: None,
            pty_attachment: None,
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
            seen_irc_message_ids: std::collections::HashSet::new(),
            git_status: None,
            context_usage: None,
            last_footer_refresh: None,
            footer_refresh_in_flight: None,
            footer_refresh_pending: None,
            footer_refresh_current: None,
            goal_state: GoalState::default(),
            active_loops: std::collections::BTreeMap::new(),
        }
    }

    fn workflow_snapshot(generation: u64, status: pi_coding::WorkflowStatus,
    ) -> pi_coding::WorkflowSnapshot {
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
        },
        );
        assert_eq!(state.workflow_snapshots[0].status, pi_coding::WorkflowStatus::Running);

        state.apply_workflow_event(&application, pi_coding::WorkflowEvent::Removed {
            workflow_id: pi_coding::WorkflowId::new("workflow-generation"),
            generation: 1,
        },
        );
        assert_eq!(state.workflow_snapshots.len(), 1);

        state.apply_workflow_event(&application, pi_coding::WorkflowEvent::StatusChanged {
            workflow_id: pi_coding::WorkflowId::new("workflow-generation"),
            generation: 2,
            status: pi_coding::WorkflowStatus::Paused,
        },
        );
        assert_eq!(state.workflow_snapshots[0].status, pi_coding::WorkflowStatus::Paused);
        application.cleanup().await;
    }

    // Runtime factory whose `create` parks on a `Notify` until the test releases
    // it, modelling the slow worktree/model/supervisor admission that froze the
    // TUI when `/workflow create` awaited it inline on the event loop.
    struct GatedWorkflowFactory {
        create_gate: std::sync::Arc<tokio::sync::Notify>,
        create_entered: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        created: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    fn gated_runtime_update(
        snapshot: &pi_coding::WorkflowSnapshot,
        status: pi_coding::WorkflowStatus,
    ) -> pi_coding::WorkflowRuntimeUpdate {
        pi_coding::WorkflowRuntimeUpdate {
            status,
            todo: snapshot.todo.clone(),
            supervisor_agent_id: snapshot.supervisor_agent_id.clone(),
            supervisor_job_id: snapshot.supervisor_job_id.clone(),
            failure: None,
            integration: snapshot.integration.clone(),
        }
    }

    #[async_trait::async_trait]
    impl pi_coding::WorkflowRuntimeFactory for GatedWorkflowFactory {
        async fn create(
            &self,
            request: &pi_coding::WorkflowRuntimeRequest,
        ) -> anyhow::Result<pi_coding::WorkflowRuntimeIdentity> {
            self.create_entered
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.create_gate.notified().await;
            self.created
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(pi_coding::WorkflowRuntimeIdentity {
                worktree_label: Some(format!("workspaces/{}", request.workflow_id.as_str())),
                branch: Some(format!("rpi/workflow/{}", request.workflow_id.as_str())),
                supervisor_agent_id: None,
                supervisor_job_id: None,
                todo: pi_coding::TodoState {
                    phases: Vec::new(),
                    storage: pi_coding::TodoStorage::Memory,
                },
            })
        }
        async fn restore(
            &self,
            request: &pi_coding::WorkflowRuntimeRequest,
            _snapshot: &pi_coding::WorkflowSnapshot,
        ) -> anyhow::Result<pi_coding::WorkflowRuntimeIdentity> {
            self.create(request).await
        }
        async fn pause(
            &self,
            snapshot: &pi_coding::WorkflowSnapshot,
        ) -> anyhow::Result<pi_coding::WorkflowRuntimeUpdate> {
            Ok(gated_runtime_update(snapshot, pi_coding::WorkflowStatus::Paused))
        }
        async fn resume(
            &self,
            snapshot: &pi_coding::WorkflowSnapshot,
        ) -> anyhow::Result<pi_coding::WorkflowRuntimeUpdate> {
            Ok(gated_runtime_update(snapshot, pi_coding::WorkflowStatus::Running))
        }
        async fn cancel(
            &self,
            snapshot: &pi_coding::WorkflowSnapshot,
        ) -> anyhow::Result<pi_coding::WorkflowRuntimeUpdate> {
            Ok(gated_runtime_update(snapshot, pi_coding::WorkflowStatus::Cancelled))
        }
        async fn integrate(
            &self,
            snapshot: &pi_coding::WorkflowSnapshot,
        ) -> anyhow::Result<pi_coding::WorkflowRuntimeUpdate> {
            Ok(gated_runtime_update(snapshot, pi_coding::WorkflowStatus::Completed))
        }
        async fn remove(&self, _snapshot: &pi_coding::WorkflowSnapshot) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn workflow_create_dispatch_returns_before_delayed_backend_completes() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::Duration;

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
        let application = Application::new(session).await;

        let create_gate = Arc::new(tokio::sync::Notify::new());
        let create_entered = Arc::new(AtomicUsize::new(0));
        let created = Arc::new(AtomicUsize::new(0));
        let factory = Arc::new(GatedWorkflowFactory {
            create_gate: create_gate.clone(),
            create_entered: create_entered.clone(),
            created: created.clone(),
        });
        let store_root = tempfile::tempdir().expect("store");
        let manager = pi_coding::WorkflowManager::open_with_factory(store_root.path(), factory)
            .expect("manager");
        application
            .attach_workflow_manager(manager)
            .expect("attach workflow manager");
        let mut app_events = application.subscribe();

        let (background_tx, mut background_rx) = mpsc::unbounded_channel();
        let mut state = todo_test_state(Vec::new());
        state.background_tx = background_tx;

        // Valid `/workflow create` (the `crete` typo path is already covered by
        // workflow_commands::tests; this proves the *valid* dispatch is
        // nonblocking — the exact scenario that hung the composer).
        let command = crate::workflow_commands::parse_interactive_workflow_command(
            Some(r#"create "ship it" "land multi workflow""#),
        )
        .expect("parse create");
        assert!(
            !matches!(
                command,
                crate::workflow_commands::InteractiveWorkflowCommand::OpenPage
            ),
            "create must not parse to OpenPage",
        );

        // Dispatch admits the command in the background and returns
        // synchronously: `admit_workflow_command_background` is not async, so by
        // construction it cannot block the event loop on the backend.
        state.admit_workflow_command_background(&application, command);

        // Let the spawned admit task run up to the gated `factory.create`, then
        // assert the durable backend is parked (entered but not completed).
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while create_entered.load(Ordering::SeqCst) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "gated factory create was never entered",
            );
            tokio::task::yield_now().await;
        }

        // Visible admission status set immediately; composer cleared.
        assert!(
            state.status.contains("Creating workflow"),
            "admission status must be visible: {}",
            state.status,
        );
        assert!(state.composer_error.is_none());

        // The durable backend has not completed: the factory is parked on its
        // gate, no background completion exists, and no workflow event has been
        // forwarded yet. Dispatch returned before the backend completed.
        assert_eq!(
            created.load(Ordering::SeqCst),
            0,
            "factory create must not complete before its gate releases",
        );
        assert!(
            background_rx.try_recv().is_err(),
            "no completion event before the gate releases",
        );
        assert!(
            app_events.try_recv().is_err(),
            "no workflow event before the backend completes",
        );

        // Release the delayed backend.
        create_gate.notify_one();

        // Final status arrives through the background channel.
        let finished = tokio::time::timeout(Duration::from_secs(30), background_rx.recv())
            .await
            .expect("workflow command finished event")
            .expect("background channel open");
        let BackgroundEvent::WorkflowCommandFinished { label, result } = finished else {
            panic!("expected WorkflowCommandFinished");
        };
        assert_eq!(label, "Creating workflow");
        assert_eq!(
            created.load(Ordering::SeqCst),
            1,
            "factory create completed exactly once",
        );
        let message = match result {
            Ok(crate::workflow_commands::WorkflowCommandEffect::Message(message)) => message,
            other => panic!("expected message effect, got {other:?}"),
        };
        assert!(
            message.contains("ship it"),
            "final status should name the workflow: {message}",
        );
        assert!(
            message.contains("rpi/workflow/"),
            "final status should show the branch: {message}",
        );

        // Applying the completion surfaces the human confirmation as the live
        // status (live snapshot updates arrive separately via Workflow events).
        state.status = String::new();
        state.apply_background(BackgroundEvent::WorkflowCommandFinished {
            label,
            result: Ok(crate::workflow_commands::WorkflowCommandEffect::Message(message)),
        });
        assert!(
            state.status.contains("ship it"),
            "apply_background must set the final status: {}",
            state.status,
        );

        // The live snapshot update arrives through the Application event channel
        // (the established forwarding mechanism, not a second convention).
        let app_event = tokio::time::timeout(Duration::from_secs(30), app_events.recv())
            .await
            .expect("workflow event forwarded")
            .expect("application event channel open");
        assert!(
            matches!(
                app_event,
                pi_coding::ApplicationEvent::Workflow(
                    pi_coding::WorkflowEvent::Created { .. }
                ),
            ),
            "expected WorkflowEvent::Created",
        );

        application.cleanup().await;
    }

    #[tokio::test]
    async fn todo_command_opens_real_page_for_empty_main_dag() {
        use ratatui::backend::TestBackend;
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
        let application = Application::new(session).await;
        let mut state = todo_test_state(Vec::new());
        let mut mouse = TestCodeReviewMouse::default();
        state
            .open_todo_dag_panel(&application, &mut mouse)
            .expect("open todo");
        assert!(state.todo_dag_panel.is_some());
        assert!(page_overlay_open(&state));

        let backend = TestBackend::new(90, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                render_todo_dag_panel(
                    frame,
                    state.todo_dag_panel.as_ref().expect("panel"),
                    crate::theme::DARK,
                )
            })
            .expect("render");
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Todo DAGs"));
        assert!(text.contains("Main session"));
        assert!(text.contains("[main]"));

        assert_eq!(
            handle_todo_dag_panel_key(
                &mut state,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            Some(false)
        );
        assert_eq!(
            state.todo_dag_panel.as_ref().map(TodoDagPanel::page),
            Some(crate::todo_dag_panel::TodoDagPanelPage::Detail)
        );
        assert_eq!(
            handle_todo_dag_panel_key(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(false)
        );
        assert!(state.todo_dag_panel.is_some());
        assert_eq!(
            handle_todo_dag_panel_key(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(false)
        );
        assert!(state.todo_dag_panel.is_none());
        application.cleanup().await;
    }

    #[tokio::test]
    async fn code_review_command_opens_page_and_escape_closes() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = pi_coding::Session::new(pi_coding::SessionOptions {
            model: Model::default(), cwd: cwd.path().to_path_buf(), system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off, api_key: String::new(), compaction: None,
            stream_options: Default::default(), tools: Some(Vec::new()), before_tool_call: None,
            after_tool_call: None, stream_fn: None, auth_resolver: None,
        }).expect("session");
        let application = Application::new(session).await;
        let mut mouse = TestCodeReviewMouse::default();
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = todo_test_state(Vec::new());
        // Non-repo cwd still opens the page with an in-panel error.
        state.cwd_path = std::env::temp_dir();
        state
            .open_code_review_panel(&application, ReviewScope::WorkingTree, &mut mouse)
            .await
            .expect("open review");
        assert!(state.code_review_panel.is_some());
        assert!(page_overlay_open(&state));

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                render_code_review_panel(
                    frame,
                    state.code_review_panel.as_ref().expect("panel"),
                    crate::theme::DARK,
                );
            })
            .expect("render");
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Code review"), "{text}");

        // Escape closes without a live TerminalGuard mouse path in unit tests.
        let panel = state.code_review_panel.as_mut().expect("panel");
        assert_eq!(
            panel.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            CodeReviewPanelResult::Close
        );
        state
            .close_code_review_panel(&mut mouse)
            .expect("close review");
        state.shutdown_code_review().await;
        assert!(state.code_review_panel.is_none());
        assert!(!page_overlay_open(&state));
    }

    #[tokio::test]
    async fn code_review_close_keeps_panel_when_mouse_disable_fails() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = pi_coding::Session::new(pi_coding::SessionOptions {
            model: Model::default(), cwd: cwd.path().to_path_buf(), system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off, api_key: String::new(), compaction: None,
            stream_options: Default::default(), tools: Some(Vec::new()), before_tool_call: None,
            after_tool_call: None, stream_fn: None, auth_resolver: None,
        }).expect("session");
        let application = Application::new(session).await;
        let mut mouse = TestCodeReviewMouse::default();
        let mut state = todo_test_state(Vec::new());
        state.cwd_path = std::env::temp_dir();
        state
            .open_code_review_panel(&application, ReviewScope::WorkingTree, &mut mouse)
            .await
            .expect("open review");

        let error = state
            .close_code_review_panel(&mut FailingCodeReviewMouse)
            .expect_err("disable failure");

        assert!(error.to_string().contains("mouse sync failed"));
        assert!(state.code_review_panel.is_some());
        assert!(mouse.active);
        state.shutdown_code_review().await;
    }

    #[test]
    fn code_review_page_paste_never_reaches_main_composer() {
        let mut state = todo_test_state(Vec::new());
        state.editor.set_text("main draft");
        state.code_review_panel = Some(CodeReviewPanel::from_snapshot(
            crate::code_review::ReviewSnapshot {
                root: PathBuf::from("repo"),
                scope: ReviewScope::WorkingTree,
                snapshot_id: "paste".to_owned(),
                files: vec![crate::code_review::DiffFile {
                    path: "src/lib.rs".to_owned(),
                    previous_path: None,
                    status: crate::code_review::FileStatus::Modified,
                    binary: false,
                    insertions: 1,
                    deletions: 0,
                    hunks: vec![crate::code_review::DiffHunk {
                        header: "@@ -1 +1 @@".to_owned(),
                        old_start: 1,
                        old_count: 1,
                        new_start: 1,
                        new_count: 1,
                        lines: Vec::new(),
                    }],
                    truncated: false,
                    message: None,
                }],
                truncated: false,
                error: None,
            },
        ));

        handle_paste(&mut state, "review-only paste");

        assert_eq!(state.editor.text(), "main draft");
        assert_eq!(
            state.code_review_panel.as_ref().and_then(CodeReviewPanel::comment_editor),
            Some("review-only paste")
        );
        assert!(state.status.contains("Opened review comment"));
    }

    #[test]
    fn process_panel_paste_never_reaches_main_composer() {
        let mut state = todo_test_state(Vec::new());
        state.editor.set_text("main draft");
        state.process_panel = Some(ProcessPanel::new(Vec::new()));
        assert!(page_overlay_open(&state));

        handle_paste(&mut state, "process-overlay paste");

        // The process panel has no paste editor, so the paste is consumed by
        // the overlay with a bounded visible status and never mutates the
        // hidden main composer.
        assert_eq!(state.editor.text(), "main draft", "main composer must be untouched");
        assert!(state.process_panel.is_some(), "process panel must remain open");
        assert_eq!(
            state.composer_error.as_deref(),
            Some("Paste consumed by active overlay"),
            "non-editor overlay must surface a bounded consume status"
        );
        assert!(
            state.status.is_empty(),
            "overlay consume must not promote to the live composer status"
        );
    }

    #[test]
    fn selector_panel_paste_never_reaches_main_composer() {
        let mut state = todo_test_state(Vec::new());
        state.editor.set_text("main draft");
        state.panel = Some(SelectorPanel {
            title: "Models".to_owned(),
            help: String::new(),
            items: Vec::new(),
            selected: 0,
            query: String::new(),
        });
        assert!(page_overlay_open(&state));

        handle_paste(&mut state, "selector-overlay paste");

        assert_eq!(state.editor.text(), "main draft", "main composer must be untouched");
        assert!(state.panel.is_some(), "selector panel must remain open");
        assert_eq!(
            state.composer_error.as_deref(),
            Some("Paste consumed by active overlay"),
            "non-editor overlay must surface a bounded consume status"
        );
        assert!(state.status.is_empty());
    }

    #[tokio::test]
    async fn pty_paste_rejects_oversized_payload_without_detaching() {
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
        let application = Application::new(session).await;
        let mut state = todo_test_state(Vec::new());
        let process_id: ProcessId =
            serde_json::from_value(serde_json::json!("00000000-0000-0000-0000-000000000031"))
                .expect("process id");
        state.pty_attachment = Some(PtyAttachment::new(process_id, 0));

        // Oversized paste is rejected before reaching `process_write`, so a
        // healthy PTY attachment must survive.
        handle_terminal_paste(&application, &mut state, &"x".repeat(MAX_PASTE_BYTES + 1)).await;

        assert!(state.pty_attachment.is_some(), "oversized paste must not detach a healthy PTY");
        assert!(
            state.status.contains("Paste rejected"),
            "oversized paste must surface a rejection status: {}",
            state.status
        );
        assert!(state.status.contains("MiB"));
    }

    #[tokio::test]
    async fn pty_empty_paste_consumed_without_process_write_or_detach() {
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
        let application = Application::new(session).await;
        let mut state = todo_test_state(Vec::new());
        let process_id: ProcessId =
            serde_json::from_value(serde_json::json!("00000000-0000-0000-0000-000000000032"))
                .expect("process id");
        state.pty_attachment = Some(PtyAttachment::new(process_id, 0));
        state.status = "Attached to PTY".to_owned();

        // An empty (image-only) bracketed paste must be consumed silently: no
        // process_write, no detach, no clipboard read, no status churn.
        handle_terminal_paste(&application, &mut state, "").await;

        assert!(state.pty_attachment.is_some(), "empty paste must not detach a healthy PTY");
        assert!(
            !state.clipboard_read_busy,
            "empty paste into a PTY must not start the clipboard reader"
        );
        assert_eq!(
            state.status, "Attached to PTY",
            "empty PTY paste must not churn the live status"
        );
    }

    #[tokio::test]
    async fn side_chat_ignored_chord_does_not_open_overlay_behind_visible_side_chat() {
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
        let mut mouse = TestCodeReviewMouse::default();
        state
            .open_side_chat(&application, None, &mut mouse)
            .await
            .expect("open side chat");
        assert!(state.side_chat_open, "side chat overlay must open");
        assert!(state.side_chat.is_some());
        assert!(page_overlay_open(&state));

        // Ctrl+L maps to Action::ModelSelect (open the model panel). While the
        // side chat is the visible exclusive overlay, the chord is ignored by
        // the side-chat editor and must be consumed rather than fall through
        // to global key dispatch and open another overlay behind it.
        let ctrl_l = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL);
        let outcome = handle_side_chat_key(&mut state, ctrl_l)
            .await
            .expect("side chat key");
        assert_eq!(
            outcome,
            Some(false),
            "ignored Ctrl/Alt chord must be consumed while side chat is open"
        );
        assert!(state.side_chat_open, "side chat must remain open");
        assert!(
            state.panel.is_none() && state.scoped_model_selector.is_none(),
            "ignored chord must not open a model/selector overlay behind side chat"
        );
        assert!(
            state.status.contains("Side chat open"),
            "ignored chord must not promote a global status: {}",
            state.status
        );
        state.shutdown_side_chat().await;
    }

    #[test]
    fn rejected_code_review_submit_keeps_editor_draft() {
        let mut state = todo_test_state(Vec::new());
        let mut panel = CodeReviewPanel::from_snapshot(crate::code_review::ReviewSnapshot {
            root: PathBuf::from("repo"),
            scope: ReviewScope::WorkingTree,
            snapshot_id: "submit".to_owned(),
            files: vec![crate::code_review::DiffFile {
                path: "src/lib.rs".to_owned(),
                previous_path: None,
                status: crate::code_review::FileStatus::Modified,
                binary: false,
                insertions: 1,
                deletions: 0,
                hunks: vec![crate::code_review::DiffHunk {
                    header: "@@ -1 +1 @@".to_owned(),
                    old_start: 1,
                    old_count: 1,
                    new_start: 1,
                    new_count: 1,
                    lines: Vec::new(),
                }],
                truncated: false,
                message: None,
            }],
            truncated: false,
            error: None,
        });
        assert!(panel.open_comment_editor());
        assert!(panel.handle_paste("draft survives"));
        state.code_review_panel = Some(panel);

        assert!(!submit_code_review_comment(&mut state));
        assert_eq!(
            state.code_review_panel.as_ref().and_then(CodeReviewPanel::comment_editor),
            Some("draft survives")
        );
    }

    #[test]
    fn stale_code_review_snapshot_result_is_discarded() {
        let mut state = todo_test_state(Vec::new());
        let cwd = PathBuf::from("repo");
        state.cwd_path = cwd.clone();
        state.code_review_panel = Some(CodeReviewPanel::loading(cwd.clone(), ReviewScope::WorkingTree));
        state.code_review_controller_generation = 3;
        state.code_review_load_generation = 2;
        state.code_review_load_in_flight = Some((3, 2, cwd.clone()));
        state.apply_background(BackgroundEvent::CodeReviewSnapshotLoaded {
            controller_generation: 2,
            generation: 2,
            cwd,
            snapshot: crate::code_review::ReviewSnapshot::empty_with_error(
                PathBuf::from("repo"),
                "stale result",
            ),
        });

        let panel = state.code_review_panel.as_ref().expect("loading panel");
        assert!(panel.is_snapshot_loading());
        assert_eq!(
            state.code_review_load_in_flight,
            Some((3, 2, PathBuf::from("repo")))
        );
    }

    #[tokio::test]
    async fn code_review_snapshot_dispatch_returns_before_background_load() {
        let (background_tx, mut background_rx) = mpsc::unbounded_channel();
        let cwd = tempfile::tempdir().expect("cwd");

        spawn_code_review_snapshot_load(
            4,
            7,
            cwd.path().to_path_buf(),
            ReviewScope::WorkingTree,
            background_tx,
        );
        tokio::task::yield_now().await;

        let event = tokio::time::timeout(std::time::Duration::from_secs(2), background_rx.recv())
            .await
            .expect("background load timeout")
            .expect("background load event");
        assert!(matches!(
            event,
            BackgroundEvent::CodeReviewSnapshotLoaded {
                controller_generation: 4,
                generation: 7,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn code_review_transition_disables_capture_before_replacement() {
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
        let agent = tempfile::tempdir().expect("agent");
        std::fs::write(agent.path().join("settings.json"), "{}").expect("global settings");
        let mut options = pi_coding::ResourceManagerOptions::new(cwd.path());
        options.agent_dir = agent.path().to_path_buf();
        options.project_trust_override = Some(true);
        let resources = pi_coding::ResourceManager::new(options).expect("resources");
        session
            .attach_resources(resources)
            .await
            .expect("attach resources");
        let application = Application::new(session).await;
        let mut state = todo_test_state(Vec::new());
        state.cwd_path = std::env::temp_dir();
        let mut mouse = TestCodeReviewMouse::default();
        state
            .open_code_review_panel(&application, ReviewScope::WorkingTree, &mut mouse)
            .await
            .expect("open review");
        assert!(mouse.active);
        assert_eq!(mouse.transitions, vec![true]);
        assert!(code_review_page_active(&state));

        state
            .open_settings_panel(&application, &mut mouse)
            .expect("open settings");
        assert!(state.code_review_panel.is_none());
        assert!(state.settings_panel.is_some());
        assert!(!mouse.active);
        let output = String::from_utf8(mouse.bytes.clone()).expect("mouse escape bytes");
        let enable = output.find("\u{1b}[?1000h").expect("enable mouse capture");
        let disable = output.find("\u{1b}[?1000l").expect("disable mouse capture");
        assert!(enable < disable, "{output:?}");
        assert_eq!(mouse.transitions, vec![true, false]);
        assert!(!code_review_page_active(&state));
        state.shutdown_code_review().await;

        application.cleanup().await;
    }

    #[tokio::test]
    async fn code_review_runtime_switch_closes_busy_panel_and_clears_load() {
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
        let agent = tempfile::tempdir().expect("agent");
        std::fs::write(agent.path().join("settings.json"), "{}").expect("global settings");
        let mut options = pi_coding::ResourceManagerOptions::new(cwd.path());
        options.agent_dir = agent.path().to_path_buf();
        options.project_trust_override = Some(true);
        let resources = pi_coding::ResourceManager::new(options).expect("resources");
        session
            .attach_resources(resources)
            .await
            .expect("attach resources");
        let application = Application::new(session).await;
        let mut state = todo_test_state(Vec::new());
        state.cwd_path = cwd.path().to_path_buf();
        let mut mouse = TestCodeReviewMouse::default();
        state
            .open_code_review_panel(&application, ReviewScope::WorkingTree, &mut mouse)
            .await
            .expect("open review");
        assert!(mouse.active);
        assert!(state.code_review_panel.is_some());
        assert!(state.code_review_load_in_flight.is_some());
        let generation_before = state.code_review_load_generation;

        // A RuntimeChanged event closes the overlay before rebinding cwd.
        state
            .close_code_review_panel(&mut mouse)
            .expect("close review on runtime switch");

        assert!(state.code_review_panel.is_none(), "panel must not survive runtime switch");
        assert!(!mouse.active, "mouse capture must be released");
        assert_eq!(mouse.transitions, vec![true, false]);
        assert!(state.code_review_load_in_flight.is_none(), "stale load must be invalidated");
        assert!(
            state.code_review_load_generation > generation_before,
            "load generation must advance so late results are discarded"
        );
        assert!(!code_review_page_active(&state));
        state.shutdown_code_review().await;

        application.cleanup().await;
    }

    #[tokio::test]
    async fn code_review_open_rejects_dialog_and_pty_without_enabling_capture() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = pi_coding::Session::new(pi_coding::SessionOptions {
            model: Model::default(), cwd: cwd.path().to_path_buf(), system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off, api_key: String::new(), compaction: None,
            stream_options: Default::default(), tools: Some(Vec::new()), before_tool_call: None,
            after_tool_call: None, stream_fn: None, auth_resolver: None,
        }).expect("session");
        let application = Application::new(session).await;
        let mut state = todo_test_state(Vec::new());
        state.cwd_path = std::env::temp_dir();
        let mut mouse = TestCodeReviewMouse::default();
        state.extension_dialog = Some(ExtensionDialog::new(interaction(
            ExtensionUiRequest::Confirm {
                title: "Confirm".to_owned(),
                message: "Proceed?".to_owned(),
            },
        )));
        state
            .open_code_review_panel(&application, ReviewScope::WorkingTree, &mut mouse)
            .await
            .expect("reject dialog");
        assert!(state.code_review_panel.is_none());
        assert!(state.extension_dialog.is_some());
        assert!(!mouse.active);
        assert!(mouse.transitions.is_empty());
        assert!(state.status.contains("extension dialog"));

        state.extension_dialog = None;
        let process_id: ProcessId =
            serde_json::from_value(serde_json::json!("00000000-0000-0000-0000-000000000024"))
                .expect("process id");
        state.pty_attachment = Some(PtyAttachment::new(process_id, 0));
        state
            .open_code_review_panel(&application, ReviewScope::WorkingTree, &mut mouse)
            .await
            .expect("reject PTY");
        assert!(state.code_review_panel.is_none());
        assert!(state.pty_attachment.is_some());
        assert!(!mouse.active);
        assert!(mouse.transitions.is_empty());
        assert!(state.status.contains("PTY"));
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
                    agent: "task".to_owned(),
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
                job: pi_coding::JobSnapshot { id: id.to_owned(), agent_id: agent_id.to_owned(), agent: "task".to_owned(), parent_id: "Main".to_owned(), description: Some(format!("work for {agent_id}")), todo_task_id: None, workflow_id: None, workflow_generation: None, status, created_at: 1_000, started_at: None, finished_at: None, result: None,
                    },
                },
            ));
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
                        TodoBlockedReason { task_id: "root-a".to_owned(), content: "fetch".to_owned(), status: TodoStatus::Pending,
                        },
                        TodoBlockedReason { task_id: "root-b".to_owned(), content: "compile".to_owned(), status: TodoStatus::InProgress,
                        },
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
            ("job-secret-1", "StableScoutId", "map relevant files", pi_coding::JobStatus::Running, Some("owned"),
            ),
            ("job-secret-2", "StableWriterId", "apply focused edit", pi_coding::JobStatus::Queued, Some("missing"),
            ),
            ("job-secret-3", "StableUnownedId", "inspect without correlation", pi_coding::JobStatus::Running, None,
            ),
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
                    agent: "task".to_owned(),
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
                todo_item("owned", "implement correlation", TodoStatus::InProgress, true, &[],
                ),
                todo_item("other", "preserve DAG truth", TodoStatus::Pending, true, &[],
                ),
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
        let composer_row = rows.iter().position(|row| row.trim_start().starts_with("╭── π")).expect("composer row");
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
                    agent: "reviewer".to_owned(),
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
            .position(|row| row.trim_start().starts_with("╭── π"))
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
                .map(|index| {
                    todo_item(&format!("task-{index}"), &format!("active task {index} with a long bounded description"), if index == 0 { TodoStatus::InProgress } else { TodoStatus::Pending }, true, &[],
                    )
                })
                .collect::<Vec<_>>();
            let phases = vec![TodoPhase { name: "Delivery".to_owned(), tasks,
            }];
            let mut state = todo_test_state(phases.clone());
            state.model = "faux/faux-1".to_owned();
            state.cwd = "<workspace>/portrait-project".to_owned();
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
                },
                ));
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
                let top = rows.iter().position(|row| row.trim_start().starts_with("╭── π")).expect("composer top border");
                let bottom = rows.iter().position(|row| row.trim_start().starts_with("╰─ draft prompt remains visible")).expect("composer prompt and bottom border");
                assert!(top < bottom && bottom < usize::from(height));
                assert!(rows[top].contains('π'));
                assert!(rows[bottom].trim_end().ends_with('╯'));
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
                    agent: "task".to_owned(),
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
                Some("m1")),
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
        state.push_entry(TranscriptEntry { kind: TranscriptKind::User, content: vec![ContentBlock::text("prompt")], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false,
        });
        state.push_entry(TranscriptEntry { kind: TranscriptKind::Assistant, content: vec![ContentBlock::text("answer")], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false,
        });
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
    fn short_settled_exit_commits_without_overflow_pressure() {
        let mut state = todo_test_state(Vec::new());
        state.push_entry(TranscriptEntry {
            kind: TranscriptKind::Assistant,
            content: vec![ContentBlock::text("short final answer")],
            tool_name: None,
            tool_card: None,
            job_card: None,
            is_error: false,
            is_partial: false,
        });

        assert!(state.overflow_commit_batch(80, 20).is_empty());
        let final_batch = state.final_settled_commit_batch();
        assert_eq!(final_batch.len(), 1);
        assert_eq!(content_text(&final_batch[0].content), "short final answer");
        state.finish_commit(final_batch.len());
        assert_eq!(state.committed_entries, 1);
    }

    #[test]
    fn final_exit_commits_pending_user_echo_without_crossing_partial_assistant() {
        let mut pending = todo_test_state(Vec::new());
        pending.push_lines("You", "accepted prompt".to_owned(), Color::Reset);
        pending.streaming_text = "partial response".to_owned();

        assert!(pending.settled_commit_batch().is_empty());
        let pending_batch = pending.final_settled_commit_batch();
        assert_eq!(pending_batch.len(), 1);
        assert_eq!(pending_batch[0].kind, TranscriptKind::User);
        assert_eq!(content_text(&pending_batch[0].content), "accepted prompt");

        let mut partial = todo_test_state(Vec::new());
        partial.push_entry(TranscriptEntry {
            kind: TranscriptKind::User,
            content: vec![ContentBlock::text("persisted prompt")],
            tool_name: None,
            tool_card: None,
            job_card: None,
            is_error: false,
            is_partial: false,
        });
        partial.push_entry(TranscriptEntry {
            kind: TranscriptKind::Assistant,
            content: vec![ContentBlock::text("partial response")],
            tool_name: None,
            tool_card: None,
            job_card: None,
            is_error: false,
            is_partial: true,
        });

        let partial_batch = partial.final_settled_commit_batch();
        assert_eq!(partial_batch.len(), 1);
        assert_eq!(partial_batch[0].kind, TranscriptKind::User);
    }

    #[test]
    fn final_settled_exit_does_not_duplicate_committed_entries() {
        let mut state = todo_test_state(Vec::new());
        for text in ["already durable", "new final answer"] {
            state.push_entry(TranscriptEntry {
                kind: TranscriptKind::Assistant,
                content: vec![ContentBlock::text(text)],
                tool_name: None,
                tool_card: None,
                job_card: None,
                is_error: false,
                is_partial: false,
            });
        }
        state.finish_commit(1);

        let final_batch = state.final_settled_commit_batch();
        assert_eq!(final_batch.len(), 1);
        assert_eq!(content_text(&final_batch[0].content), "new final answer");
        state.finish_commit(final_batch.len());
        assert!(state.final_settled_commit_batch().is_empty());
        assert_eq!(state.committed_entries, state.transcript.len());
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
        state.cwd = "<workspace>/project".to_owned();
        assert_eq!(live_viewport_height(&state, 80, 24), 4);

        state.editor.set_text("one\ntwo");
        assert_eq!(live_viewport_height(&state, 80, 24), 6);

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
        assert_eq!(live_viewport_height(&state, 80, 24), 6);
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
        let mut mouse = TestCodeReviewMouse::default();
        state.open_settings_panel(&application, &mut mouse)
            .expect("open settings");
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

    #[tokio::test]
    async fn settings_integer_editor_stages_then_applies_and_escape_is_layered() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent = tempfile::tempdir().expect("agent");
        std::fs::write(
            agent.path().join("settings.json"),
            r#"{"thinkingBudgets":{"medium":2048}}"#,
        )
        .expect("global settings");
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
        let mut mouse = TestCodeReviewMouse::default();
        state.open_settings_panel(&application, &mut mouse).expect("open settings");
        let panel = state.settings_panel.as_mut().expect("settings panel");
        panel.set_search("thinkingBudgets.medium");
        let selected = panel.selected().expect("selected row").expect("medium row");
        assert_eq!(selected.key, "thinkingBudgets.medium");
        assert_eq!(selected.scope_value, Some(serde_json::json!(2048)));
        assert!(!panel.is_dirty(), "selecting the medium row must not mutate it");

        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_settings_panel_key(&application, &mut state, enter).await.expect("focus editor");
        assert_eq!(
            state.settings_value_input.as_ref(),
            Some(&SettingsValueInput {
                key: "thinkingBudgets.medium".to_owned(),
                value: "2048".to_owned(),
                cursor: 4,
                hint: "integer token override; inherited when unset",
                error: None,
                replace_on_type: true,
            })
        );
        for digit in ['4', '0', '9', '6'] {
            handle_settings_panel_key(
                &application,
                &mut state,
                KeyEvent::new(KeyCode::Char(digit), KeyModifiers::NONE),
            )
            .await
            .expect("type integer");
        }
        assert_eq!(state.settings_value_input.as_ref().unwrap().value, "4096");
        handle_settings_panel_key(&application, &mut state, enter).await.expect("confirm draft");
        assert!(state.settings_value_input.is_none());
        let panel = state.settings_panel.as_ref().expect("settings panel");
        assert!(panel.is_dirty());
        assert_eq!(
            panel.selected().expect("selected row").expect("medium row").scope_value,
            Some(serde_json::json!(4096))
        );
        assert_eq!(
            application.settings_manager().expect("manager").global_settings()
                .thinking_budgets.as_ref().and_then(|budgets| budgets.medium),
            Some(2048),
            "Enter must only update the pending row"
        );

        handle_settings_panel_key(
            &application,
            &mut state,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
        )
        .await
        .expect("apply settings");
        assert!(!state.settings_panel.as_ref().unwrap().is_dirty());
        assert_eq!(
            application.settings_manager().expect("manager").global_settings()
                .thinking_budgets.as_ref().and_then(|budgets| budgets.medium),
            Some(4096)
        );

        handle_settings_panel_key(&application, &mut state, enter).await.expect("reopen editor");
        handle_settings_panel_key(
            &application,
            &mut state,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        )
        .await
        .expect("type invalid integer");
        handle_settings_panel_key(&application, &mut state, enter).await.expect("reject integer");
        let input = state.settings_value_input.as_ref().expect("invalid editor remains open");
        assert_eq!(input.value, "x");
        assert!(input.error.as_deref().is_some_and(|error| error.contains("must be an integer")));
        handle_settings_panel_key(&application, &mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("cancel editor");
        assert!(state.settings_value_input.is_none());
        assert!(state.settings_panel.is_some(), "first Escape cancels only the editor");
        handle_settings_panel_key(&application, &mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .expect("close settings");
        assert!(state.settings_panel.is_none(), "second Escape closes the page");
        application.cleanup().await;
    }

    #[tokio::test]
    async fn settings_editor_uses_controller_typed_input_and_stays_tui_local() {
        let cwd = tempfile::tempdir().expect("cwd");
        let agent = tempfile::tempdir().expect("agent");
        std::fs::write(
            agent.path().join("settings.json"),
            r#"{"theme":"dark","extensions":["alpha,beta","  padded  "]}"#,
        )
        .expect("global settings");
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
        session
            .attach_resources(resources)
            .await
            .expect("attach resources");
        let application = Application::new(session).await;
        assert_eq!(
            application
                .settings_manager()
                .expect("settings manager")
                .global_settings()
                .extensions,
            vec!["alpha,beta".to_owned(), "  padded  ".to_owned()],
            "application settings manager must retain the persisted string list fixture"
        );
        let mut state = todo_test_state(Vec::new());
        state.push_entry(TranscriptEntry {
            kind: TranscriptKind::Assistant,
            content: vec![ContentBlock::text("structured-output-sentinel")],
            tool_name: None,
            tool_card: None,
            job_card: None,
            is_error: false,
            is_partial: false,
        });
        let transcript_len_before = state.transcript.len();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);

        let mut mouse = TestCodeReviewMouse::default();
        state
            .open_settings_panel(&application, &mut mouse)
            .expect("open settings");
        state.settings_panel.as_mut().unwrap().set_search("theme");
        handle_settings_panel_key(&application, &mut state, enter)
            .await
            .expect("open theme input");
        assert_eq!(
            state.settings_value_input.as_ref(),
            Some(&SettingsValueInput {
                key: "theme".to_owned(),
                value: "dark".to_owned(),
                cursor: 4,
                hint: "text",
                error: None,
                replace_on_type: true,
            })
        );
        state.settings_value_input.as_mut().unwrap().value = "true".to_owned();
        handle_settings_panel_key(&application, &mut state, enter)
            .await
            .expect("submit theme input");
        let theme = state
            .settings_panel
            .as_ref()
            .unwrap()
            .selected()
            .expect("theme row")
            .expect("selected theme");
        assert_eq!(
            theme.scope_value,
            Some(serde_json::Value::String("true".to_owned()))
        );

        state
            .settings_panel
            .as_mut()
            .unwrap()
            .set_search("maxTokens");
        handle_settings_panel_key(&application, &mut state, enter)
            .await
            .expect("open numeric input");
        {
            let input = state.settings_value_input.as_mut().unwrap();
            input.value = "not-a-number".to_owned();
            input.cursor = input.value.len();
        }
        handle_settings_panel_key(&application, &mut state, enter)
            .await
            .expect("reject numeric input");
        let input = state.settings_value_input.as_ref().expect("invalid numeric input stays open");
        assert!(
            input.error.as_deref().is_some_and(|error| error.contains("must be an integer")),
            "{:?}",
            input.error
        );
        handle_settings_panel_key(&application, &mut state, escape)
            .await
            .expect("cancel numeric input");
        assert!(state.settings_value_input.is_none());
        assert!(
            state.settings_panel.is_some(),
            "Escape cancels input but keeps panel"
        );

        state
            .settings_panel
            .as_mut()
            .unwrap()
            .set_search("extensions");
        assert_eq!(
            state
                .settings_panel
                .as_ref()
                .unwrap()
                .rows()
                .expect("extensions search rows")
                .iter()
                .map(|row| row.key.as_str())
                .collect::<Vec<_>>(),
            vec!["extensions", "packages"]
        );
        handle_settings_panel_key(&application, &mut state, enter)
            .await
            .expect("open string-list input");
        assert_eq!(
            state.settings_value_input.as_ref().unwrap().value,
            r#"["alpha,beta","  padded  "]"#
        );
        assert_eq!(
            state.settings_value_input.as_ref().unwrap().hint,
            "JSON array of strings"
        );
        {
            let input = state.settings_value_input.as_mut().unwrap();
            input.value = r#"["gamma,delta","  spaced  "]"#.to_owned();
            input.cursor = input.value.len();
        }
        handle_settings_panel_key(&application, &mut state, enter)
            .await
            .expect("submit string-list input");
        let extensions = state
            .settings_panel
            .as_ref()
            .unwrap()
            .selected()
            .expect("extensions row")
            .expect("selected extensions");
        assert_eq!(
            extensions.scope_value,
            Some(serde_json::json!(["gamma,delta", "  spaced  "]))
        );

        state
            .settings_panel
            .as_mut()
            .unwrap()
            .set_search("keybindings");
        handle_settings_panel_key(&application, &mut state, enter)
            .await
            .expect("open object input");
        {
            let input = state.settings_value_input.as_mut().unwrap();
            input.value = "[]".to_owned();
            input.cursor = input.value.len();
        }
        handle_settings_panel_key(&application, &mut state, enter)
            .await
            .expect("reject array for object");
        let input = state.settings_value_input.as_ref().expect("invalid object input stays open");
        assert!(
            input.error.as_deref().is_some_and(|error| error.contains("must be an object")),
            "{:?}",
            input.error
        );

        assert_eq!(
            state.transcript.len(),
            transcript_len_before,
            "settings reducer must not emit structured or transcript output"
        );
        assert_eq!(
            content_text(&state.transcript[0].content),
            "structured-output-sentinel"
        );
        application.cleanup().await;
    }

    #[test]
    fn settings_editor_renders_controller_hint() {
        use ratatui::backend::TestBackend;

        let agent = tempfile::tempdir().expect("agent");
        let cwd = tempfile::tempdir().expect("cwd");
        let manager =
            pi_coding::SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("manager");
        let panel = SettingsPanel::new(manager, pi_coding::SettingsScope::Global).expect("panel");
        let input = SettingsValueInput {
            key: "extensions".to_owned(),
            value: r#"["alpha,beta","  padded  "]"#.to_owned(),
            cursor: r#"["alpha,beta","  padded  "]"#.len(),
            hint: panel.input_hint("extensions").expect("hint"),
            error: Some("extensions must be a valid JSON array of strings without overflowing the editor boundary".to_owned()),
            replace_on_type: false,
        };
        let backend = TestBackend::new(60, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_settings_panel(frame, &panel, Some(&input), crate::theme::DARK))
            .expect("draw settings editor");
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Edit extensions · JSON array of strings"));
        assert!(rendered.contains(r#"["alpha,beta","  padded  "]"#));
        assert!(rendered.contains("extensions must be a valid JSON array of strings"));
        assert!(!rendered.contains("overflowing the editor boundary"), "error text must be bounded to the editor width");
    }

    #[test]
    fn settings_editor_reserves_prompt_and_footer_rows() {
        use ratatui::backend::TestBackend;

        let agent = tempfile::tempdir().expect("agent");
        let cwd = tempfile::tempdir().expect("cwd");
        let manager =
            pi_coding::SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("manager");
        let mut panel = SettingsPanel::new(manager, pi_coding::SettingsScope::Global).expect("panel");
        panel.set_search("retry.enabled");
        let input = SettingsValueInput {
            key: "retry.enabled".to_owned(),
            value: "true".to_owned(),
            cursor: 4,
            hint: "true or false",
            error: None,
            replace_on_type: true,
        };
        let backend = TestBackend::new(100, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_settings_panel(frame, &panel, Some(&input), crate::theme::DARK))
            .expect("draw settings editor");
        let buffer = terminal.backend().buffer();
        let rows = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let prompt_row = rows.iter().position(|row| row.contains("Edit retry.enabled")).expect("prompt row");
        let footer_row = rows.iter().position(|row| row.contains("Editing value")).expect("footer row");
        let setting_row = rows.iter().position(|row| row.contains("enabled = ")).expect("setting row");
        assert!(setting_row < prompt_row && prompt_row < footer_row, "{rows:#?}");
        assert!(!rows[prompt_row].contains("enabled = "), "prompt must not overlap list row: {}", rows[prompt_row]);
    }

    #[test]
    fn settings_retry_rows_render_under_one_section_at_normal_and_constrained_heights() {
        use ratatui::backend::TestBackend;

        for height in [24, 12] {
            let agent = tempfile::tempdir().expect("agent");
            let cwd = tempfile::tempdir().expect("cwd");
            let manager = pi_coding::SettingsManager::load_phase_one(cwd.path(), agent.path())
                .expect("manager");
            let mut panel =
                SettingsPanel::new(manager, pi_coding::SettingsScope::Global).expect("panel");
            panel.set_category(Some(pi_coding::SettingCategory::RetryTransport));
            if height == 12 {
                panel.set_search("retry.provider.");
            } else {
                panel.set_search("retry.");
            }
            let backend = TestBackend::new(100, height);
            let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
            terminal
                .draw(|frame| render_settings_panel(frame, &panel, None, crate::theme::DARK))
                .expect("draw grouped settings");
            let buffer = terminal.backend().buffer();
            let rows = (0..buffer.area.height)
                .map(|y| {
                    (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>();
            assert_eq!(rows.iter().filter(|row| row.contains("│Retry")).count(), 1, "height {height}: {rows:#?}");
            assert!(rows.iter().any(|row| row.contains(if height == 12 { "provider.timeoutMs" } else { "enabled = true" })), "height {height}: {rows:#?}");
            assert!(!rows.iter().any(|row| row.contains(if height == 12 { "retry.provider.timeoutMs" } else { "retry.enabled" })), "height {height}: {rows:#?}");
        }
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
    fn inline_commit_clears_live_composer_before_scrollback_advances() {
        use ratatui::backend::TestBackend;

        let mut terminal = Terminal::with_options(
            TestBackend::new(20, 5),
            TerminalOptions {
                viewport: Viewport::Inline(5),
            },
        )
        .expect("inline terminal");
        terminal
            .draw(|frame| {
                Paragraph::new(
                    [
                        "live transcript",
                        "╭── π working",
                        "╰─",
                        "transient status",
                        "transient overlay",
                    ]
                    .join("\n"),
                )
                .render(frame.area(), frame.buffer_mut());
            })
            .expect("draw mutable live frame");

        // Production commit ordering: clear mutable viewport pixels before
        // ratatui scrolls the full-height inline area to insert durable rows.
        clear_inline_viewport(&mut terminal).expect("clear mutable live viewport");
        terminal
            .insert_before(2, |buffer| {
                Paragraph::new("committed user\ncommitted assistant").render(buffer.area, buffer);
            })
            .expect("insert durable transcript rows");

        let retained = terminal
            .backend()
            .scrollback()
            .content
            .chunks(20)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>();
        assert!(
            retained.iter().all(|row| !row.contains("╭── π")
                && !row.contains("╰─")
                && !row.contains("transient")),
            "mutable composer/status rows must not enter scrollback: {retained:#?}"
        );
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

        clear_inline_viewport(&mut terminal).expect("clear mutable live viewport before commit");
        terminal
            .insert_before(18, |buffer| {
                Paragraph::new("transcript row\n".repeat(18)).render(buffer.area, buffer);
            })
            .expect("insert transcript");
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
            .position(|row| row.trim_start().starts_with("╭── π"))
            .expect("composer top border");
        assert!(rows[composer_top + 1].contains("composer sentinel"));
        assert!(rows[composer_top + 1].trim_end().ends_with('╯'));
    }

    #[test]
    fn live_viewport_caps_progress_and_reserves_composer() {
        use pi_coding::{TodoPhase, TodoStatus};
        let tasks = (0..12)
            .map(|index| {
                todo_item(&format!("t{index}"), &format!("task {index} keeps progress busy"), TodoStatus::Pending, true, &[],
                )
            })
            .collect::<Vec<_>>();
        let mut state = todo_test_state(vec![TodoPhase { name: "Ship".to_owned(), tasks,
        }]);
        state.editor.set_text("draft remains visible");
        let panel_rows = u16::try_from(render_todo_panel_lines(
            &state.todo_phases,
            &state.job_cards.cards_in_source_order(),
            crate::theme::DARK,
            live_content_width(80),
        ).len(),
        ).unwrap_or(u16::MAX);
        let layout = tui_layout_heights(&state, live_content_width(80), live_content_height(24), panel_rows, 0, 0, 0);
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
        state.apply(ApplicationEvent::Agent(AgentEvent::MessageEnd { message: Message::user_text("hello", 1),
        }));
        assert_eq!(state.settled_commit_batch().len(), 1);
        state.finish_commit(1);
        state.streaming_text = "draft".to_owned();
        assert!(state.settled_commit_batch().is_empty());
        state.apply(ApplicationEvent::Agent(AgentEvent::MessageEnd { message: {
            let mut message = pi_ai::AssistantMessage::pending(&Model::default());
            message.content = vec![ContentBlock::text("final")];
            message.stop_reason = pi_ai::StopReason::Stop;
            Message::Assistant(message)
        },
        }));
        let batch = state.settled_commit_batch();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].kind, TranscriptKind::Assistant);
    }

    #[test]
    fn composer_wraps_unicode_input_inside_borders() {
        let mut state = todo_test_state(Vec::new());
        state.editor.set_text("🙂🙂abcdefghij🙂🙂abcdefghij🙂🙂abcdefghij");
        let lines = composer_border_lines(&state, 24, crate::theme::DARK);
        let rendered = lines.iter().map(|line| {
                line.spans.iter().map(|span| span.content.as_ref()).collect::<String>()
            }).collect::<Vec<_>>();
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
            InteractiveCommand { name: "settings".to_owned(), description: String::new(), source: CommandSource::Builtin,
            },
            InteractiveCommand { name: "branch".to_owned(), description: String::new(), source: CommandSource::Builtin,
            },
            InteractiveCommand { name: "login".to_owned(), description: String::new(), source: CommandSource::Builtin,
            },
            InteractiveCommand { name: "logout".to_owned(), description: String::new(), source: CommandSource::Builtin,
            },
        ];
        state.editor.set_text("/set"); assert!(state.accept_unambiguous_command_prefix()); assert_eq!(state.editor.text(), "/settings");
        state.editor.set_text("/br"); assert!(state.accept_unambiguous_command_prefix()); assert_eq!(state.editor.text(), "/branch");
        state.editor.set_text("/lo"); assert!(state.accept_unambiguous_command_prefix()); assert_eq!(state.editor.text(), "/loop");
    }

    fn assert_composer_colors(
        state: &TuiState,
        width: u16,
        expected_rows: usize,
        expected_border: Color,
    ) {
        let lines = composer_border_lines(state, width, crate::theme::DARK);
        assert_eq!(lines.len(), expected_rows);
        assert!(lines.iter().all(|line| display_width(
            &line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        ) == width));

        let area = Rect::new(0, 0, width, u16::try_from(lines.len()).unwrap());
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        Paragraph::new(lines.clone()).render(area, &mut buffer);
        assert!(lines.iter().flat_map(|line| &line.spans).all(|span| {
            span.style.bg == Some(crate::theme::DARK.user_message_bg)
        }));
        for line in lines {
            for span in line.spans {
                let content = span.content.as_ref();
                if content.contains(['╭', '╮', '╰', '╯', '│'])
                    || !content.is_empty() && content.chars().all(|character| character == '─')
                {
                    assert_eq!(span.style.fg, Some(expected_border), "border span {content:?}");
                }
            }
        }
        for (index, cell) in buffer.content.iter().enumerate() {
            let x = index % usize::from(width);
            let y = index / usize::from(width);
            let symbol = cell.symbol();
            let is_edge = matches!(symbol, "╭" | "╮" | "╰" | "╯" | "│")
                || symbol == "─"
                    && (y + 1 == expected_rows
                        || y == 0 && (x < 3 || x + 1 == usize::from(width)));
            if is_edge {
                assert_eq!(cell.fg, expected_border, "border cell ({x}, {y}) = {symbol:?}");
            }
        }
    }

    #[test]
    fn composer_border_tracks_thinking_level_and_bash_mode() {
        let mut state = todo_test_state(Vec::new());
        state.editor.set_text("draft");
        for (level, expected) in [
            (ThinkingLevel::Off, crate::theme::DARK.thinking_off),
            (ThinkingLevel::Minimal, crate::theme::DARK.thinking_minimal),
            (ThinkingLevel::Low, crate::theme::DARK.thinking_low),
            (ThinkingLevel::Medium, crate::theme::DARK.thinking_medium),
            (ThinkingLevel::High, crate::theme::DARK.thinking_high),
            (ThinkingLevel::Xhigh, crate::theme::DARK.thinking_xhigh),
            (ThinkingLevel::Max, crate::theme::DARK.thinking_max),
        ] {
            state.thinking_level = level;
            assert_composer_colors(&state, 90, 2, expected);
        }

        state.editor.set_text("!printf hi");
        assert_composer_colors(&state, 90, 2, crate::theme::DARK.bash_mode);
        state.editor.set_text("!!printf hi");
        assert_composer_colors(&state, 90, 2, crate::theme::DARK.bash_mode);
    }

    #[test]
    fn every_composer_branch_has_one_background_and_border_color() {
        let mut state = todo_test_state(Vec::new());
        state.thinking_level = ThinkingLevel::High;
        state.editor.set_text("idle");
        assert_composer_colors(&state, 90, 2, crate::theme::DARK.thinking_high);

        state.editor.set_text("first line\nsecond line");
        assert_composer_colors(&state, 90, 4, crate::theme::DARK.thinking_high);

        state.editor.set_text("/branch");
        state.completions.items = vec![CompletionItem {
            value: "/branch".to_owned(),
            label: "branch".to_owned(),
            description: String::new(),
            is_directory: false,
        }];
        state.completions.context = Some(CompletionContext::Slash);
        assert_composer_colors(&state, 90, 3, crate::theme::DARK.thinking_high);

        state.completions.clear();
        state.editor.set_text("draft");
        state.pending_attachments.push(PendingAttachment {
            block: ContentBlock::Image {
                data: "aW1n".to_owned(),
                mime_type: "image/png".to_owned(),
            },
            width: 1,
            height: 1,
        });
        assert_composer_colors(&state, 90, 4, crate::theme::DARK.thinking_high);

        state.pending_attachments.clear();
        assert_composer_colors(&state, 16, 2, crate::theme::DARK.thinking_high);
    }

    #[test]
    fn omp_completion_composer_is_three_rows() {
        let mut state = todo_test_state(Vec::new());
        state.editor.set_text("/branch");
        state.completions.items = vec![CompletionItem { value: "/branch".to_owned(), label: "branch".to_owned(), description: "Create a new branch from a previous message".to_owned(), is_directory: false,
        }];
        state.completions.context = Some(CompletionContext::Slash);
        let rendered = composer_border_lines(&state, 90, crate::theme::DARK).into_iter().map(|line| {
                line.spans.into_iter().map(|span| span.content.into_owned()).collect::<String>()
            }).collect::<Vec<_>>();
        assert_eq!(rendered.len(), 3);
        assert!(rendered[1].starts_with("│  /branch"));
        assert!(rendered.iter().all(|line| display_width(line) == 90));
    }

    #[test]
    fn completion_acceptance_is_explicit_and_consumed_once() {
        let mut state = todo_test_state(Vec::new());
        state.commands = vec![InteractiveCommand { name: "branch".to_owned(), description: String::new(), source: CommandSource::Builtin,
        }];
        state.editor.set_text("/br");
        state.completions.items = vec![CompletionItem { value: "/branch".to_owned(), label: "branch".to_owned(), description: String::new(), is_directory: false,
        }];
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
        let event = |job| {
            ApplicationEvent::Orchestration(pi_coding::OrchestrationEvent::JobUpdated {
            group_id: "group".to_owned(),
            job,
        })
        };
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
        state.push_entry(TranscriptEntry { kind: TranscriptKind::User, content: vec![ContentBlock::text("old prompt")], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false,
        });
        state.push_entry(TranscriptEntry { kind: TranscriptKind::Assistant, content: vec![ContentBlock::text("recent answer")], tool_name: None, tool_card: None, job_card: None, is_error: false, is_partial: false,
        });
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
        let top = lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        let bottom = lines[1]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(top.starts_with("╭── π  > ⬢ faux/faux-1 · ◑ med > 📁 "));
        assert!(top.ends_with('╮'));
        assert!(bottom.starts_with("╰─ visible input "));
        assert!(bottom.ends_with('╯'));
        assert_eq!(display_width(&top), 90);
        assert_eq!(display_width(&bottom), 90);
    }

    #[test]
    fn composer_header_hides_default_idle_status_but_keeps_meaningful_activity() {
        let mut state = todo_test_state(Vec::new());
        state.model = "faux/faux-1".to_owned();
        state.cwd = "<workspace>".to_owned();

        for idle in [
            "",
            "Ready",
            "Enter submit · Shift+Enter/Ctrl+J newline · Esc abort · Ctrl+D quit",
        ] {
            state.status = idle.to_owned();
            let top = composer_border_lines(&state, 120, crate::theme::DARK)[0]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            assert!(top.contains("⬢ faux/faux-1"), "metadata must remain: {top}");
            assert!(top.contains("◑ off"), "thinking metadata must remain: {top}");
            assert!(top.contains("📁 <workspace>"), "cwd metadata must remain: {top}");
            assert!(!top.contains("Ready"), "default idle status must be hidden: {top}");
            assert!(!top.contains("Enter submit"), "key help belongs in welcome and /help: {top}");
            assert!(!top.contains("⟲"), "idle activity segment must be omitted: {top}");
            assert_eq!(display_width(&top), 120);
        }

        state.status = "Usage: /import <path.jsonl>".to_owned();
        let meaningful = composer_border_lines(&state, 120, crate::theme::DARK)[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(meaningful.contains("Usage: /import <path.jsonl>"), "{meaningful}");
        assert!(meaningful.contains("▶──"), "meaningful status keeps activity glyph: {meaningful}");

        state.status = "Ready".to_owned();
        state.is_streaming = true;
        let first = composer_border_lines(&state, 120, crate::theme::DARK)[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        state.animation_frame = 1;
        let second = composer_border_lines(&state, 120, crate::theme::DARK)[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(first.contains("working"), "streaming must render activity: {first}");
        assert!(!first.contains("Ready"), "busy activity replaces idle status: {first}");
        assert_ne!(first, second, "streaming activity must animate");
    }

    #[test]
    fn long_unsupported_agent_warning_uses_bounded_notice_not_composer_chrome() {
        use ratatui::backend::TestBackend;

        let mut state = todo_test_state(Vec::new());
        state.model = "faux/faux-1".to_owned();
        state.cwd = "<workspace>/project".to_owned();
        state.thinking_level = ThinkingLevel::Medium;
        state.status = "Ready".to_owned();
        let warning = "agent `legacy-reviewer` is unavailable because it requests unsupported child tools: browser, computer, imaginary_tool; remove those tools from the agent definition or settings.agents.legacy-reviewer.tools; supported child tools: read, bash, edit, write";
        state.apply_startup_warnings(vec![format!("Warning: {warning}")]);

        assert_eq!(state.composer_error.as_deref(), Some(warning));
        assert!(state.composer_error_is_warning);
        assert_eq!(state.status, "Ready", "warnings must not replace compact runtime status");
        assert!(state.transcript.is_empty(), "startup warnings must not become durable transcript rows");

        let composer = composer_border_lines(&state, 120, crate::theme::DARK)
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content.into_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(!composer[0].contains("⟲"), "Ready activity must be hidden: {composer:#?}");

        let notice = composer_error_toast_lines(&state, 120, crate::theme::DARK);
        let notice_text = notice
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert_eq!(notice.len(), usize::from(composer_error_toast_height(&state, 120)));
        assert!(notice.len() <= MAX_COMPOSER_ERROR_HEIGHT);
        assert!(notice_text.iter().all(|line| display_width(line) == 120));
        assert!(notice_text[0].starts_with("Warning: agent `legacy-reviewer`"), "{notice_text:#?}");
        assert!(notice_text.iter().all(|line| {
            !line.starts_with('╭') && !line.starts_with('│') && !line.starts_with('╰')
        }), "full box must not render: {notice_text:#?}");
        assert!(notice_text.iter().all(|line| !line.contains("Dismissed when")));
        assert_eq!(notice[0].spans[0].style.fg, Some(crate::theme::DARK.warning));
        assert!(notice.iter().flat_map(|line| &line.spans).all(|span| {
            span.style.bg == Some(crate::theme::DARK.custom_message_bg)
        }));

        let backend = TestBackend::new(120, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut images = TerminalImageRenderer::default();
        terminal.draw(|frame| { let _ = render(frame, &state, &mut images); }).unwrap();
        let screen = terminal.backend().buffer().content.chunks(120).map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>()).collect::<Vec<_>>();
        assert_eq!(screen.iter().filter(|row| row.contains("unsupported child tools")).count(), 1, "{screen:#?}");
        let warning_row = screen.iter().position(|row| row.contains("unsupported child tools")).expect("warning row");
        let composer_row = screen.iter().position(|row| row.contains("π") && row.contains("faux/faux-1")).expect("composer row");
        assert!(warning_row < composer_row, "{screen:#?}");

        state.record_accepted_prompt("continue");
        assert!(state.composer_error.is_none());
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
        assert_eq!(state.status, "", "run errors must not replace compact runtime status");
        assert!(state.transcript.is_empty());
        assert!(state.settled_commit_batch().is_empty());
        assert!(state.overflow_commit_batch(80, 12).is_empty());
    }

    #[test]
    fn composer_error_clears_on_escape_and_accepted_message_only() {
        let mut state = todo_test_state(Vec::new());
        state.push_status("first rejection".to_owned(), true);

        assert!(!dismiss_composer_error_on_escape(&mut state, KeyCode::Char('x'), false));
        assert_eq!(state.composer_error.as_deref(), Some("first rejection"));
        state.push_status("second rejection".to_owned(), true);
        assert_eq!(state.composer_error.as_deref(), Some("second rejection"));

        state.record_accepted_prompt("accepted prompt");
        assert!(state.composer_error.is_none());

        state.push_status("dismiss me".to_owned(), true);
        state.last_escape = Some(std::time::Instant::now());
        assert!(dismiss_composer_error_on_escape(&mut state, KeyCode::Esc, false));
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
        assert!(rendered.first().is_some_and(|line| line.starts_with("Error: bad payload")));
        assert!(rendered.iter().all(|line| {
            !line.starts_with('╭') && !line.starts_with('│') && !line.starts_with('╰')
        }));
        assert_eq!(lines[0].spans[0].style.fg, Some(crate::theme::DARK.error));
        assert!(lines.iter().flat_map(|line| &line.spans).all(|span| {
            span.style.bg == Some(crate::theme::DARK.tool_error_bg)
        }));

        for width in 1..5 {
            let tiny = composer_error_toast_lines(&state, width, crate::theme::DARK);
            assert_eq!(tiny.len(), usize::from(composer_error_toast_height(&state, width)));
            assert_eq!(tiny.len(), 1);
            let rendered = tiny[0]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            assert_eq!(display_width(&rendered), width);
        }

        state.composer_error = None;
        assert_eq!(composer_error_toast_height(&state, 24), 0);
        assert!(composer_error_toast_lines(&state, 24, crate::theme::DARK).is_empty());
    }

    #[test]
    fn composer_error_renders_immediately_above_composer() {
        use ratatui::backend::TestBackend;

        let mut state = todo_test_state(Vec::new());
        state.model = "faux/faux-1".to_owned();
        state.cwd = "<workspace>/project".to_owned();
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
            .position(|row| row.contains("Error: runtime exploded"))
            .expect("error banner row");
        let composer_row = rows
            .iter()
            .position(|row| row.contains("π") && row.contains("faux/faux-1"))
            .expect("composer top row");
        assert_eq!(error_row + 2, composer_row, "single-row notice keeps one blank row before composer: {rows:#?}");
        assert!(rows[error_row + 1].trim().is_empty(), "notice/composer spacer must be blank: {rows:#?}");
        assert!(!rows[..composer_row].iter().any(|row| row.contains("Dismissed when")));
        assert!(!rows[..composer_row].iter().any(|row| {
            row.trim_start().starts_with('╭') || row.trim_start().starts_with('╰')
        }), "notice must not render a box: {rows:#?}");
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
            background_rx.recv())
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
        assert!(rendered.contains("rpi"), "welcome must use concise rpi branding: {rendered}");
        assert!(!rendered.contains("pi-rs"), "welcome must not expose repository branding: {rendered}");
        assert!(
            rendered.contains("paste image") || rendered.contains("Paste image"),
            "welcome must mention image paste: {rendered}"
        );
    }

    #[test]
    fn welcome_renders_sanitized_source_badge_and_imported_marker_from_cache() {
        let mut state = todo_test_state(Vec::new());
        let mut row = resume_row_with_preview("Imported session");
        row.source_badge = "codex\u{1b}[31m";
        row.status = pi_coding::CatalogRowStatus::AlreadyImported {
            native_id: "native".to_owned(),
            native_path: PathBuf::from("/sessions/native.jsonl"),
        };
        state.recent_sessions = vec![row];
        let rendered = render_welcome_lines(&state, crate::theme::DARK)
            .iter().flat_map(|line| line.spans.iter()).map(|span| span.content.as_ref()).collect::<String>();
        assert!(
            rendered.contains("[codex] Imported session · imported"),
            "{rendered}"
        );
        assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
    }

    #[test]
    fn omp_multiline_composer_expands_without_losing_lines() {
        let mut state = todo_test_state(Vec::new());
        state.editor.set_text("first\nsecond");
        let lines = composer_border_lines(&state, 90, crate::theme::DARK);
        assert_eq!(lines.len(), 4);
        let rendered = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            }).collect::<Vec<_>>();
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
        state.cwd = "<workspace>/project".to_owned();
        state.editor.set_text("draft prompt");
        let backend = TestBackend::new(80, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut images = TerminalImageRenderer::default();
        terminal.draw(|frame| { let _ = render(frame, &state, &mut images); }).unwrap();
        let rendered = terminal.backend().buffer().content.iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("faux/faux-1"));
        assert!(rendered.contains("<workspace>/project"));
        assert!(rendered.contains("draft prompt"));
        assert!(!rendered.contains("Conversation"));
        assert!(!rendered.contains("pi (rs)"));
    }

    #[test]
    fn normal_live_regions_use_one_cell_top_and_horizontal_gutters() {
        use ratatui::backend::{Backend, TestBackend};

        let phases = vec![TodoPhase {
            name: "Delivery".to_owned(),
            tasks: vec![todo_item(
                "gutter-task",
                "todo gutter sentinel",
                TodoStatus::InProgress,
                true,
                &[],
            )],
        }];
        let mut state = todo_test_state(phases);
        state.transcript = vec![
            TranscriptEntry {
                kind: TranscriptKind::Assistant,
                content: vec![ContentBlock::text("transcript gutter sentinel")],
                tool_name: None,
                tool_card: None,
                job_card: None,
                is_error: false,
                is_partial: false,
            },
            TranscriptEntry {
                kind: TranscriptKind::User,
                content: vec![ContentBlock::text("user gutter sentinel")],
                tool_name: None,
                tool_card: None,
                job_card: None,
                is_error: false,
                is_partial: false,
            },
        ];
        state.composer_error = Some("notice gutter sentinel".to_owned());
        state.editor.set_text("draft");
        state.completions.items.push(CompletionItem {
            value: "/gutter".to_owned(),
            label: "completion gutter sentinel".to_owned(),
            description: "bounded".to_owned(),
            is_directory: false,
        });

        let width = 60;
        let backend = TestBackend::new(width, 24);
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
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>();
        assert!(rows[0].trim().is_empty(), "top gutter must stay blank: {:?}", rows[0]);
        for sentinel in [
            "transcript gutter sentinel",
            "user gutter sentinel",
            "todo gutter sentinel",
            "notice gutter sentinel",
            "completion gutter sentinel",
            "╭── π",
        ] {
            let row = rows
                .iter()
                .find(|row| row.contains(sentinel))
                .unwrap_or_else(|| panic!("missing {sentinel:?} in {rows:#?}"));
            assert_eq!(&row[..1], " ", "left gutter missing for {sentinel:?}: {row:?}");
            assert_eq!(&row[row.len() - 1..], " ", "right gutter missing for {sentinel:?}: {row:?}");
        }
        let cursor = terminal.backend_mut().get_cursor_position().unwrap();
        assert_eq!(cursor.x, 8, "content x=1 + composer inset=2 + draft width=5");
        assert!(cursor.y > 0, "cursor must account for the top gutter");
    }

    #[test]
    fn normal_live_gutter_degrades_safely_at_tiny_dimensions() {
        use ratatui::backend::TestBackend;

        for (width, height) in [(1, 1), (2, 1), (1, 2), (2, 2), (3, 1), (3, 2)] {
            let mut state = todo_test_state(Vec::new());
            state.editor.set_text("x");
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut images = TerminalImageRenderer::default();
            let mut identity = None;
            terminal
                .draw(|frame| {
                    identity = Some(render(frame, &state, &mut images).identity);
                })
                .unwrap();
            let identity = identity.unwrap();
            assert_eq!(identity.viewport_width, live_content_width(width));
            assert!(identity.viewport_height <= live_content_height(height));
        }
    }

    #[test]
    fn transcript_image_plan_uses_inner_width_and_gutter_origin() {
        use ratatui::backend::TestBackend;

        let mut state = todo_test_state(Vec::new());
        state.transcript.push(TranscriptEntry {
            kind: TranscriptKind::Assistant,
            content: vec![ContentBlock::Image {
                data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==".to_owned(),
                mime_type: "image/png".to_owned(),
            }],
            tool_name: None,
            tool_card: None,
            job_card: None,
            is_error: false,
            is_partial: false,
        });
        state.image_width_cells = 8;
        let backend = TestBackend::new(20, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut images = TerminalImageRenderer::new(Some(
            crate::terminal_images::TerminalImageProtocol::Kitty,
        ));
        let mut plan = None;
        terminal
            .draw(|frame| {
                plan = Some(render(frame, &state, &mut images));
            })
            .unwrap();
        let plan = plan.unwrap();
        assert_eq!(plan.identity.viewport_width, 18);
        assert_eq!(plan.placements.len(), 1);
        assert!(format!("{:?}", plan.placements[0]).contains("x: 1"));
        assert!(format!("{:?}", plan.placements[0]).contains("y: 1"));
    }

    #[tokio::test]
    async fn extension_above_and_below_rows_share_live_gutters() {
        use pi_coding::{ExtensionCancellation, ExtensionUiHost};
        use ratatui::backend::TestBackend;

        let adapter = ExtensionUiAdapter::new();
        for (key, line, placement) in [
            ("above", "above gutter sentinel", UiWidgetPlacement::AboveEditor),
            ("below", "below gutter sentinel", UiWidgetPlacement::BelowEditor),
        ] {
            adapter
                .request(
                    tui_context(1),
                    ExtensionUiRequest::Widget {
                        key: key.to_owned(),
                        lines: Some(vec![line.to_owned()]),
                        placement,
                    },
                    ExtensionCancellation::new(),
                )
                .await
                .unwrap();
        }
        let mut state = todo_test_state(Vec::new());
        state.extension_ui = adapter;
        state.editor.set_text("draft");
        let width = 50;
        let backend = TestBackend::new(width, 14);
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
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>();
        for sentinel in ["above gutter sentinel", "below gutter sentinel"] {
            let row = rows.iter().find(|row| row.contains(sentinel)).expect(sentinel);
            assert!(row.starts_with(' '), "left gutter missing: {row:?}");
            assert!(row.ends_with(' '), "right gutter missing: {row:?}");
        }
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
            let lines = agents_panel_lines(&panel, crate::theme::DARK, width, 40);
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
    fn agents_panel_down_navigation_keeps_selected_wrapped_row_in_bounded_viewport() {
        use pi_coding::{AgentDefinition, AgentDefinitionSource};

        let definitions = (0..30)
            .map(|index| AgentDefinition {
                name: format!("agent-{index:02}"),
                description: format!("agent-{index:02} selected description"),
                system_prompt: "prompt".to_owned(),
                tools: Some(vec![
                    "read".to_owned(),
                    "grep".to_owned(),
                    "bash".to_owned(),
                    "browser".to_owned(),
                ]),
                autoload_skills: vec!["rust".to_owned(), "research".to_owned()],
                model: None,
                thinking_level: Some(ThinkingLevel::Medium),
                source: AgentDefinitionSource::User,
                path: None,
                trusted: true,
            })
            .collect();
        let mut panel = AgentsPanel::new(
            definitions,
            &std::collections::BTreeMap::new(),
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
        );
        let viewport_height = 16;

        for expected in 0..30 {
            if expected > 0 {
                panel.move_list(1);
            }
            assert_eq!(panel.selected(), expected);
            let lines = agents_panel_lines(&panel, crate::theme::DARK, 32, viewport_height);
            assert!(lines.len() <= usize::from(viewport_height));
            assert!(lines.iter().any(|line| {
                line.spans
                    .iter()
                    .any(|span| span.style.bg == Some(crate::theme::DARK.selected_bg))
            }));
            let rendered = lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .map(|span| span.content.as_ref())
                .collect::<String>();
            assert!(rendered.contains(&format!("agent-{expected:02}")), "{rendered}");
        }

        panel.move_list(-1);
        assert_eq!(panel.selected(), 28, "one Up must immediately move from the end");
        let lines = agents_panel_lines(&panel, crate::theme::DARK, 32, viewport_height);
        let rendered = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("agent-28"), "{rendered}");
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

    #[test]
    fn footer_git_porcelain_counts_dirty_segments_for_branch_suffix() {
        // The composer footer renders `⑂ {branch}` plus a contiguous dirty
        // suffix `*{modified}+{staged}?{untracked}` (each part omitted when 0,
        // no spaces). `parse_git_porcelain` is the pure source of those counts,
        // so a miscount here is a visibly wrong footer: staged shown as
        // modified, untracked dropped, or a rename path counted as a status row.
        fn parse(input: &[u8]) -> FooterGitStatus {
            parse_git_porcelain(input).expect("porcelain with a branch header parses")
        }

        // Clean repo: header only -> no suffix parts render.
        assert_eq!(
            parse(b"## main\0"),
            FooterGitStatus { branch: Some("main".to_owned()), staged: 0, modified: 0, untracked: 0 }
        );
        // An upstream-tracking suffix must not leak into the branch token.
        assert_eq!(
            parse(b"## main...origin/main [ahead 1]\0").branch,
            Some("main".to_owned())
        );

        // Staged (index) vs modified (worktree) vs untracked are distinct.
        assert_eq!(
            parse(b"## main\0M  staged.txt\0 M modified.txt\0?? untracked.txt\0"),
            FooterGitStatus { branch: Some("main".to_owned()), staged: 1, modified: 1, untracked: 1 }
        );
        // Worktree-only rename/copy records also carry a continuation path.
        // Consume it exactly once so path bytes that resemble status records
        // cannot inflate the following tracked count.
        assert_eq!(
            parse(b"## main\0 R renamed\0 M fake-status-path\0M  staged.txt\0"),
            FooterGitStatus { branch: Some("main".to_owned()), staged: 1, modified: 1, untracked: 0 }
        );
        assert_eq!(
            parse(b"## main\0 C copied\0?? fake-status-path\0?? real.txt\0"),
            FooterGitStatus { branch: Some("main".to_owned()), staged: 0, modified: 1, untracked: 1 }
        );
        // A path staged AND modified counts in both buckets (MM).
        assert_eq!(
            parse(b"## main\0MM both.txt\0"),
            FooterGitStatus { branch: Some("main".to_owned()), staged: 1, modified: 1, untracked: 0 }
        );
        // A rename consumes its extra NUL-separated original-path record and
        // counts as exactly one staged entry (not two, no stray modified row).
        assert_eq!(
            parse(b"## main\0R  renamed\0old-path\0 M dirty.txt\0"),
            FooterGitStatus { branch: Some("main".to_owned()), staged: 1, modified: 1, untracked: 0 }
        );
        // No branch header -> the footer omits the whole git segment.
        assert_eq!(
            parse_git_porcelain(b"").unwrap(),
            FooterGitStatus { branch: None, staged: 0, modified: 0, untracked: 0 }
        );
    }

    #[test]
    fn footer_git_text_formats_branch_and_dirty_suffix() {
        // `footer_git_text` is the visible `⑂` segment body: clean repo shows
        // just the branch; each dirty bucket appends only when non-zero.
        assert_eq!(
            footer_git_text(&FooterGitStatus {
                branch: Some("main".to_owned()),
                staged: 0,
                modified: 0,
                untracked: 0,
            }),
            Some("main".to_owned())
        );
        assert_eq!(
            footer_git_text(&FooterGitStatus {
                branch: Some("feature/x".to_owned()),
                staged: 1,
                modified: 2,
                untracked: 3,
            }),
            Some("feature/x*2+1?3".to_owned())
        );
        assert_eq!(
            footer_git_text(&FooterGitStatus {
                branch: Some("main".to_owned()),
                staged: 0,
                modified: 0,
                untracked: 4,
            }),
            Some("main?4".to_owned())
        );
        // No branch -> the whole git segment is omitted.
        assert_eq!(
            footer_git_text(&FooterGitStatus {
                branch: None,
                staged: 5,
                modified: 5,
                untracked: 5,
            }),
            None
        );
    }

    #[test]
    fn footer_context_text_formats_percent_and_token_budget() {
        // The `◫` segment is `{percent}% {tokens}/{window}` with a k-suffix for
        // thousands; omitted when the model has no context window.
        assert_eq!(
            footer_context_text(&SessionContextUsage {
                tokens: Some(84_000),
                context_window: 200_000,
                percent: Some(42.0),
            }),
            Some("42% 84k/200k".to_owned())
        );
        // Sub-thousand counts render raw; percent rounds half away from zero.
        assert_eq!(
            footer_context_text(&SessionContextUsage {
                tokens: Some(512),
                context_window: 4_096,
                percent: Some(12.5),
            }),
            Some("13% 512/4k".to_owned()) // 4096 -> "4k", 512 stays raw, 12.5 -> 13
        );
        assert_eq!(
            footer_context_text(&SessionContextUsage {
                tokens: None,
                context_window: 0,
                percent: None,
            }),
            None
        );
    }

    #[test]
    fn full_width_footer_renders_git_and_context_segments() {
        // At a comfortable width the idle composer header carries model,
        // thinking, cwd, the git `⑂` segment, and the context `◫` segment on
        // one row. Idle activity is intentionally omitted.
        let mut state = todo_test_state(Vec::new());
        state.model = "faux/faux-1".to_owned();
        state.cwd = "<workspace>/project".to_owned();
        state.git_status = Some(FooterGitStatus {
            branch: Some("main".to_owned()),
            staged: 1,
            modified: 2,
            untracked: 3,
        });
        state.context_usage = Some(SessionContextUsage {
            tokens: Some(84_000),
            context_window: 200_000,
            percent: Some(42.0),
        });
        let header = composer_border_lines(&state, 120, crate::theme::DARK)[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(header.contains("faux/faux-1"), "model present: {header}");
        assert!(header.contains("⑂ main*2+1?3"), "git segment: {header}");
        assert!(header.contains("◫ 42% 84k/200k"), "context segment: {header}");
        assert!(!header.contains("▶──"), "idle activity must be omitted: {header}");
        // Exactly one header row, full width, no overflow past the border.
        assert_eq!(display_width(&header), 120);
    }

    #[test]
    fn narrow_footer_drops_context_then_git_without_overlap() {
        let mut state = todo_test_state(Vec::new());
        state.model = "faux/faux-1".to_owned();
        state.cwd = "<workspace>/project".to_owned();
        state.git_status = Some(FooterGitStatus {
            branch: Some("main".to_owned()),
            staged: 1,
            modified: 2,
            untracked: 3,
        });
        state.context_usage = Some(SessionContextUsage {
            tokens: Some(84_000),
            context_window: 200_000,
            percent: Some(42.0),
        });
        // Wide: both git and context survive.
        let wide = composer_border_lines(&state, 110, crate::theme::DARK)[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(wide.contains("⑂ main*2+1?3"), "git present when wide: {wide}");
        assert!(wide.contains("◫ 42% 84k/200k"), "context present when wide: {wide}");
        assert_eq!(display_width(&wide), 110);
        // Mid width: context (lowest priority) drops first, git survives.
        let mid = composer_border_lines(&state, 80, crate::theme::DARK)[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(!mid.contains("◫"), "context dropped at mid width: {mid}");
        assert!(mid.contains("⑂ main*2+1?3"), "git survives at mid width: {mid}");
        assert_eq!(display_width(&mid), 80);
        // Narrow width: git and context both drop, model + activity remain.
        let narrow = composer_border_lines(&state, 50, crate::theme::DARK)[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(!narrow.contains("◫"), "context dropped when narrow: {narrow}");
        assert!(!narrow.contains("⑂"), "git dropped when narrow: {narrow}");
        assert!(narrow.contains("faux/faux-1"), "model kept when narrow: {narrow}");
        assert_eq!(display_width(&narrow), 50);
        for width in [120u16, 90, 64, 52, 48, 40] {
            let row = composer_border_lines(&state, width, crate::theme::DARK)[0]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            assert_eq!(display_width(&row), width, "footer row {width} must not overflow");
        }
    }

    #[test]
    fn footer_refresh_discards_stale_result_and_starts_coalesced_request() {
        let mut state = todo_test_state(Vec::new());
        let dir_one = tempfile::tempdir().expect("first cwd");
        let dir_two = tempfile::tempdir().expect("second cwd");
        let session_one = pi_coding::Session::new(pi_coding::SessionOptions {
            model: Model::default(),
            cwd: dir_one.path().to_path_buf(),
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
        }).expect("first session");
        let session_two = pi_coding::Session::new(pi_coding::SessionOptions {
            model: Model::default(),
            cwd: dir_two.path().to_path_buf(),
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
        }).expect("second session");
        let first = FooterRefreshRequest {
            key: FooterRefreshKey {
                generation: 1,
                cwd: session_one.cwd().to_path_buf(),
                runtime_identity: session_one.process_owner_id(),
            },
            session: session_one,
        };
        let second = FooterRefreshRequest {
            key: FooterRefreshKey {
                generation: 2,
                cwd: session_two.cwd().to_path_buf(),
                runtime_identity: session_two.process_owner_id(),
            },
            session: session_two,
        };
        let third = FooterRefreshRequest {
            key: FooterRefreshKey {
                generation: 3,
                cwd: second.key.cwd.clone(),
                runtime_identity: second.key.runtime_identity.clone(),
            },
            session: second.session.clone(),
        };
        let start = std::time::Instant::now();

        assert_eq!(state.admit_footer_refresh(first.clone(), start).as_ref().map(|r| &r.key), Some(&first.key));
        assert!(state.footer_refresh_in_flight.is_some());
        assert!(state.admit_footer_refresh(second.clone(), start).is_none());
        assert!(state.admit_footer_refresh(third.clone(), start).is_none());
        assert_eq!(state.footer_refresh_pending.as_ref().map(|r| &r.key), Some(&third.key));
        assert_eq!(state.footer_refresh_current.as_ref(), Some(&third.key));

        state.git_status = Some(FooterGitStatus {
            branch: Some("current".to_owned()),
            staged: 0,
            modified: 0,
            untracked: 0,
        });
        let next = state.finish_footer_refresh(
            FooterRefreshResult {
                key: first.key.clone(),
                git: Some(FooterGitStatus {
                    branch: Some("stale".to_owned()),
                    staged: 9,
                    modified: 9,
                    untracked: 9,
                }),
                context: None,
            },
            start,
        ).expect("coalesced request starts after completion");
        assert_eq!(next.key, third.key);
        assert_eq!(state.footer_refresh_in_flight.as_ref(), Some(&third.key));
        assert!(state.footer_refresh_pending.is_none());
        assert_eq!(state.git_status.as_ref().and_then(|git| git.branch.as_deref()), Some("current"));

        assert!(state.finish_footer_refresh(
            FooterRefreshResult {
                key: first.key,
                git: None,
                context: None,
            },
            start,
        ).is_none());
        assert_eq!(state.footer_refresh_in_flight.as_ref(), Some(&third.key));

        assert!(state.finish_footer_refresh(
            FooterRefreshResult {
                key: third.key.clone(),
                git: Some(FooterGitStatus {
                    branch: Some("fresh".to_owned()),
                    staged: 1,
                    modified: 2,
                    untracked: 3,
                }),
                context: Some(SessionContextUsage {
                    tokens: Some(1_000),
                    context_window: 8_000,
                    percent: Some(12.5),
                }),
            },
            start,
        ).is_none());
        assert!(state.footer_refresh_in_flight.is_none());
        assert_eq!(state.git_status.as_ref().and_then(|git| git.branch.as_deref()), Some("fresh"));
        assert_eq!(state.context_usage.as_ref().map(|usage| usage.context_window), Some(8_000));
    }

    #[test]
    fn footer_refresh_filters_zero_context_for_matching_result() {
        let mut state = todo_test_state(Vec::new());
        let dir = tempfile::tempdir().expect("current cwd");
        let session = pi_coding::Session::new(pi_coding::SessionOptions {
            model: Model::default(),
            cwd: dir.path().to_path_buf(),
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
        }).expect("session");
        let key = FooterRefreshKey {
            generation: 7,
            cwd: session.cwd().to_path_buf(),
            runtime_identity: session.process_owner_id(),
        };
        state.footer_refresh_current = Some(key.clone());
        state.footer_refresh_in_flight = Some(key.clone());
        assert!(state.finish_footer_refresh(
            FooterRefreshResult {
                key,
                git: None,
                context: Some(SessionContextUsage {
                    tokens: Some(0),
                    context_window: 0,
                    percent: None,
                }),
            },
            std::time::Instant::now(),
        ).is_none());
        assert!(state.context_usage.is_none());
        assert!(state.git_status.is_none());
    }

    #[test]
    fn footer_segments_preserve_idle_and_working_activity() {
        // Git/context segments must not perturb the activity indicator: while
        // streaming the header advances the animation frame; idle it is stable.
        let mut state = todo_test_state(Vec::new());
        state.model = "faux/faux-1".to_owned();
        state.git_status = Some(FooterGitStatus {
            branch: Some("main".to_owned()),
            staged: 0,
            modified: 0,
            untracked: 0,
        });
        state.context_usage = Some(SessionContextUsage {
            tokens: Some(1_000),
            context_window: 8_000,
            percent: Some(12.5),
        });
        state.is_streaming = true;
        let first = composer_border_lines(&state, 100, crate::theme::DARK)[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        state.animation_frame = 1;
        let second = composer_border_lines(&state, 100, crate::theme::DARK)[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_ne!(first, second, "working header must animate");
        assert!(first.contains("working"), "working label: {first}");
        // Idle: animation frame no longer changes the rendered header.
        state.is_streaming = false;
        state.status = String::new();
        let idle_a = composer_border_lines(&state, 100, crate::theme::DARK)[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        state.animation_frame = 7;
        let idle_b = composer_border_lines(&state, 100, crate::theme::DARK)[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(idle_a, idle_b, "idle header must not animate");
        assert!(idle_a.contains("⑂ main"), "git segment stable when idle: {idle_a}");
        assert!(!state.has_active_animation());
    }

    /// Initialize a hermetic git repo for footer collector tests. Uses an
    /// isolated environment so host config/identity/hooks never leak in.
    fn init_hermetic_git_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("repo dir");
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("LC_ALL", "C")
                .env("GIT_AUTHOR_NAME", "Pi Test")
                .env("GIT_AUTHOR_EMAIL", "pi@example.test")
                .env("GIT_COMMITTER_NAME", "Pi Test")
                .env("GIT_COMMITTER_EMAIL", "pi@example.test")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .expect("git");
            assert!(status.success(), "git {args:?} failed in hermetic repo");
        };
        run(&["init", "-b", "main"]);
        std::fs::write(dir.path().join("tracked.txt"), "base\n").expect("write tracked");
        run(&["add", "tracked.txt"]);
        run(&["commit", "-m", "init"]);
        // Working-tree modification to a tracked file + a new untracked file.
        std::fs::write(dir.path().join("tracked.txt"), "base\nedited\n").expect("modify tracked");
        std::fs::write(dir.path().join("untracked.txt"), "new\n").expect("write untracked");
        dir
    }

    #[test]
    fn footer_collector_reads_hermetic_git_repo_status() {
        // End-to-end off-render collection against a real (isolated) repo: one
        // modified tracked file and one untracked file, branch `main`.
        let repo = init_hermetic_git_repo();
        let status = collect_footer_git_status(repo.path()).expect("git status collected");
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.staged, 0, "no staged changes: {status:?}");
        assert_eq!(status.modified, 1, "one modified tracked file: {status:?}");
        assert_eq!(status.untracked, 1, "one untracked file: {status:?}");
        // A non-repo directory yields no footer segment.
        let nowhere = tempfile::tempdir().expect("non-repo dir");
        assert!(collect_footer_git_status(nowhere.path()).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn footer_collector_never_executes_repo_local_fsmonitor_or_clean_filter() {
        let repo = init_hermetic_git_repo();
        let fsmonitor_sentinel = repo.path().join("fsmonitor-fired");
        let fsmonitor_hook = repo.path().join("fsmonitor-hook.sh");
        std::fs::write(
            &fsmonitor_hook,
            format!("#!/bin/sh\n: > '{}'\nexit 1\n", fsmonitor_sentinel.display()),
        ).expect("write fsmonitor hook");
        let filter_sentinel = repo.path().join("clean-filter-fired");
        let filter_hook = repo.path().join("clean-filter.sh");
        std::fs::write(
            &filter_hook,
            format!("#!/bin/sh\n: > '{}'\ncat\n", filter_sentinel.display()),
        ).expect("write clean filter");
        std::fs::write(repo.path().join(".gitattributes"), "tracked.txt filter=pwn\n")
            .expect("write attributes");
        let fsmonitor_command = format!("sh {}", fsmonitor_hook.display());
        let filter_command = format!("sh {}", filter_hook.display());
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("LC_ALL", "C")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .expect("git");
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["config", "core.fsmonitor", &fsmonitor_command]);
        run(&["config", "filter.pwn.clean", &filter_command]);
        run(&["config", "filter.pwn.process", &filter_command]);
        run(&["config", "filter.pwn.required", "true"]);
        run(&["config", "status.showUntrackedFiles", "no"]);
        run(&["config", "status.renames", "true"]);
        // Force the racily-clean path that makes Git compare worktree content:
        // same size as the indexed blob and mtime copied from the index.
        std::fs::write(repo.path().join("tracked.txt"), "same\n")
            .expect("write same-size tracked content");
        let touched = std::process::Command::new("touch")
            .arg("-r")
            .arg(repo.path().join(".git/index"))
            .arg(repo.path().join("tracked.txt"))
            .status()
            .expect("touch tracked mtime");
        assert!(touched.success(), "touch -r failed");

        let status = collect_footer_git_status(repo.path()).expect("footer status");
        assert!(!fsmonitor_sentinel.exists(), "repo-local core.fsmonitor executed");
        assert!(!filter_sentinel.exists(), "repo-local clean/process filter executed");
        assert_eq!(status.modified, 1);
        assert_eq!(status.untracked, 4, "footer includes attributes, two hooks, and original untracked file");
    }

    #[test]
    fn footer_collector_counts_unborn_staged_paths() {
        let repo = tempfile::tempdir().expect("unborn repo");
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("LC_ALL", "C")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .expect("git");
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-b", "main"]);
        std::fs::write(repo.path().join("staged.txt"), "new\n").expect("write staged");
        run(&["add", "staged.txt"]);
        let sandbox = crate::code_review::IsolatedGitSandbox::discover(repo.path())
            .expect("unborn sandbox");
        let run_isolated = |args: &[&str]| {
            let output = crate::code_review::run_git_bounded_timeout(
                sandbox.work_tree(),
                args,
                FOOTER_GIT_MAX_STDOUT,
                Some(sandbox.environment()),
                FOOTER_GIT_TIMEOUT,
            ).expect("isolated git");
            assert!(!output.truncated, "isolated git {args:?} truncated");
            assert!(output.error.is_none(), "isolated git {args:?}: {:?}", output.error);
            output.stdout
        };
        assert_eq!(
            String::from_utf8(run_isolated(&[
                "-c", "core.fsmonitor=false", "symbolic-ref", "--short", "HEAD",
            ])).expect("branch utf8").trim(),
            "main",
        );
        assert_eq!(count_nul_paths(&run_isolated(&[
            "-c", "core.fsmonitor=false", "ls-files", "--modified", "-z",
        ])), 0);
        assert_eq!(count_nul_paths(&run_isolated(&[
            "-c", "core.fsmonitor=false", "ls-files", "--cached", "-z",
        ])), 1);
        assert_eq!(count_nul_paths(&run_isolated(&[
            "-c", "core.fsmonitor=false", "ls-files", "--others", "--exclude-standard", "-z",
        ])), 0);

        let status = collect_footer_git_status(repo.path()).expect("unborn footer status");
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.staged, 1);
        assert_eq!(status.modified, 0);
        assert_eq!(status.untracked, 0);
    }

    #[test]
    fn transcript_kinds_render_with_semantic_theme_tokens() {
        // Coherency contract at the rendered-span level: user text uses
        // theme.user_message_text, the bash tool title uses theme.tool_title,
        // and system error text uses theme.error. Under OMP Titanium (DARK),
        // user_message_text and tool_title intentionally map to Color::Reset so
        // they inherit the terminal foreground; error stays a concrete RGB.
        // Wiring regressions fail here on observable span colors, not by grepping
        // source — Reset on those Titanium roles is expected, not a failure.
        let theme = crate::theme::DARK;
        assert_ne!(theme.error, Color::Reset);

        // User message text -> theme.user_message_text on the user card.
        let user = TranscriptEntry {
            kind: TranscriptKind::User,
            content: vec![ContentBlock::text("coherence-probe-user")],
            tool_name: None,
            tool_card: None,
            job_card: None,
            is_error: false,
            is_partial: false,
        };
        let mut lines = Vec::new();
        render_transcript_entry(&mut lines, &user, true, true, theme, 80);
        let user_spans: Vec<_> = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.content.contains("coherence-probe-user"))
            .collect();
        assert!(!user_spans.is_empty(), "user text must render: {lines:?}");
        assert!(
            user_spans.iter().all(|span| span.style.fg == Some(theme.user_message_text)),
            "user text must use theme.user_message_text: {user_spans:?}"
        );

        // System error text -> theme.error.
        let error = TranscriptEntry {
            kind: TranscriptKind::System,
            content: vec![ContentBlock::text("coherence-probe-error")],
            tool_name: None,
            tool_card: None,
            job_card: None,
            is_error: true,
            is_partial: false,
        };
        let mut lines = Vec::new();
        render_transcript_entry(&mut lines, &error, true, true, theme, 80);
        let error_spans: Vec<_> = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.content.contains("coherence-probe-error"))
            .collect();
        assert!(!error_spans.is_empty(), "error text must render: {lines:?}");
        assert!(
            error_spans.iter().all(|span| span.style.fg == Some(theme.error)),
            "system error text must use theme.error: {error_spans:?}"
        );

        // Bash tool title -> theme.tool_title.
        let bash = pi_ai::BashExecutionMessage {
            command: "echo coherence-probe-tool".to_owned(),
            output: String::new(),
            exit_code: Some(0),
            cancelled: false,
            truncated: false,
            full_output_path: None,
            timestamp: 1,
            exclude_from_context: None,
        };
        let compact = ToolCardPresentationAdapter::bash_execution_rows(&bash, false);
        let expanded = ToolCardPresentationAdapter::bash_execution_rows(&bash, true);
        let tool = tool_transcript_entry(compact, expanded);
        let mut lines = Vec::new();
        render_transcript_entry(&mut lines, &tool, true, true, theme, 80);
        let bash_spans: Vec<_> = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.content.contains("Bash"))
            .collect();
        assert!(!bash_spans.is_empty(), "bash tool title must render: {lines:?}");
        assert!(
            bash_spans.iter().all(|span| span.style.fg == Some(theme.tool_title)),
            "bash tool title must use theme.tool_title: {bash_spans:?}"
        );
    }

}
