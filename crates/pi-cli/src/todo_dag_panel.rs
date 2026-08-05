//! Dedicated two-level Todo DAG overview and detail panel.

use std::cell::Cell;
use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use pi_coding::{JobStatus, TodoDagExecutionStatus, TodoPhase, TodoStatus, WorkflowRuntimeJobDetail, WorkflowStatus};
use unicode_width::UnicodeWidthChar;
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use crate::job_card_adapter::JobCardRows;
use crate::theme::Theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TodoDagPanelPage {
    Overview,
    Detail,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TodoDagCounts {
    pub completed: usize,
    pub open: usize,
    pub active: usize,
    pub blocked: usize,
}

impl TodoDagCounts {
    #[must_use]
    pub fn from_phases(phases: &[TodoPhase]) -> Self {
        let mut counts = Self::default();
        for task in phases.iter().flat_map(|phase| &phase.tasks) {
            match task.status {
                TodoStatus::Completed => counts.completed += 1,
                TodoStatus::InProgress => {
                    counts.open += 1;
                    counts.active += 1;
                }
                TodoStatus::Pending => {
                    counts.open += 1;
                    if !task.blocked_by.is_empty() {
                        counts.blocked += 1;
                    }
                }
                TodoStatus::Abandoned => {}
            }
        }
        counts
    }
}

#[derive(Clone, Debug)]
pub enum TodoDagExecutionLabel {
    Main(TodoDagExecutionStatus),
    Workflow(WorkflowStatus),
}

impl TodoDagExecutionLabel {
    fn label(&self) -> &'static str {
        match self {
            Self::Main(TodoDagExecutionStatus::Dormant) => "dormant",
            Self::Main(TodoDagExecutionStatus::Active) => "active",
            Self::Main(TodoDagExecutionStatus::Settled) => "settled",
            Self::Main(TodoDagExecutionStatus::Blocked) => "blocked",
            Self::Workflow(status) => status.as_str(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct TodoDagSnapshot {
    pub id: String,
    pub label: String,
    pub phases: Vec<TodoPhase>,
    pub execution: TodoDagExecutionLabel,
    pub jobs: Vec<JobCardRows>,
    pub workflow_jobs: Vec<WorkflowRuntimeJobDetail>,
}

impl TodoDagSnapshot {
    #[must_use]
    pub fn main(phases: Vec<TodoPhase>, status: TodoDagExecutionStatus, jobs: Vec<JobCardRows>) -> Self {
        Self {
            id: "main".to_owned(),
            label: "Main session".to_owned(),
            phases,
            execution: TodoDagExecutionLabel::Main(status),
            jobs,
            workflow_jobs: Vec::new(),
        }
    }

    #[must_use]
    pub fn workflow(
        id: String,
        label: String,
        phases: Vec<TodoPhase>,
        status: WorkflowStatus,
        jobs: Vec<WorkflowRuntimeJobDetail>,
    ) -> Self {
        Self {
            id,
            label,
            phases,
            execution: TodoDagExecutionLabel::Workflow(status),
            jobs: Vec::new(),
            workflow_jobs: jobs,
        }
    }

    #[must_use]
    pub fn counts(&self) -> TodoDagCounts {
        TodoDagCounts::from_phases(&self.phases)
    }
}

#[derive(Clone, Debug)]
pub struct TodoDagPanel {
    dags: Vec<TodoDagSnapshot>,
    selected: usize,
    page: TodoDagPanelPage,
    detail_scroll: Cell<usize>,
    detail_width: Cell<usize>,
    detail_viewport_height: Cell<usize>,
    detail_display_rows: Cell<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TodoDagPanelResult {
    Handled,
    Close,
    Unknown,
}

impl TodoDagPanel {
    #[must_use]
    pub fn new(main: TodoDagSnapshot, mut workflows: Vec<TodoDagSnapshot>) -> Self {
        workflows.sort_by(|left, right| left.label.cmp(&right.label).then_with(|| left.id.cmp(&right.id)));
        let mut dags = Vec::with_capacity(workflows.len() + 1);
        dags.push(main);
        dags.extend(workflows);
        Self { dags, selected: 0, page: TodoDagPanelPage::Overview, detail_scroll: Cell::new(0), detail_width: Cell::new(1), detail_viewport_height: Cell::new(0), detail_display_rows: Cell::new(0) }
    }

    pub fn replace(&mut self, main: TodoDagSnapshot, mut workflows: Vec<TodoDagSnapshot>) {
        let selected_id = self.selected_dag().map(|dag| dag.id.clone());
        workflows.sort_by(|left, right| left.label.cmp(&right.label).then_with(|| left.id.cmp(&right.id)));
        self.dags.clear();
        self.dags.push(main);
        self.dags.extend(workflows);
        self.selected = selected_id
            .and_then(|id| self.dags.iter().position(|dag| dag.id == id))
            .unwrap_or(0)
            .min(self.dags.len().saturating_sub(1));
        self.clamp_detail_scroll();
    }

    pub fn update_main(&mut self, phases: Vec<TodoPhase>, status: TodoDagExecutionStatus, jobs: Vec<JobCardRows>) {
        if let Some(main) = self.dags.first_mut() {
            main.phases = phases;
            main.execution = TodoDagExecutionLabel::Main(status);
            main.jobs = jobs;
        }
        self.clamp_detail_scroll();
    }

    pub fn update_main_phases(&mut self, phases: Vec<TodoPhase>) {
        if let Some(main) = self.dags.first_mut() { main.phases = phases; }
        self.clamp_detail_scroll();
    }

    pub fn update_main_jobs(&mut self, jobs: Vec<JobCardRows>) {
        if let Some(main) = self.dags.first_mut() { main.jobs = jobs; }
        self.clamp_detail_scroll();
    }

    pub fn update_workflows(&mut self, workflows: Vec<TodoDagSnapshot>) {
        let main = self.dags.first().cloned().unwrap_or_else(|| TodoDagSnapshot::main(Vec::new(), TodoDagExecutionStatus::Dormant, Vec::new()));
        self.replace(main, workflows);
    }

    #[must_use]
    pub fn page(&self) -> TodoDagPanelPage { self.page }


    #[must_use]
    pub fn selected_dag(&self) -> Option<&TodoDagSnapshot> { self.dags.get(self.selected) }

    #[must_use]
    pub fn detail_scroll(&self) -> usize { self.detail_scroll.get() }

    fn detail_scroll_max(&self) -> usize {
        self.detail_display_rows.get().saturating_sub(self.detail_viewport_height.get())
    }

    fn refresh_detail_geometry(&self) {
        let rows = self.selected_dag().map_or(0, |dag| detail_display_row_count(dag, self.detail_width.get()));
        self.detail_display_rows.set(rows);
        self.detail_scroll.set(self.detail_scroll.get().min(self.detail_scroll_max()));
    }

    fn clamp_detail_scroll(&self) { self.refresh_detail_geometry(); }

    pub fn handle_key(&mut self, key: KeyEvent) -> TodoDagPanelResult {
        if key.kind != KeyEventKind::Press {
            return TodoDagPanelResult::Handled;
        }
        match key.code {
            KeyCode::Char('q') if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => TodoDagPanelResult::Close,
            KeyCode::Esc => {
                if self.page == TodoDagPanelPage::Detail {
                    self.page = TodoDagPanelPage::Overview;
                    TodoDagPanelResult::Handled
                } else {
                    TodoDagPanelResult::Close
                }
            }
            KeyCode::Up => {
                if self.page == TodoDagPanelPage::Overview {
                    if !self.dags.is_empty() {
                        self.selected = (self.selected + self.dags.len() - 1) % self.dags.len();
                    }
                } else {
                    self.detail_scroll.set(self.detail_scroll.get().saturating_sub(1));
                }
                TodoDagPanelResult::Handled
            }
            KeyCode::Down => {
                if self.page == TodoDagPanelPage::Overview {
                    if !self.dags.is_empty() {
                        self.selected = (self.selected + 1) % self.dags.len();
                    }
                } else {
                    self.detail_scroll.set(self.detail_scroll.get().saturating_add(1).min(self.detail_scroll_max()));
                }
                TodoDagPanelResult::Handled
            }
            KeyCode::Enter if self.page == TodoDagPanelPage::Overview => {
                self.page = TodoDagPanelPage::Detail;
                self.detail_scroll.set(0);
                TodoDagPanelResult::Handled
            }
            _ => TodoDagPanelResult::Unknown,
        }
    }
}

pub fn render_todo_dag_panel(frame: &mut ratatui::Frame<'_>, panel: &TodoDagPanel, theme: Theme) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    let title = match panel.page { TodoDagPanelPage::Overview => " Todo DAGs ", TodoDagPanelPage::Detail => " Todo DAG detail " };
    let block = Block::default().title(title).borders(Borders::ALL).border_style(Style::default().fg(theme.border_accent));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    match panel.page {
        TodoDagPanelPage::Overview => render_overview(frame, panel, inner, theme),
        TodoDagPanelPage::Detail => render_detail(frame, panel, inner, theme),
    }
}

fn render_overview(frame: &mut ratatui::Frame<'_>, panel: &TodoDagPanel, area: Rect, theme: Theme) {
    let footer_height = 2.min(area.height);
    let rows_height = area.height.saturating_sub(footer_height);
    let rows = Rect { height: rows_height, ..area };
    let footer = Rect { y: area.y.saturating_add(rows_height), height: footer_height, ..area };
    let viewport = usize::from(rows.height).max(1);
    let start = panel.selected.saturating_add(1).saturating_sub(viewport).min(panel.dags.len().saturating_sub(viewport));
    let lines = panel.dags.iter().enumerate().skip(start).take(viewport).map(|(index, dag)| {
        let selected = index == panel.selected;
        let counts = dag.counts();
        let style = if selected { Style::default().fg(theme.text).bg(theme.selected_bg).add_modifier(Modifier::BOLD) } else { Style::default().fg(theme.text) };
        Line::from(vec![
            Span::styled(format!(" {} ", if selected { '›' } else { ' ' }), style),
            Span::styled(format!("{:<24}", truncate(&dag.label, 24)), style),
            Span::styled(format!(" [{}] ", truncate(&dag.id, 18)), style.patch(Style::default().fg(theme.dim))),
            Span::styled(format!("{} · ✓{} open {} active {} blocked {}", dag.execution.label(), counts.completed, counts.open, counts.active, counts.blocked), status_style(&dag.execution, theme)),
        ])
    }).collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), rows);
    frame.render_widget(Paragraph::new("↑/↓ select · Enter details · Esc/q close").style(Style::default().fg(theme.dim)), footer);
}

fn render_detail(frame: &mut ratatui::Frame<'_>, panel: &TodoDagPanel, area: Rect, theme: Theme) {
    let Some(dag) = panel.selected_dag() else {
        frame.render_widget(Paragraph::new("No Todo DAG selected").style(Style::default().fg(theme.muted)), area);
        return;
    };
    let footer_height = 1.min(area.height);
    let content_height = area.height.saturating_sub(footer_height);
    let content = Rect { height: content_height, ..area };
    let footer = Rect { y: area.y.saturating_add(content_height), height: footer_height, ..area };
    let content_width = usize::from(content.width.max(1));
    panel.detail_width.set(content_width);
    panel.detail_viewport_height.set(usize::from(content_height));
    let lines = detail_display_lines(dag, theme, content_width);
    panel.detail_display_rows.set(lines.len());
    panel.clamp_detail_scroll();
    let max_scroll = panel.detail_scroll_max();
    let scroll = panel.detail_scroll.get();
    let visible = lines.into_iter().skip(scroll).take(usize::from(content_height)).collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(visible), content);
    frame.render_widget(Paragraph::new(format!("↑/↓ scroll · {}/{} · Esc DAGs · q close", scroll.saturating_add(1), max_scroll.saturating_add(1))).style(Style::default().fg(theme.dim)), footer);
}

fn detail_display_row_count(dag: &TodoDagSnapshot, width: usize) -> usize {
    detail_display_lines(dag, crate::theme::DARK, width).len()
}

fn detail_display_lines(dag: &TodoDagSnapshot, theme: Theme, width: usize) -> Vec<Line<'static>> {
    detail_lines(dag, theme).into_iter().flat_map(|line| wrap_styled_line(line, width.max(1))).collect()
}

fn wrap_styled_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    if line.spans.is_empty() { return vec![Line::default()]; }
    let mut rows = vec![Line::default()];
    let mut columns = 0usize;
    for span in line.spans {
        let style = span.style;
        let mut fragment = String::new();
        for character in span.content.chars() {
            let character_width = character.width().unwrap_or(0);
            if columns > 0 && columns.saturating_add(character_width) > width {
                if !fragment.is_empty() { rows.last_mut().expect("row").spans.push(Span::styled(std::mem::take(&mut fragment), style)); }
                rows.push(Line::default());
                columns = 0;
            }
            fragment.push(character);
            columns = columns.saturating_add(character_width);
        }
        if !fragment.is_empty() { rows.last_mut().expect("row").spans.push(Span::styled(fragment, style)); }
    }
    rows
}

fn detail_lines(dag: &TodoDagSnapshot, theme: Theme) -> Vec<Line<'static>> {
    let counts = dag.counts();
    let mut lines = vec![
        Line::from(vec![Span::styled(sanitize(&dag.label), Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)), Span::styled(format!("  [{}]", sanitize(&dag.id)), Style::default().fg(theme.dim))]),
        Line::from(vec![Span::styled("Execution  ", Style::default().fg(theme.dim)), Span::styled(dag.execution.label(), status_style(&dag.execution, theme).add_modifier(Modifier::BOLD))]),
        Line::from(Span::styled(format!("✓ {} completed · {} open · {} active · {} blocked", counts.completed, counts.open, counts.active, counts.blocked), Style::default().fg(theme.muted))),
        Line::default(),
    ];
    let names = dag.phases.iter().flat_map(|phase| &phase.tasks).map(|task| (task.id.as_str(), sanitize(&task.content))).collect::<HashMap<_, _>>();
    if dag.phases.is_empty() { lines.push(Line::from(Span::styled("No phases or tasks yet", Style::default().fg(theme.muted)))); }
    for phase in &dag.phases {
        lines.push(Line::from(Span::styled(sanitize(&phase.name), Style::default().fg(theme.accent).add_modifier(Modifier::BOLD))));
        if phase.tasks.is_empty() { lines.push(Line::from(Span::styled("  No tasks", Style::default().fg(theme.muted)))); }
        for task in &phase.tasks {
            lines.push(Line::from(vec![
                Span::styled(format!("  {} ", task_marker(task.status)), todo_style(task.status, theme)),
                Span::styled(sanitize(&task.content), Style::default().fg(theme.text)),
                Span::styled(format!("  [{}] · {}", sanitize(&task.id), todo_status(task.status)), Style::default().fg(theme.dim)),
            ]));
            let dependencies = task.depends_on.iter().map(|id| names.get(id.as_str()).cloned().unwrap_or_else(|| sanitize(id))).collect::<Vec<_>>();
            if !dependencies.is_empty() { lines.push(Line::from(Span::styled(format!("      depends_on: {}", dependencies.join(", ")), Style::default().fg(theme.dim)))); }
            let blockers = task.blocked_by.iter().map(|blocked| format!("{} ({})", sanitize(&blocked.content), todo_status(blocked.status))).collect::<Vec<_>>();
            if !blockers.is_empty() { lines.push(Line::from(Span::styled(format!("      blocked_by: {}", blockers.join(", ")), Style::default().fg(theme.warning)))); }
            push_linked_jobs(&mut lines, dag, &task.id, theme);
        }
        lines.push(Line::default());
    }
    lines
}

fn push_linked_jobs(lines: &mut Vec<Line<'static>>, dag: &TodoDagSnapshot, task_id: &str, theme: Theme) {
    for card in dag.jobs.iter().filter(|card| card.todo_task_id.as_deref() == Some(task_id)) {
        let summary = card.rows.iter().find(|row| matches!(row.role, crate::job_card_adapter::JobCardRowRole::Description)).map_or("", |row| row.text.as_str());
        let suffix = if summary.is_empty() { String::new() } else { format!(" · {}", sanitize(summary)) };
        lines.push(Line::from(Span::styled(format!("      job: {} · {}{suffix}", sanitize(&card.display_name), job_status(card.job_status)), Style::default().fg(job_color(card.job_status, theme)))));
    }
    for job in dag.workflow_jobs.iter().filter(|job| job.todo_task_id.as_deref() == Some(task_id)) {
        let suffix = job.task_summary.as_deref().map_or(String::new(), |summary| format!(" · {}", sanitize(summary)));
        lines.push(Line::from(Span::styled(format!("      job: {} · {}{suffix}", sanitize(&job.display_name), job_status(job.status)), Style::default().fg(job_color(job.status, theme)))));
    }
}

fn status_style(status: &TodoDagExecutionLabel, theme: Theme) -> Style {
    let color = match status {
        TodoDagExecutionLabel::Main(TodoDagExecutionStatus::Active) | TodoDagExecutionLabel::Workflow(WorkflowStatus::Running) => theme.accent,
        TodoDagExecutionLabel::Main(TodoDagExecutionStatus::Settled) | TodoDagExecutionLabel::Workflow(WorkflowStatus::Completed) => theme.success,
        TodoDagExecutionLabel::Main(TodoDagExecutionStatus::Blocked) | TodoDagExecutionLabel::Workflow(WorkflowStatus::Failed | WorkflowStatus::Conflicted) => theme.error,
        TodoDagExecutionLabel::Workflow(WorkflowStatus::Planning | WorkflowStatus::Integrating) => theme.warning,
        _ => theme.muted,
    };
    Style::default().fg(color)
}

fn todo_style(status: TodoStatus, theme: Theme) -> Style { Style::default().fg(match status { TodoStatus::Pending => theme.muted, TodoStatus::InProgress => theme.accent, TodoStatus::Completed => theme.success, TodoStatus::Abandoned => theme.dim }) }
fn task_marker(status: TodoStatus) -> &'static str { match status { TodoStatus::Pending => "○", TodoStatus::InProgress => "●", TodoStatus::Completed => "✓", TodoStatus::Abandoned => "×" } }
fn todo_status(status: TodoStatus) -> &'static str { match status { TodoStatus::Pending => "pending", TodoStatus::InProgress => "in progress", TodoStatus::Completed => "completed", TodoStatus::Abandoned => "abandoned" } }
fn job_status(status: JobStatus) -> &'static str { match status { JobStatus::Queued => "queued", JobStatus::Running => "running", JobStatus::Completed => "completed", JobStatus::Failed => "failed", JobStatus::Cancelled => "cancelled" } }
fn job_color(status: JobStatus, theme: Theme) -> ratatui::style::Color { match status { JobStatus::Queued => theme.muted, JobStatus::Running => theme.accent, JobStatus::Completed => theme.success, JobStatus::Failed => theme.error, JobStatus::Cancelled => theme.dim } }
fn sanitize(value: &str) -> String { value.chars().map(|character| if character.is_control() { ' ' } else { character }).collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ") }
fn truncate(value: &str, max: usize) -> String { let mut chars = value.chars(); let mut output = chars.by_ref().take(max).collect::<String>(); if chars.next().is_some() && max > 0 { output.pop(); output.push('…'); } output }

#[cfg(test)]
mod tests {
    use super::*;
    use pi_coding::{TodoBlockedReason, TodoItem};
    use ratatui::{Terminal, backend::TestBackend};

    fn task(id: &str, content: &str, status: TodoStatus, ready: bool, blocked_by: &[&str]) -> TodoItem {
        TodoItem {
            id: id.to_owned(), content: content.to_owned(), status, depends_on: blocked_by.iter().map(|id| (*id).to_owned()).collect(), ready,
            blocked_by: blocked_by.iter().map(|id| TodoBlockedReason { task_id: (*id).to_owned(), content: format!("dependency {id}"), status: TodoStatus::InProgress }).collect(),
        }
    }

    fn snapshot() -> TodoDagSnapshot {
        TodoDagSnapshot::main(vec![TodoPhase { name: "Build".to_owned(), tasks: vec![task("done", "Plan", TodoStatus::Completed, false, &[]), task("active", "Implement", TodoStatus::InProgress, true, &[]), task("blocked", "Verify", TodoStatus::Pending, false, &["active"])] }], TodoDagExecutionStatus::Active, Vec::new())
    }

    #[test]
    fn counts_statuses_deterministically() {
        assert_eq!(snapshot().counts(), TodoDagCounts { completed: 1, open: 2, active: 1, blocked: 1 });
    }

    #[test]
    fn navigation_enters_detail_and_escape_steps_back_then_closes() {
        let workflow = TodoDagSnapshot::workflow("wf-b".to_owned(), "Beta".to_owned(), Vec::new(), WorkflowStatus::Running, Vec::new());
        let mut panel = TodoDagPanel::new(snapshot(), vec![workflow]);
        assert_eq!(panel.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)), TodoDagPanelResult::Handled);
        assert_eq!(panel.selected_dag().map(|dag| dag.id.as_str()), Some("wf-b"));
        panel.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(panel.page(), TodoDagPanelPage::Detail);
        assert_eq!(panel.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), TodoDagPanelResult::Handled);
        assert_eq!(panel.page(), TodoDagPanelPage::Overview);
        assert_eq!(panel.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), TodoDagPanelResult::Close);
    }

    #[test]
    fn render_distinguishes_dags_and_shows_blockers() {
        let workflow = TodoDagSnapshot::workflow("wf-b".to_owned(), "Beta workflow".to_owned(), Vec::new(), WorkflowStatus::Running, Vec::new());
        let mut panel = TodoDagPanel::new(snapshot(), vec![workflow]);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK)).unwrap();
        let overview = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(overview.contains("Main session"));
        assert!(overview.contains("Beta workflow"));
        panel.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        terminal.draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK)).unwrap();
        let detail = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(detail.contains("Build"));
        assert!(detail.contains("Verify"));
        assert!(detail.contains("blocked_by: dependency active (in progress)"));
    }

    #[test]
    fn detail_renders_linked_job_card_state() {
        let mut dag = snapshot();
        dag.jobs.push(JobCardRows {
            job_id: "opaque-job".to_owned(), ordinal: 0, agent_id: "opaque-agent".to_owned(), agent: "task".to_owned(), display_name: "Verifier".to_owned(),
            todo_task_id: Some("blocked".to_owned()), job_status: JobStatus::Running, agent_status: None, summary: None,
            rows: vec![crate::job_card_adapter::JobCardRow { job_id: Some("opaque-job".to_owned()), role: crate::job_card_adapter::JobCardRowRole::Description, text: "Run focused verification".to_owned() }],
        });
        let mut panel = TodoDagPanel::new(dag, Vec::new());
        panel.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK)).unwrap();
        let detail = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(detail.contains("job: Verifier · running · Run focused verification"));
        assert!(!detail.contains("opaque-job"));
    }

    #[test]
    fn long_detail_scroll_reaches_tasks_below_viewport_and_clamps_after_shrink() {
        let tasks = (0..24).map(|index| task(&format!("task-{index}"), &format!("Task row {index}"), TodoStatus::Pending, true, &[])).collect();
        let dag = TodoDagSnapshot::main(vec![TodoPhase { name: "Long phase".to_owned(), tasks }], TodoDagExecutionStatus::Active, Vec::new());
        let mut panel = TodoDagPanel::new(dag, Vec::new());
        panel.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK)).unwrap();
        let first = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(first.contains("Task row 0"));
        assert!(!first.contains("Task row 23"));
        for _ in 0..40 { panel.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)); }
        terminal.draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK)).unwrap();
        let last = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(last.contains("Task row 23"));
        let bottom = panel.detail_scroll();
        assert!(bottom > 0);
        panel.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(panel.detail_scroll(), bottom - 1);

        panel.update_main_phases(vec![TodoPhase { name: "Short".to_owned(), tasks: vec![task("one", "Only row", TodoStatus::Pending, true, &[])] }]);
        assert_eq!(panel.detail_scroll(), 0);
        terminal.draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK)).unwrap();
        let shrunk = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(shrunk.contains("Only row"));
        assert!(shrunk.contains("1/1"));
    }

    #[test]
    fn narrow_wrapped_rows_scroll_to_final_sentinel() {
        let long = "界".repeat(32);
        let mut first = task("long", &format!("Long task {long}"), TodoStatus::Pending, false, &["dependency"]);
        first.blocked_by[0].content = format!("Long dependency {long}");
        let final_task = task("final", "FINAL SENTINEL", TodoStatus::Pending, true, &[]);
        let mut dag = TodoDagSnapshot::main(vec![TodoPhase { name: "Narrow".to_owned(), tasks: vec![first, final_task] }], TodoDagExecutionStatus::Active, Vec::new());
        dag.jobs.push(JobCardRows {
            job_id: "job".to_owned(), ordinal: 0, agent_id: "agent".to_owned(), agent: "task".to_owned(), display_name: "Worker".to_owned(),
            todo_task_id: Some("long".to_owned()), job_status: JobStatus::Running, agent_status: None, summary: None,
            rows: vec![crate::job_card_adapter::JobCardRow { job_id: Some("job".to_owned()), role: crate::job_card_adapter::JobCardRowRole::Description, text: format!("Long job {long}") }],
        });
        let mut panel = TodoDagPanel::new(dag, Vec::new());
        panel.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let backend = TestBackend::new(24, 9);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK)).unwrap();
        let first_view = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(!first_view.contains("FINAL SENTINEL"));
        for _ in 0..100 { panel.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)); }
        terminal.draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK)).unwrap();
        let final_view = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(final_view.contains("FINAL SENTINEL"));
        let bottom = panel.detail_scroll();
        panel.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(panel.detail_scroll(), bottom - 1);
    }
}
