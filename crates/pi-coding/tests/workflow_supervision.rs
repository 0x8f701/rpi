use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use parking_lot::Mutex;
use pi_coding::{
    ApplicationEvent, JobSnapshot, JobStatus, MailboxMessage, OrchestrationConcurrencyGate,
    OrchestrationEvent, TodoDagExecutionOutcome, TodoDagExecutionStatus, TodoItem, TodoPhase,
    TodoState, TodoStatus, TodoStorage, WorkflowJobSnapshot, WorkflowRuntimeScope, WorkflowStatus,
    WorkflowSupervisor, WorkflowSupervisorBackend, WorkflowSupervisorContract,
};

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
            })),
        }
    }

    fn set_dag_status(&self, status: TodoDagExecutionStatus) {
        self.state.lock().dag_status = status;
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

    async fn prompt_supervisor(&self, prompt: String) -> Result<()> {
        self.state.lock().prompt = Some(prompt);
        Ok(())
    }

    async fn steer_supervisor(&self, message: String) -> Result<()> {
        self.state.lock().steers.push(message);
        Ok(())
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
    }
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
        },
    }
}

fn job_event(job: WorkflowJobSnapshot) -> ApplicationEvent {
    ApplicationEvent::Orchestration(OrchestrationEvent::JobUpdated {
        group_id: format!("group-{}", job.workflow_id),
        job: job.job,
    })
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
async fn paused_empty_todo_resume_stays_planning_without_starting_dag() {
    let backend = Arc::new(FakeBackend::new(2, Vec::new()));
    let supervisor = WorkflowSupervisor::spawn(
        contract("planning", 3),
        backend.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
    )
    .expect("supervisor");
    supervisor.start().await.expect("start");
    assert_eq!(supervisor.projection().status, WorkflowStatus::Planning);

    supervisor.pause().await.expect("pause");
    assert_eq!(supervisor.projection().status, WorkflowStatus::Paused);
    supervisor.resume().await.expect("resume");
    assert_eq!(supervisor.projection().status, WorkflowStatus::Planning);
    assert_eq!(backend.state().resume_count, 0);
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

    let complete_backend = Arc::new(FakeBackend::new(2, vec![task("complete")]));
    let complete = WorkflowSupervisor::spawn(
        contract("complete", 1),
        complete_backend.clone(),
        OrchestrationConcurrencyGate::new(2).expect("global gate"),
    )
    .expect("complete supervisor");
    complete.start().await.expect("start complete");
    complete_backend.set_dag_status(TodoDagExecutionStatus::Settled);
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
        .expect("unapplied terminal");
    assert_eq!(
        complete.projection().status,
        WorkflowStatus::Failed,
        "settled jobs cannot complete a workflow while canonical Todo remains open"
    );
    assert_eq!(
        complete.projection().todo.phases[0].tasks[0].status,
        TodoStatus::Pending,
        "actor must never forge Todo completion; backend coordinator owns it"
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
