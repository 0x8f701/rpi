use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use crate::child::{ChildPlatformEnforcement, SandboxChildRequestTemplate};
use crate::launcher::{ExecutableIdentity, TrustedExecutable, open_pinned_executable};
use crate::platform::LinuxPlatformFacts;
use crate::spawn::{PreparedSpawnRequest, PreparedSpawnRequestParts};
use crate::{AbsolutePath, NetworkPolicy, PolicySnapshot, SandboxError, SandboxIoOperation};

use super::{
    LinuxMountAccess, LinuxMountEntryKind, LinuxPreparedSpawn, LinuxRevisionRoots,
    PinnedExecutable, PinnedMount, UnixFdConsumer, UnixTransportPurpose, UnixTransportSource,
    captured_open_file_limit, next_request_id,
};

pub(crate) fn prepare_linux_spawn(
    facts: &LinuxPlatformFacts,
    snapshot: &PolicySnapshot,
    retained_revision_roots: &crate::RetainedRevisionRoots,
    bwrap: &TrustedExecutable,
    request: PreparedSpawnRequest,
) -> Result<LinuxPreparedSpawn, SandboxError> {
    let mut parts = request.into_parts();
    let current_executable = pin_current_executable()?;
    let target_executable = resolve_target(&mut parts)?;
    let working_directory = resolve_working_directory(&mut parts)?;
    let PreparedMounts {
        mut rows,
        runtime_count,
        read_only_count,
        writable_count,
        protected_count,
        device_count,
        denied_file_count,
        denied_directories,
        synthetics,
    } = prepare_mounts(facts, snapshot)?;
    let revision_roots = prepare_revision_roots(retained_revision_roots)?;
    let canonical_mappings = parts
        .target_fd_mappings
        .iter()
        .map(crate::TargetFdMapping::canonical)
        .collect::<Vec<_>>();
    for mapping in parts.target_fd_mappings.iter() {
        let _ = mapping.canonical();
    }
    let enforcement = match snapshot.policy().network() {
        Some(NetworkPolicy::None) => ChildPlatformEnforcement::LinuxNoNetwork,
        Some(NetworkPolicy::FullAccess) => ChildPlatformEnforcement::LinuxFullNetwork,
        Some(NetworkPolicy::ProxyRouted) => return Err(SandboxError::ProxyRoutedUnavailable),
        None => ChildPlatformEnforcement::LinuxFullNetwork,
    };
    let child_request_template = build_template(snapshot, &parts, enforcement, canonical_mappings)?;
    let caller_mounts = prepare_caller_mounts(snapshot, parts.mount_sources)?;
    let caller_mount_count = caller_mounts.len();
    let writable_end = device_count + runtime_count + read_only_count + writable_count;
    rows.splice(writable_end..writable_end, caller_mounts);
    let mount_destinations = rows
        .iter()
        .map(|mount| (mount.destination.clone(), mount.access, mount.entry_kind))
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let mut transport_sources = rows
        .into_iter()
        .map(|mount| {
            UnixTransportSource::new(
                mount.source,
                UnixTransportPurpose::MountSource,
                UnixFdConsumer::BwrapMount,
            )
        })
        .collect::<Vec<_>>();
    for mapping in parts.target_fd_mappings {
        let (source, canonical) = mapping.into_parts();
        transport_sources.push(UnixTransportSource::new(
            source,
            UnixTransportPurpose::TargetMapping(canonical),
            UnixFdConsumer::HiddenChild,
        ));
    }
    Ok(LinuxPreparedSpawn {
        bwrap: bwrap.try_clone()?,
        stdio: parts.stdio,
        transport_sources: transport_sources.into_boxed_slice(),
        mount_destinations,
        captured_open_file_limit: captured_open_file_limit()?,
        child_request_template,
        current_executable,
        target_executable,
        working_directory,
        network: snapshot
            .policy()
            .network()
            .ok_or_else(|| SandboxError::InvalidSpawn {
                failure: crate::SpawnValidationError::FamilyTargetMappingMismatch,
            })?,
        cwd: parts.cwd,
        denied_directories: denied_directories.into_boxed_slice(),
        device_mount_count: device_count,
        runtime_mount_count: runtime_count,
        read_only_mount_count: read_only_count,
        writable_mount_count: writable_count + caller_mount_count,
        protected_mount_count: protected_count,
        denied_file_mount_count: denied_file_count,
        revision_roots,
        synthetics,
    })
}

fn build_template(
    snapshot: &PolicySnapshot,
    parts: &PreparedSpawnRequestParts,
    enforcement: ChildPlatformEnforcement,
    canonical_mappings: Vec<crate::CanonicalTargetFdMapping>,
) -> Result<SandboxChildRequestTemplate, SandboxError> {
    let argv = os_strings(&parts.argv)?;
    let environment = os_environment(&parts.environment)?;
    SandboxChildRequestTemplate::new(
        next_request_id(),
        parts.family,
        argv,
        path_string(&parts.cwd)?,
        environment,
        snapshot
            .policy()
            .limits()
            .ok_or_else(|| SandboxError::InvalidSpawn {
                failure: crate::SpawnValidationError::OpenFileLimitExceeded,
            })?,
        enforcement,
        canonical_mappings,
    )
}

fn pin_current_executable() -> Result<PinnedExecutable, SandboxError> {
    let canonical_path = AbsolutePath::parse(
        std::fs::canonicalize("/proc/self/exe")
            .map_err(|error| SandboxError::from_io(SandboxIoOperation::OpenExecutable, &error))?,
    )?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC)
        .open("/proc/self/exe")
        .map_err(|error| SandboxError::from_io(SandboxIoOperation::OpenExecutable, &error))?;
    let identity = ExecutableIdentity::from_fd(file.as_raw_fd())?;
    Ok(PinnedExecutable::new(file.into(), identity, canonical_path))
}

fn resolve_target(parts: &mut PreparedSpawnRequestParts) -> Result<PinnedExecutable, SandboxError> {
    if let Some(retained) = parts.retained_executable.take() {
        return retained_target(parts, retained);
    }
    let command = PathBuf::from(&parts.argv[0]);
    let candidates = if command.is_absolute() {
        vec![command]
    } else if command.components().count() > 1 {
        vec![parts.cwd.as_path().join(command)]
    } else {
        authorized_path_candidates(&parts.environment, &command)?
    };
    for candidate in candidates {
        let Ok(canonical) = std::fs::canonicalize(&candidate) else {
            continue;
        };
        let Ok((descriptor, identity)) = open_pinned_executable(&canonical) else {
            continue;
        };
        return Ok(PinnedExecutable::new(
            descriptor,
            identity,
            AbsolutePath::parse(canonical)?,
        ));
    }
    Err(SandboxError::from_io(
        SandboxIoOperation::OpenExecutable,
        &std::io::Error::from_raw_os_error(libc::ENOENT),
    ))
}

fn retained_target(
    parts: &PreparedSpawnRequestParts,
    retained: crate::RetainedExecutable,
) -> Result<PinnedExecutable, SandboxError> {
    let command = AbsolutePath::parse(PathBuf::from(&parts.argv[0]))?;
    let (executable, canonical_path) = retained.into_parts();
    if command != canonical_path {
        return Err(SandboxError::InvalidSpawn {
            failure: crate::SpawnValidationError::InvalidRetainedExecutable,
        });
    }
    let identity = ExecutableIdentity::from_fd(executable.as_raw_fd())?;
    Ok(PinnedExecutable::new(executable, identity, canonical_path))
}

fn resolve_working_directory(
    parts: &mut PreparedSpawnRequestParts,
) -> Result<Option<super::PinnedWorkingDirectory>, SandboxError> {
    let Some(retained) = parts.retained_working_directory.take() else {
        return Ok(None);
    };
    let (directory, canonical_path) = retained.into_parts();
    if canonical_path != parts.cwd {
        return Err(SandboxError::InvalidSpawn {
            failure: crate::SpawnValidationError::InvalidRetainedWorkingDirectory,
        });
    }
    let identity = ExecutableIdentity::from_fd(directory.as_raw_fd())?;
    Ok(Some(super::PinnedWorkingDirectory::new(
        directory,
        identity,
        canonical_path,
    )))
}

fn authorized_path_candidates(
    environment: &BTreeMap<OsString, OsString>,
    command: &Path,
) -> Result<Vec<PathBuf>, SandboxError> {
    let path = environment.get(&OsString::from("PATH")).ok_or_else(|| {
        SandboxError::from_io(
            SandboxIoOperation::OpenExecutable,
            &std::io::Error::from_raw_os_error(libc::ENOENT),
        )
    })?;
    Ok(std::env::split_paths(path)
        .filter(|directory| directory.is_absolute() && !directory.as_os_str().is_empty())
        .map(|directory| directory.join(command))
        .collect())
}

struct PreparedMounts {
    device_count: usize,
    rows: Vec<PinnedMount>,
    runtime_count: usize,
    read_only_count: usize,
    writable_count: usize,
    protected_count: usize,
    denied_file_count: usize,
    denied_directories: Vec<AbsolutePath>,
    synthetics: crate::synthetic::SyntheticDirs,
}

fn prepare_mounts(
    facts: &LinuxPlatformFacts,
    snapshot: &PolicySnapshot,
) -> Result<PreparedMounts, SandboxError> {
    let filesystem = snapshot
        .policy()
        .filesystem()
        .ok_or_else(|| SandboxError::InvalidSpawn {
            failure: crate::SpawnValidationError::FamilyTargetMappingMismatch,
        })?;
    let mut rows: Vec<PinnedMount> = Vec::new();
    let mut denied_directories: Vec<AbsolutePath> = Vec::new();
    // Destinations are deduplicated across buckets keeping the first occurrence,
    // so the fixed platform runtime projections are never re-emitted as generic
    // snapshot read-only rows when a resolved filesystem echoes them back.
    let mut seen: BTreeSet<AbsolutePath> = BTreeSet::new();

    for path in facts.device_paths() {
        if !seen.insert(path.clone()) {
            continue;
        }
        rows.push(open_policy_mount(path, LinuxMountAccess::ReadWrite)?);
    }
    let device_count = rows.len();
    // Only the fixed platform runtime source candidates may canonicalize their
    // source so merged-usr aliases (e.g. /lib -> /usr/lib) bind correctly; the
    // destination stays the literal candidate alias.
    for path in facts.runtime_roots() {
        if !seen.insert(path.clone()) {
            continue;
        }
        rows.push(open_runtime_mount(path, LinuxMountAccess::ReadOnly)?);
    }
    let runtime_count = rows.len() - device_count;
    // Generic policy mounts must not canonicalize or follow leaf symlinks: a
    // leaf symlink at a policy path is rejected fail-closed instead of being
    // quietly redirected to its target.
    for path in filesystem.read_only_roots() {
        if !seen.insert(path.clone()) {
            continue;
        }
        rows.push(open_policy_mount(path, LinuxMountAccess::ReadOnly)?);
    }
    let read_only_count = rows.len() - device_count - runtime_count;
    if let Some(scratch) = filesystem.sealed_scratch_root() {
        if seen.insert(scratch.clone()) {
            rows.push(open_policy_mount(scratch, LinuxMountAccess::ReadWrite)?);
        }
    }
    for root in filesystem.writable_roots() {
        if !seen.insert(root.root().clone()) {
            continue;
        }
        rows.push(open_policy_mount(root.root(), LinuxMountAccess::ReadWrite)?);
    }
    let writable_count = rows.len() - device_count - runtime_count - read_only_count;
    // Protected and denied paths are HIGHER precedence than the permissive
    // device/runtime/read-only/writable mounts: they must overlay an earlier
    // permissive mount at the same destination (e.g. a session path that is both
    // echoed as a read-only root and denied) instead of being silently dropped
    // by the permissive dedup. bwrap applies later mounts on top, so emitting
    // them after the permissive rows makes the restrictive overlay win. Only
    // exact duplicates WITHIN the same precedence bucket are skipped.
    let mut protected_seen: BTreeSet<AbsolutePath> = BTreeSet::new();
    let mut synthetics = crate::synthetic::SyntheticDirs::default();
    for root in filesystem.writable_roots() {
        for relative in root.protected_descendants() {
            let destination = AbsolutePath::parse(root.root().as_path().join(relative.as_path()))?;
            if !protected_seen.insert(destination.clone()) {
                continue;
            }
            if let Some(parent_fd) = rows
                .iter()
                .find(|row| row.destination == *root.root())
                .map(|row| row.source.as_raw_fd())
            {
                let source = crate::synthetic::open_or_create_protected_directory(
                    parent_fd,
                    root.root(),
                    relative,
                    &mut synthetics,
                )?;
                rows.push(pinned_mount_from(
                    File::from(source),
                    &destination,
                    LinuxMountAccess::ReadOnly,
                )?);
            } else {
                rows.push(open_policy_mount(&destination, LinuxMountAccess::ReadOnly)?);
            }
        }
    }
    let protected_count =
        rows.len() - device_count - runtime_count - read_only_count - writable_count;
    // Denied is the highest precedence: it overlays permissive AND protected
    // mounts, so it is never skipped for colliding with an earlier mount.
    // Private roots share that highest precedence: they seal a retained store
    // base that must stay unreadable even when it nests beneath a writable
    // workspace mount.
    let mut denied_seen: BTreeSet<AbsolutePath> = BTreeSet::new();
    for denied in filesystem
        .private_roots()
        .iter()
        .chain(filesystem.denied_paths().iter().filter(|denied| {
            !filesystem
                .private_roots()
                .iter()
                .any(|private| denied.as_path().starts_with(private.as_path()))
        }))
    {
        match classify_denied_path(denied)? {
            None => {}
            Some((DeniedKind::File, path)) => {
                if !denied_seen.insert(path.clone()) {
                    continue;
                }
                rows.push(create_zero_mask(&path)?);
            }
            Some((DeniedKind::Directory, path)) => {
                if !denied_seen.insert(path.clone()) {
                    continue;
                }
                denied_directories.push(path);
            }
        }
    }
    let denied_file_count = rows.len()
        - device_count
        - runtime_count
        - read_only_count
        - writable_count
        - protected_count;
    Ok(PreparedMounts {
        rows,
        device_count,
        runtime_count,
        read_only_count,
        writable_count,
        protected_count,
        denied_file_count,
        denied_directories,
        synthetics,
    })
}

fn prepare_caller_mounts(
    snapshot: &PolicySnapshot,
    sources: Box<[crate::MountSource]>,
) -> Result<Vec<PinnedMount>, SandboxError> {
    sources
        .into_vec()
        .into_iter()
        .map(|source| {
            let destination = source.destination().clone();
            validate_caller_mount_destination(snapshot, &destination)?;
            let (source, destination) = source.into_parts();
            let file = std::fs::File::from(source);
            let metadata = file.metadata().map_err(|error| {
                SandboxError::from_io(SandboxIoOperation::InspectDescriptor, &error)
            })?;
            if !metadata.is_dir() {
                return Err(SandboxError::InvalidSpawn {
                    failure: crate::SpawnValidationError::InvalidMountSource,
                });
            }
            Ok(PinnedMount {
                identity: ExecutableIdentity::from_fd(file.as_raw_fd())?,
                source: file.into(),
                destination,
                access: LinuxMountAccess::ReadWrite,
                entry_kind: LinuxMountEntryKind::Directory,
            })
        })
        .collect()
}

fn validate_caller_mount_destination(
    snapshot: &PolicySnapshot,
    destination: &AbsolutePath,
) -> Result<(), SandboxError> {
    let filesystem = snapshot
        .policy()
        .filesystem()
        .ok_or(SandboxError::InvalidSpawn {
            failure: crate::SpawnValidationError::MountDestinationOutsideWritableAuthority,
        })?;
    super::mount_validation::validate_caller_mount_destination(filesystem, destination)
}

fn prepare_revision_roots(
    revision_roots: &crate::RetainedRevisionRoots,
) -> Result<LinuxRevisionRoots, SandboxError> {
    let [home, tmp, scratch] = revision_roots.try_clone_parts()?;
    // Identity-bind: revalidate every retained revision root FD against its declared
    // path at staging so a directory replaced between probe and staging is rejected.
    // The read-only revision `tmp` is validated but not projected: the writable sealed
    // scratch is projected at /tmp (TMPDIR), not the read-only revision tmp.
    let home = pinned_revision_root(home)?;
    let scratch = pinned_revision_root(scratch)?;
    revalidate_revision_root(&tmp)?;
    Ok(LinuxRevisionRoots { home, scratch })
}

fn pinned_revision_root(
    (source, destination): (OwnedFd, AbsolutePath),
) -> Result<PinnedMount, SandboxError> {
    let identity = ExecutableIdentity::from_fd(source.as_raw_fd())?;
    revalidate_directory_identity(&destination, identity)?;
    Ok(PinnedMount {
        source,
        identity,
        destination,
        access: LinuxMountAccess::ReadWrite,
        entry_kind: LinuxMountEntryKind::Directory,
    })
}

fn revalidate_revision_root(
    (source, destination): &(OwnedFd, AbsolutePath),
) -> Result<(), SandboxError> {
    let identity = ExecutableIdentity::from_fd(source.as_raw_fd())?;
    revalidate_directory_identity(destination, identity)
}

fn revalidate_directory_identity(
    destination: &AbsolutePath,
    expected: ExecutableIdentity,
) -> Result<(), SandboxError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(destination.as_path())
        .map_err(|error| SandboxError::from_io(SandboxIoOperation::OpenPolicyPath, &error))?;
    if !expected.directory_matches_fd(file.as_raw_fd())? {
        return Err(SandboxError::InvalidSpawn {
            failure: crate::SpawnValidationError::InvalidRevisionRoots,
        });
    }
    Ok(())
}

fn open_runtime_mount(
    path: &AbsolutePath,
    access: LinuxMountAccess,
) -> Result<PinnedMount, SandboxError> {
    // Fixed platform runtime source candidates may canonicalize their source so
    // merged-usr aliases (e.g. /lib -> /usr/lib) bind to the real inode; the
    // destination stays the literal candidate alias the child sees.
    let source_path = std::fs::canonicalize(path.as_path())
        .map_err(|error| SandboxError::from_io(SandboxIoOperation::OpenPolicyPath, &error))?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_PATH)
        .open(&source_path)
        .map_err(|error| SandboxError::from_io(SandboxIoOperation::OpenPolicyPath, &error))?;
    pinned_mount_from(file, path, access)
}

fn open_policy_mount(
    path: &AbsolutePath,
    access: LinuxMountAccess,
) -> Result<PinnedMount, SandboxError> {
    // Generic policy mounts must not canonicalize or follow a leaf symlink: an
    // adversarial symlink at a policy path is rejected fail-closed rather than
    // quietly rebound to its target. Ancestor symlinks are resolved by the
    // kernel during open; only the leaf is pinned with O_NOFOLLOW.
    let metadata = std::fs::symlink_metadata(path.as_path()).map_err(|_| {
        SandboxError::UnrepresentablePolicy {
            reason: crate::PolicyRepresentabilityError::MissingPolicyRoot { path: path.clone() },
        }
    })?;
    if metadata.file_type().is_symlink() {
        return Err(SandboxError::UnrepresentablePolicy {
            reason: crate::PolicyRepresentabilityError::PolicySymlinkRejected {
                path: path.clone(),
            },
        });
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_PATH)
        .open(path.as_path())
        .map_err(|error| SandboxError::from_io(SandboxIoOperation::OpenPolicyPath, &error))?;
    pinned_mount_from(file, path, access)
}

fn pinned_mount_from(
    file: std::fs::File,
    destination: &AbsolutePath,
    access: LinuxMountAccess,
) -> Result<PinnedMount, SandboxError> {
    let metadata = file
        .metadata()
        .map_err(|error| SandboxError::from_io(SandboxIoOperation::InspectDescriptor, &error))?;
    let entry_kind = if metadata.is_dir() {
        LinuxMountEntryKind::Directory
    } else if metadata.is_file() {
        LinuxMountEntryKind::RegularFile
    } else if metadata.file_type().is_char_device() {
        LinuxMountEntryKind::CharacterDevice
    } else {
        return Err(SandboxError::from_io(
            SandboxIoOperation::BuildMount,
            &std::io::Error::from_raw_os_error(libc::EINVAL),
        ));
    };
    Ok(PinnedMount {
        identity: ExecutableIdentity::from_fd(file.as_raw_fd())?,
        source: file.into(),
        destination: destination.clone(),
        access,
        entry_kind,
    })
}

enum DeniedKind {
    File,
    Directory,
}

fn classify_denied_path(
    denied: &AbsolutePath,
) -> Result<Option<(DeniedKind, AbsolutePath)>, SandboxError> {
    let metadata = std::fs::symlink_metadata(denied.as_path()).map_err(|_| {
        SandboxError::UnrepresentablePolicy {
            reason: crate::PolicyRepresentabilityError::MissingDeniedPath {
                path: denied.clone(),
            },
        }
    })?;
    if !metadata.file_type().is_symlink() {
        if metadata.is_file() {
            return Ok(Some((DeniedKind::File, denied.clone())));
        }
        if metadata.is_dir() {
            return Ok(Some((DeniedKind::Directory, denied.clone())));
        }
        return Err(denied_symlink_error(denied));
    }
    match std::fs::metadata(denied.as_path()) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(denied_symlink_error(denied)),
        Ok(target) => {
            let resolved = std::fs::canonicalize(denied.as_path())
                .map_err(|_| denied_symlink_error(denied))?;
            let path = AbsolutePath::parse(resolved).map_err(|_| denied_symlink_error(denied))?;
            if target.is_file() {
                Ok(Some((DeniedKind::File, path)))
            } else if target.is_dir() {
                Ok(Some((DeniedKind::Directory, path)))
            } else {
                Err(denied_symlink_error(denied))
            }
        }
    }
}

fn denied_symlink_error(path: &AbsolutePath) -> SandboxError {
    SandboxError::UnrepresentablePolicy {
        reason: crate::PolicyRepresentabilityError::DeniedSymlink { path: path.clone() },
    }
}

fn create_zero_mask(destination: &AbsolutePath) -> Result<PinnedMount, SandboxError> {
    let descriptor = unsafe_memfd_create(c"rpi-denied-mask".as_ptr(), libc::MFD_CLOEXEC);
    if descriptor < 0 {
        return Err(SandboxError::from_io(
            SandboxIoOperation::BuildMount,
            &std::io::Error::last_os_error(),
        ));
    }
    let file = unsafe_owned_fd(descriptor);
    if unsafe_fchmod(file.as_raw_fd(), 0) != 0 || unsafe_fsync(file.as_raw_fd()) != 0 {
        return Err(SandboxError::from_io(
            SandboxIoOperation::BuildMount,
            &std::io::Error::last_os_error(),
        ));
    }
    Ok(PinnedMount {
        identity: ExecutableIdentity::from_fd(file.as_raw_fd())?,
        source: file,
        destination: destination.clone(),
        access: LinuxMountAccess::DenyFileMask,
        entry_kind: LinuxMountEntryKind::RegularFile,
    })
}

#[allow(unsafe_code)]
fn unsafe_memfd_create(name: *const libc::c_char, flags: libc::c_uint) -> RawFd {
    unsafe { libc::memfd_create(name, flags) }
}

#[allow(unsafe_code)]
fn unsafe_fchmod(descriptor: RawFd, mode: libc::mode_t) -> i32 {
    unsafe { libc::fchmod(descriptor, mode) }
}

#[allow(unsafe_code)]
fn unsafe_fsync(descriptor: RawFd) -> i32 {
    unsafe { libc::fsync(descriptor) }
}

#[allow(unsafe_code)]
fn unsafe_owned_fd(descriptor: RawFd) -> OwnedFd {
    unsafe { OwnedFd::from_raw_fd(descriptor) }
}

fn os_strings(values: &[OsString]) -> Result<Vec<String>, SandboxError> {
    values
        .iter()
        .map(|value| {
            std::str::from_utf8(value.as_bytes())
                .map(str::to_owned)
                .map_err(|_| SandboxError::InvalidChildProtocol {
                    failure: crate::ChildProtocolError::InvalidJson,
                })
        })
        .collect()
}

fn os_environment(
    environment: &BTreeMap<OsString, OsString>,
) -> Result<BTreeMap<String, String>, SandboxError> {
    environment
        .iter()
        .map(|(name, value)| {
            Ok((
                std::str::from_utf8(name.as_bytes())
                    .map_err(|_| SandboxError::InvalidChildProtocol {
                        failure: crate::ChildProtocolError::InvalidJson,
                    })?
                    .to_owned(),
                std::str::from_utf8(value.as_bytes())
                    .map_err(|_| SandboxError::InvalidChildProtocol {
                        failure: crate::ChildProtocolError::InvalidJson,
                    })?
                    .to_owned(),
            ))
        })
        .collect()
}

fn path_string(path: &AbsolutePath) -> Result<String, SandboxError> {
    path.as_path()
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| SandboxError::InvalidChildProtocol {
            failure: crate::ChildProtocolError::InvalidCwd,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::OpenOptionsExt as _;

    #[test]
    fn target_resolution_pins_inode() {
        assert!(std::mem::size_of::<PinnedExecutable>() > std::mem::size_of::<OwnedFd>());
    }

    #[test]
    fn current_binary_uses_proc_self_exe_fd() {
        let pinned = pin_current_executable().expect("current executable");
        assert!(
            pinned
                .identity()
                .matches_fd(pinned.raw_fd())
                .expect("identity")
        );
    }

    #[test]
    fn retained_executable_survives_path_replacement() {
        use std::io::Write as _;
        use std::os::fd::AsRawFd as _;
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().expect("temporary executable directory");
        let path = directory.path().join("extension");
        let backup = directory.path().join("original-extension");
        let mut original = std::fs::File::create(&path).expect("create original executable");
        original
            .write_all(b"#!/bin/sh\nexit 17\n")
            .expect("write original executable");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("make original executable");
        drop(original);

        let original = std::fs::File::open(&path).expect("open original executable");
        let original_identity = ExecutableIdentity::from_fd(original.as_raw_fd())
            .expect("original executable identity");
        let canonical_path = AbsolutePath::parse(path.clone()).expect("canonical test path");
        let retained = crate::RetainedExecutable::new(original.into(), canonical_path.clone())
            .expect("retain original executable");

        std::fs::rename(&path, &backup).expect("retain original inode under backup name");
        let mut replacement = std::fs::File::create(&path).expect("create replacement executable");
        replacement
            .write_all(b"#!/bin/sh\nexit 23\n")
            .expect("write replacement executable");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("make replacement executable");
        let replacement_identity = ExecutableIdentity::from_fd(replacement.as_raw_fd())
            .expect("replacement executable identity");

        let parts = PreparedSpawnRequestParts {
            family: crate::SpawnFamily::ProcessExtension,
            argv: vec![path.as_os_str().to_owned()].into_boxed_slice(),
            cwd: AbsolutePath::parse(directory.path().to_path_buf()).expect("absolute cwd"),
            environment: BTreeMap::new(),
            stdio: crate::spawn::SpawnStdioParts::Null,
            target_fd_mappings: Vec::new().into_boxed_slice(),
            mount_sources: Vec::new().into_boxed_slice(),
            retained_executable: None,
            retained_working_directory: None,
        };
        let pinned = retained_target(&parts, retained).expect("consume retained executable");

        assert!(
            original_identity
                .matches_fd(pinned.raw_fd())
                .expect("original identity"),
            "renaming the retained inode may change ctime but must preserve executable identity",
        );
        assert!(
            !replacement_identity
                .matches_fd(pinned.raw_fd())
                .expect("replacement identity"),
            "path replacement must not replace the retained executable",
        );
    }

    #[test]
    fn prepared_plan_owns_duplicated_launcher() {
        assert!(std::mem::needs_drop::<LinuxPreparedSpawn>());
    }

    #[test]
    fn every_bwrap_argv_fd_survives_wrapper_exec() {
        assert!(std::mem::needs_drop::<UnixTransportSource>());
    }

    fn absolute(path: &Path) -> AbsolutePath {
        AbsolutePath::parse(path.to_path_buf()).expect("absolute path")
    }

    fn mount_source(path: &Path) -> crate::MountSource {
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY)
            .open(path)
            .expect("open mount source");
        crate::MountSource::read_write_directory(directory.into(), absolute(path))
            .expect("mount source")
    }

    fn snapshot(filesystem: crate::FileSystemPolicy) -> PolicySnapshot {
        PolicySnapshot::new(
            crate::PolicyRevision::new(1).expect("revision"),
            crate::HostPlatform::Linux,
            crate::SandboxPolicy::workspace_write(
                filesystem,
                NetworkPolicy::None,
                crate::HostHttpPolicy::new(Vec::new()).expect("HTTP policy"),
                crate::CredentialPolicy::new(Vec::new()).expect("credential policy"),
                crate::ResourceLimits::confined_defaults(),
            )
            .expect("sandbox policy"),
        )
        .expect("snapshot")
    }

    #[test]
    fn caller_mount_destinations_cannot_widen_or_override_policy() {
        let root = tempfile::tempdir().expect("fixture root");
        let writable = root.path().join("writable");
        let safe_writable = root.path().join("safe-writable");
        let read_only = root.path().join("read-only");
        let denied = writable.join("denied");
        let protected = writable.join("protected");
        for path in [&writable, &safe_writable, &read_only, &denied, &protected] {
            std::fs::create_dir(path).expect("fixture directory");
        }
        let filesystem = crate::FileSystemPolicy::new(
            vec![absolute(&read_only)],
            vec![
                crate::WritableRoot::new(
                    absolute(&writable),
                    vec![crate::RelativePath::parse("protected".into()).expect("protected")],
                )
                .expect("writable root"),
                crate::WritableRoot::new(absolute(&safe_writable), Vec::new())
                    .expect("safe writable root"),
            ],
            vec![absolute(&denied)],
        )
        .expect("filesystem policy");
        let snapshot = snapshot(filesystem);

        assert!(
            prepare_caller_mounts(
                &snapshot,
                vec![mount_source(&safe_writable)].into_boxed_slice(),
            )
            .is_ok()
        );
        for (path, failure) in [
            (
                &read_only,
                crate::SpawnValidationError::MountDestinationOutsideWritableAuthority,
            ),
            (
                &writable,
                crate::SpawnValidationError::MountDestinationDenied,
            ),
            (
                &protected,
                crate::SpawnValidationError::MountDestinationOutsideWritableAuthority,
            ),
        ] {
            assert!(matches!(
                prepare_caller_mounts(&snapshot, vec![mount_source(path)].into_boxed_slice()),
                Err(SandboxError::InvalidSpawn { failure: actual }) if actual == failure
            ));
        }
    }

    #[test]
    fn denied_directories_are_not_fd_mount_rows() {
        let root = tempfile::tempdir().expect("fixture root");
        let denied = root.path().join("denied-directory");
        std::fs::create_dir(&denied).expect("denied directory");
        let facts = crate::derive_platform_facts(
            crate::PlatformFactInputs::new(
                "denied-directory".to_owned(),
                vec![absolute(root.path())],
                absolute(root.path()),
                absolute(&denied),
            )
            .expect("platform inputs"),
        )
        .expect("platform facts");
        let linux = facts.linux().expect("Linux facts");
        let prepared = prepare_mounts(
            linux,
            &snapshot(
                crate::FileSystemPolicy::new(
                    Vec::new(),
                    vec![
                        crate::WritableRoot::new(absolute(root.path()), Vec::new())
                            .expect("writable root"),
                    ],
                    vec![absolute(&denied)],
                )
                .expect("filesystem policy"),
            ),
        )
        .expect("prepared mounts");

        assert_eq!(prepared.denied_directories, vec![absolute(&denied)]);
        assert!(
            !prepared
                .rows
                .iter()
                .any(|row| row.destination == absolute(&denied))
        );
    }

    #[test]
    fn denied_symlink_masks_resolved_target_and_skips_dangling() {
        let root = tempfile::tempdir().expect("fixture root");
        let target = root.path().join("target");
        std::fs::create_dir(&target).expect("symlink target directory");
        let link = root.path().join("denied-link");
        std::os::unix::fs::symlink(&target, &link).expect("denied symlink");
        let dangling = root.path().join("dangling-link");
        std::os::unix::fs::symlink(root.path().join("missing"), &dangling)
            .expect("dangling symlink");
        let facts = crate::derive_platform_facts(
            crate::PlatformFactInputs::new(
                "denied-symlink".to_owned(),
                vec![absolute(root.path())],
                absolute(root.path()),
                absolute(&link),
            )
            .expect("platform inputs"),
        )
        .expect("platform facts");
        let linux = facts.linux().expect("Linux facts");
        let prepared = prepare_mounts(
            linux,
            &snapshot(
                crate::FileSystemPolicy::new(
                    Vec::new(),
                    vec![
                        crate::WritableRoot::new(absolute(root.path()), Vec::new())
                            .expect("writable root"),
                    ],
                    vec![absolute(&link), absolute(&dangling)],
                )
                .expect("filesystem policy"),
            ),
        )
        .expect("denied symlink resolves to target");
        assert_eq!(prepared.denied_directories, vec![absolute(&target)]);
        assert!(!prepared.denied_directories.contains(&absolute(&dangling)));
    }

    #[test]
    fn revision_root_identity_mismatch_is_rejected() {
        use std::os::fd::AsRawFd as _;
        let root = tempfile::tempdir().expect("fixture root");
        let dir = root.path().join("rev-home");
        std::fs::create_dir(&dir).expect("revision root directory");
        let file = std::fs::File::open(&dir).expect("open retained directory fd");
        let identity = ExecutableIdentity::from_fd(file.as_raw_fd()).expect("identity");
        let backup = root.path().join("rev-home-backup");
        std::fs::rename(&dir, &backup).expect("retain original inode under backup name");
        std::fs::create_dir(&dir).expect("recreate a different inode at the declared path");
        let result = revalidate_directory_identity(&absolute(&dir), identity);
        assert!(matches!(
            result,
            Err(SandboxError::InvalidSpawn {
                failure: crate::SpawnValidationError::InvalidRevisionRoots,
            })
        ));
    }

    #[test]
    fn runtime_mount_canonicalizes_source_but_keeps_literal_alias_destination() {
        use std::os::fd::AsRawFd as _;
        let root = tempfile::tempdir().expect("fixture root");
        let real = root.path().join("real-runtime");
        std::fs::create_dir(&real).expect("real runtime directory");
        let alias = root.path().join("alias-runtime");
        std::os::unix::fs::symlink(&real, &alias).expect("runtime alias symlink");
        let alias_path = absolute(&alias);
        let mount = open_runtime_mount(&alias_path, LinuxMountAccess::ReadOnly)
            .expect("runtime mount canonicalizes the source");
        // Destination stays the literal alias the child sees.
        assert_eq!(mount.destination, alias_path);
        // Source fd resolves through the symlink to the real inode.
        let real_file = std::fs::File::open(&real).expect("open real runtime");
        assert!(
            mount
                .identity
                .directory_matches_fd(real_file.as_raw_fd())
                .expect("compare runtime identity"),
            "runtime source fd must point at the canonicalized real inode"
        );
    }

    #[test]
    fn policy_mount_rejects_leaf_symlink_fail_closed() {
        let root = tempfile::tempdir().expect("fixture root");
        let real = root.path().join("real-target");
        std::fs::create_dir(&real).expect("real target directory");
        let link = root.path().join("policy-link");
        std::os::unix::fs::symlink(&real, &link).expect("adversarial policy symlink");
        let result = open_policy_mount(&absolute(&link), LinuxMountAccess::ReadOnly);
        assert!(matches!(
            result,
            Err(SandboxError::UnrepresentablePolicy {
                reason: crate::PolicyRepresentabilityError::PolicySymlinkRejected { .. }
            })
        ));
    }

    #[test]
    fn policy_mount_rejects_missing_root_fail_closed() {
        let root = tempfile::tempdir().expect("fixture root");
        let missing = root.path().join("does-not-exist");
        let result = open_policy_mount(&absolute(&missing), LinuxMountAccess::ReadOnly);
        assert!(matches!(
            result,
            Err(SandboxError::UnrepresentablePolicy {
                reason: crate::PolicyRepresentabilityError::MissingPolicyRoot { .. }
            })
        ));
    }

    #[test]
    fn prepare_mounts_dedups_runtime_echoed_by_resolved_snapshot() {
        let root = tempfile::tempdir().expect("fixture root");
        let writable = root.path().join("writable");
        std::fs::create_dir(&writable).expect("writable directory");
        let facts = crate::derive_platform_facts(
            crate::PlatformFactInputs::new(
                "dedup-runtime".to_owned(),
                vec![absolute(&writable)],
                absolute(&writable),
                absolute(&writable),
            )
            .expect("platform inputs"),
        )
        .expect("platform facts");
        let linux = facts.linux().expect("Linux facts");
        // A resolved filesystem echoes every runtime root back as a read-only
        // root (the snapshot path) plus a fresh directory; the runtime rows must
        // not be duplicated by the snapshot read-only rows.
        let extra = root.path().join("extra-read-only");
        std::fs::create_dir(&extra).expect("extra read-only directory");
        let read_only_roots = facts
            .runtime_roots()
            .iter()
            .cloned()
            .chain([absolute(&extra)])
            .collect::<Vec<_>>();
        let prepared = prepare_mounts(
            linux,
            &snapshot(
                crate::FileSystemPolicy::new(
                    read_only_roots,
                    vec![
                        crate::WritableRoot::new(absolute(&writable), Vec::new())
                            .expect("writable root"),
                    ],
                    Vec::new(),
                )
                .expect("filesystem policy"),
            ),
        )
        .expect("deduped mounts");
        let mut destinations = prepared
            .rows
            .iter()
            .map(|mount| mount.destination.clone())
            .collect::<Vec<_>>();
        destinations.sort();
        let original = destinations.len();
        destinations.dedup();
        assert_eq!(
            destinations.len(),
            original,
            "no duplicate mount destinations across runtime and snapshot rows"
        );
        // The extra read-only root is still represented exactly once.
        assert_eq!(
            prepared
                .rows
                .iter()
                .filter(|mount| mount.destination == absolute(&extra))
                .count(),
            1
        );
        // The read-only count reflects only the deduped snapshot additions.
        assert_eq!(prepared.read_only_count, 1);
    }

    #[test]
    fn prepare_mounts_rejects_adversarial_read_only_symlink() {
        let root = tempfile::tempdir().expect("fixture root");
        let writable = root.path().join("writable");
        let real = root.path().join("real-target");
        std::fs::create_dir(&writable).expect("writable directory");
        std::fs::create_dir(&real).expect("real target directory");
        let link = root.path().join("read-only-link");
        std::os::unix::fs::symlink(&real, &link).expect("adversarial read-only symlink");
        let facts = crate::derive_platform_facts(
            crate::PlatformFactInputs::new(
                "read-only-symlink".to_owned(),
                vec![absolute(&writable)],
                absolute(&writable),
                absolute(&writable),
            )
            .expect("platform inputs"),
        )
        .expect("platform facts");
        let linux = facts.linux().expect("Linux facts");
        let result = prepare_mounts(
            linux,
            &snapshot(
                crate::FileSystemPolicy::new(
                    vec![absolute(&link)],
                    vec![
                        crate::WritableRoot::new(absolute(&writable), Vec::new())
                            .expect("writable root"),
                    ],
                    Vec::new(),
                )
                .expect("filesystem policy"),
            ),
        );
        assert!(matches!(
            result,
            Err(SandboxError::UnrepresentablePolicy {
                reason: crate::PolicyRepresentabilityError::PolicySymlinkRejected { .. }
            })
        ));
    }

    #[test]
    fn protected_and_denied_collide_at_same_directory_both_emit() {
        let root = tempfile::tempdir().expect("fixture root");
        let workspace = root.path().join("workspace");
        let session = workspace.join("session");
        std::fs::create_dir_all(&session).expect("workspace-contained session directory");
        let facts = crate::derive_platform_facts(
            crate::PlatformFactInputs::new(
                "protected-denied-collision".to_owned(),
                vec![absolute(&workspace)],
                absolute(&workspace),
                absolute(&workspace),
            )
            .expect("platform inputs"),
        )
        .expect("platform facts");
        let linux = facts.linux().expect("Linux facts");
        // The same directory is a protected descendant of the writable workspace
        // AND a denied path. The protected RO row and the denied directory
        // overlay must BOTH be emitted (denied overlays protected in bwrap);
        // the first-wins dedup must not silently drop the denied overlay and
        // expose the session through the protected mount.
        let prepared = prepare_mounts(
            linux,
            &snapshot(
                crate::FileSystemPolicy::new(
                    Vec::new(),
                    vec![
                        crate::WritableRoot::new(
                            absolute(&workspace),
                            vec![
                                crate::RelativePath::parse("session".into())
                                    .expect("protected descendant"),
                            ],
                        )
                        .expect("writable root"),
                    ],
                    vec![absolute(&session)],
                )
                .expect("filesystem policy"),
            ),
        )
        .expect("prepared mounts");
        let session_path = absolute(&session);
        assert!(
            prepared.rows.iter().any(|mount| {
                mount.destination == session_path && mount.access == LinuxMountAccess::ReadOnly
            }),
            "protected RO row must be emitted at the colliding session path"
        );
        assert_eq!(
            prepared.denied_directories,
            vec![session_path],
            "denied directory overlay must be emitted at the colliding session path"
        );
    }
}
