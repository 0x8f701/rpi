//! Pure state and rendering helpers for the dedicated workflow master-detail page.

use std::cell::Cell;
use std::collections::{BTreeSet, HashMap, HashSet};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use pi_coding::{TodoPhase, WorkflowStatus};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use serde::{Deserialize, Serialize};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::theme::Theme;
use crate::todo_dag_view::{wrap_styled_line, workflow_todo_dag_lines};


/// A named workflow participant or durable job projection.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowActorSnapshot {
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub task: Option<String>,
    /// Opaque Todo-DAG task id backing `task` when the actor works a
    /// delegated todo item. Never rendered; used to dedupe the Active tasks
    /// section against job-derived rows by task identity.
    #[serde(default)]
    pub task_id: Option<String>,
    /// Bounded live activity feed (newest-last) shown when the agent's row is
    /// expanded: delegated task lifecycle entries plus IRC the agent sent or
    /// received. Live-only; never part of the durable workflow record.
    #[serde(default)]
    pub activity: Vec<WorkflowAgentActivitySnapshot>,
}

/// One row of the workflow detail's Active tasks list: a live worker job's
/// task summary or an agent's current task summary. The Todo-DAG task id
/// (when known) is the identity key — job-derived and agent-derived rows for
/// the same task collapse to one row in the section.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowActiveTaskSnapshot {
    /// Opaque Todo-DAG task id backing this row (None for free-form agent
    /// summaries without a job/todo backing). Never rendered.
    #[serde(default)]
    pub task_id: Option<String>,
    pub summary: String,
}

/// One recent, already-authorized IRC projection.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowIrcSnapshot {
    pub sender: String,
    pub text: String,
    /// Epoch millis of the delivery; `0` when unknown (durable-only fallback).
    #[serde(default)]
    pub at_ms: u64,
}

/// One bounded entry of an agent's activity feed (delegated task lifecycle or
/// IRC) shown under its expanded row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowAgentActivitySnapshot {
    /// Epoch millis when the activity was observed.
    pub at_ms: u64,
    pub kind: WorkflowAgentActivityKind,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowAgentActivityKind {
    /// A delegated task (job) lifecycle entry.
    Task,
    /// A delivered IRC message involving the agent.
    Irc,
}

/// Worktree information deliberately limited to safe display labels.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowWorktreeSnapshot {
    pub label: String,
    pub branch: String,
}

/// One bounded entry of the supervisor's live activity feed (coalesced
/// thinking chunks, tool calls, IRC progress) projected into the workflow
/// page so planning never reads as a static spinner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowActivitySnapshot {
    /// Epoch millis when the activity was observed.
    pub at_ms: u64,
    pub kind: pi_coding::WorkflowSupervisorActivityKind,
    pub text: String,
}

/// The final Todo-DAG step: merge the workflow worktree back to the source
/// branch. `Pending` while todos are open; the workflow auto-integrates on
/// completion (or the user runs `/workflow integrate`), then the merge is
/// `Applied` or requires manual `Conflicted` resolution.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowIntegrateStep {
    /// Todos are not all complete yet; the merge has not started.
    #[default]
    Pending,
    /// Todos completed; the worktree merge is running (auto-integrate).
    Integrating,
    /// The merge was applied back to the source branch.
    Applied,
    /// The merge hit conflicts; the user resolves and integrates manually.
    Conflicted,
}

/// Integration state shown without diff bodies or repository paths.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowIntegrationSnapshot {
    /// The final DAG step — merge the worktree back to the source branch.
    #[serde(default)]
    pub step: WorkflowIntegrateStep,
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
    pub active_tasks: Vec<WorkflowActiveTaskSnapshot>,
    #[serde(default)]
    pub worktree: WorkflowWorktreeSnapshot,
    #[serde(default)]
    pub integration: WorkflowIntegrationSnapshot,
    /// Bounded live activity feed for the supervisor's own turn (newest-last).
    /// Live-only; never part of the durable workflow record.
    #[serde(default)]
    pub planning_activity: Vec<WorkflowActivitySnapshot>,
    /// Epoch millis when the current Planning phase started (None otherwise).
    #[serde(default)]
    pub planning_started_at_ms: Option<u64>,
}

impl From<&pi_coding::WorkflowSnapshot> for WorkflowPanelSnapshot {
    fn from(snapshot: &pi_coding::WorkflowSnapshot) -> Self {
        let integration = match &snapshot.integration {
            pi_coding::WorkflowIntegration::None => WorkflowIntegrationSnapshot {
                // Todos settled into Completed but the merge is not recorded
                // yet: the auto-integrate is running (or the workflow awaits
                // a manual integrate). Any other status means the DAG is
                // still open and the final step is pending.
                step: if snapshot.status == WorkflowStatus::Completed {
                    WorkflowIntegrateStep::Integrating
                } else {
                    WorkflowIntegrateStep::Pending
                },
                ..WorkflowIntegrationSnapshot::default()
            },
            pi_coding::WorkflowIntegration::Applied { .. } => WorkflowIntegrationSnapshot {
                step: WorkflowIntegrateStep::Applied,
                ..WorkflowIntegrationSnapshot::default()
            },
            pi_coding::WorkflowIntegration::Conflicted { conflicts } => WorkflowIntegrationSnapshot {
                step: WorkflowIntegrateStep::Conflicted,
                summary: "Manual resolution required".to_owned(),
                conflicts: conflicts.clone(),
                ..WorkflowIntegrationSnapshot::default()
            },
        };
        Self {
            id: snapshot.workflow_id.to_string(),
            generation: snapshot.generation,
            name: snapshot.name.clone(),
            objective: snapshot.objective.clone(),
            status: snapshot.status,
            todo: snapshot.todo.phases.clone(),
            supervisor: snapshot.supervisor_agent_id.as_ref().map(|name| WorkflowActorSnapshot { name: name.clone(), status: snapshot.status.as_str().to_owned(), task: None, task_id: None, activity: Vec::new() }),
            subagents: Vec::new(),
            recent_irc: Vec::new(),
            active_tasks: Vec::new(),
            worktree: WorkflowWorktreeSnapshot { label: snapshot.worktree_label.clone().unwrap_or_default(), branch: snapshot.branch.clone().unwrap_or_default() },
            integration,
            planning_activity: Vec::new(),
            // Without a live runtime the durable record cannot stream
            // activity; fall back to the creation time so the elapsed clock
            // still reads sensibly while the workflow sits in Planning.
            planning_started_at_ms: (snapshot.status == WorkflowStatus::Planning)
                .then_some(snapshot.created_at_ms),
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
            task: supervisor.task_summary.clone(),
            task_id: supervisor.todo_task_id.clone(),
            activity: supervisor.activity.iter().map(agent_activity_snapshot).collect(),
        });
        projected.subagents = detail.subagents.iter().map(|subagent| WorkflowActorSnapshot {
            name: subagent.display_name.clone(),
            status: agent_status_label(subagent.status).to_owned(),
            task: subagent.task_summary.clone(),
            task_id: subagent.todo_task_id.clone(),
            activity: subagent.activity.iter().map(agent_activity_snapshot).collect(),
        }).collect();
        projected.recent_irc = detail.irc.iter().map(|message| WorkflowIrcSnapshot {
            sender: message.from.clone(),
            text: message.body.clone(),
            at_ms: message.timestamp_ms,
        }).collect();
        projected.active_tasks = detail.jobs.iter().filter(|job| !job.status.is_settled()).filter_map(|job| {
            let summary = job.task_summary.clone()?;
            Some(WorkflowActiveTaskSnapshot { task_id: job.todo_task_id.clone(), summary })
        }).collect();
        projected.planning_activity = detail.supervisor_activity.iter().map(|entry| WorkflowActivitySnapshot {
            at_ms: entry.at_ms,
            kind: entry.kind,
            text: entry.text.clone(),
        }).collect();
        projected.planning_started_at_ms = detail.planning_started_at_ms;
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

/// Which pane of the master-detail split currently receives navigation keys.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorkflowPanelFocus {
    #[default]
    List,
    Detail,
}

/// Selection, filter, focus and scroll state for the workflow master-detail page.
#[derive(Clone, Debug, Default)]
pub struct WorkflowPanel {
    workflows: Vec<WorkflowPanelSnapshot>,
    selected: usize,
    filter: String,
    filtering: bool,
    focus: WorkflowPanelFocus,
    /// Whether the user has explicitly picked a pane with Tab. Until then
    /// navigation keys follow the pane that has content to navigate (see
    /// `navigation_focus`): the detail when it overflows, the list otherwise.
    /// Tab flips this on and the stored `focus` routes ↑/↓ as today.
    focus_explicit: bool,
    detail_scroll: Cell<usize>,
    detail_width: Cell<usize>,
    detail_viewport_height: Cell<usize>,
    detail_display_rows: Cell<usize>,
    /// Expanded agent rows per workflow id (agent display names). Rows start
    /// collapsed — one marker line each — and Enter/Space on a row in Detail
    /// focus expands it to its bounded activity feed. Keyed by workflow id so
    /// folds survive `replace`.
    expanded_agents: HashMap<String, BTreeSet<String>>,
    /// Wall-clock epoch millis for live elapsed/inactivity rendering.
    /// Refreshed on the TUI render tick like the T92 job-card adapter.
    now: Cell<u64>,
}

impl WorkflowPanel {
    #[must_use]
    pub fn new(workflows: Vec<WorkflowPanelSnapshot>) -> Self {
        Self { workflows, selected: 0, filter: String::new(), filtering: false, focus: WorkflowPanelFocus::List, focus_explicit: false, detail_scroll: Cell::new(0), detail_width: Cell::new(1), detail_viewport_height: Cell::new(0), detail_display_rows: Cell::new(0), expanded_agents: HashMap::new(), now: Cell::new(0) }
    }

    /// Provide the wall-clock for live elapsed/inactivity rendering. Callers
    /// refresh this on their render tick; `0` (the default) leaves the
    /// elapsed/inactivity projections off (plain stage-only text).
    pub fn set_now(&mut self, now: u64) {
        self.now.set(now);
    }

    /// Whether any listed workflow is mid-planning — used to keep the TUI
    /// render tick alive so the planning elapsed/inactivity stays live.
    #[must_use]
    pub fn has_active_planning(&self) -> bool {
        self.workflows
            .iter()
            .any(|workflow| workflow.status == WorkflowStatus::Planning)
    }

    /// Whether the selected workflow's planning has produced no activity for
    /// the bounded inactivity window (with a live clock). `now == 0` means no
    /// clock yet; the notice stays off and Esc keeps its default behavior.
    fn selected_workflow_stalled(&self) -> bool {
        let Some(workflow) = self.selected_workflow() else {
            return false;
        };
        let now = self.now.get();
        if now == 0 || workflow.status != WorkflowStatus::Planning {
            return false;
        }
        let last_activity_at = workflow
            .planning_activity
            .last()
            .map(|entry| entry.at_ms)
            .or(workflow.planning_started_at_ms);
        last_activity_at.is_some_and(|last| {
            now.saturating_sub(last) >= PLANNING_INACTIVITY_NOTICE_MS
        })
    }

    pub fn replace(&mut self, workflows: Vec<WorkflowPanelSnapshot>) {
        let selected_id = self.selected_workflow().map(|workflow| workflow.id.clone());
        // Drop fold state for workflows that no longer exist, and for agent
        // names that no longer appear in a surviving workflow.
        let live_ids = workflows.iter().map(|workflow| workflow.id.as_str()).collect::<HashSet<_>>();
        self.expanded_agents.retain(|id, folds| {
            if !live_ids.contains(id.as_str()) {
                return false;
            }
            if let Some(workflow) = workflows.iter().find(|workflow| workflow.id == *id) {
                let names = workflow.subagents.iter().chain(workflow.supervisor.iter()).map(|actor| actor.name.as_str()).collect::<HashSet<_>>();
                folds.retain(|name| names.contains(name.as_str()));
            }
            true
        });
        self.workflows = workflows;
        let retained = selected_id.and_then(|id| {
            self.visible_indices().into_iter().position(|index| self.workflows[index].id == id)
        });
        if retained.is_none() {
            self.detail_scroll.set(0);
        }
        self.selected = retained.unwrap_or(0);
        self.clamp_selection();
        self.clamp_detail_scroll();
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
    pub const fn focus(&self) -> WorkflowPanelFocus { self.focus }

    /// The pane navigation keys (↑/↓/j/k, Home/End, PageUp/PageDown) act on.
    ///
    /// Before the user has explicitly toggled panes with Tab, navigation
    /// follows the pane that has content to navigate: the detail when the
    /// selected workflow overflows its pane, the list when the detail fits.
    /// Once Tab has been used, the stored `focus` routes navigation exactly
    /// as before. The shared footer's `focus:` label reports this target.
    #[must_use]
    pub fn navigation_focus(&self) -> WorkflowPanelFocus {
        if self.focus_explicit {
            self.focus
        } else if self.detail_scroll_max() > 0 {
            WorkflowPanelFocus::Detail
        } else {
            WorkflowPanelFocus::List
        }
    }

    #[must_use]
    pub fn detail_scroll(&self) -> usize { self.detail_scroll.get() }

    #[must_use]
    pub fn detail_scroll_max(&self) -> usize { self.detail_display_rows.get().saturating_sub(self.detail_viewport_height.get()) }

    /// Select the visible workflow whose opaque id matches `id`.
    ///
    /// Returns `false` (and leaves the selection untouched) when no visible
    /// workflow carries that id, e.g. when a filter hides it or it no longer
    /// exists. Used by the TUI to restore the application-selected workflow
    /// when the page opens.
    pub fn select_id(&mut self, id: &str) -> bool {
        if let Some(position) = self.visible_indices().into_iter().position(|index| self.workflows[index].id == id) {
            self.selected = position;
            self.detail_scroll.set(0);
            true
        } else {
            false
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> WorkflowPanelResult {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return WorkflowPanelResult::Handled;
        }
        if self.filtering {
            return match key.code {
                KeyCode::Esc | KeyCode::Enter => { self.filtering = false; WorkflowPanelResult::Handled }
                KeyCode::Backspace => { self.filter.pop(); self.selected = 0; self.clamp_selection(); self.detail_scroll.set(0); WorkflowPanelResult::Handled }
                KeyCode::Char(character) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                    self.filter.push(character); self.selected = 0; self.clamp_selection(); self.detail_scroll.set(0); WorkflowPanelResult::Handled
                }
                _ => WorkflowPanelResult::Handled,
            };
        }
        match key.code {
            KeyCode::Char('p') => return self.intent(WorkflowIntentKind::Pause),
            KeyCode::Char('r') => return self.intent(WorkflowIntentKind::Resume),
            KeyCode::Char('c') => return self.intent(WorkflowIntentKind::Cancel),
            KeyCode::Char('i') => return self.intent(WorkflowIntentKind::Integrate),
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus_explicit = true;
                self.focus = match self.focus { WorkflowPanelFocus::List => WorkflowPanelFocus::Detail, WorkflowPanelFocus::Detail => WorkflowPanelFocus::List };
                self.detail_scroll.set(0);
                return WorkflowPanelResult::Handled;
            }
            KeyCode::Esc => {
                if self.selected_workflow_stalled() {
                    // The detail's inactivity notice promises "Esc to cancel":
                    // a planning workflow with no progress for the bounded
                    // window gets cancelled instead of closing the page.
                    return self.intent(WorkflowIntentKind::Cancel);
                }
                return match self.focus {
                    WorkflowPanelFocus::Detail => { self.focus = WorkflowPanelFocus::List; WorkflowPanelResult::Handled }
                    WorkflowPanelFocus::List => WorkflowPanelResult::Close,
                };
            }
            KeyCode::Char('q') if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => return WorkflowPanelResult::Close,
            _ => {}
        }
        match self.focus {
            WorkflowPanelFocus::List => match key.code {
                KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                    if self.selected_workflow().is_some() { self.focus = WorkflowPanelFocus::Detail; self.detail_scroll.set(0); }
                    WorkflowPanelResult::Handled
                }
                KeyCode::Char('/') => { self.filtering = true; WorkflowPanelResult::Handled }
                _ => self.handle_navigation(key),
            },
            WorkflowPanelFocus::Detail => match key.code {
                KeyCode::Enter | KeyCode::Char(' ') => {
                    // Fold/unfold the agent row under the cursor: scroll to an
                    // agent row and press Enter/Space to expand its bounded
                    // activity feed (or collapse it back to one line).
                    self.toggle_agent_at_cursor();
                    WorkflowPanelResult::Handled
                }
                KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => { self.focus = WorkflowPanelFocus::List; WorkflowPanelResult::Handled }
                _ => self.handle_navigation(key),
            },
        }
    }

    /// Route ↑/↓/j/k (and Home/End/PageUp/PageDown) to the pane reported by
    /// `navigation_focus`: scroll the detail when it owns the keys, move the
    /// list selection when the list owns them. Detail scrolling clamps at the
    /// content edge; a selection change resets the detail scroll so the next
    /// workflow renders from its top.
    fn handle_navigation(&mut self, key: KeyEvent) -> WorkflowPanelResult {
        match self.navigation_focus() {
            WorkflowPanelFocus::List => match key.code {
                KeyCode::Up | KeyCode::Char('k') => { self.move_selection(-1); self.detail_scroll.set(0); WorkflowPanelResult::Handled }
                KeyCode::Down | KeyCode::Char('j') => { self.move_selection(1); self.detail_scroll.set(0); WorkflowPanelResult::Handled }
                KeyCode::Home => { self.selected = 0; self.detail_scroll.set(0); WorkflowPanelResult::Handled }
                KeyCode::End => { self.selected = self.visible_indices().len().saturating_sub(1); self.detail_scroll.set(0); WorkflowPanelResult::Handled }
                _ => WorkflowPanelResult::Unknown,
            },
            WorkflowPanelFocus::Detail => match key.code {
                KeyCode::Up | KeyCode::Char('k') => { self.detail_scroll.set(self.detail_scroll.get().saturating_sub(1)); WorkflowPanelResult::Handled }
                KeyCode::Down | KeyCode::Char('j') => { self.detail_scroll.set(self.detail_scroll.get().saturating_add(1).min(self.detail_scroll_max())); WorkflowPanelResult::Handled }
                KeyCode::PageUp => { let viewport = self.detail_viewport_height.get().max(1); self.detail_scroll.set(self.detail_scroll.get().saturating_sub(viewport)); WorkflowPanelResult::Handled }
                KeyCode::PageDown => { let viewport = self.detail_viewport_height.get().max(1); self.detail_scroll.set(self.detail_scroll.get().saturating_add(viewport).min(self.detail_scroll_max())); WorkflowPanelResult::Handled }
                KeyCode::Home => { self.detail_scroll.set(0); WorkflowPanelResult::Handled }
                KeyCode::End => { self.detail_scroll.set(self.detail_scroll_max()); WorkflowPanelResult::Handled }
                _ => WorkflowPanelResult::Unknown,
            },
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

    fn refresh_detail_geometry(&self) {
        let rows = self.selected_workflow().map_or(0, |workflow| {
            let expanded = self.expanded_agents.get(&workflow.id).cloned().unwrap_or_default();
            detail_display_row_count(workflow, &expanded, self.detail_width.get(), self.now.get())
        });
        self.detail_display_rows.set(rows);
        self.detail_scroll.set(self.detail_scroll.get().min(self.detail_scroll_max()));
    }

    fn clamp_detail_scroll(&self) { self.refresh_detail_geometry(); }

    /// Fold or unfold the agent row that contains the current cursor line
    /// (the top visible line of the detail). Rows without an activity feed
    /// are not foldable and stay unchanged. The fold state is keyed by the
    /// workflow id and the agent's display name so it survives `replace`.
    fn toggle_agent_at_cursor(&mut self) {
        let Some(workflow) = self.selected_workflow() else {
            return;
        };
        let width = self.detail_width.get().max(1);
        let now = self.now.get();
        let expanded = self.expanded_agents.get(&workflow.id).cloned().unwrap_or_default();
        let offsets = agent_row_display_offsets(workflow, &expanded, crate::theme::DARK, width, now);
        let scroll = self.detail_scroll.get();
        let Some((_, name)) = offsets.iter().rev().find(|(offset, _)| *offset <= scroll) else {
            return;
        };
        let folds = self.expanded_agents.entry(workflow.id.clone()).or_default();
        if !folds.remove(name) {
            folds.insert(name.clone());
        }
        self.clamp_detail_scroll();
    }

    fn intent(&self, kind: WorkflowIntentKind) -> WorkflowPanelResult {
        self.selected_workflow().map_or(WorkflowPanelResult::Handled, |workflow| WorkflowPanelResult::Intent {
            workflow_id: workflow.id.clone(), kind,
        })
    }
}

const LIST_MIN_WIDTH: usize = 28;
const LIST_MAX_WIDTH: usize = 44;
const LIST_PCT: usize = 34;
const DETAIL_MIN_WIDTH: usize = 36;
const DIVIDER_WIDTH: usize = 1;
const FOOTER_HEIGHT: u16 = 1;
const STACKED_MIN_WIDTH: u16 = 70;
const STACKED_MIN_HEIGHT: u16 = 8;
/// After this long without any supervisor activity the planning phase reads
/// as stalled: the detail shows a bounded inactivity notice instead of a
/// static spinner, and Esc cancels the workflow.
const PLANNING_INACTIVITY_NOTICE_MS: u64 = 45_000;
/// How many of the most recent activity entries the collapsed planning feed
/// shows (mirrors the main transcript's thinking collapse: bounded window,
/// never the full supervisor turn).
const PLANNING_ACTIVITY_FEED_LINES: usize = 3;
/// How many of the most recent activity entries an expanded agent row shows;
/// older entries collapse to one "… N earlier" line.
const AGENT_ACTIVITY_FEED_LINES: usize = 5;
/// How many of the most recent IRC messages the Recent IRC section shows;
/// older messages collapse to one "… N earlier" line.
const IRC_FEED_LINES: usize = 6;

/// Split the inner panel body into `(list, divider, detail, footer)` rects.
///
/// Wide/tall terminals get a side-by-side split with a 1-column divider and a
/// shared 1-row footer. Terminals below the stacked threshold fall back to a
/// vertical stack (list top 40%, detail bottom 60%, no divider). All rect math
/// uses saturating operations so the layout never panics.
fn compute_workflow_layout(area: Rect) -> (Rect, Rect, Rect, Rect) {
    let footer_height = FOOTER_HEIGHT.min(area.height);
    let body_height = area.height.saturating_sub(footer_height);
    let body = Rect { height: body_height, ..area };
    let footer = Rect { y: area.y.saturating_add(body_height), height: footer_height, ..area };

    if area.width < STACKED_MIN_WIDTH || area.height < STACKED_MIN_HEIGHT {
        let list_height = (body_height.saturating_mul(2).saturating_div(5)).max(1).min(body_height.saturating_sub(1));
        let list = Rect { height: list_height, ..body };
        let detail = Rect { y: body.y.saturating_add(list_height), height: body_height.saturating_sub(list_height), ..body };
        let divider = Rect { width: 0, ..body };
        return (list, divider, detail, footer);
    }

    let body_width = usize::from(body.width);
    let left = (body_width.saturating_mul(LIST_PCT) / 100).clamp(LIST_MIN_WIDTH, LIST_MAX_WIDTH).min(body_width.saturating_sub(DETAIL_MIN_WIDTH).saturating_sub(DIVIDER_WIDTH)).max(1);
    let left_u16 = u16::try_from(left).unwrap_or(u16::MAX);
    let divider_x = body.x.saturating_add(left_u16);
    let divider = Rect { x: divider_x, y: body.y, width: DIVIDER_WIDTH as u16, height: body_height };
    let detail_x = divider_x.saturating_add(DIVIDER_WIDTH as u16);
    let detail_width = body.x.saturating_add(body.width).saturating_sub(detail_x);
    let detail = Rect { x: detail_x, y: body.y, width: detail_width, height: body_height };
    let list = Rect { x: body.x, y: body.y, width: left_u16, height: body_height };
    (list, divider, detail, footer)
}

pub fn render_workflow_panel(frame: &mut ratatui::Frame<'_>, panel: &WorkflowPanel, theme: Theme) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    let visible = panel.visible_indices();
    let active = panel.workflows.iter().filter(|workflow| workflow.status.is_active()).count();
    let title = format!(" Workflows · {}/{} · {} active ", visible.len(), panel.workflows.len(), active);
    let block = Block::default().title(title).borders(Borders::ALL).border_style(Style::default().fg(theme.border_accent));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let (list, divider, detail, footer) = compute_workflow_layout(inner);
    render_workflow_list(frame, panel, list, theme);
    if divider.width > 0 {
        frame.render_widget(Block::default().borders(Borders::RIGHT).border_style(Style::default().fg(theme.border)), divider);
    }
    render_workflow_detail(frame, panel, detail, theme);
    // The footer reports which pane ↑/↓/j/k act on right now: content-aware
    // until the user picks a pane with Tab, then the explicit pane. The fold
    // hint follows the stored focus because Enter folds agents only there.
    let focus_label = match panel.navigation_focus() { WorkflowPanelFocus::List => "list", WorkflowPanelFocus::Detail => "detail" };
    let fold_hint = match panel.focus { WorkflowPanelFocus::Detail => " · Enter fold agent", WorkflowPanelFocus::List => "" };
    let footer_text = format!("focus:{focus_label} · ↑/↓ select/scroll · Tab pane · / filter · p/r/c/i lifecycle · Esc back/close · q close{fold_hint}");
    frame.render_widget(Paragraph::new(footer_text).style(Style::default().fg(theme.dim)), footer);
}

fn render_workflow_list(frame: &mut ratatui::Frame<'_>, panel: &WorkflowPanel, area: Rect, theme: Theme) {
    if area.height == 0 { return; }
    let header_height = 1u16.min(area.height);
    let rows_height = area.height.saturating_sub(header_height);
    let header = Rect { height: header_height, ..area };
    let rows = Rect { y: area.y.saturating_add(header_height), height: rows_height, ..area };

    let mut header_spans = vec![Span::styled("Workflows", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD))];
    if panel.filtering { header_spans.push(Span::styled(format!(" · filter:{}", panel.filter), Style::default().fg(theme.warning))); }
    frame.render_widget(Paragraph::new(Line::from(header_spans)), header);

    let visible = panel.visible_indices();
    let viewport = usize::from(rows.height).max(1);
    let lines = if panel.workflows.is_empty() {
        vec![Line::from(Span::styled("No workflows yet. Create one with /workflow create <objective>", Style::default().fg(theme.muted)))]
    } else if visible.is_empty() {
        vec![Line::from(Span::styled("No matching workflows", Style::default().fg(theme.muted)))]
    } else {
        let start = panel.selected.saturating_add(1).saturating_sub(viewport).min(visible.len().saturating_sub(viewport));
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
    let empty_state = panel.workflows.is_empty() || visible.is_empty();
    let paragraph = if empty_state { Paragraph::new(lines).wrap(Wrap { trim: true }) } else { Paragraph::new(lines) };
    frame.render_widget(paragraph, rows);
}

fn render_workflow_detail(frame: &mut ratatui::Frame<'_>, panel: &WorkflowPanel, area: Rect, theme: Theme) {
    let Some(workflow) = panel.selected_workflow() else {
        frame.render_widget(Paragraph::new("Select a workflow to view details").style(Style::default().fg(theme.muted)), area);
        return;
    };
    let footer_height = 1u16.min(area.height);
    let content_height = area.height.saturating_sub(footer_height);
    let content = Rect { height: content_height, ..area };
    let footer = Rect { y: area.y.saturating_add(content_height), height: footer_height, ..area };
    let content_width = usize::from(content.width.max(1));
    panel.detail_width.set(content_width);
    panel.detail_viewport_height.set(usize::from(content_height));
    let expanded = panel.expanded_agents.get(&workflow.id).cloned().unwrap_or_default();
    let lines = detail_display_lines(workflow, &expanded, theme, content_width, panel.now.get());
    panel.detail_display_rows.set(lines.len());
    panel.clamp_detail_scroll();
    let max_scroll = panel.detail_scroll_max();
    let scroll = panel.detail_scroll.get();
    let visible = lines.into_iter().skip(scroll).take(usize::from(content_height)).collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(visible), content);
    let footer_text = if max_scroll > 0 { format!("↑/↓ scroll · {}/{}", scroll.saturating_add(1), max_scroll.saturating_add(1)) } else { String::new() };
    frame.render_widget(Paragraph::new(footer_text).style(Style::default().fg(theme.dim)), footer);
}

fn detail_display_row_count(workflow: &WorkflowPanelSnapshot, expanded: &BTreeSet<String>, width: usize, now: u64) -> usize {
    detail_display_lines(workflow, expanded, crate::theme::DARK, width, now).len()
}

fn detail_display_lines(workflow: &WorkflowPanelSnapshot, expanded: &BTreeSet<String>, theme: Theme, width: usize, now: u64) -> Vec<Line<'static>> {
    detail_lines(workflow, expanded, theme, now).0.into_iter().flat_map(|line| wrap_styled_line(line, width.max(1))).collect()
}

/// Display-line offset of every foldable agent row (agent name), so the
/// panel can map a cursor (scroll) line back to the agent row it sits on.
/// Agents without an activity feed are not foldable and never appear here.
fn agent_row_display_offsets(workflow: &WorkflowPanelSnapshot, expanded: &BTreeSet<String>, theme: Theme, width: usize, now: u64) -> Vec<(usize, String)> {
    let (lines, rows) = detail_lines(workflow, expanded, theme, now);
    let mut offsets = Vec::new();
    let mut display_index = 0usize;
    for (index, line) in lines.into_iter().enumerate() {
        let wrapped = wrap_styled_line(line, width.max(1)).len();
        if let Some((_, name)) = rows.iter().find(|(row_index, _)| *row_index == index) {
            offsets.push((display_index, name.clone()));
        }
        display_index += wrapped;
    }
    offsets
}

/// Returns the detail's pre-wrap lines plus `(line index, agent name)` for
/// every foldable agent row (marker lines), so fold/unfold can map the
/// cursor back to an agent.
fn detail_lines(workflow: &WorkflowPanelSnapshot, expanded: &BTreeSet<String>, theme: Theme, now: u64) -> (Vec<Line<'static>>, Vec<(usize, String)>) {
    let mut lines = Vec::new();
    let mut agent_rows = Vec::new();

    // Meta (no header).
    lines.push(Line::from(Span::styled(sanitize(&workflow.name), Style::default().fg(theme.accent).add_modifier(Modifier::BOLD))));
    lines.push(field_line("Objective", &workflow.objective, theme));
    lines.push(Line::from(vec![Span::styled("Status     ", Style::default().fg(theme.dim)), Span::styled(workflow.status.as_str(), status_style(workflow.status, theme).add_modifier(Modifier::BOLD))]));
    if !workflow.worktree.label.is_empty() || !workflow.worktree.branch.is_empty() {
        let leaf = safe_path_label(&workflow.worktree.label);
        let branch = safe_branch_label(&workflow.worktree.branch);
        lines.push(Line::from(vec![
            Span::styled("Worktree   ", Style::default().fg(theme.dim)),
            Span::styled(leaf, Style::default().fg(theme.text)),
            Span::styled(format!(" · {}", branch), Style::default().fg(theme.dim)),
        ]));
    }
    match &workflow.supervisor {
        Some(supervisor) => lines.push(Line::from(vec![
            Span::styled("Supervisor ", Style::default().fg(theme.dim)),
            Span::styled(sanitize(&supervisor.name), Style::default().fg(theme.text)),
            Span::styled(format!(" · {}", sanitize(&supervisor.status)), Style::default().fg(theme.dim)),
        ])),
        None => lines.push(field_line("Supervisor", "Not started", theme)),
    }
    if workflow.status == WorkflowStatus::Planning {
        // Live planning progress: the supervisor turn is in flight, so the
        // detail must read as actively working even before the Todo DAG or
        // any worker job exists. The progress line reuses the T92 job-card
        // format (`◉ planning · <latest activity> · <elapsed>`); the bounded
        // activity feed below projects the supervisor's own turn (thinking
        // chunks, tool calls, IRC progress). Thinking is rendered in the
        // thinking color and collapsed to a bounded window — never the full
        // supervisor turn — so planning never reads as a static spinner nor
        // dumps unbounded reasoning into the page.
        push_section(&mut lines, "Planning", theme);
        let supervisor_state = workflow.supervisor.as_ref().map_or("not started", |supervisor| supervisor.status.as_str());
        let started_at = workflow.planning_started_at_ms;
        let last_activity_at = workflow.planning_activity.last().map(|entry| entry.at_ms).or(started_at);
        let elapsed_ms = started_at.and_then(|started| now.checked_sub(started));
        let (activity_text, activity_style) = workflow.planning_activity.last().map_or_else(
            || ("drafting the plan".to_owned(), Style::default().fg(theme.dim)),
            |entry| (sanitize(&entry.text), planning_activity_style(entry.kind, theme)),
        );
        let mut progress = vec![
            Span::styled("◉ ", Style::default().fg(theme.warning)),
            Span::styled("planning", Style::default().fg(theme.warning).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" · {activity_text}"), activity_style),
        ];
        if let Some(elapsed) = elapsed_ms {
            progress.push(Span::styled(format!(" · {}", crate::job_card_adapter::format_duration(elapsed)), Style::default().fg(theme.dim)));
        }
        progress.push(Span::styled(format!(" · supervisor {supervisor_state}"), Style::default().fg(theme.dim)));
        lines.push(Line::from(progress));
        // Bounded, collapsed activity feed (newest-last); older entries are
        // summarized in one dim line.
        if workflow.planning_activity.len() > PLANNING_ACTIVITY_FEED_LINES {
            lines.push(Line::from(Span::styled(
                format!("  … {} earlier", workflow.planning_activity.len() - PLANNING_ACTIVITY_FEED_LINES),
                Style::default().fg(theme.dim),
            )));
        }
        for entry in workflow.planning_activity.iter().rev().take(PLANNING_ACTIVITY_FEED_LINES).rev() {
            let label = planning_activity_label(entry.kind);
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default().fg(theme.dim)),
                Span::styled(format!("{label}{}", truncate(&entry.text, 120)), planning_activity_style(entry.kind, theme)),
            ]));
        }
        if let Some(task) = workflow.supervisor.as_ref().and_then(|supervisor| supervisor.task.as_deref()) {
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default().fg(theme.dim)),
                Span::styled(sanitize(task), Style::default().fg(theme.text)),
            ]));
        }
        // Bounded inactivity notice: after N seconds with no supervisor
        // activity the panel stops pretending the spinner is alive and tells
        // the user the workflow is stalled and how to leave it.
        if let Some(last) = last_activity_at {
            let idle_secs = now.saturating_sub(last) / 1000;
            if now != 0 && idle_secs * 1000 >= PLANNING_INACTIVITY_NOTICE_MS {
                lines.push(Line::from(vec![
                    Span::styled("⚠ ", Style::default().fg(theme.warning)),
                    Span::styled(format!("planning has shown no progress for {idle_secs}s — Esc to cancel"), Style::default().fg(theme.warning).add_modifier(Modifier::BOLD)),
                ]));
            }
        }
        lines.push(Line::default());
    }
    lines.push(Line::default());

    // Active tasks. Job-derived rows (live worker jobs) and agent-derived rows
    // (the supervisor's / subagents' current task summaries) share one origin
    // — the agent's latest job description — so the same task used to render
    // twice. Dedupe by the Todo-DAG task id: rows carrying the same id render
    // once (first occurrence keeps its position), while rows without a known
    // id always render, since summary text alone never proves identity.
    push_section(&mut lines, "Active tasks", theme);
    let mut seen_task_ids = HashSet::new();
    let mut active_tasks = Vec::new();
    for task in &workflow.active_tasks {
        if task.task_id.as_ref().is_none_or(|id| seen_task_ids.insert(id.clone())) {
            active_tasks.push(task.summary.as_str());
        }
    }
    for actor in workflow.subagents.iter().chain(workflow.supervisor.iter()) {
        let Some(task) = actor.task.as_deref() else { continue };
        if actor.task_id.as_ref().is_none_or(|id| seen_task_ids.insert(id.clone())) {
            active_tasks.push(task);
        }
    }
    if active_tasks.is_empty() {
        lines.push(dim_line("None · idle", theme));
    } else {
        for task in active_tasks.iter().take(6) {
            lines.push(Line::from(vec![Span::styled("● ", Style::default().fg(theme.accent)), Span::styled(sanitize(task), Style::default().fg(theme.text))]));
        }
    }
    lines.push(Line::default());

    // Agents: one row per workflow participant (supervisor first, then every
    // subagent). Rows with a bounded activity feed are collapsible — the
    // default collapsed row shows the fold marker plus name · status · task
    // in one line; expanding it reveals the bounded feed (delegated task
    // lifecycle + IRC, newest entries first) with older entries summarized.
    // Enter/Space on the row toggles the fold. Placed right after Active
    // tasks so the running state reads as who + what, with the full plan
    // below.
    push_section(&mut lines, "Agents", theme);
    if workflow.supervisor.is_none() && workflow.subagents.is_empty() {
        lines.push(dim_line("No agents yet", theme));
    } else {
        for agent in workflow.supervisor.iter().chain(workflow.subagents.iter()) {
            let foldable = !agent.activity.is_empty();
            if !foldable {
                lines.push(actor_line(Some(agent), "", theme));
                continue;
            }
            let is_expanded = expanded.contains(&agent.name);
            agent_rows.push((lines.len(), agent.name.clone()));
            lines.push(Line::from(vec![
                Span::styled(if is_expanded { "▾ " } else { "▸ " }, Style::default().fg(theme.accent)),
                Span::styled(sanitize(&agent.name), Style::default().fg(theme.text)),
                Span::styled(format!(" · {}{}", sanitize(&agent.status), agent.task.as_deref().map_or(String::new(), |task| format!(" · {}", sanitize(task)))), Style::default().fg(theme.dim)),
            ]));
            if is_expanded {
                if agent.activity.len() > AGENT_ACTIVITY_FEED_LINES {
                    lines.push(Line::from(Span::styled(
                        format!("  … {} earlier", agent.activity.len() - AGENT_ACTIVITY_FEED_LINES),
                        Style::default().fg(theme.dim),
                    )));
                }
                for entry in agent.activity.iter().rev().take(AGENT_ACTIVITY_FEED_LINES).rev() {
                    lines.push(agent_activity_line(entry, theme));
                }
            }
        }
    }
    lines.push(Line::default());

    // Todo DAG (shared module). Lean rows: phase + task content + status
    // bullet; in-progress tasks carry a compact subagent association so the
    // running mapping stays visible even with collapsed agent rows.
    push_section(&mut lines, "Todo DAG", theme);
    if workflow.status == WorkflowStatus::Planning && workflow.todo.is_empty() {
        if let Some(latest) = workflow.planning_activity.last() {
            // Live supervisor activity replaces the static placeholder: the
            // DAG is not built yet, but the turn is visibly moving.
            lines.push(Line::from(vec![
                Span::styled("◐ ", Style::default().fg(theme.warning)),
                Span::styled(
                    format!("{}{}", planning_activity_label(latest.kind), truncate(&latest.text, 120)),
                    planning_activity_style(latest.kind, theme),
                ),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled("◐ ", Style::default().fg(theme.warning)),
                Span::styled("Planning…", Style::default().fg(theme.warning).add_modifier(Modifier::BOLD)),
            ]));
            lines.push(dim_line("The supervisor is drafting the plan", theme));
        }
    } else {
        let agent_by_task_id = workflow
            .subagents
            .iter()
            .chain(workflow.supervisor.iter())
            .filter_map(|actor| actor.task_id.as_ref().map(|task_id| (task_id.clone(), actor.name.clone())))
            .collect::<HashMap<_, _>>();
        lines.extend(workflow_todo_dag_lines(&workflow.todo, &agent_by_task_id, theme));
    }
    lines.push(Line::default());

    // Recent IRC: the workflow group's live message stream (subagent ⇄
    // subagent included), newest messages first with sender, bounded text and
    // a wall-clock timestamp; older messages collapse to one summary line.
    push_section(&mut lines, "Recent IRC", theme);
    if workflow.recent_irc.is_empty() {
        lines.push(dim_line("No recent messages", theme));
    } else {
        if workflow.recent_irc.len() > IRC_FEED_LINES {
            lines.push(Line::from(Span::styled(
                format!("  … {} earlier", workflow.recent_irc.len() - IRC_FEED_LINES),
                Style::default().fg(theme.dim),
            )));
        }
        for message in workflow.recent_irc.iter().rev().take(IRC_FEED_LINES).rev() {
            lines.push(Line::from(vec![
                Span::styled(format!("{}: ", sanitize(&message.sender)), Style::default().fg(theme.accent)),
                Span::styled(truncate(&message.text, 120), Style::default().fg(theme.text)),
                Span::styled(format!(" · {}", format_clock(message.at_ms)), Style::default().fg(theme.dim)),
            ]));
        }
    }
    lines.push(Line::default());

    // Integration — the final DAG step: merge the workflow worktree back to
    // the source branch (auto-integrate on todo completion, or a manual
    // /workflow integrate). The step row transitions "after todos complete"
    // → "Integrating…" → "Integrated" / conflict resolution.
    push_section(&mut lines, "Integration", theme);
    let step_line = match workflow.integration.step {
        WorkflowIntegrateStep::Pending => Line::from(vec![
            Span::styled("○ ", Style::default().fg(theme.muted)),
            Span::styled("Integrate · after todos complete", Style::default().fg(theme.muted)),
        ]),
        WorkflowIntegrateStep::Integrating => Line::from(vec![
            Span::styled("⇄ ", Style::default().fg(theme.warning)),
            Span::styled("Integrating…", Style::default().fg(theme.warning).add_modifier(Modifier::BOLD)),
        ]),
        WorkflowIntegrateStep::Applied => Line::from(vec![
            Span::styled("✓ ", Style::default().fg(theme.success)),
            Span::styled("Integrated", Style::default().fg(theme.success).add_modifier(Modifier::BOLD)),
        ]),
        WorkflowIntegrateStep::Conflicted => Line::from(vec![
            Span::styled("! ", Style::default().fg(theme.error)),
            Span::styled("Resolve conflicts · integrate", Style::default().fg(theme.error).add_modifier(Modifier::BOLD)),
        ]),
    };
    lines.push(step_line);
    let conflicted = workflow.status == WorkflowStatus::Conflicted;
    if conflicted {
        lines.push(Line::from(Span::styled("CONFLICTED", Style::default().fg(theme.error).add_modifier(Modifier::BOLD))));
    }
    let integration_style = if conflicted { Style::default().fg(theme.error) } else { Style::default().fg(theme.text) };
    if !workflow.integration.summary.is_empty() {
        lines.push(Line::from(Span::styled(sanitize(&workflow.integration.summary), integration_style)));
    }
    if workflow.integration.files_changed > 0 || workflow.integration.insertions > 0 || workflow.integration.deletions > 0 {
        lines.push(Line::from(Span::styled(format!("{} files · +{} −{}", workflow.integration.files_changed, workflow.integration.insertions, workflow.integration.deletions), Style::default().fg(theme.dim))));
    }
    for conflict in workflow.integration.conflicts.iter().take(3) {
        lines.push(Line::from(Span::styled(format!("! {}", safe_path_label(conflict)), Style::default().fg(theme.error))));
    }

    (lines, agent_rows)
}

fn push_section(lines: &mut Vec<Line<'static>>, label: &str, theme: Theme) { lines.push(Line::from(Span::styled(label.to_owned(), Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)))); }
fn field_line(label: &str, value: &str, theme: Theme) -> Line<'static> { Line::from(vec![Span::styled(format!("{label:<11}"), Style::default().fg(theme.dim)), Span::styled(sanitize(value), Style::default().fg(theme.text))]) }
fn dim_line(value: &str, theme: Theme) -> Line<'static> { Line::from(Span::styled(value.to_owned(), Style::default().fg(theme.dim))) }
fn actor_line(actor: Option<&WorkflowActorSnapshot>, empty: &str, theme: Theme) -> Line<'static> { actor.map_or_else(|| dim_line(empty, theme), |actor| { let task = actor.task.as_deref().map_or(String::new(), |task| format!(" · {}", sanitize(task))); Line::from(vec![Span::styled(sanitize(&actor.name), Style::default().fg(theme.text)), Span::styled(format!(" · {}{task}", sanitize(&actor.status)), Style::default().fg(theme.dim))]) }) }

/// Map a pi-coding agent activity entry onto the panel's snapshot type.
fn agent_activity_snapshot(entry: &pi_coding::WorkflowAgentActivity) -> WorkflowAgentActivitySnapshot {
    WorkflowAgentActivitySnapshot {
        at_ms: entry.at_ms,
        kind: match entry.kind {
            pi_coding::WorkflowAgentActivityKind::Task => WorkflowAgentActivityKind::Task,
            pi_coding::WorkflowAgentActivityKind::Irc => WorkflowAgentActivityKind::Irc,
        },
        text: entry.text.clone(),
    }
}

/// One bounded entry of an expanded agent row: task lifecycle entries take
/// the job-card running accent, IRC entries the plain text color — the same
/// kind vocabulary as the planning feed.
fn agent_activity_line(entry: &WorkflowAgentActivitySnapshot, theme: Theme) -> Line<'static> {
    let (label, style) = match entry.kind {
        WorkflowAgentActivityKind::Task => ("task · ", Style::default().fg(theme.accent)),
        WorkflowAgentActivityKind::Irc => ("irc · ", Style::default().fg(theme.text)),
    };
    Line::from(vec![
        Span::styled("  ", Style::default().fg(theme.dim)),
        Span::styled(format!("{label}{}", truncate(&entry.text, 120)), style),
    ])
}

/// Epoch millis → `HH:MM:SS` wall-clock display for Recent IRC rows.
fn format_clock(millis: u64) -> String {
    let total_seconds = millis / 1_000;
    let seconds = total_seconds % 60;
    let minutes = (total_seconds / 60) % 60;
    let hours = (total_seconds / 3_600) % 24;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}
fn todo_counts(phases: &[TodoPhase]) -> (usize, usize) { let tasks = phases.iter().flat_map(|phase| &phase.tasks).collect::<Vec<_>>(); (tasks.iter().filter(|task| task.ready).count(), tasks.len()) }
fn status_marker(status: WorkflowStatus) -> &'static str { match status { WorkflowStatus::Queued => "○", WorkflowStatus::Planning => "◐", WorkflowStatus::Running => "●", WorkflowStatus::Paused => "Ⅱ", WorkflowStatus::Integrating => "⇄", WorkflowStatus::Completed => "✓", WorkflowStatus::Failed => "!", WorkflowStatus::Cancelled => "×", WorkflowStatus::Conflicted => "!" } }
fn agent_status_label(status: pi_coding::AgentStatus) -> &'static str { match status { pi_coding::AgentStatus::Queued => "queued", pi_coding::AgentStatus::Running => "running", pi_coding::AgentStatus::Idle => "idle", pi_coding::AgentStatus::Parked => "parked", pi_coding::AgentStatus::Aborted => "aborted" } }
fn status_style(status: WorkflowStatus, theme: Theme) -> Style { Style::default().fg(match status { WorkflowStatus::Queued | WorkflowStatus::Paused | WorkflowStatus::Cancelled => theme.muted, WorkflowStatus::Planning | WorkflowStatus::Integrating => theme.warning, WorkflowStatus::Running => theme.accent, WorkflowStatus::Completed => theme.success, WorkflowStatus::Failed | WorkflowStatus::Conflicted => theme.error }) }

/// Activity color by kind: thinking stays in the thinking color (never the
/// accent), tool calls take the job-card running accent, and reply text / IRC
/// progress use the plain text color.
fn planning_activity_style(kind: pi_coding::WorkflowSupervisorActivityKind, theme: Theme) -> Style {
    Style::default().fg(match kind {
        pi_coding::WorkflowSupervisorActivityKind::Thinking => theme.thinking_text,
        pi_coding::WorkflowSupervisorActivityKind::Tool => theme.accent,
        pi_coding::WorkflowSupervisorActivityKind::Text | pi_coding::WorkflowSupervisorActivityKind::Irc => theme.text,
    })
}

/// Feed prefix for one supervisor activity kind (`thinking · `, `tool · `,
/// `irc · `; reply text carries no prefix). Shared with the composer status
/// line, which renders the wave's latest supervisor activity verbatim.
pub(crate) fn planning_activity_label(kind: pi_coding::WorkflowSupervisorActivityKind) -> &'static str {
    match kind {
        pi_coding::WorkflowSupervisorActivityKind::Thinking => "thinking · ",
        pi_coding::WorkflowSupervisorActivityKind::Tool => "tool · ",
        pi_coding::WorkflowSupervisorActivityKind::Irc => "irc · ",
        pi_coding::WorkflowSupervisorActivityKind::Text => "",
    }
}

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
    use pi_coding::{TodoBlockedReason, TodoItem, TodoStatus};
    use ratatui::{Terminal, backend::TestBackend};

    fn task(id: &str, content: &str, status: TodoStatus, ready: bool, depends_on: &[&str]) -> TodoItem {
        TodoItem { id: id.to_owned(), content: content.to_owned(), status, depends_on: depends_on.iter().map(|value| (*value).to_owned()).collect(), ready, blocked_by: if ready { Vec::new() } else { depends_on.iter().map(|dependency| TodoBlockedReason { task_id: (*dependency).to_owned(), content: "dependency".to_owned(), status: TodoStatus::InProgress }).collect() }, agent: None }
    }

    fn workflow(id: &str, name: &str, status: WorkflowStatus) -> WorkflowPanelSnapshot {
        WorkflowPanelSnapshot {
            id: id.to_owned(), generation: 1, name: name.to_owned(), objective: "Ship isolated workflow orchestration safely".to_owned(), status,
            todo: vec![TodoPhase { name: "Build".to_owned(), tasks: vec![task("design-id", "Design protocol", TodoStatus::Completed, false, &[]), task("render-id", "Render page", TodoStatus::InProgress, true, &["design-id"])] }],
            supervisor: Some(WorkflowActorSnapshot { name: "Supervisor".to_owned(), status: "running".to_owned(), task: Some("Coordinate workers".to_owned()), task_id: None, activity: Vec::new() }),
            subagents: vec![WorkflowActorSnapshot { name: "PanelWorker".to_owned(), status: "running".to_owned(), task: Some("Render page".to_owned()), task_id: None, activity: Vec::new() }],
            recent_irc: vec![WorkflowIrcSnapshot { sender: "Supervisor".to_owned(), text: "Integrate after focused tests".to_owned(), at_ms: 0 }],
            active_tasks: vec![WorkflowActiveTaskSnapshot { task_id: None, summary: "Run focused tests".to_owned() }],
            worktree: WorkflowWorktreeSnapshot { label: "<workspace>/worktrees/feature-panel".to_owned(), branch: "rpi/workflow/feature-panel".to_owned() },
            integration: WorkflowIntegrationSnapshot { step: WorkflowIntegrateStep::Pending, summary: "Ready after review".to_owned(), files_changed: 3, insertions: 80, deletions: 4, conflicts: Vec::new() },
            planning_activity: Vec::new(),
            planning_started_at_ms: None,
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
    fn split_renders_list_and_detail_without_enter() {
        let mut panel = WorkflowPanel::new(vec![workflow("opaque-one", "Panel foundation", WorkflowStatus::Running), workflow("opaque-two", "RPC transport", WorkflowStatus::Queued)]);
        assert_eq!(panel.focus(), WorkflowPanelFocus::List);
        let text = render(&panel, 140, 36).join("\n");
        for needle in ["Panel foundation", "RPC transport", "running", "queued"] { assert!(text.contains(needle), "missing list {needle:?}\n{text}"); }
        for needle in ["Objective", "Ship isolated workflow orchestration safely", "Active tasks", "Todo DAG"] { assert!(text.contains(needle), "missing detail {needle:?}\n{text}"); }
        // Todo DAG counts header from the shared module.
        assert!(text.contains("completed") && text.contains("open") && text.contains("active") && text.contains("blocked"), "missing counts header\n{text}");
        // depends_on line for the ready render task.
        assert!(text.contains("depends_on: Design protocol"), "missing depends_on line\n{text}");
    }

    #[test]
    fn escape_and_backspace_follow_detail_then_list_hierarchy() {
        let mut panel = WorkflowPanel::new(vec![workflow("one", "Alpha", WorkflowStatus::Running)]);
        assert_eq!(panel.handle_key(key(KeyCode::Enter)), WorkflowPanelResult::Handled);
        assert_eq!(panel.focus(), WorkflowPanelFocus::Detail);
        assert_eq!(panel.handle_key(key(KeyCode::Esc)), WorkflowPanelResult::Handled);
        assert_eq!(panel.focus(), WorkflowPanelFocus::List);
        assert_eq!(panel.handle_key(key(KeyCode::Enter)), WorkflowPanelResult::Handled);
        assert_eq!(panel.focus(), WorkflowPanelFocus::Detail);
        assert_eq!(panel.handle_key(key(KeyCode::Backspace)), WorkflowPanelResult::Handled);
        assert_eq!(panel.focus(), WorkflowPanelFocus::List);
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
        beta.recent_irc.push(WorkflowIrcSnapshot { sender: "PanelWorker".to_owned(), text: "Live snapshot arrived".to_owned(), at_ms: 0 });
        panel.replace(vec![alpha.clone(), beta]);
        assert_eq!(panel.focus(), WorkflowPanelFocus::Detail);
        assert_eq!(panel.selected_workflow().unwrap().name, "Beta");
        let text = render(&panel, 110, 38).join("\n");
        for needle in ["Live replacement task", "Live worker task", "Live snapshot arrived"] { assert!(text.contains(needle), "missing live value {needle:?}\n{text}"); }
    }

    #[test]
    fn removing_selected_detail_keeps_focus_and_clamps_selection() {
        let mut panel = WorkflowPanel::new(vec![workflow("one", "Alpha", WorkflowStatus::Running), workflow("two", "Beta", WorkflowStatus::Paused)]);
        panel.handle_key(key(KeyCode::Down));
        panel.handle_key(key(KeyCode::Enter));
        panel.replace(vec![workflow("one", "Alpha", WorkflowStatus::Running)]);
        assert_eq!(panel.focus(), WorkflowPanelFocus::Detail);
        assert_eq!(panel.selected_workflow().unwrap().name, "Alpha");
        let text = render(&panel, 80, 20).join("\n");
        assert!(text.contains("Alpha"));
        assert!(!text.contains("Beta"));
    }

    #[test]
    fn detail_redacts_opaque_ids_and_repository_paths() {
        let mut item = workflow("550e8400-e29b-41d4-a716-446655440000", "Integration", WorkflowStatus::Conflicted);
        item.worktree.label = "<workspace>/.git/worktrees/secret-worktree".to_owned();
        item.integration.step = WorkflowIntegrateStep::Conflicted;
        item.integration.summary = "Manual resolution required".to_owned();
        item.integration.conflicts = vec!["<workspace>/src/application.rs".to_owned()];
        item.worktree.branch = "<workspace>/secret-branch".to_owned();
        let mut panel = WorkflowPanel::new(vec![item]);
        panel.handle_key(key(KeyCode::Enter));
        assert_eq!(panel.focus(), WorkflowPanelFocus::Detail);
        let text = render(&panel, 90, 52).join("\n");
        for needle in ["CONFLICTED", "Manual resolution required", "secret-worktree", "! application.rs"] { assert!(text.contains(needle), "missing {needle:?}\n{text}"); }
        assert!(text.contains("secret-branch"));
        for forbidden in ["550e8400", "<workspace>", ".git/worktrees"] { assert!(!text.contains(forbidden), "leaked {forbidden:?}\n{text}"); }
    }

    #[test]
    fn detail_renders_integrate_step_across_lifecycle_states() {
        // Pending: todos still open, the final DAG step waits for completion.
        let mut pending = workflow("pending", "Pending", WorkflowStatus::Running);
        pending.integration = WorkflowIntegrationSnapshot { step: WorkflowIntegrateStep::Pending, summary: "Ready after review".to_owned(), files_changed: 3, insertions: 80, deletions: 4, conflicts: Vec::new() };
        let mut panel = WorkflowPanel::new(vec![pending]);
        panel.handle_key(key(KeyCode::Enter));
        let text = render(&panel, 110, 38).join("\n");
        assert!(text.contains("Integrate · after todos complete"), "pending step row missing\n{text}");

        // Integrating: todos settled, the auto-integrate merge is running.
        let mut integrating = workflow("integrating", "Integrating", WorkflowStatus::Completed);
        integrating.integration = WorkflowIntegrationSnapshot { step: WorkflowIntegrateStep::Integrating, ..WorkflowIntegrationSnapshot::default() };
        let mut panel = WorkflowPanel::new(vec![integrating]);
        panel.handle_key(key(KeyCode::Enter));
        let text = render(&panel, 110, 38).join("\n");
        assert!(text.contains("Integrating…"), "integrating step row missing\n{text}");

        // Applied: the merge landed back on the source branch.
        let mut applied = workflow("applied", "Applied", WorkflowStatus::Completed);
        applied.integration = WorkflowIntegrationSnapshot { step: WorkflowIntegrateStep::Applied, ..WorkflowIntegrationSnapshot::default() };
        let mut panel = WorkflowPanel::new(vec![applied]);
        panel.handle_key(key(KeyCode::Enter));
        let text = render(&panel, 110, 38).join("\n");
        assert!(text.contains("Integrated"), "applied step row missing\n{text}");
        assert!(!text.contains("0 files · +0 −0"), "applied state must not render empty diff noise\n{text}");

        // Conflicted: the merge needs manual resolution.
        let mut conflicted = workflow("conflicted", "Conflicted", WorkflowStatus::Conflicted);
        conflicted.integration = WorkflowIntegrationSnapshot { step: WorkflowIntegrateStep::Conflicted, summary: "Manual resolution required".to_owned(), conflicts: vec!["<workspace>/src/application.rs".to_owned()], ..WorkflowIntegrationSnapshot::default() };
        let mut panel = WorkflowPanel::new(vec![conflicted]);
        panel.handle_key(key(KeyCode::Enter));
        let text = render(&panel, 110, 38).join("\n");
        for needle in ["Resolve conflicts · integrate", "CONFLICTED", "Manual resolution required", "! application.rs"] {
            assert!(text.contains(needle), "conflicted step missing {needle:?}\n{text}");
        }
        assert!(!text.contains("<workspace>"), "conflicted step must redact repository paths\n{text}");
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
            supervisor: Some(pi_coding::WorkflowSupervisorDetail { display_name: "Supervisor".to_owned(), status: pi_coding::AgentStatus::Running, task_summary: Some("Direct workers".to_owned()), todo_task_id: None, activity: vec![pi_coding::WorkflowAgentActivity { at_ms: 9, kind: pi_coding::WorkflowAgentActivityKind::Irc, text: "Worker: progress".to_owned() }] }),
            subagents: vec![pi_coding::WorkflowSubagentDetail { display_name: "Worker".to_owned(), status: pi_coding::AgentStatus::Idle, task_summary: Some("Review changes".to_owned()), todo_task_id: Some("render-id".to_owned()), activity: vec![pi_coding::WorkflowAgentActivity { at_ms: 7, kind: pi_coding::WorkflowAgentActivityKind::Task, text: "Review changes".to_owned() }] }],
            jobs: vec![pi_coding::WorkflowRuntimeJobDetail { display_name: "Worker".to_owned(), status: pi_coding::JobStatus::Running, task_summary: Some("Run integration".to_owned()), todo_task_id: Some("render-id".to_owned()), created_at_ms: 3, started_at_ms: Some(4), finished_at_ms: None }],
            irc: vec![pi_coding::WorkflowIrcMessage { from: "Worker".to_owned(), to: "Supervisor".to_owned(), body: "Live update".to_owned(), timestamp_ms: 5 }],
            supervisor_activity: vec![pi_coding::WorkflowSupervisorActivity { at_ms: 6, kind: pi_coding::WorkflowSupervisorActivityKind::Thinking, text: "weigh options".to_owned() }],
            planning_started_at_ms: Some(4),
        };
        let projected = WorkflowPanelSnapshot::from_runtime_detail(&detail, &snapshot);
        assert_eq!(projected.name, "Live runtime workflow");
        assert_eq!(projected.supervisor.as_ref().unwrap().status, "running");
        assert_eq!(projected.supervisor.as_ref().unwrap().task.as_deref(), Some("Direct workers"));
        assert_eq!(projected.subagents[0].task.as_deref(), Some("Review changes"));
        // The job's Todo-DAG task id flows through both the Active tasks row
        // and the owning agent's summary so the panel can dedupe by identity.
        assert_eq!(projected.subagents[0].task_id.as_deref(), Some("render-id"));
        assert_eq!(projected.active_tasks, vec![WorkflowActiveTaskSnapshot { task_id: Some("render-id".to_owned()), summary: "Run integration".to_owned() }]);
        // Per-agent activity feeds flow into the actor rows for the fold view.
        assert_eq!(projected.supervisor.as_ref().unwrap().activity.len(), 1);
        assert_eq!(projected.supervisor.as_ref().unwrap().activity[0].kind, WorkflowAgentActivityKind::Irc);
        assert_eq!(projected.supervisor.as_ref().unwrap().activity[0].text, "Worker: progress");
        assert_eq!(projected.subagents[0].activity.len(), 1);
        assert_eq!(projected.subagents[0].activity[0].kind, WorkflowAgentActivityKind::Task);
        // The IRC projection carries the delivery timestamp for the clock row.
        assert_eq!(projected.recent_irc[0].text, "Live update");
        assert_eq!(projected.recent_irc[0].at_ms, 5);
        assert_eq!(projected.planning_started_at_ms, Some(4));
        assert_eq!(projected.planning_activity.len(), 1);
        assert_eq!(projected.planning_activity[0].kind, pi_coding::WorkflowSupervisorActivityKind::Thinking);
        assert_eq!(projected.planning_activity[0].text, "weigh options");
    }

    /// Concatenate the span text of the lines between the `start` header line
    /// (exclusive) and the `end` header line (exclusive).
    fn section_text(lines: &[Line<'static>], start: &str, end: &str) -> String {
        let find = |needle: &str| lines.iter().position(|line| line.spans.iter().any(|span| span.content.contains(needle))).unwrap_or_else(|| panic!("missing section header {needle:?}"));
        let start_index = find(start);
        let end_index = find(end);
        lines[start_index + 1..end_index].iter().flat_map(|line| line.spans.iter().map(|span| span.content.as_ref())).collect::<Vec<_>>().join(" ")
    }

    fn count_in_section(lines: &[Line<'static>], start: &str, end: &str, needle: &str) -> usize {
        section_text(lines, start, end).matches(needle).count()
    }

    #[test]
    fn active_tasks_dedupe_same_task_id_across_job_and_actor_rows() {
        // Regression for the double-render bug: job-derived Active tasks rows
        // and agent-derived rows share one origin (the agent's latest job
        // description), so the same task used to appear twice. Rows carrying
        // the same Todo-DAG task id must collapse into one; the Todo DAG
        // section below is separate and legitimately lists the task again.
        let summary = "Inventory the current workspace and related references";
        let mut item = workflow("dedupe", "Dedupe", WorkflowStatus::Running);
        item.todo = vec![TodoPhase { name: "Build".to_owned(), tasks: vec![task("render-id", summary, TodoStatus::InProgress, true, &[])] }];
        item.active_tasks = vec![WorkflowActiveTaskSnapshot { task_id: Some("render-id".to_owned()), summary: summary.to_owned() }];
        item.supervisor = Some(WorkflowActorSnapshot { name: "Supervisor".to_owned(), status: "running".to_owned(), task: Some(summary.to_owned()), task_id: Some("render-id".to_owned()), activity: Vec::new() });
        item.subagents = Vec::new();

        let rendered = detail_lines(&item, &BTreeSet::new(), crate::theme::DARK, 1_000_000).0;
        assert_eq!(count_in_section(&rendered, "Active tasks", "Agents", summary), 1, "same task id must render once in Active tasks");
        // The Todo DAG section below is separate and legitimately lists the
        // task again — as a lean row (bullet + content) with the live
        // subagent association, never the opaque task id.
        let dag = section_text(&rendered, "Todo DAG", "Recent IRC").split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(dag.contains("● Inventory the current workspace and related references · Supervisor"), "the Todo DAG section must list the active task with its subagent\n{dag}");
        assert!(!dag.contains("render-id"), "Todo DAG rows must not render task ids\n{dag}");
    }

    #[test]
    fn active_tasks_dedupe_by_id_not_content() {
        // Distinct tasks with identical text render once each: dedupe by the
        // Todo-DAG task id, never by the summary text.
        let summary = "Render the page and verify layout";
        let mut item = workflow("distinct", "Distinct", WorkflowStatus::Running);
        item.todo = Vec::new();
        item.active_tasks = vec![
            WorkflowActiveTaskSnapshot { task_id: Some("render-a".to_owned()), summary: summary.to_owned() },
            WorkflowActiveTaskSnapshot { task_id: Some("render-b".to_owned()), summary: summary.to_owned() },
            WorkflowActiveTaskSnapshot { task_id: None, summary: summary.to_owned() },
        ];
        item.subagents = Vec::new();
        item.supervisor = None;

        let rendered = detail_lines(&item, &BTreeSet::new(), crate::theme::DARK, 1_000_000).0;
        assert_eq!(count_in_section(&rendered, "Active tasks", "Agents", summary), 3, "distinct ids with identical text must each render");
    }

    #[test]
    fn active_tasks_dedupe_repeated_jobs_for_same_todo_task() {
        // Two live jobs backed by the same todo task (retry/redelegation)
        // render a single row; first occurrence keeps its position.
        let summary = "Run integration";
        let mut item = workflow("dup-jobs", "Dup jobs", WorkflowStatus::Running);
        item.todo = Vec::new();
        item.active_tasks = vec![
            WorkflowActiveTaskSnapshot { task_id: Some("render-id".to_owned()), summary: summary.to_owned() },
            WorkflowActiveTaskSnapshot { task_id: Some("render-id".to_owned()), summary: summary.to_owned() },
        ];
        item.subagents = Vec::new();
        item.supervisor = None;

        let rendered = detail_lines(&item, &BTreeSet::new(), crate::theme::DARK, 1_000_000).0;
        assert_eq!(count_in_section(&rendered, "Active tasks", "Agents", summary), 1);
    }

    #[test]
    fn todo_dag_rows_are_lean_with_subagent_association() {
        // The Todo DAG section renders phase + task content + status bullet
        // only: no `[task-…]` id suffix, no status words, no `ready` marker
        // line. In-progress tasks carry a compact association to the
        // subagent handling them (live actor mapping by task id), so the
        // running mapping stays visible even with collapsed agent rows.
        let mut item = workflow("lean", "Lean", WorkflowStatus::Running);
        item.todo = vec![TodoPhase {
            name: "Build".to_owned(),
            tasks: vec![
                task("design-id", "Design protocol", TodoStatus::Completed, false, &[]),
                task("render-id", "Render page", TodoStatus::InProgress, true, &["design-id"]),
                task("ship-id", "Ship the panel", TodoStatus::Pending, true, &[]),
            ],
        }];
        item.subagents = vec![WorkflowActorSnapshot {
            name: "PanelWorker".to_owned(),
            status: "running".to_owned(),
            task: Some("Render page".to_owned()),
            task_id: Some("render-id".to_owned()),
            activity: Vec::new(),
        }];
        item.supervisor = None;
        let rendered = detail_lines(&item, &BTreeSet::new(), crate::theme::DARK, 1_000_000).0;
        let dag = section_text(&rendered, "Todo DAG", "Recent IRC").split_whitespace().collect::<Vec<_>>().join(" ");
        // Phase header + content + status bullet for every row.
        assert!(dag.contains("Build"), "phase header missing\n{dag}");
        assert!(dag.contains("✓ Design protocol"), "completed bullet missing\n{dag}");
        assert!(dag.contains("● Render page"), "in-progress bullet missing\n{dag}");
        assert!(dag.contains("○ Ship the panel"), "pending bullet missing\n{dag}");
        // No task ids, no ready marker, no status words.
        for forbidden in ["design-id", "render-id", "ship-id", "ready", "in progress", "pending"] {
            assert!(!dag.contains(forbidden), "Todo DAG must not render {forbidden:?}\n{dag}");
        }
        // In-progress task shows its live subagent; pending/completed stay bare.
        assert!(dag.contains("● Render page · PanelWorker"), "in-progress task must show its subagent\n{dag}");
        assert!(!dag.contains("Design protocol · PanelWorker"), "completed task must not carry an association\n{dag}");
        // Agents sit right after Active tasks, before the plan: who + what
        // reads front and center.
        let active = rendered.iter().position(|line| line.spans.iter().any(|span| span.content.contains("Active tasks"))).expect("Active tasks");
        let agents = rendered.iter().position(|line| line.spans.iter().any(|span| span.content.contains("Agents"))).expect("Agents");
        let todo_dag = rendered.iter().position(|line| line.spans.iter().any(|span| span.content.contains("Todo DAG"))).expect("Todo DAG");
        assert!(active < agents && agents < todo_dag, "Agents must be front and center: Active tasks < Agents < Todo DAG");
    }

    #[test]
    fn planning_snapshot_shows_progress_indicator_not_empty_todo_placeholder() {
        let mut item = workflow("planning", "Planning phase", WorkflowStatus::Planning);
        item.todo = Vec::new();
        item.subagents = Vec::new();
        item.active_tasks = Vec::new();
        item.supervisor = Some(WorkflowActorSnapshot { name: "Main".to_owned(), status: "running".to_owned(), task: Some("Planning: Ship isolated workflow orchestration safely".to_owned()), task_id: None, activity: Vec::new() });
        item.recent_irc = vec![WorkflowIrcSnapshot { sender: "Main".to_owned(), text: "Drafting the Todo DAG".to_owned(), at_ms: 0 }];
        let mut panel = WorkflowPanel::new(vec![item]);
        panel.handle_key(key(KeyCode::Enter));
        let text = render(&panel, 110, 38).join("\n");
        for needle in ["Planning", "◉ planning", "Planning: Ship isolated workflow orchestration safely", "Drafting the Todo DAG", "supervisor running"] { assert!(text.contains(needle), "missing planning indicator {needle:?}\n{text}"); }
        assert!(!text.contains("No phases or tasks yet"), "planning must replace the empty todo placeholder\n{text}");
        assert!(!text.contains("None · idle"), "planning supervisor must not read as idle\n{text}");
    }

    fn agent_feed_workflow(id: &str, entries: Vec<WorkflowAgentActivitySnapshot>) -> WorkflowPanelSnapshot {
        let mut item = workflow(id, "Agents", WorkflowStatus::Running);
        item.supervisor = None;
        item.subagents = vec![WorkflowActorSnapshot {
            name: "PanelWorker".to_owned(),
            status: "running".to_owned(),
            task: Some("Render page".to_owned()),
            task_id: None,
            activity: entries,
        }];
        item
    }

    fn agent_feed_entries(count: usize) -> Vec<WorkflowAgentActivitySnapshot> {
        (1..=count)
            .map(|i| WorkflowAgentActivitySnapshot { at_ms: i as u64 * 1_000, kind: WorkflowAgentActivityKind::Task, text: format!("task-entry-{i}") })
            .collect()
    }

    /// Scroll the foldable agent row to the top of the detail viewport (the
    /// panel's cursor is the top visible line). Re-issued after every render
    /// because a viewport that fits the content clamps the scroll to 0.
    fn scroll_to_agent(panel: &mut WorkflowPanel) {
        let workflow = panel.selected_workflow().expect("selected");
        let expanded = panel.expanded_agents.get(&workflow.id).cloned().unwrap_or_default();
        let offsets = agent_row_display_offsets(workflow, &expanded, crate::theme::DARK, panel.detail_width.get().max(1), 0);
        assert_eq!(offsets.len(), 1, "one foldable agent row");
        panel.detail_scroll.set(offsets[0].0);
    }

    #[test]
    fn agent_rows_fold_and_unfold_bounded_activity_feed() {
        // The Agents section renders one collapsible row per subagent with a
        // fold marker; rows start collapsed (one line each), Enter/Space on
        // the row expands the bounded feed (newest entries first, older ones
        // summarized), and toggles back to the marker row.
        let mut panel = WorkflowPanel::new(vec![agent_feed_workflow("fold", agent_feed_entries(7))]);
        panel.handle_key(key(KeyCode::Enter));
        assert_eq!(panel.focus(), WorkflowPanelFocus::Detail);
        // Collapsed by default: marker row only, no feed, no earlier line.
        let collapsed_text = render(&panel, 110, 38).join("\n");
        assert!(collapsed_text.contains("▸ PanelWorker · running · Render page"), "collapsed row missing\n{collapsed_text}");
        assert!(!collapsed_text.contains("task-entry-"), "collapsed row must hide the feed\n{collapsed_text}");
        assert!(!collapsed_text.contains("earlier"), "no feed must mean no earlier line\n{collapsed_text}");

        // Scroll to the agent row and press Enter: the feed expands.
        scroll_to_agent(&mut panel);
        assert_eq!(panel.handle_key(key(KeyCode::Enter)), WorkflowPanelResult::Handled);
        let expanded_text = render(&panel, 110, 38).join("\n");
        assert!(expanded_text.contains("▾ PanelWorker · running · Render page"), "expanded marker missing\n{expanded_text}");
        for needle in ["task-entry-7", "task-entry-6", "task-entry-5", "task-entry-4", "task-entry-3"] {
            assert!(expanded_text.contains(needle), "missing feed entry {needle}\n{expanded_text}");
        }
        assert!(!expanded_text.contains("task-entry-2"), "bounded feed must drop old entries\n{expanded_text}");
        assert!(!expanded_text.contains("task-entry-1"), "bounded feed must drop old entries\n{expanded_text}");
        assert!(expanded_text.contains("… 2 earlier"), "older entries must be summarized\n{expanded_text}");

        // Enter again on the row folds back to the marker line.
        scroll_to_agent(&mut panel);
        panel.handle_key(key(KeyCode::Enter));
        let recollapsed = render(&panel, 110, 38).join("\n");
        assert!(recollapsed.contains("▸ PanelWorker"), "second Enter must fold again\n{recollapsed}");
        assert!(!recollapsed.contains("task-entry-"), "folded row must hide the feed\n{recollapsed}");

        // Space toggles exactly like Enter.
        scroll_to_agent(&mut panel);
        panel.handle_key(key(KeyCode::Char(' ')));
        assert!(render(&panel, 110, 38).join("\n").contains("▾ PanelWorker"), "Space must unfold like Enter");
        scroll_to_agent(&mut panel);
        panel.handle_key(key(KeyCode::Char(' ')));
        let respaced = render(&panel, 110, 38).join("\n");
        assert!(respaced.contains("▸ PanelWorker"), "Space must fold like Enter\n{respaced}");
        assert!(!respaced.contains("task-entry-"), "Space-folded row must hide the feed\n{respaced}");

        // Enter on a non-agent line (the top of the detail) is a no-op.
        panel.detail_scroll.set(0);
        panel.handle_key(key(KeyCode::Enter));
        assert!(render(&panel, 110, 38).join("\n").contains("▸ PanelWorker"), "Enter on a non-agent line must not toggle");
    }

    #[test]
    fn agent_rows_without_activity_are_plain_rows() {
        // Agents with no live feed keep the coarse one-line status row — no
        // fold marker, so Enter on them is a no-op, not a broken expansion.
        let mut panel = WorkflowPanel::new(vec![workflow("plain", "Plain", WorkflowStatus::Running)]);
        panel.handle_key(key(KeyCode::Enter));
        let text = render(&panel, 110, 38).join("\n");
        assert!(text.contains("Supervisor · running · Coordinate workers"), "supervisor row missing\n{text}");
        assert!(text.contains("PanelWorker · running · Render page"), "subagent row missing\n{text}");
        assert!(!text.contains('▸') && !text.contains('▾'), "no feed must mean no fold markers\n{text}");
    }

    #[test]
    fn agent_fold_state_survives_replace_and_prunes_stale_rows() {
        // Folds are keyed by workflow id + agent name so live snapshot
        // replacements keep the expansion; a workflow or agent that
        // disappears prunes its fold state instead of leaking it.
        let item = agent_feed_workflow("fold-id", agent_feed_entries(1));
        let mut panel = WorkflowPanel::new(vec![item.clone()]);
        panel.handle_key(key(KeyCode::Enter));
        let offsets = agent_row_display_offsets(panel.selected_workflow().expect("selected"), &BTreeSet::new(), crate::theme::DARK, panel.detail_width.get().max(1), 0);
        panel.detail_scroll.set(offsets[0].0);
        panel.handle_key(key(KeyCode::Enter));
        assert!(render(&panel, 110, 38).join("\n").contains("task-entry-1"));
        // A live replacement keeps the fold.
        panel.replace(vec![item.clone()]);
        let text = render(&panel, 110, 38).join("\n");
        assert!(text.contains("▾ PanelWorker"), "fold must survive replace\n{text}");
        assert!(text.contains("task-entry-1"), "expanded feed must survive replace\n{text}");
        // Replacing with a workflow that dropped the agent prunes the fold.
        let mut bare = item.clone();
        bare.subagents = Vec::new();
        panel.replace(vec![bare]);
        let pruned = render(&panel, 110, 38).join("\n");
        assert!(!pruned.contains("PanelWorker"), "pruned agent must vanish\n{pruned}");
        assert!(!pruned.contains("task-entry-1"), "pruned feed must vanish\n{pruned}");
    }

    #[test]
    fn recent_irc_renders_sender_text_timestamp_and_bound() {
        // Recent IRC shows the live message stream: newest messages with
        // sender, bounded text and a wall-clock timestamp; older messages
        // collapse to one summary line; the empty state stays when there is
        // genuinely no IRC.
        let mut item = workflow("irc", "IRC", WorkflowStatus::Running);
        item.recent_irc = (1..=8)
            .map(|i| WorkflowIrcSnapshot { sender: format!("Worker{i}"), text: format!("progress message {i}"), at_ms: i * 1_000 })
            .collect();
        let mut panel = WorkflowPanel::new(vec![item.clone()]);
        panel.handle_key(key(KeyCode::Enter));
        let text = render(&panel, 110, 38).join("\n");
        for i in 3..=8 {
            assert!(text.contains(&format!("Worker{i}: progress message {i}")), "missing IRC row {i}\n{text}");
        }
        assert!(!text.contains("Worker2: progress message 2"), "bounded IRC must drop older messages\n{text}");
        assert!(text.contains("… 2 earlier"), "older messages must be summarized\n{text}");
        // Wall-clock timestamp of the newest message (8000 ms → 00:00:08).
        assert!(text.contains("00:00:08"), "IRC row must carry a wall-clock timestamp\n{text}");

        // Empty state stays when the stream is genuinely empty.
        item.recent_irc = Vec::new();
        let mut panel = WorkflowPanel::new(vec![item]);
        panel.handle_key(key(KeyCode::Enter));
        let text = render(&panel, 110, 38).join("\n");
        assert!(text.contains("No recent messages"), "empty IRC state missing\n{text}");
    }

    #[test]
    fn narrow_detail_keeps_rows_bounded_and_truncates_long_content() {
        // Narrow detail panes must never produce single-character columns or
        // overlapping content: long agent activity and IRC text is truncated
        // with an ellipsis and every rendered row stays within the pane
        // width (wrapped, never overflowing into the next column).
        let long = "a very long delegated task summary that would overflow a narrow detail pane if it were not truncated at the bounded feed width".repeat(2);
        let mut item = workflow("narrow", "Narrow", WorkflowStatus::Running);
        item.subagents = vec![WorkflowActorSnapshot {
            name: "PanelWorker".to_owned(),
            status: "running".to_owned(),
            task: Some("Render page".to_owned()),
            task_id: None,
            activity: vec![
                WorkflowAgentActivitySnapshot { at_ms: 1_000, kind: WorkflowAgentActivityKind::Task, text: long.clone() },
                WorkflowAgentActivitySnapshot { at_ms: 2_000, kind: WorkflowAgentActivityKind::Irc, text: long.clone() },
            ],
        }];
        item.recent_irc = vec![WorkflowIrcSnapshot { sender: "PanelWorker".to_owned(), text: long.clone(), at_ms: 3_000 }];
        let mut panel = WorkflowPanel::new(vec![item]);
        panel.handle_key(key(KeyCode::Enter));
        // Prime the detail width so the agent-row offset below is real (the
        // scroll helper falls back to width 1 before the first render).
        render(&panel, 70, 40);
        scroll_to_agent(&mut panel);
        panel.handle_key(key(KeyCode::Enter));
        // With Agents above the Todo DAG the clamped bottom viewport starts
        // below the Agents header; park the viewport on the header so the
        // header, the expanded feed, and the Recent IRC section all stay in
        // view for the width/truncation assertions.
        let workflow = panel.selected_workflow().expect("selected");
        let (lines, _) = detail_lines(workflow, &BTreeSet::new(), crate::theme::DARK, 0);
        let header_index = lines.iter().position(|line| line.spans.iter().any(|span| span.content.contains("Agents"))).expect("Agents header");
        let header_offset = lines.iter().take(header_index).map(|line| wrap_styled_line(line.clone(), panel.detail_width.get().max(1)).len()).sum::<usize>();
        panel.detail_scroll.set(header_offset);
        let lines = render(&panel, 70, 40);
        for (index, line) in lines.iter().enumerate() {
            assert!(line.chars().count() <= 70, "row {index} overflows the pane: {line:?}");
        }
        let text = lines.join("\n");
        assert!(text.contains('…'), "long content must be truncated with an ellipsis\n{text}");
        assert!(text.contains("task · "), "expanded task activity missing\n{text}");
        assert!(text.contains("irc · "), "expanded IRC activity missing\n{text}");
        assert!(text.contains("PanelWorker: "), "IRC sender missing\n{text}");
        assert!(text.contains("Agents") && text.contains("Recent IRC"), "section headers must render fully\n{text}");
    }

    #[test]
    fn agents_and_irc_sections_keep_empty_states() {
        let mut item = workflow("empty", "Empty", WorkflowStatus::Running);
        item.supervisor = None;
        item.subagents = Vec::new();
        item.recent_irc = Vec::new();
        let mut panel = WorkflowPanel::new(vec![item]);
        panel.handle_key(key(KeyCode::Enter));
        let text = render(&panel, 110, 38).join("\n");
        assert!(text.contains("Supervisor") && text.contains("Not started"), "empty supervisor state missing\n{text}");
        assert!(text.contains("No agents yet"), "empty agents state missing\n{text}");
        assert!(text.contains("No recent messages"), "empty IRC state missing\n{text}");
    }

    #[test]
    fn planning_progress_projects_live_activity_with_thinking_color() {
        // The supervisor's own turn streams into the planning section (T92
        // job-card style): the latest activity drives the progress line, and
        // thinking entries render in the thinking color — never the accent
        // that titles/objectives use.
        let mut item = workflow("planning-live", "Planning live", WorkflowStatus::Planning);
        item.todo = Vec::new();
        item.subagents = Vec::new();
        item.active_tasks = Vec::new();
        item.planning_started_at_ms = Some(1_000_000);
        item.planning_activity = vec![
            WorkflowActivitySnapshot { at_ms: 1_000_001, kind: pi_coding::WorkflowSupervisorActivityKind::Thinking, text: "仔细调研 codebase".to_owned() },
            WorkflowActivitySnapshot { at_ms: 1_000_010, kind: pi_coding::WorkflowSupervisorActivityKind::Tool, text: "read tools.rs".to_owned() },
        ];
        let mut panel = WorkflowPanel::new(vec![item.clone()]);
        panel.set_now(1_012_000);
        panel.handle_key(key(KeyCode::Enter));
        let text = render(&panel, 110, 38).join("\n");
        for needle in ["◉ planning · read tools.rs · 12s", "tool · read tools.rs", "thinking ·", "codebase"] {
            assert!(text.contains(needle), "missing planning activity {needle:?}\n{text}");
        }
        // The progress line itself must never fall back to the placeholder
        // while a live activity exists (the Todo DAG's own "drafting the
        // plan" line for an empty DAG is separate and legitimate).
        assert!(!text.contains("◉ planning · drafting the plan"), "live activity must drive the progress line\n{text}");
        assert!(text.contains("supervisor running"), "present supervisor state missing\n{text}");
        item.supervisor = None;
        let without_supervisor = detail_lines(&item, &BTreeSet::new(), crate::theme::DARK, 1_012_000)
            .0
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(without_supervisor.contains("Supervisor  Not started"), "missing planning supervisor empty state\n{without_supervisor}");
        assert!(without_supervisor.contains("supervisor not started"), "planning progress contradicts empty supervisor state\n{without_supervisor}");
        assert!(!without_supervisor.contains("supervisor running"), "absent supervisor must not render running\n{without_supervisor}");

        // Color contract: thinking uses the thinking color, tool activity the
        // job-card running accent, and the workflow name stays accent.
        let theme = crate::theme::DARK;
        let rendered = detail_lines(&item, &BTreeSet::new(), theme, 1_012_000).0;
        let line = |needle: &str| rendered.iter().find(|line| line.spans.iter().any(|span| span.content.contains(needle))).expect("line");
        let thinking_span = line("仔细调研").spans.iter().find(|span| span.content.contains("仔细调研")).expect("thinking span");
        assert_eq!(thinking_span.style.fg, Some(theme.thinking_text), "thinking must not be accent");
        let tool_span = line("read tools.rs").spans.iter().find(|span| span.content.contains("read tools.rs")).expect("tool span");
        assert_eq!(tool_span.style.fg, Some(theme.accent), "tool activity takes the job-card accent");
        let title_span = rendered[0].spans[0].clone();
        assert_eq!(title_span.style.fg, Some(theme.accent), "title stays accent");
    }

    #[test]
    fn planning_inactivity_notice_and_escape_cancel() {
        // Bounded inactivity notice: a planning workflow with no supervisor
        // activity for the window reads as stalled, and Esc cancels it.
        let mut item = workflow("planning-stalled", "Planning stalled", WorkflowStatus::Planning);
        item.todo = Vec::new();
        item.subagents = Vec::new();
        item.active_tasks = Vec::new();
        item.planning_started_at_ms = Some(1_000_000);
        let mut panel = WorkflowPanel::new(vec![item]);
        panel.set_now(1_060_000);
        panel.handle_key(key(KeyCode::Enter));
        let text = render(&panel, 110, 38).join("\n");
        for needle in ["planning has shown no progress for 60s", "Esc to cancel"] {
            assert!(text.contains(needle), "missing inactivity notice {needle:?}\n{text}");
        }
        assert_eq!(panel.handle_key(key(KeyCode::Esc)), WorkflowPanelResult::Intent { workflow_id: "planning-stalled".to_owned(), kind: WorkflowIntentKind::Cancel });
    }

    #[test]
    fn planning_with_recent_activity_shows_no_inactivity_notice() {
        // Live activity resets the inactivity window: Esc keeps its default
        // behavior (detail -> list -> close) while the supervisor is active.
        let mut item = workflow("planning-active", "Planning active", WorkflowStatus::Planning);
        item.todo = Vec::new();
        item.subagents = Vec::new();
        item.active_tasks = Vec::new();
        item.planning_started_at_ms = Some(1_000_000);
        item.planning_activity = vec![WorkflowActivitySnapshot { at_ms: 1_059_000, kind: pi_coding::WorkflowSupervisorActivityKind::Thinking, text: "still weighing".to_owned() }];
        let mut panel = WorkflowPanel::new(vec![item]);
        panel.set_now(1_060_000);
        panel.handle_key(key(KeyCode::Enter));
        let text = render(&panel, 110, 38).join("\n");
        assert!(!text.contains("no progress"), "recent activity must suppress the notice\n{text}");
        assert_eq!(panel.handle_key(key(KeyCode::Esc)), WorkflowPanelResult::Handled);
        assert_eq!(panel.focus(), WorkflowPanelFocus::List);
        assert_eq!(panel.handle_key(key(KeyCode::Esc)), WorkflowPanelResult::Close);
    }

    #[test]
    fn planning_placeholder_falls_back_when_activity_is_empty() {
        // A planning workflow whose supervisor has not streamed any activity
        // yet keeps the static "drafting the plan" placeholder.
        let mut item = workflow("planning-empty", "Planning empty", WorkflowStatus::Planning);
        item.todo = Vec::new();
        item.subagents = Vec::new();
        item.active_tasks = Vec::new();
        item.planning_started_at_ms = Some(1_000_000);
        let mut panel = WorkflowPanel::new(vec![item]);
        panel.handle_key(key(KeyCode::Enter));
        let text = render(&panel, 110, 38).join("\n");
        assert!(text.contains("Planning…"), "placeholder must show when activity is empty\n{text}");
        assert!(text.contains("The supervisor is drafting the plan"), "placeholder line missing\n{text}");
    }

    #[test]
    fn planning_placeholder_shows_latest_tool_activity_when_present() {
        // Once the supervisor streams activity, the empty-DAG placeholder is
        // replaced by the latest activity line: the tool row keeps the
        // `tool · ` prefix and the bounded bash command summary.
        let mut item = workflow("planning-live-dag", "Planning live DAG", WorkflowStatus::Planning);
        item.todo = Vec::new();
        item.subagents = Vec::new();
        item.active_tasks = Vec::new();
        item.planning_started_at_ms = Some(1_000_000);
        item.planning_activity = vec![
            WorkflowActivitySnapshot { at_ms: 1_000_001, kind: pi_coding::WorkflowSupervisorActivityKind::Thinking, text: "survey the codebase".to_owned() },
            WorkflowActivitySnapshot { at_ms: 1_000_010, kind: pi_coding::WorkflowSupervisorActivityKind::Tool, text: "bash · cargo +1.88.0 test -p pi-coding --lib...".to_owned() },
        ];
        let mut panel = WorkflowPanel::new(vec![item]);
        panel.set_now(1_012_000);
        panel.handle_key(key(KeyCode::Enter));
        let text = render(&panel, 110, 38).join("\n");
        assert!(text.contains("◐ tool · bash · cargo +1.88.0 test -p pi-coding --lib..."), "latest activity must replace the placeholder\n{text}");
        assert!(!text.contains("Planning…"), "placeholder must not show while activity exists\n{text}");
        assert!(!text.contains("The supervisor is drafting the plan"), "placeholder line must not show while activity exists\n{text}");
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
    fn lifecycle_intents_fire_from_either_focus() {
        let mut panel = WorkflowPanel::new(vec![workflow("one", "Alpha", WorkflowStatus::Running)]);
        // From Detail focus.
        panel.handle_key(key(KeyCode::Enter));
        assert_eq!(panel.focus(), WorkflowPanelFocus::Detail);
        for (key_code, kind) in [('p', WorkflowIntentKind::Pause), ('r', WorkflowIntentKind::Resume), ('c', WorkflowIntentKind::Cancel), ('i', WorkflowIntentKind::Integrate)] {
            assert_eq!(panel.handle_key(key(KeyCode::Char(key_code))), WorkflowPanelResult::Intent { workflow_id: "one".to_owned(), kind });
        }
        // Back to List focus; intents still fire.
        panel.handle_key(key(KeyCode::Esc));
        assert_eq!(panel.focus(), WorkflowPanelFocus::List);
        assert_eq!(panel.handle_key(key(KeyCode::Char('p'))), WorkflowPanelResult::Intent { workflow_id: "one".to_owned(), kind: WorkflowIntentKind::Pause });
    }

    #[test]
    fn tab_toggles_focus_between_panes() {
        let mut panel = WorkflowPanel::new(vec![workflow("one", "Alpha", WorkflowStatus::Running)]);
        assert_eq!(panel.focus(), WorkflowPanelFocus::List);
        assert_eq!(panel.handle_key(key(KeyCode::Tab)), WorkflowPanelResult::Handled);
        assert_eq!(panel.focus(), WorkflowPanelFocus::Detail);
        assert_eq!(panel.handle_key(key(KeyCode::BackTab)), WorkflowPanelResult::Handled);
        assert_eq!(panel.focus(), WorkflowPanelFocus::List);
    }

    #[test]
    fn select_id_restores_visible_workflow() {
        let mut panel = WorkflowPanel::new(vec![workflow("one", "Alpha", WorkflowStatus::Running), workflow("two", "Beta", WorkflowStatus::Paused)]);
        assert!(panel.select_id("two"));
        assert_eq!(panel.selected_workflow().unwrap().name, "Beta");
        // A missing id leaves the selection untouched.
        assert!(!panel.select_id("missing"));
        assert_eq!(panel.selected_workflow().unwrap().name, "Beta");
        assert!(panel.select_id("one"));
        assert_eq!(panel.selected_workflow().unwrap().name, "Alpha");
        // When a filter hides the workflow, select_id cannot reach it (the filter
        // string persists after Esc, so exercise the hidden case last).
        panel.handle_key(key(KeyCode::Char('/')));
        for character in "alpha".chars() { panel.handle_key(key(KeyCode::Char(character))); }
        assert_eq!(panel.selected_workflow().unwrap().name, "Alpha");
        assert!(!panel.select_id("two"));
        assert_eq!(panel.selected_workflow().unwrap().name, "Alpha");
    }

    #[test]
    fn detail_scroll_clamps_within_content() {
        let mut panel = WorkflowPanel::new(vec![workflow("one", "Alpha", WorkflowStatus::Running)]);
        panel.handle_key(key(KeyCode::Enter));
        render(&panel, 80, 10);
        let max = panel.detail_scroll_max();
        assert!(max > 0, "expected scrollable detail, max={max}");
        for _ in 0..200 { panel.handle_key(key(KeyCode::Down)); }
        assert_eq!(panel.detail_scroll(), max);
        panel.handle_key(key(KeyCode::Home));
        assert_eq!(panel.detail_scroll(), 0);
        panel.handle_key(key(KeyCode::End));
        assert_eq!(panel.detail_scroll(), max);
        panel.handle_key(key(KeyCode::Home));
        panel.handle_key(key(KeyCode::PageUp));
        assert_eq!(panel.detail_scroll(), 0);
        panel.handle_key(key(KeyCode::PageDown));
        assert!(panel.detail_scroll() <= max);
    }

    #[test]
    fn too_small_terminal_falls_back_to_stacked_split() {
        let panel = WorkflowPanel::new(vec![workflow("one", "Panel foundation", WorkflowStatus::Running), workflow("two", "RPC transport", WorkflowStatus::Queued)]);
        let text = render(&panel, 50, 14).join("\n");
        assert!(text.contains("Panel foundation"), "list missing in stacked fallback\n{text}");
        assert!(text.contains("Objective"), "detail missing in stacked fallback\n{text}");
    }

    #[test]
    fn empty_workflow_panel_shows_create_hint() {
        let panel = WorkflowPanel::new(Vec::new());
        let text = render(&panel, 90, 16).join("\n");
        // The hint wraps inside the narrow list pane; assert the wrapped pieces.
        assert!(text.contains("No workflows yet"), "missing create hint start\n{text}");
        assert!(text.contains("/workflow create"), "missing create command\n{text}");
        assert!(text.contains("<objective>"), "missing objective placeholder\n{text}");
    }

    #[test]
    fn detail_longer_than_pane_scrolls_with_arrows() {
        // Reported bug: with the list focused and the detail overflowing, ↓
        // moved the list cursor (resetting the detail to its top) instead of
        // scrolling the detail, so the lower phases were unreachable. Before
        // any Tab the navigation keys follow the pane that has content: an
        // overflowing detail scrolls with ↑/↓/j/k, the selection stays put,
        // and the bottom is reachable with clamping.
        let mut panel = WorkflowPanel::new(vec![workflow("one", "Alpha", WorkflowStatus::Running), workflow("two", "Beta", WorkflowStatus::Paused)]);
        render(&panel, 80, 10);
        let max = panel.detail_scroll_max();
        assert!(max > 0, "expected an overflowing detail, max={max}");
        assert_eq!(panel.navigation_focus(), WorkflowPanelFocus::Detail, "overflowing detail must own the navigation keys");
        assert_eq!(panel.handle_key(key(KeyCode::Down)), WorkflowPanelResult::Handled);
        assert_eq!(panel.detail_scroll(), 1, "↓ must scroll the detail");
        assert_eq!(panel.selected_workflow().unwrap().name, "Alpha", "↓ must not move the list cursor");
        assert_eq!(panel.handle_key(key(KeyCode::Char('j'))), WorkflowPanelResult::Handled);
        assert_eq!(panel.detail_scroll(), 2, "j must scroll like ↓");
        for _ in 0..200 { panel.handle_key(key(KeyCode::Down)); }
        assert_eq!(panel.detail_scroll(), max, "the bottom must be reachable");
        assert_eq!(panel.selected_workflow().unwrap().name, "Alpha", "scrolling must never change the selection");
        assert_eq!(panel.handle_key(key(KeyCode::Up)), WorkflowPanelResult::Handled);
        assert_eq!(panel.detail_scroll(), max.saturating_sub(1), "↑ must scroll back up");
        assert_eq!(panel.handle_key(key(KeyCode::Char('k'))), WorkflowPanelResult::Handled);
        assert_eq!(panel.detail_scroll(), max.saturating_sub(2), "k must scroll like ↑");
        // Home/End and PageUp/PageDown also follow the overflowing detail.
        assert_eq!(panel.handle_key(key(KeyCode::Home)), WorkflowPanelResult::Handled);
        assert_eq!(panel.detail_scroll(), 0, "Home must jump to the detail top");
        assert_eq!(panel.handle_key(key(KeyCode::End)), WorkflowPanelResult::Handled);
        assert_eq!(panel.detail_scroll(), max, "End must jump to the detail bottom");
        assert_eq!(panel.handle_key(key(KeyCode::Home)), WorkflowPanelResult::Handled);
        assert_eq!(panel.handle_key(key(KeyCode::PageUp)), WorkflowPanelResult::Handled);
        assert_eq!(panel.detail_scroll(), 0, "PageUp must not go above the top");
        assert_eq!(panel.handle_key(key(KeyCode::PageDown)), WorkflowPanelResult::Handled);
        assert!(panel.detail_scroll() <= max, "PageDown must clamp at the bottom");
    }

    #[test]
    fn tab_toggles_focus_and_explicit_pane_restores_list_navigation() {
        // Tab keeps its today behavior — stored pane focus List ↔ Detail —
        // and once the user has Tab'd, navigation follows that explicit
        // choice: a Tab back to the list restores ↑/↓ list navigation even
        // while the detail still overflows.
        let mut panel = WorkflowPanel::new(vec![workflow("one", "Alpha", WorkflowStatus::Running), workflow("two", "Beta", WorkflowStatus::Paused)]);
        render(&panel, 80, 10);
        let max = panel.detail_scroll_max();
        assert!(max > 0, "expected an overflowing detail, max={max}");
        // Auto state before any Tab: an overflowing detail owns ↑/↓.
        assert_eq!(panel.handle_key(key(KeyCode::Down)), WorkflowPanelResult::Handled);
        assert_eq!(panel.detail_scroll(), 1);
        assert_eq!(panel.selected_workflow().unwrap().name, "Alpha");
        assert_eq!(panel.handle_key(key(KeyCode::Tab)), WorkflowPanelResult::Handled);
        assert_eq!(panel.focus(), WorkflowPanelFocus::Detail, "Tab must focus the detail");
        assert_eq!(panel.handle_key(key(KeyCode::Down)), WorkflowPanelResult::Handled);
        assert_eq!(panel.detail_scroll(), 1, "detail focus keeps scrolling the detail");
        assert_eq!(panel.handle_key(key(KeyCode::BackTab)), WorkflowPanelResult::Handled);
        assert_eq!(panel.focus(), WorkflowPanelFocus::List, "BackTab must focus the list");
        assert_eq!(panel.handle_key(key(KeyCode::Down)), WorkflowPanelResult::Handled);
        assert_eq!(panel.selected_workflow().unwrap().name, "Beta", "explicit list focus must move the selection");
        assert_eq!(panel.detail_scroll(), 0, "a selection change resets the detail scroll");
    }

    #[test]
    fn list_navigation_works_when_detail_fits() {
        // When the selected workflow's detail fits the pane there is nothing
        // to scroll, so ↑/↓/j/k navigate the list as before.
        let mut panel = WorkflowPanel::new(vec![workflow("one", "Alpha", WorkflowStatus::Running), workflow("two", "Beta", WorkflowStatus::Paused)]);
        render(&panel, 300, 200);
        assert_eq!(panel.detail_scroll_max(), 0, "the detail must fit the pane");
        assert_eq!(panel.navigation_focus(), WorkflowPanelFocus::List, "a fitting detail must leave navigation on the list");
        assert_eq!(panel.handle_key(key(KeyCode::Down)), WorkflowPanelResult::Handled);
        assert_eq!(panel.selected_workflow().unwrap().name, "Beta");
        assert_eq!(panel.handle_key(key(KeyCode::Up)), WorkflowPanelResult::Handled);
        assert_eq!(panel.selected_workflow().unwrap().name, "Alpha");
        assert_eq!(panel.handle_key(key(KeyCode::Char('j'))), WorkflowPanelResult::Handled);
        assert_eq!(panel.selected_workflow().unwrap().name, "Beta", "j must move down the list");
        assert_eq!(panel.handle_key(key(KeyCode::Char('k'))), WorkflowPanelResult::Handled);
        assert_eq!(panel.selected_workflow().unwrap().name, "Alpha", "k must move up the list");
        assert_eq!(panel.handle_key(key(KeyCode::Home)), WorkflowPanelResult::Handled);
        assert_eq!(panel.selected_workflow().unwrap().name, "Alpha", "Home must jump to the first workflow");
        assert_eq!(panel.handle_key(key(KeyCode::End)), WorkflowPanelResult::Handled);
        assert_eq!(panel.selected_workflow().unwrap().name, "Beta", "End must jump to the last workflow");
    }

    #[test]
    fn footer_focus_label_reflects_navigation_pane() {
        // The shared footer reports which pane actually receives ↑/↓/j/k:
        // focus:detail while the detail overflows, focus:list when it fits,
        // and the stored pane once the user has Tab'd.
        let mut panel = WorkflowPanel::new(vec![workflow("one", "Alpha", WorkflowStatus::Running)]);
        let overflowing = render(&panel, 80, 10).join("\n");
        assert!(overflowing.contains("focus:detail"), "overflowing detail must read focus:detail\n{overflowing}");
        let fitting = render(&panel, 300, 200).join("\n");
        assert!(fitting.contains("focus:list"), "fitting detail must read focus:list\n{fitting}");
        panel.handle_key(key(KeyCode::Tab));
        let explicit = render(&panel, 80, 10).join("\n");
        assert!(explicit.contains("focus:detail"), "Tab to the detail keeps focus:detail\n{explicit}");
    }
}