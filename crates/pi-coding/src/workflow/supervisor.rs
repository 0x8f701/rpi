//! Workflow-scoped supervisor actor over a worktree-bound `Application`.
//!
//! This module expects canonical parent-module types:
//! `WorkflowStatus` with queued/planning/running/paused/integrating/completed/
//! failed/cancelled/conflicted variants, and `WorkflowTaskOwnership` with
//! explicit `workflow_id` and `todo_task_id` fields.

use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::{
    ApplicationEvent, JobStatus, OrchestrationConcurrencyGate, OrchestrationEvent,
    TodoDagExecutionOutcome, TodoDagExecutionStatus, TodoState, WorkflowRuntimeScope,
};

use super::{WorkflowStatus, WorkflowTaskOwnership};

const SUPERVISOR_CHANNEL_CAPACITY: usize = 64;

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
            "You supervise workflow {workflow_id}. Objective: {objective}\n\
             Work only in the current workflow Application cwd. Maintain this Application's canonical Todo DAG. \
             Delegate ready Todo tasks to workers using the task tool and always pass the exact todoTaskId. \
             Workers are your children. Direct them and report progress through hub IRC. Never infer workflow or Todo ownership from assignment text.",
            workflow_id = self.workflow_id,
            objective = self.objective,
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
    async fn prompt_supervisor(&self, prompt: String) -> Result<()>;
    async fn steer_supervisor(&self, message: String) -> Result<()>;
    fn execute_todo_dag(&self) -> Result<TodoDagExecutionOutcome>;
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
    Cancel,
    Steer(String),
    ApplicationEvent {
        generation: u64,
        event: ApplicationEvent,
    },
}

struct SupervisorActor<B> {
    contract: WorkflowSupervisorContract,
    backend: Arc<B>,
    projection: Arc<Mutex<WorkflowSupervisorProjection>>,
    events: broadcast::Sender<WorkflowSupervisorEvent>,
    status: WorkflowStatus,
    failure: Option<String>,
}

impl<B> SupervisorActor<B>
where
    B: WorkflowSupervisorBackend,
{
    async fn run(mut self, mut commands: mpsc::Receiver<SupervisorCommand>) {
        while let Some(command) = commands.recv().await {
            let result = self.handle(command.request).await;
            let _ = command.reply.send(result);
        }
    }

    async fn handle(&mut self, request: SupervisorRequest) -> Result<Vec<String>> {
        match request {
            SupervisorRequest::Start => {
                if self.status != WorkflowStatus::Queued {
                    return Ok(Vec::new());
                }
                self.set_status(WorkflowStatus::Planning);
                if let Err(error) = self
                    .backend
                    .prompt_supervisor(self.contract.initial_prompt())
                    .await
                {
                    return self.fail(error);
                }
                if self
                    .backend
                    .todo_state()
                    .phases
                    .iter()
                    .any(|phase| !phase.tasks.is_empty())
                {
                    if let Err(error) = self.backend.execute_todo_dag() {
                        return self.fail(error);
                    }
                }
                self.set_status(self.status_from_backend());
                let _ = self.events.send(WorkflowSupervisorEvent::Started {
                    workflow_id: self.contract.workflow_id.clone(),
                    generation: self.contract.generation,
                });
                Ok(Vec::new())
            }
            SupervisorRequest::Pause => {
                if is_terminal(self.status) || self.status == WorkflowStatus::Paused {
                    return Ok(Vec::new());
                }
                self.backend.pause().await?;
                self.set_status(WorkflowStatus::Paused);
                Ok(Vec::new())
            }
            SupervisorRequest::Resume => {
                if self.status != WorkflowStatus::Paused {
                    return Ok(Vec::new());
                }
                if self
                    .backend
                    .todo_state()
                    .phases
                    .iter()
                    .any(|phase| !phase.tasks.is_empty())
                {
                    self.backend.resume().await?;
                }
                self.set_status(self.status_from_backend());
                Ok(Vec::new())
            }
            SupervisorRequest::Cancel => {
                if self.status == WorkflowStatus::Cancelled {
                    return Ok(Vec::new());
                }
                if is_terminal(self.status) {
                    return Ok(Vec::new());
                }
                let ids = self.backend.active_workflow_job_ids(
                    &self.contract.workflow_id,
                    self.contract.generation,
                );
                let cancelled = self.backend.cancel_jobs(&ids).await?;
                self.set_status(WorkflowStatus::Cancelled);
                Ok(cancelled)
            }
            SupervisorRequest::Steer(message) => {
                if is_terminal(self.status) {
                    bail!("cannot steer terminal workflow supervisor");
                }
                self.backend.steer_supervisor(message).await?;
                self.refresh();
                Ok(Vec::new())
            }
            SupervisorRequest::ApplicationEvent { generation, event } => {
                if generation != self.contract.generation || is_terminal(self.status) {
                    return Ok(Vec::new());
                }
                match &event {
                    ApplicationEvent::RunFailed { message } => {
                        return self.fail(anyhow!(message.clone()));
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
                        return Ok(Vec::new());
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
                    }
                    _ => {}
                }
                if self.status != WorkflowStatus::Paused {
                    self.set_status(self.status_from_backend());
                } else {
                    self.refresh();
                }
                Ok(Vec::new())
            }
        }
    }

    fn fail(&mut self, error: anyhow::Error) -> Result<Vec<String>> {
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

    fn status_from_backend(&self) -> WorkflowStatus {
        match self.backend.todo_dag_status() {
            TodoDagExecutionStatus::Active => WorkflowStatus::Running,
            TodoDagExecutionStatus::Settled => {
                if todo_is_exactly_complete(&self.backend.todo_state()) {
                    WorkflowStatus::Completed
                } else {
                    WorkflowStatus::Failed
                }
            }
            TodoDagExecutionStatus::Blocked => WorkflowStatus::Failed,
            TodoDagExecutionStatus::Dormant => {
                if todo_is_exactly_complete(&self.backend.todo_state()) {
                    WorkflowStatus::Completed
                } else {
                    WorkflowStatus::Planning
                }
            }
        }
    }

    fn set_status(&mut self, status: WorkflowStatus) {
        let changed = self.status != status;
        self.status = status;
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
        );
        *self.projection.lock() = projection.clone();
        let _ = self
            .events
            .send(WorkflowSupervisorEvent::ProjectionChanged { projection });
    }
}

fn project_backend<B: WorkflowSupervisorBackend + ?Sized>(
    contract: &WorkflowSupervisorContract,
    backend: &B,
    status: WorkflowStatus,
    failure: Option<String>,
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
