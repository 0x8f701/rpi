use std::collections::HashMap;

use crate::{WorkflowEvent, WorkflowId, WorkflowSnapshot};

pub(super) struct WorkflowForwardingState {
    snapshots: HashMap<WorkflowId, WorkflowSnapshot>,
}

impl WorkflowForwardingState {
    pub(super) fn new(current: Vec<WorkflowSnapshot>) -> Self {
        Self {
            snapshots: current
                .into_iter()
                .map(|snapshot| (snapshot.workflow_id.clone(), snapshot))
                .collect(),
        }
    }

    pub(super) fn reconcile(&mut self, current: Vec<WorkflowSnapshot>) -> Vec<WorkflowEvent> {
        let current = current
            .into_iter()
            .map(|snapshot| (snapshot.workflow_id.clone(), snapshot))
            .collect::<HashMap<_, _>>();
        let mut events = self
            .snapshots
            .iter()
            .filter(|(workflow_id, _)| !current.contains_key(*workflow_id))
            .map(|(workflow_id, snapshot)| WorkflowEvent::Removed {
                workflow_id: workflow_id.clone(),
                generation: snapshot.generation,
            })
            .collect::<Vec<_>>();
        events.extend(
            current
                .values()
                .cloned()
                .map(|snapshot| WorkflowEvent::Updated { snapshot }),
        );
        self.snapshots = current;
        events
    }

    pub(super) fn accept(&mut self, event: &WorkflowEvent, authoritative: Option<WorkflowSnapshot>) -> bool {
        match event {
            WorkflowEvent::Created { snapshot } | WorkflowEvent::Updated { snapshot } => {
                if authoritative.as_ref() != Some(snapshot)
                    || self
                        .snapshots
                        .get(&snapshot.workflow_id)
                        .is_some_and(|current| current.generation > snapshot.generation)
                {
                    return false;
                }
                self.snapshots
                    .insert(snapshot.workflow_id.clone(), snapshot.clone());
                true
            }
            WorkflowEvent::StatusChanged {
                workflow_id,
                generation,
                status,
            } => {
                let Some(snapshot) = authoritative.filter(|snapshot| {
                    snapshot.generation == *generation && snapshot.status == *status
                }) else {
                    return false;
                };
                if self
                    .snapshots
                    .get(workflow_id)
                    .is_some_and(|current| current.generation > *generation)
                {
                    return false;
                }
                self.snapshots.insert(workflow_id.clone(), snapshot);
                true
            }
            WorkflowEvent::Removed {
                workflow_id,
                generation,
            } => {
                if authoritative.is_some()
                    || !self
                        .snapshots
                        .get(workflow_id)
                        .is_some_and(|snapshot| snapshot.generation <= *generation)
                {
                    return false;
                }
                self.snapshots.remove(workflow_id);
                true
            }
        }
    }
}

pub(super) fn event_workflow_id(event: &WorkflowEvent) -> &WorkflowId {
    match event {
        WorkflowEvent::Created { snapshot } | WorkflowEvent::Updated { snapshot } => &snapshot.workflow_id,
        WorkflowEvent::StatusChanged { workflow_id, .. } | WorkflowEvent::Removed { workflow_id, .. } => workflow_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TodoState, TodoStorage, WorkflowIntegration, WorkflowStatus};

    fn snapshot(id: &str, generation: u64, status: WorkflowStatus) -> WorkflowSnapshot {
        WorkflowSnapshot {
            workflow_id: WorkflowId::new(id), name: id.to_owned(), objective: "test forwarding".to_owned(), status,
            created_at_ms: 1, updated_at_ms: 2, generation,
            todo: TodoState { phases: Vec::new(), storage: TodoStorage::Memory },
            worktree_label: Some(id.to_owned()), branch: Some(format!("rpi/workflow/{id}")), supervisor_agent_id: None,
            supervisor_job_id: None, failure: None, integration: WorkflowIntegration::None,
        }
    }

    #[test]
    fn reconciliation_emits_removals_and_updates() {
        let removed = snapshot("removed", 3, WorkflowStatus::Cancelled);
        let current = snapshot("current", 5, WorkflowStatus::Running);
        let mut state = WorkflowForwardingState::new(vec![removed.clone(), snapshot("current", 5, WorkflowStatus::Paused)]);
        let events = state.reconcile(vec![current.clone()]);
        assert!(events.contains(&WorkflowEvent::Removed { workflow_id: removed.workflow_id, generation: 3 }));
        assert!(events.contains(&WorkflowEvent::Updated { snapshot: current }));
    }

    #[test]
    fn reconciliation_keeps_newer_generation_authoritative() {
        let current = snapshot("current", 5, WorkflowStatus::Running);
        let mut state = WorkflowForwardingState::new(vec![snapshot("current", 4, WorkflowStatus::Paused)]);
        let events = state.reconcile(vec![current.clone()]);
        assert_eq!(events, vec![WorkflowEvent::Updated { snapshot: current }]);
    }

    #[test]
    fn retained_events_after_reconciliation_are_rejected() {
        let current = snapshot("current", 5, WorkflowStatus::Running);
        let mut state = WorkflowForwardingState::new(Vec::new());
        state.reconcile(vec![current.clone()]);
        let stale_events = [
            WorkflowEvent::Updated { snapshot: snapshot("current", 4, WorkflowStatus::Paused) },
            WorkflowEvent::StatusChanged { workflow_id: current.workflow_id.clone(), generation: 4, status: WorkflowStatus::Failed },
            WorkflowEvent::Removed { workflow_id: current.workflow_id.clone(), generation: 4 },
        ];
        for event in stale_events {
            assert!(!state.accept(&event, Some(current.clone())));
        }
    }
}
