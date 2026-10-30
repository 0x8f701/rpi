//! Ratatui-neutral inline job and subagent cards reduced from orchestration events.

use std::collections::HashMap;

use crate::tool_card_adapter::{
    TASK_TARGET_CHAR_LIMIT, TaskDelegationSections, task_delegation_sections,
};
use pi_coding::{
    AgentSnapshot, AgentStatus, ApplicationEvent, JobSnapshot, JobStatus, MailboxMessage,
    OrchestrationEvent, redact_value,
};
use serde::Serialize;

/// Task description row budget, aligned with the backend task description
/// bound (`MAX_AGENT_DESCRIPTION_LENGTH`, 1024). Compact overview rows cap
/// their own width and detail pages wrap at the panel width, so the raise
/// keeps cards compact while details stay complete.
const MAX_DESCRIPTION_CHARS: usize = 1024;
const MAX_RESULT_CHARS: usize = 600;
const MAX_ERROR_CHARS: usize = 300;
const MAX_REFERENCE_CHARS: usize = 240;
/// Character budget for the requested agent/persona overlay.
const MAX_REQUESTED_AGENT_CHARS: usize = 64;
/// Bound for the per-subagent live activity log kept for the detail panel.
const MAX_JOB_EVENT_LOG: usize = 64;
/// Bound for a single live activity line (a child IRC progress message body).
const MAX_ACTIVITY_CHARS: usize = 140;

/// Semantic row role for later TUI styling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobCardRowRole {
    Title,
    Description,
    /// Live per-subagent progress line: latest activity · elapsed (or coarse stage).
    Progress,
    Timing,
    Usage,
    Result,
    Error,
    Reference,
    Aggregate,
}

/// Origin of one entry in the live per-subagent activity log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobEventKind {
    /// A job lifecycle transition (queued/started/completed/failed/cancelled).
    Job,
    /// A live child → Main orchestration IRC message (progress report).
    Message,
}

/// One timestamped entry of the live activity log shown in the detail panel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobEventLogEntry {
    /// Epoch millis of the event; job transitions prefer snapshot timestamps.
    pub at: u64,
    pub kind: JobEventKind,
    /// Bounded and credential-redacted display text.
    pub text: String,
}

/// Live per-subagent progress projected from the latest orchestration events.
/// `stage` is always present; the other fields degrade gracefully to `None`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobCardProgress {
    /// Coarse stage label derived from the job status (queued/running/…).
    pub stage: Option<String>,
    /// Latest live activity text (child IRC progress body), when one arrived.
    pub activity: Option<String>,
    /// Epoch millis of the latest activity, when known.
    pub activity_at: Option<u64>,
    /// Elapsed wall-clock millis: `now - start` while active, runtime when settled.
    pub elapsed_ms: Option<u64>,
    /// Bounded event log, oldest first, for the detail panel.
    pub events: Vec<JobEventLogEntry>,
    /// Where the full child transcript lives (`history://<id>`), when known.
    pub history_ref: Option<String>,
    /// Artifact handle (`agent://<id>`), when known.
    pub artifact_ref: Option<String>,
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
    /// Live progress detail for the card and the subagent detail panel.
    pub progress: Option<JobCardProgress>,
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
    /// Bounded, oldest-first activity log for the detail panel.
    events: Vec<JobEventLogEntry>,
    /// Latest live activity text (child IRC progress body), when one arrived.
    activity: Option<String>,
    activity_at: Option<u64>,
}

impl ProjectedJob {
    fn push_event(&mut self, at: u64, kind: JobEventKind, text: String) {
        self.events.push(JobEventLogEntry { at, kind, text });
        if self.events.len() > MAX_JOB_EVENT_LOG {
            let excess = self.events.len() - MAX_JOB_EVENT_LOG;
            self.events.drain(..excess);
        }
    }
}

/// Truthful display-only projection of canonical orchestration events.
#[derive(Clone, Debug, Default)]
pub struct JobCardPresentationAdapter {
    jobs: HashMap<String, ProjectedJob>,
    agents: HashMap<String, AgentSnapshot>,
    next_ordinal: u64,
    group_id: Option<String>,
    task_context: TaskDelegationSections,
    requested_children: Vec<RequestedChild>,
    /// Runtime group that owns the stored request after the first following
    /// orchestration event. `None` while a just-observed Task call awaits its
    /// group, so a clean group rollover does not discard the new request.
    task_request_group_id: Option<String>,
    task_request_pending: bool,
    /// Wall-clock epoch millis for live elapsed rendering. `0` (unset) means
    /// "unknown": active jobs then fall back to coarse stage-only text.
    now: u64,
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
        self.prepare_group(group_id);
        match event {
            OrchestrationEvent::JobUpdated { job, .. } => self.upsert_job(job.clone()),
            OrchestrationEvent::AgentUpdated { agent, .. } => self.upsert_agent(agent.clone()),
            OrchestrationEvent::MessageDelivered { message, .. } => self.record_message(message),
        }
    }

    /// Attach a live child → Main IRC message to the child's active job as the
    /// latest progress line. With concurrent jobs on one agent the running job
    /// wins, otherwise the newest; messages from unknown agents are dropped.
    fn record_message(&mut self, message: &MailboxMessage) {
        let candidate = self
            .jobs
            .values_mut()
            .filter(|job| job.snapshot.agent_id == message.from)
            .max_by_key(|job| match job.snapshot.status {
                JobStatus::Running => 2,
                JobStatus::Queued => 1,
                JobStatus::Completed | JobStatus::Failed | JobStatus::Cancelled => 0,
            });
        if let Some(job) = candidate {
            let text = bounded_redacted(&message.body, MAX_ACTIVITY_CHARS);
            job.push_event(message.timestamp, JobEventKind::Message, text.clone());
            job.activity = Some(text);
            job.activity_at = Some(message.timestamp);
        }
    }

    pub fn replace_snapshots(
        &mut self,
        group_id: impl Into<String>,
        jobs: Vec<JobSnapshot>,
        agents: Vec<AgentSnapshot>,
    ) {
        let group_id = group_id.into();
        self.prepare_group(&group_id);
        self.jobs.clear();
        self.agents.clear();
        self.next_ordinal = 0;
        for agent in agents {
            self.upsert_agent(agent);
        }
        for job in jobs {
            self.upsert_job(job);
        }
    }

    /// Provide the wall-clock for live elapsed rendering. Callers refresh this
    /// on their render tick; `0` (the default) leaves active jobs on the
    /// coarse stage-only fallback.
    pub fn set_now(&mut self, now: u64) {
        self.now = now;
    }

    /// Store one safe detail authority for the Task delegation request.
    /// Callers still parse the tool arguments during the TUI cutover; this
    /// adapter owns the redacted/bounded request details exposed afterward.
    pub fn set_task_request(
        &mut self,
        context: String,
        children: impl IntoIterator<Item = (Option<String>, Option<String>, String)>,
    ) {
        self.task_context = task_delegation_sections(&redacted_text(&context));
        self.requested_children = children
            .into_iter()
            .map(|(name, agent, task)| RequestedChild {
                name,
                agent: agent.and_then(|agent| {
                    let agent = bounded_redacted(agent.trim(), MAX_REQUESTED_AGENT_CHARS);
                    (!agent.is_empty()).then_some(agent)
                }),
                task: bounded_redacted(task.trim(), TASK_TARGET_CHAR_LIMIT),
            })
            .collect();
        self.task_request_group_id = None;
        self.task_request_pending = true;
    }

    /// Bounded, credential-redacted shared Task context split for the explicit
    /// Todo detail surface. This is the sole structured context authority;
    /// legacy Task-card text is reconstructed from these sections.
    #[must_use]
    pub(crate) fn task_context_sections(&self) -> &TaskDelegationSections {
        &self.task_context
    }

    fn prepare_group(&mut self, group_id: &str) {
        if self
            .group_id
            .as_deref()
            .is_some_and(|current| current != group_id)
        {
            self.jobs.clear();
            self.agents.clear();
            self.next_ordinal = 0;
        }
        if self.task_request_pending {
            self.task_request_group_id = Some(group_id.to_owned());
            self.task_request_pending = false;
        } else if self
            .task_request_group_id
            .as_deref()
            .is_some_and(|owner| owner != group_id)
        {
            self.clear_task_request();
        }
        self.group_id = Some(group_id.to_owned());
    }

    fn clear_task_request(&mut self) {
        self.task_context = TaskDelegationSections::default();
        self.requested_children.clear();
        self.task_request_group_id = None;
        self.task_request_pending = false;
    }

    #[must_use]
    pub fn task_card(&self) -> Option<TaskCardRows> {
        let group_id = self.group_id.clone()?;
        let children = self.cards_in_source_order();
        if children.is_empty() {
            return None;
        }
        Some(TaskCardRows {
            group_id,
            context: legacy_task_context(&self.task_context),
            aggregate: self.aggregate_row().expect("task card has children"),
            children,
        })
    }

    pub fn clear(&mut self) {
        self.jobs.clear();
        self.agents.clear();
        self.next_ordinal = 0;
        self.group_id = None;
        self.clear_task_request();
    }

    #[must_use]
    pub fn cards_in_source_order(&self) -> Vec<JobCardRows> {
        let mut jobs = self.jobs.values().collect::<Vec<_>>();
        jobs.sort_by_key(|job| job.ordinal);
        let mut cards = jobs
            .into_iter()
            .map(|job| card_rows(job, self.now))
            .collect::<Vec<_>>();
        let mut assignments = vec![None; cards.len()];
        let mut consumed = vec![false; self.requested_children.len()];

        // Exact requested names are authoritative even when runtime source
        // order differs. Reserve those first so index fallback cannot reuse a
        // named request that already belongs to another card.
        for (card_index, card) in cards.iter().enumerate() {
            if let Some(request_index) =
                self.requested_children
                    .iter()
                    .enumerate()
                    .position(|(request_index, requested)| {
                        !consumed[request_index]
                            && requested.name.as_deref() == Some(card.agent_id.as_str())
                    })
            {
                assignments[card_index] = Some(request_index);
                consumed[request_index] = true;
            }
        }
        // Unnamed/unmatched runtime children consume only requests not already
        // claimed above, preserving request order without duplicate overlays.
        let mut remaining = consumed
            .iter()
            .enumerate()
            .filter_map(|(index, used)| (!used).then_some(index));
        for assignment in assignments
            .iter_mut()
            .filter(|assignment| assignment.is_none())
        {
            *assignment = remaining.next();
        }
        for (card, request_index) in cards.iter_mut().zip(assignments) {
            let Some(requested) =
                request_index.and_then(|index| self.requested_children.get(index))
            else {
                continue;
            };
            card.summary = Some(requested.task.clone());
            if let Some(agent) = &requested.agent {
                card.agent.clone_from(agent);
            }
        }
        cards
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
            if job
                .agent
                .as_ref()
                .is_some_and(|agent| agent.status == AgentStatus::Parked)
            {
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
            if job.snapshot.status != snapshot.status {
                let (at, text) = transition_event(&job.snapshot, &snapshot);
                job.push_event(at, JobEventKind::Job, text);
            }
            job.snapshot = snapshot;
            if agent.is_some() {
                job.agent = agent;
            }
            return;
        }
        let ordinal = self.next_ordinal;
        self.next_ordinal = self.next_ordinal.saturating_add(1);
        let mut events = Vec::new();
        if let Some((at, text)) = initial_event(&snapshot) {
            events.push(JobEventLogEntry {
                at,
                kind: JobEventKind::Job,
                text,
            });
        }
        self.jobs.insert(
            snapshot.id.clone(),
            ProjectedJob {
                ordinal,
                snapshot,
                agent,
                events,
                activity: None,
                activity_at: None,
            },
        );
    }

    fn upsert_agent(&mut self, agent: AgentSnapshot) {
        self.agents.insert(agent.id.clone(), agent.clone());
        for job in self
            .jobs
            .values_mut()
            .filter(|job| job.snapshot.agent_id == agent.id)
        {
            job.agent = Some(agent.clone());
        }
    }
}
/// Temporary text compatibility for the legacy transcript Task card. The
/// structured, safe sections above remain the only stored context authority.
fn legacy_task_context(sections: &TaskDelegationSections) -> String {
    [
        ("Goal", sections.goal.as_str()),
        ("Target", sections.target.as_str()),
        ("Constraints", sections.constraints.as_str()),
        ("Contract", sections.contract.as_str()),
    ]
    .into_iter()
    .filter(|(_, body)| !body.is_empty())
    .map(|(heading, body)| format!("# {heading}\n{body}"))
    .collect::<Vec<_>>()
    .join("\n\n")
}

fn card_rows(job: &ProjectedJob, now: u64) -> JobCardRows {
    let snapshot = &job.snapshot;
    let agent_status = job.agent.as_ref().map(|agent| agent.status);
    let mut title_status = job_status_label(snapshot.status).to_owned();
    if agent_status == Some(AgentStatus::Parked) {
        title_status.push_str(" · parked");
    }
    let mut rows = vec![row(
        snapshot,
        JobCardRowRole::Title,
        format!(
            "{} ({}) · {title_status}",
            snapshot.agent_id, snapshot.agent
        ),
    )];
    let progress = JobCardProgress {
        stage: Some(job_status_label(snapshot.status).to_owned()),
        activity: job.activity.clone(),
        activity_at: job.activity_at,
        elapsed_ms: elapsed_ms(snapshot, now),
        events: job.events.clone(),
        history_ref: job
            .agent
            .as_ref()
            .and_then(|agent| agent.history_ref.clone()),
        artifact_ref: job
            .agent
            .as_ref()
            .and_then(|agent| agent.artifact_ref.clone()),
    };
    if let Some(description) = snapshot
        .description
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        rows.push(row(
            snapshot,
            JobCardRowRole::Description,
            bounded_redacted(description, MAX_DESCRIPTION_CHARS),
        ));
    }
    if matches!(snapshot.status, JobStatus::Queued | JobStatus::Running) {
        // Live one-liner for active jobs: `read tools.rs · 12s`, falling back
        // to the coarse `running · 12s` / `queued` when no detail arrived yet.
        rows.push(row(
            snapshot,
            JobCardRowRole::Progress,
            progress_row_text(&progress),
        ));
    } else {
        let timing = timing_text(snapshot);
        if !timing.is_empty() {
            rows.push(row(snapshot, JobCardRowRole::Timing, timing));
        }
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
        if let Some(error) = result
            .error
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            rows.push(row(
                snapshot,
                JobCardRowRole::Error,
                bounded_redacted(error, MAX_ERROR_CHARS),
            ));
        }
        for reference in [
            &result.artifact_ref,
            &result.history_ref,
            &result.artifact_uri,
        ] {
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
        progress: Some(progress),
    }
}

fn row(snapshot: &JobSnapshot, role: JobCardRowRole, text: String) -> JobCardRow {
    JobCardRow {
        job_id: Some(snapshot.id.clone()),
        role,
        text,
    }
}

pub(crate) fn job_status_label(status: JobStatus) -> &'static str {
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
    let runtime = snapshot
        .finished_at
        .zip(snapshot.started_at)
        .map(|(finished, started)| finished.saturating_sub(started));
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

pub(crate) fn format_duration(milliseconds: u64) -> String {
    if milliseconds < 1_000 {
        return format!("{milliseconds}ms");
    }
    let seconds = milliseconds / 1_000;
    if seconds < 60 {
        return format!("{seconds}s");
    }
    format!("{}m {}s", seconds / 60, seconds % 60)
}

/// Compact live one-liner: latest activity · elapsed, coarse stage as fallback.
pub(crate) fn progress_row_text(progress: &JobCardProgress) -> String {
    let mut parts = Vec::new();
    if let Some(activity) = progress
        .activity
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    {
        parts.push(activity.to_owned());
    }
    if parts.is_empty() {
        if let Some(stage) = progress.stage.as_deref() {
            parts.push(stage.to_owned());
        }
    }
    if let Some(elapsed) = progress.elapsed_ms {
        parts.push(format_duration(elapsed));
    }
    parts.join(" · ")
}

/// Wall-clock elapsed: `now - start` while queued/running (needs a live clock),
/// settled runtime (`finished - started`) otherwise.
fn elapsed_ms(snapshot: &JobSnapshot, now: u64) -> Option<u64> {
    match snapshot.status {
        JobStatus::Queued | JobStatus::Running => {
            if now == 0 {
                return None;
            }
            let start = snapshot.started_at.unwrap_or(snapshot.created_at);
            Some(now.saturating_sub(start))
        }
        JobStatus::Completed | JobStatus::Failed | JobStatus::Cancelled => snapshot
            .finished_at
            .zip(snapshot.started_at)
            .map(|(finished, started)| finished.saturating_sub(started)),
    }
}

/// Lifecycle event recorded when a job first appears, truthful for both live
/// upserts and full snapshot replacements.
fn initial_event(snapshot: &JobSnapshot) -> Option<(u64, String)> {
    let at = snapshot
        .started_at
        .or(snapshot.finished_at)
        .unwrap_or(snapshot.created_at);
    let text = match snapshot.status {
        JobStatus::Queued => "queued".to_owned(),
        JobStatus::Running => "started".to_owned(),
        JobStatus::Completed => "completed".to_owned(),
        JobStatus::Failed => failure_text(snapshot),
        JobStatus::Cancelled => "cancelled".to_owned(),
    };
    Some((at, text))
}

/// Lifecycle event for a status transition between two snapshots of one job.
fn transition_event(before: &JobSnapshot, after: &JobSnapshot) -> (u64, String) {
    let at = after
        .finished_at
        .or(after.started_at)
        .unwrap_or(after.created_at);
    let text = match (before.status, after.status) {
        (_, JobStatus::Queued) => "queued".to_owned(),
        (JobStatus::Queued, JobStatus::Running) => "started".to_owned(),
        (_, JobStatus::Running) => "running".to_owned(),
        (_, JobStatus::Completed) => "completed".to_owned(),
        (_, JobStatus::Failed) => failure_text(after),
        (_, JobStatus::Cancelled) => "cancelled".to_owned(),
    };
    (at, text)
}

fn failure_text(snapshot: &JobSnapshot) -> String {
    let detail = snapshot
        .result
        .as_ref()
        .and_then(|result| result.error.as_deref())
        .map(str::trim)
        .filter(|error| !error.is_empty());
    match detail {
        Some(error) => format!("failed: {}", bounded_redacted(error, MAX_ERROR_CHARS)),
        None => "failed".to_owned(),
    }
}

fn usage_text(usage: &pi_ai::Usage) -> String {
    let total = if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        [
            usage.input,
            usage.output,
            usage.cache_read,
            usage.cache_write,
        ]
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

fn redacted_text(text: &str) -> String {
    redact_value(&serde_json::Value::String(text.to_owned()))
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

fn bounded_redacted(text: &str, max_chars: usize) -> String {
    truncate_chars(&redacted_text(text), max_chars)
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
    use crate::tool_card_adapter::TASK_SECTION_CHAR_LIMIT;
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
            soft_budget_exhausted: false,
        }
    }

    fn agent(id: &str, status: AgentStatus) -> AgentSnapshot {
        AgentSnapshot {
            id: id.to_owned(),
            display_name: format!("task: work for {id}"),
            agent: "task".to_owned(),
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
            status: if error.is_some() {
                AgentStatus::Idle
            } else {
                AgentStatus::Idle
            },
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
            soft_budget_exhausted: false,
            structured_output: None,
        }
    }

    #[test]
    fn queued_running_completed_updates_one_card() {
        let mut adapter = JobCardPresentationAdapter::new();
        let mut snapshot = job("job-1", "Child", JobStatus::Queued);
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
            group_id: "group".to_owned(),
            job: snapshot.clone(),
        });
        snapshot.status = JobStatus::Running;
        snapshot.started_at = Some(1_100);
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
            group_id: "group".to_owned(),
            job: snapshot.clone(),
        });
        snapshot.status = JobStatus::Completed;
        snapshot.finished_at = Some(2_100);
        snapshot.result = Some(result("Child", "done", None));
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
            group_id: "group".to_owned(),
            job: snapshot,
        });
        let cards = adapter.cards_in_source_order();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].job_status, JobStatus::Completed);
        assert!(cards[0].rows.iter().any(|row| row.text == "done"));
        assert!(
            cards[0]
                .rows
                .iter()
                .any(|row| row.text.contains("runtime 1s"))
        );
    }

    #[test]
    fn failed_cancelled_and_parked_are_truthful() {
        let mut adapter = JobCardPresentationAdapter::new();
        let mut failed = job("failed", "Same", JobStatus::Failed);
        failed.result = Some(result("Same", "", Some("provider failed")));
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
            group_id: "group".to_owned(),
            job: failed,
        });
        adapter.apply_orchestration_event(&OrchestrationEvent::AgentUpdated {
            group_id: "group".to_owned(),
            agent: agent("Same", AgentStatus::Parked),
        });
        let cancelled = job("cancelled", "Other", JobStatus::Cancelled);
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
            group_id: "group".to_owned(),
            job: cancelled,
        });
        let cards = adapter.cards_in_source_order();
        assert_eq!(cards[0].job_status, JobStatus::Failed);
        assert_eq!(cards[0].agent_status, Some(AgentStatus::Parked));
        assert!(cards[0].rows[0].text.contains("failed · parked"));
        assert_eq!(cards[1].job_status, JobStatus::Cancelled);
    }

    #[test]
    fn concurrent_same_agent_jobs_keep_job_identity_order() {
        let mut adapter = JobCardPresentationAdapter::new();
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
            group_id: "group".to_owned(),
            job: job("first", "Shared", JobStatus::Running),
        });
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
            group_id: "group".to_owned(),
            job: job("second", "Shared", JobStatus::Queued),
        });
        let mut first = job("first", "Shared", JobStatus::Completed);
        first.result = Some(result("Shared", "first result", None));
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
            group_id: "group".to_owned(),
            job: first,
        });
        let cards = adapter.cards_in_source_order();
        assert_eq!(
            cards
                .iter()
                .map(|card| card.job_id.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert_eq!(cards[0].job_status, JobStatus::Completed);
        assert_eq!(cards[1].job_status, JobStatus::Queued);
    }

    #[test]
    fn result_is_redacted_bounded_and_terminal_update_not_duplicated() {
        let secret = "credential-redaction-fixture-value";
        let mut adapter = JobCardPresentationAdapter::new();
        let mut snapshot = job("terminal", "Child", JobStatus::Completed);
        snapshot.result = Some(result(
            "Child",
            &format!("token={secret} {}", "é".repeat(700)),
            None,
        ));
        let event = OrchestrationEvent::JobUpdated {
            group_id: "group".to_owned(),
            job: snapshot,
        };
        adapter.apply_orchestration_event(&event);
        adapter.apply_orchestration_event(&event);
        let cards = adapter.cards_in_source_order();
        assert_eq!(cards.len(), 1);
        let encoded = serde_json::to_string(&cards).expect("serialize cards");
        assert!(!encoded.contains(secret));
        assert!(encoded.contains("[REDACTED]"));
        let result_row = cards[0]
            .rows
            .iter()
            .find(|row| row.role == JobCardRowRole::Result)
            .expect("result row");
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
            adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
                group_id: "group".to_owned(),
                job: snapshot,
            });
        }
        adapter.apply_orchestration_event(&OrchestrationEvent::AgentUpdated {
            group_id: "group".to_owned(),
            agent: agent("C", AgentStatus::Parked),
        });
        assert_eq!(adapter.running_count(), 2);
        let aggregate = adapter.aggregate_row().expect("aggregate");
        assert!(aggregate.text.contains("1 running"));
        assert!(aggregate.text.contains("1 queued"));
        assert!(aggregate.text.contains("1 parked"));
        assert!(aggregate.text.contains("1 completed"));
    }
    #[test]
    fn requested_details_overlay_cards_without_changing_source_identity_or_order() {
        let mut adapter = JobCardPresentationAdapter::new();
        adapter.set_task_request(
            "# Goal\nShip the Task card\n\n# Contract\nKeep stable ids".to_owned(),
            [
                (
                    Some("Alpha".to_owned()),
                    Some("reviewer".to_owned()),
                    "Review adapter".to_owned(),
                ),
                (Some("Beta".to_owned()), None, "Render card".to_owned()),
            ],
        );
        // Runtime source order differs from request order. Explicit names win,
        // while the stable job/agent authority ids remain untouched.
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
            group_id: "group".to_owned(),
            job: job("job-b", "Beta", JobStatus::Queued),
        });
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
            group_id: "group".to_owned(),
            job: job("job-a", "Alpha", JobStatus::Running),
        });
        let cards = adapter.cards_in_source_order();
        assert_eq!(
            cards
                .iter()
                .map(|child| child.job_id.as_str())
                .collect::<Vec<_>>(),
            ["job-b", "job-a"]
        );
        assert_eq!(cards[0].agent_id, "Beta");
        assert_eq!(cards[0].summary.as_deref(), Some("Render card"));
        assert_eq!(cards[0].agent, "task");
        assert_eq!(cards[1].agent_id, "Alpha");
        assert_eq!(cards[1].summary.as_deref(), Some("Review adapter"));
        assert_eq!(cards[1].agent, "reviewer");

        // Legacy Task-card callers consume the exact same enriched cards and
        // the reconstructed safe context until the TUI cutover removes them.
        let card = adapter.task_card().expect("task card");
        assert_eq!(card.children, cards);
        assert!(card.context.contains("# Contract\nKeep stable ids"));
    }
    #[test]
    fn mixed_exact_and_fallback_requests_are_consumed_once() {
        let mut adapter = JobCardPresentationAdapter::new();
        adapter.set_task_request(
            "shared".to_owned(),
            [
                (
                    Some("Alpha".to_owned()),
                    Some("researcher".to_owned()),
                    "Alpha request".to_owned(),
                ),
                (
                    Some("Beta".to_owned()),
                    Some("reviewer".to_owned()),
                    "Beta request".to_owned(),
                ),
            ],
        );
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
            group_id: "group".to_owned(),
            job: job("job-beta", "Beta", JobStatus::Running),
        });
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
            group_id: "group".to_owned(),
            job: job("job-unknown", "Generated", JobStatus::Queued),
        });

        let cards = adapter.cards_in_source_order();
        assert_eq!(cards[0].summary.as_deref(), Some("Beta request"));
        assert_eq!(cards[0].agent, "reviewer");
        assert_eq!(cards[1].summary.as_deref(), Some("Alpha request"));
        assert_eq!(cards[1].agent, "researcher");
        assert_ne!(
            cards[0].summary, cards[1].summary,
            "one request must not overlay two cards"
        );
    }

    #[test]
    fn task_request_is_owned_by_one_runtime_group_without_losing_pending_rollover() {
        let mut adapter = JobCardPresentationAdapter::new();
        adapter.set_task_request(
            "# Goal\nOld detail".to_owned(),
            [(
                Some("Old".to_owned()),
                Some("reviewer".to_owned()),
                "Old request".to_owned(),
            )],
        );
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
            group_id: "old-group".to_owned(),
            job: job("old-job", "Old", JobStatus::Running),
        });
        assert_eq!(
            adapter.cards_in_source_order()[0].summary.as_deref(),
            Some("Old request")
        );

        // A group that arrives without a preceding Task request cannot inherit
        // the old group's request context or source-index overlays.
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
            group_id: "unrelated-group".to_owned(),
            job: job("unrelated-job", "Other", JobStatus::Queued),
        });
        let unrelated = adapter.cards_in_source_order();
        assert_eq!(unrelated.len(), 1);
        assert!(unrelated[0].summary.is_none());
        assert_eq!(unrelated[0].agent, "task");
        assert_eq!(
            adapter.task_context_sections(),
            &TaskDelegationSections::default()
        );

        // A fresh Task request is pending until its first orchestration event,
        // so the normal Task-start → new-group ordering preserves the request.
        adapter.set_task_request(
            "# Contract\nNew detail".to_owned(),
            [(
                Some("Fresh".to_owned()),
                Some("scout".to_owned()),
                "Fresh request".to_owned(),
            )],
        );
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
            group_id: "fresh-group".to_owned(),
            job: job("fresh-job", "Fresh", JobStatus::Running),
        });
        let fresh = adapter.cards_in_source_order();
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].summary.as_deref(), Some("Fresh request"));
        assert_eq!(fresh[0].agent, "scout");
        assert_eq!(adapter.task_context_sections().contract, "New detail");
    }

    #[test]
    fn requested_task_persona_and_shared_context_are_redacted_and_bounded() {
        let secret = "credential-redaction-fixture-value";
        let long_task = format!(
            "token={secret} {}",
            "任".repeat(TASK_TARGET_CHAR_LIMIT + 80)
        );
        let long_goal = format!(
            "password={secret} {}",
            "界".repeat(TASK_SECTION_CHAR_LIMIT + 80)
        );
        let long_target = "標".repeat(TASK_SECTION_CHAR_LIMIT + 80);
        let long_persona = format!(
            "token={secret} {}",
            "p".repeat(MAX_REQUESTED_AGENT_CHARS + 80)
        );
        let mut adapter = JobCardPresentationAdapter::new();
        adapter.set_task_request(
            format!(
                "# Goal\n{long_goal}\n\n# Target\n{long_target}\n\n# Constraints\nNo leaks\n\n# Contract\nOne authority"
            ),
            [(Some("Alpha".to_owned()), Some(long_persona), long_task)],
        );
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
            group_id: "group".to_owned(),
            job: job("job-a", "Alpha", JobStatus::Running),
        });

        let cards = adapter.cards_in_source_order();
        let card = &cards[0];
        let summary = card.summary.as_deref().expect("requested task summary");
        assert!(!summary.contains(secret));
        assert!(summary.contains("[REDACTED]"));
        assert_eq!(summary.chars().count(), TASK_TARGET_CHAR_LIMIT);
        assert!(summary.ends_with('…'));
        assert!(!card.agent.contains(secret));
        assert!(card.agent.contains("[REDACTED]"));
        assert_eq!(card.agent.chars().count(), MAX_REQUESTED_AGENT_CHARS);
        assert!(card.agent.ends_with('…'));

        let sections = adapter.task_context_sections();
        assert!(!sections.goal.contains(secret));
        assert!(sections.goal.contains("[REDACTED]"));
        assert_eq!(sections.goal.chars().count(), TASK_SECTION_CHAR_LIMIT);
        assert!(sections.goal.ends_with('…'));
        assert_eq!(sections.target.chars().count(), TASK_SECTION_CHAR_LIMIT);
        assert!(sections.target.ends_with('…'));
        assert_eq!(sections.constraints, "No leaks");
        assert_eq!(sections.contract, "One authority");

        let legacy = adapter.task_card().expect("legacy task card");
        assert!(!legacy.context.contains(secret));
        assert!(legacy.context.contains("[REDACTED]"));
    }

    #[test]
    fn live_progress_line_shows_activity_elapsed_and_coarse_fallback() {
        let mut adapter = JobCardPresentationAdapter::new();
        let mut snapshot = job("job-1", "Child", JobStatus::Running);
        snapshot.started_at = Some(500);
        adapter.set_now(12_500);
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
            group_id: "group".to_owned(),
            job: snapshot,
        });
        // No detail yet: coarse stage + elapsed fallback.
        let cards = adapter.cards_in_source_order();
        let progress_row = cards[0]
            .rows
            .iter()
            .find(|row| row.role == JobCardRowRole::Progress)
            .expect("progress row");
        assert_eq!(progress_row.text, "running · 12s");
        assert_eq!(
            cards[0].progress.as_ref().expect("progress").elapsed_ms,
            Some(12_000)
        );
        // A child → Main IRC progress message becomes the live activity.
        adapter.apply_orchestration_event(&OrchestrationEvent::MessageDelivered {
            group_id: "group".to_owned(),
            waiter_claimed: false,
            message: MailboxMessage {
                id: "m-1".to_owned(),
                from: "Child".to_owned(),
                to: "Main".to_owned(),
                body: "read tools.rs".to_owned(),
                timestamp: 12_100,
                reply_to: None,
            },
        });
        let cards = adapter.cards_in_source_order();
        let progress_row = cards[0]
            .rows
            .iter()
            .find(|row| row.role == JobCardRowRole::Progress)
            .expect("progress row");
        assert_eq!(progress_row.text, "read tools.rs · 12s");
        let progress = cards[0].progress.as_ref().expect("progress");
        assert_eq!(progress.activity.as_deref(), Some("read tools.rs"));
        assert_eq!(progress.activity_at, Some(12_100));
        // started + message, oldest first.
        assert_eq!(progress.events.len(), 2);
        assert_eq!(progress.events[0].kind, JobEventKind::Job);
        assert_eq!(progress.events[0].text, "started");
        assert_eq!(progress.events[1].kind, JobEventKind::Message);
        assert_eq!(progress.events[1].text, "read tools.rs");
        assert!(progress.events[1].at >= progress.events[0].at);
    }

    #[test]
    fn elapsed_renders_from_clock_and_settled_jobs_keep_timing_and_usage() {
        let mut adapter = JobCardPresentationAdapter::new();
        let mut snapshot = job("job-1", "Child", JobStatus::Running);
        snapshot.started_at = Some(1_000);
        adapter.set_now(1_500);
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
            group_id: "group".to_owned(),
            job: snapshot,
        });
        assert_eq!(
            adapter.cards_in_source_order()[0]
                .progress
                .as_ref()
                .expect("progress")
                .elapsed_ms,
            Some(500)
        );
        adapter.set_now(13_000);
        let cards = adapter.cards_in_source_order();
        let progress_row = cards[0]
            .rows
            .iter()
            .find(|row| row.role == JobCardRowRole::Progress)
            .expect("progress row");
        assert_eq!(progress_row.text, "running · 12s");
        // Settled: no Progress row; Timing + Usage rows and settled runtime return.
        let mut done = job("job-1", "Child", JobStatus::Completed);
        done.started_at = Some(1_000);
        done.finished_at = Some(4_000);
        done.result = Some(result("Child", "done", None));
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
            group_id: "group".to_owned(),
            job: done,
        });
        let cards = adapter.cards_in_source_order();
        assert!(
            cards[0]
                .rows
                .iter()
                .all(|row| row.role != JobCardRowRole::Progress)
        );
        assert!(
            cards[0]
                .rows
                .iter()
                .any(|row| row.role == JobCardRowRole::Timing && row.text.contains("runtime 3s"))
        );
        assert!(
            cards[0]
                .rows
                .iter()
                .any(|row| row.role == JobCardRowRole::Usage && row.text.contains("15 tokens"))
        );
        assert_eq!(
            cards[0].progress.as_ref().expect("progress").elapsed_ms,
            Some(3_000)
        );
    }

    #[test]
    fn event_log_records_lifecycle_transitions_in_order() {
        let mut adapter = JobCardPresentationAdapter::new();
        let mut snapshot = job("job-1", "Child", JobStatus::Queued);
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
            group_id: "group".to_owned(),
            job: snapshot.clone(),
        });
        snapshot.status = JobStatus::Running;
        snapshot.started_at = Some(1_100);
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
            group_id: "group".to_owned(),
            job: snapshot.clone(),
        });
        snapshot.status = JobStatus::Failed;
        snapshot.finished_at = Some(2_000);
        snapshot.result = Some(result("Child", "", Some("provider failed")));
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
            group_id: "group".to_owned(),
            job: snapshot,
        });
        let cards = adapter.cards_in_source_order();
        let progress = cards[0].progress.as_ref().expect("progress");
        let texts = progress
            .events
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(texts, ["queued", "started", "failed: provider failed"]);
        assert!(
            progress
                .events
                .iter()
                .all(|entry| entry.kind == JobEventKind::Job)
        );
    }

    #[test]
    fn progress_activity_is_redacted_and_bounded() {
        let secret = "credential-redaction-fixture-value";
        let mut adapter = JobCardPresentationAdapter::new();
        let mut snapshot = job("job-1", "Child", JobStatus::Running);
        snapshot.started_at = Some(500);
        adapter.set_now(12_500);
        adapter.apply_orchestration_event(&OrchestrationEvent::JobUpdated {
            group_id: "group".to_owned(),
            job: snapshot,
        });
        adapter.apply_orchestration_event(&OrchestrationEvent::MessageDelivered {
            group_id: "group".to_owned(),
            waiter_claimed: false,
            message: MailboxMessage {
                id: "m-secret".to_owned(),
                from: "Child".to_owned(),
                to: "Main".to_owned(),
                body: format!("token={secret} {}", "é".repeat(200)),
                timestamp: 12_100,
                reply_to: None,
            },
        });
        let cards = adapter.cards_in_source_order();
        let encoded = serde_json::to_string(&cards).expect("serialize cards");
        assert!(!encoded.contains(secret), "activity log must be redacted");
        assert!(encoded.contains("[REDACTED]"));
        let activity = cards[0]
            .progress
            .as_ref()
            .expect("progress")
            .activity
            .clone()
            .expect("activity");
        assert!(activity.chars().count() <= MAX_ACTIVITY_CHARS);
    }
}
