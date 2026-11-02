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
mod rebind_tests;
#[cfg(test)]
mod tests;

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::fs::{
    CapabilityAccess, CapabilityFs, CapabilityRootLease, CapabilityRootPurpose, EntryKind,
    FileIdentity, FileMode, FsError, Replacement, RootCreation,
};
use crate::settings::WorkflowIsolationSetting;

use catalog::{
    WorktreeCatalog, acquire_repo_lock, parse_catalog_bytes, read_catalog_bytes, with_catalog_mut,
};
use overlay::copy_tree_governed;
use git::{
    allocate_branch_name, branch_exists, command_failed as command_failure, commit_count_ahead,
    discover_repo, first_descendant_of, integrate_merge, integrate_rebase, is_ancestor, is_dirty,
    list_changed_files, now_ms, reject_if_inside, resolve_commit, rev_parse, run_git,
    run_git_allow_fail, run_git_capture, sanitize_path_segment, validate_workflow_id,
    verify_identity_ownership, verify_worktree_registration, worktree_is_registered,
};
pub use overlay::{
    NoopWorkflowIsolation, OVERLAY_BRANCH_PREFIX, OVERLAY_ROOT_DIR_NAME, OverlayWorkflowManager,
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
    /// The source repository has no commits yet. A git worktree cannot be
    /// created without a base commit, so the workflow must fail before the
    /// runtime factory can mask the underlying git error. The fixed message
    /// is actionable and never carries a path or secret.
    #[error(
        "source repository has no commits; create at least one commit before starting a workflow"
    )]
    UnbornHead,
    #[error("integration base is dirty; refusing to proceed")]
    DirtyBase,
    #[error("workflow worktree {0} is dirty; commit or discard its changes before integration")]
    DirtyWorktree(String),
    /// The workflow branch has no commits ahead of the integration base.
    /// Uncommitted files are not integrated work: the caller must commit them
    /// before integration. The fixed message is actionable and never carries a
    /// path or secret.
    #[error(
        "workflow has no commits to integrate; commit the worktree changes before integration"
    )]
    MissingCommits,
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
    #[error("workflow overlay storage requires retained descriptor-relative mount authority")]
    OverlayStorageUnavailable,
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
    pub fn workflow_id(&self) -> &str {
        &self.workflow_id
    }
    #[must_use]
    pub fn branch(&self) -> &str {
        &self.branch
    }
    #[must_use]
    pub fn base_commit(&self) -> &str {
        &self.base_commit
    }
    #[must_use]
    pub fn head_commit(&self) -> &str {
        &self.head_commit
    }
    #[must_use]
    pub const fn created_at_ms(&self) -> u64 {
        self.created_at_ms
    }
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
#[derive(Clone)]
pub struct WorkflowWorktreeManager {
    source_root: CapabilityRootLease,
    managed_root: CapabilityRootLease,
    timeout: Duration,
    process: Option<crate::GovernedProcessContext>,
}

impl WorkflowWorktreeManager {
    pub(crate) fn new(source_root: CapabilityRootLease, managed_root: CapabilityRootLease) -> Self {
        debug_assert_eq!(
            source_root.purpose(),
            crate::fs::CapabilityRootPurpose::ProjectResources
        );
        debug_assert!(matches!(
            source_root.access(),
            crate::fs::CapabilityAccess::ReadWrite | crate::fs::CapabilityAccess::PrivateReadWrite
        ));
        debug_assert_eq!(
            managed_root.purpose(),
            crate::fs::CapabilityRootPurpose::WorkflowWorktrees
        );
        Self {
            source_root,
            managed_root,
            timeout: DEFAULT_GIT_TIMEOUT,
            process: None,
        }
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
    pub fn with_process_context(mut self, process: crate::GovernedProcessContext) -> Self {
        #[cfg(unix)]
        let process = process
            .with_repository_authorities(self.source_root.clone(), self.managed_root.clone());
        self.process = Some(process);
        self
    }
    pub fn process_context(&self) -> Result<&crate::GovernedProcessContext, WorktreeError> {
        self.process()
    }

    pub fn discover(&self) -> Result<RepoDiscovery, WorktreeError> {
        discover_repo(
            self.process()?,
            self.source_root.display_root(),
            self.timeout,
        )
        .map_err(|error| self.classify_unborn_head(error))
    }

    pub fn repository_namespace(&self) -> Result<String, WorktreeError> {
        repository_namespace_with(
            self.process()?,
            self.source_root.display_root(),
            self.timeout,
        )
    }

    pub fn session_namespace(&self, session_id: &str) -> String {
        session_namespace_with(
            self.process.as_ref(),
            self.source_root.display_root(),
            session_id,
        )
    }

    /// Map a discovery failure onto the actionable, path/secret-free
    /// [`WorktreeError::UnbornHead`] category ONLY when independently verified:
    /// the repository is valid (`discover_repo` already resolved the toplevel
    /// and git-dir before reaching HEAD) AND there are no commits reachable
    /// from ANY ref (no `refs/heads/*`, `refs/tags/*`, `refs/remotes/*`, ...). A
    /// bare `git rev-parse --verify HEAD` failure is NOT sufficient — a broken
    /// HEAD symbolic ref, a tag-only repo with no local branches, a detached
    /// HEAD repointed at a missing ref, a corrupt object store, or a permission
    /// error all share the same argv — so every other HEAD failure stays
    /// [`WorktreeError::CommandFailed`] for the generic fail-closed factory
    /// masking. Never mutate the repository; the probe is read-only.
    fn classify_unborn_head(&self, error: WorktreeError) -> WorktreeError {
        match &error {
            WorktreeError::CommandFailed { args, .. }
                if args.len() == 3
                    && args[0] == "rev-parse"
                    && args[1] == "--verify"
                    && args[2] == "HEAD"
                    && self.repository_has_no_commits() =>
            {
                WorktreeError::UnbornHead
            }
            _ => error,
        }
    }

    /// Independently verify the repository has no commits reachable from any ref.
    /// An unborn repository has NO refs of any kind (no heads, tags, remotes,
    /// ...); any committed repository — including a tag-only repo with no local
    /// branches, or a committed repo with a broken/detached HEAD — has at least
    /// one ref. Any failure to run the read-only probe is treated as "has
    /// commits" so the caller keeps the original [`WorktreeError::CommandFailed`]
    /// (fail-closed: never claim `UnbornHead` without proof).
    fn repository_has_no_commits(&self) -> bool {
        let process = match self.process() {
            Ok(process) => process,
            Err(_) => return false,
        };
        // `for-each-ref` with no pattern lists EVERY ref (heads, tags, remotes,
        // ...); an unborn repo has none. `--count=1` bounds the output so the
        // probe stays cheap on large histories. Any non-zero exit (broken repo,
        // permission, timeout) yields `Err` and is treated as "has commits".
        matches!(
            run_git_capture(
                process,
                self.source_root.display_root(),
                &["for-each-ref", "--count=1"],
                self.timeout,
            ),
            Ok(ref output) if output.trim().is_empty()
        )
    }

    pub fn create(
        &self,
        workflow_id: &str,
        options: CreateWorktreeOptions,
    ) -> Result<WorkflowWorktreeIdentity, WorktreeError> {
        validate_workflow_id(workflow_id)?;
        let process = self.process()?;
        let timeout = options.timeout.unwrap_or(self.timeout);
        let discovery = self.discover()?;
        self.require_managed_root(&options.managed_root, workflow_id)?;
        reject_if_inside(self.managed_root.display_root(), &discovery.repo_root)
            .map_err(|detail| WorktreeError::ownership(workflow_id, detail))?;
        let _lock = acquire_repo_lock(&self.managed_root)?;
        if self.load_identity(workflow_id)?.is_some() {
            return Err(WorktreeError::ownership(
                workflow_id,
                "workflow already has a recorded worktree",
            ));
        }

        let base_commit = options.base_commit.as_deref().map_or_else(
            || Ok(discovery.head_commit.clone()),
            |revision| resolve_commit(process, &discovery.repo_root, revision, timeout),
        )?;
        let branch = allocate_branch_name(process, &discovery.repo_root, workflow_id, timeout)?;
        let relative =
            PathBuf::from(WORKTREE_ROOT_DIR_NAME).join(sanitize_path_segment(workflow_id));
        self.managed_root
            .create_dir_all(Path::new(WORKTREE_ROOT_DIR_NAME), FileMode::Private)
            .map_err(capability_error)?;
        if self.entry_exists(&relative)? {
            return Err(WorktreeError::ownership(
                workflow_id,
                "managed worktree path already exists",
            ));
        }
        let worktree_path = self.managed_root.display_root().join(&relative);
        let path_argument = worktree_path.to_string_lossy().into_owned();
        let args = [
            "worktree",
            "add",
            "-b",
            branch.as_str(),
            "--",
            path_argument.as_str(),
            base_commit.as_str(),
        ];
        if let Err(error) = run_git(process, &discovery.repo_root, &args, timeout) {
            let _ = run_git(
                process,
                &discovery.repo_root,
                &[
                    "worktree",
                    "remove",
                    "--force",
                    "--",
                    path_argument.as_str(),
                ],
                timeout,
            );
            let _ = run_git(
                process,
                &discovery.repo_root,
                &["branch", "-D", "--", branch.as_str()],
                timeout,
            );
            if let Ok(metadata) = self.managed_root.metadata(&relative)
                && metadata.identity.kind() == EntryKind::Directory
            {
                let _ = self
                    .managed_root
                    .remove_tree_expected(&relative, metadata.identity);
            }
            return Err(error);
        }

        let directory_identity = self.managed_directory_identity(&relative, workflow_id)?;
        let result = (|| {
            let head_commit = rev_parse(process, &worktree_path, "HEAD", timeout)?;
            verify_worktree_registration(
                process,
                &discovery.repo_root,
                &worktree_path,
                &branch,
                &head_commit,
                timeout,
            )?;
            if self.managed_directory_identity(&relative, workflow_id)? != directory_identity {
                return Err(WorktreeError::ownership(
                    workflow_id,
                    "managed worktree identity changed during creation",
                ));
            }
            let identity = WorkflowWorktreeIdentity {
                workflow_id: workflow_id.to_owned(),
                source_root: discovery.repo_root.clone(),
                common_git_dir: discovery.common_git_dir.clone(),
                worktree_path,
                branch: branch.clone(),
                base_commit,
                head_commit,
                created_at_ms: now_ms(),
            };
            self.with_catalog_mut(|catalog| {
                if catalog.entries.contains_key(workflow_id) {
                    return Err(WorktreeError::ownership(
                        workflow_id,
                        "workflow ownership appeared during creation",
                    ));
                }
                catalog
                    .entries
                    .insert(workflow_id.to_owned(), identity.clone());
                Ok(())
            })?;
            Ok(identity)
        })();
        if result.is_err() {
            let _ = run_git(
                process,
                &discovery.repo_root,
                &[
                    "worktree",
                    "remove",
                    "--force",
                    "--",
                    path_argument.as_str(),
                ],
                timeout,
            );
            let _ = run_git(
                process,
                &discovery.repo_root,
                &["branch", "-D", "--", branch.as_str()],
                timeout,
            );
            if self
                .managed_root
                .metadata(&relative)
                .is_ok_and(|metadata| metadata.identity == directory_identity)
            {
                let _ = self
                    .managed_root
                    .remove_tree_expected(&relative, directory_identity);
            }
        }
        result
    }

    pub fn inspect(&self, workflow_id: &str) -> Result<WorkflowWorktreeStatus, WorktreeError> {
        validate_workflow_id(workflow_id)?;
        let process = self.process()?;
        let discovery = self.discover()?;
        let _lock = acquire_repo_lock(&self.managed_root)?;
        let identity = self
            .load_identity(workflow_id)?
            .ok_or_else(|| WorktreeError::NotRegistered(workflow_id.to_owned()))?;
        let directory_identity = self.verify_managed_identity(&identity)?;
        verify_identity_ownership(process, &identity, &discovery, self.timeout)?;
        let dirty = is_dirty(process, &identity.worktree_path, self.timeout)?;
        let head = rev_parse(process, &identity.worktree_path, "HEAD", self.timeout)?;
        let ahead_commits = commit_count_ahead(
            process,
            &identity.worktree_path,
            &identity.base_commit,
            &head,
            self.timeout,
        )?;
        let changed_files = list_changed_files(
            process,
            &identity.worktree_path,
            &identity.base_commit,
            self.timeout,
        )?;
        self.verify_managed_identity_exact(&identity, directory_identity)?;
        let mut identity = identity;
        identity.head_commit = head;
        Ok(WorkflowWorktreeStatus {
            identity,
            dirty,
            ahead_commits,
            changed_files,
        })
    }

    pub fn integrate(
        &self,
        workflow_id: &str,
        options: IntegrateOptions,
    ) -> Result<IntegrateOutcome, WorktreeError> {
        validate_workflow_id(workflow_id)?;
        let process = self.process()?;
        let timeout = options.timeout.unwrap_or(self.timeout);
        let discovery = self.discover()?;
        let _lock = acquire_repo_lock(&self.managed_root)?;
        let identity = self
            .load_identity(workflow_id)?
            .ok_or_else(|| WorktreeError::NotRegistered(workflow_id.to_owned()))?;
        let directory_identity = self.verify_managed_identity(&identity)?;
        verify_identity_ownership(process, &identity, &discovery, self.timeout)?;
        let head = rev_parse(process, &identity.worktree_path, "HEAD", timeout)?;
        let ahead_commits = commit_count_ahead(
            process,
            &identity.worktree_path,
            &identity.base_commit,
            &head,
            timeout,
        )?;
        let dirty_worktree = is_dirty(process, &identity.worktree_path, timeout)?;
        if ahead_commits == 0 {
            // Branch HEAD equals the creation base: zero new commits is not
            // applied work. Uncommitted files in the worktree are also not
            // integrated work. Callers must see an explicit failure, never a
            // silent Applied no-op that the manager would publish as Completed.
            return Err(WorktreeError::MissingCommits);
        }
        self.verify_managed_identity_exact(&identity, directory_identity)?;
        let source_head = rev_parse(process, &discovery.repo_root, "HEAD", timeout)?;
        if is_ancestor(process, &discovery.repo_root, &head, &source_head, timeout)? {
            // Already merged or fast-forwarded. For --no-ff, the recorded
            // Applied commit is the merge descendant of the branch HEAD, not
            // the branch HEAD itself. Recover that identity so retry stays
            // stable even after later source commits.
            let result_commit = first_descendant_of(
                process,
                &discovery.repo_root,
                &head,
                &source_head,
                timeout,
            )?
            .unwrap_or(head);
            return Ok(IntegrateOutcome::Applied {
                strategy: options.strategy,
                result_commit,
                merge_commit: None,
            });
        }
        if dirty_worktree {
            return Err(WorktreeError::DirtyWorktree(workflow_id.to_owned()));
        }
        if options.require_clean_base && is_dirty(process, &discovery.repo_root, timeout)? {
            return Err(WorktreeError::DirtyBase);
        }
        let outcome = match options.strategy {
            IntegrateStrategy::Merge => {
                integrate_merge(process, &discovery, &identity, &head, timeout)
            }
            IntegrateStrategy::Rebase => {
                integrate_rebase(process, &discovery, &identity, &head, timeout)
            }
        }?;
        self.verify_managed_identity_exact(&identity, directory_identity)?;
        if matches!(outcome, IntegrateOutcome::Applied { .. }) {
            let live_head = rev_parse(process, &identity.worktree_path, "HEAD", timeout)?;
            if live_head != identity.head_commit {
                self.with_catalog_mut(|catalog| {
                    let current = catalog
                        .entries
                        .get(workflow_id)
                        .ok_or_else(|| WorktreeError::NotRegistered(workflow_id.to_owned()))?;
                    if current != &identity {
                        return Err(WorktreeError::ownership(
                            workflow_id,
                            "ownership catalog changed during integration",
                        ));
                    }
                    let mut refreshed = identity.clone();
                    refreshed.head_commit = live_head;
                    catalog.entries.insert(workflow_id.to_owned(), refreshed);
                    Ok(())
                })?;
            }
        }
        Ok(outcome)
    }

    pub fn verify_owned(
        &self,
        identity: &WorkflowWorktreeIdentity,
    ) -> Result<TrustedWorkflowCwd, WorktreeError> {
        validate_workflow_id(&identity.workflow_id)?;
        let process = self.process()?;
        let discovery = self.discover()?;
        let _lock = acquire_repo_lock(&self.managed_root)?;
        let recorded = self
            .load_identity(&identity.workflow_id)?
            .ok_or_else(|| WorktreeError::NotRegistered(identity.workflow_id.clone()))?;
        if recorded != *identity {
            return Err(WorktreeError::ownership(
                &identity.workflow_id,
                "provided identity does not exactly match the ownership catalog",
            ));
        }
        let directory_identity = self.verify_managed_identity(identity)?;
        verify_identity_ownership(process, identity, &discovery, self.timeout)?;
        if rev_parse(process, &identity.worktree_path, "HEAD", self.timeout)?
            != identity.head_commit
        {
            return Err(WorktreeError::ownership(
                &identity.workflow_id,
                "live worktree HEAD does not match the recorded identity",
            ));
        }
        self.verify_managed_identity_exact(identity, directory_identity)?;
        Ok(TrustedWorkflowCwd {
            path: identity.worktree_path.clone(),
            workflow_id: identity.workflow_id.clone(),
        })
    }

    pub fn verify_owned_current(
        &self,
        workflow_id: &str,
    ) -> Result<(WorkflowWorktreeIdentity, TrustedWorkflowCwd), WorktreeError> {
        validate_workflow_id(workflow_id)?;
        let process = self.process()?;
        let discovery = self.discover()?;
        let _lock = acquire_repo_lock(&self.managed_root)?;
        let recorded = self
            .load_identity(workflow_id)?
            .ok_or_else(|| WorktreeError::NotRegistered(workflow_id.to_owned()))?;
        let directory_identity = self.verify_managed_identity(&recorded)?;
        verify_identity_ownership(process, &recorded, &discovery, self.timeout)?;
        let live_head = rev_parse(process, &recorded.worktree_path, "HEAD", self.timeout)?;
        verify_worktree_registration(
            process,
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
                process,
                &recorded.worktree_path,
                &[
                    "merge-base",
                    "--is-ancestor",
                    &recorded.head_commit,
                    &live_head,
                ],
                self.timeout,
            )?;
            match ancestry.status {
                Some(0) => {}
                Some(1) => {
                    return Err(WorktreeError::ownership(
                        workflow_id,
                        "live worktree HEAD rewound or diverged from the recorded history",
                    ));
                }
                _ => {
                    return Err(command_failure(
                        &[
                            "merge-base",
                            "--is-ancestor",
                            &recorded.head_commit,
                            &live_head,
                        ],
                        &ancestry,
                    ));
                }
            }
        }
        self.verify_managed_identity_exact(&recorded, directory_identity)?;
        let mut refreshed = recorded.clone();
        refreshed.head_commit = live_head;
        self.with_catalog_mut(|catalog| {
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
        self.verify_managed_identity_exact(&refreshed, directory_identity)?;
        let cwd = TrustedWorkflowCwd {
            path: refreshed.worktree_path.clone(),
            workflow_id: refreshed.workflow_id.clone(),
        };
        Ok((refreshed, cwd))
    }

    pub fn remove(&self, identity: &WorkflowWorktreeIdentity) -> Result<(), WorktreeError> {
        validate_workflow_id(&identity.workflow_id)?;
        let process = self.process()?;
        let discovery = self.discover()?;
        let _lock = acquire_repo_lock(&self.managed_root)?;
        let recorded = self
            .load_identity(&identity.workflow_id)?
            .ok_or_else(|| WorktreeError::NotRegistered(identity.workflow_id.clone()))?;
        if recorded != *identity {
            return Err(WorktreeError::ownership(
                &identity.workflow_id,
                "provided identity does not exactly match the ownership catalog",
            ));
        }
        let relative = self.worktree_relative(&identity.workflow_id)?;
        let directory_identity = self.verify_managed_identity(identity)?;
        verify_identity_ownership(process, identity, &discovery, self.timeout)?;
        if self.load_identity(&identity.workflow_id)?.as_ref() != Some(identity) {
            return Err(WorktreeError::ownership(
                &identity.workflow_id,
                "ownership catalog changed during removal",
            ));
        }
        let path_argument = identity.worktree_path.to_string_lossy().into_owned();
        run_git(
            process,
            &discovery.repo_root,
            &[
                "worktree",
                "remove",
                "--force",
                "--",
                path_argument.as_str(),
            ],
            self.timeout,
        )?;
        match self.managed_root.metadata(&relative) {
            Ok(metadata) => {
                if metadata.identity != directory_identity {
                    return Err(WorktreeError::ownership(
                        &identity.workflow_id,
                        "managed worktree identity changed during removal",
                    ));
                }
                self.managed_root
                    .remove_tree_expected(&relative, directory_identity)
                    .map_err(capability_error)?;
            }
            Err(error) if is_missing_capability(&error) => {}
            Err(error) => return Err(capability_error(error)),
        }
        if branch_exists(
            process,
            &discovery.repo_root,
            &identity.branch,
            self.timeout,
        )? {
            run_git(
                process,
                &discovery.repo_root,
                &["branch", "-D", "--", identity.branch.as_str()],
                self.timeout,
            )?;
        }
        self.with_catalog_mut(|catalog| {
            let current = catalog
                .entries
                .get(&identity.workflow_id)
                .ok_or_else(|| WorktreeError::NotRegistered(identity.workflow_id.clone()))?;
            if current != identity {
                return Err(WorktreeError::ownership(
                    &identity.workflow_id,
                    "ownership catalog changed during removal",
                ));
            }
            catalog.entries.remove(&identity.workflow_id);
            Ok(())
        })
    }

    pub fn prune_stale(&self) -> Result<Vec<String>, WorktreeError> {
        let process = self.process()?;
        let discovery = self.discover()?;
        let _lock = acquire_repo_lock(&self.managed_root)?;
        let mut removed = Vec::new();
        self.with_catalog_mut(|catalog| {
            for id in catalog.entries.keys().cloned().collect::<Vec<_>>() {
                let Some(entry) = catalog.entries.get(&id).cloned() else {
                    continue;
                };
                let relative = match self.worktree_relative(&id) {
                    Ok(relative) => relative,
                    Err(_) => continue,
                };
                let path_alive = match self.managed_root.metadata(&relative) {
                    Ok(metadata) if metadata.identity.kind() == EntryKind::Directory => true,
                    Ok(_) => continue,
                    Err(error) if is_missing_capability(&error) => false,
                    Err(error) => return Err(capability_error(error)),
                };
                let registered = worktree_is_registered(
                    process,
                    &discovery.repo_root,
                    &entry.worktree_path,
                    self.timeout,
                )?;
                let branch_alive =
                    branch_exists(process, &discovery.repo_root, &entry.branch, self.timeout)?;
                if !path_alive && !registered && !branch_alive {
                    catalog.entries.remove(&id);
                    removed.push(id);
                }
            }
            Ok(())
        })?;
        Ok(removed)
    }

    pub fn list(&self) -> Result<Vec<WorkflowWorktreeIdentity>, WorktreeError> {
        let _lock = acquire_repo_lock(&self.managed_root)?;
        let mut entries = self
            .load_catalog()?
            .entries
            .into_values()
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.workflow_id.cmp(&right.workflow_id));
        Ok(entries)
    }
    fn require_managed_root(
        &self,
        requested: &Path,
        _workflow_id: &str,
    ) -> Result<(), WorktreeError> {
        if requested == self.managed_root.display_root() {
            Ok(())
        } else {
            Err(WorktreeError::Symlink)
        }
    }

    fn worktree_relative(&self, workflow_id: &str) -> Result<PathBuf, WorktreeError> {
        validate_workflow_id(workflow_id)?;
        Ok(PathBuf::from(WORKTREE_ROOT_DIR_NAME).join(sanitize_path_segment(workflow_id)))
    }

    fn managed_directory_identity(
        &self,
        relative: &Path,
        workflow_id: &str,
    ) -> Result<crate::fs::FileIdentity, WorktreeError> {
        let metadata = self.managed_root.metadata(relative).map_err(|_| {
            WorktreeError::ownership(
                workflow_id,
                "managed worktree is missing or its retained identity cannot be verified",
            )
        })?;
        if metadata.identity.kind() != EntryKind::Directory {
            return Err(WorktreeError::ownership(
                workflow_id,
                "managed worktree is not a directory",
            ));
        }
        Ok(metadata.identity)
    }

    fn verify_managed_identity(
        &self,
        identity: &WorkflowWorktreeIdentity,
    ) -> Result<crate::fs::FileIdentity, WorktreeError> {
        let relative = self.worktree_relative(&identity.workflow_id)?;
        if identity.worktree_path != self.managed_root.display_root().join(&relative) {
            return Err(WorktreeError::ownership(
                &identity.workflow_id,
                "recorded worktree path is outside the exact retained managed root",
            ));
        }
        self.managed_directory_identity(&relative, &identity.workflow_id)
    }

    fn verify_managed_identity_exact(
        &self,
        identity: &WorkflowWorktreeIdentity,
        expected: crate::fs::FileIdentity,
    ) -> Result<(), WorktreeError> {
        let relative = self.worktree_relative(&identity.workflow_id)?;
        if self.managed_directory_identity(&relative, &identity.workflow_id)? != expected {
            return Err(WorktreeError::ownership(
                &identity.workflow_id,
                "managed worktree identity changed during verification",
            ));
        }
        Ok(())
    }

    fn entry_exists(&self, relative: &Path) -> Result<bool, WorktreeError> {
        match self.managed_root.metadata(relative) {
            Ok(_) => Ok(true),
            Err(error) if is_missing_capability(&error) => Ok(false),
            Err(error) => Err(capability_error(error)),
        }
    }

    fn process(&self) -> Result<&crate::GovernedProcessContext, WorktreeError> {
        self.process.as_ref().ok_or_else(|| {
            WorktreeError::Other(anyhow!(
                "workflow repository process authority is unavailable"
            ))
        })
    }

    fn load_catalog(&self) -> Result<WorktreeCatalog, WorktreeError> {
        catalog::load_catalog(&self.managed_root)
    }

    fn load_identity(
        &self,
        workflow_id: &str,
    ) -> Result<Option<WorkflowWorktreeIdentity>, WorktreeError> {
        Ok(self.load_catalog()?.entries.get(workflow_id).cloned())
    }

    fn with_catalog_mut<F>(&self, mutate: F) -> Result<(), WorktreeError>
    where
        F: FnOnce(&mut WorktreeCatalog) -> Result<(), WorktreeError>,
    {
        catalog::with_catalog_mut(&self.managed_root, mutate)
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

pub fn repository_namespace(
    process: &crate::GovernedProcessContext,
    source_cwd: &Path,
) -> Result<String, WorktreeError> {
    repository_namespace_with(process, source_cwd, DEFAULT_GIT_TIMEOUT)
}

pub fn session_namespace(
    process: Option<&crate::GovernedProcessContext>,
    source_cwd: &Path,
    session_id: &str,
) -> String {
    session_namespace_with(process, source_cwd, session_id)
}

fn repository_namespace_with(
    process: &crate::GovernedProcessContext,
    source_cwd: &Path,
    timeout: Duration,
) -> Result<String, WorktreeError> {
    use sha2::{Digest, Sha256};
    let discovery = discover_repo(process, source_cwd, timeout)?;
    let digest = Sha256::digest(discovery.common_git_dir.as_os_str().as_encoded_bytes());
    let mut namespace = String::with_capacity(32);
    for byte in digest.iter().take(16) {
        use std::fmt::Write as _;
        let _ = write!(namespace, "{byte:02x}");
    }
    Ok(namespace)
}

fn session_namespace_with(
    process: Option<&crate::GovernedProcessContext>,
    source_cwd: &Path,
    session_id: &str,
) -> String {
    let encoded = encode_session_id(session_id);
    let repository = process
        .and_then(|process| repository_namespace(process, source_cwd).ok())
        .unwrap_or_else(|| cwd_namespace_digest(source_cwd));
    format!("{repository}/{encoded}")
}

/// Digest of the literal cwd bytes that keys the durable session-scoped
/// workflow layout when no governed repository context is available (see
/// [`session_namespace_with`]). Sixteen hex characters of SHA-256.
fn cwd_namespace_digest(cwd: &Path) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(cwd.as_os_str().as_encoded_bytes());
    let mut namespace = String::with_capacity(32);
    for byte in digest.iter().take(16) {
        use std::fmt::Write as _;
        let _ = write!(namespace, "{byte:02x}");
    }
    namespace
}

fn is_missing_capability(error: &FsError) -> bool {
    matches!(
        error,
        FsError::System {
            kind: std::io::ErrorKind::NotFound,
            ..
        }
    )
}

fn capability_error(error: FsError) -> WorktreeError {
    WorktreeError::Other(anyhow!("workflow worktree capability failed: {error}"))
}
impl fmt::Debug for WorkflowWorktreeManager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkflowWorktreeManager")
            .finish_non_exhaustive()
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

// ---------------------------------------------------------------------------
// Session worktree root rebind
//
// Relocates a session's workflow storage roots when its checkout moves from an
// old repository path to a new one. The durable layout is
// `agent_dir/workflows/<cwd-digest>/<session>` and
// `agent_dir/workflow-worktrees/<cwd-digest>/<session>` (see
// `workflow_storage_roots` in pi-cli), so a moved checkout orphans both roots
// under the old digest. The rebind is two-phase:
//
// * [`prepare_session_worktree_root_rebind`] validates the source, catalog,
//   and managed roots, copies both per-session directories to the new
//   namespace through the governed streaming copier, and rewrites the
//   relocated worktrees' `.git` pointers. The ownership catalog (the
//   authority) is not rewritten yet and the old roots stay untouched.
// * [`PreparedWorktreeRootRebind::commit`] rewrites the relocated catalog's
//   `sourceRoot`/`commonGitDir`/`worktreePath` fields through the exact
//   `StoredWorktreeIdentity` round-trip (no workflow id, branch, commit, or
//   timestamp is ever regenerated) and removes the old roots only after the
//   destination copy and the rewritten catalog are durable.
//
// The operation is rollbackable until the first old-root removal, idempotent
// (a completed rebind re-resolves to a no-op), and collision fail-closed: any
// ambiguous pre-existing target aborts without touching the old namespace.
// ---------------------------------------------------------------------------

/// Directory name under the agent root holding per-session workflow stores.
///
/// Mirrors the durable `agent_dir/workflows/<namespace>` layout selected by
/// `workflow_storage_roots` in pi-cli, so the rebind relocates it alongside
/// the managed worktree root and both halves of a session's workflow state
/// move together.
pub const WORKFLOW_STORE_ROOT_DIR_NAME: &str = "workflows";

/// Upper bound for reading a worktree `.git` pointer file during relocation.
const GIT_POINTER_MAX_BYTES: u64 = 4096;

/// Absolute-path prefix rewrites applied during a root rebind.
struct WorktreePathRewrites {
    old_cwd: PathBuf,
    new_cwd: PathBuf,
    old_managed_root: PathBuf,
    new_managed_root: PathBuf,
}

impl fmt::Debug for WorktreePathRewrites {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Absolute repository paths stay opaque, matching identity hygiene.
        formatter.debug_struct("WorktreePathRewrites").finish()
    }
}

impl WorktreePathRewrites {
    fn rewrite(&self, path: &Path, old_prefix: &Path, new_prefix: &Path) -> Option<PathBuf> {
        let rest = path.strip_prefix(old_prefix).ok()?;
        // `join("")` would append a trailing separator when the path equals
        // the old prefix exactly; the rewritten root must stay clean.
        if rest.as_os_str().is_empty() {
            Some(new_prefix.to_path_buf())
        } else {
            Some(new_prefix.join(rest))
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ManagedPlan {
    /// No managed root exists on either side.
    None,
    /// Fresh relocation: prepare copies the managed root; commit rewrites the
    /// catalog and removes the old root.
    Copy,
    /// A previous commit already rewrote the target catalog; only the old
    /// roots still need removal.
    RemoveOldOnly,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StorePlan {
    /// No store root work remains on either side.
    None,
    /// Fresh relocation: prepare copies the store; commit removes the old one.
    Copy,
    /// The store was already relocated by a previous prepare; only the old one
    /// needs removal.
    RemoveOld,
}

#[derive(PartialEq, Eq)]
enum ManagedDisposition {
    /// Target is an interrupted staging copy: clear it and copy fresh.
    Leftover,
    /// Target catalog is already fully rewritten: keep it, remove old roots.
    Rewritten,
}

/// Normalized cwd used for namespace digesting: the canonical path when the
/// directory exists (matching `std::env::current_dir`-derived sessions),
/// otherwise the lexically cleaned literal path (the checkout has moved away
/// and the recorded bytes are the only remaining identity).
fn rebind_namespace_cwd(cwd: &Path) -> PathBuf {
    std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.components().collect::<PathBuf>())
}

/// Relative store/managed root paths for one session namespace digest.
fn namespace_rels(digest: &str, session_segment: &str) -> (PathBuf, PathBuf) {
    (
        PathBuf::from(WORKFLOW_STORE_ROOT_DIR_NAME)
            .join(digest)
            .join(session_segment),
        PathBuf::from(WORKTREE_ROOT_DIR_NAME)
            .join(digest)
            .join(session_segment),
    )
}

/// Presence of a real directory under the agent root. Anything else (missing
/// parent, file, link-backed entry) fails closed.
fn probe_managed_directory(
    agent_root: &CapabilityRootLease,
    relative: &Path,
) -> Result<Option<FileIdentity>, WorktreeError> {
    match agent_root.metadata(relative) {
        Ok(metadata) if metadata.identity.kind() == EntryKind::Directory => {
            Ok(Some(metadata.identity))
        }
        Ok(_) => Err(WorktreeError::Other(anyhow!(
            "worktree root rebind requires a real directory; refusing through a non-directory entry"
        ))),
        Err(error) if is_missing_capability(&error) => Ok(None),
        Err(error) => Err(capability_error(error)),
    }
}

fn catalog_is_rewritten(
    catalog: &WorktreeCatalog,
    rewrites: &WorktreePathRewrites,
) -> bool {
    catalog.entries.values().all(|entry| {
        !entry.worktree_path.starts_with(&rewrites.old_managed_root)
            && !entry.source_root.starts_with(&rewrites.old_cwd)
            && !entry.common_git_dir.starts_with(&rewrites.old_cwd)
    })
}

/// Fail-closed catalog validation: every recorded worktree must live inside
/// the session's old or new managed root, or the entry does not belong to
/// this session's namespace and the rebind refuses to guess.
fn validate_relocatable_catalog(
    catalog: &WorktreeCatalog,
    rewrites: &WorktreePathRewrites,
) -> Result<(), WorktreeError> {
    for (workflow_id, entry) in &catalog.entries {
        if !entry.worktree_path.starts_with(&rewrites.old_managed_root)
            && !entry.worktree_path.starts_with(&rewrites.new_managed_root)
        {
            return Err(WorktreeError::ownership(
                workflow_id.clone(),
                "recorded worktree path is outside the session's managed roots",
            ));
        }
    }
    Ok(())
}

/// Rewrite one catalog entry's absolute paths onto the new namespace. All
/// other `StoredWorktreeIdentity` fields pass through untouched.
fn rewrite_identity_paths(
    entry: &mut WorkflowWorktreeIdentity,
    workflow_id: &str,
    rewrites: &WorktreePathRewrites,
) -> Result<(), WorktreeError> {
    if let Some(rewritten) = rewrites.rewrite(&entry.source_root, &rewrites.old_cwd, &rewrites.new_cwd)
    {
        entry.source_root = rewritten;
    }
    if let Some(rewritten) =
        rewrites.rewrite(&entry.common_git_dir, &rewrites.old_cwd, &rewrites.new_cwd)
    {
        entry.common_git_dir = rewritten;
    }
    if let Some(rewritten) = rewrites.rewrite(
        &entry.worktree_path,
        &rewrites.old_managed_root,
        &rewrites.new_managed_root,
    ) {
        entry.worktree_path = rewritten;
    } else if !entry.worktree_path.starts_with(&rewrites.new_managed_root) {
        return Err(WorktreeError::ownership(
            workflow_id.to_owned(),
            "recorded worktree path is outside the session's managed roots",
        ));
    }
    Ok(())
}

/// Classify a `(old managed root, target managed root)` pair that exists
/// simultaneously. The target can only be (a) an interrupted staging copy
/// whose catalog is byte-identical to the old one, or (b) a fully rewritten
/// copy whose old half is a stale leftover. Anything else is foreign content
/// and fails closed.
fn classify_managed_pair(
    old_lease: &CapabilityRootLease,
    new_lease: &CapabilityRootLease,
    rewrites: &WorktreePathRewrites,
) -> Result<ManagedDisposition, WorktreeError> {
    let old_bytes = read_catalog_bytes(old_lease)?;
    let new_bytes = read_catalog_bytes(new_lease)?;
    match (&old_bytes, &new_bytes) {
        (None, None) => Ok(ManagedDisposition::Leftover),
        (Some(old), Some(new)) if old == new => Ok(ManagedDisposition::Leftover),
        (_, Some(new)) => {
            let parsed = parse_catalog_bytes(new)?;
            if catalog_is_rewritten(&parsed, rewrites) {
                Ok(ManagedDisposition::Rewritten)
            } else {
                Err(WorktreeError::Other(anyhow!(
                    "worktree root rebind target namespace already exists with foreign content; refusing ambiguous relocation"
                )))
            }
        }
        (Some(_), None) => Err(WorktreeError::Other(anyhow!(
            "worktree root rebind target namespace already exists without the session catalog; refusing ambiguous relocation"
        ))),
    }
}

/// Rewrite each relocated worktree's `.git` pointer onto the moved checkout.
/// The pointer file lives inside the manager-owned copy, so the rewrite is
/// deterministic (prefix replacement, never a regenerated value) and keeps
/// `git worktree repair` able to reconnect the relocated worktree.
fn rewrite_worktree_git_pointers(
    new_managed: &CapabilityRootLease,
    rewrites: &WorktreePathRewrites,
) -> Result<(), WorktreeError> {
    let worktrees = Path::new(WORKTREE_ROOT_DIR_NAME);
    for name in new_managed
        .read_dir_names_strict(worktrees)
        .map_err(capability_error)?
    {
        let git_pointer = worktrees.join(&name).join(".git");
        let metadata = match new_managed.metadata(&git_pointer) {
            Ok(metadata) => metadata,
            Err(error) if is_missing_capability(&error) => continue,
            Err(error) => return Err(capability_error(error)),
        };
        if metadata.identity.kind() != EntryKind::RegularFile {
            return Err(WorktreeError::Other(anyhow!(
                "relocated worktree .git entry is not a regular file"
            )));
        }
        let bytes = new_managed
            .read_bounded(&git_pointer, GIT_POINTER_MAX_BYTES)
            .map_err(capability_error)?;
        let text = std::str::from_utf8(&bytes).map_err(|_| {
            WorktreeError::Other(anyhow!("relocated worktree .git file is not UTF-8"))
        })?;
        let target = text
            .trim_end_matches(['\n', '\r'])
            .strip_prefix("gitdir: ")
            .ok_or_else(|| {
                WorktreeError::Other(anyhow!(
                    "relocated worktree .git file does not carry a gitdir pointer"
                ))
            })?;
        let Some(rest) = Path::new(target).strip_prefix(&rewrites.old_cwd).ok() else {
            continue;
        };
        let rewritten = format!("gitdir: {}\n", rewrites.new_cwd.join(rest).display());
        new_managed
            .replace_file(Replacement {
                relative: git_pointer,
                expected: Some(metadata.identity),
                bytes: rewritten.into_bytes(),
                mode: FileMode::Private,
            })
            .map_err(capability_error)?;
    }
    Ok(())
}

fn remove_relocated_source(
    agent_root: &CapabilityRootLease,
    relative: &Path,
    expected: FileIdentity,
) -> Result<(), WorktreeError> {
    match agent_root.metadata(relative) {
        Ok(_) => agent_root
            .remove_tree_expected(relative, expected)
            .map_err(capability_error),
        Err(error) if is_missing_capability(&error) => Ok(()),
        Err(error) => Err(capability_error(error)),
    }
}

/// Prepared, two-phase relocation of one session's workflow storage roots.
///
/// Created by [`prepare_session_worktree_root_rebind`]; terminal via
/// [`PreparedWorktreeRootRebind::commit`]. Dropping the handle without
/// committing only releases the held repository lock; use
/// [`PreparedWorktreeRootRebind::rollback`] to undo a prepare that has not
/// yet removed any old root.
pub struct PreparedWorktreeRootRebind {
    agent_root: CapabilityRootLease,
    // Serializes against workflow managers still operating on the old root.
    _lock: Option<catalog::RepoLock>,
    old_store_rel: PathBuf,
    new_store_rel: PathBuf,
    old_managed_rel: PathBuf,
    new_managed_rel: PathBuf,
    rewrites: WorktreePathRewrites,
    managed_plan: ManagedPlan,
    store_plan: StorePlan,
    old_managed_identity: Option<FileIdentity>,
    old_store_identity: Option<FileIdentity>,
    new_managed_identity: Option<FileIdentity>,
    new_store_identity: Option<FileIdentity>,
    copied_managed: bool,
    copied_store: bool,
    old_removed: bool,
}

impl PreparedWorktreeRootRebind {
    /// Completes the rebind: rewrites the relocated catalog's paths, then
    /// removes the old roots. Removal happens strictly after the destination
    /// copy (durable from prepare) and the rewritten catalog (durable via the
    /// atomic catalog write) exist, so every crash window re-classifies as a
    /// resumable state on the next prepare instead of destroying the only
    /// complete copy.
    pub fn commit(mut self) -> Result<(), WorktreeError> {
        if self.managed_plan != ManagedPlan::None {
            let new_managed = self
                .agent_root
                .lease_directory(
                    &self.new_managed_rel,
                    CapabilityAccess::PrivateReadWrite,
                    RootCreation::Existing,
                )
                .map_err(capability_error)?;
            if self.managed_plan == ManagedPlan::Copy
                && read_catalog_bytes(&new_managed)?.is_some()
            {
                with_catalog_mut(&new_managed, |catalog| {
                    for (workflow_id, entry) in catalog.entries.iter_mut() {
                        rewrite_identity_paths(entry, workflow_id, &self.rewrites)?;
                    }
                    Ok(())
                })?;
            }
        }
        // Any old-root removal attempt (successful or not) forbids rollback:
        // the relocated namespace is the only complete copy from here on.
        self.old_removed = true;
        if self.managed_plan != ManagedPlan::None {
            if let Some(identity) = self.old_managed_identity {
                remove_relocated_source(&self.agent_root, &self.old_managed_rel, identity)?;
            }
        }
        if matches!(self.store_plan, StorePlan::Copy | StorePlan::RemoveOld) {
            if let Some(identity) = self.old_store_identity {
                remove_relocated_source(&self.agent_root, &self.old_store_rel, identity)?;
            }
        }
        Ok(())
    }

    /// Undoes a prepare that has not yet removed any old root: the staged
    /// copies are removed and the old namespace stays authoritative. Refuses
    /// once commit has touched an old root — at that point the relocated
    /// namespace is the only complete copy and the correct recovery is
    /// another prepare + commit.
    pub fn rollback(mut self) -> Result<(), WorktreeError> {
        if self.old_removed {
            return Err(WorktreeError::Other(anyhow!(
                "worktree root rebind already removed the old namespace; complete with commit instead of rolling back"
            )));
        }
        if self.copied_store {
            if let Some(identity) = self.new_store_identity {
                self.agent_root
                    .remove_tree_expected(&self.new_store_rel, identity)
                    .map_err(capability_error)?;
            }
        }
        if self.copied_managed {
            if let Some(identity) = self.new_managed_identity {
                self.agent_root
                    .remove_tree_expected(&self.new_managed_rel, identity)
                    .map_err(capability_error)?;
            }
        }
        Ok(())
    }
}

/// Prepares the relocation of a session's workflow storage roots from the old
/// checkout namespace to the new one.
///
/// * `agent_dir` — absolute agent root holding `workflows/` and
///   `workflow-worktrees/`; must be byte-identical to the agent dir the
///   session ran with.
/// * `old_cwd` — the session's original checkout path as recorded (the digest
///   keys on these bytes; a canonical fallback is probed when the directory
///   still exists).
/// * `new_cwd` — the checkout's new location.
/// * `session_id` — the moved session's id.
///
/// Returns `Ok(None)` when there is nothing to rebind: no old roots, an
/// already-completed rebind, or an identical old/new checkout. Returns a
/// handle whose [`PreparedWorktreeRootRebind::commit`] finishes the move and
/// whose [`PreparedWorktreeRootRebind::rollback`] undoes it while every old
/// root is still in place.
pub fn prepare_session_worktree_root_rebind(
    agent_dir: &Path,
    old_cwd: &Path,
    new_cwd: &Path,
    session_id: &str,
) -> Result<Option<PreparedWorktreeRootRebind>, WorktreeError> {
    if !agent_dir.is_absolute() || !old_cwd.is_absolute() || !new_cwd.is_absolute() {
        return Err(WorktreeError::Other(anyhow!(
            "worktree root rebind requires absolute agent directory, old cwd, and new cwd paths"
        )));
    }
    if session_id.is_empty() {
        return Err(WorktreeError::Other(anyhow!(
            "worktree root rebind requires a non-empty session id"
        )));
    }
    let capabilities = CapabilityFs::new().map_err(|error| {
        WorktreeError::Other(anyhow!(
            "worktree root rebind capability setup failed: {error}"
        ))
    })?;
    let agent_absolute = pi_sandbox::AbsolutePath::parse(agent_dir.to_path_buf()).map_err(|_| {
        WorktreeError::Other(anyhow!(
            "worktree root rebind agent directory is not a valid absolute path"
        ))
    })?;
    let agent_root = match capabilities.open_root(
        CapabilityRootPurpose::AgentStore,
        agent_absolute,
        CapabilityAccess::PrivateReadWrite,
        RootCreation::Existing,
    ) {
        Ok(lease) => lease,
        Err(error) if is_missing_capability(&error) => return Ok(None),
        Err(error) => return Err(capability_error(error)),
    };
    let session_segment = encode_session_id(session_id);

    // The durable layout digests the cwd bytes the runtime was launched with.
    let old_literal = old_cwd.components().collect::<PathBuf>();
    let canonical_old = std::fs::canonicalize(old_cwd).ok();
    let mut old_candidates = vec![old_literal.clone()];
    if canonical_old.as_ref().is_some_and(|canonical| *canonical != old_literal) {
        old_candidates.push(canonical_old.clone().expect("canonical old cwd"));
    }
    let canonical_new = rebind_namespace_cwd(new_cwd);
    if old_candidates.iter().any(|candidate| *candidate == canonical_new) {
        return Ok(None); // same checkout location: nothing to rebind
    }
    let new_digest = cwd_namespace_digest(&canonical_new);
    let (new_store_rel, new_managed_rel) = namespace_rels(&new_digest, &session_segment);
    let mut new_store_identity =
        probe_managed_directory(&agent_root, &new_store_rel)?;
    let mut new_managed_identity =
        probe_managed_directory(&agent_root, &new_managed_rel)?;

    // Resolve the old namespace: prefer the literal recorded bytes, then the
    // canonical form when the old checkout still exists.
    let mut resolved: Option<(PathBuf, PathBuf, PathBuf, Option<FileIdentity>, Option<FileIdentity>)> =
        None;
    for candidate in std::iter::once(old_literal).chain(canonical_old) {
        let digest = cwd_namespace_digest(&candidate);
        let (old_store_rel, old_managed_rel) = namespace_rels(&digest, &session_segment);
        let old_store_identity = probe_managed_directory(&agent_root, &old_store_rel)?;
        let old_managed_identity = probe_managed_directory(&agent_root, &old_managed_rel)?;
        if old_store_identity.is_some() || old_managed_identity.is_some() {
            resolved = Some((candidate, old_store_rel, old_managed_rel, old_store_identity, old_managed_identity));
            break;
        }
    }
    let Some((old_input, old_store_rel, old_managed_rel, old_store_identity, old_managed_identity)) =
        resolved
    else {
        return Ok(None); // no old roots: nothing to rebind
    };
    let rewrites = WorktreePathRewrites {
        old_cwd: old_input,
        new_cwd: canonical_new,
        old_managed_root: agent_root.display_root().join(&old_managed_rel),
        new_managed_root: agent_root.display_root().join(&new_managed_rel),
    };

    // Serialize against workflow managers still working the old root.
    let lock = match old_managed_identity {
        Some(_) => {
            let old_managed = agent_root
                .lease_directory(
                    &old_managed_rel,
                    CapabilityAccess::PrivateReadWrite,
                    RootCreation::Existing,
                )
                .map_err(capability_error)?;
            Some(acquire_repo_lock(&old_managed)?)
        }
        None => None,
    };

    let managed_plan = match (old_managed_identity, new_managed_identity) {
        (None, None) => ManagedPlan::None,
        (Some(_), None) => ManagedPlan::Copy,
        (Some(_), Some(_)) => {
            let old_managed = agent_root
                .lease_directory(
                    &old_managed_rel,
                    CapabilityAccess::PrivateReadWrite,
                    RootCreation::Existing,
                )
                .map_err(capability_error)?;
            let new_managed = agent_root
                .lease_directory(
                    &new_managed_rel,
                    CapabilityAccess::PrivateReadWrite,
                    RootCreation::Existing,
                )
                .map_err(capability_error)?;
            match classify_managed_pair(&old_managed, &new_managed, &rewrites)? {
                ManagedDisposition::Leftover => ManagedPlan::Copy,
                ManagedDisposition::Rewritten => ManagedPlan::RemoveOldOnly,
            }
        }
        (None, Some(_)) => {
            // The old root is gone, so a previous commit already got past the
            // rewrite step; a stale target catalog here is external damage.
            let new_managed = agent_root
                .lease_directory(
                    &new_managed_rel,
                    CapabilityAccess::PrivateReadWrite,
                    RootCreation::Existing,
                )
                .map_err(capability_error)?;
            let catalog = match read_catalog_bytes(&new_managed)? {
                Some(bytes) => parse_catalog_bytes(&bytes)?,
                None => WorktreeCatalog::default(),
            };
            if !catalog_is_rewritten(&catalog, &rewrites) {
                return Err(WorktreeError::Other(anyhow!(
                    "worktree root rebind found a stale target catalog without the old namespace; refusing inconsistent relocation"
                )));
            }
            validate_relocatable_catalog(&catalog, &rewrites)?;
            ManagedPlan::RemoveOldOnly
        }
    };
    let store_plan = match (old_store_identity, new_store_identity) {
        (None, None) => StorePlan::None,
        (Some(_), None) => match managed_plan {
            ManagedPlan::None | ManagedPlan::Copy => StorePlan::Copy,
            ManagedPlan::RemoveOldOnly => {
                return Err(WorktreeError::Other(anyhow!(
                    "worktree root rebind found a relocated store without its copy; refusing inconsistent relocation"
                )));
            }
        },
        (Some(_), Some(_)) => match managed_plan {
            // A target store beside a live old store is only consistent as
            // part of an interrupted staging copy (cleared below) or as a
            // prior prepare whose commit still owes old-root removal.
            ManagedPlan::Copy => StorePlan::Copy,
            ManagedPlan::RemoveOldOnly => StorePlan::RemoveOld,
            ManagedPlan::None => {
                return Err(WorktreeError::Other(anyhow!(
                    "worktree root rebind target store namespace already exists with no recorded worktrees; refusing ambiguous relocation"
                )));
            }
        },
        (None, Some(_)) => match managed_plan {
            ManagedPlan::Copy => {
                return Err(WorktreeError::Other(anyhow!(
                    "worktree root rebind found a relocated store without its source; refusing inconsistent relocation"
                )));
            }
            ManagedPlan::RemoveOldOnly | ManagedPlan::None => StorePlan::None,
        },
    };

    // Validate before mutating anything.
    if let Some(_) = old_managed_identity {
        let old_managed = agent_root
            .lease_directory(
                &old_managed_rel,
                CapabilityAccess::PrivateReadWrite,
                RootCreation::Existing,
            )
            .map_err(capability_error)?;
        let catalog = catalog::load_catalog(&old_managed)?;
        validate_relocatable_catalog(&catalog, &rewrites)?;
    }

    // Clear an interrupted staging copy before a fresh relocation.
    if managed_plan == ManagedPlan::Copy {
        if let Some(identity) = new_managed_identity {
            agent_root
                .remove_tree_expected(&new_managed_rel, identity)
                .map_err(capability_error)?;
            new_managed_identity = None;
        }
    }
    if store_plan == StorePlan::Copy {
        if let Some(identity) = new_store_identity {
            agent_root
                .remove_tree_expected(&new_store_rel, identity)
                .map_err(capability_error)?;
            new_store_identity = None;
        }
    }

    // Stage the copies. The authority (catalog) is not touched yet and the
    // old roots stay exactly where they are.
    let mut copied_managed = false;
    let mut copied_store = false;
    if managed_plan == ManagedPlan::Copy {
        copy_tree_governed(&agent_root, &old_managed_rel, &agent_root, &new_managed_rel)?;
        copied_managed = true;
        let new_managed = agent_root
            .lease_directory(
                &new_managed_rel,
                CapabilityAccess::PrivateReadWrite,
                RootCreation::Existing,
            )
            .map_err(capability_error)?;
        // A copied repository lock would wedge every later manager on the new
        // root (create-new loops until timeout), so it never carries over.
        let lock_path = Path::new(catalog::REPO_LOCK_FILE_NAME);
        if new_managed
            .entry_exists(lock_path)
            .map_err(capability_error)?
        {
            new_managed
                .remove_nondirectory_entry(lock_path)
                .map_err(capability_error)?;
        }
        rewrite_worktree_git_pointers(&new_managed, &rewrites)?;
        new_managed_identity = probe_managed_directory(&agent_root, &new_managed_rel)?;
    }
    if store_plan == StorePlan::Copy {
        copy_tree_governed(&agent_root, &old_store_rel, &agent_root, &new_store_rel)?;
        copied_store = true;
        new_store_identity = probe_managed_directory(&agent_root, &new_store_rel)?;
    }

    if managed_plan == ManagedPlan::RemoveOldOnly
        && old_managed_identity.is_none()
        && store_plan == StorePlan::None
    {
        return Ok(None); // already fully rebound
    }
    if managed_plan == ManagedPlan::None && store_plan == StorePlan::None {
        return Ok(None); // nothing to rebind
    }

    Ok(Some(PreparedWorktreeRootRebind {
        agent_root,
        _lock: lock,
        old_store_rel,
        new_store_rel,
        old_managed_rel,
        new_managed_rel,
        rewrites,
        managed_plan,
        store_plan,
        old_managed_identity,
        old_store_identity,
        new_managed_identity,
        new_store_identity,
        copied_managed,
        copied_store,
        old_removed: false,
    }))
}

/// One-shot rebind of a session's workflow storage roots: prepares the
/// relocation and immediately commits it. Returns `true` when roots were
/// relocated (or a previously interrupted relocation was completed) and
/// `false` when there was nothing to rebind. Intended to be called by the
/// session move core after the session file move landed; failures must
/// surface as warnings, never abort the move.
pub fn rebind_session_worktree_roots(
    agent_dir: &Path,
    old_cwd: &Path,
    new_cwd: &Path,
    session_id: &str,
) -> Result<bool, WorktreeError> {
    let Some(prepared) =
        prepare_session_worktree_root_rebind(agent_dir, old_cwd, new_cwd, session_id)?
    else {
        return Ok(false);
    };
    prepared.commit()?;
    Ok(true)
}
