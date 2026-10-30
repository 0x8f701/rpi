#![allow(unsafe_code)]

use std::ffi::{CString, OsStr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Component;

use crate::launcher::{duplicate_fd, ExecutableIdentity};
use crate::{AbsolutePath, RelativePath, SandboxError, SandboxIoOperation};

struct SyntheticEntry {
    parent: OwnedFd,
    name: CString,
    identity: ExecutableIdentity,
}

#[derive(Default)]
pub(crate) struct SyntheticDirs {
    entries: Vec<SyntheticEntry>,
    cleaned: bool,
}

impl SyntheticDirs {
    pub(crate) fn cleanup(&mut self) -> Result<(), SandboxError> {
        self.cleaned = true;
        let mut first_error = None;
        for entry in self.entries.drain(..).rev() {
            if let Err(error) = remove_synthetic_entry(&entry)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl Drop for SyntheticDirs {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = self.cleanup();
        }
    }
}

pub(crate) fn open_or_create_protected_directory(
    parent: RawFd,
    root: &AbsolutePath,
    relative: &RelativePath,
    synthetics: &mut SyntheticDirs,
) -> Result<OwnedFd, SandboxError> {
    let mut current = duplicate_fd(parent)?;
    let mut current_path = root.as_path().to_path_buf();
    let components = relative
        .as_path()
        .components()
        .map(|component| match component {
            Component::Normal(name) => Ok(name),
            _ => Err(SandboxError::from_io(
                SandboxIoOperation::OpenPolicyPath,
                &std::io::Error::from_raw_os_error(libc::EINVAL),
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if components.is_empty() {
        return Err(SandboxError::from_io(
            SandboxIoOperation::OpenPolicyPath,
            &std::io::Error::from_raw_os_error(libc::EINVAL),
        ));
    }
    for name in components {
        current_path.push(name);
        match openat_directory_nofollow(current.as_raw_fd(), name) {
            Ok(opened) => {
                current = opened;
                continue;
            }
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {}
            Err(error)
                if error.raw_os_error() == Some(libc::ELOOP)
                    || error.raw_os_error() == Some(libc::ENOTDIR) =>
            {
                return Err(SandboxError::UnrepresentablePolicy {
                    reason: crate::PolicyRepresentabilityError::PolicySymlinkRejected {
                        path: AbsolutePath::parse(current_path).map_err(|_| {
                            SandboxError::from_io(SandboxIoOperation::OpenPolicyPath, &error)
                        })?,
                    },
                });
            }
            Err(error) => {
                return Err(SandboxError::from_io(
                    SandboxIoOperation::OpenPolicyPath,
                    &error,
                ));
            }
        }
        match mkdirat_directory(current.as_raw_fd(), name) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {
                current = openat_directory_nofollow(current.as_raw_fd(), name).map_err(|error| {
                    SandboxError::from_io(SandboxIoOperation::OpenPolicyPath, &error)
                })?;
                continue;
            }
            Err(error) => {
                return Err(SandboxError::from_io(
                    SandboxIoOperation::OpenPolicyPath,
                    &error,
                ));
            }
        }
        let opened = openat_directory_nofollow(current.as_raw_fd(), name).map_err(|error| {
            SandboxError::from_io(SandboxIoOperation::OpenPolicyPath, &error)
        })?;
        let identity = ExecutableIdentity::from_fd(opened.as_raw_fd())?;
        synthetics.entries.push(SyntheticEntry {
            parent: duplicate_fd(current.as_raw_fd())?,
            name: cstring_name(name)?,
            identity,
        });
        current = opened;
    }
    Ok(current)
}

fn cstring_name(name: &OsStr) -> Result<CString, SandboxError> {
    CString::new(name.as_bytes()).map_err(|_| {
        SandboxError::from_io(
            SandboxIoOperation::OpenPolicyPath,
            &std::io::Error::from_raw_os_error(libc::EINVAL),
        )
    })
}

fn openat_directory_nofollow(parent: RawFd, name: &OsStr) -> Result<OwnedFd, std::io::Error> {
    let c_name = CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW;
    let fd = unsafe { libc::openat(parent, c_name.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn mkdirat_directory(parent: RawFd, name: &OsStr) -> Result<(), std::io::Error> {
    let c_name = CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    let created = unsafe { libc::mkdirat(parent, c_name.as_ptr(), 0o700) };
    if created != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn remove_synthetic_entry(entry: &SyntheticEntry) -> Result<(), SandboxError> {
    let opened = match openat_named(&entry.parent, &entry.name) {
        Ok(opened) => opened,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(SandboxError::from_io(
                SandboxIoOperation::OpenPolicyPath,
                &error,
            ));
        }
    };
    if !entry
        .identity
        .directory_matches_fd(opened.as_raw_fd())
        .unwrap_or(false)
    {
        return Ok(());
    }
    let removed =
        unsafe { libc::unlinkat(entry.parent.as_raw_fd(), entry.name.as_ptr(), libc::AT_REMOVEDIR) };
    if removed == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOTEMPTY) || error.raw_os_error() == Some(libc::EEXIST) {
        return Ok(());
    }
    Err(SandboxError::from_io(
        SandboxIoOperation::OpenPolicyPath,
        &error,
    ))
}

fn openat_named(parent: &OwnedFd, name: &CString) -> Result<OwnedFd, std::io::Error> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW;
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
