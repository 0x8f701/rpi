use std::fs::Metadata;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::{LauncherKind, LauncherTrustFailure, SandboxError, SandboxIoOperation};

const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ExecutableIdentity {
    device: u64,
    inode: u64,
    mode: u32,
    owner_uid: u32,
    owner_gid: u32,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

impl ExecutableIdentity {
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.mode(),
            owner_uid: metadata.uid(),
            owner_gid: metadata.gid(),
            size: metadata.size(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }

    pub(crate) fn from_fd(descriptor: RawFd) -> Result<Self, SandboxError> {
        let mut status = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe_fstat(descriptor, status.as_mut_ptr()) != 0 {
            return Err(SandboxError::from_io(
                SandboxIoOperation::InspectDescriptor,
                &std::io::Error::last_os_error(),
            ));
        }
        let status = unsafe_assume_stat(status);
        Ok(Self {
            device: status.st_dev as u64,
            inode: status.st_ino as u64,
            mode: status.st_mode,
            owner_uid: status.st_uid,
            owner_gid: status.st_gid,
            size: status.st_size as u64,
            modified_seconds: status.st_mtime,
            modified_nanoseconds: status.st_mtime_nsec,
            changed_seconds: status.st_ctime,
            changed_nanoseconds: status.st_ctime_nsec,
        })
    }

    #[cfg(test)]
    pub(crate) const fn for_test(device: u64, inode: u64) -> Self {
        Self {
            device,
            inode,
            mode: libc::S_IFREG | 0o755,
            owner_uid: 0,
            owner_gid: 0,
            size: 1,
            modified_seconds: 0,
            modified_nanoseconds: 0,
            changed_seconds: 0,
            changed_nanoseconds: 0,
        }
    }
    pub(crate) fn matches_fd(self, descriptor: RawFd) -> Result<bool, SandboxError> {
        self.matches_fd_with_owner_view(descriptor, IdentityOwnerView::Host)
    }

    pub(crate) fn matches_fd_in_user_namespace(
        self,
        descriptor: RawFd,
    ) -> Result<bool, SandboxError> {
        self.matches_fd_with_owner_view(descriptor, IdentityOwnerView::UserNamespace)
    }

    fn matches_fd_with_owner_view(
        self,
        descriptor: RawFd,
        owner_view: IdentityOwnerView,
    ) -> Result<bool, SandboxError> {
        let current = Self::from_fd(descriptor)?;
        Ok(self.device == current.device
            && self.inode == current.inode
            && self.mode == current.mode
            && ownership_matches(self.owner_uid, current.owner_uid, owner_view)
            && ownership_matches(self.owner_gid, current.owner_gid, owner_view)
            && self.size == current.size
            && self.modified_seconds == current.modified_seconds
            && self.modified_nanoseconds == current.modified_nanoseconds)
    }

    /// Content-agnostic directory identity: a directory keeps the same identity across
    /// entry additions/removals (mtime/size change), so only the stable inode/device,
    /// mode, and ownership are compared. A replaced directory (different inode) fails.
    pub(crate) fn directory_matches_fd(self, descriptor: RawFd) -> Result<bool, SandboxError> {
        self.directory_matches_fd_with_owner_view(descriptor, IdentityOwnerView::Host)
    }

    pub(crate) fn directory_matches_fd_in_user_namespace(
        self,
        descriptor: RawFd,
    ) -> Result<bool, SandboxError> {
        self.directory_matches_fd_with_owner_view(descriptor, IdentityOwnerView::UserNamespace)
    }

    fn directory_matches_fd_with_owner_view(
        self,
        descriptor: RawFd,
        owner_view: IdentityOwnerView,
    ) -> Result<bool, SandboxError> {
        let current = Self::from_fd(descriptor)?;
        Ok(self.device == current.device
            && self.inode == current.inode
            && (self.mode & libc::S_IFMT) == (current.mode & libc::S_IFMT)
            && ownership_matches(self.owner_uid, current.owner_uid, owner_view)
            && ownership_matches(self.owner_gid, current.owner_gid, owner_view))
    }
}

#[derive(Clone, Copy)]
enum IdentityOwnerView {
    Host,
    UserNamespace,
}

fn ownership_matches(expected: u32, current: u32, owner_view: IdentityOwnerView) -> bool {
    expected == current
        || (matches!(owner_view, IdentityOwnerView::UserNamespace)
            && expected == 0
            && current == 65_534)
}

#[cfg(test)]
mod identity_tests {
    use super::{IdentityOwnerView, ownership_matches};

    #[test]
    fn user_namespace_ownership_matches_root_overflow_only() {
        assert!(ownership_matches(
            0,
            65_534,
            IdentityOwnerView::UserNamespace
        ));
        assert!(ownership_matches(
            1_000,
            1_000,
            IdentityOwnerView::UserNamespace
        ));
        assert!(!ownership_matches(
            1_000,
            65_534,
            IdentityOwnerView::UserNamespace
        ));
        assert!(!ownership_matches(
            0,
            1_000,
            IdentityOwnerView::UserNamespace
        ));
    }

    #[test]
    fn host_ownership_requires_an_exact_match() {
        assert!(ownership_matches(0, 0, IdentityOwnerView::Host));
        assert!(ownership_matches(1_000, 1_000, IdentityOwnerView::Host));
        assert!(!ownership_matches(0, 65_534, IdentityOwnerView::Host));
    }
}

pub(crate) struct TrustedExecutable {
    executable: OwnedFd,
    identity: ExecutableIdentity,
    launcher: LauncherKind,
}

impl TrustedExecutable {
    pub(crate) fn try_clone_fd(&self) -> Result<OwnedFd, SandboxError> {
        duplicate_fd(self.executable.as_raw_fd())
    }

    pub(crate) fn try_clone(&self) -> Result<Self, SandboxError> {
        Ok(Self {
            executable: self.try_clone_fd()?,
            identity: self.identity,
            launcher: self.launcher,
        })
    }

    pub(crate) const fn identity(&self) -> ExecutableIdentity {
        self.identity
    }

    pub(crate) fn revalidate(&self) -> Result<(), SandboxError> {
        if self.identity.matches_fd(self.executable.as_raw_fd())? {
            Ok(())
        } else {
            Err(SandboxError::LauncherChanged {
                launcher: self.launcher,
            })
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn probe_trusted_bwrap() -> Result<TrustedExecutable, SandboxError> {
    let candidate = ["/usr/bin/bwrap", "/bin/bwrap"]
        .into_iter()
        .find(|path| Path::new(path).exists())
        .ok_or(SandboxError::LauncherUnavailable {
            launcher: LauncherKind::Bubblewrap,
        })?;
    let executable = open_trusted(candidate, LauncherKind::Bubblewrap)?;
    executable.revalidate()?;
    let child = Command::new(candidate)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| SandboxError::from_io(SandboxIoOperation::ProbeLauncher, &error))?;
    let output = run_bounded(child, LauncherKind::Bubblewrap)?;
    let version = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .find_map(parse_version)
        .ok_or(SandboxError::LauncherUntrusted {
            launcher: LauncherKind::Bubblewrap,
            failure: LauncherTrustFailure::ProbeRejected,
        })?;
    const REQUIRED_BWRAP_VERSION: (u32, u32, u32) = (0, 10, 0);
    if version < REQUIRED_BWRAP_VERSION {
        return Err(SandboxError::LauncherUntrusted {
            launcher: LauncherKind::Bubblewrap,
            failure: LauncherTrustFailure::VersionTooOld {
                found: version,
                required: REQUIRED_BWRAP_VERSION,
            },
        });
    }
    executable.revalidate()?;
    Ok(executable)
}

#[cfg(target_os = "macos")]
pub(crate) fn probe_sandbox_exec() -> Result<TrustedExecutable, SandboxError> {
    let candidate = "/usr/bin/sandbox-exec";
    let executable = open_trusted(candidate, LauncherKind::SandboxExec)?;
    let child = Command::new(candidate)
        .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| SandboxError::from_io(SandboxIoOperation::ProbeLauncher, &error))?;
    let output = run_bounded(child, LauncherKind::SandboxExec)?;
    if !output.status.success() {
        return Err(SandboxError::LauncherUntrusted {
            launcher: LauncherKind::SandboxExec,
            failure: LauncherTrustFailure::ProbeRejected,
        });
    }
    executable.revalidate()?;
    Ok(executable)
}

fn open_trusted(path: &str, launcher: LauncherKind) -> Result<TrustedExecutable, SandboxError> {
    validate_ancestors(Path::new(path), launcher)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| SandboxError::from_io(SandboxIoOperation::OpenExecutable, &error))?;
    let metadata = file
        .metadata()
        .map_err(|error| SandboxError::from_io(SandboxIoOperation::InspectDescriptor, &error))?;
    if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
        return Err(SandboxError::LauncherUntrusted {
            launcher,
            failure: LauncherTrustFailure::NotRegular,
        });
    }
    if metadata.uid() != 0 {
        return Err(SandboxError::LauncherUntrusted {
            launcher,
            failure: LauncherTrustFailure::WrongOwner,
        });
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(SandboxError::LauncherUntrusted {
            launcher,
            failure: LauncherTrustFailure::WritableByGroupOrOther,
        });
    }
    Ok(TrustedExecutable {
        identity: ExecutableIdentity::from_metadata(&metadata),
        executable: file.into(),
        launcher,
    })
}

fn validate_ancestors(path: &Path, launcher: LauncherKind) -> Result<(), SandboxError> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| SandboxError::from_io(SandboxIoOperation::OpenExecutable, &error))?;
    let mut current = canonical.parent();
    while let Some(ancestor) = current {
        let metadata = std::fs::symlink_metadata(ancestor).map_err(|error| {
            SandboxError::from_io(SandboxIoOperation::InspectDescriptor, &error)
        })?;
        if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(SandboxError::LauncherUntrusted {
                launcher,
                failure: LauncherTrustFailure::UnsafeAncestor,
            });
        }
        current = ancestor.parent();
    }
    Ok(())
}

fn parse_version(value: &str) -> Option<(u32, u32, u32)> {
    let mut parts = value.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

fn run_bounded(
    mut child: std::process::Child,
    launcher: LauncherKind,
) -> Result<std::process::Output, SandboxError> {
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return child
                    .wait_with_output()
                    .map_err(|error| SandboxError::from_io(SandboxIoOperation::WaitChild, &error));
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(SandboxError::LauncherUntrusted {
                    launcher,
                    failure: LauncherTrustFailure::ProbeTimedOut,
                });
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(SandboxError::from_io(SandboxIoOperation::WaitChild, &error));
            }
        }
    }
}

pub(crate) fn open_pinned_executable(
    path: &Path,
) -> Result<(OwnedFd, ExecutableIdentity), SandboxError> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| SandboxError::from_io(SandboxIoOperation::OpenExecutable, &error))?;
    let metadata = file
        .metadata()
        .map_err(|error| SandboxError::from_io(SandboxIoOperation::InspectDescriptor, &error))?;
    if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
        return Err(SandboxError::from_io(
            SandboxIoOperation::OpenExecutable,
            &std::io::Error::from_raw_os_error(libc::EACCES),
        ));
    }
    let identity = ExecutableIdentity::from_metadata(&metadata);
    Ok((file.into(), identity))
}

pub(crate) fn duplicate_fd(descriptor: RawFd) -> Result<OwnedFd, SandboxError> {
    let duplicated = unsafe_fcntl_dupfd_cloexec(descriptor, 7);
    if duplicated < 0 {
        return Err(SandboxError::from_io(
            SandboxIoOperation::DuplicateDescriptor,
            &std::io::Error::last_os_error(),
        ));
    }
    Ok(unsafe_owned_fd(duplicated))
}

#[allow(unsafe_code)]
fn unsafe_fstat(descriptor: RawFd, status: *mut libc::stat) -> i32 {
    unsafe { libc::fstat(descriptor, status) }
}

#[allow(unsafe_code)]
fn unsafe_assume_stat(status: std::mem::MaybeUninit<libc::stat>) -> libc::stat {
    unsafe { status.assume_init() }
}

#[allow(unsafe_code)]
fn unsafe_fcntl_dupfd_cloexec(descriptor: RawFd, minimum: RawFd) -> RawFd {
    unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, minimum) }
}

#[allow(unsafe_code)]
fn unsafe_owned_fd(descriptor: RawFd) -> OwnedFd {
    unsafe { OwnedFd::from_raw_fd(descriptor) }
}

pub(crate) fn duplicate_fd_at_least(
    descriptor: RawFd,
    minimum: RawFd,
) -> Result<OwnedFd, SandboxError> {
    let duplicated = unsafe_fcntl_dupfd_cloexec(descriptor, minimum);
    if duplicated < 0 {
        return Err(SandboxError::from_io(
            SandboxIoOperation::DuplicateDescriptor,
            &std::io::Error::last_os_error(),
        ));
    }
    Ok(unsafe_owned_fd(duplicated))
}
#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    #[test]
    fn exec_uses_opened_bwrap_identity() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("launcher");
        let mut file = File::create(&path).expect("create");
        file.write_all(b"#!/bin/sh\nexit 0\n").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("permissions");
        let (descriptor, identity) = open_pinned_executable(&path).expect("open");
        std::fs::remove_file(&path).expect("remove");
        File::create(&path).expect("replace");
        assert!(
            identity
                .matches_fd(descriptor.as_raw_fd())
                .expect("inspect")
        );
    }
}
