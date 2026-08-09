//! LF-delimited workflow RPC wire types, redaction, and host adapter.
//!
//! Commands are dispatched from main `RpcCommand` handling in `modes::rpc`.
//! Wire snapshots/events project canonical [`WorkflowSnapshot`] /
//! [`WorkflowId`] / [`WorkflowStatus`] with worktree labels only (never absolute
//! paths). Production binds the Application-owned [`WorkflowManager`]; there is
//! no production `set_host` and no independent manager open. Test doubles stay
//! under `cfg(test)`.

use std::path::{Component, Path};
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use parking_lot::Mutex;
use pi_coding::{
    Application, WorkflowCreateRequest, WorkflowEvent, WorkflowId, WorkflowManager,
    WorkflowSnapshot, WorkflowStatus, WorkflowTaskOwnership,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Public workflow snapshot. Absolute worktree paths are never serialized.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowWireSnapshot {
    pub workflow_id: WorkflowId,
    pub name: String,
    pub objective: String,
    pub status: WorkflowStatus,
    pub generation: u64,
    /// Exact composite ownership of delegated todo tasks (`workflowId` +
    /// `todoTaskId` + `generation`). One entry per canonical snapshot todo
    /// task. Never includes task content or paths.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ownership: Vec<WorkflowTaskOwnership>,
    /// Worktree label only (basename / relative). Never an absolute filesystem path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor_job_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration: Option<String>,
}

/// Public workflow event stream records (one JSON object per LF on stdout).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum WorkflowWireEvent {
    WorkflowUpdated {
        workflow_id: WorkflowId,
        generation: u64,
        snapshot: WorkflowWireSnapshot,
    },
    WorkflowStatusChanged {
        workflow_id: WorkflowId,
        generation: u64,
        status: WorkflowStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    WorkflowRemoved {
        workflow_id: WorkflowId,
        generation: u64,
    },
}

/// Typed workflow RPC commands with fail-closed unknown fields.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum WorkflowRpcCommand {
    WorkflowCreate {
        #[serde(default)]
        id: Option<String>,
        name: String,
        objective: String,
    },
    WorkflowList {
        #[serde(default)]
        id: Option<String>,
    },
    WorkflowGet {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        workflow_id: Option<String>,
        #[serde(default)]
        name: Option<String>,
    },
    /// Live detail projection (supervisor state, planning activity feed,
    /// active tasks, workers with activity) — the workflow panel's detail.
    WorkflowDetail {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        workflow_id: Option<String>,
        #[serde(default)]
        name: Option<String>,
    },
    WorkflowPause {
        #[serde(default)]
        id: Option<String>,
        workflow_id: String,
    },
    WorkflowResume {
        #[serde(default)]
        id: Option<String>,
        workflow_id: String,
    },
    WorkflowCancel {
        #[serde(default)]
        id: Option<String>,
        workflow_id: String,
    },
    WorkflowIntegrate {
        #[serde(default)]
        id: Option<String>,
        workflow_id: String,
    },
    WorkflowRemove {
        #[serde(default)]
        id: Option<String>,
        workflow_id: String,
    },
}

impl WorkflowRpcCommand {
    #[must_use]
    pub fn request_id(&self) -> Option<String> {
        match self {
            Self::WorkflowCreate { id, .. }
            | Self::WorkflowList { id }
            | Self::WorkflowGet { id, .. }
            | Self::WorkflowDetail { id, .. }
            | Self::WorkflowPause { id, .. }
            | Self::WorkflowResume { id, .. }
            | Self::WorkflowCancel { id, .. }
            | Self::WorkflowIntegrate { id, .. }
            | Self::WorkflowRemove { id, .. } => id.clone(),
        }
    }

    #[must_use]
    pub const fn command_name(&self) -> &'static str {
        match self {
            Self::WorkflowCreate { .. } => "workflow_create",
            Self::WorkflowList { .. } => "workflow_list",
            Self::WorkflowGet { .. } => "workflow_get",
            Self::WorkflowDetail { .. } => "workflow_detail",
            Self::WorkflowPause { .. } => "workflow_pause",
            Self::WorkflowResume { .. } => "workflow_resume",
            Self::WorkflowCancel { .. } => "workflow_cancel",
            Self::WorkflowIntegrate { .. } => "workflow_integrate",
            Self::WorkflowRemove { .. } => "workflow_remove",
        }
    }

    #[must_use]
    pub fn is_workflow_type(command: &str) -> bool {
        matches!(
            command,
            "workflow_create"
                | "workflow_list"
                | "workflow_get"
                | "workflow_detail"
                | "workflow_pause"
                | "workflow_resume"
                | "workflow_cancel"
                | "workflow_integrate"
                | "workflow_remove"
        )
    }
}

/// Async host RPC dispatches against (Application-owned manager in production).
#[async_trait]
pub trait WorkflowRpcHost: Send + Sync {
    async fn create(&self, name: String, objective: String) -> Result<WorkflowSnapshot>;
    fn list(&self) -> Result<Vec<WorkflowSnapshot>>;
    fn get(&self, workflow_id: Option<&str>, name: Option<&str>) -> Result<WorkflowSnapshot>;
    /// Live workflow-panel detail: supervisor state, planning activity feed,
    /// active tasks, and workers with per-agent activity. Worktree labels are
    /// already redacted to display-safe basenames.
    async fn detail(
        &self,
        workflow_id: Option<&str>,
        name: Option<&str>,
    ) -> Result<crate::workflow_panel::WorkflowPanelSnapshot>;
    async fn pause(&self, workflow_id: &str) -> Result<WorkflowSnapshot>;
    async fn resume(&self, workflow_id: &str) -> Result<WorkflowSnapshot>;
    async fn cancel(&self, workflow_id: &str) -> Result<WorkflowSnapshot>;
    async fn integrate(&self, workflow_id: &str) -> Result<WorkflowSnapshot>;
    async fn remove(&self, workflow_id: &str) -> Result<WorkflowSnapshot>;
}

/// Project a canonical domain snapshot onto the public wire shape.
#[must_use]
pub fn project_workflow_snapshot(snapshot: &WorkflowSnapshot) -> WorkflowWireSnapshot {
    WorkflowWireSnapshot {
        workflow_id: snapshot.workflow_id.clone(),
        name: snapshot.name.clone(),
        objective: snapshot.objective.clone(),
        status: snapshot.status,
        generation: snapshot.generation,
        ownership: snapshot
            .todo
            .phases
            .iter()
            .flat_map(|phase| &phase.tasks)
            .map(|task| WorkflowTaskOwnership {
                workflow_id: snapshot.workflow_id.to_string(),
                todo_task_id: task.id.clone(),
                generation: snapshot.generation,
            })
            .collect(),
        worktree: snapshot
            .worktree_label
            .as_deref()
            .and_then(redact_worktree_path),
        branch: snapshot.branch.clone(),
        supervisor_agent_id: snapshot.supervisor_agent_id.clone(),
        supervisor_job_id: snapshot.supervisor_job_id.clone(),
        failure: snapshot.failure.as_ref().map(|f| f.message.clone()),
        integration: match &snapshot.integration {
            pi_coding::WorkflowIntegration::None => None,
            pi_coding::WorkflowIntegration::Applied { result_commit } => {
                Some(format!("applied:{result_commit}"))
            }
            pi_coding::WorkflowIntegration::Conflicted { conflicts } => {
                Some(format!("conflicted:{}", conflicts.join(",")))
            }
        },
    }
}

/// Project a domain event onto the public wire shape.
#[must_use]
pub fn project_workflow_event(event: &WorkflowEvent) -> WorkflowWireEvent {
    match event {
        WorkflowEvent::Created { snapshot } | WorkflowEvent::Updated { snapshot } => {
            WorkflowWireEvent::WorkflowUpdated {
                workflow_id: snapshot.workflow_id.clone(),
                generation: snapshot.generation,
                snapshot: project_workflow_snapshot(snapshot),
            }
        }
        WorkflowEvent::StatusChanged {
            workflow_id,
            generation,
            status,
        } => WorkflowWireEvent::WorkflowStatusChanged {
            workflow_id: workflow_id.clone(),
            generation: *generation,
            status: *status,
            name: None,
        },
        WorkflowEvent::Removed {
            workflow_id,
            generation,
        } => WorkflowWireEvent::WorkflowRemoved {
            workflow_id: workflow_id.clone(),
            generation: *generation,
        },
    }
}

/// Redact an internal worktree path/label to a non-absolute label (basename).
#[must_use]
pub fn redact_worktree_path(path: &str) -> Option<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return None;
    }
    let as_path = Path::new(trimmed);
    if as_path.is_absolute() {
        if let Some(name) = as_path.file_name().and_then(|n| n.to_str()) {
            if !name.is_empty() && name != "." && name != ".." {
                return Some(name.to_owned());
            }
        }
        let mut parts = Vec::new();
        for component in as_path.components() {
            match component {
                Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
                Component::CurDir | Component::ParentDir | Component::Prefix(_) | Component::RootDir => {}
            }
        }
        return if parts.is_empty() {
            Some("worktree".to_owned())
        } else {
            Some(parts.join("/"))
        };
    }
    // Already a label — still strip any accidental separators to basename.
    if let Some(name) = as_path.file_name().and_then(|n| n.to_str()) {
        if !name.is_empty() {
            return Some(name.to_owned());
        }
    }
    Some(trimmed.to_owned())
}

/// True when any JSON string value in `encoded` is an absolute filesystem path.
#[must_use]
pub fn wire_json_leaks_absolute_path(encoded: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(encoded) else {
        return false;
    };
    fn walk(value: &Value) -> bool {
        match value {
            Value::String(text) => Path::new(text).is_absolute(),
            Value::Array(items) => items.iter().any(walk),
            Value::Object(map) => map.values().any(walk),
            _ => false,
        }
    }
    walk(&value)
}

/// Execute one workflow RPC command against a host and return JSON `data`.
pub async fn dispatch_workflow_command(
    host: &dyn WorkflowRpcHost,
    command: WorkflowRpcCommand,
) -> Result<Value> {
    match command {
        WorkflowRpcCommand::WorkflowCreate { name, objective, .. } => {
            if name.trim().is_empty() {
                bail!("workflow name must not be empty");
            }
            if objective.trim().is_empty() {
                bail!("workflow objective must not be empty");
            }
            let snap = host.create(name, objective).await?;
            Ok(serde_json::to_value(project_workflow_snapshot(&snap))?)
        }
        WorkflowRpcCommand::WorkflowList { .. } => {
            let items = host
                .list()?
                .iter()
                .map(project_workflow_snapshot)
                .collect::<Vec<_>>();
            Ok(json!({ "workflows": items }))
        }
        WorkflowRpcCommand::WorkflowGet {
            workflow_id, name, ..
        } => {
            if workflow_id.as_deref().map(str::trim).unwrap_or("").is_empty()
                && name.as_deref().map(str::trim).unwrap_or("").is_empty()
            {
                bail!("workflow_get requires workflowId or name");
            }
            let snap = host.get(workflow_id.as_deref(), name.as_deref())?;
            Ok(serde_json::to_value(project_workflow_snapshot(&snap))?)
        }
        WorkflowRpcCommand::WorkflowDetail {
            workflow_id, name, ..
        } => {
            if workflow_id.as_deref().map(str::trim).unwrap_or("").is_empty()
                && name.as_deref().map(str::trim).unwrap_or("").is_empty()
            {
                bail!("workflow_detail requires workflowId or name");
            }
            let mut panel = host.detail(workflow_id.as_deref(), name.as_deref()).await?;
            // Worktree labels are display-only: never ship an absolute path.
            panel.worktree.label = redact_worktree_path(&panel.worktree.label).unwrap_or_default();
            Ok(serde_json::to_value(panel)?)
        }
        WorkflowRpcCommand::WorkflowPause { workflow_id, .. } => {
            let snap = host.pause(workflow_id.trim()).await?;
            Ok(serde_json::to_value(project_workflow_snapshot(&snap))?)
        }
        WorkflowRpcCommand::WorkflowResume { workflow_id, .. } => {
            let snap = host.resume(workflow_id.trim()).await?;
            Ok(serde_json::to_value(project_workflow_snapshot(&snap))?)
        }
        WorkflowRpcCommand::WorkflowCancel { workflow_id, .. } => {
            let snap = host.cancel(workflow_id.trim()).await?;
            Ok(serde_json::to_value(project_workflow_snapshot(&snap))?)
        }
        WorkflowRpcCommand::WorkflowIntegrate { workflow_id, .. } => {
            let snap = host.integrate(workflow_id.trim()).await?;
            Ok(serde_json::to_value(project_workflow_snapshot(&snap))?)
        }
        WorkflowRpcCommand::WorkflowRemove { workflow_id, .. } => {
            let snap = host.remove(workflow_id.trim()).await?;
            Ok(serde_json::to_value(project_workflow_snapshot(&snap))?)
        }
    }
}

/// Parse a JSON value as a workflow command; unknown fields fail closed.
pub fn parse_workflow_command(value: Value) -> Result<WorkflowRpcCommand, String> {
    serde_json::from_value(value).map_err(|error| error.to_string())
}

/// Host backed by the canonical Application-owned [`WorkflowManager`].
#[derive(Clone)]
pub struct ApplicationWorkflowHost {
    manager: WorkflowManager,
    application: Application,
}

impl ApplicationWorkflowHost {
    #[must_use]
    pub fn new(manager: WorkflowManager, application: Application) -> Self {
        Self {
            manager,
            application,
        }
    }

    fn resolve(&self, workflow_id: Option<&str>, name: Option<&str>) -> Result<WorkflowSnapshot> {
        if let Some(id) = workflow_id.map(str::trim).filter(|s| !s.is_empty()) {
            return self.manager.get(&WorkflowId::new(id));
        }
        if let Some(name) = name.map(str::trim).filter(|s| !s.is_empty()) {
            return self.manager.get_by_name(name);
        }
        bail!("workflow selector requires workflowId or name")
    }

    async fn lifecycle(
        &self,
        workflow_id: &str,
        op: LifecycleOp,
    ) -> Result<WorkflowSnapshot> {
        let current = self.manager.get(&WorkflowId::new(workflow_id))?;
        let id = current.workflow_id.clone();
        let generation = current.generation;
        match op {
            LifecycleOp::Pause => self.manager.pause(&id, generation).await,
            LifecycleOp::Resume => self.manager.resume(&id, generation).await,
            LifecycleOp::Cancel => self.manager.cancel(&id, generation).await,
            LifecycleOp::Integrate => self.manager.integrate(&id, generation).await,
            LifecycleOp::Remove => self.manager.remove(&id, generation).await,
        }
    }
}

enum LifecycleOp {
    Pause,
    Resume,
    Cancel,
    Integrate,
    Remove,
}

#[async_trait]
impl WorkflowRpcHost for ApplicationWorkflowHost {
    async fn create(&self, name: String, objective: String) -> Result<WorkflowSnapshot> {
        self.manager
            .create(WorkflowCreateRequest { name, objective })
            .await
    }

    fn list(&self) -> Result<Vec<WorkflowSnapshot>> {
        Ok(self.manager.list())
    }

    fn get(&self, workflow_id: Option<&str>, name: Option<&str>) -> Result<WorkflowSnapshot> {
        self.resolve(workflow_id, name)
    }

    async fn detail(
        &self,
        workflow_id: Option<&str>,
        name: Option<&str>,
    ) -> Result<crate::workflow_panel::WorkflowPanelSnapshot> {
        let snapshot = self.resolve(workflow_id, name)?;
        let detail = self
            .application
            .workflow_detail(&snapshot.workflow_id, snapshot.generation)?;
        Ok(crate::workflow_panel::WorkflowPanelSnapshot::from_runtime_detail(
            &detail, &snapshot,
        ))
    }

    async fn pause(&self, workflow_id: &str) -> Result<WorkflowSnapshot> {
        self.lifecycle(workflow_id, LifecycleOp::Pause).await
    }

    async fn resume(&self, workflow_id: &str) -> Result<WorkflowSnapshot> {
        self.lifecycle(workflow_id, LifecycleOp::Resume).await
    }

    async fn cancel(&self, workflow_id: &str) -> Result<WorkflowSnapshot> {
        self.lifecycle(workflow_id, LifecycleOp::Cancel).await
    }

    async fn integrate(&self, workflow_id: &str) -> Result<WorkflowSnapshot> {
        self.lifecycle(workflow_id, LifecycleOp::Integrate).await
    }

    async fn remove(&self, workflow_id: &str) -> Result<WorkflowSnapshot> {
        self.lifecycle(workflow_id, LifecycleOp::Remove).await
    }
}

/// Production RPC workflow state: Application-owned manager only.
#[derive(Clone, Default)]
pub struct WorkflowRpcState {
    host: Arc<Mutex<Option<Arc<dyn WorkflowRpcHost>>>>,
}

impl WorkflowRpcState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind the live Application-owned workflow manager.
    ///
    /// Uses `Application::workflow_manager()` when available. Does not open an
    /// independent manager from agent_dir/cwd.
    #[must_use]
    pub fn for_application(application: &Application) -> Self {
        let state = Self::new();
        if let Ok(manager) = application.workflow_manager() {
            *state.host.lock() = Some(Arc::new(ApplicationWorkflowHost::new(
                manager,
                application.clone(),
            )));
        }
        state
    }

    /// Test-only host install.
    #[cfg(test)]
    pub fn set_host(&self, host: Arc<dyn WorkflowRpcHost>) {
        *self.host.lock() = Some(host);
    }

    /// Test-only in-memory host.
    #[cfg(test)]
    pub fn set_memory_host(&self) -> MemoryWorkflowRpcHost {
        let memory = MemoryWorkflowRpcHost::new();
        self.set_host(Arc::new(memory.clone()));
        memory
    }

    pub async fn dispatch(&self, command: WorkflowRpcCommand) -> Result<Value> {
        let host = {
            let guard = self.host.lock();
            guard
                .clone()
                .ok_or_else(|| anyhow!("workflow manager is not available in this session"))?
        };
        dispatch_workflow_command(host.as_ref(), command).await
    }

    #[cfg(test)]
    pub fn with_host<R>(&self, f: impl FnOnce(&dyn WorkflowRpcHost) -> Result<R>) -> Result<R> {
        let guard = self.host.lock();
        let host = guard
            .as_ref()
            .ok_or_else(|| anyhow!("workflow manager is not available in this session"))?;
        f(host.as_ref())
    }
}

#[cfg(test)]
mod memory_host {
    use super::*;

    #[derive(Clone, Default)]
    pub struct MemoryWorkflowRpcHost {
        inner: Arc<Mutex<Vec<WorkflowSnapshot>>>,
    }

    impl MemoryWorkflowRpcHost {
        #[must_use]
        pub fn new() -> Self {
            Self::default()
        }

        fn find_mut<'a>(
            workflows: &'a mut [WorkflowSnapshot],
            workflow_id: &str,
        ) -> Result<&'a mut WorkflowSnapshot> {
            workflows
                .iter_mut()
                .find(|item| item.workflow_id.as_str() == workflow_id)
                .ok_or_else(|| anyhow!("unknown workflow {workflow_id}"))
        }

        fn bump(item: &mut WorkflowSnapshot, status: WorkflowStatus) {
            item.status = status;
            item.generation = item.generation.saturating_add(1);
        }
    }

    #[async_trait]
    impl WorkflowRpcHost for MemoryWorkflowRpcHost {
        async fn create(&self, name: String, objective: String) -> Result<WorkflowSnapshot> {
            let mut workflows = self.inner.lock();
            if workflows.iter().any(|item| item.name == name) {
                bail!("workflow name already exists: {name}");
            }
            let id = WorkflowId::new(uuid::Uuid::now_v7().to_string());
            let label = format!("wf-{}", id.as_str());
            // Absolute internal path for redaction tests only.
            let absolute = std::env::temp_dir().join("pi-workflows").join(&label);
            assert!(absolute.is_absolute());
            let snap = WorkflowSnapshot {
                workflow_id: id,
                name,
                objective,
                status: WorkflowStatus::Queued,
                created_at_ms: 1,
                updated_at_ms: 1,
                generation: 1,
                todo: pi_coding::TodoState {
                    phases: Vec::new(),
                    storage: pi_coding::TodoStorage::Memory,
                },
                // Store absolute path in label field to exercise redaction.
                worktree_label: Some(absolute.to_string_lossy().into_owned()),
                branch: Some(format!("workflow/{label}")),
                supervisor_agent_id: None,
                supervisor_job_id: None,
                failure: None,
                integration: pi_coding::WorkflowIntegration::None,
            };
            workflows.push(snap.clone());
            Ok(snap)
        }

        fn list(&self) -> Result<Vec<WorkflowSnapshot>> {
            Ok(self.inner.lock().clone())
        }

        fn get(&self, workflow_id: Option<&str>, name: Option<&str>) -> Result<WorkflowSnapshot> {
            let workflows = self.inner.lock();
            if let Some(id) = workflow_id.map(str::trim).filter(|s| !s.is_empty()) {
                return workflows
                    .iter()
                    .find(|item| item.workflow_id.as_str() == id)
                    .cloned()
                    .ok_or_else(|| anyhow!("unknown workflow {id}"));
            }
            if let Some(name) = name.map(str::trim).filter(|s| !s.is_empty()) {
                let matches: Vec<_> = workflows.iter().filter(|item| item.name == name).collect();
                return match matches.as_slice() {
                    [one] => Ok((*one).clone()),
                    [] => bail!("unknown workflow name {name}"),
                    _ => bail!("ambiguous workflow name {name}"),
                };
            }
            bail!("workflow selector requires workflowId or name")
        }

        async fn detail(
            &self,
            workflow_id: Option<&str>,
            name: Option<&str>,
        ) -> Result<crate::workflow_panel::WorkflowPanelSnapshot> {
            // Test double: no live runtime, so project the durable snapshot
            // (same fallback the Application uses for terminal workflows).
            let snapshot = self.get(workflow_id, name)?;
            Ok(crate::workflow_panel::WorkflowPanelSnapshot::from(&snapshot))
        }

        async fn pause(&self, workflow_id: &str) -> Result<WorkflowSnapshot> {
            let mut workflows = self.inner.lock();
            let item = Self::find_mut(&mut workflows, workflow_id)?;
            match item.status {
                WorkflowStatus::Running
                | WorkflowStatus::Planning
                | WorkflowStatus::Queued
                | WorkflowStatus::Integrating => {
                    Self::bump(item, WorkflowStatus::Paused);
                    Ok(item.clone())
                }
                other => bail!("cannot pause workflow in status {}", other.as_str()),
            }
        }

        async fn resume(&self, workflow_id: &str) -> Result<WorkflowSnapshot> {
            let mut workflows = self.inner.lock();
            let item = Self::find_mut(&mut workflows, workflow_id)?;
            match item.status {
                WorkflowStatus::Paused | WorkflowStatus::Queued => {
                    Self::bump(item, WorkflowStatus::Running);
                    Ok(item.clone())
                }
                other => bail!("cannot resume workflow in status {}", other.as_str()),
            }
        }

        async fn cancel(&self, workflow_id: &str) -> Result<WorkflowSnapshot> {
            let mut workflows = self.inner.lock();
            let item = Self::find_mut(&mut workflows, workflow_id)?;
            if item.status.is_terminal() {
                bail!("cannot cancel terminal workflow {}", item.status.as_str());
            }
            Self::bump(item, WorkflowStatus::Cancelled);
            Ok(item.clone())
        }

        async fn integrate(&self, workflow_id: &str) -> Result<WorkflowSnapshot> {
            let mut workflows = self.inner.lock();
            let item = Self::find_mut(&mut workflows, workflow_id)?;
            match item.status {
                WorkflowStatus::Completed | WorkflowStatus::Conflicted | WorkflowStatus::Paused => {
                    Self::bump(item, WorkflowStatus::Integrating);
                    Ok(item.clone())
                }
                other => bail!("cannot integrate workflow in status {}", other.as_str()),
            }
        }

        async fn remove(&self, workflow_id: &str) -> Result<WorkflowSnapshot> {
            let mut workflows = self.inner.lock();
            let index = workflows
                .iter()
                .position(|item| item.workflow_id.as_str() == workflow_id)
                .ok_or_else(|| anyhow!("unknown workflow {workflow_id}"))?;
            if !workflows[index].status.is_terminal() {
                bail!(
                    "remove requires a terminal workflow; status is {}",
                    workflows[index].status.as_str()
                );
            }
            Ok(workflows.remove(index))
        }
    }
}

#[cfg(test)]
pub use memory_host::MemoryWorkflowRpcHost;

#[cfg(test)]
#[path = "workflow_rpc_tests.rs"]
mod tests;
