//! Pure Todo-DAG line rendering shared by the Todo DAG panel and the
//! workflow master-detail page.

use std::collections::HashMap;

use pi_coding::markdown::fit_text;
use pi_coding::redact::redact_secrets;
use pi_coding::{TodoPhase, TodoStatus};
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
    pub waiting: usize,
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
                    if !task.waiting_on.is_empty() {
                        counts.waiting += 1;
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
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// [`redacted_sanitize`] is the centralized redact-before-sanitize helper
/// every Todo DAG / detail / child-projection field calls; see its docs for
/// why a plain `sanitize` is not enough.
/// Centralized redact-before-sanitize for every model-generated / untrusted
/// field the Todo DAG surfaces. `sanitize` only strips control characters
/// and collapses whitespace; the supervisor's activity feed, markdown
/// renderer, and bash tool cards all redact before sanitize, so the Todo
/// DAG must too — a model that writes a credential into a task title, job
/// display name, IRC body, or delegation-context body would otherwise render
/// it verbatim (and persist it unredacted in the durable store). This is
/// the single helper the DAG panel, subagent detail, and child-detail
/// projection all call so redaction cannot be skipped at one callsite.
pub fn redacted_sanitize(value: &str) -> String {
    sanitize(&redact_secrets(value))
}

/// Redact, sanitize, then truncate to `max_width` terminal cells with a
/// trailing ellipsis. Redaction runs first so a credential a model writes
/// into a label or summary is scrubbed before the panel ever measures it;
/// the truncate step can never split a secret because none remain by then.
/// Canonical authority IDs are not secret-shaped, so they pass through
/// unchanged wherever the existing presentation intentionally exposes them;
/// opaque IDs that the UI already hides remain hidden.
#[must_use]
pub fn redacted_truncate(value: &str, max_width: usize) -> String {
    truncate(&redacted_sanitize(value), max_width)
}

/// Truncate to `max_width` terminal cells with a trailing ellipsis.
pub fn truncate(value: &str, max_width: usize) -> String {
    fit_text(value, max_width)
}

/// Render the Todo-DAG lines used by the workflow master-detail page.
///
/// Rows are deliberately lean: phase + task content + a status bullet only.
/// The opaque task id and the `ready` marker are never rendered (they read as
/// noise next to live actor rows); in-progress tasks instead carry a compact
/// association to the subagent handling them — `agent_by_task_id` maps the
/// Todo-DAG task id to the live actor working it, falling back to the task's
/// planned `agent` role.
pub fn workflow_todo_dag_lines(
    phases: &[TodoPhase],
    agent_by_task_id: &HashMap<String, String>,
    theme: Theme,
) -> Vec<Line<'static>> {
    let counts = TodoDagCounts::from_phases(phases);
    let mut lines = Vec::new();

    lines.push(Line::from(Span::styled(
        format!(
            "✓ {} completed · {} open · {} active · {} waiting",
            counts.completed, counts.open, counts.active, counts.waiting
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
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )));

        for task in &phase.tasks {
            let mut row = vec![
                Span::styled(
                    format!("  {} ", task_marker(task.status)),
                    todo_style(task.status, theme),
                ),
                Span::styled(
                    redacted_sanitize(&task.content),
                    Style::default().fg(theme.text),
                ),
            ];
            if task.status == TodoStatus::InProgress {
                if let Some(agent) = agent_by_task_id
                    .get(task.id.as_str())
                    .or(task.agent.as_ref())
                {
                    row.push(Span::styled(
                        format!(" · {}", redacted_sanitize(agent)),
                        Style::default().fg(theme.dim),
                    ));
                }
            }
            lines.push(Line::from(row));

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
        }

        lines.push(Line::default());
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_coding::{TodoItem, TodoWaitingOn};

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
        // content, phase names, agent roles, and dependency-waiting content.
        // `sanitize` alone only strips control characters and collapses whitespace,
        // so a credential a model writes into a task title would render verbatim
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
                    waiting_on: Vec::new(),
                    agent: None,
                },
                TodoItem {
                    id: "task-2".to_owned(),
                    content: "wire the api key".to_owned(),
                    status: TodoStatus::Pending,
                    depends_on: vec!["task-1".to_owned()],
                    ready: false,
                    waiting_on: vec![TodoWaitingOn {
                        task_id: "task-3".to_owned(),
                        content: format!("waiting on Bearer {blocker_secret}"),
                        status: TodoStatus::InProgress,
                    }],
                    agent: None,
                },
            ],
        }];
        let agent_by_task_id =
            HashMap::from([("task-1".to_owned(), format!("worker {task_secret}"))]);
        let text = rendered(&workflow_todo_dag_lines(
            &phases,
            &agent_by_task_id,
            crate::theme::DARK,
        ));
        for secret in [&phase_secret, &task_secret, &blocker_secret] {
            assert!(
                !text.contains(secret),
                "secret {secret:?} must be redacted:\n{text}"
            );
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

    #[test]
    fn truncate_bounds_cjk_by_terminal_columns() {
        let body = "界".repeat(80);
        let output = truncate(&body, 80);

        assert!(pi_coding::markdown::display_width(&body) > 80);
        assert!(
            pi_coding::markdown::display_width(&output) <= 80,
            "{output}"
        );
        assert!(output.ends_with('…'), "{output}");
    }

    /// Secret sentinel used by every redaction test: the `sk-…` shape is the
    /// first whole-match pattern in `SECRET_PATTERNS`, so a single fixture
    /// exercises the central `redact_secrets` path the panel now shares.
    const SECRET_SENTINEL: &str = "sk-abcdef1234567890aabbccdd";

    #[test]
    fn redacted_sanitize_redacts_secret_sentinel_and_preserves_plain_text() {
        // The sentinel must be recognized as a secret and replaced whole.
        assert!(SECRET_SENTINEL.starts_with("sk-") && SECRET_SENTINEL.len() > "sk-".len() + 16);
        let input = format!("push key {SECRET_SENTINEL} then deploy");
        let output = redacted_sanitize(&input);
        assert!(
            !output.contains(SECRET_SENTINEL),
            "sentinel must be redacted: {output}"
        );
        assert!(
            output.contains("[REDACTED]"),
            "must carry the redaction marker: {output}"
        );
        // Surrounding plain text and whitespace collapsing survive.
        assert!(output.contains("push key"), "{output}");
        assert!(output.contains("then deploy"), "{output}");
        // Control characters collapse to spaces, not literal controls.
        let dirty = format!("a\x07b\x0bc {SECRET_SENTINEL}");
        let cleaned = redacted_sanitize(&dirty);
        assert!(
            !cleaned.contains('\x07') && !cleaned.contains('\x0b'),
            "{cleaned}"
        );
        assert!(cleaned.contains("[REDACTED]"), "{cleaned}");
        // Canonical authority IDs are not secret-shaped and pass through.
        assert_eq!(redacted_sanitize("task-1"), "task-1");
        assert_eq!(redacted_sanitize("main"), "main");
    }

    #[test]
    fn redacted_truncate_redacts_before_truncating_and_bounds_display_width() {
        // A secret near the truncation boundary must be scrubbed BEFORE the
        // ellipsis cut — the panel never projects a partial credential even
        // at a narrow width.
        let body = format!(
            "Bearer {SECRET_SENTINEL} and a long tail of plain words that overflow thirty columns easily"
        );
        let narrow = redacted_truncate(&body, 30);
        assert!(
            !narrow.contains(SECRET_SENTINEL),
            "sentinel must be redacted before truncation: {narrow}"
        );
        assert!(
            narrow.contains("[REDACTED]"),
            "redaction marker must survive the cut: {narrow}"
        );
        assert!(
            pi_coding::markdown::display_width(&narrow) <= 30,
            "display width must stay within the 30-cell bound: {} ({})",
            narrow,
            pi_coding::markdown::display_width(&narrow),
        );
        // When truncation lands inside the `[REDACTED]` marker, no secret
        // leaks — only the cosmetic marker can split.
        let tight = redacted_truncate(SECRET_SENTINEL, 4);
        assert!(
            !tight.contains("abcdef"),
            "no secret fragment survives a tight cut: {tight}"
        );
        assert!(
            pi_coding::markdown::display_width(&tight) <= 4,
            "tight cut must respect the 4-cell bound: {tight}"
        );
    }
}
