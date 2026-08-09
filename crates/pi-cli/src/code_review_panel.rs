//! Fullscreen two-pane TUI state and rendering for `/code-review`.
//!
//! Keyboard and mouse navigation live here. Mouse capture is toggled by the
//! TUI only while this page is open so ordinary terminal selection is preserved.
//! Pure Rust/ratatui — no external diff server, browser, or HTML.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, anyhow};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use pi_agent::{Agent, AgentEvent, ToolCapability};
use pi_ai::{AssistantMessage, AssistantMessageEvent, ContentBlock, Message, StopReason};
use pi_coding::{
    Application, SideChatFork, create_read_only_tools, filter_tools_by_capabilities,
    tools_include_mutation,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use unicode_width::UnicodeWidthStr;

use crate::code_review::{
    DiffFile, DiffHunk, DiffLineKind, FileStatus, FileTree, HunkIdentity, ReviewScope,
    ReviewSnapshot, TreeNodeKind, load_review_snapshot,
};
use crate::markdown::ratatui::render_ratatui_markdown;
use crate::theme::Theme;
use crate::tui::{clean_terminal_text, markdown_ratatui_styles};

/// Which pane currently receives navigation keys.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CodeReviewFocus {
    #[default]
    Tree,
    Diff,
}

/// Result of handling input while the code-review page is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodeReviewPanelResult {
    Close,
    SubmitComment,
    AbortReview,
    Refork,
    Refresh,
    Handled,
    Busy,
    Unknown,
}

/// Hit regions recorded on the last successful render for mouse dispatch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CodeReviewHitRegions {
    pub tree_list: Rect,
    pub diff_body: Rect,
    pub tree_scroll: usize,
    pub diff_scroll_y: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewCommentRole {
    User,
    Assistant,
    System,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewComment {
    pub role: ReviewCommentRole,
    pub text: String,
    pub partial: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HunkThread {
    pub identity: HunkIdentity,
    pub comments: Vec<ReviewComment>,
    pub streaming_text: String,
    pub error: Option<String>,
    pub stale: bool,
}

impl HunkThread {
    fn new(identity: HunkIdentity) -> Self {
        Self {
            identity,
            comments: Vec::new(),
            streaming_text: String::new(),
            error: None,
            stale: false,
        }
    }

    fn push_assistant_delta(&mut self, delta: &str) {
        self.streaming_text.push_str(delta);
    }

    fn replace_pending_assistant(&mut self, assistant: &AssistantMessage) {
        let text = content_text(&assistant.content);
        if !text.is_empty() {
            self.streaming_text = text;
        }
    }

    fn finish_assistant_message(&mut self, assistant: &AssistantMessage) {
        if assistant.stop_reason == StopReason::Error {
            let message = assistant
                .error_message
                .clone()
                .filter(|message| !message.is_empty())
                .unwrap_or_else(|| {
                    let text = content_text(&assistant.content);
                    if text.is_empty() {
                        "Review failed".to_owned()
                    } else {
                        text
                    }
                });
            self.streaming_text.clear();
            self.error = Some(message);
            return;
        }
        let partial = assistant.stop_reason == StopReason::Aborted;
        let mut text = content_text(&assistant.content);
        if text.is_empty() {
            text = std::mem::take(&mut self.streaming_text);
        } else {
            self.streaming_text.clear();
        }
        if !text.is_empty() {
            self.comments.push(ReviewComment {
                role: ReviewCommentRole::Assistant,
                text,
                partial,
            });
        }
    }

    fn finish_streaming(&mut self, partial: bool) {
        if self.streaming_text.is_empty() {
            return;
        }
        let comment = ReviewComment {
            role: ReviewCommentRole::Assistant,
            text: std::mem::take(&mut self.streaming_text),
            partial,
        };
        if self
            .comments
            .last()
            .is_some_and(|existing| existing.role == ReviewCommentRole::Assistant)
        {
            *self.comments.last_mut().expect("assistant comment exists") = comment;
        } else {
            self.comments.push(comment);
        }
    }
}

#[derive(Clone, Debug)]
enum ReviewInternalEvent {
    Agent { generation: u64, event: AgentEvent },
    PromptFailed {
        generation: u64,
        identity: HunkIdentity,
        message: String,
    },
}

/// Page-scoped detached Agent and per-hunk thread store. Events are consumed
/// only by this controller and never forwarded to the main Application.
pub struct CodeReviewController {
    agent: Agent,
    fork: SideChatFork,
    main_application: Application,
    threads: BTreeMap<HunkIdentity, HunkThread>,
    stale_threads: Vec<HunkThread>,
    active_hunk: Option<HunkIdentity>,
    generation: u64,
    event_tx: mpsc::UnboundedSender<ReviewInternalEvent>,
    event_rx: mpsc::UnboundedReceiver<ReviewInternalEvent>,
    prompt_task: Option<JoinHandle<()>>,
    subscription: Option<pi_agent::Subscription>,
    abort_requested: bool,
    is_streaming: bool,
}

impl CodeReviewController {
    pub async fn fork_from(application: &Application) -> Result<Self> {
        let fork = application.fork_side_chat().await?;
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let generation = 1;
        let (agent, subscription) = Self::create_agent(&fork, generation, &event_tx).await?;
        Ok(Self {
            agent,
            fork,
            main_application: application.clone(),
            threads: BTreeMap::new(),
            stale_threads: Vec::new(),
            active_hunk: None,
            generation,
            event_tx,
            event_rx,
            prompt_task: None,
            subscription: Some(subscription),
            abort_requested: false,
            is_streaming: false,
        })
    }

    async fn create_agent(
        fork: &SideChatFork,
        generation: u64,
        event_tx: &mpsc::UnboundedSender<ReviewInternalEvent>,
    ) -> Result<(Agent, pi_agent::Subscription)> {
        let cwd = fork.cwd.to_string_lossy();
        let tools = filter_tools_by_capabilities(
            create_read_only_tools(&cwd),
            &[ToolCapability::Read],
        );
        if tools_include_mutation(&tools) {
            return Err(anyhow!("code-review Agent contains mutating tools"));
        }
        let agent = Application::create_side_chat_agent(fork, tools);
        let tx = event_tx.clone();
        let subscription = agent
            .subscribe_simple(move |event| {
                let tx = tx.clone();
                async move {
                    let _ = tx.send(ReviewInternalEvent::Agent { generation, event });
                    Ok(())
                }
            })
            .await;
        Ok((agent, subscription))
    }

    #[must_use]
    pub fn threads(&self) -> &BTreeMap<HunkIdentity, HunkThread> {
        &self.threads
    }

    #[must_use]
    pub fn stale_threads(&self) -> &[HunkThread] {
        &self.stale_threads
    }

    #[must_use]
    pub fn thread(&self, identity: &HunkIdentity) -> Option<&HunkThread> {
        self.threads.get(identity)
    }

    #[must_use]
    pub fn is_streaming(&self) -> bool {
        self.is_streaming
    }

    #[must_use]
    pub fn active_hunk(&self) -> Option<&HunkIdentity> {
        self.active_hunk.as_ref()
    }

    pub fn poll_events(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.event_rx.try_recv() {
            changed = true;
            match event {
                ReviewInternalEvent::Agent { generation, event }
                    if generation == self.generation => self.apply_agent_event(event),
                ReviewInternalEvent::PromptFailed {
                    generation,
                    identity,
                    message,
                } if generation == self.generation => {
                    if let Some(thread) = self.threads.get_mut(&identity) {
                        thread.streaming_text.clear();
                        thread.error = Some(message);
                    }
                    self.is_streaming = false;
                    self.active_hunk = None;
                    self.abort_requested = false;
                }
                ReviewInternalEvent::Agent { .. } | ReviewInternalEvent::PromptFailed { .. } => {}
            }
        }
        if self.prompt_task.as_ref().is_some_and(|handle| handle.is_finished()) {
            self.prompt_task = None;
        }
        changed
    }

    fn apply_agent_event(&mut self, event: AgentEvent) {
        let Some(identity) = self.active_hunk.clone() else {
            return;
        };
        let Some(thread) = self.threads.get_mut(&identity) else {
            return;
        };
        match event {
            AgentEvent::AgentStart => {
                self.is_streaming = true;
                thread.streaming_text.clear();
                thread.error = None;
            }
            AgentEvent::MessageStart {
                message: Message::Assistant(_),
            } => {
                thread.streaming_text.clear();
            }
            AgentEvent::MessageUpdate {
                message: Message::Assistant(assistant),
                ..
            }
            | AgentEvent::MessageEnd {
                message: Message::Assistant(assistant),
            } => thread.replace_pending_assistant(&assistant),
            AgentEvent::AgentEnd { messages } => {
                if let Some(Message::Assistant(mut assistant)) = messages
                    .into_iter()
                    .rev()
                    .find(|message| matches!(message, Message::Assistant(_)))
                {
                    if self.abort_requested {
                        assistant.stop_reason = StopReason::Aborted;
                    }
                    thread.finish_assistant_message(&assistant);
                } else {
                    thread.finish_streaming(self.abort_requested);
                }
                self.is_streaming = false;
                self.active_hunk = None;
            }
            _ => {}
        }
    }

    pub fn submit_comment(
        &mut self,
        snapshot: &ReviewSnapshot,
        file: &DiffFile,
        hunk: &DiffHunk,
        comment: &str,
    ) -> bool {
        let comment = comment.trim();
        if comment.is_empty() || self.is_streaming {
            return false;
        }
        let identity = snapshot.hunk_identity(file, hunk);
        let thread = self
            .threads
            .entry(identity.clone())
            .or_insert_with(|| HunkThread::new(identity.clone()));
        thread.comments.push(ReviewComment {
            role: ReviewCommentRole::User,
            text: comment.to_owned(),
            partial: false,
        });
        thread.streaming_text.clear();
        thread.error = None;
        let prompt = review_prompt(file, hunk, comment);
        let agent = self.agent.clone();
        let tx = self.event_tx.clone();
        let generation = self.generation;
        let failed_identity = identity.clone();
        self.active_hunk = Some(identity);
        self.abort_requested = false;
        self.is_streaming = true;
        self.prompt_task = Some(tokio::spawn(async move {
            if let Err(error) = agent.prompt(prompt).await {
                let _ = tx.send(ReviewInternalEvent::PromptFailed {
                    generation,
                    identity: failed_identity,
                    message: format!("Review failed: {error:#}"),
                });
            }
        }));
        true
    }

    pub async fn abort(&mut self) {
        let was_streaming = self.is_streaming || self.agent.state().await.is_streaming;
        self.abort_requested = was_streaming;
        if was_streaming {
            self.agent.abort().await;
        }
        if let Some(handle) = self.prompt_task.take() {
            handle.abort();
            let _ = handle.await;
        }
        self.agent.wait_for_idle().await;
        self.poll_events();
        if let Some(identity) = self.active_hunk.take()
            && let Some(thread) = self.threads.get_mut(&identity)
        {
            thread.finish_streaming(true);
        }
        self.is_streaming = false;
        self.abort_requested = false;
    }

    pub async fn refork(&mut self) -> Result<()> {
        self.abort().await;
        let fork = self.main_application.fork_side_chat().await?;
        self.generation = self.generation.wrapping_add(1);
        let (agent, subscription) =
            Self::create_agent(&fork, self.generation, &self.event_tx).await?;
        self.agent = agent;
        self.fork = fork;
        self.subscription = Some(subscription);
        Ok(())
    }

    pub async fn shutdown(&mut self) {
        self.abort().await;
        self.subscription = None;
        while self.event_rx.try_recv().is_ok() {}
    }

    pub fn reconcile_snapshot(&mut self, snapshot: &ReviewSnapshot) {
        let mut identities = Vec::new();
        for file in &snapshot.files {
            for hunk in &file.hunks {
                identities.push(snapshot.hunk_identity(file, hunk));
            }
        }
        let mut old = std::mem::take(&mut self.threads);
        let mut next = BTreeMap::new();
        let mut next_active = None;
        for identity in identities {
            let matched = old
                .keys()
                .find(|candidate| candidate.matches_across_snapshots(&identity))
                .cloned();
            if let Some(matched) = matched
                && let Some(mut thread) = old.remove(&matched)
            {
                if self.active_hunk.as_ref() == Some(&matched) {
                    next_active = Some(identity.clone());
                }
                thread.identity = identity.clone();
                thread.stale = false;
                next.insert(identity, thread);
            }
        }
        self.active_hunk = next_active;
        self.stale_threads.extend(old.into_values().map(|mut thread| {
            thread.stale = true;
            thread
        }));
        self.stale_threads
            .sort_by(|left, right| left.identity.cmp(&right.identity));
        self.threads = next;
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

#[must_use]
pub fn review_prompt(file: &DiffFile, hunk: &DiffHunk, comment: &str) -> String {
    format!(
        "Review this exact code-review hunk and answer the user's comment. Do not modify files.\n\nPath: {}\nOld range: {},{}\nNew range: {},{}\n\nExact hunk diff:\n```diff\n{}\n```\n\nUser comment:\n{}",
        file.path,
        hunk.old_start,
        hunk.old_count,
        hunk.new_start,
        hunk.new_count,
        hunk.unified_diff(),
        comment,
    )
}

/// Fullscreen HEAD→working-tree diff browser.
#[derive(Clone, Debug)]
pub struct CodeReviewPanel {
    snapshot: ReviewSnapshot,
    tree: FileTree,
    /// Index into `tree.visible_rows()`.
    tree_selected: usize,
    tree_scroll: usize,
    focus: CodeReviewFocus,
    diff_scroll_y: usize,
    diff_scroll_x: usize,
    selected_hunk: usize,
    comment_editor: Option<String>,
    comment_cursor: usize,
    threads: BTreeMap<HunkIdentity, HunkThread>,
    collapsed_threads: BTreeSet<HunkIdentity>,
    stale_thread_count: usize,
    snapshot_loading: bool,
    review_streaming: bool,
    /// Last known content viewport for clamping.
    tree_viewport_rows: usize,
    diff_viewport_rows: usize,
    diff_viewport_cols: usize,
    /// Hit-test regions from the most recent render (or layout sync).
    hits: CodeReviewHitRegions,
}

impl CodeReviewPanel {
    #[must_use]
    pub fn load(cwd: &std::path::Path) -> Self {
        Self::from_snapshot(load_review_snapshot(cwd))
    }

    #[must_use]
    pub fn from_snapshot(snapshot: ReviewSnapshot) -> Self {
        let tree = FileTree::from_snapshot(&snapshot);
        let tree_selected = tree.first_file_visible_index().unwrap_or(0);
        let mut panel = Self {
            snapshot,
            tree,
            tree_selected,
            tree_scroll: 0,
            focus: CodeReviewFocus::Tree,
            diff_scroll_y: 0,
            diff_scroll_x: 0,
            selected_hunk: 0,
            comment_editor: None,
            comment_cursor: 0,
            threads: BTreeMap::new(),
            collapsed_threads: BTreeSet::new(),
            stale_thread_count: 0,
            snapshot_loading: false,
            review_streaming: false,
            tree_viewport_rows: 1,
            diff_viewport_rows: 1,
            diff_viewport_cols: 1,
            hits: CodeReviewHitRegions::default(),
        };
        panel.ensure_tree_selection_valid();
        panel.reset_diff_scroll();
        panel
    }

    #[must_use]
    pub fn loading(root: std::path::PathBuf, scope: ReviewScope) -> Self {
        let mut panel = Self::from_snapshot(ReviewSnapshot {
            root,
            scope,
            snapshot_id: "loading".to_owned(),
            files: Vec::new(),
            truncated: false,
            error: None,
        });
        panel.snapshot_loading = true;
        panel
    }

    #[must_use]
    pub fn snapshot(&self) -> &ReviewSnapshot {
        &self.snapshot
    }

    #[must_use]
    pub const fn focus(&self) -> CodeReviewFocus {
        self.focus
    }

    #[must_use]
    pub const fn tree_selected(&self) -> usize {
        self.tree_selected
    }

    #[must_use]
    pub const fn diff_scroll_y(&self) -> usize {
        self.diff_scroll_y
    }

    #[must_use]
    pub const fn diff_scroll_x(&self) -> usize {
        self.diff_scroll_x
    }

    #[must_use]
    pub const fn hits(&self) -> CodeReviewHitRegions {
        self.hits
    }

    #[must_use]
    pub fn selected_file(&self) -> Option<&DiffFile> {
        let file_index = self.tree.file_index_at_visible(self.tree_selected)?;
        self.snapshot.files.get(file_index)
    }

    #[must_use]
    pub fn selected_hunk(&self) -> Option<&DiffHunk> {
        self.selected_file()?.hunks.get(self.selected_hunk)
    }

    #[must_use]
    pub fn selected_hunk_identity(&self) -> Option<HunkIdentity> {
        let file = self.selected_file()?;
        let hunk = file.hunks.get(self.selected_hunk)?;
        Some(self.snapshot.hunk_identity(file, hunk))
    }

    #[must_use]
    pub fn comment_editor(&self) -> Option<&str> {
        self.comment_editor.as_deref()
    }

    #[must_use]
    pub const fn comment_cursor(&self) -> usize {
        self.comment_cursor
    }

    #[must_use]
    pub const fn is_snapshot_loading(&self) -> bool {
        self.snapshot_loading
    }

    #[must_use]
    pub const fn is_review_streaming(&self) -> bool {
        self.review_streaming
    }

    pub fn complete_comment_submit(&mut self, accepted: bool) {
        if accepted {
            self.cancel_comment();
        }
    }

    pub fn cancel_comment(&mut self) {
        self.comment_editor = None;
        self.comment_cursor = 0;
    }

    pub fn handle_paste(&mut self, payload: &str) -> bool {
        let Some(editor) = self.comment_editor.as_mut() else {
            return false;
        };
        let normalized = payload.replace("\r\n", "\n").replace('\r', "\n");
        editor.insert_str(self.comment_cursor, &normalized);
        self.comment_cursor += normalized.len();
        true
    }

    pub fn open_comment_editor(&mut self) -> bool {
        if self.review_streaming || self.snapshot_loading || self.selected_hunk().is_none() {
            return false;
        }
        self.focus = CodeReviewFocus::Diff;
        self.comment_editor = Some(String::new());
        self.comment_cursor = 0;
        true
    }

    pub fn sync_controller(&mut self, controller: &CodeReviewController) {
        self.threads = controller.threads().clone();
        self.reconcile_collapsed_threads();
        if let Some(active) = controller.active_hunk() {
            self.collapsed_threads.remove(active);
        }
        self.stale_thread_count = controller.stale_threads().len();
        self.review_streaming = controller.is_streaming();
    }

    pub fn set_snapshot_loading(&mut self, loading: bool) {
        self.snapshot_loading = loading;
    }

    pub fn replace_snapshot(&mut self, snapshot: ReviewSnapshot) {
        let selected_path = self.selected_file().map(|file| file.path.clone());
        self.snapshot = snapshot;
        self.snapshot_loading = false;
        self.tree = FileTree::from_snapshot(&self.snapshot);
        self.tree_selected = selected_path
            .as_deref()
            .and_then(|path| {
                self.tree.visible_rows().iter().position(|row| {
                    self.tree.nodes[row.node_index].path == path
                })
            })
            .or_else(|| self.tree.first_file_visible_index())
            .unwrap_or(0);
        self.selected_hunk = 0;
        self.reconcile_collapsed_threads();
        self.reset_diff_scroll();
        self.ensure_tree_selection_valid();
    }

    fn reconcile_collapsed_threads(&mut self) {
        let current = self.threads.keys().cloned().collect::<Vec<_>>();
        self.collapsed_threads = self
            .collapsed_threads
            .iter()
            .filter_map(|old| {
                current
                    .iter()
                    .find(|identity| old.matches_across_snapshots(identity))
                    .cloned()
            })
            .collect();
    }

    /// Fold/unfold the selected hunk: hides its unchanged context lines while
    /// the changed +/- lines and any thread cards stay readable under the
    /// folded header. Works on any hunk, with or without a thread, so Space
    /// always gives visible feedback.
    fn toggle_selected_thread(&mut self) {
        let Some(identity) = self.selected_hunk_identity() else {
            return;
        };
        if !self.collapsed_threads.remove(&identity) {
            self.collapsed_threads.insert(identity);
        }
        self.clamp_diff_scroll();
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> CodeReviewPanelResult {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return CodeReviewPanelResult::Handled;
        }
        if self.comment_editor.is_some() {
            return self.handle_comment_key(key);
        }
        if key.code == KeyCode::Esc && self.review_streaming {
            return CodeReviewPanelResult::AbortReview;
        }
        if key.code == KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::ALT) {
            return CodeReviewPanelResult::Refork;
        }
        if key.code == KeyCode::Char('r') && key.modifiers.is_empty() {
            return if self.review_streaming || self.snapshot_loading {
                CodeReviewPanelResult::Busy
            } else {
                CodeReviewPanelResult::Refresh
            };
        }
        if key.modifiers.is_empty() && key.code == KeyCode::Char('c') {
            return if self.open_comment_editor() {
                CodeReviewPanelResult::Handled
            } else if self.review_streaming || self.snapshot_loading {
                CodeReviewPanelResult::Busy
            } else {
                CodeReviewPanelResult::Unknown
            };
        }
        if key.modifiers.is_empty() && key.code == KeyCode::Char('[') {
            self.focus = CodeReviewFocus::Diff;
            self.selected_hunk = self.selected_hunk.saturating_sub(1);
            self.scroll_selected_hunk_into_view();
            return CodeReviewPanelResult::Handled;
        }
        if key.modifiers.is_empty() && key.code == KeyCode::Char(']') {
            self.focus = CodeReviewFocus::Diff;
            let hunk_count = self.selected_file().map_or(0, |file| file.hunks.len());
            self.selected_hunk = self
                .selected_hunk
                .saturating_add(1)
                .min(hunk_count.saturating_sub(1));
            self.scroll_selected_hunk_into_view();
            return CodeReviewPanelResult::Handled;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q')
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                CodeReviewPanelResult::Close
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    CodeReviewFocus::Tree => CodeReviewFocus::Diff,
                    CodeReviewFocus::Diff => CodeReviewFocus::Tree,
                };
                CodeReviewPanelResult::Handled
            }
            other => match self.focus {
                CodeReviewFocus::Tree => self.handle_tree_key(other, key.modifiers),
                CodeReviewFocus::Diff => self.handle_diff_key(other, key.modifiers),
            },
        }
    }

    fn handle_comment_key(&mut self, key: KeyEvent) -> CodeReviewPanelResult {
        match key.code {
            KeyCode::Esc => {
                self.cancel_comment();
                CodeReviewPanelResult::Handled
            }
            KeyCode::Enter if !key.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                if self.comment_editor.as_deref().is_some_and(|text| !text.trim().is_empty()) {
                    CodeReviewPanelResult::SubmitComment
                } else {
                    CodeReviewPanelResult::Handled
                }
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                if let Some(editor) = self.comment_editor.as_mut() {
                    editor.insert(self.comment_cursor, '\n');
                    self.comment_cursor += 1;
                }
                CodeReviewPanelResult::Handled
            }
            KeyCode::Backspace => {
                if self.comment_cursor > 0
                    && let Some(editor) = self.comment_editor.as_mut()
                {
                    let previous = editor[..self.comment_cursor]
                        .chars()
                        .next_back()
                        .map(char::len_utf8)
                        .unwrap_or(0);
                    let start = self.comment_cursor.saturating_sub(previous);
                    editor.replace_range(start..self.comment_cursor, "");
                    self.comment_cursor = start;
                }
                CodeReviewPanelResult::Handled
            }
            KeyCode::Left => {
                if self.comment_cursor > 0
                    && let Some(editor) = self.comment_editor.as_ref()
                {
                    self.comment_cursor -= editor[..self.comment_cursor]
                        .chars()
                        .next_back()
                        .map(char::len_utf8)
                        .unwrap_or(0);
                }
                CodeReviewPanelResult::Handled
            }
            KeyCode::Right => {
                if let Some(editor) = self.comment_editor.as_ref()
                    && self.comment_cursor < editor.len()
                {
                    self.comment_cursor += editor[self.comment_cursor..]
                        .chars()
                        .next()
                        .map(char::len_utf8)
                        .unwrap_or(0);
                }
                CodeReviewPanelResult::Handled
            }
            KeyCode::Char(character)
                if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(editor) = self.comment_editor.as_mut() {
                    editor.insert(self.comment_cursor, character);
                    self.comment_cursor += character.len_utf8();
                }
                CodeReviewPanelResult::Handled
            }
            _ => CodeReviewPanelResult::Handled,
        }
    }

    /// Dispatch a mouse event using the last rendered hit regions.
    pub fn handle_mouse(&mut self, event: MouseEvent) -> CodeReviewPanelResult {
        let col = event.column;
        let row = event.row;
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => self.handle_click(col, row),
            MouseEventKind::ScrollUp => self.handle_wheel(col, row, -1),
            MouseEventKind::ScrollDown => self.handle_wheel(col, row, 1),
            MouseEventKind::ScrollLeft => {
                if point_in(self.hits.diff_body, col, row) {
                    self.focus = CodeReviewFocus::Diff;
                    self.diff_scroll_x = self.diff_scroll_x.saturating_sub(4);
                    self.clamp_diff_scroll();
                }
                CodeReviewPanelResult::Handled
            }
            MouseEventKind::ScrollRight => {
                if point_in(self.hits.diff_body, col, row) {
                    self.focus = CodeReviewFocus::Diff;
                    self.diff_scroll_x = self.diff_scroll_x.saturating_add(4);
                    self.clamp_diff_scroll();
                }
                CodeReviewPanelResult::Handled
            }
            // Consume other mouse noise so it does not fall through to composer.
            _ => CodeReviewPanelResult::Handled,
        }
    }

    fn handle_click(&mut self, col: u16, row: u16) -> CodeReviewPanelResult {
        if point_in(self.hits.tree_list, col, row) {
            self.focus = CodeReviewFocus::Tree;
            let offset = usize::from(row.saturating_sub(self.hits.tree_list.y));
            let visible_index = self.hits.tree_scroll.saturating_add(offset);
            let rows = self.tree.visible_rows();
            let Some(tree_row) = rows.get(visible_index).copied() else {
                return CodeReviewPanelResult::Handled;
            };
            self.tree_selected = visible_index;
            self.ensure_tree_visible();
            if tree_row.is_dir {
                self.tree.toggle_collapse(tree_row.node_index);
                self.ensure_tree_selection_valid();
                self.ensure_tree_visible();
            } else {
                self.selected_hunk = 0;
                self.reset_diff_scroll();
                self.focus = CodeReviewFocus::Diff;
            }
            return CodeReviewPanelResult::Handled;
        }
        if point_in(self.hits.diff_body, col, row) {
            self.focus = CodeReviewFocus::Diff;
            let display_index = self.hits.diff_scroll_y.saturating_add(usize::from(
                row.saturating_sub(self.hits.diff_body.y),
            ));
            let Some(file) = self.selected_file() else {
                return CodeReviewPanelResult::Handled;
            };
            let display = build_diff_display_lines(self, file);
            if let Some(line) = display.get(display_index)
                && let Some(hunk_index) = line.hunk_index
            {
                // Clicking a diff or thread row selects the hunk; thread cards
                // have no separate collapse UI, so the fold stays keyboard-only.
                self.selected_hunk = hunk_index;
            }
            return CodeReviewPanelResult::Handled;
        }
        // Outside interactive panes: no-op (still handled so composer is idle).
        CodeReviewPanelResult::Handled
    }

    fn handle_wheel(&mut self, col: u16, row: u16, delta: isize) -> CodeReviewPanelResult {
        if point_in(self.hits.diff_body, col, row) {
            self.focus = CodeReviewFocus::Diff;
            if delta < 0 {
                self.diff_scroll_y = self.diff_scroll_y.saturating_sub(delta.unsigned_abs());
            } else {
                self.diff_scroll_y = self.diff_scroll_y.saturating_add(delta as usize);
            }
            self.clamp_diff_scroll();
            return CodeReviewPanelResult::Handled;
        }
        if point_in(self.hits.tree_list, col, row) {
            self.focus = CodeReviewFocus::Tree;
            self.move_tree_selection(delta);
            return CodeReviewPanelResult::Handled;
        }
        CodeReviewPanelResult::Handled
    }

    fn handle_tree_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> CodeReviewPanelResult {
        if modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
            return CodeReviewPanelResult::Unknown;
        }
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_tree_selection(-1);
                CodeReviewPanelResult::Handled
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_tree_selection(1);
                CodeReviewPanelResult::Handled
            }
            KeyCode::PageUp => {
                let step = self.tree_viewport_rows.max(1) as isize;
                self.move_tree_selection(-step);
                CodeReviewPanelResult::Handled
            }
            KeyCode::PageDown => {
                let step = self.tree_viewport_rows.max(1) as isize;
                self.move_tree_selection(step);
                CodeReviewPanelResult::Handled
            }
            KeyCode::Home => {
                self.tree_selected = 0;
                self.ensure_tree_selection_valid();
                self.reset_diff_scroll();
                self.ensure_tree_visible();
                CodeReviewPanelResult::Handled
            }
            KeyCode::End => {
                let len = self.tree.visible_rows().len();
                self.tree_selected = len.saturating_sub(1);
                self.ensure_tree_selection_valid();
                self.reset_diff_scroll();
                self.ensure_tree_visible();
                CodeReviewPanelResult::Handled
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.collapse_or_parent();
                CodeReviewPanelResult::Handled
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => {
                self.expand_or_focus_diff();
                CodeReviewPanelResult::Handled
            }
            KeyCode::Char(' ') => {
                if let Some(row) = self.tree.visible_rows().get(self.tree_selected).copied() {
                    if row.is_dir {
                        self.tree.toggle_collapse(row.node_index);
                        self.ensure_tree_selection_valid();
                        self.ensure_tree_visible();
                    }
                }
                CodeReviewPanelResult::Handled
            }
            _ => CodeReviewPanelResult::Unknown,
        }
    }

    fn handle_diff_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> CodeReviewPanelResult {
        if modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
            return CodeReviewPanelResult::Unknown;
        }
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.select_relative_hunk(-1);
                CodeReviewPanelResult::Handled
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.select_relative_hunk(1);
                CodeReviewPanelResult::Handled
            }
            KeyCode::Char(' ') | KeyCode::Char('o') => {
                self.toggle_selected_thread();
                CodeReviewPanelResult::Handled
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.diff_scroll_x = self.diff_scroll_x.saturating_sub(4);
                self.clamp_diff_scroll();
                CodeReviewPanelResult::Handled
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.diff_scroll_x = self.diff_scroll_x.saturating_add(4);
                self.clamp_diff_scroll();
                CodeReviewPanelResult::Handled
            }
            KeyCode::PageUp => {
                self.diff_scroll_y = self
                    .diff_scroll_y
                    .saturating_sub(self.diff_viewport_rows.max(1));
                self.clamp_diff_scroll();
                CodeReviewPanelResult::Handled
            }
            KeyCode::PageDown => {
                self.diff_scroll_y = self
                    .diff_scroll_y
                    .saturating_add(self.diff_viewport_rows.max(1));
                self.clamp_diff_scroll();
                CodeReviewPanelResult::Handled
            }
            KeyCode::Home => {
                self.diff_scroll_y = 0;
                self.diff_scroll_x = 0;
                CodeReviewPanelResult::Handled
            }
            KeyCode::End => {
                self.diff_scroll_y = usize::MAX / 4;
                self.clamp_diff_scroll();
                CodeReviewPanelResult::Handled
            }
            _ => CodeReviewPanelResult::Unknown,
        }
    }

    fn select_relative_hunk(&mut self, delta: isize) {
        let hunk_count = self.selected_file().map_or(0, |file| file.hunks.len());
        if hunk_count == 0 {
            return;
        }
        self.selected_hunk = (self.selected_hunk as isize + delta)
            .clamp(0, hunk_count.saturating_sub(1) as isize) as usize;
        self.scroll_selected_hunk_into_view();
    }

    fn scroll_selected_hunk_into_view(&mut self) {
        let Some(file) = self.selected_file() else {
            return;
        };
        let display = build_diff_display_lines(self, file);
        let Some(target) = display.iter().position(|line| {
            line.hunk_index == Some(self.selected_hunk) && line.kind == DiffDisplayKind::HunkHeader
        }) else {
            return;
        };
        let viewport = self.diff_viewport_rows.max(1);
        if target < self.diff_scroll_y {
            self.diff_scroll_y = target;
        } else if target >= self.diff_scroll_y.saturating_add(viewport) {
            self.diff_scroll_y = target.saturating_add(1).saturating_sub(viewport);
        }
        self.clamp_diff_scroll();
    }

    fn move_tree_selection(&mut self, delta: isize) {
        let len = self.tree.visible_rows().len();
        if len == 0 {
            self.tree_selected = 0;
            return;
        }
        let next = (self.tree_selected as isize + delta).clamp(0, (len as isize) - 1);
        self.tree_selected = next as usize;
        self.selected_hunk = 0;
        self.reset_diff_scroll();
        self.ensure_tree_visible();
    }

    fn collapse_or_parent(&mut self) {
        let rows = self.tree.visible_rows();
        let Some(row) = rows.get(self.tree_selected).copied() else {
            return;
        };
        if row.is_dir && row.expanded {
            self.tree.set_collapsed(row.node_index, true);
            self.ensure_tree_selection_valid();
            self.ensure_tree_visible();
            return;
        }
        let node = &self.tree.nodes[row.node_index];
        let parent_path = std::path::Path::new(&node.path)
            .parent()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .filter(|p| !p.is_empty() && p != ".");
        if let Some(parent_path) = parent_path {
            if let Some(parent_vis) = self
                .tree
                .visible_rows()
                .into_iter()
                .position(|r| self.tree.nodes[r.node_index].path == parent_path)
            {
                self.tree_selected = parent_vis;
                self.selected_hunk = 0;
                self.reset_diff_scroll();
                self.ensure_tree_visible();
            }
        }
    }

    fn expand_or_focus_diff(&mut self) {
        let rows = self.tree.visible_rows();
        let Some(row) = rows.get(self.tree_selected).copied() else {
            return;
        };
        if row.is_dir {
            if !row.expanded {
                self.tree.set_collapsed(row.node_index, false);
                self.ensure_tree_selection_valid();
                self.ensure_tree_visible();
            }
        } else {
            self.selected_hunk = 0;
            self.focus = CodeReviewFocus::Diff;
            self.reset_diff_scroll();
        }
    }

    fn ensure_tree_selection_valid(&mut self) {
        let len = self.tree.visible_rows().len();
        if len == 0 {
            self.tree_selected = 0;
            return;
        }
        if self.tree_selected >= len {
            self.tree_selected = len - 1;
        }
    }

    fn ensure_tree_visible(&mut self) {
        let viewport = self.tree_viewport_rows.max(1);
        let len = self.tree.visible_rows().len();
        if len == 0 {
            self.tree_scroll = 0;
            return;
        }
        if self.tree_selected < self.tree_scroll {
            self.tree_scroll = self.tree_selected;
        } else if self.tree_selected >= self.tree_scroll.saturating_add(viewport) {
            self.tree_scroll = self
                .tree_selected
                .saturating_add(1)
                .saturating_sub(viewport);
        }
        let max_scroll = len.saturating_sub(viewport);
        if self.tree_scroll > max_scroll {
            self.tree_scroll = max_scroll;
        }
    }

    fn reset_diff_scroll(&mut self) {
        self.diff_scroll_y = 0;
        self.diff_scroll_x = 0;
    }

    /// Clamp diff scroll against current file and viewport.
    pub fn clamp_diff_scroll(&mut self) {
        let (line_count, max_line_width) = self.diff_metrics();
        let max_y = line_count.saturating_sub(self.diff_viewport_rows.max(1));
        if self.diff_scroll_y > max_y {
            self.diff_scroll_y = max_y;
        }
        let gutter = DIFF_GUTTER_WIDTH;
        let body_cols = self.diff_viewport_cols.saturating_sub(gutter).max(1);
        let max_x = max_line_width.saturating_sub(body_cols);
        if self.diff_scroll_x > max_x {
            self.diff_scroll_x = max_x;
        }
    }

    /// Update viewport sizes from the latest layout and clamp scroll state.
    pub fn set_viewports(&mut self, tree_rows: usize, diff_rows: usize, diff_cols: usize) {
        self.tree_viewport_rows = tree_rows.max(1);
        self.diff_viewport_rows = diff_rows.max(1);
        self.diff_viewport_cols = diff_cols.max(1);
        self.ensure_tree_selection_valid();
        self.ensure_tree_visible();
        self.clamp_diff_scroll();
    }

    /// Record absolute hit regions after layout (terminal coordinates).
    pub fn set_hit_regions(&mut self, tree_list: Rect, diff_body: Rect) {
        self.hits = CodeReviewHitRegions {
            tree_list,
            diff_body,
            tree_scroll: self.tree_scroll,
            diff_scroll_y: self.diff_scroll_y,
        };
    }

    fn diff_metrics(&self) -> (usize, usize) {
        let Some(file) = self.selected_file() else {
            return (0, 0);
        };
        if file.binary {
            return (2, 40);
        }
        let lines = build_diff_display_lines(self, file);
        let max_width = lines.iter().map(DiffDisplayLine::width).max().unwrap_or(0);
        (lines.len(), max_width)
    }
}

const DIFF_GUTTER_WIDTH: usize = 12;
const TREE_MIN_WIDTH: u16 = 24;
const TREE_MAX_WIDTH: u16 = 42;

fn point_in(area: Rect, col: u16, row: u16) -> bool {
    area.width > 0
        && area.height > 0
        && col >= area.x
        && row >= area.y
        && col < area.x.saturating_add(area.width)
        && row < area.y.saturating_add(area.height)
}

/// Layout geometry derived from the terminal frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodeReviewLayout {
    pub too_small: bool,
    pub tree_list: Rect,
    pub diff_body: Rect,
    pub tree_rows: usize,
    pub diff_rows: usize,
    pub diff_cols: usize,
    pub inner: Rect,
    pub footer: Rect,
    pub tree_pane: Rect,
    pub diff_pane: Rect,
}

/// Compute pane geometry for an outer frame area (including border).
#[must_use]
pub fn compute_layout(area: Rect) -> CodeReviewLayout {
    if area.width < 10 || area.height < 4 {
        return CodeReviewLayout {
            too_small: true,
            tree_list: Rect::default(),
            diff_body: Rect::default(),
            tree_rows: 1,
            diff_rows: 1,
            diff_cols: 1,
            inner: area,
            footer: Rect::default(),
            tree_pane: Rect::default(),
            diff_pane: Rect::default(),
        };
    }
    let block_inner = Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    let footer_height = 1.min(block_inner.height);
    let body_height = block_inner.height.saturating_sub(footer_height);
    let body = Rect {
        height: body_height,
        ..block_inner
    };
    let footer = Rect {
        y: block_inner.y.saturating_add(body_height),
        height: footer_height,
        ..block_inner
    };
    let tree_width = body
        .width
        .saturating_mul(32)
        .saturating_div(100)
        .clamp(TREE_MIN_WIDTH.min(body.width / 3).max(12), TREE_MAX_WIDTH)
        .min(body.width.saturating_sub(20).max(12));
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(tree_width),
            Constraint::Length(1),
            Constraint::Min(20),
        ])
        .split(body);
    let tree_pane = columns[0];
    let diff_pane = columns[2];
    let tree_header_h = 1.min(tree_pane.height);
    let tree_list = Rect {
        y: tree_pane.y.saturating_add(tree_header_h),
        height: tree_pane.height.saturating_sub(tree_header_h),
        ..tree_pane
    };
    let diff_header_h = 2.min(diff_pane.height);
    let diff_body = Rect {
        y: diff_pane.y.saturating_add(diff_header_h),
        height: diff_pane.height.saturating_sub(diff_header_h),
        ..diff_pane
    };
    CodeReviewLayout {
        too_small: false,
        tree_list,
        diff_body,
        tree_rows: usize::from(tree_list.height.max(1)),
        diff_rows: usize::from(diff_body.height.max(1)),
        diff_cols: usize::from(diff_body.width.max(1)),
        inner: block_inner,
        footer,
        tree_pane,
        diff_pane,
    }
}

/// Sync viewports + hit regions from the current terminal area before input.
pub fn sync_code_review_layout(panel: &mut CodeReviewPanel, area: Rect) {
    let layout = compute_layout(area);
    if layout.too_small {
        panel.set_viewports(1, 1, 1);
        panel.set_hit_regions(Rect::default(), Rect::default());
        return;
    }
    if panel.snapshot.error.is_some() {
        panel.set_viewports(1, layout.diff_rows, layout.diff_cols);
        panel.set_hit_regions(Rect::default(), Rect::default());
        return;
    }
    panel.set_viewports(layout.tree_rows, layout.diff_rows, layout.diff_cols);
    panel.set_hit_regions(layout.tree_list, layout.diff_body);
}

/// Render the fullscreen code-review overlay.
///
/// Call [`sync_code_review_layout`] before handling input so hit-testing matches
/// the last painted geometry. Render itself is pure and does not mutate state.
pub fn render_code_review_panel(
    frame: &mut ratatui::Frame<'_>,
    panel: &CodeReviewPanel,
    theme: Theme,
) {
    let area = frame.area();
    frame.render_widget(Clear, area);

    let title = format!(
        " Code review · {} · {} file{} · +{} −{} ",
        panel.snapshot.comparison_label(),
        panel.snapshot.files.len(),
        if panel.snapshot.files.len() == 1 {
            ""
        } else {
            "s"
        },
        panel.snapshot.total_insertions(),
        panel.snapshot.total_deletions(),
    );
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_accent));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let layout = compute_layout(area);
    if layout.too_small {
        frame.render_widget(
            Paragraph::new("Terminal too small for code review")
                .style(Style::default().fg(theme.warning)),
            inner,
        );
        return;
    }

    if panel.snapshot_loading {
        frame.render_widget(
            Paragraph::new("Loading code review snapshot…")
                .style(Style::default().fg(theme.muted)),
            layout.inner,
        );
        frame.render_widget(
            Paragraph::new("Esc/q close").style(Style::default().fg(theme.dim)),
            layout.footer,
        );
        return;
    }

    if let Some(error) = panel.snapshot.error.as_deref() {
        let lines = vec![
            Line::from(Span::styled(
                "Unable to load diff",
                Style::default()
                    .fg(theme.error)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(sanitize(error), Style::default().fg(theme.text))),
            Line::default(),
            Line::from(Span::styled("Esc close", Style::default().fg(theme.dim))),
        ];
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }),
            layout.inner,
        );
        frame.render_widget(
            Paragraph::new("Esc/q close").style(Style::default().fg(theme.dim)),
            layout.footer,
        );
        return;
    }

    render_tree_pane(frame, panel, layout.tree_pane, layout.tree_list, theme);
    let divider = Rect {
        x: layout.tree_pane.x.saturating_add(layout.tree_pane.width),
        y: layout.tree_pane.y,
        width: 1,
        height: layout.tree_pane.height,
    };
    if divider.width > 0 && divider.height > 0 {
        let style = Style::default().fg(theme.border);
        let lines = vec![
            Line::from(Span::styled("│", style));
            usize::from(divider.height)
        ];
        frame.render_widget(Paragraph::new(lines), divider);
    }
    render_diff_pane(frame, panel, layout.diff_pane, layout.diff_body, theme);

    let focus_label = match panel.focus {
        CodeReviewFocus::Tree => "tree",
        CodeReviewFocus::Diff => "diff",
    };
    let footer_text = format!(
        "focus:{focus_label} · Tab pane · j/k hunk · c comment · Space fold · click select · r refresh · Alt+R refork · Esc close"
    );
    frame.render_widget(
        Paragraph::new(truncate_width(
            &footer_text,
            usize::from(layout.footer.width),
        ))
        .style(Style::default().fg(theme.dim)),
        layout.footer,
    );
}

fn render_tree_pane(
    frame: &mut ratatui::Frame<'_>,
    panel: &CodeReviewPanel,
    pane: Rect,
    list: Rect,
    theme: Theme,
) {
    let header = Rect {
        height: 1.min(pane.height),
        ..pane
    };
    let focused = panel.focus == CodeReviewFocus::Tree;
    let title_style = if focused {
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.dim)
    };
    frame.render_widget(Paragraph::new(Span::styled(" Files ", title_style)), header);

    let rows = panel.tree.visible_rows();
    if rows.is_empty() {
        let msg = if panel.snapshot.truncated {
            " Diff truncated · no files parsed ".to_owned()
        } else {
            format!(" No changes · {} ", panel.snapshot.comparison_label())
        };
        frame.render_widget(
            Paragraph::new(Span::styled(msg, Style::default().fg(theme.muted))),
            list,
        );
        return;
    }

    let viewport = usize::from(list.height).max(1);
    let start = panel.tree_scroll.min(rows.len().saturating_sub(1));
    let mut lines = Vec::with_capacity(viewport);
    for (offset, row) in rows.iter().enumerate().skip(start).take(viewport) {
        let visible_index = offset;
        let node = &panel.tree.nodes[row.node_index];
        let selected = visible_index == panel.tree_selected;
        let base_style = if selected {
            Style::default()
                .fg(if row.is_dir { theme.muted } else { theme.text })
                .bg(theme.selected_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(if row.is_dir { theme.muted } else { theme.text })
        };
        let marker = if row.is_dir {
            if row.expanded { "▾" } else { "▸" }
        } else {
            " "
        };
        let status = node.status.map(FileStatus::as_str).unwrap_or(" ");
        let indent = "  ".repeat(row.depth);
        let counts = if node.insertions > 0 || node.deletions > 0 {
            format!(" +{} −{}", node.insertions, node.deletions)
        } else {
            String::new()
        };
        let label_prefix = format!("{indent}{marker} {status} ");
        let width = usize::from(list.width).max(1);
        let counts_width = UnicodeWidthStr::width(counts.as_str());
        let label_budget = width.saturating_sub(counts_width).max(1);
        let name_budget = label_budget.saturating_sub(UnicodeWidthStr::width(label_prefix.as_str()));
        let name = truncate_width(&node.name, name_budget);
        let label_width = UnicodeWidthStr::width(label_prefix.as_str())
            .saturating_add(UnicodeWidthStr::width(name.as_str()));
        let pad = width.saturating_sub(label_width).saturating_sub(counts_width);
        let status_color = node
            .status
            .map(|value| status_color(value, theme))
            .unwrap_or(theme.dim);
        let status_style = base_style.patch(Style::default().fg(status_color));
        let mut spans = vec![
            Span::styled(format!("{indent}{marker} "), base_style),
            Span::styled(status.to_owned(), status_style),
            Span::styled(format!(" {name}{}", " ".repeat(pad)), base_style),
        ];
        if node.insertions > 0 || node.deletions > 0 {
            spans.push(Span::styled(
                format!(" +{}", node.insertions),
                base_style.patch(Style::default().fg(theme.tool_diff_added)),
            ));
            spans.push(Span::styled(
                format!(" −{}", node.deletions),
                base_style.patch(Style::default().fg(theme.tool_diff_removed)),
            ));
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(lines), list);
}

fn render_diff_pane(
    frame: &mut ratatui::Frame<'_>,
    panel: &CodeReviewPanel,
    pane: Rect,
    body: Rect,
    theme: Theme,
) {
    let header_h = 2.min(pane.height);
    let header = Rect {
        height: header_h,
        ..pane
    };
    let focused = panel.focus == CodeReviewFocus::Diff;
    let title_style = if focused {
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.dim)
    };

    let Some(file) = panel.selected_file() else {
        let selected_name = panel
            .tree
            .visible_rows()
            .get(panel.tree_selected)
            .map(|row| panel.tree.nodes[row.node_index].path.as_str())
            .unwrap_or("(none)");
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(" Diff ", title_style)),
                Line::from(Span::styled(
                    format!(" {selected_name}"),
                    Style::default().fg(theme.muted),
                )),
            ]),
            header,
        );
        frame.render_widget(
            Paragraph::new(Span::styled(
                " Select a file to view its diff ",
                Style::default().fg(theme.muted),
            )),
            body,
        );
        return;
    };

    let mut header_lines = Vec::new();
    let rename = file
        .previous_path
        .as_deref()
        .map(|prev| format!(" ({prev} → {})", file.path))
        .unwrap_or_default();
    header_lines.push(Line::from(vec![
        Span::styled(" Diff ", title_style),
        Span::styled(
            format!("{} ", file.status.as_str()),
            Style::default()
                .fg(status_color(file.status, theme))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(file.path.clone(), Style::default().fg(theme.text)),
        Span::styled(rename, Style::default().fg(theme.dim)),
    ]));
    if header_h > 1 {
        let mut suffix = String::new();
        if file.binary {
            suffix.push_str(" · binary");
        }
        if file.truncated {
            suffix.push_str(" · truncated");
        }
        if panel.snapshot.truncated {
            suffix.push_str(" · snapshot truncated");
        }
        let meta_bg = theme.tool_pending_bg;
        let meta_text_width = UnicodeWidthStr::width(file.status.label())
            .saturating_add(UnicodeWidthStr::width(suffix.as_str()))
            .saturating_add(file.insertions.to_string().len())
            .saturating_add(file.deletions.to_string().len())
            .saturating_add(8);
        let meta_pad = usize::from(header.width).saturating_sub(meta_text_width);
        header_lines.push(Line::from(vec![
            Span::styled(
                format!(" {} · ", file.status.label()),
                Style::default().fg(theme.muted).bg(meta_bg),
            ),
            Span::styled(
                format!("+{}", file.insertions),
                Style::default().fg(theme.tool_diff_added).bg(meta_bg),
            ),
            Span::styled(
                format!(" −{}", file.deletions),
                Style::default().fg(theme.tool_diff_removed).bg(meta_bg),
            ),
            Span::styled(
                format!("{suffix}{}", " ".repeat(meta_pad)),
                Style::default().fg(theme.dim).bg(meta_bg),
            ),
        ]));
    }
    frame.render_widget(Paragraph::new(header_lines), header);

    if file.binary {
        let msg = file
            .message
            .clone()
            .unwrap_or_else(|| "Binary file; content not shown".to_owned());
        frame.render_widget(
            Paragraph::new(Span::styled(format!(" {msg} "), Style::default().fg(theme.warning))),
            body,
        );
        return;
    }

    let display = build_diff_display_lines(panel, file);
    if display.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(" No textual hunks ", Style::default().fg(theme.muted))),
            body,
        );
        return;
    }
    let viewport = usize::from(body.height).max(1);
    let width = usize::from(body.width).max(1);
    let start = panel.diff_scroll_y.min(display.len().saturating_sub(1));
    let lines = display
        .iter()
        .skip(start)
        .take(viewport)
        .map(|entry| render_diff_line(entry, panel.diff_scroll_x, width, theme, panel.selected_hunk))
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), body);

    if let Some(editor) = panel.comment_editor.as_deref() {
        let editor_height = 3.min(body.height);
        let editor_area = Rect { y: body.bottom().saturating_sub(editor_height), height: editor_height, ..body };
        let block = Block::default()
            .title(" Comment · Enter submit · Esc cancel ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent));
        let inner = block.inner(editor_area);
        frame.render_widget(Clear, editor_area);
        frame.render_widget(block, editor_area);
        frame.render_widget(Paragraph::new(clean_terminal_text(editor)).wrap(Wrap { trim: false }), inner);
        let prefix = clean_terminal_text(&editor[..panel.comment_cursor.min(editor.len())]);
        let cursor_x = inner.x.saturating_add(u16::try_from(UnicodeWidthStr::width(prefix.lines().last().unwrap_or(""))).unwrap_or(u16::MAX));
        let cursor_y = inner.y.saturating_add(u16::try_from(prefix.lines().count().saturating_sub(1)).unwrap_or(u16::MAX));
        frame.set_cursor_position((cursor_x.min(inner.right().saturating_sub(1)), cursor_y.min(inner.bottom().saturating_sub(1))));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiffDisplayKind {
    HunkHeader,
    Diff,
    AnnotationHeader,
    AnnotationBody,
    /// Bottom border row of a comment/error card frame (see
    /// [`push_annotation_card`]).
    AnnotationBottom,
    AnnotationSpacer,
    /// One row of a markdown-rendered assistant reply inside a bordered box.
    /// The whole box is laid out as one display row per terminal row so
    /// scrolling and mouse hit-testing stay 1:1 with what is painted.
    MarkdownBody,
    Meta,
}

/// One row of a reply box. `row` 0 is the top border with the label,
/// `1..=rows` are markdown content rows, and `total - 1` is the bottom border.
/// The full source is carried so the draw pass can re-render the row with the
/// live theme; the box width is derived from the diff pane width at draw time.
#[derive(Clone)]
struct ReplyMarkdownBox {
    source: String,
    label: String,
    row: usize,
    total: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AnnotationTone {
    User,
    Assistant,
    Error,
}

#[derive(Clone)]
struct DiffDisplayLine {
    old_no: Option<u32>,
    new_no: Option<u32>,
    diff_kind: DiffLineKind,
    kind: DiffDisplayKind,
    hunk_index: Option<usize>,
    annotation_tone: Option<AnnotationTone>,
    text: String,
    /// Present only for [`DiffDisplayKind::MarkdownBody`] rows.
    markdown: Option<ReplyMarkdownBox>,
}

impl DiffDisplayLine {
    fn width(&self) -> usize {
        UnicodeWidthStr::width(self.text.as_str())
    }
}

fn push_diff_display_line(out: &mut Vec<DiffDisplayLine>, line: DiffDisplayLine) -> bool {
    let limit = crate::code_review::MAX_FILE_RENDER_LINES;
    if out.len() >= limit.saturating_sub(1) {
        if out.len() == limit.saturating_sub(1) {
            out.push(DiffDisplayLine {
                old_no: None,
                new_no: None,
                diff_kind: DiffLineKind::Meta,
                kind: DiffDisplayKind::Meta,
                hunk_index: None,
                annotation_tone: None,
                text: "… truncated …".to_owned(),
                markdown: None,
            });
        }
        return false;
    }
    out.push(line);
    true
}

fn wrap_annotation_body(value: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let clean = clean_terminal_text(value);
    let mut out = Vec::new();
    for source in clean.split('\n') {
        if source.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut current = String::new();
        let mut current_width: usize = 0;
        for word in source.split_inclusive(char::is_whitespace) {
            let word_width = UnicodeWidthStr::width(word);
            if current_width > 0 && current_width.saturating_add(word_width) > width {
                out.push(current.trim_end().to_owned());
                current.clear();
                current_width = 0;
            }
            if word_width <= width {
                current.push_str(word);
                current_width = current_width.saturating_add(word_width);
                continue;
            }
            for character in word.chars() {
                let character_width = unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
                if current_width > 0 && current_width.saturating_add(character_width) > width {
                    out.push(current.trim_end().to_owned());
                    current.clear();
                    current_width = 0;
                }
                current.push(character);
                current_width = current_width.saturating_add(character_width);
            }
        }
        out.push(current.trim_end().to_owned());
    }
    out
}

/// Lay out a comment (or ⚠ error) card as a complete bordered frame: one
/// display row per terminal row (`╭─ Comment 1 ─╮` / `│ … │` / `╰─…─╯`), using
/// the same width math as the assistant reply box. `box_width` is the outer
/// frame width (diff pane width − 4), matching [`push_reply_box`]; the wrapped
/// body shares the frame's inner width. The surrounding blank rows are the
/// thread block's own spacers (see [`build_diff_display_lines`]); cards stack
/// directly so the exchange reads as one contiguous run of frames.
fn push_annotation_card(
    out: &mut Vec<DiffDisplayLine>,
    hunk_index: usize,
    header: String,
    body: &str,
    box_width: usize,
    tone: AnnotationTone,
) -> bool {
    if !push_diff_display_line(out, DiffDisplayLine {
        old_no: None,
        new_no: None,
        diff_kind: DiffLineKind::Meta,
        kind: DiffDisplayKind::AnnotationHeader,
        hunk_index: Some(hunk_index),
        annotation_tone: Some(tone),
        text: header,
        markdown: None,
    }) {
        return false;
    }
    let content_width = box_width.saturating_sub(4).max(1);
    for text in wrap_annotation_body(body, content_width) {
        if !push_diff_display_line(out, DiffDisplayLine {
            old_no: None,
            new_no: None,
            diff_kind: DiffLineKind::Meta,
            kind: DiffDisplayKind::AnnotationBody,
            hunk_index: Some(hunk_index),
            annotation_tone: Some(tone),
            text,
            markdown: None,
        }) {
            return false;
        }
    }
    // Close the frame: the bottom border shares the top/side rows' width so
    // the card reads as one complete box instead of an open top edge.
    if !push_diff_display_line(out, DiffDisplayLine {
        old_no: None,
        new_no: None,
        diff_kind: DiffLineKind::Meta,
        kind: DiffDisplayKind::AnnotationBottom,
        hunk_index: Some(hunk_index),
        annotation_tone: Some(tone),
        text: String::new(),
        markdown: None,
    }) {
        return false;
    }
    true
}

/// Lay out an assistant reply as a markdown-rendered bordered box: one display
/// row per terminal row (`╭─ label ─╮` / `│ … │` / `╰─…─╯`). The full source
/// rides on every row so the draw pass re-renders with the live theme; `text`
/// carries the plain row content for tests and width metrics.
fn push_reply_box(
    out: &mut Vec<DiffDisplayLine>,
    hunk_index: usize,
    label: String,
    source: &str,
    box_width: usize,
) -> bool {
    let content_width = box_width.saturating_sub(4).max(1);
    // The card path sanitizes bodies before wrapping; the reply box does the
    // same so terminal control sequences never reach the markdown renderer.
    let clean = clean_terminal_text(source);
    let rendered = pi_coding::markdown::render_markdown(
        &clean,
        &pi_coding::markdown::MarkdownRenderOptions {
            width: content_width,
            ..pi_coding::markdown::MarkdownRenderOptions::default()
        },
    );
    let rows = rendered.plain_lines();
    let total = rows.len().saturating_add(2);
    if !push_diff_display_line(out, DiffDisplayLine {
        old_no: None,
        new_no: None,
        diff_kind: DiffLineKind::Meta,
        kind: DiffDisplayKind::MarkdownBody,
        hunk_index: Some(hunk_index),
        annotation_tone: Some(AnnotationTone::Assistant),
        text: String::new(),
        markdown: Some(ReplyMarkdownBox {
            source: clean.clone(),
            label: label.clone(),
            row: 0,
            total,
        }),
    }) {
        return false;
    }
    for (index, text) in rows.into_iter().enumerate() {
        if !push_diff_display_line(out, DiffDisplayLine {
            old_no: None,
            new_no: None,
            diff_kind: DiffLineKind::Meta,
            kind: DiffDisplayKind::MarkdownBody,
            hunk_index: Some(hunk_index),
            annotation_tone: Some(AnnotationTone::Assistant),
            text: text.clone(),
            markdown: Some(ReplyMarkdownBox {
                source: clean.clone(),
                label: label.clone(),
                row: index + 1,
                total,
            }),
        }) {
            return false;
        }
    }
    if !push_diff_display_line(out, DiffDisplayLine {
        old_no: None,
        new_no: None,
        diff_kind: DiffLineKind::Meta,
        kind: DiffDisplayKind::MarkdownBody,
        hunk_index: Some(hunk_index),
        annotation_tone: Some(AnnotationTone::Assistant),
        text: String::new(),
        markdown: Some(ReplyMarkdownBox {
            source: clean,
            label,
            row: total.saturating_sub(1),
            total,
        }),
    }) {
        return false;
    }
    true
}

fn build_diff_display_lines(panel: &CodeReviewPanel, file: &DiffFile) -> Vec<DiffDisplayLine> {
    let mut out = Vec::new();
    let annotation_width = panel.diff_viewport_cols.saturating_sub(4).max(1);
    for (hunk_index, hunk) in file.hunks.iter().enumerate() {
        let selected = hunk_index == panel.selected_hunk;
        let identity = panel.snapshot.hunk_identity(file, hunk);
        // Space (and `o`) folds the selected hunk: unchanged context lines
        // collapse to the hunk header (with counts) while changed +/- lines
        // and the thread cards stay visible so the diff and its review
        // exchange remain readable after folding. The fold state is
        // `collapsed_threads`, keyed by hunk identity so it survives
        // snapshot refreshes.
        let folded = panel.collapsed_threads.contains(&identity);
        let header_text = if folded {
            let context_count = hunk
                .lines
                .iter()
                .filter(|line| line.kind == DiffLineKind::Context)
                .count();
            let changed = hunk.lines.len().saturating_sub(context_count);
            let mut text = format!(
                "{} · {context_count} line{} folded",
                hunk.header,
                if context_count == 1 { "" } else { "s" },
            );
            if changed > 0 {
                text.push_str(&format!(" · {changed} changed"));
            }
            text
        } else {
            hunk.header.clone()
        };
        if !push_diff_display_line(&mut out, DiffDisplayLine {
            old_no: None,
            new_no: None,
            diff_kind: DiffLineKind::Meta,
            kind: DiffDisplayKind::HunkHeader,
            hunk_index: Some(hunk_index),
            annotation_tone: None,
            text: header_text,
            markdown: None,
        }) {
            return out;
        }
        if selected && panel.comment_editor.is_some()
            && !push_diff_display_line(&mut out, DiffDisplayLine {
                old_no: None,
                new_no: None,
                diff_kind: DiffLineKind::Meta,
                kind: DiffDisplayKind::Meta,
                hunk_index: Some(hunk_index),
                annotation_tone: None,
                text: "comment editor open".to_owned(),
                markdown: None,
            })
        {
            return out;
        }
        if let Some(thread) = panel.threads.get(&identity) {
            // Thread annotations attach immediately after the hunk header (and the
            // comment-editor marker) so comments render next to the header they
            // belong to, before the hunk's diff lines. The cards render inline in
            // both fold states: folding hides only unchanged context, never the
            // review exchange, so the thread stays readable alongside the changes.
            if !push_diff_display_line(&mut out, DiffDisplayLine {
                old_no: None,
                new_no: None,
                diff_kind: DiffLineKind::Meta,
                kind: DiffDisplayKind::AnnotationSpacer,
                hunk_index: Some(hunk_index),
                annotation_tone: None,
                text: String::new(),
                markdown: None,
            }) {
                return out;
            }
            let mut exchange_number = 0;
            for comment in &thread.comments {
                match comment.role {
                    ReviewCommentRole::User => {
                        exchange_number += 1;
                        // Comment lines keep the plain card surface.
                        if !push_annotation_card(
                            &mut out,
                            hunk_index,
                            format!("Comment {exchange_number}"),
                            &comment.text,
                            annotation_width,
                            AnnotationTone::User,
                        ) {
                            return out;
                        }
                    }
                    ReviewCommentRole::Assistant => {
                        // Replies render as markdown (tables included) inside a
                        // bordered box labeled with the answer number.
                        let label = format!(
                            "Answer {}{}",
                            exchange_number.max(1),
                            if comment.partial { " · aborted" } else { "" },
                        );
                        if !push_reply_box(
                            &mut out,
                            hunk_index,
                            label,
                            &comment.text,
                            annotation_width,
                        ) {
                            return out;
                        }
                    }
                    ReviewCommentRole::System => {
                        if !push_annotation_card(
                            &mut out,
                            hunk_index,
                            "⚠ Error".to_owned(),
                            &comment.text,
                            annotation_width,
                            AnnotationTone::Error,
                        ) {
                            return out;
                        }
                    }
                }
            }
            if !thread.streaming_text.is_empty()
                && !push_reply_box(
                    &mut out,
                    hunk_index,
                    format!("Answer {} · streaming", exchange_number.max(1)),
                    &thread.streaming_text,
                    annotation_width,
                )
            {
                return out;
            }
            if let Some(error) = thread.error.as_deref()
                && !push_annotation_card(
                    &mut out,
                    hunk_index,
                    "⚠ Error".to_owned(),
                    error,
                    annotation_width,
                    AnnotationTone::Error,
                )
            {
                return out;
            }
            if !push_diff_display_line(&mut out, DiffDisplayLine {
                old_no: None,
                new_no: None,
                diff_kind: DiffLineKind::Meta,
                kind: DiffDisplayKind::AnnotationSpacer,
                hunk_index: Some(hunk_index),
                annotation_tone: None,
                text: String::new(),
                markdown: None,
            }) {
                return out;
            }
        }
        for line in &hunk.lines {
            // Folded hunks hide only unchanged context; added/removed lines
            // (and meta markers like `\ No newline at end of file`) stay
            // visible under the folded header so the changes never disappear.
            if folded && line.kind == DiffLineKind::Context {
                continue;
            }
            if !push_diff_display_line(&mut out, DiffDisplayLine {
                old_no: line.old_no,
                new_no: line.new_no,
                diff_kind: line.kind,
                kind: DiffDisplayKind::Diff,
                hunk_index: Some(hunk_index),
                annotation_tone: None,
                text: line.text.clone(),
                markdown: None,
            }) {
                return out;
            }
        }
    }
    if file.truncated {
        let _ = push_diff_display_line(&mut out, DiffDisplayLine {
            old_no: None,
            new_no: None,
            diff_kind: DiffLineKind::Meta,
            kind: DiffDisplayKind::Meta,
            hunk_index: None,
            annotation_tone: None,
            text: "… file truncated …".to_owned(),
            markdown: None,
        });
    }
    out
}

fn render_diff_line(
    line: &DiffDisplayLine,
    scroll_x: usize,
    width: usize,
    theme: Theme,
    selected_hunk: usize,
) -> Line<'static> {
    if line.kind == DiffDisplayKind::MarkdownBody {
        let Some(md) = line.markdown.as_ref() else {
            // Malformed row: render the text as a plain annotation body line.
            let body_budget = width.saturating_sub(4).max(1);
            let body = slice_width(&line.text, 0, body_budget);
            return Line::from(vec![
                Span::styled("  │ ", Style::default().fg(theme.text)),
                Span::styled(body, Style::default().fg(theme.text)),
            ]);
        };
        let box_outer = width.saturating_sub(4).max(4);
        let border_style = Style::default().fg(theme.accent);
        if md.row == 0 {
            // Top border with the answer label: ╭─ Answer 1 ───────╮
            // The row is `  ╭─ label{dashes}╮`: 2 indent + ╭ + ─ + space +
            // label + dashes + ╮. Dashes must fill the rest of the box so the
            // top shares the content/bottom row width (box_outer + 2) — one
            // less dash and the top border lands one column short of the
            // side-bordered rows' right edge.
            let label_budget = box_outer.saturating_sub(5);
            let label = truncate_width(&md.label, label_budget);
            let dashes = "─".repeat(
                box_outer
                    .saturating_sub(4)
                    .saturating_sub(UnicodeWidthStr::width(label.as_str())),
            );
            return Line::from(Span::styled(
                format!("  ╭─ {label}{dashes}╮"),
                border_style,
            ));
        }
        if md.row.saturating_add(1) >= md.total {
            // Bottom border: ╰──────────────╯
            return Line::from(Span::styled(
                format!("  ╰{}╯", "─".repeat(box_outer.saturating_sub(2))),
                border_style,
            ));
        }
        // Content row: re-render the reply with the live theme and pick this row.
        let content_width = box_outer.saturating_sub(4).max(1);
        let styles = markdown_ratatui_styles(theme, theme.text);
        let rendered = render_ratatui_markdown(
            &md.source,
            u16::try_from(content_width).unwrap_or(u16::MAX),
            styles,
        );
        let content = rendered
            .lines
            .get(md.row.saturating_sub(1))
            .cloned()
            .unwrap_or_else(|| Line::from(Span::styled(line.text.clone(), styles.text)));
        let pad = content_width.saturating_sub(usize::from(content.width()));
        let mut spans = Vec::with_capacity(content.spans.len() + 2);
        spans.push(Span::styled("  │ ", border_style));
        spans.extend(content.spans);
        spans.push(Span::styled(format!("{} │", " ".repeat(pad)), border_style));
        return Line::from(spans);
    }
    if line.kind != DiffDisplayKind::Diff {
        // Comment cards (and ⚠ error cards) render as complete bordered frames
        // using the answer card's width math: every frame row is `box_outer + 2`
        // columns wide starting 2 columns into the pane. The border color
        // follows the tone — the theme's green for user comments, red for
        // errors — while the body keeps the tone's surface.
        if matches!(
            line.kind,
            DiffDisplayKind::AnnotationHeader
                | DiffDisplayKind::AnnotationBody
                | DiffDisplayKind::AnnotationBottom
        ) {
            let box_outer = width.saturating_sub(4).max(4);
            let (border_color, body_color, background) = match line.annotation_tone {
                Some(AnnotationTone::User) => (theme.success, theme.user_message_text, Some(theme.user_message_bg)),
                Some(AnnotationTone::Assistant) => (theme.accent, theme.text, Some(theme.tool_pending_bg)),
                Some(AnnotationTone::Error) => (theme.error, theme.text, Some(theme.tool_error_bg)),
                None => (theme.accent, theme.text, Some(theme.tool_pending_bg)),
            };
            let mut border = Style::default().fg(border_color);
            let mut body_style = Style::default().fg(body_color);
            if let Some(background) = background {
                border = border.bg(background);
                body_style = body_style.bg(background);
            }
            return match line.kind {
                DiffDisplayKind::AnnotationHeader => {
                    // Top border with the title: ╭─ Comment 1 ───────╮. The
                    // row is `  ╭─ label{dashes}╮`: 2 indent + ╭ + ─ + space +
                    // label + dashes + ╮. Dashes fill the rest of the box so
                    // the top shares the content/bottom row width
                    // (box_outer + 2) — the answer card's exact math.
                    let label_budget = box_outer.saturating_sub(5);
                    let label = truncate_width(&line.text, label_budget);
                    let dashes = "─".repeat(
                        box_outer
                            .saturating_sub(4)
                            .saturating_sub(UnicodeWidthStr::width(label.as_str())),
                    );
                    Line::from(vec![Span::styled(
                        format!("  ╭─ {label}{dashes}╮"),
                        border.add_modifier(Modifier::BOLD),
                    )])
                }
                DiffDisplayKind::AnnotationBody => {
                    let content_width = box_outer.saturating_sub(4).max(1);
                    let body = slice_width(&line.text, 0, content_width);
                    let pad = content_width.saturating_sub(UnicodeWidthStr::width(body.as_str()));
                    Line::from(vec![
                        Span::styled("  │ ", border),
                        Span::styled(body, body_style),
                        Span::styled(format!("{} │", " ".repeat(pad)), border),
                    ])
                }
                DiffDisplayKind::AnnotationBottom => Line::from(vec![Span::styled(
                    format!("  ╰{}╯", "─".repeat(box_outer.saturating_sub(2))),
                    border,
                )]),
                _ => unreachable!(),
            };
        }
        let selected = line.hunk_index == Some(selected_hunk);
        let (prefix, color, background, bold) = match line.kind {
            DiffDisplayKind::HunkHeader => (
                if selected { "▸ " } else { "@ " },
                if selected { theme.accent } else { theme.muted },
                Some(theme.tool_pending_bg),
                selected,
            ),
            DiffDisplayKind::AnnotationSpacer => ("", theme.border_muted, None, false),
            DiffDisplayKind::Meta => ("@ ", theme.accent, Some(theme.tool_pending_bg), false),
            // Annotation frames return from the branch above; MarkdownBody and
            // Diff rows are handled before this block.
            DiffDisplayKind::AnnotationHeader
                | DiffDisplayKind::AnnotationBody
                | DiffDisplayKind::AnnotationBottom
                | DiffDisplayKind::MarkdownBody
                | DiffDisplayKind::Diff => unreachable!(),
        };
        let body_budget = width.saturating_sub(UnicodeWidthStr::width(prefix)).max(1);
        let mut body = slice_width(
            &line.text,
            if line.kind == DiffDisplayKind::AnnotationSpacer {
                0
            } else {
                scroll_x
            },
            body_budget,
        );
        if background.is_some() {
            body.push_str(&" ".repeat(body_budget.saturating_sub(UnicodeWidthStr::width(body.as_str()))));
        }
        let mut style = Style::default().fg(color);
        if let Some(background) = background {
            style = style.bg(background);
        }
        if bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        return Line::from(vec![Span::styled(prefix, style), Span::styled(body, style)]);
    }
    let (marker, marker_color, background, body_color) = match line.diff_kind {
        DiffLineKind::Addition => ('+', theme.tool_diff_added, Some(diff_tint(theme.tool_success_bg, theme.tool_diff_added)), theme.text),
        DiffLineKind::Deletion => ('-', theme.tool_diff_removed, Some(diff_tint(theme.tool_error_bg, theme.tool_diff_removed)), theme.text),
        DiffLineKind::Context => (' ', theme.dim, None, theme.text),
        DiffLineKind::Meta => ('@', theme.accent, Some(theme.tool_pending_bg), theme.accent),
    };
    let old = line.old_no.map(|n| format!("{n:>4}")).unwrap_or_else(|| "    ".to_owned());
    let new = line.new_no.map(|n| format!("{n:>4}")).unwrap_or_else(|| "    ".to_owned());
    let numbers = format!("{old} {new} ");
    let gutter_width = UnicodeWidthStr::width(numbers.as_str()).saturating_add(2);
    let body_budget = width.saturating_sub(gutter_width).max(1);
    let mut body = slice_width(&line.text, scroll_x, body_budget);
    if background.is_some() { body.push_str(&" ".repeat(body_budget.saturating_sub(UnicodeWidthStr::width(body.as_str())))); }
    let background_style = background.map_or_else(Style::default, |color| Style::default().bg(color));
    Line::from(vec![
        Span::styled(numbers, background_style.patch(Style::default().fg(theme.dim))),
        Span::styled(format!("{marker} "), background_style.patch(Style::default().fg(marker_color).add_modifier(Modifier::BOLD))),
        Span::styled(body, background_style.patch(Style::default().fg(body_color))),
    ])
}

fn diff_tint(surface: ratatui::style::Color, accent: ratatui::style::Color) -> ratatui::style::Color {
    match (surface, accent) {
        (
            ratatui::style::Color::Rgb(surface_r, surface_g, surface_b),
            ratatui::style::Color::Rgb(accent_r, accent_g, accent_b),
        ) => ratatui::style::Color::Rgb(
            mix_channel(surface_r, accent_r),
            mix_channel(surface_g, accent_g),
            mix_channel(surface_b, accent_b),
        ),
        _ => surface,
    }
}

const fn mix_channel(surface: u8, accent: u8) -> u8 {
    let mixed = (surface as u16) * 7 + accent as u16;
    (mixed / 8) as u8
}

fn status_color(status: FileStatus, theme: Theme) -> ratatui::style::Color {
    match status {
        FileStatus::Added | FileStatus::Copied => theme.success,
        FileStatus::Deleted => theme.error,
        FileStatus::Modified => theme.warning,
        FileStatus::Renamed => theme.accent,
        FileStatus::Binary => theme.warning,
        FileStatus::Unknown => theme.muted,
    }
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate_width(value: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(value) <= max {
        return value.to_owned();
    }
    let mut out = String::new();
    let mut width = 0usize;
    for ch in value.chars() {
        let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_w + 1 > max {
            break;
        }
        out.push(ch);
        width += ch_w;
    }
    out.push('…');
    out
}

fn slice_width(value: &str, skip: usize, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let mut skipped = 0usize;
    let mut started = skip == 0;
    let mut out = String::new();
    let mut width = 0usize;
    for ch in value.chars() {
        let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if !started {
            skipped = skipped.saturating_add(ch_w);
            if skipped >= skip {
                started = true;
            }
            continue;
        }
        if width + ch_w > max {
            break;
        }
        out.push(ch);
        width += ch_w;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_review::{DiffFile, DiffHunk, DiffLine, DiffLineKind, FileStatus, ReviewSnapshot};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    fn sample_file(path: &str, lines: Vec<DiffLine>) -> DiffFile {
        let insertions = lines
            .iter()
            .filter(|l| l.kind == DiffLineKind::Addition)
            .count();
        let deletions = lines
            .iter()
            .filter(|l| l.kind == DiffLineKind::Deletion)
            .count();
        DiffFile {
            path: path.to_owned(),
            previous_path: None,
            status: FileStatus::Modified,
            binary: false,
            insertions,
            deletions,
            hunks: vec![DiffHunk {
                header: "@@ -1,3 +1,4 @@".to_owned(),
                old_start: 1,
                old_count: 3,
                new_start: 1,
                new_count: 4,
                lines,
            }],
            truncated: false,
            message: None,
        }
    }

    fn sample_snapshot() -> ReviewSnapshot {
        ReviewSnapshot {
            root: PathBuf::from("repo"),
            scope: ReviewScope::WorkingTree,
            snapshot_id: "snapshot".to_owned(),
            files: vec![
                sample_file(
                    "src/a.rs",
                    vec![
                        DiffLine {
                            kind: DiffLineKind::Context,
                            old_no: Some(1),
                            new_no: Some(1),
                            text: "fn main() {".into(),
                        },
                        DiffLine {
                            kind: DiffLineKind::Deletion,
                            old_no: Some(2),
                            new_no: None,
                            text: "    old();".into(),
                        },
                        DiffLine {
                            kind: DiffLineKind::Addition,
                            old_no: None,
                            new_no: Some(2),
                            text: "    new();".into(),
                        },
                        DiffLine {
                            kind: DiffLineKind::Context,
                            old_no: Some(3),
                            new_no: Some(3),
                            text: "}".into(),
                        },
                    ],
                ),
                sample_file(
                    "src/nested/b.rs",
                    (1..40)
                        .map(|i| DiffLine {
                            kind: if i % 2 == 0 {
                                DiffLineKind::Addition
                            } else {
                                DiffLineKind::Context
                            },
                            old_no: Some(i),
                            new_no: Some(i),
                            text: format!("line {i} {}", "x".repeat(80)),
                        })
                        .collect(),
                ),
                DiffFile {
                    path: "README.md".into(),
                    previous_path: None,
                    status: FileStatus::Modified,
                    binary: false,
                    insertions: 1,
                    deletions: 0,
                    hunks: vec![DiffHunk {
                        header: "@@ -1 +1,2 @@".into(),
                        old_start: 1,
                        old_count: 1,
                        new_start: 1,
                        new_count: 2,
                        lines: vec![
                            DiffLine {
                                kind: DiffLineKind::Context,
                                old_no: Some(1),
                                new_no: Some(1),
                                text: "hello".into(),
                            },
                            DiffLine {
                                kind: DiffLineKind::Addition,
                                old_no: None,
                                new_no: Some(2),
                                text: "world".into(),
                            },
                        ],
                    }],
                    truncated: false,
                    message: None,
                },
            ],
            truncated: false,
            error: None,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn click(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn wheel(col: u16, row: u16, up: bool) -> MouseEvent {
        MouseEvent {
            kind: if up {
                MouseEventKind::ScrollUp
            } else {
                MouseEventKind::ScrollDown
            },
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn prepared_panel() -> CodeReviewPanel {
        let mut panel = CodeReviewPanel::from_snapshot(sample_snapshot());
        let area = Rect::new(0, 0, 100, 30);
        sync_code_review_layout(&mut panel, area);
        panel
    }

    #[test]
    fn tree_collapse_and_selection_stay_valid() {
        let mut panel = CodeReviewPanel::from_snapshot(sample_snapshot());
        panel.set_viewports(10, 10, 80);
        let before = panel.tree.visible_rows().len();
        assert!(before >= 3);

        let src_vis = panel
            .tree
            .visible_rows()
            .into_iter()
            .position(|row| panel.tree.nodes[row.node_index].path == "src")
            .expect("src");
        panel.tree_selected = src_vis;
        assert_eq!(
            panel.handle_key(key(KeyCode::Left)),
            CodeReviewPanelResult::Handled
        );
        let after = panel.tree.visible_rows().len();
        assert!(after < before);
        assert!(panel.tree_selected < after || after == 0);

        assert_eq!(
            panel.handle_key(key(KeyCode::Right)),
            CodeReviewPanelResult::Handled
        );
        assert_eq!(panel.tree.visible_rows().len(), before);
    }

    #[test]
    fn right_pane_scroll_and_clamp() {
        let mut panel = CodeReviewPanel::from_snapshot(sample_snapshot());
        let target = panel
            .tree
            .visible_rows()
            .into_iter()
            .position(|row| {
                matches!(
                    panel.tree.nodes[row.node_index].kind,
                    TreeNodeKind::File { file_index }
                        if panel.snapshot.files[file_index].path == "src/nested/b.rs"
                )
            })
            .expect("nested file");
        panel.tree_selected = target;
        panel.focus = CodeReviewFocus::Diff;
        panel.set_viewports(8, 5, 40);

        assert_eq!(
            panel.handle_key(key(KeyCode::PageDown)),
            CodeReviewPanelResult::Handled
        );
        assert!(panel.diff_scroll_y > 0);
        for _ in 0..50 {
            let _ = panel.handle_key(key(KeyCode::PageDown));
        }
        let (line_count, _) = panel.diff_metrics();
        let max_y = line_count.saturating_sub(5);
        assert_eq!(panel.diff_scroll_y, max_y);

        assert_eq!(
            panel.handle_key(key(KeyCode::Right)),
            CodeReviewPanelResult::Handled
        );
        assert!(panel.diff_scroll_x > 0);
        for _ in 0..100 {
            let _ = panel.handle_key(key(KeyCode::Right));
        }
        let x = panel.diff_scroll_x;
        let _ = panel.handle_key(key(KeyCode::Right));
        assert_eq!(panel.diff_scroll_x, x);

        panel.set_viewports(8, 3, 20);
        assert!(panel.diff_scroll_y <= line_count.saturating_sub(3));
    }

    #[test]
    fn narrow_rendering_does_not_panic() {
        let mut panel = CodeReviewPanel::from_snapshot(sample_snapshot());
        sync_code_review_layout(&mut panel, Rect::new(0, 0, 40, 12));
        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_code_review_panel(frame, &panel, crate::theme::DARK))
            .expect("draw");
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Code review"), "{text}");
        assert!(text.contains("Files"), "{text}");
    }

    #[test]
    fn escape_closes_and_tab_switches_focus() {
        let mut panel = CodeReviewPanel::from_snapshot(sample_snapshot());
        assert_eq!(panel.focus(), CodeReviewFocus::Tree);
        assert_eq!(
            panel.handle_key(key(KeyCode::Tab)),
            CodeReviewPanelResult::Handled
        );
        assert_eq!(panel.focus(), CodeReviewFocus::Diff);
        assert_eq!(
            panel.handle_key(key(KeyCode::Tab)),
            CodeReviewPanelResult::Handled
        );
        assert_eq!(panel.focus(), CodeReviewFocus::Tree);
        assert_eq!(
            panel.handle_key(key(KeyCode::Esc)),
            CodeReviewPanelResult::Close
        );
    }

    #[test]
    fn directory_click_toggles_collapse() {
        let mut panel = prepared_panel();
        let before = panel.tree.visible_rows().len();
        let src_vis = panel
            .tree
            .visible_rows()
            .into_iter()
            .position(|row| panel.tree.nodes[row.node_index].path == "src")
            .expect("src");
        let hits = panel.hits();
        let row = hits.tree_list.y
            + u16::try_from(src_vis.saturating_sub(hits.tree_scroll)).unwrap_or(0);
        assert!(
            row < hits.tree_list.y.saturating_add(hits.tree_list.height),
            "src row must be on-screen"
        );
        assert_eq!(
            panel.handle_mouse(click(hits.tree_list.x + 1, row)),
            CodeReviewPanelResult::Handled
        );
        assert!(panel.tree.visible_rows().len() < before);
        let src_vis = panel
            .tree
            .visible_rows()
            .into_iter()
            .position(|row| panel.tree.nodes[row.node_index].path == "src")
            .expect("src still visible");
        let row = hits.tree_list.y
            + u16::try_from(src_vis.saturating_sub(panel.tree_scroll)).unwrap_or(0);
        assert_eq!(
            panel.handle_mouse(click(hits.tree_list.x + 1, row)),
            CodeReviewPanelResult::Handled
        );
        assert_eq!(panel.tree.visible_rows().len(), before);
    }

    #[test]
    fn file_click_selects_and_opens_diff() {
        let mut panel = prepared_panel();
        let readme_vis = panel
            .tree
            .visible_rows()
            .into_iter()
            .position(|row| {
                matches!(
                    panel.tree.nodes[row.node_index].kind,
                    TreeNodeKind::File { .. }
                ) && panel.tree.nodes[row.node_index].path == "README.md"
            })
            .expect("readme");
        let hits = panel.hits();
        let row = hits.tree_list.y
            + u16::try_from(readme_vis.saturating_sub(hits.tree_scroll)).unwrap_or(0);
        assert_eq!(
            panel.handle_mouse(click(hits.tree_list.x + 2, row)),
            CodeReviewPanelResult::Handled
        );
        assert_eq!(panel.tree_selected, readme_vis);
        assert_eq!(panel.focus(), CodeReviewFocus::Diff);
        assert_eq!(
            panel.selected_file().map(|f| f.path.as_str()),
            Some("README.md")
        );
    }

    #[test]
    fn wheel_scrolls_diff_pane() {
        let mut panel = prepared_panel();
        let target = panel
            .tree
            .visible_rows()
            .into_iter()
            .position(|row| {
                matches!(
                    panel.tree.nodes[row.node_index].kind,
                    TreeNodeKind::File { file_index }
                        if panel.snapshot.files[file_index].path == "src/nested/b.rs"
                )
            })
            .expect("nested");
        panel.tree_selected = target;
        panel.reset_diff_scroll();
        panel.focus = CodeReviewFocus::Diff;
        let hits = panel.hits();
        assert!(hits.diff_body.height > 0);
        let before = panel.diff_scroll_y;
        assert_eq!(
            panel.handle_mouse(wheel(hits.diff_body.x + 1, hits.diff_body.y + 1, false)),
            CodeReviewPanelResult::Handled
        );
        assert!(panel.diff_scroll_y > before);
        assert_eq!(panel.focus(), CodeReviewFocus::Diff);
    }

    #[test]
    fn outside_click_is_noop_for_selection() {
        let mut panel = prepared_panel();
        let selected = panel.tree_selected;
        let focus = panel.focus();
        assert_eq!(
            panel.handle_mouse(click(250, 250)),
            CodeReviewPanelResult::Handled
        );
        assert_eq!(panel.tree_selected, selected);
        assert_eq!(panel.focus(), focus);
    }

    #[test]
    fn escape_requests_close() {
        let mut panel = prepared_panel();
        assert_eq!(
            panel.handle_key(key(KeyCode::Esc)),
            CodeReviewPanelResult::Close
        );
    }

    #[test]
    fn error_snapshot_renders_message() {
        let mut panel = CodeReviewPanel::from_snapshot(ReviewSnapshot::empty_with_error(
            PathBuf::from("repo"),
            "not a git repository",
        ));
        sync_code_review_layout(&mut panel, Rect::new(0, 0, 60, 16));
        let backend = TestBackend::new(60, 16);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_code_review_panel(frame, &panel, crate::theme::DARK))
            .expect("draw");
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("not a git repository"), "{text}");
    }
    #[test]
    fn revision_snapshot_renders_comparison_label() {
        let mut snapshot = sample_snapshot();
        snapshot.scope = ReviewScope::Revisions {
            from: "main".to_owned(),
            to: "feature".to_owned(),
        };
        let panel = CodeReviewPanel::from_snapshot(snapshot);
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_code_review_panel(frame, &panel, crate::theme::DARK))
            .expect("render");
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("main → feature"), "{text}");
    }

    #[test]
    fn revision_scope_loading_renders_comparison_label_from_first_frame() {
        let panel = CodeReviewPanel::loading(
            PathBuf::from("repo"),
            ReviewScope::Revisions {
                from: "main".to_owned(),
                to: "feature".to_owned(),
            },
        );
        assert!(panel.is_snapshot_loading());
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_code_review_panel(frame, &panel, crate::theme::DARK))
            .expect("render");
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("main → feature"), "{text}");
        assert!(text.contains("Loading code review snapshot…"), "{text}");
    }

    #[test]
    fn diff_cells_use_tinted_rows_and_neutral_code_text() {
        for theme in [crate::theme::DARK, crate::theme::LIGHT] {
            let mut panel = CodeReviewPanel::from_snapshot(sample_snapshot());
            panel.tree_selected = panel
                .tree
                .visible_rows()
                .iter()
                .position(|row| panel.tree.nodes[row.node_index].path == "src/a.rs")
                .expect("src/a.rs visible");
            panel.reset_diff_scroll();
            let area = Rect::new(0, 0, 100, 20);
            sync_code_review_layout(&mut panel, area);
            let layout = compute_layout(area);
            let backend = TestBackend::new(area.width, area.height);
            let mut terminal = Terminal::new(backend).expect("terminal");
            terminal
                .draw(|frame| render_code_review_panel(frame, &panel, theme))
                .expect("draw");

            let buffer = terminal.backend().buffer();
            let x = layout.diff_body.x;
            let y = layout.diff_body.y;

            assert_eq!(buffer[(x, y)].symbol(), "▸");
            assert_eq!(buffer[(x, y)].fg, theme.accent, "selected hunk marker uses accent");
            assert_eq!(buffer[(x, y)].bg, theme.tool_pending_bg, "selected hunk uses a calm surface");
            assert_eq!(buffer[(x + 2, y)].fg, theme.accent);
            assert_eq!(buffer[(x + 2, y)].bg, theme.tool_pending_bg);

            assert_eq!(buffer[(x, y + 1)].fg, theme.dim, "context line numbers are muted");
            assert_eq!(buffer[(x, y + 1)].bg, ratatui::style::Color::Reset);
            assert_eq!(buffer[(x + 12, y + 1)].fg, theme.text);
            assert_eq!(buffer[(x + 12, y + 1)].bg, ratatui::style::Color::Reset);

            assert_eq!(buffer[(x + 10, y + 2)].symbol(), "-");
            assert_eq!(buffer[(x + 10, y + 2)].fg, theme.tool_diff_removed);
            assert_eq!(buffer[(x + 10, y + 2)].bg, diff_tint(theme.tool_error_bg, theme.tool_diff_removed));
            assert_eq!(buffer[(x + 16, y + 2)].fg, theme.text);
            assert_eq!(buffer[(x + 16, y + 2)].bg, diff_tint(theme.tool_error_bg, theme.tool_diff_removed));

            assert_eq!(buffer[(x + 10, y + 3)].symbol(), "+");
            assert_eq!(buffer[(x + 10, y + 3)].fg, theme.tool_diff_added);
            assert_eq!(buffer[(x + 10, y + 3)].bg, diff_tint(theme.tool_success_bg, theme.tool_diff_added));
            assert_eq!(buffer[(x + 16, y + 3)].fg, theme.text);
            assert_eq!(buffer[(x + 16, y + 3)].bg, diff_tint(theme.tool_success_bg, theme.tool_diff_added));
        }
    }

    #[test]
    fn review_thread_cards_use_distinct_semantic_surfaces() {
        for theme in [crate::theme::DARK, crate::theme::LIGHT] {
            let header = DiffDisplayLine {
                old_no: None, new_no: None, diff_kind: DiffLineKind::Meta,
                kind: DiffDisplayKind::AnnotationHeader, hunk_index: Some(0),
                annotation_tone: Some(AnnotationTone::User), text: "Comment 1".to_owned(),
                markdown: None,
            };
            let body = DiffDisplayLine {
                old_no: None, new_no: None, diff_kind: DiffLineKind::Meta,
                kind: DiffDisplayKind::AnnotationBody, hunk_index: Some(0),
                annotation_tone: Some(AnnotationTone::User), text: "question".to_owned(),
                markdown: None,
            };
            let bottom = DiffDisplayLine {
                old_no: None, new_no: None, diff_kind: DiffLineKind::Meta,
                kind: DiffDisplayKind::AnnotationBottom, hunk_index: Some(0),
                annotation_tone: Some(AnnotationTone::User), text: String::new(),
                markdown: None,
            };
            let answer = DiffDisplayLine {
                old_no: None, new_no: None, diff_kind: DiffLineKind::Meta,
                kind: DiffDisplayKind::AnnotationBody, hunk_index: Some(0),
                annotation_tone: Some(AnnotationTone::Assistant), text: "answer".to_owned(),
                markdown: None,
            };
            let error = DiffDisplayLine {
                old_no: None, new_no: None, diff_kind: DiffLineKind::Meta,
                kind: DiffDisplayKind::AnnotationHeader, hunk_index: Some(0),
                annotation_tone: Some(AnnotationTone::Error), text: "⚠ Error".to_owned(),
                markdown: None,
            };
            let lines = [
                render_diff_line(&header, 0, 30, theme, 0),
                render_diff_line(&body, 0, 30, theme, 0),
                render_diff_line(&bottom, 0, 30, theme, 0),
                render_diff_line(&answer, 0, 30, theme, 0),
                render_diff_line(&error, 0, 30, theme, 0),
            ];
            // Comment cards frame in the theme's green on the user surface, and
            // the top border carries the title.
            assert_eq!(lines[0].spans[0].style.fg, Some(theme.success), "comment top border must be green");
            assert_eq!(lines[0].spans[0].style.bg, Some(theme.user_message_bg));
            assert!(lines[0].spans[0].style.add_modifier.contains(Modifier::BOLD), "title row must be bold");
            let top_text = lines[0].spans[0].content.as_ref();
            assert!(top_text.starts_with("  ╭─ Comment 1") && top_text.ends_with('╮'), "comment top border: {top_text}");
            assert_eq!(lines[1].spans[0].style.fg, Some(theme.success), "comment side border must be green");
            assert_eq!(lines[1].spans[0].style.bg, Some(theme.user_message_bg));
            assert_eq!(lines[1].spans[1].style.fg, Some(theme.user_message_text));
            assert_eq!(lines[1].spans[1].style.bg, Some(theme.user_message_bg));
            assert_eq!(lines[1].spans[2].style.fg, Some(theme.success), "comment right border must be green");
            assert_eq!(lines[1].spans[2].style.bg, Some(theme.user_message_bg));
            let body_text = lines[1].spans.iter().map(|span| span.content.as_ref()).collect::<String>();
            assert!(body_text.starts_with("  │ ") && body_text.trim_end().ends_with('│'), "comment body row: {body_text}");
            assert_eq!(lines[2].spans[0].style.fg, Some(theme.success), "comment bottom border must be green");
            assert_eq!(lines[2].spans[0].style.bg, Some(theme.user_message_bg));
            let bottom_text = lines[2].spans[0].content.as_ref();
            assert!(bottom_text.starts_with("  ╰") && bottom_text.ends_with('╯'), "comment bottom border: {bottom_text}");
            // The answer box keeps the pending surface; the error card keeps
            // the error surface (red frame on the error background).
            assert_eq!(lines[3].spans[0].style.fg, Some(theme.accent));
            assert_eq!(lines[3].spans[0].style.bg, Some(theme.tool_pending_bg));
            assert_eq!(lines[3].spans[1].style.bg, Some(theme.tool_pending_bg));
            assert_eq!(lines[3].spans[2].style.bg, Some(theme.tool_pending_bg));
            assert_eq!(lines[4].spans[0].style.fg, Some(theme.error));
            assert_eq!(lines[4].spans[0].style.bg, Some(theme.tool_error_bg));
            // Every frame row shares one width — the answer card's math:
            // box_outer + 2 = pane width − 2.
            for line in &lines {
                assert_eq!(line.width(), 28, "card surface must share the frame width");
            }
        }
    }

    #[test]
    fn tree_selection_and_header_keep_status_layers_readable() {
        for theme in [crate::theme::DARK, crate::theme::LIGHT] {
            let mut panel = CodeReviewPanel::from_snapshot(sample_snapshot());
            panel.tree_selected = panel
                .tree
                .visible_rows()
                .iter()
                .position(|row| panel.tree.nodes[row.node_index].path == "src/a.rs")
                .expect("src/a.rs visible");
            panel.reset_diff_scroll();
            let area = Rect::new(0, 0, 100, 20);
            sync_code_review_layout(&mut panel, area);
            let layout = compute_layout(area);
            let selected_row = panel.tree_selected;
            let tree_y = layout.tree_list.y + u16::try_from(selected_row).expect("visible row");
            let backend = TestBackend::new(area.width, area.height);
            let mut terminal = Terminal::new(backend).expect("terminal");
            terminal
                .draw(|frame| render_code_review_panel(frame, &panel, theme))
                .expect("draw");

            let buffer = terminal.backend().buffer();
            let row_text = (layout.tree_list.x..layout.tree_list.x + layout.tree_list.width)
                .map(|x| buffer[(x, tree_y)].symbol())
                .collect::<String>();
            let status_offset = u16::try_from(row_text.find("M ").expect("status marker"))
                .expect("status offset");
            let name_offset = u16::try_from(row_text.find("a.rs").expect("file name"))
                .expect("name offset");
            let add_offset = u16::try_from(row_text.rfind("+1").expect("addition count"))
                .expect("addition offset");
            let remove_offset = u16::try_from(row_text.rfind("−1").expect("deletion count"))
                .expect("deletion offset");
            let row_x = layout.tree_list.x;

            assert_eq!(buffer[(row_x + status_offset, tree_y)].fg, theme.warning);
            assert_eq!(buffer[(row_x + name_offset, tree_y)].fg, theme.text);
            assert_eq!(buffer[(row_x + add_offset, tree_y)].fg, theme.tool_diff_added);
            assert_eq!(buffer[(row_x + remove_offset, tree_y)].fg, theme.tool_diff_removed);
            for x in layout.tree_list.x..layout.tree_list.x + layout.tree_list.width {
                assert_eq!(buffer[(x, tree_y)].bg, theme.selected_bg, "selected row background");
            }

            let header_y = layout.diff_pane.y;
            let header_text = (layout.diff_pane.x..layout.diff_pane.x + layout.diff_pane.width)
                .map(|x| buffer[(x, header_y)].symbol())
                .collect::<String>();
            let header_status = u16::try_from(header_text.find("M ").expect("header status"))
                .expect("header status offset");
            let header_path = u16::try_from(header_text.find("src/a.rs").expect("header path"))
                .expect("header path offset");
            assert_eq!(buffer[(layout.diff_pane.x + header_status, header_y)].fg, theme.warning);
            assert_eq!(buffer[(layout.diff_pane.x + header_path, header_y)].fg, theme.text);

            let meta_y = header_y + 1;
            let meta_text = (layout.diff_pane.x..layout.diff_pane.x + layout.diff_pane.width)
                .map(|x| buffer[(x, meta_y)].symbol())
                .collect::<String>();
            let meta_add = u16::try_from(meta_text.find("+1").expect("header addition"))
                .expect("header addition offset");
            let meta_remove = u16::try_from(meta_text.find("−1").expect("header deletion"))
                .expect("header deletion offset");
            assert_eq!(buffer[(layout.diff_pane.x, meta_y)].bg, theme.tool_pending_bg);
            assert_eq!(buffer[(layout.diff_pane.x + meta_add, meta_y)].fg, theme.tool_diff_added);
            assert_eq!(buffer[(layout.diff_pane.x + meta_remove, meta_y)].fg, theme.tool_diff_removed);
        }
    }

    #[test]
    fn divider_paints_full_height_column() {
        let mut panel = CodeReviewPanel::from_snapshot(sample_snapshot());
        let area = Rect::new(0, 0, 80, 20);
        sync_code_review_layout(&mut panel, area);
        let layout = compute_layout(area);
        assert!(!layout.too_small);
        assert!(layout.tree_pane.height > 1, "need multi-row body for regression");

        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_code_review_panel(frame, &panel, crate::theme::DARK))
            .expect("draw");

        let buffer = terminal.backend().buffer();
        let divider_x = layout.tree_pane.x.saturating_add(layout.tree_pane.width);
        for row in layout.tree_pane.y..layout.tree_pane.y.saturating_add(layout.tree_pane.height)
        {
            let cell = &buffer[(divider_x, row)];
            assert_eq!(
                cell.symbol(),
                "│",
                "divider missing at ({divider_x},{row}); got {:?}",
                cell.symbol()
            );
        }

        let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains("Code review"), "{text}");
        assert!(text.contains("Files"), "{text}");
        assert!(text.contains("Diff") || text.contains("src"), "{text}");
    }

    #[test]
    fn review_prompt_contains_exact_hunk_context() {
        let snapshot = sample_snapshot();
        let file = &snapshot.files[0];
        let hunk = &file.hunks[0];
        let prompt = review_prompt(file, hunk, "Is the replacement safe?");
        assert!(prompt.contains("Path: src/a.rs"));
        assert!(prompt.contains("Old range: 1,3"));
        assert!(prompt.contains("New range: 1,4"));
        assert!(prompt.contains(&hunk.unified_diff()));
        assert!(prompt.contains("Is the replacement safe?"));
    }
    #[test]
    fn comment_editor_key_flow_and_sanitized_thread_rows() {
        let mut panel = CodeReviewPanel::from_snapshot(sample_snapshot());
        panel.set_viewports(10, 10, 80);
        assert_eq!(panel.focus(), CodeReviewFocus::Tree);
        assert_eq!(panel.handle_key(key(KeyCode::Char('c'))), CodeReviewPanelResult::Handled);
        assert_eq!(panel.focus(), CodeReviewFocus::Diff);
        assert_eq!(panel.comment_editor(), Some(""));
        assert_eq!(panel.handle_key(key(KeyCode::Char('h'))), CodeReviewPanelResult::Handled);
        assert_eq!(panel.handle_key(key(KeyCode::Char('i'))), CodeReviewPanelResult::Handled);
        assert_eq!(panel.comment_editor(), Some("hi"));

        assert_eq!(panel.handle_key(key(KeyCode::Enter)), CodeReviewPanelResult::SubmitComment);

        let identity = panel.selected_hunk_identity().expect("hunk identity");
        panel.threads.insert(identity.clone(), HunkThread {
            identity,
            comments: vec![ReviewComment {
                role: ReviewCommentRole::Assistant,
                text: "safe\u{1b}[2Janswer".to_owned(),
                partial: false,
            }],
            streaming_text: String::new(),
            error: None,
            stale: false,
        });
        let lines = build_diff_display_lines(&panel, panel.selected_file().expect("file"));
        let text = lines.into_iter().map(|line| line.text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("safeanswer"), "{text}");
        assert!(!text.contains('\u{1b}'));
    }
    #[test]
    fn consecutive_assistant_rounds_commit_only_terminal_answer() {
        let snapshot = sample_snapshot();
        let identity = snapshot.hunk_identity(&snapshot.files[0], &snapshot.files[0].hunks[0]);
        let mut thread = HunkThread::new(identity);
        thread.comments.push(ReviewComment {
            role: ReviewCommentRole::User,
            text: "Explain this change".to_owned(),
            partial: false,
        });
        let progress = AssistantMessage {
            content: vec![ContentBlock::text("I will inspect the code first.")],
            api: "test".into(), provider: "test".into(), model: "test".to_owned(),
            response_model: None, response_id: None, diagnostics: Vec::new(),
            usage: Default::default(), stop_reason: StopReason::ToolUse,
            error_message: None, raw_stop_reason: None, timestamp: 0,
        };
        thread.replace_pending_assistant(&progress);
        assert_eq!(thread.comments.len(), 1, "tool-progress MessageEnd must not commit an answer");
        let final_answer = AssistantMessage {
            content: vec![ContentBlock::text("The visible catalog contains only primary commands.")],
            stop_reason: StopReason::Stop,
            ..progress
        };
        thread.replace_pending_assistant(&final_answer);
        thread.finish_assistant_message(&final_answer);

        assert_eq!(thread.comments.len(), 2);
        assert_eq!(thread.comments[1].text, "The visible catalog contains only primary commands.");
    }

    #[test]
    fn busy_review_rejects_refresh_and_new_editor() {
        let mut panel = CodeReviewPanel::from_snapshot(sample_snapshot());
        panel.review_streaming = true;

        assert_eq!(
            panel.handle_key(key(KeyCode::Char('r'))),
            CodeReviewPanelResult::Busy
        );
        assert_eq!(
            panel.handle_key(key(KeyCode::Char('c'))),
            CodeReviewPanelResult::Busy
        );
        assert_eq!(panel.comment_editor(), None);
    }

    #[test]
    fn rejected_comment_submit_retains_draft() {
        let mut panel = CodeReviewPanel::from_snapshot(sample_snapshot());
        assert!(panel.open_comment_editor());
        assert!(panel.handle_paste("keep this draft"));

        panel.complete_comment_submit(false);
        assert_eq!(panel.comment_editor(), Some("keep this draft"));
        assert_eq!(panel.comment_cursor(), "keep this draft".len());

        panel.complete_comment_submit(true);
        assert_eq!(panel.comment_editor(), None);
        assert_eq!(panel.comment_cursor(), 0);
    }

    #[test]
    fn aborted_assistant_content_and_stream_fallback_are_partial() {
        let snapshot = sample_snapshot();
        let identity = snapshot.hunk_identity(&snapshot.files[0], &snapshot.files[0].hunks[0]);
        let mut thread = HunkThread::new(identity);
        thread.streaming_text = "stream fallback".to_owned();
        thread.finish_assistant_message(&AssistantMessage {
            content: vec![ContentBlock::text("partial answer")],
            api: "test".into(),
            provider: "test".into(),
            model: "test".to_owned(),
            response_model: None,
            response_id: None,
            diagnostics: Vec::new(),
            usage: Default::default(),
            stop_reason: StopReason::Aborted,
            error_message: None,
            raw_stop_reason: None,
            timestamp: 0,
        });
        assert_eq!(thread.comments.len(), 1);
        assert_eq!(thread.comments[0].text, "partial answer");
        assert!(thread.comments[0].partial);

        thread.streaming_text = "aborted before MessageEnd".to_owned();
        thread.finish_streaming(true);
        assert_eq!(thread.comments.len(), 1);
        assert_eq!(thread.comments[0].text, "aborted before MessageEnd");
        assert!(thread.comments[0].partial);
    }

    #[test]
    fn diff_display_bound_includes_comments_stream_and_errors() {
        for source in [ReviewCommentRole::User, ReviewCommentRole::Assistant, ReviewCommentRole::System] {
            let mut panel = CodeReviewPanel::from_snapshot(sample_snapshot());
            let identity = panel.selected_hunk_identity().expect("hunk identity");
            let many_lines = "row\n".repeat(crate::code_review::MAX_FILE_RENDER_LINES + 100);
            let mut thread = HunkThread::new(identity.clone());
            match source {
                ReviewCommentRole::User | ReviewCommentRole::Assistant => thread.comments.push(ReviewComment {
                    role: source,
                    text: many_lines,
                    partial: false,
                }),
                ReviewCommentRole::System => thread.error = Some(many_lines),
            }
            panel.threads.insert(identity, thread);
            let lines = build_diff_display_lines(&panel, panel.selected_file().expect("file"));
            assert_eq!(lines.len(), crate::code_review::MAX_FILE_RENDER_LINES);
            assert_eq!(lines.last().map(|line| line.text.as_str()), Some("… truncated …"));
        }

        let mut panel = CodeReviewPanel::from_snapshot(sample_snapshot());
        let identity = panel.selected_hunk_identity().expect("hunk identity");
        let mut thread = HunkThread::new(identity.clone());
        thread.streaming_text = "row\n".repeat(crate::code_review::MAX_FILE_RENDER_LINES + 100);
        panel.threads.insert(identity, thread);
        let lines = build_diff_display_lines(&panel, panel.selected_file().expect("file"));
        assert_eq!(lines.len(), crate::code_review::MAX_FILE_RENDER_LINES);
        assert_eq!(lines.last().map(|line| line.text.as_str()), Some("… truncated …"));
    }

    #[test]
    fn footer_advertises_comment_hunk_and_refork_keys() {
        let panel = CodeReviewPanel::from_snapshot(sample_snapshot());
        let backend = TestBackend::new(140, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_code_review_panel(frame, &panel, crate::theme::DARK))
            .expect("draw");
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("j/k hunk"), "{text}");
        assert!(text.contains("c comment"), "{text}");
        assert!(text.contains("Space fold"), "{text}");
        assert!(text.contains("click select"), "{text}");
        assert!(text.contains("Alt+R refork"), "{text}");
    }

    #[test]
    fn matching_hunk_identity_survives_snapshot_change() {
        let snapshot = sample_snapshot();
        let identity = snapshot.hunk_identity(&snapshot.files[0], &snapshot.files[0].hunks[0]);
        let next = ReviewSnapshot { snapshot_id: "next".to_owned(), ..snapshot };
        let next_identity = next.hunk_identity(&next.files[0], &next.files[0].hunks[0]);
        assert!(identity.matches_across_snapshots(&next_identity));
        assert_ne!(identity.snapshot_id, next_identity.snapshot_id);
    }

    fn two_hunk_snapshot() -> ReviewSnapshot {
        let mut snapshot = sample_snapshot();
        snapshot.files.truncate(1);
        snapshot.files[0].hunks.push(DiffHunk {
            header: "@@ -20,2 +20,2 @@".to_owned(),
            old_start: 20,
            old_count: 2,
            new_start: 20,
            new_count: 2,
            lines: vec![
                DiffLine { kind: DiffLineKind::Context, old_no: Some(20), new_no: Some(20), text: "second hunk context".to_owned() },
                DiffLine { kind: DiffLineKind::Addition, old_no: None, new_no: Some(21), text: "second hunk change".to_owned() },
            ],
        });
        snapshot
    }

    fn add_thread(panel: &mut CodeReviewPanel, hunk_index: usize, comments: Vec<ReviewComment>) -> HunkIdentity {
        let file = panel.selected_file().expect("file");
        let identity = panel.snapshot.hunk_identity(file, &file.hunks[hunk_index]);
        panel.threads.insert(identity.clone(), HunkThread {
            identity: identity.clone(),
            comments,
            streaming_text: String::new(),
            error: None,
            stale: false,
        });
        identity
    }

    #[test]
    fn review_thread_ux_keyboard_selects_hunks_and_space_folds() {
        let mut panel = CodeReviewPanel::from_snapshot(two_hunk_snapshot());
        panel.focus = CodeReviewFocus::Diff;
        panel.set_viewports(5, 3, 70);
        let identity = add_thread(&mut panel, 0, vec![ReviewComment { role: ReviewCommentRole::User, text: "question".to_owned(), partial: false }]);
        assert_eq!(panel.selected_hunk, 0);
        assert_eq!(panel.handle_key(key(KeyCode::Char('j'))), CodeReviewPanelResult::Handled);
        assert_eq!(panel.selected_hunk, 1);
        assert!(panel.diff_scroll_y > 0, "selected hunk must scroll into view");
        assert_eq!(panel.handle_key(key(KeyCode::Char('k'))), CodeReviewPanelResult::Handled);
        assert_eq!(panel.selected_hunk, 0);
        assert_eq!(panel.handle_key(key(KeyCode::Char(' '))), CodeReviewPanelResult::Handled);
        assert!(panel.collapsed_threads.contains(&identity));
        assert_eq!(panel.handle_key(key(KeyCode::Char('o'))), CodeReviewPanelResult::Handled);
        assert!(!panel.collapsed_threads.contains(&identity));
    }

    #[test]
    fn review_thread_ux_mouse_selects_hunk_but_thread_rows_do_not_fold() {
        let mut panel = CodeReviewPanel::from_snapshot(two_hunk_snapshot());
        panel.focus = CodeReviewFocus::Diff;
        panel.set_viewports(5, 3, 70);
        let identity = add_thread(&mut panel, 1, vec![ReviewComment { role: ReviewCommentRole::User, text: "question".to_owned(), partial: false }]);
        let area = Rect::new(0, 0, 100, 12);
        sync_code_review_layout(&mut panel, area);
        let layout = compute_layout(area);
        let display = build_diff_display_lines(&panel, panel.selected_file().expect("file"));
        let second_header = display.iter().position(|line| line.hunk_index == Some(1) && line.kind == DiffDisplayKind::HunkHeader).expect("second header");
        panel.diff_scroll_y = second_header;
        panel.clamp_diff_scroll();
        panel.set_hit_regions(layout.tree_list, layout.diff_body);
        let header_row = layout.diff_body.y
            + u16::try_from(second_header.saturating_sub(panel.diff_scroll_y)).expect("header row");
        assert_eq!(panel.handle_mouse(click(layout.diff_body.x + 2, header_row)), CodeReviewPanelResult::Handled);
        assert_eq!(panel.selected_hunk, 1);
        // Thread cards render inline with no separate collapse row: clicking a
        // card selects the hunk but must NOT toggle the fold.
        let display = build_diff_display_lines(&panel, panel.selected_file().expect("file"));
        let card = display.iter().position(|line| line.hunk_index == Some(1) && line.kind == DiffDisplayKind::AnnotationHeader).expect("thread card");
        panel.diff_scroll_y = card;
        panel.clamp_diff_scroll();
        panel.set_hit_regions(layout.tree_list, layout.diff_body);
        let card_row = layout.diff_body.y
            + u16::try_from(card.saturating_sub(panel.diff_scroll_y)).expect("card row");
        assert_eq!(panel.handle_mouse(click(layout.diff_body.x + 2, card_row)), CodeReviewPanelResult::Handled);
        assert!(panel.collapsed_threads.is_empty(), "clicking a thread card must not fold the hunk");
        // Folding stays keyboard-only (Space): the fold keeps the cards visible
        // and hides only the hunk's unchanged context.
        assert_eq!(panel.handle_key(key(KeyCode::Char(' '))), CodeReviewPanelResult::Handled);
        assert!(panel.collapsed_threads.contains(&identity));
        let display = build_diff_display_lines(&panel, panel.selected_file().expect("file"));
        assert!(display.iter().any(|line| line.hunk_index == Some(1) && line.kind == DiffDisplayKind::AnnotationHeader), "thread cards stay visible while folded");
        assert!(!display.iter().any(|line| line.hunk_index == Some(1) && line.kind == DiffDisplayKind::Diff && line.diff_kind == DiffLineKind::Context), "folded hunk hides its context");
        // Clicking the card while folded still must not unfold.
        let card = display.iter().position(|line| line.hunk_index == Some(1) && line.kind == DiffDisplayKind::AnnotationHeader).expect("folded thread card");
        panel.diff_scroll_y = card;
        panel.clamp_diff_scroll();
        panel.set_hit_regions(layout.tree_list, layout.diff_body);
        let card_row = layout.diff_body.y
            + u16::try_from(card.saturating_sub(panel.diff_scroll_y)).expect("card row while folded");
        assert_eq!(panel.handle_mouse(click(layout.diff_body.x + 2, card_row)), CodeReviewPanelResult::Handled);
        assert!(panel.collapsed_threads.contains(&identity), "clicking a thread card must not unfold the hunk");
    }

    #[test]
    fn review_thread_ux_cards_wrap_without_chat_prefixes_and_stay_with_hunk() {
        let mut panel = CodeReviewPanel::from_snapshot(two_hunk_snapshot());
        panel.set_viewports(5, 10, 34);
        add_thread(&mut panel, 0, vec![
            ReviewComment { role: ReviewCommentRole::User, text: "a long markdown **question** that wraps across display rows".to_owned(), partial: false },
            ReviewComment { role: ReviewCommentRole::Assistant, text: "a long analysis response that also wraps without repeated labels".to_owned(), partial: true },
        ]);
        let identity = panel.snapshot.hunk_identity(&panel.snapshot.files[0], &panel.snapshot.files[0].hunks[0]);
        panel.threads.get_mut(&identity).expect("thread").streaming_text = "more streamed analysis".to_owned();
        panel.threads.get_mut(&identity).expect("thread").error = Some("review failed safely".to_owned());
        let display = build_diff_display_lines(&panel, panel.selected_file().expect("file"));
        let text = display.iter().map(|line| line.text.as_str()).collect::<Vec<_>>().join("\n");
        assert!(text.contains("Comment 1"), "{text}");
        assert!(!text.contains("Review thread"), "no thread-summary row: {text}");
        assert!(!text.contains("◆"), "comment marker must not carry the ◆ decoration: {text}");
        // Assistant replies now render as markdown reply boxes; the answer label
        // lives in the box metadata, and the reply body becomes markdown rows.
        let labels = display
            .iter()
            .filter_map(|line| line.markdown.as_ref())
            .map(|md| md.label.as_str())
            .collect::<Vec<_>>();
        assert!(labels.contains(&"Answer 1 · aborted"), "{labels:?}");
        assert!(labels.contains(&"Answer 1 · streaming"), "{labels:?}");
        assert!(!text.contains("Analysis"), "{text}");
        assert!(text.contains("⚠ Error"), "{text}");
        assert!(!text.contains("you:"), "{text}");
        assert!(!text.contains("review:"), "{text}");
        assert!(!text.contains("review…"), "{text}");
        // Thread content must attach immediately after the hunk header and before
        // the hunk's diff lines: header, spacer, cards…, spacer, then diffs.
        let zero_header = display.iter().position(|line| line.hunk_index == Some(0) && line.kind == DiffDisplayKind::HunkHeader).expect("first header");
        assert_eq!(display[zero_header + 1].kind, DiffDisplayKind::AnnotationSpacer);
        assert_eq!(display[zero_header + 2].kind, DiffDisplayKind::AnnotationHeader);
        let first_zero_diff = display.iter().position(|line| line.hunk_index == Some(0) && line.kind == DiffDisplayKind::Diff).expect("first hunk zero diff");
        assert!(display[zero_header..first_zero_diff].iter().any(|line| matches!(line.kind, DiffDisplayKind::AnnotationHeader | DiffDisplayKind::AnnotationBody)), "thread content must render before the hunk diff lines");
        let final_hunk_zero_diff = display.iter().rposition(|line| line.hunk_index == Some(0) && line.kind == DiffDisplayKind::Diff).expect("final hunk zero diff");
        assert!(!display[first_zero_diff..=final_hunk_zero_diff].iter().any(|line| matches!(line.kind, DiffDisplayKind::AnnotationHeader | DiffDisplayKind::AnnotationBody)), "no thread content may interleave with the hunk diff lines");
        let second_header = display.iter().position(|line| line.hunk_index == Some(1) && line.kind == DiffDisplayKind::HunkHeader).expect("second header");
        assert_eq!(display[second_header - 1].kind, DiffDisplayKind::Diff, "second hunk header follows the first hunk's diff lines");
        assert!(!display[second_header..].iter().any(|line| matches!(line.kind, DiffDisplayKind::AnnotationHeader | DiffDisplayKind::AnnotationBody)), "second hunk has no thread");

        // The rendered terminal must show the same order: hunk header, then the
        // thread cards, then the hunk's diff lines (no thread-summary row).
        sync_code_review_layout(&mut panel, Rect::new(0, 0, 160, 60));
        let backend = TestBackend::new(160, 60);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render_code_review_panel(frame, &panel, crate::theme::DARK)).expect("draw");
        let rows: Vec<String> = terminal
            .backend()
            .buffer()
            .content
            .chunks(160)
            .map(|cells| cells.iter().map(|cell| cell.symbol()).collect())
            .collect();
        let header_row = rows.iter().position(|row| row.contains("@@ -1,3 +1,4 @@")).expect("hunk header row");
        let comment_row = rows.iter().position(|row| row.contains("Comment 1")).expect("comment card row");
        let diff_row = rows.iter().position(|row| row.contains("fn main() {")).expect("first diff row");
        assert!(
            header_row < comment_row && comment_row < diff_row,
            "expected header < comment < diff, got {header_row} < {comment_row} < {diff_row}"
        );
        assert!(!rows.iter().any(|row| row.contains("Review thread")), "no thread-summary row in render");
    }

    #[test]
    fn review_thread_ux_repeated_cycles_share_thread_and_refresh_preserves_fold() {
        let mut panel = CodeReviewPanel::from_snapshot(two_hunk_snapshot());
        let identity = add_thread(&mut panel, 0, vec![
            ReviewComment { role: ReviewCommentRole::User, text: "first".to_owned(), partial: false },
            ReviewComment { role: ReviewCommentRole::Assistant, text: "first answer".to_owned(), partial: false },
            ReviewComment { role: ReviewCommentRole::User, text: "second".to_owned(), partial: false },
            ReviewComment { role: ReviewCommentRole::Assistant, text: "second answer".to_owned(), partial: false },
        ]);
        panel.collapsed_threads.insert(identity.clone());
        let mut next = panel.snapshot.clone();
        next.snapshot_id = "next".to_owned();
        let next_identity = next.hunk_identity(&next.files[0], &next.files[0].hunks[0]);
        let mut thread = panel.threads.remove(&identity).expect("thread");
        thread.identity = next_identity.clone();
        panel.threads.insert(next_identity.clone(), thread);
        panel.replace_snapshot(next);
        panel.reconcile_collapsed_threads();
        assert_eq!(panel.threads.len(), 1);
        assert_eq!(panel.threads[&next_identity].comments.len(), 4);
        assert!(panel.collapsed_threads.contains(&next_identity));
        let display = build_diff_display_lines(&panel, panel.selected_file().expect("file"));
        assert!(!display.iter().any(|line| line.text.contains("Review thread")), "no thread-summary row after refresh");
        assert!(display.iter().any(|line| line.kind == DiffDisplayKind::AnnotationHeader), "thread cards stay visible while folded after refresh");
    }

    #[test]
    fn review_thread_ux_total_cap_and_footer_hints_hold() {
        let mut panel = CodeReviewPanel::from_snapshot(two_hunk_snapshot());
        let many_lines = "annotation row\n".repeat(crate::code_review::MAX_FILE_RENDER_LINES + 100);
        add_thread(&mut panel, 0, vec![ReviewComment { role: ReviewCommentRole::Assistant, text: many_lines, partial: false }]);
        let display = build_diff_display_lines(&panel, panel.selected_file().expect("file"));
        assert_eq!(display.len(), crate::code_review::MAX_FILE_RENDER_LINES);
        assert_eq!(display.last().map(|line| line.text.as_str()), Some("… truncated …"));
        let backend = TestBackend::new(160, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render_code_review_panel(frame, &panel, crate::theme::DARK)).expect("draw");
        let text = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect::<String>();
        for hint in ["j/k hunk", "c comment", "Space fold", "click select"] {
            assert!(text.contains(hint), "missing {hint}: {text}");
        }
    }

    #[test]
    fn space_folds_selected_hunk_diff_lines_and_thread_annotations() {
        let mut panel = CodeReviewPanel::from_snapshot(two_hunk_snapshot());
        panel.focus = CodeReviewFocus::Diff;
        panel.set_viewports(5, 3, 70);
        let identity = add_thread(&mut panel, 0, vec![
            ReviewComment { role: ReviewCommentRole::User, text: "question".to_owned(), partial: false },
            ReviewComment { role: ReviewCommentRole::Assistant, text: "answer".to_owned(), partial: false },
        ]);
        let hunk_zero_diffs = |panel: &CodeReviewPanel| {
            let file = panel.selected_file().expect("file");
            build_diff_display_lines(panel, file)
                .iter()
                .filter(|line| line.hunk_index == Some(0) && line.kind == DiffDisplayKind::Diff)
                .count()
        };
        let hunk_zero_kinds = |panel: &CodeReviewPanel| {
            let file = panel.selected_file().expect("file");
            let mut context = 0usize;
            let mut changed = 0usize;
            for line in build_diff_display_lines(panel, file) {
                if line.hunk_index == Some(0) && line.kind == DiffDisplayKind::Diff {
                    match line.diff_kind {
                        DiffLineKind::Context => context += 1,
                        DiffLineKind::Addition | DiffLineKind::Deletion | DiffLineKind::Meta => changed += 1,
                    }
                }
            }
            (context, changed)
        };

        // Unfolded: the selected hunk shows its thread annotations and all diff lines.
        let display = build_diff_display_lines(&panel, panel.selected_file().expect("file"));
        assert!(hunk_zero_diffs(&panel) > 0, "hunk 0 diff lines visible before fold");
        assert!(display.iter().any(|line| line.kind == DiffDisplayKind::AnnotationHeader));
        assert!(!display.iter().any(|line| line.text.contains("Review thread")), "no thread-summary row while unfolded");

        // Space folds the selected hunk: unchanged context lines collapse to
        // the header (with fold counts) while the changed +/- lines AND the
        // thread cards stay visible so the diff and its review exchange
        // remain readable. There is no separate thread-summary collapse row.
        assert_eq!(panel.handle_key(key(KeyCode::Char(' '))), CodeReviewPanelResult::Handled);
        assert!(panel.collapsed_threads.contains(&identity));
        let display = build_diff_display_lines(&panel, panel.selected_file().expect("file"));
        let (context, changed) = hunk_zero_kinds(&panel);
        assert_eq!(context, 0, "hunk 0 context lines must fold away");
        assert_eq!(changed, 2, "hunk 0 changed lines must stay visible after fold");
        assert!(display.iter().any(|line| line.kind == DiffDisplayKind::AnnotationHeader), "thread cards must stay visible after fold");
        assert!(!display.iter().any(|line| line.text.contains("Review thread")), "no thread-summary row while folded");
        let header = display.iter().find(|line| line.hunk_index == Some(0) && line.kind == DiffDisplayKind::HunkHeader).expect("hunk 0 header");
        assert_eq!(header.text, "@@ -1,3 +1,4 @@ · 2 lines folded · 2 changed", "folded header must count hidden context and kept changes: {}", header.text);
        // The other hunk stays intact.
        assert!(display.iter().any(|line| line.hunk_index == Some(1) && line.kind == DiffDisplayKind::Diff));

        // Space again unfolds: the folded context lines come back (the cards
        // were never hidden).
        assert_eq!(panel.handle_key(key(KeyCode::Char(' '))), CodeReviewPanelResult::Handled);
        assert!(!panel.collapsed_threads.contains(&identity));
        let display = build_diff_display_lines(&panel, panel.selected_file().expect("file"));
        assert!(hunk_zero_diffs(&panel) > 0, "hunk 0 diff lines restored after unfold");
        assert!(display.iter().any(|line| line.kind == DiffDisplayKind::AnnotationHeader));
        let (context, changed) = hunk_zero_kinds(&panel);
        assert_eq!(context, 2, "hunk 0 context lines restored after unfold");
        assert_eq!(changed, 2, "hunk 0 changed lines still visible after unfold");
        let header = display.iter().find(|line| line.hunk_index == Some(0) && line.kind == DiffDisplayKind::HunkHeader).expect("hunk 0 header");
        assert!(!header.text.contains("folded"), "unfolded header has no marker: {}", header.text);
    }

    #[test]
    fn space_folds_hunk_without_thread() {
        let mut panel = CodeReviewPanel::from_snapshot(two_hunk_snapshot());
        panel.focus = CodeReviewFocus::Diff;
        panel.set_viewports(5, 3, 70);
        // No thread exists on either hunk; Space must still fold the selected
        // hunk's context lines (the "Space fold" footer contract), keeping the
        // changed +/- lines readable under the folded header.
        assert_eq!(panel.selected_hunk, 0);
        let hunk_zero_diffs = |panel: &CodeReviewPanel| {
            let file = panel.selected_file().expect("file");
            build_diff_display_lines(panel, file)
                .iter()
                .filter(|line| line.hunk_index == Some(0) && line.kind == DiffDisplayKind::Diff)
                .count()
        };
        assert!(hunk_zero_diffs(&panel) > 0);
        assert_eq!(panel.handle_key(key(KeyCode::Char(' '))), CodeReviewPanelResult::Handled);
        let display = build_diff_display_lines(&panel, panel.selected_file().expect("file"));
        let hunk_zero_kinds = |panel: &CodeReviewPanel| {
            let file = panel.selected_file().expect("file");
            let mut context = 0usize;
            let mut changed = 0usize;
            for line in build_diff_display_lines(panel, file) {
                if line.hunk_index == Some(0) && line.kind == DiffDisplayKind::Diff {
                    match line.diff_kind {
                        DiffLineKind::Context => context += 1,
                        DiffLineKind::Addition | DiffLineKind::Deletion | DiffLineKind::Meta => changed += 1,
                    }
                }
            }
            (context, changed)
        };
        let (context, changed) = hunk_zero_kinds(&panel);
        assert_eq!(context, 0, "thread-less hunk must fold its context on Space");
        assert_eq!(changed, 2, "thread-less hunk must keep changed lines visible on Space");
        let header = display.iter().find(|line| line.hunk_index == Some(0) && line.kind == DiffDisplayKind::HunkHeader).expect("header");
        assert_eq!(header.text, "@@ -1,3 +1,4 @@ · 2 lines folded · 2 changed", "folded header must count hidden context and kept changes: {}", header.text);
        assert_eq!(panel.handle_key(key(KeyCode::Char(' '))), CodeReviewPanelResult::Handled);
        let display = build_diff_display_lines(&panel, panel.selected_file().expect("file"));
        assert!(hunk_zero_diffs(&panel) > 0, "thread-less hunk must unfold on second Space");
    }

    #[test]
    fn fold_hides_only_context_and_keeps_changes_readable() {
        // Regression: folding a hunk must never hide the CHANGES. Only the
        // unchanged context collapses; every +/- line stays rendered under the
        // folded header (with counts) so the diff remains readable after fold.
        let mut panel = CodeReviewPanel::from_snapshot(two_hunk_snapshot());
        panel.focus = CodeReviewFocus::Diff;
        panel.set_viewports(5, 3, 70);
        let display = |panel: &CodeReviewPanel| {
            build_diff_display_lines(panel, panel.selected_file().expect("file"))
        };
        let hunk_zero_lines = |panel: &CodeReviewPanel| {
            let file = panel.selected_file().expect("file");
            build_diff_display_lines(panel, file)
                .iter()
                .filter(|line| line.hunk_index == Some(0))
                .map(|line| (line.kind, line.diff_kind, line.text.clone()))
                .collect::<Vec<_>>()
        };

        // Unfolded state is unchanged: the hunk header followed by every diff
        // line, context and changes alike.
        let before = hunk_zero_lines(&panel);
        let texts = before.iter().map(|(_, _, text)| text.as_str()).collect::<Vec<_>>();
        assert!(texts.contains(&"fn main() {") && texts.contains(&"    old();")
            && texts.contains(&"    new();") && texts.contains(&"}"),
            "unfolded hunk must show context and changes: {texts:?}");
        let unfolded = display(&panel);
        let header = unfolded.iter().find(|line| line.kind == DiffDisplayKind::HunkHeader).expect("header");
        assert_eq!(header.text, "@@ -1,3 +1,4 @@", "unfolded header is unadorned: {}", header.text);

        // Fold: context collapses, changes stay.
        assert_eq!(panel.handle_key(key(KeyCode::Char(' '))), CodeReviewPanelResult::Handled);
        let folded_display = display(&panel);
        let folded_lines = hunk_zero_lines(&panel);
        let folded_texts = folded_lines.iter().map(|(_, _, text)| text.as_str()).collect::<Vec<_>>();
        let header = folded_display.iter().find(|line| line.kind == DiffDisplayKind::HunkHeader).expect("folded header");
        assert_eq!(header.text, "@@ -1,3 +1,4 @@ · 2 lines folded · 2 changed", "folded header: {}", header.text);
        assert!(folded_texts.contains(&"    old();"), "deleted line must stay visible after fold: {folded_texts:?}");
        assert!(folded_texts.contains(&"    new();"), "added line must stay visible after fold: {folded_texts:?}");
        assert!(!folded_texts.contains(&"fn main() {") && !folded_texts.contains(&"}"),
            "unchanged context must fold away: {folded_texts:?}");
        assert!(folded_lines.iter().any(|(kind, diff_kind, _)| *kind == DiffDisplayKind::Diff && *diff_kind == DiffLineKind::Deletion));
        assert!(folded_lines.iter().any(|(kind, diff_kind, _)| *kind == DiffDisplayKind::Diff && *diff_kind == DiffLineKind::Addition));
        assert!(!folded_lines.iter().any(|(_, diff_kind, _)| *diff_kind == DiffLineKind::Context),
            "no context rows may remain under the folded header");

        // The rendered pane shows the changed lines while folded (the reported
        // bug: fold hid the modifications).
        sync_code_review_layout(&mut panel, Rect::new(0, 0, 160, 60));
        let backend = TestBackend::new(160, 60);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render_code_review_panel(frame, &panel, crate::theme::DARK)).expect("draw");
        let rows: Vec<String> = terminal
            .backend()
            .buffer()
            .content
            .chunks(160)
            .map(|cells| cells.iter().map(|cell| cell.symbol()).collect())
            .collect();
        let rendered = rows.join("\n");
        assert!(rendered.contains("lines folded · 2 changed"), "folded header visible: {rendered}");
        assert!(rendered.contains("old();") && rendered.contains("new();"), "changes must be visible after fold: {rendered}");
        assert!(!rendered.contains("fn main() {"), "context must be hidden after fold: {rendered}");

        // Toggle back: full hunk restored.
        assert_eq!(panel.handle_key(key(KeyCode::Char(' '))), CodeReviewPanelResult::Handled);
        assert!(panel.collapsed_threads.is_empty(), "second Space must unfold: {:?}", panel.collapsed_threads);
        let after = hunk_zero_lines(&panel);
        let after_texts = after.iter().map(|(_, _, text)| text.as_str()).collect::<Vec<_>>();
        assert!(after_texts.contains(&"fn main() {") && after_texts.contains(&"    old();")
            && after_texts.contains(&"    new();") && after_texts.contains(&"}"),
            "unfold restores every line: {after_texts:?}");
        let unfolded = display(&panel);
        let header = unfolded.iter().find(|line| line.kind == DiffDisplayKind::HunkHeader).expect("header");
        assert_eq!(header.text, "@@ -1,3 +1,4 @@", "unfolded header is unadorned again: {}", header.text);
    }

    #[test]
    fn reply_box_renders_markdown_table_inside_border() {
        let mut panel = CodeReviewPanel::from_snapshot(two_hunk_snapshot());
        let table = "| Feature | Status |\n| ------- | ------ |\n| Tables | Working |";
        add_thread(&mut panel, 0, vec![
            ReviewComment { role: ReviewCommentRole::User, text: "Does this support tables?".to_owned(), partial: false },
            ReviewComment { role: ReviewCommentRole::Assistant, text: table.to_owned(), partial: false },
        ]);
        sync_code_review_layout(&mut panel, Rect::new(0, 0, 160, 60));
        let backend = TestBackend::new(160, 60);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render_code_review_panel(frame, &panel, crate::theme::DARK)).expect("draw");
        let rows: Vec<String> = terminal
            .backend()
            .buffer()
            .content
            .chunks(160)
            .map(|cells| cells.iter().map(|cell| cell.symbol()).collect())
            .collect();
        // The reply box has a labeled top border and a bottom border.
        assert!(
            rows.iter().any(|row| row.contains("╭") && row.contains("Answer 1")),
            "reply box top border with label missing: {rows:?}"
        );
        assert!(rows.iter().any(|row| row.contains("╰")), "reply box bottom border missing: {rows:?}");
        // The markdown table is rendered (box-drawing table chrome, header and
        // body cells) inside the box, not left as raw pipe source.
        assert!(rows.iter().any(|row| row.contains("Feature")), "table header missing: {rows:?}");
        assert!(rows.iter().any(|row| row.contains("Working")), "table body missing: {rows:?}");
        assert!(rows.iter().any(|row| row.contains('┬')), "table border chrome missing: {rows:?}");
        // The comment line keeps its card surface, and the reply box sits
        // between the comment card and the hunk's diff lines (no thread-summary row).
        let box_row = rows.iter().position(|row| row.contains("Answer 1")).expect("box row");
        let comment_row = rows.iter().position(|row| row.contains("Comment 1")).expect("comment row");
        let diff_row = rows.iter().position(|row| row.contains("fn main() {")).expect("diff row");
        assert!(comment_row < box_row && box_row < diff_row,
            "expected comment < box < diff, got {comment_row} < {box_row} < {diff_row}");
        assert!(!rows.iter().any(|row| row.contains("Review thread")), "no thread-summary row in render");
    }

    #[test]
    fn reply_box_keeps_plain_text_replies_readable() {
        let mut panel = CodeReviewPanel::from_snapshot(two_hunk_snapshot());
        add_thread(&mut panel, 0, vec![
            ReviewComment { role: ReviewCommentRole::User, text: "question".to_owned(), partial: false },
            ReviewComment { role: ReviewCommentRole::Assistant, text: "plain prose reply **with bold** and `code`".to_owned(), partial: false },
        ]);
        sync_code_review_layout(&mut panel, Rect::new(0, 0, 160, 60));
        let backend = TestBackend::new(160, 60);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render_code_review_panel(frame, &panel, crate::theme::DARK)).expect("draw");
        let rows: Vec<String> = terminal
            .backend()
            .buffer()
            .content
            .chunks(160)
            .map(|cells| cells.iter().map(|cell| cell.symbol()).collect())
            .collect();
        assert!(rows.iter().any(|row| row.contains("Answer 1")), "reply box label missing: {rows:?}");
        let reply = rows.iter().find(|row| row.contains("plain prose reply")).expect("reply body visible");
        assert!(reply.contains("bold") && reply.contains("code"), "inline markdown must render: {reply}");
        assert!(rows.iter().any(|row| row.contains('╭')), "box top border missing: {rows:?}");
        assert!(rows.iter().any(|row| row.contains('╰')), "box bottom border missing: {rows:?}");
    }

    /// Render every reply-box row for `thread` content at `pane_width` (the
    /// diff-pane width passed to `render_diff_line`) and return the joined
    /// text of each row.
    fn render_reply_box_rows(
        panel: &mut CodeReviewPanel,
        pane_width: usize,
    ) -> Vec<String> {
        let display = build_diff_display_lines(panel, panel.selected_file().expect("file"));
        display
            .iter()
            .filter(|line| line.kind == DiffDisplayKind::MarkdownBody)
            .map(|line| {
                let rendered = render_diff_line(line, 0, pane_width, crate::theme::DARK, 0);
                rendered
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn reply_box_top_content_and_bottom_borders_share_one_width() {
        // User-reported (screenshot): the streaming Answer card's frame is
        // broken — the labeled top border is drawn one column narrower than
        // the wrapped content rows, so a content row's right `│` lands to the
        // right of the top border's `╮`. Contract: every frame row — labeled
        // top, wrapped content, and bottom — shares one width and ends on the
        // same right-border column, at any pane width and with both wrapped
        // and max-width rows.
        for pane_width in [20usize, 24, 30, 40, 80] {
            let mut panel = CodeReviewPanel::from_snapshot(two_hunk_snapshot());
            panel.set_viewports(5, 10, pane_width);
            let identity = add_thread(&mut panel, 0, vec![
                ReviewComment { role: ReviewCommentRole::User, text: "question".to_owned(), partial: false },
            ]);
            // A single long streaming line that must wrap across the inner
            // width, plus a line exactly at the max content width.
            let content_width = pane_width.saturating_sub(8);
            panel.threads.get_mut(&identity).expect("thread").streaming_text = format!(
                "streamed answer {}\n{}",
                "word ".repeat(content_width),
                "x".repeat(content_width),
            );
            let rows = render_reply_box_rows(&mut panel, pane_width);
            assert!(
                rows.len() >= 4,
                "reply box must render top + wrapped content + bottom rows at {pane_width}: {rows:?}"
            );
            let widths: Vec<usize> = rows
                .iter()
                .map(|row| unicode_width::UnicodeWidthStr::width(row.as_str()))
                .collect();
            let expected = pane_width.saturating_sub(2);
            assert!(
                widths.iter().all(|&w| w == expected),
                "every reply box row must be {expected} columns wide (pane {pane_width}), got {widths:?}: {rows:?}"
            );
            // The right border glyph must sit at the same column on every row.
            assert!(
                rows.iter().all(|row| row.starts_with("  ╭") || row.starts_with("  │") || row.starts_with("  ╰")),
                "reply box rows must share the 2-column indent frame: {rows:?}"
            );
            assert!(
                rows.iter().all(|row| row.ends_with('╮') || row.ends_with('│') || row.ends_with('╯')),
                "reply box rows must end on the right border column: {rows:?}"
            );
        }
    }

    #[test]
    fn reply_box_painted_right_border_columns_align() {
        // End-to-end: the painted buffer must show the top `╮`, every content
        // `│`, and the bottom `╯` of the streaming answer box on one column.
        let mut panel = CodeReviewPanel::from_snapshot(two_hunk_snapshot());
        let identity = add_thread(&mut panel, 0, vec![
            ReviewComment { role: ReviewCommentRole::User, text: "question".to_owned(), partial: false },
        ]);
        panel.threads.get_mut(&identity).expect("thread").streaming_text =
            "streamed answer ".repeat(30);
        sync_code_review_layout(&mut panel, Rect::new(0, 0, 120, 40));
        let layout = compute_layout(Rect::new(0, 0, 120, 40));
        let pane_left = usize::from(layout.diff_pane.x);
        let pane_width = usize::from(layout.diff_pane.width);
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render_code_review_panel(frame, &panel, crate::theme::DARK)).expect("draw");
        let rows: Vec<String> = terminal
            .backend()
            .buffer()
            .content
            .chunks(120)
            .map(|cells| cells.iter().map(|cell| cell.symbol()).collect())
            .collect();
        let top = rows.iter().position(|row| row.contains("╭") && row.contains("Answer 1 · streaming"))
            .expect("streaming box top border row");
        let bottom = rows[top..]
            .iter()
            .position(|row| row.contains("╰"))
            .map(|offset| top + offset)
            .expect("streaming box bottom border row");
        let box_rows = &rows[top..=bottom];
        // Every box row's right border glyph must be at the same column.
        let right_columns: Vec<usize> = box_rows
            .iter()
            .map(|row| {
                // Restrict to the diff-pane columns; the pane starts at
                // `pane_left`, not at column 0 (tree pane renders first).
                let pane: Vec<char> = row
                    .chars()
                    .skip(pane_left)
                    .take(pane_width)
                    .collect();
                let local = pane
                    .iter()
                    .rposition(|c| matches!(c, '╮' | '│' | '╯'))
                    .expect("box row must carry a right border glyph in the diff pane");
                pane_left + local
            })
            .collect();
        assert!(
            right_columns.iter().all(|&col| col == right_columns[0]),
            "box right border must align on one column, got {right_columns:?}: {box_rows:?}"
        );
        // The frame is box_outer wide starting 2 columns into the pane.
        assert_eq!(
            right_columns[0],
            pane_left + pane_width.saturating_sub(3),
            "box right border must sit at the last frame column: {box_rows:?}"
        );
        // The wrapped content rows must carry the side border on both edges.
        assert!(
            box_rows
                .iter()
                .any(|row| row.contains("streamed answer") && row.contains('│')),
            "wrapped content rows missing: {box_rows:?}"
        );
    }

    /// Render every comment-card frame row (labeled top, wrapped bodies,
    /// closing bottom) for the panel's thread content at `pane_width` (the
    /// diff-pane width passed to `render_diff_line`) and return the joined
    /// text of each row.
    fn render_comment_card_rows(
        panel: &mut CodeReviewPanel,
        pane_width: usize,
    ) -> Vec<String> {
        let display = build_diff_display_lines(panel, panel.selected_file().expect("file"));
        display
            .iter()
            .filter(|line| {
                matches!(
                    line.kind,
                    DiffDisplayKind::AnnotationHeader
                        | DiffDisplayKind::AnnotationBody
                        | DiffDisplayKind::AnnotationBottom
                )
            })
            .map(|line| {
                let rendered = render_diff_line(line, 0, pane_width, crate::theme::DARK, 0);
                rendered
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn comment_cards_render_complete_green_frames() {
        // User feedback: the Comment card looked odd without a complete border.
        // Contract: the card renders as a full frame — labeled top, side
        // borders on every content row, and a closing bottom — sharing the
        // answer card's width math at any pane width, with the border and
        // title in the theme's green.
        for pane_width in [20usize, 24, 30, 40, 80] {
            let mut panel = CodeReviewPanel::from_snapshot(two_hunk_snapshot());
            panel.set_viewports(5, 10, pane_width);
            let identity = add_thread(&mut panel, 0, vec![
                ReviewComment { role: ReviewCommentRole::User, text: "question".to_owned(), partial: false },
            ]);
            // One long line that must wrap across the inner width, plus a line
            // exactly at the max content width, so the frame covers both.
            let content_width = pane_width.saturating_sub(8);
            panel.threads.get_mut(&identity).expect("thread").comments[0].text = format!(
                "a question that wraps {}\n{}",
                "word ".repeat(content_width),
                "x".repeat(content_width),
            );
            let rows = render_comment_card_rows(&mut panel, pane_width);
            assert!(
                rows.len() >= 4,
                "comment card must render top + wrapped content + bottom rows at {pane_width}: {rows:?}"
            );
            let widths: Vec<usize> = rows
                .iter()
                .map(|row| unicode_width::UnicodeWidthStr::width(row.as_str()))
                .collect();
            let expected = pane_width.saturating_sub(2);
            assert!(
                widths.iter().all(|&w| w == expected),
                "every comment card row must be {expected} columns wide (pane {pane_width}), got {widths:?}: {rows:?}"
            );
            assert!(
                rows.iter().all(|row| row.starts_with("  ╭") || row.starts_with("  │") || row.starts_with("  ╰")),
                "comment card rows must share the 2-column indent frame: {rows:?}"
            );
            assert!(
                rows.iter().all(|row| row.ends_with('╮') || row.ends_with('│') || row.ends_with('╯')),
                "comment card rows must end on the right border column: {rows:?}"
            );
            assert!(
                rows[0].contains("Comment 1"),
                "top border must carry the title: {rows:?}"
            );
        }
        // The frame rows are painted in the theme's green (theme.success), in
        // both palettes, and the title rides on the top border.
        for theme in [crate::theme::DARK, crate::theme::LIGHT] {
            let mut panel = CodeReviewPanel::from_snapshot(two_hunk_snapshot());
            add_thread(&mut panel, 0, vec![
                ReviewComment { role: ReviewCommentRole::User, text: "question".to_owned(), partial: false },
            ]);
            let display = build_diff_display_lines(&panel, panel.selected_file().expect("file"));
            let frame_rows: Vec<Line<'static>> = display
                .iter()
                .filter(|line| {
                    matches!(
                        line.kind,
                        DiffDisplayKind::AnnotationHeader
                            | DiffDisplayKind::AnnotationBody
                            | DiffDisplayKind::AnnotationBottom
                    )
                })
                .map(|line| render_diff_line(line, 0, 30, theme, 0))
                .collect();
            assert!(frame_rows.len() >= 3, "complete frame expected: {frame_rows:?}");
            for row in &frame_rows {
                let text = row.spans.iter().map(|span| span.content.as_ref()).collect::<String>();
                if text.starts_with("  │ ") {
                    assert_eq!(row.spans[0].style.fg, Some(theme.success), "left border: {text}");
                    assert_eq!(row.spans[2].style.fg, Some(theme.success), "right border: {text}");
                } else {
                    assert_eq!(row.spans[0].style.fg, Some(theme.success), "top/bottom border: {text}");
                }
            }
        }
    }

    #[test]
    fn comment_card_painted_green_frame_right_border_aligns() {
        // End-to-end: the painted buffer must show the comment card's top ╮,
        // every content │, and the bottom ╯ on one column, in green.
        let mut panel = CodeReviewPanel::from_snapshot(two_hunk_snapshot());
        add_thread(&mut panel, 0, vec![
            ReviewComment { role: ReviewCommentRole::User, text: "a wrapping question ".repeat(4), partial: false },
        ]);
        sync_code_review_layout(&mut panel, Rect::new(0, 0, 120, 40));
        let layout = compute_layout(Rect::new(0, 0, 120, 40));
        let pane_left = usize::from(layout.diff_pane.x);
        let pane_width = usize::from(layout.diff_pane.width);
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render_code_review_panel(frame, &panel, crate::theme::DARK)).expect("draw");
        let rows: Vec<String> = terminal
            .backend()
            .buffer()
            .content
            .chunks(120)
            .map(|cells| cells.iter().map(|cell| cell.symbol()).collect())
            .collect();
        let top = rows
            .iter()
            .position(|row| row.contains("╭") && row.contains("Comment 1"))
            .expect("comment card top border row");
        let bottom = rows[top..]
            .iter()
            .position(|row| row.contains("╰"))
            .map(|offset| top + offset)
            .expect("comment card bottom border row");
        let card_rows = &rows[top..=bottom];
        // Every frame row's right border glyph must be at the same column.
        let right_columns: Vec<usize> = card_rows
            .iter()
            .map(|row| {
                let pane: Vec<char> = row
                    .chars()
                    .skip(pane_left)
                    .take(pane_width)
                    .collect();
                let local = pane
                    .iter()
                    .rposition(|c| matches!(c, '╮' | '│' | '╯'))
                    .expect("card row must carry a right border glyph in the diff pane");
                pane_left + local
            })
            .collect();
        assert!(
            right_columns.iter().all(|&col| col == right_columns[0]),
            "comment card right border must align on one column, got {right_columns:?}: {card_rows:?}"
        );
        assert_eq!(
            right_columns[0],
            pane_left + pane_width.saturating_sub(3),
            "comment card right border must sit at the last frame column: {card_rows:?}"
        );
        // The frame is painted in the theme's green: every border glyph on the
        // left/right frame columns, plus the title on the top border.
        let green = crate::theme::DARK.success;
        for (offset, row) in card_rows.iter().enumerate() {
            let pane: Vec<char> = row.chars().skip(pane_left).take(pane_width).collect();
            for local in [2, pane_width.saturating_sub(1)] {
                let glyph = pane[local];
                if matches!(glyph, '╭' | '│' | '╮' | '╰' | '╯' | '─') {
                    let cell = &terminal.backend().buffer()[
                        (u16::try_from(pane_left + local).expect("frame column"), u16::try_from(top + offset).expect("frame row"))
                    ];
                    assert_eq!(cell.fg, green, "border glyph {glyph} must be green: {card_rows:?}");
                }
            }
        }
        let title_col = card_rows[0]
            .chars()
            .skip(pane_left)
            .take(pane_width)
            .position(|c| c == 'C')
            .map(|local| pane_left + local)
            .expect("title column");
        let title_cell = &terminal.backend().buffer()[
            (u16::try_from(title_col).expect("title column"), u16::try_from(top).expect("title row"))
        ];
        assert_eq!(title_cell.fg, green, "comment title must be green: {card_rows:?}");
    }
}
