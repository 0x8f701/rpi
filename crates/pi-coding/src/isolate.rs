//! Fixed Linux overlay operator helpers and the durable overlay backend label.
//!
//! Workflow overlay storage is governed by a retained `WorkflowWorktrees`
//! capability. The current mount-source interface cannot yet express the lower,
//! upper, work, and merged directories relative to that retained authority, so
//! the public storage lifecycle fails closed rather than reopening display paths
//! or recursively traversing the host filesystem. The private raw helpers remain
//! the exact fixed-operator exceptions reviewed by the sandbox raw-spawn gate;
//! production workflow code cannot reach them until a descriptor-relative mount
//! adapter is available.

#[cfg(test)]
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use pi_sandbox::AbsolutePath;
use serde::{Deserialize, Serialize};

use crate::fs::{
    CapabilityAccess, CapabilityRootLease, CapabilityRootPurpose, FileMode, RootCreation,
};
use crate::workflow_worktree::overlay::copy_tree_governed;

/// Which backend materializes the writable merged view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OverlayBackend {
    /// Kernel overlay mount (`mount -t overlay`), detached with `umount -l`.
    Kernel,
    /// fuse-overlayfs FUSE daemon, unmounted with `fusermount -u`.
    FuseOverlayfs,
    /// Recursive copy of the lower tree (no mount).
    Rcopy,
}

impl OverlayBackend {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Kernel => "kernel overlay",
            Self::FuseOverlayfs => "fuse-overlayfs",
            Self::Rcopy => "recursive copy",
        }
    }
}
/// Fail-closed overlay lifecycle failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum OverlayIsolationError {
    #[error("no overlayfs backend candidates were provided")]
    NoBackendCandidates,
    #[error(
        "workflow overlay cannot {operation} without retained descriptor-relative mount authority"
    )]
    RetainedMountSourceUnavailable { operation: &'static str },
}

/// An active overlayfs isolation over `lower`, materialized at `merged` with
/// private `upper`/`work` layers. `stop` must be called explicitly (the
/// workflow isolation manager owns the lifecycle); dropping the handle does
/// not detach anything.
#[derive(Clone)]
pub struct OverlayfsIsolation {
    lower: PathBuf,
    upper: PathBuf,
    work: PathBuf,
    merged: PathBuf,
    backend: OverlayBackend,
    /// Governed rcopy handles retain the descriptor-relative managed-root
    /// capability so `stop` cleans the private layers through the lease
    /// without reopening display paths.
    retained_managed: Option<RetainedManagedRoot>,
}

impl std::fmt::Debug for OverlayfsIsolation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OverlayfsIsolation")
            .field("lower", &self.lower)
            .field("upper", &self.upper)
            .field("work", &self.work)
            .field("merged", &self.merged)
            .field("backend", &self.backend)
            .finish_non_exhaustive()
    }
}

/// Retained managed-root capability plus the descriptor-relative private
/// layers, enabling governed rcopy materialization and cleanup.
#[derive(Clone)]
struct RetainedManagedRoot {
    lease: CapabilityRootLease,
    upper_rel: PathBuf,
    work_rel: PathBuf,
}

/// Per-invocation fixed operator command timeout.
const MOUNT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the fixed fuse helper waits for its mount to appear.
const FUSE_READY_TIMEOUT: Duration = Duration::from_secs(5);
const FUSE_READY_POLL: Duration = Duration::from_millis(50);

impl OverlayfsIsolation {
    /// Fail closed until overlay paths can be supplied as descriptor-relative
    /// children of a retained `WorkflowWorktrees` capability.
    pub fn start(
        lower: impl Into<PathBuf>,
        upper: impl Into<PathBuf>,
        work: impl Into<PathBuf>,
        merged: impl Into<PathBuf>,
    ) -> Result<Self> {
        Self::start_with(
            lower,
            upper,
            work,
            merged,
            &[
                OverlayBackend::Kernel,
                OverlayBackend::FuseOverlayfs,
                OverlayBackend::Rcopy,
            ],
        )
    }

    /// Fail closed for production storage. Unit-only fixtures retain the old
    /// native recursive-copy path without granting it to product code.
    pub fn start_with(
        lower: impl Into<PathBuf>,
        upper: impl Into<PathBuf>,
        work: impl Into<PathBuf>,
        merged: impl Into<PathBuf>,
        candidates: &[OverlayBackend],
    ) -> Result<Self> {
        let lower = lower.into();
        let upper = upper.into();
        let work = work.into();
        let merged = merged.into();
        if candidates.is_empty() {
            return Err(OverlayIsolationError::NoBackendCandidates.into());
        }

        #[cfg(all(test, target_os = "linux"))]
        {
            let lower = canonicalize_or(lower);
            if !path_is_dir(&lower) {
                bail!(
                    "overlayfs lower layer {} does not exist or is not a directory",
                    lower.display()
                );
            }
            for dir in [&upper, &work, &merged] {
                create_dir_all(dir).map_err(|error| {
                    anyhow!("creating overlayfs {} directory: {error}", dir.display())
                })?;
            }
            for backend in candidates {
                let result = match backend {
                    OverlayBackend::Kernel => mount_kernel(&lower, &upper, &work, &merged),
                    OverlayBackend::FuseOverlayfs => mount_fuse(&lower, &upper, &work, &merged),
                    OverlayBackend::Rcopy => copy_tree(&lower, &merged),
                };
                if result.is_ok() {
                    return Ok(Self {
                        lower,
                        upper,
                        work,
                        merged,
                        backend: *backend,
                        retained_managed: None,
                    });
                }
            }
            bail!(
                "no overlayfs backend could materialize the merged view (tried: {})",
                candidates
                    .iter()
                    .map(|backend| backend.label())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }

        #[cfg(any(not(test), not(target_os = "linux")))]
        {
            let _ = (lower, upper, work, merged, candidates);
            Err(OverlayIsolationError::RetainedMountSourceUnavailable {
                operation: "materialize",
            }
            .into())
        }
    }

    /// Capability-aware recursive-copy materialization through the exact
    /// retained authority production requires: `source_root` is opened as a
    /// `ProjectResources` read-write lease and `managed_root` as a
    /// `WorkflowWorktrees` private lease, then every byte of the merged view
    /// is copied through those descriptors. No display path is reopened and no
    /// ambient process is spawned. The returned backend is always
    /// [`OverlayBackend::Rcopy`]; `stop` cleans the private upper/work layers
    /// through the retained managed lease and keeps the merged view (the
    /// workflow manager owns workspace removal).
    ///
    /// This is the descriptor-relative mount adapter the governed
    /// `OverlayWorkflowManager` materialization requires; plain-path
    /// `start`/`start_with` continue to fail closed for production storage.
    pub fn start_rcopy_governed(
        source_root: &Path,
        managed_root: &Path,
        upper_rel: &Path,
        work_rel: &Path,
        merged_rel: &Path,
    ) -> Result<Self> {
        let capabilities = crate::fs::CapabilityFs::new()
            .map_err(|error| anyhow!("creating overlay capability filesystem: {error}"))?;
        let source_lease = capabilities
            .open_root(
                CapabilityRootPurpose::ProjectResources,
                AbsolutePath::parse(source_root.to_path_buf())
                    .map_err(|_| anyhow!("overlay source root is not absolute"))?,
                CapabilityAccess::ReadWrite,
                RootCreation::Existing,
            )
            .map_err(|error| anyhow!("opening retained overlay source root: {error}"))?;
        let managed_lease = capabilities
            .open_root(
                CapabilityRootPurpose::WorkflowWorktrees,
                AbsolutePath::parse(managed_root.to_path_buf())
                    .map_err(|_| anyhow!("overlay managed root is not absolute"))?,
                CapabilityAccess::PrivateReadWrite,
                RootCreation::CreateDirectory,
            )
            .map_err(|error| anyhow!("opening retained overlay managed root: {error}"))?;
        for relative in [upper_rel, work_rel, merged_rel] {
            managed_lease
                .create_dir_all(relative, FileMode::Private)
                .map_err(|error| {
                    anyhow!(
                        "creating governed overlay layer {}: {error}",
                        relative.display()
                    )
                })?;
        }
        copy_tree_governed(&source_lease, Path::new(""), &managed_lease, merged_rel)
            .map_err(|error| anyhow!("governed overlay materialization: {error:#}"))?;
        Ok(Self {
            lower: source_root.to_path_buf(),
            upper: managed_root.join(upper_rel),
            work: managed_root.join(work_rel),
            merged: managed_root.join(merged_rel),
            backend: OverlayBackend::Rcopy,
            retained_managed: Some(RetainedManagedRoot {
                lease: managed_lease,
                upper_rel: upper_rel.to_path_buf(),
                work_rel: work_rel.to_path_buf(),
            }),
        })
    }

    /// Reconstruct a handle for an existing isolation **without** mounting:
    /// used to check/stop an overlay after a process restart (the mount
    /// itself is re-established with [`Self::start_with`]).
    #[must_use]
    pub fn restore(
        lower: impl Into<PathBuf>,
        upper: impl Into<PathBuf>,
        work: impl Into<PathBuf>,
        merged: impl Into<PathBuf>,
        backend: OverlayBackend,
    ) -> Self {
        Self {
            lower: lower.into(),
            upper: upper.into(),
            work: work.into(),
            merged: merged.into(),
            backend,
            retained_managed: None,
        }
    }

    /// Detach the merged mount (kernel overlay → `umount -l`; fuse-overlayfs →
    /// `fusermount -u`, then `umount -l` fallback) and remove the private
    /// `upper`/`work` layers.
    ///
    /// The mount detach is the fixed-operator raw exception reviewed by the
    /// sandbox raw-spawn gate, so it is only reachable from native fixtures
    /// (`#[cfg(test)]`). Production workflow code never holds a kernel/fuse
    /// handle — the governed `OverlayWorkflowManager` materializes via the
    /// retained capability's recursive-copy fallback — so a non-rcopy stop in
    /// a production build fails closed rather than raw-spawning `umount`.
    /// Rcopy stops are always a no-op (no mount to detach); the workflow
    /// isolation manager owns private-layer cleanup through the retained
    /// capability.
    pub fn stop(&self) -> Result<()> {
        #[cfg(all(test, target_os = "linux"))]
        {
            match self.backend {
                OverlayBackend::Kernel => detach_mount(&self.merged, false)?,
                OverlayBackend::FuseOverlayfs => detach_mount(&self.merged, true)?,
                OverlayBackend::Rcopy => {}
            }
            for dir in [&self.upper, &self.work] {
                if path_exists(dir) {
                    remove_dir_all(dir).map_err(|error| {
                        anyhow!("cleaning overlayfs layer {}: {error}", dir.display())
                    })?;
                }
            }
            return Ok(());
        }

        #[cfg(any(not(test), not(target_os = "linux")))]
        {
            if matches!(self.backend, OverlayBackend::Rcopy) {
                if let Some(retained) = &self.retained_managed {
                    for relative in [&retained.upper_rel, &retained.work_rel] {
                        match retained.lease.remove_tree(relative) {
                            Ok(()) => {}
                            Err(error)
                                if matches!(
                                    error,
                                    crate::fs::FsError::System {
                                        kind: std::io::ErrorKind::NotFound,
                                        ..
                                    }
                                ) => {}
                            Err(error) => {
                                return Err(anyhow!(
                                    "cleaning governed overlay layer {}: {error}",
                                    relative.display()
                                ));
                            }
                        }
                    }
                }
                return Ok(());
            }
            Err(OverlayIsolationError::RetainedMountSourceUnavailable {
                operation: "detach",
            }
            .into())
        }
    }

    #[must_use]
    pub fn backend(&self) -> OverlayBackend {
        self.backend
    }

    #[must_use]
    pub fn lower_path(&self) -> &Path {
        &self.lower
    }

    #[must_use]
    pub fn upper_path(&self) -> &Path {
        &self.upper
    }

    #[must_use]
    pub fn work_path(&self) -> &Path {
        &self.work
    }

    #[must_use]
    pub fn merged_path(&self) -> &Path {
        &self.merged
    }

    /// Reports whether this backend verifiably owns a live mount over the
    /// merged view.
    ///
    /// Recursive copies never mount and report `false`. Kernel and FUSE
    /// backends report `true` only when the ambient mount table can be
    /// inspected and confirms the merged path is a real mount point — native
    /// fixtures do this directly. Production builds cannot raw-spawn a
    /// mount-table probe or reopen the merged display path, so a restored
    /// kernel/fuse handle conservatively reports `false`: a persisted
    /// kernel/fuse catalog entry cannot proceed as mounted without a real
    /// governed mount, and the caller fails closed or governed-rematerializes
    /// to rcopy rather than trusting a stale catalog entry.
    #[must_use]
    pub fn is_mounted(&self) -> bool {
        #[cfg(all(test, target_os = "linux"))]
        {
            return !matches!(self.backend, OverlayBackend::Rcopy) && is_mountpoint(&self.merged);
        }
        #[cfg(any(not(test), not(target_os = "linux")))]
        {
            // Rcopy never mounts; kernel/fuse cannot be verified in
            // production without raw-spawning a mount-table probe, so both
            // report `false` (fail closed) — a stale catalog must not proceed
            // as mounted.
            let _ = &self.merged;
            false
        }
    }
}

/// True when an unprivileged user namespace with mounts can be created on this
/// host (the same probe the sandbox smoke tests use). The mount tests in
/// `tests/overlayfs_smoke.rs` skip-guard on this, and re-execute under
/// `unshare --user --map-root-user --mount` so the kernel overlay backend can
/// be exercised unprivileged.
#[must_use]
pub fn userns_mount_probe() -> bool {
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
    #[cfg(target_os = "linux")]
    {
        let probe = Command::new("/usr/bin/unshare")
            .args([
                "--user",
                "--map-root-user",
                "--mount",
                "--pid",
                "--fork",
                "--net",
                "--mount-proc",
            ])
            .arg("sh")
            .arg("-c")
            .arg("exit 0")
            .status();
        match probe {
            Ok(status) => status.success(),
            Err(_) => false,
        }
    }
}

#[cfg(target_os = "linux")]
fn mount_kernel(lower: &Path, upper: &Path, work: &Path, merged: &Path) -> Result<()> {
    let mut command = Command::new("/usr/bin/mount");
    command
        .arg("-t")
        .arg("overlay")
        .arg("-o")
        .arg(format!(
            "lowerdir={},upperdir={},workdir={}",
            lower.display(),
            upper.display(),
            work.display()
        ))
        .arg("overlay")
        .arg(merged);
    let status = run_command(&mut command, MOUNT_TIMEOUT)?;
    if !status.success() {
        return Err(anyhow!(
            "kernel overlay mount failed (exit {}) at {}",
            status.code().unwrap_or(-1),
            merged.display()
        ));
    }
    #[cfg(test)]
    if !is_mountpoint(merged) {
        return Err(anyhow!(
            "kernel overlay mount reported success but {} is not a mount point",
            merged.display()
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn mount_fuse(lower: &Path, upper: &Path, work: &Path, merged: &Path) -> Result<()> {
    let mut command = Command::new("/usr/bin/fuse-overlayfs");
    command
        .arg("-o")
        .arg(format!(
            "lowerdir={},upperdir={},workdir={}",
            lower.display(),
            upper.display(),
            work.display()
        ))
        .arg(merged)
        .env("LC_ALL", "C");
    let mut child = command
        .spawn()
        .map_err(|error| anyhow!("spawning fuse-overlayfs: {error}"))?;
    let started = Instant::now();
    let mut daemon_exit: Option<std::process::ExitStatus> = None;
    loop {
        #[cfg(test)]
        if is_mountpoint(merged) {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) | Ok(None) => {}
            }
            return Ok(());
        }
        #[cfg(not(test))]
        if daemon_exit.is_some_and(|status| status.success()) {
            return Ok(());
        }
        match child.try_wait() {
            Ok(Some(status)) => daemon_exit = Some(status),
            Ok(None) => {}
            Err(error) => return Err(anyhow!("waiting for fuse-overlayfs: {error}")),
        }
        if started.elapsed() >= FUSE_READY_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!(
                "fuse-overlayfs did not mount {} within {:?} (daemon exit: {:?})",
                merged.display(),
                FUSE_READY_TIMEOUT,
                daemon_exit.map(|status| status.code().unwrap_or(-1))
            ));
        }
        std::thread::sleep(FUSE_READY_POLL);
    }
}

#[cfg(target_os = "linux")]
fn run_command(command: &mut Command, timeout: Duration) -> Result<std::process::ExitStatus> {
    command.env("LC_ALL", "C");
    let mut child = command.spawn().map_err(|error| {
        anyhow!(
            "spawning {}: {error}",
            command.get_program().to_string_lossy()
        )
    })?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow!(
                    "command {} timed out after {:?}",
                    command.get_program().to_string_lossy(),
                    timeout
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow!(
                    "waiting for {}: {error}",
                    command.get_program().to_string_lossy()
                ));
            }
        }
    }
}

/// Test-only fixed-operator mount detach: `fusermount -u` (fuse only) then
/// `umount -l`. No-op when `merged` is not currently a mount point.
#[cfg(all(test, target_os = "linux"))]
fn detach_mount(merged: &Path, fuse: bool) -> Result<()> {
    if !is_mountpoint(merged) {
        return Ok(());
    }
    if fuse {
        let mut fusermount = Command::new("fusermount");
        fusermount.args(["-u", "--"]).arg(merged);
        if run_command(&mut fusermount, MOUNT_TIMEOUT).is_ok_and(|status| status.success()) {
            return Ok(());
        }
    }
    let mut umount = Command::new("umount");
    umount.args(["-l", "--"]).arg(merged);
    let status = run_command(&mut umount, MOUNT_TIMEOUT)?;
    if !status.success() {
        bail!(
            "failed to detach workflow overlay mount at {}",
            merged.display()
        );
    }
    Ok(())
}

/// Recursive copy is retained only for native unit fixtures. Product code must
/// use a retained workflow-worktrees capability and therefore fails closed.
pub fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    #[cfg(all(test, target_os = "linux"))]
    {
        return copy_tree_inner(src, dst);
    }
    #[cfg(any(not(test), not(target_os = "linux")))]
    {
        let _ = (src, dst);
        Err(OverlayIsolationError::RetainedMountSourceUnavailable {
            operation: "recursively copy storage",
        }
        .into())
    }
}

#[cfg(all(test, target_os = "linux"))]
fn copy_tree_inner(src: &Path, dst: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let entries = std::fs::read_dir(src)
        .map_err(|error| anyhow!("reading directory {}: {error}", src.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| anyhow!("reading directory entry: {error}"))?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|error| anyhow!("inspecting {}: {error}", from.display()))?;
        if file_type.is_dir() {
            let mode = std::fs::metadata(&from)
                .map_err(|error| anyhow!("reading metadata of {}: {error}", from.display()))?
                .permissions()
                .mode();
            create_dir_all(&to)
                .map_err(|error| anyhow!("creating directory {}: {error}", to.display()))?;
            std::fs::set_permissions(&to, std::fs::Permissions::from_mode(mode))
                .map_err(|error| anyhow!("setting permissions on {}: {error}", to.display()))?;
            copy_tree_inner(&from, &to)?;
        } else if file_type.is_symlink() {
            let target = std::fs::read_link(&from)
                .map_err(|error| anyhow!("reading symlink {}: {error}", from.display()))?;
            match std::fs::remove_file(&to) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(anyhow!("replacing symlink {}: {error}", to.display()));
                }
            }
            std::os::unix::fs::symlink(&target, &to)
                .map_err(|error| anyhow!("creating symlink {}: {error}", to.display()))?;
        } else {
            std::fs::copy(&from, &to)
                .map_err(|error| anyhow!("copying {}: {error}", from.display()))?;
        }
    }
    Ok(())
}

/// Test-only mount-table observation for native overlay fixtures.
#[cfg(all(test, target_os = "linux"))]
fn is_mountpoint(path: &Path) -> bool {
    let target = path.to_string_lossy().replace(' ', "\\040");
    std::fs::read_to_string("/proc/self/mounts")
        .map(|table| {
            table.lines().any(|line| {
                line.split_whitespace()
                    .nth(1)
                    .is_some_and(|mountpoint| mountpoint == target)
            })
        })
        .unwrap_or(false)
}

#[cfg(all(test, target_os = "linux"))]
fn path_exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

#[cfg(all(test, target_os = "linux"))]
fn path_is_dir(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|meta| meta.is_dir())
        .unwrap_or(false)
}

#[cfg(all(test, target_os = "linux"))]
fn canonicalize_or(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
}

#[cfg(all(test, target_os = "linux"))]
fn create_dir_all(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

#[cfg(all(test, target_os = "linux"))]
fn remove_dir_all(path: &Path) -> std::io::Result<()> {
    std::fs::remove_dir_all(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    #[test]
    fn copy_tree_materializes_files_dirs_symlinks_and_permissions() {
        let sandbox = tree();
        let src = sandbox.path().join("src");
        let dst = sandbox.path().join("dst");
        std::fs::create_dir_all(src.join("sub")).expect("subdir");
        std::fs::write(src.join("file.txt"), "hello").expect("file");
        std::fs::write(src.join("sub").join("deep.txt"), "deep").expect("deep file");
        std::os::unix::fs::symlink("file.txt", src.join("link")).expect("symlink");
        std::fs::create_dir_all(&dst).expect("dst");
        copy_tree(&src, &dst).expect("copy");
        assert_eq!(
            std::fs::read_to_string(dst.join("file.txt")).expect("read"),
            "hello"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("sub").join("deep.txt")).expect("read"),
            "deep"
        );
        assert_eq!(
            std::fs::read_link(dst.join("link")).expect("readlink"),
            PathBuf::from("file.txt"),
            "symlinks must be recreated, not followed"
        );
    }

    #[test]
    fn stop_cleans_upper_and_work_for_rcopy_backend() {
        let sandbox = tree();
        let lower = sandbox.path().join("lower");
        let upper = sandbox.path().join("upper");
        let work = sandbox.path().join("work");
        let merged = sandbox.path().join("merged");
        std::fs::create_dir_all(&lower).expect("lower");
        std::fs::write(lower.join("file.txt"), "base").expect("lower file");
        let iso = OverlayfsIsolation::start_with(
            &lower,
            &upper,
            &work,
            &merged,
            &[OverlayBackend::Rcopy],
        )
        .expect("rcopy isolation");
        assert_eq!(iso.backend(), OverlayBackend::Rcopy);
        assert_eq!(
            std::fs::read_to_string(merged.join("file.txt")).expect("merged file"),
            "base"
        );
        assert!(!iso.is_mounted(), "rcopy never mounts");

        // The merged view is an independent copy: writes do not touch lower.
        std::fs::write(merged.join("written.txt"), "w").expect("write merged");
        assert!(!lower.join("written.txt").exists());

        iso.stop().expect("stop");
        assert!(!upper.exists(), "upper must be cleaned by stop");
        assert!(!work.exists(), "work must be cleaned by stop");
        assert!(merged.exists(), "stop must not remove the merged view");
        assert_eq!(
            std::fs::read_to_string(merged.join("file.txt")).expect("merged survives"),
            "base"
        );
    }

    #[test]
    fn start_with_empty_candidates_fails_cleanly() {
        let sandbox = tree();
        let lower = sandbox.path().join("lower");
        let upper = sandbox.path().join("upper");
        let work = sandbox.path().join("work");
        let merged = sandbox.path().join("merged");
        std::fs::create_dir_all(&lower).expect("lower");
        let error = OverlayfsIsolation::start_with(&lower, &upper, &work, &merged, &[])
            .expect_err("no candidates must fail");
        assert!(
            error
                .to_string()
                .contains("no overlayfs backend candidates")
        );
    }

    #[test]
    fn start_rejects_missing_lower_layer() {
        let sandbox = tree();
        let missing = sandbox.path().join("does-not-exist");
        let error = OverlayfsIsolation::start(
            &missing,
            sandbox.path().join("upper"),
            sandbox.path().join("work"),
            sandbox.path().join("merged"),
        )
        .expect_err("missing lower must fail");
        assert!(error.to_string().contains("lower layer"));
    }
}
