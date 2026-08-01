//! Pure state and rendering helpers for the dedicated workflow page.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use pi_coding::{TodoPhase, TodoStatus, WorkflowStatus};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use serde::{Deserialize, Serialize};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::theme::Theme;


/// A named workflow participant or durable job projection.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowActorSnapshot {
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub task: Option<String>,
}

/// One recent, already-authorized IRC projection.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowIrcSnapshot {
    pub sender: String,
    pub text: String,
}

/// Worktree information deliberately limited to safe display labels.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowWorktreeSnapshot {
    pub label: String,
    pub branch: String,
}

/// Integration state shown without diff bodies or repository paths.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowIntegrationSnapshot {
    pub summary: String,
    #[serde(default)]
    pub files_changed: usize,
    #[serde(default)]
    pub insertions: usize,
    #[serde(default)]
    pub deletions: usize,
    #[serde(default)]
    pub conflicts: Vec<String>,
}

/// Serializable, manager-independent projection consumed by the workflow page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowPanelSnapshot {
    /// Opaque routing identity. It is returned in intents and never rendered.
    pub id: String,
    /// Monotonic runtime generation used to reject stale incremental events.
    pub generation: u64,
    pub name: String,
    pub objective: String,
    pub status: WorkflowStatus,
    #[serde(default)]
    pub todo: Vec<TodoPhase>,
    #[serde(default)]
    pub supervisor: Option<WorkflowActorSnapshot>,
    #[serde(default)]
    pub subagents: Vec<WorkflowActorSnapshot>,
    #[serde(default)]
    pub recent_irc: Vec<WorkflowIrcSnapshot>,
    #[serde(default)]
    pub active_tasks: Vec<String>,
    #[serde(default)]
    pub worktree: WorkflowWorktreeSnapshot,
    #[serde(default)]
    pub integration: WorkflowIntegrationSnapshot,
}

impl From<&pi_coding::WorkflowSnapshot> for WorkflowPanelSnapshot {
    fn from(snapshot: &pi_coding::WorkflowSnapshot) -> Self {
        let integration = match &snapshot.integration {
            pi_coding::WorkflowIntegration::None => WorkflowIntegrationSnapshot::default(),
            pi_coding::WorkflowIntegration::Applied { .. } => WorkflowIntegrationSnapshot { summary: "Integrated".to_owned(), ..WorkflowIntegrationSnapshot::default() },
            pi_coding::WorkflowIntegration::Conflicted { conflicts } => WorkflowIntegrationSnapshot { summary: "Manual resolution required".to_owned(), conflicts: conflicts.clone(), ..WorkflowIntegrationSnapshot::default() },
        };
        Self {
            id: snapshot.workflow_id.to_string(),
            generation: snapshot.generation,
            name: snapshot.name.clone(),
            objective: snapshot.objective.clone(),
            status: snapshot.status,
            todo: snapshot.todo.phases.clone(),
            supervisor: snapshot.supervisor_agent_id.as_ref().map(|name| WorkflowActorSnapshot { name: name.clone(), status: snapshot.status.as_str().to_owned(), task: None }),
            subagents: Vec::new(),
            recent_irc: Vec::new(),
            active_tasks: Vec::new(),
            worktree: WorkflowWorktreeSnapshot { label: snapshot.worktree_label.clone().unwrap_or_default(), branch: snapshot.branch.clone().unwrap_or_default() },
            integration,
        }
    }
}

impl WorkflowPanelSnapshot {
    #[must_use]
    pub fn from_runtime_detail(detail: &pi_coding::WorkflowRuntimeDetail, snapshot: &pi_coding::WorkflowSnapshot) -> Self {
        let mut projected = Self::from(snapshot);
        projected.name = detail.name.clone();
        projected.objective = detail.objective.clone();
        projected.status = detail.status;
        projected.todo = detail.todo.phases.clone();
        projected.supervisor = detail.supervisor.as_ref().map(|supervisor| WorkflowActorSnapshot {
            name: supervisor.display_name.clone(),
            status: agent_status_label(supervisor.status).to_owned(),
            task: None,
        });
        projected.subagents = detail.subagents.iter().map(|subagent| WorkflowActorSnapshot {
            name: subagent.display_name.clone(),
            status: agent_status_label(subagent.status).to_owned(),
            task: subagent.task_summary.clone(),
        }).collect();
        projected.recent_irc = detail.irc.iter().map(|message| WorkflowIrcSnapshot {
            sender: message.from.clone(),
            text: message.body.clone(),
        }).collect();
        projected.active_tasks = detail.jobs.iter().filter(|job| !job.status.is_settled()).filter_map(|job| job.task_summary.clone()).collect();
        projected
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkflowIntentKind {
    Pause,
    Resume,
    Cancel,
    Integrate,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkflowPanelResult {
    Handled,
    Close,
    Intent { workflow_id: String, kind: WorkflowIntentKind },
    Unknown,
}

/// Page shown by the transient workflow overlay.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorkflowPanelPage {
    #[default]
    List,
    Detail,
}

/// Selection and filter state for the dedicated workflow page.
#[derive(Clone, Debug, Default)]
pub struct WorkflowPanel {
    workflows: Vec<WorkflowPanelSnapshot>,
    selected: usize,
    filter: String,
    filtering: bool,
    page: WorkflowPanelPage,
}

impl WorkflowPanel {
    #[must_use]
    pub fn new(workflows: Vec<WorkflowPanelSnapshot>) -> Self {
        Self { workflows, selected: 0, filter: String::new(), filtering: false, page: WorkflowPanelPage::List }
    }

    pub fn replace(&mut self, workflows: Vec<WorkflowPanelSnapshot>) {
        let selected_id = self.selected_workflow().map(|workflow| workflow.id.clone());
        self.workflows = workflows;
        let retained = selected_id.and_then(|id| {
            self.visible_indices().into_iter().position(|index| self.workflows[index].id == id)
        });
        if self.page == WorkflowPanelPage::Detail && retained.is_none() {
            self.page = WorkflowPanelPage::List;
        }
        self.selected = retained.unwrap_or(0);
        self.clamp_selection();
    }

    #[must_use]
    pub fn selected_workflow(&self) -> Option<&WorkflowPanelSnapshot> {
        let index = *self.visible_indices().get(self.selected)?;
        self.workflows.get(index)
    }

    #[must_use]
    pub fn filter(&self) -> &str { &self.filter }

    #[must_use]
    pub const fn filtering(&self) -> bool { self.filtering }

    #[must_use]
    pub const fn page(&self) -> WorkflowPanelPage { self.page }

    pub fn handle_key(&mut self, key: KeyEvent) -> WorkflowPanelResult {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return WorkflowPanelResult::Handled;
        }
        if self.page == WorkflowPanelPage::Detail {
            return match key.code {
                KeyCode::Esc | KeyCode::Backspace => { self.page = WorkflowPanelPage::List; WorkflowPanelResult::Handled }
                KeyCode::Char('p') => self.intent(WorkflowIntentKind::Pause),
                KeyCode::Char('r') => self.intent(WorkflowIntentKind::Resume),
                KeyCode::Char('c') => self.intent(WorkflowIntentKind::Cancel),
                KeyCode::Char('i') => self.intent(WorkflowIntentKind::Integrate),
                _ => WorkflowPanelResult::Unknown,
            };
        }
        if self.filtering {
            return match key.code {
                KeyCode::Esc | KeyCode::Enter => { self.filtering = false; WorkflowPanelResult::Handled }
                KeyCode::Backspace => { self.filter.pop(); self.selected = 0; WorkflowPanelResult::Handled }
                KeyCode::Char(character) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                    self.filter.push(character); self.selected = 0; WorkflowPanelResult::Handled
                }
                _ => WorkflowPanelResult::Handled,
            };
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => WorkflowPanelResult::Close,
            KeyCode::Enter if self.selected_workflow().is_some() => { self.page = WorkflowPanelPage::Detail; WorkflowPanelResult::Handled }
            KeyCode::Char('/') => { self.filtering = true; WorkflowPanelResult::Handled }
            KeyCode::Up | KeyCode::Char('k') => { self.move_selection(-1); WorkflowPanelResult::Handled }
            KeyCode::Down | KeyCode::Char('j') => { self.move_selection(1); WorkflowPanelResult::Handled }
            KeyCode::Home => { self.selected = 0; WorkflowPanelResult::Handled }
            KeyCode::End => { self.selected = self.visible_indices().len().saturating_sub(1); WorkflowPanelResult::Handled }
            KeyCode::Char('p') => self.intent(WorkflowIntentKind::Pause),
            KeyCode::Char('r') => self.intent(WorkflowIntentKind::Resume),
            KeyCode::Char('c') => self.intent(WorkflowIntentKind::Cancel),
            KeyCode::Char('i') => self.intent(WorkflowIntentKind::Integrate),
            _ => WorkflowPanelResult::Unknown,
        }
    }

    fn visible_indices(&self) -> Vec<usize> {
        let query = self.filter.to_lowercase();
        self.workflows.iter().enumerate().filter_map(|(index, workflow)| {
            (query.is_empty() || workflow.name.to_lowercase().contains(&query) || workflow.objective.to_lowercase().contains(&query)).then_some(index)
        }).collect()
    }

    fn move_selection(&mut self, delta: isize) {
        let count = self.visible_indices().len();
        if count > 0 { self.selected = (self.selected as isize + delta).rem_euclid(count as isize) as usize; }
    }

    fn clamp_selection(&mut self) {
        self.selected = self.selected.min(self.visible_indices().len().saturating_sub(1));
    }

    fn intent(&self, kind: WorkflowIntentKind) -> WorkflowPanelResult {
        self.selected_workflow().map_or(WorkflowPanelResult::Handled, |workflow| WorkflowPanelResult::Intent {
            workflow_id: workflow.id.clone(), kind,
        })
    }
}

/// The only workflow projection rendered in normal conversation view.
#[must_use]
pub fn compact_workflow_status(workflows: &[WorkflowPanelSnapshot]) -> String {
    let active = workflows.iter().filter(|workflow| workflow.status.is_active()).count();
    crate::workflow_commands::format_workflows_header(active, workflows.len())
}

pub fn render_workflow_panel(frame: &mut ratatui::Frame<'_>, panel: &WorkflowPanel, theme: Theme) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    let title = match panel.page { WorkflowPanelPage::List => " Workflows ", WorkflowPanelPage::Detail => " Workflow detail " };
    let block = Block::default().title(title).borders(Borders::ALL).border_style(Style::default().fg(theme.border_accent));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    match panel.page {
        WorkflowPanelPage::List => render_workflow_list(frame, panel, inner, theme),
        WorkflowPanelPage::Detail => render_workflow_detail(frame, panel.selected_workflow(), inner, theme),
    }
}


fn render_workflow_list(frame: &mut ratatui::Frame<'_>, panel: &WorkflowPanel, area: Rect, theme: Theme) {
    let visible = panel.visible_indices();
    let footer_height = 2.min(area.height);
    let rows_height = area.height.saturating_sub(footer_height);
    let rows = Rect { height: rows_height, ..area };
    let footer = Rect { y: area.y.saturating_add(rows_height), height: footer_height, ..area };
    let viewport = usize::from(rows.height).max(1);
    let start = panel.selected.saturating_add(1).saturating_sub(viewport).min(visible.len().saturating_sub(viewport));
    let lines = if visible.is_empty() {
        vec![Line::from(Span::styled(" No matching workflows", Style::default().fg(theme.muted)))]
    } else {
        visible.iter().enumerate().skip(start).take(viewport).map(|(visible_index, index)| {
            let workflow = &panel.workflows[*index];
            let selected = visible_index == panel.selected;
            let style = if selected { Style::default().fg(theme.text).bg(theme.selected_bg).add_modifier(Modifier::BOLD) } else { Style::default().fg(theme.text) };
            let (ready, total) = todo_counts(&workflow.todo);
            let available = usize::from(rows.width).saturating_sub(22);
            Line::from(vec![
                Span::styled(format!(" {} ", if selected { '›' } else { ' ' }), style),
                Span::styled(format!("{} ", status_marker(workflow.status)), style.patch(status_style(workflow.status, theme))),
                Span::styled(format!("{:<width$}", truncate(&workflow.name, available), width = available), style),
                Span::styled(format!(" {:<11} {ready}/{total}", workflow.status.as_str()), style.patch(Style::default().fg(theme.dim))),
            ])
        }).collect()
    };
    frame.render_widget(Paragraph::new(lines), rows);
    let summary = if panel.filtering { format!("Filter: {}_", panel.filter) } else if panel.filter.is_empty() { format!("{}/{} workflows · ↑/↓ select · Enter details · / filter · p/r/c/i lifecycle · Esc close", visible.len(), panel.workflows.len()) } else { format!("Filter: {} · {}/{} · Enter details · Esc close", panel.filter, visible.len(), panel.workflows.len()) };
    frame.render_widget(Paragraph::new(summary).style(Style::default().fg(if panel.filtering { theme.accent } else { theme.dim })).wrap(Wrap { trim: true }), footer);
}

fn render_workflow_detail(frame: &mut ratatui::Frame<'_>, workflow: Option<&WorkflowPanelSnapshot>, area: Rect, theme: Theme) {
    let Some(workflow) = workflow else {
        frame.render_widget(Paragraph::new(" No workflow selected").style(Style::default().fg(theme.muted)), area);
        return;
    };
    let mut lines = vec![
        Line::from(Span::styled(sanitize(&workflow.name), Style::default().fg(theme.accent).add_modifier(Modifier::BOLD))),
        field_line("Objective", &workflow.objective, theme),
        Line::from(vec![Span::styled("Status      ", Style::default().fg(theme.dim)), Span::styled(workflow.status.as_str(), status_style(workflow.status, theme).add_modifier(Modifier::BOLD))]),
        Line::default(),
    ];
    push_section(&mut lines, "Todos", theme);
    push_todo_lines(&mut lines, &workflow.todo, theme);
    push_section(&mut lines, "Supervisor", theme);
    lines.push(actor_line(workflow.supervisor.as_ref(), "not started", theme));
    push_section(&mut lines, "Subagents", theme);
    if workflow.subagents.is_empty() { lines.push(dim_line("None", theme)); } else { lines.extend(workflow.subagents.iter().take(5).map(|actor| actor_line(Some(actor), "", theme))); }
    push_section(&mut lines, "Active tasks", theme);
    let active_tasks = workflow.active_tasks.iter().map(String::as_str).chain(workflow.subagents.iter().chain(workflow.supervisor.iter()).filter_map(|actor| actor.task.as_deref())).collect::<Vec<_>>();
    if active_tasks.is_empty() { lines.push(dim_line("None", theme)); } else { lines.extend(active_tasks.into_iter().take(6).map(|task| Line::from(Span::styled(sanitize(task), Style::default().fg(theme.text))))); }
    push_section(&mut lines, "Recent IRC", theme);
    if workflow.recent_irc.is_empty() { lines.push(dim_line("No recent messages", theme)); } else {
        lines.extend(workflow.recent_irc.iter().rev().take(4).rev().map(|message| Line::from(vec![Span::styled(format!("{}: ", sanitize(&message.sender)), Style::default().fg(theme.accent)), Span::styled(sanitize(&message.text), Style::default().fg(theme.text))])));
    }
    push_section(&mut lines, "Worktree", theme);
    let label = safe_path_label(&workflow.worktree.label);
    lines.push(Line::from(vec![Span::styled(label, Style::default().fg(theme.text)), Span::styled(format!(" · {}", safe_branch_label(&workflow.worktree.branch)), Style::default().fg(theme.dim))]));
    push_section(&mut lines, "Integration", theme);
    let integration_style = if workflow.status == WorkflowStatus::Conflicted { Style::default().fg(theme.error).add_modifier(Modifier::BOLD) } else { Style::default().fg(theme.text) };
    if workflow.status == WorkflowStatus::Conflicted { lines.push(Line::from(Span::styled("CONFLICTED", integration_style))); }
    lines.push(Line::from(Span::styled(sanitize(&workflow.integration.summary), integration_style)));
    lines.push(Line::from(Span::styled(format!("{} files · +{} −{}", workflow.integration.files_changed, workflow.integration.insertions, workflow.integration.deletions), Style::default().fg(theme.dim))));
    for conflict in workflow.integration.conflicts.iter().take(3) { lines.push(Line::from(Span::styled(format!("! {}", safe_path_label(conflict)), Style::default().fg(theme.error)))); }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled("p pause · r resume · c cancel · i integrate · Esc/Backspace workflows", Style::default().fg(theme.dim))));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn push_todo_lines(lines: &mut Vec<Line<'static>>, phases: &[TodoPhase], theme: Theme) {
    let tasks = phases.iter().flat_map(|phase| &phase.tasks).collect::<Vec<_>>();
    if tasks.is_empty() { lines.push(dim_line("No tasks yet", theme)); return; }
    let names = tasks.iter().map(|task| (task.id.as_str(), sanitize(&task.content))).collect::<HashMap<_, _>>();
    let ready = tasks.iter().filter(|task| task.ready).count();
    lines.push(dim_line(&format!("{ready}/{} ready", tasks.len()), theme));
    for phase in phases {
        lines.push(Line::from(Span::styled(sanitize(&phase.name), Style::default().fg(theme.muted).add_modifier(Modifier::BOLD))));
        for task in phase.tasks.iter().take(8) {
            let dependencies = task.depends_on.iter().filter_map(|id| names.get(id.as_str())).cloned().collect::<Vec<_>>();
            let dependency = if dependencies.is_empty() { String::new() } else { format!(" ← {}", dependencies.join(", ")) };
            let ready_marker = if task.ready { " ready" } else { "" };
            lines.push(Line::from(vec![
                Span::styled(format!("{} ", todo_marker(task.status)), todo_style(task.status, theme)),
                Span::styled(sanitize(&task.content), Style::default().fg(theme.text)),
                Span::styled(format!("{dependency}{ready_marker}"), Style::default().fg(if task.ready { theme.success } else { theme.dim })),
            ]));
        }
    }
}

fn push_section(lines: &mut Vec<Line<'static>>, label: &str, theme: Theme) { lines.push(Line::from(Span::styled(label.to_owned(), Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)))); }
fn field_line(label: &str, value: &str, theme: Theme) -> Line<'static> { Line::from(vec![Span::styled(format!("{label:<11}"), Style::default().fg(theme.dim)), Span::styled(sanitize(value), Style::default().fg(theme.text))]) }
fn dim_line(value: &str, theme: Theme) -> Line<'static> { Line::from(Span::styled(value.to_owned(), Style::default().fg(theme.dim))) }
fn actor_line(actor: Option<&WorkflowActorSnapshot>, empty: &str, theme: Theme) -> Line<'static> { actor.map_or_else(|| dim_line(empty, theme), |actor| { let task = actor.task.as_deref().map_or(String::new(), |task| format!(" · {}", sanitize(task))); Line::from(vec![Span::styled(sanitize(&actor.name), Style::default().fg(theme.text)), Span::styled(format!(" · {}{task}", sanitize(&actor.status)), Style::default().fg(theme.dim))]) }) }
fn todo_counts(phases: &[TodoPhase]) -> (usize, usize) { let tasks = phases.iter().flat_map(|phase| &phase.tasks).collect::<Vec<_>>(); (tasks.iter().filter(|task| task.ready).count(), tasks.len()) }
fn todo_marker(status: TodoStatus) -> &'static str { match status { TodoStatus::Pending => "○", TodoStatus::InProgress => "●", TodoStatus::Completed => "✓", TodoStatus::Abandoned => "×" } }
fn todo_style(status: TodoStatus, theme: Theme) -> Style { Style::default().fg(match status { TodoStatus::Pending => theme.muted, TodoStatus::InProgress => theme.accent, TodoStatus::Completed => theme.success, TodoStatus::Abandoned => theme.dim }) }
fn status_marker(status: WorkflowStatus) -> &'static str { match status { WorkflowStatus::Queued => "○", WorkflowStatus::Planning => "◐", WorkflowStatus::Running => "●", WorkflowStatus::Paused => "Ⅱ", WorkflowStatus::Integrating => "⇄", WorkflowStatus::Completed => "✓", WorkflowStatus::Failed => "!", WorkflowStatus::Cancelled => "×", WorkflowStatus::Conflicted => "!" } }
fn agent_status_label(status: pi_coding::AgentStatus) -> &'static str { match status { pi_coding::AgentStatus::Queued => "queued", pi_coding::AgentStatus::Running => "running", pi_coding::AgentStatus::Idle => "idle", pi_coding::AgentStatus::Parked => "parked", pi_coding::AgentStatus::Aborted => "aborted" } }
fn status_style(status: WorkflowStatus, theme: Theme) -> Style { Style::default().fg(match status { WorkflowStatus::Queued | WorkflowStatus::Paused | WorkflowStatus::Cancelled => theme.muted, WorkflowStatus::Planning | WorkflowStatus::Integrating => theme.warning, WorkflowStatus::Running => theme.accent, WorkflowStatus::Completed => theme.success, WorkflowStatus::Failed | WorkflowStatus::Conflicted => theme.error }) }

fn sanitize(value: &str) -> String { value.chars().map(|character| if character.is_control() { ' ' } else { character }).collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ") }
fn safe_branch_label(value: &str) -> String {
    let clean = sanitize(value);
    if clean.is_empty() {
        "branch unavailable".to_owned()
    } else if clean.starts_with('/') || clean.starts_with('\\') || clean.starts_with('<') {
        safe_path_label(&clean)
    } else {
        truncate(&clean, 48)
    }
}
fn safe_path_label(value: &str) -> String {
    let clean = sanitize(value);
    let leaf = clean.rsplit(['/', '\\']).find(|part| !part.is_empty()).unwrap_or("worktree");
    truncate(leaf, 48)
}
fn truncate(value: &str, width: usize) -> String {
    if width == 0 { return String::new(); }
    let clean = sanitize(value);
    if UnicodeWidthStr::width(clean.as_str()) <= width { return clean; }
    let target = width.saturating_sub(1);
    let mut used: usize = 0;
    let mut output = String::new();
    for character in clean.chars() {
        let character_width = character.width().unwrap_or(0);
        if used.saturating_add(character_width) > target { break; }
        output.push(character); used = used.saturating_add(character_width);
    }
    output.push('…'); output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use pi_coding::{TodoBlockedReason, TodoItem};
    use ratatui::{Terminal, backend::TestBackend};

    fn task(id: &str, content: &str, status: TodoStatus, ready: bool, depends_on: &[&str]) -> TodoItem {
        TodoItem { id: id.to_owned(), content: content.to_owned(), status, depends_on: depends_on.iter().map(|value| (*value).to_owned()).collect(), ready, blocked_by: if ready { Vec::new() } else { depends_on.iter().map(|dependency| TodoBlockedReason { task_id: (*dependency).to_owned(), content: "dependency".to_owned(), status: TodoStatus::InProgress }).collect() } }
    }

    fn workflow(id: &str, name: &str, status: WorkflowStatus) -> WorkflowPanelSnapshot {
        WorkflowPanelSnapshot {
            id: id.to_owned(), generation: 1, name: name.to_owned(), objective: "Ship isolated workflow orchestration safely".to_owned(), status,
            todo: vec![TodoPhase { name: "Build".to_owned(), tasks: vec![task("design-id", "Design protocol", TodoStatus::Completed, false, &[]), task("render-id", "Render page", TodoStatus::InProgress, true, &["design-id"])] }],
            supervisor: Some(WorkflowActorSnapshot { name: "Supervisor".to_owned(), status: "running".to_owned(), task: Some("Coordinate workers".to_owned()) }),
            subagents: vec![WorkflowActorSnapshot { name: "PanelWorker".to_owned(), status: "running".to_owned(), task: Some("Render page".to_owned()) }],
            recent_irc: vec![WorkflowIrcSnapshot { sender: "Supervisor".to_owned(), text: "Integrate after focused tests".to_owned() }],
            active_tasks: vec!["Run focused tests".to_owned()],
            worktree: WorkflowWorktreeSnapshot { label: "<workspace>/worktrees/feature-panel".to_owned(), branch: "rpi/workflow/feature-panel".to_owned() },
            integration: WorkflowIntegrationSnapshot { summary: "Ready after review".to_owned(), files_changed: 3, insertions: 80, deletions: 4, conflicts: Vec::new() },
        }
    }

    fn render(panel: &WorkflowPanel, width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height); let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render_workflow_panel(frame, panel, crate::theme::DARK)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height).map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect::<String>().trim_end().to_owned()).collect()
    }

    fn key(code: KeyCode) -> KeyEvent { KeyEvent::new(code, KeyModifiers::NONE) }

    #[test]
    fn list_stays_compact_until_enter_opens_detail() {
        let mut panel = WorkflowPanel::new(vec![workflow("opaque-one", "Panel foundation", WorkflowStatus::Running), workflow("opaque-two", "RPC transport", WorkflowStatus::Queued)]);
        let list = render(&panel, 110, 36).join("\n");
        for needle in ["Panel foundation", "RPC transport", "running", "1/2", "Enter details"] { assert!(list.contains(needle), "missing {needle:?}\n{list}"); }
        for forbidden in ["Ship isolated workflow orchestration safely", "Design protocol", "Render page", "Supervisor", "Subagents", "Recent IRC"] { assert!(!list.contains(forbidden), "list leaked {forbidden:?}\n{list}"); }

        assert_eq!(panel.handle_key(key(KeyCode::Enter)), WorkflowPanelResult::Handled);
        assert_eq!(panel.page(), WorkflowPanelPage::Detail);
        let detail = render(&panel, 110, 36).join("\n");
        for needle in ["Panel foundation", "Objective", "Ship isolated workflow orchestration safely", "Todos", "1/2 ready", "Render page ← Design protocol ready", "Supervisor", "Subagents", "Active tasks", "Coordinate workers", "Render page", "Recent IRC", "Worktree", "feature-panel · rpi/workflow/feature-panel", "Integration", "3 files · +80 −4", "Esc/Backspace workflows"] { assert!(detail.contains(needle), "missing {needle:?}\n{detail}"); }
    }

    #[test]
    fn escape_and_backspace_follow_detail_then_list_hierarchy() {
        let mut panel = WorkflowPanel::new(vec![workflow("one", "Alpha", WorkflowStatus::Running)]);
        assert_eq!(panel.handle_key(key(KeyCode::Enter)), WorkflowPanelResult::Handled);
        assert_eq!(panel.handle_key(key(KeyCode::Esc)), WorkflowPanelResult::Handled);
        assert_eq!(panel.page(), WorkflowPanelPage::List);
        assert_eq!(panel.handle_key(key(KeyCode::Enter)), WorkflowPanelResult::Handled);
        assert_eq!(panel.handle_key(key(KeyCode::Backspace)), WorkflowPanelResult::Handled);
        assert_eq!(panel.page(), WorkflowPanelPage::List);
        assert_eq!(panel.handle_key(key(KeyCode::Esc)), WorkflowPanelResult::Close);
    }

    #[test]
    fn detail_preserves_selection_and_renders_live_replacement() {
        let mut panel = WorkflowPanel::new(vec![workflow("one", "Alpha", WorkflowStatus::Running), workflow("two", "Beta", WorkflowStatus::Paused)]);
        assert_eq!(panel.handle_key(key(KeyCode::Down)), WorkflowPanelResult::Handled);
        assert_eq!(panel.handle_key(key(KeyCode::Enter)), WorkflowPanelResult::Handled);
        let alpha = workflow("one", "Alpha", WorkflowStatus::Completed);
        let mut beta = workflow("two", "Beta", WorkflowStatus::Running);
        beta.todo[0].tasks[1].content = "Live replacement task".to_owned();
        beta.subagents[0].task = Some("Live worker task".to_owned());
        beta.recent_irc.push(WorkflowIrcSnapshot { sender: "PanelWorker".to_owned(), text: "Live snapshot arrived".to_owned() });
        panel.replace(vec![alpha.clone(), beta]);
        assert_eq!(panel.page(), WorkflowPanelPage::Detail);
        assert_eq!(panel.selected_workflow().unwrap().name, "Beta");
        let text = render(&panel, 110, 38).join("\n");
        for needle in ["Live replacement task", "Live worker task", "Live snapshot arrived"] { assert!(text.contains(needle), "missing live value {needle:?}\n{text}"); }
    }

    #[test]
    fn removing_selected_detail_returns_safely_to_list() {
        let mut panel = WorkflowPanel::new(vec![workflow("one", "Alpha", WorkflowStatus::Running), workflow("two", "Beta", WorkflowStatus::Paused)]);
        panel.handle_key(key(KeyCode::Down));
        panel.handle_key(key(KeyCode::Enter));
        panel.replace(vec![workflow("one", "Alpha", WorkflowStatus::Running)]);
        assert_eq!(panel.page(), WorkflowPanelPage::List);
        assert_eq!(panel.selected_workflow().unwrap().name, "Alpha");
        let text = render(&panel, 80, 20).join("\n");
        assert!(text.contains("Alpha"));
        assert!(!text.contains("Beta"));
        assert!(!text.contains("Design protocol"));
    }

    #[test]
    fn detail_redacts_opaque_ids_and_repository_paths() {
        let mut item = workflow("550e8400-e29b-41d4-a716-446655440000", "Integration", WorkflowStatus::Conflicted);
        item.worktree.label = "<workspace>/.git/worktrees/secret-worktree".to_owned();
        item.integration.summary = "Manual resolution required".to_owned();
        item.integration.conflicts = vec!["<workspace>/src/application.rs".to_owned()];
        item.worktree.branch = "<workspace>/secret-branch".to_owned();
        let mut panel = WorkflowPanel::new(vec![item]);
        panel.handle_key(key(KeyCode::Enter));
        let text = render(&panel, 90, 34).join("\n");
        for needle in ["CONFLICTED", "Manual resolution required", "secret-worktree", "! application.rs"] { assert!(text.contains(needle), "missing {needle:?}\n{text}"); }
        assert!(text.contains("secret-branch"));
        for forbidden in ["550e8400", "<workspace>", ".git/worktrees"] { assert!(!text.contains(forbidden), "leaked {forbidden:?}\n{text}"); }
    }

    #[test]
    fn runtime_detail_conversion_projects_live_actors_jobs_and_irc() {
        let fixture = workflow("fixture", "Fixture", WorkflowStatus::Running);
        let snapshot = pi_coding::WorkflowSnapshot {
            workflow_id: pi_coding::WorkflowId::new("opaque"), name: "Runtime workflow".to_owned(), objective: "Original objective".to_owned(), status: WorkflowStatus::Running,
            created_at_ms: 1, updated_at_ms: 2, generation: 7,
            todo: pi_coding::TodoState { phases: fixture.todo, storage: pi_coding::TodoStorage::Memory },
            worktree_label: Some("safe-worktree".to_owned()), branch: Some("rpi/workflow/runtime".to_owned()), supervisor_agent_id: None, supervisor_job_id: None, failure: None, integration: pi_coding::WorkflowIntegration::None,
        };
        let detail = pi_coding::WorkflowRuntimeDetail {
            workflow_id: snapshot.workflow_id.clone(), generation: 7, name: "Live runtime workflow".to_owned(), objective: "Live objective".to_owned(), status: WorkflowStatus::Running, todo: snapshot.todo.clone(),
            supervisor: Some(pi_coding::WorkflowSupervisorDetail { display_name: "Supervisor".to_owned(), status: pi_coding::AgentStatus::Running }),
            subagents: vec![pi_coding::WorkflowSubagentDetail { display_name: "Worker".to_owned(), status: pi_coding::AgentStatus::Idle, task_summary: Some("Review changes".to_owned()) }],
            jobs: vec![pi_coding::WorkflowRuntimeJobDetail { display_name: "Worker".to_owned(), status: pi_coding::JobStatus::Running, task_summary: Some("Run integration".to_owned()), todo_task_id: Some("render-id".to_owned()), created_at_ms: 3, started_at_ms: Some(4), finished_at_ms: None }],
            irc: vec![pi_coding::WorkflowIrcMessage { from: "Worker".to_owned(), to: "Supervisor".to_owned(), body: "Live update".to_owned(), timestamp_ms: 5 }],
        };
        let projected = WorkflowPanelSnapshot::from_runtime_detail(&detail, &snapshot);
        assert_eq!(projected.name, "Live runtime workflow");
        assert_eq!(projected.supervisor.as_ref().unwrap().status, "running");
        assert_eq!(projected.subagents[0].task.as_deref(), Some("Review changes"));
        assert_eq!(projected.active_tasks, vec!["Run integration"]);
        assert_eq!(projected.recent_irc[0].text, "Live update");
    }

    #[test]
    fn navigation_filter_and_generation_gated_action_intents_are_pure() {
        let mut panel = WorkflowPanel::new(vec![workflow("one", "Alpha", WorkflowStatus::Running), workflow("two", "Beta", WorkflowStatus::Paused)]);
        assert_eq!(panel.handle_key(key(KeyCode::Down)), WorkflowPanelResult::Handled); assert_eq!(panel.selected_workflow().unwrap().name, "Beta");
        for (key_code, kind) in [('r', WorkflowIntentKind::Resume), ('p', WorkflowIntentKind::Pause), ('c', WorkflowIntentKind::Cancel), ('i', WorkflowIntentKind::Integrate)] {
            assert_eq!(panel.handle_key(key(KeyCode::Char(key_code))), WorkflowPanelResult::Intent { workflow_id: "two".to_owned(), kind });
        }
        assert_eq!(panel.handle_key(key(KeyCode::Char('/'))), WorkflowPanelResult::Handled); assert_eq!(panel.handle_key(key(KeyCode::Char('a'))), WorkflowPanelResult::Handled); assert_eq!(panel.selected_workflow().unwrap().name, "Alpha");
        assert_eq!(panel.handle_key(key(KeyCode::Esc)), WorkflowPanelResult::Handled); assert_eq!(panel.handle_key(key(KeyCode::Esc)), WorkflowPanelResult::Close);
    }

    #[test]
    fn compact_status_line_has_only_counts() {
        let workflows = vec![workflow("raw-secret-one", "Secret objective one", WorkflowStatus::Running), workflow("raw-secret-two", "Secret objective two", WorkflowStatus::Completed), workflow("raw-secret-three", "Secret objective three", WorkflowStatus::Planning)];
        assert_eq!(compact_workflow_status(&workflows), "Workflows · 2 active · 3 total");
    }
}
