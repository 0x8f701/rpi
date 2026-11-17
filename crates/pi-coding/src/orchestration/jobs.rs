use std::collections::{HashMap, HashSet};
use std::fmt;
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

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct JobRetention {
    pub(crate) max_settled: usize,
    pub(crate) ttl: Duration,
}

impl JobRetention {
    pub(crate) fn validate(self) -> Result<Self> {
        if self.max_settled == 0 {
            return Err(anyhow!(
                "orchestration retained job limit must be greater than zero"
            ));
        }
        if self.ttl.is_zero() {
            return Err(anyhow!(
                "orchestration retained job TTL must be greater than zero"
            ));
        }
        Ok(self)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PruneCandidate {
    pub(crate) job_id: String,
    pub(crate) agent_id: String,
}
/// A requested wait id (job id or agent id) with no surviving snapshot: the
/// job was pruned by retention or removed while the wait was parked. Waits
/// fail closed with this error instead of looping on an id that can never
/// settle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct JobWaitMissingError {
    /// Requested ids with no surviving snapshot, in request order.
    pub(crate) missing: Vec<String>,
}

impl fmt::Display for JobWaitMissingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "orchestration job pruned or removed while waited: {:?}",
            self.missing
        )
    }
}

impl std::error::Error for JobWaitMissingError {}

/// Execution phase of one orchestration job.
///
/// `Cancelling` is nonterminal: cancellation intent has committed, but the
/// runner still owns abort, drain, and cleanup. It is not a second lifecycle
/// flag beside status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Queued,
    Running,
    /// Cancellation intent is durable; the runner has not settled yet.
    Cancelling,
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
/// Why a cancellation was requested. Metadata only — not another state machine.
///
/// Wire values are `"task"` and `"shutdown"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CancellationCause {
    /// The parent task, abort signal, or an explicit job cancel requested stop.
    Task,
    /// Orchestration shutdown requested stop.
    Shutdown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
    /// Present from the committed cancellation request through terminal
    /// `Cancelled`. Absent for every other status, including legacy records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancellation_cause: Option<CancellationCause>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<TaskResult>,
    /// True when the job settled early because a configured soft budget was
    /// reached (max requests, max tokens, or a yield-after threshold). The job
    /// is not failed: it completed with a partial result and the parent
    /// decides whether to continue it. Serialization omits the field when
    /// false so persisted legacy snapshots and wire payloads stay compatible.
    #[serde(default, skip_serializing_if = "is_false")]
    pub soft_budget_exhausted: bool,
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

/// Cancellation candidates prepared under the caller's lifecycle lock.
///
/// Preparation does not mutate job records and does not signal tokens. The
/// caller persists `take_snapshots()` and only then calls
/// [`JobManager::commit_cancellation`]. The runtime captures matching tokens
/// itself and signals them after that commit succeeds.
pub(crate) struct PreparedJobCancellation {
    cause: CancellationCause,
    cancelling: Vec<String>,
    immediate: Vec<String>,
    snapshots: Vec<JobSnapshot>,
}

impl PreparedJobCancellation {
    /// Full job table with the prepared transition applied. Persist this set
    /// before committing the in-memory transition.
    pub(crate) fn take_snapshots(&mut self) -> Vec<JobSnapshot> {
        std::mem::take(&mut self.snapshots)
    }
    /// Cause written for jobs this plan still transitions.
    #[must_use]
    pub(crate) fn cause(&self) -> CancellationCause {
        self.cause
    }


    /// Jobs with a live owner. They become nonterminal `Cancelling`.
    #[must_use]
    pub(crate) fn cancelling_ids(&self) -> &[String] {
        &self.cancelling
    }

    /// Jobs the caller proved ownerless. They settle `Cancelled` immediately.
    #[must_use]
    pub(crate) fn immediate_ids(&self) -> &[String] {
        &self.immediate
    }
}

/// Identities committed by [`JobManager::commit_cancellation`].
///
/// Tokens are intentionally absent: signalling is the runtime's step after
/// persistence and this commit both succeed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CancellationCommit {
    pub(crate) cancelling_ids: Vec<String>,
    pub(crate) immediate_ids: Vec<String>,
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
        self.records
            .lock()
            .values()
            .any(|record| record.snapshot.id == id || record.snapshot.agent_id == id)
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

    pub(crate) fn insert(
        &self,
        snapshot: JobSnapshot,
        cancel: CancellationToken,
    ) -> Result<JobSnapshot> {
        let mut records = self.records.lock();
        if records.contains_key(&snapshot.id) {
            return Err(anyhow!(
                "orchestration job id {:?} already exists",
                snapshot.id
            ));
        }
        records.insert(
            snapshot.id.clone(),
            JobRecord {
                snapshot: snapshot.clone(),
                cancel,
            },
        );
        drop(records);
        self.changed.notify_waiters();
        Ok(snapshot)
    }

    pub(crate) fn cancellation_token(&self, id: &str) -> Option<CancellationToken> {
        self.records
            .lock()
            .get(id)
            .map(|record| record.cancel.clone())
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
        if record.snapshot.status.is_settled() {
            return None;
        }
        let cancelling = record.snapshot.status == JobStatus::Cancelling;
        let mut result = result;
        record.snapshot.status = if cancelling || result.status == super::AgentStatus::Aborted {
            if cancelling {
                // The committed cause stays. A simultaneous completion select
                // must not win over a cancellation that already committed.
                result.status = super::AgentStatus::Aborted;
            }
            JobStatus::Cancelled
        } else if result.error.is_some() {
            JobStatus::Failed
        } else {
            JobStatus::Completed
        };
        record.snapshot.finished_at = Some(timestamp);
        record.snapshot.soft_budget_exhausted = result.soft_budget_exhausted;
        record.snapshot.result = Some(result);
        let snapshot = record.snapshot.clone();
        drop(records);
        self.changed.notify_waiters();
        Some(snapshot)
    }
    pub(crate) fn append_result_error(&self, id: &str, error: &str) -> Option<JobSnapshot> {
        let mut records = self.records.lock();
        let record = records.get_mut(id)?;
        // A committed cancellation stays Cancelled. The persistence diagnostic
        // is attached to the actual result; rewriting the status to Failed
        // would leave a cause on a non-cancellation status.
        if record.snapshot.status != JobStatus::Cancelled {
            record.snapshot.status = JobStatus::Failed;
        }
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
        let ttl_ms = self
            .retention
            .ttl
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        let records = self.records.lock();
        let mut settled = records
            .iter()
            .filter_map(|(id, record)| {
                record.snapshot.status.is_settled().then_some((
                    id.clone(),
                    record.snapshot.agent_id.clone(),
                    record
                        .snapshot
                        .finished_at
                        .unwrap_or(record.snapshot.created_at),
                ))
            })
            .collect::<Vec<_>>();
        settled.sort_by(
            |(left_id, _, left_finished), (right_id, _, right_finished)| {
                left_finished
                    .cmp(right_finished)
                    .then_with(|| left_id.cmp(right_id))
            },
        );
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

    /// Plan cancellation without mutating records or signalling tokens.
    ///
    /// `ownerless` is an explicit set of **job ids** the runtime has proven
    /// have no runner, launch reservation, or concurrent registration. Agent
    /// ids in that set are ignored. Status and timing are not evidence of
    /// ownership. An ownerless match settles `Cancelled` immediately with the
    /// intentional-stop explanation. Every other unsettled match becomes
    /// nonterminal `Cancelling` with no result and no `finished_at`.
    ///
    /// Already-`Cancelling` and settled jobs are left untouched, including
    /// their first committed cause.
    pub(crate) fn prepare_cancellation(
        &self,
        ids: &[String],
        cause: CancellationCause,
        ownerless: &HashSet<String>,
    ) -> PreparedJobCancellation {
        let records = self.records.lock();
        let mut cancelling = Vec::new();
        let mut immediate = Vec::new();
        let mut seen = HashSet::new();
        for id in ids {
            for record in records.values().filter(|record| {
                (record.snapshot.id == *id || record.snapshot.agent_id == *id)
                    && !record.snapshot.status.is_settled()
                    && record.snapshot.status != JobStatus::Cancelling
            }) {
                if seen.insert(record.snapshot.id.clone()) {
                    if ownerless.contains(&record.snapshot.id) {
                        immediate.push(record.snapshot.id.clone());
                    } else {
                        cancelling.push(record.snapshot.id.clone());
                    }
                }
            }
        }
        cancelling.sort();
        immediate.sort();
        let timestamp = (self.clock)();
        let mut snapshots = records
            .values()
            .map(|record| {
                let mut snapshot = record.snapshot.clone();
                if cancelling.binary_search(&snapshot.id).is_ok() {
                    snapshot.status = JobStatus::Cancelling;
                    snapshot.cancellation_cause = Some(cause);
                    snapshot.finished_at = None;
                    snapshot.result = None;
                } else if immediate.binary_search(&snapshot.id).is_ok() {
                    snapshot.status = JobStatus::Cancelled;
                    snapshot.cancellation_cause = Some(cause);
                    snapshot.finished_at = Some(timestamp);
                    snapshot.soft_budget_exhausted = false;
                    snapshot.result = Some(intentional_stop_result(&snapshot, cause));
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
            cause,
            cancelling,
            immediate,
            snapshots,
        }
    }

    /// Install a previously prepared cancellation. Does not signal tokens.
    ///
    /// Jobs that settled or entered `Cancelling` after preparation are left
    /// unchanged, so a repeated request cannot reset timestamps or replace the
    /// first committed cause or a real final result.
    pub(crate) fn commit_cancellation(
        &self,
        prepared: PreparedJobCancellation,
    ) -> CancellationCommit {
        let PreparedJobCancellation {
            cause,
            cancelling,
            immediate,
            snapshots,
        } = prepared;
        debug_assert!(snapshots.is_empty(), "persist snapshots before commit");
        let cancelling_ids = cancelling.iter().map(String::as_str).collect::<HashSet<_>>();
        let immediate_ids = immediate.iter().map(String::as_str).collect::<HashSet<_>>();
        let timestamp = (self.clock)();
        let mut committed_cancelling = Vec::new();
        let mut committed_immediate = Vec::new();
        let mut records = self.records.lock();
        for record in records.values_mut() {
            let id = record.snapshot.id.as_str();
            let still_open = !record.snapshot.status.is_settled()
                && record.snapshot.status != JobStatus::Cancelling;
            if cancelling_ids.contains(id) && still_open {
                record.snapshot.status = JobStatus::Cancelling;
                record.snapshot.cancellation_cause = Some(cause);
                record.snapshot.finished_at = None;
                record.snapshot.result = None;
                committed_cancelling.push(record.snapshot.id.clone());
            } else if immediate_ids.contains(id) && still_open {
                record.snapshot.status = JobStatus::Cancelled;
                record.snapshot.cancellation_cause = Some(cause);
                record.snapshot.finished_at = Some(timestamp);
                record.snapshot.soft_budget_exhausted = false;
                record.snapshot.result = Some(intentional_stop_result(&record.snapshot, cause));
                committed_immediate.push(record.snapshot.id.clone());
            }
        }
        drop(records);
        self.changed.notify_waiters();
        committed_cancelling.sort();
        committed_immediate.sort();
        CancellationCommit {
            cancelling_ids: committed_cancelling,
            immediate_ids: committed_immediate,
        }
    }

    /// A wait resolves only when EVERY requested job is terminal. A parent
    /// waiting on several children must never observe a partially settled
    /// batch — otherwise the parent turn could conclude (the outer Task
    /// projecting Done) while a delegated child job is still Running/Queued.
    /// Single-id waits keep their exact prior behavior.
    ///
    /// Fail-closed: a requested id (job id or agent id) that loses its record
    /// to a prune or removal before settling resolves the wait with
    /// [`JobWaitMissingError`] — a missing id can never settle, so waiting on
    /// it would hang. A timeout keeps the prior contract and returns the
    /// current snapshots.
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
                if let Some(missing) = missing_ids(ids, &snapshots) {
                    return Err(missing);
                }
                if ids.iter().all(|id| {
                    snapshots.iter().any(|snapshot| {
                        (snapshot.id == *id || snapshot.agent_id == *id)
                            && snapshot.status.is_settled()
                    })
                }) {
                    return Ok(snapshots);
                }
                changed.await;
            }
        };
        match (timeout, abort) {
            (Some(timeout), Some(abort)) => tokio::select! {
                resolved = tokio::time::timeout(timeout, wait) => match resolved {
                    Ok(result) => result.map_err(anyhow::Error::from),
                    Err(_elapsed) => Ok(self.snapshots(Some(ids))),
                },
                () = abort.cancelled() => Err(anyhow!("job wait aborted")),
                () = shutdown.cancelled() => Err(anyhow!("orchestration shut down")),
            },
            (Some(timeout), None) => tokio::select! {
                resolved = tokio::time::timeout(timeout, wait) => match resolved {
                    Ok(result) => result.map_err(anyhow::Error::from),
                    Err(_elapsed) => Ok(self.snapshots(Some(ids))),
                },
                () = shutdown.cancelled() => Err(anyhow!("orchestration shut down")),
            },
            (None, Some(abort)) => tokio::select! {
                resolved = wait => resolved.map_err(anyhow::Error::from),
                () = abort.cancelled() => Err(anyhow!("job wait aborted")),
                () = shutdown.cancelled() => Err(anyhow!("orchestration shut down")),
            },
            (None, None) => tokio::select! {
                resolved = wait => resolved.map_err(anyhow::Error::from),
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
        loop {
            let changed = self.changed.notified();
            let snapshots = self.snapshots(Some(ids));
            if let Some(missing) = missing_ids(ids, &snapshots) {
                return Err(anyhow::Error::from(missing));
            }
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
            tokio::select! {
                () = changed => {}
                () = abort.cancelled() => {
                    return Err(anyhow!(
                        "job result wait aborted before durable cancellation settled"
                    ));
                }
                () = shutdown.cancelled() => {
                    return Err(anyhow!(
                        "orchestration shut down before durable cancellation settled"
                    ));
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
            !records
                .values()
                .any(|record| record.snapshot.id == **id || record.snapshot.agent_id == **id)
        }) {
            return Err(anyhow!("unknown orchestration job or agent id {id:?}"));
        }
        Ok(())
    }
}

/// Requested ids matched by NO surviving snapshot — the same job-id/agent-id
/// rule as `validate_ids`. A missing id can never settle, so a parked wait
/// must fail closed with this error instead of looping forever.
fn missing_ids(ids: &[String], snapshots: &[JobSnapshot]) -> Option<JobWaitMissingError> {
    let missing = ids
        .iter()
        .filter(|id| {
            !snapshots
                .iter()
                .any(|snapshot| snapshot.id == **id || snapshot.agent_id == **id)
        })
        .cloned()
        .collect::<Vec<_>>();
    (!missing.is_empty()).then_some(JobWaitMissingError { missing })
}

fn intentional_stop_result(snapshot: &JobSnapshot, _cause: CancellationCause) -> TaskResult {
    // Intentional typed stop: `error: None` — the durable cause lives on the
    // snapshot's `cancellation_cause`, set by the caller. A real failure stays
    // `Some(error)`; nothing is manufactured here.
    TaskResult {
        index: 0,
        id: snapshot.agent_id.clone(),
        agent: snapshot.agent.clone(),
        status: super::AgentStatus::Aborted,
        output: String::new(),
        usage: pi_ai::Usage::default(),
        soft_budget_exhausted: false,
        error: None,
        structured_output: None,
        artifact_ref: format!("agent://{}", snapshot.agent_id),
        history_ref: format!("history://{}", snapshot.agent_id),
        artifact_uri: format!("artifact://{}", snapshot.agent_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;

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
            cancellation_cause: None,
            result: None,
            soft_budget_exhausted: false,
        }
    }

    #[test]
    fn resolve_agent_id_maps_job_and_agent_identifiers() {
        let jobs = JobManager::new();
        jobs.insert(snapshot("job-a", "Worker"), CancellationToken::new())
            .expect("insert");

        assert_eq!(jobs.resolve_agent_id("job-a").expect("job uuid"), "Worker");
        assert_eq!(jobs.resolve_agent_id("Worker").expect("agent id"), "Worker");
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

    fn task_result(soft_budget_exhausted: bool) -> TaskResult {
        TaskResult {
            index: 0,
            id: "Worker".to_owned(),
            agent: "task".to_owned(),
            status: super::super::AgentStatus::Idle,
            output: "partial".to_owned(),
            usage: pi_ai::Usage::default(),
            soft_budget_exhausted,
            error: None,
            artifact_ref: "agent://Worker".to_owned(),
            history_ref: "history://Worker".to_owned(),
            artifact_uri: "artifact://Worker".to_owned(),
            structured_output: None,
        }
    }

    #[test]
    fn finish_propagates_soft_budget_marker_from_result() {
        let jobs = JobManager::new();
        jobs.insert(snapshot("job-budget", "Worker"), CancellationToken::new())
            .expect("insert");

        let finished = jobs
            .finish("job-budget", task_result(true), 2)
            .expect("finish with soft budget");
        assert!(
            finished.soft_budget_exhausted,
            "snapshot must carry the marker"
        );
        assert_eq!(
            finished.status,
            JobStatus::Completed,
            "soft budget must not fail the job"
        );
        assert_eq!(finished.finished_at, Some(2));
        assert!(
            finished
                .result
                .is_some_and(|result| result.soft_budget_exhausted)
        );

        assert!(
            jobs.finish("job-budget", task_result(false), 3).is_none(),
            "a settled job must not be overwritten by a later result",
        );
        jobs.insert(snapshot("job-plain", "Worker"), CancellationToken::new())
            .expect("insert plain job");
        let plain = jobs
            .finish("job-plain", task_result(false), 3)
            .expect("finish without soft budget");
        assert!(!plain.soft_budget_exhausted);
        assert_eq!(plain.status, JobStatus::Completed);
    }

    #[test]
    fn finish_maps_aborted_agent_status_to_cancelled_without_inventing_cause() {
        let jobs = JobManager::new();
        jobs.insert(snapshot("job-abort", "Worker"), CancellationToken::new())
            .expect("insert");
        let mut result = task_result(false);
        result.status = super::super::AgentStatus::Aborted;
        result.error = None;
        let finished = jobs
            .finish("job-abort", result, 4)
            .expect("aborted status settles");
        assert_eq!(finished.status, JobStatus::Cancelled);
        assert_eq!(finished.cancellation_cause, None);
        let settled = finished.result.expect("result");
        assert_eq!(settled.status, super::super::AgentStatus::Aborted);
        assert!(settled.error.is_none());
    }

    #[test]
    fn finish_keeps_real_error_on_aborted_status() {
        let jobs = JobManager::new();
        jobs.insert(snapshot("job-abort-error", "Worker"), CancellationToken::new())
            .expect("insert");
        let mut result = task_result(false);
        result.status = super::super::AgentStatus::Aborted;
        result.error = Some("provider aborted the socket".to_owned());
        let finished = jobs
            .finish("job-abort-error", result, 5)
            .expect("aborted drain settles");
        assert_eq!(finished.status, JobStatus::Cancelled);
        assert_eq!(finished.cancellation_cause, None);
        let settled = finished.result.expect("result");
        assert_eq!(settled.error.as_deref(), Some("provider aborted the socket"));
        assert_eq!(settled.status, super::super::AgentStatus::Aborted);
    }

    #[test]
    fn finish_idle_status_stays_failed_or_completed_by_error() {
        let jobs = JobManager::new();
        jobs.insert(snapshot("job-error", "Worker"), CancellationToken::new())
            .expect("insert error");
        let mut failed = task_result(false);
        failed.status = super::super::AgentStatus::Idle;
        failed.error = Some("provider aborted the socket".to_owned());
        let failed = jobs.finish("job-error", failed, 6).expect("error settles");
        assert_eq!(failed.status, JobStatus::Failed);
        assert_eq!(failed.cancellation_cause, None);
        assert_eq!(
            failed.result.as_ref().and_then(|result| result.error.as_deref()),
            Some("provider aborted the socket")
        );

        jobs.insert(snapshot("job-stop", "Worker"), CancellationToken::new())
            .expect("insert stop");
        let stopped = jobs
            .finish("job-stop", task_result(false), 8)
            .expect("stop settles");
        assert_eq!(stopped.status, JobStatus::Completed);
        assert_eq!(stopped.cancellation_cause, None);
        assert_eq!(
            stopped.result.expect("result").status,
            super::super::AgentStatus::Idle
        );
    }

    #[test]
    fn finish_committed_cancelling_stays_cancelled_and_keeps_cause() {
        let jobs = JobManager::new();
        let mut running = snapshot("job-cancel", "Worker");
        running.status = JobStatus::Cancelling;
        running.cancellation_cause = Some(CancellationCause::Task);
        jobs.insert(running, CancellationToken::new()).expect("insert");
        let mut result = task_result(false);
        result.status = super::super::AgentStatus::Idle;
        result.error = Some("late provider error".to_owned());
        let finished = jobs
            .finish("job-cancel", result, 9)
            .expect("committed cancel settles");
        assert_eq!(finished.status, JobStatus::Cancelled);
        assert_eq!(finished.cancellation_cause, Some(CancellationCause::Task));
        let settled = finished.result.expect("result");
        assert_eq!(settled.status, super::super::AgentStatus::Aborted);
        assert_eq!(settled.error.as_deref(), Some("late provider error"));
    }



    #[test]
    fn soft_budget_marker_serializes_only_when_set() {
        let plain = snapshot("job-plain", "Worker");
        let wire = serde_json::to_value(&plain).expect("serialize plain job");
        assert!(wire.get("softBudgetExhausted").is_none());

        let marked = JobSnapshot {
            soft_budget_exhausted: true,
            ..plain
        };
        let wire = serde_json::to_value(&marked).expect("serialize marked job");
        assert_eq!(wire["softBudgetExhausted"], true);

        // Legacy payloads without the field deserialize as unlimited (false).
        let mut legacy = wire;
        legacy
            .as_object_mut()
            .expect("marked object")
            .remove("softBudgetExhausted");
        let decoded: JobSnapshot = serde_json::from_value(legacy).expect("deserialize legacy");
        assert!(!decoded.soft_budget_exhausted);
    }

    fn fixed_clock(millis: u64) -> JobClock {
        Arc::new(move || millis)
    }

    #[test]
    fn prepare_cancellation_keeps_owned_jobs_nonterminal() {
        let jobs = JobManager::with_retention(
            JobRetention {
                max_settled: 8,
                ttl: Duration::from_secs(60),
            },
            fixed_clock(50),
        );
        let token = CancellationToken::new();
        jobs.insert(snapshot("owned", "Worker"), token.clone())
            .expect("insert owned");
        jobs.insert(snapshot("free", "Spare"), CancellationToken::new())
            .expect("insert ownerless");

        let ownerless = HashSet::from(["free".to_owned()]);
        let mut prepared = jobs.prepare_cancellation(
            &["Worker".to_owned(), "Spare".to_owned()],
            CancellationCause::Shutdown,
            &ownerless,
        );
        assert!(!token.is_cancelled(), "prepare must not signal");
        assert_eq!(prepared.cancelling_ids(), &["owned".to_owned()]);
        assert_eq!(prepared.immediate_ids(), &["free".to_owned()]);
        let snapshots = prepared.take_snapshots();
        let owned = snapshots.iter().find(|job| job.id == "owned").expect("owned");
        assert_eq!(owned.status, JobStatus::Cancelling);
        assert_eq!(owned.cancellation_cause, Some(CancellationCause::Shutdown));
        assert!(owned.finished_at.is_none());
        assert!(owned.result.is_none());
        assert!(!owned.status.is_settled());
        let free = snapshots.iter().find(|job| job.id == "free").expect("free");
        assert_eq!(free.status, JobStatus::Cancelled);
        assert_eq!(free.cancellation_cause, Some(CancellationCause::Shutdown));
        assert_eq!(free.finished_at, Some(50));
        assert_eq!(
            free.result.as_ref().and_then(|result| result.error.as_deref()),
            None,
            "ownerless intentional stop must not manufacture an error"
        );

        let live = jobs.snapshots(Some(&["owned".to_owned()]));
        assert_eq!(live[0].status, JobStatus::Queued, "prepare is not a commit");
        assert!(!token.is_cancelled());
    }

    #[test]
    fn commit_cancellation_does_not_signal_and_is_idempotent() {
        let jobs = JobManager::with_retention(
            JobRetention {
                max_settled: 8,
                ttl: Duration::from_secs(60),
            },
            fixed_clock(50),
        );
        let token = CancellationToken::new();
        jobs.insert(snapshot("owned", "Worker"), token.clone())
            .expect("insert");
        let mut prepared = jobs.prepare_cancellation(
            &["owned".to_owned()],
            CancellationCause::Task,
            &HashSet::new(),
        );
        let _persisted = prepared.take_snapshots();
        let committed = jobs.commit_cancellation(prepared);
        assert_eq!(committed.cancelling_ids, vec!["owned".to_owned()]);
        assert!(committed.immediate_ids.is_empty());
        assert!(!token.is_cancelled(), "commit must not signal");
        let first = jobs.snapshots(None);
        assert_eq!(first[0].status, JobStatus::Cancelling);
        assert!(first[0].result.is_none());

        let mut again = jobs.prepare_cancellation(
            &["owned".to_owned()],
            CancellationCause::Shutdown,
            &HashSet::new(),
        );
        assert!(again.cancelling_ids().is_empty());
        let _persisted = again.take_snapshots();
        let committed_again = jobs.commit_cancellation(again);
        assert!(committed_again.cancelling_ids.is_empty());
        let second = jobs.snapshots(None);
        assert_eq!(second[0].cancellation_cause, Some(CancellationCause::Task));
        assert!(second[0].finished_at.is_none());
        assert!(second[0].result.is_none());
        assert!(!token.is_cancelled());
    }

    #[test]
    fn finish_from_cancelling_keeps_actual_result_and_aborts() {
        let jobs = JobManager::new();
        jobs.insert(snapshot("owned", "Worker"), CancellationToken::new())
            .expect("insert");
        let mut prepared = jobs.prepare_cancellation(
            &["owned".to_owned()],
            CancellationCause::Task,
            &HashSet::new(),
        );
        let _persisted = prepared.take_snapshots();
        jobs.commit_cancellation(prepared);
        let mut actual = task_result(false);
        actual.status = super::super::AgentStatus::Idle;
        actual.output = "drained".to_owned();
        actual.error = Some("provider failed during drain".to_owned());
        let finished = jobs.finish("owned", actual, 9).expect("finish");
        assert_eq!(finished.status, JobStatus::Cancelled);
        assert_eq!(finished.cancellation_cause, Some(CancellationCause::Task));
        assert_eq!(finished.finished_at, Some(9));
        let result = finished.result.expect("actual result");
        assert_eq!(result.status, super::super::AgentStatus::Aborted);
        assert_eq!(result.output, "drained");
        assert_eq!(
            result.error.as_deref(),
            Some("provider failed during drain")
        );
    }

    #[tokio::test]
    async fn unbounded_wait_does_not_release_on_cancelling() {
        let jobs = Arc::new(JobManager::new());
        jobs.insert(snapshot("owned", "Worker"), CancellationToken::new())
            .expect("insert");
        let mut prepared = jobs.prepare_cancellation(
            &["owned".to_owned()],
            CancellationCause::Task,
            &HashSet::new(),
        );
        let _persisted = prepared.take_snapshots();
        jobs.commit_cancellation(prepared);
        let waiting = {
            let jobs = Arc::clone(&jobs);
            tokio::spawn(async move {
                jobs.wait(
                    &["owned".to_owned()],
                    None,
                    None,
                    CancellationToken::new(),
                )
                .await
                .expect("wait")
            })
        };
        tokio::task::yield_now().await;
        assert!(
            !waiting.is_finished(),
            "Cancelling must not release an unbounded terminal wait"
        );
        let mut actual = task_result(false);
        actual.error = Some("cleanup failed".to_owned());
        jobs.finish("owned", actual, 4).expect("finish");
        let settled = waiting.await.expect("wait join");
        assert_eq!(settled[0].status, JobStatus::Cancelled);
        assert_eq!(
            settled[0]
                .result
                .as_ref()
                .and_then(|result| result.error.as_deref()),
            Some("cleanup failed")
        );
    }
    /// Polls a pinned future once: true iff Pending (parked), false if ready.
    async fn poll_once_parked<F: Future>(mut fut: std::pin::Pin<&mut F>) -> bool {
        std::future::poll_fn(|cx| match fut.as_mut().poll(cx) {
            std::task::Poll::Pending => std::task::Poll::Ready(true),
            std::task::Poll::Ready(_) => std::task::Poll::Ready(false),
        })
        .await
    }

    #[tokio::test]
    async fn unbounded_wait_is_bounded_when_a_settled_sibling_is_pruned() {
        // Control: no prune — both settle, wait returns both.
        let jobs = Arc::new(JobManager::new());
        jobs.insert(snapshot("ctl-a", "Alpha"), CancellationToken::new())
            .expect("insert");
        jobs.insert(snapshot("ctl-b", "Beta"), CancellationToken::new())
            .expect("insert");
        let ids = ["ctl-a".to_owned(), "ctl-b".to_owned()];
        let mut wait = std::pin::pin!(jobs.wait(&ids, None, None, CancellationToken::new()));
        assert!(poll_once_parked(wait.as_mut()).await, "control wait parked");
        jobs.finish("ctl-a", task_result(false), 2).expect("finish ctl-a");
        jobs.finish("ctl-b", task_result(false), 3).expect("finish ctl-b");
        let settled = tokio::time::timeout(Duration::from_secs(5), wait.as_mut())
            .await
            .expect("control without prune must resolve")
            .expect("control wait");
        assert_eq!(settled.len(), 2);
        assert!(settled.iter().all(|s| s.status.is_settled()));
        // Prune the settled sibling mid-wait: fail closed, never hang.
        let jobs = Arc::new(JobManager::new());
        jobs.insert(snapshot("prune-a", "Gamma"), CancellationToken::new())
            .expect("insert");
        jobs.insert(snapshot("prune-b", "Delta"), CancellationToken::new())
            .expect("insert");
        let ids = ["prune-a".to_owned(), "prune-b".to_owned()];
        let mut wait = std::pin::pin!(jobs.wait(&ids, None, None, CancellationToken::new()));
        assert!(poll_once_parked(wait.as_mut()).await, "prune wait parked");
        jobs.finish("prune-a", task_result(false), 2).expect("finish prune-a");
        assert!(jobs.remove_settled("prune-a"), "settled sibling pruned");
        jobs.finish("prune-b", task_result(false), 3).expect("finish prune-b");
        let error = tokio::time::timeout(Duration::from_secs(2), wait.as_mut())
            .await
            .expect("prune mid-wait must resolve in bounded time")
            .expect_err("a pruned requested id must fail the wait, not hang");
        assert_eq!(
            error
                .downcast_ref::<JobWaitMissingError>()
                .expect("typed missing-id error")
                .missing,
            ["prune-a".to_owned()]
        );
    }

    #[tokio::test]
    async fn wait_all_results_fails_closed_when_a_requested_job_is_pruned() {
        let jobs = Arc::new(JobManager::new());
        jobs.insert(snapshot("wa", "Alpha"), CancellationToken::new())
            .expect("insert");
        jobs.insert(snapshot("wb", "Beta"), CancellationToken::new())
            .expect("insert");
        let ids = ["wa".to_owned(), "wb".to_owned()];
        let mut waiting = std::pin::pin!(jobs.wait_all_results(
            &ids,
            pi_agent::AbortSignal::none(),
            CancellationToken::new(),
        ));
        assert!(poll_once_parked(waiting.as_mut()).await, "result wait parked");
        jobs.finish("wa", task_result(false), 2).expect("finish wa");
        assert!(jobs.remove_settled("wa"), "settled job pruned");
        jobs.finish("wb", task_result(false), 3).expect("finish wb");
        let error = tokio::time::timeout(Duration::from_secs(2), waiting.as_mut())
            .await
            .expect("prune mid-wait must resolve in bounded time")
            .expect_err("a pruned requested id must fail the result wait, not hang");
        assert_eq!(
            error
                .downcast_ref::<JobWaitMissingError>()
                .expect("typed missing-id error")
                .missing,
            ["wa".to_owned()]
        );
    }
}
