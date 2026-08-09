//! Per-workflow git worktree lifecycle.
//!
//! Owns safe creation, inspection, integration, and removal of isolated git
//! worktrees for concurrent workflows. Every git invocation uses direct argv
//! (no shell). Operations are fail-closed: non-git trees, symlinks, ownership
//! mismatches, dirty integration bases, timeouts, and nonzero exits never
//! degrade into warnings.
//!
//! Worktrees live under a managed root outside the source worktree. Branches
//! use the `rpi/workflow/<workflow-id>` namespace. Removal and prune only touch
//! manager-owned identities recorded at creation time.

mod catalog;
mod git;
pub(crate) mod overlay;

#[cfg(test)]
mod tests;

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::settings::WorkflowIsolationSetting;

use catalog::{acquire_repo_lock, WorktreeCatalog};
use git::{
    allocate_branch_name, branch_exists, canonicalize_no_symlink, canonicalize_or_create_dir,
    command_failed as command_failure, commit_count_ahead, discover_repo, integrate_merge,
    integrate_rebase, is_dirty, list_changed_files, now_ms, path_exists, reject_if_inside,
    resolve_commit, rev_parse, run_git, run_git_allow_fail, sanitize_path_segment,
    validate_workflow_id, verify_identity_ownership, verify_worktree_registration,
    worktree_is_registered,
};
pub use overlay::{
    NoopWorkflowIsolation, OverlayWorkflowManager, OVERLAY_BRANCH_PREFIX, OVERLAY_ROOT_DIR_NAME,
};

/// Branch namespace prefix for every workflow worktree branch.
pub const WORKFLOW_BRANCH_PREFIX: &str = "rpi/workflow/";

/// Directory name under the managed root holding per-workflow worktrees.
pub const WORKTREE_ROOT_DIR_NAME: &str = "workflow-worktrees";

/// Default timeout for a single git argv invocation.
pub const DEFAULT_GIT_TIMEOUT: Duration = Duration::from_secs(60);

/// Typed failures for the worktree lifecycle. Callers should treat every variant
/// as fail-closed — never fall back to the source tree.
#[derive(Error)]
pub enum WorktreeError {
    #[error("source is not inside a git repository")]
    NotGit,
    #[error("integration base is dirty; refusing to proceed")]
    DirtyBase,
    #[error("workflow worktree {0} is dirty; commit or discard its changes before integration")]
    DirtyWorktree(String),
    #[error("refusing to operate through a symbolic link")]
    Symlink,
    #[error("worktree ownership mismatch for workflow {workflow_id}: {detail}")]
    OwnershipMismatch { workflow_id: String, detail: String },
    #[error("git command timed out after {timeout:?}: git {args:?}")]
    Timeout {
        timeout: Duration,
        args: Vec<String>,
    },
    #[error("git command failed (exit {status}): git {args:?}: {stderr}")]
    CommandFailed {
        status: i32,
        args: Vec<String>,
        stderr: String,
    },
    #[error("worktree not registered for workflow {0}")]
    NotRegistered(String),
    #[error("invalid workflow id {0:?}")]
    InvalidWorkflowId(String),
    #[error("worktree operation failed: {0}")]
    Other(#[from] anyhow::Error),
}

impl WorktreeError {
    pub(crate) fn ownership(workflow_id: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::OwnershipMismatch {
            workflow_id: workflow_id.into(),
            detail: detail.into(),
        }
    }
}
impl fmt::Debug for WorktreeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}


/// Durable identity of a manager-owned workflow worktree.
///
/// Absolute repository paths are deliberately opaque and omitted from `Debug`
/// and serialization. Trusted local persistence uses a private catalog record;
/// RPC and human-facing projections use the safe accessors below.
#[derive(Clone, PartialEq, Eq)]
pub struct WorkflowWorktreeIdentity {
    pub(crate) workflow_id: String,
    pub(crate) source_root: PathBuf,
    pub(crate) common_git_dir: PathBuf,
    pub(crate) worktree_path: PathBuf,
    pub(crate) branch: String,
    pub(crate) base_commit: String,
    pub(crate) head_commit: String,
    pub(crate) created_at_ms: u64,
}

impl WorkflowWorktreeIdentity {
    #[must_use]
    pub fn workflow_id(&self) -> &str { &self.workflow_id }
    #[must_use]
    pub fn branch(&self) -> &str { &self.branch }
    #[must_use]
    pub fn base_commit(&self) -> &str { &self.base_commit }
    #[must_use]
    pub fn head_commit(&self) -> &str { &self.head_commit }
    #[must_use]
    pub const fn created_at_ms(&self) -> u64 { self.created_at_ms }
}

impl fmt::Debug for WorkflowWorktreeIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkflowWorktreeIdentity")
            .field("workflow_id", &self.workflow_id)
            .field("branch", &self.branch)
            .field("base_commit", &self.base_commit)
            .field("head_commit", &self.head_commit)
            .field("created_at_ms", &self.created_at_ms)
            .finish_non_exhaustive()
    }
}
/// Opaque, manager-verified capability to enter an owned workflow checkout.
///
/// It cannot be constructed outside this module, does not serialize, and its
/// `Debug` representation never exposes the absolute checkout path.
#[derive(Clone, PartialEq, Eq)]
pub struct TrustedWorkflowCwd {
    path: PathBuf,
    workflow_id: String,
}

impl TrustedWorkflowCwd {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn workflow_id(&self) -> &str {
        &self.workflow_id
    }
}

impl fmt::Debug for TrustedWorkflowCwd {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TrustedWorkflowCwd")
            .field("workflow_id", &self.workflow_id)
            .finish_non_exhaustive()
    }
}


/// Snapshot returned by inspect: identity plus working-tree status.
#[derive(Clone, PartialEq, Eq)]
pub struct WorkflowWorktreeStatus {
    pub identity: WorkflowWorktreeIdentity,
    pub dirty: bool,
    pub ahead_commits: u64,
    pub changed_files: Vec<String>,
}

impl fmt::Debug for WorkflowWorktreeStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkflowWorktreeStatus")
            .field("identity", &self.identity)
            .field("dirty", &self.dirty)
            .field("ahead_commits", &self.ahead_commits)
            .field("changed_files", &self.changed_files)
            .finish()
    }
}

/// How integration merges the workflow branch back into the source base.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum IntegrateStrategy {
    /// Fast-forward when possible; otherwise create a merge commit.
    Merge,
    /// Replay workflow commits onto the current source HEAD.
    Rebase,
}

/// Outcome of an integration attempt. [`IntegrateOutcome::Conflicted`] never
/// destroys the worktree or branch — the caller decides how to resolve.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum IntegrateOutcome {
    #[serde(rename_all = "camelCase")]
    Applied {
        strategy: IntegrateStrategy,
        result_commit: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        merge_commit: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Conflicted {
        strategy: IntegrateStrategy,
        conflicts: Vec<String>,
        workflow_id: String,
        branch: String,
        head_commit: String,
    },
}

/// Options controlling worktree creation.
#[derive(Clone, Debug)]
pub struct CreateWorktreeOptions {
    /// Absolute managed root that will hold `workflow-worktrees/<id>/`.
    /// Must not be inside the source worktree.
    pub managed_root: PathBuf,
    /// Optional base commit (defaults to source HEAD).
    pub base_commit: Option<String>,
    /// Git command timeout override.
    pub timeout: Option<Duration>,
}

/// Options for integration preflight / execution.
#[derive(Clone, Debug)]
pub struct IntegrateOptions {
    pub strategy: IntegrateStrategy,
    /// When true (default), refuse if the source worktree is dirty.
    pub require_clean_base: bool,
    pub timeout: Option<Duration>,
}

impl Default for IntegrateOptions {
    fn default() -> Self {
        Self {
            strategy: IntegrateStrategy::Merge,
            require_clean_base: true,
            timeout: None,
        }
    }
}

/// Canonical repository discovery result for trusted runtime use.
#[derive(Clone, PartialEq, Eq)]
pub struct RepoDiscovery {
    pub(crate) repo_root: PathBuf,
    pub(crate) common_git_dir: PathBuf,
    pub(crate) git_dir: PathBuf,
    pub(crate) head_commit: String,
}

impl fmt::Debug for RepoDiscovery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RepoDiscovery")
            .field("head_commit", &self.head_commit)
            .finish_non_exhaustive()
    }
}

/// Manages per-workflow git worktrees for a single source project.
///
/// Standalone pending Application-owned handoff via the runtime factory.
#[derive(Clone, Debug)]
pub struct WorkflowWorktreeManager {
    /// Trusted source project cwd (must resolve to a git worktree root).
    source_cwd: PathBuf,
    timeout: Duration,
    /// Optional override for catalog path (tests).
    catalog_path_override: Option<PathBuf>,
}

impl WorkflowWorktreeManager {
    /// Build a manager bound to a trusted source project directory.
    pub fn new(source_cwd: impl Into<PathBuf>) -> Self {
        Self {
            source_cwd: source_cwd.into(),
            timeout: DEFAULT_GIT_TIMEOUT,
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

    /// Discover canonical repo root, common git dir, and HEAD commit.
    pub fn discover(&self) -> Result<RepoDiscovery, WorktreeError> {
        let source = canonicalize_no_symlink(&self.source_cwd)?;
        discover_repo(&source, self.timeout)
    }

    /// Stable local namespace for durable workflow state belonging to this repository.
    /// The digest is derived from the canonical common git directory and is never exposed.
    pub fn repository_namespace(&self) -> Result<String, WorktreeError> {
        use sha2::{Digest, Sha256};

        let discovery = self.discover()?;
        let digest = Sha256::digest(discovery.common_git_dir.as_os_str().as_encoded_bytes());
        let mut namespace = String::with_capacity(32);
        for byte in digest.iter().take(16) {
            use std::fmt::Write as _;
            let _ = write!(namespace, "{byte:02x}");
        }
        Ok(namespace)
    }

    /// Per-session namespace for durable workflow state: the repository digest
    /// scoped by the owning session id. A resumed session (same session id)
    /// resolves to the same namespace, so it restores its workflows; every
    /// distinct session id gets its own namespace, so a new session in the
    /// same repository starts with an empty workflow list. Non-git directories
    /// fall back to the session id alone. The session id is encoded
    /// filesystem-safely (separators mapped to `-`), because resumed headers
    /// may originate from foreign session files.
    pub fn session_namespace(&self, session_id: &str) -> String {
        let encoded = encode_session_id(session_id);
        match self.repository_namespace() {
            Ok(repo) => format!("{repo}/{encoded}"),
            Err(_) => encoded,
        }
    }

    /// Create an isolated worktree + branch for `workflow_id`.
    pub fn create(
        &self,
        workflow_id: &str,
        options: CreateWorktreeOptions,
    ) -> Result<WorkflowWorktreeIdentity, WorktreeError> {
        validate_workflow_id(workflow_id)?;
        let timeout = options.timeout.unwrap_or(self.timeout);
        let discovery = self.discover()?;
        let managed_root = canonicalize_or_create_dir(&options.managed_root)?;
        reject_if_inside(&managed_root, &discovery.repo_root).map_err(|detail| {
            WorktreeError::Other(anyhow!(
                "managed root must not be inside the source worktree: {detail}"
            ))
        })?;

        let _lock = acquire_repo_lock(&discovery.common_git_dir)?;

        let base_commit = match &options.base_commit {
            Some(c) => resolve_commit(&discovery.repo_root, c, timeout)?,
            None => discovery.head_commit.clone(),
        };

        let branch = allocate_branch_name(&discovery.repo_root, workflow_id, timeout)?;
        let worktree_path = managed_root
            .join(WORKTREE_ROOT_DIR_NAME)
            .join(sanitize_path_segment(workflow_id));

        if worktree_path.exists() {
            return Err(WorktreeError::Other(anyhow!(
                "managed worktree location already exists for workflow {workflow_id}"
            )));
        }
        if let Some(parent) = worktree_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                WorktreeError::Other(anyhow!(
                    "creating managed worktree parent for workflow {workflow_id}: {e}"
                ))
            })?;
        }

        run_git(
            &discovery.repo_root,
            &["branch", "--", &branch, &base_commit],
            timeout,
        )?;

        let wt_arg = worktree_path.to_string_lossy().into_owned();
        let create_result = run_git(
            &discovery.repo_root,
            &["worktree", "add", "--", &wt_arg, &branch],
            timeout,
        );

        if let Err(error) = create_result {
            if branch_exists(&discovery.repo_root, &branch, timeout)? {
                run_git(
                    &discovery.repo_root,
                    &["branch", "-D", "--", &branch],
                    timeout,
                )?;
            }
            return Err(error);
        }

        let worktree_path = canonicalize_no_symlink(&worktree_path)?;
        verify_worktree_registration(
            &discovery.repo_root,
            &worktree_path,
            &branch,
            &base_commit,
            timeout,
        )?;

        let head_commit = rev_parse(&worktree_path, "HEAD", timeout)?;
        let identity = WorkflowWorktreeIdentity {
            workflow_id: workflow_id.to_string(),
            source_root: discovery.repo_root.clone(),
            common_git_dir: discovery.common_git_dir.clone(),
            worktree_path,
            branch,
            base_commit,
            head_commit,
            created_at_ms: now_ms(),
        };

        self.with_catalog_mut(&discovery, |catalog| {
            catalog
                .entries
                .insert(workflow_id.to_string(), identity.clone());
            Ok(())
        })?;

        Ok(identity)
    }

    /// Inspect a registered worktree: dirty state, ahead commits, changed files.
    pub fn inspect(&self, workflow_id: &str) -> Result<WorkflowWorktreeStatus, WorktreeError> {
        validate_workflow_id(workflow_id)?;
        let discovery = self.discover()?;
        let _lock = acquire_repo_lock(&discovery.common_git_dir)?;
        let identity = self
            .load_identity(&discovery, workflow_id)?
            .ok_or_else(|| WorktreeError::NotRegistered(workflow_id.to_string()))?;
        verify_identity_ownership(&identity, &discovery, self.timeout)?;

        let dirty = is_dirty(&identity.worktree_path, self.timeout)?;
        let head = rev_parse(&identity.worktree_path, "HEAD", self.timeout)?;
        let ahead_commits = commit_count_ahead(
            &identity.worktree_path,
            &identity.base_commit,
            &head,
            self.timeout,
        )?;
        let changed_files =
            list_changed_files(&identity.worktree_path, &identity.base_commit, self.timeout)?;

        let mut identity = identity;
        identity.head_commit = head;

        Ok(WorkflowWorktreeStatus {
            identity,
            dirty,
            ahead_commits,
            changed_files,
        })
    }

    /// Integrate workflow changes into the source worktree via the chosen strategy.
    ///
    /// On conflict returns [`IntegrateOutcome::Conflicted`] and leaves the
    /// worktree + branch intact. Never deletes manager-owned resources here.
    pub fn integrate(
        &self,
        workflow_id: &str,
        options: IntegrateOptions,
    ) -> Result<IntegrateOutcome, WorktreeError> {
        validate_workflow_id(workflow_id)?;
        let timeout = options.timeout.unwrap_or(self.timeout);
        let discovery = self.discover()?;
        let _lock = acquire_repo_lock(&discovery.common_git_dir)?;
        let identity = self
            .load_identity(&discovery, workflow_id)?
            .ok_or_else(|| WorktreeError::NotRegistered(workflow_id.to_string()))?;
        verify_identity_ownership(&identity, &discovery, self.timeout)?;

        if is_dirty(&identity.worktree_path, timeout)? {
            return Err(WorktreeError::DirtyWorktree(workflow_id.to_owned()));
        }

        if options.require_clean_base && is_dirty(&discovery.repo_root, timeout)? {
            return Err(WorktreeError::DirtyBase);
        }

        let head = rev_parse(&identity.worktree_path, "HEAD", timeout)?;
        if head == rev_parse(&discovery.repo_root, "HEAD", timeout)? {
            return Ok(IntegrateOutcome::Applied {
                strategy: options.strategy,
                result_commit: head,
                merge_commit: None,
            });
        }

        match options.strategy {
            IntegrateStrategy::Merge => integrate_merge(&discovery, &identity, &head, timeout),
            IntegrateStrategy::Rebase => integrate_rebase(&discovery, &identity, &head, timeout),
        }
    }

    /// Verify exact catalog ownership and live git registration, then mint an
    /// opaque capability for trusted runtime use of the workflow checkout.
    pub fn verify_owned(
        &self,
        identity: &WorkflowWorktreeIdentity,
    ) -> Result<TrustedWorkflowCwd, WorktreeError> {
        validate_workflow_id(&identity.workflow_id)?;
        let discovery = self.discover()?;
        let _lock = acquire_repo_lock(&discovery.common_git_dir)?;
        let recorded = self
            .load_identity(&discovery, &identity.workflow_id)?
            .ok_or_else(|| WorktreeError::NotRegistered(identity.workflow_id.clone()))?;
        if recorded != *identity {
            return Err(WorktreeError::ownership(
                &identity.workflow_id,
                "provided identity does not exactly match the ownership catalog",
            ));
        }
        let recorded_common = canonicalize_no_symlink(&identity.common_git_dir).map_err(|_| {
            WorktreeError::ownership(&identity.workflow_id, "repository identity is unavailable")
        })?;
        if recorded_common != discovery.common_git_dir {
            return Err(WorktreeError::ownership(
                &identity.workflow_id,
                "recorded repository identity does not match manager repository",
            ));
        }
        verify_identity_ownership(identity, &discovery, self.timeout)?;
        let live_head = rev_parse(&identity.worktree_path, "HEAD", self.timeout)?;
        if live_head != identity.head_commit {
            return Err(WorktreeError::ownership(
                &identity.workflow_id,
                "live worktree HEAD does not match the recorded identity",
            ));
        }
        Ok(TrustedWorkflowCwd {
            path: identity.worktree_path.clone(),
            workflow_id: identity.workflow_id.clone(),
        })
    }
    /// Refresh an owned worktree after committed workflow progress and mint a
    /// trusted checkout capability.
    ///
    /// The catalog identity, repository, registered path, and branch must still
    /// match exactly. The live HEAD may only remain equal to or advance from the
    /// recorded HEAD; rewinds and divergent histories fail closed. The updated
    /// identity is persisted atomically before the capability is returned.
    pub fn verify_owned_current(
        &self,
        workflow_id: &str,
    ) -> Result<(WorkflowWorktreeIdentity, TrustedWorkflowCwd), WorktreeError> {
        validate_workflow_id(workflow_id)?;
        let discovery = self.discover()?;
        let _lock = acquire_repo_lock(&discovery.common_git_dir)?;
        let recorded = self
            .load_identity(&discovery, workflow_id)?
            .ok_or_else(|| WorktreeError::NotRegistered(workflow_id.to_owned()))?;

        let recorded_common = canonicalize_no_symlink(&recorded.common_git_dir).map_err(|_| {
            WorktreeError::ownership(workflow_id, "repository identity is unavailable")
        })?;
        if recorded_common != discovery.common_git_dir {
            return Err(WorktreeError::ownership(
                workflow_id,
                "recorded repository identity does not match manager repository",
            ));
        }
        verify_identity_ownership(&recorded, &discovery, self.timeout)?;

        let live_head = rev_parse(&recorded.worktree_path, "HEAD", self.timeout)?;
        verify_worktree_registration(
            &discovery.repo_root,
            &recorded.worktree_path,
            &recorded.branch,
            &live_head,
            self.timeout,
        )
        .map_err(|_| {
            WorktreeError::ownership(
                workflow_id,
                "live git registration changed during ownership verification",
            )
        })?;

        if live_head != recorded.head_commit {
            let ancestry = run_git_allow_fail(
                &recorded.worktree_path,
                &["merge-base", "--is-ancestor", &recorded.head_commit, &live_head],
                self.timeout,
            )?;
            match ancestry.status.code() {
                Some(0) => {}
                Some(1) => {
                    return Err(WorktreeError::ownership(
                        workflow_id,
                        "live worktree HEAD rewound or diverged from the recorded history",
                    ));
                }
                _ => {
                    return Err(command_failure(
                        &["merge-base", "--is-ancestor", &recorded.head_commit, &live_head],
                        &ancestry,
                    ));
                }
            }
        }

        let mut refreshed = recorded.clone();
        refreshed.head_commit = live_head;
        self.with_catalog_mut(&discovery, |catalog| {
            let current = catalog
                .entries
                .get(workflow_id)
                .ok_or_else(|| WorktreeError::NotRegistered(workflow_id.to_owned()))?;
            if current != &recorded {
                return Err(WorktreeError::ownership(
                    workflow_id,
                    "ownership catalog changed during verification",
                ));
            }
            catalog
                .entries
                .insert(workflow_id.to_owned(), refreshed.clone());
            Ok(())
        })?;

        let capability = TrustedWorkflowCwd {
            path: refreshed.worktree_path.clone(),
            workflow_id: refreshed.workflow_id.clone(),
        };
        Ok((refreshed, capability))
    }


    /// Remove a manager-owned worktree and its branch. Foreign identities fail closed.
    pub fn remove(&self, identity: &WorkflowWorktreeIdentity) -> Result<(), WorktreeError> {
        validate_workflow_id(&identity.workflow_id)?;
        let discovery = self.discover()?;
        let _lock = acquire_repo_lock(&discovery.common_git_dir)?;

        let recorded_source = canonicalize_no_symlink(&identity.source_root)
            .unwrap_or_else(|_| identity.source_root.clone());
        if recorded_source != discovery.repo_root {
            return Err(WorktreeError::ownership(
                &identity.workflow_id,
                "recorded source does not match manager source",
            ));
        }

        let recorded_common = canonicalize_no_symlink(&identity.common_git_dir)
            .unwrap_or_else(|_| identity.common_git_dir.clone());
        if recorded_common != discovery.common_git_dir {
            return Err(WorktreeError::ownership(
                &identity.workflow_id,
                "recorded repository identity does not match manager repository",
            ));
        }

        let recorded = self
            .load_identity(&discovery, &identity.workflow_id)?
            .ok_or_else(|| WorktreeError::NotRegistered(identity.workflow_id.clone()))?;
        if recorded != *identity {
            return Err(WorktreeError::ownership(
                &identity.workflow_id,
                "provided identity does not exactly match the ownership catalog",
            ));
        }
        verify_identity_ownership(identity, &discovery, self.timeout)?;

        let wt = identity.worktree_path.to_string_lossy().into_owned();
        let registered = worktree_is_registered(
            &discovery.repo_root,
            &identity.worktree_path,
            self.timeout,
        )?;
        if path_exists(&identity.worktree_path) && !registered {
            return Err(WorktreeError::ownership(
                &identity.workflow_id,
                "owned path exists but is not the recorded git worktree",
            ));
        }
        if registered {
            run_git(
                &discovery.repo_root,
                &["worktree", "remove", "--force", "--", &wt],
                self.timeout,
            )?;
        }

        if branch_exists(&discovery.repo_root, &identity.branch, self.timeout)? {
            run_git(
                &discovery.repo_root,
                &["branch", "-D", "--", &identity.branch],
                self.timeout,
            )?;
        }

        self.with_catalog_mut(&discovery, |catalog| {
            catalog.entries.remove(&identity.workflow_id);
            Ok(())
        })?;

        run_git(&discovery.repo_root, &["worktree", "prune"], self.timeout)?;
        Ok(())
    }

    /// Drop catalog entries whose worktree/branch no longer exist.
    /// Never touches live foreign worktrees.
    pub fn prune_stale(&self) -> Result<Vec<String>, WorktreeError> {
        let discovery = self.discover()?;
        let _lock = acquire_repo_lock(&discovery.common_git_dir)?;
        let mut removed = Vec::new();

        self.with_catalog_mut(&discovery, |catalog| {
            let ids: Vec<String> = catalog.entries.keys().cloned().collect();
            for id in ids {
                let Some(entry) = catalog.entries.get(&id).cloned() else {
                    continue;
                };
                if !entry.branch.starts_with(WORKFLOW_BRANCH_PREFIX) {
                    catalog.entries.remove(&id);
                    removed.push(id);
                    continue;
                }
                let path_gone = !path_exists(&entry.worktree_path);
                let registered = worktree_is_registered(
                    &discovery.repo_root,
                    &entry.worktree_path,
                    self.timeout,
                )?;
                let branch_alive =
                    branch_exists(&discovery.repo_root, &entry.branch, self.timeout)?;
                if path_gone && !registered && !branch_alive {
                    catalog.entries.remove(&id);
                    removed.push(id);
                }
            }
            Ok(())
        })?;

        Ok(removed)
    }

    /// List identities currently recorded in the catalog.
    pub fn list(&self) -> Result<Vec<WorkflowWorktreeIdentity>, WorktreeError> {
        let discovery = self.discover()?;
        let _lock = acquire_repo_lock(&discovery.common_git_dir)?;
        let catalog = self.load_catalog(&discovery)?;
        let mut entries: Vec<_> = catalog.entries.into_values().collect();
        entries.sort_by(|a, b| a.workflow_id.cmp(&b.workflow_id));
        Ok(entries)
    }


    fn catalog_path(&self, discovery: &RepoDiscovery) -> PathBuf {
        catalog::catalog_path(self.catalog_path_override.as_deref(), discovery)
    }

    fn load_catalog(&self, discovery: &RepoDiscovery) -> Result<WorktreeCatalog, WorktreeError> {
        catalog::load_catalog(&self.catalog_path(discovery))
    }

    fn load_identity(
        &self,
        discovery: &RepoDiscovery,
        workflow_id: &str,
    ) -> Result<Option<WorkflowWorktreeIdentity>, WorktreeError> {
        let cat = self.load_catalog(discovery)?;
        Ok(cat.entries.get(workflow_id).cloned())
    }

    fn with_catalog_mut<F>(&self, discovery: &RepoDiscovery, f: F) -> Result<(), WorktreeError>
    where
        F: FnOnce(&mut WorktreeCatalog) -> Result<(), WorktreeError>,
    {
        let path = self.catalog_path(discovery);
        catalog::with_catalog_mut(&path, f)
    }
}

/// Isolation backend behind workflow working copies.
///
/// The git worktree backend ([`WorkflowWorktreeManager`]) is the default;
/// [`OverlayWorkflowManager`] materializes each workflow in an overlayfs
/// (source repo as the read-only lower layer) with the same lifecycle, and
/// [`NoopWorkflowIsolation`] disables isolation entirely. The workflow runtime
/// factory owns an `Arc<dyn WorkflowIsolation>` selected from
/// `settings.orchestration.isolation`, so every backend shares the same
/// create/verify/integrate/remove contract.
pub trait WorkflowIsolation: Send + Sync {
    /// Create an isolated working copy for `workflow_id`.
    fn create(
        &self,
        workflow_id: &str,
        options: CreateWorktreeOptions,
    ) -> Result<WorkflowWorktreeIdentity, WorktreeError>;

    /// Verify exact ownership of `identity` and mint an opaque capability for
    /// trusted runtime use of the checkout.
    fn verify_owned(
        &self,
        identity: &WorkflowWorktreeIdentity,
    ) -> Result<TrustedWorkflowCwd, WorktreeError>;

    /// Refresh an owned checkout after committed workflow progress and mint a
    /// trusted checkout capability.
    fn verify_owned_current(
        &self,
        workflow_id: &str,
    ) -> Result<(WorkflowWorktreeIdentity, TrustedWorkflowCwd), WorktreeError>;

    /// Integrate workflow changes back into the source.
    fn integrate(
        &self,
        workflow_id: &str,
        options: IntegrateOptions,
    ) -> Result<IntegrateOutcome, WorktreeError>;

    /// Remove a manager-owned checkout. Foreign identities fail closed.
    fn remove(&self, identity: &WorkflowWorktreeIdentity) -> Result<(), WorktreeError>;

    /// Inspect a registered checkout: dirty state, ahead commits, changed files.
    fn inspect(&self, workflow_id: &str) -> Result<WorkflowWorktreeStatus, WorktreeError>;

    /// List identities currently recorded in the catalog.
    fn list(&self) -> Result<Vec<WorkflowWorktreeIdentity>, WorktreeError>;

    /// Drop catalog entries whose checkout no longer exists.
    fn prune_stale(&self) -> Result<Vec<String>, WorktreeError>;

    /// The isolation kind this backend implements.
    #[must_use]
    fn kind(&self) -> WorkflowIsolationSetting;
}

impl WorkflowIsolation for WorkflowWorktreeManager {
    fn create(
        &self,
        workflow_id: &str,
        options: CreateWorktreeOptions,
    ) -> Result<WorkflowWorktreeIdentity, WorktreeError> {
        self.create(workflow_id, options)
    }

    fn verify_owned(
        &self,
        identity: &WorkflowWorktreeIdentity,
    ) -> Result<TrustedWorkflowCwd, WorktreeError> {
        self.verify_owned(identity)
    }

    fn verify_owned_current(
        &self,
        workflow_id: &str,
    ) -> Result<(WorkflowWorktreeIdentity, TrustedWorkflowCwd), WorktreeError> {
        self.verify_owned_current(workflow_id)
    }

    fn integrate(
        &self,
        workflow_id: &str,
        options: IntegrateOptions,
    ) -> Result<IntegrateOutcome, WorktreeError> {
        self.integrate(workflow_id, options)
    }

    fn remove(&self, identity: &WorkflowWorktreeIdentity) -> Result<(), WorktreeError> {
        self.remove(identity)
    }

    fn inspect(&self, workflow_id: &str) -> Result<WorkflowWorktreeStatus, WorktreeError> {
        self.inspect(workflow_id)
    }

    fn list(&self) -> Result<Vec<WorkflowWorktreeIdentity>, WorktreeError> {
        self.list()
    }

    fn prune_stale(&self) -> Result<Vec<String>, WorktreeError> {
        self.prune_stale()
    }

    fn kind(&self) -> WorkflowIsolationSetting {
        WorkflowIsolationSetting::Worktree
    }
}

/// Filesystem-safe, collision-free encoding of a session id into a single
/// namespace path segment. Native ids without separators (UUIDs) pass through
/// unchanged. Ids containing path separators map `/\:` to `-` and append a
/// short digest of the raw id, so distinct ids can never collapse onto the
/// same segment (`proj/abc` and `proj-abc` both map to `proj-abc` today).
fn encode_session_id(session_id: &str) -> String {
    if session_id.is_empty() {
        return "session".to_owned();
    }
    if !session_id.contains(['/', '\\', ':']) {
        return session_id.to_owned();
    }
    let encoded = session_id.replace(['/', '\\', ':'], "-");
    // 3 digest bytes (6 hex chars) disambiguate the mapped form; the digest
    // is derived from the raw id, never from the lossy mapped form.
    use sha2::{Digest, Sha256};
    let mut disambiguator = String::with_capacity(6);
    for byte in Sha256::digest(session_id.as_bytes()).iter().take(3) {
        use std::fmt::Write as _;
        let _ = write!(disambiguator, "{byte:02x}");
    }
    format!("{encoded}-{disambiguator}")
}
