use std::num::{NonZeroU16, NonZeroU32};

#[cfg(unix)]
use std::collections::{BTreeMap, BTreeSet};
#[cfg(unix)]
use std::ffi::{OsStr, OsString};
#[cfg(unix)]
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt as _;

use serde::{Deserialize, Serialize};

use crate::{AbsolutePath, PolicyValidationError, SandboxError, SpawnFamily, SpawnValidationError};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalSize {
    rows: NonZeroU16,
    columns: NonZeroU16,
}

impl TerminalSize {
    pub fn new(rows: u16, columns: u16) -> Result<Self, PolicyValidationError> {
        let rows = NonZeroU16::new(rows).ok_or(PolicyValidationError::InvalidField {
            field: "terminal.rows",
            reason: "must be nonzero",
        })?;
        let columns = NonZeroU16::new(columns).ok_or(PolicyValidationError::InvalidField {
            field: "terminal.columns",
            reason: "must be nonzero",
        })?;
        Ok(Self { rows, columns })
    }

    pub const fn rows(self) -> u16 {
        self.rows.get()
    }

    pub const fn columns(self) -> u16 {
        self.columns.get()
    }
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self {
            rows: NonZeroU16::new(24).expect("nonzero terminal rows"),
            columns: NonZeroU16::new(80).expect("nonzero terminal columns"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum StdioMode {
    Null,
    Piped,
    Merged,
    Pty,
    Inherit,
}

impl StdioMode {
    pub const fn canonical_tag(self) -> u8 {
        match self {
            Self::Null => 0,
            Self::Piped => 1,
            Self::Merged => 2,
            Self::Pty => 3,
            Self::Inherit => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetFdDirection {
    ParentReads,
    ParentWrites,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct CanonicalTargetFdMapping {
    target: NonZeroU32,
    direction: TargetFdDirection,
}

impl CanonicalTargetFdMapping {
    pub fn new(target: u32, direction: TargetFdDirection) -> Result<Self, PolicyValidationError> {
        let target = NonZeroU32::new(target).ok_or(PolicyValidationError::InvalidField {
            field: "targetFd",
            reason: "must be nonzero",
        })?;
        Ok(Self { target, direction })
    }

    pub const fn target(self) -> u32 {
        self.target.get()
    }

    pub const fn direction(self) -> TargetFdDirection {
        self.direction
    }
}

#[cfg(unix)]
#[derive(Debug)]
pub struct PipeStdio {
    child_stdin: OwnedFd,
    child_stdout: OwnedFd,
    child_stderr: OwnedFd,
}

#[cfg(unix)]
impl PipeStdio {
    pub fn new(
        child_stdin: OwnedFd,
        child_stdout: OwnedFd,
        child_stderr: OwnedFd,
    ) -> Result<Self, SandboxError> {
        let descriptors = [
            child_stdin.as_raw_fd(),
            child_stdout.as_raw_fd(),
            child_stderr.as_raw_fd(),
        ];
        if BTreeSet::from(descriptors).len() != descriptors.len() {
            return Err(SandboxError::InvalidSpawn {
                failure: SpawnValidationError::DuplicateSourceDescriptor,
            });
        }
        Ok(Self {
            child_stdin,
            child_stdout,
            child_stderr,
        })
    }

    pub(crate) fn into_fds(self) -> (OwnedFd, OwnedFd, OwnedFd) {
        (self.child_stdin, self.child_stdout, self.child_stderr)
    }
}

#[cfg(unix)]
#[derive(Debug)]
pub enum SpawnStdio {
    Null,
    Piped(PipeStdio),
    Merged(PipeStdio),
    Pty(TerminalSize),
    Inherit,
}

#[cfg(unix)]
impl SpawnStdio {
    pub const fn mode(&self) -> StdioMode {
        match self {
            Self::Null => StdioMode::Null,
            Self::Piped(_) => StdioMode::Piped,
            Self::Merged(_) => StdioMode::Merged,
            Self::Pty(_) => StdioMode::Pty,
            Self::Inherit => StdioMode::Inherit,
        }
    }

    pub(crate) fn into_parts(self) -> SpawnStdioParts {
        match self {
            Self::Null => SpawnStdioParts::Null,
            Self::Piped(stdio) => {
                let (stdin, stdout, stderr) = stdio.into_fds();
                SpawnStdioParts::Piped(stdin, stdout, stderr)
            }
            Self::Merged(stdio) => {
                let (stdin, stdout, stderr) = stdio.into_fds();
                SpawnStdioParts::Merged(stdin, stdout, stderr)
            }
            Self::Pty(size) => SpawnStdioParts::Pty(size),
            Self::Inherit => SpawnStdioParts::Inherit,
        }
    }
}

#[cfg(unix)]
pub(crate) enum SpawnStdioParts {
    Null,
    Piped(OwnedFd, OwnedFd, OwnedFd),
    Merged(OwnedFd, OwnedFd, OwnedFd),
    Pty(TerminalSize),
    Inherit,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TargetFd(NonZeroU32);

#[cfg(unix)]
impl TargetFd {
    pub fn new(value: RawFd) -> Result<Self, PolicyValidationError> {
        let value = u32::try_from(value).ok().and_then(NonZeroU32::new).ok_or(
            PolicyValidationError::InvalidField {
                field: "targetFd",
                reason: "must be a positive descriptor",
            },
        )?;
        Ok(Self(value))
    }

    pub const fn get(self) -> RawFd {
        self.0.get() as RawFd
    }
}

#[cfg(unix)]
#[derive(Debug)]
pub struct TargetFdMapping {
    source: OwnedFd,
    target: TargetFd,
    direction: TargetFdDirection,
}

#[cfg(unix)]
impl TargetFdMapping {
    pub fn new(
        source: OwnedFd,
        target: TargetFd,
        direction: TargetFdDirection,
    ) -> Result<Self, SandboxError> {
        Ok(Self {
            source,
            target,
            direction,
        })
    }

    pub const fn canonical(&self) -> CanonicalTargetFdMapping {
        CanonicalTargetFdMapping {
            target: self.target.0,
            direction: self.direction,
        }
    }

    pub(crate) fn into_parts(self) -> (OwnedFd, CanonicalTargetFdMapping) {
        let canonical = self.canonical();
        (self.source, canonical)
    }
}

#[cfg(unix)]
#[derive(Debug)]
pub struct MountSource {
    source: OwnedFd,
    destination: AbsolutePath,
}

#[cfg(unix)]
impl MountSource {
    #[doc(hidden)]
    pub fn read_write_directory(
        source: OwnedFd,
        destination: AbsolutePath,
    ) -> Result<Self, SandboxError> {
        let file = std::fs::File::from(source);
        let metadata = file.metadata().map_err(|error| {
            SandboxError::from_io(crate::SandboxIoOperation::InspectDescriptor, &error)
        })?;
        if !metadata.is_dir() {
            return Err(SandboxError::InvalidSpawn {
                failure: SpawnValidationError::InvalidMountSource,
            });
        }
        Ok(Self {
            source: file.into(),
            destination,
        })
    }

    pub(crate) fn source_fd(&self) -> RawFd {
        self.source.as_raw_fd()
    }

    pub(crate) fn destination(&self) -> &AbsolutePath {
        &self.destination
    }

    pub(crate) fn into_parts(self) -> (OwnedFd, AbsolutePath) {
        (self.source, self.destination)
    }
}

#[cfg(unix)]
#[derive(Debug)]
pub struct RetainedExecutable {
    executable: OwnedFd,
    canonical_path: AbsolutePath,
}

#[cfg(unix)]
impl RetainedExecutable {
    #[doc(hidden)]
    pub fn new(executable: OwnedFd, canonical_path: AbsolutePath) -> Result<Self, SandboxError> {
        let file = std::fs::File::from(executable);
        let metadata = file.metadata().map_err(|error| {
            SandboxError::from_io(crate::SandboxIoOperation::InspectDescriptor, &error)
        })?;
        use std::os::unix::fs::MetadataExt as _;
        if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
            return Err(SandboxError::InvalidSpawn {
                failure: SpawnValidationError::InvalidRetainedExecutable,
            });
        }
        Ok(Self {
            executable: file.into(),
            canonical_path,
        })
    }

    pub(crate) fn canonical_path(&self) -> &AbsolutePath {
        &self.canonical_path
    }
    pub(crate) fn into_parts(self) -> (OwnedFd, AbsolutePath) {
        (self.executable, self.canonical_path)
    }
}

#[cfg(unix)]
#[derive(Debug)]
pub struct RetainedWorkingDirectory {
    directory: OwnedFd,
    canonical_path: AbsolutePath,
}

#[cfg(unix)]
impl RetainedWorkingDirectory {
    #[doc(hidden)]
    pub fn new(directory: OwnedFd, canonical_path: AbsolutePath) -> Result<Self, SandboxError> {
        let file = std::fs::File::from(directory);
        let metadata = file.metadata().map_err(|error| {
            SandboxError::from_io(crate::SandboxIoOperation::InspectDescriptor, &error)
        })?;
        if !metadata.is_dir() {
            return Err(SandboxError::InvalidSpawn {
                failure: SpawnValidationError::InvalidRetainedWorkingDirectory,
            });
        }
        Ok(Self {
            directory: file.into(),
            canonical_path,
        })
    }

    pub(crate) fn canonical_path(&self) -> &AbsolutePath {
        &self.canonical_path
    }

    pub(crate) fn into_parts(self) -> (OwnedFd, AbsolutePath) {
        (self.directory, self.canonical_path)
    }
}

#[cfg(unix)]
#[derive(Debug)]
pub struct RetainedRevisionRoots {
    home: RetainedDirectory,
    tmp: RetainedDirectory,
    scratch: RetainedDirectory,
}

#[cfg(unix)]
#[derive(Debug)]
struct RetainedDirectory {
    directory: OwnedFd,
    canonical_path: AbsolutePath,
}

#[cfg(unix)]
impl RetainedRevisionRoots {
    #[doc(hidden)]
    pub fn new(
        home: OwnedFd,
        home_path: AbsolutePath,
        tmp: OwnedFd,
        tmp_path: AbsolutePath,
        scratch: OwnedFd,
        scratch_path: AbsolutePath,
    ) -> Result<Self, SandboxError> {
        let home = RetainedDirectory::new(home, home_path)?;
        let tmp = RetainedDirectory::new(tmp, tmp_path)?;
        let scratch = RetainedDirectory::new(scratch, scratch_path)?;
        if home.canonical_path == tmp.canonical_path
            || home.canonical_path == scratch.canonical_path
            || tmp.canonical_path == scratch.canonical_path
        {
            return Err(SandboxError::InvalidSpawn {
                failure: SpawnValidationError::InvalidRevisionRoots,
            });
        }
        Ok(Self { home, tmp, scratch })
    }

    pub(crate) fn paths(&self) -> (&AbsolutePath, &AbsolutePath, &AbsolutePath) {
        (
            &self.home.canonical_path,
            &self.tmp.canonical_path,
            &self.scratch.canonical_path,
        )
    }

    pub(crate) fn try_clone_parts(&self) -> Result<[(OwnedFd, AbsolutePath); 3], SandboxError> {
        Ok([
            self.home.try_clone_parts()?,
            self.tmp.try_clone_parts()?,
            self.scratch.try_clone_parts()?,
        ])
    }
    pub(crate) fn into_parts(self) -> [(OwnedFd, AbsolutePath); 3] {
        [
            self.home.into_parts(),
            self.tmp.into_parts(),
            self.scratch.into_parts(),
        ]
    }
}

#[cfg(unix)]
impl RetainedDirectory {
    fn new(directory: OwnedFd, canonical_path: AbsolutePath) -> Result<Self, SandboxError> {
        let file = std::fs::File::from(directory);
        if !file
            .metadata()
            .map_err(|error| {
                SandboxError::from_io(crate::SandboxIoOperation::InspectDescriptor, &error)
            })?
            .is_dir()
        {
            return Err(SandboxError::InvalidSpawn {
                failure: SpawnValidationError::InvalidRevisionRoots,
            });
        }
        Ok(Self {
            directory: file.into(),
            canonical_path,
        })
    }

    fn into_parts(self) -> (OwnedFd, AbsolutePath) {
        (self.directory, self.canonical_path)
    }

    fn try_clone_parts(&self) -> Result<(OwnedFd, AbsolutePath), SandboxError> {
        let directory = crate::launcher::duplicate_fd(self.directory.as_raw_fd())?;
        Ok((directory, self.canonical_path.clone()))
    }
}

#[cfg(unix)]
#[derive(Debug)]
pub struct PreparedSpawnRequest {
    family: SpawnFamily,
    argv: Box<[OsString]>,
    cwd: AbsolutePath,
    environment: BTreeMap<OsString, OsString>,
    stdio: SpawnStdio,
    retained_executable: Option<RetainedExecutable>,
    retained_working_directory: Option<RetainedWorkingDirectory>,
    target_fd_mappings: Box<[TargetFdMapping]>,
    mount_sources: Box<[MountSource]>,
}
#[cfg(unix)]
impl PreparedSpawnRequest {
    pub fn new(
        family: SpawnFamily,
        argv: Vec<OsString>,
        cwd: AbsolutePath,
        environment: BTreeMap<OsString, OsString>,
        stdio: SpawnStdio,
        target_fd_mappings: Vec<TargetFdMapping>,
        mount_sources: Vec<MountSource>,
        retained_executable: Option<RetainedExecutable>,
        retained_working_directory: Option<RetainedWorkingDirectory>,
    ) -> Result<Self, SandboxError> {
        validate_argv(&argv)?;
        validate_environment(&environment)?;
        validate_stdio(family, stdio.mode())?;
        validate_target_mappings(family, &target_fd_mappings)?;
        if let Some(retained) = retained_executable.as_ref() {
            let path_matches = AbsolutePath::parse(std::path::PathBuf::from(&argv[0]))
                .is_ok_and(|path| path == retained.canonical_path);
            if !path_matches {
                return Err(SandboxError::InvalidSpawn {
                    failure: SpawnValidationError::InvalidRetainedExecutable,
                });
            }
        }
        if retained_working_directory
            .as_ref()
            .is_some_and(|retained| retained.canonical_path != cwd)
        {
            return Err(SandboxError::InvalidSpawn {
                failure: SpawnValidationError::InvalidRetainedWorkingDirectory,
            });
        }
        validate_mount_sources(&target_fd_mappings, &mount_sources)?;
        Ok(Self {
            family,
            argv: argv.into_boxed_slice(),
            cwd,
            environment,
            stdio,
            retained_executable,
            retained_working_directory,
            target_fd_mappings: target_fd_mappings.into_boxed_slice(),
            mount_sources: mount_sources.into_boxed_slice(),
        })
    }

    pub const fn family(&self) -> SpawnFamily {
        self.family
    }

    pub fn canonical_target_fd_mappings(&self) -> Vec<CanonicalTargetFdMapping> {
        let mut mappings = self
            .target_fd_mappings
            .iter()
            .map(TargetFdMapping::canonical)
            .collect::<Vec<_>>();
        mappings.sort_unstable();
        mappings
    }

    pub(crate) fn into_parts(self) -> PreparedSpawnRequestParts {
        PreparedSpawnRequestParts {
            family: self.family,
            argv: self.argv,
            cwd: self.cwd,
            environment: self.environment,
            stdio: self.stdio.into_parts(),
            target_fd_mappings: self.target_fd_mappings,
            mount_sources: self.mount_sources,
            retained_executable: self.retained_executable,
            retained_working_directory: self.retained_working_directory,
        }
    }
}

#[cfg(unix)]
pub(crate) struct PreparedSpawnRequestParts {
    pub(crate) family: SpawnFamily,
    pub(crate) argv: Box<[OsString]>,
    pub(crate) cwd: AbsolutePath,
    pub(crate) environment: BTreeMap<OsString, OsString>,
    pub(crate) stdio: SpawnStdioParts,
    pub(crate) target_fd_mappings: Box<[TargetFdMapping]>,
    pub(crate) mount_sources: Box<[MountSource]>,
    pub(crate) retained_executable: Option<RetainedExecutable>,
    pub(crate) retained_working_directory: Option<RetainedWorkingDirectory>,
}

#[cfg(unix)]
fn validate_argv(argv: &[OsString]) -> Result<(), SandboxError> {
    if argv.first().is_none_or(|argument| argument.is_empty()) {
        return Err(SandboxError::InvalidSpawn {
            failure: SpawnValidationError::EmptyArgv,
        });
    }
    if argv.iter().any(|argument| contains_nul(argument)) {
        return Err(SandboxError::InvalidSpawn {
            failure: SpawnValidationError::EmbeddedNul,
        });
    }
    Ok(())
}

#[cfg(unix)]
fn validate_environment(environment: &BTreeMap<OsString, OsString>) -> Result<(), SandboxError> {
    if environment.iter().any(|(name, value)| {
        name.is_empty()
            || name.as_os_str().as_bytes().contains(&b'=')
            || contains_nul(name)
            || contains_nul(value)
    }) {
        return Err(SandboxError::InvalidSpawn {
            failure: SpawnValidationError::InvalidEnvironment,
        });
    }
    Ok(())
}

#[cfg(unix)]
fn contains_nul(value: &OsStr) -> bool {
    value.as_bytes().contains(&0)
}

#[cfg(unix)]
fn validate_stdio(family: SpawnFamily, stdio: StdioMode) -> Result<(), SandboxError> {
    let is_valid = match family {
        SpawnFamily::ForegroundShell | SpawnFamily::ManagedPtyProcess => stdio == StdioMode::Pty,
        _ => stdio != StdioMode::Pty,
    };
    if !is_valid {
        return Err(SandboxError::InvalidSpawn {
            failure: SpawnValidationError::FamilyStdioMismatch,
        });
    }
    Ok(())
}

#[cfg(unix)]
fn validate_target_mappings(
    family: SpawnFamily,
    mappings: &[TargetFdMapping],
) -> Result<(), SandboxError> {
    let mut sources = BTreeSet::new();
    let mut canonical = Vec::with_capacity(mappings.len());
    for mapping in mappings {
        if !sources.insert(mapping.source.as_raw_fd()) {
            return Err(SandboxError::InvalidSpawn {
                failure: SpawnValidationError::DuplicateSourceDescriptor,
            });
        }
        canonical.push(mapping.canonical());
    }
    canonical.sort_unstable();
    if canonical
        .windows(2)
        .any(|pair| pair[0].target() == pair[1].target())
    {
        return Err(SandboxError::InvalidSpawn {
            failure: SpawnValidationError::DuplicateTargetDescriptor,
        });
    }
    let is_valid = if family == SpawnFamily::Browser {
        canonical.len() == 2
            && canonical[0].target() == 3
            && canonical[0].direction() == TargetFdDirection::ParentWrites
            && canonical[1].target() == 4
            && canonical[1].direction() == TargetFdDirection::ParentReads
    } else {
        canonical.iter().all(|mapping| mapping.target() >= 7)
    };
    if !is_valid {
        return Err(SandboxError::InvalidSpawn {
            failure: SpawnValidationError::FamilyTargetMappingMismatch,
        });
    }
    Ok(())
}

#[cfg(unix)]
fn validate_mount_sources(
    target_fd_mappings: &[TargetFdMapping],
    mount_sources: &[MountSource],
) -> Result<(), SandboxError> {
    let mut descriptors = target_fd_mappings
        .iter()
        .map(|mapping| mapping.source.as_raw_fd())
        .collect::<BTreeSet<_>>();
    let mut destinations = BTreeSet::new();
    for source in mount_sources {
        if !descriptors.insert(source.source_fd()) {
            return Err(SandboxError::InvalidSpawn {
                failure: SpawnValidationError::DuplicateSourceDescriptor,
            });
        }
        if !destinations.insert(source.destination().clone()) {
            return Err(SandboxError::InvalidSpawn {
                failure: SpawnValidationError::DuplicateMountDestination,
            });
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs::File;
    use std::os::fd::OwnedFd;

    use super::*;

    fn null_fd() -> OwnedFd {
        File::open("/dev/null").expect("open null").into()
    }

    #[test]
    fn browser_target_fd_contract_is_exact() {
        let browser = vec![
            TargetFdMapping::new(
                null_fd(),
                TargetFd::new(3).expect("target"),
                TargetFdDirection::ParentWrites,
            )
            .expect("mapping"),
            TargetFdMapping::new(
                null_fd(),
                TargetFd::new(4).expect("target"),
                TargetFdDirection::ParentReads,
            )
            .expect("mapping"),
        ];
        assert!(validate_target_mappings(SpawnFamily::Browser, &browser).is_ok());
        let wrong = vec![
            TargetFdMapping::new(
                null_fd(),
                TargetFd::new(3).expect("target"),
                TargetFdDirection::ParentReads,
            )
            .expect("mapping"),
            TargetFdMapping::new(
                null_fd(),
                TargetFd::new(4).expect("target"),
                TargetFdDirection::ParentWrites,
            )
            .expect("mapping"),
        ];
        assert!(matches!(
            validate_target_mappings(SpawnFamily::Browser, &wrong),
            Err(SandboxError::InvalidSpawn {
                failure: SpawnValidationError::FamilyTargetMappingMismatch
            })
        ));
    }

    #[test]
    fn owned_spawn_parts_are_consumed_exactly_once() {
        let stdio = PipeStdio::new(null_fd(), null_fd(), null_fd()).expect("stdio");
        let request = PreparedSpawnRequest::new(
            SpawnFamily::ManagedPipeProcess,
            vec![OsString::from("/bin/true")],
            AbsolutePath::parse("/".into()).expect("cwd"),
            BTreeMap::new(),
            SpawnStdio::Piped(stdio),
            vec![
                TargetFdMapping::new(
                    null_fd(),
                    TargetFd::new(7).expect("target"),
                    TargetFdDirection::ParentReads,
                )
                .expect("mapping"),
            ],
            Vec::new(),
            None,
            None,
        )
        .expect("request");
        let parts = request.into_parts();
        assert_eq!(parts.target_fd_mappings.len(), 1);
        assert!(matches!(parts.stdio, SpawnStdioParts::Piped(_, _, _)));
    }
}
