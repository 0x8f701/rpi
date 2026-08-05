use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use super::{WorkflowId, WorkflowSnapshot, WorkflowStatus, WorkflowSupervisorProjection};
use crate::{AgentSnapshot, AgentStatus, JobStatus, MailboxMessage, TodoState, WorkflowJobSnapshot};

const RECENT_IRC_LIMIT: usize = 50;

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
}

impl fmt::Debug for WorkflowSupervisorDetail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("WorkflowSupervisorDetail").field("status", &self.status).finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowSubagentDetail {
    pub display_name: String,
    pub status: AgentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_summary: Option<String>,
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

impl WorkflowRuntimeDetail {
    pub(crate) fn snapshot_fallback(snapshot: &WorkflowSnapshot) -> Self {
        Self {
            workflow_id: snapshot.workflow_id.clone(), generation: snapshot.generation,
            name: safe_text(&snapshot.name, 160), objective: safe_text(&snapshot.objective, 600),
            status: snapshot.status, todo: safe_todo(&snapshot.todo), supervisor: None,
            subagents: Vec::new(), jobs: Vec::new(), irc: Vec::new(),
        }
    }

    pub(crate) fn from_live(snapshot: &WorkflowSnapshot, projection: WorkflowSupervisorProjection, agents: Vec<AgentSnapshot>, mut jobs: Vec<WorkflowJobSnapshot>, irc: Vec<MailboxMessage>) -> Self {
        let agent_names = agents.iter().map(|agent| {
            let display_name = if agent.display_name.trim().is_empty() { "Agent" } else { &agent.display_name };
            (agent.id.as_str(), safe_text(display_name, 160))
        }).collect::<HashMap<_, _>>();
        let supervisor = agents.iter().find(|agent| agent.id == projection.supervisor_agent_id).map(|agent| WorkflowSupervisorDetail {
            display_name: agent_names.get(agent.id.as_str()).cloned().unwrap_or_else(|| "Supervisor".to_owned()),
            status: agent.status,
        });
        jobs.sort_by(|left, right| left.job.created_at.cmp(&right.job.created_at).then_with(|| left.job.id.cmp(&right.job.id)));
        let mut latest_summary = HashMap::<&str, &str>::new();
        for job in &jobs { if let Some(summary) = job.job.description.as_deref() { latest_summary.insert(job.job.agent_id.as_str(), summary); } }
        let subagents = agents.iter().filter(|agent| agent.id != projection.supervisor_agent_id).map(|agent| WorkflowSubagentDetail {
            display_name: agent_names.get(agent.id.as_str()).cloned().unwrap_or_else(|| "Agent".to_owned()),
            status: agent.status,
            task_summary: latest_summary.get(agent.id.as_str()).map(|summary| safe_text(summary, 240)),
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
        Self {
            workflow_id: snapshot.workflow_id.clone(), generation: snapshot.generation,
            name: safe_text(&snapshot.name, 160), objective: safe_text(&snapshot.objective, 600),
            status: projection.status, todo: safe_todo(&projection.todo), supervisor, subagents, jobs, irc,
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
            },
        }
    }

    #[test]
    fn live_detail_projects_jobs_todo_ownership_and_deduplicated_irc() {
        let projection = WorkflowSupervisorProjection {
            workflow_id: "workflow-safe".to_owned(), generation: 7, status: WorkflowStatus::Running,
            supervisor_agent_id: "supervisor-private-id".to_owned(), todo: snapshot(WorkflowStatus::Running).todo,
            jobs: Vec::new(), irc: Vec::new(), failure: None,
        };
        let message = MailboxMessage { id: "message-private-id".to_owned(), from: "worker-private-id".to_owned(), to: "supervisor-private-id".to_owned(), body: "done /private/file token=hidden".to_owned(), timestamp: 9, reply_to: None };
        let detail = WorkflowRuntimeDetail::from_live(
            &snapshot(WorkflowStatus::Running), projection,
            vec![agent("supervisor-private-id", "Supervisor", AgentStatus::Running), agent("worker-private-id", "Worker", AgentStatus::Running)],
            vec![job(JobStatus::Running), job(JobStatus::Completed)], vec![message.clone(), message],
        );

        assert_eq!(detail.jobs.len(), 2);
        assert_eq!(detail.jobs[0].todo_task_id.as_deref(), Some("todo-safe"));
        assert_eq!(detail.subagents[0].task_summary.as_deref(), Some("edit [path] api_key=[REDACTED]"));
        assert_eq!(detail.irc.len(), 1);
        assert_eq!(detail.irc[0].from, "Worker");
        assert_eq!(detail.irc[0].body, "done [path] token=[REDACTED]");
        let encoded = serde_json::to_string(&detail).expect("serialize live detail");
        for private in ["private-job-id", "message-private-id", "worker-private-id", "supervisor-private-id", "/private", "hidden"] { assert!(!encoded.contains(private), "leaked {private}"); }
    }

    #[test]
    fn terminal_fallback_and_debug_are_redacted() {
        let detail = WorkflowRuntimeDetail::snapshot_fallback(&snapshot(WorkflowStatus::Completed));
        assert!(detail.supervisor.is_none() && detail.jobs.is_empty() && detail.irc.is_empty());
        let encoded = serde_json::to_string(&detail).expect("serialize fallback");
        let debug = format!("{detail:?}");
        for private in ["/private", "hidden", "private-branch", "private-supervisor-id"] { assert!(!encoded.contains(private)); assert!(!debug.contains(private)); }
    }
}
