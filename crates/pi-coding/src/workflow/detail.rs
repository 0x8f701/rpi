use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use super::{WorkflowId, WorkflowSnapshot, WorkflowStatus, WorkflowSupervisorActivity, WorkflowSupervisorProjection};
use crate::{AgentSnapshot, AgentStatus, JobStatus, MailboxMessage, TodoState, WorkflowJobSnapshot};

const RECENT_IRC_LIMIT: usize = 50;
/// Bound on one agent's live activity feed (delegated task lifecycle + IRC).
const AGENT_ACTIVITY_CAP: usize = 12;

/// Redacted, exact-generation workflow state for human-facing live detail views.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRuntimeDetail {
    pub workflow_id: WorkflowId,
    pub generation: u64,
    pub name: String,
    pub objective: String,
    pub status: WorkflowStatus,
    pub todo: TodoState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor: Option<WorkflowSupervisorDetail>,
    pub subagents: Vec<WorkflowSubagentDetail>,
    pub jobs: Vec<WorkflowRuntimeJobDetail>,
    pub irc: Vec<WorkflowIrcMessage>,
    /// Bounded live activity feed for the supervisor's own turn (planning
    /// progress projection). Live-only, never persisted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supervisor_activity: Vec<WorkflowSupervisorActivity>,
    /// Epoch millis when the current Planning phase started (None otherwise).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planning_started_at_ms: Option<u64>,
}

impl fmt::Debug for WorkflowRuntimeDetail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("WorkflowRuntimeDetail")
            .field("workflow_id", &self.workflow_id)
            .field("generation", &self.generation)
            .field("status", &self.status)
            .field("phase_count", &self.todo.phases.len())
            .field("has_supervisor", &self.supervisor.is_some())
            .field("subagent_count", &self.subagents.len())
            .field("job_count", &self.jobs.len())
            .field("irc_count", &self.irc.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowSupervisorDetail {
    pub display_name: String,
    pub status: AgentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_summary: Option<String>,
    /// Todo-DAG task id backing the current task summary — the agent's latest
    /// delegated job's todo task — so live projections can dedupe the actor
    /// row against the job-derived Active tasks list by task identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todo_task_id: Option<String>,
    /// Bounded live activity feed (newest-last): delegated task lifecycle
    /// entries plus IRC messages the agent sent or received. Live-only; never
    /// part of the durable workflow record.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activity: Vec<WorkflowAgentActivity>,
}

impl fmt::Debug for WorkflowSupervisorDetail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkflowSupervisorDetail")
            .field("status", &self.status)
            .field("has_task_summary", &self.task_summary.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowSubagentDetail {
    pub display_name: String,
    pub status: AgentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_summary: Option<String>,
    /// Todo-DAG task id backing the current task summary — the agent's latest
    /// delegated job's todo task — so live projections can dedupe the actor
    /// row against the job-derived Active tasks list by task identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todo_task_id: Option<String>,
    /// Bounded live activity feed (newest-last): delegated task lifecycle
    /// entries plus IRC messages the agent sent or received. Live-only; never
    /// part of the durable workflow record.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activity: Vec<WorkflowAgentActivity>,
}

impl fmt::Debug for WorkflowSubagentDetail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("WorkflowSubagentDetail")
            .field("status", &self.status)
            .field("has_task_summary", &self.task_summary.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRuntimeJobDetail {
    pub display_name: String,
    pub status: JobStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todo_task_id: Option<String>,
    pub created_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
}

impl fmt::Debug for WorkflowRuntimeJobDetail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("WorkflowRuntimeJobDetail")
            .field("status", &self.status)
            .field("has_task_summary", &self.task_summary.is_some())
            .field("has_todo_task_id", &self.todo_task_id.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowIrcMessage {
    pub from: String,
    pub to: String,
    pub body: String,
    pub timestamp_ms: u64,
}

impl fmt::Debug for WorkflowIrcMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("WorkflowIrcMessage").field("timestamp_ms", &self.timestamp_ms).finish_non_exhaustive()
    }
}

/// One bounded entry of a workflow agent's live activity feed: a delegated
/// task lifecycle transition or a delivered IRC message the agent sent or
/// received. Projected per agent so the workflow page can expand an agent row
/// to a live feed instead of a static name/status pair.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowAgentActivity {
    /// Epoch millis when the activity was observed (job transitions prefer
    /// the latest known timestamp; IRC uses the delivery timestamp).
    pub at_ms: u64,
    pub kind: WorkflowAgentActivityKind,
    /// Bounded, credential-redacted display text.
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

impl WorkflowRuntimeDetail {
    pub(crate) fn snapshot_fallback(snapshot: &WorkflowSnapshot) -> Self {
        Self {
            workflow_id: snapshot.workflow_id.clone(), generation: snapshot.generation,
            name: safe_text(&snapshot.name, 160), objective: safe_text(&snapshot.objective, 600),
            status: snapshot.status, todo: safe_todo(&snapshot.todo), supervisor: None,
            subagents: Vec::new(), jobs: Vec::new(), irc: Vec::new(),
            supervisor_activity: Vec::new(), planning_started_at_ms: None,
        }
    }

    pub(crate) fn from_live(snapshot: &WorkflowSnapshot, projection: WorkflowSupervisorProjection, agents: Vec<AgentSnapshot>, mut jobs: Vec<WorkflowJobSnapshot>, irc: Vec<MailboxMessage>) -> Self {
        let agent_names = agents.iter().map(|agent| {
            let display_name = if agent.display_name.trim().is_empty() { "Agent" } else { &agent.display_name };
            (agent.id.as_str(), safe_text(display_name, 160))
        }).collect::<HashMap<_, _>>();
        jobs.sort_by(|left, right| left.job.created_at.cmp(&right.job.created_at).then_with(|| left.job.id.cmp(&right.job.id)));
        let mut latest_summary = HashMap::<&str, &str>::new();
        let mut latest_todo_task_id = HashMap::<&str, &str>::new();
        for job in &jobs {
            if let Some(summary) = job.job.description.as_deref() { latest_summary.insert(job.job.agent_id.as_str(), summary); }
            if let Some(task_id) = job.todo_task_id.as_deref().or(job.job.todo_task_id.as_deref()) { latest_todo_task_id.insert(job.job.agent_id.as_str(), task_id); }
        }
        // Per-agent bounded activity feed: delegated task lifecycle entries
        // plus IRC messages the agent sent or received, merged and newest-last
        // so each agent row on the workflow page can expand to a live feed.
        let activity_by_agent = agents
            .iter()
            .map(|agent| (agent.id.as_str(), agent_activity(agent.id.as_str(), &jobs, &irc, &agent_names, &projection.supervisor_agent_id)))
            .collect::<HashMap<_, _>>();
        // The supervisor's own orchestration entry stays Idle while its
        // planning turn runs in the main child session (only delegated
        // workers transition through Queued/Running). During Planning the
        // supervisor is by definition actively working, so the live detail
        // must read as Running with the objective it is planning rather than
        // a confusing idle/empty projection.
        let supervisor = agents.iter().find(|agent| agent.id == projection.supervisor_agent_id).map(|agent| {
            let status = match projection.status {
                WorkflowStatus::Planning if agent.status == AgentStatus::Idle => AgentStatus::Running,
                WorkflowStatus::Queued if agent.status == AgentStatus::Idle => AgentStatus::Queued,
                _ => agent.status,
            };
            let task_summary = latest_summary
                .get(agent.id.as_str())
                .map(|summary| safe_text(summary, 240))
                .or_else(|| {
                    (projection.status == WorkflowStatus::Planning)
                        .then(|| format!("Planning: {}", safe_text(&snapshot.objective, 240)))
                });
            WorkflowSupervisorDetail {
                display_name: agent_names.get(agent.id.as_str()).cloned().unwrap_or_else(|| "Supervisor".to_owned()),
                status,
                task_summary,
                todo_task_id: latest_todo_task_id.get(agent.id.as_str()).map(|id| (*id).to_owned()),
                activity: activity_by_agent.get(agent.id.as_str()).cloned().unwrap_or_default(),
            }
        });
        let subagents = agents.iter().filter(|agent| agent.id != projection.supervisor_agent_id).map(|agent| WorkflowSubagentDetail {
            display_name: agent_names.get(agent.id.as_str()).cloned().unwrap_or_else(|| "Agent".to_owned()),
            status: agent.status,
            task_summary: latest_summary.get(agent.id.as_str()).map(|summary| safe_text(summary, 240)),
            todo_task_id: latest_todo_task_id.get(agent.id.as_str()).map(|id| (*id).to_owned()),
            activity: activity_by_agent.get(agent.id.as_str()).cloned().unwrap_or_default(),
        }).collect();
        let jobs = jobs.into_iter().map(|workflow_job| {
            let job = workflow_job.job;
            WorkflowRuntimeJobDetail {
                display_name: agent_names.get(job.agent_id.as_str()).cloned().unwrap_or_else(|| safe_text(&job.agent, 160)),
                status: job.status,
                task_summary: job.description.as_deref().map(|summary| safe_text(summary, 240)),
                todo_task_id: workflow_job.todo_task_id.or(job.todo_task_id),
                created_at_ms: job.created_at, started_at_ms: job.started_at, finished_at_ms: job.finished_at,
            }
        }).collect();
        let irc = recent_irc(irc, &agent_names, &projection.supervisor_agent_id);
        // The supervisor projection is the live view, but the durable snapshot
        // is authoritative for terminal lifecycle outcomes: an integrate that
        // lands after the supervisor already projected Completed (auto or
        // manual) leaves the durable record Conflicted while the projection
        // still reads Completed. Never let the live view mask a terminal
        // status — the panel must show the conflict, not "completed".
        let status = if snapshot.status.is_terminal() && snapshot.status != projection.status {
            snapshot.status
        } else {
            projection.status
        };
        Self {
            workflow_id: snapshot.workflow_id.clone(), generation: snapshot.generation,
            name: safe_text(&snapshot.name, 160), objective: safe_text(&snapshot.objective, 600),
            status, todo: safe_todo(&projection.todo), supervisor, subagents, jobs, irc,
            supervisor_activity: projection.activity,
            planning_started_at_ms: projection.planning_started_at_ms,
        }
    }
}

fn recent_irc(mut messages: Vec<MailboxMessage>, agent_names: &HashMap<&str, String>, supervisor_agent_id: &str) -> Vec<WorkflowIrcMessage> {
    messages.sort_by(|left, right| left.timestamp.cmp(&right.timestamp).then_with(|| left.id.cmp(&right.id)));
    let mut seen = HashSet::new();
    let mut messages = messages.into_iter().filter(|message| seen.insert(message.id.clone())).collect::<Vec<_>>();
    if messages.len() > RECENT_IRC_LIMIT { messages.drain(..messages.len() - RECENT_IRC_LIMIT); }
    messages.into_iter().map(|message| WorkflowIrcMessage {
        from: safe_actor_name(&message.from, agent_names, supervisor_agent_id),
        to: safe_actor_name(&message.to, agent_names, supervisor_agent_id),
        body: safe_text(&message.body, 600), timestamp_ms: message.timestamp,
    }).collect()
}

fn safe_actor_name(agent_id: &str, agent_names: &HashMap<&str, String>, supervisor_agent_id: &str) -> String {
    agent_names.get(agent_id).cloned().unwrap_or_else(|| if agent_id == supervisor_agent_id { "Supervisor".to_owned() } else { "Agent".to_owned() })
}

/// Build one agent's bounded activity feed (newest-last): a Task entry per
/// delegated job the agent owns (text = the job description, or a coarse
/// status label when the job carries none) and an Irc entry per delivered
/// message the agent sent or received, naming the other party. Entries are
/// merged in timestamp order and capped so the expanded agent row stays
/// bounded even for long delegations.
fn agent_activity(
    agent_id: &str,
    jobs: &[WorkflowJobSnapshot],
    irc: &[MailboxMessage],
    agent_names: &HashMap<&str, String>,
    supervisor_agent_id: &str,
) -> Vec<WorkflowAgentActivity> {
    let mut entries = Vec::new();
    for workflow_job in jobs {
        if workflow_job.job.agent_id != agent_id {
            continue;
        }
        let text = workflow_job
            .job
            .description
            .as_deref()
            .map(|summary| safe_text(summary, 240))
            .unwrap_or_else(|| format!("task {:?}", workflow_job.job.status).to_lowercase());
        let at_ms = workflow_job
            .job
            .finished_at
            .or(workflow_job.job.started_at)
            .unwrap_or(workflow_job.job.created_at);
        entries.push(WorkflowAgentActivity { at_ms, kind: WorkflowAgentActivityKind::Task, text });
    }
    // The runtime union may deliver the same message via several mailboxes
    // (supervisor projection + group inboxes); dedupe by id like `recent_irc`.
    let mut seen_ids = HashSet::new();
    for message in irc {
        if !seen_ids.insert(message.id.as_str()) {
            continue;
        }
        let (other, direction) = if message.from == agent_id {
            (safe_actor_name(&message.to, agent_names, supervisor_agent_id), true)
        } else if message.to == agent_id {
            (safe_actor_name(&message.from, agent_names, supervisor_agent_id), false)
        } else {
            continue;
        };
        let text = if direction {
            format!("→ {other}: {}", safe_text(&message.body, 240))
        } else {
            format!("{other}: {}", safe_text(&message.body, 240))
        };
        entries.push(WorkflowAgentActivity {
            at_ms: message.timestamp,
            kind: WorkflowAgentActivityKind::Irc,
            text,
        });
    }
    entries.sort_by(|left, right| left.at_ms.cmp(&right.at_ms));
    if entries.len() > AGENT_ACTIVITY_CAP {
        entries.drain(..entries.len() - AGENT_ACTIVITY_CAP);
    }
    entries
}

fn safe_todo(todo: &TodoState) -> TodoState {
    let mut safe = todo.clone();
    for phase in &mut safe.phases {
        phase.name = safe_text(&phase.name, 160);
        for task in &mut phase.tasks {
            task.content = safe_text(&task.content, 600);
            for blocked in &mut task.blocked_by { blocked.content = safe_text(&blocked.content, 600); }
        }
    }
    safe
}

fn safe_text(text: &str, max_chars: usize) -> String {
    let value = crate::redact_value(&serde_json::Value::String(text.to_owned()));
    let redacted = value.as_str().unwrap_or_default();
    let without_paths = absolute_path_pattern().replace_all(redacted, "${prefix}[path]");
    if without_paths.chars().count() <= max_chars { return without_paths.into_owned(); }
    let mut output = without_paths.chars().take(max_chars.saturating_sub(1)).collect::<String>();
    output.push('…'); output
}

fn absolute_path_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r#"(?P<prefix>^|[\s(\"'`])(?:/[^\s)\"'`,;]+|[A-Za-z]:\\[^\s)\"'`,;]+)"#).expect("workflow detail path redaction pattern is valid"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{JobSnapshot, TodoStorage, WorkflowIntegration};

    fn snapshot(status: WorkflowStatus) -> WorkflowSnapshot {
        WorkflowSnapshot {
            workflow_id: WorkflowId::new("workflow-safe"),
            name: "Safe workflow".to_owned(), objective: "work /private/root token=hidden".to_owned(), status,
            created_at_ms: 1, updated_at_ms: 2, generation: 7,
            todo: TodoState { phases: Vec::new(), storage: TodoStorage::Memory },
            worktree_label: Some("workflow-safe".to_owned()), branch: Some("private-branch".to_owned()),
            supervisor_agent_id: Some("private-supervisor-id".to_owned()), supervisor_job_id: None,
            failure: None, integration: WorkflowIntegration::None,
        }
    }

    fn agent(id: &str, name: &str, status: AgentStatus) -> AgentSnapshot {
        AgentSnapshot { id: id.to_owned(), display_name: name.to_owned(), agent: name.to_owned(), parent_id: None, status, created_at: 1, last_activity: 1, unread: 0, artifact_ref: None, history_ref: None }
    }

    fn job(status: JobStatus) -> WorkflowJobSnapshot {
        WorkflowJobSnapshot {
            workflow_id: "workflow-safe".to_owned(), generation: 7, todo_task_id: Some("todo-safe".to_owned()),
            job: JobSnapshot {
                id: "private-job-id".to_owned(), agent_id: "worker-private-id".to_owned(), agent: "task".to_owned(), parent_id: "supervisor-private-id".to_owned(),
                description: Some("edit /private/file api_key=hidden".to_owned()), todo_task_id: Some("todo-safe".to_owned()),
                workflow_id: Some("workflow-safe".to_owned()), workflow_generation: Some(7), status, created_at: 3,
                started_at: Some(4), finished_at: status.is_settled().then_some(5), result: None,
                soft_budget_exhausted: false,
            },
        }
    }

    #[test]
    fn live_detail_projects_jobs_todo_ownership_and_deduplicated_irc() {
        let projection = WorkflowSupervisorProjection {
            workflow_id: "workflow-safe".to_owned(), generation: 7, status: WorkflowStatus::Running,
            supervisor_agent_id: "supervisor-private-id".to_owned(), todo: snapshot(WorkflowStatus::Running).todo,
            jobs: Vec::new(), irc: Vec::new(), failure: None,
            activity: Vec::new(), planning_started_at_ms: None,
        };
        let message = MailboxMessage { id: "message-private-id".to_owned(), from: "worker-private-id".to_owned(), to: "supervisor-private-id".to_owned(), body: "done /private/file token=hidden".to_owned(), timestamp: 9, reply_to: None };
        let detail = WorkflowRuntimeDetail::from_live(
            &snapshot(WorkflowStatus::Running), projection,
            vec![agent("supervisor-private-id", "Supervisor", AgentStatus::Running), agent("worker-private-id", "Worker", AgentStatus::Running)],
            vec![job(JobStatus::Running), job(JobStatus::Completed)], vec![message.clone(), message],
        );

        assert_eq!(detail.jobs.len(), 2);
        assert_eq!(detail.jobs[0].todo_task_id.as_deref(), Some("todo-safe"));
        // The subagent's current task summary is its latest job's description,
        // so the detail must pair it with that job's todo-task id — the
        // identity key the panel uses to dedupe Active tasks rows.
        assert_eq!(detail.subagents[0].task_summary.as_deref(), Some("edit [path] api_key=[REDACTED]"));
        assert_eq!(detail.subagents[0].todo_task_id.as_deref(), Some("todo-safe"));
        assert!(detail.supervisor.as_ref().is_none_or(|supervisor| supervisor.todo_task_id.is_none()), "supervisor owns no job; its todo_task_id must stay None");
        assert_eq!(detail.irc.len(), 1);
        assert_eq!(detail.irc[0].from, "Worker");
        assert_eq!(detail.irc[0].body, "done [path] token=[REDACTED]");
        let encoded = serde_json::to_string(&detail).expect("serialize live detail");
        for private in ["private-job-id", "message-private-id", "worker-private-id", "supervisor-private-id", "/private", "hidden"] { assert!(!encoded.contains(private), "leaked {private}"); }
    }

    #[test]
    fn live_detail_projects_per_agent_activity_feed() {
        // Each agent's expanded row needs a bounded live feed: delegated task
        // lifecycle entries plus IRC the agent sent or received, merged in
        // timestamp order, newest-last, names resolved, secrets redacted.
        let projection = WorkflowSupervisorProjection {
            workflow_id: "workflow-safe".to_owned(), generation: 7, status: WorkflowStatus::Running,
            supervisor_agent_id: "supervisor-private-id".to_owned(), todo: snapshot(WorkflowStatus::Running).todo,
            jobs: Vec::new(), irc: Vec::new(), failure: None,
            activity: Vec::new(), planning_started_at_ms: None,
        };
        let mut jobs = vec![job(JobStatus::Completed)];
        jobs[0].job.description = Some("finish the report".to_owned());
        jobs[0].job.started_at = Some(10);
        jobs[0].job.finished_at = Some(11);
        let irc = vec![
            MailboxMessage { id: "m-1".to_owned(), from: "worker-private-id".to_owned(), to: "supervisor-private-id".to_owned(), body: "done, see /private/file".to_owned(), timestamp: 12, reply_to: None },
            MailboxMessage { id: "m-2".to_owned(), from: "worker-private-id".to_owned(), to: "peer-private-id".to_owned(), body: "handoff to peer".to_owned(), timestamp: 13, reply_to: None },
        ];
        let detail = WorkflowRuntimeDetail::from_live(
            &snapshot(WorkflowStatus::Running), projection,
            vec![
                agent("supervisor-private-id", "Supervisor", AgentStatus::Running),
                agent("worker-private-id", "Worker", AgentStatus::Running),
                agent("peer-private-id", "Peer", AgentStatus::Parked),
            ],
            jobs, irc,
        );

        let worker = &detail.subagents[0];
        assert_eq!(worker.activity.len(), 3, "task + outgoing + incoming IRC entries");
        // Newest-last: oldest entry first (task finish at 11), newest last
        // (the worker → peer IRC at 13).
        assert_eq!(worker.activity[0].kind, WorkflowAgentActivityKind::Task);
        assert_eq!(worker.activity[0].text, "finish the report");
        assert_eq!(worker.activity[1].kind, WorkflowAgentActivityKind::Irc);
        assert!(worker.activity[1].text.contains("Supervisor: done, see [path]"), "incoming IRC names the sender: {}", worker.activity[1].text);
        assert_eq!(worker.activity[2].at_ms, 13);
        assert_eq!(worker.activity[2].text, "→ Peer: handoff to peer");
        // The peer's feed shows the incoming message from the worker.
        let peer = detail.subagents.iter().find(|subagent| subagent.display_name == "Peer").expect("peer");
        assert_eq!(peer.activity.len(), 1);
        assert_eq!(peer.activity[0].text, "Worker: handoff to peer");
        // The supervisor's feed shows only its own IRC, not the worker/peer hop.
        let supervisor = detail.supervisor.as_ref().expect("supervisor");
        assert_eq!(supervisor.activity.len(), 1);
        assert_eq!(supervisor.activity[0].text, "Worker: done, see [path]");
        // Redaction: no raw ids or paths leak through the feed.
        let encoded = serde_json::to_string(&detail).expect("serialize live detail");
        for private in ["private-job-id", "worker-private-id", "peer-private-id", "/private"] {
            assert!(!encoded.contains(private), "leaked {private}");
        }
    }

    #[test]
    fn live_detail_shows_subagent_to_subagent_irc() {
        // IRC between two subagents (neither is the supervisor) must surface
        // in the workflow detail's recent IRC, not just supervisor-bound
        // messages, so the page can show subagent ⇄ subagent communication.
        let projection = WorkflowSupervisorProjection {
            workflow_id: "workflow-safe".to_owned(), generation: 7, status: WorkflowStatus::Running,
            supervisor_agent_id: "supervisor-private-id".to_owned(), todo: snapshot(WorkflowStatus::Running).todo,
            jobs: Vec::new(), irc: Vec::new(), failure: None,
            activity: Vec::new(), planning_started_at_ms: None,
        };
        let irc = vec![
            MailboxMessage { id: "peer-a".to_owned(), from: "worker-a-private".to_owned(), to: "worker-b-private".to_owned(), body: "results ready".to_owned(), timestamp: 20, reply_to: None },
            MailboxMessage { id: "peer-b".to_owned(), from: "worker-b-private".to_owned(), to: "worker-a-private".to_owned(), body: "acknowledged".to_owned(), timestamp: 21, reply_to: None },
            MailboxMessage { id: "peer-b".to_owned(), from: "worker-b-private".to_owned(), to: "worker-a-private".to_owned(), body: "acknowledged".to_owned(), timestamp: 21, reply_to: None },
        ];
        let detail = WorkflowRuntimeDetail::from_live(
            &snapshot(WorkflowStatus::Running), projection,
            vec![
                agent("supervisor-private-id", "Supervisor", AgentStatus::Running),
                agent("worker-a-private", "WorkerA", AgentStatus::Parked),
                agent("worker-b-private", "WorkerB", AgentStatus::Parked),
            ],
            Vec::new(), irc,
        );
        assert_eq!(detail.irc.len(), 2, "duplicate message ids must collapse");
        assert_eq!(detail.irc[0].from, "WorkerA");
        assert_eq!(detail.irc[0].to, "WorkerB");
        assert_eq!(detail.irc[0].body, "results ready");
        assert_eq!(detail.irc[1].from, "WorkerB");
        assert_eq!(detail.irc[1].to, "WorkerA");
        // Both directions also feed each agent's activity row.
        let worker_a = detail.subagents.iter().find(|subagent| subagent.display_name == "WorkerA").expect("worker a");
        assert_eq!(worker_a.activity.len(), 2);
        let worker_b = detail.subagents.iter().find(|subagent| subagent.display_name == "WorkerB").expect("worker b");
        assert_eq!(worker_b.activity.len(), 2);
    }

    #[test]
    fn planning_detail_reads_supervisor_as_active_with_planning_task() {
        let projection = WorkflowSupervisorProjection {
            workflow_id: "workflow-safe".to_owned(), generation: 7, status: WorkflowStatus::Planning,
            supervisor_agent_id: "supervisor-private-id".to_owned(), todo: TodoState { phases: Vec::new(), storage: TodoStorage::Memory },
            jobs: Vec::new(), irc: Vec::new(), failure: None,
            activity: Vec::new(), planning_started_at_ms: None,
        };
        // The supervisor's orchestration entry is Idle while its planning turn
        // runs (only delegated workers transition to Running); the live detail
        // must still read the supervisor as actively planning.
        let detail = WorkflowRuntimeDetail::from_live(
            &snapshot(WorkflowStatus::Planning), projection,
            vec![agent("supervisor-private-id", "Supervisor", AgentStatus::Idle)],
            Vec::new(), Vec::new(),
        );
        assert_eq!(detail.status, WorkflowStatus::Planning);
        let supervisor = detail.supervisor.expect("planning supervisor");
        assert_eq!(supervisor.status, AgentStatus::Running);
        assert_eq!(supervisor.display_name, "Supervisor");
        let task = supervisor.task_summary.expect("planning task summary");
        assert!(task.starts_with("Planning: "), "planning task must name the objective: {task}");
        assert!(!task.contains("private"), "planning task must not leak raw objective: {task}");
        assert!(supervisor.todo_task_id.is_none(), "the planning summary is not job-backed; it must carry no todo task id");
        assert!(detail.jobs.is_empty() && detail.todo.phases.is_empty());
    }

    #[test]
    fn terminal_fallback_and_debug_are_redacted() {
        let detail = WorkflowRuntimeDetail::snapshot_fallback(&snapshot(WorkflowStatus::Completed));
        assert!(detail.supervisor.is_none() && detail.jobs.is_empty() && detail.irc.is_empty());
        let encoded = serde_json::to_string(&detail).expect("serialize fallback");
        let debug = format!("{detail:?}");
        for private in ["/private", "hidden", "private-branch", "private-supervisor-id"] { assert!(!encoded.contains(private)); assert!(!debug.contains(private)); }
    }

    #[test]
    fn terminal_durable_status_wins_over_stale_live_projection() {
        // An integrate (auto or manual) that lands after the supervisor
        // already projected Completed leaves the durable record Conflicted
        // while the live projection still reads Completed. The live detail
        // must surface the durable terminal status so the panel shows the
        // conflict instead of a stale "completed".
        let projection = WorkflowSupervisorProjection {
            workflow_id: "workflow-safe".to_owned(), generation: 7, status: WorkflowStatus::Completed,
            supervisor_agent_id: "supervisor-private-id".to_owned(), todo: TodoState { phases: Vec::new(), storage: TodoStorage::Memory },
            jobs: Vec::new(), irc: Vec::new(), failure: None,
            activity: Vec::new(), planning_started_at_ms: None,
        };
        let mut durable = snapshot(WorkflowStatus::Completed);
        durable.integration = WorkflowIntegration::Conflicted { conflicts: vec!["private/path".to_owned()] };
        durable.status = WorkflowStatus::Conflicted;
        let detail = WorkflowRuntimeDetail::from_live(
            &durable, projection,
            vec![agent("supervisor-private-id", "Supervisor", AgentStatus::Idle)],
            Vec::new(), Vec::new(),
        );
        assert_eq!(detail.status, WorkflowStatus::Conflicted);
    }
}
