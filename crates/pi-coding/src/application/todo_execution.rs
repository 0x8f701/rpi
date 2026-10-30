use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use anyhow::{Result, anyhow};
use parking_lot::Mutex;

use super::{Application, ApplicationEvent, ApplicationInner};
use crate::{
    JobSnapshot, JobStatus, OrchestrationEvent, OrchestrationRuntime, TaskItem, TaskSpawn, TodoOp,
    TodoPhase, TodoStatus,
};

pub use crate::{TodoDagExecutionStatus, TodoDagFailureReason};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TodoDagExecutionOutcome {
    pub status: TodoDagExecutionStatus,
    pub failure_reason: Option<TodoDagFailureReason>,
    pub spawns: Vec<TaskSpawn>,
}

#[derive(Clone)]
struct OwnedTodoJob {
    generation: u64,
    task_id: String,
}

#[derive(Clone)]
pub(super) struct TodoDagCoordinatorState {
    status: TodoDagExecutionStatus,
    failure_reason: Option<TodoDagFailureReason>,
    generation: u64,
    attempted_task_ids: HashSet<String>,
    handled_terminal_job_ids: HashSet<String>,
    owned_jobs: HashMap<String, OwnedTodoJob>,
}

#[derive(Default)]
pub(super) struct TodoDagCoordinator {
    pub(super) status: TodoDagExecutionStatus,
    pub(super) failure_reason: Option<TodoDagFailureReason>,
    generation: u64,
    attempted_task_ids: HashSet<String>,
    handled_terminal_job_ids: HashSet<String>,
    owned_jobs: HashMap<String, OwnedTodoJob>,
    pub(super) reconcile_gate: Arc<Mutex<()>>,
}

impl TodoDagCoordinator {
    pub(super) fn restore_from_session(&mut self, session: &crate::Session) {
        self.restore_from_session_with_jobs(session, &[]);
    }

    pub(super) fn restore_from_session_with_jobs(
        &mut self,
        session: &crate::Session,
        jobs: &[JobSnapshot],
    ) {
        let (mut status, mut failure_reason) = session.todo_execution();
        if jobs.iter().any(|job| {
            job.todo_task_id.is_some()
                && matches!(job.status, JobStatus::Queued | JobStatus::Running)
        }) {
            status = TodoDagExecutionStatus::Active;
            failure_reason = None;
        }
        self.status = status;
        self.failure_reason = failure_reason;
        self.generation = 0;
        self.attempted_task_ids.clear();
        self.handled_terminal_job_ids.clear();
        self.owned_jobs.clear();
        self.adopt_orchestration_jobs(jobs);
    }

    pub(super) fn adopt_orchestration_jobs(&mut self, jobs: &[JobSnapshot]) {
        for job in jobs {
            let Some(task_id) = job.todo_task_id.as_ref() else {
                continue;
            };
            // Durable snapshots are authoritative correlation records even
            // when the child settled during recovery. Re-own every correlated
            // job so reconcile can apply a terminal completion exactly once;
            // pre-marking terminal jobs handled would drop that completion and
            // make the still-InProgress task eligible for a duplicate spawn.
            self.attempted_task_ids.insert(task_id.clone());
            self.owned_jobs.insert(
                job.id.clone(),
                OwnedTodoJob {
                    generation: self.generation,
                    task_id: task_id.clone(),
                },
            );
        }
    }

    pub(super) fn snapshot_state(&self) -> TodoDagCoordinatorState {
        TodoDagCoordinatorState {
            status: self.status,
            failure_reason: self.failure_reason.clone(),
            generation: self.generation,
            attempted_task_ids: self.attempted_task_ids.clone(),
            handled_terminal_job_ids: self.handled_terminal_job_ids.clone(),
            owned_jobs: self.owned_jobs.clone(),
        }
    }

    pub(super) fn restore_state(&mut self, state: TodoDagCoordinatorState) {
        self.status = state.status;
        self.failure_reason = state.failure_reason;
        self.generation = state.generation;
        self.attempted_task_ids = state.attempted_task_ids;
        self.handled_terminal_job_ids = state.handled_terminal_job_ids;
        self.owned_jobs = state.owned_jobs;
    }

    #[cfg(test)]
    pub(super) fn owns_job_for_test(&self, job_id: &str) -> bool {
        self.owned_jobs.contains_key(job_id)
    }

    #[cfg(test)]
    pub(super) fn state_eq_for_test(&self, other: &TodoDagCoordinatorState) -> bool {
        self.status == other.status
            && self.failure_reason == other.failure_reason
            && self.generation == other.generation
            && self.attempted_task_ids == other.attempted_task_ids
            && self.handled_terminal_job_ids == other.handled_terminal_job_ids
            && self.owned_jobs.keys().collect::<HashSet<_>>()
                == other.owned_jobs.keys().collect::<HashSet<_>>()
    }

    #[cfg(test)]
    pub(super) fn seed_nontrivial_state_for_test(&mut self) {
        self.generation = 7;
        self.attempted_task_ids = HashSet::from(["attempted-task".to_owned()]);
        self.handled_terminal_job_ids = HashSet::from(["handled-job".to_owned()]);
        self.owned_jobs.insert(
            "owned-job".to_owned(),
            OwnedTodoJob {
                generation: 7,
                task_id: "owned-task".to_owned(),
            },
        );
    }

    #[cfg(test)]
    pub(super) fn generation_for_test(&self) -> u64 {
        self.generation
    }




}

fn persist_coordinator_status(
    session: &crate::Session,
    coordinator: &TodoDagCoordinator,
) -> Result<()> {
    session.persist_todo_execution(coordinator.status, coordinator.failure_reason.clone())
}

pub(super) fn persist_coordinator_status_for_runtime(
    active: &super::runtime::ApplicationRuntime,
    coordinator: &TodoDagCoordinator,
) -> Result<()> {
    persist_coordinator_status(&active.session, coordinator)
}

fn rollback_spawn_phases<Set, Restore>(
    previous_phases: Vec<TodoPhase>,
    error: anyhow::Error,
    set: Set,
    restore: Restore,
) -> Result<anyhow::Error>
where
    Set: FnOnce(Vec<TodoPhase>) -> Result<()>,
    Restore: FnOnce(Vec<TodoPhase>) -> Result<()>,
{
    match set(previous_phases.clone()) {
        Ok(()) => Ok(error),
        Err(rollback_error) => {
            restore(previous_phases).map_err(|restore_error| {
                anyhow!(
                    "{error}; additionally failed to restore Todo state: {rollback_error}; and the in-memory restore also failed: {restore_error}"
                )
            })?;
            Ok(anyhow!(
                "{error}; additionally failed to restore Todo state: {rollback_error}"
            ))
        }
    }
}

impl Application {
    /// Explicitly start or restart parent-owned execution of the canonical Todo DAG.
    pub fn execute_todo_dag(&self) -> Result<TodoDagExecutionOutcome> {
        let runtime = self
            .orchestration_runtime()
            .ok_or_else(|| anyhow!("application orchestration is not configured"))?;
        self.inner
            .start_todo_dag_cycle(&runtime, true)
            .inspect_err(|error| self.inner.todo_dag_failed(error))
    }

    /// Re-evaluate an already armed execution without creating execution intent.
    pub fn reconcile_todo_dag_if_armed(&self) -> Result<TodoDagExecutionOutcome> {
        if self.todo_dag_status() == TodoDagExecutionStatus::Dormant {
            return Ok(TodoDagExecutionOutcome {
                status: TodoDagExecutionStatus::Dormant,
                failure_reason: None,
                spawns: Vec::new(),
            });
        }
        let runtime = self
            .orchestration_runtime()
            .ok_or_else(|| anyhow!("application orchestration is not configured"))?;
        self.inner
            .reconcile_todo_dag(&runtime, true)
            .inspect_err(|error| self.inner.todo_dag_failed(error))
    }

    #[must_use]
    pub fn todo_dag_status(&self) -> TodoDagExecutionStatus {
        self.runtime().todo_dag.lock().status
    }

    #[must_use]
    pub fn todo_dag_failure_reason(&self) -> Option<TodoDagFailureReason> {
        self.runtime().todo_dag.lock().failure_reason.clone()
    }

    pub async fn wait_todo_dag(&self) -> TodoDagExecutionStatus {
        let active = self.runtime();
        loop {
            let changed = active.todo_dag_changed.notified();
            let status = self.todo_dag_status();
            if status.is_terminal() {
                return status;
            }
            changed.await;
        }
    }

    #[must_use]
    pub fn todo_dag_outcome(&self) -> TodoDagExecutionOutcome {
        let active = self.runtime();
        let coordinator = active.todo_dag.lock();
        TodoDagExecutionOutcome {
            status: coordinator.status,
            failure_reason: coordinator.failure_reason.clone(),
            spawns: Vec::new(),
        }
    }
}

impl ApplicationInner {
    pub(super) fn arm_todo_dag(
        &self,
        runtime: &OrchestrationRuntime,
        reset_attempts: bool,
    ) -> Result<TodoDagExecutionOutcome> {
        let gate = self.todo_dag_transaction_gate();
        let _reconcile = gate.lock();
        self.arm_todo_dag_locked(runtime, reset_attempts)
    }

    pub(super) fn todo_dag_transaction_gate(&self) -> Arc<Mutex<()>> {
        self.runtime().todo_dag.lock().reconcile_gate.clone()
    }

    pub(super) fn arm_todo_dag_locked(
        &self,
        runtime: &OrchestrationRuntime,
        reset_attempts: bool,
    ) -> Result<TodoDagExecutionOutcome> {
        let active = self.runtime();
        if active
            .todo_transition_active
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(anyhow!("Todo execution rejected during session transition"));
        }
        let jobs = runtime.jobs(None);
        {
            let mut coordinator = active.todo_dag.lock();
            coordinator.status = TodoDagExecutionStatus::Active;
            coordinator.failure_reason = None;
            persist_coordinator_status(&active.session, &coordinator)?;
            if reset_attempts {
                let active_job_ids = jobs
                    .iter()
                    .filter(|job| matches!(job.status, JobStatus::Queued | JobStatus::Running))
                    .map(|job| job.id.as_str())
                    .collect::<HashSet<_>>();
                coordinator.attempted_task_ids.clear();
                let active_task_ids = coordinator
                    .owned_jobs
                    .iter()
                    .filter_map(|(job_id, owned)| {
                        active_job_ids
                            .contains(job_id.as_str())
                            .then(|| owned.task_id.clone())
                    })
                    .collect::<Vec<_>>();
                coordinator.attempted_task_ids.extend(active_task_ids);
            }
        }
        active.todo_dag_changed.notify_waiters();
        self.reconcile_todo_dag_locked(runtime, true)
    }
    pub(super) fn begin_todo_dag_transition(
        &self,
        runtime: &OrchestrationRuntime,
    ) -> Result<Vec<String>> {
        let gate = self.todo_dag_transaction_gate();
        let _reconcile = gate.lock();
        self.begin_todo_dag_transition_locked(runtime)
    }

    pub(super) fn begin_todo_dag_transition_locked(
        &self,
        runtime: &OrchestrationRuntime,
    ) -> Result<Vec<String>> {
        let active_runtime = self.runtime();
        let active_status = runtime
            .jobs(None)
            .into_iter()
            .map(|job| (job.id, job.status))
            .collect::<HashMap<_, _>>();
        let mut coordinator = active_runtime.todo_dag.lock();
        let previous_status = coordinator.status;
        let previous_failure = coordinator.failure_reason.clone();
        let previous_generation = coordinator.generation;
        let previous_attempted = coordinator.attempted_task_ids.clone();
        let previous_handled = coordinator.handled_terminal_job_ids.clone();
        let previous_owned = coordinator.owned_jobs.clone();
        let generation = coordinator.generation;
        let mut active = coordinator
            .owned_jobs
            .iter()
            .filter_map(|(job_id, owned)| {
                (owned.generation == generation
                    && active_status.get(job_id).is_some_and(|status| {
                        matches!(status, JobStatus::Queued | JobStatus::Running)
                    }))
                .then(|| job_id.clone())
            })
            .collect::<Vec<_>>();
        active.sort();
        coordinator.generation = coordinator.generation.wrapping_add(1);
        coordinator.status = TodoDagExecutionStatus::Dormant;
        coordinator.failure_reason = None;
        coordinator.attempted_task_ids.clear();
        coordinator.handled_terminal_job_ids.clear();
        coordinator.owned_jobs.clear();
        drop(coordinator);
        active_runtime.todo_dag_changed.notify_waiters();
        let _ = (previous_status, previous_failure, previous_generation, previous_attempted, previous_handled, previous_owned);
        Ok(active)
    }

    pub(super) fn observe_orchestration_event(
        &self,
        runtime: &OrchestrationRuntime,
        event: &OrchestrationEvent,
        allow_spawn: bool,
    ) -> Result<TodoDagExecutionStatus> {
        let gate = self.runtime().todo_dag.lock().reconcile_gate.clone();
        let _reconcile = gate.lock();
        let OrchestrationEvent::JobUpdated { job, .. } = event else {
            return Ok(self.runtime().todo_dag.lock().status);
        };
        if !job.status.is_settled() {
            return Ok(self.runtime().todo_dag.lock().status);
        }
        self.record_terminal_todo_job(job)?;
        let status = self.runtime().todo_dag.lock().status;
        if status == TodoDagExecutionStatus::Dormant {
            return Ok(TodoDagExecutionStatus::Dormant);
        }
        // Workflow children: the supervisor owns canonical Todo DAG execution
        // (BUG-1). The event loop only records the terminal job so the
        // forwarded `JobUpdated` observation drives the supervisor's explicit
        // Done-marking and dependent spawning; it must never auto-rearm the
        // DAG here; that double-spawns every ready task and races the
        // supervisor's `execute_todo_dag`/`reconcile_todo_dag` (and would arm a
        // mutation's replaced DAG, forging foreign task linkage).
        if runtime.workflow_scope().is_some() {
            return Ok(status);
        }
        Ok(self.reconcile_todo_dag_locked(runtime, allow_spawn)?.status)
    }

    pub(super) fn record_terminal_todo_job_for_recovery(
        &self,
        active: &super::runtime::ApplicationRuntime,
        job: &JobSnapshot,
    ) -> Result<()> {
        self.record_terminal_todo_job_for_runtime(active, job)
    }

    fn record_terminal_todo_job(&self, job: &JobSnapshot) -> Result<()> {
        let active = self.runtime();
        self.record_terminal_todo_job_for_runtime(&active, job)
    }

    fn record_terminal_todo_job_for_runtime(
        &self,
        active: &super::runtime::ApplicationRuntime,
        job: &JobSnapshot,
    ) -> Result<()> {
        let ownership = {
            let mut coordinator = active.todo_dag.lock();
            if coordinator.handled_terminal_job_ids.contains(&job.id) {
                return Ok(());
            }
            let Some(owned) = coordinator.owned_jobs.get(&job.id) else {
                return Ok(());
            };
            if owned.generation != coordinator.generation {
                return Ok(());
            }
            let owned = coordinator.owned_jobs.remove(&job.id).ok_or_else(|| {
                anyhow!("owned todo job {} was absent at terminal record", job.id)
            })?;
            coordinator.handled_terminal_job_ids.insert(job.id.clone());
            if job.status != JobStatus::Completed
                && coordinator.status != TodoDagExecutionStatus::Dormant
            {
                coordinator.attempted_task_ids.insert(owned.task_id.clone());
            }
            owned
        };
        if job.status == JobStatus::Completed {
            // Workflow-scoped jobs (workflow children) must never forge Todo
            // completion: the supervisor owns the canonical Todo DAG and marks
            // tasks Done through the todo tool after verifying the delegated
            // work. Auto-Done here diverged the supervisor's belief (and let
            // dependents appear completed before their real jobs ran — BUG-3).
            // Non-workflow applications keep the automatic transition.
            if job.workflow_id.is_none() {
                let state = active.session.todo_state();
                let status = state
                    .phases
                    .iter()
                    .flat_map(|phase| &phase.tasks)
                    .find(|task| task.id == ownership.task_id)
                    .map(|task| task.status);
                if matches!(status, Some(TodoStatus::Pending | TodoStatus::InProgress)) {
                    let result = active.session.apply_todo_raw(TodoOp::Done {
                        task: Some(ownership.task_id),
                        phase: None,
                    })?;
                    let coordinator = active.todo_dag.lock();
                    self.publish(ApplicationEvent::TodoUpdated {
                        phases: result.phases,
                        completed_tasks: result.completed_tasks,
                        execution_status: coordinator.status,
                        failure_reason: coordinator.failure_reason.clone(),
                    });
                }
            }
        }
        active.todo_dag_changed.notify_waiters();
        Ok(())
    }

    pub(super) fn reconcile_todo_dag(
        &self,
        runtime: &OrchestrationRuntime,
        allow_spawn: bool,
    ) -> Result<TodoDagExecutionOutcome> {
        let gate = self.runtime().todo_dag.lock().reconcile_gate.clone();
        let _reconcile = gate.lock();
        self.reconcile_todo_dag_locked(runtime, allow_spawn)
    }

    pub(super) fn reconcile_todo_dag_locked(
        &self,
        runtime: &OrchestrationRuntime,
        allow_spawn: bool,
    ) -> Result<TodoDagExecutionOutcome> {
        let active = self.runtime();
        if active
            .todo_transition_active
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(anyhow!("Todo execution rejected during session transition"));
        }
        let jobs = runtime.jobs(None);
        for job in jobs.iter().filter(|job| job.status.is_settled()) {
            self.record_terminal_todo_job(job)?;
        }

        let mut coordinator = active.todo_dag.lock();
        if matches!(
            coordinator.status,
            TodoDagExecutionStatus::Dormant | TodoDagExecutionStatus::Failed
        ) {
            return Ok(TodoDagExecutionOutcome {
                status: coordinator.status,
                failure_reason: coordinator.failure_reason.clone(),
                spawns: Vec::new(),
            });
        }
        for job in jobs
            .iter()
            .filter(|job| matches!(job.status, JobStatus::Queued | JobStatus::Running))
        {
            if let Some(task_id) = &job.todo_task_id {
                coordinator.attempted_task_ids.insert(task_id.clone());
            }
        }

        let state = active.session.todo_state();
        let open_count = state
            .phases
            .iter()
            .flat_map(|phase| &phase.tasks)
            .filter(|task| matches!(task.status, TodoStatus::Pending | TodoStatus::InProgress))
            .count();
        if open_count == 0 {
            coordinator.status = TodoDagExecutionStatus::Settled;
            coordinator.failure_reason = None;
            persist_coordinator_status(&active.session, &coordinator)?;
            drop(coordinator);
            active.todo_dag_changed.notify_waiters();
            return Ok(TodoDagExecutionOutcome {
                status: TodoDagExecutionStatus::Settled,
                failure_reason: None,
                spawns: Vec::new(),
            });
        }

        let candidates = state
            .phases
            .iter()
            .flat_map(|phase| &phase.tasks)
            .enumerate()
            .filter(|(_, task)| {
                matches!(task.status, TodoStatus::Pending | TodoStatus::InProgress)
                    && task.ready
                    && !coordinator.attempted_task_ids.contains(&task.id)
            })
            .map(|(index, task)| {
                (
                    index,
                    task.id.clone(),
                    task.content.clone(),
                    task.agent.clone(),
                )
            })
            .collect::<Vec<_>>();
        let active_job_count = jobs
            .iter()
            .filter(|job| matches!(job.status, JobStatus::Queued | JobStatus::Running))
            .count();
        let slots = runtime.max_concurrency().saturating_sub(active_job_count);
        let selected = if allow_spawn {
            candidates.iter().take(slots).cloned().collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        if !selected.is_empty() {
            let previous_phases = state.phases.clone();
            let selected_ids = selected
                .iter()
                .map(|(_, task_id, _, _)| task_id.as_str())
                .collect::<HashSet<_>>();
            let mut started_phases = state.phases;
            for task in started_phases
                .iter_mut()
                .flat_map(|phase| phase.tasks.iter_mut())
                .filter(|task| selected_ids.contains(task.id.as_str()))
            {
                task.status = TodoStatus::InProgress;
            }
            let started = active.session.set_todos_raw(started_phases)?;
            // Typed agent routing: the Todo item's explicit `agent` field wins,
            // then exact mentions in the task content, then ranked selection.
            // A missing/disabled/ambiguous explicit agent fails actionably
            // (never a silent fallback to the default `task` agent) and rolls
            // the Todo state back.
            let spawns = (|| -> Result<Vec<TaskSpawn>> {
                let mut items = Vec::with_capacity(selected.len());
                for (batch_index, (todo_index, task_id, content, agent)) in
                    selected.iter().enumerate()
                {
                    let resolved = runtime
                        .resolve_task_agent(content, agent.as_deref())
                        .map_err(|error| {
                            anyhow!(
                                "workflow Todo task {todo_index:?} ({content:?}) cannot route to an agent: {error:#}"
                            )
                        })?;
                    items.push(TaskItem {
                        index: batch_index,
                        id: format!("Todo{}", todo_index + 1),
                        agent: resolved,
                        assignment: content.to_owned(),
                        todo_task_id: Some(task_id.clone()),
                        ..TaskItem::default()
                    });
                }
                runtime.spawn_tasks(runtime.main_agent_id(), 0, items)
            })();
            let spawns = match spawns {
                Ok(spawns) => spawns,
                Err(error) => {
                    // Roll back the Todo store to the pre-spawn phases. The
                    // persistent rollback may itself be rejected (e.g. the
                    // session recorder refuses the snapshot); in that case
                    // force the canonical in-memory phases back anyway so the
                    // TUI counters and rows always agree with the pre-spawn
                    // state — a zero-job "active" header must never survive.
                    let error = rollback_spawn_phases(
                        previous_phases,
                        error,
                        |phases| active.session.set_todos_raw(phases).map(|_| ()),
                        |phases| active.session.restore_todos_raw(phases),
                    )?;
                    let reason = TodoDagFailureReason::Spawn {
                        message: format!(
                            "Todo executor could not spawn ready tasks: {error:#}. Fix the task agent routing or runtime configuration, then explicitly restart the Todo DAG."
                        ),
                    };
                    coordinator.status = TodoDagExecutionStatus::Failed;
                    coordinator.failure_reason = Some(reason.clone());
                    persist_coordinator_status(&active.session, &coordinator)?;
                    drop(coordinator);
                    active.todo_dag_changed.notify_waiters();
                    self.publish(ApplicationEvent::TodoUpdated {
                        phases: active.session.todo_state().phases,
                        completed_tasks: Vec::new(),
                        execution_status: TodoDagExecutionStatus::Failed,
                        failure_reason: Some(reason.clone()),
                    });
                    self.publish(ApplicationEvent::RunFailed {
                        message: reason.message().to_owned(),
                    });
                    return Ok(TodoDagExecutionOutcome {
                        status: TodoDagExecutionStatus::Failed,
                        failure_reason: Some(reason),
                        spawns: Vec::new(),
                    });
                }
            };
            let generation = coordinator.generation;
            for ((_, task_id, _, _), spawn) in selected.iter().zip(&spawns) {
                coordinator.attempted_task_ids.insert(task_id.clone());
                coordinator.owned_jobs.insert(
                    spawn.job_id.clone(),
                    OwnedTodoJob {
                        generation,
                        task_id: task_id.clone(),
                    },
                );
            }
            coordinator.status = TodoDagExecutionStatus::Active;
            coordinator.failure_reason = None;
            persist_coordinator_status(&active.session, &coordinator)?;
            drop(coordinator);

            self.publish(ApplicationEvent::TodoUpdated {
                phases: started.phases,
                completed_tasks: started.completed_tasks,
                execution_status: TodoDagExecutionStatus::Active,
                failure_reason: None,
            });
            active.todo_dag_changed.notify_waiters();
            return Ok(TodoDagExecutionOutcome {
                status: TodoDagExecutionStatus::Active,
                failure_reason: None,
                spawns,
            });
        }

        let active_owned = jobs.iter().any(|job| {
            matches!(job.status, JobStatus::Queued | JobStatus::Running)
                && job
                    .todo_task_id
                    .as_ref()
                    .is_some_and(|task_id| coordinator.attempted_task_ids.contains(task_id))
        });
        if active_owned || (!candidates.is_empty() && (!allow_spawn || slots == 0)) {
            coordinator.status = TodoDagExecutionStatus::Active;
            coordinator.failure_reason = None;
        } else if runtime.workflow_scope().is_some() {
            // Workflow children hand DAG control back to their supervisor:
            // open tasks with no spawnable candidates wait for the supervisor
            // agent to mark work Done or re-delegate it.
            coordinator.status = TodoDagExecutionStatus::Dormant;
            coordinator.failure_reason = None;
        } else {
            let open_tasks = state
                .phases
                .iter()
                .flat_map(|phase| &phase.tasks)
                .filter(|task| matches!(task.status, TodoStatus::Pending | TodoStatus::InProgress))
                .map(|task| format!("{} ({})", task.content, task.id))
                .collect::<Vec<_>>()
                .join(", ");
            let reason = TodoDagFailureReason::NoProgress {
                message: format!(
                    "Todo executor made no progress: open tasks remain but no task is ready, running, or eligible to retry ({open_tasks}). Inspect waitingOn dependencies and failed task owners, then explicitly restart the Todo DAG."
                ),
            };
            coordinator.status = TodoDagExecutionStatus::Failed;
            coordinator.failure_reason = Some(reason.clone());
            persist_coordinator_status(&active.session, &coordinator)?;
            drop(coordinator);
            active.todo_dag_changed.notify_waiters();
            self.publish(ApplicationEvent::TodoUpdated {
                phases: active.session.todo_state().phases,
                completed_tasks: Vec::new(),
                execution_status: TodoDagExecutionStatus::Failed,
                failure_reason: Some(reason.clone()),
            });
            self.publish(ApplicationEvent::RunFailed {
                message: reason.message().to_owned(),
            });
            return Ok(TodoDagExecutionOutcome {
                status: TodoDagExecutionStatus::Failed,
                failure_reason: Some(reason),
                spawns: Vec::new(),
            });
        }
        persist_coordinator_status(&active.session, &coordinator)?;
        let status = coordinator.status;
        let failure_reason = coordinator.failure_reason.clone();
        drop(coordinator);
        active.todo_dag_changed.notify_waiters();
        Ok(TodoDagExecutionOutcome {
            status,
            failure_reason,
            spawns: Vec::new(),
        })
    }
}

#[cfg(test)]
mod rollback_tests {
    use super::*;
    use crate::todo::TodoItem;
    use parking_lot::Mutex;
    use std::sync::Arc;

    fn phases(status: TodoStatus) -> Vec<TodoPhase> {
        vec![TodoPhase {
            name: "Main".to_owned(),
            tasks: vec![TodoItem {
                id: "task-1".to_owned(),
                content: "first task".to_owned(),
                status,
                depends_on: Vec::new(),
                ready: true,
                waiting_on: Vec::new(),
                agent: None,
            }],
        }]
    }

    #[test]
    fn failed_persistent_rollback_restores_canonical_phases_in_memory() {
        let previous = phases(TodoStatus::Pending);
        let canonical = Arc::new(Mutex::new(phases(TodoStatus::InProgress)));
        let restored = canonical.clone();
        let error = rollback_spawn_phases(
            previous.clone(),
            anyhow!("spawn failed"),
            |_| Err(anyhow!("snapshot refused")),
            move |phases| {
                *restored.lock() = phases;
                Ok(())
            },
        )
        .expect("in-memory restore");

        assert_eq!(*canonical.lock(), previous);
        assert!(error.to_string().contains("spawn failed"));
        assert!(error.to_string().contains("snapshot refused"));
    }

    #[test]
    fn successful_persistent_rollback_does_not_run_memory_fallback() {
        let previous = phases(TodoStatus::Pending);
        let fallback_called = Arc::new(Mutex::new(false));
        let called = fallback_called.clone();
        let error = rollback_spawn_phases(
            previous,
            anyhow!("spawn failed"),
            |_| Ok(()),
            move |_| {
                *called.lock() = true;
                Ok(())
            },
        )
        .expect("persistent rollback");

        assert!(!*fallback_called.lock());
        assert_eq!(error.to_string(), "spawn failed");
    }
}

#[cfg(test)]
mod restart_persistence_tests {
    use super::*;
    use crate::todo::TodoItem;
    use pi_agent::ThinkingLevel;
    use pi_ai::Model;

    fn recorded_session(cwd: &std::path::Path, session_id: &str) -> crate::Session {
        let session = crate::Session::new(crate::SessionOptions {
            model: Model::default(),
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
            auth_manager: None,
        })
        .expect("session");
        let recorder = crate::start_session_in(
            cwd,
            None,
            Some("off"),
            Some(cwd),
            Some(session_id),
            None,
        )
        .expect("start recorder");
        session.record(recorder).expect("attach recorder");
        session
    }

    fn phase(status: TodoStatus) -> TodoPhase {
        TodoPhase {
            name: "Main".to_owned(),
            tasks: vec![TodoItem {
                id: "task-1".to_owned(),
                content: "first task".to_owned(),
                status,
                depends_on: Vec::new(),
                ready: true,
                waiting_on: Vec::new(),
                agent: None,
            }],
        }
    }


    fn persist_and_reload(
        cwd: &std::path::Path,
        session_id: &str,
        phases: Vec<TodoPhase>,
        status: TodoDagExecutionStatus,
        failure_reason: Option<TodoDagFailureReason>,
    ) -> (TodoDagExecutionStatus, Option<TodoDagFailureReason>) {
        let session = recorded_session(cwd, session_id);
        session.set_todos(phases).expect("set phases");
        session
            .persist_todo_execution(status, failure_reason)
            .expect("persist execution");
        let path = session.recorder_info().expect("recorder path").1;
        drop(session);
        let restored = crate::Session::new(crate::SessionOptions {
            model: Model::default(),
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
            auth_manager: None,
        })
        .expect("restored session");
        restored
            .record(crate::resume_session(&path).expect("resume"))
            .expect("attach");
        let mut coordinator = TodoDagCoordinator::default();
        coordinator.restore_from_session(&restored);
        (coordinator.status, coordinator.failure_reason)
    }

    #[test]
    fn failed_dag_survives_serialize_reload_as_failed() {
        let cwd = tempfile::tempdir().expect("cwd");
        let reason = TodoDagFailureReason::NoProgress {
            message: "open tasks remain but no task is ready".to_owned(),
        };
        let (status, restored_reason) = persist_and_reload(
            cwd.path(),
            "todo-failed-restart",
            vec![phase(TodoStatus::Pending)],
            TodoDagExecutionStatus::Failed,
            Some(reason.clone()),
        );
        assert_eq!(status, TodoDagExecutionStatus::Failed);
        assert_eq!(restored_reason.as_ref().map(TodoDagFailureReason::message), Some(reason.message()));
    }

    #[test]
    fn settled_dag_survives_serialize_reload_as_completed() {
        let cwd = tempfile::tempdir().expect("cwd");
        let (status, reason) = persist_and_reload(
            cwd.path(),
            "todo-settled-restart",
            vec![phase(TodoStatus::Completed)],
            TodoDagExecutionStatus::Settled,
            None,
        );
        assert_eq!(status, TodoDagExecutionStatus::Settled);
        assert!(reason.is_none());
    }

    #[test]
    fn stale_active_after_process_death_becomes_interrupted_failed() {
        let cwd = tempfile::tempdir().expect("cwd");
        let (status, reason) = persist_and_reload(
            cwd.path(),
            "todo-active-restart",
            vec![phase(TodoStatus::InProgress)],
            TodoDagExecutionStatus::Active,
            None,
        );
        assert_eq!(status, TodoDagExecutionStatus::Failed);
        let message = reason.expect("interrupted reason").message().to_owned();
        assert!(message.contains("interrupted by process restart"), "{message}");
        assert!(message.contains("was not replayed"), "{message}");
    }

    #[test]
    fn coordinator_is_restored_before_application_state_snapshot() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = recorded_session(cwd.path(), "todo-snapshot-order");
        session
            .set_todos(vec![phase(TodoStatus::Pending)])
            .expect("set phases");
        session
            .persist_todo_execution(
                TodoDagExecutionStatus::Failed,
                Some(TodoDagFailureReason::Spawn {
                    message: "spawn refused".to_owned(),
                }),
            )
            .expect("persist failed");
        let path = session.recorder_info().expect("path").1;
        drop(session);

        let restored = crate::Session::new(crate::SessionOptions {
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
            auth_manager: None,
        })
        .expect("restored session");
        restored
            .record(crate::resume_session(&path).expect("resume"))
            .expect("attach");
        let mut coordinator = TodoDagCoordinator::default();
        assert_eq!(coordinator.status, TodoDagExecutionStatus::Dormant);
        coordinator.restore_from_session(&restored);
        assert_eq!(coordinator.status, TodoDagExecutionStatus::Failed);
        assert_eq!(
            restored.todo_execution().0,
            TodoDagExecutionStatus::Failed,
            "session store must reconstruct Failed before any snapshot publish"
        );
    }

    fn job(id: &str, task_id: &str, status: JobStatus) -> JobSnapshot {
        JobSnapshot {
            id: id.to_owned(),
            agent_id: format!("agent-{id}"),
            agent: "task".to_owned(),
            parent_id: "Main".to_owned(),
            description: None,
            todo_task_id: Some(task_id.to_owned()),
            workflow_id: None,
            workflow_generation: None,
            status,
            created_at: 1,
            started_at: None,
            finished_at: None,
            result: None,
            soft_budget_exhausted: false,
        }
    }

    #[test]
    fn restored_active_jobs_reconstruct_todo_ownership_before_reconcile() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = recorded_session(cwd.path(), "todo-owned-restart");
        session
            .set_todos(vec![phase(TodoStatus::InProgress)])
            .expect("set Todo");
        session
            .restore_todo_execution(TodoDagExecutionStatus::Active, None);
        let recovered = job("job-1", "task-1", JobStatus::Running);
        let mut coordinator = TodoDagCoordinator::default();
        coordinator.restore_from_session_with_jobs(&session, std::slice::from_ref(&recovered));

        assert!(coordinator.attempted_task_ids.contains("task-1"));
        let owned = coordinator.owned_jobs.get("job-1").expect("owned recovered job");
        assert_eq!(owned.task_id, "task-1");
        assert_eq!(owned.generation, coordinator.generation);

        assert_eq!(coordinator.status, TodoDagExecutionStatus::Active);
        assert!(coordinator.failure_reason.is_none());
    }
    #[test]
    fn restored_terminal_jobs_remain_owned_until_completion_is_reconciled() {
        let cwd = tempfile::tempdir().expect("cwd");
        let session = recorded_session(cwd.path(), "todo-terminal-restart");
        let recovered = job("job-1", "task-1", JobStatus::Completed);
        let mut coordinator = TodoDagCoordinator::default();
        coordinator.restore_from_session_with_jobs(&session, std::slice::from_ref(&recovered));

        assert!(coordinator.attempted_task_ids.contains("task-1"));
        assert!(coordinator.owned_jobs.contains_key("job-1"));
        assert!(!coordinator.handled_terminal_job_ids.contains("job-1"));
    }
}
