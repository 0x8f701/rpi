//! Ratatui-neutral inline job and subagent cards reduced from orchestration events.

use std::collections::HashMap;

use pi_coding::{
    AgentSnapshot, AgentStatus, ApplicationEvent, JobSnapshot, JobStatus, OrchestrationEvent,
    redact_value,
};
use serde::Serialize;

const MAX_DESCRIPTION_CHARS: usize = 160;
const MAX_RESULT_CHARS: usize = 600;
const MAX_ERROR_CHARS: usize = 300;
const MAX_REFERENCE_CHARS: usize = 240;

/// Semantic row role for later TUI styling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobCardRowRole {
    Title,
    Description,
    Timing,
    Usage,
    Result,
    Error,
    Reference,
    Aggregate,
}

/// One terminal-neutral job-card row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobCardRow {
    pub job_id: Option<String>,
    pub role: JobCardRowRole,
    pub text: String,
}

/// One projected card, stable by job id and ordered by first observation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobCardRows {
    pub job_id: String,
    pub ordinal: u64,
    pub agent_id: String,
    pub agent: String,
    pub display_name: String,
    pub todo_task_id: Option<String>,
    pub job_status: JobStatus,
    pub agent_status: Option<AgentStatus>,
    pub summary: Option<String>,
    pub rows: Vec<JobCardRow>,
}

/// One Task delegation card containing every child in the active runtime group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskCardRows {
    pub group_id: String,
    pub context: String,
    pub children: Vec<JobCardRows>,
    pub aggregate: JobCardRow,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RequestedChild {
    name: Option<String>,
    agent: Option<String>,
    task: String,
}

#[derive(Clone, Debug)]
struct ProjectedJob {
    ordinal: u64,
    snapshot: JobSnapshot,
    agent: Option<AgentSnapshot>,
}

/// Truthful display-only projection of canonical orchestration events.
#[derive(Clone, Debug, Default)]
pub struct JobCardPresentationAdapter {
    jobs: HashMap<String, ProjectedJob>,
    agents: HashMap<String, AgentSnapshot>,
    next_ordinal: u64,
    group_id: Option<String>,
    context: String,
    requested_children: Vec<RequestedChild>,
}

impl JobCardPresentationAdapter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply_application_event(&mut self, event: &ApplicationEvent) {
        if let ApplicationEvent::Orchestration(event) = event {
            self.apply_orchestration_event(event);
        }
    }

    pub fn apply_orchestration_event(&mut self, event: &OrchestrationEvent) {
        let group_id = match event {
            OrchestrationEvent::JobUpdated { group_id, .. }
            | OrchestrationEvent::AgentUpdated { group_id, .. }
            | OrchestrationEvent::MessageDelivered { group_id, .. } => group_id,
        };
        if self.group_id.as_deref().is_some_and(|current| current != group_id) {
            self.jobs.clear();
            self.agents.clear();
            self.next_ordinal = 0;
        }
        self.group_id = Some(group_id.clone());
        match event {
            OrchestrationEvent::JobUpdated { job, .. } => self.upsert_job(job.clone()),
            OrchestrationEvent::AgentUpdated { agent, .. } => self.upsert_agent(agent.clone()),
            OrchestrationEvent::MessageDelivered { .. } => {}
        }
    }

    pub fn replace_snapshots(
        &mut self,
        group_id: impl Into<String>,
        jobs: Vec<JobSnapshot>,
        agents: Vec<AgentSnapshot>,
    ) {
        self.jobs.clear();
        self.agents.clear();
        self.next_ordinal = 0;
        self.group_id = Some(group_id.into());
        for agent in agents {
            self.upsert_agent(agent);
        }
        for job in jobs {
            self.upsert_job(job);
        }
    }

    pub fn set_task_request(
        &mut self,
        context: String,
        children: impl IntoIterator<Item = (Option<String>, Option<String>, String)>,
    ) {
        self.context = context;
        self.requested_children = children
            .into_iter()
            .map(|(name, agent, task)| RequestedChild { name, agent, task })
            .collect();
    }

    #[must_use]
    pub fn task_card(&self) -> Option<TaskCardRows> {
        let group_id = self.group_id.clone()?;
        let mut children = self.cards_in_source_order();
        if children.is_empty() {
            return None;
        }
        for (index, child) in children.iter_mut().enumerate() {
            let requested = self.requested_children.iter().find(|requested| {
                requested.name.as_deref() == Some(child.agent_id.as_str())
            }).or_else(|| self.requested_children.get(index));
            if let Some(requested) = requested {
                child.summary = Some(requested.task.clone());
                if let Some(agent) = &requested.agent {
                    child.agent = agent.clone();
                }
            }
        }
        Some(TaskCardRows {
            group_id,
            context: self.context.clone(),
            aggregate: self.aggregate_row().expect("task card has children"),
            children,
        })
    }

    pub fn clear(&mut self) {
        self.jobs.clear();
        self.agents.clear();
        self.next_ordinal = 0;
        self.group_id = None;
        self.context.clear();
        self.requested_children.clear();
    }

    #[must_use]
    pub fn cards_in_source_order(&self) -> Vec<JobCardRows> {
        let mut jobs = self.jobs.values().collect::<Vec<_>>();
        jobs.sort_by_key(|job| job.ordinal);
        jobs.into_iter().map(card_rows).collect()
    }

    #[must_use]
    pub fn aggregate_row(&self) -> Option<JobCardRow> {
        if self.jobs.is_empty() {
            return None;
        }
        let mut queued = 0usize;
        let mut running = 0usize;
        let mut parked = 0usize;
        let mut completed = 0usize;
        let mut failed = 0usize;
        let mut cancelled = 0usize;
        for job in self.jobs.values() {
            match job.snapshot.status {
                JobStatus::Queued => queued += 1,
                JobStatus::Running => running += 1,
                JobStatus::Completed => completed += 1,
                JobStatus::Failed => failed += 1,
                JobStatus::Cancelled => cancelled += 1,
            }
            if job.agent.as_ref().is_some_and(|agent| agent.status == AgentStatus::Parked) {
                parked += 1;
            }
        }
        let mut parts = Vec::new();
        for (count, label) in [
            (running, "running"),
            (queued, "queued"),
            (parked, "parked"),
            (completed, "completed"),
            (failed, "failed"),
            (cancelled, "cancelled"),
        ] {
            if count > 0 {
                parts.push(format!("{count} {label}"));
            }
        }
        Some(JobCardRow {
            job_id: None,
            role: JobCardRowRole::Aggregate,
            text: format!("Jobs · {}", parts.join(" · ")),
        })
    }

    #[must_use]
    pub fn running_count(&self) -> usize {
        self.jobs
            .values()
            .filter(|job| matches!(job.snapshot.status, JobStatus::Queued | JobStatus::Running))
            .count()
    }

    /// Resolve a human-facing agent label; falls back to the stable id.
    #[must_use]
    pub fn agent_display_name(&self, agent_id: &str) -> String {
        self.agents
            .get(agent_id)
            .map(|agent| {
                let name = agent.display_name.trim();
                if name.is_empty() {
                    agent.id.clone()
                } else {
                    agent.display_name.clone()
                }
            })
            .unwrap_or_else(|| agent_id.to_owned())
    }

    fn upsert_job(&mut self, snapshot: JobSnapshot) {
        let agent = self.agents.get(&snapshot.agent_id).cloned();
        if let Some(job) = self.jobs.get_mut(&snapshot.id) {
            job.snapshot = snapshot;
            if agent.is_some() {
                job.agent = agent;
            }
            return;
        }
        let ordinal = self.next_ordinal;
        self.next_ordinal = self.next_ordinal.saturating_add(1);
        self.jobs.insert(
            snapshot.id.clone(),
            ProjectedJob {
                ordinal,
                snapshot,
                agent,
            },
        );
    }

    fn upsert_agent(&mut self, agent: AgentSnapshot) {
        self.agents.insert(agent.id.clone(), agent.clone());
        for job in self.jobs.values_mut().filter(|job| job.snapshot.agent_id == agent.id) {
            job.agent = Some(agent.clone());
        }
    }
}

fn card_rows(job: &ProjectedJob) -> JobCardRows {
    let snapshot = &job.snapshot;
    let agent_status = job.agent.as_ref().map(|agent| agent.status);
    let mut title_status = job_status_label(snapshot.status).to_owned();
    if agent_status == Some(AgentStatus::Parked) {
        title_status.push_str(" · parked");
    }
    let mut rows = vec![row(
        snapshot,
        JobCardRowRole::Title,
        format!("{} ({}) · {title_status}", snapshot.agent_id, snapshot.agent),
    )];
    if let Some(description) = snapshot.description.as_deref().filter(|value| !value.is_empty()) {
        rows.push(row(
            snapshot,
            JobCardRowRole::Description,
            bounded_redacted(description, MAX_DESCRIPTION_CHARS),
        ));
    }
    let timing = timing_text(snapshot);
    if !timing.is_empty() {
        rows.push(row(snapshot, JobCardRowRole::Timing, timing));
    }
    if let Some(result) = &snapshot.result {
        let usage = usage_text(&result.usage);
        if !usage.is_empty() {
            rows.push(row(snapshot, JobCardRowRole::Usage, usage));
        }
        if !result.output.trim().is_empty() {
            rows.push(row(
                snapshot,
                JobCardRowRole::Result,
                bounded_redacted(&result.output, MAX_RESULT_CHARS),
            ));
        }
        if let Some(error) = result.error.as_deref().filter(|value| !value.trim().is_empty()) {
            rows.push(row(
                snapshot,
                JobCardRowRole::Error,
                bounded_redacted(error, MAX_ERROR_CHARS),
            ));
        }
        for reference in [&result.artifact_ref, &result.history_ref, &result.artifact_uri] {
            if !reference.is_empty() {
                rows.push(row(
                    snapshot,
                    JobCardRowRole::Reference,
                    truncate_chars(reference, MAX_REFERENCE_CHARS),
                ));
            }
        }
    }
    JobCardRows {
        job_id: snapshot.id.clone(),
        ordinal: job.ordinal,
        agent_id: snapshot.agent_id.clone(),
        agent: snapshot.agent.clone(),
        display_name: job
            .agent
            .as_ref()
            .map(|agent| agent.display_name.clone())
            .unwrap_or_else(|| snapshot.agent_id.clone()),
        todo_task_id: snapshot.todo_task_id.clone(),
        job_status: snapshot.status,
        agent_status,
        summary: None,
        rows,
    }
}

fn row(snapshot: &JobSnapshot, role: JobCardRowRole, text: String) -> JobCardRow {
    JobCardRow {
        job_id: Some(snapshot.id.clone()),
        role,
        text,
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

fn timing_text(snapshot: &JobSnapshot) -> String {
    let age = snapshot
        .started_at
        .unwrap_or(snapshot.created_at)
        .saturating_sub(snapshot.created_at);
    let runtime = snapshot.finished_at.zip(snapshot.started_at).map(|(finished, started)| {
        finished.saturating_sub(started)
    });
    match (snapshot.started_at, runtime) {
        (None, _) => "request queued".to_owned(),
        (Some(_), None) if age == 0 => "running".to_owned(),
        (Some(_), None) => format!("queued {} · running", format_duration(age)),
        (Some(_), Some(runtime)) if age == 0 => format!("runtime {}", format_duration(runtime)),
        (Some(_), Some(runtime)) => format!(
            "queued {} · runtime {}",
            format_duration(age),
            format_duration(runtime)
        ),
    }
}

fn format_duration(milliseconds: u64) -> String {
    if milliseconds < 1_000 {
        return format!("{milliseconds}ms");
    }
    let seconds = milliseconds / 1_000;
    if seconds < 60 {
        return format!("{seconds}s");
    }
    format!("{}m {}s", seconds / 60, seconds % 60)
}

fn usage_text(usage: &pi_ai::Usage) -> String {
    let total = if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        [usage.input, usage.output, usage.cache_read, usage.cache_write]
            .into_iter()
            .filter(|value| *value > 0)
            .sum()
    };
    let mut parts = Vec::new();
    if total > 0 {
        parts.push(format!("{total} tokens"));
    }
    if usage.input > 0 {
        parts.push(format!("{} in", usage.input));
    }
    if usage.output > 0 {
        parts.push(format!("{} out", usage.output));
    }
    if usage.cache_read > 0 {
        parts.push(format!("{} cache", usage.cache_read));
    }
    if usage.cost.total > 0.0 {
        parts.push(format!("${:.4}", usage.cost.total));
    }
    parts.join(" · ")
}

fn bounded_redacted(text: &str, max_chars: usize) -> String {
    let value = redact_value(&serde_json::Value::String(text.to_owned()));
    let redacted = value.as_str().unwrap_or_default();
    truncate_chars(redacted, max_chars)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    if max_chars <= 1 {
        return text.chars().take(max_chars).collect();
    }
    let mut output = text.chars().take(max_chars - 1).collect::<String>();
    output.push('…');
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_coding::{AgentStatus, TaskResult};

    fn job(id: &str, agent_id: &str, status: JobStatus) -> JobSnapshot {
        JobSnapshot {
            id: id.to_owned(),
            agent_id: agent_id.to_owned(),
            agent: "task".to_owned(),
            parent_id: "Main".to_owned(),
            description: Some(format!("work for {agent_id}")),
            todo_task_id: None,
            workflow_id: None,
            workflow_generation: None,
            status,
            created_at: 1_000,
            started_at: None,
            finished_at: None,
            result: None,
        }
    }

    fn agent(id: &str, status: AgentStatus) -> AgentSnapshot {
        AgentSnapshot {
            id: id.to_owned(),
            display_name: format!("task: work for {id}"),
            parent_id: Some("Main".to_owned()),
            status,
            created_at: 1_000,
            last_activity: 1_000,
            unread: 0,
            artifact_ref: None,
            history_ref: None,
        }
    }

    fn result(id: &str, output: &str, error: Option<&str>) -> TaskResult {
        TaskResult {
            index: 0,
            id: id.to_owned(),
            agent: "task".to_owned(),
            status: if error.is_some() { AgentStatus::Idle } else { AgentStatus::Idle },
            output: output.to_owned(),
            usage: pi_ai::Usage {
                input: 10,
                output: 5,
                total_tokens: 15,
                ..pi_ai::Usage::default()
            },
            error: error.map(str::to_owned),
            artifact_ref: format!("agent://{id}"),
            history_ref: format!("history://{id}"),
            artifact_uri: format!("artifact://{id}"),
        }
    }

    #[test]
    fn queued_running_completed_updates_one_card() {
        let mut adapter = JobCardPresentationAdapter::new();
        let mut snapshot = job("job-1", "Child", JobStatus::Queued);
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated { group_id: "group".to_owned(), job: snapshot.clone() });
        snapshot.status = JobStatus::Running;
        snapshot.started_at = Some(1_100);
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated { group_id: "group".to_owned(), job: snapshot.clone() });
        snapshot.status = JobStatus::Completed;
        snapshot.finished_at = Some(2_100);
        snapshot.result = Some(result("Child", "done", None));
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated { group_id: "group".to_owned(), job: snapshot });
        let cards = adapter.cards_in_source_order();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].job_status, JobStatus::Completed);
        assert!(cards[0].rows.iter().any(|row| row.text == "done"));
        assert!(cards[0].rows.iter().any(|row| row.text.contains("runtime 1s")));
    }

    #[test]
    fn failed_cancelled_and_parked_are_truthful() {
        let mut adapter = JobCardPresentationAdapter::new();
        let mut failed = job("failed", "Same", JobStatus::Failed);
        failed.result = Some(result("Same", "", Some("provider failed")));
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated { group_id: "group".to_owned(), job: failed });
        adapter.apply_orchestration_event(&OrchestrationEvent::AgentUpdated { group_id: "group".to_owned(), agent: agent("Same", AgentStatus::Parked) });
        let cancelled = job("cancelled", "Other", JobStatus::Cancelled);
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated { group_id: "group".to_owned(), job: cancelled });
        let cards = adapter.cards_in_source_order();
        assert_eq!(cards[0].job_status, JobStatus::Failed);
        assert_eq!(cards[0].agent_status, Some(AgentStatus::Parked));
        assert!(cards[0].rows[0].text.contains("failed · parked"));
        assert_eq!(cards[1].job_status, JobStatus::Cancelled);
    }

    #[test]
    fn concurrent_same_agent_jobs_keep_job_identity_order() {
        let mut adapter = JobCardPresentationAdapter::new();
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated { group_id: "group".to_owned(), job: job("first", "Shared", JobStatus::Running) });
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated { group_id: "group".to_owned(), job: job("second", "Shared", JobStatus::Queued) });
        let mut first = job("first", "Shared", JobStatus::Completed);
        first.result = Some(result("Shared", "first result", None));
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated { group_id: "group".to_owned(), job: first });
        let cards = adapter.cards_in_source_order();
        assert_eq!(cards.iter().map(|card| card.job_id.as_str()).collect::<Vec<_>>(), ["first", "second"]);
        assert_eq!(cards[0].job_status, JobStatus::Completed);
        assert_eq!(cards[1].job_status, JobStatus::Queued);
    }

    #[test]
    fn result_is_redacted_bounded_and_terminal_update_not_duplicated() {
        let secret = "credential-redaction-fixture-value";
        let mut adapter = JobCardPresentationAdapter::new();
        let mut snapshot = job("terminal", "Child", JobStatus::Completed);
        snapshot.result = Some(result("Child", &format!("token={secret} {}", "é".repeat(700)), None));
        let event = OrchestrationEvent::JobUpdated { group_id: "group".to_owned(), job: snapshot };
        adapter.apply_orchestration_event(&event);
        adapter.apply_orchestration_event(&event);
        let cards = adapter.cards_in_source_order();
        assert_eq!(cards.len(), 1);
        let encoded = serde_json::to_string(&cards).expect("serialize cards");
        assert!(!encoded.contains(secret));
        assert!(encoded.contains("[REDACTED]"));
        let result_row = cards[0].rows.iter().find(|row| row.role == JobCardRowRole::Result).expect("result row");
        assert!(result_row.text.chars().count() <= MAX_RESULT_CHARS);
        assert!(result_row.text.ends_with('…'));
    }

    #[test]
    fn new_runtime_group_replaces_stale_projection() {
        let mut adapter = JobCardPresentationAdapter::new();
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
            group_id: "old".to_owned(),
            job: job("old-job", "Old", JobStatus::Completed),
        });
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
            group_id: "new".to_owned(),
            job: job("new-job", "New", JobStatus::Queued),
        });
        let cards = adapter.cards_in_source_order();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].job_id, "new-job");
    }

    #[test]
    fn aggregate_reports_running_queue_and_terminal_counts() {
        let mut adapter = JobCardPresentationAdapter::new();
        for snapshot in [
            job("q", "Q", JobStatus::Queued),
            job("r", "R", JobStatus::Running),
            job("c", "C", JobStatus::Completed),
        ] {
            adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated { group_id: "group".to_owned(), job: snapshot });
        }
        adapter.apply_orchestration_event(&OrchestrationEvent::AgentUpdated { group_id: "group".to_owned(), agent: agent("C", AgentStatus::Parked) });
        assert_eq!(adapter.running_count(), 2);
        let aggregate = adapter.aggregate_row().expect("aggregate");
        assert!(aggregate.text.contains("1 running"));
        assert!(aggregate.text.contains("1 queued"));
        assert!(aggregate.text.contains("1 parked"));
        assert!(aggregate.text.contains("1 completed"));
    }
    #[test]
    fn task_card_groups_children_and_merges_by_stable_job_id() {
        let mut adapter = JobCardPresentationAdapter::new();
        adapter.set_task_request(
            "# Goal\nShip the Task card\n\n# Contract\nKeep stable ids".to_owned(),
            [
                (Some("Alpha".to_owned()), Some("reviewer".to_owned()), "Review adapter".to_owned()),
                (Some("Beta".to_owned()), None, "Render card".to_owned()),
            ],
        );
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated { group_id: "group".to_owned(), job: job("job-a", "Alpha", JobStatus::Running) });
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated { group_id: "group".to_owned(), job: job("job-b", "Beta", JobStatus::Queued) });
        let mut updated = job("job-a", "Alpha", JobStatus::Completed);
        updated.result = Some(result("Alpha", "done", None));
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated { group_id: "group".to_owned(), job: updated });
        let card = adapter.task_card().expect("task card");
        assert_eq!(card.children.len(), 2);
        assert_eq!(card.children.iter().map(|child| child.job_id.as_str()).collect::<Vec<_>>(), ["job-a", "job-b"]);
        assert_eq!(card.children[0].summary.as_deref(), Some("Review adapter"));
        assert_eq!(card.children[0].agent, "reviewer");
        assert_eq!(card.children[0].job_status, JobStatus::Completed);
        assert_eq!(card.children[1].job_status, JobStatus::Queued);
        assert!(card.context.contains("# Contract"));
    }
}
