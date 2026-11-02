//! Autonomous child state subpage for the unified Todo TUI surface.
//!
//! [`pi_coding::TodoChildDetailSnapshot`] is the sole detail authority. The
//! subpage keys every request and response by parent session plus child session.

use crate::todo_dag_view::redacted_sanitize;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use pi_coding::markdown::fit_text;
use pi_coding::{
    AgentStatus, JobStatus, TodoChildDetailActivityKind, TodoChildDetailSnapshot,
    TodoChildManifestEntry, TodoChildReceiptStatus, TodoChildStatus, TodoChildTaskStatus,
};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use crate::theme::Theme;

const TASK_LIMIT: usize = 12;
const RECEIPT_LIMIT: usize = 8;
const AGENT_LIMIT: usize = 8;
const JOB_LIMIT: usize = 8;
const ACTIVITY_LIMIT: usize = 8;
const IRC_LIMIT: usize = 8;
const TRANSCRIPT_LIMIT: usize = 12;
const DEPENDENCY_LIMIT: usize = 4;
const FIELD_WIDTH: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TodoChildDetailRowKind {
    Heading,
    Field,
    Item,
    Empty,
    Overflow,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TodoChildDetailRow {
    pub kind: TodoChildDetailRowKind,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TodoChildInputKind {
    Send,
    AddTask,
    Irc,
    Create,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TodoChildPanelIntent {
    Close,
    Select(String),
    Refresh(String),
    Send {
        child_session_id: String,
        message: String,
    },
    AddTask {
        child_session_id: String,
        content: String,
        depends_on: Vec<String>,
        agent: Option<String>,
        role: Option<String>,
        expected_revision: u64,
    },
    HubSend {
        child_session_id: String,
        recipient: String,
        body: String,
        reply_to: Option<String>,
    },
    Pause(String),
    Resume(String),
    Stop(String),
    Cancel(String),
    Archive(String),
    Create {
        label: Option<String>,
    },
    Handled,
    Unknown,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum TodoChildAddTaskField {
    #[default]
    Content,
    DependsOn,
    Agent,
    Role,
}

#[derive(Clone, Debug, Default)]
struct TodoChildAddTaskDraft {
    field: TodoChildAddTaskField,
    content: String,
    depends_on: String,
    agent: String,
    role: String,
}

impl TodoChildAddTaskDraft {
    fn active(&self) -> &str {
        match self.field {
            TodoChildAddTaskField::Content => &self.content,
            TodoChildAddTaskField::DependsOn => &self.depends_on,
            TodoChildAddTaskField::Agent => &self.agent,
            TodoChildAddTaskField::Role => &self.role,
        }
    }

    fn active_mut(&mut self) -> &mut String {
        match self.field {
            TodoChildAddTaskField::Content => &mut self.content,
            TodoChildAddTaskField::DependsOn => &mut self.depends_on,
            TodoChildAddTaskField::Agent => &mut self.agent,
            TodoChildAddTaskField::Role => &mut self.role,
        }
    }

    fn advance(&mut self, backwards: bool) {
        self.field = match (self.field, backwards) {
            (TodoChildAddTaskField::Content, false) | (TodoChildAddTaskField::Agent, true) => {
                TodoChildAddTaskField::DependsOn
            }
            (TodoChildAddTaskField::DependsOn, false) | (TodoChildAddTaskField::Role, true) => {
                TodoChildAddTaskField::Agent
            }
            (TodoChildAddTaskField::Agent, false) | (TodoChildAddTaskField::Content, true) => {
                TodoChildAddTaskField::Role
            }
            (TodoChildAddTaskField::Role, false) | (TodoChildAddTaskField::DependsOn, true) => {
                TodoChildAddTaskField::Content
            }
        };
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum TodoChildIrcField {
    #[default]
    Recipient,
    Body,
    ReplyTo,
}

#[derive(Clone, Debug, Default)]
struct TodoChildIrcDraft {
    field: TodoChildIrcField,
    recipient: String,
    body: String,
    reply_to: String,
}

impl TodoChildIrcDraft {
    fn active(&self) -> &str {
        match self.field {
            TodoChildIrcField::Recipient => &self.recipient,
            TodoChildIrcField::Body => &self.body,
            TodoChildIrcField::ReplyTo => &self.reply_to,
        }
    }

    fn active_mut(&mut self) -> &mut String {
        match self.field {
            TodoChildIrcField::Recipient => &mut self.recipient,
            TodoChildIrcField::Body => &mut self.body,
            TodoChildIrcField::ReplyTo => &mut self.reply_to,
        }
    }

    fn advance(&mut self, backwards: bool) {
        self.field = match (self.field, backwards) {
            (TodoChildIrcField::Recipient, false) | (TodoChildIrcField::ReplyTo, true) => {
                TodoChildIrcField::Body
            }
            (TodoChildIrcField::Body, false) | (TodoChildIrcField::Recipient, true) => {
                TodoChildIrcField::ReplyTo
            }
            (TodoChildIrcField::ReplyTo, false) | (TodoChildIrcField::Body, true) => {
                TodoChildIrcField::Recipient
            }
        };
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TodoChildPanel {
    parent_session_id: String,
    children: Vec<TodoChildManifestEntry>,
    selected: usize,
    detail: Option<TodoChildDetailSnapshot>,
    loading_detail: Option<(String, u64)>,
    latest_detail_request: u64,
    error: Option<String>,
    scroll: usize,
    input: Option<TodoChildInputKind>,
    draft: String,
    add_task: TodoChildAddTaskDraft,
    irc: TodoChildIrcDraft,
}

impl TodoChildPanel {
    pub(crate) fn new(parent_session_id: String, children: Vec<TodoChildManifestEntry>) -> Self {
        Self {
            parent_session_id,
            children,
            selected: 0,
            detail: None,
            loading_detail: None,
            latest_detail_request: 0,
            error: None,
            scroll: 0,
            input: None,
            draft: String::new(),
            add_task: TodoChildAddTaskDraft::default(),
            irc: TodoChildIrcDraft::default(),
        }
    }

    pub(crate) fn parent_session_id(&self) -> &str {
        &self.parent_session_id
    }

    pub(crate) fn selected_child_id(&self) -> Option<&str> {
        self.children
            .get(self.selected)
            .map(|entry| entry.child_session_id.as_str())
    }
    pub(crate) fn detail_request_in_flight(&self) -> bool {
        self.loading_detail.is_some()
    }

    /// True while an inline input (Send/AddTask/Irc/Create) is active. The
    /// unified Todo overlay uses this to keep Tab for AddTask/Irc field
    /// advance instead of toggling to the Tasks (DAG) page mid-input.
    pub(crate) fn input_active(&self) -> bool {
        self.input.is_some()
    }

    pub(crate) fn select_child(&mut self, child_session_id: &str) {
        if let Some(index) = self
            .children
            .iter()
            .position(|entry| entry.child_session_id == child_session_id)
        {
            self.selected = index;
            self.detail = None;
            self.loading_detail = None;
            self.scroll = 0;
        }
    }

    pub(crate) fn replace_children(
        &mut self,
        parent_session_id: &str,
        children: Vec<TodoChildManifestEntry>,
    ) -> bool {
        if self.parent_session_id != parent_session_id {
            return false;
        }
        let selected = self.selected_child_id().map(str::to_owned);
        self.children = children;
        self.selected = selected
            .as_deref()
            .and_then(|id| {
                self.children
                    .iter()
                    .position(|entry| entry.child_session_id == id)
            })
            .unwrap_or(0)
            .min(self.children.len().saturating_sub(1));
        if self.selected_child_id() != selected.as_deref() {
            self.detail = None;
            self.loading_detail = None;
        }
        true
    }

    pub(crate) fn begin_loading(
        &mut self,
        parent_session_id: &str,
        child_session_id: &str,
        request_generation: u64,
    ) -> bool {
        if self.parent_session_id != parent_session_id
            || self.selected_child_id() != Some(child_session_id)
            || request_generation < self.latest_detail_request
        {
            return false;
        }
        self.latest_detail_request = request_generation;
        self.loading_detail = Some((child_session_id.to_owned(), request_generation));
        self.error = None;
        true
    }

    pub(crate) fn apply_detail(
        &mut self,
        request_generation: u64,
        detail: TodoChildDetailSnapshot,
    ) -> bool {
        if detail.parent_session_id != self.parent_session_id
            || self.selected_child_id() != Some(detail.child_session_id.as_str())
            || self.loading_detail.as_ref()
                != Some(&(detail.child_session_id.clone(), request_generation))
            || self
                .detail
                .as_ref()
                .is_some_and(|current| detail.revision < current.revision)
        {
            return false;
        }
        self.loading_detail = None;
        self.error = None;
        self.detail = Some(detail);
        true
    }

    pub(crate) fn apply_error(
        &mut self,
        parent_session_id: &str,
        child_session_id: &str,
        request_generation: u64,
        error: String,
    ) -> bool {
        if self.parent_session_id != parent_session_id
            || self.selected_child_id() != Some(child_session_id)
            || self.loading_detail.as_ref()
                != Some(&(child_session_id.to_owned(), request_generation))
        {
            return false;
        }
        self.loading_detail = None;
        self.error = Some(error);
        true
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> TodoChildPanelIntent {
        if key.kind == KeyEventKind::Release {
            return TodoChildPanelIntent::Handled;
        }
        if self.input.is_some() {
            return self.handle_input_key(key);
        }
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') if plain => TodoChildPanelIntent::Close,
            KeyCode::Up | KeyCode::Char('k') if plain => {
                self.selected = self.selected.saturating_sub(1);
                self.detail = None;
                self.loading_detail = None;
                self.scroll = 0;
                self.selected_intent()
            }
            KeyCode::Down | KeyCode::Char('j') if plain => {
                self.selected = (self.selected + 1).min(self.children.len().saturating_sub(1));
                self.detail = None;
                self.loading_detail = None;
                self.scroll = 0;
                self.selected_intent()
            }
            KeyCode::Enter => self.selected_intent(),
            KeyCode::Char('r') if plain => self.child_intent(TodoChildPanelIntent::Refresh),
            KeyCode::Char('s') if plain => self.begin_input(TodoChildInputKind::Send),
            KeyCode::Char('a') if plain => self.begin_input(TodoChildInputKind::AddTask),
            KeyCode::Char('n') if plain => self.begin_input(TodoChildInputKind::Create),
            KeyCode::Char('p') if plain => self.child_intent(TodoChildPanelIntent::Pause),
            KeyCode::Char('u') if plain => self.child_intent(TodoChildPanelIntent::Resume),
            KeyCode::Char('x') if plain => self.child_intent(TodoChildPanelIntent::Stop),
            KeyCode::Char('c') if plain => self.child_intent(TodoChildPanelIntent::Cancel),
            KeyCode::Char('d') if plain => self.child_intent(TodoChildPanelIntent::Archive),
            KeyCode::Char('i') if plain => self.begin_input(TodoChildInputKind::Irc),
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(8);
                TodoChildPanelIntent::Handled
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(8);
                TodoChildPanelIntent::Handled
            }
            _ => TodoChildPanelIntent::Unknown,
        }
    }

    pub(crate) fn handle_paste(&mut self, payload: &str) -> bool {
        match self.input {
            Some(TodoChildInputKind::Send) | Some(TodoChildInputKind::Create) => {
                self.draft.push_str(payload)
            }
            Some(TodoChildInputKind::AddTask) => self.add_task.active_mut().push_str(payload),
            Some(TodoChildInputKind::Irc) => self.irc.active_mut().push_str(payload),
            None => return false,
        }
        true
    }

    fn handle_input_key(&mut self, key: KeyEvent) -> TodoChildPanelIntent {
        match key.code {
            KeyCode::Esc => {
                self.input = None;
                self.draft.clear();
                self.add_task = TodoChildAddTaskDraft::default();
                self.irc = TodoChildIrcDraft::default();
                TodoChildPanelIntent::Handled
            }
            KeyCode::Backspace => {
                match self.input {
                    Some(TodoChildInputKind::Send) | Some(TodoChildInputKind::Create) => {
                        self.draft.pop();
                    }
                    Some(TodoChildInputKind::AddTask) => {
                        self.add_task.active_mut().pop();
                    }
                    Some(TodoChildInputKind::Irc) => {
                        self.irc.active_mut().pop();
                    }
                    None => {}
                }
                TodoChildPanelIntent::Handled
            }
            KeyCode::Tab | KeyCode::BackTab
                if matches!(
                    self.input,
                    Some(TodoChildInputKind::AddTask | TodoChildInputKind::Irc)
                ) =>
            {
                let backwards = key.code == KeyCode::BackTab;
                match self.input {
                    Some(TodoChildInputKind::AddTask) => self.add_task.advance(backwards),
                    Some(TodoChildInputKind::Irc) => self.irc.advance(backwards),
                    _ => {}
                }
                TodoChildPanelIntent::Handled
            }
            KeyCode::Enter => self.submit_input(),
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                match self.input {
                    Some(TodoChildInputKind::Send) | Some(TodoChildInputKind::Create) => {
                        self.draft.push(character)
                    }
                    Some(TodoChildInputKind::AddTask) => self.add_task.active_mut().push(character),
                    Some(TodoChildInputKind::Irc) => self.irc.active_mut().push(character),
                    None => {}
                }
                TodoChildPanelIntent::Handled
            }
            _ => TodoChildPanelIntent::Unknown,
        }
    }

    fn submit_input(&mut self) -> TodoChildPanelIntent {
        let Some(kind) = self.input.take() else {
            return TodoChildPanelIntent::Handled;
        };
        if matches!(kind, TodoChildInputKind::Create) {
            let label = nonempty(self.draft.trim().to_owned());
            self.draft.clear();
            return TodoChildPanelIntent::Create { label };
        }
        let Some(child_session_id) = self.selected_child_id().map(str::to_owned) else {
            return TodoChildPanelIntent::Handled;
        };
        match kind {
            TodoChildInputKind::Send => {
                let message = self.draft.trim().to_owned();
                self.draft.clear();
                if message.is_empty() {
                    TodoChildPanelIntent::Handled
                } else {
                    TodoChildPanelIntent::Send {
                        child_session_id,
                        message,
                    }
                }
            }
            TodoChildInputKind::AddTask => {
                let Some(expected_revision) = self.detail.as_ref().map(|detail| detail.revision)
                else {
                    return TodoChildPanelIntent::Handled;
                };
                let draft = std::mem::take(&mut self.add_task);
                let content = draft.content.trim().to_owned();
                if content.is_empty() {
                    return TodoChildPanelIntent::Handled;
                }
                TodoChildPanelIntent::AddTask {
                    child_session_id,
                    content,
                    depends_on: draft
                        .depends_on
                        .split(',')
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(str::to_owned)
                        .collect(),
                    agent: nonempty(draft.agent),
                    role: nonempty(draft.role),
                    expected_revision,
                }
            }
            TodoChildInputKind::Irc => {
                let draft = std::mem::take(&mut self.irc);
                let recipient = draft.recipient.trim().to_owned();
                let body = draft.body.trim().to_owned();
                if recipient.is_empty() || body.is_empty() {
                    return TodoChildPanelIntent::Handled;
                }
                TodoChildPanelIntent::HubSend {
                    child_session_id,
                    recipient,
                    body,
                    reply_to: nonempty(draft.reply_to),
                }
            }
            TodoChildInputKind::Create => TodoChildPanelIntent::Handled,
        }
    }

    fn begin_input(&mut self, kind: TodoChildInputKind) -> TodoChildPanelIntent {
        let can_open = match kind {
            TodoChildInputKind::Create => true,
            TodoChildInputKind::Send | TodoChildInputKind::Irc => {
                self.selected_child_id().is_some()
            }
            TodoChildInputKind::AddTask => self.detail.is_some(),
        };
        if can_open {
            self.input = Some(kind);
            self.draft.clear();
            self.add_task = TodoChildAddTaskDraft::default();
            self.irc = TodoChildIrcDraft::default();
        }
        TodoChildPanelIntent::Handled
    }

    fn selected_intent(&self) -> TodoChildPanelIntent {
        self.child_intent(TodoChildPanelIntent::Select)
    }

    fn child_intent(
        &self,
        intent: impl FnOnce(String) -> TodoChildPanelIntent,
    ) -> TodoChildPanelIntent {
        self.selected_child_id()
            .map(|id| intent(id.to_owned()))
            .unwrap_or(TodoChildPanelIntent::Handled)
    }
}

pub(crate) fn render_todo_child_panel(
    frame: &mut ratatui::Frame<'_>,
    panel: &TodoChildPanel,
    theme: Theme,
) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_accent))
        .title(" Todo · Child sessions · Tab Tasks ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(32),
            Constraint::Length(1),
            Constraint::Min(20),
        ])
        .split(inner);
    render_child_list(frame, panel, columns[0], theme);
    frame.render_widget(
        Paragraph::new("│").style(Style::default().fg(theme.border)),
        columns[1],
    );
    render_child_detail(frame, panel, columns[2], theme);
}

fn render_child_list(
    frame: &mut ratatui::Frame<'_>,
    panel: &TodoChildPanel,
    area: Rect,
    theme: Theme,
) {
    let mut lines = vec![Line::from(Span::styled(
        "Todo children",
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    ))];
    if panel.children.is_empty() {
        lines.push(Line::from(Span::styled(
            "No Todo children",
            Style::default().fg(theme.muted),
        )));
    }
    for (index, child) in panel.children.iter().enumerate() {
        let selected = index == panel.selected;
        let marker = if selected { "›" } else { " " };
        let name = child.label.as_deref().unwrap_or(&child.child_session_id);
        let text = fit_text(
            &format!(
                "{marker} {} · {}",
                compact_text(name),
                lifecycle_status_label(child.status)
            ),
            usize::from(area.width.max(1)),
        );
        let style = if selected {
            Style::default()
                .fg(theme.text)
                .bg(theme.selected_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text)
        };
        lines.push(Line::from(Span::styled(text, style)));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_child_detail(
    frame: &mut ratatui::Frame<'_>,
    panel: &TodoChildPanel,
    area: Rect,
    theme: Theme,
) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(2)])
        .split(area);
    let width = usize::from(sections[0].width.max(1));
    let mut lines = if let Some(detail) = panel.detail.as_ref() {
        project_todo_child_detail_rows(detail, width)
            .into_iter()
            .map(|row| detail_line(row, theme))
            .collect::<Vec<_>>()
    } else {
        // No authoritative snapshot yet: the detail fetch is loading or has
        // failed (for example after `/todo stop` leaves the child runtime
        // unavailable, so `todo_child_detail` returns Err). The compact list
        // row already shows `<label> · <state>`; mirror the selected child's
        // manifest status as an explicit `Status: <state>` detail line through
        // the same `lifecycle_status_label` mapping the snapshot path uses, so
        // a stopped (or paused/failed/...) child stays legible even when the
        // runtime-unavailable error is the only detail payload. The error
        // itself is retained below the status line.
        let mut base: Vec<Line<'static>> = Vec::new();
        if let Some(status) = panel.children.get(panel.selected).map(|child| child.status) {
            base.push(detail_line(
                TodoChildDetailRow {
                    kind: TodoChildDetailRowKind::Field,
                    text: fit_text(
                        &compact_text(&format!("Status: {}", lifecycle_status_label(status))),
                        width,
                    ),
                },
                theme,
            ));
        }
        if let Some(error) = panel.error.as_deref() {
            base.push(Line::from(Span::styled(
                fit_text(&compact_text(error), width),
                Style::default().fg(theme.error),
            )));
        } else if panel.loading_detail.is_some() {
            base.push(Line::from(Span::styled(
                "Loading authoritative child detail…",
                Style::default().fg(theme.muted),
            )));
        } else if panel.selected_child_id().is_none() {
            base.push(Line::from(Span::styled(
                "Select a Todo child",
                Style::default().fg(theme.muted),
            )));
        }
        base
    };
    if panel.loading_detail.is_some() && panel.detail.is_some() {
        lines.insert(
            0,
            Line::from(Span::styled(
                "Refreshing authoritative child detail…",
                Style::default().fg(theme.muted),
            )),
        );
    }
    let viewport = usize::from(sections[0].height.max(1));
    let scroll = panel.scroll.min(lines.len().saturating_sub(viewport));
    lines = lines.into_iter().skip(scroll).take(viewport).collect();
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        sections[0],
    );
    let footer = match panel.input {
        Some(TodoChildInputKind::Send) => format!(
            " Send message › {}",
            fit_text(&compact_text(&panel.draft), width.saturating_sub(16))
        ),
        Some(TodoChildInputKind::AddTask) => format!(
            " Add task [{:?}] › {} · Tab field · Enter submit",
            panel.add_task.field,
            fit_text(
                &compact_text(panel.add_task.active()),
                width.saturating_sub(44),
            )
        ),
        Some(TodoChildInputKind::Irc) => format!(
            " Child IRC [{:?}] › {} · Tab field · Enter send",
            panel.irc.field,
            fit_text(
                &compact_text(panel.irc.active()),
                width.saturating_sub(42),
            )
        ),
        Some(TodoChildInputKind::Create) => format!(
            " Create child › {} · Enter submit",
            fit_text(&compact_text(&panel.draft), width.saturating_sub(24))
        ),
        None => " ↑/↓ child · Enter detail · Tab Tasks · n create · s parent send · i child IRC · a add task · p/u pause/resume · x stop · c cancel · d archive · r refresh · Esc close ".to_owned(),
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            fit_text(&footer, width),
            Style::default().fg(theme.dim),
        ))),
        sections[1],
    );
}

fn detail_line(row: TodoChildDetailRow, theme: Theme) -> Line<'static> {
    let style = match row.kind {
        TodoChildDetailRowKind::Heading => Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
        TodoChildDetailRowKind::Field | TodoChildDetailRowKind::Item => {
            Style::default().fg(theme.text)
        }
        TodoChildDetailRowKind::Empty | TodoChildDetailRowKind::Overflow => {
            Style::default().fg(theme.muted)
        }
    };
    Line::from(Span::styled(row.text, style))
}

#[must_use]
pub(crate) fn project_todo_child_detail_rows(
    snapshot: &TodoChildDetailSnapshot,
    width: usize,
) -> Vec<TodoChildDetailRow> {
    if width == 0 {
        return Vec::new();
    }
    let mut rows = Vec::with_capacity(88);
    push_row(
        &mut rows,
        TodoChildDetailRowKind::Heading,
        &format!("Todo {}", snapshot.todo_id),
        width,
    );
    let parent_entry = snapshot
        .parent
        .entry_id
        .as_deref()
        .map(|entry| format!(" · entry {}", brief(entry)))
        .unwrap_or_default();
    push_row(
        &mut rows,
        TodoChildDetailRowKind::Field,
        &format!(
            "Parent: {}{} · revision {}",
            snapshot.parent.session_id, parent_entry, snapshot.parent.revision
        ),
        width,
    );
    push_row(
        &mut rows,
        TodoChildDetailRowKind::Field,
        &format!("Workspace: {}", snapshot.workspace.workspace),
        width,
    );
    push_row(
        &mut rows,
        TodoChildDetailRowKind::Field,
        &format!("CWD: {}", snapshot.workspace.cwd),
        width,
    );
    push_row(
        &mut rows,
        TodoChildDetailRowKind::Field,
        &format!(
            "Status: {} · revision {} · {}",
            lifecycle_status_label(snapshot.status),
            snapshot.revision,
            if snapshot.is_running {
                "running"
            } else {
                "not running"
            }
        ),
        width,
    );
    push_row(
        &mut rows,
        TodoChildDetailRowKind::Field,
        &format!("Created: {}", format_timestamp(snapshot.created_at)),
        width,
    );
    push_row(
        &mut rows,
        TodoChildDetailRowKind::Field,
        &format!("Transcript entries: {}", snapshot.transcript_entries),
        width,
    );
    if let Some(label) = snapshot.label.as_deref() {
        push_row(
            &mut rows,
            TodoChildDetailRowKind::Field,
            &format!("Label: {label}"),
            width,
        );
    }
    if let Some(error) = snapshot.last_error.as_deref() {
        push_row(
            &mut rows,
            TodoChildDetailRowKind::Field,
            &format!("Last error: {error}"),
            width,
        );
    }

    push_heading(&mut rows, "Supervisor", width);
    match snapshot.supervisor.as_ref() {
        Some(supervisor) => push_row(
            &mut rows,
            TodoChildDetailRowKind::Item,
            &format!(
                "{} · {} · {}",
                display_name(&supervisor.display_name, &supervisor.id),
                supervisor
                    .role
                    .as_deref()
                    .map(brief)
                    .unwrap_or_else(|| "no role".to_owned()),
                agent_status_label(supervisor.status)
            ),
            width,
        ),
        None => push_empty(&mut rows, "No supervisor projection", width),
    }

    push_heading(&mut rows, "Tasks", width);
    if snapshot.tasks.is_empty() {
        push_empty(&mut rows, "No tasks", width);
    }
    for task in snapshot.tasks.iter().take(TASK_LIMIT) {
        push_row(
            &mut rows,
            TodoChildDetailRowKind::Item,
            &format!(
                "{} {} · {} · created {} · deps {} · {}",
                task_status_marker(task.status),
                brief(&task.id),
                task_status_label(task.status),
                format_timestamp(task.created_at),
                dependency_label(&task.depends_on),
                ownership_label(task.agent.as_deref(), task.role.as_deref())
            ),
            width,
        );
        push_row(
            &mut rows,
            TodoChildDetailRowKind::Item,
            &format!("  {}", task.content),
            width,
        );
        if let Some(summary) = task.summary.as_deref() {
            push_row(
                &mut rows,
                TodoChildDetailRowKind::Item,
                &format!("  summary · {summary}"),
                width,
            );
        }
    }
    push_overflow(
        &mut rows,
        snapshot.tasks.len().saturating_sub(TASK_LIMIT),
        "tasks",
        width,
    );

    push_heading(&mut rows, "Receipts", width);
    if snapshot.receipts.is_empty() {
        push_empty(&mut rows, "No receipts", width);
    }
    for receipt in snapshot.receipts.iter().rev().take(RECEIPT_LIMIT).rev() {
        let summary = receipt
            .summary
            .as_deref()
            .map(|value| format!(" · {value}"))
            .unwrap_or_default();
        push_row(
            &mut rows,
            TodoChildDetailRowKind::Item,
            &format!(
                "{} · {} · message {}{}",
                brief(&receipt.id),
                receipt_status_label(receipt.status),
                brief(&receipt.message_id),
                summary
            ),
            width,
        );
    }
    push_earlier(
        &mut rows,
        snapshot.receipts.len().saturating_sub(RECEIPT_LIMIT),
        width,
    );

    push_heading(&mut rows, "Agents", width);
    if snapshot.agents.is_empty() {
        push_empty(&mut rows, "No agents", width);
    }
    for agent in snapshot.agents.iter().take(AGENT_LIMIT) {
        let task = agent
            .task_id
            .as_deref()
            .map(|id| format!(" · task {}", brief(id)))
            .unwrap_or_default();
        push_row(
            &mut rows,
            TodoChildDetailRowKind::Item,
            &format!(
                "{} · {} · {}{}",
                display_name(&agent.display_name, &agent.id),
                agent
                    .role
                    .as_deref()
                    .map(brief)
                    .unwrap_or_else(|| "no role".to_owned()),
                agent_status_label(agent.status),
                task
            ),
            width,
        );
    }
    push_overflow(
        &mut rows,
        snapshot.agents.len().saturating_sub(AGENT_LIMIT),
        "agents",
        width,
    );

    push_heading(&mut rows, "Jobs", width);
    if snapshot.jobs.is_empty() {
        push_empty(&mut rows, "No jobs", width);
    }
    for job in snapshot.jobs.iter().take(JOB_LIMIT) {
        let task = job
            .task_id
            .as_deref()
            .map(|id| format!(" · task {}", brief(id)))
            .unwrap_or_default();
        push_row(
            &mut rows,
            TodoChildDetailRowKind::Item,
            &format!(
                "{} · {} · {} · job {}{}",
                display_name(&job.display_name, &job.job_id),
                job.role
                    .as_deref()
                    .map(brief)
                    .unwrap_or_else(|| "no role".to_owned()),
                job_status_label(job.status),
                brief(&job.job_id),
                task
            ),
            width,
        );
        if let Some(summary) = job.summary.as_deref() {
            push_row(
                &mut rows,
                TodoChildDetailRowKind::Item,
                &format!("  {summary}"),
                width,
            );
        }
    }
    push_overflow(
        &mut rows,
        snapshot.jobs.len().saturating_sub(JOB_LIMIT),
        "jobs",
        width,
    );

    push_heading(&mut rows, "Activity", width);
    if snapshot.activity.is_empty() {
        push_empty(&mut rows, "No activity", width);
    }
    for entry in snapshot.activity.iter().rev().take(ACTIVITY_LIMIT).rev() {
        let actor = entry
            .actor
            .as_deref()
            .map(|value| format!("{} · ", brief(value)))
            .unwrap_or_default();
        push_row(
            &mut rows,
            TodoChildDetailRowKind::Item,
            &format!(
                "{} {}{} · {}",
                format_clock(entry.at_ms),
                actor,
                activity_kind_label(entry.kind),
                entry.text
            ),
            width,
        );
    }
    push_earlier(
        &mut rows,
        snapshot.activity.len().saturating_sub(ACTIVITY_LIMIT),
        width,
    );

    push_heading(&mut rows, "IRC", width);
    if snapshot.irc.is_empty() {
        push_empty(&mut rows, "No IRC messages", width);
    }
    for message in snapshot.irc.iter().rev().take(IRC_LIMIT).rev() {
        let reply = message
            .reply_to
            .as_deref()
            .map(|id| format!(" · reply {}", brief(id)))
            .unwrap_or_default();
        push_row(
            &mut rows,
            TodoChildDetailRowKind::Item,
            &format!(
                "{} {} → {} · {}{}",
                format_clock(message.at_ms),
                message.from,
                message.to,
                message.text,
                reply
            ),
            width,
        );
    }
    push_earlier(
        &mut rows,
        snapshot.irc.len().saturating_sub(IRC_LIMIT),
        width,
    );

    push_heading(&mut rows, "Transcript", width);
    if snapshot.transcript.is_empty() {
        push_empty(&mut rows, "No transcript entries", width);
    }
    for entry in snapshot
        .transcript
        .iter()
        .rev()
        .take(TRANSCRIPT_LIMIT)
        .rev()
    {
        let at = entry
            .at_ms
            .and_then(|millis| u64::try_from(millis).ok())
            .map(|millis| format!("{} ", format_clock(millis)))
            .unwrap_or_default();
        let actor = entry
            .actor
            .as_deref()
            .map(|value| format!("/{}", brief(value)))
            .unwrap_or_default();
        let interrupted = if entry.interrupted {
            " · interrupted"
        } else {
            ""
        };
        push_row(
            &mut rows,
            TodoChildDetailRowKind::Item,
            &format!(
                "{}{}{}{} · {}",
                at, entry.role, actor, interrupted, entry.text
            ),
            width,
        );
    }
    push_earlier(
        &mut rows,
        snapshot.transcript.len().saturating_sub(TRANSCRIPT_LIMIT),
        width,
    );
    rows
}

fn push_heading(rows: &mut Vec<TodoChildDetailRow>, label: &str, width: usize) {
    push_row(rows, TodoChildDetailRowKind::Heading, label, width);
}
fn push_empty(rows: &mut Vec<TodoChildDetailRow>, label: &str, width: usize) {
    push_row(rows, TodoChildDetailRowKind::Empty, label, width);
}
fn push_overflow(rows: &mut Vec<TodoChildDetailRow>, omitted: usize, label: &str, width: usize) {
    if omitted > 0 {
        push_row(
            rows,
            TodoChildDetailRowKind::Overflow,
            &format!("… {omitted} more {label}"),
            width,
        );
    }
}
fn push_earlier(rows: &mut Vec<TodoChildDetailRow>, omitted: usize, width: usize) {
    if omitted > 0 {
        push_row(
            rows,
            TodoChildDetailRowKind::Overflow,
            &format!("… {omitted} earlier"),
            width,
        );
    }
}
fn push_row(
    rows: &mut Vec<TodoChildDetailRow>,
    kind: TodoChildDetailRowKind,
    value: &str,
    width: usize,
) {
    rows.push(TodoChildDetailRow {
        kind,
        text: fit_text(&compact_text(value), width),
    });
}
fn lifecycle_status_label(status: TodoChildStatus) -> &'static str {
    match status {
        TodoChildStatus::Created => "created",
        TodoChildStatus::Idle => "idle",
        TodoChildStatus::Running => "running",
        TodoChildStatus::Paused => "paused",
        TodoChildStatus::Stopped => "stopped",
        TodoChildStatus::Failed => "failed",
    }
}
fn task_status_label(status: TodoChildTaskStatus) -> &'static str {
    match status {
        TodoChildTaskStatus::Pending => "pending",
        TodoChildTaskStatus::Running => "running",
        TodoChildTaskStatus::Completed => "completed",
        TodoChildTaskStatus::Failed => "failed",
        TodoChildTaskStatus::Interrupted => "interrupted",
    }
}
fn task_status_marker(status: TodoChildTaskStatus) -> &'static str {
    match status {
        TodoChildTaskStatus::Pending => "○",
        TodoChildTaskStatus::Running => "●",
        TodoChildTaskStatus::Completed => "✓",
        TodoChildTaskStatus::Failed | TodoChildTaskStatus::Interrupted => "!",
    }
}
fn receipt_status_label(status: TodoChildReceiptStatus) -> &'static str {
    match status {
        TodoChildReceiptStatus::Queued => "queued",
        TodoChildReceiptStatus::Running => "running",
        TodoChildReceiptStatus::Completed => "completed",
        TodoChildReceiptStatus::Failed => "failed",
    }
}
fn agent_status_label(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Queued => "queued",
        AgentStatus::Running => "running",
        AgentStatus::Idle => "idle",
        AgentStatus::Parked => "parked",
        AgentStatus::Aborted => "aborted",
    }
}
fn job_status_label(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Queued => "queued",
        JobStatus::Running => "running",
        JobStatus::Completed => "completed",
        JobStatus::Failed => "failed",
        JobStatus::Cancelled => "cancelled",
    }
}
fn activity_kind_label(kind: TodoChildDetailActivityKind) -> &'static str {
    match kind {
        TodoChildDetailActivityKind::Task => "task",
        TodoChildDetailActivityKind::Irc => "irc",
    }
}
fn dependency_label(dependencies: &[String]) -> String {
    if dependencies.is_empty() {
        return "none".to_owned();
    }
    let shown = dependencies
        .iter()
        .take(DEPENDENCY_LIMIT)
        .map(|dependency| brief(dependency))
        .collect::<Vec<_>>()
        .join(", ");
    let omitted = dependencies.len().saturating_sub(DEPENDENCY_LIMIT);
    if omitted == 0 {
        shown
    } else {
        format!("{shown} +{omitted}")
    }
}
fn ownership_label(agent: Option<&str>, role: Option<&str>) -> String {
    match (agent, role) {
        (Some(agent), Some(role)) => format!("agent {} · role {}", brief(agent), brief(role)),
        (Some(agent), None) => format!("agent {}", brief(agent)),
        (None, Some(role)) => format!("role {}", brief(role)),
        (None, None) => "unassigned".to_owned(),
    }
}
fn display_name(name: &str, id: &str) -> String {
    if name.trim().is_empty() {
        brief(id)
    } else {
        brief(name)
    }
}
fn nonempty(value: String) -> Option<String> {
    let value = value.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

fn brief(value: &str) -> String {
    fit_text(&compact_text(value), FIELD_WIDTH)
}
/// Centralized redact-before-sanitize shared with the Todo DAG panel: every
/// child-detail field (task/receipt/agent/job/activity/IRC/transcript/identity
/// text, plus the error-panel fallback below) flows through here so a
/// credential a child or model writes into any surfaced field is scrubbed
/// before it can be measured or rendered. Canonical authority IDs are not
/// secret-shaped, so they remain unchanged internally and wherever the
/// existing presentation intentionally exposes them; opaque fields stay
/// hidden.
fn compact_text(value: &str) -> String {
    redacted_sanitize(value)
}
fn format_timestamp(millis: i64) -> String {
    u64::try_from(millis)
        .map(format_clock)
        .unwrap_or_else(|_| millis.to_string())
}

fn format_clock(millis: u64) -> String {
    let total_seconds = millis / 1_000;
    format!(
        "{:02}:{:02}:{:02}",
        (total_seconds / 3_600) % 24,
        (total_seconds / 60) % 60,
        total_seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_coding::{
        TodoChildDetailActivity, TodoChildDetailAgent, TodoChildDetailIrcMessage,
        TodoChildDetailJob, TodoChildDetailTranscriptEntry, TodoChildKind,
        TodoChildParentProvenance, TodoChildReceipt, TodoChildTask, TodoChildWorkspaceIdentity,
    };
    use ratatui::{Terminal, backend::TestBackend};

    /// The `sk-…` shape is the first whole-match pattern in `SECRET_PATTERNS`;
    /// one fixture exercises the shared redaction path every child-detail
    /// field now flows through via `compact_text` -> `redacted_sanitize`.
    const SECRET: &str = "sk-abcdef1234567890aabbccdd";

    fn secret_field(prefix: &str) -> String {
        format!("{prefix} {SECRET} tail")
    }

    fn snapshot_with_secrets_in_every_section() -> TodoChildDetailSnapshot {
        TodoChildDetailSnapshot {
            parent_session_id: secret_field("parent"),
            child_session_id: "child-session-fixed".to_owned(),
            todo_id: secret_field("todo"),
            parent: TodoChildParentProvenance {
                session_id: secret_field("parent-session"),
                entry_id: Some(secret_field("entry")),
                revision: 1,
            },
            workspace: TodoChildWorkspaceIdentity {
                workspace: secret_field("workspace"),
                cwd: secret_field("cwd"),
            },
            status: TodoChildStatus::Failed,
            is_running: false,
            revision: 1,
            label: Some(secret_field("label")),
            created_at: 0,
            last_error: Some(secret_field("last error")),
            transcript_entries: 1,
            tasks: vec![TodoChildTask {
                id: "task-fixed".to_owned(),
                content: secret_field("task content"),
                depends_on: vec!["dep".to_owned()],
                status: TodoChildTaskStatus::Running,
                agent: Some(secret_field("agent")),
                role: Some(secret_field("role")),
                created_at: 0,
                summary: Some(secret_field("task summary")),
            }],
            receipts: vec![TodoChildReceipt {
                id: "receipt-fixed".to_owned(),
                parent_session_id: secret_field("receipt parent"),
                child_session_id: "child-session-fixed".to_owned(),
                message_id: "msg-fixed".to_owned(),
                accepted_at: 0,
                status: TodoChildReceiptStatus::Completed,
                summary: Some(secret_field("receipt summary")),
            }],
            supervisor: Some(TodoChildDetailAgent {
                id: "sup-fixed".to_owned(),
                display_name: secret_field("supervisor"),
                role: Some(secret_field("sup role")),
                status: AgentStatus::Running,
                task_id: Some("task-fixed".to_owned()),
            }),
            agents: vec![TodoChildDetailAgent {
                id: "agent-fixed".to_owned(),
                display_name: secret_field("agent name"),
                role: Some(secret_field("agent role")),
                status: AgentStatus::Idle,
                task_id: Some("task-fixed".to_owned()),
            }],
            jobs: vec![TodoChildDetailJob {
                job_id: "job-fixed".to_owned(),
                display_name: secret_field("job name"),
                role: Some(secret_field("job role")),
                status: JobStatus::Running,
                task_id: Some("task-fixed".to_owned()),
                summary: Some(secret_field("job summary")),
            }],
            activity: vec![TodoChildDetailActivity {
                at_ms: 0,
                actor: Some(secret_field("actor")),
                kind: TodoChildDetailActivityKind::Task,
                text: secret_field("activity text"),
            }],
            irc: vec![TodoChildDetailIrcMessage {
                message_id: "irc-fixed".to_owned(),
                from: secret_field("from"),
                to: secret_field("to"),
                text: secret_field("irc body"),
                at_ms: 0,
                reply_to: Some(secret_field("reply")),
            }],
            transcript: vec![TodoChildDetailTranscriptEntry {
                id: "transcript-fixed".to_owned(),
                role: secret_field("role"),
                actor: Some(secret_field("transcript actor")),
                text: secret_field("transcript text"),
                at_ms: Some(0),
                interrupted: false,
            }],
        }
    }

    fn rendered_rows(snapshot: &TodoChildDetailSnapshot, width: usize) -> String {
        project_todo_child_detail_rows(snapshot, width)
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn project_todo_child_detail_rows_redacts_secrets_across_every_section() {
        let text = rendered_rows(&snapshot_with_secrets_in_every_section(), 80);
        assert!(
            !text.contains(SECRET),
            "secret must be redacted across every section:\n{text}"
        );
        // Every section that surfaced a secret must show the redaction marker.
        assert!(
            text.matches("[REDACTED]").count() >= 15,
            "expected broad redaction across sections:\n{text}"
        );
        // Canonical authority IDs stay intact internally; the IDs the
        // existing detail presentation intentionally exposes are still
        // visible. The child session id remains the request/response key and
        // is not promoted into a new presentation field.
        assert!(
            text.contains("task-fixed"),
            "canonical task id must stay visible: {text}"
        );
        assert!(
            text.contains("receipt-fixed"),
            "canonical receipt id must stay visible: {text}"
        );
        assert!(
            text.contains("job-fixed"),
            "canonical job id must stay visible: {text}"
        );
        for heading in [
            "Supervisor",
            "Tasks",
            "Receipts",
            "Agents",
            "Jobs",
            "Activity",
            "IRC",
            "Transcript",
        ] {
            assert!(
                text.contains(heading),
                "section {heading:?} must render: {text}"
            );
        }
    }

    #[test]
    fn narrow_width_redacts_and_bounds_child_detail_rows() {
        let rows = project_todo_child_detail_rows(&snapshot_with_secrets_in_every_section(), 24);
        for row in &rows {
            assert!(
                !row.text.contains(SECRET),
                "secret must stay redacted at 24 cols: {:?}",
                row.text
            );
            assert!(
                pi_coding::markdown::display_width(&row.text) <= 24,
                "row must respect the 24-cell width bound: {:?} ({})",
                row.text,
                pi_coding::markdown::display_width(&row.text),
            );
        }
    }

    #[test]
    fn render_child_detail_redacts_error_panel_path() {
        // The error-panel fallback (no detail loaded) must redact the failure
        // message via `compact_text` before fitting it — not project a
        // credential verbatim. This is the one path that previously bypassed
        // the centralized `push_row`/`compact_text` redaction.
        let parent = "parent-session-fixed";
        let child = "child-session-fixed";
        let mut panel = TodoChildPanel::new(
            parent.to_owned(),
            vec![TodoChildManifestEntry {
                child_session_id: child.to_owned(),
                label: None,
                created_at: 0,
                status: TodoChildStatus::Idle,
                parent_entry_id: None,
                parent_revision: 0,
                kind: TodoChildKind::Todo,
            }],
        );
        assert!(panel.begin_loading(parent, child, 1));
        let secret_error = format!("spawn failed: {SECRET}");
        assert!(panel.apply_error(parent, child, 1, secret_error));
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_todo_child_panel(frame, &panel, crate::theme::DARK))
            .unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            screen.contains("Todo · Child sessions"),
            "child renderer must identify the unified Todo subpage:\n{screen}"
        );
        assert!(
            screen.contains("Tab Tasks"),
            "child renderer must advertise navigation back to Tasks:\n{screen}"
        );
        assert!(
            !screen.contains(SECRET),
            "error path must redact the secret:\n{screen}"
        );
        assert!(
            screen.contains("[REDACTED]"),
            "error path must show the redaction marker:\n{screen}"
        );
    }

    fn rendered_child_screen(panel: &TodoChildPanel, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_todo_child_panel(frame, panel, crate::theme::DARK))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    fn manifest_child(
        child_session_id: &str,
        label: Option<&str>,
        status: TodoChildStatus,
    ) -> TodoChildManifestEntry {
        TodoChildManifestEntry {
            child_session_id: child_session_id.to_owned(),
            label: label.map(str::to_owned),
            created_at: 0,
            status,
            parent_entry_id: None,
            parent_revision: 0,
            kind: TodoChildKind::Todo,
        }
    }

    #[test]
    fn render_child_detail_shows_explicit_status_alongside_runtime_unavailable_error() {
        // Reproduces the durable-resume stopped-detail failure: after a
        // `/todo stop` the child runtime is unavailable, so
        // `todo_child_detail` returns Err and the panel has no snapshot. The
        // detail section must still expose the selected child's explicit
        // `Status: stopped` (read from the manifest list, through the same
        // `lifecycle_status_label` mapping the snapshot path uses) alongside
        // the actionable runtime-unavailable error, while the compact list
        // row keeps `<label> · stopped`.
        let parent = "parent-session-fixed";
        let child = "durable-child";
        let mut panel = TodoChildPanel::new(
            parent.to_owned(),
            vec![manifest_child(
                child,
                Some("durable-child"),
                TodoChildStatus::Stopped,
            )],
        );
        assert!(panel.begin_loading(parent, child, 1));
        assert!(panel.apply_error(
            parent,
            child,
            1,
            "Todo child runtime unavailable: process not running".to_owned(),
        ));
        let screen = rendered_child_screen(&panel, 80, 16);
        // Compact list row retained.
        assert!(
            screen.contains("durable-child · stopped"),
            "compact list row must keep `<label> · stopped`:\n{screen}"
        );
        // Explicit detail Status line, not special-cased: derived from the
        // manifest status through the shared lifecycle label mapping.
        assert!(
            screen.contains("Status: stopped"),
            "detail section must expose explicit `Status: stopped` for the selected child:\n{screen}"
        );
        // Actionable runtime-unavailable error retained alongside the status.
        assert!(
            screen.contains("runtime unavailable"),
            "runtime-unavailable error must coexist with the status line:\n{screen}"
        );
        // No snapshot was loaded, so the snapshot-only `revision`/`running`
        // suffix must not leak into the manifest status line.
        assert!(
            !screen.contains("Status: stopped · revision"),
            "manifest status line must not fabricate snapshot-only revision/running fields:\n{screen}"
        );
    }

    #[test]
    fn render_child_detail_manifest_status_line_covers_every_lifecycle_state() {
        // The manifest-status detail line is the normal renderer fallback for
        // every lifecycle state, not a stopped special case: each label
        // matches the snapshot path's `lifecycle_status_label` mapping, and
        // the compact list row stays in sync.
        let parent = "parent-session-fixed";
        for (status, label) in [
            (TodoChildStatus::Created, "created"),
            (TodoChildStatus::Idle, "idle"),
            (TodoChildStatus::Running, "running"),
            (TodoChildStatus::Paused, "paused"),
            (TodoChildStatus::Stopped, "stopped"),
            (TodoChildStatus::Failed, "failed"),
        ] {
            let child_id = format!("child-{label}");
            let panel = TodoChildPanel::new(
                parent.to_owned(),
                vec![manifest_child(&child_id, Some(child_id.as_str()), status)],
            );
            let screen = rendered_child_screen(&panel, 80, 12);
            assert!(
                screen.contains(&format!("Status: {label}")),
                "detail section must show `Status: {label}` without a snapshot:\n{screen}"
            );
            assert!(
                screen.contains(&format!("{child_id} · {label}")),
                "compact list row must keep `{child_id} · {label}`:\n{screen}"
            );
        }
    }
}
