//! Pure Todo-DAG line rendering shared by the Todo DAG panel and the
//! workflow master-detail page.

use std::collections::HashMap;

use pi_coding::{TodoPhase, TodoStatus};
use pi_coding::redact::redact_secrets;
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthChar;

use crate::theme::Theme;

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

/// Wrap a styled [`Line`] to a width while preserving per-span styles.
pub fn wrap_styled_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    if line.spans.is_empty() {
        return vec![Line::default()];
    }
    let mut rows = vec![Line::default()];
    let mut columns = 0usize;
    for span in line.spans {
        let style = span.style;
        let mut fragment = String::new();
        for character in span.content.chars() {
            let character_width = character.width().unwrap_or(0);
            if columns > 0 && columns.saturating_add(character_width) > width {
                if !fragment.is_empty() {
                    rows.last_mut()
                        .expect("row")
                        .spans
                        .push(Span::styled(std::mem::take(&mut fragment), style));
                }
                rows.push(Line::default());
                columns = 0;
            }
            fragment.push(character);
            columns = columns.saturating_add(character_width);
        }
        if !fragment.is_empty() {
            rows.last_mut()
                .expect("row")
                .spans
                .push(Span::styled(fragment, style));
        }
    }
    rows
}

pub fn task_marker(status: TodoStatus) -> &'static str {
    match status {
        TodoStatus::Pending => "○",
        TodoStatus::InProgress => "●",
        TodoStatus::Completed => "✓",
        TodoStatus::Abandoned => "×",
    }
}

pub fn todo_style(status: TodoStatus, theme: Theme) -> Style {
    Style::default().fg(match status {
        TodoStatus::Pending => theme.muted,
        TodoStatus::InProgress => theme.accent,
        TodoStatus::Completed => theme.success,
        TodoStatus::Abandoned => theme.dim,
    })
}

pub fn todo_status(status: TodoStatus) -> &'static str {
    match status {
        TodoStatus::Pending => "pending",
        TodoStatus::InProgress => "in progress",
        TodoStatus::Completed => "completed",
        TodoStatus::Abandoned => "abandoned",
    }
}

pub fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|character| if character.is_control() { ' ' } else { character })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Sanitize model-generated text for the panel AND redact credentials first.
/// `sanitize` only strips control characters and collapses whitespace; the
/// supervisor's activity feed, markdown renderer, and bash tool cards all
/// redact before sanitize, so the Todo DAG must too — a model that writes a
/// credential into a task title would otherwise render it verbatim in the
/// panel (and persist it unredacted in the durable workflow store).
fn redacted_sanitize(value: &str) -> String {
    sanitize(&redact_secrets(value))
}

pub fn truncate(value: &str, max: usize) -> String {
    let mut chars = value.chars();
    let mut output = chars.by_ref().take(max).collect::<String>();
    if chars.next().is_some() && max > 0 {
        output.pop();
        output.push('…');
    }
    output
}

/// Render the Todo-DAG lines used by the workflow master-detail page.
///
/// Rows are deliberately lean: phase + task content + a status bullet only.
/// The opaque task id and the `ready` marker are never rendered (they read as
/// noise next to live actor rows); in-progress tasks instead carry a compact
/// association to the subagent handling them — `agent_by_task_id` maps the
/// Todo-DAG task id to the live actor working it, falling back to the task's
/// planned `agent` role.
pub fn workflow_todo_dag_lines(phases: &[TodoPhase], agent_by_task_id: &HashMap<String, String>, theme: Theme) -> Vec<Line<'static>> {
    let counts = TodoDagCounts::from_phases(phases);
    let mut lines = Vec::new();

    lines.push(Line::from(Span::styled(
        format!(
            "✓ {} completed · {} open · {} active · {} blocked",
            counts.completed, counts.open, counts.active, counts.blocked
        ),
        Style::default().fg(theme.muted),
    )));

    if phases.is_empty() {
        lines.push(Line::from(Span::styled(
            "No phases or tasks yet",
            Style::default().fg(theme.muted),
        )));
        return lines;
    }

    let names = phases
        .iter()
        .flat_map(|phase| &phase.tasks)
        .map(|task| (task.id.as_str(), redacted_sanitize(&task.content)))
        .collect::<HashMap<_, _>>();

    for phase in phases {
        lines.push(Line::from(Span::styled(
            redacted_sanitize(&phase.name),
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        )));

        for task in &phase.tasks {
            let mut row = vec![
                Span::styled(
                    format!("  {} ", task_marker(task.status)),
                    todo_style(task.status, theme),
                ),
                Span::styled(redacted_sanitize(&task.content), Style::default().fg(theme.text)),
            ];
            if task.status == TodoStatus::InProgress {
                if let Some(agent) = agent_by_task_id.get(task.id.as_str()).or(task.agent.as_ref()) {
                    row.push(Span::styled(format!(" · {}", redacted_sanitize(agent)), Style::default().fg(theme.dim)));
                }
            }
            lines.push(Line::from(row));

            let dependencies = task
                .depends_on
                .iter()
                .map(|id| names.get(id.as_str()).cloned().unwrap_or_else(|| sanitize(id)))
                .collect::<Vec<_>>();
            if !dependencies.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("      depends_on: {}", dependencies.join(", ")),
                    Style::default().fg(theme.dim),
                )));
            }

            let blockers = task
                .blocked_by
                .iter()
                .map(|blocked| format!("{} ({})", redacted_sanitize(&blocked.content), todo_status(blocked.status)))
                .collect::<Vec<_>>();
            if !blockers.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("      blocked_by: {}", blockers.join(", ")),
                    Style::default().fg(theme.warning),
                )));
            }
        }

        lines.push(Line::default());
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_coding::{TodoBlockedReason, TodoItem};

    fn rendered(lines: &[Line<'static>]) -> String {
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
    fn workflow_todo_dag_lines_redacts_secrets_in_model_generated_text() {
        // Model-generated text must be redacted before sanitize: task
        // content, phase names, agent roles, and blocker content. `sanitize`
        // alone only strips control characters and collapses whitespace, so a
        // credential a model writes into a task title would render verbatim
        // (and persist unredacted in the durable workflow store).
        let phase_secret = ["s", "k-live-", "abcdef1234567890"].concat();
        let task_secret = ["s", "k-", "abc123def456ghi789jkl012"].concat();
        let blocker_secret = ["s", "k-", "zzz123456789012345678"].concat();
        let phases = vec![TodoPhase {
            name: format!("Auth: Bearer {phase_secret}"),
            tasks: vec![
                TodoItem {
                    id: "task-1".to_owned(),
                    content: format!("call /v1/verify with Bearer {task_secret}"),
                    status: TodoStatus::InProgress,
                    depends_on: Vec::new(),
                    ready: false,
                    blocked_by: Vec::new(),
                    agent: None,
                },
                TodoItem {
                    id: "task-2".to_owned(),
                    content: "wire the api key".to_owned(),
                    status: TodoStatus::Pending,
                    depends_on: vec!["task-1".to_owned()],
                    ready: false,
                    blocked_by: vec![TodoBlockedReason {
                        task_id: "task-3".to_owned(),
                        content: format!("waiting on Bearer {blocker_secret}"),
                        status: TodoStatus::InProgress,
                    }],
                    agent: None,
                },
            ],
        }];
        let agent_by_task_id = HashMap::from([(
            "task-1".to_owned(),
            format!("worker {task_secret}"),
        )]);
        let text = rendered(&workflow_todo_dag_lines(
            &phases,
            &agent_by_task_id,
            crate::theme::DARK,
        ));
        for secret in [&phase_secret, &task_secret, &blocker_secret] {
            assert!(!text.contains(secret), "secret {secret:?} must be redacted:\n{text}");
        }
        assert!(
            text.matches("[REDACTED]").count() >= 4,
            "phase name, task content, agent role, and blocker content must each redact:\n{text}"
        );
        // Non-secret text still renders untouched.
        assert!(text.contains("call /v1/verify with"), "{text}");
        assert!(text.contains("wire the api key"), "{text}");
        assert!(text.contains("worker"), "{text}");
    }
}
