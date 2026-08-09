//! Overlayfs isolation backend (OMP pi-iso parity): a writable "merged" view
//! over a read-only "lower" tree without a deep copy.
//!
//! [`OverlayfsIsolation::start`] materializes `merged` as the union of `lower`
//! (read-only) and `upper` (private, writable copy-on-write) using Linux
//! overlay semantics; [`OverlayfsIsolation::stop`] detaches the mount
//! (MNT_DETACH, i.e. `umount -l`) and cleans the upper/work directories.
//!
//! Backend fallback chain, tried in order until one succeeds:
//!
//! 1. **Kernel overlay** — `mount -t overlay -o lowerdir=,upperdir=,workdir=`.
//!    Requires mount privilege: real root, or a user namespace that owns the
//!    process mount namespace (the pi process, or a test binary re-executed
//!    under `unshare --user --map-root-user --mount`).
//! 2. **fuse-overlayfs** — PATH lookup; runs as a FUSE daemon, which works
//!    unprivileged and is visible to every process.
//! 3. **rcopy** — recursive copy of `lower` into `merged`. No mount needed;
//!    `merged` becomes a plain independent copy and `upper`/`work` stay
//!    unused (cleaned by `stop`).
//!
//! Backends never degrade silently: the first candidate that succeeds wins and
//! every later candidate is skipped; when every mount-based candidate fails,
//! the copy fallback guarantees callers always get a usable writable view.
//! [`OverlayBackend`] is serialized by the workflow isolation manager so a
//! restored workflow re-establishes exactly the backend it had before a
//! process restart (rcopy must never be re-run over an existing upper, which
//! would clobber the workflow's changes).
//!
//! Platform note: on non-Linux targets every entry point returns the explicit
//! "overlayfs isolation unsupported on this platform" error.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};

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

/// An active overlayfs isolation over `lower`, materialized at `merged` with
/// private `upper`/`work` layers. `stop` must be called explicitly (the
/// workflow isolation manager owns the lifecycle); dropping the handle does
/// not detach anything.
#[derive(Clone, Debug)]
pub struct OverlayfsIsolation {
    lower: PathBuf,
    upper: PathBuf,
    work: PathBuf,
    merged: PathBuf,
    backend: OverlayBackend,
}

/// Per-invocation mount command timeout; a hung mount (e.g. a network-backed
/// lower filesystem) must never block workflow creation forever.
const MOUNT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to wait for a fuse-overlayfs daemon to publish its mount.
const FUSE_READY_TIMEOUT: Duration = Duration::from_secs(5);
const FUSE_READY_POLL: Duration = Duration::from_millis(50);

impl OverlayfsIsolation {
    /// Start an overlayfs isolation at `merged` using the full backend
    /// fallback chain: kernel overlay → fuse-overlayfs → recursive copy.
    ///
    /// All four paths must be absolute. `upper`, `work`, and `merged` are
    /// created if missing; `lower` must exist.
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
            &[OverlayBackend::Kernel, OverlayBackend::FuseOverlayfs, OverlayBackend::Rcopy],
        )
    }

    /// Start an overlayfs isolation trying only `candidates` in order. Tests
    /// use this to force a specific backend deterministically; the workflow
    /// isolation manager uses it to re-establish the exact backend recorded
    /// for a restored workflow.
    pub fn start_with(
        lower: impl Into<PathBuf>,
        upper: impl Into<PathBuf>,
        work: impl Into<PathBuf>,
        merged: impl Into<PathBuf>,
        candidates: &[OverlayBackend],
    ) -> Result<Self> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (lower, upper, work, merged, candidates);
            bail!(
                "overlayfs isolation is only supported on Linux (this build targets {})",
                std::env::consts::OS
            );
        }
        #[cfg(target_os = "linux")]
        {
            let lower = canonicalize_or(lower.into());
            let upper = upper.into();
            let work = work.into();
            let merged = merged.into();
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
            if candidates.is_empty() {
                bail!("no overlayfs backend candidates were provided");
            }
            for backend in candidates {
                let result = match backend {
                    OverlayBackend::Kernel => mount_kernel(&lower, &upper, &work, &merged),
                    OverlayBackend::FuseOverlayfs => mount_fuse(&lower, &upper, &work, &merged),
                    OverlayBackend::Rcopy => {
                        copy_tree(&lower, &merged)?;
                        Ok(())
                    }
                };
                if result.is_ok() {
                    return Ok(Self {
                        lower,
                        upper,
                        work,
                        merged,
                        backend: *backend,
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
            )
        }
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
        }
    }

    /// Detach the mount (MNT_DETACH for kernel overlay; `fusermount -u` for
    /// fuse-overlayfs; no-op for rcopy) and remove the private upper/work
    /// directories. Idempotent: an already-detached mount is skipped, so a
    /// stale handle after a process restart can still clean the layers.
    pub fn stop(&self) -> Result<()> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = self;
            bail!(
                "overlayfs isolation is only supported on Linux (this build targets {})",
                std::env::consts::OS
            );
        }
        #[cfg(target_os = "linux")]
        {
            match self.backend {
                OverlayBackend::Kernel => {
                    if self.is_mounted() {
                        let mut umount = Command::new("umount");
                        umount.args(["-l", "--"]).arg(&self.merged);
                        let status = run_command(&mut umount, MOUNT_TIMEOUT).map_err(|error| {
                            anyhow!("detaching workflow overlay mount: {error}")
                        })?;
                        if !status.success() {
                            // Fall back to a non-lazy detach attempt.
                            let mut plain = Command::new("umount");
                            plain.args(["--"]).arg(&self.merged);
                            let status = run_command(&mut plain, MOUNT_TIMEOUT).map_err(
                                |error| anyhow!("detaching workflow overlay mount: {error}"),
                            )?;
                            if !status.success() {
                                bail!(
                                    "failed to detach workflow overlay mount at {}",
                                    self.merged.display()
                                );
                            }
                        }
                    }
                }
                OverlayBackend::FuseOverlayfs => {
                    if self.is_mounted() {
                        let mut fusermount = Command::new("fusermount");
                        fusermount.args(["-u", "--"]).arg(&self.merged);
                        let status = run_command(&mut fusermount, MOUNT_TIMEOUT).map_err(
                            |error| anyhow!("unmounting workflow fuse-overlayfs: {error}"),
                        )?;
                        if !status.success() {
                            let mut umount = Command::new("umount");
                            umount.args(["-l", "--"]).arg(&self.merged);
                            let status = run_command(&mut umount, MOUNT_TIMEOUT).map_err(
                                |error| anyhow!("unmounting workflow fuse-overlayfs: {error}"),
                            )?;
                            if !status.success() {
                                bail!(
                                    "failed to unmount workflow fuse-overlayfs at {}",
                                    self.merged.display()
                                );
                            }
                        }
                    }
                }
                OverlayBackend::Rcopy => {}
            }
            // Clean the private layers (never the merged view — the workflow
            // isolation manager owns that directory).
            for dir in [&self.upper, &self.work] {
                if path_exists(dir) {
                    remove_dir_all(dir).map_err(|error| {
                        anyhow!("cleaning overlayfs layer {}: {error}", dir.display())
                    })?;
                }
            }
            Ok(())
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

    /// True when the merged path is currently a mount point (kernel overlay or
    /// fuse-overlayfs in effect). rcopy backends always report false.
    ///
    /// On non-Linux targets overlayfs isolation is unsupported — `start` and
    /// `start_with` always fail, and `restore` only reconstructs a handle
    /// without mounting — so no overlay mount can ever exist there; this
    /// reports `false` rather than claim a mount that cannot be present.
    #[must_use]
    pub fn is_mounted(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            is_mountpoint(&self.merged)
        }
        #[cfg(not(target_os = "linux"))]
        {
            // Overlayfs isolation is unsupported off Linux: `start`/`start_with`
            // always fail and `restore` does not mount, so no overlay mount can
            // ever exist. Reporting `false` is the truthful lifecycle state.
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
        if look_path("unshare").is_none() {
            return false;
        }
        let probe = Command::new("unshare")
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

/// True when the current process can perform mounts in its own mount namespace
/// without a helper: real root, or already inside a user namespace that owns
/// the mount namespace. `start` falls back to fuse-overlayfs/rcopy when this
/// is false, so callers never need to probe.
#[cfg(target_os = "linux")]
fn can_mount_in_process() -> bool {
    is_root_euid()
}

#[cfg(target_os = "linux")]
fn mount_kernel(
    lower: &Path,
    upper: &Path,
    work: &Path,
    merged: &Path,
) -> Result<()> {
    let mount = look_path("mount")
        .ok_or_else(|| anyhow!("`mount` is not available in PATH"))?;
    let mut command = Command::new(mount);
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
    if !is_mountpoint(merged) {
        return Err(anyhow!(
            "kernel overlay mount reported success but {} is not a mount point",
            merged.display()
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn mount_fuse(
    lower: &Path,
    upper: &Path,
    work: &Path,
    merged: &Path,
) -> Result<()> {
    let fuse = look_path("fuse-overlayfs")
        .ok_or_else(|| anyhow!("`fuse-overlayfs` is not available in PATH"))?;
    let mut command = Command::new(fuse);
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
    // fuse-overlayfs daemonizes by itself (the spawned process forks and the
    // daemon performs the mount), so the parent exiting does not mean failure:
    // poll for the FUSE mount and only fail on a nonzero exit or timeout.
    let started = Instant::now();
    let mut daemon_exit: Option<std::process::ExitStatus> = None;
    loop {
        if is_mountpoint(merged) {
            // Reap the daemonized parent if it already exited; a foreground
            // daemon stays alive and keeps owning the mount — never kill it.
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => {}
                Ok(None) => {}
            }
            return Ok(());
        }
        match child.try_wait() {
            Ok(Some(status)) => daemon_exit = Some(status),
            Ok(None) => {}
            Err(error) => {
                return Err(anyhow!("waiting for fuse-overlayfs: {error}"));
            }
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
    let mut child = command
        .spawn()
        .map_err(|error| anyhow!("spawning {}: {error}", command.get_program().to_string_lossy()))?;
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
                return Err(anyhow!("waiting for {}: {error}", command.get_program().to_string_lossy()));
            }
        }
    }
}

/// Recursive copy of `src` into `dst` (the rcopy fallback). Files are copied
/// with their permissions, directories are recreated with their modes, and
/// symbolic links are recreated as links (never followed). `dst` must exist.
pub fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (src, dst);
        bail!(
            "overlayfs isolation is only supported on Linux (this build targets {})",
            std::env::consts::OS
        );
    }
    #[cfg(target_os = "linux")]
    {
        copy_tree_inner(src, dst)
    }
}

#[cfg(target_os = "linux")]
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
            create_dir_all(&to).map_err(|error| {
                anyhow!("creating directory {}: {error}", to.display())
            })?;
            std::fs::set_permissions(&to, std::fs::Permissions::from_mode(mode)).map_err(
                |error| anyhow!("setting permissions on {}: {error}", to.display()),
            )?;
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

/// True when `path` is currently a mount point, read from `/proc/self/mounts`
/// (no libc/unsafe needed). Space-containing paths appear escaped (`\040`) in
/// the mount table and are compared in that form.
#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
fn is_root_euid() -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self")
        .map(|meta| meta.uid() == 0)
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn path_exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

#[cfg(target_os = "linux")]
fn path_is_dir(path: &Path) -> bool {
    std::fs::metadata(path).map(|meta| meta.is_dir()).unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn canonicalize_or(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
}

#[cfg(target_os = "linux")]
fn look_path(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn create_dir_all(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

#[cfg(target_os = "linux")]
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
        assert_eq!(std::fs::read_to_string(dst.join("file.txt")).expect("read"), "hello");
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
        assert!(error.to_string().contains("no overlayfs backend candidates"));
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
