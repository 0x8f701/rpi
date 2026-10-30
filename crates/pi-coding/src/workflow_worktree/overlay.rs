//! Governed overlayfs workflow isolation lifecycle.
//!
//! Each workflow gets a private writable "merged" view whose read-only lower
//! layer is the live source tree. The lifecycle is fully capability-retained:
//! the source repository and the per-workflow storage are opened exactly once
//! as `CapabilityRootLease`s (`ProjectResources` and `WorkflowWorktrees`), and
//! every git invocation runs through the governed `GovernedProcessContext`
//! (`SpawnFamily::RepositoryHelper`), which binds the working directory to the
//! retained authority. No production path reopens a stored/display path or
//! raw-spawns a process.
//!
//! Materialization tries the kernel overlay and fuse-overlayfs backends first
//! — those are the fixed-operator raw-mount exceptions reviewed by the sandbox
//! raw-spawn gate and are only reachable from native fixtures
//! (`#[cfg(test)]` on Linux with mount privilege). When neither is
//! representable (production, or unprivileged hosts), the manager materializes
//! the merged view with a governed recursive copy through the retained
//! capabilities (`OverlayBackend::Rcopy`). The chosen backend is persisted in
//! the ownership catalog so a restored workflow re-establishes exactly what it
//! had before a restart.
//!
//! Integration is transactional: the merged tree is synchronized into a
//! detached candidate worktree under the retained managed root and committed
//! there. The authoritative source HEAD, index bytes, entry identities, modes,
//! and contents are captured before construction and revalidated immediately
//! before the candidate commit is applied. Any mismatch fails closed without
//! copying candidate files into the source.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use serde::{Deserialize, Serialize};

use crate::fs::{
    CapabilityAccess, CapabilityDirectory, CapabilityEntry, CapabilityRootLease, EntryKind,
    FileIdentity, FileMode, FsError, Replacement, ReplacementTarget, RootCreation,
    StreamCopyBuffer, StreamingReplacement,
};
use crate::isolate::{OverlayBackend, OverlayfsIsolation};
use crate::process::GovernedProcessContext;

use super::catalog::{CATALOG_DIR_NAME, acquire_repo_lock};
use super::git::{
    command_failed, commit_count_ahead, discover_repo, first_commit_with_message_line,
    is_ancestor, is_dirty, list_changed_files, now_ms, reject_if_inside, resolve_commit,
    rev_parse, run_git, run_git_allow_fail, run_git_capture, sanitize_path_segment,
    validate_workflow_id,
};
use super::{
    CreateWorktreeOptions, DEFAULT_GIT_TIMEOUT, IntegrateOptions, IntegrateOutcome,
    IntegrateStrategy, RepoDiscovery, TrustedWorkflowCwd, WorkflowIsolation,
    WorkflowIsolationSetting, WorkflowWorktreeIdentity, WorkflowWorktreeStatus, WorktreeError,
};

/// Directory name under the managed root holding per-workflow overlay trees.
pub const OVERLAY_ROOT_DIR_NAME: &str = "overlay-workflows";

/// Branch-namespace label for overlay-isolated workflows (no real git branch
/// is created; the label keeps snapshots/UI self-consistent).
pub const OVERLAY_BRANCH_PREFIX: &str = "rpi/overlay/";

const CATALOG_FILE_NAME: &str = "overlay-workflows.json";
const CATALOG_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Bounded governed recursive copy: a workflow source tree larger than this
/// fails closed rather than copying unbounded data through the retained
/// capabilities.
const OVERLAY_COPY_MAX_BYTES: u64 = 8 * 1024 * 1024 * 1024;

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IntegrationHookPoint {
    AfterDeletion,
    MidCopy,
    AfterCopy,
    Add,
    Commit,
    ConcurrentMutation,
    PublicationMutation,
    MergeMutation,
    AppliedMarker,
}

#[cfg(test)]
thread_local! {
    static INTEGRATION_HOOK: std::cell::RefCell<Option<Arc<dyn Fn(IntegrationHookPoint) -> Result<(), WorktreeError>>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn integration_hook(point: IntegrationHookPoint) -> Result<(), WorktreeError> {
    INTEGRATION_HOOK.with(|hook| match hook.borrow().as_ref() {
        Some(hook) => hook(point),
        None => Ok(()),
    })
}

#[cfg(test)]
thread_local! {
    static OVERLAY_COPY_LIMIT_OVERRIDE: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

fn overlay_copy_limit() -> u64 {
    #[cfg(test)]
    if let Some(limit) = OVERLAY_COPY_LIMIT_OVERRIDE.with(std::cell::Cell::get) {
        return limit;
    }
    OVERLAY_COPY_MAX_BYTES
}

#[cfg(test)]
struct OverlayCopyLimitGuard;

#[cfg(test)]
impl Drop for OverlayCopyLimitGuard {
    fn drop(&mut self) {
        OVERLAY_COPY_LIMIT_OVERRIDE.with(|limit| limit.set(None));
    }
}

#[cfg(test)]
fn set_overlay_copy_limit(limit: u64) -> OverlayCopyLimitGuard {
    OVERLAY_COPY_LIMIT_OVERRIDE.with(|slot| slot.set(Some(limit)));
    OverlayCopyLimitGuard
}


#[cfg(test)]
fn set_integration_hook(
    hook: Option<Arc<dyn Fn(IntegrationHookPoint) -> Result<(), WorktreeError>>>,
) {
    INTEGRATION_HOOK.with(|slot| *slot.borrow_mut() = hook);
}
#[derive(Clone, Debug, Eq, PartialEq)]
struct SourceIdentity {
    head: String,
    index_tree: String,
    index_hash: String,
    status: String,
}

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
    /// Overlay HEAD that was last copy-backed. Skip only when live overlay
    /// HEAD still equals this commit, so a later overlay commit is applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    applied_overlay_head: Option<String>,
    /// Source HEAD recorded after a successful copy-back. Retry skips
    /// copy-back when this commit is still an ancestor of live source HEAD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    applied_source_commit: Option<String>,
    /// Overlay HEAD recorded before copy-back so a crash after the source
    /// commit but before the applied marker can recover from history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_overlay_head: Option<String>,
    /// Source HEAD immediately before copy-back. Recovery only accepts a
    /// child of this commit whose message binds the pending overlay HEAD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_source_head: Option<String>,
    /// True only after copy-back completed for the pending overlay/source pair.
    /// A false value means retry must re-run the idempotent sync; a true value
    /// means retry must preserve the copied dirty tree and continue add/commit.
    #[serde(default)]
    pending_copy_complete: bool,
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
            applied_overlay_head: None,
            applied_source_commit: None,
            pending_overlay_head: None,
            pending_source_head: None,
            pending_copy_complete: false,
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
#[derive(Clone)]
pub struct OverlayWorkflowManager {
    source_root: CapabilityRootLease,
    managed_root: CapabilityRootLease,
    timeout: Duration,
    backend_candidates: Vec<OverlayBackend>,
    process: Option<GovernedProcessContext>,
}

impl OverlayWorkflowManager {
    /// Build a manager bound to the retained source repository capability and
    /// the session-scoped managed root capability that will hold the overlay
    /// workflows. Both leases are retained for the lifetime of the manager;
    /// no production path reopens their display roots.
    pub(crate) fn new(source_root: CapabilityRootLease, managed_root: CapabilityRootLease) -> Self {
        debug_assert_eq!(
            source_root.purpose(),
            crate::fs::CapabilityRootPurpose::ProjectResources
        );
        debug_assert!(
            matches!(
                source_root.access(),
                crate::fs::CapabilityAccess::ReadWrite
                    | crate::fs::CapabilityAccess::PrivateReadWrite
            ),
            "overlay source root must be writable for copy-back integration"
        );
        debug_assert_eq!(
            managed_root.purpose(),
            crate::fs::CapabilityRootPurpose::WorkflowWorktrees
        );
        debug_assert_eq!(
            managed_root.access(),
            crate::fs::CapabilityAccess::PrivateReadWrite
        );
        Self {
            source_root,
            managed_root,
            timeout: DEFAULT_GIT_TIMEOUT,
            backend_candidates: default_backend_candidates(),
            process: None,
        }
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    #[must_use]
    pub fn with_process_context(mut self, process: GovernedProcessContext) -> Self {
        #[cfg(unix)]
        let process = process
            .with_repository_authorities(self.source_root.clone(), self.managed_root.clone());
        self.process = Some(process);
        self
    }

    /// Restrict the backend chain. Tests force [`OverlayBackend::Rcopy`] to
    /// exercise the governed materialization deterministically without mount
    /// privilege.
    #[must_use]
    pub fn with_backend_candidates(mut self, candidates: Vec<OverlayBackend>) -> Self {
        self.backend_candidates = candidates;
        self
    }

    #[must_use]
    pub fn source_cwd(&self) -> &Path {
        self.source_root.display_root()
    }

    fn process(&self) -> Result<&GovernedProcessContext, WorktreeError> {
        self.process.as_ref().ok_or_else(|| {
            WorktreeError::Other(anyhow!(
                "workflow repository process authority is unavailable"
            ))
        })
    }

    fn discover(&self) -> Result<Option<crate::workflow_worktree::RepoDiscovery>, WorktreeError> {
        match discover_repo(
            self.process()?,
            self.source_root.display_root(),
            self.timeout,
        ) {
            Ok(discovery) => Ok(Some(discovery)),
            Err(WorktreeError::NotGit) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn identity_dir_relative(&self, workflow_id: &str) -> PathBuf {
        PathBuf::from(OVERLAY_ROOT_DIR_NAME).join(sanitize_path_segment(workflow_id))
    }

    fn merged_relative(&self, workflow_id: &str) -> PathBuf {
        self.identity_dir_relative(workflow_id).join("merged")
    }

    fn upper_relative(&self, workflow_id: &str) -> PathBuf {
        self.identity_dir_relative(workflow_id).join("upper")
    }

    fn work_relative(&self, workflow_id: &str) -> PathBuf {
        self.identity_dir_relative(workflow_id).join("work")
    }

    fn expected_worktree_path(&self, workflow_id: &str) -> PathBuf {
        self.managed_root
            .display_root()
            .join(self.merged_relative(workflow_id))
    }

    /// Strict identity: the recorded worktree path must be the exact merged
    /// directory under the retained managed root.
    fn verify_managed_identity(
        &self,
        identity: &WorkflowWorktreeIdentity,
    ) -> Result<(), WorktreeError> {
        let expected = self.expected_worktree_path(&identity.workflow_id);
        if identity.worktree_path != expected {
            return Err(WorktreeError::ownership(
                &identity.workflow_id,
                "recorded overlay worktree path is outside the exact retained managed root",
            ));
        }
        Ok(())
    }

    fn catalog_lease(&self) -> Result<CapabilityRootLease, WorktreeError> {
        self.managed_root
            .lease_directory(
                Path::new(CATALOG_DIR_NAME),
                CapabilityAccess::PrivateReadWrite,
                RootCreation::CreateDirectory,
            )
            .map_err(capability_failure)
    }

    fn load_catalog(&self) -> Result<OverlayCatalog, WorktreeError> {
        let catalog = self.catalog_lease()?;
        let bytes = match catalog.read_bounded(Path::new(CATALOG_FILE_NAME), CATALOG_MAX_BYTES) {
            Ok(bytes) => bytes,
            Err(error) if is_missing(&error) => return Ok(OverlayCatalog::default()),
            Err(error) => return Err(capability_failure(error)),
        };
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
        let bytes = serde_json::to_vec(&catalog).map_err(|error| {
            WorktreeError::Other(anyhow!("serializing overlay ownership catalog: {error}"))
        })?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > CATALOG_MAX_BYTES {
            return Err(WorktreeError::Other(anyhow!(
                "serialized overlay ownership catalog exceeds {CATALOG_MAX_BYTES} bytes"
            )));
        }
        let catalog_lease = self.catalog_lease()?;
        let relative = PathBuf::from(CATALOG_FILE_NAME);
        let expected = match catalog_lease.metadata(&relative) {
            Ok(metadata) => Some(metadata.identity),
            Err(error) if is_missing(&error) => None,
            Err(error) => return Err(capability_failure(error)),
        };
        catalog_lease
            .replace_file(Replacement {
                relative,
                expected,
                bytes,
                mode: FileMode::Private,
            })
            .map_err(capability_failure)?;
        Ok(())
    }

    fn load_entry(&self, workflow_id: &str) -> Result<Option<OverlayCatalogEntry>, WorktreeError> {
        Ok(self.load_catalog()?.entries.get(workflow_id).cloned())
    }

    /// Materialize the merged view at `merged_relative` under the retained
    /// managed root. Kernel/fuse are attempted only when representable (native
    /// fixtures with mount privilege); otherwise the governed recursive copy
    /// through the retained capabilities materializes an independent writable
    /// copy. Returns the backend that succeeded.
    fn materialize(
        &self,
        workflow_id: &str,
        discovery: Option<&super::RepoDiscovery>,
    ) -> Result<OverlayBackend, WorktreeError> {
        let identity_dir = self.identity_dir_relative(workflow_id);
        let merged_relative = self.merged_relative(workflow_id);
        let upper_relative = self.upper_relative(workflow_id);
        let work_relative = self.work_relative(workflow_id);

        self.managed_root
            .create_dir_all(&identity_dir, FileMode::Private)
            .map_err(capability_failure)?;
        let identity = self
            .managed_root
            .metadata(&identity_dir)
            .map_err(capability_failure)?
            .identity;
        let materialized = (|| {
            for relative in [&merged_relative, &upper_relative, &work_relative] {
                self.managed_root
                    .create_dir_all(relative, FileMode::Private)
                    .map_err(capability_failure)?;
            }
            Ok::<(), WorktreeError>(())
        })();
        if let Err(error) = materialized {
            let _ = self
                .managed_root
                .remove_tree_expected(&identity_dir, identity);
            return Err(error);
        }

        let mount_candidates: Vec<OverlayBackend> = self
            .backend_candidates
            .iter()
            .copied()
            .filter(|backend| !matches!(backend, OverlayBackend::Rcopy))
            .collect();

        let source_display = self.source_root.display_root().to_path_buf();
        let upper_display = self.managed_root.display_root().join(&upper_relative);
        let work_display = self.managed_root.display_root().join(&work_relative);
        let merged_display = self.managed_root.display_root().join(&merged_relative);

        // Kernel/fuse mount attempt: only succeeds under the test raw-mount
        // exception. Production builds never raw-spawn `mount`, so this
        // returns `RetainedMountSourceUnavailable` and we fall through to the
        // governed recursive copy.
        if !mount_candidates.is_empty() {
            match OverlayfsIsolation::start_with(
                &source_display,
                &upper_display,
                &work_display,
                &merged_display,
                &mount_candidates,
            ) {
                Ok(handle) => return Ok(handle.backend()),
                Err(_) => {}
            }
        }
        // Governed recursive copy through the retained capabilities.
        if let Err(error) = copy_tree_governed(
            &self.source_root,
            Path::new(""),
            &self.managed_root,
            &merged_relative,
        ) {
            let _ = self
                .managed_root
                .remove_tree_expected(&identity_dir, identity);
            return Err(error);
        }
        let _ = discovery; // discovery is not required for rcopy materialization
        Ok(OverlayBackend::Rcopy)
    }

    /// Re-establish the recorded overlay mount after a process restart. rcopy
    /// backends need nothing — the merged view IS the persistent working copy.
    /// Mounted backends are re-mounted with the recorded backend only through
    /// the test raw-mount exception; production cannot raw-spawn a re-mount, so
    /// a persisted kernel/fuse backend fails closed until a native fixture
    /// re-establishes it.
    fn ensure_mounted(&self, entry: &OverlayCatalogEntry) -> Result<(), WorktreeError> {
        self.verify_managed_identity(&entry.identity())?;
        let merged_relative = self.merged_relative(&entry.workflow_id);
        match entry.backend {
            OverlayBackend::Rcopy => match self.managed_root.metadata(&merged_relative) {
                Ok(metadata) if metadata.identity.kind() == EntryKind::Directory => Ok(()),
                Ok(_) => Err(WorktreeError::ownership(
                    &entry.workflow_id,
                    "recorded overlay merged view is not a directory",
                )),
                Err(error) if is_missing(&error) => Err(WorktreeError::ownership(
                    &entry.workflow_id,
                    "recorded overlay merged view is missing",
                )),
                Err(error) => Err(capability_failure(error)),
            },
            OverlayBackend::Kernel | OverlayBackend::FuseOverlayfs => {
                let upper_display = self
                    .managed_root
                    .display_root()
                    .join(self.upper_relative(&entry.workflow_id));
                let work_display = self
                    .managed_root
                    .display_root()
                    .join(self.work_relative(&entry.workflow_id));
                let merged_display = self.managed_root.display_root().join(&merged_relative);
                let isolation = OverlayfsIsolation::restore(
                    &entry.source_root,
                    &upper_display,
                    &work_display,
                    &merged_display,
                    entry.backend,
                );
                if isolation.is_mounted() {
                    return Ok(());
                }
                // The recorded kernel/fuse mount is not verifiably present.
                // Native fixtures re-establish it directly; production cannot
                // raw-spawn a re-mount, so a persisted kernel/fuse catalog
                // must fail closed rather than proceed as mounted on a stale
                // merged view (the uncommitted upper layer would be lost).
                #[cfg(all(test, target_os = "linux"))]
                {
                    OverlayfsIsolation::start_with(
                        &entry.source_root,
                        &upper_display,
                        &work_display,
                        &merged_display,
                        &[entry.backend],
                    )
                    .map(|_| ())
                    .map_err(|error| {
                        WorktreeError::Other(anyhow!(
                            "re-establishing workflow overlay mount: {error}"
                        ))
                    })
                }
                #[cfg(any(not(test), not(target_os = "linux")))]
                {
                    Err(WorktreeError::ownership(
                        &entry.workflow_id,
                        "persisted kernel/fuse overlay backend cannot be re-established or verified without a retained mount authority; recreate the workflow or integrate before restart",
                    ))
                }
            }
        }
    }
    fn record_overlay_markers(
        &self,
        workflow_id: &str,
        identity: &WorkflowWorktreeIdentity,
        applied_overlay_head: Option<String>,
        applied_source_commit: Option<String>,
        pending_overlay_head: Option<String>,
        pending_source_head: Option<String>,
        pending_copy_complete: bool,
    ) -> Result<(), WorktreeError> {
        self.with_catalog_mut(|catalog| {
            let current = catalog
                .entries
                .get_mut(workflow_id)
                .ok_or_else(|| WorktreeError::NotRegistered(workflow_id.to_owned()))?;
            if current.identity() != *identity {
                return Err(WorktreeError::ownership(
                    workflow_id,
                    "ownership catalog changed during integration",
                ));
            }
            current.applied_overlay_head = applied_overlay_head;
            current.applied_source_commit = applied_source_commit;
            current.pending_overlay_head = pending_overlay_head;
            current.pending_source_head = pending_source_head;
            current.pending_copy_complete = pending_copy_complete;
            Ok(())
        })
    }
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

fn capability_failure(error: FsError) -> WorktreeError {
    WorktreeError::Other(anyhow!("workflow overlay capability failed: {error}"))
}
fn recovered_applied_source(
    process: &GovernedProcessContext,
    discovery: &RepoDiscovery,
    entry: &OverlayCatalogEntry,
    overlay_head: &str,
    source_head: &str,
    timeout: Duration,
) -> Result<Option<String>, WorktreeError> {
    if entry.applied_overlay_head.as_deref() == Some(overlay_head)
        && let Some(applied) = entry.applied_source_commit.as_deref()
        && is_ancestor(process, &discovery.repo_root, applied, source_head, timeout)?
    {
        return Ok(Some(applied.to_owned()));
    }
    let trailer = format!("overlay-head:{overlay_head}");
    if let Some(applied) = first_commit_with_message_line(
        process,
        &discovery.repo_root,
        &entry.base_commit,
        source_head,
        &trailer,
        timeout,
    )? {
        return Ok(Some(applied));
    }
    let Some(pending_overlay) = entry.pending_overlay_head.as_deref() else {
        return Ok(None);
    };
    let Some(pending_source) = entry.pending_source_head.as_deref() else {
        return Ok(None);
    };
    if pending_overlay != overlay_head {
        return Ok(None);
    }
    if !is_ancestor(process, &discovery.repo_root, pending_source, source_head, timeout)? {
        return Ok(None);
    }
    if source_head == pending_source {
        return Ok(None);
    }
    let trailer = format!("overlay-head:{overlay_head}");
    let Some(applied) = first_commit_with_message_line(
        process,
        &discovery.repo_root,
        pending_source,
        source_head,
        &trailer,
        timeout,
    )?
    else {
        return Ok(None);
    };
    if is_ancestor(process, &discovery.repo_root, pending_source, &applied, timeout)?
        && is_ancestor(process, &discovery.repo_root, &applied, source_head, timeout)?
    {
        Ok(Some(applied))
    } else {
        Ok(None)
    }
}

impl std::fmt::Debug for OverlayWorkflowManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OverlayWorkflowManager")
            .finish_non_exhaustive()
    }
}

impl WorkflowIsolation for OverlayWorkflowManager {
    fn create(
        &self,
        workflow_id: &str,
        options: CreateWorktreeOptions,
    ) -> Result<WorkflowWorktreeIdentity, WorktreeError> {
        validate_workflow_id(workflow_id)?;
        let process = self.process()?;
        let timeout = options.timeout.unwrap_or(self.timeout);
        if options.managed_root != self.managed_root.display_root() {
            return Err(WorktreeError::ownership(
                workflow_id,
                "managed root does not match the overlay isolation manager's retained root",
            ));
        }
        reject_if_inside(
            self.managed_root.display_root(),
            self.source_root.display_root(),
        )
        .map_err(|detail| {
            WorktreeError::ownership(
                workflow_id,
                &format!("managed root must not be inside the source worktree: {detail}"),
            )
        })?;

        let discovery = self.discover()?;
        let base_commit = match (&options.base_commit, &discovery) {
            (Some(revision), Some(_)) => {
                resolve_commit(process, self.source_root.display_root(), revision, timeout)?
            }
            (None, Some(discovery)) => discovery.head_commit.clone(),
            (Some(_), None) | (None, None) => "none".to_owned(),
        };

        let _lock = acquire_repo_lock(&self.managed_root)?;
        if self.load_entry(workflow_id)?.is_some() {
            return Err(WorktreeError::ownership(
                workflow_id,
                "workflow already has a recorded overlay",
            ));
        }
        if self
            .managed_root
            .metadata(&self.identity_dir_relative(workflow_id))
            .is_ok()
        {
            return Err(WorktreeError::ownership(
                workflow_id,
                "managed overlay location already exists",
            ));
        }

        let backend = self.materialize(workflow_id, discovery.as_ref())?;
        let merged_display = self.expected_worktree_path(workflow_id);

        let head_commit = if discovery.is_some() {
            match rev_parse(process, &merged_display, "HEAD", timeout) {
                Ok(head) => head,
                Err(error) => {
                    let _ = self.teardown(workflow_id, backend);
                    return Err(error);
                }
            }
        } else {
            "none".to_owned()
        };

        let identity = WorkflowWorktreeIdentity {
            workflow_id: workflow_id.to_string(),
            source_root: self.source_root.display_root().to_path_buf(),
            common_git_dir: discovery
                .as_ref()
                .map(|d| d.common_git_dir.clone())
                .unwrap_or_else(|| self.source_root.display_root().to_path_buf()),
            worktree_path: merged_display,
            branch: format!("{OVERLAY_BRANCH_PREFIX}{workflow_id}"),
            base_commit,
            head_commit,
            created_at_ms: now_ms(),
        };
        let result = self.with_catalog_mut(|catalog| {
            if catalog.entries.contains_key(workflow_id) {
                return Err(WorktreeError::ownership(
                    workflow_id,
                    "workflow ownership appeared during creation",
                ));
            }
            catalog.entries.insert(
                workflow_id.to_string(),
                OverlayCatalogEntry::from_identity(&identity, backend),
            );
            Ok(())
        });
        if result.is_err() {
            let _ = self.teardown(workflow_id, backend);
        }
        result?;
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
        if entry.identity() != *identity {
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
        let process = self.process()?;
        let entry = self
            .load_entry(workflow_id)?
            .ok_or_else(|| WorktreeError::NotRegistered(workflow_id.to_owned()))?;
        self.ensure_mounted(&entry)?;
        let identity = entry.identity();
        let git = self
            .managed_root
            .metadata(&self.merged_relative(workflow_id))
            .is_ok_and(|metadata| {
                metadata.identity.kind() == EntryKind::Directory
                    && self
                        .managed_root
                        .entry_exists(&self.merged_relative(workflow_id).join(".git"))
                        .unwrap_or(false)
            });
        let live_head = if git {
            rev_parse(process, &identity.worktree_path, "HEAD", self.timeout)?
        } else {
            "none".to_owned()
        };
        if git && live_head != identity.head_commit {
            let ancestry = run_git_allow_fail(
                process,
                &identity.worktree_path,
                &[
                    "merge-base",
                    "--is-ancestor",
                    &identity.head_commit,
                    &live_head,
                ],
                self.timeout,
            )?;
            match ancestry.status {
                Some(0) => {}
                Some(1) => {
                    return Err(WorktreeError::ownership(
                        workflow_id,
                        "live overlay HEAD rewound or diverged from the recorded history",
                    ));
                }
                _ => {
                    return Err(command_failed(
                        &[
                            "merge-base",
                            "--is-ancestor",
                            &identity.head_commit,
                            &live_head,
                        ],
                        &ancestry,
                    ));
                }
            }
        }
        let mut refreshed = entry.clone();
        refreshed.head_commit = live_head;
        let _lock = acquire_repo_lock(&self.managed_root)?;
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
            catalog
                .entries
                .insert(workflow_id.to_owned(), refreshed.clone());
            Ok(())
        })?;
        let cwd = TrustedWorkflowCwd {
            path: refreshed.worktree_path.clone(),
            workflow_id: workflow_id.to_owned(),
        };
        Ok((refreshed.identity(), cwd))
    }
    fn integrate(
        &self,
        workflow_id: &str,
        options: IntegrateOptions,
    ) -> Result<IntegrateOutcome, WorktreeError> {
        validate_workflow_id(workflow_id)?;
        if options.strategy != IntegrateStrategy::Merge || !options.require_clean_base {
            return Err(WorktreeError::Other(anyhow!(
                "overlay integration supports merge with a required clean source only"
            )));
        }
        let process = self.process()?;
        let timeout = options.timeout.unwrap_or(self.timeout);
        let _lock = acquire_repo_lock(&self.managed_root)?;
        let entry = self
            .load_entry(workflow_id)?
            .ok_or_else(|| WorktreeError::NotRegistered(workflow_id.to_owned()))?;
        self.ensure_mounted(&entry)?;
        let identity = entry.identity();
        let merged_relative = self.merged_relative(workflow_id);
        let discovery = self.discover()?;
        let overlay_head = if discovery.is_some() {
            Some(rev_parse(process, &identity.worktree_path, "HEAD", timeout)?)
        } else {
            None
        };

        let Some(discovery) = discovery else {
            return Err(WorktreeError::NotGit);
        };
        let overlay_head = overlay_head
            .ok_or_else(|| WorktreeError::Other(anyhow!("git overlay records overlay HEAD")))?;
        if commit_count_ahead(
            process,
            &identity.worktree_path,
            &identity.base_commit,
            &overlay_head,
            timeout,
        )? == 0
        {
            return Err(WorktreeError::MissingCommits);
        }
        let source_head = rev_parse(process, &discovery.repo_root, "HEAD", timeout)?;
        if let Some(applied_source) = recovered_applied_source(
            process,
            &discovery,
            &entry,
            &overlay_head,
            &source_head,
            timeout,
        )? {
            if entry.applied_source_commit.as_deref() != Some(applied_source.as_str())
                || entry.applied_overlay_head.as_deref() != Some(overlay_head.as_str())
                || entry.pending_overlay_head.is_some()
                || entry.pending_source_head.is_some()
                || entry.pending_copy_complete
            {
                self.record_overlay_markers(
                    workflow_id,
                    &identity,
                    Some(overlay_head),
                    Some(applied_source.clone()),
                    None,
                    None,
                    false,
                )?;
            }
            return Ok(IntegrateOutcome::Applied {
                strategy: IntegrateStrategy::Merge,
                result_commit: applied_source,
                merge_commit: None,
            });
        }
        if is_dirty(process, &identity.worktree_path, timeout)? {
            return Err(WorktreeError::DirtyWorktree(workflow_id.to_owned()));
        }
        let first_integration = entry.applied_overlay_head.is_none()
            && entry.pending_overlay_head.is_none();
        if first_integration && source_head != identity.base_commit {
            return Err(WorktreeError::DirtyBase);
        }
        if is_dirty(process, &discovery.repo_root, timeout)? {
            return Err(WorktreeError::DirtyBase);
        }
        let source_identity = capture_source_identity(process, &discovery, timeout)?;
        let candidate_relative = PathBuf::from(OVERLAY_ROOT_DIR_NAME)
            .join(format!(".integrate-{}-{}", sanitize_path_segment(workflow_id), now_ms()));
        let candidate_path = self.managed_root.display_root().join(&candidate_relative);
        let candidate_path_text = candidate_path.to_string_lossy().into_owned();
        let source_path_text = discovery.repo_root.to_string_lossy().into_owned();
        self.managed_root
            .create_dir_all(&candidate_relative, FileMode::Private)
            .map_err(capability_failure)?;
        let candidate = self
            .managed_root
            .open_directory(&candidate_relative)
            .map_err(capability_failure)?;
        if let Err(error) = run_git(
            process,
            self.managed_root.display_root(),
            &[
                "clone",
                "--no-hardlinks",
                "--no-checkout",
                &source_path_text,
                &candidate_path_text,
            ],
            timeout,
        ) {
            return Err(remove_candidate_tree(&candidate, error));
        }
        let candidate_result = (|| {
            run_git(
                process,
                &candidate_path,
                &["checkout", "--detach", &source_head],
                timeout,
            )?;
            sync_tree_governed(
                &self.managed_root,
                &merged_relative,
                &self.managed_root,
                &candidate_relative,
                &[".git"],
            )?;
            #[cfg(test)]
            integration_hook(IntegrationHookPoint::AfterCopy)?;
            #[cfg(test)]
            integration_hook(IntegrationHookPoint::Add)?;
            run_git(process, &candidate_path, &["add", "-A", "--", "."], timeout)?;
            #[cfg(test)]
            integration_hook(IntegrationHookPoint::Commit)?;
            run_git(
                process,
                &candidate_path,
                &[
                    "-c",
                    "user.name=rpi",
                    "-c",
                    "user.email=rpi@localhost",
                    "commit",
                    "--no-verify",
                    "--allow-empty",
                    "-m",
                    &format!(
                        "workflow {workflow_id}: integrate overlay changes\noverlay-head:{overlay_head}"
                    ),
                ],
                timeout,
            )?;
            rev_parse(process, &candidate_path, "HEAD", timeout)
        })();
        let candidate_commit = match candidate_result {
            Ok(commit) => commit,
            Err(error) => return Err(remove_candidate_tree(&candidate, error)),
        };
        if let Err(error) = run_git(
            process,
            &discovery.repo_root,
            &["fetch", "--no-tags", &candidate_path_text, &candidate_commit],
            timeout,
        ) {
            return Err(remove_candidate_tree(&candidate, error));
        }
        #[cfg(test)]
        if let Err(error) = integration_hook(IntegrationHookPoint::ConcurrentMutation) {
            return Err(remove_candidate_tree(&candidate, error));
        }
        let current_identity = match capture_source_identity(process, &discovery, timeout) {
            Ok(identity) => identity,
            Err(error) => return Err(remove_candidate_tree(&candidate, error)),
        };
        if current_identity != source_identity {
            return Err(remove_candidate_tree(&candidate, WorktreeError::DirtyBase));
        }
        #[cfg(test)]
        if let Err(error) = integration_hook(IntegrationHookPoint::PublicationMutation) {
            return Err(remove_candidate_tree(&candidate, error));
        }
        let publication_identity = match capture_source_identity(process, &discovery, timeout) {
            Ok(identity) => identity,
            Err(error) => return Err(remove_candidate_tree(&candidate, error)),
        };
        if publication_identity != source_identity {
            return Err(remove_candidate_tree(&candidate, WorktreeError::DirtyBase));
        }
        #[cfg(test)]
        if let Err(error) = integration_hook(IntegrationHookPoint::MergeMutation) {
            return Err(remove_candidate_tree(&candidate, error));
        }
        if let Err(error) = run_git(
            process,
            &discovery.repo_root,
            &["merge", "--ff-only", "--no-edit", "FETCH_HEAD"],
            timeout,
        ) {
            return Err(remove_candidate_tree(&candidate, error));
        }
        let applied_identity = match capture_source_identity(process, &discovery, timeout) {
            Ok(identity) => identity,
            Err(error) => return Err(remove_candidate_tree(&candidate, error)),
        };
        if applied_identity.head != candidate_commit || !applied_identity.status.is_empty() {
            return Err(remove_candidate_tree(&candidate, WorktreeError::DirtyBase));
        }
        #[cfg(test)]
        let marker_result = integration_hook(IntegrationHookPoint::AppliedMarker).and_then(|()| {
            self.record_overlay_markers(
                workflow_id,
                &identity,
                Some(overlay_head.clone()),
                Some(candidate_commit.clone()),
                None,
                None,
                false,
            )
        });
        #[cfg(not(test))]
        let marker_result = self.record_overlay_markers(
            workflow_id,
            &identity,
            Some(overlay_head),
            Some(candidate_commit.clone()),
            None,
            None,
            false,
        );
        let _ = marker_result;
        candidate.remove_tree_expected().map_err(|cleanup| {
            WorktreeError::Other(anyhow!("candidate cleanup conflict after successful integration: {cleanup}"))
        })?;
        Ok(IntegrateOutcome::Applied {
            strategy: IntegrateStrategy::Merge,
            result_commit: candidate_commit,
            merge_commit: None,
        })
    }
    fn remove(&self, identity: &WorkflowWorktreeIdentity) -> Result<(), WorktreeError> {
        validate_workflow_id(&identity.workflow_id)?;
        let entry = self
            .load_entry(&identity.workflow_id)?
            .ok_or_else(|| WorktreeError::NotRegistered(identity.workflow_id.clone()))?;
        if entry.identity() != *identity {
            return Err(WorktreeError::ownership(
                &identity.workflow_id,
                "provided identity does not exactly match the ownership catalog",
            ));
        }
        self.verify_managed_identity(identity)?;
        if self.load_entry(&identity.workflow_id)?.as_ref() != Some(&entry) {
            return Err(WorktreeError::ownership(
                &identity.workflow_id,
                "ownership catalog changed during removal",
            ));
        }
        self.teardown(&identity.workflow_id, entry.backend)?;
        let _lock = acquire_repo_lock(&self.managed_root)?;
        self.with_catalog_mut(|catalog| {
            let current = catalog
                .entries
                .get(&identity.workflow_id)
                .ok_or_else(|| WorktreeError::NotRegistered(identity.workflow_id.clone()))?;
            if current != &entry {
                return Err(WorktreeError::ownership(
                    &identity.workflow_id,
                    "ownership catalog changed during removal",
                ));
            }
            catalog.entries.remove(&identity.workflow_id);
            Ok(())
        })
    }

    fn inspect(&self, workflow_id: &str) -> Result<WorkflowWorktreeStatus, WorktreeError> {
        validate_workflow_id(workflow_id)?;
        let process = self.process()?;
        let entry = self
            .load_entry(workflow_id)?
            .ok_or_else(|| WorktreeError::NotRegistered(workflow_id.to_owned()))?;
        self.ensure_mounted(&entry)?;
        let identity = entry.identity();
        let git = self
            .managed_root
            .entry_exists(&self.merged_relative(workflow_id).join(".git"))
            .unwrap_or(false);
        let dirty = if git {
            is_dirty(process, &identity.worktree_path, self.timeout)?
        } else {
            false
        };
        let head = if git {
            rev_parse(process, &identity.worktree_path, "HEAD", self.timeout)?
        } else {
            identity.head_commit.clone()
        };
        let ahead_commits = if git {
            commit_count_ahead(
                process,
                &identity.worktree_path,
                &identity.base_commit,
                &head,
                self.timeout,
            )?
        } else {
            0
        };
        let changed_files = if git {
            list_changed_files(
                process,
                &identity.worktree_path,
                &identity.base_commit,
                self.timeout,
            )?
        } else {
            Vec::new()
        };
        let mut identity = identity;
        identity.head_commit = head;
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
        let _lock = acquire_repo_lock(&self.managed_root)?;
        let mut removed = Vec::new();
        self.with_catalog_mut(|catalog| {
            let ids: Vec<String> = catalog.entries.keys().cloned().collect();
            for id in ids {
                let Some(entry) = catalog.entries.get(&id).cloned() else {
                    continue;
                };
                let merged_relative = self.merged_relative(&id);
                let alive = match self.managed_root.metadata(&merged_relative) {
                    Ok(metadata) => metadata.identity.kind() == EntryKind::Directory,
                    Ok(_) => false,
                    Err(error) if is_missing(&error) => false,
                    Err(error) => return Err(capability_failure(error)),
                };
                if !alive {
                    catalog.entries.remove(&id);
                    removed.push(id);
                }
                let _ = entry;
            }
            Ok(())
        })?;
        Ok(removed)
    }

    fn kind(&self) -> WorkflowIsolationSetting {
        WorkflowIsolationSetting::Overlayfs
    }
}

impl OverlayWorkflowManager {
    /// Tear down a workflow's overlay: detach the mount (kernel/fuse via the
    /// test raw exception) and remove the identity directory through the
    /// retained managed-root capability. Rcopy has no mount to detach.
    fn teardown(&self, workflow_id: &str, backend: OverlayBackend) -> Result<(), WorktreeError> {
        let identity_dir = self.identity_dir_relative(workflow_id);
        let upper_display = self
            .managed_root
            .display_root()
            .join(self.upper_relative(workflow_id));
        let work_display = self
            .managed_root
            .display_root()
            .join(self.work_relative(workflow_id));
        let merged_display = self
            .managed_root
            .display_root()
            .join(self.merged_relative(workflow_id));
        if !matches!(backend, OverlayBackend::Rcopy) {
            let source_display = self.source_root.display_root().to_path_buf();
            let isolation = OverlayfsIsolation::restore(
                &source_display,
                &upper_display,
                &work_display,
                &merged_display,
                backend,
            );
            isolation.stop().map_err(|error| {
                WorktreeError::Other(anyhow!("workflow overlay teardown failed: {error}"))
            })?;
        }
        match self.managed_root.metadata(&identity_dir) {
            Ok(metadata) if metadata.identity.kind() == EntryKind::Directory => {
                self.managed_root
                    .remove_tree_expected(&identity_dir, metadata.identity)
                    .map_err(capability_failure)?;
            }
            Ok(_) => {
                return Err(WorktreeError::ownership(
                    workflow_id,
                    "managed overlay identity dir is not a directory",
                ));
            }
            Err(error) if is_missing(&error) => {}
            Err(error) => return Err(capability_failure(error)),
        }
        Ok(())
    }
}

/// Isolation-less backend (`settings.orchestration.isolation: none`):
/// workflows operate directly on the source working tree. Integration is a
/// no-op because the workflow's changes are already in place; removal is a
/// no-op because nothing was created. Multiple concurrent workflows share the
/// source tree — the caller accepts that hazard by selecting `none`.
///
/// The backend still runs git through the governed repository-helper spawn
/// path (integration and inspect call `discover_repo`), so it must hold the
/// same retained repository authorities as the other backends and install
/// them on its `GovernedProcessContext`. Without them every repository-helper
/// spawn fails the cwd-provenance precondition (`repository source capability
/// does not cover this generation`) and every no-isolation workflow fails at
/// integration.
#[derive(Clone, Debug)]
pub struct NoopWorkflowIsolation {
    source_lease: CapabilityRootLease,
    managed_lease: CapabilityRootLease,
    timeout: Duration,
    process: Option<GovernedProcessContext>,
}

impl NoopWorkflowIsolation {
    /// Build the backend bound to the retained source repository capability
    /// and the session-scoped managed root capability. No worktree is ever
    /// created, but the governed repository helper requires both leases for
    /// working-directory provenance.
    pub(crate) fn new(
        source_lease: CapabilityRootLease,
        managed_lease: CapabilityRootLease,
    ) -> Self {
        debug_assert_eq!(
            source_lease.purpose(),
            crate::fs::CapabilityRootPurpose::ProjectResources
        );
        debug_assert_eq!(
            managed_lease.purpose(),
            crate::fs::CapabilityRootPurpose::WorkflowWorktrees
        );
        Self {
            source_lease,
            managed_lease,
            timeout: DEFAULT_GIT_TIMEOUT,
            process: None,
        }
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    #[must_use]
    pub fn with_process_context(mut self, process: GovernedProcessContext) -> Self {
        #[cfg(unix)]
        let process = process.with_repository_authorities(
            self.source_lease.clone(),
            self.managed_lease.clone(),
        );
        self.process = Some(process);
        self
    }

    fn process_context(&self) -> Result<&GovernedProcessContext, WorktreeError> {
        self.process.as_ref().ok_or_else(|| {
            WorktreeError::Other(anyhow!(
                "workflow repository process authority is unavailable"
            ))
        })
    }

    /// Deterministic identity for a workflow id (no persistence needed: every
    /// field derives from the id and the source path).
    fn identity(&self, workflow_id: &str) -> Result<WorkflowWorktreeIdentity, WorktreeError> {
        validate_workflow_id(workflow_id)?;
        let source_root = self.source_lease.display_root().to_path_buf();
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
        match discover_repo(self.process_context()?, &identity.source_root, self.timeout) {
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
        let process = self.process_context()?;
        let git = discover_repo(process, &identity.source_root, self.timeout).is_ok();
        let dirty = if git {
            is_dirty(process, &identity.source_root, self.timeout)?
        } else {
            false
        };
        let changed_files = if dirty {
            list_changed_files(process, &identity.source_root, "HEAD", self.timeout)?
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

/// Governed recursive copy through the retained capabilities: every byte is
/// streamed from an already-open source file into a staged target sibling, so
/// no display path is reopened and no whole-file allocation is required.
/// Symlinks and other non-representable entry kinds fail closed
/// (`FsError::UnsupportedEntryType`).
pub(crate) fn copy_tree_governed(
    source: &CapabilityRootLease,
    source_relative: &Path,
    target: &CapabilityRootLease,
    target_relative: &Path,
) -> Result<(), WorktreeError> {
    copy_tree_governed_with_limit(
        source,
        source_relative,
        target,
        target_relative,
        overlay_copy_limit(),
    )
    .map(|_| ())
}

fn copy_tree_governed_with_limit(
    source: &CapabilityRootLease,
    source_relative: &Path,
    target: &CapabilityRootLease,
    target_relative: &Path,
    aggregate_limit: u64,
) -> Result<(u64, usize), WorktreeError> {
    target
        .create_dir_all(target_relative, FileMode::Private)
        .map_err(capability_failure)?;
    let directory = source
        .open_directory(source_relative)
        .map_err(capability_failure)?;
    let mut aggregate_actual = 0;
    let mut buffer = StreamCopyBuffer::new();
    copy_opened_directory_governed(
        directory,
        target,
        target_relative,
        &mut aggregate_actual,
        aggregate_limit,
        &mut buffer,
    )?;
    Ok((aggregate_actual, buffer.len()))
}

fn copy_opened_directory_governed(
    source: CapabilityDirectory,
    target: &CapabilityRootLease,
    target_relative: &Path,
    aggregate_actual: &mut u64,
    aggregate_limit: u64,
    buffer: &mut StreamCopyBuffer,
) -> Result<(), WorktreeError> {
    let mut entries = source
        .read_dir_rejecting_unsupported()
        .map_err(capability_failure)?;
    while let Some(entry) = entries.next().map_err(capability_failure)? {
        let destination = target_relative.join(entry.name());
        match entry.into_entry() {
            CapabilityEntry::Directory(directory) => {
                target
                    .create_dir_all(&destination, FileMode::Private)
                    .map_err(capability_failure)?;
                copy_opened_directory_governed(
                    directory,
                    target,
                    &destination,
                    aggregate_actual,
                    aggregate_limit,
                    buffer,
                )?;
            }
            CapabilityEntry::File(file) => replace_opened_file_governed(
                target,
                destination,
                ReplacementTarget::Absent,
                &file,
                aggregate_actual,
                aggregate_limit,
                buffer,
            )?,
        }
    }
    Ok(())
}

fn replace_opened_file_governed(
    target: &CapabilityRootLease,
    relative: PathBuf,
    expected: ReplacementTarget,
    source: &crate::fs::CapabilityFile,
    aggregate_actual: &mut u64,
    aggregate_limit: u64,
    buffer: &mut StreamCopyBuffer,
) -> Result<(), WorktreeError> {
    #[cfg(not(test))]
    let result = target.replace_file_streaming(
        StreamingReplacement { relative, expected },
        source,
        aggregate_actual,
        aggregate_limit,
        buffer,
    );
    #[cfg(test)]
    let result = target.replace_file_streaming_with(
        StreamingReplacement { relative, expected },
        source,
        aggregate_actual,
        aggregate_limit,
        buffer,
        || {
            integration_hook(IntegrationHookPoint::MidCopy)
                .map_err(|_| FsError::IdentityChanged)
        },
    );
    result.map(|_| ()).map_err(capability_failure)
}

/// Governed copy-back sync: make `target` (the detached candidate) an exact
/// copy of `source`, never touching top-level entries listed in `excluded_top`.
/// Existing regular files are replaced only at their exact captured identity;
/// missing files use the typed no-replace contract.
fn sync_tree_governed(
    source: &CapabilityRootLease,
    source_relative: &Path,
    target: &CapabilityRootLease,
    target_relative: &Path,
    excluded_top: &[&str],
) -> Result<(), WorktreeError> {
    sync_tree_governed_with_limit(
        source,
        source_relative,
        target,
        target_relative,
        excluded_top,
        overlay_copy_limit(),
    )
    .map(|_| ())
}

fn sync_tree_governed_with_limit(
    source: &CapabilityRootLease,
    source_relative: &Path,
    target: &CapabilityRootLease,
    target_relative: &Path,
    excluded_top: &[&str],
    aggregate_limit: u64,
) -> Result<(u64, usize), WorktreeError> {
    let excluded = |name: &std::ffi::OsStr| {
        excluded_top
            .iter()
            .any(|top| std::ffi::OsStr::new(*top) == name)
    };

    let target_dir = target
        .open_directory(target_relative)
        .map_err(capability_failure)?;
    let mut target_entries = target_dir
        .read_dir_rejecting_unsupported()
        .map_err(capability_failure)?;
    let mut to_remove: Vec<(std::ffi::OsString, EntryKind, FileIdentity)> = Vec::new();
    while let Some(entry) = target_entries.next().map_err(capability_failure)? {
        let name = entry.name().to_os_string();
        if excluded(&name) {
            continue;
        }
        let kind = entry.entry().kind();
        let identity = entry.entry().identity();
        let present_in_source = source
            .entry_exists(&source_relative.join(&name))
            .map_err(capability_failure)?;
        if !present_in_source {
            to_remove.push((name, kind, identity));
        }
    }
    drop(target_entries);
    for (name, kind, identity) in to_remove {
        let target_path = target_relative.join(&name);
        match kind {
            EntryKind::Directory => target
                .remove_tree_expected(&target_path, identity)
                .map_err(capability_failure)?,
            EntryKind::RegularFile => target
                .remove_file_expected(&target_path, identity)
                .map_err(capability_failure)?,
        }
    }
    #[cfg(test)]
    integration_hook(IntegrationHookPoint::AfterDeletion)?;

    let source_dir = source
        .open_directory(source_relative)
        .map_err(capability_failure)?;
    let mut aggregate_actual = 0;
    let mut buffer = StreamCopyBuffer::new();
    sync_opened_directory_governed(
        source_dir,
        target,
        target_relative,
        excluded,
        &mut aggregate_actual,
        aggregate_limit,
        &mut buffer,
    )?;
    Ok((aggregate_actual, buffer.len()))
}

fn sync_opened_directory_governed(
    source: CapabilityDirectory,
    target: &CapabilityRootLease,
    target_relative: &Path,
    excluded: impl Fn(&std::ffi::OsStr) -> bool,
    aggregate_actual: &mut u64,
    aggregate_limit: u64,
    buffer: &mut StreamCopyBuffer,
) -> Result<(), WorktreeError> {
    let mut entries = source
        .read_dir_rejecting_unsupported()
        .map_err(capability_failure)?;
    while let Some(entry) = entries.next().map_err(capability_failure)? {
        let name = entry.name().to_os_string();
        if excluded(&name) {
            continue;
        }
        let target_path = target_relative.join(&name);
        match entry.into_entry() {
            CapabilityEntry::Directory(directory) => {
                target
                    .create_dir_all(&target_path, FileMode::Private)
                    .map_err(capability_failure)?;
                sync_opened_directory_governed(
                    directory,
                    target,
                    &target_path,
                    &excluded_no_top,
                    aggregate_actual,
                    aggregate_limit,
                    buffer,
                )?;
            }
            CapabilityEntry::File(file) => {
                let expected = match target.metadata(&target_path) {
                    Ok(metadata) if metadata.identity.kind() == EntryKind::RegularFile => {
                        ReplacementTarget::ExactIdentity(metadata.identity)
                    }
                    Ok(_) => {
                        return Err(capability_failure(FsError::UnsupportedEntryType));
                    }
                    Err(error) if is_missing(&error) => ReplacementTarget::Absent,
                    Err(error) => return Err(capability_failure(error)),
                };
                replace_opened_file_governed(
                    target,
                    target_path,
                    expected,
                    &file,
                    aggregate_actual,
                    aggregate_limit,
                    buffer,
                )?;
            }
        }
    }
    Ok(())
}

fn capture_source_identity(
    process: &GovernedProcessContext,
    discovery: &RepoDiscovery,
    timeout: Duration,
) -> Result<SourceIdentity, WorktreeError> {
    let index_path = run_git_capture(
        process,
        &discovery.repo_root,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        timeout,
    )?;
    Ok(SourceIdentity {
        head: rev_parse(process, &discovery.repo_root, "HEAD", timeout)?,
        index_tree: run_git_capture(
            process,
            &discovery.repo_root,
            &["write-tree"],
            timeout,
        )?,
        index_hash: run_git_capture(
            process,
            &discovery.repo_root,
            &["hash-object", "--no-filters", &index_path],
            timeout,
        )?,
        status: run_git_capture(
            process,
            &discovery.repo_root,
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
            timeout,
        )?,
    })
}

fn remove_candidate_tree(candidate: &CapabilityDirectory, error: WorktreeError) -> WorktreeError {
    match candidate.remove_tree_expected() {
        Ok(()) => error,
        Err(cleanup) => WorktreeError::Other(anyhow!(
            "{error}; candidate cleanup conflict: {cleanup}"
        )),
    }
}


fn excluded_no_top(_name: &std::ffi::OsStr) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Arc;
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;
    use crate::fs::{CapabilityAccess, CapabilityRootPurpose, RootCreation};
    use crate::isolate::OverlayBackend;
    use crate::workflow_worktree::{
        CreateWorktreeOptions, IntegrateOptions, IntegrateStrategy, TrustedWorkflowCwd,
        WorkflowIsolation, WorkflowWorktreeIdentity, WorktreeError,
    };

    const TEST_TIMEOUT: Duration = Duration::from_secs(30);

    fn repository_test_executor() -> crate::process::RepositoryTestExecutor {
        Arc::new(|authorized, stdout_bytes, stderr_bytes| {
            assert!(matches!(
                authorized.audience(),
                crate::sandbox::CredentialAudience::SpawnFamily {
                    family: pi_sandbox::SpawnFamily::RepositoryHelper,
                }
            ));
            let argv = authorized.argv();
            let program = PathBuf::from(argv.first().expect("git program"));
            let mut command = Command::new(program);
            command.args(&argv[1..]);
            command.current_dir(authorized.cwd().as_path());
            command.env_clear();
            command.envs(authorized.environment());
            let output = command
                .output()
                .expect("execute governed repository fixture");
            let stdout_capped = output.stdout.len() > stdout_bytes;
            let stderr_capped = output.stderr.len() > stderr_bytes;
            Ok(crate::process::GovernedProcessOutput {
                status: output.status.code(),
                stdout: output.stdout.into_iter().take(stdout_bytes).collect(),
                stderr: output.stderr.into_iter().take(stderr_bytes).collect(),
                timed_out: false,
                cancelled: false,
                stdout_capped,
                stderr_capped,
            })
        })
    }

    fn fixture_git(cwd: &Path, args: &[&str]) -> String {
        let null_device = if cfg!(windows) { "NUL" } else { "/dev/null" };
        let hooks_path = format!("core.hooksPath={null_device}");
        let output = Command::new("git")
            .args([
                "-c",
                "color.ui=false",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
                "-c",
                hooks_path.as_str(),
            ])
            .args(args)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null_device)
            .env("LC_ALL", "C")
            .output()
            .expect("execute fixture git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("git output utf8")
            .trim()
            .to_owned()
    }

    struct IntegrationHookGuard;

    #[test]
    fn governed_copy_streams_multiple_chunks_with_one_fixed_buffer() {
        let f = Fixture::new();
        let bytes = (0..(64 * 1024 * 2 + 19))
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        fs::write(f.repo.join("large.bin"), &bytes).expect("write multi-chunk source");
        fs::write(f.repo.join("small.bin"), b"second file").expect("write second source");

        let (actual, buffer_bytes) = copy_tree_governed_with_limit(
            &f.source_lease,
            Path::new(""),
            &f.managed_lease,
            Path::new("stream-copy"),
            OVERLAY_COPY_MAX_BYTES,
        )
        .expect("stream governed tree");

        assert_eq!(buffer_bytes, 64 * 1024);
        assert_eq!(fs::read(f.managed.join("stream-copy/large.bin")).unwrap(), bytes);
        assert_eq!(
            actual,
            tree_logical_bytes(&f.repo),
            "aggregate must count actual streamed bytes exactly once"
        );
    }

    #[test]
    fn governed_copy_aggregate_boundary_is_inclusive_and_next_file_fails() {
        let f = Fixture::new();
        fs::remove_dir_all(f.repo.join(".git")).expect("remove fixture metadata");
        fs::remove_file(f.repo.join("README.md")).expect("remove fixture file");
        fs::write(f.repo.join("a"), b"1234").expect("first boundary file");
        fs::write(f.repo.join("b"), b"5678").expect("second boundary file");
        let exact = copy_tree_governed_with_limit(
            &f.source_lease,
            Path::new(""),
            &f.managed_lease,
            Path::new("exact"),
            8,
        )
        .expect("exact aggregate boundary");
        assert_eq!(exact.0, 8);

        fs::write(f.repo.join("c"), b"9").expect("one byte over boundary");
        let target = Path::new("over");
        f.managed_lease
            .create_dir_all(target, FileMode::Private)
            .expect("over target");
        let directory = f
            .source_lease
            .open_directory(Path::new(""))
            .expect("source directory");
        let mut actual = 0;
        let mut buffer = StreamCopyBuffer::new();
        copy_opened_directory_governed(
            directory,
            &f.managed_lease,
            target,
            &mut actual,
            8,
            &mut buffer,
        )
        .expect_err("later byte must exceed aggregate");
        assert_eq!(actual, 8, "failed reservation must not double-count bytes");
        assert!(!f.managed.join("over/c").exists());
    }

    #[cfg(unix)]
    #[test]
    fn governed_copy_strictly_rejects_symlink_and_materialize_cleans_identity() {
        use std::os::unix::fs::symlink;

        let f = Fixture::new();
        symlink("README.md", f.repo.join("unsupported-link")).expect("source symlink");
        let manager = f.manager();
        manager
            .create("strict-source", f.create_options())
            .expect_err("strict traversal must reject symlink");
        assert!(
            !f.managed.join(OVERLAY_ROOT_DIR_NAME).join("strict-source").exists(),
            "failed materialization must remove its captured identity tree"
        );
    }

    #[test]
    fn materialize_aggregate_failure_removes_partial_identity_tree() {
        let f = Fixture::new();
        fs::write(f.repo.join("aggregate-a"), b"1234").expect("aggregate first file");
        fs::write(f.repo.join("aggregate-b"), b"56789").expect("aggregate overflow file");
        let _limit = set_overlay_copy_limit(8);

        f.manager()
            .create("aggregate-cleanup", f.create_options())
            .expect_err("aggregate limit must fail materialization");

        assert!(
            !f.managed
                .join(OVERLAY_ROOT_DIR_NAME)
                .join("aggregate-cleanup")
                .exists(),
            "partial streamed tree and staged siblings must be removed"
        );
    }

    fn tree_logical_bytes(directory: &Path) -> u64 {
        fs::read_dir(directory)
            .expect("tree directory")
            .map(|entry| {
                let entry = entry.expect("tree entry");
                let metadata = entry.metadata().expect("tree metadata");
                if metadata.is_dir() {
                    tree_logical_bytes(&entry.path())
                } else {
                    metadata.len()
                }
            })
            .sum()
    }
    impl Drop for IntegrationHookGuard {
        fn drop(&mut self) {
            set_integration_hook(None);
        }
    }

    fn fail_integration_at(point: IntegrationHookPoint) -> IntegrationHookGuard {
        set_integration_hook(Some(Arc::new(move |current| {
            if current == point {
                Err(WorktreeError::Other(anyhow!("injected integration failure")))
            } else {
                Ok(())
            }
        })));
        IntegrationHookGuard
    }

    #[derive(Debug, Eq, PartialEq)]
    struct RepoSnapshot {
        head: String,
        index: Vec<u8>,
        status: String,
        files: Vec<(PathBuf, Vec<u8>, fs::FileType, u32)>,
    }

    fn repo_snapshot(repo: &Path) -> RepoSnapshot {
        let mut files = Vec::new();
        collect_repo_files(repo, repo, &mut files);
        files.sort_by(|left, right| left.0.cmp(&right.0));
        RepoSnapshot {
            head: fixture_git(repo, &["rev-parse", "HEAD"]),
            index: fs::read(repo.join(".git/index")).expect("index bytes"),
            status: fixture_git(repo, &["status", "--porcelain=v1", "-z"]),
            files,
        }
    }

    fn collect_repo_files(root: &Path, directory: &Path, files: &mut Vec<(PathBuf, Vec<u8>, fs::FileType, u32)>) {
        for entry in fs::read_dir(directory).expect("read snapshot directory") {
            let entry = entry.expect("snapshot entry");
            let path = entry.path();
            let relative = path.strip_prefix(root).expect("snapshot relative").to_path_buf();
            if relative == Path::new(".git") || relative.starts_with(".git") {
                continue;
            }
            let metadata = fs::symlink_metadata(&path).expect("snapshot metadata");
            let file_type = metadata.file_type();
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::MetadataExt;
                metadata.mode()
            };
            #[cfg(not(unix))]
            let mode = u32::from(metadata.permissions().readonly());
            if file_type.is_dir() {
                files.push((relative, Vec::new(), file_type, mode));
                collect_repo_files(root, &path, files);
            } else if file_type.is_symlink() {
                files.push((
                    relative,
                    fs::read_link(&path)
                        .expect("snapshot symlink")
                        .as_os_str()
                        .to_string_lossy()
                        .as_bytes()
                        .to_vec(),
                    file_type,
                    mode,
                ));
            } else {
                files.push((relative, fs::read(&path).expect("snapshot file"), file_type, mode));
            }
        }
    }

    fn prepared_overlay(f: &Fixture, workflow_id: &str) -> (OverlayWorkflowManager, WorkflowWorktreeIdentity) {
        let manager = f.manager();
        let identity = manager
            .create(workflow_id, f.create_options())
            .expect("create transactional overlay");
        fs::remove_file(identity.worktree_path.join("README.md")).expect("overlay deletion");
        fs::write(identity.worktree_path.join("first.txt"), "first\n").expect("first overlay file");
        fs::write(identity.worktree_path.join("second.txt"), "second\n").expect("second overlay file");
        fixture_git(&identity.worktree_path, &["add", "-A", "--", "."]);
        fixture_git(&identity.worktree_path, &["commit", "-m", "transaction candidate"]);
        (manager, identity)
    }

    #[test]
    fn overlay_copy_failure_restores_source_exactly() {
        let f = Fixture::new();
        let (manager, _) = prepared_overlay(&f, "wf-copy-failure");
        let before = repo_snapshot(&f.repo);
        let _guard = fail_integration_at(IntegrationHookPoint::MidCopy);
        manager
            .integrate("wf-copy-failure", IntegrateOptions::default())
            .expect_err("mid-copy failure");
        assert_eq!(repo_snapshot(&f.repo), before);
    }

    #[test]
    fn overlay_commit_failure_restores_source_and_index() {
        let f = Fixture::new();
        let (manager, _) = prepared_overlay(&f, "wf-commit-failure");
        let before = repo_snapshot(&f.repo);
        let _guard = fail_integration_at(IntegrationHookPoint::Commit);
        manager
            .integrate("wf-commit-failure", IntegrateOptions::default())
            .expect_err("candidate commit failure");
        assert_eq!(repo_snapshot(&f.repo), before);
    }

    #[test]
    fn candidate_cleanup_preserves_swapped_foreign_directory_and_reports_conflict() {
        let f = Fixture::new();
        let (manager, _) = prepared_overlay(&f, "wf-candidate-swap");
        let overlay_root = f.managed.join(OVERLAY_ROOT_DIR_NAME);
        set_integration_hook(Some(Arc::new(move |point| {
            if point == IntegrationHookPoint::Commit {
                let candidate = fs::read_dir(&overlay_root)
                    .expect("candidate parent")
                    .map(|entry| entry.expect("candidate entry").path())
                    .find(|path| {
                        path.file_name()
                            .is_some_and(|name| name.to_string_lossy().starts_with(".integrate-wf-candidate-swap-"))
                    })
                    .expect("captured candidate");
                let displaced = candidate.with_extension("captured");
                fs::rename(&candidate, &displaced).expect("displace captured candidate");
                fs::create_dir(&candidate).expect("install foreign candidate");
                fs::write(candidate.join("foreign.txt"), b"foreign\n").expect("foreign marker");
                return Err(WorktreeError::Other(anyhow!("injected candidate failure")));
            }
            Ok(())
        })));
        let _guard = IntegrationHookGuard;

        let error = manager
            .integrate("wf-candidate-swap", IntegrateOptions::default())
            .expect_err("swapped candidate cleanup must conflict");

        let message = error.to_string();
        assert!(message.contains("injected candidate failure"), "missing original error: {message}");
        assert!(message.contains("candidate cleanup conflict"), "missing cleanup context: {message}");
        let foreign = fs::read_dir(f.managed.join(OVERLAY_ROOT_DIR_NAME))
            .expect("overlay root")
            .map(|entry| entry.expect("overlay entry").path())
            .find(|path| path.join("foreign.txt").is_file())
            .expect("foreign candidate preserved");
        assert_eq!(fs::read(foreign.join("foreign.txt")).expect("foreign bytes"), b"foreign\n");
    }

    #[test]
    fn overlay_concurrent_source_mutation_fails_without_committing_foreign_files() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let f = Fixture::new();
        let (manager, _) = prepared_overlay(&f, "wf-concurrent");
        let before = repo_snapshot(&f.repo);
        let repo = f.repo.clone();
        let hook_ran = Arc::new(AtomicBool::new(false));
        let hook_ran_in_call = hook_ran.clone();
        set_integration_hook(Some(Arc::new(move |point| {
            if point == IntegrationHookPoint::ConcurrentMutation {
                fs::write(repo.join("foreign.txt"), "foreign\n").expect("foreign mutation");
                hook_ran_in_call.store(true, Ordering::SeqCst);
            }
            Ok(())
        })));
        let _guard = IntegrationHookGuard;
        let error = manager
            .integrate("wf-concurrent", IntegrateOptions::default())
            .expect_err("concurrent mutation rejected");
        assert!(hook_ran.load(Ordering::SeqCst), "final race hook must run");
        assert!(matches!(error, WorktreeError::DirtyBase), "unexpected error: {error:?}");
        assert_eq!(fixture_git(&f.repo, &["rev-parse", "HEAD"]), before.head);
        assert_eq!(
            fs::read(f.repo.join(".git/index")).expect("index preserved"),
            before.index
        );
        assert_eq!(fs::read(f.repo.join("foreign.txt")).expect("foreign preserved"), b"foreign\n");
        assert!(!f.repo.join("first.txt").exists());
        assert!(fixture_git(&f.repo, &["status", "--porcelain=v1", "--", "foreign.txt"]).starts_with("??"));
        let mut after_files = repo_snapshot(&f.repo).files;
        after_files.retain(|(path, ..)| path != Path::new("foreign.txt"));
        assert_eq!(after_files, before.files, "pre-existing worktree bytes and modes changed");
    }

    #[test]
    fn overlay_applied_marker_failure_returns_applied_and_retry_repairs_once() {
        let f = Fixture::new();
        let (manager, _) = prepared_overlay(&f, "wf-marker-failure");
        let _guard = fail_integration_at(IntegrationHookPoint::AppliedMarker);
        let first = manager
            .integrate("wf-marker-failure", IntegrateOptions::default())
            .expect("post-ff marker failure remains applied");
        let IntegrateOutcome::Applied {
            result_commit: applied,
            ..
        } = first
        else {
            panic!("marker failure must return Applied, got {first:?}");
        };
        assert_eq!(fixture_git(&f.repo, &["rev-parse", "HEAD"]), applied);
        drop(_guard);

        let retry = manager
            .integrate("wf-marker-failure", IntegrateOptions::default())
            .expect("retry recovers commit trailer and repairs markers");
        let IntegrateOutcome::Applied {
            result_commit: recovered,
            ..
        } = retry
        else {
            panic!("marker recovery must return Applied, got {retry:?}");
        };
        assert_eq!(recovered, applied);
        assert_eq!(fixture_git(&f.repo, &["rev-parse", "HEAD"]), applied);
        assert_eq!(
            fixture_git(
                &f.repo,
                &[
                    "log",
                    "--format=%H",
                    "--grep",
                    "^workflow wf-marker-failure: integrate overlay changes$",
                ],
            )
            .lines()
            .count(),
            1,
            "marker retry must not create a duplicate integration commit"
        );
        let catalog: serde_json::Value = serde_json::from_slice(
            &fs::read(overlay_catalog_path(&f.managed)).expect("repaired catalog"),
        )
        .expect("parse repaired catalog");
        assert_eq!(
            catalog["entries"]["wf-marker-failure"]["appliedSourceCommit"],
            serde_json::Value::String(applied)
        );
    }

    #[test]
    fn overlay_candidate_failures_before_commit_leave_source_exactly_unchanged() {
        for (suffix, point) in [
            ("delete", IntegrationHookPoint::AfterDeletion),
            ("copy", IntegrationHookPoint::AfterCopy),
            ("add", IntegrationHookPoint::Add),
        ] {
            let f = Fixture::new();
            let workflow_id = format!("wf-{suffix}");
            let (manager, _) = prepared_overlay(&f, &workflow_id);
            let before = repo_snapshot(&f.repo);
            let _guard = fail_integration_at(point);
            manager
                .integrate(&workflow_id, IntegrateOptions::default())
                .expect_err("candidate failure");
            assert_eq!(repo_snapshot(&f.repo), before, "failure point {point:?}");
        }
    }

    #[test]
    fn overlay_no_git_integration_preserves_source_exactly() {
        let f = Fixture::new();
        let (manager, _) = prepared_overlay(&f, "wf-no-git");
        let before = repo_snapshot(&f.repo);
        let hidden_git = f.repo.join(".git.hidden");
        fs::rename(f.repo.join(".git"), &hidden_git).expect("hide git metadata");
        let source_before = collect_plain_tree(&f.repo);
        let error = manager
            .integrate("wf-no-git", IntegrateOptions::default())
            .expect_err("non-git source rejected");
        assert!(matches!(error, WorktreeError::NotGit));
        assert_eq!(collect_plain_tree(&f.repo), source_before);
        fs::rename(hidden_git, f.repo.join(".git")).expect("restore git metadata");
        assert_eq!(repo_snapshot(&f.repo), before);
    }

    #[test]
    fn overlay_unsupported_options_fail_before_source_or_managed_mutation() {
        for (workflow_id, options, named) in [
            (
                "wf-rebase-option",
                IntegrateOptions {
                    strategy: IntegrateStrategy::Rebase,
                    ..IntegrateOptions::default()
                },
                "merge",
            ),
            (
                "wf-dirty-option",
                IntegrateOptions {
                    require_clean_base: false,
                    ..IntegrateOptions::default()
                },
                "clean source",
            ),
        ] {
            let f = Fixture::new();
            let (manager, _) = prepared_overlay(&f, workflow_id);
            let source_before = repo_snapshot(&f.repo);
            let managed_before = collect_plain_tree(&f.managed);
            let error = manager
                .integrate(workflow_id, options)
                .expect_err("unsupported option rejected");
            assert!(error.to_string().contains(named), "error must name unsupported option: {error:?}");
            assert_eq!(repo_snapshot(&f.repo), source_before);
            assert_eq!(collect_plain_tree(&f.managed), managed_before);
        }
    }

    #[test]
    fn overlay_publication_races_preserve_foreign_files() {
        use std::sync::atomic::{AtomicBool, Ordering};

        for (case, source_contents, overlay_contents) in [
            ("overwrite", Some("base tracked\n"), Some("overlay replacement\n")),
            ("delete", Some("base tracked\n"), None),
            ("create", None, Some("overlay creation\n")),
        ] {
            let f = Fixture::new();
            if let Some(contents) = source_contents {
                commit_file(&f.repo, "race.txt", contents, "publication race base");
            }
            let manager = f.manager();
            let workflow_id = format!("wf-publication-{case}");
            let identity = manager
                .create(&workflow_id, f.create_options())
                .expect("create publication overlay");
            match overlay_contents {
                Some(contents) => fs::write(identity.worktree_path.join("race.txt"), contents)
                    .expect("overlay publication file"),
                None => fs::remove_file(identity.worktree_path.join("race.txt"))
                    .expect("overlay publication deletion"),
            }
            fixture_git(&identity.worktree_path, &["add", "-A", "--", "."]);
            fixture_git(&identity.worktree_path, &["commit", "-m", "publication candidate"]);
            let before = repo_snapshot(&f.repo);
            let repo = f.repo.clone();
            let hook_ran = Arc::new(AtomicBool::new(false));
            let hook_ran_in_call = hook_ran.clone();
            set_integration_hook(Some(Arc::new(move |point| {
                if point == IntegrationHookPoint::MergeMutation {
                    fs::write(repo.join("race.txt"), "foreign conflict\n")
                        .expect("merge mutation");
                    hook_ran_in_call.store(true, Ordering::SeqCst);
                }
                Ok(())
            })));
            let _guard = IntegrationHookGuard;
            let error = manager
                .integrate(&workflow_id, IntegrateOptions::default())
                .expect_err("git rejects publication race");
            assert!(hook_ran.load(Ordering::SeqCst), "merge hook must run for {case}");
            assert!(matches!(error, WorktreeError::CommandFailed { .. }), "unexpected {case} error: {error:?}");
            assert_eq!(fixture_git(&f.repo, &["rev-parse", "HEAD"]), before.head);
            assert_eq!(fs::read(f.repo.join(".git/index")).expect("index preserved"), before.index);
            assert_eq!(fs::read(f.repo.join("race.txt")).expect("foreign preserved"), b"foreign conflict\n");
        }
    }

    #[test]
    fn overlay_publication_foreign_deletion_is_rejected_before_merge() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let f = Fixture::new();
        commit_file(&f.repo, "race.txt", "base tracked\n", "publication deletion base");
        let manager = f.manager();
        let identity = manager
            .create("wf-publication-delete-foreign", f.create_options())
            .expect("create publication overlay");
        fs::write(identity.worktree_path.join("race.txt"), "overlay replacement\n")
            .expect("overlay replacement");
        fixture_git(&identity.worktree_path, &["add", "race.txt"]);
        fixture_git(&identity.worktree_path, &["commit", "-m", "publication replacement"]);
        let before = repo_snapshot(&f.repo);
        let repo = f.repo.clone();
        let hook_ran = Arc::new(AtomicBool::new(false));
        let hook_ran_in_call = hook_ran.clone();
        set_integration_hook(Some(Arc::new(move |point| {
            if point == IntegrationHookPoint::PublicationMutation {
                fs::remove_file(repo.join("race.txt")).expect("foreign deletion");
                hook_ran_in_call.store(true, Ordering::SeqCst);
            }
            Ok(())
        })));
        let _guard = IntegrationHookGuard;
        let error = manager
            .integrate("wf-publication-delete-foreign", IntegrateOptions::default())
            .expect_err("foreign deletion rejected before merge");
        assert!(hook_ran.load(Ordering::SeqCst), "publication hook must run");
        assert!(matches!(error, WorktreeError::DirtyBase), "unexpected error: {error:?}");
        assert_eq!(fixture_git(&f.repo, &["rev-parse", "HEAD"]), before.head);
        assert_eq!(fs::read(f.repo.join(".git/index")).expect("index preserved"), before.index);
        assert!(!f.repo.join("race.txt").exists(), "foreign deletion must remain");
        assert!(!f.repo.join("second.txt").exists(), "overlay must not land");
    }

    fn collect_plain_tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        fn walk(root: &Path, directory: &Path, entries: &mut Vec<(PathBuf, Vec<u8>)>) {
            for entry in fs::read_dir(directory).expect("read plain tree") {
                let entry = entry.expect("plain tree entry");
                let path = entry.path();
                let relative = path.strip_prefix(root).expect("plain relative").to_path_buf();
                if entry.file_type().expect("plain type").is_dir() {
                    walk(root, &path, entries);
                } else {
                    entries.push((relative, fs::read(&path).expect("plain bytes")));
                }
            }
        }
        let mut entries = Vec::new();
        walk(root, root, &mut entries);
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        entries
    }

    fn init_repo(repo: &Path) {
        fs::create_dir_all(repo).expect("source directory");
        fixture_git(repo, &["init"]);
        fixture_git(repo, &["config", "user.name", "Pi Test"]);
        fixture_git(repo, &["config", "user.email", "pi@example.test"]);
        fs::write(repo.join("README.md"), "base\n").expect("base file");
        fixture_git(repo, &["add", "README.md"]);
        fixture_git(repo, &["commit", "-m", "initial"]);
    }

    fn commit_file(cwd: &Path, relative: &str, contents: &str, message: &str) -> String {
        let path = cwd.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("file parent");
        }
        fs::write(&path, contents).expect("write commit file");
        fixture_git(cwd, &["add", "--", relative]);
        fixture_git(cwd, &["commit", "-m", message]);
        fixture_git(cwd, &["rev-parse", "HEAD"])
    }

    fn overlay_catalog_path(managed: &Path) -> PathBuf {
        managed.join(CATALOG_DIR_NAME).join(CATALOG_FILE_NAME)
    }

    fn patch_overlay_catalog_entry(
        managed: &Path,
        workflow_id: &str,
        patch: impl FnOnce(&mut serde_json::Value),
    ) {
        let catalog_path = overlay_catalog_path(managed);
        let mut catalog: serde_json::Value =
            serde_json::from_slice(&fs::read(&catalog_path).expect("catalog bytes"))
                .expect("parse overlay catalog");
        patch(&mut catalog["entries"][workflow_id]);
        fs::write(
            &catalog_path,
            serde_json::to_vec(&catalog).expect("serialize overlay catalog"),
        )
        .expect("write overlay catalog");
    }

    struct Fixture {
        sandbox: TempDir,
        repo: PathBuf,
        managed: PathBuf,
        _agent: TempDir,
        _sessions: TempDir,
        _faux: pi_ai::providers::FauxProviderRegistration,
        application: crate::Application,
        source_lease: CapabilityRootLease,
        managed_lease: CapabilityRootLease,
    }

    impl Fixture {
        fn new() -> Self {
            let sandbox = TempDir::new().expect("temporary sandbox");
            let repo = sandbox.path().join("source");
            let managed = sandbox.path().join("managed");
            init_repo(&repo);
            let agent = TempDir::new().expect("agent directory");
            let sessions = TempDir::new().expect("session directory");
            fs::write(
                agent.path().join("settings.json"),
                r#"{"sandbox":{"mode":"danger-full-access"}}"#,
            )
            .expect("sandbox settings");
            let mut resources = crate::ResourceManagerOptions::new(&repo);
            resources.agent_dir = agent.path().to_path_buf();
            resources.project_trust_override = Some(true);
            resources.disable_extensions = true;
            resources.disable_skills = true;
            resources.disable_prompt_templates = true;
            resources.disable_themes = true;
            resources.disable_context_files = true;
            let resources = crate::ResourceManager::new(resources).expect("isolated resources");
            let faux = pi_ai::providers::register_faux_provider(Default::default());
            let workspace = crate::WorkspaceRoots::new(&repo, Vec::<PathBuf>::new())
                .expect("isolated workspace");
            let session =
                crate::Session::new_with_additional_tools_filtered_discovery_workspace_and_uri(
                    crate::SessionOptions {
                        model: faux.model(None).expect("overlay fixture model"),
                        cwd: repo.clone(),
                        system_prompt: String::new(),
                        thinking_level: pi_agent::ThinkingLevel::Off,
                        api_key: String::new(),
                        compaction: None,
                        stream_options: Default::default(),
                        tools: Some(Vec::new()),
                        before_tool_call: None,
                        after_tool_call: None,
                        stream_fn: None,
                        auth_resolver: None,
                        auth_manager: None,
                    },
                    Vec::new(),
                    crate::ToolSelection::default(),
                    crate::ResourceDiscovery::Disabled,
                    workspace,
                    None,
                )
                .expect("test session");
            session.set_session_dir(sessions.path().to_path_buf());
            let application = std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("application runtime")
                    .block_on(async {
                        session
                            .attach_resources(resources)
                            .await
                            .expect("attach isolated resources");
                        crate::Application::from_runtime_candidate(
                            crate::ApplicationRuntimeCandidate::new(session),
                        )
                        .await
                        .expect("publish isolated application")
                    })
            })
            .join()
            .expect("application fixture thread");
            let capabilities = application
                .capture_generation()
                .sandbox()
                .capabilities()
                .clone();
            let source_lease = capabilities
                .open_root(
                    CapabilityRootPurpose::ProjectResources,
                    pi_sandbox::AbsolutePath::parse(repo.clone()).expect("absolute source root"),
                    CapabilityAccess::ReadWrite,
                    RootCreation::Existing,
                )
                .expect("source lease");
            let managed_lease = capabilities
                .open_root(
                    CapabilityRootPurpose::WorkflowWorktrees,
                    pi_sandbox::AbsolutePath::parse(managed.clone())
                        .expect("absolute managed root"),
                    CapabilityAccess::PrivateReadWrite,
                    RootCreation::CreateDirectory,
                )
                .expect("managed lease");
            Self {
                sandbox,
                repo,
                managed,
                _agent: agent,
                _sessions: sessions,
                _faux: faux,
                application,
                source_lease,
                managed_lease,
            }
        }

        fn manager(&self) -> OverlayWorkflowManager {
            OverlayWorkflowManager::new(self.source_lease.clone(), self.managed_lease.clone())
                .with_timeout(TEST_TIMEOUT)
                .with_backend_candidates(vec![OverlayBackend::Rcopy])
                .with_process_context(
                    self.application
                        .governed_process_context()
                        .with_repository_test_executor(repository_test_executor()),
                )
        }

        fn create_options(&self) -> CreateWorktreeOptions {
            CreateWorktreeOptions {
                managed_root: self.managed.clone(),
                base_commit: None,
                timeout: Some(TEST_TIMEOUT),
            }
        }
    }

    #[test]
    fn overlay_create_inspect_integrate_remove_roundtrip() {
        let f = Fixture::new();
        let manager = f.manager();
        let identity = manager
            .create("wf-roundtrip", f.create_options())
            .expect("create overlay workflow");
        assert_eq!(identity.workflow_id, "wf-roundtrip");
        assert_eq!(
            identity.branch,
            format!("{OVERLAY_BRANCH_PREFIX}wf-roundtrip")
        );
        assert_eq!(
            identity.worktree_path,
            f.managed
                .join(OVERLAY_ROOT_DIR_NAME)
                .join("wf-roundtrip")
                .join("merged"),
            "worktree path must be the exact retained managed-root descendant"
        );
        assert_eq!(
            fs::read_to_string(identity.worktree_path.join("README.md")).expect("merged README"),
            "base\n",
            "rcopy materialization must copy the source tree into the merged view"
        );

        // Strict identity: a foreign identity must be rejected without touching
        // the recorded checkout.
        let mut foreign = identity.clone();
        foreign.head_commit = "0000000000000000000000000000000000000000".to_owned();
        let error = manager
            .remove(&foreign)
            .expect_err("foreign identity rejected");
        assert!(
            matches!(error, WorktreeError::OwnershipMismatch { .. }),
            "strict identity check must reject a mismatched head commit"
        );

        let status = manager.inspect("wf-roundtrip").expect("inspect");
        assert_eq!(status.identity, identity);
        assert!(!status.dirty, "fresh overlay must not be dirty");
        assert_eq!(status.ahead_commits, 0);

        // Commit a workflow change inside the merged view through governed git.
        let workflow_head = commit_file(
            &identity.worktree_path,
            "feature.txt",
            "workflow change\n",
            "workflow feature",
        );

        let (refreshed, cwd) = manager
            .verify_owned_current("wf-roundtrip")
            .expect("verify owned current advances head");
        assert_eq!(refreshed.head_commit, workflow_head);
        assert_eq!(cwd.path(), &identity.worktree_path);
        assert_eq!(cwd.workflow_id(), "wf-roundtrip");

        let dirty_status = manager
            .inspect("wf-roundtrip")
            .expect("inspect after workflow commit");
        assert!(
            !dirty_status.dirty,
            "a committed workflow change leaves the merged working tree clean"
        );
        assert_eq!(
            dirty_status.ahead_commits, 1,
            "one workflow commit ahead of the base"
        );

        let outcome = manager
            .integrate(
                "wf-roundtrip",
                IntegrateOptions {
                    strategy: IntegrateStrategy::Merge,
                    require_clean_base: true,
                    timeout: Some(TEST_TIMEOUT),
                },
            )
            .expect("integrate");
        match outcome {
            IntegrateOutcome::Applied { result_commit, .. } => {
                assert_ne!(
                    result_commit, identity.base_commit,
                    "integration must advance source HEAD"
                );
                assert_eq!(
                    fs::read_to_string(f.repo.join("feature.txt")).expect("source feature"),
                    "workflow change\n",
                    "copy-back must land the workflow change in the source tree"
                );
            }
            other => panic!("integration must apply, got {other:?}"),
        }

        manager.remove(&refreshed).expect("remove");
        assert!(
            !f.managed
                .join(OVERLAY_ROOT_DIR_NAME)
                .join("wf-roundtrip")
                .exists(),
            "remove must delete the identity directory"
        );
        assert!(
            manager.list().expect("list").is_empty(),
            "remove must drop the catalog entry"
        );
    }

    #[test]
    fn overlay_empty_branch_integration_is_missing_commits() {
        let f = Fixture::new();
        let manager = f.manager();
        let identity = manager
            .create("wf-empty", f.create_options())
            .expect("create overlay workflow");

        let error = manager
            .integrate("wf-empty", IntegrateOptions::default())
            .expect_err("empty overlay must not apply");
        assert!(
            matches!(error, WorktreeError::MissingCommits),
            "empty overlay must be MissingCommits, got {error:?}"
        );
        let message = error.to_string();
        assert!(
            message.contains("no commits"),
            "missing-commits message must name commits: {message}"
        );
        assert!(
            !message.contains(f.repo.to_string_lossy().as_ref()),
            "missing-commits must not leak the source path: {message}"
        );
        assert!(!f.repo.join("untracked.txt").exists());
        let _ = identity;
    }

    #[test]
    fn overlay_first_integration_refuses_source_head_advance() {
        let f = Fixture::new();
        let manager = f.manager();
        let identity = manager
            .create("wf-source-advanced", f.create_options())
            .expect("create overlay workflow");
        commit_file(
            &identity.worktree_path,
            "feature.txt",
            "overlay change\n",
            "overlay feature",
        );
        let source_commit = commit_file(&f.repo, "later.txt", "source later\n", "source advance");

        let error = manager
            .integrate("wf-source-advanced", IntegrateOptions::default())
            .expect_err("stale overlay must not overwrite advanced source");
        assert!(matches!(error, WorktreeError::DirtyBase));
        assert_eq!(fixture_git(&f.repo, &["rev-parse", "HEAD"]), source_commit);
        assert_eq!(
            fs::read_to_string(f.repo.join("later.txt")).expect("source advance preserved"),
            "source later\n"
        );
        assert!(!f.repo.join("feature.txt").exists());
    }

    #[test]
    fn overlay_uncommitted_files_without_commits_are_missing_commits() {
        let f = Fixture::new();
        let manager = f.manager();
        let identity = manager
            .create("wf-uncommitted", f.create_options())
            .expect("create overlay workflow");
        fs::write(
            identity.worktree_path.join("untracked.txt"),
            "not committed\n",
        )
        .expect("untracked overlay file");

        let error = manager
            .integrate("wf-uncommitted", IntegrateOptions::default())
            .expect_err("overlay files without commits must fail closed");
        assert!(
            matches!(error, WorktreeError::MissingCommits),
            "uncommitted overlay files with no commits must be MissingCommits, got {error:?}"
        );
        assert!(identity.worktree_path.join("untracked.txt").exists());
        assert!(!f.repo.join("untracked.txt").exists());
    }

    #[test]
    fn overlay_repeated_integration_of_committed_work_is_idempotent() {
        let f = Fixture::new();
        let manager = f.manager();
        let identity = manager
            .create("wf-repeat", f.create_options())
            .expect("create overlay workflow");
        commit_file(
            &identity.worktree_path,
            "feature.txt",
            "workflow change\n",
            "workflow feature",
        );

        let first = manager
            .integrate(
                "wf-repeat",
                IntegrateOptions {
                    strategy: IntegrateStrategy::Merge,
                    require_clean_base: true,
                    timeout: Some(TEST_TIMEOUT),
                },
            )
            .expect("first overlay integrate");
        let IntegrateOutcome::Applied {
            result_commit: first_commit,
            ..
        } = first
        else {
            panic!("expected applied overlay integration, got {first:?}");
        };
        assert_ne!(first_commit, identity.base_commit);
        assert_eq!(
            fs::read_to_string(f.repo.join("feature.txt")).expect("source feature"),
            "workflow change\n"
        );

        let second = manager
            .integrate(
                "wf-repeat",
                IntegrateOptions {
                    strategy: IntegrateStrategy::Merge,
                    require_clean_base: true,
                    timeout: Some(TEST_TIMEOUT),
                },
            )
            .expect("repeated overlay integrate");
        let IntegrateOutcome::Applied {
            result_commit: second_commit,
            ..
        } = second
        else {
            panic!("retry of committed overlay work must stay Applied, got {second:?}");
        };
        assert_eq!(
            second_commit, first_commit,
            "idempotent overlay retry must not create a second integrate commit"
        );
    }

    #[test]
    fn overlay_later_workflow_commit_is_applied_then_source_advance_is_noop() {
        let f = Fixture::new();
        let manager = f.manager();
        let identity = manager
            .create("wf-later", f.create_options())
            .expect("create overlay workflow");
        commit_file(
            &identity.worktree_path,
            "feature.txt",
            "first\n",
            "first overlay",
        );
        let first = manager
            .integrate("wf-later", IntegrateOptions::default())
            .expect("first overlay integrate");
        let IntegrateOutcome::Applied {
            result_commit: first_commit,
            ..
        } = first
        else {
            panic!("expected applied, got {first:?}");
        };

        commit_file(
            &identity.worktree_path,
            "feature.txt",
            "second\n",
            "second overlay",
        );
        let second = manager
            .integrate("wf-later", IntegrateOptions::default())
            .expect("second overlay integrate");
        let IntegrateOutcome::Applied {
            result_commit: second_commit,
            ..
        } = second
        else {
            panic!("expected applied second, got {second:?}");
        };
        assert_ne!(second_commit, first_commit);
        assert_eq!(
            fs::read_to_string(f.repo.join("feature.txt")).expect("second overlay"),
            "second\n"
        );
        commit_file(&f.repo, "later.txt", "source later\n", "later source");
        let retry = manager
            .integrate("wf-later", IntegrateOptions::default())
            .expect("retry after source advanced");
        let IntegrateOutcome::Applied {
            result_commit: retry_commit,
            ..
        } = retry
        else {
            panic!("retry must stay Applied, got {retry:?}");
        };
        assert_eq!(retry_commit, second_commit);
        assert_eq!(
            fs::read_to_string(f.repo.join("later.txt")).expect("later source preserved"),
            "source later\n"
        );
        assert_eq!(
            fs::read_to_string(f.repo.join("feature.txt")).expect("overlay still second"),
            "second\n"
        );
    }

    #[test]
    fn overlay_pending_old_integration_commit_does_not_prove_new_head() {
        let f = Fixture::new();
        let manager = f.manager();
        let identity = manager
            .create("wf-false-positive", f.create_options())
            .expect("create overlay workflow");
        commit_file(
            &identity.worktree_path,
            "feature.txt",
            "first\n",
            "first overlay",
        );
        let first = manager
            .integrate("wf-false-positive", IntegrateOptions::default())
            .expect("first overlay integrate");
        let IntegrateOutcome::Applied {
            result_commit: first_commit,
            ..
        } = first
        else {
            panic!("expected applied, got {first:?}");
        };

        let overlay_b = commit_file(
            &identity.worktree_path,
            "feature.txt",
            "second\n",
            "second overlay",
        );
        let source_head = fixture_git(&f.repo, &["rev-parse", "HEAD"]);
        patch_overlay_catalog_entry(&f.managed, "wf-false-positive", |entry| {
            entry["pendingOverlayHead"] = serde_json::Value::String(overlay_b);
            entry["pendingSourceHead"] = serde_json::Value::String(source_head.clone());
            entry["appliedOverlayHead"] = serde_json::Value::Null;
            entry["appliedSourceCommit"] = serde_json::Value::Null;
        });

        let second = manager
            .integrate("wf-false-positive", IntegrateOptions::default())
            .expect("second overlay integrate must not treat old A as proof of B");
        let IntegrateOutcome::Applied {
            result_commit: second_commit,
            ..
        } = second
        else {
            panic!("expected applied second, got {second:?}");
        };
        assert_ne!(
            second_commit, first_commit,
            "old integration commit must not prove the later overlay HEAD applied"
        );
        assert_eq!(
            fs::read_to_string(f.repo.join("feature.txt")).expect("second overlay landed"),
            "second\n"
        );
        let _ = source_head;
    }

    #[test]
    fn overlay_crash_after_commit_before_applied_marker_recovers_exact_head() {
        let f = Fixture::new();
        let manager = f.manager();
        let identity = manager
            .create("wf-crash-commit", f.create_options())
            .expect("create overlay workflow");
        let overlay_head = commit_file(
            &identity.worktree_path,
            "feature.txt",
            "crash recover\n",
            "crash overlay",
        );
        let pending_source = fixture_git(&f.repo, &["rev-parse", "HEAD"]);
        fs::write(f.repo.join("feature.txt"), "crash recover\n").expect("copied overlay file");
        fixture_git(&f.repo, &["add", "-A", "--", "."]);
        fixture_git(
            &f.repo,
            &[
                "commit",
                "-m",
                &format!(
                    "workflow wf-crash-commit: integrate overlay changes\noverlay-head:{overlay_head}"
                ),
            ],
        );
        let committed = fixture_git(&f.repo, &["rev-parse", "HEAD"]);
        patch_overlay_catalog_entry(&f.managed, "wf-crash-commit", |entry| {
            entry["pendingOverlayHead"] = serde_json::Value::String(overlay_head.clone());
            entry["pendingSourceHead"] = serde_json::Value::String(pending_source);
            entry["pendingCopyComplete"] = serde_json::Value::Bool(true);
            entry["appliedOverlayHead"] = serde_json::Value::Null;
            entry["appliedSourceCommit"] = serde_json::Value::Null;
        });

        let outcome = manager
            .integrate("wf-crash-commit", IntegrateOptions::default())
            .expect("crash-after-commit recovery");
        let IntegrateOutcome::Applied { result_commit, .. } = outcome else {
            panic!("expected applied recovery, got {outcome:?}");
        };
        assert_eq!(
            result_commit, committed,
            "recovery must reuse the exact overlay-head-bound commit"
        );
        assert_eq!(
            fixture_git(&f.repo, &["rev-parse", "HEAD"]),
            committed,
            "recovery must not create a second integrate commit"
        );
        let _ = identity;
    }

    #[test]
    fn overlay_retry_after_source_advance_and_dirty_preserves_result() {
        let f = Fixture::new();
        let manager = f.manager();
        let identity = manager
            .create("wf-advance-dirty", f.create_options())
            .expect("create overlay workflow");
        commit_file(
            &identity.worktree_path,
            "feature.txt",
            "applied\n",
            "overlay feature",
        );
        let first = manager
            .integrate("wf-advance-dirty", IntegrateOptions::default())
            .expect("first overlay integrate");
        let IntegrateOutcome::Applied {
            result_commit: first_commit,
            ..
        } = first
        else {
            panic!("expected applied, got {first:?}");
        };
        commit_file(&f.repo, "later.txt", "later\n", "later source");
        fs::write(f.repo.join("uncommitted.txt"), "dirty\n").expect("dirty source");
        let retry = manager
            .integrate("wf-advance-dirty", IntegrateOptions::default())
            .expect("retry after source advanced and became dirty");
        let IntegrateOutcome::Applied {
            result_commit: retry_commit,
            ..
        } = retry
        else {
            panic!("retry must stay Applied, got {retry:?}");
        };
        assert_eq!(retry_commit, first_commit);
        assert_eq!(
            fs::read_to_string(f.repo.join("later.txt")).expect("later source preserved"),
            "later\n"
        );
        assert_eq!(
            fs::read_to_string(f.repo.join("uncommitted.txt")).expect("dirty preserved"),
            "dirty\n"
        );
        assert_eq!(
            fs::read_to_string(f.repo.join("feature.txt")).expect("overlay still applied"),
            "applied\n"
        );
    }

    #[test]
    fn overlay_pending_marker_mismatch_does_not_recover_wrong_head() {
        let f = Fixture::new();
        let manager = f.manager();
        let identity = manager
            .create("wf-mismatch", f.create_options())
            .expect("create overlay workflow");
        let overlay_a = commit_file(
            &identity.worktree_path,
            "feature.txt",
            "first\n",
            "first overlay",
        );
        let pending_source = fixture_git(&f.repo, &["rev-parse", "HEAD"]);
        fs::write(f.repo.join("feature.txt"), "first\n").expect("copied first overlay");
        fixture_git(&f.repo, &["add", "-A", "--", "."]);
        fixture_git(
            &f.repo,
            &[
                "commit",
                "-m",
                &format!(
                    "workflow wf-mismatch: integrate overlay changes\noverlay-head:{overlay_a}"
                ),
            ],
        );
        let first_commit = fixture_git(&f.repo, &["rev-parse", "HEAD"]);
        let overlay_b = commit_file(
            &identity.worktree_path,
            "feature.txt",
            "second\n",
            "second overlay",
        );
        patch_overlay_catalog_entry(&f.managed, "wf-mismatch", |entry| {
            entry["pendingOverlayHead"] = serde_json::Value::String(overlay_b);
            entry["pendingSourceHead"] = serde_json::Value::String(pending_source);
            entry["appliedOverlayHead"] = serde_json::Value::Null;
            entry["appliedSourceCommit"] = serde_json::Value::Null;
        });

        let second = manager
            .integrate("wf-mismatch", IntegrateOptions::default())
            .expect("mismatched pending head must copy the new overlay");
        let IntegrateOutcome::Applied {
            result_commit: second_commit,
            ..
        } = second
        else {
            panic!("expected applied second, got {second:?}");
        };
        assert_ne!(
            second_commit, first_commit,
            "an overlay-head:A commit must not satisfy pending overlay HEAD B"
        );
        assert_eq!(
            fs::read_to_string(f.repo.join("feature.txt")).expect("second overlay landed"),
            "second\n"
        );
    }

    #[test]
    fn overlay_persisted_backend_recovery_restores_recorded_workflow() {
        let f = Fixture::new();
        let first = f.manager();
        let identity = first
            .create("wf-restore", f.create_options())
            .expect("create overlay workflow");
        let workflow_head = commit_file(
            &identity.worktree_path,
            "persisted.txt",
            "persisted change\n",
            "persisted workflow change",
        );
        let recorded = first
            .verify_owned_current("wf-restore")
            .expect("record advanced head")
            .0;

        // Simulate a process restart: rebuild the manager from the same
        // retained capabilities and ownership catalog.
        let restored = f.manager();
        let listed = restored.list().expect("list after restart");
        assert_eq!(
            listed.len(),
            1,
            "catalog must persist across manager rebuilds"
        );
        assert_eq!(
            listed[0], recorded,
            "persisted catalog reflects the advanced workflow head"
        );

        let (reloaded, cwd) = restored
            .verify_owned_current("wf-restore")
            .expect("restore re-establishes the workflow");
        assert_eq!(reloaded, recorded);
        assert_eq!(reloaded.head_commit, workflow_head);
        assert_eq!(cwd.path(), &identity.worktree_path);

        let status = restored
            .inspect("wf-restore")
            .expect("inspect after restore");
        assert_eq!(status.identity.head_commit, workflow_head);

        restored.remove(&reloaded).expect("remove after restore");
        assert!(restored.list().expect("list").is_empty());
    }

    #[test]
    fn overlay_verify_owned_requires_exact_catalog_identity() {
        let f = Fixture::new();
        let manager = f.manager();
        let identity = manager
            .create("wf-strict", f.create_options())
            .expect("create");
        let exact = manager
            .verify_owned(&identity)
            .expect("exact identity verifies");
        assert_eq!(exact.path(), &identity.worktree_path);

        let mut tampered = identity.clone();
        tampered.base_commit = "deadbeef".to_owned();
        let error = manager
            .verify_owned(&tampered)
            .expect_err("tampered identity rejected");
        assert!(matches!(error, WorktreeError::OwnershipMismatch { .. }));

        let foreign = WorkflowWorktreeIdentity {
            workflow_id: "wf-foreign".to_owned(),
            ..identity.clone()
        };
        let error = manager
            .verify_owned(&foreign)
            .expect_err("foreign workflow rejected");
        assert!(matches!(error, WorktreeError::NotRegistered(_)));

        manager.remove(&identity).expect("cleanup");
    }

    #[test]
    fn overlay_create_rejects_managed_root_mismatch_and_duplicate() {
        let f = Fixture::new();
        let manager = f.manager();
        let mut options = f.create_options();
        options.managed_root = f.sandbox.path().join("elsewhere");
        let error = manager
            .create("wf-mismatch", options)
            .expect_err("managed root mismatch");
        assert!(matches!(error, WorktreeError::OwnershipMismatch { .. }));

        let identity = manager
            .create("wf-dup", f.create_options())
            .expect("first create");
        let error = manager
            .create("wf-dup", f.create_options())
            .expect_err("duplicate workflow");
        assert!(matches!(error, WorktreeError::OwnershipMismatch { .. }));
        manager.remove(&identity).expect("cleanup");
    }

    #[test]
    fn overlay_prune_stale_drops_entries_whose_merged_view_is_gone() {
        let f = Fixture::new();
        let manager = f.manager();
        let identity = manager
            .create("wf-prune", f.create_options())
            .expect("create");
        // Drop the merged view out from under the catalog via the retained
        // capability to simulate an externally removed checkout.
        f.managed_lease
            .remove_tree(&manager.merged_relative("wf-prune"))
            .expect("remove merged view");
        let removed = manager.prune_stale().expect("prune");
        assert_eq!(removed, vec!["wf-prune".to_owned()]);
        assert!(manager.list().expect("list").is_empty());
        let _ = identity;
    }

    #[test]
    fn noop_identity_is_deterministic_and_matches_verify() {
        let sandbox = tempfile::tempdir().expect("sandbox");
        let source = sandbox.path().join("source");
        let managed = sandbox.path().join("managed");
        fs::create_dir_all(&source).expect("source");
        fs::create_dir_all(&managed).expect("managed root");
        // The noop backend needs the same retained repository authorities as
        // the other backends: its integration runs git through the governed
        // repository-helper spawn path, which validates cwd provenance
        // against these leases.
        let capabilities = crate::fs::CapabilityFs::new().expect("capability fs");
        let source_lease = capabilities
            .open_root(
                CapabilityRootPurpose::ProjectResources,
                pi_sandbox::AbsolutePath::parse(source.clone()).expect("absolute source root"),
                CapabilityAccess::ReadWrite,
                RootCreation::Existing,
            )
            .expect("source lease");
        let managed_lease = capabilities
            .open_root(
                CapabilityRootPurpose::WorkflowWorktrees,
                pi_sandbox::AbsolutePath::parse(managed).expect("absolute managed root"),
                CapabilityAccess::PrivateReadWrite,
                RootCreation::CreateDirectory,
            )
            .expect("managed lease");
        let noop = NoopWorkflowIsolation::new(source_lease, managed_lease);
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
