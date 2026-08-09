use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use parking_lot::Mutex;
use pi_coding::{
    ApplicationEvent, JobSnapshot, JobStatus, MailboxMessage, OrchestrationConcurrencyGate,
    OrchestrationEvent, PlanningTurnOutcome, TodoApplyResult, TodoDagExecutionOutcome,
    TodoDagExecutionStatus, TodoItem, TodoOp, TodoPhase, TodoState, TodoStatus, TodoStorage,
    WorkflowJobSnapshot, WorkflowRuntimeScope, WorkflowStatus, WorkflowSupervisor,
    WorkflowSupervisorActivityKind, WorkflowSupervisorBackend, WorkflowSupervisorContract,
    WorkflowSupervisorTodoObservation,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Optional gate that blocks a planning prompt until released, so tests can
/// observe the supervisor mid-turn (projection refresh, status stability).
#[derive(Clone)]
struct PromptGate {
    started: Arc<Notify>,
    started_flag: Arc<AtomicBool>,
    release: CancellationToken,
}

impl PromptGate {
    fn new() -> Self {
        Self {
            started: Arc::new(Notify::new()),
            started_flag: Arc::new(AtomicBool::new(false)),
            release: CancellationToken::new(),
        }
    }

    fn signal_started(&self) {
        self.started_flag.store(true, Ordering::Release);
        // notify_one stores a permit: a waiter that polls after the signal
        // still completes immediately (notify_waiters would lose it).
        self.started.notify_one();
    }

    async fn wait_started(&self) {
        loop {
            let notified = self.started.notified();
            if self.started_flag.load(Ordering::Acquire) {
                break;
            }
            notified.await;
        }
    }
}

#[derive(Clone)]
struct FakeBackend {
    state: Arc<Mutex<FakeState>>,
}

struct FakeState {
    workflow_scope: Option<WorkflowRuntimeScope>,
    max_concurrency: usize,
    todo: TodoState,
    dag_status: TodoDagExecutionStatus,
    jobs: Vec<WorkflowJobSnapshot>,
    irc: Vec<MailboxMessage>,
    prompt: Option<String>,
    steers: Vec<String>,
    cancelled: Vec<String>,
    pause_count: usize,
    resume_count: usize,
    prompt_count: usize,
    /// After the N-th prompt, replace the Todo DAG with these tasks (used to
    /// exercise the bounded re-prompt success path).
    populate_tasks_after_prompt: Option<(usize, Vec<TodoItem>)>,
    /// Scripted planning-prompt outcomes, consumed in order. Empty = every
    /// prompt returns `Completed` (the natural run-end).
    prompt_outcomes: VecDeque<PlanningTurnOutcome>,
    /// Wall-clock budget override for the supervisor's planning deadline
    /// (None = crate default).
    planning_deadline: Option<Duration>,
    /// Whether a non-progress abort (pause) was requested while a prompt was
    /// in flight (observable effect of the P1-2 detection / P0-1 deadline).
    aborted_while_prompting: bool,
    prompt_gate: Option<PromptGate>,
    /// Number of DAG reconciliations the supervisor triggered. Lets tests
    /// assert that soft-budget partial worker results never re-arm the DAG.
    reconcile_count: usize,
    /// Task ids the last reconciliation would have spawned as newly ready
    /// (dependencies satisfied, task still pending).
    last_spawned_task_ids: Vec<String>,
}

impl FakeBackend {
    fn new(max_concurrency: usize, tasks: Vec<TodoItem>) -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeState {
                workflow_scope: None,
                max_concurrency,
                todo: TodoState {
                    phases: vec![TodoPhase {
                        name: "Build".to_owned(),
                        tasks,
                    }],
                    storage: TodoStorage::Memory,
                },
                dag_status: TodoDagExecutionStatus::Dormant,
                jobs: Vec::new(),
                irc: Vec::new(),
                prompt: None,
                steers: Vec::new(),
                cancelled: Vec::new(),
                pause_count: 0,
                resume_count: 0,
                prompt_count: 0,
                populate_tasks_after_prompt: None,
                prompt_outcomes: VecDeque::new(),
                planning_deadline: None,
                aborted_while_prompting: false,
                prompt_gate: None,
                reconcile_count: 0,
                last_spawned_task_ids: Vec::new(),
            })),
        }
    }

    fn set_dag_status(&self, status: TodoDagExecutionStatus) {
        self.state.lock().dag_status = status;
    }

    fn set_tasks(&self, tasks: Vec<TodoItem>) {
        let mut state = self.state.lock();
        state.todo = TodoState {
            phases: vec![TodoPhase {
                name: "Build".to_owned(),
                tasks,
            }],
            storage: TodoStorage::Memory,
        };
    }

    fn set_task_status(&self, task_id: &str, status: TodoStatus) {
        let mut state = self.state.lock();
        let task = state
            .todo
            .phases
            .iter_mut()
            .flat_map(|phase| &mut phase.tasks)
            .find(|task| task.id == task_id)
            .expect("task");
        task.status = status;
    }

    fn push_job(
        &self,
        workflow_id: &str,
        generation: u64,
        task_id: &str,
        job_id: &str,
        status: JobStatus,
    ) {
        self.state.lock().jobs.push(workflow_job(
            workflow_id,
            generation,
            task_id,
            job_id,
            status,
        ));
    }

    fn state(&self) -> parking_lot::MutexGuard<'_, FakeState> {
        self.state.lock()
    }
}

#[async_trait]
impl WorkflowSupervisorBackend for FakeBackend {
    fn todo_state(&self) -> TodoState {
        self.state.lock().todo.clone()
    }

    fn todo_dag_status(&self) -> TodoDagExecutionStatus {
        self.state.lock().dag_status
    }

    fn workflow_jobs(&self, workflow_id: &str, generation: u64) -> Vec<WorkflowJobSnapshot> {
        self.state
            .lock()
            .jobs
            .iter()
            .filter(|job| job.workflow_id == workflow_id && job.generation == generation)
            .cloned()
            .collect()
    }

    fn inbox(&self, _agent_id: &str, _peek: bool) -> Vec<MailboxMessage> {
        self.state.lock().irc.clone()
    }

    fn active_workflow_job_ids(&self, workflow_id: &str, generation: u64) -> Vec<String> {
        self.workflow_jobs(workflow_id, generation)
            .into_iter()
            .filter(|job| matches!(job.job.status, JobStatus::Queued | JobStatus::Running))
            .map(|job| job.job.id)
            .collect()
    }

    fn configure_workflow_runtime(
        &self,
        scope: WorkflowRuntimeScope,
        max_concurrency: usize,
        _global_concurrency: OrchestrationConcurrencyGate,
    ) -> Result<()> {
        let mut state = self.state.lock();
        assert_eq!(state.max_concurrency, max_concurrency);
        state.workflow_scope = Some(scope);
        Ok(())
    }

    async fn prompt_supervisor(&self, prompt: String) -> Result<PlanningTurnOutcome> {
        let gate = {
            let mut state = self.state.lock();
            state.prompt = Some(prompt);
            state.prompt_count += 1;
            if let Some((count, tasks)) = state.populate_tasks_after_prompt.clone()
                && state.prompt_count >= count
            {
                state.populate_tasks_after_prompt = None;
                state.todo = TodoState {
                    phases: vec![TodoPhase {
                        name: "Build".to_owned(),
                        tasks,
                    }],
                    storage: TodoStorage::Memory,
                };
            }
            state.prompt_gate.clone()
        };
        if let Some(gate) = gate {
            gate.signal_started();
            gate.release.cancelled().await;
        }
        // Re-read the scripted outcome once the prompt settles.
        let outcome = self
            .state
            .lock()
            .prompt_outcomes
            .pop_front()
            .unwrap_or(PlanningTurnOutcome::Completed);
        Ok(outcome)
    }

    async fn steer_supervisor(&self, message: String) -> Result<()> {
        self.state.lock().steers.push(message);
        Ok(())
    }

    fn planning_deadline(&self) -> Duration {
        self.state
            .lock()
            .planning_deadline
            .unwrap_or(Duration::from_secs(90))
    }

    fn apply_todo(&self, op: TodoOp) -> Result<TodoApplyResult> {
        let mut state = self.state.lock();
        let previous = state.todo.phases.clone();
        let mut updated = previous.clone();
        match op {
            TodoOp::Done { task, .. } => {
                let Some(task) = task else {
                    return Err(anyhow::anyhow!("fake done requires a task id"));
                };
                let target = updated
                    .iter_mut()
                    .flat_map(|phase| &mut phase.tasks)
                    .find(|item| item.id == task || item.content == task)
                    .ok_or_else(|| anyhow::anyhow!("task not found"))?;
                target.status = TodoStatus::Completed;
            }
            TodoOp::Start { task } => {
                let target = updated
                    .iter_mut()
                    .flat_map(|phase| &mut phase.tasks)
                    .find(|item| item.id == task || item.content == task)
                    .ok_or_else(|| anyhow::anyhow!("task not found"))?;
                target.status = TodoStatus::InProgress;
            }
            other => return Err(anyhow::anyhow!("fake backend does not implement {other:?}")),
        }
        let completed_tasks = previous
            .iter()
            .flat_map(|phase| &phase.tasks)
            .filter_map(|task| {
                (task.status != TodoStatus::Completed
                    && updated
                        .iter()
                        .flat_map(|phase| &phase.tasks)
                        .any(|item| item.id == task.id && item.status == TodoStatus::Completed))
                .then(|| pi_coding::TodoCompletionTransition {
                    phase: String::new(),
                    content: task.content.clone(),
                })
            })
            .collect();
        state.todo.phases = updated.clone();
        Ok(TodoApplyResult {
            phases: updated,
            completed_tasks,
            summary: "ok".to_owned(),
        })
    }

    fn reconcile_todo_dag(&self) -> Result<TodoDagExecutionOutcome> {
        let mut state = self.state.lock();
        state.reconcile_count += 1;
        let tasks = state
            .todo
            .phases
            .iter()
            .flat_map(|phase| &phase.tasks)
            .cloned()
            .collect::<Vec<_>>();
        let all_done = !tasks.is_empty()
            && tasks
                .iter()
                .all(|task| matches!(task.status, TodoStatus::Completed | TodoStatus::Abandoned));
        if all_done {
            state.dag_status = TodoDagExecutionStatus::Settled;
        }
        // Newly ready tasks (dependencies satisfied, task still pending) are
        // recorded so tests can observe whether dependents advance on a
        // worker result.
        state.last_spawned_task_ids = tasks
            .iter()
            .filter(|task| {
                task.status == TodoStatus::Pending
                    && task.depends_on.iter().all(|dependency| {
                        tasks.iter().any(|other| {
                            other.id == *dependency
                                && matches!(
                                    other.status,
                                    TodoStatus::Completed | TodoStatus::Abandoned
                                )
                        })
                    })
            })
            .map(|task| task.id.clone())
            .collect();
        Ok(TodoDagExecutionOutcome {
            status: state.dag_status,
            spawns: Vec::new(),
        })
    }

    fn execute_todo_dag(&self) -> Result<TodoDagExecutionOutcome> {
        let mut state = self.state.lock();
        state.dag_status = TodoDagExecutionStatus::Active;
        Ok(TodoDagExecutionOutcome {
            status: TodoDagExecutionStatus::Active,
            spawns: Vec::new(),
        })
    }

    async fn pause(&self) -> Result<()> {
        let mut state = self.state.lock();
        state.pause_count += 1;
        state.aborted_while_prompting = true;
        Ok(())
    }

    async fn resume(&self) -> Result<TodoDagExecutionOutcome> {
        let mut state = self.state.lock();
        state.resume_count += 1;
        state.dag_status = TodoDagExecutionStatus::Active;
        Ok(TodoDagExecutionOutcome {
            status: TodoDagExecutionStatus::Active,
            spawns: Vec::new(),
        })
    }

    async fn cancel_jobs(&self, ids: &[String]) -> Result<Vec<String>> {
        let mut state = self.state.lock();
        let mut status = HashMap::new();
        for job in &state.jobs {
            status.insert(job.job.id.clone(), job.job.status);
        }
        let mut cancelled = ids
            .iter()
            .filter(|id| status.get(*id).is_some_and(|job| !job.is_settled()))
            .cloned()
            .collect::<Vec<_>>();
        cancelled.sort();
        for job in &mut state.jobs {
            if cancelled.contains(&job.job.id) {
                job.job.status = JobStatus::Cancelled;
            }
        }
        state.cancelled.extend(cancelled.clone());
        Ok(cancelled)
    }
}

fn task(id: &str) -> TodoItem {
    TodoItem {
        id: id.to_owned(),
        content: format!("work {id}"),
        status: TodoStatus::Pending,
        depends_on: Vec::new(),
        ready: true,
        blocked_by: Vec::new(),
        agent: None,
    }
}

/// Task gated behind `depends_on` — not ready until every dependency is
/// Completed/Abandoned.
fn task_with_deps(id: &str, depends_on: Vec<&str>) -> TodoItem {
    let mut item = task(id);
    item.depends_on = depends_on.into_iter().map(str::to_owned).collect();
    item.ready = false;
    item
}

fn contract(workflow_id: &str, generation: u64) -> WorkflowSupervisorContract {
    WorkflowSupervisorContract {
        workflow_id: workflow_id.to_owned(),
        generation,
        worktree_label: workflow_id.to_owned(),
        objective: format!("complete {workflow_id}"),
        supervisor_agent_id: format!("Supervisor-{workflow_id}"),
        max_concurrency: 2,
    }
}

fn workflow_job(
    workflow_id: &str,
    generation: u64,
    task_id: &str,
    job_id: &str,
    status: JobStatus,
) -> WorkflowJobSnapshot {
    WorkflowJobSnapshot {
        workflow_id: workflow_id.to_owned(),
        generation,
        todo_task_id: Some(task_id.to_owned()),
        job: JobSnapshot {
            id: job_id.to_owned(),
            agent_id: format!("Worker-{job_id}"),
            agent: "task".to_owned(),
            parent_id: format!("Supervisor-{workflow_id}"),
            description: Some(format!("work {task_id}")),
            todo_task_id: Some(task_id.to_owned()),
            workflow_id: Some(workflow_id.to_owned()),
            workflow_generation: Some(generation),
            status,
            created_at: 1,
            started_at: None,
            finished_at: None,
            result: None,
            soft_budget_exhausted: false,
        },
    }
}

fn job_event(job: WorkflowJobSnapshot) -> ApplicationEvent {
    ApplicationEvent::Orchestration(OrchestrationEvent::JobUpdated {
        group_id: format!("group-{}", job.workflow_id),
        job: job.job,
    })
}

/// A settled job snapshot carrying the soft-budget marker — the worker
/// yielded on a configured budget with a partial result.
fn soft_budget_job(
    workflow_id: &str,
    generation: u64,
    task_id: &str,
    job_id: &str,
    status: JobStatus,
) -> WorkflowJobSnapshot {
    let mut snapshot = workflow_job(workflow_id, generation, task_id, job_id, status);
    snapshot.job.soft_budget_exhausted = true;
    snapshot
}

#[tokio::test]
async fn two_supervisors_coexist_and_route_irc_without_cross_workflow_mutation() {
    let global = OrchestrationConcurrencyGate::new(3).expect("global gate");
    let alpha_backend = Arc::new(FakeBackend::new(2, vec![task("same-task")]));
    let beta_backend = Arc::new(FakeBackend::new(2, vec![task("same-task")]));
    let alpha = WorkflowSupervisor::spawn(contract("alpha", 1), alpha_backend.clone(), global.clone())
        .expect("alpha");
    let beta = WorkflowSupervisor::spawn(contract("beta", 1), beta_backend.clone(), global)
        .expect("beta");

    alpha.start().await.expect("start alpha");
    beta.start().await.expect("start beta");
    assert_eq!(alpha.projection().status, WorkflowStatus::Running);
    assert_eq!(beta.projection().status, WorkflowStatus::Running);
    assert!(alpha_backend.state().prompt.as_deref().is_some_and(|prompt| {
        prompt.contains("workflow alpha") && prompt.contains("todoTaskId")
    }));
    assert!(beta_backend.state().prompt.as_deref().is_some_and(|prompt| {
        prompt.contains("workflow beta") && prompt.contains("todoTaskId")
    }));

    alpha.steer("Main directive for alpha").await.expect("steer alpha");
    assert_eq!(alpha_backend.state().steers, vec!["Main directive for alpha"]);
    assert!(beta_backend.state().steers.is_empty());

    beta_backend.set_task_status("same-task", TodoStatus::Completed);
    beta_backend.set_dag_status(TodoDagExecutionStatus::Settled);
    alpha
        .observe_application_event(
            1,
            job_event(workflow_job(
                "beta",
                1,
                "same-task",
                "beta-job",
                JobStatus::Completed,
            )),
        )
        .await
        .expect("foreign event ignored");
    assert_eq!(alpha.projection().status, WorkflowStatus::Running);
    assert_eq!(alpha.projection().todo.phases[0].tasks[0].status, TodoStatus::Pending);

    beta
        .observe_application_event(
            1,
            job_event(workflow_job(
                "beta",
                1,
                "same-task",
                "beta-job",
                JobStatus::Completed,
            )),
        )
        .await
        .expect("beta terminal");
    assert_eq!(beta.projection().status, WorkflowStatus::Completed);
}

#[tokio::test]
async fn stale_generation_duplicate_terminal_is_idempotent_and_preserves_open_todo() {
    let backend = Arc::new(FakeBackend::new(2, vec![task("same-task")]));
    let supervisor = WorkflowSupervisor::spawn(
        contract("alpha", 9),
        backend.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
    )
    .expect("supervisor");
    supervisor.start().await.expect("start");

    let stale = job_event(workflow_job(
        "alpha",
        8,
        "same-task",
        "stale-job",
        JobStatus::Completed,
    ));
    supervisor
        .observe_application_event(8, stale.clone())
        .await
        .expect("stale outer generation ignored");
    supervisor
        .observe_application_event(9, stale.clone())
        .await
        .expect("stale job generation ignored");
    supervisor
        .observe_application_event(9, stale)
        .await
        .expect("duplicate stale ignored");

    assert_eq!(supervisor.projection().status, WorkflowStatus::Running);
    assert_eq!(supervisor.projection().todo.phases[0].tasks[0].status, TodoStatus::Pending);
}

#[tokio::test]
async fn empty_planning_fails_after_one_bounded_reprompt() {
    let backend = Arc::new(FakeBackend::new(2, Vec::new()));
    let supervisor = WorkflowSupervisor::spawn(
        contract("empty-planning", 3),
        backend.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
    )
    .expect("supervisor");
    supervisor.start().await.expect("start");

    // The initial planning turn produced no Todo tasks: the supervisor gets
    // exactly ONE bounded re-prompt and then fails with an actionable
    // message instead of sitting in Planning forever.
    let projection = supervisor.projection();
    assert_eq!(projection.status, WorkflowStatus::Failed);
    assert!(
        projection
            .failure
            .as_deref()
            .is_some_and(|message| message.contains("planning produced no tasks")),
        "failure must be actionable, got {:?}",
        projection.failure
    );
    assert_eq!(backend.state().prompt_count, 2, "exactly one bounded re-prompt");
    assert!(
        backend
            .state()
            .prompt
            .as_deref()
            .is_some_and(|prompt| prompt.contains("produced no Todo tasks")),
        "the last prompt must be the replan continuation"
    );
    assert!(
        backend
            .state()
            .prompt
            .as_deref()
            .is_some_and(|prompt| prompt.contains("call the todo tool")),
        "the replan prompt must command the todo tool"
    );
}

#[tokio::test]
async fn replan_prompt_can_still_produce_tasks_and_workflow_runs() {
    let backend = Arc::new(FakeBackend::new(2, Vec::new()));
    backend.state().populate_tasks_after_prompt = Some((2, vec![task("second-chance")]));
    let supervisor = WorkflowSupervisor::spawn(
        contract("replan-success", 3),
        backend.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
    )
    .expect("supervisor");
    supervisor.start().await.expect("start");

    assert_eq!(backend.state().prompt_count, 2, "initial + one bounded re-prompt");
    assert_eq!(supervisor.projection().status, WorkflowStatus::Running);
    assert_eq!(
        supervisor.projection().todo.phases[0].tasks[0].id,
        "second-chance"
    );
}

#[tokio::test]
async fn paused_with_tasks_resumes_without_reprompting_and_empty_paused_resume_fails_bounded() {
    let backend = Arc::new(FakeBackend::new(2, vec![task("open")]));
    let supervisor = WorkflowSupervisor::spawn(
        contract("paused-resume", 3),
        backend.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
    )
    .expect("supervisor");
    supervisor.start().await.expect("start");
    supervisor.pause().await.expect("pause");
    assert_eq!(supervisor.projection().status, WorkflowStatus::Paused);

    supervisor.resume().await.expect("resume");
    assert_eq!(backend.state().resume_count, 1);
    assert_eq!(backend.state().prompt_count, 1, "resume with tasks must not re-prompt");
    assert_eq!(supervisor.projection().status, WorkflowStatus::Running);

    // A Paused workflow with no Todo tasks continues the bounded planning
    // flow on resume and fails instead of parking in Planning again.
    let empty = Arc::new(FakeBackend::new(2, Vec::new()));
    let empty_supervisor = WorkflowSupervisor::spawn_restored(
        contract("empty-paused", 3),
        empty.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
        WorkflowStatus::Paused,
    )
    .expect("empty supervisor");
    assert_eq!(empty_supervisor.projection().status, WorkflowStatus::Paused);
    empty_supervisor.resume().await.expect("resume empty");
    assert_eq!(empty.state().prompt_count, 2);
    assert_eq!(empty_supervisor.projection().status, WorkflowStatus::Failed);
}

#[tokio::test]
async fn pause_stops_actor_waves_resume_reuses_backend_and_cancel_drains_exact_jobs() {
    let backend = Arc::new(FakeBackend::new(2, vec![task("root-a"), task("root-b")]));
    backend.push_job("alpha", 3, "root-a", "owned-running", JobStatus::Running);
    backend.push_job("alpha", 2, "root-b", "old-generation", JobStatus::Running);
    backend.push_job("beta", 3, "root-b", "foreign-running", JobStatus::Running);
    backend.push_job("alpha", 3, "root-b", "owned-settled", JobStatus::Completed);
    let supervisor = WorkflowSupervisor::spawn(
        contract("alpha", 3),
        backend.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
    )
    .expect("supervisor");
    supervisor.start().await.expect("start");

    supervisor.pause().await.expect("pause");
    assert_eq!(supervisor.projection().status, WorkflowStatus::Paused);
    backend.set_task_status("root-a", TodoStatus::Completed);
    backend.set_dag_status(TodoDagExecutionStatus::Settled);
    supervisor
        .observe_application_event(
            3,
            job_event(workflow_job(
                "alpha",
                3,
                "root-a",
                "owned-running",
                JobStatus::Completed,
            )),
        )
        .await
        .expect("paused event");
    assert_eq!(supervisor.projection().status, WorkflowStatus::Paused);
    assert_eq!(backend.state().resume_count, 0);

    supervisor.resume().await.expect("resume");
    assert_eq!(backend.state().resume_count, 1);
    assert_eq!(supervisor.projection().status, WorkflowStatus::Running);

    let cancelled = supervisor.cancel().await.expect("cancel");
    assert_eq!(cancelled, vec!["owned-running"]);
    assert_eq!(backend.state().cancelled, vec!["owned-running"]);
    assert_eq!(supervisor.projection().status, WorkflowStatus::Cancelled);
    assert_eq!(backend.state().todo.phases[0].tasks[1].status, TodoStatus::Pending);
    assert!(supervisor.cancel().await.expect("duplicate cancel").is_empty());
}

#[tokio::test]
async fn restored_paused_supervisor_does_not_prompt_or_spawn_until_resume() {
    let backend = Arc::new(FakeBackend::new(2, vec![task("open")]));
    let supervisor = WorkflowSupervisor::spawn_restored(
        contract("restored", 12),
        backend.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
        WorkflowStatus::Paused,
    )
    .expect("restored supervisor");
    assert_eq!(supervisor.projection().status, WorkflowStatus::Paused);
    assert!(backend.state().prompt.is_none());
    assert_eq!(backend.state().resume_count, 0);

    supervisor.resume().await.expect("resume restored");
    assert_eq!(backend.state().resume_count, 1);
    assert_eq!(supervisor.projection().status, WorkflowStatus::Running);
    assert!(backend.state().prompt.is_none());

    let terminal = WorkflowSupervisor::spawn_restored(
        contract("terminal", 1),
        Arc::new(FakeBackend::new(2, vec![task("open")])),
        OrchestrationConcurrencyGate::new(2).expect("terminal gate"),
        WorkflowStatus::Completed,
    );
    assert!(terminal.is_err());
}

#[tokio::test]
async fn supervisor_failure_preserves_open_todo_and_terminal_completion_is_exact() {
    let backend = Arc::new(FakeBackend::new(2, vec![task("open")]));
    let supervisor = WorkflowSupervisor::spawn(
        contract("alpha", 4),
        backend.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
    )
    .expect("supervisor");
    supervisor.start().await.expect("start");
    supervisor
        .observe_application_event(
            4,
            ApplicationEvent::RunFailed {
                message: "supervisor crashed".to_owned(),
            },
        )
        .await
        .expect("failure handled");
    let failed = supervisor.projection();
    assert_eq!(failed.status, WorkflowStatus::Failed);
    assert_eq!(failed.failure.as_deref(), Some("supervisor crashed"));
    assert_eq!(failed.todo.phases[0].tasks[0].status, TodoStatus::Pending);

    // Execution-supervision under the plan/execution split: a completed
    // worker job for a workflow-owned Todo task makes the SUPERVISOR mark the
    // task Done (the DAG coordinator must never forge workflow-owned
    // completion — BUG-3), which settles the DAG and completes the workflow.
    let complete_backend = Arc::new(FakeBackend::new(2, vec![task("complete")]));
    let complete = WorkflowSupervisor::spawn(
        contract("complete", 1),
        complete_backend.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
    )
    .expect("complete supervisor");
    complete.start().await.expect("start complete");
    assert_eq!(complete.projection().status, WorkflowStatus::Running);
    complete
        .observe_application_event(
            1,
            job_event(workflow_job(
                "complete",
                1,
                "complete",
                "job",
                JobStatus::Completed,
            )),
        )
        .await
        .expect("terminal job event");
    assert_eq!(
        complete.projection().status,
        WorkflowStatus::Completed,
        "a completed delegated job must settle the workflow-owned DAG"
    );
    assert_eq!(
        complete.projection().todo.phases[0].tasks[0].status,
        TodoStatus::Completed,
        "the supervisor must mark the delegated task Done after its job completes"
    );

    let exact_backend = Arc::new(FakeBackend::new(2, vec![task("complete")]));
    let exact = WorkflowSupervisor::spawn(
        contract("exact", 1),
        exact_backend.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
    )
    .expect("exact supervisor");
    exact.start().await.expect("start exact");
    exact_backend.set_task_status("complete", TodoStatus::Completed);
    exact_backend.set_dag_status(TodoDagExecutionStatus::Settled);
    exact
        .observe_application_event(
            1,
            job_event(workflow_job(
                "exact",
                1,
                "complete",
                "exact-job",
                JobStatus::Completed,
            )),
        )
        .await
        .expect("exact terminal");
    assert_eq!(exact.projection().status, WorkflowStatus::Completed);
}

#[tokio::test]
async fn soft_budget_completed_worker_job_keeps_todo_open_and_dependents_wait() {
    // A Completed worker job carrying the soft-budget marker is PARTIAL work:
    // the supervisor must not mark its Todo task Done (the parent decides
    // whether to continue the worker) and must not re-arm the DAG, so the
    // task's dependents never advance on a partial result. A normal
    // Completed job for the same task still closes it and spawns dependents.
    let backend = Arc::new(FakeBackend::new(
        2,
        vec![task("partial"), task_with_deps("dependent", vec!["partial"])],
    ));
    let supervisor = WorkflowSupervisor::spawn(
        contract("soft-budget", 1),
        backend.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
    )
    .expect("supervisor");
    supervisor.start().await.expect("start");
    assert_eq!(supervisor.projection().status, WorkflowStatus::Running);

    supervisor
        .observe_application_event(
            1,
            job_event(soft_budget_job(
                "soft-budget",
                1,
                "partial",
                "partial-job",
                JobStatus::Completed,
            )),
        )
        .await
        .expect("soft-budget worker result");
    assert_eq!(
        supervisor.projection().todo.phases[0].tasks[0].status,
        TodoStatus::Pending,
        "soft-budget partial work must stay open for the parent's decision"
    );
    assert_eq!(
        backend.state().reconcile_count, 0,
        "a partial worker result must never re-arm the DAG"
    );
    assert!(
        backend.state().last_spawned_task_ids.is_empty(),
        "dependents must not advance on a partial result"
    );
    assert_eq!(
        supervisor.projection().status,
        WorkflowStatus::Running,
        "the workflow stays running while the parent decides"
    );

    // The same task closed by a NORMAL completed job marks it Done and
    // reconciles, which spawns the now-ready dependent.
    supervisor
        .observe_application_event(
            1,
            job_event(workflow_job(
                "soft-budget",
                1,
                "partial",
                "partial-job",
                JobStatus::Completed,
            )),
        )
        .await
        .expect("normal worker result");
    assert_eq!(
        supervisor.projection().todo.phases[0].tasks[0].status,
        TodoStatus::Completed,
        "a normal completed job still closes its delegated task"
    );
    assert_eq!(backend.state().reconcile_count, 1);
    assert_eq!(
        backend.state().last_spawned_task_ids,
        vec!["dependent".to_owned()],
        "the dependent advances only after the real completion"
    );
    assert_eq!(supervisor.projection().status, WorkflowStatus::Running);
}

#[tokio::test]
async fn planning_turn_plan_commit_ends_planning_while_run_continues() {
    let backend = Arc::new(FakeBackend::new(2, Vec::new()));
    let gate = PromptGate::new();
    backend.state().prompt_gate = Some(gate.clone());
    let supervisor = WorkflowSupervisor::spawn(
        contract("planning-projection", 5),
        backend.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
    )
    .expect("supervisor");

    let start = tokio::spawn({
        let supervisor = supervisor.clone();
        async move { supervisor.start().await.expect("start") }
    });
    gate.wait_started().await;
    assert_eq!(supervisor.projection().status, WorkflowStatus::Planning);

    // The planning agent creates Todo tasks while its turn is still running;
    // the projection must expose them live.
    backend.set_tasks(vec![task("mid-plan")]);
    let mid_plan_phases = backend.state().todo.phases.clone();
    supervisor
        .observe_application_event(
            5,
            ApplicationEvent::TodoUpdated {
                phases: mid_plan_phases,
                completed_tasks: Vec::new(),
            },
        )
        .await
        .expect("mid-planning Todo mutation observed");
    assert_eq!(
        supervisor.projection().todo.phases[0].tasks[0].id,
        "mid-plan",
        "Todo created mid-planning must be visible before the turn ends"
    );

    // P0-2: the first successful `todo init` commits the plan and ends
    // planning — the workflow leaves Planning (and the DAG is preserved)
    // immediately, even while the model's run continues.
    supervisor
        .observe_todo_observation(
            5,
            WorkflowSupervisorTodoObservation {
                op: Some("init".to_owned()),
                target_ids: vec!["mid-plan".to_owned()],
                is_error: false,
                error_prefix: None,
            },
        )
        .await
        .expect("plan-commit observation");
    assert_eq!(
        supervisor.projection().status,
        WorkflowStatus::Running,
        "a committed plan must leave Planning before the model run ends"
    );
    assert_eq!(
        supervisor.projection().todo.phases[0].tasks[0].id,
        "mid-plan",
        "the committed DAG must be preserved"
    );

    gate.release.cancel();
    tokio::time::timeout(Duration::from_secs(5), start)
        .await
        .expect("start did not finish")
        .expect("start failed");
    assert_eq!(supervisor.projection().status, WorkflowStatus::Running);
    assert_eq!(
        backend.state().dag_status,
        TodoDagExecutionStatus::Active,
        "the committed plan must arm Todo DAG execution"
    );
    assert_eq!(
        supervisor.projection().todo.phases[0].tasks[0].id,
        "mid-plan",
        "arming must not replace the committed DAG"
    );
}

#[tokio::test]
async fn cancel_during_blocked_planning_interrupts_and_settles_cancelled() {
    let backend = Arc::new(FakeBackend::new(2, Vec::new()));
    let gate = PromptGate::new();
    backend.state().prompt_gate = Some(gate.clone());
    let supervisor = WorkflowSupervisor::spawn(
        contract("cancel-planning", 3),
        backend.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
    )
    .expect("supervisor");

    let start = tokio::spawn({
        let supervisor = supervisor.clone();
        async move { supervisor.start().await.expect("start") }
    });
    gate.wait_started().await;
    assert_eq!(supervisor.projection().status, WorkflowStatus::Planning);

    // A user cancel while the planning prompt is blocked must NOT be deferred
    // until the provider returns (which may be never): it interrupts the turn
    // (backend.pause aborts the in-flight prompt) and settles Cancelled.
    tokio::time::timeout(Duration::from_secs(5), supervisor.cancel())
        .await
        .expect("cancel must not wait for the blocked prompt")
        .expect("cancel ok");
    assert_eq!(supervisor.projection().status, WorkflowStatus::Cancelled);
    assert_eq!(backend.state().pause_count, 1, "the in-flight prompt must be aborted");

    // Releasing the gate settles the abandoned turn; the workflow stays
    // Cancelled — never re-plans, never clobbers back to Planning.
    gate.release.cancel();
    tokio::time::timeout(Duration::from_secs(5), start)
        .await
        .expect("start settles")
        .expect("start ok");
    assert_eq!(supervisor.projection().status, WorkflowStatus::Cancelled);
    assert_eq!(
        backend.state().prompt_count, 1,
        "a cancelled workflow must never re-prompt"
    );
}

#[tokio::test]
async fn activity_feed_tracks_thinking_tool_and_irc_and_is_generation_gated() {
    let backend = Arc::new(FakeBackend::new(2, vec![task("open")]));
    let supervisor = WorkflowSupervisor::spawn(
        contract("activity", 4),
        backend.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
    )
    .expect("supervisor");

    supervisor
        .observe_activity(4, WorkflowSupervisorActivityKind::Thinking, "weigh options")
        .await
        .expect("thinking activity");
    supervisor
        .observe_activity(4, WorkflowSupervisorActivityKind::Tool, "read tools.rs")
        .await
        .expect("tool activity");

    // IRC deliveries also land in the activity feed (progress projection).
    let message = MailboxMessage {
        id: "m-1".to_owned(),
        from: "Supervisor-activity".to_owned(),
        to: "Main".to_owned(),
        body: "still investigating".to_owned(),
        timestamp: 7,
        reply_to: None,
    };
    // Mirror the real backend: the delivered message sits in the agent's
    // inbox (the projection reads it back through `inbox`).
    backend.state().irc.push(message.clone());
    supervisor
        .observe_application_event(
            4,
            ApplicationEvent::Orchestration(OrchestrationEvent::MessageDelivered {
                group_id: "g".to_owned(),
                message,
            }),
        )
        .await
        .expect("irc activity");

    let projection = supervisor.projection();
    assert_eq!(projection.activity.len(), 3, "thinking + tool + irc entries");
    assert_eq!(projection.activity[0].kind, WorkflowSupervisorActivityKind::Thinking);
    assert_eq!(projection.activity[0].text, "weigh options");
    assert_eq!(projection.activity[1].kind, WorkflowSupervisorActivityKind::Tool);
    assert_eq!(projection.activity[1].text, "read tools.rs");
    assert_eq!(projection.activity[2].kind, WorkflowSupervisorActivityKind::Irc);
    assert_eq!(projection.activity[2].text, "still investigating");
    assert_eq!(projection.irc.len(), 1, "IRC stays in the inbox projection too");

    // Foreign generations are rejected (never occupy command capacity).
    supervisor
        .observe_activity(99, WorkflowSupervisorActivityKind::Tool, "stale")
        .await
        .expect("stale activity accepted");
    assert_eq!(supervisor.projection().activity.len(), 3);
}

#[tokio::test]
async fn restored_planning_continues_automatically_and_runs() {
    let backend = Arc::new(FakeBackend::new(2, vec![task("open")]));
    let supervisor = WorkflowSupervisor::spawn_restored(
        contract("restored-planning", 8),
        backend.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
        WorkflowStatus::Planning,
    )
    .expect("restored planning");
    assert_eq!(supervisor.projection().status, WorkflowStatus::Planning);
    assert!(backend.state().prompt.is_none(), "restore itself must not prompt");

    supervisor.continue_restored().await.expect("restore continuation");
    assert_eq!(
        backend.state().prompt_count,
        1,
        "restored planning continues with the planning prompt"
    );
    assert_eq!(supervisor.projection().status, WorkflowStatus::Running);
    assert_eq!(supervisor.projection().todo.phases[0].tasks[0].id, "open");
}

#[tokio::test]
async fn restored_empty_planning_fails_bounded_instead_of_freezing() {
    let backend = Arc::new(FakeBackend::new(2, Vec::new()));
    let supervisor = WorkflowSupervisor::spawn_restored(
        contract("restored-empty", 8),
        backend.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
        WorkflowStatus::Planning,
    )
    .expect("restored planning");
    supervisor.continue_restored().await.expect("restore continuation");
    assert_eq!(
        supervisor.projection().status,
        WorkflowStatus::Failed,
        "restored empty planning must not freeze in Planning"
    );
    assert_eq!(backend.state().prompt_count, 2);
}

#[tokio::test]
async fn restored_running_rearms_dag_execution_and_resume_acts_from_planning_and_running() {
    // Restored Running: continuation re-arms DAG execution over stored tasks.
    let backend = Arc::new(FakeBackend::new(2, vec![task("open")]));
    let supervisor = WorkflowSupervisor::spawn_restored(
        contract("restored-running", 9),
        backend.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
        WorkflowStatus::Running,
    )
    .expect("restored running");
    assert_eq!(supervisor.projection().status, WorkflowStatus::Running);
    supervisor.continue_restored().await.expect("restore continuation");
    assert_eq!(
        backend.state().dag_status,
        TodoDagExecutionStatus::Active,
        "restored Running must execute the Todo DAG over stored tasks"
    );
    assert_eq!(backend.state().prompt_count, 0, "Running continuation must not re-prompt");
    assert_eq!(supervisor.projection().status, WorkflowStatus::Running);

    // /workflow resume is effective for restored Planning.
    let planning = Arc::new(FakeBackend::new(2, vec![task("open")]));
    let planning_supervisor = WorkflowSupervisor::spawn_restored(
        contract("resume-planning", 9),
        planning.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
        WorkflowStatus::Planning,
    )
    .expect("restored planning");
    planning_supervisor.resume().await.expect("resume from planning");
    assert_eq!(planning.state().prompt_count, 1);
    assert_eq!(planning_supervisor.projection().status, WorkflowStatus::Running);

    // /workflow resume is effective for restored Running.
    let running = Arc::new(FakeBackend::new(2, vec![task("open")]));
    let running_supervisor = WorkflowSupervisor::spawn_restored(
        contract("resume-running", 9),
        running.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
        WorkflowStatus::Running,
    )
    .expect("restored running");
    running_supervisor.resume().await.expect("resume from running");
    assert_eq!(
        running.state().dag_status,
        TodoDagExecutionStatus::Active,
        "resume from Running must re-arm the Todo DAG"
    );
    assert_eq!(running_supervisor.projection().status, WorkflowStatus::Running);
}

#[tokio::test]
async fn planning_budget_reached_preserves_committed_dag_and_runs() {
    // A provider that exhausts the planning budget AFTER committing a valid
    // DAG: the workflow must preserve the DAG, arm DAG execution, and move to
    // Running instead of failing (P0-1).
    let backend = Arc::new(FakeBackend::new(2, vec![task("committed-root")]));
    backend
        .state()
        .prompt_outcomes
        .push_back(PlanningTurnOutcome::PlanBudgetReached {
            reason: "planning exceeded the bound (8 assistant turns / 8 tool calls)".to_owned(),
        });
    let supervisor = WorkflowSupervisor::spawn(
        contract("budget-dag", 6),
        backend.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
    )
    .expect("supervisor");
    supervisor.start().await.expect("start");

    assert_eq!(supervisor.projection().status, WorkflowStatus::Running);
    assert_eq!(
        backend.state().dag_status,
        TodoDagExecutionStatus::Active,
        "a preserved plan must arm Todo DAG execution"
    );
    assert_eq!(
        supervisor.projection().todo.phases[0].tasks[0].id,
        "committed-root",
        "the committed DAG must be preserved across the budget stop"
    );
}

#[tokio::test]
async fn planning_budget_reached_without_tasks_fails_with_bound_name() {
    // A provider that exhausts the planning budget WITHOUT committing any
    // tasks: the workflow fails with an actionable reason naming the bound
    // (P0-1), never lingering in Planning.
    let backend = Arc::new(FakeBackend::new(2, Vec::new()));
    backend
        .state()
        .prompt_outcomes
        .push_back(PlanningTurnOutcome::PlanBudgetReached {
            reason: "planning exceeded the bound (8 assistant turns / 8 tool calls)".to_owned(),
        });
    let supervisor = WorkflowSupervisor::spawn(
        contract("budget-empty", 6),
        backend.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
    )
    .expect("supervisor");
    supervisor.start().await.expect("start");

    let failed = supervisor.projection();
    assert_eq!(failed.status, WorkflowStatus::Failed);
    assert!(
        failed.failure.as_deref().is_some_and(|message| {
            message.contains("planning stopped without a committed plan")
                && message.contains("planning exceeded the bound")
        }),
        "failure must name the tripped bound, got {:?}",
        failed.failure
    );
}

#[tokio::test]
async fn planning_wall_clock_deadline_aborts_and_settles_from_todo_state() {
    // With tasks: an unresponsive provider (the prompt never returns) is
    // aborted by the wall-clock deadline; the committed DAG is preserved and
    // armed, and the workflow runs (P0-1).
    let with_tasks = Arc::new(FakeBackend::new(2, vec![task("deadline-root")]));
    with_tasks.state().planning_deadline = Some(Duration::from_millis(150));
    with_tasks.state().prompt_gate = Some(PromptGate::new());
    let with_tasks_supervisor = WorkflowSupervisor::spawn(
        contract("deadline-dag", 7),
        with_tasks.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
    )
    .expect("supervisor");
    let start = tokio::spawn({
        let supervisor = with_tasks_supervisor.clone();
        async move { supervisor.start().await.expect("start") }
    });
    tokio::time::timeout(Duration::from_secs(5), start)
        .await
        .expect("deadline must settle the workflow")
        .expect("start failed");
    assert_eq!(
        with_tasks_supervisor.projection().status,
        WorkflowStatus::Running,
        "a preserved DAG must run after the deadline aborts the prompt"
    );
    assert_eq!(
        with_tasks_supervisor.projection().todo.phases[0].tasks[0].id,
        "deadline-root",
        "the committed DAG must be preserved across the deadline abort"
    );
    assert_eq!(
        with_tasks.state().dag_status,
        TodoDagExecutionStatus::Active,
        "the preserved DAG must be armed"
    );
    assert!(
        with_tasks.state().aborted_while_prompting,
        "the deadline must abort the in-flight prompt"
    );

    // Without tasks: the same deadline fails the workflow naming the timeout.
    let empty = Arc::new(FakeBackend::new(2, Vec::new()));
    empty.state().planning_deadline = Some(Duration::from_millis(150));
    empty.state().prompt_gate = Some(PromptGate::new());
    let empty_supervisor = WorkflowSupervisor::spawn(
        contract("deadline-empty", 7),
        empty.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
    )
    .expect("supervisor");
    let start = tokio::spawn({
        let supervisor = empty_supervisor.clone();
        async move { supervisor.start().await.expect("start") }
    });
    tokio::time::timeout(Duration::from_secs(5), start)
        .await
        .expect("deadline must settle the workflow")
        .expect("start failed");
    let failed = empty_supervisor.projection();
    assert_eq!(failed.status, WorkflowStatus::Failed);
    assert!(
        failed
            .failure
            .as_deref()
            .is_some_and(|message| message.contains("timed out")),
        "failure must name the timeout, got {:?}",
        failed.failure
    );
    assert!(empty.state().aborted_while_prompting);
}

#[tokio::test]
async fn planning_deadline_is_total_budget_not_reset_by_activity_deltas() {
    // Regression for the per-iteration sleep reset: `await_planning_turn`
    // used to recreate `tokio::time::sleep(deadline)` on every select
    // iteration, so each forwarded Activity/TodoObservation/ApplicationEvent
    // command restarted the countdown. A provider that streams keepalive
    // deltas more often than the deadline but never completes the turn held
    // the workflow in Planning forever. The sleep is now pinned once before
    // the loop, making the deadline a true total budget: the turn must abort
    // at ~deadline even while deltas keep arriving.
    let deadline = Duration::from_millis(300);
    let backend = Arc::new(FakeBackend::new(2, Vec::new()));
    backend.state().planning_deadline = Some(deadline);
    // The planning prompt never returns (a stuck/stalled provider).
    backend.state().prompt_gate = Some(PromptGate::new());
    let supervisor = WorkflowSupervisor::spawn(
        contract("deadline-activity", 7),
        backend.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
    )
    .expect("supervisor");
    let start = tokio::spawn({
        let supervisor = supervisor.clone();
        async move { supervisor.start().await.expect("start") }
    });
    // Feed thinking deltas at intervals well inside the budget for the whole
    // observation window. Pre-fix each delta reset the countdown, so the
    // workflow never settled while the stream was alive; post-fix it settles
    // at ~deadline regardless.
    let delta_interval = Duration::from_millis(50);
    let began = std::time::Instant::now();
    loop {
        supervisor
            .observe_activity(7, WorkflowSupervisorActivityKind::Thinking, "delta")
            .await
            .expect("activity observed");
        if supervisor.projection().status != WorkflowStatus::Planning {
            break;
        }
        assert!(
            began.elapsed() < Duration::from_secs(3),
            "planning must settle at the total wall-clock budget even while activity deltas \
             keep arriving (elapsed {:?})",
            began.elapsed()
        );
        tokio::time::sleep(delta_interval).await;
    }
    tokio::time::timeout(Duration::from_secs(5), start)
        .await
        .expect("deadline must settle the workflow")
        .expect("start failed");
    assert_eq!(
        supervisor.projection().status,
        WorkflowStatus::Failed,
        "an empty planning turn must fail when the total budget expires"
    );
    assert!(
        supervisor
            .projection()
            .failure
            .as_deref()
            .is_some_and(|message| message.contains("timed out")),
        "failure must name the timeout, got {:?}",
        supervisor.projection().failure
    );
    assert!(
        backend.state().aborted_while_prompting,
        "the deadline must abort the in-flight prompt despite the delta stream"
    );
}

#[tokio::test]
async fn planning_non_progress_detection_aborts_after_identical_failed_ops() {
    // Three identical failed Todo operations (same op + target IDs + error
    // prefix) with no Todo-state change: the session doom-loop detector would
    // catch this too, but the semantic detector must terminate planning with
    // an actionable reason (P1-2).
    let backend = Arc::new(FakeBackend::new(2, Vec::new()));
    backend.state().prompt_gate = Some(PromptGate::new());
    let supervisor = WorkflowSupervisor::spawn(
        contract("non-progress-identical", 7),
        backend.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
    )
    .expect("supervisor");
    let start = tokio::spawn({
        let supervisor = supervisor.clone();
        async move { supervisor.start().await.expect("start") }
    });
    let observation = WorkflowSupervisorTodoObservation {
        op: Some("update_dependencies".to_owned()),
        target_ids: vec!["task-1".to_owned(), "task-2".to_owned()],
        is_error: true,
        error_prefix: Some("Task ID \"task-2\" not found".to_owned()),
    };
    // Deliver the observations only once the planning prompt is actually in
    // flight: pre-planning deliveries would be dropped by the actor's
    // planning guard and the detector would never see them.
    let gate = backend.state().prompt_gate.clone().expect("gate");
    gate.wait_started().await;
    for _ in 0..3 {
        supervisor
            .observe_todo_observation(7, observation.clone())
            .await
            .expect("observation");
    }
    tokio::time::timeout(Duration::from_secs(5), start)
        .await
        .expect("non-progress detection must settle the workflow")
        .expect("start failed");
    let failed = supervisor.projection();
    assert_eq!(failed.status, WorkflowStatus::Failed);
    assert!(
        failed
            .failure
            .as_deref()
            .is_some_and(|message| message.contains("planning is not converging")),
        "failure must name non-convergence, got {:?}",
        failed.failure
    );
    assert!(
        backend.state().aborted_while_prompting,
        "non-progress detection must abort the in-flight prompt"
    );
}

#[tokio::test]
async fn planning_non_progress_detection_trips_on_varying_failed_ids() {
    // Six failed Todo corrections with VARYING target IDs/error text (which
    // the session identical-error detector cannot see) and no Todo-state
    // change: the semantic detector still terminates planning (P1-2).
    let backend = Arc::new(FakeBackend::new(2, Vec::new()));
    backend.state().prompt_gate = Some(PromptGate::new());
    let supervisor = WorkflowSupervisor::spawn(
        contract("non-progress-varying", 7),
        backend.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
    )
    .expect("supervisor");
    let start = tokio::spawn({
        let supervisor = supervisor.clone();
        async move { supervisor.start().await.expect("start") }
    });
    // Deliver the observations only once the planning prompt is actually in
    // flight (see the identical-op test for the ordering rationale).
    let gate = backend.state().prompt_gate.clone().expect("gate");
    gate.wait_started().await;
    for attempt in 0..6 {
        supervisor
            .observe_todo_observation(
                7,
                WorkflowSupervisorTodoObservation {
                    op: Some("update_dependencies".to_owned()),
                    target_ids: vec![format!("task-{attempt}")],
                    is_error: true,
                    error_prefix: Some(format!("Task ID \"task-{attempt}\" not found")),
                },
            )
            .await
            .expect("observation");
    }
    tokio::time::timeout(Duration::from_secs(5), start)
        .await
        .expect("non-progress detection must settle the workflow")
        .expect("start failed");
    let failed = supervisor.projection();
    assert_eq!(failed.status, WorkflowStatus::Failed);
    assert!(
        failed
            .failure
            .as_deref()
            .is_some_and(|message| message.contains("planning is not converging")),
        "failure must name non-convergence, got {:?}",
        failed.failure
    );
    assert!(backend.state().aborted_while_prompting);
}

#[tokio::test]
async fn planning_non_progress_tolerates_legit_init_and_mutations() {
    // A legitimate init + successful dependency sequence changes the
    // canonical Todo state on every call: the semantic detector must NOT trip
    // (P1-2) — Todo-state progress is the decisive signal.
    let backend = Arc::new(FakeBackend::new(2, Vec::new()));
    let gate = PromptGate::new();
    backend.state().prompt_gate = Some(gate.clone());
    let supervisor = WorkflowSupervisor::spawn(
        contract("legit-progress", 7),
        backend.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
    )
    .expect("supervisor");
    let start = tokio::spawn({
        let supervisor = supervisor.clone();
        async move { supervisor.start().await.expect("start") }
    });
    gate.wait_started().await;

    backend.set_tasks(vec![task("root")]);
    supervisor
        .observe_todo_observation(
            7,
            WorkflowSupervisorTodoObservation {
                op: Some("init".to_owned()),
                target_ids: vec!["root".to_owned()],
                is_error: false,
                error_prefix: None,
            },
        )
        .await
        .expect("init observation");
    // Each subsequent successful mutation changes the Todo state.
    for index in 0..8 {
        let mut tasks = backend.state().todo.phases[0].tasks.clone();
        tasks.push(task(&format!("task-{index}")));
        backend.set_tasks(tasks);
        supervisor
            .observe_todo_observation(
                7,
                WorkflowSupervisorTodoObservation {
                    op: Some("update_dependencies".to_owned()),
                    target_ids: vec![format!("task-{index}")],
                    is_error: false,
                    error_prefix: None,
                },
            )
            .await
            .expect("mutation observation");
    }

    gate.release.cancel();
    tokio::time::timeout(Duration::from_secs(5), start)
        .await
        .expect("legit planning must settle")
        .expect("start failed");
    assert_eq!(supervisor.projection().status, WorkflowStatus::Running);
    assert!(
        !backend.state().aborted_while_prompting,
        "legitimate progress must never trip non-progress detection"
    );
    assert_eq!(
        backend.state().prompt_count, 1,
        "legitimate planning must not be aborted or re-prompted"
    );
}

#[tokio::test]
async fn plan_committed_outcome_arms_dag_and_runs() {
    // A prompt whose run was terminated by the plan-commit stop hook: the
    // outcome is PlanCommitted and the workflow arms the DAG and runs.
    let backend = Arc::new(FakeBackend::new(2, Vec::new()));
    backend
        .state()
        .prompt_outcomes
        .push_back(PlanningTurnOutcome::PlanCommitted);
    backend.set_tasks(vec![task("committed")]);
    let supervisor = WorkflowSupervisor::spawn(
        contract("plan-committed", 7),
        backend.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
    )
    .expect("supervisor");
    supervisor.start().await.expect("start");

    assert_eq!(supervisor.projection().status, WorkflowStatus::Running);
    assert_eq!(
        backend.state().dag_status,
        TodoDagExecutionStatus::Active,
        "a committed plan must arm Todo DAG execution"
    );
    assert_eq!(supervisor.projection().todo.phases[0].tasks[0].id, "committed");
}
