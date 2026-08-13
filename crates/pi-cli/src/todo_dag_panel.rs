//! Dedicated two-level Todo DAG overview and detail panel.

use std::cell::Cell;
use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use pi_coding::{AgentStatus, JobStatus, TodoDagExecutionStatus, TodoPhase, TodoStatus, WorkflowRuntimeJobDetail, WorkflowStatus};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::job_card_adapter::JobCardRows;
use crate::theme::Theme;
pub use crate::todo_dag_view::TodoDagCounts;
use crate::todo_dag_view::{sanitize, task_marker, todo_status, todo_style, truncate, wrap_styled_line};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TodoDagPanelPage {
    Overview,
    Detail,
    /// Focused detail of one subagent job, opened from a navigable job row.
    Subagent,
}

/// One subagent row projected from either a main-session job card or a
/// workflow runtime job. Kept next to the source types so the overview rows,
/// the navigable list, and the subagent detail page share one identity
/// resolution (display name · status · current task).
#[derive(Clone, Debug, PartialEq, Eq)]
enum TodoDagSubagentJob {
    Card(JobCardRows),
    Workflow(WorkflowRuntimeJobDetail),
}

/// Concrete subagent identity for display: who it is, its type, what it is
/// doing right now, and which todo task it owns.
struct TodoSubagentIdentity {
    display_name: String,
    agent_type: Option<String>,
    status: String,
    status_kind: JobStatus,
    task_summary: String,
    todo_task_id: Option<String>,
}

fn subagent_identity(job: &TodoDagSubagentJob) -> TodoSubagentIdentity {
    match job {
        TodoDagSubagentJob::Card(card) => {
            let task_summary = card
                .summary
                .clone()
                .filter(|summary| !summary.trim().is_empty())
                .or_else(|| {
                    card.rows
                        .iter()
                        .find(|row| matches!(row.role, crate::job_card_adapter::JobCardRowRole::Description))
                        .map(|row| row.text.clone())
                })
                .unwrap_or_default();
            let status = match card.agent_status {
                Some(AgentStatus::Parked) => format!("{} · parked", job_status(card.job_status)),
                _ => job_status(card.job_status).to_owned(),
            };
            TodoSubagentIdentity {
                display_name: card.display_name.clone(),
                agent_type: (!card.agent.trim().is_empty()).then(|| card.agent.clone()),
                status,
                status_kind: card.job_status,
                task_summary,
                todo_task_id: card.todo_task_id.clone(),
            }
        }
        TodoDagSubagentJob::Workflow(job) => TodoSubagentIdentity {
            display_name: job.display_name.clone(),
            agent_type: None,
            status: job_status(job.status).to_owned(),
            status_kind: job.status,
            task_summary: job.task_summary.clone().unwrap_or_default(),
            todo_task_id: job.todo_task_id.clone(),
        },
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

    /// Number of subagent jobs attached to this DAG (main cards + workflow jobs).
    #[must_use]
    pub fn subagent_job_count(&self) -> usize {
        self.jobs.len() + self.workflow_jobs.len()
    }

    /// The `index`-th subagent job: main-session job cards first, then workflow jobs.
    fn subagent_job(&self, index: usize) -> Option<TodoDagSubagentJob> {
        if index < self.jobs.len() {
            self.jobs.get(index).cloned().map(TodoDagSubagentJob::Card)
        } else {
            self.workflow_jobs
                .get(index.saturating_sub(self.jobs.len()))
                .cloned()
                .map(TodoDagSubagentJob::Workflow)
        }
    }
}

#[derive(Clone, Debug)]
pub struct TodoDagPanel {
    dags: Vec<TodoDagSnapshot>,
    /// Selected DAG index; the overview list also selects a subagent row inside it.
    selected: usize,
    /// Selected subagent row inside `selected` (`None` selects the DAG header).
    selected_job: Option<usize>,
    /// (dag index, job index) shown by the Subagent page.
    subagent: Option<(usize, usize)>,
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
        Self { dags, selected: 0, selected_job: None, subagent: None, page: TodoDagPanelPage::Overview, detail_scroll: Cell::new(0), detail_width: Cell::new(1), detail_viewport_height: Cell::new(0), detail_display_rows: Cell::new(0) }
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
        self.clamp_selection();
    }

    pub fn update_main(&mut self, phases: Vec<TodoPhase>, status: TodoDagExecutionStatus, jobs: Vec<JobCardRows>) {
        if let Some(main) = self.dags.first_mut() {
            main.phases = phases;
            main.execution = TodoDagExecutionLabel::Main(status);
            main.jobs = jobs;
        }
        self.clamp_selection();
    }

    pub fn update_main_phases(&mut self, phases: Vec<TodoPhase>) {
        if let Some(main) = self.dags.first_mut() { main.phases = phases; }
        self.clamp_selection();
    }

    pub fn update_main_jobs(&mut self, jobs: Vec<JobCardRows>) {
        if let Some(main) = self.dags.first_mut() { main.jobs = jobs; }
        self.clamp_selection();
    }

    pub fn update_workflows(&mut self, workflows: Vec<TodoDagSnapshot>) {
        let main = self.dags.first().cloned().unwrap_or_else(|| TodoDagSnapshot::main(Vec::new(), TodoDagExecutionStatus::Dormant, Vec::new()));
        self.replace(main, workflows);
    }

    /// Keep the overview and subagent selections inside the current DAG/job
    /// bounds after a snapshot replacement; drop a stale subagent page back to
    /// the overview.
    fn clamp_selection(&mut self) {
        if self.selected >= self.dags.len() {
            self.selected = self.dags.len().saturating_sub(1);
        }
        if let Some(job_index) = self.selected_job {
            let in_bounds = self
                .selected_dag()
                .is_some_and(|dag| job_index < dag.subagent_job_count());
            if !in_bounds {
                self.selected_job = None;
            }
        }
        if let Some((dag_index, job_index)) = self.subagent {
            let in_bounds = self
                .dags
                .get(dag_index)
                .is_some_and(|dag| job_index < dag.subagent_job_count());
            if !in_bounds {
                self.subagent = None;
                if self.page == TodoDagPanelPage::Subagent {
                    self.page = TodoDagPanelPage::Overview;
                }
            }
        }
        self.clamp_detail_scroll();
    }

    #[must_use]
    pub fn page(&self) -> TodoDagPanelPage { self.page }


    #[must_use]
    pub fn selected_dag(&self) -> Option<&TodoDagSnapshot> { self.dags.get(self.selected) }

    /// Selected subagent row inside the selected DAG (`None` = DAG header).
    #[must_use]
    pub fn selected_job(&self) -> Option<usize> { self.selected_job }

    #[must_use]
    pub fn detail_scroll(&self) -> usize { self.detail_scroll.get() }

    fn detail_scroll_max(&self) -> usize {
        self.detail_display_rows.get().saturating_sub(self.detail_viewport_height.get())
    }

    fn refresh_detail_geometry(&self) {
        let rows = match self.page {
            TodoDagPanelPage::Overview => 0,
            TodoDagPanelPage::Detail | TodoDagPanelPage::Subagent => self
                .detail_source_lines(crate::theme::DARK)
                .into_iter()
                .flat_map(|line| wrap_styled_line(line, self.detail_width.get().max(1)))
                .count(),
        };
        self.detail_display_rows.set(rows);
        self.detail_scroll.set(self.detail_scroll.get().min(self.detail_scroll_max()));
    }

    /// Raw (unwrapped) lines for the current detail-like page.
    fn detail_source_lines(&self, theme: Theme) -> Vec<Line<'static>> {
        match self.page {
            TodoDagPanelPage::Overview => Vec::new(),
            TodoDagPanelPage::Detail => self
                .selected_dag()
                .map_or_else(Vec::new, |dag| detail_lines(dag, theme)),
            TodoDagPanelPage::Subagent => self
                .subagent
                .and_then(|(dag_index, job_index)| {
                    let dag = self.dags.get(dag_index)?;
                    let job = dag.subagent_job(job_index)?;
                    Some(subagent_detail_lines(dag, &job, theme))
                })
                .unwrap_or_default(),
        }
    }

    /// Cursor position over the flat overview list of DAG headers + subagent rows.
    fn overview_cursor(&self) -> usize {
        let mut cursor = 0;
        for (index, dag) in self.dags.iter().enumerate() {
            if index == self.selected {
                return cursor + self.selected_job.map_or(0, |job| job + 1);
            }
            cursor += 1 + dag.subagent_job_count();
        }
        cursor.min(self.overview_item_count().saturating_sub(1))
    }

    fn overview_item_count(&self) -> usize {
        self.dags.iter().map(|dag| 1 + dag.subagent_job_count()).sum()
    }

    fn set_overview_cursor(&mut self, cursor: usize) {
        let mut remaining = cursor;
        for (index, dag) in self.dags.iter().enumerate() {
            if remaining == 0 {
                self.selected = index;
                self.selected_job = None;
                return;
            }
            remaining -= 1;
            let count = dag.subagent_job_count();
            if remaining < count {
                self.selected = index;
                self.selected_job = Some(remaining);
                return;
            }
            remaining -= count;
        }
        let last = self.dags.len().saturating_sub(1);
        self.selected = last;
        self.selected_job = None;
    }

    fn move_overview(&mut self, delta: isize) {
        let total = self.overview_item_count();
        if total == 0 {
            return;
        }
        let current = self.overview_cursor();
        let next = if delta < 0 {
            current.saturating_sub(1)
        } else {
            current.saturating_add(1).min(total - 1)
        };
        self.set_overview_cursor(next);
    }

    fn clamp_detail_scroll(&self) { self.refresh_detail_geometry(); }

    pub fn handle_key(&mut self, key: KeyEvent) -> TodoDagPanelResult {
        if key.kind != KeyEventKind::Press {
            return TodoDagPanelResult::Handled;
        }
        let plain = !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('q') if plain => TodoDagPanelResult::Close,
            KeyCode::Esc => match self.page {
                TodoDagPanelPage::Subagent | TodoDagPanelPage::Detail => {
                    self.page = TodoDagPanelPage::Overview;
                    TodoDagPanelResult::Handled
                }
                TodoDagPanelPage::Overview => TodoDagPanelResult::Close,
            },
            KeyCode::Up | KeyCode::Char('k') => {
                match (self.page, key.code) {
                    (TodoDagPanelPage::Overview, _) => self.move_overview(-1),
                    (TodoDagPanelPage::Detail | TodoDagPanelPage::Subagent, _) => {
                        self.detail_scroll.set(self.detail_scroll.get().saturating_sub(1));
                    }
                    _ => {}
                }
                TodoDagPanelResult::Handled
            }
            KeyCode::Down | KeyCode::Char('j') => {
                match (self.page, key.code) {
                    (TodoDagPanelPage::Overview, _) => self.move_overview(1),
                    (TodoDagPanelPage::Detail | TodoDagPanelPage::Subagent, _) => {
                        self.detail_scroll.set(self.detail_scroll.get().saturating_add(1).min(self.detail_scroll_max()));
                    }
                    _ => {}
                }
                TodoDagPanelResult::Handled
            }
            KeyCode::Enter if self.page == TodoDagPanelPage::Overview => {
                if let Some(job_index) = self.selected_job {
                    self.subagent = Some((self.selected, job_index));
                    self.page = TodoDagPanelPage::Subagent;
                } else {
                    self.page = TodoDagPanelPage::Detail;
                }
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
    let title = match panel.page {
        TodoDagPanelPage::Overview => " Todo DAGs ",
        TodoDagPanelPage::Detail => " Todo DAG detail ",
        TodoDagPanelPage::Subagent => " Subagent detail ",
    };
    let block = Block::default().title(title).borders(Borders::ALL).border_style(Style::default().fg(theme.border_accent));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    match panel.page {
        TodoDagPanelPage::Overview => render_overview(frame, panel, inner, theme),
        TodoDagPanelPage::Detail | TodoDagPanelPage::Subagent => render_detail(frame, panel, inner, theme),
    }
}

fn render_overview(frame: &mut ratatui::Frame<'_>, panel: &TodoDagPanel, area: Rect, theme: Theme) {
    let footer_height = 2.min(area.height);
    let rows_height = area.height.saturating_sub(footer_height);
    let rows = Rect { height: rows_height, ..area };
    let footer = Rect { y: area.y.saturating_add(rows_height), height: footer_height, ..area };
    let viewport = usize::from(rows.height).max(1);
    let width = usize::from(area.width.max(1));
    let mut lines = Vec::new();
    let mut cursor_line = 0usize;
    for (index, dag) in panel.dags.iter().enumerate() {
        let selected = index == panel.selected;
        let header_selected = selected && panel.selected_job.is_none();
        // The header wraps to the available width (word boundaries only) so a
        // narrow pane never clips a count term mid-word.
        let header_start = lines.len();
        lines.extend(wrap_styled_line_words(overview_header_line(dag, theme, header_selected), width));
        if header_selected {
            cursor_line = header_start;
        }
        for job_index in 0..dag.subagent_job_count() {
            let Some(job) = dag.subagent_job(job_index) else { continue };
            let job_selected = selected && panel.selected_job == Some(job_index);
            if job_selected {
                cursor_line = lines.len();
            }
            lines.push(subagent_row_line(&job, theme, job_selected));
        }
    }
    let total = lines.len();
    let start = cursor_line.saturating_add(1).saturating_sub(viewport).min(total.saturating_sub(viewport));
    let visible = lines.into_iter().skip(start).take(viewport).collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(visible), rows);
    frame.render_widget(Paragraph::new("↑/↓/j/k select subagent · Enter details · Esc/q close").style(Style::default().fg(theme.dim)), footer);
}

/// One DAG header line: selection marker, label, id, execution label, and the
/// same `✓ N completed · O open · A active · B blocked` count terms as the
/// detail page (the overview previously compressed these to `✓N open N
/// active …`, which read differently from the detail line). The count span
/// patches the header's selection style so the highlight survives wrapping.
fn overview_header_line(dag: &TodoDagSnapshot, theme: Theme, selected: bool) -> Line<'static> {
    let counts = dag.counts();
    let style = if selected {
        Style::default().fg(theme.text).bg(theme.selected_bg).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text)
    };
    Line::from(vec![
        Span::styled(format!(" {} ", if selected { '›' } else { ' ' }), style),
        Span::styled(format!("{:<24}", truncate(&dag.label, 24)), style),
        Span::styled(format!(" [{}] ", truncate(&dag.id, 18)), style.patch(Style::default().fg(theme.dim))),
        Span::styled(format!("{} · ✓ {} completed · {} open · {} active · {} blocked", dag.execution.label(), counts.completed, counts.open, counts.active, counts.blocked), style.patch(status_style(&dag.execution, theme))),
    ])
}

/// Word-boundary-aware wrap of a styled [`Line`] to `width` that preserves
/// per-span styles. Rows break between words, so a narrow pane never cuts a
/// word mid-word (the overview header previously clipped at the right edge);
/// only a single word wider than `width` is hard-split, mirroring
/// [`wrap_styled_line`]'s per-character fallback.
fn wrap_styled_line_words(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    if line.spans.is_empty() || width == 0 {
        return vec![Line::default()];
    }
    // Flatten the spans into (word, style) pairs; whitespace is a separator.
    let mut words = Vec::<(String, Style)>::new();
    for span in line.spans {
        let mut word = String::new();
        for character in span.content.chars() {
            if character.is_whitespace() {
                if !word.is_empty() {
                    words.push((std::mem::take(&mut word), span.style));
                }
            } else {
                word.push(character);
            }
        }
        if !word.is_empty() {
            words.push((word, span.style));
        }
    }
    let mut rows = Vec::<Vec<(String, Style)>>::new();
    for (word, style) in words {
        let word_width = UnicodeWidthStr::width(word.as_str());
        if word_width > width {
            // A single word wider than the pane: hard-split it across rows so
            // no content is lost (only reachable with a pathological label).
            let mut remaining = word.as_str();
            while !remaining.is_empty() {
                let mut columns = 0usize;
                let mut split = 0usize;
                for character in remaining.chars() {
                    let character_width = character.width().unwrap_or(0);
                    if columns.saturating_add(character_width) > width {
                        break;
                    }
                    columns += character_width;
                    split += character.len_utf8();
                }
                if split == 0 {
                    split = remaining.chars().next().map_or(0, char::len_utf8);
                }
                rows.push(vec![(remaining[..split].to_owned(), style)]);
                remaining = &remaining[split..];
            }
            continue;
        }
        let fits = rows.last().is_some_and(|row| {
            let row_width = row
                .iter()
                .map(|(text, _)| UnicodeWidthStr::width(text.as_str()))
                .sum::<usize>();
            row_width
                .saturating_add(row.len().saturating_sub(1))
                .saturating_add(1)
                .saturating_add(word_width)
                <= width
        });
        if fits {
            rows.last_mut().expect("row").push((word, style));
        } else {
            rows.push(vec![(word, style)]);
        }
    }
    rows.into_iter()
        .map(|row| {
            Line::from(
                row.into_iter()
                    .enumerate()
                    .map(|(index, (text, style))| {
                        Span::styled(if index == 0 { text } else { format!(" {text}") }, style)
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

/// Compact overview row per subagent job: `• <display_name> · <status> · <task>`.
fn subagent_row_line(job: &TodoDagSubagentJob, theme: Theme, selected: bool) -> Line<'static> {
    let identity = subagent_identity(job);
    let style = if selected {
        Style::default().fg(theme.text).bg(theme.selected_bg).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text)
    };
    let mut spans = vec![
        Span::styled(format!(" {} ", if selected { '›' } else { ' ' }), style),
        Span::styled("• ", Style::default().fg(if selected { theme.accent } else { theme.dim })),
        Span::styled(sanitize(&identity.display_name), style.patch(Style::default().fg(theme.accent))),
    ];
    if let Some(agent_type) = &identity.agent_type {
        spans.push(Span::styled(format!(" ({})", sanitize(agent_type)), style.patch(Style::default().fg(theme.dim))));
    }
    spans.push(Span::styled(format!(" · {}", identity.status), style.patch(Style::default().fg(job_color(identity.status_kind, theme)))));
    let task = truncate(&identity.task_summary, 60);
    if !task.is_empty() {
        spans.push(Span::styled(format!(" · {task}"), style.patch(Style::default().fg(theme.dim))));
    }
    Line::from(spans)
}

fn render_detail(frame: &mut ratatui::Frame<'_>, panel: &TodoDagPanel, area: Rect, theme: Theme) {
    let footer_height = 1.min(area.height);
    let content_height = area.height.saturating_sub(footer_height);
    let content = Rect { height: content_height, ..area };
    let footer = Rect { y: area.y.saturating_add(content_height), height: footer_height, ..area };
    let content_width = usize::from(content.width.max(1));
    panel.detail_width.set(content_width);
    panel.detail_viewport_height.set(usize::from(content_height));
    let lines = panel
        .detail_source_lines(theme)
        .into_iter()
        .flat_map(|line| wrap_styled_line(line, content_width.max(1)))
        .collect::<Vec<_>>();
    panel.detail_display_rows.set(lines.len());
    panel.clamp_detail_scroll();
    let max_scroll = panel.detail_scroll_max();
    let scroll = panel.detail_scroll.get();
    let visible = lines.into_iter().skip(scroll).take(usize::from(content_height)).collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(visible), content);
    frame.render_widget(Paragraph::new(format!("↑/↓ scroll · {}/{} · Esc DAGs · q close", scroll.saturating_add(1), max_scroll.saturating_add(1))).style(Style::default().fg(theme.dim)), footer);
}

/// Focused subagent detail page: identity, type, status, owning DAG, the
/// linked todo task content, and the current task summary.
fn subagent_detail_lines(dag: &TodoDagSnapshot, job: &TodoDagSubagentJob, theme: Theme) -> Vec<Line<'static>> {
    let identity = subagent_identity(job);
    let mut lines = vec![
        Line::from(Span::styled(sanitize(&identity.display_name), Style::default().fg(theme.accent).add_modifier(Modifier::BOLD))),
        Line::from(vec![
            Span::styled("Type        ", Style::default().fg(theme.dim)),
            Span::styled(sanitize(identity.agent_type.as_deref().unwrap_or("—")), Style::default().fg(theme.text)),
        ]),
        Line::from(vec![
            Span::styled("Status      ", Style::default().fg(theme.dim)),
            Span::styled(identity.status, Style::default().fg(job_color(identity.status_kind, theme))),
        ]),
        Line::from(vec![
            Span::styled("DAG         ", Style::default().fg(theme.dim)),
            Span::styled(format!("{} [{}]", sanitize(&dag.label), sanitize(&dag.id)), Style::default().fg(theme.text)),
        ]),
    ];
    let todo_task = identity
        .todo_task_id
        .as_deref()
        .and_then(|id| dag.phases.iter().flat_map(|phase| &phase.tasks).find(|task| task.id == id));
    match todo_task {
        Some(task) => lines.push(Line::from(vec![
            Span::styled("Todo task   ", Style::default().fg(theme.dim)),
            Span::styled(format!("{} [{}]", sanitize(&task.content), sanitize(&task.id)), Style::default().fg(theme.text)),
        ])),
        None => lines.push(Line::from(vec![
            Span::styled("Todo task   ", Style::default().fg(theme.dim)),
            Span::styled("—", Style::default().fg(theme.muted)),
        ])),
    }
    if identity.task_summary.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("Task        ", Style::default().fg(theme.dim)),
            Span::styled("—", Style::default().fg(theme.muted)),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled("Task        ", Style::default().fg(theme.dim)),
            Span::styled(sanitize(&identity.task_summary), Style::default().fg(theme.text)),
        ]));
    }
    if let TodoDagSubagentJob::Card(card) = job {
        push_card_progress_lines(&mut lines, card, theme);
    }
    lines
}

/// Live progress block for a main-session job card: the one-line progress
/// (latest activity · elapsed, coarse stage as fallback), the bounded event
/// log, and the full-transcript hint.
fn push_card_progress_lines(lines: &mut Vec<Line<'static>>, card: &JobCardRows, theme: Theme) {
    let Some(progress) = &card.progress else { return };
    let progress_text = card
        .rows
        .iter()
        .find(|row| row.role == crate::job_card_adapter::JobCardRowRole::Progress)
        .map(|row| row.text.as_str())
        .unwrap_or_else(|| progress.stage.as_deref().unwrap_or("running"));
    lines.push(Line::from(vec![
        Span::styled("Progress    ", Style::default().fg(theme.dim)),
        Span::styled(sanitize(progress_text), Style::default().fg(job_color(card.job_status, theme))),
    ]));
    if !progress.events.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(
            Span::styled("Activity log", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
        ));
        for entry in &progress.events {
            let marker = match entry.kind {
                crate::job_card_adapter::JobEventKind::Job => "•",
                crate::job_card_adapter::JobEventKind::Message => "‹",
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {marker} {}  ", format_event_time(entry.at)),
                    Style::default().fg(theme.dim),
                ),
                Span::styled(sanitize(&entry.text), Style::default().fg(theme.text)),
            ]));
        }
    }
    let history = progress
        .history_ref
        .as_deref()
        .filter(|reference| !reference.trim().is_empty());
    if let Some(history) = history {
        lines.push(Line::from(vec![
            Span::styled("Transcript  ", Style::default().fg(theme.dim)),
            Span::styled(sanitize(history), Style::default().fg(theme.muted)),
        ]));
    }
}

/// Epoch millis → `HH:MM:SS` wall-clock display for activity-log entries.
fn format_event_time(millis: u64) -> String {
    let total_seconds = millis / 1_000;
    let seconds = total_seconds % 60;
    let minutes = (total_seconds / 60) % 60;
    let hours = (total_seconds / 3_600) % 24;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
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
        let agent_type = if card.agent.trim().is_empty() { String::new() } else { format!(" ({})", sanitize(&card.agent)) };
        lines.push(Line::from(Span::styled(format!("      • {}{agent_type} · {}{suffix}", sanitize(&card.display_name), job_status(card.job_status)), Style::default().fg(job_color(card.job_status, theme)))));
    }
    for job in dag.workflow_jobs.iter().filter(|job| job.todo_task_id.as_deref() == Some(task_id)) {
        let suffix = job.task_summary.as_deref().map_or(String::new(), |summary| format!(" · {}", sanitize(summary)));
        lines.push(Line::from(Span::styled(format!("      • {} · {}{suffix}", sanitize(&job.display_name), job_status(job.status)), Style::default().fg(job_color(job.status, theme)))));
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

fn job_status(status: JobStatus) -> &'static str { match status { JobStatus::Queued => "queued", JobStatus::Running => "running", JobStatus::Completed => "completed", JobStatus::Failed => "failed", JobStatus::Cancelled => "cancelled" } }
fn job_color(status: JobStatus, theme: Theme) -> ratatui::style::Color { match status { JobStatus::Queued => theme.muted, JobStatus::Running => theme.accent, JobStatus::Completed => theme.success, JobStatus::Failed => theme.error, JobStatus::Cancelled => theme.dim } }

#[cfg(test)]
mod tests {
    use super::*;
    use pi_coding::{TodoBlockedReason, TodoItem};
    use ratatui::{Terminal, backend::TestBackend};

    fn task(id: &str, content: &str, status: TodoStatus, ready: bool, blocked_by: &[&str]) -> TodoItem {
        TodoItem {
            id: id.to_owned(), content: content.to_owned(), status, depends_on: blocked_by.iter().map(|id| (*id).to_owned()).collect(), ready,
            blocked_by: blocked_by.iter().map(|id| TodoBlockedReason { task_id: (*id).to_owned(), content: format!("dependency {id}"), status: TodoStatus::InProgress }).collect(),
            agent: None,
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
            progress: None,
            rows: vec![crate::job_card_adapter::JobCardRow { job_id: Some("opaque-job".to_owned()), role: crate::job_card_adapter::JobCardRowRole::Description, text: "Run focused verification".to_owned() }],
        });
        let mut panel = TodoDagPanel::new(dag, Vec::new());
        panel.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK)).unwrap();
        let detail = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(detail.contains("• Verifier (task) · running · Run focused verification"));
        assert!(!detail.contains("opaque-job"));
    }

    #[test]
    fn overview_shows_subagent_rows_with_name_status_and_task() {
        let mut dag = snapshot();
        dag.jobs.push(JobCardRows {
            job_id: "job-a".to_owned(), ordinal: 0, agent_id: "agent-a".to_owned(), agent: "task".to_owned(), display_name: "Verifier".to_owned(),
            todo_task_id: Some("blocked".to_owned()), job_status: JobStatus::Running, agent_status: None, summary: None,
            progress: None,
            rows: vec![crate::job_card_adapter::JobCardRow { job_id: Some("job-a".to_owned()), role: crate::job_card_adapter::JobCardRowRole::Description, text: "Run focused verification".to_owned() }],
        });
        let workflow = TodoDagSnapshot::workflow(
            "wf-a".to_owned(),
            "Alpha workflow".to_owned(),
            Vec::new(),
            WorkflowStatus::Running,
            vec![WorkflowRuntimeJobDetail {
                display_name: "Alpha Worker".to_owned(),
                status: JobStatus::Queued,
                task_summary: Some("Assemble the release".to_owned()),
                todo_task_id: Some("active".to_owned()),
                created_at_ms: 0,
                started_at_ms: None,
                finished_at_ms: None,
            }],
        );
        let panel = TodoDagPanel::new(dag, vec![workflow]);
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK)).unwrap();
        let overview = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(overview.contains("• Verifier (task) · running · Run focused verification"));
        assert!(overview.contains("• Alpha Worker · queued · Assemble the release"));
    }

    #[test]
    fn overview_header_uses_detail_count_terms_and_keeps_selection_at_wide_width() {
        let panel = TodoDagPanel::new(snapshot(), Vec::new());
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK)).unwrap();
        let buffer = terminal.backend().buffer();
        let wide = buffer.content.iter().map(|cell| cell.symbol()).collect::<String>();
        // The overview must use the same count terms as the detail page
        // (previously the ambiguous `✓N open N active …` compact form).
        assert!(wide.contains("active · ✓ 1 completed · 2 open · 1 active · 1 blocked"), "wide header must carry the detail count terms\n{wide}");
        assert!(!wide.contains("✓1 open"), "compact count form must be gone\n{wide}");
        // The default selection is the main DAG header; the highlight marker
        // must survive on the header line.
        assert!(wide.contains("› Main session [main]"), "selected header must keep its marker\n{wide}");
        let marked = buffer
            .content
            .iter()
            .filter(|cell| cell.symbol() == "›" && cell.style().bg == Some(crate::theme::DARK.selected_bg))
            .count();
        assert_eq!(marked, 1, "exactly the selected header carries the highlight marker");
    }

    #[test]
    fn overview_header_wraps_at_60_columns_without_cutting_words() {
        let panel = TodoDagPanel::new(snapshot(), Vec::new());
        let backend = TestBackend::new(60, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK)).unwrap();
        let buffer = terminal.backend().buffer();
        let narrow = buffer.content.iter().map(|cell| cell.symbol()).collect::<String>();
        // 60 columns forces the header (label + counts ≈ 80 chars) to wrap;
        // every count word must still render in full — no mid-word clipping
        // at the pane edge.
        for word in ["completed", "open", "active", "blocked"] {
            assert!(narrow.contains(word), "count word {word:?} must wrap in full at 60 columns\n{narrow}");
        }
        let header_rows = buffer
            .content
            .chunks(60)
            .skip(1)
            .take(2)
            .flat_map(|row| row[1..59].iter())
            .flat_map(|cell| cell.symbol().chars())
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        assert!(header_rows.contains("✓1completed·2open·1active·1blocked"), "all count terms must survive the wrap\n{narrow}");
    }

    #[test]
    fn scripted_planner_long_title_still_truncates_in_overview_rows() {
        // Display contract: a planner that ignores the concise-title prompt
        // requirement (a long imperative paragraph as task content) still
        // renders truncated. The fix is prompt-side only — the panel display
        // is intentionally unchanged, so the 60-char row bound still cuts
        // the title with an ellipsis and the full title never appears.
        let long_title = "Bootstrap the pi-zig source tree with a self-contained build script and vendor every single dependency".to_owned();
        assert!(long_title.chars().count() > 60, "fixture must exceed the 60-char concise-title bound");
        let mut dag = snapshot();
        dag.jobs.push(JobCardRows {
            job_id: "job-long".to_owned(), ordinal: 0, agent_id: "agent-long".to_owned(), agent: "task".to_owned(), display_name: "Verifier".to_owned(),
            todo_task_id: Some("blocked".to_owned()), job_status: JobStatus::Running, agent_status: None, summary: Some(long_title.clone()),
            progress: None,
            rows: vec![crate::job_card_adapter::JobCardRow { job_id: Some("job-long".to_owned()), role: crate::job_card_adapter::JobCardRowRole::Description, text: long_title.clone() }],
        });
        let panel = TodoDagPanel::new(dag, Vec::new());
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK)).unwrap();
        let overview = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(!overview.contains(&long_title), "long title must stay truncated in the panel\n{overview}");
        assert!(overview.contains('…'), "truncated title must carry the ellipsis\n{overview}");
        assert!(overview.contains("Verifier"), "job row must still render\n{overview}");
    }

    #[test]
    fn subagent_rows_navigate_with_arrows_and_jk_and_enter_opens_detail() {
        let mut dag = snapshot();
        dag.jobs.push(JobCardRows {
            job_id: "job-a".to_owned(), ordinal: 0, agent_id: "agent-a".to_owned(), agent: "task".to_owned(), display_name: "Verifier".to_owned(),
            todo_task_id: Some("blocked".to_owned()), job_status: JobStatus::Running, agent_status: None, summary: Some("Run focused verification".to_owned()),
            progress: None,
            rows: vec![crate::job_card_adapter::JobCardRow { job_id: Some("job-a".to_owned()), role: crate::job_card_adapter::JobCardRowRole::Description, text: "Run focused verification".to_owned() }],
        });
        let workflow = TodoDagSnapshot::workflow("wf-a".to_owned(), "Alpha workflow".to_owned(), Vec::new(), WorkflowStatus::Running, Vec::new());
        let mut panel = TodoDagPanel::new(dag, vec![workflow]);
        // Down from the main DAG header selects its first subagent row.
        panel.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(panel.selected_job(), Some(0));
        assert!(panel.selected_dag().is_some_and(|dag| dag.id == "main"));
        // j moves down past the job to the next DAG header, k returns.
        panel.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(panel.selected_job(), None);
        assert!(panel.selected_dag().is_some_and(|dag| dag.id == "wf-a"));
        panel.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(panel.selected_job(), Some(0));
        assert!(panel.selected_dag().is_some_and(|dag| dag.id == "main"));

        // Enter opens the subagent detail page.
        panel.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(panel.page(), TodoDagPanelPage::Subagent);
        // Esc returns to the overview, a second Esc closes.
        panel.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(panel.page(), TodoDagPanelPage::Overview);
        assert_eq!(panel.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), TodoDagPanelResult::Close);
    }

    #[test]
    fn subagent_detail_shows_identity_linked_todo_task_and_current_task() {
        let mut dag = snapshot();
        dag.jobs.push(JobCardRows {
            job_id: "job-a".to_owned(), ordinal: 0, agent_id: "agent-a".to_owned(), agent: "task".to_owned(), display_name: "Verifier".to_owned(),
            todo_task_id: Some("blocked".to_owned()), job_status: JobStatus::Running, agent_status: Some(AgentStatus::Parked), summary: None,
            progress: None,
            rows: vec![crate::job_card_adapter::JobCardRow { job_id: Some("job-a".to_owned()), role: crate::job_card_adapter::JobCardRowRole::Description, text: "Run focused verification".to_owned() }],
        });
        let mut panel = TodoDagPanel::new(dag, Vec::new());
        panel.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        panel.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(panel.page(), TodoDagPanelPage::Subagent);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK)).unwrap();
        let text = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(text.contains("Verifier"), "{text}");
        assert!(text.contains("task"), "{text}");
        assert!(text.contains("running · parked"), "{text}");
        assert!(text.contains("Verify [blocked]"), "{text}");
        assert!(text.contains("Run focused verification"), "{text}");
        assert!(text.contains("Main session [main]"), "{text}");
    }

    #[test]
    fn subagent_detail_shows_live_progress_event_log_and_transcript_hint() {
        let mut dag = snapshot();
        let progress = crate::job_card_adapter::JobCardProgress {
            stage: Some("running".to_owned()),
            activity: Some("read tools.rs".to_owned()),
            activity_at: Some(12_100),
            elapsed_ms: Some(12_000),
            events: vec![
                crate::job_card_adapter::JobEventLogEntry { at: 500, kind: crate::job_card_adapter::JobEventKind::Job, text: "started".to_owned() },
                crate::job_card_adapter::JobEventLogEntry { at: 12_100, kind: crate::job_card_adapter::JobEventKind::Message, text: "read tools.rs".to_owned() },
            ],
            history_ref: Some("history://agent-a".to_owned()),
            artifact_ref: None,
        };
        dag.jobs.push(JobCardRows {
            job_id: "job-a".to_owned(), ordinal: 0, agent_id: "agent-a".to_owned(), agent: "task".to_owned(), display_name: "Verifier".to_owned(),
            todo_task_id: None, job_status: JobStatus::Running, agent_status: None, summary: None,
            progress: Some(progress),
            rows: vec![
                crate::job_card_adapter::JobCardRow { job_id: Some("job-a".to_owned()), role: crate::job_card_adapter::JobCardRowRole::Description, text: "Run focused verification".to_owned() },
                crate::job_card_adapter::JobCardRow { job_id: Some("job-a".to_owned()), role: crate::job_card_adapter::JobCardRowRole::Progress, text: "read tools.rs · 12s".to_owned() },
            ],
        });
        let mut panel = TodoDagPanel::new(dag, Vec::new());
        panel.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        panel.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(panel.page(), TodoDagPanelPage::Subagent);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK)).unwrap();
        let text = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(text.contains("read tools.rs · 12s"), "{text}");
        assert!(text.contains("Activity log"), "{text}");
        assert!(text.contains("started"), "{text}");
        assert!(text.contains("00:00:12"), "{text}");
        assert!(text.contains("history://agent-a"), "{text}");
    }

    #[test]
    fn subagent_row_highlight_marks_the_selected_job_line() {
        let mut dag = snapshot();
        dag.jobs.push(JobCardRows {
            job_id: "job-a".to_owned(), ordinal: 0, agent_id: "agent-a".to_owned(), agent: "task".to_owned(), display_name: "Verifier".to_owned(),
            todo_task_id: Some("blocked".to_owned()), job_status: JobStatus::Queued, agent_status: None, summary: None,
            progress: None,
            rows: vec![crate::job_card_adapter::JobCardRow { job_id: Some("job-a".to_owned()), role: crate::job_card_adapter::JobCardRowRole::Description, text: String::new() }],
        });
        let mut panel = TodoDagPanel::new(dag, Vec::new());
        panel.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK)).unwrap();
        let buffer = terminal.backend().buffer();
        let marked = buffer.content.iter().enumerate().filter(|(_, cell)| cell.symbol() == "›" && cell.style().bg == Some(crate::theme::DARK.selected_bg)).count();
        assert_eq!(marked, 1, "exactly the selected subagent row carries the highlight marker");
        let text = buffer.content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(text.contains("› • Verifier"), "{text}");
    }

    #[test]
    fn stale_subagent_selection_drops_back_to_overview_after_job_update() {
        let mut dag = snapshot();
        dag.jobs.push(JobCardRows {
            job_id: "job-a".to_owned(), ordinal: 0, agent_id: "agent-a".to_owned(), agent: "task".to_owned(), display_name: "Verifier".to_owned(),
            todo_task_id: Some("blocked".to_owned()), job_status: JobStatus::Running, agent_status: None, summary: None,
            progress: None,
            rows: vec![crate::job_card_adapter::JobCardRow { job_id: Some("job-a".to_owned()), role: crate::job_card_adapter::JobCardRowRole::Description, text: "Run focused verification".to_owned() }],
        });
        let mut panel = TodoDagPanel::new(dag, Vec::new());
        panel.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        panel.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(panel.page(), TodoDagPanelPage::Subagent);
        // The job settles and disappears; the panel must not stay on a dead row.
        panel.update_main_jobs(Vec::new());
        assert_eq!(panel.page(), TodoDagPanelPage::Overview);
        assert_eq!(panel.selected_job(), None);
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
            progress: None,
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
