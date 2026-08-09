use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex as AsyncMutex, broadcast};
use super::store::{StoreDiagnostic, StoreDiagnosticKind, StoreRecord, WorkflowStore};
use super::{WorkflowId, WorkflowStatus}; use crate::TodoState;
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowFailure { pub message: String }
impl fmt::Debug for WorkflowFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("WorkflowFailure").finish_non_exhaustive()
    }
}
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WorkflowIntegration {
    #[default]
    None,
    Applied { result_commit: String },
    Conflicted { conflicts: Vec<String> },
}
impl fmt::Debug for WorkflowIntegration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => formatter.write_str("WorkflowIntegration::None"),
            Self::Applied { .. } => formatter.debug_struct("WorkflowIntegration::Applied").finish_non_exhaustive(),
            Self::Conflicted { conflicts } => formatter
                .debug_struct("WorkflowIntegration::Conflicted")
                .field("conflict_count", &conflicts.len())
                .finish_non_exhaustive(),
        }
    }
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowSnapshot {
    pub workflow_id: WorkflowId,
    pub name: String,
    pub objective: String,
    pub status: WorkflowStatus,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub generation: u64,
    pub todo: TodoState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor_job_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<WorkflowFailure>,
    #[serde(default)]
    pub integration: WorkflowIntegration,
}
impl fmt::Debug for WorkflowSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("WorkflowSnapshot")
            .field("status", &self.status)
            .field("generation", &self.generation)
            .field("phase_count", &self.todo.phases.len())
            .finish_non_exhaustive()
    }
}
impl StoreRecord for WorkflowSnapshot { fn workflow_id(&self) -> &WorkflowId { &self.workflow_id } }
#[derive(Clone, PartialEq, Eq)]
pub struct WorkflowCreateRequest { pub name: String, pub objective: String }
impl fmt::Debug for WorkflowCreateRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("WorkflowCreateRequest").finish_non_exhaustive()
    }
}
#[derive(Clone, PartialEq, Eq)]
pub struct WorkflowRuntimeRequest { pub workflow_id: WorkflowId, pub name: String, pub objective: String, pub generation: u64 }
impl fmt::Debug for WorkflowRuntimeRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("WorkflowRuntimeRequest").field("generation", &self.generation).finish_non_exhaustive()
    }
}
#[derive(Clone, PartialEq, Eq)]
pub struct WorkflowRuntimeIdentity {
    pub worktree_label: Option<String>, pub branch: Option<String>,
    pub supervisor_agent_id: Option<String>, pub supervisor_job_id: Option<String>, pub todo: TodoState,
}
impl fmt::Debug for WorkflowRuntimeIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("WorkflowRuntimeIdentity").field("phase_count", &self.todo.phases.len()).finish_non_exhaustive()
    }
}
#[derive(Clone, PartialEq, Eq)]
pub struct WorkflowRuntimeUpdate {
    pub status: WorkflowStatus, pub todo: TodoState, pub supervisor_agent_id: Option<String>,
    pub supervisor_job_id: Option<String>, pub failure: Option<WorkflowFailure>, pub integration: WorkflowIntegration,
}
impl fmt::Debug for WorkflowRuntimeUpdate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkflowRuntimeUpdate")
            .field("status", &self.status)
            .field("phase_count", &self.todo.phases.len())
            .field("has_failure", &self.failure.is_some())
            .field("integration", &self.integration)
            .finish_non_exhaustive()
    }
}
#[async_trait]
pub trait WorkflowRuntimeFactory: Send + Sync {
    async fn create(&self, request: &WorkflowRuntimeRequest) -> Result<WorkflowRuntimeIdentity>;
    async fn restore(&self, request: &WorkflowRuntimeRequest, snapshot: &WorkflowSnapshot) -> Result<WorkflowRuntimeIdentity>;
    async fn pause(&self, snapshot: &WorkflowSnapshot) -> Result<WorkflowRuntimeUpdate>;
    async fn resume(&self, snapshot: &WorkflowSnapshot) -> Result<WorkflowRuntimeUpdate>;
    async fn cancel(&self, snapshot: &WorkflowSnapshot) -> Result<WorkflowRuntimeUpdate>;
    async fn integrate(&self, snapshot: &WorkflowSnapshot) -> Result<WorkflowRuntimeUpdate>;
    async fn remove(&self, snapshot: &WorkflowSnapshot) -> Result<()>;
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowEvent {
    Created { snapshot: WorkflowSnapshot }, Updated { snapshot: WorkflowSnapshot },
    StatusChanged { workflow_id: WorkflowId, generation: u64, status: WorkflowStatus },
    Removed { workflow_id: WorkflowId, generation: u64 },
}
impl fmt::Debug for WorkflowEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Created { snapshot } => formatter.debug_struct("WorkflowEvent::Created").field("snapshot", snapshot).finish(),
            Self::Updated { snapshot } => formatter.debug_struct("WorkflowEvent::Updated").field("snapshot", snapshot).finish(),
            Self::StatusChanged { generation, status, .. } => formatter
                .debug_struct("WorkflowEvent::StatusChanged")
                .field("generation", generation)
                .field("status", status)
                .finish_non_exhaustive(),
            Self::Removed { generation, .. } => formatter
                .debug_struct("WorkflowEvent::Removed")
                .field("generation", generation)
                .finish_non_exhaustive(),
        }
    }
}
#[derive(Clone, PartialEq, Eq)]
pub struct WorkflowStoreDiagnostic { pub workflow_id: Option<WorkflowId>, pub message: String }
impl fmt::Debug for WorkflowStoreDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkflowStoreDiagnostic")
            .field("has_workflow_id", &self.workflow_id.is_some())
            .finish_non_exhaustive()
    }
}
struct UnconfiguredWorkflowRuntimeFactory;
#[async_trait]
impl WorkflowRuntimeFactory for UnconfiguredWorkflowRuntimeFactory {
    async fn create(&self, _: &WorkflowRuntimeRequest) -> Result<WorkflowRuntimeIdentity> { bail!("workflow runtime factory is not configured") }
    async fn restore(&self, _: &WorkflowRuntimeRequest, _: &WorkflowSnapshot) -> Result<WorkflowRuntimeIdentity> { bail!("workflow runtime factory is not configured") }
    async fn pause(&self, _: &WorkflowSnapshot) -> Result<WorkflowRuntimeUpdate> { bail!("workflow runtime factory is not configured") }
    async fn resume(&self, _: &WorkflowSnapshot) -> Result<WorkflowRuntimeUpdate> { bail!("workflow runtime factory is not configured") }
    async fn cancel(&self, _: &WorkflowSnapshot) -> Result<WorkflowRuntimeUpdate> { bail!("workflow runtime factory is not configured") }
    async fn integrate(&self, _: &WorkflowSnapshot) -> Result<WorkflowRuntimeUpdate> { bail!("workflow runtime factory is not configured") }
    async fn remove(&self, _: &WorkflowSnapshot) -> Result<()> { bail!("workflow runtime factory is not configured") }
}
struct WorkflowManagerInner {
    store: WorkflowStore,
    factory: Arc<dyn WorkflowRuntimeFactory>,
    state: RwLock<WorkflowManagerState>,
    events: broadcast::Sender<WorkflowEvent>,
    reservations: parking_lot::Mutex<WorkflowReservations>,
    selection_gate: parking_lot::Mutex<()>,
    #[cfg(test)]
    failpoints: parking_lot::Mutex<WorkflowManagerFailpoints>,
}
#[derive(Default)]
struct WorkflowManagerState { workflows: BTreeMap<WorkflowId, WorkflowSnapshot>, selected: Option<WorkflowId>, diagnostics: Vec<WorkflowStoreDiagnostic> }
#[derive(Default)]
struct WorkflowReservations {
    names: HashSet<String>,
    operations: HashMap<WorkflowId, Arc<AsyncMutex<()>>>,
    creating: HashMap<WorkflowId, CreatingWorkflow>,
}
struct CreatingWorkflow { generation: u64, status: WorkflowStatus, projection: Option<WorkflowRuntimeUpdate> }
#[derive(Clone, Copy)]
enum WorkflowManagerFailpoint { SelectAfterWrite, CreateSelectionAfterWrite, PersistAfterWrite, RemoveSelectionAfterWrite, RemoveAfterDelete }
#[cfg(test)]
#[derive(Default)]
struct WorkflowManagerFailpoints { select_after_write: usize, create_selection_after_write: usize, persist_after_write: usize, remove_selection_after_write: usize, remove_after_delete: usize }
#[derive(Clone)]
pub struct WorkflowManager { inner: Arc<WorkflowManagerInner> }
impl fmt::Debug for WorkflowManager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result { formatter.debug_struct("WorkflowManager").field("workflow_count", &self.inner.state.read().workflows.len()).finish_non_exhaustive() }
}
#[derive(Clone)]
pub(crate) struct WorkflowRuntimeProjectionSink { inner: Weak<WorkflowManagerInner> }
impl fmt::Debug for WorkflowRuntimeProjectionSink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("WorkflowRuntimeProjectionSink").finish_non_exhaustive()
    }
}
impl WorkflowRuntimeProjectionSink {
    pub(crate) async fn project(&self, id: &WorkflowId, generation: u64, update: WorkflowRuntimeUpdate) -> Result<Option<WorkflowSnapshot>> {
        let inner = self.inner.upgrade().ok_or_else(|| anyhow!("workflow manager is unavailable"))?;
        WorkflowManager { inner }.project_runtime_update(id, generation, update).await
    }
}
impl WorkflowManager {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> { Self::open_with_factory(root, Arc::new(UnconfiguredWorkflowRuntimeFactory)) }
    pub fn open_with_factory(root: impl Into<PathBuf>, factory: Arc<dyn WorkflowRuntimeFactory>) -> Result<Self> {
        let store = WorkflowStore::open(root.into()).map_err(|_| anyhow!("workflow storage could not be opened"))?;
        let loaded = store.load_all::<WorkflowSnapshot>().map_err(|_| anyhow!("workflow records could not be loaded"))?;
        let selection = store.load_selection().map_err(|_| anyhow!("workflow selection could not be loaded"))?;
        let workflows = loaded.records.into_iter().map(|snapshot| (snapshot.workflow_id.clone(), snapshot)).collect::<BTreeMap<_, _>>();
        let selected = selection.selected.filter(|id| workflows.contains_key(id));
        let mut diagnostics = loaded.diagnostics.into_iter().map(safe_store_diagnostic).collect::<Vec<_>>();
        diagnostics.extend(selection.diagnostics.into_iter().map(safe_store_diagnostic));
        let (events, _) = broadcast::channel(256);
        Ok(Self {
            inner: Arc::new(WorkflowManagerInner {
                store,
                factory,
                state: RwLock::new(WorkflowManagerState { workflows, selected, diagnostics }),
                events,
                reservations: parking_lot::Mutex::new(WorkflowReservations::default()),
                selection_gate: parking_lot::Mutex::new(()),
                #[cfg(test)]
                failpoints: parking_lot::Mutex::new(WorkflowManagerFailpoints::default()),
            }),
        })
    }
    #[must_use] pub fn subscribe(&self) -> broadcast::Receiver<WorkflowEvent> { self.inner.events.subscribe() }
    pub(crate) fn runtime_projection_sink(&self) -> WorkflowRuntimeProjectionSink { WorkflowRuntimeProjectionSink { inner: Arc::downgrade(&self.inner) } }
    #[must_use] pub fn list(&self) -> Vec<WorkflowSnapshot> {
        let mut items = self.inner.state.read().workflows.values().cloned().collect::<Vec<_>>();
        items.sort_by(|a, b| a.created_at_ms.cmp(&b.created_at_ms).then_with(|| a.workflow_id.cmp(&b.workflow_id))); items
    }
    pub fn get(&self, id: &WorkflowId) -> Result<WorkflowSnapshot> { self.inner.state.read().workflows.get(id).cloned().ok_or_else(|| anyhow!("workflow was not found")) }
    pub fn get_by_name(&self, name: &str) -> Result<WorkflowSnapshot> {
        let items = self.inner.state.read().workflows.values().filter(|item| item.name == name).cloned().collect::<Vec<_>>();
        match items.as_slice() { [item] => Ok(item.clone()), [] => bail!("workflow was not found"), _ => bail!("workflow name is ambiguous") }
    }
    #[must_use] pub fn selected(&self) -> Option<WorkflowSnapshot> { let state = self.inner.state.read(); state.selected.as_ref().and_then(|id| state.workflows.get(id)).cloned() }
    pub fn select(&self, id: Option<&WorkflowId>) -> Result<Option<WorkflowSnapshot>> {
        let _selection = self.inner.selection_gate.lock();
        let (prior, selected) = {
            let state = self.inner.state.read();
            if let Some(id) = id && !state.workflows.contains_key(id) { bail!("workflow was not found"); }
            (state.selected.clone(), id.and_then(|id| state.workflows.get(id)).cloned())
        };
        let mut write_failed = self.inner.store.write_selection(id).is_err();
        write_failed |= self.take_failpoint(WorkflowManagerFailpoint::SelectAfterWrite);
        if write_failed {
            if self.inner.store.write_selection(prior.as_ref()).is_err() { bail!("workflow selection rollback failed"); }
            bail!("workflow selection could not be saved");
        }
        self.inner.state.write().selected = id.cloned();
        Ok(selected)
    }
    pub async fn create(&self, request: WorkflowCreateRequest) -> Result<WorkflowSnapshot> {
        let name = request.name.trim().to_owned(); let objective = request.objective.trim().to_owned();
        if name.is_empty() { bail!("workflow name must not be empty"); } if objective.is_empty() { bail!("workflow objective must not be empty"); }
        {
            let state = self.inner.state.read();
            let mut reservations = self.inner.reservations.lock();
            if state.workflows.values().any(|item| item.name == name) || !reservations.names.insert(name.clone()) { bail!("workflow name already exists"); }
        }
        let result = self.create_reserved(name.clone(), objective).await;
        self.inner.reservations.lock().names.remove(&name);
        result
    }
    async fn create_reserved(&self, name: String, objective: String) -> Result<WorkflowSnapshot> {
        let workflow_id = WorkflowId::new(uuid::Uuid::now_v7().to_string()); let generation = 1;
        let gate = self.operation_gate(&workflow_id); let operation = gate.lock().await;
        self.inner.reservations.lock().creating.insert(workflow_id.clone(), CreatingWorkflow { generation, status: WorkflowStatus::Queued, projection: None });
        let runtime_request = WorkflowRuntimeRequest { workflow_id: workflow_id.clone(), name: name.clone(), objective: objective.clone(), generation };
        let identity = match self.inner.factory.create(&runtime_request).await {
            Ok(identity) => identity,
            Err(_) => { self.clear_create_reservation(&workflow_id, true); drop(operation); bail!("workflow runtime creation failed"); }
        };
        let timestamp = now_ms();
        let mut snapshot = WorkflowSnapshot { workflow_id: workflow_id.clone(), name, objective, status: WorkflowStatus::Queued, created_at_ms: timestamp, updated_at_ms: timestamp, generation, todo: identity.todo, worktree_label: identity.worktree_label, branch: identity.branch, supervisor_agent_id: identity.supervisor_agent_id, supervisor_job_id: identity.supervisor_job_id, failure: None, integration: WorkflowIntegration::None };
        if let Some(update) = self.take_create_projection(&workflow_id, generation) { apply_runtime_projection(&mut snapshot, update); }
        let selection = self.inner.selection_gate.lock();
        let prior_selection = self.inner.state.read().selected.clone();
        if self.inner.store.write(&snapshot).is_err() {
            let record_cleanup_failed = self.inner.store.remove(&workflow_id).is_err(); self.clear_create_reservation(&workflow_id, true);
            drop(selection); drop(operation); let runtime_cleanup_failed = self.inner.factory.remove(&snapshot).await.is_err();
            if record_cleanup_failed || runtime_cleanup_failed { bail!("workflow creation rollback failed"); } bail!("workflow creation could not be saved");
        }
        let mut selection_failed = self.inner.store.write_selection(Some(&workflow_id)).is_err();
        selection_failed |= self.take_failpoint(WorkflowManagerFailpoint::CreateSelectionAfterWrite);
        if selection_failed {
            let record_cleanup_failed = self.inner.store.remove(&workflow_id).is_err();
            let selection_cleanup_failed = self.inner.store.write_selection(prior_selection.as_ref()).is_err(); self.clear_create_reservation(&workflow_id, true);
            drop(selection); drop(operation); let runtime_cleanup_failed = self.inner.factory.remove(&snapshot).await.is_err();
            if record_cleanup_failed || selection_cleanup_failed || runtime_cleanup_failed { bail!("workflow creation rollback failed"); } bail!("workflow creation could not be saved");
        }
        while let Some(update) = self.take_create_projection_or_commit(&snapshot) {
            apply_runtime_projection(&mut snapshot, update); snapshot.updated_at_ms = now_ms();
            if self.inner.store.write(&snapshot).is_err() {
                let record_cleanup_failed = self.inner.store.remove(&workflow_id).is_err();
                let selection_cleanup_failed = self.inner.store.write_selection(prior_selection.as_ref()).is_err(); self.clear_create_reservation(&workflow_id, true);
                drop(selection); drop(operation); let runtime_cleanup_failed = self.inner.factory.remove(&snapshot).await.is_err();
                if record_cleanup_failed || selection_cleanup_failed || runtime_cleanup_failed { bail!("workflow creation rollback failed"); } bail!("workflow creation could not be saved");
            }
        }
        drop(selection); drop(operation);
        let _ = self.inner.events.send(WorkflowEvent::Created { snapshot: snapshot.clone() }); Ok(snapshot)
    }
    pub async fn restore_all(&self) -> Result<Vec<WorkflowSnapshot>> {
        let snapshots = self.list().into_iter().filter(|snapshot| !snapshot.status.is_terminal()).collect::<Vec<_>>();
        let mut restored = Vec::with_capacity(snapshots.len());
        for listed in snapshots {
            let gate = self.operation_gate(&listed.workflow_id); let _operation = gate.lock().await;
            let snapshot = self.checked(&listed.workflow_id, listed.generation)?;
            if snapshot.status.is_terminal() { continue; }
            let request = WorkflowRuntimeRequest { workflow_id: snapshot.workflow_id.clone(), name: snapshot.name.clone(), objective: snapshot.objective.clone(), generation: snapshot.generation };
            match self.inner.factory.restore(&request, &snapshot).await {
                Ok(identity) => {
                    let mut next = snapshot.clone(); next.todo = identity.todo; next.worktree_label = identity.worktree_label; next.branch = identity.branch;
                    next.supervisor_agent_id = identity.supervisor_agent_id; next.supervisor_job_id = identity.supervisor_job_id; next.updated_at_ms = now_ms();
                    self.persist_replace(&snapshot, &next)?;
                    let _ = self.inner.events.send(WorkflowEvent::Updated { snapshot: next.clone() }); restored.push(next);
                }
                Err(_) => {
                    let mut failed = snapshot.clone(); failed.status = WorkflowStatus::Failed; failed.failure = Some(WorkflowFailure { message: "workflow runtime recovery failed".to_owned() }); failed.updated_at_ms = now_ms();
                    self.persist_replace(&snapshot, &failed)?;
                    if snapshot.status != failed.status { let _ = self.inner.events.send(WorkflowEvent::StatusChanged { workflow_id: failed.workflow_id.clone(), generation: failed.generation, status: failed.status }); }
                    let _ = self.inner.events.send(WorkflowEvent::Updated { snapshot: failed });
                }
            }
        }
        Ok(restored)
    }
    pub async fn pause(&self, id: &WorkflowId, generation: u64) -> Result<WorkflowSnapshot> { let gate = self.operation_gate(id); let _operation = gate.lock().await; self.lifecycle(id, generation, WorkflowAction::Pause).await }
    pub async fn resume(&self, id: &WorkflowId, generation: u64) -> Result<WorkflowSnapshot> { let gate = self.operation_gate(id); let _operation = gate.lock().await; self.lifecycle(id, generation, WorkflowAction::Resume).await }
    pub async fn cancel(&self, id: &WorkflowId, generation: u64) -> Result<WorkflowSnapshot> { let gate = self.operation_gate(id); let _operation = gate.lock().await; let current = self.checked(id, generation)?; if current.status == WorkflowStatus::Cancelled { return Ok(current); } self.lifecycle(id, generation, WorkflowAction::Cancel).await }
    pub async fn integrate(&self, id: &WorkflowId, generation: u64) -> Result<WorkflowSnapshot> { let gate = self.operation_gate(id); let _operation = gate.lock().await; self.lifecycle(id, generation, WorkflowAction::Integrate).await }
    pub(crate) async fn project_runtime_update(&self, id: &WorkflowId, generation: u64, update: WorkflowRuntimeUpdate) -> Result<Option<WorkflowSnapshot>> {
        {
            let mut reservations = self.inner.reservations.lock();
            if let Some(creating) = reservations.creating.get_mut(id) {
                if creating.generation != generation { bail!("workflow generation is stale"); }
                validate_runtime_projection(creating.status, update.status)?;
                creating.status = update.status; creating.projection = Some(update);
                return Ok(None);
            }
        }
        let gate = self.operation_gate(id); let _operation = gate.lock().await;
        let current = self.checked(id, generation)?;
        validate_runtime_projection(current.status, update.status)?;
        let mut next = current.clone(); apply_runtime_projection(&mut next, update); next.updated_at_ms = now_ms();
        self.persist_replace(&current, &next)?;
        if current.status != next.status { let _ = self.inner.events.send(WorkflowEvent::StatusChanged { workflow_id: next.workflow_id.clone(), generation: next.generation, status: next.status }); }
        let _ = self.inner.events.send(WorkflowEvent::Updated { snapshot: next.clone() });
        // Auto-integrate: when the Todo DAG settles into Completed the
        // workflow merges its worktree back to the source branch through the
        // same lifecycle path as `/workflow integrate`. Gated so the merge
        // never re-runs on a repeated Completed projection (e.g. a straggler
        // event after a manual integrate already applied/conflicted the
        // worktree) and never runs while Paused (a Paused runtime can never
        // project a status change, so this projection cannot originate from
        // one). A merge conflict lands the workflow in Conflicted for manual
        // resolution, exactly like the manual integrate path.
        if next.status == WorkflowStatus::Completed
            && current.status != WorkflowStatus::Completed
            && next.integration == WorkflowIntegration::None
        {
            drop(_operation);
            return self.lifecycle(id, generation, WorkflowAction::Integrate).await.map(Some);
        }
        Ok(Some(next))
    }
    /// Remove a workflow. Non-terminal workflows are cancelled first; a cancellation
    /// failure aborts removal and leaves the workflow in place.
    pub async fn remove(&self, id: &WorkflowId, generation: u64) -> Result<WorkflowSnapshot> {
        let gate = self.operation_gate(id); let _operation = gate.lock().await;
        let snapshot = self.checked(id, generation)?;
        let snapshot = if snapshot.status.is_terminal() { snapshot } else {
            self.lifecycle(id, generation, WorkflowAction::Cancel).await.map_err(|error| anyhow!("workflow could not be cancelled before removal: {error:#}"))?
        };
        self.inner.factory.remove(&snapshot).await.map_err(|_| anyhow!("workflow runtime removal failed"))?;
        let selection = self.inner.selection_gate.lock();
        let prior_selection = self.inner.state.read().selected.clone();
        let clear = prior_selection.as_ref() == Some(id);
        let mut selection_failed = clear && self.inner.store.write_selection(None).is_err();
        selection_failed |= clear && self.take_failpoint(WorkflowManagerFailpoint::RemoveSelectionAfterWrite);
        if selection_failed {
            let selection_rollback_failed = self.inner.store.write_selection(prior_selection.as_ref()).is_err();
            drop(selection);
            let runtime_rollback_failed = self.restore_removed_runtime(&snapshot).await;
            if selection_rollback_failed || runtime_rollback_failed { bail!("workflow removal rollback failed"); }
            bail!("workflow removal could not be committed");
        }
        let mut remove_failed = self.inner.store.remove(id).is_err();
        remove_failed |= self.take_failpoint(WorkflowManagerFailpoint::RemoveAfterDelete);
        if remove_failed {
            let record_rollback_failed = self.inner.store.write(&snapshot).is_err();
            let selection_rollback_failed = clear && self.inner.store.write_selection(prior_selection.as_ref()).is_err();
            drop(selection);
            let runtime_rollback_failed = self.restore_removed_runtime(&snapshot).await;
            if record_rollback_failed || selection_rollback_failed || runtime_rollback_failed { bail!("workflow removal rollback failed"); }
            bail!("workflow removal could not be committed");
        }
        { let mut state = self.inner.state.write(); state.workflows.remove(id); if clear { state.selected = None; } }
        drop(selection);
        self.inner.reservations.lock().operations.remove(id);
        let _ = self.inner.events.send(WorkflowEvent::Removed { workflow_id: id.clone(), generation }); Ok(snapshot)
    }
    async fn lifecycle(&self, id: &WorkflowId, generation: u64, action: WorkflowAction) -> Result<WorkflowSnapshot> {
        let current = self.checked(id, generation)?; ensure_allowed(current.status, action)?;
        let update = match action {
            WorkflowAction::Pause => self.inner.factory.pause(&current).await,
            WorkflowAction::Resume => self.inner.factory.resume(&current).await,
            WorkflowAction::Cancel => self.inner.factory.cancel(&current).await,
            WorkflowAction::Integrate => self.inner.factory.integrate(&current).await,
        }.map_err(|_| anyhow!(action.factory_error()))?;
        validate_status(action, update.status)?;
        let latest = self.checked(id, generation)?;
        if latest.status != current.status || latest.updated_at_ms != current.updated_at_ms { bail!("workflow operation result is stale"); }
        let mut next = current.clone(); next.status = update.status; next.todo = update.todo; next.supervisor_agent_id = update.supervisor_agent_id; next.supervisor_job_id = update.supervisor_job_id; next.failure = update.failure; next.integration = update.integration; next.updated_at_ms = now_ms();
        self.persist_replace(&current, &next)?;
        if current.status != next.status { let _ = self.inner.events.send(WorkflowEvent::StatusChanged { workflow_id: next.workflow_id.clone(), generation: next.generation, status: next.status }); }
        let _ = self.inner.events.send(WorkflowEvent::Updated { snapshot: next.clone() });
        // A Resume that settles the Todo DAG into Completed (the DAG finished
        // while the workflow was Paused) cannot project that settle through
        // the Paused record — the runtime projection is rejected — so the
        // settled-complete state arrives here. Run the same auto-integrate
        // merge the projection path would have run once the workflow is no
        // longer Paused. A workflow that already integrated (manual or auto)
        // carries a recorded integration and is never re-merged.
        if action == WorkflowAction::Resume
            && next.status == WorkflowStatus::Completed
            && next.integration == WorkflowIntegration::None
        {
            // Async recursion (see E0733).
            return Box::pin(self.lifecycle(id, generation, WorkflowAction::Integrate)).await;
        }
        Ok(next)
    }
    fn operation_gate(&self, id: &WorkflowId) -> Arc<AsyncMutex<()>> {
        self.inner.reservations.lock().operations.entry(id.clone()).or_insert_with(|| Arc::new(AsyncMutex::new(()))).clone()
    }
    fn checked(&self, id: &WorkflowId, generation: u64) -> Result<WorkflowSnapshot> { let item = self.get(id)?; if item.generation != generation { bail!("workflow generation is stale"); } Ok(item) }
    fn persist_replace(&self, prior: &WorkflowSnapshot, next: &WorkflowSnapshot) -> Result<()> {
        let mut write_failed = self.inner.store.write(next).is_err();
        write_failed |= self.take_failpoint(WorkflowManagerFailpoint::PersistAfterWrite);
        if write_failed {
            if self.inner.store.write(prior).is_err() { bail!("workflow durable rollback failed"); }
            bail!("workflow update could not be saved");
        }
        self.inner.state.write().workflows.insert(next.workflow_id.clone(), next.clone()); Ok(())
    }
    async fn restore_removed_runtime(&self, snapshot: &WorkflowSnapshot) -> bool {
        let request = WorkflowRuntimeRequest { workflow_id: snapshot.workflow_id.clone(), name: snapshot.name.clone(), objective: snapshot.objective.clone(), generation: snapshot.generation };
        self.inner.factory.restore(&request, snapshot).await.is_err()
    }
    fn take_create_projection(&self, id: &WorkflowId, generation: u64) -> Option<WorkflowRuntimeUpdate> {
        let mut reservations = self.inner.reservations.lock(); let creating = reservations.creating.get_mut(id)?;
        if creating.generation == generation { creating.projection.take() } else { None }
    }
    fn take_create_projection_or_commit(&self, snapshot: &WorkflowSnapshot) -> Option<WorkflowRuntimeUpdate> {
        let mut reservations = self.inner.reservations.lock();
        if let Some(update) = reservations.creating.get_mut(&snapshot.workflow_id)?.projection.take() { return Some(update); }
        reservations.creating.remove(&snapshot.workflow_id);
        let mut state = self.inner.state.write(); state.workflows.insert(snapshot.workflow_id.clone(), snapshot.clone()); state.selected = Some(snapshot.workflow_id.clone()); None
    }
    fn clear_create_reservation(&self, id: &WorkflowId, remove_operation: bool) {
        let mut reservations = self.inner.reservations.lock(); reservations.creating.remove(id);
        if remove_operation { reservations.operations.remove(id); }
    }
    #[cfg(test)]
    fn fail_next(&self, failpoint: WorkflowManagerFailpoint) {
        let mut failpoints = self.inner.failpoints.lock();
        let count = match failpoint {
            WorkflowManagerFailpoint::SelectAfterWrite => &mut failpoints.select_after_write,
            WorkflowManagerFailpoint::CreateSelectionAfterWrite => &mut failpoints.create_selection_after_write,
            WorkflowManagerFailpoint::PersistAfterWrite => &mut failpoints.persist_after_write,
            WorkflowManagerFailpoint::RemoveSelectionAfterWrite => &mut failpoints.remove_selection_after_write,
            WorkflowManagerFailpoint::RemoveAfterDelete => &mut failpoints.remove_after_delete,
        };
        *count += 1;
    }
    #[cfg(test)]
    fn take_failpoint(&self, failpoint: WorkflowManagerFailpoint) -> bool {
        let mut failpoints = self.inner.failpoints.lock();
        let count = match failpoint {
            WorkflowManagerFailpoint::SelectAfterWrite => &mut failpoints.select_after_write,
            WorkflowManagerFailpoint::CreateSelectionAfterWrite => &mut failpoints.create_selection_after_write,
            WorkflowManagerFailpoint::PersistAfterWrite => &mut failpoints.persist_after_write,
            WorkflowManagerFailpoint::RemoveSelectionAfterWrite => &mut failpoints.remove_selection_after_write,
            WorkflowManagerFailpoint::RemoveAfterDelete => &mut failpoints.remove_after_delete,
        };
        if *count > 0 { *count -= 1; true } else { false }
    }
    #[cfg(not(test))]
    const fn take_failpoint(&self, _: WorkflowManagerFailpoint) -> bool { false }
}

#[derive(Clone, Copy, PartialEq, Eq)] enum WorkflowAction { Pause, Resume, Cancel, Integrate }
impl WorkflowAction {
    const fn factory_error(self) -> &'static str {
        match self {
            Self::Pause => "workflow runtime pause failed",
            Self::Resume => "workflow runtime resume failed",
            Self::Cancel => "workflow runtime cancellation failed",
            Self::Integrate => "workflow runtime integration failed",
        }
    }
}
fn ensure_allowed(status: WorkflowStatus, action: WorkflowAction) -> Result<()> {
    let ok = match action { WorkflowAction::Pause => matches!(status, WorkflowStatus::Queued | WorkflowStatus::Planning | WorkflowStatus::Running), WorkflowAction::Resume => matches!(status, WorkflowStatus::Paused | WorkflowStatus::Planning | WorkflowStatus::Running), WorkflowAction::Cancel => !status.is_terminal(), WorkflowAction::Integrate => matches!(status, WorkflowStatus::Completed | WorkflowStatus::Paused | WorkflowStatus::Conflicted) }; if ok { Ok(()) } else { bail!("workflow lifecycle transition is not allowed") }
}
fn validate_status(action: WorkflowAction, status: WorkflowStatus) -> Result<()> {
    let ok = match action { WorkflowAction::Pause => status == WorkflowStatus::Paused, WorkflowAction::Resume => matches!(status, WorkflowStatus::Planning | WorkflowStatus::Running | WorkflowStatus::Completed | WorkflowStatus::Failed), WorkflowAction::Cancel => status == WorkflowStatus::Cancelled, WorkflowAction::Integrate => matches!(status, WorkflowStatus::Completed | WorkflowStatus::Conflicted | WorkflowStatus::Failed) }; if ok { Ok(()) } else { bail!("workflow runtime returned an invalid lifecycle status") }
}
fn validate_runtime_projection(prior: WorkflowStatus, next: WorkflowStatus) -> Result<()> {
    let allowed = prior == next || match prior {
        WorkflowStatus::Queued => matches!(next, WorkflowStatus::Planning | WorkflowStatus::Running | WorkflowStatus::Completed | WorkflowStatus::Failed),
        WorkflowStatus::Planning => matches!(next, WorkflowStatus::Running | WorkflowStatus::Completed | WorkflowStatus::Failed),
        WorkflowStatus::Running => matches!(next, WorkflowStatus::Completed | WorkflowStatus::Failed),
        WorkflowStatus::Paused | WorkflowStatus::Integrating | WorkflowStatus::Completed | WorkflowStatus::Failed | WorkflowStatus::Cancelled | WorkflowStatus::Conflicted => false,
    };
    if allowed { Ok(()) } else { bail!("workflow runtime projection is invalid") }
}
fn apply_runtime_projection(snapshot: &mut WorkflowSnapshot, update: WorkflowRuntimeUpdate) {
    snapshot.status = update.status;
    snapshot.todo = update.todo;
    snapshot.supervisor_agent_id = update.supervisor_agent_id;
    snapshot.supervisor_job_id = update.supervisor_job_id;
    snapshot.failure = update.failure;
}
fn safe_store_diagnostic(diagnostic: StoreDiagnostic) -> WorkflowStoreDiagnostic {
    let message = match diagnostic.kind {
        StoreDiagnosticKind::CorruptRecord => "ignored a corrupt workflow record",
        StoreDiagnosticKind::InvalidRecordName => "ignored a workflow record with an invalid name",
        StoreDiagnosticKind::UnsupportedRecordVersion => "ignored a workflow record with an unsupported version",
        StoreDiagnosticKind::RecordIdentityMismatch => "ignored a workflow record with inconsistent identity",
        StoreDiagnosticKind::RecordLimitExceeded => "ignored a workflow record exceeding storage limits",
        StoreDiagnosticKind::UnsafeRecord => "ignored an unsafe workflow record",
        StoreDiagnosticKind::CorruptSelection => "ignored corrupt workflow selection",
        StoreDiagnosticKind::UnsupportedSelectionVersion => "ignored workflow selection with an unsupported version",
        StoreDiagnosticKind::UnsafeSelection => "ignored unsafe workflow selection",
    };
    WorkflowStoreDiagnostic { workflow_id: diagnostic.record_id, message: message.to_owned() }
}
fn now_ms() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).ok().and_then(|value| u64::try_from(value.as_millis()).ok()).unwrap_or(0) }
#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::anyhow; use parking_lot::Mutex; use tokio::sync::{Barrier, Notify};
    use super::*;
    use crate::TodoStorage;

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    enum FactoryCall { Create, Restore, Pause, Resume, Cancel, Integrate, Remove }
    impl Default for FakeFactory {
        fn default() -> Self {
            Self {
                calls: Mutex::new(Vec::new()), failures: Mutex::new(HashMap::new()), pause_started: Notify::new(), pause_release: Notify::new(),
                block_pause: AtomicUsize::new(0), projection_sink: Mutex::new(None), create_projection: Mutex::new(Vec::new()),
                integrate_outcome: Mutex::new(None), resume_outcome: Mutex::new(None),
            }
        }
    }

    struct FakeFactory {
        calls: Mutex<Vec<(FactoryCall, WorkflowId)>>,
        failures: Mutex<HashMap<FactoryCall, VecDeque<String>>>,
        pause_started: Notify,
        pause_release: Notify,
        block_pause: AtomicUsize,
        projection_sink: Mutex<Option<WorkflowRuntimeProjectionSink>>,
        create_projection: Mutex<Vec<WorkflowRuntimeUpdate>>,
        /// Overrides the status the fake integrate returns (default: Conflicted).
        integrate_outcome: Mutex<Option<WorkflowStatus>>,
        /// Overrides the status the fake resume returns (default: Running).
        resume_outcome: Mutex<Option<WorkflowStatus>>,
    }
    impl FakeFactory {
        fn identity() -> WorkflowRuntimeIdentity {
            WorkflowRuntimeIdentity {
                worktree_label: Some("private-worktree".to_owned()),
                branch: Some("private-branch".to_owned()),
                supervisor_agent_id: Some("private-agent".to_owned()),
                supervisor_job_id: Some("private-job".to_owned()),
                todo: todo(),
            }
        }
        fn update(status: WorkflowStatus) -> WorkflowRuntimeUpdate {
            WorkflowRuntimeUpdate {
                status,
                todo: todo(),
                supervisor_agent_id: Some("updated-private-agent".to_owned()),
                supervisor_job_id: Some("updated-private-job".to_owned()),
                failure: None,
                integration: WorkflowIntegration::None,
            }
        }
        fn record(&self, call: FactoryCall, id: &WorkflowId) -> Result<()> {
            self.calls.lock().push((call, id.clone()));
            if let Some(message) = self.failures.lock().get_mut(&call).and_then(VecDeque::pop_front) { Err(anyhow!(message)) } else { Ok(()) }
        }
        fn fail(&self, call: FactoryCall, message: &str) { self.failures.lock().entry(call).or_default().push_back(message.to_owned()); }
        fn count(&self, call: FactoryCall) -> usize { self.calls.lock().iter().filter(|(seen, _)| *seen == call).count() }
        fn project_on_create(&self, sink: WorkflowRuntimeProjectionSink, update: WorkflowRuntimeUpdate) {
            *self.projection_sink.lock() = Some(sink); self.create_projection.lock().push(update);
        }
        fn block_next_pause(&self) { self.block_pause.store(1, Ordering::SeqCst); }
    }
    #[async_trait]
    impl WorkflowRuntimeFactory for FakeFactory {
        async fn create(&self, request: &WorkflowRuntimeRequest) -> Result<WorkflowRuntimeIdentity> {
            self.record(FactoryCall::Create, &request.workflow_id)?;
            let sink = self.projection_sink.lock().clone(); let updates = std::mem::take(&mut *self.create_projection.lock());
            if let Some(sink) = sink { for update in updates { sink.project(&request.workflow_id, request.generation, update).await?; } }
            Ok(Self::identity())
        }
        async fn restore(&self, request: &WorkflowRuntimeRequest, _: &WorkflowSnapshot) -> Result<WorkflowRuntimeIdentity> {
            self.record(FactoryCall::Restore, &request.workflow_id)?; Ok(Self::identity())
        }
        async fn pause(&self, snapshot: &WorkflowSnapshot) -> Result<WorkflowRuntimeUpdate> {
            self.record(FactoryCall::Pause, &snapshot.workflow_id)?;
            if self.block_pause.swap(0, Ordering::SeqCst) == 1 { self.pause_started.notify_one(); self.pause_release.notified().await; }
            Ok(Self::update(WorkflowStatus::Paused))
        }
        async fn resume(&self, snapshot: &WorkflowSnapshot) -> Result<WorkflowRuntimeUpdate> {
            self.record(FactoryCall::Resume, &snapshot.workflow_id)?;
            let status = self.resume_outcome.lock().unwrap_or(WorkflowStatus::Running);
            Ok(Self::update(status))
        }
        async fn cancel(&self, snapshot: &WorkflowSnapshot) -> Result<WorkflowRuntimeUpdate> {
            self.record(FactoryCall::Cancel, &snapshot.workflow_id)?; Ok(Self::update(WorkflowStatus::Cancelled))
        }
        async fn integrate(&self, snapshot: &WorkflowSnapshot) -> Result<WorkflowRuntimeUpdate> {
            self.record(FactoryCall::Integrate, &snapshot.workflow_id)?;
            let status = self.integrate_outcome.lock().unwrap_or(WorkflowStatus::Conflicted);
            let mut update = Self::update(status);
            update.integration = match status {
                WorkflowStatus::Completed => WorkflowIntegration::Applied { result_commit: "private-result-commit".to_owned() },
                WorkflowStatus::Conflicted => WorkflowIntegration::Conflicted { conflicts: vec!["private/path".to_owned()] },
                _ => WorkflowIntegration::None,
            };
            Ok(update)
        }
        async fn remove(&self, snapshot: &WorkflowSnapshot) -> Result<()> { self.record(FactoryCall::Remove, &snapshot.workflow_id) }
    }

    fn todo() -> TodoState { TodoState { phases: Vec::new(), storage: TodoStorage::Memory } }
    fn request(name: &str) -> WorkflowCreateRequest { WorkflowCreateRequest { name: name.to_owned(), objective: format!("objective-{name}") } }
    fn manager(factory: Arc<FakeFactory>) -> (tempfile::TempDir, WorkflowManager) {
        let directory = tempfile::tempdir().expect("temporary workflow directory");
        let manager = WorkflowManager::open_with_factory(directory.path(), factory).expect("open workflow manager");
        (directory, manager)
    }
    fn reload(directory: &tempfile::TempDir, factory: Arc<FakeFactory>) -> WorkflowManager {
        WorkflowManager::open_with_factory(directory.path(), factory).expect("reload workflow manager")
    }
    async fn created(manager: &WorkflowManager, name: &str) -> WorkflowSnapshot { manager.create(request(name)).await.expect("create workflow") }
    async fn cancelled(manager: &WorkflowManager, name: &str) -> WorkflowSnapshot {
        let snapshot = created(manager, name).await;
        manager.cancel(&snapshot.workflow_id, snapshot.generation).await.expect("cancel workflow")
    }

    #[tokio::test]
    async fn concurrent_duplicate_names_have_one_durable_winner() {
        let factory = Arc::new(FakeFactory::default()); let (directory, manager) = manager(factory.clone());
        let barrier = Arc::new(Barrier::new(3));
        let first = { let manager = manager.clone(); let barrier = barrier.clone(); tokio::spawn(async move { barrier.wait().await; manager.create(request("same")).await }) };
        let second = { let manager = manager.clone(); let barrier = barrier.clone(); tokio::spawn(async move { barrier.wait().await; manager.create(request("same")).await }) };
        barrier.wait().await;
        let outcomes = [first.await.expect("first join"), second.await.expect("second join")];
        assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(outcomes.iter().filter(|result| result.as_ref().is_err_and(|error| error.to_string() == "workflow name already exists")).count(), 1);
        assert_eq!(reload(&directory, factory).list().len(), 1);
    }

    #[tokio::test]
    async fn stale_generation_is_rejected_before_factory_call() {
        let factory = Arc::new(FakeFactory::default()); let (_directory, manager) = manager(factory.clone()); let snapshot = created(&manager, "stale").await;
        let error = manager.pause(&snapshot.workflow_id, snapshot.generation + 1).await.expect_err("stale generation must fail");
        assert_eq!(error.to_string(), "workflow generation is stale"); assert_eq!(factory.count(FactoryCall::Pause), 0);
    }

    #[tokio::test]
    async fn pause_and_cancel_are_serialized_per_workflow() {
        let factory = Arc::new(FakeFactory::default()); factory.block_next_pause(); let (_directory, manager) = manager(factory.clone()); let snapshot = created(&manager, "serialized").await;
        let generation = snapshot.generation;
        let pause = { let manager = manager.clone(); let id = snapshot.workflow_id.clone(); tokio::spawn(async move { manager.pause(&id, generation).await }) };
        factory.pause_started.notified().await;
        let cancel = { let manager = manager.clone(); let id = snapshot.workflow_id.clone(); tokio::spawn(async move { manager.cancel(&id, generation).await }) };
        tokio::task::yield_now().await; assert_eq!(factory.count(FactoryCall::Cancel), 0);
        factory.pause_release.notify_one(); assert_eq!(pause.await.expect("pause join").expect("pause").status, WorkflowStatus::Paused);
        assert_eq!(cancel.await.expect("cancel join").expect("cancel").status, WorkflowStatus::Cancelled);
        assert_eq!(factory.count(FactoryCall::Pause), 1); assert_eq!(factory.count(FactoryCall::Cancel), 1);
    }

    #[tokio::test]
    async fn remove_failure_restores_runtime_record_selection_and_memory() {
        let factory = Arc::new(FakeFactory::default()); let (directory, manager) = manager(factory.clone()); let snapshot = cancelled(&manager, "remove-rollback").await;
        manager.fail_next(WorkflowManagerFailpoint::RemoveAfterDelete);
        let error = manager.remove(&snapshot.workflow_id, snapshot.generation).await.expect_err("injected remove failure");
        assert_eq!(error.to_string(), "workflow removal could not be committed"); assert_eq!(factory.count(FactoryCall::Restore), 1);
        assert_eq!(manager.get(&snapshot.workflow_id).expect("memory restored"), snapshot); assert_eq!(manager.selected(), Some(snapshot.clone()));
        let reopened = reload(&directory, factory); assert_eq!(reopened.get(&snapshot.workflow_id).expect("record restored"), snapshot); assert_eq!(reopened.selected(), Some(snapshot));
    }

    #[tokio::test]
    async fn remove_non_terminal_auto_cancels_then_removes() {
        let factory = Arc::new(FakeFactory::default()); let (_directory, manager) = manager(factory.clone());
        let snapshot = created(&manager, "auto-cancel").await;
        let planning = manager.project_runtime_update(&snapshot.workflow_id, snapshot.generation, FakeFactory::update(WorkflowStatus::Planning)).await.expect("project planning").expect("existing snapshot");
        assert_eq!(planning.status, WorkflowStatus::Planning);
        let removed = manager.remove(&planning.workflow_id, planning.generation).await.expect("remove auto-cancels");
        assert_eq!(removed.status, WorkflowStatus::Cancelled);
        assert_eq!(factory.calls.lock().clone(), vec![
            (FactoryCall::Create, planning.workflow_id.clone()),
            (FactoryCall::Cancel, planning.workflow_id.clone()),
            (FactoryCall::Remove, planning.workflow_id.clone()),
        ]);
        assert!(manager.list().is_empty());
    }

    #[tokio::test]
    async fn remove_terminal_skips_cancel() {
        let factory = Arc::new(FakeFactory::default()); let (_directory, manager) = manager(factory.clone());
        let snapshot = cancelled(&manager, "terminal-remove").await;
        let removed = manager.remove(&snapshot.workflow_id, snapshot.generation).await.expect("remove terminal");
        assert_eq!(removed, snapshot);
        assert_eq!(factory.calls.lock().clone(), vec![
            (FactoryCall::Create, snapshot.workflow_id.clone()),
            (FactoryCall::Cancel, snapshot.workflow_id.clone()),
            (FactoryCall::Remove, snapshot.workflow_id.clone()),
        ]);
        assert!(manager.list().is_empty());
    }

    #[tokio::test]
    async fn remove_non_terminal_cancel_failure_is_actionable() {
        let secret = "private cancellation failure /secret/path";
        let factory = Arc::new(FakeFactory::default()); let (_directory, manager) = manager(factory.clone());
        let snapshot = created(&manager, "cancel-failure").await;
        factory.fail(FactoryCall::Cancel, secret);
        let error = manager.remove(&snapshot.workflow_id, snapshot.generation).await.expect_err("cancel failure must surface");
        assert_eq!(error.to_string(), "workflow could not be cancelled before removal: workflow runtime cancellation failed");
        assert!(!error.to_string().contains(secret));
        assert_eq!(factory.count(FactoryCall::Remove), 0);
        assert_eq!(manager.get(&snapshot.workflow_id).expect("workflow remains"), snapshot);
    }

    #[tokio::test]
    async fn lifecycle_durable_failure_rewrites_prior_snapshot() {
        let factory = Arc::new(FakeFactory::default()); let (directory, manager) = manager(factory.clone()); let snapshot = created(&manager, "persist-rollback").await;
        manager.fail_next(WorkflowManagerFailpoint::PersistAfterWrite);
        let error = manager.pause(&snapshot.workflow_id, snapshot.generation).await.expect_err("injected persist failure");
        assert_eq!(error.to_string(), "workflow update could not be saved"); assert_eq!(manager.get(&snapshot.workflow_id).expect("memory prior"), snapshot);
        assert_eq!(reload(&directory, factory).get(&snapshot.workflow_id).expect("disk prior"), snapshot);
    }

    #[tokio::test]
    async fn restore_failure_emits_status_before_updated() {
        let first_factory = Arc::new(FakeFactory::default()); let (directory, first) = manager(first_factory); let snapshot = created(&first, "restore-order").await;
        let factory = Arc::new(FakeFactory::default()); factory.fail(FactoryCall::Restore, "private restore failure /secret/path"); let manager = reload(&directory, factory); let mut events = manager.subscribe();
        assert!(manager.restore_all().await.expect("restore all").is_empty());
        assert!(matches!(events.recv().await.expect("status event"), WorkflowEvent::StatusChanged { status: WorkflowStatus::Failed, .. }));
        assert!(matches!(events.recv().await.expect("updated event"), WorkflowEvent::Updated { snapshot: failed } if failed.workflow_id == snapshot.workflow_id && failed.status == WorkflowStatus::Failed));
    }

    #[tokio::test]
    async fn factory_errors_and_creation_cleanup_errors_are_safe() {
        let secret = "private-id private-label private-branch /private/path";
        let create_factory = Arc::new(FakeFactory::default()); create_factory.fail(FactoryCall::Create, secret); let (_directory, create_manager) = manager(create_factory);
        let create_error = create_manager.create(request("factory-safe")).await.expect_err("create factory failure");
        assert_eq!(create_error.to_string(), "workflow runtime creation failed"); assert!(!create_error.to_string().contains(secret));

        let factory = Arc::new(FakeFactory::default()); let (_directory, workflow_manager) = manager(factory.clone()); let snapshot = created(&workflow_manager, "pause-safe").await;
        factory.fail(FactoryCall::Pause, secret); let pause_error = workflow_manager.pause(&snapshot.workflow_id, snapshot.generation).await.expect_err("pause factory failure");
        assert_eq!(pause_error.to_string(), "workflow runtime pause failed"); assert!(!pause_error.to_string().contains(secret));

        let cleanup_factory = Arc::new(FakeFactory::default()); let (_directory, cleanup_manager) = manager(cleanup_factory.clone()); cleanup_manager.fail_next(WorkflowManagerFailpoint::CreateSelectionAfterWrite); cleanup_factory.fail(FactoryCall::Remove, secret);
        let cleanup_error = cleanup_manager.create(request("cleanup-safe")).await.expect_err("cleanup failure");
        assert_eq!(cleanup_error.to_string(), "workflow creation rollback failed"); assert!(!cleanup_error.to_string().contains(secret));
    }

    #[tokio::test]
    async fn pause_resume_cancel_and_integrate_cover_lifecycle_contracts() {
        let factory = Arc::new(FakeFactory::default()); let (_directory, manager) = manager(factory.clone());
        let first = created(&manager, "lifecycle").await;
        let paused = manager.pause(&first.workflow_id, first.generation).await.expect("pause");
        let resumed = manager.resume(&paused.workflow_id, paused.generation).await.expect("resume");
        assert_eq!(resumed.status, WorkflowStatus::Running);
        let cancelled = manager.cancel(&resumed.workflow_id, resumed.generation).await.expect("cancel");
        assert_eq!(cancelled.status, WorkflowStatus::Cancelled);
        assert_eq!(manager.cancel(&cancelled.workflow_id, cancelled.generation).await.expect("repeat cancel"), cancelled);
        assert_eq!(factory.count(FactoryCall::Cancel), 1);

        let second = created(&manager, "integration").await;
        let paused = manager.pause(&second.workflow_id, second.generation).await.expect("pause for integration");
        let integrated = manager.integrate(&paused.workflow_id, paused.generation).await.expect("integrate");
        assert_eq!(integrated.status, WorkflowStatus::Conflicted);
        assert!(matches!(integrated.integration, WorkflowIntegration::Conflicted { ref conflicts } if conflicts.len() == 1));
    }

    #[tokio::test]
    async fn independent_workflows_are_not_globally_serialized() {
        let factory = Arc::new(FakeFactory::default()); let (_directory, manager) = manager(factory.clone());
        let first = created(&manager, "first-independent").await; let second = created(&manager, "second-independent").await;
        factory.block_next_pause(); let first_generation = first.generation;
        let first_pause = { let manager = manager.clone(); let id = first.workflow_id.clone(); tokio::spawn(async move { manager.pause(&id, first_generation).await }) };
        factory.pause_started.notified().await;
        let second_paused = tokio::time::timeout(std::time::Duration::from_secs(1), manager.pause(&second.workflow_id, second.generation)).await.expect("second workflow not blocked").expect("pause second");
        assert_eq!(second_paused.status, WorkflowStatus::Paused);
        factory.pause_release.notify_one(); assert_eq!(first_pause.await.expect("first join").expect("pause first").status, WorkflowStatus::Paused);
    }

    #[tokio::test]
    async fn selection_failure_restores_prior_durable_and_memory_selection() {
        let factory = Arc::new(FakeFactory::default()); let (directory, manager) = manager(factory.clone());
        let first = created(&manager, "selected-first").await; let second = created(&manager, "selected-second").await;
        assert_eq!(manager.selected(), Some(second.clone()));
        manager.fail_next(WorkflowManagerFailpoint::SelectAfterWrite);
        let error = manager.select(Some(&first.workflow_id)).expect_err("injected selection failure");
        assert_eq!(error.to_string(), "workflow selection could not be saved"); assert_eq!(manager.selected(), Some(second.clone()));
        assert_eq!(reload(&directory, factory).selected(), Some(second));
    }

    #[tokio::test]
    async fn remove_compensation_failure_reports_only_safe_message() {
        let secret = "private restore failure /secret/path";
        let factory = Arc::new(FakeFactory::default()); let (_directory, manager) = manager(factory.clone()); let snapshot = cancelled(&manager, "remove-rollback-failure").await;
        manager.fail_next(WorkflowManagerFailpoint::RemoveAfterDelete); factory.fail(FactoryCall::Restore, secret);
        let error = manager.remove(&snapshot.workflow_id, snapshot.generation).await.expect_err("rollback must report failure");
        assert_eq!(error.to_string(), "workflow removal rollback failed"); assert!(!error.to_string().contains(secret));
        assert_eq!(manager.get(&snapshot.workflow_id).expect("memory remains prior"), snapshot);
    }

    #[tokio::test]
    async fn runtime_projection_persists_orders_events_and_rejects_stale_or_terminal_regression() {
        let factory = Arc::new(FakeFactory::default()); let (directory, manager) = manager(factory.clone()); let snapshot = created(&manager, "projection").await; let mut events = manager.subscribe();
        let running = manager.project_runtime_update(&snapshot.workflow_id, snapshot.generation, FakeFactory::update(WorkflowStatus::Running)).await.expect("project running").expect("existing snapshot"); assert_eq!(running.status, WorkflowStatus::Running);
        assert!(matches!(events.recv().await.expect("status"), WorkflowEvent::StatusChanged { status: WorkflowStatus::Running, .. })); assert!(matches!(events.recv().await.expect("updated"), WorkflowEvent::Updated { snapshot } if snapshot.status == WorkflowStatus::Running));
        let completed = manager.project_runtime_update(&snapshot.workflow_id, snapshot.generation, FakeFactory::update(WorkflowStatus::Completed)).await.expect("project completed").expect("existing snapshot");
        // The settled DAG auto-integrates through the same lifecycle as
        // `/workflow integrate`: the fake runtime merge conflicts, so the
        // workflow lands Conflicted with the merge recorded.
        assert_eq!(completed.status, WorkflowStatus::Conflicted);
        assert!(matches!(&completed.integration, WorkflowIntegration::Conflicted { conflicts } if conflicts.as_slice() == ["private/path"]));
        assert_eq!(factory.count(FactoryCall::Integrate), 1);
        assert!(matches!(events.recv().await.expect("status"), WorkflowEvent::StatusChanged { status: WorkflowStatus::Completed, .. }));
        assert!(matches!(events.recv().await.expect("updated"), WorkflowEvent::Updated { snapshot } if snapshot.status == WorkflowStatus::Completed && snapshot.integration == WorkflowIntegration::None));
        assert!(matches!(events.recv().await.expect("status"), WorkflowEvent::StatusChanged { status: WorkflowStatus::Conflicted, .. }));
        assert!(matches!(events.recv().await.expect("updated"), WorkflowEvent::Updated { snapshot } if snapshot.status == WorkflowStatus::Conflicted));
        assert_eq!(reload(&directory, factory).get(&snapshot.workflow_id).expect("durable projection"), completed);
        assert_eq!(manager.project_runtime_update(&snapshot.workflow_id, snapshot.generation + 1, FakeFactory::update(WorkflowStatus::Completed)).await.expect_err("stale").to_string(), "workflow generation is stale");
        assert_eq!(manager.project_runtime_update(&snapshot.workflow_id, snapshot.generation, FakeFactory::update(WorkflowStatus::Running)).await.expect_err("terminal regression").to_string(), "workflow runtime projection is invalid");
    }
    #[tokio::test]
    async fn dag_settle_auto_integrates_once_and_repeat_completed_is_noop() {
        let factory = Arc::new(FakeFactory::default());
        *factory.integrate_outcome.lock() = Some(WorkflowStatus::Completed);
        let (_directory, manager) = manager(factory.clone());
        let snapshot = created(&manager, "auto-integrate-once").await;
        manager.project_runtime_update(&snapshot.workflow_id, snapshot.generation, FakeFactory::update(WorkflowStatus::Running)).await.expect("project running").expect("existing snapshot");
        let integrated = manager.project_runtime_update(&snapshot.workflow_id, snapshot.generation, FakeFactory::update(WorkflowStatus::Completed)).await.expect("project completed").expect("existing snapshot");
        assert_eq!(integrated.status, WorkflowStatus::Completed);
        assert!(matches!(integrated.integration, WorkflowIntegration::Applied { ref result_commit } if result_commit == "private-result-commit"));
        assert_eq!(factory.count(FactoryCall::Integrate), 1);
        // A repeated Completed projection (e.g. a straggler event after the
        // merge applied) never re-runs the merge.
        let again = manager.project_runtime_update(&snapshot.workflow_id, snapshot.generation, FakeFactory::update(WorkflowStatus::Completed)).await.expect("repeat completed").expect("existing snapshot");
        assert_eq!(again.status, WorkflowStatus::Completed);
        assert!(matches!(again.integration, WorkflowIntegration::Applied { .. }));
        assert_eq!(factory.count(FactoryCall::Integrate), 1);
    }
    #[tokio::test]
    async fn paused_workflow_projection_cannot_settle_or_auto_integrate() {
        let factory = Arc::new(FakeFactory::default()); let (_directory, manager) = manager(factory.clone());
        let snapshot = created(&manager, "paused-settle").await;
        let paused = manager.pause(&snapshot.workflow_id, snapshot.generation).await.expect("pause");
        assert_eq!(paused.status, WorkflowStatus::Paused);
        // A Paused runtime never projects a status change; even a forged
        // Completed projection is rejected, so the merge can never run while
        // the workflow is paused.
        let error = manager.project_runtime_update(&paused.workflow_id, paused.generation, FakeFactory::update(WorkflowStatus::Completed)).await.expect_err("paused settle must be rejected");
        assert_eq!(error.to_string(), "workflow runtime projection is invalid");
        assert_eq!(factory.count(FactoryCall::Integrate), 0);
        assert_eq!(manager.get(&paused.workflow_id).expect("paused record").status, WorkflowStatus::Paused);
    }
    #[tokio::test]
    async fn resume_settling_completed_dag_auto_integrates() {
        let factory = Arc::new(FakeFactory::default());
        *factory.integrate_outcome.lock() = Some(WorkflowStatus::Completed);
        *factory.resume_outcome.lock() = Some(WorkflowStatus::Completed);
        let (_directory, manager) = manager(factory.clone());
        let snapshot = created(&manager, "resume-settle").await;
        let paused = manager.pause(&snapshot.workflow_id, snapshot.generation).await.expect("pause");
        // The DAG settles while paused; resume returns Completed and the
        // manager auto-integrates through the same lifecycle path.
        let integrated = manager.resume(&paused.workflow_id, paused.generation).await.expect("resume settle");
        assert_eq!(integrated.status, WorkflowStatus::Completed);
        assert!(matches!(integrated.integration, WorkflowIntegration::Applied { .. }));
        assert_eq!(factory.count(FactoryCall::Integrate), 1);
    }
    #[tokio::test]
    async fn create_folds_latest_synchronous_projection_without_deadlock() {
        let factory = Arc::new(FakeFactory::default()); let (directory, manager) = manager(factory.clone()); let sink = manager.runtime_projection_sink();
        factory.project_on_create(sink.clone(), FakeFactory::update(WorkflowStatus::Planning)); factory.project_on_create(sink, FakeFactory::update(WorkflowStatus::Completed));
        let snapshot = tokio::time::timeout(std::time::Duration::from_secs(1), manager.create(request("create-race"))).await.expect("no deadlock").expect("create"); assert_eq!(snapshot.status, WorkflowStatus::Completed);
        assert_eq!(reload(&directory, factory).get(&snapshot.workflow_id).expect("first durable projection").status, WorkflowStatus::Completed);
    }
    #[tokio::test]
    async fn projection_rejects_updates_before_record_or_exact_create_reservation() {
        let factory = Arc::new(FakeFactory::default()); let (_directory, manager) = manager(factory); let id = WorkflowId::new("private-missing-id");
        let error = manager.runtime_projection_sink().project(&id, 1, FakeFactory::update(WorkflowStatus::Running)).await.expect_err("missing workflow"); assert_eq!(error.to_string(), "workflow was not found"); assert!(!format!("{error:?}").contains(id.as_str()));
    }

    #[test]
    fn debug_output_redacts_public_manager_payloads() {
        let secret = "private-id private-label private-branch private-agent private-job /private/path";
        let snapshot = WorkflowSnapshot {
            workflow_id: WorkflowId::new("private-id"), name: "private-label".to_owned(), objective: "/private/path".to_owned(), status: WorkflowStatus::Failed,
            created_at_ms: 1, updated_at_ms: 2, generation: 3, todo: todo(), worktree_label: Some("private-label".to_owned()), branch: Some("private-branch".to_owned()),
            supervisor_agent_id: Some("private-agent".to_owned()), supervisor_job_id: Some("private-job".to_owned()), failure: Some(WorkflowFailure { message: secret.to_owned() }),
            integration: WorkflowIntegration::Conflicted { conflicts: vec!["/private/path".to_owned()] },
        };
        let values = [
            format!("{:?}", WorkflowCreateRequest { name: secret.to_owned(), objective: secret.to_owned() }),
            format!("{:?}", WorkflowRuntimeRequest { workflow_id: snapshot.workflow_id.clone(), name: secret.to_owned(), objective: secret.to_owned(), generation: 3 }),
            format!("{:?}", FakeFactory::identity()),
            format!("{:?}", WorkflowRuntimeUpdate { status: WorkflowStatus::Failed, todo: todo(), supervisor_agent_id: Some(secret.to_owned()), supervisor_job_id: Some(secret.to_owned()), failure: snapshot.failure.clone(), integration: snapshot.integration.clone() }),
            format!("{:?}", WorkflowEvent::Updated { snapshot: snapshot.clone() }),
            format!("{:?}", WorkflowStoreDiagnostic { workflow_id: Some(snapshot.workflow_id.clone()), message: secret.to_owned() }),
            format!("{:?}", snapshot.failure.clone().expect("failure")),
            format!("{:?}", snapshot.integration.clone()),
        ];
        for debug in values { for private in secret.split_whitespace() { assert!(!debug.contains(private), "debug leaked {private}: {debug}"); } }
    }
}
