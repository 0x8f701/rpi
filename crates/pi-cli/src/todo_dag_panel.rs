//! Task-DAG subpage and detail rendering for the unified Todo TUI surface.

use std::cell::Cell;
use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use pi_coding::{
    AgentStatus, JobStatus, TodoDagExecutionStatus, TodoDagFailureReason, TodoItem, TodoPhase,
    TodoStatus, WorkflowRuntimeJobDetail, WorkflowStatus,
};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::job_card_adapter::{JobCardRow, JobCardRowRole, JobCardRows};
use crate::theme::Theme;
pub use crate::todo_dag_view::TodoDagCounts;
use crate::todo_dag_view::{
    redacted_sanitize, redacted_truncate, task_marker, todo_status, todo_style, wrap_styled_line,
};
use crate::tool_card_adapter::{
    TASK_SECTION_CHAR_LIMIT, TaskDelegationSections, bound_display_text,
};

/// Independent ordinary-Todo / ordinary-Activity / workflow-DAG page surfaces.
/// These are distinct page modes, not tabs inside one overlay: the TUI mounts
/// one surface at a time. Workflow stays on `WorkflowPanel`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TodoSurface {
    /// Ordinary Todo DAG page (main session todos). Independent of Activity.
    #[default]
    Todo,
    /// Ordinary Activity page (bounded job output). Independent of Todo.
    Activity,
    /// Combined DAG overview used by `/todo` when both main and workflow DAGs
    /// are listed. Jobs remain a row list inside this surface, never a third
    /// product page.
    Overview,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TodoDagPanelPage {
    Overview,
    Detail,
    /// Focused detail of one subagent job, opened from a navigable job row.
    Subagent,
}

/// Internal row list inside the Overview surface. Not a product page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TodoPanelTab {
    #[default]
    Todo,
    Jobs,
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

impl TodoDagSubagentJob {
    /// Recent human-readable output rows for the Activity section: the card's
    /// rendered rows, or the workflow job's activity lines.
    fn activity_rows(&self) -> Vec<String> {
        match self {
            TodoDagSubagentJob::Card(card) => {
                card.rows.iter().map(|row| row.text.clone()).collect()
            }
            // Workflow runtime jobs carry a task summary, not a rolling
            // output buffer; surface it as the job's activity line.
            TodoDagSubagentJob::Workflow(job) => job.task_summary.clone().into_iter().collect(),
        }
    }
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
                        .find(|row| {
                            matches!(
                                row.role,
                                crate::job_card_adapter::JobCardRowRole::Description
                            )
                        })
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
            Self::Main(TodoDagExecutionStatus::Failed) => "failed",
            Self::Workflow(status) => status.as_str(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct TodoDagSnapshot {
    pub id: String,
    /// Owning session identity. Combined with `id` this is the DAG key —
    /// the same workflow id in two sessions must not collide. Empty for
    /// unstamped ordinary Todo/Activity pages.
    pub session_id: String,
    /// Workflow generation used to reject stale incremental events. `0` for
    /// the ordinary main-session Todo/Activity surfaces.
    pub generation: u64,
    pub label: String,
    pub phases: Vec<TodoPhase>,
    pub execution: TodoDagExecutionLabel,
    pub failure_reason: Option<TodoDagFailureReason>,
    pub jobs: Vec<JobCardRows>,
    pub workflow_jobs: Vec<WorkflowRuntimeJobDetail>,
    /// Bounded delegation context the main Todo DAG detail renders as Goal /
    /// Target / Constraints / Contract sections. This stores the canonical
    /// [`TaskDelegationSections`] returned by
    /// `JobCardPresentationAdapter::task_context_sections`; no second wrapper
    /// or projection is introduced. Existing constructors default it empty so
    /// tui.rs keeps compiling until Main wires the additive builder below.
    delegation_context: TaskDelegationSections,
}

impl TodoDagSnapshot {
    #[must_use]
    pub fn main(
        phases: Vec<TodoPhase>,
        status: TodoDagExecutionStatus,
        failure_reason: Option<TodoDagFailureReason>,
        jobs: Vec<JobCardRows>,
    ) -> Self {
        Self {
            id: "main".to_owned(),
            session_id: String::new(),
            generation: 0,
            label: "Main session".to_owned(),
            phases,
            execution: TodoDagExecutionLabel::Main(status),
            failure_reason,
            jobs,
            workflow_jobs: Vec::new(),
            delegation_context: TaskDelegationSections::default(),
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
            session_id: String::new(),
            generation: 0,
            label,
            phases,
            execution: TodoDagExecutionLabel::Workflow(status),
            failure_reason: None,
            jobs: Vec::new(),
            workflow_jobs: jobs,
            delegation_context: TaskDelegationSections::default(),
        }
    }

    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = session_id.into();
        self
    }

    /// Attach the canonical bounded delegation context returned by
    /// `JobCardPresentationAdapter::task_context_sections()`. The borrowed
    /// parameter makes Main's cutover additive and direct:
    /// `TodoDagSnapshot::main(...).with_delegation_context(adapter.task_context_sections())`.
    /// Redaction runs before the shared section bound even if a non-adapter
    /// caller constructs `TaskDelegationSections` manually.
    #[must_use]
    pub fn with_delegation_context(mut self, context: &TaskDelegationSections) -> Self {
        self.delegation_context = bounded_delegation_context(context);
        self
    }

    #[must_use]
    pub fn identity_key(&self) -> String {
        crate::workflow_panel::workflow_identity_key(&self.session_id, &self.id)
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
    /// Internal Jobs row list on the Overview surface only.
    tab: TodoPanelTab,
    /// Independent product page: ordinary Todo, ordinary Activity, or the
    /// combined DAG overview. The TUI mounts one surface at a time.
    surface: TodoSurface,
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
        workflows.sort_by(|left, right| {
            left.label
                .cmp(&right.label)
                .then_with(|| left.identity_key().cmp(&right.identity_key()))
        });
        let mut dags = Vec::with_capacity(workflows.len() + 1);
        dags.push(main);
        dags.extend(workflows);
        Self {
            dags,
            selected: 0,
            selected_job: None,
            subagent: None,
            page: TodoDagPanelPage::Overview,
            detail_scroll: Cell::new(0),
            detail_width: Cell::new(1),
            detail_viewport_height: Cell::new(0),
            detail_display_rows: Cell::new(0),
            tab: TodoPanelTab::Todo,
            surface: TodoSurface::Overview,
        }
    }

    /// Ordinary Todo page: main-session DAG only. Independent of Activity
    /// and of the Workflow master-detail page.
    #[must_use]
    pub fn todo(main: TodoDagSnapshot) -> Self {
        let mut panel = Self::new(main, Vec::new());
        panel.surface = TodoSurface::Todo;
        panel
    }

    /// Ordinary Activity page: bounded job output only. Independent of Todo.
    #[must_use]
    pub fn activity(main: TodoDagSnapshot) -> Self {
        let mut panel = Self::new(main, Vec::new());
        panel.surface = TodoSurface::Activity;
        panel
    }

    #[must_use]
    pub fn surface(&self) -> TodoSurface {
        self.surface
    }

    pub fn replace(&mut self, main: TodoDagSnapshot, mut workflows: Vec<TodoDagSnapshot>) {
        let selected_key = self.selected_dag().map(|dag| dag.identity_key());
        workflows.sort_by(|left, right| {
            left.label
                .cmp(&right.label)
                .then_with(|| left.identity_key().cmp(&right.identity_key()))
        });
        self.dags.clear();
        self.dags.push(main);
        self.dags.extend(workflows);
        self.selected = selected_key
            .and_then(|key| self.dags.iter().position(|dag| dag.identity_key() == key))
            .unwrap_or(0)
            .min(self.dags.len().saturating_sub(1));
        self.clamp_selection();
    }

    pub fn update_main(
        &mut self,
        phases: Vec<TodoPhase>,
        status: TodoDagExecutionStatus,
        failure_reason: Option<TodoDagFailureReason>,
        jobs: Vec<JobCardRows>,
    ) {
        if let Some(main) = self.dags.first_mut() {
            main.phases = phases;
            main.execution = TodoDagExecutionLabel::Main(status);
            main.failure_reason = failure_reason;
            main.jobs = jobs;
        }
        self.clamp_selection();
    }

    pub fn update_main_phases(&mut self, phases: Vec<TodoPhase>) {
        if let Some(main) = self.dags.first_mut() {
            main.phases = phases;
        }
        self.clamp_selection();
    }

    pub fn update_main_state(
        &mut self,
        phases: Vec<TodoPhase>,
        status: TodoDagExecutionStatus,
        failure_reason: Option<TodoDagFailureReason>,
    ) {
        if let Some(main) = self.dags.first_mut() {
            main.phases = phases;
            main.execution = TodoDagExecutionLabel::Main(status);
            main.failure_reason = failure_reason;
        }
        self.clamp_selection();
    }

    pub fn update_main_jobs(&mut self, jobs: Vec<JobCardRows>) {
        if let Some(main) = self.dags.first_mut() {
            main.jobs = jobs;
        }
        self.clamp_selection();
    }

    /// Refresh the main snapshot's canonical delegation context without
    /// changing any existing `update_main*` signatures. Main should call this
    /// after job-projection refreshes so an already-open Todo detail does not
    /// retain stale Goal/Target/Constraints/Contract bodies.
    pub fn update_main_delegation_context(&mut self, context: &TaskDelegationSections) {
        if let Some(main) = self.dags.first_mut() {
            main.delegation_context = bounded_delegation_context(context);
        }
        self.clamp_selection();
    }

    pub fn update_workflows(&mut self, workflows: Vec<TodoDagSnapshot>) {
        let main = self.dags.first().cloned().unwrap_or_else(|| {
            TodoDagSnapshot::main(
                Vec::new(),
                TodoDagExecutionStatus::Dormant,
                None,
                Vec::new(),
            )
        });
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
    pub fn page(&self) -> TodoDagPanelPage {
        self.page
    }

    #[must_use]
    pub fn selected_dag(&self) -> Option<&TodoDagSnapshot> {
        self.dags.get(self.selected)
    }

    /// Selected subagent row inside the selected DAG (`None` = DAG header).
    #[must_use]
    pub fn selected_job(&self) -> Option<usize> {
        self.selected_job
    }

    #[must_use]
    pub fn detail_scroll(&self) -> usize {
        self.detail_scroll.get()
    }

    fn detail_scroll_max(&self) -> usize {
        self.detail_display_rows
            .get()
            .saturating_sub(self.detail_viewport_height.get())
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
        self.detail_scroll
            .set(self.detail_scroll.get().min(self.detail_scroll_max()));
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
        self.dags
            .iter()
            .map(|dag| 1 + dag.subagent_job_count())
            .sum()
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

    fn clamp_detail_scroll(&self) {
        self.refresh_detail_geometry();
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> TodoDagPanelResult {
        if key.kind != KeyEventKind::Press {
            return TodoDagPanelResult::Handled;
        }
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
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
                        self.detail_scroll
                            .set(self.detail_scroll.get().saturating_sub(1));
                    }
                    _ => {}
                }
                TodoDagPanelResult::Handled
            }
            KeyCode::Down | KeyCode::Char('j') => {
                match (self.page, key.code) {
                    (TodoDagPanelPage::Overview, _) => self.move_overview(1),
                    (TodoDagPanelPage::Detail | TodoDagPanelPage::Subagent, _) => {
                        self.detail_scroll.set(
                            self.detail_scroll
                                .get()
                                .saturating_add(1)
                                .min(self.detail_scroll_max()),
                        );
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
            KeyCode::Tab | KeyCode::BackTab
                if self.page == TodoDagPanelPage::Overview
                    && self.surface == TodoSurface::Overview =>
            {
                // Overview-only Jobs row list. Ordinary Todo and Activity are
                // independent pages, not cycled from here.
                self.tab = match self.tab {
                    TodoPanelTab::Todo => TodoPanelTab::Jobs,
                    TodoPanelTab::Jobs => TodoPanelTab::Todo,
                };
                self.selected_job = None;
                TodoDagPanelResult::Handled
            }
            _ => TodoDagPanelResult::Unknown,
        }
    }
}

pub fn render_todo_dag_panel(frame: &mut ratatui::Frame<'_>, panel: &TodoDagPanel, theme: Theme) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    let title = match (panel.surface, panel.page) {
        (TodoSurface::Todo, _) => " Todo · Tasks · Tab Child sessions ",
        (TodoSurface::Activity, _) => " Activity ",
        (TodoSurface::Overview, TodoDagPanelPage::Overview) => " Todo DAGs ",
        (TodoSurface::Overview, TodoDagPanelPage::Detail) => " Todo DAG detail ",
        (TodoSurface::Overview, TodoDagPanelPage::Subagent) => " Subagent detail ",
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_accent));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    match (panel.surface, panel.page) {
        (TodoSurface::Activity, _) => render_activity_tab(frame, panel, inner, theme),
        (TodoSurface::Todo, TodoDagPanelPage::Overview) => {
            render_todo_tab(frame, panel, inner, theme)
        }
        (TodoSurface::Todo, TodoDagPanelPage::Detail | TodoDagPanelPage::Subagent)
        | (TodoSurface::Overview, TodoDagPanelPage::Detail | TodoDagPanelPage::Subagent) => {
            render_detail(frame, panel, inner, theme)
        }
        (TodoSurface::Overview, TodoDagPanelPage::Overview) => {
            render_overview(frame, panel, inner, theme)
        }
    }
}

fn render_overview(frame: &mut ratatui::Frame<'_>, panel: &TodoDagPanel, area: Rect, theme: Theme) {
    let tabs_height = 1.min(area.height);
    let tabs = Rect {
        height: tabs_height,
        ..area
    };
    let body = Rect {
        y: area.y.saturating_add(tabs_height),
        height: area.height.saturating_sub(tabs_height),
        ..area
    };
    frame.render_widget(Paragraph::new(todo_tab_strip_line(panel.tab, theme)), tabs);
    match panel.tab {
        TodoPanelTab::Todo => render_todo_tab(frame, panel, body, theme),
        TodoPanelTab::Jobs => render_jobs_tab(frame, panel, body, theme),
    }
}

/// Overview strip: `Todo / Jobs`. Activity is an independent page.
fn todo_tab_strip_line(tab: TodoPanelTab, theme: Theme) -> Line<'static> {
    let sections = [(TodoPanelTab::Todo, "Todo"), (TodoPanelTab::Jobs, "Jobs")];
    let mut spans = Vec::with_capacity(sections.len() * 2);
    for (section, label) in sections {
        let style = if section == tab {
            Style::default()
                .fg(theme.text)
                .add_modifier(Modifier::BOLD)
                .bg(theme.selected_bg)
        } else {
            Style::default().fg(theme.dim)
        };
        spans.push(Span::styled(format!(" {label} "), style));
        spans.push(Span::styled("│", Style::default().fg(theme.border)));
    }
    Line::from(spans)
}

/// Jobs section: every live subagent job across DAGs as selectable rows
/// (Enter opens the same subagent detail page as the Todo rows).
fn render_jobs_tab(frame: &mut ratatui::Frame<'_>, panel: &TodoDagPanel, area: Rect, theme: Theme) {
    let footer_height = 2.min(area.height);
    let rows = Rect {
        height: area.height.saturating_sub(footer_height),
        ..area
    };
    let footer = Rect {
        y: area.y.saturating_add(rows.height),
        height: footer_height,
        ..area
    };
    let viewport = usize::from(rows.height).max(1);
    let width = usize::from(area.width.max(1));
    let mut lines = Vec::new();
    let mut cursor_line = 0usize;
    for (index, dag) in panel.dags.iter().enumerate() {
        for job_index in 0..dag.subagent_job_count() {
            let Some(job) = dag.subagent_job(job_index) else {
                continue;
            };
            let selected = index == panel.selected && panel.selected_job == Some(job_index);
            if selected {
                cursor_line = lines.len();
            }
            lines.push(subagent_row_line(&job, theme, selected));
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            " No running subagent jobs",
            Style::default().fg(theme.dim),
        )));
    }
    let total = lines.len();
    let start = cursor_line
        .saturating_add(1)
        .saturating_sub(viewport)
        .min(total.saturating_sub(viewport));
    frame.render_widget(
        Paragraph::new(
            lines
                .into_iter()
                .skip(start)
                .take(viewport)
                .collect::<Vec<_>>(),
        ),
        rows,
    );
    frame.render_widget(
        Paragraph::new("↑/↓/j/k select job · Enter details · Tab back to Todo · Esc/q close")
            .style(Style::default().fg(theme.dim)),
        footer,
    );
}

/// Activity section: the most recent bounded output rows aggregated across
/// all jobs, newest DAG first. Capped so a chatty child cannot flood the pane.
const ACTIVITY_MAX_LINES: usize = 200;

fn render_activity_tab(
    frame: &mut ratatui::Frame<'_>,
    panel: &TodoDagPanel,
    area: Rect,
    theme: Theme,
) {
    let rows = Rect {
        height: area.height,
        ..area
    };
    let viewport = usize::from(rows.height).max(1);
    let width = usize::from(area.width.max(1));
    let mut lines = Vec::new();
    for dag in &panel.dags {
        for job_index in 0..dag.subagent_job_count() {
            let Some(job) = dag.subagent_job(job_index) else {
                continue;
            };
            let identity = subagent_identity(&job);
            for row in job.activity_rows().iter().rev().take(20) {
                let text = redacted_sanitize(row);
                if text.trim().is_empty() {
                    continue;
                }
                lines.extend(wrap_styled_line_words(
                    Line::from(vec![
                        Span::styled(
                            format!(" {:<14}", redacted_truncate(&identity.display_name, 14)),
                            Style::default().fg(theme.dim),
                        ),
                        Span::styled(text, Style::default().fg(theme.text)),
                    ]),
                    width,
                ));
                if lines.len() >= ACTIVITY_MAX_LINES {
                    break;
                }
            }
            if lines.len() >= ACTIVITY_MAX_LINES {
                break;
            }
        }
        if lines.len() >= ACTIVITY_MAX_LINES {
            break;
        }
    }
    let body = if lines.is_empty() {
        vec![Line::from(Span::styled(
            " No subagent activity yet",
            Style::default().fg(theme.dim),
        ))]
    } else {
        lines.into_iter().take(viewport).collect::<Vec<_>>()
    };
    frame.render_widget(Paragraph::new(body), rows);
}

fn render_todo_tab(frame: &mut ratatui::Frame<'_>, panel: &TodoDagPanel, area: Rect, theme: Theme) {
    let footer_height = 2.min(area.height);
    let rows_height = area.height.saturating_sub(footer_height);
    let rows = Rect {
        height: rows_height,
        ..area
    };
    let footer = Rect {
        y: area.y.saturating_add(rows_height),
        height: footer_height,
        ..area
    };
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
        lines.extend(wrap_styled_line_words(
            overview_header_line(dag, theme, header_selected),
            width,
        ));
        if header_selected {
            cursor_line = header_start;
        }
        for phase in &dag.phases {
            for task in &phase.tasks {
                if task.status == TodoStatus::Abandoned {
                    continue;
                }
                lines.extend(wrap_styled_line_words(
                    todo_task_overview_line(task, theme),
                    width,
                ));
            }
        }
        for job_index in 0..dag.subagent_job_count() {
            let Some(job) = dag.subagent_job(job_index) else {
                continue;
            };
            let job_selected = selected && panel.selected_job == Some(job_index);
            if job_selected {
                cursor_line = lines.len();
            }
            lines.push(subagent_row_line(&job, theme, job_selected));
        }
    }
    let total = lines.len();
    let start = cursor_line
        .saturating_add(1)
        .saturating_sub(viewport)
        .min(total.saturating_sub(viewport));
    let visible = lines
        .into_iter()
        .skip(start)
        .take(viewport)
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(visible), rows);
    frame.render_widget(
        Paragraph::new(
            "↑/↓/j/k select subagent · Enter details · Tab Child sessions · Esc/q close",
        )
        .style(Style::default().fg(theme.dim)),
        footer,
    );
}

fn todo_task_overview_line(task: &TodoItem, theme: Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("   {} ", task_marker(task.status)),
            todo_style(task.status, theme),
        ),
        Span::styled(
            redacted_sanitize(&task.content),
            Style::default().fg(theme.text),
        ),
        Span::styled(
            format!(" · {}", todo_status(task.status)),
            Style::default().fg(theme.dim),
        ),
    ])
}

/// One DAG header line: selection marker, label, id, execution label, and the
/// same `✓ N completed · O open · A active · W waiting` count terms as the
/// detail page. Waiting is derived from unsatisfied dependencies; it is not an
/// execution or task status. The count span patches the header's selection
/// style so the highlight survives wrapping.
fn overview_header_line(dag: &TodoDagSnapshot, theme: Theme, selected: bool) -> Line<'static> {
    let counts = dag.counts();
    let style = if selected {
        Style::default()
            .fg(theme.text)
            .bg(theme.selected_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text)
    };
    Line::from(vec![
        Span::styled(format!(" {} ", if selected { '›' } else { ' ' }), style),
        Span::styled(format!("{:<24}", redacted_truncate(&dag.label, 24)), style),
        Span::styled(
            format!(" [{}] ", redacted_truncate(&dag.id, 18)),
            style.patch(Style::default().fg(theme.dim)),
        ),
        Span::styled(
            format!(
                "{} · ✓ {} completed · {} open · {} active · {} waiting",
                dag.execution.label(),
                counts.completed,
                counts.open,
                counts.active,
                counts.waiting
            ),
            style.patch(status_style(&dag.execution, theme)),
        ),
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
        Style::default()
            .fg(theme.text)
            .bg(theme.selected_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text)
    };
    let mut spans = vec![
        Span::styled(format!(" {} ", if selected { '›' } else { ' ' }), style),
        Span::styled(
            "• ",
            Style::default().fg(if selected { theme.accent } else { theme.dim }),
        ),
        Span::styled(
            redacted_sanitize(&identity.display_name),
            style.patch(Style::default().fg(theme.accent)),
        ),
    ];
    if let Some(agent_type) = &identity.agent_type {
        spans.push(Span::styled(
            format!(" ({})", redacted_sanitize(agent_type)),
            style.patch(Style::default().fg(theme.dim)),
        ));
    }
    spans.push(Span::styled(
        format!(" · {}", identity.status),
        style.patch(Style::default().fg(job_color(identity.status_kind, theme))),
    ));
    let task = redacted_truncate(&identity.task_summary, 60);
    if !task.is_empty() {
        spans.push(Span::styled(
            format!(" · {task}"),
            style.patch(Style::default().fg(theme.dim)),
        ));
    }
    Line::from(spans)
}

fn render_detail(frame: &mut ratatui::Frame<'_>, panel: &TodoDagPanel, area: Rect, theme: Theme) {
    let footer_height = 1.min(area.height);
    let content_height = area.height.saturating_sub(footer_height);
    let content = Rect {
        height: content_height,
        ..area
    };
    let footer = Rect {
        y: area.y.saturating_add(content_height),
        height: footer_height,
        ..area
    };
    let content_width = usize::from(content.width.max(1));
    panel.detail_width.set(content_width);
    panel
        .detail_viewport_height
        .set(usize::from(content_height));
    let lines = panel
        .detail_source_lines(theme)
        .into_iter()
        .flat_map(|line| wrap_styled_line(line, content_width.max(1)))
        .collect::<Vec<_>>();
    panel.detail_display_rows.set(lines.len());
    panel.clamp_detail_scroll();
    let max_scroll = panel.detail_scroll_max();
    let scroll = panel.detail_scroll.get();
    let visible = lines
        .into_iter()
        .skip(scroll)
        .take(usize::from(content_height))
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(visible), content);
    frame.render_widget(
        Paragraph::new(format!(
            "↑/↓ scroll · {}/{} · Esc DAGs · q close",
            scroll.saturating_add(1),
            max_scroll.saturating_add(1)
        ))
        .style(Style::default().fg(theme.dim)),
        footer,
    );
}

/// Focused subagent detail page: identity, type, status, owning DAG, the
/// linked todo task content, and the current task summary.
fn subagent_detail_lines(
    dag: &TodoDagSnapshot,
    job: &TodoDagSubagentJob,
    theme: Theme,
) -> Vec<Line<'static>> {
    let identity = subagent_identity(job);
    let mut lines = vec![
        Line::from(Span::styled(
            redacted_sanitize(&identity.display_name),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![
            Span::styled("Type        ", Style::default().fg(theme.dim)),
            Span::styled(
                redacted_sanitize(identity.agent_type.as_deref().unwrap_or("—")),
                Style::default().fg(theme.text),
            ),
        ]),
        Line::from(vec![
            Span::styled("Status      ", Style::default().fg(theme.dim)),
            Span::styled(
                identity.status,
                Style::default().fg(job_color(identity.status_kind, theme)),
            ),
        ]),
        Line::from(vec![
            Span::styled("DAG         ", Style::default().fg(theme.dim)),
            Span::styled(
                format!(
                    "{} [{}]",
                    redacted_sanitize(&dag.label),
                    redacted_sanitize(&dag.id)
                ),
                Style::default().fg(theme.text),
            ),
        ]),
    ];
    let todo_task = identity.todo_task_id.as_deref().and_then(|id| {
        dag.phases
            .iter()
            .flat_map(|phase| &phase.tasks)
            .find(|task| task.id == id)
    });
    match todo_task {
        Some(task) => lines.push(Line::from(vec![
            Span::styled("Todo task   ", Style::default().fg(theme.dim)),
            Span::styled(
                format!(
                    "{} [{}]",
                    redacted_sanitize(&task.content),
                    redacted_sanitize(&task.id)
                ),
                Style::default().fg(theme.text),
            ),
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
            Span::styled(
                redacted_sanitize(&identity.task_summary),
                Style::default().fg(theme.text),
            ),
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
    let Some(progress) = &card.progress else {
        return;
    };
    let progress_text = card
        .rows
        .iter()
        .find(|row| row.role == crate::job_card_adapter::JobCardRowRole::Progress)
        .map(|row| row.text.as_str())
        .unwrap_or_else(|| progress.stage.as_deref().unwrap_or("running"));
    lines.push(Line::from(vec![
        Span::styled("Progress    ", Style::default().fg(theme.dim)),
        Span::styled(
            redacted_sanitize(progress_text),
            Style::default().fg(job_color(card.job_status, theme)),
        ),
    ]));
    if !progress.events.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "Activity log",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )));
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
                Span::styled(
                    redacted_sanitize(&entry.text),
                    Style::default().fg(theme.text),
                ),
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
            Span::styled(redacted_sanitize(history), Style::default().fg(theme.muted)),
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

fn bounded_delegation_section(value: &str) -> String {
    bound_display_text(&redacted_sanitize(value), TASK_SECTION_CHAR_LIMIT)
}

fn bounded_delegation_context(context: &TaskDelegationSections) -> TaskDelegationSections {
    TaskDelegationSections {
        goal: bounded_delegation_section(&context.goal),
        target: bounded_delegation_section(&context.target),
        constraints: bounded_delegation_section(&context.constraints),
        contract: bounded_delegation_section(&context.contract),
    }
}

fn detail_lines(dag: &TodoDagSnapshot, theme: Theme) -> Vec<Line<'static>> {
    let counts = dag.counts();
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                redacted_sanitize(&dag.label),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  [{}]", redacted_sanitize(&dag.id)),
                Style::default().fg(theme.dim),
            ),
        ]),
        Line::from(vec![
            Span::styled("Execution  ", Style::default().fg(theme.dim)),
            Span::styled(
                dag.execution.label(),
                status_style(&dag.execution, theme).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            format!(
                "✓ {} completed · {} open · {} active · {} waiting",
                counts.completed, counts.open, counts.active, counts.waiting
            ),
            Style::default().fg(theme.muted),
        )),
        Line::default(),
    ];
    if let Some(reason) = &dag.failure_reason {
        lines.push(Line::from(Span::styled(
            format!("Failure  {}", redacted_sanitize(reason.message())),
            Style::default().fg(theme.error),
        )));
        lines.push(Line::default());
    }

    let delegation_sections = [
        ("Goal", dag.delegation_context.goal.as_str()),
        ("Target", dag.delegation_context.target.as_str()),
        ("Constraints", dag.delegation_context.constraints.as_str()),
        ("Contract", dag.delegation_context.contract.as_str()),
    ];
    if delegation_sections
        .iter()
        .any(|(_, body)| !body.trim().is_empty())
    {
        lines.push(Line::from(Span::styled(
            "Delegation context",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )));
        for (label, body) in delegation_sections {
            if body.trim().is_empty() {
                continue;
            }
            lines.push(Line::from(vec![
                Span::styled(format!("  {label}: "), Style::default().fg(theme.dim)),
                Span::styled(
                    bounded_delegation_section(body),
                    Style::default().fg(theme.text),
                ),
            ]));
        }
        lines.push(Line::default());
    }

    let names = dag
        .phases
        .iter()
        .flat_map(|phase| &phase.tasks)
        .map(|task| (task.id.as_str(), redacted_sanitize(&task.content)))
        .collect::<HashMap<_, _>>();
    if dag.phases.is_empty() {
        lines.push(Line::from(Span::styled(
            "No phases or tasks yet",
            Style::default().fg(theme.muted),
        )));
    }
    for phase in &dag.phases {
        lines.push(Line::from(Span::styled(
            redacted_sanitize(&phase.name),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )));
        if phase.tasks.is_empty() {
            lines.push(Line::from(Span::styled(
                "  No tasks",
                Style::default().fg(theme.muted),
            )));
        }
        for task in &phase.tasks {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {} ", task_marker(task.status)),
                    todo_style(task.status, theme),
                ),
                Span::styled(
                    redacted_sanitize(&task.content),
                    Style::default().fg(theme.text),
                ),
                Span::styled(
                    format!(
                        "  [{}] · {}",
                        redacted_sanitize(&task.id),
                        todo_status(task.status)
                    ),
                    Style::default().fg(theme.dim),
                ),
            ]));
            let dependencies = task
                .depends_on
                .iter()
                .map(|id| {
                    names
                        .get(id.as_str())
                        .cloned()
                        .unwrap_or_else(|| redacted_sanitize(id))
                })
                .collect::<Vec<_>>();
            if !dependencies.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("      depends_on: {}", dependencies.join(", ")),
                    Style::default().fg(theme.dim),
                )));
            }
            let waiting = task
                .waiting_on
                .iter()
                .map(|dependency| {
                    format!(
                        "{} ({})",
                        redacted_sanitize(&dependency.content),
                        todo_status(dependency.status)
                    )
                })
                .collect::<Vec<_>>();
            if !waiting.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("      waiting_on: {}", waiting.join(", ")),
                    Style::default().fg(theme.dim),
                )));
            }
            push_linked_jobs(&mut lines, dag, &task.id, theme);
        }
        lines.push(Line::default());
    }
    lines
}

fn push_linked_jobs(
    lines: &mut Vec<Line<'static>>,
    dag: &TodoDagSnapshot,
    task_id: &str,
    theme: Theme,
) {
    for card in dag
        .jobs
        .iter()
        .filter(|card| card.todo_task_id.as_deref() == Some(task_id))
    {
        let summary = card
            .rows
            .iter()
            .find(|row| {
                matches!(
                    row.role,
                    crate::job_card_adapter::JobCardRowRole::Description
                )
            })
            .map_or("", |row| row.text.as_str());
        let suffix = if summary.is_empty() {
            String::new()
        } else {
            format!(" · {}", redacted_sanitize(summary))
        };
        let agent_type = if card.agent.trim().is_empty() {
            String::new()
        } else {
            format!(" ({})", redacted_sanitize(&card.agent))
        };
        lines.push(Line::from(Span::styled(
            format!(
                "      • {}{agent_type} · {}{suffix}",
                redacted_sanitize(&card.display_name),
                job_status(card.job_status)
            ),
            Style::default().fg(job_color(card.job_status, theme)),
        )));
    }
    for job in dag
        .workflow_jobs
        .iter()
        .filter(|job| job.todo_task_id.as_deref() == Some(task_id))
    {
        let suffix = job
            .task_summary
            .as_deref()
            .map_or(String::new(), |summary| {
                format!(" · {}", redacted_sanitize(summary))
            });
        lines.push(Line::from(Span::styled(
            format!(
                "      • {} · {}{suffix}",
                redacted_sanitize(&job.display_name),
                job_status(job.status)
            ),
            Style::default().fg(job_color(job.status, theme)),
        )));
    }
}

fn status_style(status: &TodoDagExecutionLabel, theme: Theme) -> Style {
    let color = match status {
        TodoDagExecutionLabel::Main(TodoDagExecutionStatus::Active)
        | TodoDagExecutionLabel::Workflow(WorkflowStatus::Running) => theme.accent,
        TodoDagExecutionLabel::Main(TodoDagExecutionStatus::Settled)
        | TodoDagExecutionLabel::Workflow(WorkflowStatus::Completed) => theme.success,
        TodoDagExecutionLabel::Main(TodoDagExecutionStatus::Failed)
        | TodoDagExecutionLabel::Workflow(WorkflowStatus::Failed | WorkflowStatus::Conflicted) => {
            theme.error
        }
        TodoDagExecutionLabel::Workflow(WorkflowStatus::Planning | WorkflowStatus::Integrating) => {
            theme.warning
        }
        _ => theme.muted,
    };
    Style::default().fg(color)
}

fn job_status(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Queued => "queued",
        JobStatus::Running => "running",
        JobStatus::Completed => "completed",
        JobStatus::Failed => "failed",
        JobStatus::Cancelled => "cancelled",
    }
}
fn job_color(status: JobStatus, theme: Theme) -> ratatui::style::Color {
    match status {
        JobStatus::Queued => theme.muted,
        JobStatus::Running => theme.accent,
        JobStatus::Completed => theme.success,
        JobStatus::Failed => theme.error,
        JobStatus::Cancelled => theme.dim,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_coding::{TodoItem, TodoWaitingOn};
    use ratatui::{Terminal, backend::TestBackend};

    fn task(
        id: &str,
        content: &str,
        status: TodoStatus,
        ready: bool,
        waiting_on: &[&str],
    ) -> TodoItem {
        TodoItem {
            id: id.to_owned(),
            content: content.to_owned(),
            status,
            depends_on: waiting_on.iter().map(|id| (*id).to_owned()).collect(),
            ready,
            waiting_on: waiting_on
                .iter()
                .map(|id| TodoWaitingOn {
                    task_id: (*id).to_owned(),
                    content: format!("dependency {id}"),
                    status: TodoStatus::InProgress,
                })
                .collect(),
            agent: None,
        }
    }

    fn snapshot() -> TodoDagSnapshot {
        TodoDagSnapshot::main(
            vec![TodoPhase {
                name: "Build".to_owned(),
                tasks: vec![
                    task("done", "Plan", TodoStatus::Completed, false, &[]),
                    task("active", "Implement", TodoStatus::InProgress, true, &[]),
                    task("waiting", "Verify", TodoStatus::Pending, false, &["active"]),
                ],
            }],
            TodoDagExecutionStatus::Active,
            None,
            Vec::new(),
        )
    }

    /// The `sk-…` shape is the first whole-match pattern in the shared
    /// redaction set; one sentinel exercises every DAG/detail field path.
    const SECRET_SENTINEL: &str = "sk-abcdef1234567890aabbccdd";

    fn rendered_lines(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn overview_tab_toggles_todo_and_jobs_not_activity() {
        let mut panel = TodoDagPanel::new(snapshot(), Vec::new());
        assert_eq!(panel.surface(), TodoSurface::Overview);
        assert_eq!(panel.tab, TodoPanelTab::Todo);
        panel.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(panel.tab, TodoPanelTab::Jobs);
        panel.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(panel.tab, TodoPanelTab::Todo);
        panel.selected_job = Some(0);
        panel.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
        assert_eq!(panel.selected_job, None);
        panel.tab = TodoPanelTab::Todo;
        panel.page = TodoDagPanelPage::Detail;
        assert_eq!(
            panel.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            TodoDagPanelResult::Unknown
        );
    }

    #[test]
    fn ordinary_todo_and_activity_are_independent_surfaces() {
        let todo = TodoDagPanel::todo(snapshot());
        let activity = TodoDagPanel::activity(snapshot());
        assert_eq!(todo.surface(), TodoSurface::Todo);
        assert_eq!(activity.surface(), TodoSurface::Activity);
        assert_ne!(todo.surface(), activity.surface());
    }

    #[test]
    fn ordinary_todo_title_exposes_child_sessions_navigation() {
        let panel = TodoDagPanel::todo(snapshot());
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK))
            .unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("Todo · Tasks"), "{screen}");
        assert!(screen.contains("Tab Child sessions"), "{screen}");
    }

    #[test]
    fn activity_surface_renders_job_rows_bounded_and_empty_state() {
        let mut dag = snapshot();
        dag.jobs = vec![JobCardRows {
            job_id: "job-1".to_owned(),
            ordinal: 1,
            agent_id: "researcher".to_owned(),
            agent: "researcher".to_owned(),
            display_name: "Researcher".to_owned(),
            todo_task_id: None,
            job_status: pi_coding::JobStatus::Running,
            agent_status: None,
            summary: None,
            rows: (0..30)
                .map(|index| JobCardRow {
                    job_id: None,
                    role: JobCardRowRole::Aggregate,
                    text: format!("line {index}"),
                })
                .collect(),
            progress: None,
        }];
        let panel = TodoDagPanel::activity(dag);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK))
            .unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("Activity"), "{screen}");
        assert!(
            screen.contains("Researcher"),
            "job attribution missing: {screen}"
        );
        assert!(screen.contains("line"), "output rows missing: {screen}");

        let empty_panel = TodoDagPanel::activity(snapshot());
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_todo_dag_panel(frame, &empty_panel, crate::theme::DARK))
            .unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("No subagent activity yet"), "{screen}");
    }

    #[test]
    fn jobs_tab_lists_jobs_and_enter_opens_subagent_detail() {
        let mut dag = snapshot();
        dag.jobs = vec![JobCardRows {
            job_id: "job-9".to_owned(),
            ordinal: 1,
            agent_id: "writer".to_owned(),
            agent: "writer".to_owned(),
            display_name: "Writer".to_owned(),
            todo_task_id: None,
            job_status: pi_coding::JobStatus::Queued,
            agent_status: None,
            summary: None,
            rows: Vec::new(),
            progress: None,
        }];
        let mut panel = TodoDagPanel::new(dag, Vec::new());
        panel.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK))
            .unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("Jobs"), "{screen}");
        assert!(screen.contains("Writer"), "{screen}");
        // Enter on the selected job row opens the subagent detail page.
        panel.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        panel.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(panel.page(), TodoDagPanelPage::Subagent);
    }

    #[test]
    fn counts_statuses_deterministically() {
        assert_eq!(
            snapshot().counts(),
            TodoDagCounts {
                completed: 1,
                open: 2,
                active: 1,
                waiting: 1
            }
        );
    }

    #[test]
    fn navigation_enters_detail_and_escape_steps_back_then_closes() {
        let workflow = TodoDagSnapshot::workflow(
            "wf-b".to_owned(),
            "Beta".to_owned(),
            Vec::new(),
            WorkflowStatus::Running,
            Vec::new(),
        );
        let mut panel = TodoDagPanel::new(snapshot(), vec![workflow]);
        assert_eq!(
            panel.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            TodoDagPanelResult::Handled
        );
        assert_eq!(
            panel.selected_dag().map(|dag| dag.id.as_str()),
            Some("wf-b")
        );
        panel.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(panel.page(), TodoDagPanelPage::Detail);
        assert_eq!(
            panel.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            TodoDagPanelResult::Handled
        );
        assert_eq!(panel.page(), TodoDagPanelPage::Overview);
        assert_eq!(
            panel.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            TodoDagPanelResult::Close
        );
    }

    #[test]
    fn render_distinguishes_dags_and_shows_dependency_waiting() {
        let workflow = TodoDagSnapshot::workflow(
            "wf-b".to_owned(),
            "Beta workflow".to_owned(),
            Vec::new(),
            WorkflowStatus::Running,
            Vec::new(),
        );
        let mut panel = TodoDagPanel::new(snapshot(), vec![workflow]);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK))
            .unwrap();
        let overview = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(overview.contains("Main session"));
        assert!(overview.contains("Beta workflow"));
        panel.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        terminal
            .draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK))
            .unwrap();
        let detail = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(detail.contains("Build"));
        assert!(detail.contains("Verify"));
        assert!(detail.contains("waiting_on: dependency active (in progress)"));
    }

    #[test]
    fn detail_renders_linked_job_card_state() {
        let mut dag = snapshot();
        dag.jobs.push(JobCardRows {
            job_id: "opaque-job".to_owned(),
            ordinal: 0,
            agent_id: "opaque-agent".to_owned(),
            agent: "task".to_owned(),
            display_name: "Verifier".to_owned(),
            todo_task_id: Some("waiting".to_owned()),
            job_status: JobStatus::Running,
            agent_status: None,
            summary: None,
            progress: None,
            rows: vec![crate::job_card_adapter::JobCardRow {
                job_id: Some("opaque-job".to_owned()),
                role: crate::job_card_adapter::JobCardRowRole::Description,
                text: "Run focused verification".to_owned(),
            }],
        });
        let mut panel = TodoDagPanel::new(dag, Vec::new());
        panel.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK))
            .unwrap();
        let detail = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(detail.contains("• Verifier (task) · running · Run focused verification"));
        assert!(!detail.contains("opaque-job"));
    }

    #[test]
    fn overview_shows_subagent_rows_with_name_status_and_task() {
        let mut dag = snapshot();
        dag.jobs.push(JobCardRows {
            job_id: "job-a".to_owned(),
            ordinal: 0,
            agent_id: "agent-a".to_owned(),
            agent: "task".to_owned(),
            display_name: "Verifier".to_owned(),
            todo_task_id: Some("waiting".to_owned()),
            job_status: JobStatus::Running,
            agent_status: None,
            summary: None,
            progress: None,
            rows: vec![crate::job_card_adapter::JobCardRow {
                job_id: Some("job-a".to_owned()),
                role: crate::job_card_adapter::JobCardRowRole::Description,
                text: "Run focused verification".to_owned(),
            }],
        });
        let workflow = TodoDagSnapshot::workflow(
            "wf-a".to_owned(),
            "Alpha workflow".to_owned(),
            Vec::new(),
            WorkflowStatus::Running,
            vec![WorkflowRuntimeJobDetail {
                display_name: "Alpha Worker".to_owned(),
                job_id: "job-a".to_owned(),
                role: "task".to_owned(),
                status: JobStatus::Queued,
                task_summary: Some("Assemble the release".to_owned()),
                todo_task_id: Some("active".to_owned()),
                created_at_ms: 0,
                started_at_ms: None,
                finished_at_ms: None,
                soft_budget_exhausted: false,
            }],
        );
        let panel = TodoDagPanel::new(dag, vec![workflow]);
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK))
            .unwrap();
        let overview = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(overview.contains("• Verifier (task) · running · Run focused verification"));
        assert!(overview.contains("• Alpha Worker · queued · Assemble the release"));
    }

    #[test]
    fn overview_header_uses_detail_count_terms_and_keeps_selection_at_wide_width() {
        let panel = TodoDagPanel::new(snapshot(), Vec::new());
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let wide = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        // The overview must use the same count terms as the detail page
        // (previously the ambiguous `✓N open N active …` compact form).
        assert!(
            wide.contains("active · ✓ 1 completed · 2 open · 1 active · 1 waiting"),
            "wide header must carry the detail count terms\n{wide}"
        );
        assert!(
            !wide.contains("✓1 open"),
            "compact count form must be gone\n{wide}"
        );
        // The default selection is the main DAG header; the highlight marker
        // must survive on the header line.
        assert!(
            wide.contains("› Main session [main]"),
            "selected header must keep its marker\n{wide}"
        );
        let marked = buffer
            .content
            .iter()
            .filter(|cell| {
                cell.symbol() == "›" && cell.style().bg == Some(crate::theme::DARK.selected_bg)
            })
            .count();
        assert_eq!(
            marked, 1,
            "exactly the selected header carries the highlight marker"
        );
    }

    #[test]
    fn tasks_page_rows_match_completed_and_open_counts_without_jobs() {
        let mut dag = snapshot();
        dag.jobs.clear();
        dag.phases[0].tasks.push(task(
            "dropped",
            "Dropped task",
            TodoStatus::Abandoned,
            false,
            &[],
        ));
        let counts = dag.counts();
        let expected_rows = counts.completed + counts.open;
        let task_rows = dag
            .phases
            .iter()
            .flat_map(|phase| &phase.tasks)
            .filter(|task| task.status != TodoStatus::Abandoned)
            .map(|task| rendered_lines(&[todo_task_overview_line(task, crate::theme::DARK)]))
            .collect::<Vec<_>>();
        assert_eq!(task_rows.len(), expected_rows);

        let panel = TodoDagPanel::todo(dag);
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK))
            .unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        for expected in ["Plan", "Implement", "Verify"] {
            assert!(screen.contains(expected), "missing task row {expected:?}: {screen}");
        }
        assert!(!screen.contains("Dropped task"), "abandoned task leaked: {screen}");
        assert!(screen.contains("2 open · 1 active"), "count mismatch: {screen}");
    }

    #[test]
    fn overview_header_wraps_at_60_columns_without_cutting_words() {
        let panel = TodoDagPanel::new(snapshot(), Vec::new());
        let backend = TestBackend::new(60, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let narrow = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        // 60 columns forces the header (label + counts ≈ 80 chars) to wrap;
        // every count word must still render in full — no mid-word clipping
        // at the pane edge.
        for word in ["completed", "open", "active", "waiting"] {
            assert!(
                narrow.contains(word),
                "count word {word:?} must wrap in full at 60 columns\n{narrow}"
            );
        }
        let header_rows = buffer
            .content
            .chunks(60)
            .skip(2)
            .take(2)
            .flat_map(|row| row[1..59].iter())
            .flat_map(|cell| cell.symbol().chars())
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        assert!(
            header_rows.contains("✓1completed·2open·1active·1waiting"),
            "all count terms must survive the wrap\n{narrow}"
        );
    }

    #[test]
    fn scripted_planner_long_title_still_truncates_in_overview_rows() {
        // Display contract: a planner that ignores the concise-title prompt
        // requirement (a long imperative paragraph as task content) still
        // renders truncated. The fix is prompt-side only — the panel display
        // is intentionally unchanged, so the 60-char row bound still cuts
        // the title with an ellipsis and the full title never appears.
        let long_title = "Bootstrap the pi-zig source tree with a self-contained build script and vendor every single dependency".to_owned();
        assert!(
            long_title.chars().count() > 60,
            "fixture must exceed the 60-char concise-title bound"
        );
        let mut dag = snapshot();
        dag.jobs.push(JobCardRows {
            job_id: "job-long".to_owned(),
            ordinal: 0,
            agent_id: "agent-long".to_owned(),
            agent: "task".to_owned(),
            display_name: "Verifier".to_owned(),
            todo_task_id: Some("waiting".to_owned()),
            job_status: JobStatus::Running,
            agent_status: None,
            summary: Some(long_title.clone()),
            progress: None,
            rows: vec![crate::job_card_adapter::JobCardRow {
                job_id: Some("job-long".to_owned()),
                role: crate::job_card_adapter::JobCardRowRole::Description,
                text: long_title.clone(),
            }],
        });
        let panel = TodoDagPanel::new(dag, Vec::new());
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK))
            .unwrap();
        let overview = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            !overview.contains(&long_title),
            "long title must stay truncated in the panel\n{overview}"
        );
        assert!(
            overview.contains('…'),
            "truncated title must carry the ellipsis\n{overview}"
        );
        assert!(
            overview.contains("Verifier"),
            "job row must still render\n{overview}"
        );
    }

    #[test]
    fn subagent_rows_navigate_with_arrows_and_jk_and_enter_opens_detail() {
        let mut dag = snapshot();
        dag.jobs.push(JobCardRows {
            job_id: "job-a".to_owned(),
            ordinal: 0,
            agent_id: "agent-a".to_owned(),
            agent: "task".to_owned(),
            display_name: "Verifier".to_owned(),
            todo_task_id: Some("waiting".to_owned()),
            job_status: JobStatus::Running,
            agent_status: None,
            summary: Some("Run focused verification".to_owned()),
            progress: None,
            rows: vec![crate::job_card_adapter::JobCardRow {
                job_id: Some("job-a".to_owned()),
                role: crate::job_card_adapter::JobCardRowRole::Description,
                text: "Run focused verification".to_owned(),
            }],
        });
        let workflow = TodoDagSnapshot::workflow(
            "wf-a".to_owned(),
            "Alpha workflow".to_owned(),
            Vec::new(),
            WorkflowStatus::Running,
            Vec::new(),
        );
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
        assert_eq!(
            panel.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            TodoDagPanelResult::Close
        );
    }

    #[test]
    fn subagent_detail_shows_identity_linked_todo_task_and_current_task() {
        let mut dag = snapshot();
        dag.jobs.push(JobCardRows {
            job_id: "job-a".to_owned(),
            ordinal: 0,
            agent_id: "agent-a".to_owned(),
            agent: "task".to_owned(),
            display_name: "Verifier".to_owned(),
            todo_task_id: Some("waiting".to_owned()),
            job_status: JobStatus::Running,
            agent_status: Some(AgentStatus::Parked),
            summary: None,
            progress: None,
            rows: vec![crate::job_card_adapter::JobCardRow {
                job_id: Some("job-a".to_owned()),
                role: crate::job_card_adapter::JobCardRowRole::Description,
                text: "Run focused verification".to_owned(),
            }],
        });
        let mut panel = TodoDagPanel::new(dag, Vec::new());
        panel.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        panel.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(panel.page(), TodoDagPanelPage::Subagent);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Verifier"), "{text}");
        assert!(text.contains("task"), "{text}");
        assert!(text.contains("running · parked"), "{text}");
        assert!(text.contains("Verify [waiting]"), "{text}");
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
                crate::job_card_adapter::JobEventLogEntry {
                    at: 500,
                    kind: crate::job_card_adapter::JobEventKind::Job,
                    text: "started".to_owned(),
                },
                crate::job_card_adapter::JobEventLogEntry {
                    at: 12_100,
                    kind: crate::job_card_adapter::JobEventKind::Message,
                    text: "read tools.rs".to_owned(),
                },
            ],
            history_ref: Some("history://agent-a".to_owned()),
            artifact_ref: None,
        };
        dag.jobs.push(JobCardRows {
            job_id: "job-a".to_owned(),
            ordinal: 0,
            agent_id: "agent-a".to_owned(),
            agent: "task".to_owned(),
            display_name: "Verifier".to_owned(),
            todo_task_id: None,
            job_status: JobStatus::Running,
            agent_status: None,
            summary: None,
            progress: Some(progress),
            rows: vec![
                crate::job_card_adapter::JobCardRow {
                    job_id: Some("job-a".to_owned()),
                    role: crate::job_card_adapter::JobCardRowRole::Description,
                    text: "Run focused verification".to_owned(),
                },
                crate::job_card_adapter::JobCardRow {
                    job_id: Some("job-a".to_owned()),
                    role: crate::job_card_adapter::JobCardRowRole::Progress,
                    text: "read tools.rs · 12s".to_owned(),
                },
            ],
        });
        let mut panel = TodoDagPanel::new(dag, Vec::new());
        panel.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        panel.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(panel.page(), TodoDagPanelPage::Subagent);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
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
            job_id: "job-a".to_owned(),
            ordinal: 0,
            agent_id: "agent-a".to_owned(),
            agent: "task".to_owned(),
            display_name: "Verifier".to_owned(),
            todo_task_id: Some("waiting".to_owned()),
            job_status: JobStatus::Queued,
            agent_status: None,
            summary: None,
            progress: None,
            rows: vec![crate::job_card_adapter::JobCardRow {
                job_id: Some("job-a".to_owned()),
                role: crate::job_card_adapter::JobCardRowRole::Description,
                text: String::new(),
            }],
        });
        let mut panel = TodoDagPanel::new(dag, Vec::new());
        panel.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let marked = buffer
            .content
            .iter()
            .enumerate()
            .filter(|(_, cell)| {
                cell.symbol() == "›" && cell.style().bg == Some(crate::theme::DARK.selected_bg)
            })
            .count();
        assert_eq!(
            marked, 1,
            "exactly the selected subagent row carries the highlight marker"
        );
        let text = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("› • Verifier"), "{text}");
    }

    #[test]
    fn stale_subagent_selection_drops_back_to_overview_after_job_update() {
        let mut dag = snapshot();
        dag.jobs.push(JobCardRows {
            job_id: "job-a".to_owned(),
            ordinal: 0,
            agent_id: "agent-a".to_owned(),
            agent: "task".to_owned(),
            display_name: "Verifier".to_owned(),
            todo_task_id: Some("waiting".to_owned()),
            job_status: JobStatus::Running,
            agent_status: None,
            summary: None,
            progress: None,
            rows: vec![crate::job_card_adapter::JobCardRow {
                job_id: Some("job-a".to_owned()),
                role: crate::job_card_adapter::JobCardRowRole::Description,
                text: "Run focused verification".to_owned(),
            }],
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
        let tasks = (0..24)
            .map(|index| {
                task(
                    &format!("task-{index}"),
                    &format!("Task row {index}"),
                    TodoStatus::Pending,
                    true,
                    &[],
                )
            })
            .collect();
        let dag = TodoDagSnapshot::main(
            vec![TodoPhase {
                name: "Long phase".to_owned(),
                tasks,
            }],
            TodoDagExecutionStatus::Active,
            None,
            Vec::new(),
        );
        let mut panel = TodoDagPanel::new(dag, Vec::new());
        panel.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK))
            .unwrap();
        let first = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(first.contains("Task row 0"));
        assert!(!first.contains("Task row 23"));
        for _ in 0..40 {
            panel.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        terminal
            .draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK))
            .unwrap();
        let last = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(last.contains("Task row 23"));
        let bottom = panel.detail_scroll();
        assert!(bottom > 0);
        panel.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(panel.detail_scroll(), bottom - 1);

        panel.update_main_phases(vec![TodoPhase {
            name: "Short".to_owned(),
            tasks: vec![task("one", "Only row", TodoStatus::Pending, true, &[])],
        }]);
        assert_eq!(panel.detail_scroll(), 0);
        terminal
            .draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK))
            .unwrap();
        let shrunk = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(shrunk.contains("Only row"));
        assert!(shrunk.contains("1/1"));
    }

    #[test]
    fn narrow_wrapped_rows_scroll_to_final_sentinel() {
        let long = "界".repeat(32);
        let mut first = task(
            "long",
            &format!("Long task {long}"),
            TodoStatus::Pending,
            false,
            &["dependency"],
        );
        first.waiting_on[0].content = format!("Long dependency {long}");
        let final_task = task("final", "FINAL SENTINEL", TodoStatus::Pending, true, &[]);
        let mut dag = TodoDagSnapshot::main(
            vec![TodoPhase {
                name: "Narrow".to_owned(),
                tasks: vec![first, final_task],
            }],
            TodoDagExecutionStatus::Active,
            None,
            Vec::new(),
        );
        dag.jobs.push(JobCardRows {
            job_id: "job".to_owned(),
            ordinal: 0,
            agent_id: "agent".to_owned(),
            agent: "task".to_owned(),
            display_name: "Worker".to_owned(),
            todo_task_id: Some("long".to_owned()),
            job_status: JobStatus::Running,
            agent_status: None,
            summary: None,
            progress: None,
            rows: vec![crate::job_card_adapter::JobCardRow {
                job_id: Some("job".to_owned()),
                role: crate::job_card_adapter::JobCardRowRole::Description,
                text: format!("Long job {long}"),
            }],
        });
        let mut panel = TodoDagPanel::new(dag, Vec::new());
        panel.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let backend = TestBackend::new(24, 9);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK))
            .unwrap();
        let first_view = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!first_view.contains("FINAL SENTINEL"));
        for _ in 0..100 {
            panel.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        terminal
            .draw(|frame| render_todo_dag_panel(frame, &panel, crate::theme::DARK))
            .unwrap();
        let final_view = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(final_view.contains("FINAL SENTINEL"));
        let bottom = panel.detail_scroll();
        panel.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(panel.detail_scroll(), bottom - 1);
    }

    #[test]
    fn delegation_context_builder_keeps_canonical_bounded_sections_and_redacts() {
        let long_goal = format!(
            "Requested task {SECRET_SENTINEL} {}",
            "x".repeat(TASK_SECTION_CHAR_LIMIT + 64)
        );
        let context = TaskDelegationSections {
            goal: long_goal,
            target: format!("Requested target {SECRET_SENTINEL}"),
            constraints: format!("Requested constraints {SECRET_SENTINEL}"),
            contract: format!("Requested contract {SECRET_SENTINEL}"),
        };

        let dag = snapshot().with_delegation_context(&context);
        assert_eq!(
            dag.delegation_context.goal.chars().count(),
            TASK_SECTION_CHAR_LIMIT
        );
        for body in [
            dag.delegation_context.goal.as_str(),
            dag.delegation_context.target.as_str(),
            dag.delegation_context.constraints.as_str(),
            dag.delegation_context.contract.as_str(),
        ] {
            assert!(
                !body.contains(SECRET_SENTINEL),
                "stored canonical section must redact: {body}"
            );
            assert!(
                body.contains("[REDACTED]"),
                "stored canonical section must carry marker: {body}"
            );
        }
        let text = rendered_lines(&detail_lines(&dag, crate::theme::DARK));
        assert!(text.contains("Delegation context"), "{text}");
        for heading in ["Goal", "Target", "Constraints", "Contract"] {
            assert!(
                text.contains(heading),
                "requested {heading:?} section must render: {text}"
            );
        }
        assert!(
            !text.contains(SECRET_SENTINEL),
            "context secret must be redacted: {text}"
        );
        let mut panel = TodoDagPanel::todo(snapshot());
        panel.update_main_delegation_context(&context);
        let refreshed = panel.selected_dag().expect("main snapshot");
        assert_eq!(refreshed.delegation_context, dag.delegation_context);
    }

    #[test]
    fn todo_dag_and_subagent_detail_redact_failure_task_job_activity_and_identity_fields() {
        let secret_field = |label: &str| format!("{label} {SECRET_SENTINEL}");
        let todo = TodoItem {
            id: "authority-task".to_owned(),
            content: secret_field("task content"),
            status: TodoStatus::InProgress,
            depends_on: vec!["authority-blocker".to_owned()],
            ready: false,
            waiting_on: vec![TodoWaitingOn {
                task_id: "authority-blocker".to_owned(),
                content: secret_field("waiting content"),
                status: TodoStatus::InProgress,
            }],
            agent: Some(secret_field("planned persona")),
        };
        let progress = crate::job_card_adapter::JobCardProgress {
            stage: Some(secret_field("stage")),
            activity: Some(secret_field("activity")),
            activity_at: Some(12_100),
            elapsed_ms: Some(12_000),
            events: vec![
                crate::job_card_adapter::JobEventLogEntry {
                    at: 500,
                    kind: crate::job_card_adapter::JobEventKind::Job,
                    text: secret_field("job event"),
                },
                crate::job_card_adapter::JobEventLogEntry {
                    at: 12_100,
                    kind: crate::job_card_adapter::JobEventKind::Message,
                    text: secret_field("message event"),
                },
            ],
            history_ref: Some(format!("history://{SECRET_SENTINEL}")),
            artifact_ref: None,
        };
        let card = JobCardRows {
            job_id: "opaque-job-fixed".to_owned(),
            ordinal: 0,
            agent_id: "canonical-agent-fixed".to_owned(),
            agent: secret_field("job persona"),
            display_name: secret_field("display identity"),
            todo_task_id: Some("authority-task".to_owned()),
            job_status: JobStatus::Running,
            agent_status: Some(AgentStatus::Parked),
            summary: Some(secret_field("task summary")),
            progress: Some(progress),
            rows: vec![
                JobCardRow {
                    job_id: Some("opaque-job-fixed".to_owned()),
                    role: JobCardRowRole::Description,
                    text: secret_field("description"),
                },
                JobCardRow {
                    job_id: Some("opaque-job-fixed".to_owned()),
                    role: JobCardRowRole::Progress,
                    text: secret_field("progress row"),
                },
            ],
        };
        let mut dag = TodoDagSnapshot::main(
            vec![TodoPhase {
                name: secret_field("phase"),
                tasks: vec![todo],
            }],
            TodoDagExecutionStatus::Failed,
            Some(TodoDagFailureReason::Spawn {
                message: secret_field("failure"),
            }),
            vec![card],
        );
        dag.label = secret_field("main label");

        let job = dag.subagent_job(0).expect("job projection");
        let detail = rendered_lines(&detail_lines(&dag, crate::theme::DARK));
        let subagent = rendered_lines(&subagent_detail_lines(&dag, &job, crate::theme::DARK));
        let overview = rendered_lines(&[
            overview_header_line(&dag, crate::theme::DARK, true),
            subagent_row_line(&job, crate::theme::DARK, true),
        ]);
        for (surface, text) in [
            ("dag detail", detail.as_str()),
            ("subagent detail", subagent.as_str()),
            ("overview", overview.as_str()),
        ] {
            assert!(
                !text.contains(SECRET_SENTINEL),
                "{surface} must redact every untrusted field:\n{text}"
            );
            assert!(
                text.contains("[REDACTED]"),
                "{surface} must show redaction markers:\n{text}"
            );
        }
        assert!(detail.contains("Failure  failure [REDACTED]"), "{detail}");
        assert!(
            detail.contains("authority-task"),
            "canonical task authority id must stay visible: {detail}"
        );
        assert!(
            subagent.contains("authority-task"),
            "canonical linked-task id must stay visible: {subagent}"
        );
        assert!(
            !subagent.contains("opaque-job-fixed"),
            "opaque job id stays hidden from presentation: {subagent}"
        );

        // The independent Activity surface is a separate renderer; verify its
        // job identity and row bodies also use the centralized helper.
        let activity_panel = TodoDagPanel::activity(dag);
        let backend = TestBackend::new(160, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_todo_dag_panel(frame, &activity_panel, crate::theme::DARK))
            .unwrap();
        let activity = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            !activity.contains(SECRET_SENTINEL),
            "activity surface must redact:\n{activity}"
        );
        assert!(
            activity.contains("[REDACTED]"),
            "activity surface must show redaction markers:\n{activity}"
        );
    }

    #[test]
    fn narrow_detail_rows_redact_before_wrapping_and_stay_within_width() {
        let wide = "界".repeat(16);
        let mut dag = TodoDagSnapshot::main(
            vec![TodoPhase {
                name: format!("{SECRET_SENTINEL} phase {wide}"),
                tasks: vec![task(
                    "authority-task",
                    &format!("{SECRET_SENTINEL} task {wide}"),
                    TodoStatus::Pending,
                    true,
                    &[],
                )],
            }],
            TodoDagExecutionStatus::Active,
            None,
            Vec::new(),
        );
        dag.label = format!("{SECRET_SENTINEL} label {wide}");
        let context = TaskDelegationSections {
            goal: String::new(),
            target: String::new(),
            constraints: String::new(),
            contract: format!("{SECRET_SENTINEL} shared {wide}"),
        };
        dag = dag.with_delegation_context(&context);

        let width = 24;
        let wrapped = detail_lines(&dag, crate::theme::DARK)
            .into_iter()
            .flat_map(|line| wrap_styled_line(line, width))
            .collect::<Vec<_>>();
        let text = rendered_lines(&wrapped);
        assert!(
            !text.contains(SECRET_SENTINEL),
            "secret must be redacted before wrapping:\n{text}"
        );
        assert!(
            text.contains("[REDACTED]"),
            "redaction marker must survive narrow wrapping:\n{text}"
        );
        for line in &wrapped {
            let body = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            assert!(
                pi_coding::markdown::display_width(&body) <= width,
                "wrapped line must fit {width} terminal cells: {body:?} ({})",
                pi_coding::markdown::display_width(&body),
            );
        }
    }
}
