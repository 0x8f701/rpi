use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use super::{
    WorkflowId, WorkflowSnapshot, WorkflowStatus, WorkflowSupervisorActivity,
    WorkflowSupervisorProjection,
};
use crate::{
    AgentSnapshot, AgentStatus, JobStatus, MailboxMessage, TodoState, WorkflowJobSnapshot,
};

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
    /// Bounded, redacted failure reason for a terminal failed workflow (the
    /// live supervisor's failure message or the durable record's failure).
    /// None while the workflow is not failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
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
        formatter
            .debug_struct("WorkflowRuntimeDetail")
            .field("workflow_id", &self.workflow_id)
            .field("generation", &self.generation)
            .field("status", &self.status)
            .field("has_failure", &self.failure.is_some())
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
    /// Authoritative agent role/definition (`AgentSnapshot.agent`), distinct
    /// from `display_name` so the Web never substitutes a display label as the
    /// agent's role. Omitted on the wire only when the source carried none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub role: String,
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
    /// Authoritative agent role/definition (`AgentSnapshot.agent`), distinct
    /// from `display_name` so the Web never substitutes a display label as the
    /// agent's role. Omitted on the wire only when the source carried none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub role: String,
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
        formatter
            .debug_struct("WorkflowSubagentDetail")
            .field("status", &self.status)
            .field("has_task_summary", &self.task_summary.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRuntimeJobDetail {
    pub display_name: String,
    /// Opaque job correlation id (a runtime UUID). The Web correlates jobs
    /// and their active-task / todo-task identity by this id; the owning
    /// agent's routing id is never exposed (only the display name and role).
    pub job_id: String,
    /// Authoritative agent role/definition: the owning agent's
    /// `AgentSnapshot.agent`, falling back to the job's own `agent` field when
    /// no agent snapshot backs the job. Distinct from `display_name` so the
    /// Web never substitutes a display label as the role.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub role: String,
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
    /// True when the job settled early because a configured soft budget was
    /// reached (max requests, max tokens, or a yield-after threshold). The job
    /// is not failed: it completed with a partial result. Omitted when false
    /// so persisted legacy snapshots and wire payloads stay compatible.
    #[serde(default, skip_serializing_if = "is_false")]
    pub soft_budget_exhausted: bool,
}

impl fmt::Debug for WorkflowRuntimeJobDetail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkflowRuntimeJobDetail")
            .field("status", &self.status)
            .field("has_task_summary", &self.task_summary.is_some())
            .field("has_todo_task_id", &self.todo_task_id.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowIrcMessage {
    /// Opaque message correlation id (a runtime UUID) so the Web can thread
    /// replies (`replyTo`) and dedupe delivered messages by identity.
    pub message_id: String,
    pub from: String,
    pub to: String,
    pub body: String,
    pub timestamp_ms: u64,
    /// Opaque id of the message this one replies to, when the source carried
    /// one; absent when the message is not a reply. Correlation metadata only
    /// — no delivery state is invented when the source lacks it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}

impl fmt::Debug for WorkflowIrcMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkflowIrcMessage")
            .field("timestamp_ms", &self.timestamp_ms)
            .finish_non_exhaustive()
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
            workflow_id: snapshot.workflow_id.clone(),
            generation: snapshot.generation,
            name: safe_text(&snapshot.name, 160),
            objective: safe_text(&snapshot.objective, 600),
            status: snapshot.status,
            failure: snapshot
                .failure
                .as_ref()
                .map(|failure| safe_text(&failure.message, 600)),
            todo: safe_todo(&snapshot.todo),
            supervisor: None,
            subagents: Vec::new(),
            jobs: Vec::new(),
            irc: Vec::new(),
            supervisor_activity: Vec::new(),
            planning_started_at_ms: None,
        }
    }

    pub(crate) fn from_live(
        snapshot: &WorkflowSnapshot,
        projection: WorkflowSupervisorProjection,
        agents: Vec<AgentSnapshot>,
        mut jobs: Vec<WorkflowJobSnapshot>,
        irc: Vec<MailboxMessage>,
    ) -> Self {
        let agent_names = agents
            .iter()
            .map(|agent| {
                let display_name = if agent.display_name.trim().is_empty() {
                    "Agent"
                } else {
                    &agent.display_name
                };
                (agent.id.as_str(), safe_text(display_name, 160))
            })
            .collect::<HashMap<_, _>>();
        // Authoritative agent role/definition keyed by routing id. Distinct
        // from the display-name map so actors and jobs expose the role
        // separately (the Web must never substitute a display name as role).
        let agent_roles = agents
            .iter()
            .map(|agent| (agent.id.as_str(), safe_text(&agent.agent, 160)))
            .collect::<HashMap<_, _>>();
        jobs.sort_by(|left, right| {
            left.job
                .created_at
                .cmp(&right.job.created_at)
                .then_with(|| left.job.id.cmp(&right.job.id))
        });
        let mut latest_summary = HashMap::<&str, &str>::new();
        let mut latest_todo_task_id = HashMap::<&str, &str>::new();
        for job in &jobs {
            if let Some(summary) = job.job.description.as_deref() {
                latest_summary.insert(job.job.agent_id.as_str(), summary);
            }
            if let Some(task_id) = job
                .todo_task_id
                .as_deref()
                .or(job.job.todo_task_id.as_deref())
            {
                latest_todo_task_id.insert(job.job.agent_id.as_str(), task_id);
            }
        }
        // Per-agent bounded activity feed: delegated task lifecycle entries
        // plus IRC messages the agent sent or received, merged and newest-last
        // so each agent row on the workflow page can expand to a live feed.
        let activity_by_agent = agents
            .iter()
            .map(|agent| {
                (
                    agent.id.as_str(),
                    agent_activity(
                        agent.id.as_str(),
                        &jobs,
                        &irc,
                        &agent_names,
                        &projection.supervisor_agent_id,
                    ),
                )
            })
            .collect::<HashMap<_, _>>();
        // The supervisor's own orchestration entry stays Idle while its
        // planning turn runs in the main child session (only delegated
        // workers transition through Queued/Running). During Planning the
        // supervisor is by definition actively working, so the live detail
        // must read as Running with the objective it is planning rather than
        // a confusing idle/empty projection.
        let supervisor = agents
            .iter()
            .find(|agent| agent.id == projection.supervisor_agent_id)
            .map(|agent| {
                let status = match projection.status {
                    WorkflowStatus::Planning if agent.status == AgentStatus::Idle => {
                        AgentStatus::Running
                    }
                    WorkflowStatus::Queued if agent.status == AgentStatus::Idle => {
                        AgentStatus::Queued
                    }
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
                    display_name: agent_names
                        .get(agent.id.as_str())
                        .cloned()
                        .unwrap_or_else(|| "Supervisor".to_owned()),
                    role: agent_roles
                        .get(agent.id.as_str())
                        .cloned()
                        .unwrap_or_default(),
                    status,
                    task_summary,
                    todo_task_id: latest_todo_task_id
                        .get(agent.id.as_str())
                        .map(|id| (*id).to_owned()),
                    activity: activity_by_agent
                        .get(agent.id.as_str())
                        .cloned()
                        .unwrap_or_default(),
                }
            });
        let subagents = agents
            .iter()
            .filter(|agent| agent.id != projection.supervisor_agent_id)
            .map(|agent| WorkflowSubagentDetail {
                display_name: agent_names
                    .get(agent.id.as_str())
                    .cloned()
                    .unwrap_or_else(|| "Agent".to_owned()),
                role: agent_roles
                    .get(agent.id.as_str())
                    .cloned()
                    .unwrap_or_default(),
                status: agent.status,
                task_summary: latest_summary
                    .get(agent.id.as_str())
                    .map(|summary| safe_text(summary, 240)),
                todo_task_id: latest_todo_task_id
                    .get(agent.id.as_str())
                    .map(|id| (*id).to_owned()),
                activity: activity_by_agent
                    .get(agent.id.as_str())
                    .cloned()
                    .unwrap_or_default(),
            })
            .collect();
        let jobs = jobs
            .into_iter()
            .map(|workflow_job| {
                let job = workflow_job.job;
                WorkflowRuntimeJobDetail {
                    display_name: agent_names
                        .get(job.agent_id.as_str())
                        .cloned()
                        .unwrap_or_else(|| safe_text(&job.agent, 160)),
                    job_id: job.id.clone(),
                    // Authoritative role/definition: the owning agent's role when
                    // the agent snapshot is present, else the job's own agent field.
                    role: agent_roles
                        .get(job.agent_id.as_str())
                        .cloned()
                        .unwrap_or_else(|| safe_text(&job.agent, 160)),
                    status: job.status,
                    task_summary: job
                        .description
                        .as_deref()
                        .map(|summary| safe_text(summary, 240)),
                    todo_task_id: workflow_job.todo_task_id.or(job.todo_task_id),
                    created_at_ms: job.created_at,
                    started_at_ms: job.started_at,
                    finished_at_ms: job.finished_at,
                    soft_budget_exhausted: job.soft_budget_exhausted,
                }
            })
            .collect();
        let irc = recent_irc(irc, &agent_names, &projection.supervisor_agent_id);
        // The supervisor projection is the live view, but the durable snapshot
        // is authoritative for terminal lifecycle outcomes and the manager's
        // Integrating merge window (the one non-terminal status the supervisor
        // never produces). An integrate that lands after the supervisor already
        // projected Completed leaves the durable record Conflicted/Failed, and
        // during the merge itself the durable record is Integrating while the
        // projection still reads Completed. Never let the live view mask those
        // statuses — the panel must show the merge or conflict, not "completed".
        let status = if (snapshot.status.is_terminal()
            || snapshot.status == WorkflowStatus::Integrating)
            && snapshot.status != projection.status
        {
            snapshot.status
        } else {
            projection.status
        };
        Self {
            workflow_id: snapshot.workflow_id.clone(),
            generation: snapshot.generation,
            name: safe_text(&snapshot.name, 160),
            objective: safe_text(&snapshot.objective, 600),
            status,
            // The live supervisor's failure message is authoritative while a
            // runtime is resident; the durable record's failure is the
            // fallback (e.g. a restore failure with no live runtime). Both
            // are bounded and redacted before they reach the panel.
            failure: projection
                .failure
                .as_deref()
                .map(|message| safe_text(message, 600))
                .or_else(|| {
                    snapshot
                        .failure
                        .as_ref()
                        .map(|failure| safe_text(&failure.message, 600))
                }),
            todo: safe_todo(&projection.todo),
            supervisor,
            subagents,
            jobs,
            irc,
            supervisor_activity: projection.activity,
            planning_started_at_ms: projection.planning_started_at_ms,
        }
    }
}

fn recent_irc(
    mut messages: Vec<MailboxMessage>,
    agent_names: &HashMap<&str, String>,
    supervisor_agent_id: &str,
) -> Vec<WorkflowIrcMessage> {
    messages.sort_by(|left, right| {
        left.timestamp
            .cmp(&right.timestamp)
            .then_with(|| left.id.cmp(&right.id))
    });
    let mut seen = HashSet::new();
    let mut messages = messages
        .into_iter()
        .filter(|message| seen.insert(message.id.clone()))
        .collect::<Vec<_>>();
    if messages.len() > RECENT_IRC_LIMIT {
        messages.drain(..messages.len() - RECENT_IRC_LIMIT);
    }
    messages
        .into_iter()
        .map(|message| WorkflowIrcMessage {
            // Opaque correlation id preserved verbatim for reply threading and
            // dedupe; it is a runtime UUID, not user content, so no path/cred
            // redaction applies. replyTo carries the same opaque id when set.
            message_id: message.id,
            from: safe_actor_name(&message.from, agent_names, supervisor_agent_id),
            to: safe_actor_name(&message.to, agent_names, supervisor_agent_id),
            body: safe_text(&message.body, 600),
            timestamp_ms: message.timestamp,
            reply_to: message.reply_to,
        })
        .collect()
}

fn safe_actor_name(
    agent_id: &str,
    agent_names: &HashMap<&str, String>,
    supervisor_agent_id: &str,
) -> String {
    agent_names.get(agent_id).cloned().unwrap_or_else(|| {
        if agent_id == supervisor_agent_id {
            "Supervisor".to_owned()
        } else {
            "Agent".to_owned()
        }
    })
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
        entries.push(WorkflowAgentActivity {
            at_ms,
            kind: WorkflowAgentActivityKind::Task,
            text,
        });
    }
    // The runtime union may deliver the same message via several mailboxes
    // (supervisor projection + group inboxes); dedupe by id like `recent_irc`.
    let mut seen_ids = HashSet::new();
    for message in irc {
        if !seen_ids.insert(message.id.as_str()) {
            continue;
        }
        let (other, direction) = if message.from == agent_id {
            (
                safe_actor_name(&message.to, agent_names, supervisor_agent_id),
                true,
            )
        } else if message.to == agent_id {
            (
                safe_actor_name(&message.from, agent_names, supervisor_agent_id),
                false,
            )
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
            for dependency in &mut task.waiting_on {
                dependency.content = safe_text(&dependency.content, 600);
            }
        }
    }
    safe
}

fn safe_text(text: &str, max_chars: usize) -> String {
    let value = crate::redact_value(&serde_json::Value::String(text.to_owned()));
    let redacted = value.as_str().unwrap_or_default();
    let without_paths = absolute_path_pattern().replace_all(redacted, "${prefix}[path]");
    if without_paths.chars().count() <= max_chars {
        return without_paths.into_owned();
    }
    let mut output = without_paths
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    output.push('…');
    output
}

fn absolute_path_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r#"(?P<prefix>^|[\s(\"'`])(?:/[^\s)\"'`,;]+|[A-Za-z]:\\[^\s)\"'`,;]+)"#)
            .expect("workflow detail path redaction pattern is valid")
    })
}

/// Serde `skip_serializing_if` predicate for the soft-budget flag: omit it on
/// the wire when false (mirrors `orchestration::jobs::is_false` so the core
/// job detail stays wire-compatible with the durable `JobSnapshot`).
fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        JobSnapshot, TodoItem, TodoPhase, TodoStatus, TodoStorage, TodoWaitingOn,
        WorkflowIntegration,
    };

    fn snapshot(status: WorkflowStatus) -> WorkflowSnapshot {
        WorkflowSnapshot {
            workflow_id: WorkflowId::new("workflow-safe"),
            name: "Safe workflow".to_owned(),
            objective: "work /private/root token=hidden".to_owned(),
            status,
            created_at_ms: 1,
            updated_at_ms: 2,
            generation: 7,
            durable_policy: crate::sandbox::generation::test_durable_policy_state(),
            todo: TodoState {
                phases: Vec::new(),
                storage: TodoStorage::Memory,
            },
            worktree_label: Some("workflow-safe".to_owned()),
            branch: Some("private-branch".to_owned()),
            supervisor_agent_id: Some("private-supervisor-id".to_owned()),
            supervisor_job_id: None,
            failure: None,
            integration: WorkflowIntegration::None,
            planning_prompts_consumed: 0,

        }
    }

    fn agent(id: &str, name: &str, status: AgentStatus) -> AgentSnapshot {
        AgentSnapshot {
            id: id.to_owned(),
            display_name: name.to_owned(),
            agent: name.to_owned(),
            parent_id: None,
            status,
            created_at: 1,
            last_activity: 1,
            unread: 0,
            artifact_ref: None,
            history_ref: None,
        }
    }

    fn job(id: &str, status: JobStatus) -> WorkflowJobSnapshot {
        WorkflowJobSnapshot {
            workflow_id: "workflow-safe".to_owned(),
            generation: 7,
            todo_task_id: Some("todo-safe".to_owned()),
            job: JobSnapshot {
                id: id.to_owned(),
                agent_id: "worker-private-id".to_owned(),
                agent: "task".to_owned(),
                parent_id: "supervisor-private-id".to_owned(),
                description: Some("edit /private/file api_key=hidden".to_owned()),
                todo_task_id: Some("todo-safe".to_owned()),
                workflow_id: Some("workflow-safe".to_owned()),
                workflow_generation: Some(7),
                status,
                created_at: 3,
                started_at: Some(4),
                finished_at: status.is_settled().then_some(5),
                result: None,
                soft_budget_exhausted: false,
            },
        }
    }

    #[test]
    fn live_detail_projects_jobs_todo_ownership_and_deduplicated_irc() {
        let projection = WorkflowSupervisorProjection {
            workflow_id: "workflow-safe".to_owned(),
            generation: 7,
            status: WorkflowStatus::Running,
            supervisor_agent_id: "supervisor-private-id".to_owned(),
            todo: snapshot(WorkflowStatus::Running).todo,
            jobs: Vec::new(),
            irc: Vec::new(),
            failure: None,
            activity: Vec::new(),
            planning_started_at_ms: None,
            planning_prompts_consumed: 0,

        };
        let message = MailboxMessage {
            id: "msg-1".to_owned(),
            from: "worker-private-id".to_owned(),
            to: "supervisor-private-id".to_owned(),
            body: "done /private/file token=hidden".to_owned(),
            timestamp: 9,
            reply_to: Some("msg-0".to_owned()),
        };
        let detail = WorkflowRuntimeDetail::from_live(
            &snapshot(WorkflowStatus::Running),
            projection,
            vec![
                agent("supervisor-private-id", "Supervisor", AgentStatus::Running),
                agent("worker-private-id", "Worker", AgentStatus::Running),
            ],
            vec![
                job("job-a", JobStatus::Running),
                job("job-b", JobStatus::Completed),
            ],
            vec![message.clone(), message],
        );

        assert_eq!(detail.jobs.len(), 2);
        assert_eq!(detail.jobs[0].todo_task_id.as_deref(), Some("todo-safe"));
        // The job correlation id is exposed (a runtime UUID the Web correlates
        // by); the owning agent's routing id is not — only display name + role.
        assert_eq!(detail.jobs[0].job_id, "job-a");
        assert_eq!(detail.jobs[1].job_id, "job-b");
        assert_eq!(detail.jobs[0].display_name, "Worker");
        // Authoritative role/definition sourced from the owning agent's
        // AgentSnapshot.agent (here "Worker"), distinct from display_name and
        // never substituted by it. The job's own `agent: "task"` is the fallback
        // only when no agent snapshot backs the job.
        assert_eq!(detail.jobs[0].role, "Worker");
        // Job timestamps (created/started/finished) are all preserved.
        assert_eq!(detail.jobs[0].created_at_ms, 3);
        assert_eq!(detail.jobs[0].started_at_ms, Some(4));
        assert_eq!(detail.jobs[1].finished_at_ms, Some(5));
        // softBudgetExhausted defaults false and is omitted on the wire.
        assert!(!detail.jobs[0].soft_budget_exhausted);
        // The subagent's current task summary is its latest job's description,
        // so the detail must pair it with that job's todo-task id — the
        // identity key the panel uses to dedupe Active tasks rows.
        assert_eq!(
            detail.subagents[0].task_summary.as_deref(),
            Some("edit [path] api_key=[REDACTED]")
        );
        assert_eq!(
            detail.subagents[0].todo_task_id.as_deref(),
            Some("todo-safe")
        );
        assert!(
            detail
                .supervisor
                .as_ref()
                .is_none_or(|supervisor| supervisor.todo_task_id.is_none()),
            "supervisor owns no job; its todo_task_id must stay None"
        );
        assert_eq!(detail.irc.len(), 1);
        assert_eq!(detail.irc[0].from, "Worker");
        assert_eq!(detail.irc[0].body, "done [path] token=[REDACTED]");
        // IRC exposes the opaque message id and replyTo correlation field;
        // both are runtime UUIDs, preserved verbatim (not redacted as paths).
        assert_eq!(detail.irc[0].message_id, "msg-1");
        assert_eq!(detail.irc[0].reply_to.as_deref(), Some("msg-0"));
        let encoded = serde_json::to_string(&detail).expect("serialize live detail");
        // The opaque job/message correlation ids are on the wire by design;
        // the agent routing ids, paths, and credentials must not leak.
        for private in [
            "worker-private-id",
            "supervisor-private-id",
            "/private",
            "hidden",
        ] {
            assert!(!encoded.contains(private), "leaked {private}");
        }
        assert!(
            encoded.contains("\"jobId\":\"job-a\""),
            "jobId must be exposed: {encoded}"
        );
        assert!(
            encoded.contains("\"messageId\":\"msg-1\""),
            "messageId must be exposed: {encoded}"
        );
        assert!(
            encoded.contains("\"replyTo\":\"msg-0\""),
            "replyTo must be exposed: {encoded}"
        );
        // softBudgetExhausted is false for both jobs and must be omitted.
        assert!(
            !encoded.contains("softBudgetExhausted"),
            "false softBudgetExhausted must be omitted: {encoded}"
        );
    }

    #[test]
    fn live_detail_projects_per_agent_activity_feed() {
        // Each agent's expanded row needs a bounded live feed: delegated task
        // lifecycle entries plus IRC the agent sent or received, merged in
        // timestamp order, newest-last, names resolved, secrets redacted.
        let projection = WorkflowSupervisorProjection {
            workflow_id: "workflow-safe".to_owned(),
            generation: 7,
            status: WorkflowStatus::Running,
            supervisor_agent_id: "supervisor-private-id".to_owned(),
            todo: snapshot(WorkflowStatus::Running).todo,
            jobs: Vec::new(),
            irc: Vec::new(),
            failure: None,
            activity: Vec::new(),
            planning_started_at_ms: None,
            planning_prompts_consumed: 0,

        };
        let mut jobs = vec![job("job-c", JobStatus::Completed)];
        jobs[0].job.description = Some("finish the report".to_owned());
        jobs[0].job.started_at = Some(10);
        jobs[0].job.finished_at = Some(11);
        let irc = vec![
            MailboxMessage {
                id: "m-1".to_owned(),
                from: "worker-private-id".to_owned(),
                to: "supervisor-private-id".to_owned(),
                body: "done, see /private/file".to_owned(),
                timestamp: 12,
                reply_to: None,
            },
            MailboxMessage {
                id: "m-2".to_owned(),
                from: "worker-private-id".to_owned(),
                to: "peer-private-id".to_owned(),
                body: "handoff to peer".to_owned(),
                timestamp: 13,
                reply_to: None,
            },
        ];
        let detail = WorkflowRuntimeDetail::from_live(
            &snapshot(WorkflowStatus::Running),
            projection,
            vec![
                agent("supervisor-private-id", "Supervisor", AgentStatus::Running),
                agent("worker-private-id", "Worker", AgentStatus::Running),
                agent("peer-private-id", "Peer", AgentStatus::Parked),
            ],
            jobs,
            irc,
        );

        let worker = &detail.subagents[0];
        assert_eq!(
            worker.activity.len(),
            3,
            "task + outgoing + incoming IRC entries"
        );
        // Newest-last: oldest entry first (task finish at 11), newest last
        // (the worker → peer IRC at 13).
        assert_eq!(worker.activity[0].kind, WorkflowAgentActivityKind::Task);
        assert_eq!(worker.activity[0].text, "finish the report");
        assert_eq!(worker.activity[1].kind, WorkflowAgentActivityKind::Irc);
        assert!(
            worker.activity[1]
                .text
                .contains("Supervisor: done, see [path]"),
            "incoming IRC names the sender: {}",
            worker.activity[1].text
        );
        assert_eq!(worker.activity[2].at_ms, 13);
        assert_eq!(worker.activity[2].text, "→ Peer: handoff to peer");
        // The peer's feed shows the incoming message from the worker.
        let peer = detail
            .subagents
            .iter()
            .find(|subagent| subagent.display_name == "Peer")
            .expect("peer");
        assert_eq!(peer.activity.len(), 1);
        assert_eq!(peer.activity[0].text, "Worker: handoff to peer");
        // The supervisor's feed shows only its own IRC, not the worker/peer hop.
        let supervisor = detail.supervisor.as_ref().expect("supervisor");
        assert_eq!(supervisor.activity.len(), 1);
        assert_eq!(supervisor.activity[0].text, "Worker: done, see [path]");
        // Redaction: no raw ids or paths leak through the feed.
        let encoded = serde_json::to_string(&detail).expect("serialize live detail");
        for private in ["worker-private-id", "peer-private-id", "/private"] {
            assert!(!encoded.contains(private), "leaked {private}");
        }
    }

    #[test]
    fn live_detail_shows_subagent_to_subagent_irc() {
        // IRC between two subagents (neither is the supervisor) must surface
        // in the workflow detail's recent IRC, not just supervisor-bound
        // messages, so the page can show subagent ⇄ subagent communication.
        let projection = WorkflowSupervisorProjection {
            workflow_id: "workflow-safe".to_owned(),
            generation: 7,
            status: WorkflowStatus::Running,
            supervisor_agent_id: "supervisor-private-id".to_owned(),
            todo: snapshot(WorkflowStatus::Running).todo,
            jobs: Vec::new(),
            irc: Vec::new(),
            failure: None,
            activity: Vec::new(),
            planning_started_at_ms: None,
            planning_prompts_consumed: 0,

        };
        let irc = vec![
            MailboxMessage {
                id: "peer-a".to_owned(),
                from: "worker-a-private".to_owned(),
                to: "worker-b-private".to_owned(),
                body: "results ready".to_owned(),
                timestamp: 20,
                reply_to: None,
            },
            MailboxMessage {
                id: "peer-b".to_owned(),
                from: "worker-b-private".to_owned(),
                to: "worker-a-private".to_owned(),
                body: "acknowledged".to_owned(),
                timestamp: 21,
                reply_to: None,
            },
            MailboxMessage {
                id: "peer-b".to_owned(),
                from: "worker-b-private".to_owned(),
                to: "worker-a-private".to_owned(),
                body: "acknowledged".to_owned(),
                timestamp: 21,
                reply_to: None,
            },
        ];
        let detail = WorkflowRuntimeDetail::from_live(
            &snapshot(WorkflowStatus::Running),
            projection,
            vec![
                agent("supervisor-private-id", "Supervisor", AgentStatus::Running),
                agent("worker-a-private", "WorkerA", AgentStatus::Parked),
                agent("worker-b-private", "WorkerB", AgentStatus::Parked),
            ],
            Vec::new(),
            irc,
        );
        assert_eq!(detail.irc.len(), 2, "duplicate message ids must collapse");
        assert_eq!(detail.irc[0].from, "WorkerA");
        assert_eq!(detail.irc[0].to, "WorkerB");
        assert_eq!(detail.irc[0].body, "results ready");
        // Dedupe is by the opaque message id, which is also exposed for
        // correlation/threading on the Web.
        assert_eq!(detail.irc[0].message_id, "peer-a");
        assert_eq!(detail.irc[1].from, "WorkerB");
        assert_eq!(detail.irc[1].to, "WorkerA");
        assert_eq!(detail.irc[1].message_id, "peer-b");
        assert!(
            detail.irc[1].reply_to.is_none(),
            "non-reply messages omit replyTo"
        );
        // Both directions also feed each agent's activity row.
        let worker_a = detail
            .subagents
            .iter()
            .find(|subagent| subagent.display_name == "WorkerA")
            .expect("worker a");
        assert_eq!(worker_a.activity.len(), 2);
        let worker_b = detail
            .subagents
            .iter()
            .find(|subagent| subagent.display_name == "WorkerB")
            .expect("worker b");
        assert_eq!(worker_b.activity.len(), 2);
    }

    #[test]
    fn live_detail_caps_recent_irc_and_retains_correlation() {
        let projection = WorkflowSupervisorProjection {
            workflow_id: "workflow-safe".to_owned(),
            generation: 7,
            status: WorkflowStatus::Running,
            supervisor_agent_id: "supervisor-private-id".to_owned(),
            todo: snapshot(WorkflowStatus::Running).todo,
            jobs: Vec::new(),
            irc: Vec::new(),
            failure: None,
            activity: Vec::new(),
            planning_started_at_ms: None,
            planning_prompts_consumed: 0,

        };
        let irc = (0..RECENT_IRC_LIMIT + 5)
            .map(|index| MailboxMessage {
                id: format!("msg-{index}"),
                from: "worker-private-id".to_owned(),
                to: "supervisor-private-id".to_owned(),
                body: format!("message-{index}"),
                timestamp: index as u64,
                reply_to: (index > 0).then(|| format!("msg-{}", index - 1)),
            })
            .collect();
        let detail = WorkflowRuntimeDetail::from_live(
            &snapshot(WorkflowStatus::Running),
            projection,
            vec![
                agent("supervisor-private-id", "Supervisor", AgentStatus::Running),
                agent("worker-private-id", "Worker", AgentStatus::Running),
            ],
            Vec::new(),
            irc,
        );
        assert_eq!(detail.irc.len(), RECENT_IRC_LIMIT);
        assert_eq!(detail.irc[0].message_id, "msg-5");
        let newest = detail.irc.last().expect("newest IRC");
        let expected_reply_to = format!("msg-{}", RECENT_IRC_LIMIT + 3);
        assert_eq!(newest.reply_to.as_deref(), Some(expected_reply_to.as_str()));
        assert_eq!(newest.from, "Worker");
        assert_eq!(newest.to, "Supervisor");
    }

    #[test]
    fn planning_detail_reads_supervisor_as_active_with_planning_task() {
        let projection = WorkflowSupervisorProjection {
            workflow_id: "workflow-safe".to_owned(),
            generation: 7,
            status: WorkflowStatus::Planning,
            supervisor_agent_id: "supervisor-private-id".to_owned(),
            todo: TodoState {
                phases: Vec::new(),
                storage: TodoStorage::Memory,
            },
            jobs: Vec::new(),
            irc: Vec::new(),
            failure: None,
            activity: Vec::new(),
            planning_started_at_ms: None,
            planning_prompts_consumed: 1,

        };
        // The supervisor's orchestration entry is Idle while its planning turn
        // runs (only delegated workers transition to Running); the live detail
        // must still read the supervisor as actively planning.
        let detail = WorkflowRuntimeDetail::from_live(
            &snapshot(WorkflowStatus::Planning),
            projection,
            vec![agent(
                "supervisor-private-id",
                "Supervisor",
                AgentStatus::Idle,
            )],
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(detail.status, WorkflowStatus::Planning);
        let supervisor = detail.supervisor.expect("planning supervisor");
        assert_eq!(supervisor.status, AgentStatus::Running);
        assert_eq!(supervisor.display_name, "Supervisor");
        assert_eq!(
            supervisor.role, "Supervisor",
            "role is sourced from AgentSnapshot.agent, separate from display_name"
        );
        let task = supervisor.task_summary.expect("planning task summary");
        assert!(
            task.starts_with("Planning: "),
            "planning task must name the objective: {task}"
        );
        assert!(
            !task.contains("private"),
            "planning task must not leak raw objective: {task}"
        );
        assert!(
            supervisor.todo_task_id.is_none(),
            "the planning summary is not job-backed; it must carry no todo task id"
        );
        assert!(detail.jobs.is_empty() && detail.todo.phases.is_empty());
    }

    #[test]
    fn terminal_fallback_and_debug_are_redacted() {
        let detail = WorkflowRuntimeDetail::snapshot_fallback(&snapshot(WorkflowStatus::Completed));
        // The durable snapshot carries no live runtime state, so the fallback
        // must NOT fabricate it: no supervisor, no subagents, no jobs, no IRC,
        // no planning activity, and no planning-start clock. Only the durable
        // identity, status, objective, and full authoritative Todo phases are
        // preserved. The Web must read these absent sections as "unavailable"
        // live-only data, never as empty-but-real values.
        assert!(
            detail.supervisor.is_none(),
            "fallback has no live supervisor"
        );
        assert!(
            detail.subagents.is_empty(),
            "fallback has no live subagents"
        );
        assert!(detail.jobs.is_empty(), "fallback has no live jobs");
        assert!(detail.irc.is_empty(), "fallback has no live IRC");
        assert!(
            detail.supervisor_activity.is_empty(),
            "fallback has no live planning activity"
        );
        assert!(
            detail.planning_started_at_ms.is_none(),
            "fallback has no live planning-start clock"
        );
        // Durable identity + status + objective are preserved (redacted).
        assert_eq!(detail.workflow_id, WorkflowId::new("workflow-safe"));
        assert_eq!(detail.generation, 7);
        assert_eq!(detail.status, WorkflowStatus::Completed);
        let encoded = serde_json::to_string(&detail).expect("serialize fallback");
        let debug = format!("{detail:?}");
        for private in [
            "/private",
            "hidden",
            "private-branch",
            "private-supervisor-id",
        ] {
            assert!(!encoded.contains(private));
            assert!(!debug.contains(private));
        }
        // No live-only fields are fabricated onto the wire: planning activity
        // and the planning-start clock are skipped when empty/absent, while the
        // job/irc collections serialize as honest empty arrays (no live data),
        // never fabricated entries.
        assert!(
            !encoded.contains("supervisorActivity"),
            "fallback must not emit planning activity"
        );
        assert!(
            !encoded.contains("planningStartedAtMs"),
            "fallback must not emit a planning clock"
        );
        assert!(
            encoded.contains("\"jobs\":[]"),
            "fallback jobs is an empty array, not fabricated: {encoded}"
        );
        assert!(
            encoded.contains("\"irc\":[]"),
            "fallback irc is an empty array, not fabricated: {encoded}"
        );
    }

    #[test]
    fn live_detail_projects_failure_reason_bounded_and_redacted() {
        // A failed workflow must carry its failure reason through the live
        // detail: the supervisor's failure message is authoritative while a
        // runtime is resident, and the durable record's failure is the
        // fallback. Both are bounded and redacted before they reach the panel.
        let token = ["s", "k-", "abcdefghijklmnop1234"].concat();
        let mut durable = snapshot(WorkflowStatus::Failed);
        durable.failure = Some(crate::WorkflowFailure {
            message: format!("workflow runtime recovery failed: {token} /private/secret/path"),
        });
        let live_message = format!(
            "workflow runtime creation failed: {token} /private/secret/path {}",
            "x".repeat(1200)
        );
        let projection = WorkflowSupervisorProjection {
            workflow_id: "workflow-safe".to_owned(),
            generation: 7,
            status: WorkflowStatus::Failed,
            supervisor_agent_id: "supervisor-private-id".to_owned(),
            todo: snapshot(WorkflowStatus::Failed).todo,
            jobs: Vec::new(),
            irc: Vec::new(),
            failure: Some(live_message),
            activity: Vec::new(),
            planning_started_at_ms: None,
            planning_prompts_consumed: 0,

        };
        let detail = WorkflowRuntimeDetail::from_live(
            &durable,
            projection,
            vec![agent(
                "supervisor-private-id",
                "Supervisor",
                AgentStatus::Idle,
            )],
            Vec::new(),
            Vec::new(),
        );
        let failure = detail
            .failure
            .as_deref()
            .expect("live failure must be present");
        assert!(
            failure.starts_with("workflow runtime creation failed"),
            "the live supervisor failure must win over the durable fallback: {failure}"
        );
        assert!(
            failure.contains("[REDACTED]") && !failure.contains(token.as_str()),
            "live failure must be secret-redacted: {failure}"
        );
        assert!(
            failure.contains("[path]") && !failure.contains("/private/secret/path"),
            "live failure must mask absolute paths: {failure}"
        );
        assert!(
            failure.chars().count() <= 600,
            "live failure must be bounded to 600 chars: {} chars",
            failure.chars().count()
        );

        // Without a live failure the durable record's failure is the fallback,
        // also redacted and path-masked.
        let projection = WorkflowSupervisorProjection {
            workflow_id: "workflow-safe".to_owned(),
            generation: 7,
            status: WorkflowStatus::Failed,
            supervisor_agent_id: "supervisor-private-id".to_owned(),
            todo: snapshot(WorkflowStatus::Failed).todo,
            jobs: Vec::new(),
            irc: Vec::new(),
            failure: None,
            activity: Vec::new(),
            planning_started_at_ms: None,
            planning_prompts_consumed: 0,

        };
        let detail = WorkflowRuntimeDetail::from_live(
            &durable,
            projection,
            vec![agent(
                "supervisor-private-id",
                "Supervisor",
                AgentStatus::Idle,
            )],
            Vec::new(),
            Vec::new(),
        );
        let fallback_failure = detail
            .failure
            .as_deref()
            .expect("durable failure must be the fallback");
        assert!(
            fallback_failure.starts_with("workflow runtime recovery failed"),
            "the durable failure must be the fallback: {fallback_failure}"
        );
        assert!(
            fallback_failure.contains("[REDACTED]") && !fallback_failure.contains(token.as_str()),
            "durable fallback must be secret-redacted: {fallback_failure}"
        );
        assert!(
            fallback_failure.contains("[path]")
                && !fallback_failure.contains("/private/secret/path"),
            "durable fallback must mask absolute paths: {fallback_failure}"
        );

        // The terminal fallback (no live runtime) also carries the durable
        // failure, bounded and redacted.
        let fallback = WorkflowRuntimeDetail::snapshot_fallback(&durable);
        let fallback_failure = fallback
            .failure
            .as_deref()
            .expect("snapshot fallback failure");
        assert!(
            fallback_failure.contains("[REDACTED]") && !fallback_failure.contains(token.as_str()),
            "snapshot fallback must be secret-redacted: {fallback_failure}"
        );
        assert!(
            fallback_failure.contains("[path]")
                && !fallback_failure.contains("/private/secret/path"),
            "snapshot fallback must mask absolute paths: {fallback_failure}"
        );
        let encoded = serde_json::to_string(&fallback).expect("serialize fallback");
        assert!(
            encoded.contains("\"failure\":") && !encoded.contains(token.as_str()),
            "failure must be on the wire, redacted: {encoded}"
        );
    }

    #[test]
    fn terminal_durable_status_wins_over_stale_live_projection() {
        // An integrate (auto or manual) that lands after the supervisor
        // already projected Completed leaves the durable record Conflicted
        // while the live projection still reads Completed. The live detail
        // must surface the durable terminal status so the panel shows the
        // conflict instead of a stale "completed".
        let projection = WorkflowSupervisorProjection {
            workflow_id: "workflow-safe".to_owned(),
            generation: 7,
            status: WorkflowStatus::Completed,
            supervisor_agent_id: "supervisor-private-id".to_owned(),
            todo: TodoState {
                phases: Vec::new(),
                storage: TodoStorage::Memory,
            },
            jobs: Vec::new(),
            irc: Vec::new(),
            failure: None,
            activity: Vec::new(),
            planning_started_at_ms: None,
            planning_prompts_consumed: 0,

        };
        let mut durable = snapshot(WorkflowStatus::Completed);
        durable.integration = WorkflowIntegration::Conflicted {
            conflicts: vec!["private/path".to_owned()],
        };
        durable.status = WorkflowStatus::Conflicted;
        let detail = WorkflowRuntimeDetail::from_live(
            &durable,
            projection,
            vec![agent(
                "supervisor-private-id",
                "Supervisor",
                AgentStatus::Idle,
            )],
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(detail.status, WorkflowStatus::Conflicted);
    }

    #[test]
    fn durable_integrating_status_wins_over_stale_completed_projection() {
        // During the auto/manual integrate window the durable record is
        // Integrating (persisted before factory.integrate) while the
        // supervisor projection is stale Completed. The live detail must
        // prefer the durable Integrating status so the panel shows the
        // in-flight merge instead of "completed".
        let projection = WorkflowSupervisorProjection {
            workflow_id: "workflow-safe".to_owned(),
            generation: 7,
            status: WorkflowStatus::Completed,
            supervisor_agent_id: "supervisor-private-id".to_owned(),
            todo: TodoState {
                phases: Vec::new(),
                storage: TodoStorage::Memory,
            },
            jobs: Vec::new(),
            irc: Vec::new(),
            failure: None,
            activity: Vec::new(),
            planning_started_at_ms: None,
            planning_prompts_consumed: 0,

        };
        let mut durable = snapshot(WorkflowStatus::Integrating);
        durable.integration = WorkflowIntegration::None;
        let detail = WorkflowRuntimeDetail::from_live(
            &durable,
            projection,
            vec![agent(
                "supervisor-private-id",
                "Supervisor",
                AgentStatus::Idle,
            )],
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(detail.status, WorkflowStatus::Integrating);
    }

    #[test]
    fn live_detail_separates_actor_display_name_from_role() {
        // The actor's display name and authoritative role/definition are
        // distinct fields sourced independently — the Web must never
        // substitute a display label as the role. Build agents whose
        // display_name differs from their AgentSnapshot.agent (role).
        let projection = WorkflowSupervisorProjection {
            workflow_id: "workflow-safe".to_owned(),
            generation: 7,
            status: WorkflowStatus::Running,
            supervisor_agent_id: "supervisor-private-id".to_owned(),
            todo: snapshot(WorkflowStatus::Running).todo,
            jobs: Vec::new(),
            irc: Vec::new(),
            failure: None,
            activity: Vec::new(),
            planning_started_at_ms: None,
            planning_prompts_consumed: 0,

        };
        let sup = AgentSnapshot {
            id: "supervisor-private-id".to_owned(),
            display_name: "Lead Planner".to_owned(),
            agent: "supervisor".to_owned(),
            parent_id: None,
            status: AgentStatus::Running,
            created_at: 1,
            last_activity: 1,
            unread: 0,
            artifact_ref: None,
            history_ref: None,
        };
        let worker = AgentSnapshot {
            id: "worker-private-id".to_owned(),
            display_name: "Code Review".to_owned(),
            agent: "reviewer".to_owned(),
            parent_id: None,
            status: AgentStatus::Running,
            created_at: 1,
            last_activity: 1,
            unread: 0,
            artifact_ref: None,
            history_ref: None,
        };
        let detail = WorkflowRuntimeDetail::from_live(
            &snapshot(WorkflowStatus::Running),
            projection,
            vec![sup, worker],
            Vec::new(),
            Vec::new(),
        );
        let supervisor = detail.supervisor.as_ref().expect("supervisor");
        assert_eq!(supervisor.display_name, "Lead Planner");
        assert_eq!(
            supervisor.role, "supervisor",
            "role is the AgentSnapshot.agent, not the display name"
        );
        let sub = &detail.subagents[0];
        assert_eq!(sub.display_name, "Code Review");
        assert_eq!(
            sub.role, "reviewer",
            "role is the AgentSnapshot.agent, not the display name"
        );
        // On the wire the two are separate camelCase keys.
        let encoded = serde_json::to_string(&detail).expect("serialize");
        assert!(
            encoded.contains("\"displayName\":\"Lead Planner\""),
            "displayName exposed: {encoded}"
        );
        assert!(
            encoded.contains("\"role\":\"supervisor\""),
            "role exposed separately: {encoded}"
        );
        assert!(
            encoded.contains("\"role\":\"reviewer\""),
            "subagent role exposed separately: {encoded}"
        );
        // No routing ids leak.
        for private in ["supervisor-private-id", "worker-private-id"] {
            assert!(!encoded.contains(private), "leaked {private}");
        }
    }

    #[test]
    fn live_detail_job_role_falls_back_to_job_agent_when_agent_snapshot_absent() {
        // When no AgentSnapshot backs a job (e.g. a job whose agent was pruned),
        // the role/definition falls back to the job's own `agent` field rather
        // than being left empty or substituted by the display name.
        let projection = WorkflowSupervisorProjection {
            workflow_id: "workflow-safe".to_owned(),
            generation: 7,
            status: WorkflowStatus::Running,
            supervisor_agent_id: "supervisor-private-id".to_owned(),
            todo: snapshot(WorkflowStatus::Running).todo,
            jobs: Vec::new(),
            irc: Vec::new(),
            failure: None,
            activity: Vec::new(),
            planning_started_at_ms: None,
            planning_prompts_consumed: 0,

        };
        let mut orphan = job("job-orphan", JobStatus::Running);
        orphan.job.agent_id = "ghost-private-id".to_owned(); // not in the agents list
        orphan.job.agent = "researcher".to_owned();
        let detail = WorkflowRuntimeDetail::from_live(
            &snapshot(WorkflowStatus::Running),
            projection,
            vec![agent(
                "supervisor-private-id",
                "Supervisor",
                AgentStatus::Running,
            )],
            vec![orphan],
            Vec::new(),
        );
        assert_eq!(detail.jobs.len(), 1);
        assert_eq!(
            detail.jobs[0].role, "researcher",
            "role falls back to job.agent when no agent snapshot is present"
        );
        // The display name also falls back to job.agent (no display name available).
        assert_eq!(detail.jobs[0].display_name, "researcher");
        assert_eq!(detail.jobs[0].job_id, "job-orphan");
    }

    #[test]
    fn live_detail_exposes_soft_budget_exhausted_on_wire() {
        // A job that settled early on a soft budget surfaces the flag on the
        // wire so the Web can mark partial results distinctly from failures.
        let projection = WorkflowSupervisorProjection {
            workflow_id: "workflow-safe".to_owned(),
            generation: 7,
            status: WorkflowStatus::Running,
            supervisor_agent_id: "supervisor-private-id".to_owned(),
            todo: snapshot(WorkflowStatus::Running).todo,
            jobs: Vec::new(),
            irc: Vec::new(),
            failure: None,
            activity: Vec::new(),
            planning_started_at_ms: None,
            planning_prompts_consumed: 0,

        };
        let mut partial = job("job-partial", JobStatus::Completed);
        partial.job.soft_budget_exhausted = true;
        let detail = WorkflowRuntimeDetail::from_live(
            &snapshot(WorkflowStatus::Running),
            projection,
            vec![
                agent("supervisor-private-id", "Supervisor", AgentStatus::Running),
                agent("worker-private-id", "Worker", AgentStatus::Running),
            ],
            vec![partial],
            Vec::new(),
        );
        assert!(
            detail.jobs[0].soft_budget_exhausted,
            "soft budget flag flows through"
        );
        let encoded = serde_json::to_string(&detail).expect("serialize");
        assert!(
            encoded.contains("\"softBudgetExhausted\":true"),
            "softBudgetExhausted emitted when true: {encoded}"
        );
        // A normal job (false default) omits the field — covered by
        // live_detail_projects_jobs_todo_ownership_and_deduplicated_irc,
        // which asserts soft_budget_exhausted is false and the wire omits it.
    }

    #[test]
    fn live_detail_preserves_full_authoritative_todo_phases() {
        // WorkflowRuntimeDetail.todo preserves the full authoritative Todo
        // phases (phase names, task content, dependencies, status, ready, and
        // the optional per-task agent) — redacted, but structurally complete.
        let mut snap = snapshot(WorkflowStatus::Running);
        snap.todo = TodoState {
            phases: vec![
                TodoPhase {
                    name: "Build /private/phase".to_owned(),
                    tasks: vec![
                        TodoItem {
                            id: "t-1".to_owned(),
                            content: "Design /private/protocol token=secret".to_owned(),
                            status: TodoStatus::Completed,
                            depends_on: Vec::new(),
                            ready: true,
                            waiting_on: Vec::new(),
                            agent: Some("reviewer".to_owned()),
                        },
                        TodoItem {
                            id: "t-2".to_owned(),
                            content: "Render page".to_owned(),
                            status: TodoStatus::InProgress,
                            depends_on: vec!["t-1".to_owned()],
                            ready: false,
                            waiting_on: vec![TodoWaitingOn {
                                task_id: "t-1".to_owned(),
                                content: "dependency".to_owned(),
                                status: TodoStatus::Completed,
                            }],
                            agent: None,
                        },
                    ],
                },
                TodoPhase {
                    name: "Ship".to_owned(),
                    tasks: vec![TodoItem {
                        id: "t-3".to_owned(),
                        content: "Ship it".to_owned(),
                        status: TodoStatus::Pending,
                        depends_on: Vec::new(),
                        ready: true,
                        waiting_on: Vec::new(),
                        agent: None,
                    }],
                },
            ],
            storage: TodoStorage::Memory,
        };
        let projection = WorkflowSupervisorProjection {
            workflow_id: "workflow-safe".to_owned(),
            generation: 7,
            status: WorkflowStatus::Running,
            supervisor_agent_id: "supervisor-private-id".to_owned(),
            todo: snap.todo.clone(),
            jobs: Vec::new(),
            irc: Vec::new(),
            failure: None,
            activity: Vec::new(),
            planning_started_at_ms: None,
            planning_prompts_consumed: 0,

        };
        let detail = WorkflowRuntimeDetail::from_live(
            &snap,
            projection,
            vec![agent(
                "supervisor-private-id",
                "Supervisor",
                AgentStatus::Running,
            )],
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(detail.todo.phases.len(), 2, "both phases preserved");
        assert_eq!(detail.todo.phases[0].tasks.len(), 2);
        assert_eq!(detail.todo.phases[1].tasks.len(), 1);
        // Phase name + task content are redacted (paths/secrets scrubbed) but
        // structurally complete: ids, dependencies, status, ready, agent.
        assert_eq!(detail.todo.phases[0].name, "Build [path]");
        assert_eq!(detail.todo.phases[0].tasks[0].id, "t-1");
        assert_eq!(
            detail.todo.phases[0].tasks[0].content,
            "Design [path] token=[REDACTED]"
        );
        assert_eq!(detail.todo.phases[0].tasks[0].status, TodoStatus::Completed);
        assert!(detail.todo.phases[0].tasks[0].ready);
        assert_eq!(
            detail.todo.phases[0].tasks[0].agent.as_deref(),
            Some("reviewer")
        );
        assert_eq!(
            detail.todo.phases[0].tasks[1].depends_on,
            vec!["t-1".to_owned()]
        );
        assert_eq!(detail.todo.phases[0].tasks[1].waiting_on[0].task_id, "t-1");
        assert_eq!(detail.todo.phases[1].tasks[0].id, "t-3");
        // No raw paths/secrets leak through the todo projection.
        let encoded = serde_json::to_string(&detail).expect("serialize");
        for private in ["/private", "secret"] {
            assert!(!encoded.contains(private), "leaked {private}");
        }
    }
}
