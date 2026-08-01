//! Durable catalog storage and repository-scoped operation locking.

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::anyhow;
use parking_lot::{Condvar, Mutex};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{RepoDiscovery, WorkflowWorktreeIdentity, WorktreeError};

const CATALOG_DIR_NAME: &str = "pi-workflow";
const CATALOG_FILE_NAME: &str = "worktrees.json";
const CATALOG_MAX_BYTES: u64 = 4 * 1024 * 1024;
const REPO_LOCK_FILE_NAME: &str = "pi-workflow-worktree.lock";
const REPO_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const STALE_REPO_LOCK_AGE: Duration = Duration::from_secs(10 * 60);
const LOCK_FILE_MAX_BYTES: u64 = 4096;

/// Keys currently locked by this process. The key is the canonical common git
/// directory, so different repositories never block each other. The registry
/// mutex is held only while inserting/removing a key; the filesystem lock is
/// always acquired afterward.
static PROCESS_REPO_LOCKS: LazyLock<(Mutex<BTreeSet<PathBuf>>, Condvar)> =
    LazyLock::new(|| (Mutex::new(BTreeSet::new()), Condvar::new()));

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
    key: PathBuf,
}

impl Drop for ProcessRepoLock {
    fn drop(&mut self) {
        let mut active = PROCESS_REPO_LOCKS.0.lock();
        active.remove(&self.key);
        PROCESS_REPO_LOCKS.1.notify_all();
    }
}

/// Holds the process-local repository slot and the matching cross-process lock
/// file. The token check prevents an old guard from deleting a successor's lock
/// if the filesystem entry was externally replaced.
pub(super) struct RepoLock {
    path: PathBuf,
    token: String,
    _process: ProcessRepoLock,
}

impl Drop for RepoLock {
    fn drop(&mut self) {
        if lock_token_matches(&self.path, &self.token) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(super) fn acquire_repo_lock(common_git_dir: &Path) -> Result<RepoLock, WorktreeError> {
    let key = canonical_lock_key(common_git_dir)?;
    let process = acquire_process_lock(key.clone())?;
    let path = key.join(REPO_LOCK_FILE_NAME);
    reject_symlink_components(&path)?;

    let token = Uuid::new_v4().simple().to_string();
    let record = LockRecord {
        pid: std::process::id(),
        token: token.clone(),
        created_at_ms: unix_millis(),
    };
    let bytes = serde_json::to_vec(&record)
        .map_err(|error| WorktreeError::Other(anyhow!("serializing repository lock: {error}")))?;
    let started = Instant::now();

    loop {
        match private_create_new(&path) {
            Ok(mut file) => {
                let result = file
                    .write_all(&bytes)
                    .and_then(|()| file.write_all(b"\n"))
                    .and_then(|()| file.sync_all());
                if let Err(error) = result {
                    let _ = fs::remove_file(&path);
                    return Err(WorktreeError::Other(anyhow!(
                        "writing repository worktree lock: {error}"
                    )));
                }
                return Ok(RepoLock {
                    path,
                    token,
                    _process: process,
                });
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                if lock_is_stale(&path) {
                    match fs::remove_file(&path) {
                        Ok(()) => continue,
                        Err(remove_error) if remove_error.kind() == ErrorKind::NotFound => continue,
                        Err(_) => {}
                    }
                }
                if started.elapsed() >= REPO_LOCK_TIMEOUT {
                    return Err(WorktreeError::Other(anyhow!(
                        "timed out waiting for repository worktree lock"
                    )));
                }
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => {
                return Err(WorktreeError::Other(anyhow!(
                    "creating repository worktree lock: {error}"
                )));
            }
        }
    }
}

pub(super) fn catalog_path(
    override_path: Option<&Path>,
    discovery: &RepoDiscovery,
) -> PathBuf {
    override_path.map_or_else(
        || {
            discovery
                .common_git_dir
                .join(CATALOG_DIR_NAME)
                .join(CATALOG_FILE_NAME)
        },
        Path::to_path_buf,
    )
}

pub(super) fn load_catalog(path: &Path) -> Result<WorktreeCatalog, WorktreeError> {
    reject_symlink_components(path)?;
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(WorktreeCatalog::default());
        }
        Err(error) => {
            return Err(WorktreeError::Other(anyhow!(
                "opening worktree ownership catalog: {error}"
            )));
        }
    };
    let length = file
        .metadata()
        .map_err(|error| {
            WorktreeError::Other(anyhow!(
                "reading worktree ownership catalog metadata: {error}"
            ))
        })?
        .len();
    if length > CATALOG_MAX_BYTES {
        return Err(WorktreeError::Other(anyhow!(
            "worktree ownership catalog exceeds {CATALOG_MAX_BYTES} bytes"
        )));
    }

    let mut bytes = Vec::with_capacity(usize::try_from(length).unwrap_or(0));
    file.take(CATALOG_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            WorktreeError::Other(anyhow!(
                "reading worktree ownership catalog: {error}"
            ))
        })?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > CATALOG_MAX_BYTES {
        return Err(WorktreeError::Other(anyhow!(
            "worktree ownership catalog exceeds {CATALOG_MAX_BYTES} bytes"
        )));
    }
    let stored: StoredWorktreeCatalog = serde_json::from_slice(&bytes).map_err(|error| {
        WorktreeError::Other(anyhow!("parsing worktree ownership catalog: {error}"))
    })?;
    Ok(WorktreeCatalog {
        entries: stored
            .entries
            .into_iter()
            .map(|(workflow_id, identity)| (workflow_id, identity.into()))
            .collect(),
    })
}

pub(super) fn with_catalog_mut<F>(path: &Path, mutate: F) -> Result<(), WorktreeError>
where
    F: FnOnce(&mut WorktreeCatalog) -> Result<(), WorktreeError>,
{
    let mut catalog = load_catalog(path)?;
    mutate(&mut catalog)?;
    write_catalog_atomic(path, &catalog)
}

fn write_catalog_atomic(
    path: &Path,
    catalog: &WorktreeCatalog,
) -> Result<(), WorktreeError> {
    let stored = StoredWorktreeCatalog {
        entries: catalog
            .entries
            .iter()
            .map(|(workflow_id, identity)| {
                (workflow_id.clone(), StoredWorktreeIdentity::from(identity))
            })
            .collect(),
    };
    let bytes = serde_json::to_vec(&stored)
        .map_err(|error| WorktreeError::Other(anyhow!("serializing worktree catalog: {error}")))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > CATALOG_MAX_BYTES {
        return Err(WorktreeError::Other(anyhow!(
            "serialized worktree catalog exceeds {} bytes",
            CATALOG_MAX_BYTES
        )));
    }

    let parent = path.parent().ok_or_else(|| {
        WorktreeError::Other(anyhow!("worktree ownership catalog has no parent directory"))
    })?;
    create_dir_chain_no_symlink(parent)?;
    reject_symlink_components(path)?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(CATALOG_FILE_NAME);
    let temporary = parent.join(format!(
        ".{file_name}.{}.tmp",
        Uuid::new_v4().simple()
    ));

    let result = (|| -> Result<(), WorktreeError> {
        let mut file = private_create_new(&temporary).map_err(|error| {
            WorktreeError::Other(anyhow!(
                "creating temporary worktree ownership catalog: {error}"
            ))
        })?;
        file.write_all(&bytes).map_err(|error| {
            WorktreeError::Other(anyhow!(
                "writing temporary worktree ownership catalog: {error}"
            ))
        })?;
        file.sync_all().map_err(|error| {
            WorktreeError::Other(anyhow!(
                "syncing temporary worktree ownership catalog: {error}"
            ))
        })?;
        fs::rename(&temporary, path).map_err(|error| {
            WorktreeError::Other(anyhow!(
                "activating worktree ownership catalog: {error}"
            ))
        })?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| {
                WorktreeError::Other(anyhow!(
                    "syncing worktree ownership catalog directory: {error}"
                ))
            })?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn acquire_process_lock(key: PathBuf) -> Result<ProcessRepoLock, WorktreeError> {
    let started = Instant::now();
    let mut active = PROCESS_REPO_LOCKS.0.lock();
    loop {
        if active.insert(key.clone()) {
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

fn canonical_lock_key(path: &Path) -> Result<PathBuf, WorktreeError> {
    reject_symlink_components(path)?;
    fs::canonicalize(path).map_err(|error| {
        WorktreeError::Other(anyhow!(
            "canonicalizing common git directory: {error}"
        ))
    })
}

fn lock_is_stale(path: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.file_type().is_file() {
        return false;
    }
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    if !modified
        .elapsed()
        .is_ok_and(|age| age >= STALE_REPO_LOCK_AGE)
    {
        return false;
    }

    match read_lock_record(path) {
        Some(record) => !process_is_alive(record.pid),
        None => true,
    }
}

fn lock_token_matches(path: &Path, token: &str) -> bool {
    read_lock_record(path).is_some_and(|record| record.token == token)
}

fn read_lock_record(path: &Path) -> Option<LockRecord> {
    let mut file = File::open(path).ok()?;
    if file.metadata().ok()?.len() > LOCK_FILE_MAX_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(LOCK_FILE_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if u64::try_from(bytes.len()).ok()? > LOCK_FILE_MAX_BYTES {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return true;
    };
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
        Ok(()) | Err(nix::errno::Errno::EPERM) => true,
        Err(nix::errno::Errno::ESRCH) => false,
        Err(_) => true,
    }
}

#[cfg(not(unix))]
fn process_is_alive(_pid: u32) -> bool {
    true
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

fn create_dir_chain_no_symlink(path: &Path) -> Result<(), WorktreeError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| WorktreeError::Other(error.into()))?
            .join(path)
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(WorktreeError::Symlink);
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(WorktreeError::Other(anyhow!(
                    "catalog parent component is not a directory"
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(|create_error| {
                    WorktreeError::Other(anyhow!(
                        "creating catalog directory: {create_error}"
                    ))
                })?;
            }
            Err(error) => {
                return Err(WorktreeError::Other(anyhow!(
                    "inspecting catalog directory: {error}"
                )));
            }
        }
    }
    Ok(())
}

fn reject_symlink_components(path: &Path) -> Result<(), WorktreeError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| WorktreeError::Other(error.into()))?
            .join(path)
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(WorktreeError::Symlink);
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => break,
            Err(error) => {
                return Err(WorktreeError::Other(anyhow!(
                    "inspecting catalog path component: {error}"
                )));
            }
        }
    }
    Ok(())
}

fn private_create_new(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
