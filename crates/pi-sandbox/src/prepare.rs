use std::num::NonZeroU64;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::backend::BackendKind;
use crate::child::{ChildWorkingDirectoryFd, SandboxChildRequestTemplate};
use crate::launcher::{ExecutableIdentity, TrustedExecutable, duplicate_fd};
use crate::spawn::{
    CanonicalTargetFdMapping, PreparedSpawnRequest, PreparedSpawnRequestParts, SpawnStdioParts,
};
use crate::{AbsolutePath, SandboxBackend, SandboxError};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
#[cfg(unix)]
mod mount_validation {
    use crate::{AbsolutePath, FileSystemPolicy, SandboxError, SpawnValidationError};

    pub(crate) fn validate_caller_mount_destination(
        filesystem: &FileSystemPolicy,
        destination: &AbsolutePath,
    ) -> Result<(), SandboxError> {
        if filesystem
            .sealed_scratch_root()
            .is_some_and(|scratch| paths_overlap(destination, scratch))
        {
            return Err(invalid(
                SpawnValidationError::MountDestinationOutsideWritableAuthority,
            ));
        }
        if filesystem
            .denied_paths()
            .iter()
            .any(|denied| paths_overlap(destination, denied))
        {
            return Err(invalid(SpawnValidationError::MountDestinationDenied));
        }
        if filesystem
            .private_roots()
            .iter()
            .any(|private| paths_overlap(destination, private))
        {
            return Err(invalid(SpawnValidationError::MountDestinationDenied));
        }
        let Some(writable) = filesystem
            .writable_roots()
            .iter()
            .find(|writable| writable.root() == destination)
        else {
            return Err(invalid(
                SpawnValidationError::MountDestinationOutsideWritableAuthority,
            ));
        };
        if filesystem.writable_roots().iter().any(|root| {
            root.protected_descendants().iter().any(|protected| {
                let protected = root.root().as_path().join(protected.as_path());
                !filesystem
                    .private_roots()
                    .iter()
                    .any(|private| protected.starts_with(private.as_path()))
                    && (destination.as_path().starts_with(&protected)
                        || protected.starts_with(destination.as_path()))
            })
        }) {
            return Err(invalid(SpawnValidationError::MountDestinationProtected));
        }
        Ok(())
    }

    fn paths_overlap(left: &AbsolutePath, right: &AbsolutePath) -> bool {
        left.as_path().starts_with(right.as_path()) || right.as_path().starts_with(left.as_path())
    }

    fn invalid(failure: SpawnValidationError) -> SandboxError {
        SandboxError::InvalidSpawn { failure }
    }
}

pub struct PreparedSpawn {
    inner: PreparedUnixPlan,
}

pub(crate) enum PreparedUnixPlan {
    #[cfg(target_os = "linux")]
    Linux(LinuxPreparedSpawn),
    #[cfg(target_os = "macos")]
    Macos(MacosPreparedSpawn),
    RawHost(RawHostPreparedSpawn),
}

impl PreparedSpawn {
    pub(crate) fn into_unix_plan(self) -> PreparedUnixPlan {
        self.inner
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UnixFdConsumer {
    BwrapMount,
    HiddenChild,
    TargetExec,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UnixTransportPurpose {
    ChildRequest,
    ChildStatus,
    CurrentExecutable,
    TargetExecutable,
    Stdio,
    TargetMapping(CanonicalTargetFdMapping),
    MountSource,
    WorkingDirectory,
}

pub(crate) struct UnixTransportSource {
    source: OwnedFd,
    purpose: UnixTransportPurpose,
    last_consumer: UnixFdConsumer,
}

impl UnixTransportSource {
    pub(crate) fn new(
        source: OwnedFd,
        purpose: UnixTransportPurpose,
        last_consumer: UnixFdConsumer,
    ) -> Self {
        Self {
            source,
            purpose,
            last_consumer,
        }
    }

    pub(crate) fn into_parts(self) -> (OwnedFd, UnixTransportPurpose, UnixFdConsumer) {
        (self.source, self.purpose, self.last_consumer)
    }
}

pub(crate) struct UnixTransportFd {
    fd: OwnedFd,
    purpose: UnixTransportPurpose,
    last_consumer: UnixFdConsumer,
}

impl UnixTransportFd {
    pub(crate) fn new(
        fd: OwnedFd,
        purpose: UnixTransportPurpose,
        last_consumer: UnixFdConsumer,
    ) -> Self {
        Self {
            fd,
            purpose,
            last_consumer,
        }
    }

    pub(crate) fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    pub(crate) const fn purpose(&self) -> UnixTransportPurpose {
        self.purpose
    }

    pub(crate) const fn last_consumer(&self) -> UnixFdConsumer {
        self.last_consumer
    }

    pub(crate) fn into_fd(self) -> OwnedFd {
        self.fd
    }
}

pub(crate) struct PinnedExecutable {
    executable: OwnedFd,
    identity: ExecutableIdentity,
    canonical_path: AbsolutePath,
}

impl PinnedExecutable {
    pub(crate) fn new(
        executable: OwnedFd,
        identity: ExecutableIdentity,
        canonical_path: AbsolutePath,
    ) -> Self {
        Self {
            executable,
            identity,
            canonical_path,
        }
    }

    pub(crate) fn raw_fd(&self) -> RawFd {
        self.executable.as_raw_fd()
    }

    pub(crate) fn try_clone_transport(&self) -> Result<OwnedFd, SandboxError> {
        duplicate_fd(self.executable.as_raw_fd())
    }

    pub(crate) const fn identity(&self) -> ExecutableIdentity {
        self.identity
    }

    pub(crate) fn canonical_path(&self) -> &AbsolutePath {
        &self.canonical_path
    }
}

pub(crate) struct PinnedWorkingDirectory {
    directory: OwnedFd,
    identity: ExecutableIdentity,
    canonical_path: AbsolutePath,
}

impl PinnedWorkingDirectory {
    pub(crate) fn new(
        directory: OwnedFd,
        identity: ExecutableIdentity,
        canonical_path: AbsolutePath,
    ) -> Self {
        Self {
            directory,
            identity,
            canonical_path,
        }
    }
    pub(crate) fn into_parts(self) -> (OwnedFd, ExecutableIdentity, AbsolutePath) {
        (self.directory, self.identity, self.canonical_path)
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LinuxMountAccess {
    ReadOnly,
    ReadWrite,
    DenyFileMask,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LinuxMountEntryKind {
    RegularFile,
    Directory,
    CharacterDevice,
}

#[cfg(target_os = "linux")]
pub(crate) struct PinnedMount {
    pub(crate) source: OwnedFd,
    pub(crate) identity: ExecutableIdentity,
    pub(crate) destination: AbsolutePath,
    pub(crate) access: LinuxMountAccess,
    pub(crate) entry_kind: LinuxMountEntryKind,
}

#[cfg(target_os = "linux")]
pub(crate) struct LinuxRevisionRoots {
    pub(crate) home: PinnedMount,
    pub(crate) scratch: PinnedMount,
}

#[cfg(target_os = "linux")]
pub(crate) struct LinuxPreparedSpawn {
    pub(crate) bwrap: TrustedExecutable,
    pub(crate) stdio: SpawnStdioParts,
    pub(crate) transport_sources: Box<[UnixTransportSource]>,
    pub(crate) mount_destinations: Box<[(AbsolutePath, LinuxMountAccess, LinuxMountEntryKind)]>,
    pub(crate) captured_open_file_limit: NonZeroU64,
    pub(crate) child_request_template: SandboxChildRequestTemplate,
    pub(crate) current_executable: PinnedExecutable,
    pub(crate) target_executable: PinnedExecutable,
    pub(crate) working_directory: Option<PinnedWorkingDirectory>,
    pub(crate) network: crate::NetworkPolicy,
    pub(crate) cwd: AbsolutePath,
    pub(crate) denied_directories: Box<[AbsolutePath]>,
    pub(crate) device_mount_count: usize,
    pub(crate) runtime_mount_count: usize,
    pub(crate) read_only_mount_count: usize,
    pub(crate) writable_mount_count: usize,
    pub(crate) protected_mount_count: usize,
    pub(crate) denied_file_mount_count: usize,
    pub(crate) revision_roots: LinuxRevisionRoots,
    pub(crate) synthetics: crate::synthetic::SyntheticDirs,
}
#[cfg(target_os = "linux")]
impl LinuxPreparedSpawn {
    pub(crate) fn stage_wrapper(self) -> Result<crate::unix::PreparedWrapper, SandboxError> {
        use crate::child::{ChildExecutableFd, ChildTargetFdMapping};
        use crate::launcher::duplicate_fd_at_least;
        use std::ffi::CString;

        let minimum = self
            .child_request_template
            .canonical_mappings()
            .iter()
            .map(|mapping| mapping.target() as RawFd + 1)
            .fold(crate::unix::MINIMUM_TRANSPORT_FD, RawFd::max);
        let mut transports = Vec::new();
        let target_transport = duplicate_fd_at_least(self.target_executable.raw_fd(), minimum)?;
        let target_fd = target_transport.as_raw_fd();
        transports.push(UnixTransportFd::new(
            target_transport,
            UnixTransportPurpose::TargetExecutable,
            UnixFdConsumer::TargetExec,
        ));
        let working_directory = if let Some(directory) = self.working_directory {
            let (directory, identity, _) = directory.into_parts();
            let transport = duplicate_fd_at_least(directory.as_raw_fd(), minimum)?;
            let row = ChildWorkingDirectoryFd::new(transport.as_raw_fd(), identity);
            transports.push(UnixTransportFd::new(
                transport,
                UnixTransportPurpose::WorkingDirectory,
                UnixFdConsumer::HiddenChild,
            ));
            Some(row)
        } else {
            None
        };
        let mut mapping_rows = Vec::new();
        for source in self.transport_sources {
            let (fd, purpose, last_consumer) = source.into_parts();
            let transport = duplicate_fd_at_least(fd.as_raw_fd(), minimum)?;
            if let UnixTransportPurpose::TargetMapping(mapping) = purpose {
                mapping_rows.push(ChildTargetFdMapping {
                    transport_fd: transport.as_raw_fd() as u32,
                    target_fd: mapping.target(),
                    direction: mapping.direction(),
                });
            }
            transports.push(UnixTransportFd::new(transport, purpose, last_consumer));
        }
        mapping_rows.sort_by_key(|mapping| mapping.target_fd);
        let request = self.child_request_template.finalize(
            ChildExecutableFd::new(
                crate::child::SANDBOX_SELF_EXE_FD,
                self.current_executable.identity(),
            ),
            ChildExecutableFd::new(target_fd, self.target_executable.identity()),
            mapping_rows,
            working_directory,
        )?;
        let mut mount_transports = transports
            .iter()
            .filter(|transport| transport.purpose() == UnixTransportPurpose::MountSource)
            .map(UnixTransportFd::raw_fd);
        let mut rows = self
            .mount_destinations
            .into_vec()
            .into_iter()
            .map(|(destination, access, entry_kind)| {
                let source_fd =
                    mount_transports
                        .next()
                        .ok_or(SandboxError::InvalidChildProtocol {
                            failure: crate::ChildProtocolError::InvalidDescriptor,
                        })?;
                Ok(crate::bwrap::BwrapMountRow {
                    source_fd,
                    destination,
                    access,
                    entry_kind,
                })
            })
            .collect::<Result<Vec<_>, SandboxError>>()?;
        let take_rows = |rows: &mut Vec<crate::bwrap::BwrapMountRow>, count: usize| {
            rows.drain(..rows.len().min(count)).collect::<Vec<_>>()
        };
        let device_rows = take_rows(&mut rows, self.device_mount_count);
        let runtime_rows = take_rows(&mut rows, self.runtime_mount_count);
        let read_only_rows = take_rows(&mut rows, self.read_only_mount_count);
        let writable_rows = take_rows(&mut rows, self.writable_mount_count);
        let protected_rows = take_rows(&mut rows, self.protected_mount_count);
        let denied_file_rows = take_rows(&mut rows, self.denied_file_mount_count);
        if !rows.is_empty() {
            return Err(SandboxError::InvalidSpawn {
                failure: crate::SpawnValidationError::InvalidMountSource,
            });
        }
        let current_mount = duplicate_fd_at_least(self.current_executable.raw_fd(), minimum)?;
        let current_row = crate::bwrap::BwrapMountRow {
            source_fd: current_mount.as_raw_fd(),
            destination: AbsolutePath::parse("/run/pi/rpi".into())?,
            access: LinuxMountAccess::ReadOnly,
            entry_kind: LinuxMountEntryKind::RegularFile,
        };
        transports.push(UnixTransportFd::new(
            current_mount,
            UnixTransportPurpose::MountSource,
            UnixFdConsumer::BwrapMount,
        ));
        let target_mount = duplicate_fd_at_least(self.target_executable.raw_fd(), minimum)?;
        let target_row = crate::bwrap::BwrapMountRow {
            source_fd: target_mount.as_raw_fd(),
            destination: AbsolutePath::parse("/run/pi/target".into())?,
            access: LinuxMountAccess::ReadOnly,
            entry_kind: LinuxMountEntryKind::RegularFile,
        };
        transports.push(UnixTransportFd::new(
            target_mount,
            UnixTransportPurpose::MountSource,
            UnixFdConsumer::BwrapMount,
        ));
        let projection = |mount: &PinnedMount,
                          destination: &str,
                          access: LinuxMountAccess,
                          transports: &mut Vec<UnixTransportFd>|
         -> Result<crate::bwrap::BwrapMountRow, SandboxError> {
            let source = duplicate_fd_at_least(mount.source.as_raw_fd(), minimum)?;
            let source_fd = source.as_raw_fd();
            transports.push(UnixTransportFd::new(
                source,
                UnixTransportPurpose::MountSource,
                UnixFdConsumer::BwrapMount,
            ));
            Ok(crate::bwrap::BwrapMountRow {
                source_fd,
                destination: AbsolutePath::parse(destination.into())?,
                access,
                entry_kind: LinuxMountEntryKind::Directory,
            })
        };
        // Revision HOME stays read-only; the writable sealed scratch is projected at
        // /tmp (TMPDIR) and also exposed at /run/pi/scratch. The read-only revision
        // tmp is identity-bound and revalidated but not projected.
        let scratch_row = projection(
            &self.revision_roots.scratch,
            "/run/pi/scratch",
            LinuxMountAccess::ReadWrite,
            &mut transports,
        )?;
        let home_row = projection(
            &self.revision_roots.home,
            "/run/pi/home",
            LinuxMountAccess::ReadOnly,
            &mut transports,
        )?;
        let tmp_row = projection(
            &self.revision_roots.scratch,
            "/tmp",
            LinuxMountAccess::ReadWrite,
            &mut transports,
        )?;
        let argv = crate::bwrap::build_bwrap_argv(crate::bwrap::BwrapArgvInput {
            network: self.network,
            device_rows,
            destination_parents: Vec::new(),
            runtime_rows,
            read_only_rows,
            mandatory_read_only_rows: Vec::new(),
            writable_rows,
            scratch_row,
            home_row,
            tmp_row,
            protected_rows,
            denied_file_rows,
            denied_directories: self.denied_directories.into_vec(),
            current_executable_row: current_row,
            target_executable_row: target_row,
            cwd: self.cwd,
        })?;
        let environment = vec![CString::new("PATH=/usr/bin:/bin").map_err(|_| {
            SandboxError::InvalidChildProtocol {
                failure: crate::ChildProtocolError::EmbeddedNul,
            }
        })?];
        let mut wrapper = crate::unix::PreparedWrapper::new(
            self.bwrap.try_clone_fd()?,
            self.bwrap.identity(),
            argv,
            environment,
            self.stdio,
            request.encode_bounded()?,
            self.current_executable.try_clone_transport()?,
            self.current_executable.identity(),
            self.target_executable.identity(),
            transports.into_boxed_slice(),
            self.captured_open_file_limit,
        )?;
        wrapper.synthetics = self.synthetics;
        Ok(wrapper)
    }
}

#[cfg(target_os = "macos")]
pub(crate) struct MacosCallerMount {
    pub(crate) destination: AbsolutePath,
    pub(crate) identity: ExecutableIdentity,
}

#[cfg(target_os = "macos")]
pub(crate) struct MacosPreparedSpawn {
    pub(crate) sandbox_exec: TrustedExecutable,
    pub(crate) stdio: SpawnStdioParts,
    pub(crate) transport_sources: Box<[UnixTransportSource]>,
    pub(crate) captured_open_file_limit: NonZeroU64,
    pub(crate) child_request_template: SandboxChildRequestTemplate,
    pub(crate) current_executable: PinnedExecutable,
    pub(crate) target_executable: PinnedExecutable,
    pub(crate) working_directory: Option<PinnedWorkingDirectory>,
    pub(crate) mode: crate::SandboxMode,
    pub(crate) network: crate::NetworkPolicy,
    pub(crate) filesystem: crate::FileSystemPolicy,
    pub(crate) cwd: AbsolutePath,
    pub(crate) home: AbsolutePath,
    pub(crate) tmpdir: AbsolutePath,
    pub(crate) caller_mounts: Box<[MacosCallerMount]>,
}
#[cfg(target_os = "macos")]
impl MacosPreparedSpawn {
    pub(crate) fn stage_wrapper(self) -> Result<crate::unix::PreparedWrapper, SandboxError> {
        use crate::child::{ChildExecutableFd, ChildTargetFdMapping};
        use crate::launcher::duplicate_fd_at_least;
        use std::ffi::CString;

        let minimum = self
            .child_request_template
            .canonical_mappings()
            .iter()
            .map(|mapping| mapping.target() as RawFd + 1)
            .fold(crate::unix::MINIMUM_TRANSPORT_FD, RawFd::max);
        let target_transport = duplicate_fd_at_least(self.target_executable.raw_fd(), minimum)?;
        let target_fd = target_transport.as_raw_fd();
        let mut transports = vec![UnixTransportFd::new(
            target_transport,
            UnixTransportPurpose::TargetExecutable,
            UnixFdConsumer::TargetExec,
        )];
        let working_directory = if let Some(directory) = self.working_directory {
            let (directory, identity, _) = directory.into_parts();
            let transport = duplicate_fd_at_least(directory.as_raw_fd(), minimum)?;
            let row = ChildWorkingDirectoryFd::new(transport.as_raw_fd(), identity);
            transports.push(UnixTransportFd::new(
                transport,
                UnixTransportPurpose::WorkingDirectory,
                UnixFdConsumer::HiddenChild,
            ));
            Some(row)
        } else {
            None
        };
        let mut mapping_rows = Vec::new();
        for source in self.transport_sources {
            let (fd, purpose, last_consumer) = source.into_parts();
            let transport = duplicate_fd_at_least(fd.as_raw_fd(), minimum)?;
            if let UnixTransportPurpose::TargetMapping(mapping) = purpose {
                mapping_rows.push(ChildTargetFdMapping {
                    transport_fd: transport.as_raw_fd() as u32,
                    target_fd: mapping.target(),
                    direction: mapping.direction(),
                });
            }
            transports.push(UnixTransportFd::new(transport, purpose, last_consumer));
        }
        mapping_rows.sort_by_key(|mapping| mapping.target_fd);
        let request = self.child_request_template.finalize(
            ChildExecutableFd::new(
                crate::child::SANDBOX_SELF_EXE_FD,
                self.current_executable.identity(),
            ),
            ChildExecutableFd::new(target_fd, self.target_executable.identity()),
            mapping_rows,
            working_directory,
        )?;
        let mut caller_mounts = self.caller_mounts.into_vec();
        let caller_transport_fds = transports
            .iter()
            .filter(|transport| transport.purpose() == UnixTransportPurpose::MountSource)
            .map(UnixTransportFd::raw_fd)
            .collect::<Vec<_>>();
        if caller_mounts.len() != caller_transport_fds.len() {
            return Err(SandboxError::InvalidSpawn {
                failure: crate::SpawnValidationError::InvalidMountSource,
            });
        }
        for (mount, descriptor) in caller_mounts.iter().zip(&caller_transport_fds) {
            if !mount.identity.matches_fd(*descriptor)?
                || !crate::seatbelt::revalidate_directory_identity(
                    &mount.destination,
                    mount.identity,
                )?
            {
                return Err(SandboxError::InvalidSpawn {
                    failure: crate::SpawnValidationError::InvalidMountSource,
                });
            }
        }
        let launcher = self.sandbox_exec.try_clone_fd()?;
        let profile =
            crate::seatbelt::build_seatbelt_profile(&crate::seatbelt::MacosSeatbeltInput {
                mode: self.mode,
                network: self.network,
                filesystem: &self.filesystem,
                current_executable: self.current_executable.canonical_path(),
                current_transport_fd: crate::child::SANDBOX_SELF_EXE_FD,
                target_executable: self.target_executable.canonical_path(),
                target_transport_fd: target_fd,
                cwd: &self.cwd,
                home: &self.home,
                tmpdir: &self.tmpdir,
                stdio: match &self.stdio {
                    SpawnStdioParts::Null => crate::StdioMode::Null,
                    SpawnStdioParts::Piped(_, _, _) => crate::StdioMode::Piped,
                    SpawnStdioParts::Merged(_, _, _) => crate::StdioMode::Merged,
                    SpawnStdioParts::Pty(_) => crate::StdioMode::Pty,
                    SpawnStdioParts::Inherit => crate::StdioMode::Inherit,
                },
                caller_writable_roots: caller_mounts
                    .drain(..)
                    .map(|mount| mount.destination)
                    .collect(),
            })?;
        let launcher_path = format!("/dev/fd/{}", launcher.as_raw_fd());
        let argv = vec![
            CString::new(launcher_path).map_err(|_| SandboxError::InvalidChildProtocol {
                failure: crate::ChildProtocolError::EmbeddedNul,
            })?,
            CString::new("-p").map_err(|_| SandboxError::InvalidChildProtocol {
                failure: crate::ChildProtocolError::EmbeddedNul,
            })?,
            CString::new(profile).map_err(|_| SandboxError::InvalidChildProtocol {
                failure: crate::ChildProtocolError::EmbeddedNul,
            })?,
            CString::new("/dev/fd/5").map_err(|_| SandboxError::InvalidChildProtocol {
                failure: crate::ChildProtocolError::EmbeddedNul,
            })?,
            CString::new("__sandbox-child").map_err(|_| SandboxError::InvalidChildProtocol {
                failure: crate::ChildProtocolError::EmbeddedNul,
            })?,
            CString::new("3").map_err(|_| SandboxError::InvalidChildProtocol {
                failure: crate::ChildProtocolError::EmbeddedNul,
            })?,
            CString::new("4").map_err(|_| SandboxError::InvalidChildProtocol {
                failure: crate::ChildProtocolError::EmbeddedNul,
            })?,
        ];
        crate::unix::PreparedWrapper::new(
            launcher,
            self.sandbox_exec.identity(),
            argv,
            Vec::new(),
            self.stdio,
            request.encode_bounded()?,
            self.current_executable.try_clone_transport()?,
            self.current_executable.identity(),
            self.target_executable.identity(),
            transports.into_boxed_slice(),
            self.captured_open_file_limit,
        )
    }
}

pub(crate) struct RawHostPreparedSpawn {
    pub(crate) request: PreparedSpawnRequestParts,
}

pub fn prepare_spawn(
    backend: &SandboxBackend,
    request: PreparedSpawnRequest,
) -> Result<PreparedSpawn, SandboxError> {
    let inner = match backend.kind() {
        #[cfg(target_os = "linux")]
        BackendKind::LinuxBubblewrap { bwrap } => {
            let facts = backend
                .facts()
                .linux()
                .ok_or(SandboxError::UnsupportedMode {
                    platform: backend.facts().platform(),
                    mode: backend.snapshot().policy().mode(),
                })?;
            let revision_roots = backend.revision_roots().ok_or(SandboxError::InvalidSpawn {
                failure: crate::SpawnValidationError::InvalidRevisionRoots,
            })?;
            PreparedUnixPlan::Linux(linux::prepare_linux_spawn(
                facts,
                backend.snapshot(),
                revision_roots,
                bwrap,
                request,
            )?)
        }
        #[cfg(target_os = "macos")]
        BackendKind::MacosSeatbelt { sandbox_exec } => {
            let facts = backend
                .facts()
                .macos()
                .ok_or(SandboxError::UnsupportedMode {
                    platform: backend.facts().platform(),
                    mode: backend.snapshot().policy().mode(),
                })?;
            let revision_roots = backend.revision_roots().ok_or(SandboxError::InvalidSpawn {
                failure: crate::SpawnValidationError::InvalidRevisionRoots,
            })?;
            PreparedUnixPlan::Macos(macos::prepare_macos_spawn(
                facts,
                backend.snapshot(),
                revision_roots,
                sandbox_exec,
                request,
            )?)
        }
        BackendKind::RawHost { .. } => {
            let request = request.into_parts();
            if request.retained_executable.is_some() {
                return Err(SandboxError::InvalidSpawn {
                    failure: crate::SpawnValidationError::InvalidRetainedExecutable,
                });
            }
            if request.retained_working_directory.is_some() {
                return Err(SandboxError::InvalidSpawn {
                    failure: crate::SpawnValidationError::InvalidRetainedWorkingDirectory,
                });
            }
            PreparedUnixPlan::RawHost(RawHostPreparedSpawn { request })
        }
    };
    Ok(PreparedSpawn { inner })
}

pub(crate) fn next_request_id() -> String {
    format!("spawn-{}", NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed))
}

pub(crate) fn captured_open_file_limit() -> Result<NonZeroU64, SandboxError> {
    let (soft, _) = nix::sys::resource::getrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE)
        .map_err(|errno| {
            SandboxError::from_io(
                crate::SandboxIoOperation::InspectDescriptor,
                &std::io::Error::from_raw_os_error(errno as i32),
            )
        })?;
    NonZeroU64::new(soft).ok_or_else(|| SandboxError::InvalidSpawn {
        failure: crate::SpawnValidationError::OpenFileLimitExceeded,
    })
}
