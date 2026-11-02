//! Compact `/jobs` and `/irc` overlay over live orchestration snapshots.
//!
//! Display-only: jobs come from [`JobCardPresentationAdapter`], mailbox rows
//! from `OrchestrationRuntime::inbox`. No new authority.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use pi_coding::markdown::display_width;
use pi_coding::{JobStatus, MailboxMessage};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use crate::job_card_adapter::{JobCardRows, progress_row_text};
use crate::orchestration_message::orchestration_irc_view_from_mailbox;
use crate::theme::Theme;
use crate::todo_dag_view::{sanitize, truncate};

const MAX_IRC_ROWS: usize = 64;
const MAX_BODY_WIDTH: usize = 80;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JobsPanelPage {
    Jobs,
    Irc,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct JobsPanel {
    page: JobsPanelPage,
    jobs: Vec<JobCardRows>,
    messages: Vec<MailboxMessage>,
    selected: usize,
    enabled: bool,
}

impl JobsPanel {
    #[must_use]
    pub(crate) fn jobs(
        jobs: Vec<JobCardRows>,
        messages: Vec<MailboxMessage>,
        enabled: bool,
    ) -> Self {
        Self {
            page: JobsPanelPage::Jobs,
            jobs,
            messages: bound_messages(messages),
            selected: 0,
            enabled,
        }
    }

    #[must_use]
    pub(crate) fn irc(
        jobs: Vec<JobCardRows>,
        messages: Vec<MailboxMessage>,
        enabled: bool,
    ) -> Self {
        Self {
            page: JobsPanelPage::Irc,
            jobs,
            messages: bound_messages(messages),
            selected: 0,
            enabled,
        }
    }

    pub(crate) fn replace(
        &mut self,
        jobs: Vec<JobCardRows>,
        messages: Vec<MailboxMessage>,
        enabled: bool,
    ) {
        self.jobs = jobs;
        self.messages = bound_messages(messages);
        self.enabled = enabled;
        let len = self.row_count();
        if len == 0 {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(len.saturating_sub(1));
        }
    }

    pub(crate) fn replace_jobs(&mut self, jobs: Vec<JobCardRows>) {
        self.jobs = jobs;
        if self.page == JobsPanelPage::Jobs {
            let len = self.row_count();
            if len == 0 {
                self.selected = 0;
            } else {
                self.selected = self.selected.min(len.saturating_sub(1));
            }
        }
    }
    fn row_count(&self) -> usize {
        match self.page {
            JobsPanelPage::Jobs => self.jobs.len(),
            JobsPanelPage::Irc => self.messages.len(),
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return true;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q')
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                return false;
            }
            KeyCode::Tab => {
                self.page = match self.page {
                    JobsPanelPage::Jobs => JobsPanelPage::Irc,
                    JobsPanelPage::Irc => JobsPanelPage::Jobs,
                };
                self.selected = 0;
            }
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
            }
            KeyCode::Down => {
                let last = self.row_count().saturating_sub(1);
                self.selected = (self.selected + 1).min(last);
            }
            _ => {}
        }
        true
    }
}

pub(crate) fn render_jobs_panel(frame: &mut ratatui::Frame<'_>, panel: &JobsPanel, theme: Theme) {
    let width = frame.area().width.saturating_sub(4).min(110).max(36);
    let height = frame.area().height.saturating_sub(4).min(28).max(10);
    let area = centered_rect(width, height, frame.area());
    frame.render_widget(Clear, area);
    let title = match panel.page {
        JobsPanelPage::Jobs => " Jobs ",
        JobsPanelPage::Irc => " IRC ",
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_accent))
        .title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);
    frame.render_widget(Paragraph::new(header_line(panel, theme)), sections[0]);
    let rows = match panel.page {
        JobsPanelPage::Jobs => job_lines(panel, sections[1].height, sections[1].width, theme),
        JobsPanelPage::Irc => irc_lines(panel, sections[1].height, sections[1].width, theme),
    };
    frame.render_widget(Paragraph::new(rows).wrap(Wrap { trim: false }), sections[1]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " ↑/↓ select · Tab jobs/irc · Esc close ",
            Style::default().fg(theme.dim),
        ))),
        sections[2],
    );
}

fn header_line(panel: &JobsPanel, theme: Theme) -> Line<'static> {
    let text = if !panel.enabled {
        "orchestration unbound".to_owned()
    } else {
        match panel.page {
            JobsPanelPage::Jobs => job_aggregate(&panel.jobs),
            JobsPanelPage::Irc => format!("{} messages", panel.messages.len()),
        }
    };
    Line::from(Span::styled(text, Style::default().fg(theme.muted)))
}

fn job_aggregate(jobs: &[JobCardRows]) -> String {
    let mut running = 0usize;
    let mut queued = 0usize;
    let mut completed = 0usize;
    let mut failed = 0usize;
    let mut cancelled = 0usize;
    for job in jobs {
        match job.job_status {
            JobStatus::Queued => queued += 1,
            JobStatus::Running => running += 1,
            JobStatus::Completed => completed += 1,
            JobStatus::Failed => failed += 1,
            JobStatus::Cancelled => cancelled += 1,
        }
    }
    let mut parts = Vec::new();
    for (count, label) in [
        (running, "running"),
        (queued, "queued"),
        (completed, "completed"),
        (failed, "failed"),
        (cancelled, "cancelled"),
    ] {
        if count > 0 {
            parts.push(format!("{count} {label}"));
        }
    }
    if parts.is_empty() {
        "no jobs".to_owned()
    } else {
        format!("Jobs · {}", parts.join(" · "))
    }
}

fn job_lines(panel: &JobsPanel, height: u16, width: u16, theme: Theme) -> Vec<Line<'static>> {
    if panel.jobs.is_empty() {
        return vec![Line::from(Span::styled(
            " No orchestration jobs.",
            Style::default().fg(theme.muted),
        ))];
    }
    let viewport = usize::from(height.max(1));
    let start = panel
        .selected
        .saturating_add(1)
        .saturating_sub(viewport)
        .min(panel.jobs.len().saturating_sub(viewport));
    panel
        .jobs
        .iter()
        .enumerate()
        .skip(start)
        .take(viewport)
        .map(|(index, job)| {
            let selected = index == panel.selected;
            let marker = if selected { "›" } else { " " };
            let style = if selected {
                Style::default().fg(theme.text).bg(theme.selected_bg)
            } else {
                Style::default().fg(theme.text)
            };
            let status = job_status_label(job.job_status);
            let name = pad_to_width(&sanitize(&job.display_name), 16);
            let id = truncate(&sanitize(&job.job_id), 12);
            let prefix = format!(" {marker} {status:<9} {name} {id}");
            let activity = job
                .progress
                .as_ref()
                .map(progress_row_text)
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| job.summary.clone().unwrap_or_default());
            let separator = " · ";
            let available = usize::from(width)
                .saturating_sub(display_width(&prefix))
                .saturating_sub(display_width(separator))
                .min(MAX_BODY_WIDTH);
            let text = if activity.is_empty() || available == 0 {
                prefix
            } else {
                format!(
                    "{prefix}{separator}{}",
                    truncate(&sanitize(&activity), available)
                )
            };
            Line::from(Span::styled(
                truncate(&text, usize::from(width)),
                style.add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            ))
        })
        .collect()
}

fn irc_lines(panel: &JobsPanel, height: u16, width: u16, theme: Theme) -> Vec<Line<'static>> {
    if panel.messages.is_empty() {
        return vec![Line::from(Span::styled(
            " No hub mailbox messages.",
            Style::default().fg(theme.muted),
        ))];
    }
    let viewport = usize::from(height.max(1));
    let start = panel
        .selected
        .saturating_add(1)
        .saturating_sub(viewport)
        .min(panel.messages.len().saturating_sub(viewport));
    panel
        .messages
        .iter()
        .enumerate()
        .skip(start)
        .take(viewport)
        .map(|(index, message)| {
            let selected = index == panel.selected;
            let marker = if selected { "›" } else { " " };
            let style = if selected {
                Style::default().fg(theme.text).bg(theme.selected_bg)
            } else {
                Style::default().fg(theme.text)
            };
            let view = orchestration_irc_view_from_mailbox(
                &message.id,
                &message.from,
                &message.to,
                &message.body,
                message.reply_to.as_deref(),
            );
            let reply = view
                .reply_metadata()
                .map(|metadata| format!(" · {metadata}"))
                .unwrap_or_default();
            let from = truncate(&sanitize(view.from.as_ref()), 12);
            let to = truncate(&sanitize(view.to.as_ref()), 12);
            let prefix = format!(" {marker} {from} ➤ {to} · ");
            let available = usize::from(width)
                .saturating_sub(display_width(&prefix))
                .saturating_sub(display_width(&reply))
                .min(MAX_BODY_WIDTH);
            let text = format!(
                "{prefix}{}{reply}",
                truncate(&sanitize(view.body.as_ref()), available)
            );
            Line::from(Span::styled(truncate(&text, usize::from(width)), style))
        })
        .collect()
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

fn pad_to_width(value: &str, width: usize) -> String {
    let fitted = truncate(value, width);
    let padding = width.saturating_sub(display_width(&fitted));
    format!("{fitted}{}", " ".repeat(padding))
}

fn bound_messages(mut messages: Vec<MailboxMessage>) -> Vec<MailboxMessage> {
    if messages.len() > MAX_IRC_ROWS {
        let skip = messages.len() - MAX_IRC_ROWS;
        messages.drain(..skip);
    }
    messages
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job_card_adapter::JobCardPresentationAdapter;
    use pi_coding::{JobSnapshot, JobStatus};

    fn job(id: &str, agent: &str, status: JobStatus) -> JobSnapshot {
        JobSnapshot {
            id: id.to_owned(),
            agent_id: agent.to_owned(),
            agent: "task".to_owned(),
            parent_id: "Main".to_owned(),
            description: Some(format!("work for {agent}")),
            todo_task_id: None,
            workflow_id: None,
            workflow_generation: None,
            status,
            created_at: 1,
            started_at: Some(1),
            finished_at: None,
            result: None,
            soft_budget_exhausted: false,
        }
    }

    #[test]
    fn jobs_page_lists_status_and_tab_switches_to_irc() {
        let mut adapter = JobCardPresentationAdapter::new();
        adapter.replace_snapshots(
            "group",
            vec![
                job("job-1", "Scout", JobStatus::Running),
                job("job-2", "Reviewer", JobStatus::Queued),
            ],
            Vec::new(),
        );
        let messages = vec![MailboxMessage {
            id: "m1".to_owned(),
            from: "Scout".to_owned(),
            to: "Main".to_owned(),
            body: "mapped files".to_owned(),
            timestamp: 2,
            reply_to: Some("ask-1".to_owned()),
        }];
        let mut panel = JobsPanel::jobs(adapter.cards_in_source_order(), messages, true);
        assert_eq!(panel.page, JobsPanelPage::Jobs);
        assert_eq!(panel.row_count(), 2);
        assert!(panel.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
        assert_eq!(panel.page, JobsPanelPage::Irc);
        assert_eq!(panel.row_count(), 1);
        assert!(!panel.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    }

    #[test]
    fn irc_page_keeps_reply_correlation() {
        let messages = vec![MailboxMessage {
            id: "m1".to_owned(),
            from: "Scout".to_owned(),
            to: "Main".to_owned(),
            body: "done".to_owned(),
            timestamp: 2,
            reply_to: Some("ask-9".to_owned()),
        }];
        let panel = JobsPanel::irc(Vec::new(), messages, true);
        let lines = irc_lines(&panel, 8, 110, crate::theme::DARK);
        let text = lines
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("Scout"), "{text}");
        assert!(text.contains("Main"), "{text}");
        assert!(text.contains("reply to ask-9"), "{text}");
    }

    #[test]
    fn irc_page_truncates_cjk_body_by_display_width() {
        let panel = JobsPanel::irc(
            Vec::new(),
            vec![MailboxMessage {
                id: "m1".to_owned(),
                from: "Scout".to_owned(),
                to: "Main".to_owned(),
                body: "界".repeat(80),
                timestamp: 2,
                reply_to: None,
            }],
            true,
        );
        let lines = irc_lines(&panel, 8, 110, crate::theme::DARK);
        let text = lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(pi_coding::markdown::display_width(&text) <= 110, "{text}");
        assert!(text.ends_with('…'), "{text}");
    }

    #[test]
    fn irc_page_bounds_prefix_body_and_reply_to_viewport() {
        let panel = JobsPanel::irc(
            Vec::new(),
            vec![MailboxMessage {
                id: "m1".to_owned(),
                from: "发送者发送者发送者".to_owned(),
                to: "接收者接收者接收者".to_owned(),
                body: "消息".repeat(80),
                timestamp: 2,
                reply_to: Some("correlation-id-that-exceeds-the-row".to_owned()),
            }],
            true,
        );
        let width = 50;
        let text = irc_lines(&panel, 8, width, crate::theme::DARK)[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(display_width(&text) <= usize::from(width), "{text}");
    }
}
