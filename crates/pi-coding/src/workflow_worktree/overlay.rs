//! Overlayfs workflow isolation: a git-worktree-free isolation backend.
//!
//! Each workflow gets a private overlay whose read-only lower layer is the
//! live source tree and whose writable upper layer is a private
//! copy-on-write directory under the managed root. The merged view therefore
//! behaves like a full working tree (the workflow can run git inside it — the
//! shared `.git` of the lower layer is copied up per workflow on first write),
//! without creating git worktrees or touching the source's real refs.
//!
//! Integration is **copy-back**: the merged tree is synced over the source
//! working tree (excluding the repo's own `.git` — the workflow's private
//! commits must never clobber the source's history/config), then the resulting
//! source state is committed as a single commit on the source branch. There is
//! no merge history and therefore no merge-conflict detection: overlayfs
//! integration is last-writer-wins by design. [`IntegrateOutcome::Conflicted`]
//! is never produced by this backend.
//!
//! Divergences from the git-worktree backend (documented contract):
//! - The lower layer is the **live** source tree: files the workflow has not
//!   modified track the source's current state (including uncommitted
//!   changes) instead of a snapshot of HEAD.
//! - Mounts do not survive a process restart; restore re-establishes the
//!   recorded backend from the ownership catalog (rcopy is never re-run over
//!   an existing upper, which would clobber the workflow's changes).
//! - The branch label is `rpi/overlay/<id>` and no git branch is created.
//!
//! Backend selection follows [`crate::isolate`]: kernel overlay →
//! fuse-overlayfs → recursive copy. The chosen backend is persisted per
//! workflow so a restored workflow re-mounts exactly what it had before.

use std::collections::HashMap;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::isolate::{OverlayBackend, OverlayfsIsolation};

use super::catalog::acquire_repo_lock;
use super::git::{
    canonicalize_no_symlink, canonicalize_or_create_dir, command_failed, commit_count_ahead,
    discover_repo, is_dirty, list_changed_files, now_ms, path_exists, reject_if_inside,
    resolve_commit, rev_parse, run_git, run_git_allow_fail, sanitize_path_segment,
    validate_workflow_id,
};
use super::{
    CreateWorktreeOptions, IntegrateOptions, IntegrateOutcome, IntegrateStrategy,
    TrustedWorkflowCwd, WorkflowIsolation, WorkflowIsolationSetting, WorkflowWorktreeIdentity,
    WorkflowWorktreeStatus, WorktreeError, DEFAULT_GIT_TIMEOUT,
};

/// Directory name under the managed root holding per-workflow overlay trees.
pub const OVERLAY_ROOT_DIR_NAME: &str = "overlay-workflows";

/// Branch-namespace label for overlay-isolated workflows (no real git branch
/// is created; the label keeps snapshots/UI self-consistent).
pub const OVERLAY_BRANCH_PREFIX: &str = "rpi/overlay/";

/// Catalog directory/file names under the managed root.
const CATALOG_DIR_NAME: &str = "pi-workflow";
const CATALOG_FILE_NAME: &str = "overlay-workflows.json";
const CATALOG_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Default backend chain: kernel overlay, then fuse-overlayfs, then rcopy.
pub fn default_backend_candidates() -> Vec<OverlayBackend> {
    vec![
        OverlayBackend::Kernel,
        OverlayBackend::FuseOverlayfs,
        OverlayBackend::Rcopy,
    ]
}

/// Persisted ownership record: the workflow's identity plus the backend that
/// materialized its merged view (required to re-mount after a restart).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OverlayCatalogEntry {
    workflow_id: String,
    source_root: PathBuf,
    common_git_dir: PathBuf,
    worktree_path: PathBuf,
    branch: String,
    base_commit: String,
    head_commit: String,
    created_at_ms: u64,
    backend: OverlayBackend,
}

impl OverlayCatalogEntry {
    fn from_identity(identity: &WorkflowWorktreeIdentity, backend: OverlayBackend) -> Self {
        Self {
            workflow_id: identity.workflow_id.clone(),
            source_root: identity.source_root.clone(),
            common_git_dir: identity.common_git_dir.clone(),
            worktree_path: identity.worktree_path.clone(),
            branch: identity.branch.clone(),
            base_commit: identity.base_commit.clone(),
            head_commit: identity.head_commit.clone(),
            created_at_ms: identity.created_at_ms,
            backend,
        }
    }

    fn identity(&self) -> WorkflowWorktreeIdentity {
        WorkflowWorktreeIdentity {
            workflow_id: self.workflow_id.clone(),
            source_root: self.source_root.clone(),
            common_git_dir: self.common_git_dir.clone(),
            worktree_path: self.worktree_path.clone(),
            branch: self.branch.clone(),
            base_commit: self.base_commit.clone(),
            head_commit: self.head_commit.clone(),
            created_at_ms: self.created_at_ms,
        }
    }

    /// The identity dir holding merged/upper/work (siblings of the merged view).
    fn identity_dir(&self) -> Option<&Path> {
        self.worktree_path.parent()
    }

    fn upper_path(&self) -> PathBuf {
        self.identity_dir().map_or_else(PathBuf::new, |dir| dir.join("upper"))
    }

    fn work_path(&self) -> PathBuf {
        self.identity_dir().map_or_else(PathBuf::new, |dir| dir.join("work"))
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OverlayCatalog {
    #[serde(default)]
    entries: HashMap<String, OverlayCatalogEntry>,
}

/// Overlayfs-backed workflow isolation for a single source project.
///
/// The managed root (and therefore the ownership catalog) is fixed at
/// construction — it is the session-scoped workflow-worktrees root the
/// workflow factory owns, so a resumed session (same session id) restores its
/// workflows while a new session starts empty.
#[derive(Clone, Debug)]
pub struct OverlayWorkflowManager {
    source_cwd: PathBuf,
    managed_root: PathBuf,
    timeout: Duration,
    backend_candidates: Vec<OverlayBackend>,
    catalog_path_override: Option<PathBuf>,
}

impl OverlayWorkflowManager {
    /// Build a manager bound to a trusted source project directory and the
    /// session-scoped managed root that will hold the overlay workflows.
    pub fn new(source_cwd: impl Into<PathBuf>, managed_root: impl Into<PathBuf>) -> Self {
        Self {
            source_cwd: source_cwd.into(),
            managed_root: managed_root.into(),
            timeout: DEFAULT_GIT_TIMEOUT,
            backend_candidates: default_backend_candidates(),
            catalog_path_override: None,
        }
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    #[must_use]
    pub fn with_catalog_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.catalog_path_override = Some(path.into());
        self
    }

    /// Restrict the backend chain (tests force [`OverlayBackend::Rcopy`] to
    /// exercise the manager without mount privileges).
    #[must_use]
    pub fn with_backend_candidates(mut self, candidates: Vec<OverlayBackend>) -> Self {
        self.backend_candidates = candidates;
        self
    }

    #[must_use]
    pub fn source_cwd(&self) -> &Path {
        &self.source_cwd
    }

    #[must_use]
    pub fn managed_root(&self) -> &Path {
        &self.managed_root
    }

    fn catalog_path(&self) -> PathBuf {
        self.catalog_path_override.clone().unwrap_or_else(|| {
            self.managed_root
                .join(CATALOG_DIR_NAME)
                .join(CATALOG_FILE_NAME)
        })
    }

    fn load_catalog(&self) -> Result<OverlayCatalog, WorktreeError> {
        let path = self.catalog_path();
        let mut file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Ok(OverlayCatalog::default());
            }
            Err(error) => {
                return Err(WorktreeError::Other(anyhow!(
                    "opening overlay ownership catalog: {error}"
                )));
            }
        };
        let length = file
            .metadata()
            .map_err(|error| {
                WorktreeError::Other(anyhow!(
                    "reading overlay ownership catalog metadata: {error}"
                ))
            })?
            .len();
        if length > CATALOG_MAX_BYTES {
            return Err(WorktreeError::Other(anyhow!(
                "overlay ownership catalog exceeds {CATALOG_MAX_BYTES} bytes"
            )));
        }
        let mut bytes = Vec::with_capacity(usize::try_from(length).unwrap_or(0));
        file.take(CATALOG_MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| {
                WorktreeError::Other(anyhow!(
                    "reading overlay ownership catalog: {error}"
                ))
            })?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > CATALOG_MAX_BYTES {
            return Err(WorktreeError::Other(anyhow!(
                "overlay ownership catalog exceeds {CATALOG_MAX_BYTES} bytes"
            )));
        }
        serde_json::from_slice(&bytes).map_err(|error| {
            WorktreeError::Other(anyhow!("parsing overlay ownership catalog: {error}"))
        })
    }

    fn with_catalog_mut<F>(&self, mutate: F) -> Result<(), WorktreeError>
    where
        F: FnOnce(&mut OverlayCatalog) -> Result<(), WorktreeError>,
    {
        let mut catalog = self.load_catalog()?;
        mutate(&mut catalog)?;
        self.write_catalog(&catalog)
    }

    fn write_catalog(&self, catalog: &OverlayCatalog) -> Result<(), WorktreeError> {
        let path = self.catalog_path();
        let bytes = serde_json::to_vec(catalog).map_err(|error| {
            WorktreeError::Other(anyhow!("serializing overlay ownership catalog: {error}"))
        })?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > CATALOG_MAX_BYTES {
            return Err(WorktreeError::Other(anyhow!(
                "serialized overlay ownership catalog exceeds {CATALOG_MAX_BYTES} bytes"
            )));
        }
        let parent = path.parent().ok_or_else(|| {
            WorktreeError::Other(anyhow!("overlay ownership catalog has no parent directory"))
        })?;
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(CATALOG_FILE_NAME);
        let temporary = parent.join(format!(".{file_name}.{}.tmp", Uuid::new_v4().simple()));
        let result = (|| -> Result<(), WorktreeError> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|error| {
                    WorktreeError::Other(anyhow!(
                        "creating temporary overlay ownership catalog: {error}"
                    ))
                })?;
            file.write_all(&bytes).map_err(|error| {
                WorktreeError::Other(anyhow!(
                    "writing temporary overlay ownership catalog: {error}"
                ))
            })?;
            file.sync_all().map_err(|error| {
                WorktreeError::Other(anyhow!(
                    "syncing temporary overlay ownership catalog: {error}"
                ))
            })?;
            fs::rename(&temporary, &path).map_err(|error| {
                WorktreeError::Other(anyhow!(
                    "activating overlay ownership catalog: {error}"
                ))
            })?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn load_entry(&self, workflow_id: &str) -> Result<Option<OverlayCatalogEntry>, WorktreeError> {
        Ok(self.load_catalog()?.entries.get(workflow_id).cloned())
    }

    /// Re-establish the recorded overlay mount after a process restart (mounts
    /// do not survive). rcopy backends need nothing — the merged view IS the
    /// working copy. Mounted backends are re-mounted with the recorded backend
    /// only; rcopy is never re-run over an existing upper because it would
    /// clobber the workflow's changes.
    fn ensure_mounted(&self, entry: &OverlayCatalogEntry) -> Result<(), WorktreeError> {
        let identity = entry.identity();
        if entry.backend == OverlayBackend::Rcopy {
            if !path_exists(&identity.worktree_path) {
                return Err(WorktreeError::ownership(
                    &identity.workflow_id,
                    "recorded merged working copy is missing",
                ));
            }
            return Ok(());
        }
        let isolation = OverlayfsIsolation::restore(
            &identity.source_root,
            &entry.upper_path(),
            &entry.work_path(),
            &identity.worktree_path,
            entry.backend,
        );
        if isolation.is_mounted() {
            return Ok(());
        }
        OverlayfsIsolation::start_with(
            &identity.source_root,
            &entry.upper_path(),
            &entry.work_path(),
            &identity.worktree_path,
            &[entry.backend],
        )
        .map_err(|error| {
            WorktreeError::Other(anyhow!(
                "re-establishing workflow overlay mount: {error}"
            ))
        })?;
        Ok(())
    }
}

impl WorkflowIsolation for OverlayWorkflowManager {
    fn create(
        &self,
        workflow_id: &str,
        options: CreateWorktreeOptions,
    ) -> Result<WorkflowWorktreeIdentity, WorktreeError> {
        validate_workflow_id(workflow_id)?;
        let timeout = options.timeout.unwrap_or(self.timeout);
        let source_root = canonicalize_no_symlink(&self.source_cwd)?;
        let managed_root = canonicalize_or_create_dir(&options.managed_root)?;
        let expected_managed = canonicalize_or_create_dir(&self.managed_root)?;
        if managed_root != expected_managed {
            return Err(WorktreeError::Other(anyhow!(
                "managed root does not match the overlay isolation manager's root"
            )));
        }
        reject_if_inside(&managed_root, &source_root).map_err(|detail| {
            WorktreeError::Other(anyhow!(
                "managed root must not be inside the source worktree: {detail}"
            ))
        })?;

        let discovery = match discover_repo(&source_root, timeout) {
            Ok(discovery) => Some(discovery),
            Err(WorktreeError::NotGit) => None,
            Err(error) => return Err(error),
        };
        let base_commit = match (&options.base_commit, &discovery) {
            (Some(revision), Some(_)) => resolve_commit(&source_root, revision, timeout)?,
            (None, Some(discovery)) => discovery.head_commit.clone(),
            (Some(_), None) | (None, None) => "none".to_owned(),
        };

        let identity_dir = managed_root
            .join(OVERLAY_ROOT_DIR_NAME)
            .join(sanitize_path_segment(workflow_id));
        if path_exists(&identity_dir) {
            return Err(WorktreeError::Other(anyhow!(
                "managed overlay location already exists for workflow {workflow_id}"
            )));
        }
        let parent = identity_dir.parent().ok_or_else(|| {
            WorktreeError::Other(anyhow!("overlay identity dir has no parent"))
        })?;
        canonicalize_or_create_dir(parent)?;
        let catalog_dir = managed_root.join(CATALOG_DIR_NAME);
        canonicalize_or_create_dir(&catalog_dir)?;

        let merged = identity_dir.join("merged");
        let upper = identity_dir.join("upper");
        let work = identity_dir.join("work");
        let isolation = OverlayfsIsolation::start_with(
            &source_root,
            &upper,
            &work,
            &merged,
            &self.backend_candidates,
        )
        .map_err(|error| {
            WorktreeError::Other(anyhow!("workflow overlay isolation failed: {error}"))
        })?;

        let head_commit = if discovery.is_some() {
            rev_parse(&merged, "HEAD", timeout)?
        } else {
            "none".to_owned()
        };
        let identity = WorkflowWorktreeIdentity {
            workflow_id: workflow_id.to_string(),
            source_root,
            common_git_dir: managed_root,
            worktree_path: merged,
            branch: format!("{OVERLAY_BRANCH_PREFIX}{workflow_id}"),
            base_commit,
            head_commit,
            created_at_ms: now_ms(),
        };
        let _lock = acquire_repo_lock(&catalog_dir)?;
        self.with_catalog_mut(|catalog| {
            catalog.entries.insert(
                workflow_id.to_string(),
                OverlayCatalogEntry::from_identity(&identity, isolation.backend()),
            );
            Ok(())
        })?;
        Ok(identity)
    }

    fn verify_owned(
        &self,
        identity: &WorkflowWorktreeIdentity,
    ) -> Result<TrustedWorkflowCwd, WorktreeError> {
        validate_workflow_id(&identity.workflow_id)?;
        let entry = self
            .load_entry(&identity.workflow_id)?
            .ok_or_else(|| WorktreeError::NotRegistered(identity.workflow_id.clone()))?;
        let recorded = entry.identity();
        if recorded != *identity {
            return Err(WorktreeError::ownership(
                &identity.workflow_id,
                "provided identity does not exactly match the ownership catalog",
            ));
        }
        self.ensure_mounted(&entry)?;
        Ok(TrustedWorkflowCwd {
            path: identity.worktree_path.clone(),
            workflow_id: identity.workflow_id.clone(),
        })
    }

    fn verify_owned_current(
        &self,
        workflow_id: &str,
    ) -> Result<(WorkflowWorktreeIdentity, TrustedWorkflowCwd), WorktreeError> {
        validate_workflow_id(workflow_id)?;
        let mut entry = self
            .load_entry(workflow_id)?
            .ok_or_else(|| WorktreeError::NotRegistered(workflow_id.to_owned()))?;
        self.ensure_mounted(&entry)?;
        let identity = entry.identity();
        let git = path_exists(&identity.worktree_path.join(".git"));
        let live_head = if git {
            rev_parse(&identity.worktree_path, "HEAD", self.timeout)?
        } else {
            "none".to_owned()
        };
        if git && live_head != identity.head_commit {
            let ancestry = run_git_allow_fail(
                &identity.worktree_path,
                &["merge-base", "--is-ancestor", &identity.head_commit, &live_head],
                self.timeout,
            )?;
            match ancestry.status.code() {
                Some(0) => {}
                Some(1) => {
                    return Err(WorktreeError::ownership(
                        workflow_id,
                        "live overlay HEAD rewound or diverged from the recorded history",
                    ));
                }
                _ => {
                    return Err(command_failed(
                        &["merge-base", "--is-ancestor", &identity.head_commit, &live_head],
                        &ancestry,
                    ));
                }
            }
        }
        let mut refreshed = entry.clone();
        refreshed.head_commit = live_head;
        let _lock = acquire_repo_lock(&self.catalog_path().parent().unwrap_or(&self.managed_root))?;
        self.with_catalog_mut(|catalog| {
            let current = catalog
                .entries
                .get(workflow_id)
                .ok_or_else(|| WorktreeError::NotRegistered(workflow_id.to_owned()))?;
            if current != &entry {
                return Err(WorktreeError::ownership(
                    workflow_id,
                    "ownership catalog changed during verification",
                ));
            }
            catalog.entries.insert(workflow_id.to_owned(), refreshed.clone());
            Ok(())
        })?;
        Ok((
            refreshed.identity(),
            TrustedWorkflowCwd {
                path: refreshed.worktree_path,
                workflow_id: workflow_id.to_owned(),
            },
        ))
    }

    fn integrate(
        &self,
        workflow_id: &str,
        options: IntegrateOptions,
    ) -> Result<IntegrateOutcome, WorktreeError> {
        validate_workflow_id(workflow_id)?;
        let timeout = options.timeout.unwrap_or(self.timeout);
        let entry = self
            .load_entry(workflow_id)?
            .ok_or_else(|| WorktreeError::NotRegistered(workflow_id.to_owned()))?;
        self.ensure_mounted(&entry)?;
        let identity = entry.identity();
        let merged = &identity.worktree_path;
        let source = &identity.source_root;

        let discovery = match discover_repo(source, timeout) {
            Ok(discovery) => Some(discovery),
            Err(WorktreeError::NotGit) => None,
            Err(error) => return Err(error),
        };
        // Same guard as the worktree backend: never integrate a dirty checkout.
        if discovery.is_some() && is_dirty(merged, timeout)? {
            return Err(WorktreeError::DirtyWorktree(workflow_id.to_owned()));
        }
        if options.require_clean_base && discovery.is_some() && is_dirty(source, timeout)? {
            return Err(WorktreeError::DirtyBase);
        }

        // Copy-back: sync the merged tree over the source working tree,
        // excluding the repo's own `.git` (the workflow's private commit
        // history must never clobber the source's refs/config/objects).
        sync_tree_excluding(merged, source, &[".git"])?;

        let Some(discovery) = discovery else {
            return Ok(IntegrateOutcome::Applied {
                strategy: IntegrateStrategy::Merge,
                result_commit: identity.base_commit.clone(),
                merge_commit: None,
            });
        };
        if !is_dirty(source, timeout)? {
            // Nothing changed: the source is already in the integrated state.
            let result = rev_parse(source, "HEAD", timeout)?;
            return Ok(IntegrateOutcome::Applied {
                strategy: IntegrateStrategy::Merge,
                result_commit: result,
                merge_commit: None,
            });
        }
        run_git(source, &["add", "-A", "--", "."], timeout)?;
        run_git(
            source,
            &[
                "commit",
                "-m",
                &format!("workflow {workflow_id}: integrate overlay changes"),
            ],
            timeout,
        )?;
        let result = rev_parse(source, "HEAD", timeout)?;
        Ok(IntegrateOutcome::Applied {
            strategy: IntegrateStrategy::Merge,
            result_commit: result,
            merge_commit: None,
        })
    }

    fn remove(&self, identity: &WorkflowWorktreeIdentity) -> Result<(), WorktreeError> {
        validate_workflow_id(&identity.workflow_id)?;
        let entry = self
            .load_entry(&identity.workflow_id)?
            .ok_or_else(|| WorktreeError::NotRegistered(identity.workflow_id.clone()))?;
        let recorded = entry.identity();
        if recorded != *identity {
            return Err(WorktreeError::ownership(
                &identity.workflow_id,
                "provided identity does not exactly match the ownership catalog",
            ));
        }
        let identity_dir = identity.worktree_path.parent().ok_or_else(|| {
            WorktreeError::Other(anyhow!("recorded overlay merged view has no parent"))
        })?;
        let managed = canonicalize_no_symlink(&self.managed_root)
            .unwrap_or_else(|_| self.managed_root.clone());
        if !identity_dir.starts_with(&managed) {
            return Err(WorktreeError::ownership(
                &identity.workflow_id,
                "recorded overlay checkout is outside the managed root",
            ));
        }
        // Detach + clean the private layers, then remove the merged view and
        // the identity dir. `stop` refuses to leave a live mount behind.
        let isolation = OverlayfsIsolation::restore(
            &identity.source_root,
            &entry.upper_path(),
            &entry.work_path(),
            &identity.worktree_path,
            entry.backend,
        );
        isolation.stop().map_err(|error| {
            WorktreeError::Other(anyhow!("workflow overlay teardown failed: {error}"))
        })?;
        remove_dir_all(&identity.worktree_path).map_err(|error| {
            WorktreeError::Other(anyhow!(
                "removing workflow overlay merged view: {error}"
            ))
        })?;
        remove_dir_all(identity_dir).map_err(|error| {
            WorktreeError::Other(anyhow!("removing workflow overlay identity dir: {error}"))
        })?;
        let _lock = acquire_repo_lock(&self.catalog_path().parent().unwrap_or(&self.managed_root))?;
        self.with_catalog_mut(|catalog| {
            catalog.entries.remove(&identity.workflow_id);
            Ok(())
        })?;
        Ok(())
    }

    fn inspect(&self, workflow_id: &str) -> Result<WorkflowWorktreeStatus, WorktreeError> {
        validate_workflow_id(workflow_id)?;
        let entry = self
            .load_entry(workflow_id)?
            .ok_or_else(|| WorktreeError::NotRegistered(workflow_id.to_owned()))?;
        self.ensure_mounted(&entry)?;
        let identity = entry.identity();
        let git = path_exists(&identity.worktree_path.join(".git"));
        let dirty = if git {
            is_dirty(&identity.worktree_path, self.timeout)?
        } else {
            false
        };
        let head = if git {
            rev_parse(&identity.worktree_path, "HEAD", self.timeout)?
        } else {
            identity.head_commit.clone()
        };
        let ahead_commits = if git {
            commit_count_ahead(&identity.worktree_path, &identity.base_commit, &head, self.timeout)?
        } else {
            0
        };
        let changed_files = if git {
            list_changed_files(&identity.worktree_path, &identity.base_commit, self.timeout)?
        } else {
            Vec::new()
        };
        Ok(WorkflowWorktreeStatus {
            identity,
            dirty,
            ahead_commits,
            changed_files,
        })
    }

    fn list(&self) -> Result<Vec<WorkflowWorktreeIdentity>, WorktreeError> {
        let catalog = self.load_catalog()?;
        let mut entries: Vec<_> = catalog
            .entries
            .into_values()
            .map(|entry| entry.identity())
            .collect();
        entries.sort_by(|a, b| a.workflow_id.cmp(&b.workflow_id));
        Ok(entries)
    }

    fn prune_stale(&self) -> Result<Vec<String>, WorktreeError> {
        let mut removed = Vec::new();
        self.with_catalog_mut(|catalog| {
            let ids: Vec<String> = catalog.entries.keys().cloned().collect();
            for id in ids {
                let Some(entry) = catalog.entries.get(&id).cloned() else {
                    continue;
                };
                if !path_exists(&entry.worktree_path) {
                    catalog.entries.remove(&id);
                    removed.push(id);
                }
            }
            Ok(())
        })?;
        Ok(removed)
    }

    fn kind(&self) -> WorkflowIsolationSetting {
        WorkflowIsolationSetting::Overlayfs
    }
}

/// Isolation-less backend (`settings.orchestration.isolation: none`):
/// workflows operate directly on the source working tree. Integration is a
/// no-op because the workflow's changes are already in place; removal is a
/// no-op because nothing was created. Multiple concurrent workflows share the
/// source tree — the caller accepts that hazard by selecting `none`.
#[derive(Clone, Debug)]
pub struct NoopWorkflowIsolation {
    source_cwd: PathBuf,
    timeout: Duration,
}

impl NoopWorkflowIsolation {
    pub fn new(source_cwd: impl Into<PathBuf>) -> Self {
        Self {
            source_cwd: source_cwd.into(),
            timeout: DEFAULT_GIT_TIMEOUT,
        }
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Deterministic identity for a workflow id (no persistence needed: every
    /// field derives from the id and the source path).
    fn identity(&self, workflow_id: &str) -> Result<WorkflowWorktreeIdentity, WorktreeError> {
        validate_workflow_id(workflow_id)?;
        let source_root = canonicalize_no_symlink(&self.source_cwd)?;
        Ok(WorkflowWorktreeIdentity {
            workflow_id: workflow_id.to_string(),
            source_root: source_root.clone(),
            common_git_dir: source_root.clone(),
            worktree_path: source_root,
            branch: format!("rpi/none/{workflow_id}"),
            base_commit: "none".to_owned(),
            head_commit: "none".to_owned(),
            created_at_ms: 0,
        })
    }
}

impl WorkflowIsolation for NoopWorkflowIsolation {
    fn create(
        &self,
        workflow_id: &str,
        _options: CreateWorktreeOptions,
    ) -> Result<WorkflowWorktreeIdentity, WorktreeError> {
        self.identity(workflow_id)
    }

    fn verify_owned(
        &self,
        identity: &WorkflowWorktreeIdentity,
    ) -> Result<TrustedWorkflowCwd, WorktreeError> {
        let expected = self.identity(&identity.workflow_id)?;
        if expected != *identity {
            return Err(WorktreeError::ownership(
                &identity.workflow_id,
                "provided identity does not match the no-isolation identity",
            ));
        }
        Ok(TrustedWorkflowCwd {
            path: expected.worktree_path,
            workflow_id: identity.workflow_id.clone(),
        })
    }

    fn verify_owned_current(
        &self,
        workflow_id: &str,
    ) -> Result<(WorkflowWorktreeIdentity, TrustedWorkflowCwd), WorktreeError> {
        let identity = self.identity(workflow_id)?;
        Ok((
            identity.clone(),
            TrustedWorkflowCwd {
                path: identity.worktree_path,
                workflow_id: workflow_id.to_owned(),
            },
        ))
    }

    fn integrate(
        &self,
        workflow_id: &str,
        _options: IntegrateOptions,
    ) -> Result<IntegrateOutcome, WorktreeError> {
        let identity = self.identity(workflow_id)?;
        // No isolation: the workflow's changes are already in the source tree.
        match discover_repo(&identity.source_root, self.timeout) {
            Ok(discovery) => Ok(IntegrateOutcome::Applied {
                strategy: IntegrateStrategy::Merge,
                result_commit: discovery.head_commit,
                merge_commit: None,
            }),
            Err(WorktreeError::NotGit) => Ok(IntegrateOutcome::Applied {
                strategy: IntegrateStrategy::Merge,
                result_commit: "none".to_owned(),
                merge_commit: None,
            }),
            Err(error) => Err(error),
        }
    }

    fn remove(&self, _identity: &WorkflowWorktreeIdentity) -> Result<(), WorktreeError> {
        Ok(())
    }

    fn inspect(&self, workflow_id: &str) -> Result<WorkflowWorktreeStatus, WorktreeError> {
        let identity = self.identity(workflow_id)?;
        let git = discover_repo(&identity.source_root, self.timeout).is_ok();
        let dirty = if git {
            is_dirty(&identity.source_root, self.timeout)?
        } else {
            false
        };
        let changed_files = if dirty {
            list_changed_files(&identity.source_root, "HEAD", self.timeout)?
        } else {
            Vec::new()
        };
        Ok(WorkflowWorktreeStatus {
            identity,
            dirty,
            ahead_commits: 0,
            changed_files,
        })
    }

    fn list(&self) -> Result<Vec<WorkflowWorktreeIdentity>, WorktreeError> {
        Ok(Vec::new())
    }

    fn prune_stale(&self) -> Result<Vec<String>, WorktreeError> {
        Ok(Vec::new())
    }

    fn kind(&self) -> WorkflowIsolationSetting {
        WorkflowIsolationSetting::None
    }
}

/// Sync `src` onto `dst` so `dst` becomes an exact copy of `src`: entries
/// present only in `dst` are removed, entries in `src` overwrite `dst`, and
/// top-level names listed in `excluded` are never touched (the source repo's
/// own `.git`). Symlinks are recreated, never followed.
fn sync_tree_excluding(src: &Path, dst: &Path, excluded: &[&str]) -> Result<(), WorktreeError> {
    for entry in fs::read_dir(dst).map_err(|error| {
        WorktreeError::Other(anyhow!("reading destination directory {}: {error}", dst.display()))
    })? {
        let entry = entry.map_err(|error| WorktreeError::Other(anyhow!("reading directory entry: {error}")))?;
        let name = entry.file_name();
        if excluded.contains(&name.to_string_lossy().as_ref()) {
            continue;
        }
        if !path_exists(&src.join(&name)) {
            remove_path(&entry.path())?;
        }
    }
    for entry in fs::read_dir(src).map_err(|error| {
        WorktreeError::Other(anyhow!("reading source directory {}: {error}", src.display()))
    })? {
        let entry = entry.map_err(|error| WorktreeError::Other(anyhow!("reading directory entry: {error}")))?;
        let name = entry.file_name();
        if excluded.contains(&name.to_string_lossy().as_ref()) {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        let file_type = entry.file_type().map_err(|error| {
            WorktreeError::Other(anyhow!("inspecting {}: {error}", from.display()))
        })?;
        if file_type.is_dir() {
            if !path_exists(&to) {
                fs::create_dir(&to).map_err(|error| {
                    WorktreeError::Other(anyhow!("creating directory {}: {error}", to.display()))
                })?;
            }
            sync_tree_excluding(&from, &to, &[])?;
        } else if file_type.is_symlink() {
            let target = fs::read_link(&from).map_err(|error| {
                WorktreeError::Other(anyhow!("reading symlink {}: {error}", from.display()))
            })?;
            match fs::remove_file(&to) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(WorktreeError::Other(anyhow!(
                        "replacing symlink {}: {error}",
                        to.display()
                    )));
                }
            }
            std::os::unix::fs::symlink(&target, &to).map_err(|error| {
                WorktreeError::Other(anyhow!("creating symlink {}: {error}", to.display()))
            })?;
        } else {
            fs::copy(&from, &to).map_err(|error| {
                WorktreeError::Other(anyhow!("copying {}: {error}", from.display()))
            })?;
        }
    }
    Ok(())
}

fn remove_path(path: &Path) -> Result<(), WorktreeError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
    .map_err(|error| WorktreeError::Other(anyhow!("removing {}: {error}", path.display())))
}

fn remove_dir_all(path: &Path) -> std::io::Result<()> {
    fs::remove_dir_all(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_tree_excluding_mirrors_source_and_preserves_excluded_and_deletes() {
        let sandbox = tempfile::tempdir().expect("sandbox");
        let src = sandbox.path().join("src");
        let dst = sandbox.path().join("dst");
        fs::create_dir_all(src.join("sub")).expect("src sub");
        fs::create_dir_all(&dst).expect("dst");
        fs::write(src.join("keep.txt"), "new").expect("src file");
        fs::write(src.join("sub").join("deep.txt"), "deep").expect("src deep");
        fs::write(dst.join("keep.txt"), "old").expect("dst old");
        fs::write(dst.join("stale.txt"), "stale").expect("dst stale");
        fs::write(dst.join(".git"), "protected").expect("dst git");

        sync_tree_excluding(&src, &dst, &[".git"]).expect("sync");
        assert_eq!(fs::read_to_string(dst.join("keep.txt")).expect("read"), "new");
        assert_eq!(
            fs::read_to_string(dst.join("sub").join("deep.txt")).expect("read"),
            "deep"
        );
        assert!(!dst.join("stale.txt").exists(), "stale entries must be removed");
        assert_eq!(
            fs::read_to_string(dst.join(".git")).expect("read"),
            "protected",
            "excluded top-level entries must survive the sync"
        );
    }

    #[test]
    fn overlay_catalog_round_trips_backends_and_identities() {
        let sandbox = tempfile::tempdir().expect("sandbox");
        let manager = OverlayWorkflowManager::new(
            sandbox.path().join("source"),
            sandbox.path().join("managed"),
        )
        .with_catalog_path(sandbox.path().join("catalog.json"));
        let identity = WorkflowWorktreeIdentity {
            workflow_id: "wf-1".to_owned(),
            source_root: sandbox.path().join("source"),
            common_git_dir: sandbox.path().join("managed"),
            worktree_path: sandbox.path().join("managed").join("merged"),
            branch: format!("{OVERLAY_BRANCH_PREFIX}wf-1"),
            base_commit: "abc".to_owned(),
            head_commit: "abc".to_owned(),
            created_at_ms: 42,
        };
        manager
            .with_catalog_mut(|catalog| {
                catalog.entries.insert(
                    "wf-1".to_owned(),
                    OverlayCatalogEntry::from_identity(&identity, OverlayBackend::Kernel),
                );
                Ok(())
            })
            .expect("persist");
        let reloaded = OverlayWorkflowManager::new(
            sandbox.path().join("source"),
            sandbox.path().join("managed"),
        )
        .with_catalog_path(sandbox.path().join("catalog.json"));
        let entry = reloaded.load_entry("wf-1").expect("load").expect("entry");
        assert_eq!(entry.identity(), identity);
        assert_eq!(entry.backend, OverlayBackend::Kernel);
    }

    #[test]
    fn noop_identity_is_deterministic_and_matches_verify() {
        let sandbox = tempfile::tempdir().expect("sandbox");
        let source = sandbox.path().join("source");
        fs::create_dir_all(&source).expect("source");
        let noop = NoopWorkflowIsolation::new(&source);
        let first = noop.identity("wf-det").expect("identity");
        let second = noop.identity("wf-det").expect("identity");
        assert_eq!(first, second, "noop identities must be fully deterministic");
        assert_eq!(first.worktree_path, source);
        assert_eq!(first.branch, "rpi/none/wf-det");
        assert_eq!(noop.kind(), WorkflowIsolationSetting::None);
        noop.verify_owned(&first).expect("verify matches");
        let (current, cwd) = noop.verify_owned_current("wf-det").expect("verify current");
        assert_eq!(current, first);
        assert_eq!(cwd.path(), &source);
    }
}
