use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use parking_lot::{Mutex, MutexGuard};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::TaskResult;
pub(crate) type JobClock = Arc<dyn Fn() -> u64 + Send + Sync>;
const DEFAULT_MAX_SETTLED_JOBS: usize = 256;
const DEFAULT_SETTLED_JOB_TTL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct JobRetention {
    pub(crate) max_settled: usize,
    pub(crate) ttl: Duration,
}

impl JobRetention {
    pub(crate) fn validate(self) -> Result<Self> {
        if self.max_settled == 0 {
            return Err(anyhow!("orchestration retained job limit must be greater than zero"));
        }
        if self.ttl.is_zero() {
            return Err(anyhow!("orchestration retained job TTL must be greater than zero"));
        }
        Ok(self)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PruneCandidate {
    pub(crate) job_id: String,
    pub(crate) agent_id: String,
}


#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl JobStatus {
    #[must_use]
    pub fn is_settled(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobSnapshot {
    pub id: String,
    pub agent_id: String,
    pub agent: String,
    pub parent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todo_task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_generation: Option<u64>,
    pub status: JobStatus,
    pub created_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<TaskResult>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskSpawn {
    pub index: usize,
    pub job_id: String,
    pub agent_id: String,
    pub agent: String,
    pub status: JobStatus,
}

struct JobRecord {
    snapshot: JobSnapshot,
    cancel: CancellationToken,
}
pub(crate) struct PreparedJobRecords {
    records: HashMap<String, JobRecord>,
    snapshots: Vec<JobSnapshot>,
}

impl PreparedJobRecords {
    #[must_use]
    pub(crate) fn snapshots(&self) -> &[JobSnapshot] {
        &self.snapshots
    }
}

pub(crate) struct PreparedJobCancellation {
    cancelled: Vec<String>,
    tokens: Vec<CancellationToken>,
    snapshots: Vec<JobSnapshot>,
}

impl PreparedJobCancellation {
    pub(crate) fn take_snapshots(&mut self) -> Vec<JobSnapshot> {
        std::mem::take(&mut self.snapshots)
    }
}


pub(crate) struct JobManager {
    records: Mutex<HashMap<String, JobRecord>>,
    spawn_lock: Mutex<()>,
    changed: Notify,
    retention: JobRetention,
    clock: JobClock,
}

impl JobManager {
    pub(crate) fn new() -> Self {
        Self::with_retention(
            JobRetention {
                max_settled: DEFAULT_MAX_SETTLED_JOBS,
                ttl: DEFAULT_SETTLED_JOB_TTL,
            },
            Arc::new(super::runtime::now_millis),
        )
    }

    pub(crate) fn with_retention(retention: JobRetention, clock: JobClock) -> Self {
        Self {
            records: Mutex::new(HashMap::new()),
            spawn_lock: Mutex::new(()),
            changed: Notify::new(),
            retention,
            clock,
        }
    }

    pub(crate) fn lock_spawns(&self) -> MutexGuard<'_, ()> {
        self.spawn_lock.lock()
    }

    pub(crate) fn prepare_replacement(&self, snapshots: Vec<JobSnapshot>) -> PreparedJobRecords {
        let mut records = HashMap::with_capacity(snapshots.len());
        for snapshot in &snapshots {
            records.insert(
                snapshot.id.clone(),
                JobRecord {
                    snapshot: snapshot.clone(),
                    cancel: CancellationToken::new(),
                },
            );
        }
        PreparedJobRecords { records, snapshots }
    }

    pub(crate) fn install_replacement(&self, prepared: PreparedJobRecords) {
        let mut records = self.records.lock();
        for record in records.values() {
            record.cancel.cancel();
        }
        *records = prepared.records;
        drop(records);
        self.changed.notify_waiters();
    }


    pub(crate) fn contains_identifier(&self, id: &str) -> bool {
        self.records.lock().values().any(|record| {
            record.snapshot.id == id || record.snapshot.agent_id == id
        })
    }

    /// Resolve a job id or agent id accepted by job APIs to the canonical agent id.
    ///
    /// Job UUIDs and agent ids both match. Multiple distinct agent ids for one
    /// identifier are rejected as ambiguous; unknown identifiers error.
    pub(crate) fn resolve_agent_id(&self, identifier: &str) -> Result<String> {
        let records = self.records.lock();
        let mut agent_ids = records
            .values()
            .filter(|record| {
                record.snapshot.id == identifier || record.snapshot.agent_id == identifier
            })
            .map(|record| record.snapshot.agent_id.clone())
            .collect::<Vec<_>>();
        agent_ids.sort();
        agent_ids.dedup();
        match agent_ids.as_slice() {
            [agent_id] => Ok(agent_id.clone()),
            [] => Err(anyhow!(
                "unknown orchestration job or agent id {identifier:?}"
            )),
            matches => Err(anyhow!(
                "ambiguous orchestration job or agent id {identifier:?} matches agents {matches:?}; use a unique job id or agent id"
            )),
        }
    }


    pub(crate) fn insert(&self, snapshot: JobSnapshot, cancel: CancellationToken) -> Result<JobSnapshot> {
        let mut records = self.records.lock();
        if records.contains_key(&snapshot.id) {
            return Err(anyhow!("orchestration job id {:?} already exists", snapshot.id));
        }
        records.insert(snapshot.id.clone(), JobRecord { snapshot: snapshot.clone(), cancel });
        drop(records);
        self.changed.notify_waiters();
        Ok(snapshot)
    }

    pub(crate) fn mark_running(&self, id: &str, timestamp: u64) -> Option<JobSnapshot> {
        let mut records = self.records.lock();
        let record = records.get_mut(id)?;
        if record.snapshot.status != JobStatus::Queued {
            return None;
        }
        record.snapshot.status = JobStatus::Running;
        record.snapshot.started_at = Some(timestamp);
        let snapshot = record.snapshot.clone();
        drop(records);
        self.changed.notify_waiters();
        Some(snapshot)
    }

    pub(crate) fn finish(
        &self,
        id: &str,
        result: TaskResult,
        timestamp: u64,
    ) -> Option<JobSnapshot> {
        let mut records = self.records.lock();
        let record = records.get_mut(id)?;
        record.snapshot.status = if result.status == super::AgentStatus::Aborted {
            JobStatus::Cancelled
        } else if result.error.is_some() {
            JobStatus::Failed
        } else {
            JobStatus::Completed
        };
        record.snapshot.finished_at = Some(timestamp);
        record.snapshot.result = Some(result);
        let snapshot = record.snapshot.clone();
        drop(records);
        self.changed.notify_waiters();
        Some(snapshot)
    }
    pub(crate) fn append_result_error(&self, id: &str, error: &str) -> Option<JobSnapshot> {
        let mut records = self.records.lock();
        let record = records.get_mut(id)?;
        record.snapshot.status = JobStatus::Failed;
        let result = record.snapshot.result.as_mut()?;
        result.error = Some(match result.error.take() {
            Some(existing) => format!("{existing}; {error}"),
            None => error.to_owned(),
        });
        let snapshot = record.snapshot.clone();
        drop(records);
        self.changed.notify_waiters();
        Some(snapshot)
    }

    pub(crate) fn prune_candidates(&self) -> Vec<PruneCandidate> {
        let now = (self.clock)();
        let ttl_ms = self.retention.ttl.as_millis().try_into().unwrap_or(u64::MAX);
        let records = self.records.lock();
        let mut settled = records
            .iter()
            .filter_map(|(id, record)| {
                record.snapshot.status.is_settled().then_some((
                    id.clone(),
                    record.snapshot.agent_id.clone(),
                    record.snapshot.finished_at.unwrap_or(record.snapshot.created_at),
                ))
            })
            .collect::<Vec<_>>();
        settled.sort_by(|(left_id, _, left_finished), (right_id, _, right_finished)| {
            left_finished
                .cmp(right_finished)
                .then_with(|| left_id.cmp(right_id))
        });
        let over_limit = settled.len().saturating_sub(self.retention.max_settled);
        settled
            .into_iter()
            .enumerate()
            .filter_map(|(index, (job_id, agent_id, finished_at))| {
                let expired = now.saturating_sub(finished_at) >= ttl_ms;
                (index < over_limit || expired).then_some(PruneCandidate { job_id, agent_id })
            })
            .collect()
    }

    pub(crate) fn settled_candidates(&self) -> Vec<PruneCandidate> {
        self.records
            .lock()
            .iter()
            .filter_map(|(job_id, record)| {
                record.snapshot.status.is_settled().then(|| PruneCandidate {
                    job_id: job_id.clone(),
                    agent_id: record.snapshot.agent_id.clone(),
                })
            })
            .collect()
    }

    pub(crate) fn remove_settled(&self, id: &str) -> bool {
        let mut records = self.records.lock();
        if records
            .get(id)
            .is_none_or(|record| !record.snapshot.status.is_settled())
        {
            return false;
        }
        records.remove(id);
        drop(records);
        self.changed.notify_waiters();
        true
    }
    pub(crate) fn remove(&self, id: &str) -> bool {
        let removed = self.records.lock().remove(id).is_some();
        if removed {
            self.changed.notify_waiters();
        }
        removed
    }

    pub(crate) fn snapshots(&self, ids: Option<&[String]>) -> Vec<JobSnapshot> {
        let records = self.records.lock();
        let mut snapshots = match ids {
            Some(ids) => {
                let requested = ids.iter().map(String::as_str).collect::<HashSet<_>>();
                records
                    .values()
                    .filter(|record| {
                        requested.contains(record.snapshot.id.as_str())
                            || requested.contains(record.snapshot.agent_id.as_str())
                    })
                    .map(|record| record.snapshot.clone())
                    .collect::<Vec<_>>()
            }
            None => records
                .values()
                .map(|record| record.snapshot.clone())
                .collect::<Vec<_>>(),
        };
        snapshots.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        snapshots
    }

    pub(crate) fn prepare_cancellation(&self, ids: &[String]) -> PreparedJobCancellation {
        let records = self.records.lock();
        let mut cancelled = Vec::new();
        let mut tokens = Vec::new();
        let mut seen = HashSet::new();
        for id in ids {
            for record in records.values().filter(|record| {
                (record.snapshot.id == *id || record.snapshot.agent_id == *id)
                    && !record.snapshot.status.is_settled()
            }) {
                if seen.insert(record.snapshot.id.clone()) {
                    cancelled.push(record.snapshot.id.clone());
                    tokens.push(record.cancel.clone());
                }
            }
        }
        cancelled.sort();
        let mut snapshots = records
            .values()
            .map(|record| {
                let mut snapshot = record.snapshot.clone();
                if seen.contains(&snapshot.id) {
                    snapshot.status = JobStatus::Cancelled;
                    snapshot.finished_at = Some((self.clock)());
                }
                snapshot
            })
            .collect::<Vec<_>>();
        snapshots.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        PreparedJobCancellation {
            cancelled,
            tokens,
            snapshots,
        }
    }

    pub(crate) fn commit_cancellation(
        &self,
        prepared: PreparedJobCancellation,
    ) -> Vec<String> {
        let PreparedJobCancellation {
            cancelled,
            tokens,
            snapshots,
        } = prepared;
        debug_assert!(snapshots.is_empty());
        let cancelled_ids = cancelled.iter().map(String::as_str).collect::<HashSet<_>>();
        let timestamp = (self.clock)();
        let mut records = self.records.lock();
        for record in records.values_mut() {
            if cancelled_ids.contains(record.snapshot.id.as_str())
                && !record.snapshot.status.is_settled()
            {
                record.snapshot.status = JobStatus::Cancelled;
                record.snapshot.finished_at = Some(timestamp);
            }
        }
        for token in tokens {
            token.cancel();
        }
        drop(records);
        self.changed.notify_waiters();
        cancelled
    }

    pub(crate) fn cancel(&self, ids: &[String]) -> Vec<String> {
        let mut cancelled = Vec::new();
        let mut seen = HashSet::new();
        let records = self.records.lock();
        for id in ids {
            for record in records.values().filter(|record| {
                (record.snapshot.id == *id || record.snapshot.agent_id == *id)
                    && !record.snapshot.status.is_settled()
            }) {
                if seen.insert(record.snapshot.id.clone()) {
                    record.cancel.cancel();
                    cancelled.push(record.snapshot.id.clone());
                }
            }
        }
        cancelled.sort();
        cancelled
    }

    pub(crate) async fn wait(
        &self,
        ids: &[String],
        timeout: Option<Duration>,
        abort: Option<pi_agent::AbortSignal>,
        shutdown: CancellationToken,
    ) -> Result<Vec<JobSnapshot>> {
        self.validate_ids(ids)?;
        let wait = async {
            loop {
                let changed = self.changed.notified();
                let snapshots = self.snapshots(Some(ids));
                if snapshots.iter().any(|snapshot| snapshot.status.is_settled()) {
                    return snapshots;
                }
                changed.await;
            }
        };
        match (timeout, abort) {
            (Some(timeout), Some(abort)) => tokio::select! {
                snapshots = tokio::time::timeout(timeout, wait) => Ok(snapshots.unwrap_or_else(|_| self.snapshots(Some(ids)))),
                () = abort.cancelled() => Err(anyhow!("job wait aborted")),
                () = shutdown.cancelled() => Err(anyhow!("orchestration shut down")),
            },
            (Some(timeout), None) => tokio::select! {
                snapshots = tokio::time::timeout(timeout, wait) => Ok(snapshots.unwrap_or_else(|_| self.snapshots(Some(ids)))),
                () = shutdown.cancelled() => Err(anyhow!("orchestration shut down")),
            },
            (None, Some(abort)) => tokio::select! {
                snapshots = wait => Ok(snapshots),
                () = abort.cancelled() => Err(anyhow!("job wait aborted")),
                () = shutdown.cancelled() => Err(anyhow!("orchestration shut down")),
            },
            (None, None) => tokio::select! {
                snapshots = wait => Ok(snapshots),
                () = shutdown.cancelled() => Err(anyhow!("orchestration shut down")),
            },
        }
    }

    pub(crate) async fn wait_all_results(
        &self,
        ids: &[String],
        abort: pi_agent::AbortSignal,
        shutdown: CancellationToken,
    ) -> Result<Vec<TaskResult>> {
        self.validate_ids(ids)?;
        let mut cancellation_started = false;
        loop {
            let changed = self.changed.notified();
            let snapshots = self.snapshots(Some(ids));
            if snapshots.len() == ids.len()
                && snapshots.iter().all(|snapshot| snapshot.result.is_some())
            {
                let mut results = snapshots
                    .into_iter()
                    .filter_map(|snapshot| snapshot.result)
                    .collect::<Vec<_>>();
                results.sort_by_key(|result| result.index);
                return Ok(results);
            }
            if cancellation_started {
                changed.await;
                continue;
            }
            tokio::select! {
                () = changed => {}
                () = abort.cancelled() => {
                    self.cancel(ids);
                    cancellation_started = true;
                }
                () = shutdown.cancelled() => {
                    self.cancel(ids);
                    cancellation_started = true;
                }
            }
        }
    }

    fn validate_ids(&self, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Err(anyhow!("ids must not be empty"));
        }
        let records = self.records.lock();
        if let Some(id) = ids.iter().find(|id| {
            !records.values().any(|record| {
                record.snapshot.id == **id || record.snapshot.agent_id == **id
            })
        }) {
            return Err(anyhow!("unknown orchestration job or agent id {id:?}"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(id: &str, agent_id: &str) -> JobSnapshot {
        JobSnapshot {
            id: id.to_owned(),
            agent_id: agent_id.to_owned(),
            agent: "task".to_owned(),
            parent_id: "Main".to_owned(),
            description: None,
            todo_task_id: None,
            workflow_id: None,
            workflow_generation: None,
            status: JobStatus::Queued,
            created_at: 1,
            started_at: None,
            finished_at: None,
            result: None,
        }
    }

    #[test]
    fn resolve_agent_id_maps_job_and_agent_identifiers() {
        let jobs = JobManager::new();
        jobs.insert(snapshot("job-a", "Worker"), CancellationToken::new())
            .expect("insert");

        assert_eq!(jobs.resolve_agent_id("job-a").expect("job uuid"), "Worker");
        assert_eq!(
            jobs.resolve_agent_id("Worker").expect("agent id"),
            "Worker"
        );
    }

    #[test]
    fn resolve_agent_id_unknown_is_actionable() {
        let jobs = JobManager::new();
        let error = jobs
            .resolve_agent_id("missing-job")
            .expect_err("unknown id");
        let message = error.to_string();
        assert!(message.contains("unknown"), "{message}");
        assert!(message.contains("missing-job"), "{message}");
    }

    #[test]
    fn resolve_agent_id_rejects_ambiguous_agent_matches() {
        let jobs = JobManager::new();
        // Two jobs that both claim the same synthetic identifier via agent_id
        // collision with another job's id is prevented at spawn, but agent_id
        // equality across records is still possible if identifiers overlap.
        jobs.insert(snapshot("shared", "Alpha"), CancellationToken::new())
            .expect("insert alpha");
        jobs.insert(snapshot("other", "shared"), CancellationToken::new())
            .expect("insert shared agent");

        let error = jobs
            .resolve_agent_id("shared")
            .expect_err("shared matches job id and agent id of different agents");
        let message = error.to_string();
        assert!(message.contains("ambiguous"), "{message}");
        assert!(message.contains("Alpha"), "{message}");
        assert!(message.contains("shared"), "{message}");
    }
}
