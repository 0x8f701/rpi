//! Cross-session handoff summaries.
//!
//! A handoff is a concise, structured handoff summary of the current session:
//! what was done (recent user asks), the current state (active goal, todo
//! counts, running orchestration jobs), the environment (cwd, git branch and
//! dirtiness, model), and deterministic next-step hints. The structured
//! envelope is built entirely from session state — no model call. The optional
//! prose paragraph is produced by the existing summarization path (see
//! `Session::generate_handoff_with_prose`) with a handoff prompt, bounded to a
//! single provider call with a hard timeout.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use pi_ai::{ContentBlock, Message};
use serde::{Deserialize, Serialize};

use crate::redact::redact_secrets;
use crate::{GoalLifecycle, JobSnapshot, JobStatus, Session, TodoState, TodoStatus};

pub const HANDOFF_MAX_RECENT_USER_ASKS: usize = 5;
pub const HANDOFF_MAX_NEXT_STEP_HINTS: usize = 4;
/// Upper bound for the single handoff-prose provider call. Compaction uses a
/// much larger bound because it summarizes huge conversations; handoff prose
/// is deliberately small and must never stall the caller for long.
pub const HANDOFF_SUMMARIZE_TIMEOUT: Duration = Duration::from_secs(60);
pub const HANDOFF_PROSE_RESERVE_TOKENS: i64 = 600;

pub const HANDOFF_SYSTEM_PROMPT: &str = "You are a handoff summarization assistant. Read the conversation between a user and an AI coding assistant, then produce a concise prose handoff summary that a fresh session can use to continue the work. Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the handoff summary.";

/// User-side handoff-prose prompt (appended after the serialized conversation
/// and the rendered structured envelope).
pub const HANDOFF_PROMPT: &str = "The conversation and structured handoff envelope above describe work being handed off to a fresh session. Write a concise prose handoff paragraph covering: what was accomplished, what is currently in flight, and concrete next steps. Ground every claim in the conversation; do not restate the envelope; only output the prose handoff summary.";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffGitState {
    /// Current branch name; `None` when HEAD is detached.
    pub branch: Option<String>,
    pub dirty: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffGoal {
    pub objective: String,
    /// Lowercase lifecycle name (`active`, `paused`, `completed`, `dropped`).
    pub lifecycle: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_tokens: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffTodoCounts {
    pub pending: usize,
    pub in_progress: usize,
    pub completed: usize,
    pub abandoned: usize,
    pub total: usize,
}

/// One active (queued or running) orchestration job in the handoff.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffJob {
    pub id: String,
    pub agent: String,
    pub status: JobStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_id: Option<String>,
}

/// The deterministic handoff envelope — built from state, never a model call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffEnvelope {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// `provider/model` of the active model, when one is selected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<HandoffGitState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<HandoffGoal>,
    pub todo: HandoffTodoCounts,
    /// Active orchestration jobs (queued or running); settled jobs are omitted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<HandoffJob>,
    /// The most recent user asks (newest first), truncated per line.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_user_asks: Vec<String>,
    pub message_count: usize,
    /// Deterministic next-step hints derived from the active goal and
    /// in-progress todos.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next_step_hints: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Handoff {
    pub envelope: HandoffEnvelope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prose: Option<String>,
}

impl Handoff {
    /// Renders the handoff as a copyable text block (markdown-flavored plain
    /// text, safe for terminals and pasting into a fresh session).
    #[must_use]
    pub fn render(&self) -> String {
        let envelope = &self.envelope;
        let mut lines: Vec<String> = Vec::new();
        lines.push("# Handoff".to_owned());
        lines.push(String::new());

        let session_label = envelope
            .session_name
            .clone()
            .or_else(|| envelope.session_id.clone())
            .unwrap_or_else(|| "(unnamed)".to_owned());
        lines.push(format!(
            "- Session: {session_label} · {} message{}",
            envelope.message_count,
            plural(envelope.message_count)
        ));
        if let Some(model) = &envelope.model {
            lines.push(format!("- Model: {model}"));
        }
        lines.push(format!("- cwd: {}", envelope.cwd));
        match &envelope.git {
            Some(git) => match (&git.branch, git.dirty) {
                (Some(branch), true) => lines.push(format!("- git: `{branch}` (dirty)")),
                (Some(branch), false) => lines.push(format!("- git: `{branch}` (clean)")),
                (None, true) => lines.push("- git: detached HEAD (dirty)".to_owned()),
                (None, false) => lines.push("- git: detached HEAD".to_owned()),
            },
            None => lines.push("- git: not a git repository".to_owned()),
        }

        lines.push(String::new());
        lines.push("## Goal".to_owned());
        match &envelope.goal {
            Some(goal) => {
                let mut goal_line = format!("{} · {}", goal.objective, goal.lifecycle);
                if let Some(remaining) = goal.remaining_tokens {
                    goal_line.push_str(&format!(" · {remaining} tokens remaining"));
                }
                lines.push(goal_line);
            }
            None => lines.push("(no goal)".to_owned()),
        }

        lines.push(String::new());
        lines.push("## Todos".to_owned());
        let todo = &envelope.todo;
        lines.push(format!(
            "{} pending · {} in progress · {} completed · {} abandoned ({} total)",
            todo.pending, todo.in_progress, todo.completed, todo.abandoned, todo.total
        ));

        lines.push(String::new());
        lines.push("## Running jobs".to_owned());
        if envelope.jobs.is_empty() {
            lines.push("(none)".to_owned());
        } else {
            for job in &envelope.jobs {
                let mut job_line = format!("- {} ({})", job.agent, job_status_name(&job.status));
                if let Some(description) = job.description.as_deref() {
                    if let Some(first_line) = description.lines().next() {
                        let first_line = first_line.trim();
                        if !first_line.is_empty() {
                            job_line.push_str(": ");
                            job_line.push_str(&truncate_line(first_line, 120));
                        }
                    }
                }
                lines.push(job_line);
            }
        }

        lines.push(String::new());
        lines.push("## Recent asks".to_owned());
        if envelope.recent_user_asks.is_empty() {
            lines.push("(none)".to_owned());
        } else {
            for ask in &envelope.recent_user_asks {
                let first_line = ask.lines().next().unwrap_or_default().trim();
                lines.push(format!("- {}", truncate_line(first_line, 160)));
            }
        }

        lines.push(String::new());
        lines.push("## Next steps".to_owned());
        if envelope.next_step_hints.is_empty() && self.prose.is_none() {
            lines.push("(none — no active goal or in-progress todos)".to_owned());
        } else {
            for hint in &envelope.next_step_hints {
                lines.push(format!("- {hint}"));
            }
            if let Some(prose) = &self.prose {
                if !envelope.next_step_hints.is_empty() {
                    lines.push(String::new());
                }
                lines.extend(prose.lines().map(|line| format!("> {line}")));
            }
        }

        lines.join("\n")
    }
}

/// Builds the deterministic handoff envelope from live session state.
///
/// `jobs` are the orchestration job snapshots (usually `runtime.jobs(None)`);
/// only queued and running jobs are retained in the envelope.
#[must_use]
pub fn handoff_envelope(session: &Session, jobs: &[JobSnapshot]) -> HandoffEnvelope {
    let goal_state = session.goal_runtime().get();
    let todo_state = session.todo_state();
    let history = session.history();
    let message_count = history.len();

    let mut next_step_hints: Vec<String> = Vec::new();
    if let Some(goal) = goal_state.current.as_ref() {
        if goal.lifecycle == GoalLifecycle::Active {
            next_step_hints.push(redact_secrets(&format!(
                "Continue the active goal: {}",
                goal.objective
            )));
        }
    }
    for phase in &todo_state.phases {
        for item in &phase.tasks {
            if item.status == TodoStatus::InProgress
                && next_step_hints.len() < HANDOFF_MAX_NEXT_STEP_HINTS
            {
                next_step_hints.push(redact_secrets(&format!(
                    "Finish in-progress todo: {}",
                    item.content
                )));
            }
        }
    }

    HandoffEnvelope {
        session_name: session.session_name(),
        session_id: session.recorder_info().map(|(id, _)| id),
        model: session
            .model()
            .map(|model| format!("{}/{}", model.provider, model.id)),
        cwd: session.cwd().display().to_string(),
        git: probe_git_state(session.cwd()),
        goal: goal_state.current.map(|goal| {
            let remaining_tokens = goal.remaining_tokens();
            HandoffGoal {
                objective: redact_secrets(&goal.objective),
                lifecycle: goal_lifecycle_name(&goal.lifecycle),
                remaining_tokens,
            }
        }),
        todo: todo_counts(&todo_state),
        jobs: active_handoff_jobs(jobs),
        recent_user_asks: recent_user_asks(&history, HANDOFF_MAX_RECENT_USER_ASKS)
            .into_iter()
            .map(|ask| redact_secrets(&ask))
            .collect(),
        message_count,
        next_step_hints,
    }
}

#[must_use]
pub fn handoff_prose_prompt(envelope: &HandoffEnvelope, transcript: &str) -> String {
    format!(
        "<conversation>\n{transcript}\n</conversation>\n\nStructured handoff envelope:\n{}\n\n{HANDOFF_PROMPT}",
        envelope_render(envelope)
    )
}

#[must_use]
pub fn todo_counts(todo: &TodoState) -> HandoffTodoCounts {
    let mut counts = HandoffTodoCounts::default();
    for phase in &todo.phases {
        for item in &phase.tasks {
            match item.status {
                TodoStatus::Pending => counts.pending += 1,
                TodoStatus::InProgress => counts.in_progress += 1,
                TodoStatus::Completed => counts.completed += 1,
                TodoStatus::Abandoned => counts.abandoned += 1,
            }
        }
    }
    counts.total = counts.pending + counts.in_progress + counts.completed + counts.abandoned;
    counts
}

#[must_use]
pub fn recent_user_asks(messages: &[Message], max: usize) -> Vec<String> {
    messages
        .iter()
        .rev()
        .filter_map(|message| match message {
            Message::User(user) => {
                let text = user
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
                    .trim()
                    .to_owned();
                (!text.is_empty()).then_some(text)
            }
            _ => None,
        })
        .take(max)
        .collect()
}

#[must_use]
pub fn active_handoff_jobs(snapshots: &[JobSnapshot]) -> Vec<HandoffJob> {
    snapshots
        .iter()
        .filter(|job| !job.status.is_settled())
        .map(HandoffJob::from_snapshot)
        .collect()
}

impl HandoffJob {
    /// Projects one orchestration job snapshot into the handoff envelope.
    /// The description is redacted for credential-shaped text before it
    /// crosses into the rendered handoff (which is copied to the clipboard).
    #[must_use]
    pub fn from_snapshot(snapshot: &JobSnapshot) -> Self {
        Self {
            id: snapshot.id.clone(),
            agent: snapshot.agent.clone(),
            status: snapshot.status,
            description: snapshot
                .description
                .as_deref()
                .map(redact_secrets),
            workflow_id: snapshot.workflow_id.clone(),
        }
    }
}

/// Probes the git state of `cwd`. Returns `None` when `cwd` is not inside a
/// git working tree (or git is unavailable) so the handoff stays well-formed
/// outside repositories.
#[must_use]
pub fn probe_git_state(cwd: &Path) -> Option<HandoffGitState> {
    if !cwd.join(".git").exists() {
        return None;
    }
    let branch = run_git(cwd, &["symbolic-ref", "--short", "HEAD"])
        .or_else(|| run_git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]))
        .map(|line| line.trim().to_owned())
        .filter(|line| !line.is_empty() && line != "HEAD");
    let dirty = run_git(cwd, &["status", "--porcelain=v1", "-z"])
        .is_some_and(|output| !output.is_empty());
    Some(HandoffGitState { branch, dirty })
}

fn run_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn envelope_render(envelope: &HandoffEnvelope) -> String {
    let mut lines: Vec<String> = Vec::new();
    if let Some(goal) = &envelope.goal {
        lines.push(format!("goal: {} ({})", goal.objective, goal.lifecycle));
    }
    let todo = &envelope.todo;
    lines.push(format!(
        "todos: {} pending, {} in progress, {} completed, {} abandoned",
        todo.pending, todo.in_progress, todo.completed, todo.abandoned
    ));
    for job in &envelope.jobs {
        lines.push(format!(
            "job: {} {} ({})",
            job.agent,
            job_status_name(&job.status),
            job.description.as_deref().unwrap_or(&job.id)
        ));
    }
    if let Some(git) = &envelope.git {
        match (&git.branch, git.dirty) {
            (Some(branch), true) => lines.push(format!("git: {branch} (dirty)")),
            (Some(branch), false) => lines.push(format!("git: {branch} (clean)")),
            (None, true) => lines.push("git: detached HEAD (dirty)".to_owned()),
            (None, false) => lines.push("git: detached HEAD".to_owned()),
        }
    }
    if lines.is_empty() {
        "(empty session)".to_owned()
    } else {
        lines.join("\n")
    }
}

fn goal_lifecycle_name(lifecycle: &GoalLifecycle) -> String {
    match lifecycle {
        GoalLifecycle::Active => "active",
        GoalLifecycle::Paused => "paused",
        GoalLifecycle::Completed => "completed",
        GoalLifecycle::Dropped => "dropped",
    }
    .to_owned()
}

fn job_status_name(status: &JobStatus) -> String {
    match status {
        JobStatus::Queued => "queued",
        JobStatus::Running => "running",
        JobStatus::Completed => "completed",
        JobStatus::Failed => "failed",
        JobStatus::Cancelled => "cancelled",
    }
    .to_owned()
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn truncate_line(line: &str, max_chars: usize) -> String {
    let mut chars = line.chars();
    let mut prefix: Vec<char> = Vec::new();
    for ch in chars.by_ref().take(max_chars) {
        prefix.push(ch);
    }
    let mut rendered = prefix.into_iter().collect::<String>();
    if chars.next().is_some() {
        rendered.push('…');
    }
    rendered
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use pi_agent::ThinkingLevel;
    use pi_ai::{
        AssistantMessage, ContentBlock, Model, StopReason, new_assistant_message_event_stream,
    };

    use super::*;
    use crate::{
        JobStatus, SessionOptions, TodoItem, TodoPhase, TodoStatus, orchestration::JobSnapshot,
    };

    fn session_with(cwd: &Path) -> Session {
        Session::new(SessionOptions {
            model: Model {
                id: "handoff-model".to_owned(),
                provider: "faux".to_owned(),
                ..Model::default()
            },
            cwd: cwd.to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
        })
        .expect("session")
    }

    fn job(id: &str, status: JobStatus) -> JobSnapshot {
        JobSnapshot {
            id: id.to_owned(),
            agent_id: id.to_owned(),
            agent: format!("agent-{id}"),
            parent_id: "main".to_owned(),
            description: Some(format!("work for {id}")),
            todo_task_id: None,
            workflow_id: Some("wf-1".to_owned()),
            workflow_generation: None,
            status,
            created_at: 1,
            started_at: None,
            finished_at: None,
            result: None,
            soft_budget_exhausted: false,
        }
    }

    fn fixture(cwd: &Path) -> Session {
        let session = session_with(cwd);
        session
            .goal_runtime()
            .create("Implement handoff summaries", None)
            .expect("goal");
        session
            .set_todos(vec![TodoPhase {
                name: "Implementation".to_owned(),
                tasks: vec![
                    TodoItem {
                        id: String::new(),
                        content: "Build envelope".to_owned(),
                        status: TodoStatus::Completed,
                        depends_on: Vec::new(),
                        ready: false,
                        blocked_by: Vec::new(),
                        agent: None,
                    },
                    TodoItem {
                        id: String::new(),
                        content: "Wire /handoff command".to_owned(),
                        status: TodoStatus::InProgress,
                        depends_on: Vec::new(),
                        ready: false,
                        blocked_by: Vec::new(),
                        agent: None,
                    },
                    TodoItem {
                        id: String::new(),
                        content: "Write tests".to_owned(),
                        status: TodoStatus::Pending,
                        depends_on: Vec::new(),
                        ready: false,
                        blocked_by: Vec::new(),
                        agent: None,
                    },
                ],
            }])
            .expect("todos");
        session
    }

    #[tokio::test]
    async fn envelope_reflects_goal_todo_counts_and_running_jobs() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = fixture(cwd.path());
        session
            .load_history(vec![
                Message::user_text("build handoffs", 1),
                Message::user_text("then wire the command", 2),
            ])
            .await
            .expect("history");
        let jobs = vec![job("j1", JobStatus::Running), job("j2", JobStatus::Queued)];
        // A settled job must be filtered out of the active-jobs section.
        let settled = job("j3", JobStatus::Completed);

        let envelope = handoff_envelope(&session, &[jobs[0].clone(), jobs[1].clone(), settled]);

        let goal = envelope.goal.expect("goal");
        assert_eq!(goal.objective, "Implement handoff summaries");
        assert_eq!(goal.lifecycle, "active");
        assert_eq!(envelope.todo.pending, 1);
        assert_eq!(envelope.todo.in_progress, 1);
        assert_eq!(envelope.todo.completed, 1);
        assert_eq!(envelope.todo.abandoned, 0);
        assert_eq!(envelope.todo.total, 3);
        assert_eq!(envelope.jobs.len(), 2);
        assert_eq!(envelope.jobs[0].id, "j1");
        assert_eq!(envelope.jobs[0].status, JobStatus::Running);
        assert_eq!(envelope.jobs[1].status, JobStatus::Queued);
        assert_eq!(envelope.recent_user_asks, ["then wire the command", "build handoffs"]);
        assert_eq!(envelope.message_count, 2);
        assert_eq!(envelope.model.as_deref(), Some("faux/handoff-model"));
        assert_eq!(envelope.cwd, cwd.path().display().to_string());
        assert!(
            envelope
                .next_step_hints
                .iter()
                .any(|hint| hint.contains("Implement handoff summaries")),
            "active goal must produce a next-step hint: {:?}",
            envelope.next_step_hints
        );
        assert!(
            envelope
                .next_step_hints
                .iter()
                .any(|hint| hint.contains("Wire /handoff command")),
            "in-progress todo must produce a next-step hint: {:?}",
            envelope.next_step_hints
        );
    }

    #[tokio::test]
    async fn handoff_redacts_credential_shaped_text() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = session_with(cwd.path());
        let secret = ["gh", "p_", "ABCDEFGHIJKLMNOPQRSTUVWXYZ123456"].concat();
        session
            .goal_runtime()
            .create(format!("Deploy with token {secret}"), None)
            .expect("goal");
        session
            .load_history(vec![Message::user_text(format!("use {secret}"), 1)])
            .await
            .expect("history");
        let mut job = job("j1", JobStatus::Running);
        job.description = Some(format!("fetch {secret}"));

        let envelope = handoff_envelope(&session, &[job]);
        assert_eq!(
            envelope.goal.as_ref().expect("goal").objective,
            "Deploy with token [REDACTED]"
        );
        assert_eq!(envelope.recent_user_asks[0], "use [REDACTED]");
        assert_eq!(
            envelope.jobs[0].description.as_deref(),
            Some("fetch [REDACTED]")
        );
        assert!(
            envelope
                .next_step_hints
                .iter()
                .all(|hint| !hint.contains(secret.as_str())),
            "next-step hints must be redacted: {:?}",
            envelope.next_step_hints
        );

        let rendered = Handoff {
            envelope: envelope.clone(),
            prose: None,
        }
        .render();
        assert!(
            !rendered.contains(secret.as_str()),
            "rendered handoff must not leak the token:\n{rendered}"
        );
        assert!(rendered.contains("[REDACTED]"));
    }

    #[test]
    fn empty_session_handoff_is_well_formed() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = session_with(cwd.path());
        let handoff = session.generate_handoff(&[]);
        assert!(handoff.envelope.goal.is_none());
        assert_eq!(handoff.envelope.todo.total, 0);
        assert!(handoff.envelope.jobs.is_empty());
        assert!(handoff.envelope.recent_user_asks.is_empty());
        assert!(handoff.envelope.next_step_hints.is_empty());
        assert_eq!(handoff.envelope.message_count, 0);
        assert!(handoff.prose.is_none());

        let rendered = handoff.render();
        for section in [
            "# Handoff",
            "## Goal",
            "## Todos",
            "## Running jobs",
            "## Recent asks",
            "## Next steps",
        ] {
            assert!(rendered.contains(section), "missing {section:?} in:\n{rendered}");
        }
        assert!(rendered.contains("(no goal)"), "empty goal section: {rendered}");
        assert!(rendered.contains("0 pending"), "empty todo counts: {rendered}");
        assert!(rendered.contains("(none)"), "empty sections render (none): {rendered}");
    }

    #[test]
    fn render_includes_goal_todos_jobs_and_asks() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = fixture(cwd.path());
        let handoff = session.generate_handoff(&[job("j1", JobStatus::Running)]);
        let rendered = handoff.render();
        assert!(rendered.contains("# Handoff"));
        assert!(rendered.contains("Implement handoff summaries · active"));
        assert!(rendered.contains("1 pending · 1 in progress · 1 completed · 0 abandoned"));
        assert!(rendered.contains("agent-j1 (running): work for j1"));
        assert!(rendered.contains("- cwd:"));
        assert!(rendered.contains("## Next steps"));
        assert!(rendered.contains("Continue the active goal"));
    }

    #[test]
    fn probe_git_state_requires_a_repository() {
        let cwd = tempfile::tempdir().expect("cwd");
        assert_eq!(probe_git_state(cwd.path()), None, "non-repo must report no git state");

        // A real (uncommitted) file must show up as dirty on a real repo.
        let Ok(version) = Command::new("git").arg("--version").output() else {
            return; // git unavailable — nothing more to assert.
        };
        if !version.status.success() {
            return;
        }
        let repo = tempfile::tempdir().expect("repo");
        let init = Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(repo.path())
            .output()
            .expect("git init");
        assert!(init.status.success(), "git init failed: {init:?}");
        std::fs::write(repo.path().join("file.txt"), "uncommitted\n").expect("write file");
        let state = probe_git_state(repo.path()).expect("git state");
        assert_eq!(state.branch.as_deref(), Some("main"));
        assert!(state.dirty, "uncommitted file must mark the tree dirty");
    }

    #[tokio::test]
    async fn prose_path_uses_the_summarizer_with_a_handoff_prompt() {
        let cwd = tempfile::tempdir().expect("cwd");
        let mut options = SessionOptions {
            model: Model::default(),
            cwd: cwd.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
        };
        let stream_fn: pi_agent::StreamFn = Arc::new(|model, _, _| {
            Box::pin(async move {
                let stream = new_assistant_message_event_stream();
                let writer = stream.clone();
                tokio::spawn(async move {
                    let mut message = AssistantMessage::pending(&model);
                    message.stop_reason = StopReason::Stop;
                    message.content = vec![ContentBlock::text(
                        "Continue by wiring the /handoff command and adding tests.",
                    )];
                    writer.end(Some(message)).await;
                });
                stream
            })
        });
        options.stream_fn = Some(stream_fn);
        let session = Session::new(options).expect("session");
        session
            .goal_runtime()
            .create("Wire handoff", None)
            .expect("goal");

        let handoff = session
            .generate_handoff_with_prose(&[])
            .await
            .expect("handoff prose");
        assert_eq!(
            handoff.prose.as_deref(),
            Some("Continue by wiring the /handoff command and adding tests.")
        );
        assert!(handoff.envelope.goal.is_some());
        let rendered = handoff.render();
        assert!(
            rendered.contains("Continue by wiring the /handoff command"),
            "prose must render inside the block:\n{rendered}"
        );
        assert!(
            rendered.contains("> Continue by wiring"),
            "prose must render as a quoted paragraph:\n{rendered}"
        );
    }

    #[test]
    fn todo_counts_aggregate_across_phases_and_statuses() {
        let mk = |status: TodoStatus| TodoItem {
            id: String::new(),
            content: "task".to_owned(),
            status,
            depends_on: Vec::new(),
            ready: false,
            blocked_by: Vec::new(),
            agent: None,
        };
        let state = TodoState {
            phases: vec![
                TodoPhase {
                    name: "phase one".to_owned(),
                    tasks: vec![mk(TodoStatus::Pending), mk(TodoStatus::Completed)],
                },
                TodoPhase {
                    name: "phase two".to_owned(),
                    tasks: vec![
                        mk(TodoStatus::InProgress),
                        mk(TodoStatus::Abandoned),
                        mk(TodoStatus::Pending),
                    ],
                },
            ],
            storage: crate::TodoStorage::Memory,
        };
        let counts = todo_counts(&state);
        assert_eq!(counts.pending, 2);
        assert_eq!(counts.in_progress, 1);
        assert_eq!(counts.completed, 1);
        assert_eq!(counts.abandoned, 1);
        assert_eq!(counts.total, 5, "total must be the sum of all statuses");
        // An empty todo state reports all-zero counts.
        let empty = todo_counts(&TodoState {
            phases: Vec::new(),
            storage: crate::TodoStorage::Memory,
        });
        assert_eq!(empty.total, 0);
    }

    #[test]
    fn recent_user_asks_extracts_newest_first_skips_empty_and_non_user() {
        let messages = vec![
            pi_ai::Message::user_text("first question", 0),
            pi_ai::Message::user_text("   ", 1), // whitespace-only -> skipped
            pi_ai::Message::user_text("second question", 2),
            // A non-user message must not appear.
            {
                let mut am = pi_ai::AssistantMessage::pending(&Model::default());
                am.content.push(ContentBlock::Text {
                    text: "assistant answer".to_owned(),
                    text_signature: None,
                });
                pi_ai::Message::Assistant(am)
            },
            pi_ai::Message::user_text("third question", 3),
        ];
        // Newest first, bounded by max.
        let asks = recent_user_asks(&messages, 2);
        assert_eq!(asks, vec!["third question", "second question"]);
        // max=0 returns nothing.
        assert!(recent_user_asks(&messages, 0).is_empty());
        // No user messages -> empty.
        let all_assistant = vec![{
            let mut am = pi_ai::AssistantMessage::pending(&Model::default());
            am.content.push(ContentBlock::Text {
                text: "answer".to_owned(),
                text_signature: None,
            });
            pi_ai::Message::Assistant(am)
        }];
        assert!(recent_user_asks(&all_assistant, 5).is_empty());
    }

    #[test]
    fn active_handoff_jobs_keeps_queued_and_running_drops_settled() {
        let snapshots = vec![
            job("queued", JobStatus::Queued),
            job("running", JobStatus::Running),
            job("done", JobStatus::Completed),
            job("failed", JobStatus::Failed),
            job("cancelled", JobStatus::Cancelled),
        ];
        let active = active_handoff_jobs(&snapshots);
        assert_eq!(active.len(), 2, "only queued and running jobs are active");
        assert_eq!(active[0].id, "queued");
        assert_eq!(active[1].id, "running");
        // Each active job projects the snapshot identity, agent, and status.
        assert_eq!(active[0].agent, "agent-queued");
        assert_eq!(active[0].status, JobStatus::Queued);
        assert_eq!(active[0].workflow_id.as_deref(), Some("wf-1"));
    }

    #[test]
    fn handoff_job_from_snapshot_redacts_credential_shaped_description() {
        let secret = ["gh", "p_", "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij"].concat();
        let mut snapshot = job("leak", JobStatus::Running);
        snapshot.description = Some(format!("deploy using token {secret}"));
        let projected = HandoffJob::from_snapshot(&snapshot);
        assert!(
            !projected
                .description
                .as_deref()
                .unwrap_or_default()
                .contains(secret.as_str()),
            "credential-shaped description must be redacted before crossing into the handoff"
        );
    }

    #[test]
    fn handoff_prose_prompt_wraps_transcript_and_envelope_and_instructions() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = session_with(cwd.path());
        let envelope = handoff_envelope(&session, &[]);
        let prompt = handoff_prose_prompt(&envelope, "user asked to ship it");
        // The transcript is wrapped verbatim.
        assert!(prompt.contains("<conversation>\nuser asked to ship it\n</conversation>"));
        // The structured envelope section is present.
        assert!(prompt.contains("Structured handoff envelope:"));
        // The handoff instructions constant is appended verbatim.
        assert!(prompt.contains(HANDOFF_PROMPT));
    }
}
