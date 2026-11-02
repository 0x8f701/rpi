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

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use pi_ai::{ContentBlock, Message};
use serde::{Deserialize, Serialize};

use crate::application::generation::ApplicationGeneration;
use crate::process::{
    GovernedProcessContext, GovernedProcessSpec, TrustedToolProbe, run_trusted_tool_probe,
};
use crate::redact::redact_secrets;
use crate::resources::utf16_len;
use crate::{
    Goal, GoalLifecycle, GoalPauseReason, GoalRuntime, JobSnapshot, JobStatus, Session,
    SessionTree, TOOL_RESULT_MAX_CHARS, TodoItem, TodoState, TodoStatus,
    goal_events_from_session_tree, messages_as_llm, serialize_conversation,
};

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

/// Clipboard / RPC format token for a saved-session copy-context artifact.
pub const SESSION_CONTEXT_FORMAT: &str = "pi-session-context/1";
/// Hard UTF-8 byte ceiling for [`SavedSessionContext::text`].
pub const SESSION_CONTEXT_MAX_BYTES: usize = 65_536;
const SESSION_CONTEXT_SECTION_MARKER: &str = "[... section truncated ...]";
const SESSION_CONTEXT_CAP_SESSION: usize = 2 * 1024;
const SESSION_CONTEXT_CAP_MODEL: usize = 2 * 1024;
const SESSION_CONTEXT_CAP_GOAL: usize = 4 * 1024;
const SESSION_CONTEXT_CAP_TODO: usize = 12 * 1024;
const SESSION_CONTEXT_CAP_NEXT: usize = 6 * 1024;

/// Bounded, sanitized pasteable snapshot of a saved session tree.
///
/// Built entirely from [`SessionTree`] — no live `Session`, git probe, jobs,
/// or provider call. Live [`Handoff`] rendering is unchanged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedSessionContext {
    pub text: String,
    pub truncated: bool,
}

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
    handoff_envelope_with_git(session, jobs, None)
}

pub(crate) fn handoff_envelope_with_generation(
    generation: Arc<ApplicationGeneration>,
    session: &Session,
    jobs: &[JobSnapshot],
) -> HandoffEnvelope {
    let git = probe_git_state(generation, session.cwd());
    handoff_envelope_with_git(session, jobs, git)
}

fn handoff_envelope_with_git(
    session: &Session,
    jobs: &[JobSnapshot],
    git: Option<HandoffGitState>,
) -> HandoffEnvelope {
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
        git,
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
            description: snapshot.description.as_deref().map(redact_secrets),
            workflow_id: snapshot.workflow_id.clone(),
        }
    }
}

/// Probes the git state of `cwd`. Returns `None` when `cwd` is not inside a
/// git working tree (or git is unavailable) so the handoff stays well-formed
/// outside repositories.
#[must_use]
pub fn probe_git_state(
    generation: Arc<ApplicationGeneration>,
    cwd: &Path,
) -> Option<HandoffGitState> {
    let git = run_trusted_tool_probe(&generation, TrustedToolProbe::Git).ok()?;
    let branch = run_git(
        generation.clone(),
        git.as_path(),
        cwd,
        &["symbolic-ref", "--short", "HEAD"],
    )
    .or_else(|| {
        run_git(
            generation.clone(),
            git.as_path(),
            cwd,
            &["rev-parse", "--abbrev-ref", "HEAD"],
        )
    })
    .map(|line| line.trim().to_owned())
    .filter(|line| !line.is_empty() && line != "HEAD");
    let dirty = run_git(
        generation,
        git.as_path(),
        cwd,
        &["status", "--porcelain=v1", "-z"],
    )
    .is_some_and(|output| !output.is_empty());
    Some(HandoffGitState { branch, dirty })
}

fn run_git(
    generation: Arc<ApplicationGeneration>,
    git: &Path,
    cwd: &Path,
    args: &[&str],
) -> Option<String> {
    let output = GovernedProcessContext::new(generation, None)
        .run_blocking(
            pi_sandbox::SpawnFamily::HandoffHelper,
            GovernedProcessSpec {
                program: git.to_path_buf(),
                args: args.iter().map(|arg| (*arg).to_owned()).collect(),
                cwd: cwd.to_path_buf(),
                environment: BTreeMap::new(),
                timeout: Duration::from_secs(5),
                stdout_bytes: 64 * 1024,
                stderr_bytes: 16 * 1024,
                #[cfg(unix)]
                mount_sources: Vec::new(),
                #[cfg(unix)]
                retained_executable: None,
                #[cfg(unix)]
                retained_working_directory: None,
            },
        )
        .ok()?;
    (output.status == Some(0) && !output.timed_out && !output.cancelled)
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

/// Renders a versioned, bounded, sanitized copy-context artifact from a
/// saved session tree. Fails closed when the goal journal cannot be replayed.
pub fn saved_session_context_from_tree(tree: &SessionTree) -> Result<SavedSessionContext> {
    let events = goal_events_from_session_tree(tree).map_err(|error| anyhow!("{error}"))?;
    let goal_runtime = GoalRuntime::from_events(&events).map_err(|error| anyhow!("{error}"))?;
    let current_goal = goal_runtime.get().current;
    let todo = tree.latest_todo_state().unwrap_or_default();
    let context = tree.build_context(None);

    let session_body = render_saved_session_section(tree.session_name().as_deref());
    let model_body = render_saved_model_section(
        context.provider.as_deref(),
        context.model_id.as_deref(),
        &context.thinking_level,
    );
    let goal_body = render_saved_goal_section(current_goal.as_ref());
    let todo_body = render_saved_todo_section(&todo);
    let llm_messages = messages_as_llm(&context.messages);
    let conversation = serialize_conversation(&llm_messages);
    let tool_result_omitted = conversation_tool_results_truncated(&llm_messages);
    let context_body = if conversation.is_empty() {
        "(none)".to_owned()
    } else {
        sanitize_session_context_text(&conversation)
    };
    let next_body = render_saved_next_steps(current_goal.as_ref(), &todo);

    let (session_body, session_cut) = bound_section(&session_body, SESSION_CONTEXT_CAP_SESSION);
    let (model_body, model_cut) = bound_section(&model_body, SESSION_CONTEXT_CAP_MODEL);
    let (goal_body, goal_cut) = bound_section(&goal_body, SESSION_CONTEXT_CAP_GOAL);
    let (todo_body, todo_cut) = bound_section(&todo_body, SESSION_CONTEXT_CAP_TODO);
    let (next_body, next_cut) = bound_section(&next_body, SESSION_CONTEXT_CAP_NEXT);

    let prefix_yes = saved_context_prefix(true, &session_body, &model_body, &goal_body, &todo_body);
    let suffix = format!("\n\n## Next steps\n{next_body}");
    let context_budget =
        SESSION_CONTEXT_MAX_BYTES.saturating_sub(prefix_yes.len().saturating_add(suffix.len()));
    let (mut context_body, context_cut) = bound_section(&context_body, context_budget);
    if tool_result_omitted && !context_cut {
        context_body = mark_section_truncated(&context_body, context_budget);
    }
    let truncated =
        session_cut || model_cut || goal_cut || todo_cut || next_cut || context_cut || tool_result_omitted;
    let mut text = format!(
        "{}{context_body}{suffix}",
        saved_context_prefix(truncated, &session_body, &model_body, &goal_body, &todo_body)
    );
    if text.len() > SESSION_CONTEXT_MAX_BYTES {
        let overflow = text.len() - SESSION_CONTEXT_MAX_BYTES;
        let tighter = context_budget.saturating_sub(overflow);
        let (context_body, _) = bound_section(
            context_body
                .strip_suffix(SESSION_CONTEXT_SECTION_MARKER)
                .unwrap_or(&context_body),
            tighter,
        );
        text = format!(
            "{}{context_body}{suffix}",
            saved_context_prefix(true, &session_body, &model_body, &goal_body, &todo_body)
        );
        return Ok(SavedSessionContext {
            text: clamp_utf8(text, SESSION_CONTEXT_MAX_BYTES),
            truncated: true,
        });
    }
    Ok(SavedSessionContext { text, truncated })
}

fn saved_context_prefix(
    truncated: bool,
    session_body: &str,
    model_body: &str,
    goal_body: &str,
    todo_body: &str,
) -> String {
    format!(
        "# Pi session context\nFormat: {SESSION_CONTEXT_FORMAT}\nTruncated: {}\n\n## Session\n{session_body}\n\n## Model\n{model_body}\n\n## Goal\n{goal_body}\n\n## Todo\n{todo_body}\n\n## Model context\n",
        if truncated { "yes" } else { "no" }
    )
}

fn render_saved_session_section(name: Option<&str>) -> String {
    match name.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => format!("Name: {}", sanitize_session_context_text(name)),
        None => "(unnamed)".to_owned(),
    }
}

fn render_saved_model_section(
    provider: Option<&str>,
    model_id: Option<&str>,
    thinking_level: &str,
) -> String {
    format!(
        "Provider: {}\nModel: {}\nThinking: {}",
        sanitize_session_context_text(provider.unwrap_or("(unknown)")),
        sanitize_session_context_text(model_id.unwrap_or("(unknown)")),
        sanitize_session_context_text(if thinking_level.is_empty() {
            "off"
        } else {
            thinking_level
        })
    )
}

fn render_saved_goal_section(goal: Option<&Goal>) -> String {
    let Some(goal) = goal else {
        return "(none)".to_owned();
    };
    let mut lines = vec![
        format!(
            "Objective: {}",
            sanitize_session_context_text(&goal.objective)
        ),
        format!("Lifecycle: {}", goal_lifecycle_name(&goal.lifecycle)),
    ];
    if let Some(reason) = goal.pause_reason {
        lines.push(format!("Paused reason: {}", goal_pause_reason_name(reason)));
    }
    if let Some(budget) = goal.token_budget {
        lines.push(format!("Token budget: {budget}"));
        lines.push(format!("Tokens used: {}", goal.usage.tokens_used));
        lines.push(format!(
            "Remaining tokens: {}",
            goal.remaining_tokens().unwrap_or(0)
        ));
    }
    if !goal.pins.is_empty() {
        lines.push("Pins:".to_owned());
        for pin in &goal.pins {
            lines.push(format!("- {}", sanitize_session_context_text(pin)));
        }
    }
    lines.join("\n")
}

fn goal_pause_reason_name(reason: GoalPauseReason) -> &'static str {
    match reason {
        GoalPauseReason::Manual => "manual",
        GoalPauseReason::BudgetExhausted => "budget_exhausted",
        GoalPauseReason::ResumeSafety => "resume_safety",
    }
}

fn render_saved_todo_section(todo: &TodoState) -> String {
    if todo.phases.is_empty() {
        return "(none)".to_owned();
    }
    let mut content_by_id: BTreeMap<&str, &str> = BTreeMap::new();
    for phase in &todo.phases {
        for task in &phase.tasks {
            if !task.id.is_empty() {
                content_by_id.insert(task.id.as_str(), task.content.as_str());
            }
        }
    }
    let mut lines = Vec::new();
    for phase in &todo.phases {
        lines.push(format!(
            "### {}",
            sanitize_session_context_text(&phase.name)
        ));
        if phase.tasks.is_empty() {
            lines.push("(no tasks)".to_owned());
            continue;
        }
        for task in &phase.tasks {
            let mut line = format!(
                "- [{}] {} · ready={}",
                saved_todo_status_name(task.status),
                sanitize_session_context_text(&task.content),
                if task.ready { "yes" } else { "no" }
            );
            if let Some(agent) = task.agent.as_deref().map(str::trim).filter(|a| !a.is_empty())
            {
                line.push_str(" · agent=");
                line.push_str(&sanitize_session_context_text(agent));
            }
            lines.push(line);
            if !task.depends_on.is_empty() {
                let deps: Vec<String> = task
                    .depends_on
                    .iter()
                    .map(|id| {
                        sanitize_session_context_text(
                            content_by_id.get(id.as_str()).copied().unwrap_or(id.as_str()),
                        )
                    })
                    .collect();
                lines.push(format!("  depends on: {}", deps.join("; ")));
            }
            if !task.waiting_on.is_empty() {
                let waiting: Vec<String> = task
                    .waiting_on
                    .iter()
                    .map(|item| sanitize_session_context_text(&item.content))
                    .collect();
                lines.push(format!("  waiting on: {}", waiting.join("; ")));
            }
        }
    }
    lines.join("\n")
}

fn saved_todo_status_name(status: TodoStatus) -> &'static str {
    match status {
        TodoStatus::Pending => "pending",
        TodoStatus::InProgress => "in_progress",
        TodoStatus::Completed => "completed",
        TodoStatus::Abandoned => "abandoned",
    }
}

fn render_saved_next_steps(goal: Option<&Goal>, todo: &TodoState) -> String {
    let mut hints: Vec<String> = Vec::new();
    if let Some(goal) = goal
        && goal.lifecycle == GoalLifecycle::Active
    {
        hints.push(format!(
            "Continue the active goal: {}",
            sanitize_session_context_text(&goal.objective)
        ));
    }
    for phase in &todo.phases {
        for item in &phase.tasks {
            if item.status == TodoStatus::InProgress {
                hints.push(format!(
                    "Finish in-progress todo: {}",
                    sanitize_session_context_text(&item.content)
                ));
            }
        }
    }
    for phase in &todo.phases {
        for item in &phase.tasks {
            if item.status == TodoStatus::Pending && pending_todo_is_startable(item) {
                hints.push(format!(
                    "Start pending todo: {}",
                    sanitize_session_context_text(&item.content)
                ));
            }
        }
    }
    if hints.is_empty() {
        "(none)".to_owned()
    } else {
        hints
            .into_iter()
            .map(|hint| format!("- {hint}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn pending_todo_is_startable(item: &TodoItem) -> bool {
    item.ready && item.waiting_on.is_empty() && item.depends_on.is_empty()
}

fn conversation_tool_results_truncated(messages: &[Message]) -> bool {
    messages.iter().any(|message| match message {
        Message::ToolResult(result) => {
            let text: String = result
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            utf16_len(&text) > TOOL_RESULT_MAX_CHARS
        }
        _ => false,
    })
}

fn mark_section_truncated(body: &str, cap: usize) -> String {
    let marker = SESSION_CONTEXT_SECTION_MARKER;
    if body.contains(marker) {
        return clamp_utf8(body.to_owned(), cap);
    }
    if body.len() + 1 + marker.len() <= cap {
        let mut out = body.to_owned();
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(marker);
        return out;
    }
    bound_section(body, cap).0
}

fn bound_section(body: &str, cap: usize) -> (String, bool) {
    if body.len() <= cap {
        return (body.to_owned(), false);
    }
    let marker = SESSION_CONTEXT_SECTION_MARKER;
    if cap < marker.len() {
        return (clamp_utf8(marker.to_owned(), cap), true);
    }
    let prefix_budget = cap - marker.len();
    let with_newline = prefix_budget.saturating_sub(1);
    let prefix = truncate_utf8_bytes(body, with_newline);
    let mut out = prefix.to_owned();
    if !out.ends_with('\n') && prefix.len() + 1 + marker.len() <= cap {
        out.push('\n');
    } else if prefix.len() + marker.len() > cap {
        out = truncate_utf8_bytes(body, prefix_budget).to_owned();
    }
    out.push_str(marker);
    (clamp_utf8(out, cap), true)
}

fn truncate_utf8_bytes(input: &str, max_bytes: usize) -> &str {
    if input.len() <= max_bytes {
        return input;
    }
    let mut end = max_bytes;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    &input[..end]
}

fn clamp_utf8(mut text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    text.truncate(truncate_utf8_bytes(&text, max_bytes).len());
    text
}

/// Local sanitizer for copy-context text: strip ANSI/control, redact secrets,
/// then replace absolute Unix/Windows/UNC paths with `[PATH]` while keeping
/// `http://` and `https://` URLs intact.
fn sanitize_session_context_text(input: &str) -> String {
    replace_absolute_paths(&redact_secrets(&strip_ansi_and_controls(input)))
}

fn strip_ansi_and_controls(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\n' | '\t' => out.push(ch),
            '\u{1B}' => skip_esc_sequence(&mut chars),
            '\u{9B}' => skip_until_csi_final(&mut chars),
            '\u{9D}' => skip_osc_payload(&mut chars),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

fn skip_esc_sequence(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.peek().copied() {
        Some('[') => {
            chars.next();
            skip_until_csi_final(chars);
        }
        Some(']') => {
            chars.next();
            skip_osc_payload(chars);
        }
        Some(c) if ('\u{20}'..='\u{2F}').contains(&c) => {
            while matches!(chars.peek(), Some(c) if ('\u{20}'..='\u{2F}').contains(c)) {
                chars.next();
            }
            let _ = chars.next();
        }
        Some(_) => {
            let _ = chars.next();
        }
        None => {}
    }
}

fn skip_until_csi_final(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    for ch in chars.by_ref() {
        if ('\u{40}'..='\u{7E}').contains(&ch) {
            break;
        }
    }
}

fn skip_osc_payload(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(ch) = chars.next() {
        if ch == '\u{07}' || ch == '\u{9C}' {
            break;
        }
        if ch == '\u{1B}' && chars.peek() == Some(&'\\') {
            chars.next();
            break;
        }
    }
}

fn replace_absolute_paths(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        if is_path_start(&chars, i) && starts_with_http_url(&chars, i) {
            let start = i;
            i += http_scheme_len(&chars, i);
            while i < chars.len() && is_path_body_char(chars[i]) {
                i += 1;
            }
            for ch in &chars[start..i] {
                out.push(*ch);
            }
            continue;
        }
        if is_path_start(&chars, i) && is_absolute_path_at(&chars, i) {
            i += absolute_path_len(&chars, i);
            out.push_str("[PATH]");
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn is_path_start(chars: &[char], i: usize) -> bool {
    i == 0 || is_path_boundary(chars[i - 1])
}

fn is_path_boundary(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            '"' | '\''
                | '`'
                | '='
                | ':'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '<'
                | '>'
                | ','
                | ';'
                | '|'
        )
}

fn is_path_body_char(ch: char) -> bool {
    !ch.is_whitespace()
        && !matches!(
            ch,
            '"' | '\''
                | '`'
                | '<'
                | '>'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | ','
                | ';'
                | '|'
        )
}

fn starts_with_http_url(chars: &[char], i: usize) -> bool {
    http_scheme_len(chars, i) > 0
}

fn http_scheme_len(chars: &[char], i: usize) -> usize {
    const HTTP: &[char] = &['h', 't', 't', 'p', ':', '/', '/'];
    const HTTPS: &[char] = &['h', 't', 't', 'p', 's', ':', '/', '/'];
    if remaining_eq_ignore_ascii(chars, i, HTTPS) {
        HTTPS.len()
    } else if remaining_eq_ignore_ascii(chars, i, HTTP) {
        HTTP.len()
    } else {
        0
    }
}

fn remaining_eq_ignore_ascii(chars: &[char], i: usize, needle: &[char]) -> bool {
    if i + needle.len() > chars.len() {
        return false;
    }
    chars[i..i + needle.len()]
        .iter()
        .zip(needle)
        .all(|(got, want)| got.eq_ignore_ascii_case(want))
}

fn is_absolute_path_at(chars: &[char], i: usize) -> bool {
    windows_drive_len(chars, i) > 0 || unc_len(chars, i) > 0 || unix_abs_len(chars, i) > 0
}

fn absolute_path_len(chars: &[char], i: usize) -> usize {
    let start = i;
    let prefix = windows_drive_len(chars, i)
        .max(unc_len(chars, i))
        .max(unix_abs_len(chars, i));
    let mut i = i + prefix;
    while i < chars.len() && is_path_body_char(chars[i]) {
        i += 1;
    }
    i - start
}

fn windows_drive_len(chars: &[char], i: usize) -> usize {
    if i + 2 < chars.len()
        && chars[i].is_ascii_alphabetic()
        && chars[i + 1] == ':'
        && (chars[i + 2] == '\\' || chars[i + 2] == '/')
    {
        3
    } else {
        0
    }
}

fn unc_len(chars: &[char], i: usize) -> usize {
    if i + 1 < chars.len()
        && ((chars[i] == '\\' && chars[i + 1] == '\\') || (chars[i] == '/' && chars[i + 1] == '/'))
    {
        2
    } else {
        0
    }
}

fn unix_abs_len(chars: &[char], i: usize) -> usize {
    if chars[i] == '/' {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pi_agent::ThinkingLevel;
    use pi_ai::{
        AssistantMessage, ContentBlock, Model, StopReason, ToolCall, ToolResultMessage,
        new_assistant_message_event_stream,
    };
    use serde_json::json;

    use super::*;
    use crate::{
        Application, GOAL_SESSION_CUSTOM_TYPE, GOAL_SESSION_ENTRY_VERSION, GoalRuntime,
        GoalUsageDelta, JobStatus, ResourceDiscovery, ResourceManager, ResourceManagerOptions,
        SessionOptions, TOOL_RESULT_MAX_CHARS, TodoItem, TodoPhase, TodoState, TodoStatus,
        TodoStorage, ToolSelection, orchestration::JobSnapshot, start_session_in,
    };

    fn session_with_model_and_stream(
        cwd: &Path,
        model: Model,
        stream_fn: Option<pi_agent::StreamFn>,
    ) -> Session {
        Session::new_with_additional_tools_filtered_and_discovery(
            SessionOptions {
                model,
                cwd: cwd.to_path_buf(),
                system_prompt: String::new(),
                thinking_level: ThinkingLevel::Off,
                api_key: String::new(),
                compaction: None,
                stream_options: Default::default(),
                tools: Some(Vec::new()),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn,
                auth_resolver: None,
                auth_manager: None,
            },
            Vec::new(),
            ToolSelection::default(),
            ResourceDiscovery::Disabled,
        )
        .expect("session")
    }

    fn session_with_options(cwd: &Path, stream_fn: Option<pi_agent::StreamFn>) -> Session {
        session_with_model_and_stream(
            cwd,
            Model {
                id: "handoff-model".to_owned(),
                provider: "faux".to_owned(),
                ..Model::default()
            },
            stream_fn,
        )
    }

    fn session_with(cwd: &Path) -> Session {
        session_with_options(cwd, None)
    }

    async fn application_with(session: Session, agent_dir: &Path) -> Application {
        std::fs::create_dir_all(agent_dir).expect("agent directory");
        let settings_path = agent_dir.join("settings.json");
        if !settings_path.exists() {
            std::fs::write(
                &settings_path,
                r#"{"sandbox":{"mode":"danger-full-access"}}"#,
            )
            .expect("sandbox settings");
        }
        let session_dir = agent_dir.join("sessions");
        std::fs::create_dir_all(&session_dir).expect("session directory");
        session.set_session_dir(session_dir);
        let mut options = ResourceManagerOptions::new(session.cwd());
        options.agent_dir = agent_dir.to_path_buf();
        options.project_trust_override = Some(true);
        options.disable_extensions = true;
        options.disable_skills = true;
        options.disable_prompt_templates = true;
        options.disable_themes = true;
        options.disable_context_files = true;
        session
            .attach_resources(ResourceManager::new(options).expect("resources"))
            .await
            .expect("attach resources");
        Application::new(session).await
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
                        waiting_on: Vec::new(),
                        agent: None,
                    },
                    TodoItem {
                        id: String::new(),
                        content: "Wire /handoff command".to_owned(),
                        status: TodoStatus::InProgress,
                        depends_on: Vec::new(),
                        ready: false,
                        waiting_on: Vec::new(),
                        agent: None,
                    },
                    TodoItem {
                        id: String::new(),
                        content: "Write tests".to_owned(),
                        status: TodoStatus::Pending,
                        depends_on: Vec::new(),
                        ready: false,
                        waiting_on: Vec::new(),
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
        assert_eq!(
            envelope.recent_user_asks,
            ["then wire the command", "build handoffs"]
        );
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
    fn governed_handoff_family_is_exact() {
        assert_eq!(
            pi_sandbox::SpawnFamily::HandoffHelper.as_str(),
            "handoff-helper"
        );
    }

    #[test]
    fn empty_session_handoff_is_well_formed() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = session_with(cwd.path());
        let handoff = Handoff {
            envelope: handoff_envelope(&session, &[]),
            prose: None,
        };
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
            assert!(
                rendered.contains(section),
                "missing {section:?} in:\n{rendered}"
            );
        }
        assert!(
            rendered.contains("(no goal)"),
            "empty goal section: {rendered}"
        );
        assert!(
            rendered.contains("0 pending"),
            "empty todo counts: {rendered}"
        );
        assert!(
            rendered.contains("(none)"),
            "empty sections render (none): {rendered}"
        );
    }

    #[test]
    fn render_includes_goal_todos_jobs_and_asks() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = fixture(cwd.path());
        let handoff = Handoff {
            envelope: handoff_envelope(&session, &[job("j1", JobStatus::Running)]),
            prose: None,
        };
        let rendered = handoff.render();
        assert!(rendered.contains("# Handoff"));
        assert!(rendered.contains("Implement handoff summaries · active"));
        assert!(rendered.contains("1 pending · 1 in progress · 1 completed · 0 abandoned"));
        assert!(rendered.contains("agent-j1 (running): work for j1"));
        assert!(rendered.contains("- cwd:"));
        assert!(rendered.contains("## Next steps"));
        assert!(rendered.contains("Continue the active goal"));
    }

    #[tokio::test]
    async fn prose_path_uses_the_summarizer_with_a_handoff_prompt() {
        let cwd = tempfile::tempdir().expect("cwd");
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
        let faux = pi_ai::providers::register_faux_provider(Default::default());
        let session = session_with_model_and_stream(
            cwd.path(),
            faux.model(None).expect("faux model"),
            Some(stream_fn),
        );
        let agent_dir = tempfile::tempdir().expect("agent directory");
        let application = application_with(session, agent_dir.path()).await;
        let session = application.session();
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
            waiting_on: Vec::new(),
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

    fn saved_tree_recorder(label: &str) -> (tempfile::TempDir, crate::SessionRecorder) {
        let root = tempfile::tempdir().expect("saved context root");
        let cwd = root.path().join("project");
        let sessions = root.path().join("sessions");
        std::fs::create_dir_all(&cwd).expect("cwd");
        std::fs::create_dir_all(&sessions).expect("sessions");
        let recorder = start_session_in(
            &cwd,
            None,
            None,
            Some(&sessions),
            Some(label),
            Some(&sessions.join("parent.jsonl")),
        )
        .expect("recorder");
        (root, recorder)
    }

    fn persist_goal(recorder: &crate::SessionRecorder, bind: impl FnOnce(&GoalRuntime)) {
        let runtime = GoalRuntime::from_session_recorder(recorder.clone()).expect("goal runtime");
        bind(&runtime);
    }

    fn assistant_with(
        thinking: &str,
        text: &str,
        tool_name: &str,
        args: serde_json::Value,
    ) -> AssistantMessage {
        let mut message = AssistantMessage::pending(&Model {
            id: "opus".to_owned(),
            provider: "anthropic".to_owned(),
            ..Model::default()
        });
        message.stop_reason = StopReason::ToolUse;
        message.content = vec![
            ContentBlock::thinking(thinking),
            ContentBlock::text(text),
            ContentBlock::ToolCall(ToolCall {
                id: "call-1".to_owned(),
                name: tool_name.to_owned(),
                arguments: args,
                thought_signature: None,
            }),
        ];
        message
    }

    #[test]
    fn saved_session_context_preserves_thinking_tool_args_and_results() {
        let (_root, recorder) = saved_tree_recorder("ctx-fidelity");
        recorder
            .record_message(&Message::user_text("inspect /tmp/secret.env please", 1))
            .expect("user");
        recorder
            .record_message(&Message::Assistant(assistant_with(
                "I will read the env file first.",
                "Calling read.",
                "read",
                json!({"path": "/tmp/secret.env"}),
            )))
            .expect("assistant");
        recorder
            .record_message(&Message::ToolResult(ToolResultMessage {
                tool_call_id: "call-1".to_owned(),
                tool_name: "read".to_owned(),
                content: vec![ContentBlock::text("KEY=ok")],
                usage: None,
                details: None,
                added_tool_names: Vec::new(),
                is_error: false,
                timestamp: 3,
            }))
            .expect("tool result");
        let context =
            saved_session_context_from_tree(&recorder.tree().expect("tree")).expect("render");
        assert!(
            context
                .text
                .starts_with("# Pi session context\nFormat: pi-session-context/1\nTruncated: no\n")
        );
        assert!(context.text.contains("[Assistant thinking]: I will read the env file first."));
        assert!(context.text.contains("[Assistant]: Calling read."));
        assert!(context.text.contains("[Assistant tool calls]: read("));
        assert!(context.text.contains("path="));
        assert!(context.text.contains("[Tool result]: KEY=ok"));
        assert!(!context.truncated);
        assert!(context.text.len() <= SESSION_CONTEXT_MAX_BYTES);
    }

    #[test]
    fn saved_session_context_includes_goal_todo_model_thinking_and_next_steps() {
        let (_root, recorder) = saved_tree_recorder("ctx-state");
        recorder
            .record_session_name("Copy context work")
            .expect("name");
        recorder
            .record_model_change("anthropic", "opus")
            .expect("model");
        recorder
            .record_thinking_level("high")
            .expect("thinking");
        persist_goal(&recorder, |runtime| {
            runtime
                .create("Ship copy context", Some(1_000))
                .expect("goal");
            runtime.pin("Prefer explicit truncation markers").expect("pin");
            runtime
                .update_usage(GoalUsageDelta::new(250, 0))
                .expect("usage");
        });
        recorder
            .record_todo_snapshot(&TodoState {
                phases: vec![TodoPhase {
                    name: "Implementation".to_owned(),
                    tasks: vec![
                        TodoItem {
                            id: "t1".to_owned(),
                            content: "Build renderer".to_owned(),
                            status: TodoStatus::Completed,
                            depends_on: Vec::new(),
                            ready: true,
                            waiting_on: Vec::new(),
                            agent: None,
                        },
                        TodoItem {
                            id: "t2".to_owned(),
                            content: "Write tests".to_owned(),
                            status: TodoStatus::InProgress,
                            depends_on: vec!["t1".to_owned()],
                            ready: true,
                            waiting_on: Vec::new(),
                            agent: Some("qa".to_owned()),
                        },
                        TodoItem {
                            id: "t3".to_owned(),
                            content: "Wire RPC".to_owned(),
                            status: TodoStatus::Pending,
                            depends_on: vec!["t2".to_owned()],
                            ready: false,
                            waiting_on: Vec::new(),
                            agent: Some("deepseek".to_owned()),
                        },
                    ],
                }],
                storage: TodoStorage::Session,
            })
            .expect("todo");
        recorder
            .record_message(&Message::user_text("keep going", 1))
            .expect("user");
        let context =
            saved_session_context_from_tree(&recorder.tree().expect("tree")).expect("render");
        assert!(context.text.contains("## Session\nName: Copy context work"));
        assert!(context.text.contains("Provider: anthropic"));
        assert!(context.text.contains("Model: opus"));
        assert!(context.text.contains("Thinking: high"));
        assert!(context.text.contains("Objective: Ship copy context"));
        assert!(context.text.contains("Lifecycle: active"));
        assert!(context.text.contains("Token budget: 1000"));
        assert!(context.text.contains("Tokens used: 250"));
        assert!(context.text.contains("Remaining tokens: 750"));
        assert!(context.text.contains("Pins:"));
        assert!(context.text.contains("- Prefer explicit truncation markers"));
        assert!(context.text.contains("### Implementation"));
        assert!(context.text.contains("[completed] Build renderer · ready=no"));
        assert!(context.text.contains("[in_progress] Write tests · ready=yes · agent=qa"));
        assert!(context.text.contains("depends on: Build renderer"));
        assert!(context.text.contains("waiting on: Write tests"));
        assert!(context.text.contains("[pending] Wire RPC · ready=no · agent=deepseek"));
        assert!(context.text.contains("- Continue the active goal: Ship copy context"));
        assert!(context.text.contains("- Finish in-progress todo: Write tests"));
        assert!(!context.text.contains("Start pending todo: Wire RPC"));
        assert!(!context.text.contains("ctx-state"));
        assert!(!context.text.contains("parent.jsonl"));
        assert!(!context.truncated);
    }

    #[test]
    fn saved_session_context_redacts_secrets_paths_and_controls_but_keeps_urls() {
        let (_root, recorder) = saved_tree_recorder("ctx-sanitize");
        let secret = ["gh", "p_", "ABCDEFGHIJKLMNOPQRSTUVWXYZ123456"].concat();
        recorder
            .record_session_name("Alpha")
            .expect("name");
        persist_goal(&recorder, |runtime| {
            runtime
                .create(
                    format!("Read {secret} from /home/u/secret.env and https://example.com/docs"),
                    None,
                )
                .expect("goal");
        });
        recorder
            .record_todo_snapshot(&TodoState {
                phases: vec![TodoPhase {
                    name: "Secrets".to_owned(),
                    tasks: vec![TodoItem {
                        id: "s1".to_owned(),
                        content: format!(
                            "Open C:\\\\Users\\\\operator\\\\file.txt and \\\\server\\share\\x with {secret}"
                        ),
                        status: TodoStatus::Pending,
                        depends_on: Vec::new(),
                        ready: true,
                        waiting_on: Vec::new(),
                        agent: None,
                    }],
                }],
                storage: TodoStorage::Session,
            })
            .expect("todo");
        recorder
            .record_message(&Message::user_text(
                format!(
                    "see https://example.com/a and `/tmp/secret.env` plus ```\n/etc/passwd\n``` and file:///etc/passwd\u{1B}[31mred\u{1B}[0m {secret}"
                ),
                1,
            ))
            .expect("user");
        let context =
            saved_session_context_from_tree(&recorder.tree().expect("tree")).expect("render");
        assert!(
            !context.text.contains(secret.as_str()),
            "secret must be redacted: {}",
            context.text
        );
        assert!(context.text.contains("[REDACTED]"));
        assert!(context.text.contains("https://example.com/docs"));
        assert!(context.text.contains("https://example.com/a"));
        assert!(!context.text.contains("/home/u/secret.env"));
        assert!(!context.text.contains("/tmp/secret.env"));
        assert!(!context.text.contains("/etc/passwd"));
        assert!(!context.text.contains("C:\\Users\\operator\\file.txt"));
        assert!(!context.text.contains("\\\\server\\share\\x"));
        assert!(context.text.contains("[PATH]"));
        assert!(!context.text.contains('\u{1B}'));
    }

    #[test]
    fn saved_session_context_truncates_with_markers_and_utf8_bound() {
        let (_root, recorder) = saved_tree_recorder("ctx-trunc");
        let bulky = "目标 ".repeat(40_000);
        recorder
            .record_message(&Message::user_text(bulky, 1))
            .expect("user");
        let context =
            saved_session_context_from_tree(&recorder.tree().expect("tree")).expect("render");
        assert!(context.truncated);
        assert!(context.text.contains("Truncated: yes"));
        assert!(context.text.contains("## Model context"));
        assert!(context.text.contains(SESSION_CONTEXT_SECTION_MARKER));
        assert!(context.text.contains("## Next steps"));
        assert!(context.text.len() <= SESSION_CONTEXT_MAX_BYTES);
        assert!(std::str::from_utf8(context.text.as_bytes()).is_ok());
        assert_eq!(
            context.text.chars().last().map(|ch| ch.len_utf8() > 0),
            Some(true)
        );
    }

    #[test]
    fn saved_session_context_empty_tree_is_well_formed() {
        let (_root, recorder) = saved_tree_recorder("ctx-empty");
        let context =
            saved_session_context_from_tree(&recorder.tree().expect("tree")).expect("render");
        assert!(!context.truncated);
        assert!(context.text.contains("Truncated: no"));
        assert!(context.text.contains("## Session\n(unnamed)"));
        assert!(context.text.contains("Thinking: off"));
        assert!(context.text.contains("## Goal\n(none)"));
        assert!(context.text.contains("## Todo\n(none)"));
        assert!(context.text.contains("## Model context\n(none)"));
        assert!(context.text.contains("## Next steps\n(none)"));
        assert!(!context.text.contains("cwd"));
        assert!(!context.text.contains("parent.jsonl"));
    }

    #[test]
    fn sanitize_session_context_text_is_deterministic() {
        let input = "see https://ex.com/x and /var/log/syslog\u{07}";
        assert_eq!(
            sanitize_session_context_text(input),
            sanitize_session_context_text(input)
        );
        assert_eq!(
            sanitize_session_context_text(input),
            "see https://ex.com/x and [PATH]"
        );
    }

    #[test]
    fn saved_session_context_marks_serialized_tool_result_omission() {
        let (_root, recorder) = saved_tree_recorder("ctx-tool-omit");
        let bulky = "x".repeat((TOOL_RESULT_MAX_CHARS as usize) + 80);
        recorder
            .record_message(&Message::user_text("dump the file", 1))
            .expect("user");
        recorder
            .record_message(&Message::ToolResult(ToolResultMessage {
                tool_call_id: "call-1".to_owned(),
                tool_name: "read".to_owned(),
                content: vec![ContentBlock::text(bulky)],
                usage: None,
                details: None,
                added_tool_names: Vec::new(),
                is_error: false,
                timestamp: 2,
            }))
            .expect("tool result");
        let context =
            saved_session_context_from_tree(&recorder.tree().expect("tree")).expect("render");
        assert!(context.truncated);
        assert!(context.text.contains("Truncated: yes"));
        assert!(context.text.contains("## Model context"));
        assert!(context.text.contains(SESSION_CONTEXT_SECTION_MARKER));
        assert!(context.text.contains("[Tool result]:"));
        assert!(context.text.contains("more characters truncated"));
    }

    #[test]
    fn saved_session_context_malformed_goal_fails_closed() {
        let (_root, recorder) = saved_tree_recorder("ctx-bad-goal");
        recorder
            .record_custom_entry(
                GOAL_SESSION_CUSTOM_TYPE,
                Some(json!({
                    "version": GOAL_SESSION_ENTRY_VERSION,
                    "event": { "not": "a goal event" },
                })),
            )
            .expect("malformed goal");
        let error = saved_session_context_from_tree(&recorder.tree().expect("tree"))
            .expect_err("malformed goal must fail closed");
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn saved_session_context_redacts_backtick_and_fence_paths_independently() {
        assert_eq!(
            sanitize_session_context_text("see `/tmp/secret.env` and https://example.com/a"),
            "see `[PATH]` and https://example.com/a"
        );
        assert_eq!(
            sanitize_session_context_text("```\n/etc/passwd\n```"),
            "```\n[PATH]\n```"
        );
        assert_eq!(
            sanitize_session_context_text("C:\\Windows\\System32\\cmd.exe"),
            "[PATH]"
        );
        assert_eq!(
            sanitize_session_context_text("\\\\server\\share\\file.txt"),
            "[PATH]"
        );
        assert!(
            !sanitize_session_context_text("relative/path.txt and tmp/secret.env")
                .contains("[PATH]")
        );
    }

    #[test]
    fn saved_session_context_multibyte_bound_does_not_split_utf8() {
        let (_root, recorder) = saved_tree_recorder("ctx-multibyte");
        let bulky = "🎯".repeat(40_000);
        recorder
            .record_message(&Message::user_text(bulky, 1))
            .expect("user");
        let context =
            saved_session_context_from_tree(&recorder.tree().expect("tree")).expect("render");
        assert!(context.truncated);
        assert!(context.text.contains("Truncated: yes"));
        assert!(context.text.contains(SESSION_CONTEXT_SECTION_MARKER));
        assert!(context.text.len() <= SESSION_CONTEXT_MAX_BYTES);
        assert!(std::str::from_utf8(context.text.as_bytes()).is_ok());
        assert!(
            !context.text.as_bytes()[context.text.len().saturating_sub(3)..]
                .iter()
                .any(|&byte| (0x80..0xC0).contains(&byte)
                    && context
                        .text
                        .as_bytes()
                        .get(context.text.len().saturating_sub(4))
                        .is_none_or(|lead| *lead < 0x80))
        );
    }

    #[test]
    fn saved_session_context_paused_goal_includes_reason_and_skips_continue() {
        let (_root, recorder) = saved_tree_recorder("ctx-paused");
        persist_goal(&recorder, |runtime| {
            runtime
                .create("Hold the line", Some(500))
                .expect("goal");
            runtime.pause().expect("pause");
        });
        let context =
            saved_session_context_from_tree(&recorder.tree().expect("tree")).expect("render");
        assert!(context.text.contains("Lifecycle: paused"));
        assert!(context.text.contains("Paused reason: manual"));
        assert!(context.text.contains("Token budget: 500"));
        assert!(context.text.contains("Tokens used: 0"));
        assert!(context.text.contains("Remaining tokens: 500"));
        assert!(!context.text.contains("Continue the active goal"));
    }


}
