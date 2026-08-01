use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use anyhow::{Result, anyhow};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::{Application, ApplicationEvent, ApplicationInner};
use crate::{
    JobSnapshot, JobStatus, OrchestrationEvent, OrchestrationRuntime, TaskItem, TaskSpawn, TodoOp,
    TodoStatus,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoDagExecutionStatus {
    #[default]
    Dormant,
    Active,
    Settled,
    Blocked,
}

impl TodoDagExecutionStatus {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Dormant | Self::Settled | Self::Blocked)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TodoDagExecutionOutcome {
    pub status: TodoDagExecutionStatus,
    pub spawns: Vec<TaskSpawn>,
}

#[derive(Clone)]
struct OwnedTodoJob {
    generation: u64,
    task_id: String,
}

#[derive(Default)]
pub(super) struct TodoDagCoordinator {
    pub(super) status: TodoDagExecutionStatus,
    generation: u64,
    attempted_task_ids: HashSet<String>,
    handled_terminal_job_ids: HashSet<String>,
    owned_jobs: HashMap<String, OwnedTodoJob>,
    reconcile_gate: Arc<Mutex<()>>,
}

impl Application {
    /// Explicitly start or restart parent-owned execution of the canonical Todo DAG.
    pub fn execute_todo_dag(&self) -> Result<TodoDagExecutionOutcome> {
        let runtime = self
            .orchestration_runtime()
            .ok_or_else(|| anyhow!("application orchestration is not configured"))?;
        self.inner.start_todo_dag_cycle(&runtime, true)
    }

    /// Re-evaluate an already armed execution without creating execution intent.
    pub fn reconcile_todo_dag_if_armed(&self) -> Result<TodoDagExecutionOutcome> {
        if self.todo_dag_status() == TodoDagExecutionStatus::Dormant {
            return Ok(TodoDagExecutionOutcome {
                status: TodoDagExecutionStatus::Dormant,
                spawns: Vec::new(),
            });
        }
        let runtime = self
            .orchestration_runtime()
            .ok_or_else(|| anyhow!("application orchestration is not configured"))?;
        self.inner.reconcile_todo_dag(&runtime, true)
    }

    #[must_use]
    pub fn todo_dag_status(&self) -> TodoDagExecutionStatus {
        self.inner.todo_dag.lock().status
    }

    pub async fn wait_todo_dag(&self) -> TodoDagExecutionStatus {
        loop {
            let changed = self.inner.todo_dag_changed.notified();
            let status = self.todo_dag_status();
            if status.is_terminal() {
                return status;
            }
            changed.await;
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
        self.todo_dag.lock().reconcile_gate.clone()
    }

    pub(super) fn arm_todo_dag_locked(
        &self,
        runtime: &OrchestrationRuntime,
        reset_attempts: bool,
    ) -> Result<TodoDagExecutionOutcome> {
        if self.todo_transition_active.load(std::sync::atomic::Ordering::Acquire) {
            return Err(anyhow!("Todo execution rejected during session transition"));
        }
        let jobs = runtime.jobs(None);
        {
            let mut coordinator = self.todo_dag.lock();
            coordinator.status = TodoDagExecutionStatus::Active;
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
                        active_job_ids.contains(job_id.as_str()).then(|| owned.task_id.clone())
                    })
                    .collect::<Vec<_>>();
                coordinator.attempted_task_ids.extend(active_task_ids);
            }
        }
        self.todo_dag_changed.notify_waiters();
        self.reconcile_todo_dag_locked(runtime, true)
    }
    pub(super) fn begin_todo_dag_transition(
        &self,
        runtime: &OrchestrationRuntime,
    ) -> Vec<String> {
        let gate = self.todo_dag_transaction_gate();
        let _reconcile = gate.lock();
        self.begin_todo_dag_transition_locked(runtime)
    }

    pub(super) fn begin_todo_dag_transition_locked(
        &self,
        runtime: &OrchestrationRuntime,
    ) -> Vec<String> {
        let active_status = runtime
            .jobs(None)
            .into_iter()
            .map(|job| (job.id, job.status))
            .collect::<HashMap<_, _>>();
        let mut coordinator = self.todo_dag.lock();
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
        coordinator.attempted_task_ids.clear();
        coordinator.handled_terminal_job_ids.clear();
        coordinator.owned_jobs.clear();
        drop(coordinator);
        self.todo_dag_changed.notify_waiters();
        active
    }


    pub(super) fn observe_orchestration_event(
        &self,
        runtime: &OrchestrationRuntime,
        event: &OrchestrationEvent,
        allow_spawn: bool,
    ) -> Result<TodoDagExecutionStatus> {
        let gate = self.todo_dag.lock().reconcile_gate.clone();
        let _reconcile = gate.lock();
        let OrchestrationEvent::JobUpdated { job, .. } = event else {
            return Ok(self.todo_dag.lock().status);
        };
        if !job.status.is_settled() {
            return Ok(self.todo_dag.lock().status);
        }
        self.record_terminal_todo_job(job)?;
        if self.todo_dag.lock().status == TodoDagExecutionStatus::Dormant {
            return Ok(TodoDagExecutionStatus::Dormant);
        }
        Ok(self.reconcile_todo_dag_locked(runtime, allow_spawn)?.status)
    }

    fn record_terminal_todo_job(&self, job: &JobSnapshot) -> Result<()> {
        let ownership = {
            let mut coordinator = self.todo_dag.lock();
            if coordinator.handled_terminal_job_ids.contains(&job.id) {
                return Ok(());
            }
            let Some(owned) = coordinator.owned_jobs.get(&job.id) else {
                return Ok(());
            };
            if owned.generation != coordinator.generation {
                return Ok(());
            }
            let owned = coordinator.owned_jobs.remove(&job.id).expect("owned job was present");
            coordinator.handled_terminal_job_ids.insert(job.id.clone());
            if job.status != JobStatus::Completed
                && coordinator.status != TodoDagExecutionStatus::Dormant
            {
                coordinator.attempted_task_ids.insert(owned.task_id.clone());
            }
            owned
        };
        if job.status == JobStatus::Completed {
            let state = self.session.todo_state();
            let status = state
                .phases
                .iter()
                .flat_map(|phase| &phase.tasks)
                .find(|task| task.id == ownership.task_id)
                .map(|task| task.status);
            if matches!(status, Some(TodoStatus::Pending | TodoStatus::InProgress)) {
                let result = self.session.apply_todo_raw(TodoOp::Done {
                    task: Some(ownership.task_id),
                    phase: None,
                })?;
                self.publish(ApplicationEvent::TodoUpdated {
                    phases: result.phases,
                    completed_tasks: result.completed_tasks,
                });
            }
        }
        self.todo_dag_changed.notify_waiters();
        Ok(())
    }

    pub(super) fn reconcile_todo_dag(
        &self,
        runtime: &OrchestrationRuntime,
        allow_spawn: bool,
    ) -> Result<TodoDagExecutionOutcome> {
        let gate = self.todo_dag.lock().reconcile_gate.clone();
        let _reconcile = gate.lock();
        self.reconcile_todo_dag_locked(runtime, allow_spawn)
    }

    pub(super) fn reconcile_todo_dag_locked(
        &self,
        runtime: &OrchestrationRuntime,
        allow_spawn: bool,
    ) -> Result<TodoDagExecutionOutcome> {
        if self.todo_transition_active.load(std::sync::atomic::Ordering::Acquire) {
            return Err(anyhow!("Todo execution rejected during session transition"));
        }
        let jobs = runtime.jobs(None);
        for job in jobs.iter().filter(|job| job.status.is_settled()) {
            self.record_terminal_todo_job(job)?;
        }

        let mut coordinator = self.todo_dag.lock();
        if coordinator.status == TodoDagExecutionStatus::Dormant {
            return Ok(TodoDagExecutionOutcome {
                status: TodoDagExecutionStatus::Dormant,
                spawns: Vec::new(),
            });
        }
        for job in jobs.iter().filter(|job| {
            matches!(job.status, JobStatus::Queued | JobStatus::Running)
        }) {
            if let Some(task_id) = &job.todo_task_id {
                coordinator.attempted_task_ids.insert(task_id.clone());
            }
        }

        let state = self.session.todo_state();
        let open_count = state
            .phases
            .iter()
            .flat_map(|phase| &phase.tasks)
            .filter(|task| matches!(task.status, TodoStatus::Pending | TodoStatus::InProgress))
            .count();
        if open_count == 0 {
            coordinator.status = TodoDagExecutionStatus::Settled;
            drop(coordinator);
            self.todo_dag_changed.notify_waiters();
            return Ok(TodoDagExecutionOutcome {
                status: TodoDagExecutionStatus::Settled,
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
            .map(|(index, task)| (index, task.id.clone(), task.content.clone()))
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
                .map(|(_, task_id, _)| task_id.as_str())
                .collect::<HashSet<_>>();
            let mut started_phases = state.phases;
            for task in started_phases
                .iter_mut()
                .flat_map(|phase| phase.tasks.iter_mut())
                .filter(|task| selected_ids.contains(task.id.as_str()))
            {
                task.status = TodoStatus::InProgress;
            }
            let started = self.session.set_todos_raw(started_phases)?;
            let items = selected
                .iter()
                .enumerate()
                .map(|(batch_index, (todo_index, task_id, content))| TaskItem {
                    index: batch_index,
                    id: format!("Todo{}", todo_index + 1),
                    agent: runtime.select_agent(content, None),
                    assignment: content.to_owned(),
                    todo_task_id: Some(task_id.clone()),
                })
                .collect::<Vec<_>>();
            let spawns = match runtime.spawn_tasks(runtime.main_agent_id(), 0, items) {
                Ok(spawns) => spawns,
                Err(error) => {
                    return match self.session.set_todos_raw(previous_phases) {
                        Ok(_) => Err(error),
                        Err(rollback_error) => Err(anyhow!(
                            "{error}; additionally failed to restore Todo state: {rollback_error}"
                        )),
                    };
                }
            };
            let generation = coordinator.generation;
            for ((_, task_id, _), spawn) in selected.iter().zip(&spawns) {
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
            drop(coordinator);

            self.publish(ApplicationEvent::TodoUpdated {
                phases: started.phases,
                completed_tasks: started.completed_tasks,
            });
            self.todo_dag_changed.notify_waiters();
            return Ok(TodoDagExecutionOutcome {
                status: TodoDagExecutionStatus::Active,
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
        coordinator.status = if active_owned
            || (!candidates.is_empty() && (!allow_spawn || slots == 0))
        {
            TodoDagExecutionStatus::Active
        } else {
            TodoDagExecutionStatus::Blocked
        };
        let status = coordinator.status;
        drop(coordinator);
        self.todo_dag_changed.notify_waiters();
        Ok(TodoDagExecutionOutcome {
            status,
            spawns: Vec::new(),
        })
    }
}
