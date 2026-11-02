//! Durable catalog storage and repository-scoped operation locking.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::anyhow;
use parking_lot::{Condvar, Mutex};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::fs::{
    CapabilityAccess, CapabilityRootLease, FileIdentity, FileMode, FsError, Replacement,
    RootCreation,
};

use super::{WorkflowWorktreeIdentity, WorktreeError};

pub(super) const CATALOG_DIR_NAME: &str = "pi-workflow";
const CATALOG_FILE_NAME: &str = "worktrees.json";
const CATALOG_MAX_BYTES: u64 = 4 * 1024 * 1024;
pub(super) const REPO_LOCK_FILE_NAME: &str = "pi-workflow-worktree.lock";
const REPO_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const LOCK_FILE_MAX_BYTES: u64 = 4096;

static PROCESS_REPO_LOCKS: LazyLock<(Mutex<HashSet<FileIdentity>>, Condvar)> =
    LazyLock::new(|| (Mutex::new(HashSet::new()), Condvar::new()));

#[derive(Clone, Debug, Default)]
pub(super) struct WorktreeCatalog {
    pub(super) entries: HashMap<String, WorkflowWorktreeIdentity>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredWorktreeIdentity {
    workflow_id: String,
    source_root: PathBuf,
    common_git_dir: PathBuf,
    worktree_path: PathBuf,
    branch: String,
    base_commit: String,
    head_commit: String,
    created_at_ms: u64,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredWorktreeCatalog {
    #[serde(default)]
    entries: HashMap<String, StoredWorktreeIdentity>,
}

impl From<StoredWorktreeIdentity> for WorkflowWorktreeIdentity {
    fn from(stored: StoredWorktreeIdentity) -> Self {
        Self {
            workflow_id: stored.workflow_id,
            source_root: stored.source_root,
            common_git_dir: stored.common_git_dir,
            worktree_path: stored.worktree_path,
            branch: stored.branch,
            base_commit: stored.base_commit,
            head_commit: stored.head_commit,
            created_at_ms: stored.created_at_ms,
        }
    }
}

impl From<&WorkflowWorktreeIdentity> for StoredWorktreeIdentity {
    fn from(identity: &WorkflowWorktreeIdentity) -> Self {
        Self {
            workflow_id: identity.workflow_id.clone(),
            source_root: identity.source_root.clone(),
            common_git_dir: identity.common_git_dir.clone(),
            worktree_path: identity.worktree_path.clone(),
            branch: identity.branch.clone(),
            base_commit: identity.base_commit.clone(),
            head_commit: identity.head_commit.clone(),
            created_at_ms: identity.created_at_ms,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LockRecord {
    pid: u32,
    token: String,
    created_at_ms: u64,
}

struct ProcessRepoLock {
    key: FileIdentity,
}

impl Drop for ProcessRepoLock {
    fn drop(&mut self) {
        let mut active = PROCESS_REPO_LOCKS.0.lock();
        active.remove(&self.key);
        PROCESS_REPO_LOCKS.1.notify_all();
    }
}

pub(super) struct RepoLock {
    lease: CapabilityRootLease,
    token: String,
    _process: ProcessRepoLock,
}

impl Drop for RepoLock {
    fn drop(&mut self) {
        if lock_token_matches(&self.lease, &self.token) {
            let _ = self.lease.remove_file(Path::new(REPO_LOCK_FILE_NAME));
        }
    }
}

pub(super) fn acquire_repo_lock(lease: &CapabilityRootLease) -> Result<RepoLock, WorktreeError> {
    let process = acquire_process_lock(lease.identity())?;
    let token = Uuid::new_v4().simple().to_string();
    let record = LockRecord {
        pid: std::process::id(),
        token: token.clone(),
        created_at_ms: unix_millis(),
    };
    let mut bytes = serde_json::to_vec(&record)
        .map_err(|error| WorktreeError::Other(anyhow!("serializing repository lock: {error}")))?;
    bytes.push(b'\n');
    let started = Instant::now();
    loop {
        match lease.create_new(Path::new(REPO_LOCK_FILE_NAME), FileMode::Private) {
            Ok(mut file) => {
                file.write_all(&bytes).map_err(capability_error)?;
                file.finish().map_err(capability_error)?;
                return Ok(RepoLock {
                    lease: lease.clone(),
                    token,
                    _process: process,
                });
            }
            Err(error) if is_already_exists(&error) => {
                if started.elapsed() >= REPO_LOCK_TIMEOUT {
                    return Err(WorktreeError::Other(anyhow!(
                        "timed out waiting for repository worktree lock"
                    )));
                }
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(capability_error(error)),
        }
    }
}

pub(super) fn load_catalog(lease: &CapabilityRootLease) -> Result<WorktreeCatalog, WorktreeError> {
    let Some(catalog) = catalog_lease_existing(lease)? else {
        return Ok(WorktreeCatalog::default());
    };
    let bytes = match catalog.read_bounded(Path::new(CATALOG_FILE_NAME), CATALOG_MAX_BYTES) {
        Ok(bytes) => bytes,
        Err(error) if is_missing(&error) => return Ok(WorktreeCatalog::default()),
        Err(error) => return Err(capability_error(error)),
    };
    let stored: StoredWorktreeCatalog = serde_json::from_slice(&bytes).map_err(|error| {
        WorktreeError::Other(anyhow!("parsing worktree ownership catalog: {error}"))
    })?;
    Ok(WorktreeCatalog {
        entries: stored
            .entries
            .into_iter()
            .map(|(id, identity)| (id, identity.into()))
            .collect(),
    })
}

/// Raw catalog bytes for rebind state classification; `None` when the catalog
/// has never been written under this managed root. Unlike [`load_catalog`],
/// this preserves the exact on-disk bytes so a relocated copy can be compared
/// byte-for-byte against its source.
pub(super) fn read_catalog_bytes(
    lease: &CapabilityRootLease,
) -> Result<Option<Vec<u8>>, WorktreeError> {
    let Some(catalog) = catalog_lease_existing(lease)? else {
        return Ok(None);
    };
    match catalog.read_bounded(Path::new(CATALOG_FILE_NAME), CATALOG_MAX_BYTES) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if is_missing(&error) => Ok(None),
        Err(error) => Err(capability_error(error)),
    }
}

/// Parse raw catalog bytes into the in-memory catalog. Fail-closed on any
/// serialization drift: a corrupt catalog never degrades into an empty one.
pub(super) fn parse_catalog_bytes(bytes: &[u8]) -> Result<WorktreeCatalog, WorktreeError> {
    let stored: StoredWorktreeCatalog = serde_json::from_slice(bytes).map_err(|error| {
        WorktreeError::Other(anyhow!("parsing worktree ownership catalog: {error}"))
    })?;
    Ok(WorktreeCatalog {
        entries: stored
            .entries
            .into_iter()
            .map(|(id, identity)| (id, identity.into()))
            .collect(),
    })
}

pub(super) fn with_catalog_mut<F>(
    lease: &CapabilityRootLease,
    mutate: F,
) -> Result<(), WorktreeError>
where
    F: FnOnce(&mut WorktreeCatalog) -> Result<(), WorktreeError>,
{
    let mut catalog = load_catalog(lease)?;
    mutate(&mut catalog)?;
    write_catalog_atomic(lease, &catalog)
}

fn write_catalog_atomic(
    lease: &CapabilityRootLease,
    catalog: &WorktreeCatalog,
) -> Result<(), WorktreeError> {
    let stored = StoredWorktreeCatalog {
        entries: catalog
            .entries
            .iter()
            .map(|(id, identity)| (id.clone(), identity.into()))
            .collect(),
    };
    let bytes = serde_json::to_vec(&stored)
        .map_err(|error| WorktreeError::Other(anyhow!("serializing worktree catalog: {error}")))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > CATALOG_MAX_BYTES {
        return Err(WorktreeError::Other(anyhow!(
            "serialized worktree catalog exceeds {CATALOG_MAX_BYTES} bytes"
        )));
    }
    let catalog_lease = catalog_lease(lease)?;
    let relative = PathBuf::from(CATALOG_FILE_NAME);
    let expected = match catalog_lease.metadata(&relative) {
        Ok(metadata) => Some(metadata.identity),
        Err(error) if is_missing(&error) => None,
        Err(error) => return Err(capability_error(error)),
    };
    catalog_lease
        .replace_file(Replacement {
            relative,
            expected,
            bytes,
            mode: FileMode::Private,
        })
        .map_err(capability_error)?;
    Ok(())
}

fn catalog_lease(lease: &CapabilityRootLease) -> Result<CapabilityRootLease, WorktreeError> {
    lease
        .lease_directory(
            Path::new(CATALOG_DIR_NAME),
            CapabilityAccess::PrivateReadWrite,
            RootCreation::CreateDirectory,
        )
        .map_err(capability_error)
}

fn catalog_lease_existing(
    lease: &CapabilityRootLease,
) -> Result<Option<CapabilityRootLease>, WorktreeError> {
    match lease.lease_directory(
        Path::new(CATALOG_DIR_NAME),
        CapabilityAccess::PrivateReadWrite,
        RootCreation::Existing,
    ) {
        Ok(catalog) => Ok(Some(catalog)),
        Err(error) if is_missing(&error) => Ok(None),
        Err(error) => Err(capability_error(error)),
    }
}

fn acquire_process_lock(key: FileIdentity) -> Result<ProcessRepoLock, WorktreeError> {
    let started = Instant::now();
    let mut active = PROCESS_REPO_LOCKS.0.lock();
    loop {
        if active.insert(key) {
            return Ok(ProcessRepoLock { key });
        }
        let elapsed = started.elapsed();
        if elapsed >= REPO_LOCK_TIMEOUT {
            return Err(WorktreeError::Other(anyhow!(
                "timed out waiting for process-local worktree lock"
            )));
        }
        PROCESS_REPO_LOCKS
            .1
            .wait_for(&mut active, REPO_LOCK_TIMEOUT - elapsed);
    }
}

fn lock_token_matches(lease: &CapabilityRootLease, token: &str) -> bool {
    lease
        .read_bounded(Path::new(REPO_LOCK_FILE_NAME), LOCK_FILE_MAX_BYTES)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<LockRecord>(&bytes).ok())
        .is_some_and(|record| record.token == token)
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

fn is_missing(error: &FsError) -> bool {
    matches!(
        error,
        FsError::System {
            kind: std::io::ErrorKind::NotFound,
            ..
        }
    )
}

fn is_already_exists(error: &FsError) -> bool {
    matches!(
        error,
        FsError::System {
            kind: std::io::ErrorKind::AlreadyExists,
            ..
        }
    )
}

fn capability_error(error: FsError) -> WorktreeError {
    WorktreeError::Other(anyhow!("workflow worktree capability failed: {error}"))
}
