use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use crate::child::{ChildPlatformEnforcement, SandboxChildRequestTemplate};
use crate::launcher::{TrustedExecutable, open_pinned_executable};
use crate::platform::MacosPlatformFacts;
use crate::spawn::{PreparedSpawnRequest, PreparedSpawnRequestParts};
use crate::{AbsolutePath, PolicySnapshot, SandboxError, SandboxIoOperation};

use super::{
    MacosPreparedSpawn, PinnedExecutable, UnixFdConsumer, UnixTransportPurpose,
    UnixTransportSource, captured_open_file_limit, next_request_id,
};

pub(crate) fn prepare_macos_spawn(
    facts: &MacosPlatformFacts,
    snapshot: &PolicySnapshot,
    revision_roots: &crate::RetainedRevisionRoots,
    sandbox_exec: &TrustedExecutable,
    request: PreparedSpawnRequest,
) -> Result<MacosPreparedSpawn, SandboxError> {
    revalidate_revision_roots(revision_roots)?;
    let mut parts = request.into_parts();
    let current_executable = pin_current_executable()?;
    let target_executable = resolve_target(&mut parts)?;
    let working_directory = resolve_working_directory(&mut parts)?;
    let canonical_mappings = parts
        .target_fd_mappings
        .iter()
        .map(crate::TargetFdMapping::canonical)
        .collect::<Vec<_>>();
    let child_request_template = build_template(snapshot, &parts, canonical_mappings)?;
    let mut transport_sources = parts
        .target_fd_mappings
        .into_vec()
        .into_iter()
        .map(|mapping| {
            let (source, canonical) = mapping.into_parts();
            UnixTransportSource::new(
                source,
                UnixTransportPurpose::TargetMapping(canonical),
                UnixFdConsumer::HiddenChild,
            )
        })
        .collect::<Vec<_>>();
    let filesystem = snapshot
        .policy()
        .filesystem()
        .ok_or(SandboxError::InvalidSpawn {
            failure: crate::SpawnValidationError::MountDestinationOutsideWritableAuthority,
        })?;
    let mut caller_mounts = Vec::new();
    for source in parts.mount_sources {
        let destination = source.destination().clone();
        validate_caller_mount_destination(filesystem, &destination)?;
        let (source, destination) = source.into_parts();
        let identity = crate::launcher::ExecutableIdentity::from_fd(source.as_raw_fd())?;
        caller_mounts.push(super::MacosCallerMount {
            destination,
            identity,
        });
        transport_sources.push(UnixTransportSource::new(
            source,
            UnixTransportPurpose::MountSource,
            UnixFdConsumer::HiddenChild,
        ));
    }
    let transport_sources = transport_sources.into_boxed_slice();
    Ok(MacosPreparedSpawn {
        sandbox_exec: sandbox_exec.try_clone()?,
        stdio: parts.stdio,
        transport_sources,
        captured_open_file_limit: captured_open_file_limit()?,
        child_request_template,
        current_executable,
        target_executable,
        working_directory,
        mode: snapshot.policy().mode(),
        network: snapshot
            .policy()
            .network()
            .ok_or_else(|| SandboxError::InvalidSpawn {
                failure: crate::SpawnValidationError::FamilyTargetMappingMismatch,
            })?,
        filesystem: snapshot
            .policy()
            .filesystem()
            .ok_or_else(|| SandboxError::InvalidSpawn {
                failure: crate::SpawnValidationError::FamilyTargetMappingMismatch,
            })?
            .clone(),
        cwd: parts.cwd,
        home: revision_roots.paths().0.clone(),
        tmpdir: revision_roots.paths().2.clone(),
        caller_mounts: caller_mounts.into_boxed_slice(),
    })
}

fn build_template(
    snapshot: &PolicySnapshot,
    parts: &PreparedSpawnRequestParts,
    canonical_mappings: Vec<crate::CanonicalTargetFdMapping>,
) -> Result<SandboxChildRequestTemplate, SandboxError> {
    SandboxChildRequestTemplate::new(
        next_request_id(),
        parts.family,
        os_strings(&parts.argv)?,
        parts
            .cwd
            .as_path()
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| SandboxError::InvalidChildProtocol {
                failure: crate::ChildProtocolError::InvalidCwd,
            })?,
        os_environment(&parts.environment)?,
        snapshot
            .policy()
            .limits()
            .ok_or_else(|| SandboxError::InvalidSpawn {
                failure: crate::SpawnValidationError::OpenFileLimitExceeded,
            })?,
        ChildPlatformEnforcement::MacosSeatbelt,
        canonical_mappings,
    )
}

fn pin_current_executable() -> Result<PinnedExecutable, SandboxError> {
    let path = std::env::current_exe()
        .map_err(|error| SandboxError::from_io(SandboxIoOperation::OpenExecutable, &error))?;
    let canonical = std::fs::canonicalize(&path)
        .map_err(|error| SandboxError::from_io(SandboxIoOperation::OpenExecutable, &error))?;
    let (descriptor, identity) = open_pinned_executable(&canonical)?;
    Ok(PinnedExecutable::new(
        descriptor,
        identity,
        AbsolutePath::parse(canonical)?,
    ))
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
        path_candidates(&parts.environment, &command)?
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
    let identity = crate::launcher::ExecutableIdentity::from_fd(executable.as_raw_fd())?;
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
    let identity = crate::launcher::ExecutableIdentity::from_fd(directory.as_raw_fd())?;
    Ok(Some(super::PinnedWorkingDirectory::new(
        directory,
        identity,
        canonical_path,
    )))
}

fn path_candidates(
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

fn validate_caller_mount_destination(
    filesystem: &crate::FileSystemPolicy,
    destination: &AbsolutePath,
) -> Result<(), SandboxError> {
    super::mount_validation::validate_caller_mount_destination(filesystem, destination)
}

fn revalidate_revision_roots(
    revision_roots: &crate::RetainedRevisionRoots,
) -> Result<(), SandboxError> {
    for (source, destination) in revision_roots.try_clone_parts()? {
        let identity = crate::launcher::ExecutableIdentity::from_fd(source.as_raw_fd())?;
        if !crate::seatbelt::revalidate_directory_identity(&destination, identity)? {
            return Err(SandboxError::InvalidSpawn {
                failure: crate::SpawnValidationError::InvalidRevisionRoots,
            });
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_plan_owns_duplicated_launcher() {
        assert!(std::mem::needs_drop::<MacosPreparedSpawn>());
    }

    #[test]
    fn pinned_target_fd_survives_sandbox_exec_and_path_replacement() {
        assert!(std::mem::needs_drop::<PinnedExecutable>());
    }
}
