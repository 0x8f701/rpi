//! Workflow-scoped supervisor actor over a worktree-bound `Application`.
//!
//! This module expects canonical parent-module types:
//! `WorkflowStatus` with queued/planning/running/paused/integrating/completed/
//! failed/cancelled/conflicted variants, and `WorkflowTaskOwnership` with
//! explicit `workflow_id` and `todo_task_id` fields.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::{
    ApplicationEvent, JobStatus, OrchestrationConcurrencyGate, OrchestrationEvent,
    TodoApplyResult, TodoDagExecutionOutcome, TodoDagExecutionStatus, TodoOp, TodoState,
    WorkflowRuntimeScope,
};

use super::{WorkflowStatus, WorkflowTaskOwnership};

const SUPERVISOR_CHANNEL_CAPACITY: usize = 64;
/// Bound on the live activity feed kept in the supervisor projection.
const SUPERVISOR_ACTIVITY_CAP: usize = 12;
/// Bound on a single activity entry's display text.
const SUPERVISOR_ACTIVITY_TEXT_CAP: usize = 240;
/// Maximum assistant turns in one bounded planning prompt (P0-1). The inner
/// agent run is unbounded at the session layer, so the workflow imposes its
/// own budget: a correcting model must never keep the workflow in Planning by
/// looping forever. When the budget is reached the workflow settles from the
/// canonical Todo state instead of waiting for the run to end.
const PLANNING_MAX_TURNS: usize = 8;
/// Maximum Todo/tool calls in one bounded planning prompt (P0-1).
const PLANNING_MAX_TOOL_CALLS: usize = 16;
/// Default wall-clock budget for one planning prompt (P0-1). The actor aborts
/// the in-flight prompt when the budget expires and settles from the
/// canonical Todo state.
const PLANNING_DEFAULT_DEADLINE: Duration = Duration::from_secs(90);
/// Identical failed Todo operations with no Todo-state change that terminate
/// planning as non-converging (P1-2).
const PLANNING_IDENTICAL_FAILED_OP_LIMIT: usize = 3;
/// Total Todo corrections with no Todo-state change that terminate planning
/// (P1-2). The counter tolerates varying IDs/error text that the session
/// doom-loop detector cannot see.
const PLANNING_CORRECTIONS_WITHOUT_PROGRESS_LIMIT: usize = 6;

/// Typed outcome of one bounded planning prompt (P0-1). Replaces the
/// collapsed `Result<()>` so the supervisor can distinguish "the run ended
/// naturally" from "a bound stopped it while a usable DAG was already
/// committed" and act accordingly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanningTurnOutcome {
    /// The run ended naturally with no remaining tool calls.
    Completed,
    /// The run was terminated because a valid plan was committed — the first
    /// successful `todo init` — even though the model still had turns to
    /// emit (P0-2). Planning ends on the committed plan, not on run end.
    PlanCommitted,
    /// The run was stopped by the planning budget: assistant turns, tool
    /// calls, or semantic non-progress detection. `reason` names the bound.
    PlanBudgetReached { reason: String },
    /// The run was aborted by the wall-clock deadline.
    TimedOut,
}

/// Semantic fingerprint of one completed `todo` tool call during planning,
/// forwarded alongside the display activity so the supervisor can detect
/// non-progress loops that raw activity text cannot express (P1-2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowSupervisorTodoObservation {
    /// Normalized Todo operation name (`init`, `update_dependencies`, ...),
    /// when the call arguments could be parsed.
    pub op: Option<String>,
    /// Target task/dependency IDs named by the call (`task` + `dependsOn`,
    /// plus `init` items) in argument order.
    pub target_ids: Vec<String>,
    pub is_error: bool,
    /// Normalized, bounded error prefix for failed calls (`None` on success).
    pub error_prefix: Option<String>,
}

/// One bounded entry of the supervisor's live activity feed (coalesced
/// thinking chunks, tool calls, IRC progress) projected into the workflow
/// page so planning never reads as a static spinner. Live-only: the durable
/// workflow record never persists activity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowSupervisorActivity {
    /// Epoch millis when the activity was observed.
    pub at_ms: u64,
    pub kind: WorkflowSupervisorActivityKind,
    /// Bounded, credential-redacted display text.
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowSupervisorActivityKind {
    /// A coalesced model thinking chunk (collapsed in the UI, thinking color).
    Thinking,
    /// A coalesced reply-text chunk while the supervisor drafts its turn.
    Text,
    /// A tool call the supervisor started (`read tools.rs`, `bash …`).
    Tool,
    /// A delivered IRC progress message.
    Irc,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|value| u64::try_from(value.as_millis()).ok())
        .unwrap_or(0)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowSupervisorContract {
    pub workflow_id: String,
    pub generation: u64,
    pub worktree_label: String,
    pub objective: String,
    pub supervisor_agent_id: String,
    pub max_concurrency: usize,
}

impl WorkflowSupervisorContract {
    fn validate(&self) -> Result<()> {
        if self.workflow_id.trim().is_empty() {
            bail!("workflow id must not be empty");
        }
        if self.worktree_label.trim().is_empty() {
            bail!("workflow worktree label must not be empty");
        }
        if self.objective.trim().is_empty() {
            bail!("workflow objective must not be empty");
        }
        if self.supervisor_agent_id.trim().is_empty() {
            bail!("workflow supervisor agent id must not be empty");
        }
        if self.max_concurrency == 0 {
            bail!("workflow max concurrency must be greater than zero");
        }
        Ok(())
    }

    #[must_use]
    pub fn initial_prompt(&self) -> String {
        format!(
            "You plan workflow {workflow_id}. Objective: {objective}\n\
             Work only in the current workflow Application cwd. During this planning phase, create the complete canonical Todo DAG \
             exactly once with the todo tool, then stop. Do not delegate workers, modify files, wait on jobs, or keep refining \
             after the plan is accepted. Use only task IDs returned by the todo tool (every task has a stable todoTaskId). \
             If the objective assigns work to a specific agent by name, PRESERVE that role as a typed routing contract: \
             pass an `agents` array in the matching todo init phase entry, parallel to `items` (agents[i] names the agent that \
             must execute items[i]), and also keep the agent's name in that task's content. Only use agents that are defined and \
             enabled in this workflow; do not invent agent names. \
             Every Todo task must be a single concise line of at most ~60 characters: a terse imperative title \
             (e.g. \"Bootstrap pi-zig sources\"), never a paragraph. The task content is the title the panel shows — \
             longer titles are truncated — so keep detail out of the title; split work into more granular tasks when needed. \
             Prefer a WIDE, parallel DAG: keep independent work in separate tasks with no dependency edges, because the executor \
             runs every ready task concurrently up to the configured concurrency limit. Reserve depends_on for genuine data or \
             control dependencies (a task consumes another's output, or must not start before it); never chain unrelated steps. \
             Aim for several ready tasks per execution wave so independent branches progress in parallel. \
             If the todo tool rejects the plan, make at most one corrected call; otherwise report the reason you cannot plan.",
            workflow_id = self.workflow_id,
            objective = self.objective,
        )
    }

    /// Bounded continuation prompt for a planning turn that produced no Todo
    /// tasks. Exactly one re-prompt is allowed before the workflow fails, so
    /// a model that answered the first turn with plain text gets one chance
    /// to build the plan instead of leaving the workflow stuck in Planning.
    #[must_use]
    pub fn replan_prompt(&self) -> String {
        format!(
            "The planning turn for workflow {workflow_id} produced no Todo tasks. \
             This is the final attempt: call the todo tool now to create the complete canonical Todo DAG exactly once, then stop. \
             Do not delegate workers, modify files, or keep refining after the plan is accepted. \
             Use only task IDs returned by the todo tool. \
             Give every Todo task a single concise title of at most ~60 characters — a terse imperative phrase \
             (e.g. \"Bootstrap pi-zig sources\"), never a paragraph: the title is what the panel shows and \
             longer titles are truncated, so keep detail out of the title. \
             Prefer a WIDE, parallel DAG: independent work stays in separate tasks with no dependency edges — the executor \
             runs every ready task concurrently up to its limit, so reserve depends_on for genuine data or control dependencies \
             and aim for several ready tasks per execution wave. \
             If the plan is rejected, make at most one corrected call; \
             otherwise report the reason you cannot plan.",
            workflow_id = self.workflow_id,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowSupervisorProjection {
    pub workflow_id: String,
    pub generation: u64,
    pub status: WorkflowStatus,
    pub supervisor_agent_id: String,
    pub todo: TodoState,
    pub jobs: Vec<crate::WorkflowJobSnapshot>,
    pub irc: Vec<crate::MailboxMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    /// Bounded live activity feed (newest-last), so the workflow page can
    /// project the supervisor's own planning turn (thinking chunks, tool
    /// calls, IRC progress) instead of a static "planning" spinner.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activity: Vec<WorkflowSupervisorActivity>,
    /// Epoch millis when the current Planning phase started (None otherwise).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planning_started_at_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowSupervisorEvent {
    Started {
        workflow_id: String,
        generation: u64,
    },
    StatusChanged {
        workflow_id: String,
        generation: u64,
        status: WorkflowStatus,
    },
    ProjectionChanged {
        projection: WorkflowSupervisorProjection,
    },
    IrcDelivered {
        workflow_id: String,
        generation: u64,
        message: crate::MailboxMessage,
    },
    Failed {
        workflow_id: String,
        generation: u64,
        message: String,
    },
}

#[async_trait]
pub trait WorkflowSupervisorBackend: Send + Sync + 'static {
    fn todo_state(&self) -> TodoState;
    fn todo_dag_status(&self) -> TodoDagExecutionStatus;
    fn workflow_jobs(&self, workflow_id: &str, generation: u64) -> Vec<crate::WorkflowJobSnapshot>;
    fn inbox(&self, agent_id: &str, peek: bool) -> Vec<crate::MailboxMessage>;
    fn active_workflow_job_ids(&self, workflow_id: &str, generation: u64) -> Vec<String>;
    fn configure_workflow_runtime(
        &self,
        scope: WorkflowRuntimeScope,
        max_concurrency: usize,
        global_concurrency: OrchestrationConcurrencyGate,
    ) -> Result<()>;
    async fn prompt_supervisor(&self, prompt: String) -> Result<PlanningTurnOutcome>;
    async fn steer_supervisor(&self, message: String) -> Result<()>;
    /// P0-C: validate the objective's explicit agent references against the
    /// workflow child catalog before any planning prompt runs. The default is
    /// a no-op so lightweight test backends keep working.
    fn validate_objective_agents(&self, _objective: &str) -> Result<()> {
        Ok(())
    }
    fn execute_todo_dag(&self) -> Result<TodoDagExecutionOutcome>;
    /// Wall-clock budget for one planning prompt (P0-1). The actor aborts the
    /// in-flight prompt when the budget expires and settles from the
    /// canonical Todo state.
    fn planning_deadline(&self) -> Duration {
        PLANNING_DEFAULT_DEADLINE
    }
    /// Apply a canonical Todo operation through the workflow child. Used by
    /// the supervisor's execution-supervision to mark delegated tasks Done
    /// once their worker jobs complete (the DAG coordinator must never forge
    /// workflow-owned completion, BUG-3).
    fn apply_todo(&self, op: TodoOp) -> Result<TodoApplyResult>;
    /// Re-evaluate an already armed Todo DAG (settle when every task is done,
    /// spawn newly ready dependents) without creating new execution intent.
    fn reconcile_todo_dag(&self) -> Result<TodoDagExecutionOutcome>;
    async fn pause(&self) -> Result<()>;
    async fn resume(&self) -> Result<TodoDagExecutionOutcome>;
    async fn cancel_jobs(&self, ids: &[String]) -> Result<Vec<String>>;
}

#[derive(Clone)]
pub struct WorkflowSupervisor {
    contract: WorkflowSupervisorContract,
    commands: mpsc::Sender<SupervisorCommand>,
    projection: Arc<Mutex<WorkflowSupervisorProjection>>,
    events: broadcast::Sender<WorkflowSupervisorEvent>,
}

impl WorkflowSupervisor {
    pub fn spawn<B>(
        contract: WorkflowSupervisorContract,
        backend: Arc<B>,
        global_concurrency: OrchestrationConcurrencyGate,
    ) -> Result<Self>
    where
        B: WorkflowSupervisorBackend,
    {
        Self::spawn_with_status(
            contract,
            backend,
            global_concurrency,
            WorkflowStatus::Queued,
        )
    }

    pub fn spawn_restored<B>(
        contract: WorkflowSupervisorContract,
        backend: Arc<B>,
        global_concurrency: OrchestrationConcurrencyGate,
        status: WorkflowStatus,
    ) -> Result<Self>
    where
        B: WorkflowSupervisorBackend,
    {
        if !matches!(
            status,
            WorkflowStatus::Queued
                | WorkflowStatus::Planning
                | WorkflowStatus::Running
                | WorkflowStatus::Paused
        ) {
            bail!("workflow supervisor cannot restore lifecycle status {status}");
        }
        Self::spawn_with_status(contract, backend, global_concurrency, status)
    }

    fn spawn_with_status<B>(
        contract: WorkflowSupervisorContract,
        backend: Arc<B>,
        global_concurrency: OrchestrationConcurrencyGate,
        status: WorkflowStatus,
    ) -> Result<Self>
    where
        B: WorkflowSupervisorBackend,
    {
        contract.validate()?;
        backend.configure_workflow_runtime(
            WorkflowRuntimeScope {
                workflow_id: contract.workflow_id.clone(),
                generation: contract.generation,
            },
            contract.max_concurrency,
            global_concurrency,
        )?;
        let projection = Arc::new(Mutex::new(project_backend(
            &contract,
            backend.as_ref(),
            status,
            None,
            &[],
            (status == WorkflowStatus::Planning).then(now_ms),
        )));
        let (commands, receiver) = mpsc::channel(SUPERVISOR_CHANNEL_CAPACITY);
        let (events, _) = broadcast::channel(SUPERVISOR_CHANNEL_CAPACITY);
        let actor = SupervisorActor {
            contract: contract.clone(),
            backend,
            projection: projection.clone(),
            events: events.clone(),
            status,
            failure: None,
            planning_in_flight: false,
            plan_committed: false,
            suppress_abort_failure: false,
            planning_non_progress_tripped: false,
            planning_non_progress_reason: None,
            last_todo_state_hash: None,
            last_failed_todo_fingerprint: None,
            identical_failed_ops: 0,
            corrections_without_progress: 0,
            activity: Vec::new(),
            planning_started_at_ms: (status == WorkflowStatus::Planning).then(now_ms),
        };
        tokio::spawn(actor.run(receiver));
        Ok(Self {
            contract,
            commands,
            projection,
            events,
        })
    }

    #[must_use]
    pub fn contract(&self) -> &WorkflowSupervisorContract {
        &self.contract
    }

    #[must_use]
    pub fn projection(&self) -> WorkflowSupervisorProjection {
        self.projection.lock().clone()
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<WorkflowSupervisorEvent> {
        self.events.subscribe()
    }

    pub async fn start(&self) -> Result<()> {
        self.request(SupervisorRequest::Start).await.map(|_| ())
    }

    pub async fn pause(&self) -> Result<()> {
        self.request(SupervisorRequest::Pause).await.map(|_| ())
    }

    pub async fn resume(&self) -> Result<()> {
        self.request(SupervisorRequest::Resume).await.map(|_| ())
    }

    /// Continue a durably restored (non-Paused) workflow runtime so it never
    /// comes back frozen. Restored `Planning`/`Queued` workflows resume the
    /// planning flow; restored `Running` workflows re-arm Todo DAG execution
    /// over the restored tasks. Restored `Paused` workflows stay paused until
    /// an explicit [`Self::resume`].
    pub async fn continue_restored(&self) -> Result<()> {
        self.request(SupervisorRequest::RestoreContinue)
            .await
            .map(|_| ())
    }

    pub async fn cancel(&self) -> Result<Vec<String>> {
        self.request(SupervisorRequest::Cancel).await
    }

    pub async fn steer(&self, message: impl Into<String>) -> Result<()> {
        self.request(SupervisorRequest::Steer(message.into()))
            .await
            .map(|_| ())
    }

    pub async fn observe_application_event(
        &self,
        generation: u64,
        event: ApplicationEvent,
    ) -> Result<()> {
        self.request(SupervisorRequest::ApplicationEvent { generation, event })
            .await
            .map(|_| ())
    }

    /// Deliver a coalesced activity chunk from the supervisor's own turn.
    /// Bounded by the actor (kept live-only; never persisted).
    pub async fn observe_activity(
        &self,
        generation: u64,
        kind: WorkflowSupervisorActivityKind,
        text: impl Into<String>,
    ) -> Result<()> {
        self.request(SupervisorRequest::Activity {
            generation,
            kind,
            text: text.into(),
        })
        .await
        .map(|_| ())
    }

    /// Deliver a semantic `todo` tool observation from the supervisor's own
    /// turn (operation, target IDs, error prefix). The actor uses it for
    /// planning non-progress detection; it never persists.
    pub async fn observe_todo_observation(
        &self,
        generation: u64,
        observation: WorkflowSupervisorTodoObservation,
    ) -> Result<()> {
        self.request(SupervisorRequest::TodoObservation {
            generation,
            observation,
        })
        .await
        .map(|_| ())
    }

    async fn request(&self, request: SupervisorRequest) -> Result<Vec<String>> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(SupervisorCommand { request, reply })
            .await
            .map_err(|_| anyhow!("workflow supervisor actor stopped"))?;
        response
            .await
            .map_err(|_| anyhow!("workflow supervisor actor dropped response"))?
    }
}

struct SupervisorCommand {
    request: SupervisorRequest,
    reply: oneshot::Sender<Result<Vec<String>>>,
}

enum SupervisorRequest {
    Start,
    Pause,
    Resume,
    RestoreContinue,
    Cancel,
    Steer(String),
    ApplicationEvent {
        generation: u64,
        event: ApplicationEvent,
    },
    /// Coalesced live activity from the supervisor's own turn (thinking
    /// chunks, tool calls). Delivered by the application forwarder so the
    /// workflow page sees real movement while planning.
    Activity {
        generation: u64,
        kind: WorkflowSupervisorActivityKind,
        text: String,
    },
    /// Semantic `todo` tool observation from the supervisor's own turn,
    /// delivered by the application forwarder for planning non-progress
    /// detection (P1-2).
    TodoObservation {
        generation: u64,
        observation: WorkflowSupervisorTodoObservation,
    },
}

struct SupervisorActor<B> {
    contract: WorkflowSupervisorContract,
    backend: Arc<B>,
    projection: Arc<Mutex<WorkflowSupervisorProjection>>,
    events: broadcast::Sender<WorkflowSupervisorEvent>,
    status: WorkflowStatus,
    failure: Option<String>,
    /// True while a planning prompt is awaited. During that window the actor
    /// still drains `ApplicationEvent` commands (so Todo mutations made by
    /// the planning turn refresh the projection live) but keeps the workflow
    /// status at `Planning` until the turn completes.
    planning_in_flight: bool,
    /// True once the first successful `todo init` of the current planning
    /// phase was observed (P0-2). A committed plan ends planning: the
    /// workflow moves to Running immediately while the model's run finishes
    /// its short grace.
    plan_committed: bool,
    /// Set when the actor aborts the in-flight planning prompt itself (user
    /// cancel, wall-clock deadline, non-progress detection). The abort
    /// publishes an `ApplicationEvent::RunFailed` as a side effect of
    /// cancelling the provider stream; that self-inflicted failure must never
    /// fail (or clobber) the workflow, so the next `RunFailed` is swallowed.
    suppress_abort_failure: bool,
    /// Set when planning non-progress detection fires; the awaiting
    /// `await_planning_turn` aborts the run and settles with the reason.
    planning_non_progress_tripped: bool,
    planning_non_progress_reason: Option<String>,
    /// Hash of the canonical Todo state at the last observed Todo tool call
    /// (the "before" state of the next call). Todo-state change is the
    /// decisive non-progress signal (P1-2).
    last_todo_state_hash: Option<u64>,
    /// Fingerprint of the last failed Todo operation (op + target IDs +
    /// error prefix). Consecutive identical fingerprints with no Todo-state
    /// change trip planning non-progress detection.
    last_failed_todo_fingerprint: Option<String>,
    /// Consecutive identical failed Todo operations without Todo progress.
    identical_failed_ops: usize,
    /// Total failed Todo corrections without Todo progress in the current
    /// planning turn (tolerates varying IDs/error text).
    corrections_without_progress: usize,
    /// Bounded live activity feed (newest-last) for the workflow page.
    activity: Vec<WorkflowSupervisorActivity>,
    /// Epoch millis when the current Planning phase started (None otherwise).
    planning_started_at_ms: Option<u64>,
}

impl<B> SupervisorActor<B>
where
    B: WorkflowSupervisorBackend,
{
    async fn run(mut self, mut commands: mpsc::Receiver<SupervisorCommand>) {
        while let Some(command) = commands.recv().await {
            let (result, deferred) = self.handle(command.request, &mut commands).await;
            let _ = command.reply.send(result);
            // Requests that arrived while a planning turn was awaited are
            // processed only after the turn finishes (and after the calling
            // request, e.g. Start, has fully completed), preserving the
            // serial-actor ordering of the pre-concurrency design.
            for command in deferred {
                let (result, nested) = self.handle(command.request, &mut commands).await;
                let _ = command.reply.send(result);
                self.flush_deferred(nested, &mut commands).await;
            }
        }
    }

    async fn handle(
        &mut self,
        request: SupervisorRequest,
        commands: &mut mpsc::Receiver<SupervisorCommand>,
    ) -> (Result<Vec<String>>, Vec<SupervisorCommand>) {
        match request {
            SupervisorRequest::Start => {
                if self.status != WorkflowStatus::Queued {
                    return (Ok(Vec::new()), Vec::new());
                }
                self.set_status(WorkflowStatus::Planning);
                let deferred = match self.run_planning_phase(commands).await {
                    Ok(deferred) => deferred,
                    Err(error) => return (self.fail(error), Vec::new()),
                };
                // A user cancel that interrupted planning already landed the
                // workflow in Cancelled; never clobber it with a backend read.
                self.settle_status();
                if !is_terminal(self.status) {
                    let _ = self.events.send(WorkflowSupervisorEvent::Started {
                        workflow_id: self.contract.workflow_id.clone(),
                        generation: self.contract.generation,
                    });
                }
                (Ok(Vec::new()), deferred)
            }
            SupervisorRequest::RestoreContinue => {
                match self.status {
                    WorkflowStatus::Queued => {
                        // A durably restored workflow that never started runs
                        // the full planning start flow.
                        self.set_status(WorkflowStatus::Planning);
                        let deferred = match self.run_planning_phase(commands).await {
                            Ok(deferred) => deferred,
                            Err(error) => return (self.fail(error), Vec::new()),
                        };
                        self.settle_status();
                        if !is_terminal(self.status) {
                            let _ = self.events.send(WorkflowSupervisorEvent::Started {
                                workflow_id: self.contract.workflow_id.clone(),
                                generation: self.contract.generation,
                            });
                        }
                        (Ok(Vec::new()), deferred)
                    }
                    WorkflowStatus::Planning => {
                        let deferred = match self.run_planning_phase(commands).await {
                            Ok(deferred) => deferred,
                            Err(error) => return (self.fail(error), Vec::new()),
                        };
                        self.settle_status();
                        (Ok(Vec::new()), deferred)
                    }
                    WorkflowStatus::Running => {
                        let deferred = match self.continue_execution(commands).await {
                            Ok(deferred) => deferred,
                            Err(error) => return (self.fail(error), Vec::new()),
                        };
                        self.settle_status();
                        (Ok(Vec::new()), deferred)
                    }
                    // Restored Paused workflows stay paused until an explicit
                    // resume; terminal statuses cannot be restored.
                    WorkflowStatus::Paused | _ => (Ok(Vec::new()), Vec::new()),
                }
            }
            SupervisorRequest::Pause => {
                if is_terminal(self.status) || self.status == WorkflowStatus::Paused {
                    return (Ok(Vec::new()), Vec::new());
                }
                if let Err(error) = self.backend.pause().await {
                    return (Err(error), Vec::new());
                }
                self.set_status(WorkflowStatus::Paused);
                (Ok(Vec::new()), Vec::new())
            }
            SupervisorRequest::Resume => {
                match self.status {
                    WorkflowStatus::Paused => {
                        if self.todo_has_tasks() {
                            if let Err(error) = self.backend.resume().await {
                                return (Err(error), Vec::new());
                            }
                            self.set_status(self.status_from_backend());
                            (Ok(Vec::new()), Vec::new())
                        } else {
                            let deferred = match self.run_planning_phase(commands).await {
                                Ok(deferred) => deferred,
                                Err(error) => return (self.fail(error), Vec::new()),
                            };
                            self.settle_status();
                            (Ok(Vec::new()), deferred)
                        }
                    }
                    // Resume is effective for restored (or live) Planning and
                    // Running workflows: Planning continues the bounded
                    // planning flow, Running re-arms Todo DAG execution.
                    WorkflowStatus::Planning => {
                        let deferred = match self.run_planning_phase(commands).await {
                            Ok(deferred) => deferred,
                            Err(error) => return (self.fail(error), Vec::new()),
                        };
                        self.settle_status();
                        (Ok(Vec::new()), deferred)
                    }
                    WorkflowStatus::Running => {
                        let deferred = match self.continue_execution(commands).await {
                            Ok(deferred) => deferred,
                            Err(error) => return (self.fail(error), Vec::new()),
                        };
                        self.settle_status();
                        (Ok(Vec::new()), deferred)
                    }
                    _ => (Ok(Vec::new()), Vec::new()),
                }
            }
            SupervisorRequest::Cancel => {
                if self.status == WorkflowStatus::Cancelled || is_terminal(self.status) {
                    return (Ok(Vec::new()), Vec::new());
                }
                let ids = self.backend.active_workflow_job_ids(
                    &self.contract.workflow_id,
                    self.contract.generation,
                );
                let cancelled = match self.backend.cancel_jobs(&ids).await {
                    Ok(cancelled) => cancelled,
                    Err(error) => return (Err(error), Vec::new()),
                };
                self.set_status(WorkflowStatus::Cancelled);
                (Ok(cancelled), Vec::new())
            }
            SupervisorRequest::Activity {
                generation,
                kind,
                text,
            } => {
                if generation != self.contract.generation || is_terminal(self.status) {
                    return (Ok(Vec::new()), Vec::new());
                }
                self.push_activity(kind, text);
                (Ok(Vec::new()), Vec::new())
            }
            SupervisorRequest::TodoObservation {
                generation,
                observation,
            } => {
                if generation != self.contract.generation || is_terminal(self.status) {
                    return (Ok(Vec::new()), Vec::new());
                }
                // Semantic observations only mean something while a planning
                // prompt is in flight; stray deliveries outside planning
                // carry no non-progress signal.
                if self.planning_in_flight {
                    self.record_planning_observation(&observation);
                }
                (Ok(Vec::new()), Vec::new())
            }
            SupervisorRequest::Steer(message) => {
                if is_terminal(self.status) {
                    return (
                        Err(anyhow!("cannot steer terminal workflow supervisor")),
                        Vec::new(),
                    );
                }
                if let Err(error) = self.backend.steer_supervisor(message).await {
                    return (Err(error), Vec::new());
                }
                self.refresh();
                (Ok(Vec::new()), Vec::new())
            }
            SupervisorRequest::ApplicationEvent { generation, event } => {
                if generation != self.contract.generation || is_terminal(self.status) {
                    return (Ok(Vec::new()), Vec::new());
                }
                match &event {
                    ApplicationEvent::RunFailed { message } => {
                        if self.suppress_abort_failure {
                            // The failure is the side effect of an abort the
                            // actor itself requested (user cancel, wall-clock
                            // deadline, non-progress detection): never fail
                            // the workflow for its own interrupt.
                            self.suppress_abort_failure = false;
                            return (Ok(Vec::new()), Vec::new());
                        }
                        return (self.fail(anyhow!(message.clone())), Vec::new());
                    }
                    ApplicationEvent::Orchestration(OrchestrationEvent::JobUpdated {
                        job,
                        ..
                    }) if job
                        .workflow_id
                        .as_deref()
                        .is_some_and(|workflow_id| workflow_id != self.contract.workflow_id)
                        || job.workflow_generation.is_some_and(|job_generation| {
                            job_generation != self.contract.generation
                        }) =>
                    {
                        return (Ok(Vec::new()), Vec::new());
                    }
                    ApplicationEvent::Orchestration(OrchestrationEvent::JobUpdated {
                        job,
                        ..
                    }) => {
                        // Execution-supervision under the plan/execution phase
                        // split: planning ends when the plan is committed, so
                        // worker completion must settle the DAG. The
                        // supervisor owns the canonical Todo DAG and marks
                        // each delegated task Done once its worker job
                        // completes (the DAG coordinator must never forge
                        // workflow-owned completion — BUG-3). Reconcile so a
                        // fully settled DAG transitions to Completed and
                        // newly ready dependents spawn.
                        if self.status == WorkflowStatus::Running
                            && !self.planning_in_flight
                            && job.status.is_settled()
                        {
                            if job.status == JobStatus::Completed
                                && !job.soft_budget_exhausted
                                && let Some(task_id) = job.todo_task_id.clone()
                            {
                                let _ = self.backend.apply_todo(TodoOp::Done {
                                    task: Some(task_id),
                                    phase: None,
                                });
                            }
                            // Soft-budget partial work stays open for the
                            // parent's decision: the Todo task is never
                            // marked Done and the DAG is not re-armed, so
                            // dependents never advance on a partial result.
                            if job.status != JobStatus::Completed
                                || !job.soft_budget_exhausted
                            {
                                let _ = self.backend.reconcile_todo_dag();
                            }
                        }
                    }
                    ApplicationEvent::Orchestration(OrchestrationEvent::MessageDelivered {
                        message,
                        ..
                    }) => {
                        let _ = self.events.send(WorkflowSupervisorEvent::IrcDelivered {
                            workflow_id: self.contract.workflow_id.clone(),
                            generation: self.contract.generation,
                            message: message.clone(),
                        });
                        self.push_activity(
                            WorkflowSupervisorActivityKind::Irc,
                            message.body.clone(),
                        );
                    }
                    _ => {}
                }
                if self.status != WorkflowStatus::Paused {
                    if self.planning_in_flight {
                        // While the planning turn is awaited the status stays
                        // Planning; the projection (including Todo tasks the
                        // agent just created) is still refreshed live.
                        self.refresh();
                    } else {
                        self.set_status(self.status_from_backend());
                    }
                } else {
                    self.refresh();
                }
                (Ok(Vec::new()), Vec::new())
            }
        }
    }

    /// Runs the bounded planning flow: the initial planning prompt, then a
    /// single re-prompt if it produced no Todo tasks. Every planning prompt
    /// is itself bounded (assistant turns, tool calls, wall-clock deadline —
    /// P0-1), and a committed plan ends planning immediately (P0-2). On any
    /// bound with a valid canonical DAG the DAG is preserved and armed for
    /// execution before the workflow moves to Running; with no committed
    /// tasks the workflow fails naming the tripped bound. While a prompt is
    /// awaited, `ApplicationEvent` commands are still drained so Todo
    /// mutations made by the planning turn refresh the projection live
    /// (BUG-2); other requests are deferred until the turn completes. The
    /// caller processes the returned deferred requests after the workflow
    /// status has settled, preserving serial-actor ordering.
    async fn run_planning_phase(
        &mut self,
        commands: &mut mpsc::Receiver<SupervisorCommand>,
    ) -> Result<Vec<SupervisorCommand>> {
        // P0-C: fail actionably before spending a single planning turn when
        // the objective explicitly delegates to an agent that is absent or
        // disabled in the workflow child catalog (never a silent fallback to
        // the bundled `task` agent).
        self.backend
            .validate_objective_agents(&self.contract.objective)?;
        let mut deferred = Vec::new();
        let (outcome, turn_deferred) =
            self.await_planning_turn(self.contract.initial_prompt(), commands)
                .await?;
        deferred.extend(turn_deferred);
        // A user cancel that interrupted the turn already won: never run the
        // bounded re-prompt (or clobber the Cancelled status) on a cancelled
        // workflow.
        if is_terminal(self.status) {
            return Ok(deferred);
        }
        match outcome {
            PlanningTurnOutcome::Completed | PlanningTurnOutcome::PlanCommitted => {
                if self.todo_has_tasks() {
                    self.arm_plan_and_run()?;
                    return Ok(deferred);
                }
            }
            PlanningTurnOutcome::PlanBudgetReached { reason } => {
                if self.todo_has_tasks() {
                    self.preserve_plan_and_run(&reason)?;
                    return Ok(deferred);
                }
                // Never strand deferred requests: process them (against a
                // workflow that is about to fail, where Pause/Cancel/Steer
                // are all no-ops or errors) before returning the failure.
                self.flush_deferred(std::mem::take(&mut deferred), commands)
                    .await;
                bail!("planning stopped without a committed plan: {reason}; remove and re-create the workflow to retry");
            }
            PlanningTurnOutcome::TimedOut => {
                if self.todo_has_tasks() {
                    self.preserve_plan_and_run(
                        "planning exceeded the wall-clock deadline; preserving the committed plan",
                    )?;
                    return Ok(deferred);
                }
                self.flush_deferred(std::mem::take(&mut deferred), commands)
                    .await;
                bail!("planning timed out before a plan was committed; remove and re-create the workflow to retry");
            }
        }
        if is_terminal(self.status) {
            return Ok(deferred);
        }
        if self.todo_has_tasks() {
            return Ok(deferred);
        }
        let (outcome, turn_deferred) =
            self.await_planning_turn(self.contract.replan_prompt(), commands)
                .await?;
        deferred.extend(turn_deferred);
        if is_terminal(self.status) {
            return Ok(deferred);
        }
        match outcome {
            PlanningTurnOutcome::Completed | PlanningTurnOutcome::PlanCommitted => {
                if self.todo_has_tasks() {
                    self.arm_plan_and_run()?;
                    return Ok(deferred);
                }
            }
            PlanningTurnOutcome::PlanBudgetReached { reason } => {
                if self.todo_has_tasks() {
                    self.preserve_plan_and_run(&reason)?;
                    return Ok(deferred);
                }
                self.flush_deferred(std::mem::take(&mut deferred), commands)
                    .await;
                bail!("planning stopped without a committed plan: {reason}; remove and re-create the workflow to retry");
            }
            PlanningTurnOutcome::TimedOut => {
                if self.todo_has_tasks() {
                    self.preserve_plan_and_run(
                        "planning exceeded the wall-clock deadline; preserving the committed plan",
                    )?;
                    return Ok(deferred);
                }
                self.flush_deferred(std::mem::take(&mut deferred), commands)
                    .await;
                bail!("planning timed out before a plan was committed; remove and re-create the workflow to retry");
            }
        }
        if is_terminal(self.status) {
            return Ok(deferred);
        }
        if self.todo_has_tasks() {
            return Ok(deferred);
        }
        // The bounded re-prompt still produced no tasks. Never strand
        // deferred requests: process them (against a workflow that is about
        // to fail, where Pause/Cancel/Steer are all no-ops or errors) before
        // returning the failure.
        self.flush_deferred(std::mem::take(&mut deferred), commands)
            .await;
        bail!("planning produced no tasks; remove and re-create the workflow to retry")
    }

    /// Transition from Planning to Running over a committed plan: explicitly
    /// arm Todo DAG execution (workflow Todo mutations never auto-arm, see
    /// BUG-1) and set the status immediately instead of waiting for the
    /// planning run to end.
    fn arm_plan_and_run(&mut self) -> Result<()> {
        self.backend.execute_todo_dag()?;
        self.set_status(WorkflowStatus::Running);
        Ok(())
    }

    /// Preserve a DAG that was committed before a planning bound tripped:
    /// surface the bound in the live activity feed, arm DAG execution and
    /// move to Running (P0-1).
    fn preserve_plan_and_run(&mut self, reason: &str) -> Result<()> {
        let task_count = self
            .backend
            .todo_state()
            .phases
            .iter()
            .map(|phase| phase.tasks.len())
            .sum::<usize>();
        self.push_activity(
            WorkflowSupervisorActivityKind::Tool,
            format!("{reason} — preserving {task_count}-task DAG"),
        );
        self.arm_plan_and_run()
    }

    /// Re-arm Todo DAG execution over restored/resumed tasks. A Paused
    /// workflow resumes through the backend (which may execute the DAG);
    /// Planning/Running workflows execute directly. When no Todo tasks exist
    /// the workflow falls through to the bounded planning flow instead.
    async fn continue_execution(
        &mut self,
        commands: &mut mpsc::Receiver<SupervisorCommand>,
    ) -> Result<Vec<SupervisorCommand>> {
        if self.todo_has_tasks() {
            if self.status == WorkflowStatus::Paused {
                self.backend.resume().await?;
            } else {
                self.backend.execute_todo_dag()?;
            }
            Ok(Vec::new())
        } else {
            self.run_planning_phase(commands).await
        }
    }

    async fn flush_deferred(
        &mut self,
        deferred: Vec<SupervisorCommand>,
        commands: &mut mpsc::Receiver<SupervisorCommand>,
    ) {
        let mut worklist = std::collections::VecDeque::from(deferred);
        while let Some(command) = worklist.pop_front() {
            let (result, nested) = Box::pin(self.handle(command.request, commands)).await;
            let _ = command.reply.send(result);
            worklist.extend(nested);
        }
    }

    /// Awaits one planning turn while still processing `ApplicationEvent`
    /// commands concurrently. The turn is bounded three ways (P0-1): the
    /// backend's per-turn stop hook caps assistant turns and tool calls, the
    /// wall-clock deadline here aborts the in-flight prompt, and semantic
    /// Todo non-progress detection aborts it too (P1-2). Returns the typed
    /// outcome plus the requests deferred while the turn was in flight; the
    /// caller processes the deferred requests after the turn ends.
    async fn await_planning_turn(
        &mut self,
        prompt: String,
        commands: &mut mpsc::Receiver<SupervisorCommand>,
    ) -> Result<(PlanningTurnOutcome, Vec<SupervisorCommand>)> {
        self.planning_in_flight = true;
        // Seed the non-progress detector with the canonical Todo state
        // before the turn so the first observed call has a "before" state.
        self.plan_committed = false;
        self.last_todo_state_hash = Some(todo_state_hash(&self.backend.todo_state()));
        self.last_failed_todo_fingerprint = None;
        self.identical_failed_ops = 0;
        self.corrections_without_progress = 0;
        self.planning_non_progress_tripped = false;
        self.planning_non_progress_reason = None;
        // Clone the backend so the pinned prompt future does not borrow self,
        // which the concurrent ApplicationEvent handling below must mutate.
        let backend = self.backend.clone();
        let deadline = backend.planning_deadline();
        let mut planning = Box::pin(backend.prompt_supervisor(prompt));
        // Pin ONE wall-clock sleep for the whole turn. Recreating the sleep
        // inside the select would restart the countdown on every forwarded
        // command (Activity/TodoObservation/ApplicationEvent), turning the
        // budget into "deadline of silence" instead of a true total bound: a
        // provider streaming keepalive deltas every <deadline would hold the
        // workflow in Planning forever. The pinned sleep is polled
        // `&mut`-borrowed, so its deadline is measured from here.
        let deadline_sleep = tokio::time::sleep(deadline);
        tokio::pin!(deadline_sleep);
        let mut deferred = Vec::new();
        let outcome = loop {
            tokio::select! {
                outcome = &mut planning => break outcome,
                command = commands.recv() => {
                    match command {
                        Some(command) => {
                            if matches!(command.request, SupervisorRequest::ApplicationEvent { .. })
                                || matches!(command.request, SupervisorRequest::Activity { .. })
                            {
                                // Boxed: `handle` recurses through the
                                // planning phase (see E0733).
                                let (result, nested) = Box::pin(self.handle(command.request, commands)).await;
                                let _ = command.reply.send(result);
                                if !nested.is_empty() {
                                    self.flush_deferred(nested, commands).await;
                                }
                            } else if matches!(command.request, SupervisorRequest::Cancel) {
                                // A user cancel must interrupt a genuinely
                                // stuck planning turn instead of being
                                // deferred until the provider returns (which
                                // may be never). Process it immediately, then
                                // abort the in-flight prompt so the workflow
                                // settles into Cancelled promptly.
                                let (result, nested) = Box::pin(self.handle(command.request, commands)).await;
                                let _ = command.reply.send(result);
                                if !nested.is_empty() {
                                    self.flush_deferred(nested, commands).await;
                                }
                                let _ = backend.pause().await;
                                // The abort's RunFailed side effect must not
                                // clobber the Cancelled status.
                                self.suppress_abort_failure = true;
                            } else if matches!(command.request, SupervisorRequest::TodoObservation { .. }) {
                                let (result, nested) = Box::pin(self.handle(command.request, commands)).await;
                                let _ = command.reply.send(result);
                                if !nested.is_empty() {
                                    self.flush_deferred(nested, commands).await;
                                }
                                if self.planning_non_progress_tripped {
                                    // Semantic non-progress detection fired:
                                    // abort the in-flight prompt and settle
                                    // with an actionable reason.
                                    let reason = self
                                        .planning_non_progress_reason
                                        .take()
                                        .unwrap_or_else(|| "planning is not converging".to_owned());
                                    self.planning_non_progress_tripped = false;
                                    let _ = backend.pause().await;
                                    self.suppress_abort_failure = true;
                                    break Ok(PlanningTurnOutcome::PlanBudgetReached { reason });
                                }
                            } else {
                                deferred.push(command);
                            }
                        }
                        None => {
                            // All senders dropped (supervisor handle gone):
                            // nothing else will arrive, wait for the turn.
                            break planning.await;
                        }
                    }
                }
                _ = &mut deadline_sleep => {
                    // Wall-clock bound: abort the in-flight prompt (an
                    // unresponsive provider must never hold the workflow in
                    // Planning forever), then settle from canonical Todo
                    // state. The abort's RunFailed side effect is suppressed.
                    let _ = backend.pause().await;
                    self.suppress_abort_failure = true;
                    break Ok(PlanningTurnOutcome::TimedOut);
                }
            }
        };
        self.planning_in_flight = false;
        let outcome = outcome?;
        Ok((outcome, deferred))
    }

    fn fail(&mut self, error: anyhow::Error) -> Result<Vec<String>> {
        if is_terminal(self.status) {
            // The workflow already settled (Failed, Cancelled, Completed,
            // Conflicted) — e.g. a user cancel interrupted planning, or a
            // concurrent terminal event won. Never clobber it with a later
            // RunFailed from the aborted turn.
            return Ok(Vec::new());
        }
        let message = error.to_string();
        self.failure = Some(message.clone());
        self.set_status(WorkflowStatus::Failed);
        let _ = self.events.send(WorkflowSupervisorEvent::Failed {
            workflow_id: self.contract.workflow_id.clone(),
            generation: self.contract.generation,
            message,
        });
        Ok(Vec::new())
    }

    fn todo_has_tasks(&self) -> bool {
        self.backend
            .todo_state()
            .phases
            .iter()
            .any(|phase| !phase.tasks.is_empty())
    }

    /// Semantic planning non-progress detection (P1-2) plus the plan-commit
    /// signal (P0-2). The first successful `todo init` ends planning: the
    /// workflow moves to Running immediately while the model's run continues
    /// its short grace. Todo-state change is the decisive non-progress
    /// signal: a legitimate init + dependency sequence mutates the canonical
    /// Todo state on every successful call and never trips. Failed calls with
    /// no state change count toward two limits — 3 identical failed
    /// operations (same op + target IDs + error prefix) or 6 total
    /// corrections — after which planning is declared non-converging and the
    /// in-flight prompt is aborted.
    fn record_planning_observation(&mut self, observation: &WorkflowSupervisorTodoObservation) {
        if !self.plan_committed
            && !observation.is_error
            && observation.op.as_deref() == Some("init")
        {
            self.plan_committed = true;
            if self.planning_in_flight && self.status == WorkflowStatus::Planning {
                self.set_status(WorkflowStatus::Running);
            }
        }
        let state_hash = todo_state_hash(&self.backend.todo_state());
        let progressed = self.last_todo_state_hash != Some(state_hash);
        self.last_todo_state_hash = Some(state_hash);
        if progressed {
            self.last_failed_todo_fingerprint = None;
            self.identical_failed_ops = 0;
            self.corrections_without_progress = 0;
            return;
        }
        if !observation.is_error {
            return;
        }
        let fingerprint = format!(
            "{}|{:?}|{}",
            observation.op.as_deref().unwrap_or("todo"),
            observation.target_ids,
            observation.error_prefix.as_deref().unwrap_or(""),
        );
        if self.last_failed_todo_fingerprint.as_deref() == Some(&fingerprint) {
            self.identical_failed_ops += 1;
        } else {
            self.last_failed_todo_fingerprint = Some(fingerprint);
            self.identical_failed_ops = 1;
        }
        self.corrections_without_progress += 1;
        if self.identical_failed_ops >= PLANNING_IDENTICAL_FAILED_OP_LIMIT
            || self.corrections_without_progress >= PLANNING_CORRECTIONS_WITHOUT_PROGRESS_LIMIT
        {
            self.planning_non_progress_tripped = true;
            self.planning_non_progress_reason = Some(format!(
                "planning is not converging: {} identical failed todo operation(s) and \
                 {} todo correction(s) with no Todo-state change",
                self.identical_failed_ops, self.corrections_without_progress
            ));
        }
    }

    fn status_from_backend(&self) -> WorkflowStatus {
        let todo = self.backend.todo_state();
        match self.backend.todo_dag_status() {
            TodoDagExecutionStatus::Active => WorkflowStatus::Running,
            TodoDagExecutionStatus::Settled => {
                if todo_is_exactly_complete(&todo) {
                    WorkflowStatus::Completed
                } else {
                    WorkflowStatus::Failed
                }
            }
            TodoDagExecutionStatus::Blocked => WorkflowStatus::Failed,
            TodoDagExecutionStatus::Dormant => {
                if todo_is_exactly_complete(&todo) {
                    WorkflowStatus::Completed
                } else if todo_has_any_task(&todo) {
                    // The supervisor agent owns execution for workflow
                    // children: a parked DAG with open tasks is an actively
                    // worked workflow, not a Planning one.
                    WorkflowStatus::Running
                } else {
                    WorkflowStatus::Planning
                }
            }
        }
    }

    fn set_status(&mut self, status: WorkflowStatus) {
        let changed = self.status != status;
        self.status = status;
        match status {
            WorkflowStatus::Planning => {
                if self.planning_started_at_ms.is_none() {
                    self.planning_started_at_ms = Some(now_ms());
                }
            }
            _ => self.planning_started_at_ms = None,
        }
        self.refresh();
        if changed {
            let _ = self.events.send(WorkflowSupervisorEvent::StatusChanged {
                workflow_id: self.contract.workflow_id.clone(),
                generation: self.contract.generation,
                status,
            });
        }
    }

    fn refresh(&self) {
        let projection = project_backend(
            &self.contract,
            self.backend.as_ref(),
            self.status,
            self.failure.clone(),
            &self.activity,
            self.planning_started_at_ms,
        );
        *self.projection.lock() = projection.clone();
        let _ = self
            .events
            .send(WorkflowSupervisorEvent::ProjectionChanged { projection });
    }

    /// Append one bounded activity entry (newest-last) and refresh the
    /// projection so the workflow page sees the supervisor's live turn.
    fn push_activity(&mut self, kind: WorkflowSupervisorActivityKind, text: String) {
        let redacted = crate::redact_value(&serde_json::Value::String(text.clone()))
            .as_str()
            .map_or_else(|| text, str::to_owned);
        let text: String = redacted.chars().take(SUPERVISOR_ACTIVITY_TEXT_CAP).collect();
        self.activity.push(WorkflowSupervisorActivity {
            at_ms: now_ms(),
            kind,
            text,
        });
        if self.activity.len() > SUPERVISOR_ACTIVITY_CAP {
            let excess = self.activity.len() - SUPERVISOR_ACTIVITY_CAP;
            self.activity.drain(..excess);
        }
        self.refresh();
    }

    /// Settle the live status from the backend unless a terminal outcome
    /// (e.g. a user cancel that interrupted planning) already won.
    fn settle_status(&mut self) {
        if !is_terminal(self.status) {
            self.set_status(self.status_from_backend());
        }
    }
}

fn project_backend<B: WorkflowSupervisorBackend + ?Sized>(
    contract: &WorkflowSupervisorContract,
    backend: &B,
    status: WorkflowStatus,
    failure: Option<String>,
    activity: &[WorkflowSupervisorActivity],
    planning_started_at_ms: Option<u64>,
) -> WorkflowSupervisorProjection {
    WorkflowSupervisorProjection {
        workflow_id: contract.workflow_id.clone(),
        generation: contract.generation,
        status,
        supervisor_agent_id: contract.supervisor_agent_id.clone(),
        todo: backend.todo_state(),
        jobs: backend.workflow_jobs(&contract.workflow_id, contract.generation),
        irc: backend.inbox(&contract.supervisor_agent_id, true),
        failure,
        activity: activity.to_vec(),
        planning_started_at_ms,
    }
}

fn todo_is_exactly_complete(todo: &TodoState) -> bool {
    let mut saw_task = false;
    for task in todo.phases.iter().flat_map(|phase| &phase.tasks) {
        saw_task = true;
        if !matches!(task.status, crate::TodoStatus::Completed | crate::TodoStatus::Abandoned) {
            return false;
        }
    }
    saw_task
}

/// Whether a completed tool result is a successful `todo init` — the
/// plan-commit signal (P0-2). Used by the backend's planning stop hook.
pub(crate) fn workflow_supervisor_todo_init_succeeded(result: &pi_ai::ToolResultMessage) -> bool {
    result.tool_name == "todo"
        && !result.is_error
        && result
            .details
            .as_ref()
            .is_some_and(|details| details.get("op").and_then(serde_json::Value::as_str) == Some("init"))
}

fn todo_has_any_task(todo: &TodoState) -> bool {
    todo.phases.iter().any(|phase| !phase.tasks.is_empty())
}

/// Stable-enough content hash of the canonical Todo state, used by planning
/// non-progress detection to decide whether a Todo tool call changed
/// anything. Built from a canonical serialization so identical DAGs hash
/// identically regardless of presentation details.
fn todo_state_hash(todo: &TodoState) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    for phase in &todo.phases {
        phase.name.hash(&mut hasher);
        for task in &phase.tasks {
            task.id.hash(&mut hasher);
            task.content.hash(&mut hasher);
            let status = match task.status {
                crate::TodoStatus::Pending => 0u8,
                crate::TodoStatus::InProgress => 1,
                crate::TodoStatus::Completed => 2,
                crate::TodoStatus::Abandoned => 3,
            };
            status.hash(&mut hasher);
            for dependency in &task.depends_on {
                dependency.hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}

const fn is_terminal(status: WorkflowStatus) -> bool {
    matches!(
        status,
        WorkflowStatus::Completed
            | WorkflowStatus::Failed
            | WorkflowStatus::Cancelled
            | WorkflowStatus::Conflicted
    )
}

#[must_use]
pub fn workflow_task_ownership(
    contract: &WorkflowSupervisorContract,
    todo_task_id: impl Into<String>,
) -> WorkflowTaskOwnership {
    WorkflowTaskOwnership {
        workflow_id: contract.workflow_id.clone(),
        todo_task_id: todo_task_id.into(),
        generation: contract.generation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract(objective: &str) -> WorkflowSupervisorContract {
        WorkflowSupervisorContract {
            workflow_id: "wf-concise".to_owned(),
            generation: 1,
            worktree_label: "worktrees/wf-concise".to_owned(),
            objective: objective.to_owned(),
            supervisor_agent_id: "supervisor".to_owned(),
            max_concurrency: 2,
        }
    }

    /// The planning prompts pin the concise-title contract: every Todo task
    /// is a terse imperative title of ≤ ~60 chars, because the task content
    /// is exactly what the workflow panel shows and longer titles truncate.
    /// This is a prompt-content contract, not a planner-behavior test — the
    /// planner is a live model, so behavior cannot be asserted determinis-
    /// tically; the prompt string is the enforceable surface.
    #[test]
    fn initial_prompt_requires_concise_task_titles() {
        let prompt = contract("Ship the release").initial_prompt();
        for needle in [
            "single concise line",
            "~60 characters",
            "terse imperative title",
            "Bootstrap pi-zig sources",
            "longer titles are truncated",
        ] {
            assert!(
                prompt.contains(needle),
                "initial prompt must require concise task titles: missing {needle:?}\n{prompt}"
            );
        }
    }

    /// The final-attempt re-prompt builds the same canonical DAG under the
    /// same display contract, so it carries the same title requirement.
    #[test]
    fn replan_prompt_requires_concise_task_titles() {
        let prompt = contract("Ship the release").replan_prompt();
        for needle in [
            "concise title",
            "~60 characters",
            "terse imperative phrase",
            "Bootstrap pi-zig sources",
            "longer titles are truncated",
        ] {
            assert!(
                prompt.contains(needle),
                "replan prompt must require concise task titles: missing {needle:?}\n{prompt}"
            );
        }
    }

    /// The planning prompts must steer the planner toward a WIDE DAG: the
    /// executor runs every ready task concurrently, so independent work must
    /// stay in separate tasks without dependency edges and `depends_on` is
    /// reserved for genuine data/control ordering. Without this guidance the
    /// planner chains unrelated steps into a near-linear DAG and the panel
    /// shows one active task despite several independent ready ones (the
    /// "1 active · 6 next" complaint).
    #[test]
    fn initial_prompt_requires_parallel_width_guidance() {
        let prompt = contract("Ship the release").initial_prompt();
        for needle in [
            "WIDE, parallel DAG",
            "independent work",
            "runs every ready task concurrently",
            "genuine data",
            "several ready tasks per execution wave",
        ] {
            assert!(
                prompt.contains(needle),
                "initial prompt must guide DAG width: missing {needle:?}\n{prompt}"
            );
        }
    }

    /// The final-attempt re-prompt carries the same width-first guidance.
    #[test]
    fn replan_prompt_requires_parallel_width_guidance() {
        let prompt = contract("Ship the release").replan_prompt();
        for needle in [
            "WIDE, parallel DAG",
            "runs every ready task concurrently",
            "genuine data",
            "several ready tasks per execution wave",
        ] {
            assert!(
                prompt.contains(needle),
                "replan prompt must guide DAG width: missing {needle:?}\n{prompt}"
            );
        }
    }
}
