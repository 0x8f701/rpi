use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;

use super::Application;
use crate::{
    JobStatus, MailboxMessage, OrchestrationConcurrencyGate, OrchestrationRuntime,
    TodoDagExecutionOutcome, TodoDagExecutionStatus, TodoState, WorkflowJobSnapshot,
    WorkflowRuntimeScope, WorkflowSupervisorBackend,
};

const WORKFLOW_JOB_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
const WORKFLOW_JOB_DRAIN_POLL: Duration = Duration::from_millis(10);

/// Supervisor backend pinned to exactly one child Application runtime incarnation.
///
/// Keeping the orchestration runtime and generation beside the Application avoids
/// resolving either from mutable parent state while a lifecycle operation is in flight.
pub(super) struct WorkflowApplicationBackend {
    application: Application,
    orchestration: OrchestrationRuntime,
    workflow_id: String,
    generation: u64,
}

impl WorkflowApplicationBackend {
    pub(super) fn new(
        application: Application,
        orchestration: OrchestrationRuntime,
        workflow_id: String,
        generation: u64,
    ) -> Result<Self> {
        if workflow_id.trim().is_empty() {
            bail!("workflow id must not be empty");
        }
        Ok(Self {
            application,
            orchestration,
            workflow_id,
            generation,
        })
    }

    pub(super) fn application(&self) -> &Application {
        &self.application
    }

    pub(super) fn orchestration(&self) -> &OrchestrationRuntime {
        &self.orchestration
    }

    fn scoped_jobs(&self) -> Vec<WorkflowJobSnapshot> {
        self.orchestration
            .workflow_jobs(&self.workflow_id, self.generation)
    }

    fn active_scoped_job_ids(&self) -> Vec<String> {
        self.scoped_jobs()
            .into_iter()
            .filter(|job| matches!(job.job.status, JobStatus::Queued | JobStatus::Running))
            .map(|job| job.job.id)
            .collect()
    }

    async fn drain_jobs(&self, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let deadline = Instant::now() + WORKFLOW_JOB_DRAIN_TIMEOUT;
        loop {
            let active = self
                .orchestration
                .workflow_jobs(&self.workflow_id, self.generation)
                .into_iter()
                .filter(|job| ids.contains(&job.job.id) && !job.job.status.is_settled())
                .map(|job| job.job.id)
                .collect::<Vec<_>>();
            if active.is_empty() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(anyhow!("workflow job drain timed out"));
            }
            tokio::time::sleep(WORKFLOW_JOB_DRAIN_POLL).await;
        }
    }

    pub(super) async fn abort_application(&self) {
        self.application.abort().await;
        self.application.wait_for_idle().await;
    }

    pub(super) async fn cleanup(&self) -> Result<()> {
        let ids = self.active_scoped_job_ids();
        self.orchestration.cancel_jobs(&ids);
        self.abort_application().await;
        let drain_result = self.drain_jobs(&ids).await;
        self.application.cleanup().await;
        drain_result
    }
}

#[async_trait]
impl WorkflowSupervisorBackend for WorkflowApplicationBackend {
    fn todo_state(&self) -> TodoState {
        self.application.todo_state()
    }

    fn todo_dag_status(&self) -> TodoDagExecutionStatus {
        self.application.todo_dag_status()
    }

    fn workflow_jobs(&self, workflow_id: &str, generation: u64) -> Vec<WorkflowJobSnapshot> {
        self.orchestration.workflow_jobs(workflow_id, generation)
    }

    fn inbox(&self, agent_id: &str, peek: bool) -> Vec<MailboxMessage> {
        self.orchestration.inbox(agent_id, peek)
    }

    fn active_workflow_job_ids(&self, workflow_id: &str, generation: u64) -> Vec<String> {
        self.orchestration
            .workflow_jobs(workflow_id, generation)
            .into_iter()
            .filter(|job| matches!(job.job.status, JobStatus::Queued | JobStatus::Running))
            .map(|job| job.job.id)
            .collect()
    }

    fn configure_workflow_runtime(
        &self,
        scope: WorkflowRuntimeScope,
        max_concurrency: usize,
        global_concurrency: OrchestrationConcurrencyGate,
    ) -> Result<()> {
        if scope.workflow_id != self.workflow_id || scope.generation != self.generation {
            bail!("workflow runtime scope mismatch");
        }
        if self.orchestration.max_concurrency() != max_concurrency {
            bail!("workflow concurrency mismatch");
        }
        self.orchestration.set_workflow_scope(scope)?;
        self.orchestration
            .set_global_concurrency_gate(global_concurrency)
    }

    async fn prompt_supervisor(&self, prompt: String) -> Result<()> {
        let mut events = self.application.subscribe();
        self.application
            .prompt_without_natural_language_spawn(prompt, Vec::new(), None)
            .await?;
        self.application.wait_for_idle().await;
        loop {
            match events.try_recv() {
                Ok(crate::ApplicationEvent::RunFailed { message }) => {
                    return Err(anyhow!(message));
                }
                Ok(_) | Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => return Ok(()),
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                    return Err(anyhow!("workflow supervisor application stopped"));
                }
            }
        }
    }

    async fn steer_supervisor(&self, message: String) -> Result<()> {
        self.application.steer(message, Vec::new()).await;
        Ok(())
    }
    async fn pause(&self) -> Result<()> {
        let ids = self.active_scoped_job_ids();
        self.application.abort().await;
        self.application.wait_for_idle().await;
        self.drain_jobs(&ids).await
    }


    fn execute_todo_dag(&self) -> Result<TodoDagExecutionOutcome> {
        self.application.execute_todo_dag()
    }

    async fn resume(&self) -> Result<TodoDagExecutionOutcome> {
        self.application.execute_todo_dag()
    }

    async fn cancel_jobs(&self, ids: &[String]) -> Result<Vec<String>> {
        let cancelled = self.orchestration.cancel_jobs(ids);
        self.drain_jobs(&cancelled).await?;
        Ok(cancelled)
    }
}

pub(super) type SharedWorkflowApplicationBackend = Arc<WorkflowApplicationBackend>;
