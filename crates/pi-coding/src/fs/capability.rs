use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

#[cfg(target_os = "windows")]
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
#[cfg(target_os = "windows")]
use cap_std::ambient_authority;
use cap_std::fs::Dir;
#[cfg(target_os = "windows")]
use cap_std::fs::OpenOptions;
use pi_sandbox::AbsolutePath;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FsOperation {
    OpenRoot,
    OpenEntry,
    Read,
    Write,
    Sync,
    CreateDirectory,
    CreateFile,
    Replace,
    Remove,
    Rename,
    Link,
    Enumerate,
    DuplicateForSpawn,
}

#[derive(thiserror::Error)]
pub enum FsError {
    #[error("invalid filesystem path")]
    InvalidPath,
    #[error(
        "filesystem capability does not cover the requested path; add it with --add-dir or sandbox.readOnlyRoots"
    )]
    OutsideCapability,
    #[error("filesystem operation is denied by sandbox policy")]
    Denied,
    #[error("filesystem entry type is unsupported")]
    UnsupportedEntryType,
    #[error("filesystem entry identity changed")]
    IdentityChanged,
    #[error("filesystem access is read-only")]
    ReadOnly,
    #[error("bounded filesystem read exceeds {limit} bytes")]
    ReadLimitExceeded { limit: u64 },
    #[error("filesystem replacement conflicted with the expected identity")]
    ReplacementConflict,
    #[error(
        "filesystem operation {operation:?} failed for {purpose:?} at {path}: {kind:?}",
        path = redacted_path(path)
    )]
    System {
        operation: FsOperation,
        kind: io::ErrorKind,
        raw_os_error: Option<i32>,
        purpose: Option<CapabilityRootPurpose>,
        path: Option<PathBuf>,
    },
}

impl std::fmt::Debug for FsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, formatter)
    }
}

impl FsError {
    pub(crate) fn from_io(operation: FsOperation, error: &io::Error) -> Self {
        Self::System {
            operation,
            kind: error.kind(),
            raw_os_error: error.raw_os_error(),
            purpose: None,
            path: None,
        }
    }

    fn with_root_context(self, purpose: CapabilityRootPurpose, path: &AbsolutePath) -> Self {
        self.with_entry_context(purpose, path.as_path())
    }

    fn with_entry_context(self, purpose: CapabilityRootPurpose, path: &Path) -> Self {
        match self {
            Self::System {
                operation,
                kind,
                raw_os_error,
                purpose: existing_purpose,
                path: existing_path,
            } => Self::System {
                operation,
                kind,
                raw_os_error,
                purpose: existing_purpose.or(Some(purpose)),
                path: existing_path.or_else(|| Some(path.to_path_buf())),
            },
            other => other,
        }
    }
}

fn redacted_path(path: &Option<PathBuf>) -> String {
    path.as_deref()
        .map(|path| {
            let mut parts = path.iter().rev().take(3).collect::<Vec<_>>();
            parts.reverse();
            let joined = parts
                .into_iter()
                .map(|component| component.to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            if joined.is_empty() {
                "<root>".to_owned()
            } else {
                joined
            }
        })
        .unwrap_or_else(|| "<unspecified>".to_owned())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CapabilityAccess {
    ReadOnly,
    ReadWrite,
    PrivateReadWrite,
}

impl CapabilityAccess {
    const fn permits_write(self) -> bool {
        !matches!(self, Self::ReadOnly)
    }

    const fn can_narrow_to(self, requested: Self) -> bool {
        matches!(
            (self, requested),
            (Self::ReadOnly, Self::ReadOnly)
                | (Self::ReadWrite, Self::ReadOnly | Self::ReadWrite)
                | (
                    Self::PrivateReadWrite,
                    Self::ReadOnly | Self::ReadWrite | Self::PrivateReadWrite
                )
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RootCreation {
    Existing,
    CreateDirectory,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum CapabilityRootPurpose {
    AgentStore,
    ProjectResources,
    SessionStore,
    ToolExecutable,
    WorkflowStore,
    WorkflowWorktrees,
    OrchestrationArtifacts,
    PersonaStore,
    OperatorImport,
    OperatorExport,
    OperatorAuth,
}

pub(crate) enum CapabilityEntryAuthority {
    Directory(CapabilityRootLease),
    File(CapabilityFile),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum EntryKind {
    RegularFile,
    Directory,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum FileIdentity {
    Unix {
        device: u64,
        inode: u64,
        kind: EntryKind,
    },
    Windows {
        volume_serial: u64,
        file_id: [u8; 16],
        kind: EntryKind,
    },
}

impl FileIdentity {
    pub(crate) const fn kind(self) -> EntryKind {
        match self {
            Self::Unix { kind, .. } | Self::Windows { kind, .. } => kind,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ManagedLinkIdentity {
    pub(crate) device: u64,
    pub(crate) inode: u64,
}

/// Opaque, stable identity for a session file opened through retained authority.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SessionFileIdentity(FileIdentity);

impl SessionFileIdentity {
    pub(crate) fn from_file(file: &File) -> Result<Self, FsError> {
        platform_file_identity(file).map(Self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FsMetadata {
    pub(crate) identity: FileIdentity,
    pub(crate) len: u64,
    pub(crate) modified_ns: i128,
    pub(crate) readonly: bool,
    #[cfg(unix)]
    pub(crate) mode: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FileMode {
    Private,
    Shared,
}

#[derive(Clone, Debug)]
pub(crate) struct Replacement {
    pub(crate) relative: PathBuf,
    pub(crate) expected: Option<FileIdentity>,
    pub(crate) bytes: Vec<u8>,
    pub(crate) mode: FileMode,
}

pub(crate) const STREAM_COPY_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReplacementTarget {
    Absent,
    ExactIdentity(FileIdentity),
}

#[derive(Clone, Debug)]
pub(crate) struct StreamingReplacement {
    pub(crate) relative: PathBuf,
    pub(crate) expected: ReplacementTarget,
}

pub(crate) struct StreamCopyBuffer {
    bytes: Vec<u8>,
}

impl StreamCopyBuffer {
    pub(crate) fn new() -> Self {
        Self {
            bytes: vec![0; STREAM_COPY_BUFFER_BYTES],
        }
    }

    pub(crate) const fn len(&self) -> usize {
        STREAM_COPY_BUFFER_BYTES
    }
}

pub(crate) struct CapabilityFs;

pub(crate) struct CapabilityRootLease {
    fs: Arc<CapabilityFs>,
    purpose: CapabilityRootPurpose,
    access: CapabilityAccess,
    display_root: AbsolutePath,
    authority_root: AbsolutePath,
    root: Arc<OpenedDirectory>,
    root_path_guard: Option<Arc<ManagedRootPathGuard>>,
}

pub(crate) struct CapabilityDirectory {
    lease: CapabilityRootLease,
    relative: PathBuf,
    identity: FileIdentity,
    directory: Dir,
}

pub(crate) struct CapabilityFile {
    lease: CapabilityRootLease,
    relative: PathBuf,
    identity: FileIdentity,
    file: File,
}

pub(crate) struct ManagedCapabilityFile {
    file: CapabilityFile,
    resolved_path: PathBuf,
    followed_link: bool,
}

pub(crate) struct ManagedOpenedEntry {
    entry: CapabilityEntry,
    resolved_path: PathBuf,
    followed_link: bool,
}
#[derive(Clone, Debug, Eq, PartialEq)]
struct ManagedLeafLinkGuard {
    identity: ManagedLinkIdentity,
    target: PathBuf,
}

#[derive(Clone, Debug)]
enum ManagedRootPathGuard {
    Directory {
        parent: CapabilityRootLease,
        name: OsString,
        target_identity: FileIdentity,
    },
    Link {
        parent: CapabilityRootLease,
        name: OsString,
        link_identity: ManagedLinkIdentity,
        target: PathBuf,
        target_identity: FileIdentity,
    },
}

pub(crate) struct CapabilityWritableFile {
    lease: CapabilityRootLease,
    relative: PathBuf,
    identity: FileIdentity,
    file: Option<File>,
    finished: bool,
}

pub(crate) enum CapabilityEntry {
    File(CapabilityFile),
    Directory(CapabilityDirectory),
}

pub(crate) struct CapabilityDirEntry {
    name: OsString,
    entry: CapabilityEntry,
}

pub(crate) struct CapabilityReadDir {
    parent: CapabilityDirectory,
    cursor: PlatformDirectoryCursor,
}

pub(crate) struct CapabilitySpawnSource {
    fs: Arc<CapabilityFs>,
    access: CapabilityAccess,
    purpose: CapabilityRootPurpose,
    display_root: AbsolutePath,
    directory: File,
    identity: FileIdentity,
}
pub(crate) struct CapabilityExecutableSource {
    fs: Arc<CapabilityFs>,
    path: AbsolutePath,
    file: File,
    identity: FileIdentity,
}
pub(crate) struct CapabilityWorkingDirectorySource {
    fs: Arc<CapabilityFs>,
    path: AbsolutePath,
    directory: File,
    identity: FileIdentity,
    validation_root: Arc<OpenedDirectory>,
    relative: PathBuf,
}

pub(crate) struct CapabilityRevisionRoots {
    fs: Arc<CapabilityFs>,
    home: CapabilityRootLease,
    tmp: CapabilityRootLease,
    scratch: CapabilityRootLease,
}

struct OpenedDirectory {
    directory: Dir,
    identity: FileIdentity,
}

struct PlatformDirectoryCursor {
    entries: Vec<(OsString, FileIdentity)>,
    next: usize,
}

struct PreparedReplacement {
    replacement: Replacement,
    parent: CapabilityRootLease,
    name: OsString,
    temporary: OsString,
    backup: Option<OsString>,
}

struct PreparedStreamingReplacement {
    parent: CapabilityRootLease,
    name: OsString,
    temporary: OsString,
    temporary_identity: Option<FileIdentity>,
    backup: Option<(OsString, FileIdentity)>,
    installed: bool,
    committed: bool,
}

impl CapabilityRevisionRoots {
    pub(crate) fn acquire(
        capabilities: &Arc<CapabilityFs>,
        home: AbsolutePath,
        tmp: AbsolutePath,
        scratch: AbsolutePath,
    ) -> Result<Self, FsError> {
        if home == tmp || home == scratch || tmp == scratch {
            return Err(FsError::InvalidPath);
        }
        let home = capabilities.open_root(
            CapabilityRootPurpose::SessionStore,
            home,
            CapabilityAccess::PrivateReadWrite,
            RootCreation::CreateDirectory,
        )?;
        let tmp = capabilities.open_root(
            CapabilityRootPurpose::SessionStore,
            tmp,
            CapabilityAccess::PrivateReadWrite,
            RootCreation::CreateDirectory,
        )?;
        let scratch = capabilities.open_root(
            CapabilityRootPurpose::SessionStore,
            scratch,
            CapabilityAccess::PrivateReadWrite,
            RootCreation::CreateDirectory,
        )?;
        Ok(Self {
            fs: capabilities.clone(),
            home,
            tmp,
            scratch,
        })
    }

    pub(crate) fn scratch_lease(&self) -> CapabilityRootLease {
        self.scratch.clone()
    }

    #[cfg(unix)]
    pub(crate) fn into_retained(
        self,
        capabilities: &Arc<CapabilityFs>,
    ) -> Result<pi_sandbox::RetainedRevisionRoots, FsError> {
        if !Arc::ptr_eq(&self.fs, capabilities) {
            return Err(FsError::OutsideCapability);
        }
        let home = retained_root_part(self.home)?;
        let tmp = retained_root_part(self.tmp)?;
        let scratch = retained_root_part(self.scratch)?;
        pi_sandbox::RetainedRevisionRoots::new(home.0, home.1, tmp.0, tmp.1, scratch.0, scratch.1)
            .map_err(|_| FsError::IdentityChanged)
    }
}

#[cfg(unix)]
fn retained_root_part(
    lease: CapabilityRootLease,
) -> Result<(std::os::fd::OwnedFd, AbsolutePath), FsError> {
    let source = lease.duplicate_for_spawn()?;
    let (purpose, access, path, identity) = source.canonical_mount();
    if purpose != CapabilityRootPurpose::SessionStore
        || access != CapabilityAccess::PrivateReadWrite
        || identity != lease.identity()
    {
        return Err(FsError::IdentityChanged);
    }
    let file = source
        .try_clone_std()
        .map_err(|error| FsError::from_io(FsOperation::DuplicateForSpawn, &error))?;
    let directory = Dir::from_std_file(
        file.try_clone()
            .map_err(|error| FsError::from_io(FsOperation::DuplicateForSpawn, &error))?,
    );
    if platform_directory_identity(&directory)? != identity {
        return Err(FsError::IdentityChanged);
    }
    Ok((file.into(), path))
}

impl CapabilityFs {
    pub(crate) fn new() -> Result<Arc<Self>, FsError> {
        Ok(Arc::new(Self))
    }

    pub(crate) fn open_root(
        self: &Arc<Self>,
        purpose: CapabilityRootPurpose,
        path: AbsolutePath,
        access: CapabilityAccess,
        creation: RootCreation,
    ) -> Result<CapabilityRootLease, FsError> {
        let mode = if matches!(access, CapabilityAccess::PrivateReadWrite) {
            FileMode::Private
        } else {
            FileMode::Shared
        };
        let directory = platform_open_root(path.as_path(), creation, mode)
            .map_err(|error| error.with_root_context(purpose, &path))?;
        let identity = platform_directory_identity(&directory)
            .map_err(|error| error.with_root_context(purpose, &path))?;
        Ok(CapabilityRootLease {
            fs: self.clone(),
            purpose,
            access,
            display_root: path.clone(),
            authority_root: path,
            root: Arc::new(OpenedDirectory {
                directory,
                identity,
            }),
            root_path_guard: None,
        })
    }

    pub(crate) fn open_exact_file(
        self: &Arc<Self>,
        purpose: CapabilityRootPurpose,
        path: AbsolutePath,
    ) -> Result<CapabilityFile, FsError> {
        let (parent, name) = self.open_exact_parent(
            purpose,
            path,
            CapabilityAccess::ReadOnly,
            RootCreation::Existing,
        )?;
        parent.open_file(Path::new(&name))
    }

    pub(crate) fn open_entry_authority(
        self: &Arc<Self>,
        purpose: CapabilityRootPurpose,
        path: AbsolutePath,
    ) -> Result<CapabilityEntryAuthority, FsError> {
        if path.as_path().parent().is_none() {
            return self
                .open_root(
                    purpose,
                    path,
                    CapabilityAccess::ReadOnly,
                    RootCreation::Existing,
                )
                .map(CapabilityEntryAuthority::Directory);
        }
        let display_root = path.clone();
        let (parent, name) = self.open_exact_parent(
            purpose,
            path,
            CapabilityAccess::ReadOnly,
            RootCreation::Existing,
        )?;
        match parent.open_entry(Path::new(&name))? {
            CapabilityEntry::File(file) => Ok(CapabilityEntryAuthority::File(file)),
            CapabilityEntry::Directory(directory) => {
                directory_into_root_lease(directory, display_root)
                    .map(CapabilityEntryAuthority::Directory)
            }
        }
    }

    pub(crate) fn open_exact_parent(
        self: &Arc<Self>,
        purpose: CapabilityRootPurpose,
        path: AbsolutePath,
        access: CapabilityAccess,
        creation: RootCreation,
    ) -> Result<(CapabilityRootLease, OsString), FsError> {
        let name = path
            .as_path()
            .file_name()
            .ok_or(FsError::InvalidPath)?
            .to_os_string();
        validate_name(&name)?;
        let parent = path.as_path().parent().ok_or(FsError::InvalidPath)?;
        let parent = AbsolutePath::parse(parent.to_path_buf()).map_err(|_| FsError::InvalidPath)?;
        self.open_root(purpose, parent, access, creation)
            .map(|lease| (lease, name))
    }

    /// Resolves and opens one managed-config entry as a single capability
    /// operation. The returned path describes the exact target reached by the
    /// retained handle; callers that authorize the target must use this path
    /// rather than resolving the configured path a second time.
    fn open_managed_entry(
        self: &Arc<Self>,
        purpose: CapabilityRootPurpose,
        path: AbsolutePath,
    ) -> Result<ManagedOpenedEntry, FsError> {
        let mut hops = 0;
        open_managed_entry_with(self, purpose, path.as_path(), &mut hops, &mut || {})
    }

    /// Read-only resolution of a managed-config path. Resolution retains and
    /// validates the target handle before returning its path, so this helper
    /// cannot report a target different from the entry it inspected.
    pub(crate) fn resolve_managed_path_for_read(
        self: &Arc<Self>,
        purpose: CapabilityRootPurpose,
        path: AbsolutePath,
    ) -> Result<PathBuf, FsError> {
        self.open_managed_entry(purpose, path)
            .map(|opened| opened.resolved_path)
    }

    /// Opens a regular managed-config file and returns the retained target
    /// handle together with the resolved target path produced by that same
    /// operation. Ordinary symlinks are followed with a bounded manual walk;
    /// magic links are rejected by the platform probe.
    pub(crate) fn open_managed_read_file(
        self: &Arc<Self>,
        purpose: CapabilityRootPurpose,
        path: AbsolutePath,
    ) -> Result<ManagedCapabilityFile, FsError> {
        let configured_path = path.as_path().to_path_buf();
        let opened = self.open_managed_entry(purpose, path)?;
        let CapabilityEntry::File(file) = opened.entry else {
            return Err(FsError::UnsupportedEntryType);
        };
        let followed_link = opened.followed_link || configured_path != opened.resolved_path;
        Ok(ManagedCapabilityFile {
            file,
            resolved_path: opened.resolved_path,
            followed_link,
        })
    }

    /// Compatibility projection for callers that only need the retained file.
    pub(crate) fn open_read_file_allowing_link(
        self: &Arc<Self>,
        purpose: CapabilityRootPurpose,
        path: AbsolutePath,
    ) -> Result<CapabilityFile, FsError> {
        self.open_managed_read_file(purpose, path)
            .map(|managed| managed.file)
    }

    /// Read-only open of a directory whose path components may include
    /// managed-config symlinks. The returned lease is rooted at the retained
    /// target directory while preserving the configured display path.
    pub(crate) fn open_read_directory_allowing_link(
        self: &Arc<Self>,
        purpose: CapabilityRootPurpose,
        path: AbsolutePath,
    ) -> Result<CapabilityRootLease, FsError> {
        self.open_directory_allowing_link(purpose, path, CapabilityAccess::ReadOnly)
    }

    pub(crate) fn open_private_directory_allowing_link(
        self: &Arc<Self>,
        purpose: CapabilityRootPurpose,
        path: AbsolutePath,
    ) -> Result<CapabilityRootLease, FsError> {
        if purpose != CapabilityRootPurpose::AgentStore {
            return Err(FsError::OutsideCapability);
        }
        let display_root = path.clone();
        let (parent, name) = match self.open_exact_parent(
            purpose,
            path,
            CapabilityAccess::PrivateReadWrite,
            RootCreation::Existing,
        ) {
            Ok(authority) => authority,
            Err(error) if is_missing(&error) => {
                return self.open_root(
                    purpose,
                    display_root,
                    CapabilityAccess::PrivateReadWrite,
                    RootCreation::CreateDirectory,
                );
            }
            Err(error) => return Err(error),
        };
        let name_path = Path::new(&name);
        match parent.lease_directory(
            name_path,
            CapabilityAccess::PrivateReadWrite,
            RootCreation::Existing,
        ) {
            Ok(mut root) => {
                root.display_root = display_root;
                root.root_path_guard = Some(Arc::new(ManagedRootPathGuard::Directory {
                    parent,
                    name,
                    target_identity: root.identity(),
                }));
                Ok(root)
            }
            Err(error) if is_missing(&error) => {
                let mut root = self.open_root(
                    purpose,
                    display_root.clone(),
                    CapabilityAccess::PrivateReadWrite,
                    RootCreation::CreateDirectory,
                )?;
                root.display_root = display_root;
                Ok(root)
            }
            Err(error)
                if is_link_blocked(&error) || matches!(error, FsError::UnsupportedEntryType) =>
            {
                let link_identity =
                    platform_managed_link_identity(&parent.root.directory, name_path)?;
                let target = platform_read_link(&parent.root.directory, name_path)?;
                let expected = parent.followed_entry_identity(name_path)?;
                let joined;
                let target_input = if target.is_absolute() {
                    target.as_path()
                } else {
                    joined = parent.authority_root().join(&target);
                    joined.as_path()
                };
                let target_path = AbsolutePath::parse(lexically_normalize_absolute(target_input))
                    .map_err(|_| FsError::InvalidPath)?;
                let mut root = self.open_root(
                    purpose,
                    target_path,
                    CapabilityAccess::PrivateReadWrite,
                    RootCreation::Existing,
                )?;
                if platform_managed_link_identity(&parent.root.directory, name_path)?
                    != link_identity
                    || platform_read_link(&parent.root.directory, name_path)? != target
                    || root.identity() != expected
                {
                    return Err(FsError::IdentityChanged);
                }
                root.display_root = display_root;
                root.root_path_guard = Some(Arc::new(ManagedRootPathGuard::Link {
                    parent,
                    name,
                    link_identity,
                    target,
                    target_identity: expected,
                }));
                Ok(root)
            }
            Err(error) => Err(error),
        }
    }
    fn open_directory_allowing_link(
        self: &Arc<Self>,
        purpose: CapabilityRootPurpose,
        path: AbsolutePath,
        access: CapabilityAccess,
    ) -> Result<CapabilityRootLease, FsError> {
        let display_root = path.clone();
        let opened = self.open_managed_entry(purpose, path)?;
        let CapabilityEntry::Directory(directory) = opened.entry else {
            return Err(FsError::UnsupportedEntryType);
        };
        directory_into_root_lease_with_access(directory, display_root, access)
    }
}

fn directory_into_root_lease(
    directory: CapabilityDirectory,
    display_root: AbsolutePath,
) -> Result<CapabilityRootLease, FsError> {
    directory_into_root_lease_with_access(directory, display_root, CapabilityAccess::ReadOnly)
}

fn directory_into_root_lease_with_access(
    directory: CapabilityDirectory,
    display_root: AbsolutePath,
    access: CapabilityAccess,
) -> Result<CapabilityRootLease, FsError> {
    let authority_root = AbsolutePath::parse(
        directory
            .lease
            .authority_root
            .as_path()
            .join(&directory.relative),
    )
    .map_err(|_| FsError::InvalidPath)?;
    Ok(CapabilityRootLease {
        fs: directory.lease.fs.clone(),
        purpose: directory.lease.purpose,
        access,
        display_root,
        authority_root,
        root: Arc::new(OpenedDirectory {
            directory: directory.directory,
            identity: directory.identity,
        }),
        root_path_guard: directory.lease.root_path_guard.clone(),
    })
}

const MANAGED_LINK_MAX_HOPS: usize = 8;

fn open_managed_entry_with(
    fs: &Arc<CapabilityFs>,
    purpose: CapabilityRootPurpose,
    path: &Path,
    hops: &mut usize,
    on_link_captured: &mut dyn FnMut(),
) -> Result<ManagedOpenedEntry, FsError> {
    let components = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_os_string()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let root = fs.open_root(
        purpose,
        AbsolutePath::parse(PathBuf::from("/")).map_err(|_| FsError::InvalidPath)?,
        CapabilityAccess::ReadOnly,
        RootCreation::Existing,
    )?;
    open_managed_components(
        fs,
        purpose,
        root,
        PathBuf::from("/"),
        &components,
        hops,
        false,
        on_link_captured,
    )
}

fn open_managed_components(
    fs: &Arc<CapabilityFs>,
    purpose: CapabilityRootPurpose,
    mut directory: CapabilityRootLease,
    mut resolved_path: PathBuf,
    components: &[OsString],
    hops: &mut usize,
    followed_link: bool,
    on_link_captured: &mut dyn FnMut(),
) -> Result<ManagedOpenedEntry, FsError> {
    let Some((name, remaining)) = components.split_first() else {
        let identity = directory.identity();
        let directory_handle = directory
            .root
            .directory
            .try_clone()
            .map_err(|error| FsError::from_io(FsOperation::OpenEntry, &error))?;
        return Ok(ManagedOpenedEntry {
            entry: CapabilityEntry::Directory(CapabilityDirectory {
                lease: directory,
                relative: PathBuf::new(),
                identity,
                directory: directory_handle,
            }),
            resolved_path,
            followed_link,
        });
    };
    let name_path = Path::new(name);
    if platform_entry_is_symlink(&directory.root.directory, name_path)? {
        if *hops >= MANAGED_LINK_MAX_HOPS {
            return Err(FsError::InvalidPath);
        }
        *hops += 1;
        let link_identity = platform_managed_link_identity(&directory.root.directory, name_path)?;
        let target = platform_read_link(&directory.root.directory, name_path)?;
        let expected = directory.followed_entry_identity(name_path)?;
        let joined;
        let target_input = if target.is_absolute() {
            target.as_path()
        } else {
            joined = resolved_path.join(&target);
            joined.as_path()
        };
        let target_path = lexically_normalize_absolute(target_input);
        let mut opened_target =
            match open_managed_entry_with(fs, purpose, &target_path, hops, on_link_captured) {
                Ok(opened) => opened,
                Err(error) if is_missing(&error) => return Err(FsError::UnsupportedEntryType),
                Err(error) => return Err(error),
            };
        // Retained-link capture point: the target handle is now held by
        // `opened_target`, and we are about to re-pin the link identity. Fire
        // the hook here so a swap injected between capture and re-validation
        // is caught by the identity check below (FsError::IdentityChanged).
        on_link_captured();
        if platform_managed_link_identity(&directory.root.directory, name_path)? != link_identity
            || platform_read_link(&directory.root.directory, name_path)? != target
            || opened_target.entry.identity() != expected
        {
            return Err(FsError::IdentityChanged);
        }
        opened_target.followed_link = true;
        if remaining.is_empty() {
            return Ok(opened_target);
        }
        let CapabilityEntry::Directory(target_directory) = opened_target.entry else {
            return Err(FsError::UnsupportedEntryType);
        };
        let target_root = directory_into_root_lease(
            target_directory,
            AbsolutePath::parse(opened_target.resolved_path.clone())
                .map_err(|_| FsError::InvalidPath)?,
        )?;
        return open_managed_components(
            fs,
            purpose,
            target_root,
            opened_target.resolved_path,
            remaining,
            hops,
            true,
            on_link_captured,
        );
    }
    resolved_path.push(name);
    if remaining.is_empty() {
        let entry = directory.open_entry(name_path)?;
        return Ok(ManagedOpenedEntry {
            entry,
            resolved_path,
            followed_link,
        });
    }
    directory = directory.lease_directory(
        name_path,
        CapabilityAccess::ReadOnly,
        RootCreation::Existing,
    )?;
    open_managed_components(
        fs,
        purpose,
        directory,
        resolved_path,
        remaining,
        hops,
        followed_link,
        on_link_captured,
    )
}

fn open_managed_components_within_root(
    root: &CapabilityRootLease,
    mut resolved_relative: PathBuf,
    components: &[OsString],
    hops: &mut usize,
) -> Result<ManagedOpenedEntry, FsError> {
    let Some((name, remaining)) = components.split_first() else {
        let entry = if resolved_relative.as_os_str().is_empty() {
            let directory = root.open_directory(Path::new(""))?;
            CapabilityEntry::Directory(directory)
        } else {
            root.open_entry(&resolved_relative)?
        };
        return Ok(ManagedOpenedEntry {
            entry,
            resolved_path: root.authority_root.as_path().join(resolved_relative),
            followed_link: false,
        });
    };
    let parent = root.lease_directory(
        &resolved_relative,
        CapabilityAccess::ReadOnly,
        RootCreation::Existing,
    )?;
    let name_path = Path::new(name);
    if platform_entry_is_symlink(&parent.root.directory, name_path)? {
        if *hops >= MANAGED_LINK_MAX_HOPS {
            return Err(FsError::InvalidPath);
        }
        *hops += 1;
        let link_identity = platform_managed_link_identity(&parent.root.directory, name_path)?;
        let target = platform_read_link(&parent.root.directory, name_path)?;
        let target_relative = managed_target_within_root(root, &resolved_relative, &target)?;
        let target_components = target_relative
            .components()
            .filter_map(|component| match component {
                Component::Normal(name) => Some(name.to_os_string()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut opened_target = match open_managed_components_within_root(
            root,
            PathBuf::new(),
            &target_components,
            hops,
        ) {
            Ok(opened) => opened,
            Err(error) if is_missing(&error) => return Err(FsError::UnsupportedEntryType),
            Err(error) => return Err(error),
        };
        let expected = parent.followed_entry_identity(name_path)?;
        if platform_managed_link_identity(&parent.root.directory, name_path)? != link_identity
            || platform_read_link(&parent.root.directory, name_path)? != target
            || opened_target.entry.identity() != expected
        {
            return Err(FsError::IdentityChanged);
        }
        opened_target.followed_link = true;
        if remaining.is_empty() {
            return Ok(opened_target);
        }
        if !matches!(opened_target.entry, CapabilityEntry::Directory(_)) {
            return Err(FsError::UnsupportedEntryType);
        }
        let target_relative = opened_target
            .resolved_path
            .strip_prefix(root.authority_root.as_path())
            .map_err(|_| FsError::OutsideCapability)?;
        return open_managed_components_within_root(
            root,
            target_relative.to_path_buf(),
            remaining,
            hops,
        )
        .map(|mut opened| {
            opened.followed_link = true;
            opened
        });
    }
    resolved_relative.push(name);
    open_managed_components_within_root(root, resolved_relative, remaining, hops)
}

fn managed_target_within_root(
    root: &CapabilityRootLease,
    parent_relative: &Path,
    target: &Path,
) -> Result<PathBuf, FsError> {
    if target.is_absolute() {
        let normalized = lexically_normalize_absolute(target);
        return normalized
            .strip_prefix(root.authority_root.as_path())
            .map(Path::to_path_buf)
            .map_err(|_| FsError::OutsideCapability);
    }
    let mut normalized = PathBuf::new();
    for component in parent_relative.join(target).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => normalized.push(name),
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(FsError::OutsideCapability);
                }
            }
            Component::RootDir | Component::Prefix(_) => return Err(FsError::OutsideCapability),
        }
    }
    Ok(normalized)
}

impl std::fmt::Debug for CapabilityRootLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CapabilityRootLease")
            .field("purpose", &self.purpose)
            .field("access", &self.access)
            .field("identity", &self.root.identity)
            .finish_non_exhaustive()
    }
}

impl Clone for CapabilityRootLease {
    fn clone(&self) -> Self {
        Self {
            fs: self.fs.clone(),
            purpose: self.purpose,
            access: self.access,
            display_root: self.display_root.clone(),
            root: self.root.clone(),
            authority_root: self.authority_root.clone(),
            root_path_guard: self.root_path_guard.clone(),
        }
    }
}

impl CapabilityRootLease {
    pub(crate) const fn purpose(&self) -> CapabilityRootPurpose {
        self.purpose
    }

    pub(crate) const fn access(&self) -> CapabilityAccess {
        self.access
    }

    pub(crate) fn display_root(&self) -> &Path {
        self.display_root.as_path()
    }

    pub(crate) fn authority_root(&self) -> &Path {
        self.authority_root.as_path()
    }

    pub(crate) fn identity(&self) -> FileIdentity {
        self.root.identity
    }
    pub(crate) fn belongs_to(&self, capabilities: &Arc<CapabilityFs>) -> bool {
        Arc::ptr_eq(&self.fs, capabilities)
    }

    pub(crate) fn capabilities(&self) -> Arc<CapabilityFs> {
        self.fs.clone()
    }

    pub(crate) fn validate_live_root(&self) -> Result<(), FsError> {
        let Some(guard) = self.root_path_guard.as_ref() else {
            return Ok(());
        };
        match guard.as_ref() {
            ManagedRootPathGuard::Directory {
                parent,
                name,
                target_identity,
            } => {
                let current = platform_entry_identity(&parent.root.directory, Path::new(name))
                    .map_err(|error| {
                        if is_missing(&error) {
                            FsError::IdentityChanged
                        } else {
                            error
                        }
                    })?;
                if current != *target_identity || current != self.root.identity {
                    return Err(FsError::IdentityChanged);
                }
            }
            ManagedRootPathGuard::Link {
                parent,
                name,
                link_identity,
                target,
                target_identity,
            } => {
                let name = Path::new(name);
                let current_link = platform_managed_link_identity(&parent.root.directory, name)
                    .map_err(|error| {
                        if is_missing(&error) {
                            FsError::IdentityChanged
                        } else {
                            error
                        }
                    })?;
                let current_target =
                    platform_read_link(&parent.root.directory, name).map_err(|error| {
                        if is_missing(&error) {
                            FsError::IdentityChanged
                        } else {
                            error
                        }
                    })?;
                let current_identity = parent.followed_entry_identity(name).map_err(|error| {
                    if is_missing(&error) {
                        FsError::IdentityChanged
                    } else {
                        error
                    }
                })?;
                if current_link != *link_identity
                    || current_target != *target
                    || current_identity != *target_identity
                    || current_identity != self.root.identity
                {
                    return Err(FsError::IdentityChanged);
                }
            }
        }
        Ok(())
    }
    pub(crate) fn duplicate_for_spawn_working_directory(
        &self,
    ) -> Result<CapabilityWorkingDirectorySource, FsError> {
        let directory = self
            .root
            .directory
            .try_clone()
            .map_err(|error| FsError::from_io(FsOperation::DuplicateForSpawn, &error))?
            .into_std_file();
        let opened = Dir::from_std_file(
            directory
                .try_clone()
                .map_err(|error| FsError::from_io(FsOperation::DuplicateForSpawn, &error))?,
        );
        let identity = platform_directory_identity(&opened)?;
        if identity != self.root.identity {
            return Err(FsError::IdentityChanged);
        }
        Ok(CapabilityWorkingDirectorySource {
            fs: self.fs.clone(),
            path: self.display_root.clone(),
            directory,
            identity,
            validation_root: self.root.clone(),
            relative: PathBuf::new(),
        })
    }
    pub(crate) fn duplicate_descendant_for_spawn_working_directory(
        &self,
        path: &Path,
    ) -> Result<CapabilityWorkingDirectorySource, FsError> {
        if !matches!(
            self.purpose,
            CapabilityRootPurpose::ProjectResources | CapabilityRootPurpose::WorkflowWorktrees
        ) {
            return Err(FsError::OutsideCapability);
        }
        let relative = path
            .strip_prefix(self.display_root())
            .map_err(|_| FsError::OutsideCapability)?;
        validate_relative(relative)?;
        let directory = platform_open_directory(&self.root.directory, relative)?;
        let identity = platform_directory_identity(&directory)?;
        let path = AbsolutePath::parse(path.to_path_buf()).map_err(|_| FsError::InvalidPath)?;
        Ok(CapabilityWorkingDirectorySource {
            fs: self.fs.clone(),
            path,
            directory: directory.into_std_file(),
            validation_root: self.root.clone(),
            relative: relative.to_path_buf(),
            identity,
        })
    }
    pub(crate) fn lease_directory(
        &self,
        relative: &Path,
        access: CapabilityAccess,
        creation: RootCreation,
    ) -> Result<Self, FsError> {
        validate_relative(relative)?;
        if !self.access.can_narrow_to(access) {
            return Err(FsError::ReadOnly);
        }
        if matches!(creation, RootCreation::CreateDirectory) {
            self.require_write()?;
            let mode = if matches!(access, CapabilityAccess::PrivateReadWrite) {
                FileMode::Private
            } else {
                FileMode::Shared
            };
            platform_create_dir_all(&self.root.directory, relative, mode)?;
        }
        let directory = platform_open_directory(&self.root.directory, relative)?;
        let identity = platform_directory_identity(&directory)?;
        let display_root = AbsolutePath::parse(self.display_root().join(relative))
            .map_err(|_| FsError::InvalidPath)?;
        let authority_root = AbsolutePath::parse(self.authority_root().join(relative))
            .map_err(|_| FsError::InvalidPath)?;
        Ok(Self {
            fs: self.fs.clone(),
            purpose: self.purpose,
            access,
            display_root,
            authority_root,
            root: Arc::new(OpenedDirectory {
                directory,
                identity,
            }),
            root_path_guard: self.root_path_guard.clone(),
        })
    }

    pub(crate) fn entry_exists(&self, relative: &Path) -> Result<bool, FsError> {
        validate_relative(relative)?;
        match self.open_entry(relative) {
            Ok(_) => Ok(true),
            Err(FsError::System {
                kind: io::ErrorKind::NotFound,
                ..
            }) => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn metadata(&self, relative: &Path) -> Result<FsMetadata, FsError> {
        validate_relative(relative)?;
        if relative.as_os_str().is_empty() {
            return metadata_from_cap(
                self.root.identity,
                &self
                    .root
                    .directory
                    .dir_metadata()
                    .map_err(|error| FsError::from_io(FsOperation::OpenEntry, &error))?,
            );
        }
        match self.open_entry(relative)? {
            CapabilityEntry::File(file) => file.metadata(),
            CapabilityEntry::Directory(directory) => metadata_from_cap(
                directory.identity,
                &directory
                    .directory
                    .dir_metadata()
                    .map_err(|error| FsError::from_io(FsOperation::OpenEntry, &error))?,
            ),
        }
    }

    pub(crate) fn open_file(&self, relative: &Path) -> Result<CapabilityFile, FsError> {
        validate_relative(relative)?;
        let file = platform_open_read_file(&self.root.directory, relative)
            .map_err(|error| self.annotate_entry(relative, error))?;
        let identity =
            platform_file_identity(&file).map_err(|error| self.annotate_entry(relative, error))?;
        if identity.kind() != EntryKind::RegularFile {
            return Err(self.annotate_entry(relative, FsError::UnsupportedEntryType));
        }
        Ok(CapabilityFile {
            lease: self.clone(),
            relative: relative.to_path_buf(),
            identity,
            file,
        })
    }

    /// Read-only open of a final managed-config file link relative to this
    /// retained root. Strict open is attempted first; only the link failure
    /// path enters the bounded, identity-revalidated within-root walker.
    pub(crate) fn open_file_allowing_link(
        &self,
        relative: &Path,
    ) -> Result<CapabilityFile, FsError> {
        validate_relative(relative)?;
        match self.open_file(relative) {
            Ok(file) => Ok(file),
            Err(error)
                if is_link_blocked(&error) || matches!(error, FsError::UnsupportedEntryType) =>
            {
                let opened = self
                    .open_agent_store_top_level_link(relative)
                    .map_err(|error| self.annotate_entry(relative, error))?;
                let CapabilityEntry::File(file) = opened.entry else {
                    return Err(self.annotate_entry(relative, FsError::UnsupportedEntryType));
                };
                Ok(file)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn open_managed_entry_within_root(
        &self,
        relative: &Path,
    ) -> Result<ManagedOpenedEntry, FsError> {
        validate_relative(relative)?;
        let components = relative
            .components()
            .filter_map(|component| match component {
                Component::Normal(name) => Some(name.to_os_string()),
                _ => None,
            })
            .collect::<Vec<_>>();
        open_managed_components_within_root(self, PathBuf::new(), &components, &mut 0)
    }

    fn open_agent_store_top_level_link(
        &self,
        relative: &Path,
    ) -> Result<ManagedOpenedEntry, FsError> {
        if self.purpose != CapabilityRootPurpose::AgentStore {
            return Err(FsError::OutsideCapability);
        }
        validate_relative(relative)?;
        let mut components = relative.components();
        let Some(Component::Normal(name)) = components.next() else {
            return Err(FsError::InvalidPath);
        };
        if components.next().is_some() {
            return Err(FsError::OutsideCapability);
        }
        let name = Path::new(name);
        if !platform_entry_is_symlink(&self.root.directory, name)? {
            return Err(FsError::UnsupportedEntryType);
        }
        let link_identity = platform_managed_link_identity(&self.root.directory, name)?;
        let target = platform_read_link(&self.root.directory, name)?;
        let expected = self.followed_entry_identity(name)?;
        let joined;
        let target_input = if target.is_absolute() {
            target.as_path()
        } else {
            joined = self.authority_root().join(&target);
            joined.as_path()
        };
        let target_path = lexically_normalize_absolute(target_input);
        let mut hops = 1;
        let mut opened = match open_managed_entry_with(
            &self.fs,
            self.purpose,
            &target_path,
            &mut hops,
            &mut || {},
        ) {
            Ok(opened) => opened,
            Err(error) if is_missing(&error) => return Err(FsError::UnsupportedEntryType),
            Err(error) => return Err(error),
        };
        if platform_managed_link_identity(&self.root.directory, name)? != link_identity
            || platform_read_link(&self.root.directory, name)? != target
            || opened.entry.identity() != expected
        {
            return Err(FsError::IdentityChanged);
        }
        opened.followed_link = true;
        Ok(opened)
    }

    pub(crate) fn open_managed_file_within_root(
        &self,
        relative: &Path,
    ) -> Result<ManagedCapabilityFile, FsError> {
        let opened = self.open_managed_entry_within_root(relative)?;
        let CapabilityEntry::File(file) = opened.entry else {
            return Err(FsError::UnsupportedEntryType);
        };
        Ok(ManagedCapabilityFile {
            file,
            resolved_path: opened.resolved_path,
            followed_link: opened.followed_link,
        })
    }

    pub(crate) fn open_managed_directory_within_root(
        &self,
        relative: &Path,
    ) -> Result<CapabilityRootLease, FsError> {
        let display_root = AbsolutePath::parse(self.display_root().join(relative))
            .map_err(|_| FsError::InvalidPath)?;
        let opened = self.open_managed_entry_within_root(relative)?;
        let CapabilityEntry::Directory(directory) = opened.entry else {
            return Err(FsError::UnsupportedEntryType);
        };
        directory_into_root_lease(directory, display_root)
    }

    pub(crate) fn read_bounded_allowing_link(
        &self,
        relative: &Path,
        max_bytes: u64,
    ) -> Result<Vec<u8>, FsError> {
        self.open_file_allowing_link(relative)?
            .read_bounded(max_bytes)
    }

    pub(crate) fn metadata_allowing_link(&self, relative: &Path) -> Result<FsMetadata, FsError> {
        match self.metadata(relative) {
            Ok(metadata) => Ok(metadata),
            Err(error)
                if is_link_blocked(&error) || matches!(error, FsError::UnsupportedEntryType) =>
            {
                self.open_file_allowing_link(relative)?.metadata()
            }
            Err(error) => Err(error),
        }
    }

    fn annotate_entry(&self, relative: &Path, error: FsError) -> FsError {
        error.with_entry_context(self.purpose, &self.display_root().join(relative))
    }

    pub(crate) fn open_read_write_file(&self, relative: &Path) -> Result<CapabilityFile, FsError> {
        validate_relative(relative)?;
        self.require_write()?;
        let file = platform_open_read_write_file(&self.root.directory, relative)?;
        let identity = platform_file_identity(&file)?;
        if identity.kind() != EntryKind::RegularFile {
            return Err(FsError::UnsupportedEntryType);
        }
        Ok(CapabilityFile {
            lease: self.clone(),
            relative: relative.to_path_buf(),
            identity,
            file,
        })
    }

    pub(crate) fn open_directory(&self, relative: &Path) -> Result<CapabilityDirectory, FsError> {
        validate_relative(relative)?;
        let directory = platform_open_directory(&self.root.directory, relative)
            .map_err(|error| self.annotate_entry(relative, error))?;
        let identity = platform_directory_identity(&directory)
            .map_err(|error| self.annotate_entry(relative, error))?;
        Ok(CapabilityDirectory {
            lease: self.clone(),
            relative: relative.to_path_buf(),
            identity,
            directory,
        })
    }
    pub(crate) fn sync_directory(&self, relative: &Path) -> Result<(), FsError> {
        validate_relative(relative)?;
        #[cfg(target_os = "windows")]
        {
            let _ = relative;
            return Ok(());
        }
        #[cfg(not(target_os = "windows"))]
        {
            let directory = if relative.as_os_str().is_empty() {
                self.root
                    .directory
                    .try_clone()
                    .map_err(|error| FsError::from_io(FsOperation::Sync, &error))?
            } else {
                platform_open_directory(&self.root.directory, relative)?
            };
            platform_sync_directory(&directory)
                .map_err(|error| self.annotate_entry(relative, error))
        }
    }

    /// Read-only open of a final managed-config directory link relative to
    /// this retained root. Strict open is attempted first; only the link
    /// failure path enters the bounded, identity-revalidated within-root
    /// walker. The returned directory remains anchored to the captured target.
    pub(crate) fn open_directory_allowing_link(
        &self,
        relative: &Path,
    ) -> Result<CapabilityDirectory, FsError> {
        validate_relative(relative)?;
        match self.open_directory(relative) {
            Ok(directory) => Ok(directory),
            Err(error)
                if is_link_blocked(&error) || matches!(error, FsError::UnsupportedEntryType) =>
            {
                let opened = self
                    .open_agent_store_top_level_link(relative)
                    .map_err(|error| self.annotate_entry(relative, error))?;
                let CapabilityEntry::Directory(directory) = opened.entry else {
                    return Err(self.annotate_entry(relative, FsError::UnsupportedEntryType));
                };
                let display_root = AbsolutePath::parse(self.display_root().join(relative))
                    .map_err(|_| FsError::InvalidPath)?;
                let root = directory_into_root_lease(directory, display_root)?;
                let identity = root.identity();
                let directory = root
                    .root
                    .directory
                    .try_clone()
                    .map_err(|error| FsError::from_io(FsOperation::OpenEntry, &error))?;
                Ok(CapabilityDirectory {
                    lease: root,
                    relative: PathBuf::new(),
                    identity,
                    directory,
                })
            }
            Err(error) => Err(error),
        }
    }
    /// Identity of what the final path component resolves to, following any
    /// managed-config symbolic link chain anchored at this lease's directory.
    fn followed_entry_identity(&self, relative: &Path) -> Result<FileIdentity, FsError> {
        platform_followed_entry_identity(&self.root.directory, relative)
    }

    pub(crate) fn read_bounded(&self, relative: &Path, max_bytes: u64) -> Result<Vec<u8>, FsError> {
        self.open_file(relative)?.read_bounded(max_bytes)
    }

    pub(crate) fn read_text_bounded(
        &self,
        relative: &Path,
        max_bytes: u64,
    ) -> Result<String, FsError> {
        String::from_utf8(self.read_bounded(relative, max_bytes)?).map_err(|_| FsError::InvalidPath)
    }

    pub(crate) fn read_dir(&self, relative: &Path) -> Result<CapabilityReadDir, FsError> {
        self.open_directory(relative)?.read_dir()
    }

    pub(crate) fn create_dir_all(&self, relative: &Path, mode: FileMode) -> Result<(), FsError> {
        validate_relative(relative)?;
        self.require_write()?;
        platform_create_dir_all(&self.root.directory, relative, mode)
    }

    pub(crate) fn create_new(
        &self,
        relative: &Path,
        mode: FileMode,
    ) -> Result<CapabilityWritableFile, FsError> {
        validate_relative(relative)?;
        self.require_write()?;
        let file = platform_create_new_file(&self.root.directory, relative, mode)?;
        let identity = platform_file_identity(&file)?;
        Ok(CapabilityWritableFile {
            lease: self.clone(),
            relative: relative.to_path_buf(),
            identity,
            file: Some(file),
            finished: false,
        })
    }

    pub(crate) fn read_dir_rejecting_unsupported(
        &self,
        relative: &Path,
    ) -> Result<CapabilityReadDir, FsError> {
        self.open_directory(relative)?
            .read_dir_rejecting_unsupported()
    }

    pub(crate) fn replace_file_streaming(
        &self,
        replacement: StreamingReplacement,
        source: &CapabilityFile,
        aggregate_actual: &mut u64,
        aggregate_limit: u64,
        buffer: &mut StreamCopyBuffer,
    ) -> Result<FileIdentity, FsError> {
        self.require_write()?;
        let mut prepared = self.prepare_streaming_replacement(&replacement)?;
        prepared.stage_from(source, aggregate_actual, aggregate_limit, buffer)?;
        prepared.commit(replacement.expected)
    }

    #[cfg(test)]
    pub(crate) fn replace_file_streaming_with<F>(
        &self,
        replacement: StreamingReplacement,
        source: &CapabilityFile,
        aggregate_actual: &mut u64,
        aggregate_limit: u64,
        buffer: &mut StreamCopyBuffer,
        mid_copy: F,
    ) -> Result<FileIdentity, FsError>
    where
        F: FnOnce() -> Result<(), FsError>,
    {
        self.require_write()?;
        let mut prepared = self.prepare_streaming_replacement(&replacement)?;
        prepared.stage_from(source, aggregate_actual, aggregate_limit, buffer)?;
        mid_copy()?;
        prepared.commit(replacement.expected)
    }

    fn prepare_streaming_replacement(
        &self,
        replacement: &StreamingReplacement,
    ) -> Result<PreparedStreamingReplacement, FsError> {
        validate_relative(&replacement.relative)?;
        let parent_relative = replacement
            .relative
            .parent()
            .unwrap_or_else(|| Path::new(""));
        let name = replacement
            .relative
            .file_name()
            .ok_or(FsError::InvalidPath)?
            .to_os_string();
        validate_name(&name)?;
        let parent = self.lease_directory(parent_relative, self.access, RootCreation::Existing)?;
        validate_replacement_target(&parent, &name, replacement.expected)?;
        Ok(PreparedStreamingReplacement {
            parent,
            name: name.clone(),
            temporary: unique_sibling(&name, "replace"),
            temporary_identity: None,
            backup: None,
            installed: false,
            committed: false,
        })
    }
    pub(crate) fn create_new_directory(
        &self,
        relative: &Path,
        access: CapabilityAccess,
    ) -> Result<CapabilityRootLease, FsError> {
        validate_relative(relative)?;
        self.require_write()?;
        if access.permits_write() {
            self.require_write()?;
        }
        let mode = match access {
            CapabilityAccess::PrivateReadWrite => FileMode::Private,
            CapabilityAccess::ReadOnly | CapabilityAccess::ReadWrite => FileMode::Shared,
        };
        platform_create_dir_all(&self.root.directory, relative, mode)?;
        self.lease_directory(relative, access, RootCreation::Existing)
    }

    /// Creates the leaf directory only if it does not already exist. Unlike
    /// [`Self::create_new_directory`], an existing entry is an error, so the
    /// returned lease is guaranteed to have been created by this call.
    pub(crate) fn create_directory_exclusive(
        &self,
        relative: &Path,
        access: CapabilityAccess,
    ) -> Result<CapabilityRootLease, FsError> {
        validate_relative(relative)?;
        self.require_write()?;
        let mode = match access {
            CapabilityAccess::PrivateReadWrite => FileMode::Private,
            CapabilityAccess::ReadOnly | CapabilityAccess::ReadWrite => FileMode::Shared,
        };
        platform_create_directory_exclusive(&self.root.directory, relative, mode)?;
        self.lease_directory(relative, access, RootCreation::Existing)
    }

    pub(crate) fn open_entry_exact(&self, relative: &Path) -> Result<CapabilityEntry, FsError> {
        self.open_entry(relative)
    }

    pub(crate) fn read_dir_names_strict(&self, relative: &Path) -> Result<Vec<OsString>, FsError> {
        self.open_directory(relative)?
            .read_dir_names_rejecting_unsupported()
    }

    /// Atomically replaces a regular file while allowing the final path
    /// component to be a live managed link within this retained root. The
    /// generic [`Self::replace_file`] contract remains strict: only this
    /// explicit seam writes through a leaf link, and it always replaces the
    /// resolved regular target rather than the link entry.
    pub(crate) fn replace_file_allowing_live_leaf_link(
        &self,
        replacement: Replacement,
    ) -> Result<FileIdentity, FsError> {
        self.replace_file_allowing_live_leaf_link_with(replacement, || {})
    }

    fn replace_file_allowing_live_leaf_link_with<F>(
        &self,
        mut replacement: Replacement,
        on_target_resolved: F,
    ) -> Result<FileIdentity, FsError>
    where
        F: FnOnce(),
    {
        validate_relative(&replacement.relative)?;
        self.require_write()?;
        let source_relative = replacement.relative.clone();
        let source_is_link = match platform_entry_is_symlink(&self.root.directory, &source_relative)
        {
            Ok(is_link) => is_link,
            Err(error) if is_missing(&error) => false,
            Err(error) => return Err(error),
        };
        let strict_error = match self.metadata(&source_relative) {
            Ok(metadata) if metadata.identity.kind() == EntryKind::RegularFile => {
                if replacement
                    .expected
                    .is_some_and(|expected| expected != metadata.identity)
                {
                    return Err(FsError::ReplacementConflict);
                }
                replacement.expected = Some(metadata.identity);
                return self.replace_file(replacement);
            }
            Ok(_) => return Err(FsError::UnsupportedEntryType),
            Err(error) if is_missing(&error) && source_is_link => error,
            Err(error) if is_missing(&error) => {
                if replacement.expected.is_some() {
                    return Err(FsError::ReplacementConflict);
                }
                return self.replace_file(replacement);
            }
            Err(error @ FsError::UnsupportedEntryType) => error,
            Err(error) => return Err(error),
        };
        let guard = match self.capture_managed_leaf_link(&source_relative) {
            Ok(Some(guard)) => guard,
            Ok(None) => return Err(strict_error),
            Err(error) if is_missing(&error) => return Err(FsError::IdentityChanged),
            Err(error) => return Err(error),
        };
        let managed = match self.open_managed_file_within_root(&source_relative) {
            Ok(managed) => managed,
            Err(error) => {
                return match self.validate_managed_leaf_link(&source_relative, &guard) {
                    Ok(()) => Err(error),
                    Err(validation) => Err(validation),
                };
            }
        };
        let resolved_path = managed.resolved_path().to_path_buf();
        let target = managed.into_file();
        let target_identity = target.identity();
        if replacement
            .expected
            .is_some_and(|expected| expected != target_identity)
        {
            return Err(FsError::ReplacementConflict);
        }
        let (target_parent, target_relative) = target.authority();
        replacement.relative = target_relative;
        replacement.expected = Some(target_identity);

        on_target_resolved();
        self.validate_managed_leaf_link(&source_relative, &guard)?;
        let installed = target_parent.replace_file(replacement)?;
        self.validate_managed_leaf_link(&source_relative, &guard)?;
        let reopened = self
            .open_managed_file_within_root(&source_relative)
            .map_err(|_| FsError::IdentityChanged)?;
        if reopened.resolved_path() != resolved_path || reopened.into_file().identity() != installed
        {
            return Err(FsError::IdentityChanged);
        }
        Ok(installed)
    }

    fn capture_managed_leaf_link(
        &self,
        relative: &Path,
    ) -> Result<Option<ManagedLeafLinkGuard>, FsError> {
        if !platform_entry_is_symlink(&self.root.directory, relative)? {
            return Ok(None);
        }
        let guard = ManagedLeafLinkGuard {
            identity: platform_managed_link_identity(&self.root.directory, relative)?,
            target: platform_read_link(&self.root.directory, relative)?,
        };
        self.validate_managed_leaf_link(relative, &guard)?;
        Ok(Some(guard))
    }

    fn validate_managed_leaf_link(
        &self,
        relative: &Path,
        expected: &ManagedLeafLinkGuard,
    ) -> Result<(), FsError> {
        match platform_managed_link_identity(&self.root.directory, relative) {
            Ok(identity) if identity == expected.identity => {}
            Ok(_) | Err(FsError::IdentityChanged | FsError::UnsupportedEntryType) => {
                return Err(FsError::IdentityChanged);
            }
            Err(error) if is_missing(&error) => return Err(FsError::IdentityChanged),
            Err(error) => return Err(error),
        }
        match platform_read_link(&self.root.directory, relative) {
            Ok(target) if target == expected.target => Ok(()),
            Ok(_) => Err(FsError::IdentityChanged),
            Err(error) if is_missing(&error) => Err(FsError::IdentityChanged),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn replace_file(&self, replacement: Replacement) -> Result<FileIdentity, FsError> {
        self.replace_batch(vec![replacement])
            .and_then(|identities| identities.into_iter().next().ok_or(FsError::InvalidPath))
    }

    pub(crate) fn replace_batch(
        &self,
        mut replacements: Vec<Replacement>,
    ) -> Result<Vec<FileIdentity>, FsError> {
        self.require_write()?;
        replacements.sort_by(|left, right| {
            normalize_key(&left.relative).cmp(&normalize_key(&right.relative))
        });
        if replacements
            .windows(2)
            .any(|pair| normalize_key(&pair[0].relative) == normalize_key(&pair[1].relative))
        {
            return Err(FsError::InvalidPath);
        }
        let mut prepared = replacements
            .into_iter()
            .map(|replacement| self.prepare_replacement(replacement))
            .collect::<Result<Vec<_>, _>>()?;
        for index in 0..prepared.len() {
            if let Err(error) = prepared[index].stage() {
                prepared.iter_mut().for_each(PreparedReplacement::cleanup);
                return Err(error);
            }
        }
        let mut committed = 0;
        while committed < prepared.len() {
            match prepared[committed].commit() {
                Ok(()) => committed += 1,
                Err(error) => {
                    let current_restoration = prepared[committed].restore();
                    let prior_restoration = prepared[..committed]
                        .iter_mut()
                        .rev()
                        .try_for_each(PreparedReplacement::restore);
                    prepared.iter_mut().for_each(PreparedReplacement::cleanup);
                    return if current_restoration.is_err() || prior_restoration.is_err() {
                        Err(FsError::IdentityChanged)
                    } else {
                        Err(error)
                    };
                }
            }
        }
        let identities = prepared
            .iter()
            .map(PreparedReplacement::installed_identity)
            .collect::<Result<Vec<_>, _>>();
        let identities = match identities {
            Ok(identities) => identities,
            Err(error) => {
                let restoration = prepared
                    .iter_mut()
                    .rev()
                    .try_for_each(PreparedReplacement::restore);
                prepared.iter_mut().for_each(PreparedReplacement::cleanup);
                return restoration.map_or(Err(FsError::IdentityChanged), |_| Err(error));
            }
        };
        for row in &mut prepared {
            row.finish()?;
        }
        Ok(identities)
    }

    pub(crate) fn remove_file(&self, relative: &Path) -> Result<(), FsError> {
        let expected = self.metadata(relative)?.identity;
        self.remove_file_expected(relative, expected)
    }

    pub(crate) fn remove_file_expected(
        &self,
        relative: &Path,
        expected: FileIdentity,
    ) -> Result<(), FsError> {
        validate_relative(relative)?;
        self.require_write()?;
        if relative.as_os_str().is_empty() {
            return Err(FsError::InvalidPath);
        }
        if expected.kind() != EntryKind::RegularFile {
            return Err(FsError::UnsupportedEntryType);
        }
        let opened = self.open_file(relative)?;
        if opened.identity() != expected
            || platform_entry_identity(&self.root.directory, relative)? != expected
        {
            return Err(FsError::IdentityChanged);
        }
        self.root
            .directory
            .remove_file(relative)
            .map_err(|error| FsError::from_io(FsOperation::Remove, &error))
    }

    pub(crate) fn remove_directory(&self, relative: &Path) -> Result<(), FsError> {
        validate_relative(relative)?;
        self.require_write()?;
        if relative.as_os_str().is_empty() {
            return Err(FsError::InvalidPath);
        }
        if self.metadata(relative)?.identity.kind() != EntryKind::Directory {
            return Err(FsError::UnsupportedEntryType);
        }
        self.root
            .directory
            .remove_dir(relative)
            .map_err(|error| FsError::from_io(FsOperation::Remove, &error))
    }
    /// Unlinks a non-directory leaf without following its final component.
    pub(crate) fn remove_nondirectory_entry(&self, relative: &Path) -> Result<(), FsError> {
        validate_relative(relative)?;
        self.require_write()?;
        if relative.as_os_str().is_empty() {
            return Err(FsError::InvalidPath);
        }
        self.root
            .directory
            .remove_file(relative)
            .map_err(|error| FsError::from_io(FsOperation::Remove, &error))
    }

    pub(crate) fn remove_tree(&self, relative: &Path) -> Result<(), FsError> {
        let expected = self.metadata(relative)?.identity;
        self.remove_tree_expected(relative, expected)
    }

    pub(crate) fn remove_tree_expected(
        &self,
        relative: &Path,
        expected: FileIdentity,
    ) -> Result<(), FsError> {
        validate_relative(relative)?;
        self.require_write()?;
        if relative.as_os_str().is_empty() {
            return Err(FsError::InvalidPath);
        }
        if expected.kind() != EntryKind::Directory {
            return Err(FsError::UnsupportedEntryType);
        }
        let directory = self.open_directory(relative)?;
        if directory.identity() != expected
            || platform_entry_identity(&self.root.directory, relative)? != expected
        {
            return Err(FsError::IdentityChanged);
        }
        remove_opened_tree(directory)?;
        if platform_entry_identity(&self.root.directory, relative)? != expected {
            return Err(FsError::IdentityChanged);
        }
        self.root
            .directory
            .remove_dir(relative)
            .map_err(|error| FsError::from_io(FsOperation::Remove, &error))
    }

    pub(crate) fn rename(&self, from: &Path, to: &Path) -> Result<(), FsError> {
        validate_relative(from)?;
        validate_relative(to)?;
        self.require_write()?;
        let identity = platform_entry_identity(&self.root.directory, from)?;
        self.root
            .directory
            .rename(from, &self.root.directory, to)
            .map_err(|error| FsError::from_io(FsOperation::Rename, &error))?;
        let renamed = platform_entry_identity(&self.root.directory, to)?;
        if identity != renamed {
            return Err(FsError::IdentityChanged);
        }
        platform_sync_directory(&self.root.directory)
    }

    pub(crate) fn link_existing(&self, from: &Path, to: &Path) -> Result<(), FsError> {
        validate_relative(from)?;
        validate_relative(to)?;
        self.require_write()?;
        let identity = platform_entry_identity(&self.root.directory, from)?;
        if identity.kind() != EntryKind::RegularFile {
            return Err(FsError::UnsupportedEntryType);
        }
        self.root
            .directory
            .hard_link(from, &self.root.directory, to)
            .map_err(|error| FsError::from_io(FsOperation::Link, &error))?;
        if platform_entry_identity(&self.root.directory, to)? != identity {
            return Err(FsError::IdentityChanged);
        }
        platform_sync_directory(&self.root.directory)
    }

    pub(crate) fn duplicate_for_spawn(&self) -> Result<CapabilitySpawnSource, FsError> {
        let directory = self
            .root
            .directory
            .try_clone()
            .map_err(|error| FsError::from_io(FsOperation::DuplicateForSpawn, &error))?
            .into_std_file();
        Ok(CapabilitySpawnSource {
            fs: self.fs.clone(),
            access: self.access,
            purpose: self.purpose,
            display_root: self.display_root.clone(),
            directory,
            identity: self.root.identity,
        })
    }

    pub(crate) fn open_entry(&self, relative: &Path) -> Result<CapabilityEntry, FsError> {
        let identity = platform_entry_identity(&self.root.directory, relative)
            .map_err(|error| self.annotate_entry(relative, error))?;
        match identity.kind() {
            EntryKind::RegularFile => self.open_file(relative).and_then(|file| {
                (file.identity() == identity)
                    .then_some(CapabilityEntry::File(file))
                    .ok_or(FsError::IdentityChanged)
            }),
            EntryKind::Directory => self.open_directory(relative).and_then(|directory| {
                (directory.identity() == identity)
                    .then_some(CapabilityEntry::Directory(directory))
                    .ok_or(FsError::IdentityChanged)
            }),
        }
    }

    fn require_write(&self) -> Result<(), FsError> {
        self.access
            .permits_write()
            .then_some(())
            .ok_or(FsError::ReadOnly)
    }

    fn prepare_replacement(
        &self,
        replacement: Replacement,
    ) -> Result<PreparedReplacement, FsError> {
        validate_relative(&replacement.relative)?;
        let parent_relative = replacement
            .relative
            .parent()
            .unwrap_or_else(|| Path::new(""));
        let name = replacement
            .relative
            .file_name()
            .ok_or(FsError::InvalidPath)?
            .to_os_string();
        validate_name(&name)?;
        let parent = self.lease_directory(parent_relative, self.access, RootCreation::Existing)?;
        match (
            replacement.expected,
            platform_entry_identity(&parent.root.directory, Path::new(&name)),
        ) {
            (Some(expected), Ok(actual)) if expected != actual => {
                return Err(FsError::ReplacementConflict);
            }
            (Some(_), Err(error)) if is_missing(&error) => {
                return Err(FsError::ReplacementConflict);
            }
            (None, Ok(actual)) if actual.kind() != EntryKind::RegularFile => {
                return Err(FsError::UnsupportedEntryType);
            }
            (_, Err(error)) if !is_missing(&error) => return Err(error),
            _ => {}
        }
        Ok(PreparedReplacement {
            replacement,
            parent,
            name: name.clone(),
            temporary: unique_sibling(&name, "replace"),
            backup: None,
        })
    }
}

impl CapabilityDirectory {
    /// Open a regular file relative to this directory, anchored at the
    /// directory's retained lease.
    pub(crate) fn open_file(&self, relative: &Path) -> Result<CapabilityFile, FsError> {
        self.lease.open_file(&self.relative.join(relative))
    }

    pub(crate) fn display_path(&self) -> PathBuf {
        self.lease.display_root().join(&self.relative)
    }

    pub(crate) fn authority_path(&self) -> PathBuf {
        self.lease.authority_root().join(&self.relative)
    }

    pub(crate) fn into_root_lease(self) -> Result<CapabilityRootLease, FsError> {
        let display_root =
            AbsolutePath::parse(self.display_path()).map_err(|_| FsError::InvalidPath)?;
        directory_into_root_lease(self, display_root)
    }

    pub(crate) const fn identity(&self) -> FileIdentity {
        self.identity
    }

    /// Removes this exact captured directory through its retained parent.
    /// A different entry subsequently installed at the same name is preserved.
    pub(crate) fn remove_tree_expected(&self) -> Result<(), FsError> {
        self.lease
            .remove_tree_expected(&self.relative, self.identity)
    }

    pub(crate) fn open_entry_exact(&self, relative: &Path) -> Result<CapabilityEntry, FsError> {
        validate_relative(relative)?;
        self.lease.open_entry(&self.relative.join(relative))
    }

    pub(crate) fn read_dir_names(&self) -> Result<Vec<OsString>, FsError> {
        let mut names = self
            .directory
            .entries()
            .map_err(|error| {
                self.lease.annotate_entry(
                    &self.relative,
                    FsError::from_io(FsOperation::Enumerate, &error),
                )
            })?
            .map(|entry| {
                entry.map(|entry| entry.file_name()).map_err(|error| {
                    self.lease.annotate_entry(
                        &self.relative,
                        FsError::from_io(FsOperation::Enumerate, &error),
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        for name in &names {
            validate_name(name)?;
        }
        names.sort();
        Ok(names)
    }

    pub(crate) fn read_dir_names_rejecting_unsupported(&self) -> Result<Vec<OsString>, FsError> {
        let names = self.read_dir_names()?;
        for name in &names {
            platform_entry_identity(&self.directory, Path::new(name))
                .map_err(|error| self.lease.annotate_entry(&self.relative.join(name), error))?;
        }
        Ok(names)
    }

    pub(crate) fn read_dir(self) -> Result<CapabilityReadDir, FsError> {
        let mut entries = Vec::new();
        for entry in self.directory.entries().map_err(|error| {
            self.lease.annotate_entry(
                &self.relative,
                FsError::from_io(FsOperation::Enumerate, &error),
            )
        })? {
            let entry = entry.map_err(|error| {
                self.lease.annotate_entry(
                    &self.relative,
                    FsError::from_io(FsOperation::Enumerate, &error),
                )
            })?;
            let name = entry.file_name();
            validate_name(&name)?;
            let child = self.relative.join(&name);
            match platform_entry_identity(&self.directory, Path::new(&name)) {
                Ok(identity) => entries.push((name, identity)),
                Err(FsError::UnsupportedEntryType) => continue,
                Err(error) if is_link_blocked(&error) => continue,
                Err(error) => return Err(self.lease.annotate_entry(&child, error)),
            }
        }
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(CapabilityReadDir {
            parent: self,
            cursor: PlatformDirectoryCursor { entries, next: 0 },
        })
    }
    pub(crate) fn read_dir_rejecting_unsupported(self) -> Result<CapabilityReadDir, FsError> {
        let mut entries = Vec::new();
        for entry in self.directory.entries().map_err(|error| {
            self.lease.annotate_entry(
                &self.relative,
                FsError::from_io(FsOperation::Enumerate, &error),
            )
        })? {
            let entry = entry.map_err(|error| {
                self.lease.annotate_entry(
                    &self.relative,
                    FsError::from_io(FsOperation::Enumerate, &error),
                )
            })?;
            let name = entry.file_name();
            validate_name(&name)?;
            let child = self.relative.join(&name);
            let identity = platform_entry_identity(&self.directory, Path::new(&name))
                .map_err(|error| self.lease.annotate_entry(&child, error))?;
            entries.push((name, identity));
        }
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(CapabilityReadDir {
            parent: self,
            cursor: PlatformDirectoryCursor { entries, next: 0 },
        })
    }
}


impl ManagedCapabilityFile {
    pub(crate) fn resolved_path(&self) -> &Path {
        &self.resolved_path
    }

    pub(crate) const fn followed_link(&self) -> bool {
        self.followed_link
    }

    pub(crate) fn into_file(self) -> CapabilityFile {
        self.file
    }

    pub(crate) fn read_bounded(&self, max_bytes: u64) -> Result<Vec<u8>, FsError> {
        self.file.read_bounded(max_bytes)
    }
}

impl ManagedOpenedEntry {
    pub(crate) fn resolved_path(&self) -> &Path {
        &self.resolved_path
    }

    pub(crate) const fn followed_link(&self) -> bool {
        self.followed_link
    }

    pub(crate) fn into_entry(self) -> CapabilityEntry {
        self.entry
    }
}

impl CapabilityFile {
    pub(crate) fn display_path(&self) -> PathBuf {
        self.lease.display_root().join(&self.relative)
    }

    pub(crate) fn authority_path(&self) -> PathBuf {
        self.lease.authority_root().join(&self.relative)
    }

    pub(crate) const fn identity(&self) -> FileIdentity {
        self.identity
    }

    pub(crate) fn metadata(&self) -> Result<FsMetadata, FsError> {
        let metadata = self
            .file
            .metadata()
            .map_err(|error| FsError::from_io(FsOperation::OpenEntry, &error))?;
        let identity = platform_file_identity(&self.file)?;
        if identity != self.identity {
            return Err(FsError::IdentityChanged);
        }
        metadata_from_std(identity, &metadata)
    }

    pub(crate) fn read_bounded(&self, max_bytes: u64) -> Result<Vec<u8>, FsError> {
        let metadata = self.metadata()?;
        if metadata.len > max_bytes {
            return Err(FsError::ReadLimitExceeded { limit: max_bytes });
        }
        let mut file = self
            .file
            .try_clone()
            .map_err(|error| FsError::from_io(FsOperation::Read, &error))?;
        file.seek(SeekFrom::Start(0))
            .map_err(|error| FsError::from_io(FsOperation::Read, &error))?;
        let mut bytes = Vec::with_capacity(
            usize::try_from(metadata.len)
                .unwrap_or(usize::MAX)
                .min(64 * 1024),
        );
        file.take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| FsError::from_io(FsOperation::Read, &error))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
            return Err(FsError::ReadLimitExceeded { limit: max_bytes });
        }
        Ok(bytes)
    }

    pub(crate) fn try_clone_std(&self) -> io::Result<File> {
        self.file.try_clone()
    }

    pub(crate) fn public_identity(&self) -> SessionFileIdentity {
        SessionFileIdentity(self.identity)
    }

    pub(crate) fn authority(&self) -> (CapabilityRootLease, PathBuf) {
        (self.lease.clone(), self.relative.clone())
    }
}
impl CapabilityFile {
    pub(crate) fn duplicate_for_spawn_executable(
        &self,
    ) -> Result<CapabilityExecutableSource, FsError> {
        self.metadata()?;
        let file = self
            .file
            .try_clone()
            .map_err(|error| FsError::from_io(FsOperation::DuplicateForSpawn, &error))?;
        if platform_file_identity(&file)? != self.identity {
            return Err(FsError::IdentityChanged);
        }
        let path = AbsolutePath::parse(self.display_path()).map_err(|_| FsError::InvalidPath)?;
        Ok(CapabilityExecutableSource {
            fs: self.lease.fs.clone(),
            path,
            file,
            identity: self.identity,
        })
    }
}

impl CapabilityWritableFile {
    pub(crate) fn write_all(&mut self, bytes: &[u8]) -> Result<(), FsError> {
        self.file
            .as_mut()
            .ok_or(FsError::IdentityChanged)?
            .write_all(bytes)
            .map_err(|error| FsError::from_io(FsOperation::Write, &error))
    }

    pub(crate) fn sync_all(&mut self) -> Result<(), FsError> {
        self.file
            .as_ref()
            .ok_or(FsError::IdentityChanged)?
            .sync_all()
            .map_err(|error| FsError::from_io(FsOperation::Sync, &error))
    }

    pub(crate) fn finish(mut self) -> Result<CapabilityFile, FsError> {
        self.sync_all()?;
        let file = self.file.take().ok_or(FsError::IdentityChanged)?;
        if platform_file_identity(&file)? != self.identity {
            return Err(FsError::IdentityChanged);
        }
        self.finished = true;
        Ok(CapabilityFile {
            lease: self.lease.clone(),
            relative: self.relative.clone(),
            identity: self.identity,
            file,
        })
    }
}

impl Drop for CapabilityWritableFile {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.file.take();
        if platform_entry_identity(&self.lease.root.directory, &self.relative)
            .is_ok_and(|identity| identity == self.identity)
        {
            let _ = self.lease.root.directory.remove_file(&self.relative);
            let _ = platform_sync_directory(&self.lease.root.directory);
        }
    }
}

impl CapabilityDirEntry {
    pub(crate) fn name(&self) -> &OsStr {
        &self.name
    }

    pub(crate) const fn entry(&self) -> &CapabilityEntry {
        &self.entry
    }

    pub(crate) fn into_entry(self) -> CapabilityEntry {
        self.entry
    }
}

impl CapabilityEntry {
    pub(crate) const fn identity(&self) -> FileIdentity {
        match self {
            Self::File(file) => file.identity(),
            Self::Directory(directory) => directory.identity(),
        }
    }

    pub(crate) const fn kind(&self) -> EntryKind {
        self.identity().kind()
    }

    pub(crate) fn display_path(&self) -> PathBuf {
        match self {
            Self::File(file) => file.display_path(),
            Self::Directory(directory) => directory.display_path(),
        }
    }
}

impl CapabilityReadDir {
    pub(crate) fn next(&mut self) -> Result<Option<CapabilityDirEntry>, FsError> {
        let Some((name, enumerated)) = self.cursor.entries.get(self.cursor.next).cloned() else {
            return Ok(None);
        };
        self.cursor.next += 1;
        let relative = self.parent.relative.join(&name);
        let entry = match enumerated.kind() {
            EntryKind::RegularFile => {
                CapabilityEntry::File(self.parent.lease.open_file(&relative)?)
            }
            EntryKind::Directory => {
                CapabilityEntry::Directory(self.parent.lease.open_directory(&relative)?)
            }
        };
        if entry.identity() != enumerated {
            return Err(FsError::IdentityChanged);
        }
        Ok(Some(CapabilityDirEntry { name, entry }))
    }
}

impl CapabilityWorkingDirectorySource {
    pub(crate) fn canonical_working_directory(&self) -> (&AbsolutePath, FileIdentity) {
        (&self.path, self.identity)
    }

    #[cfg(unix)]
    pub(crate) fn into_retained_working_directory(
        self,
        capabilities: &Arc<CapabilityFs>,
    ) -> Result<pi_sandbox::RetainedWorkingDirectory, FsError> {
        use std::os::fd::OwnedFd;

        let directory = Dir::from_std_file(
            self.directory
                .try_clone()
                .map_err(|error| FsError::from_io(FsOperation::DuplicateForSpawn, &error))?,
        );
        let live = platform_open_directory(&self.validation_root.directory, &self.relative)?;
        if !Arc::ptr_eq(&self.fs, capabilities)
            || self.identity.kind() != EntryKind::Directory
            || platform_directory_identity(&directory)? != self.identity
            || platform_directory_identity(&live)? != self.identity
        {
            return Err(FsError::IdentityChanged);
        }
        pi_sandbox::RetainedWorkingDirectory::new(OwnedFd::from(self.directory), self.path)
            .map_err(|_| FsError::UnsupportedEntryType)
    }
}

impl CapabilityExecutableSource {
    pub(crate) fn canonical_executable(&self) -> (&AbsolutePath, FileIdentity) {
        (&self.path, self.identity)
    }

    #[cfg(unix)]
    pub(crate) fn into_retained_executable(
        self,
        capabilities: &Arc<CapabilityFs>,
    ) -> Result<pi_sandbox::RetainedExecutable, FsError> {
        use std::os::fd::OwnedFd;

        if !Arc::ptr_eq(&self.fs, capabilities)
            || self.identity.kind() != EntryKind::RegularFile
            || platform_file_identity(&self.file)? != self.identity
        {
            return Err(FsError::IdentityChanged);
        }
        pi_sandbox::RetainedExecutable::new(OwnedFd::from(self.file), self.path)
            .map_err(|_| FsError::UnsupportedEntryType)
    }
}

impl CapabilitySpawnSource {
    pub(crate) fn canonical_mount(
        &self,
    ) -> (
        CapabilityRootPurpose,
        CapabilityAccess,
        AbsolutePath,
        FileIdentity,
    ) {
        (
            self.purpose,
            self.access,
            self.display_root.clone(),
            self.identity,
        )
    }

    #[cfg(unix)]
    pub(crate) fn into_mount_source(
        self,
        capabilities: &Arc<CapabilityFs>,
    ) -> Result<pi_sandbox::MountSource, FsError> {
        use std::os::fd::OwnedFd;

        if !Arc::ptr_eq(&self.fs, capabilities)
            || self.purpose != CapabilityRootPurpose::WorkflowWorktrees
            || self.access != CapabilityAccess::PrivateReadWrite
        {
            return Err(FsError::OutsideCapability);
        }
        let directory = Dir::from_std_file(
            self.directory
                .try_clone()
                .map_err(|error| FsError::from_io(FsOperation::DuplicateForSpawn, &error))?,
        );
        if platform_directory_identity(&directory)? != self.identity {
            return Err(FsError::IdentityChanged);
        }
        pi_sandbox::MountSource::read_write_directory(
            OwnedFd::from(self.directory),
            self.display_root,
        )
        .map_err(|_| FsError::UnsupportedEntryType)
    }

    pub(crate) const fn purpose(&self) -> CapabilityRootPurpose {
        self.purpose
    }

    pub(crate) fn display_root(&self) -> &Path {
        self.display_root.as_path()
    }

    pub(crate) const fn identity(&self) -> FileIdentity {
        self.identity
    }

    pub(crate) fn try_clone_std(&self) -> io::Result<File> {
        self.directory.try_clone()
    }
}

impl PreparedReplacement {
    fn stage(&mut self) -> Result<(), FsError> {
        let mut writable = self
            .parent
            .create_new(Path::new(&self.temporary), self.replacement.mode)?;
        writable.write_all(&self.replacement.bytes)?;
        let _ = writable.finish()?;
        Ok(())
    }

    fn commit(&mut self) -> Result<(), FsError> {
        match platform_entry_identity(&self.parent.root.directory, Path::new(&self.name)) {
            Ok(actual) => {
                if self
                    .replacement
                    .expected
                    .is_some_and(|expected| expected != actual)
                {
                    return Err(FsError::ReplacementConflict);
                }
                let backup = unique_sibling(&self.name, "backup");
                self.parent
                    .rename(Path::new(&self.name), Path::new(&backup))?;
                self.backup = Some(backup);
            }
            Err(error) if is_missing(&error) && self.replacement.expected.is_none() => {}
            Err(error) if is_missing(&error) => return Err(FsError::ReplacementConflict),
            Err(error) => return Err(error),
        }
        self.parent
            .rename(Path::new(&self.temporary), Path::new(&self.name))
    }

    fn restore(&mut self) -> Result<(), FsError> {
        if let Some(backup) = self.backup.take() {
            match platform_entry_identity(&self.parent.root.directory, Path::new(&self.name)) {
                Ok(_) => self.parent.remove_file(Path::new(&self.name))?,
                Err(error) if is_missing(&error) => {}
                Err(error) => return Err(error),
            }
            self.parent
                .rename(Path::new(&backup), Path::new(&self.name))
        } else {
            match self.parent.remove_file(Path::new(&self.name)) {
                Ok(()) => Ok(()),
                Err(error) if is_missing(&error) => Ok(()),
                Err(error) => Err(error),
            }
        }
    }

    fn installed_identity(&self) -> Result<FileIdentity, FsError> {
        self.parent
            .open_file(Path::new(&self.name))
            .map(|file| file.identity())
    }

    fn finish(&mut self) -> Result<(), FsError> {
        if let Some(backup) = self.backup.take() {
            self.parent.remove_file(Path::new(&backup))?;
        }
        platform_sync_directory(&self.parent.root.directory)
    }

    fn cleanup(&mut self) {
        let _ = self.parent.remove_file(Path::new(&self.temporary));
    }
}

impl PreparedStreamingReplacement {
    fn stage_from(
        &mut self,
        source: &CapabilityFile,
        aggregate_actual: &mut u64,
        aggregate_limit: u64,
        buffer: &mut StreamCopyBuffer,
    ) -> Result<(), FsError> {
        let before = source.metadata()?;
        if before.identity.kind() != EntryKind::RegularFile {
            return Err(FsError::UnsupportedEntryType);
        }
        if before.len > aggregate_limit.saturating_sub(*aggregate_actual) {
            return Err(FsError::ReadLimitExceeded {
                limit: aggregate_limit,
            });
        }
        let relative = Path::new(&self.temporary);
        let mut writable = self.parent.create_new(relative, FileMode::Private)?;
        self.temporary_identity = Some(writable.identity);
        let result = (|| {
            let mut input = source
                .file
                .try_clone()
                .map_err(|error| FsError::from_io(FsOperation::Read, &error))?;
            input
                .seek(SeekFrom::Start(0))
                .map_err(|error| FsError::from_io(FsOperation::Read, &error))?;
            let mut file_actual = 0_u64;
            loop {
                let read = input
                    .read(&mut buffer.bytes)
                    .map_err(|error| FsError::from_io(FsOperation::Read, &error))?;
                if read == 0 {
                    break;
                }
                let read = u64::try_from(read).map_err(|_| FsError::IdentityChanged)?;
                if read > aggregate_limit.saturating_sub(*aggregate_actual) {
                    return Err(FsError::ReadLimitExceeded {
                        limit: aggregate_limit,
                    });
                }
                *aggregate_actual = aggregate_actual
                    .checked_add(read)
                    .ok_or(FsError::ReadLimitExceeded {
                        limit: aggregate_limit,
                    })?;
                file_actual = file_actual.checked_add(read).ok_or(FsError::IdentityChanged)?;
                writable.write_all(&buffer.bytes[..usize::try_from(read).map_err(|_| FsError::IdentityChanged)?])?;
            }
            if file_actual != before.len {
                return Err(FsError::IdentityChanged);
            }
            let after = source.metadata()?;
            if after != before {
                return Err(FsError::IdentityChanged);
            }
            apply_source_permissions(
                writable.file.as_ref().ok_or(FsError::IdentityChanged)?,
                before,
            )?;
            let finished = writable.finish()?;
            if finished.identity() != self.temporary_identity.ok_or(FsError::IdentityChanged)? {
                return Err(FsError::IdentityChanged);
            }
            Ok(())
        })();
        if result.is_err() {
            self.cleanup_temp();
        }
        result
    }

    fn commit(&mut self, expected: ReplacementTarget) -> Result<FileIdentity, FsError> {
        self.commit_with(expected, || Ok(()))
    }

    fn commit_with<F>(
        &mut self,
        expected: ReplacementTarget,
        after_backup: F,
    ) -> Result<FileIdentity, FsError>
    where
        F: FnOnce() -> Result<(), FsError>,
    {
        validate_replacement_target(&self.parent, &self.name, expected)?;
        let temporary_identity = self.temporary_identity.ok_or(FsError::IdentityChanged)?;
        match expected {
            ReplacementTarget::Absent => {
                self.parent
                    .root
                    .directory
                    .hard_link(
                        Path::new(&self.temporary),
                        &self.parent.root.directory,
                        Path::new(&self.name),
                    )
                    .map_err(|error| {
                        if error.kind() == io::ErrorKind::AlreadyExists {
                            FsError::ReplacementConflict
                        } else {
                            FsError::from_io(FsOperation::Link, &error)
                        }
                    })?;
                self.installed = true;
            }
            ReplacementTarget::ExactIdentity(expected_identity) => {
                let backup = unique_sibling(&self.name, "backup");
                self.parent
                    .root
                    .directory
                    .rename(
                        Path::new(&self.name),
                        &self.parent.root.directory,
                        Path::new(&backup),
                    )
                    .map_err(|error| FsError::from_io(FsOperation::Rename, &error))?;
                let moved = platform_entry_identity(&self.parent.root.directory, Path::new(&backup))?;
                self.backup = Some((backup.clone(), moved));
                if moved != expected_identity {
                    self.restore_backup()?;
                    return Err(FsError::ReplacementConflict);
                }
                after_backup()?;
                if let Err(error) = self.parent.root.directory.hard_link(
                    Path::new(&self.temporary),
                    &self.parent.root.directory,
                    Path::new(&self.name),
                ) {
                    let conflict = error.kind() == io::ErrorKind::AlreadyExists;
                    let destination_missing = matches!(
                        &platform_entry_identity(
                            &self.parent.root.directory,
                            Path::new(&self.name),
                        ),
                        Err(error) if is_missing(error)
                    );
                    if !conflict || destination_missing {
                        self.restore_backup()?;
                    }
                    return Err(if conflict {
                        FsError::ReplacementConflict
                    } else {
                        FsError::from_io(FsOperation::Link, &error)
                    });
                }
                self.installed = true;
            }
        }
        if platform_entry_identity(&self.parent.root.directory, Path::new(&self.name))?
            != temporary_identity
        {
            self.rollback()?;
            return Err(FsError::IdentityChanged);
        }
        if let Err(error) = platform_sync_directory(&self.parent.root.directory) {
            self.rollback()?;
            return Err(error);
        }
        self.committed = true;
        self.remove_temp_link()?;
        if let Some((backup, identity)) = self.backup.take() {
            self.parent.remove_file_expected(Path::new(&backup), identity)?;
        }
        platform_sync_directory(&self.parent.root.directory)?;
        Ok(temporary_identity)
    }

    fn rollback(&mut self) -> Result<(), FsError> {
        if self.installed {
            let expected = self.temporary_identity.ok_or(FsError::IdentityChanged)?;
            self.parent.remove_file_expected(Path::new(&self.name), expected)?;
            self.installed = false;
        }
        self.restore_backup()?;
        platform_sync_directory(&self.parent.root.directory)
    }

    fn restore_backup(&mut self) -> Result<(), FsError> {
        let Some((backup, identity)) = self.backup.as_ref() else {
            return Ok(());
        };
        if platform_entry_identity(&self.parent.root.directory, Path::new(backup))? != *identity {
            return Err(FsError::IdentityChanged);
        }
        self.parent
            .root
            .directory
            .hard_link(
                Path::new(backup),
                &self.parent.root.directory,
                Path::new(&self.name),
            )
            .map_err(|error| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    FsError::ReplacementConflict
                } else {
                    FsError::from_io(FsOperation::Link, &error)
                }
            })?;
        self.parent.remove_file_expected(Path::new(backup), *identity)?;
        self.backup = None;
        Ok(())
    }

    fn remove_temp_link(&mut self) -> Result<(), FsError> {
        let Some(identity) = self.temporary_identity.take() else {
            return Ok(());
        };
        match platform_entry_identity(&self.parent.root.directory, Path::new(&self.temporary)) {
            Ok(actual) if actual == identity => self
                .parent
                .root
                .directory
                .remove_file(Path::new(&self.temporary))
                .map_err(|error| FsError::from_io(FsOperation::Remove, &error)),
            Ok(_) => Err(FsError::IdentityChanged),
            Err(error) if is_missing(&error) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn cleanup_temp(&mut self) {
        let _ = self.remove_temp_link();
    }
}

impl Drop for PreparedStreamingReplacement {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.rollback();
        }
        self.cleanup_temp();
    }
}

fn validate_replacement_target(
    parent: &CapabilityRootLease,
    name: &OsStr,
    expected: ReplacementTarget,
) -> Result<(), FsError> {
    match (expected, platform_entry_identity(&parent.root.directory, Path::new(name))) {
        (ReplacementTarget::Absent, Err(error)) if is_missing(&error) => Ok(()),
        (ReplacementTarget::Absent, Ok(_)) => Err(FsError::ReplacementConflict),
        (ReplacementTarget::ExactIdentity(expected), Ok(actual))
            if expected.kind() == EntryKind::RegularFile && actual == expected => Ok(()),
        (ReplacementTarget::ExactIdentity(_), Ok(_)) => Err(FsError::ReplacementConflict),
        (ReplacementTarget::ExactIdentity(_), Err(error)) if is_missing(&error) => {
            Err(FsError::ReplacementConflict)
        }
        (_, Err(error)) => Err(error),
    }
}

fn apply_source_permissions(file: &File, metadata: FsMetadata) -> Result<(), FsError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(metadata.mode))
            .map_err(|error| FsError::from_io(FsOperation::Write, &error))
    }
    #[cfg(windows)]
    {
        let mut permissions = file
            .metadata()
            .map_err(|error| FsError::from_io(FsOperation::OpenEntry, &error))?
            .permissions();
        permissions.set_readonly(metadata.readonly);
        file.set_permissions(permissions)
            .map_err(|error| FsError::from_io(FsOperation::Write, &error))
    }
}

fn validate_relative(path: &Path) -> Result<(), FsError> {
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(FsError::InvalidPath);
    }
    Ok(())
}

fn validate_name(name: &OsStr) -> Result<(), FsError> {
    let path = Path::new(name);
    if path.components().count() == 1
        && matches!(path.components().next(), Some(Component::Normal(_)))
        && !matches!(name.to_str(), Some(".") | Some(".."))
    {
        Ok(())
    } else {
        Err(FsError::InvalidPath)
    }
}

fn normalize_key(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(windows)]
    {
        path.to_string_lossy().to_lowercase().into_bytes()
    }
}

fn metadata_from_cap(
    identity: FileIdentity,
    metadata: &cap_std::fs::Metadata,
) -> Result<FsMetadata, FsError> {
    let modified = metadata
        .modified()
        .map_err(|error| FsError::from_io(FsOperation::OpenEntry, &error))?
        .into_std();
    Ok(FsMetadata {
        identity,
        len: metadata.len(),
        modified_ns: system_time_ns(modified),
        readonly: metadata.permissions().readonly(),
        #[cfg(unix)]
        mode: {
            use cap_std::fs::PermissionsExt;
            metadata.permissions().mode()
        },
    })
}

fn metadata_from_std(
    identity: FileIdentity,
    metadata: &std::fs::Metadata,
) -> Result<FsMetadata, FsError> {
    let modified = metadata
        .modified()
        .map_err(|error| FsError::from_io(FsOperation::OpenEntry, &error))?;
    Ok(FsMetadata {
        identity,
        len: metadata.len(),
        modified_ns: system_time_ns(modified),
        readonly: metadata.permissions().readonly(),
        #[cfg(unix)]
        mode: {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode()
        },
    })
}

fn system_time_ns(time: std::time::SystemTime) -> i128 {
    match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => i128::try_from(duration.as_nanos()).unwrap_or(i128::MAX),
        Err(error) => -i128::try_from(error.duration().as_nanos()).unwrap_or(i128::MAX),
    }
}

fn unique_sibling(name: &OsStr, label: &str) -> OsString {
    let mut temporary = OsString::from(".rpi-");
    temporary.push(label);
    temporary.push("-");
    temporary.push(uuid::Uuid::new_v4().simple().to_string());
    temporary.push("-");
    temporary.push(name);
    temporary
}

fn is_missing(error: &FsError) -> bool {
    matches!(
        error,
        FsError::System {
            kind: io::ErrorKind::NotFound | io::ErrorKind::NotADirectory,
            ..
        }
    )
}

fn remove_opened_tree(directory: CapabilityDirectory) -> Result<(), FsError> {
    // Typed enumeration skips symlinks and sockets; tree cleanup must visit every name.
    let lease = directory.lease.clone();
    let parent_relative = directory.relative.clone();
    let names = directory.read_dir_names()?;
    for name in names {
        let child_relative = parent_relative.join(&name);
        match platform_entry_identity(&directory.directory, Path::new(&name)) {
            Ok(identity) => match identity.kind() {
                EntryKind::RegularFile => {
                    lease.remove_file(&child_relative)?;
                }
                EntryKind::Directory => {
                    let child = lease.open_directory(&child_relative)?;
                    remove_opened_tree(child)?;
                    lease.remove_directory(&child_relative)?;
                }
            },
            Err(error)
                if is_link_blocked(&error) || matches!(error, FsError::UnsupportedEntryType) =>
            {
                lease.remove_nondirectory_entry(&child_relative)?;
            }
            Err(error) => return Err(lease.annotate_entry(&child_relative, error)),
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn platform_open_root(path: &Path, creation: RootCreation, mode: FileMode) -> Result<Dir, FsError> {
    super::linux::open_root(path, creation, mode)
}
#[cfg(target_os = "macos")]
fn platform_open_root(path: &Path, creation: RootCreation, mode: FileMode) -> Result<Dir, FsError> {
    super::macos::open_root(path, creation, mode)
}
#[cfg(target_os = "windows")]
fn platform_open_root(path: &Path, creation: RootCreation, mode: FileMode) -> Result<Dir, FsError> {
    let _ = mode;
    if matches!(creation, RootCreation::CreateDirectory) {
        Dir::create_ambient_dir_all(path, ambient_authority())
            .map_err(|error| FsError::from_io(FsOperation::CreateDirectory, &error))?;
    }
    Dir::open_ambient_dir(path, ambient_authority())
        .map_err(|error| FsError::from_io(FsOperation::OpenRoot, &error))
}

#[cfg(target_os = "linux")]
fn platform_open_directory(directory: &Dir, relative: &Path) -> Result<Dir, FsError> {
    super::linux::open_directory(directory, relative)
}
#[cfg(target_os = "macos")]
fn platform_open_directory(directory: &Dir, relative: &Path) -> Result<Dir, FsError> {
    super::macos::open_directory(directory, relative)
}
#[cfg(target_os = "windows")]
fn platform_open_directory(directory: &Dir, relative: &Path) -> Result<Dir, FsError> {
    directory
        .open_dir_nofollow(relative)
        .map_err(|error| FsError::from_io(FsOperation::OpenEntry, &error))
}

#[cfg(target_os = "linux")]
fn platform_create_dir_all(
    directory: &Dir,
    relative: &Path,
    mode: FileMode,
) -> Result<(), FsError> {
    super::linux::create_dir_all(directory, relative, mode)
}
#[cfg(target_os = "macos")]
fn platform_create_dir_all(
    directory: &Dir,
    relative: &Path,
    mode: FileMode,
) -> Result<(), FsError> {
    super::macos::create_dir_all(directory, relative, mode)
}
#[cfg(target_os = "windows")]
fn platform_create_dir_all(
    directory: &Dir,
    relative: &Path,
    _mode: FileMode,
) -> Result<(), FsError> {
    directory
        .create_dir_all(relative)
        .map_err(|error| FsError::from_io(FsOperation::CreateDirectory, &error))
}

#[cfg(target_os = "linux")]
fn platform_create_directory_exclusive(
    directory: &Dir,
    relative: &Path,
    mode: FileMode,
) -> Result<(), FsError> {
    super::linux::create_directory_exclusive(directory, relative, mode)
}
#[cfg(target_os = "macos")]
fn platform_create_directory_exclusive(
    directory: &Dir,
    relative: &Path,
    mode: FileMode,
) -> Result<(), FsError> {
    super::macos::create_directory_exclusive(directory, relative, mode)
}
#[cfg(target_os = "windows")]
fn platform_create_directory_exclusive(
    directory: &Dir,
    relative: &Path,
    _mode: FileMode,
) -> Result<(), FsError> {
    directory
        .create_dir(relative)
        .map_err(|error| FsError::from_io(FsOperation::CreateDirectory, &error))
}

#[cfg(target_os = "linux")]
fn platform_open_read_file(directory: &Dir, relative: &Path) -> Result<File, FsError> {
    super::linux::open_read_file(directory, relative)
}
#[cfg(target_os = "linux")]
fn platform_open_read_write_file(directory: &Dir, relative: &Path) -> Result<File, FsError> {
    super::linux::open_read_write_file(directory, relative)
}
#[cfg(target_os = "macos")]
fn platform_open_read_file(directory: &Dir, relative: &Path) -> Result<File, FsError> {
    super::macos::open_read_file(directory, relative)
}
#[cfg(target_os = "macos")]
fn platform_open_read_write_file(directory: &Dir, relative: &Path) -> Result<File, FsError> {
    super::macos::open_read_write_file(directory, relative)
}
#[cfg(target_os = "windows")]
fn platform_open_read_file(directory: &Dir, relative: &Path) -> Result<File, FsError> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    directory
        .open_with(relative, &options)
        .map(cap_std::fs::File::into_std)
        .map_err(|error| FsError::from_io(FsOperation::OpenEntry, &error))
}
#[cfg(target_os = "windows")]
fn platform_open_read_write_file(directory: &Dir, relative: &Path) -> Result<File, FsError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).follow(FollowSymlinks::No);
    directory
        .open_with(relative, &options)
        .map(cap_std::fs::File::into_std)
        .map_err(|error| FsError::from_io(FsOperation::OpenEntry, &error))
}

#[cfg(target_os = "linux")]
fn platform_create_new_file(
    directory: &Dir,
    relative: &Path,
    mode: FileMode,
) -> Result<File, FsError> {
    super::linux::create_new_file(directory, relative, mode)
}
#[cfg(target_os = "macos")]
fn platform_create_new_file(
    directory: &Dir,
    relative: &Path,
    mode: FileMode,
) -> Result<File, FsError> {
    super::macos::create_new_file(directory, relative, mode)
}
#[cfg(target_os = "windows")]
fn platform_create_new_file(
    directory: &Dir,
    relative: &Path,
    _mode: FileMode,
) -> Result<File, FsError> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    directory
        .open_with(relative, &options)
        .map(cap_std::fs::File::into_std)
        .map_err(|error| FsError::from_io(FsOperation::CreateFile, &error))
}

#[cfg(target_os = "linux")]
fn platform_directory_identity(directory: &Dir) -> Result<FileIdentity, FsError> {
    super::linux::directory_identity(directory)
}
#[cfg(target_os = "macos")]
fn platform_directory_identity(directory: &Dir) -> Result<FileIdentity, FsError> {
    super::macos::directory_identity(directory)
}
#[cfg(target_os = "windows")]
fn platform_directory_identity(directory: &Dir) -> Result<FileIdentity, FsError> {
    super::windows::directory_identity(directory)
}

#[cfg(target_os = "linux")]
fn platform_file_identity(file: &File) -> Result<FileIdentity, FsError> {
    super::linux::file_identity(file)
}
#[cfg(target_os = "macos")]
fn platform_file_identity(file: &File) -> Result<FileIdentity, FsError> {
    super::macos::file_identity(file)
}
#[cfg(target_os = "windows")]
fn platform_file_identity(file: &File) -> Result<FileIdentity, FsError> {
    super::windows::file_identity(file)
}

#[cfg(target_os = "linux")]
fn platform_entry_identity(directory: &Dir, relative: &Path) -> Result<FileIdentity, FsError> {
    super::linux::entry_identity(directory, relative)
}
#[cfg(target_os = "macos")]
fn platform_entry_identity(directory: &Dir, relative: &Path) -> Result<FileIdentity, FsError> {
    super::macos::entry_identity(directory, relative)
}
#[cfg(target_os = "windows")]
fn platform_entry_identity(directory: &Dir, relative: &Path) -> Result<FileIdentity, FsError> {
    super::windows::entry_identity(directory, relative)
}

#[cfg(target_os = "linux")]
fn platform_sync_directory(directory: &Dir) -> Result<(), FsError> {
    super::linux::sync_directory(directory)
}
#[cfg(target_os = "macos")]
fn platform_sync_directory(directory: &Dir) -> Result<(), FsError> {
    super::macos::sync_directory(directory)
}
#[cfg(target_os = "windows")]
fn platform_sync_directory(_directory: &Dir) -> Result<(), FsError> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn platform_entry_is_symlink(directory: &Dir, relative: &Path) -> Result<bool, FsError> {
    super::linux::entry_is_symlink(directory, relative)
}
#[cfg(target_os = "macos")]
fn platform_entry_is_symlink(directory: &Dir, relative: &Path) -> Result<bool, FsError> {
    super::macos::entry_is_symlink(directory, relative)
}
#[cfg(target_os = "windows")]
fn platform_entry_is_symlink(directory: &Dir, relative: &Path) -> Result<bool, FsError> {
    super::windows::entry_is_symlink(directory, relative)
}

#[cfg(target_os = "linux")]
fn platform_read_link(directory: &Dir, relative: &Path) -> Result<PathBuf, FsError> {
    super::linux::read_link(directory, relative)
}
#[cfg(target_os = "macos")]
fn platform_read_link(directory: &Dir, relative: &Path) -> Result<PathBuf, FsError> {
    super::macos::read_link(directory, relative)
}
#[cfg(target_os = "windows")]
fn platform_read_link(directory: &Dir, relative: &Path) -> Result<PathBuf, FsError> {
    super::windows::read_link(directory, relative)
}

#[cfg(target_os = "linux")]
fn platform_followed_entry_identity(
    directory: &Dir,
    relative: &Path,
) -> Result<FileIdentity, FsError> {
    super::linux::followed_entry_identity(directory, relative)
}
#[cfg(target_os = "macos")]
fn platform_followed_entry_identity(
    directory: &Dir,
    relative: &Path,
) -> Result<FileIdentity, FsError> {
    super::macos::followed_entry_identity(directory, relative)
}
#[cfg(target_os = "windows")]
fn platform_followed_entry_identity(
    directory: &Dir,
    relative: &Path,
) -> Result<FileIdentity, FsError> {
    super::windows::followed_entry_identity(directory, relative)
}

#[cfg(target_os = "linux")]
fn platform_managed_link_identity(
    directory: &Dir,
    relative: &Path,
) -> Result<ManagedLinkIdentity, FsError> {
    super::linux::managed_link_identity(directory, relative)
}
#[cfg(target_os = "macos")]
fn platform_managed_link_identity(
    directory: &Dir,
    relative: &Path,
) -> Result<ManagedLinkIdentity, FsError> {
    super::macos::managed_link_identity(directory, relative)
}
#[cfg(target_os = "windows")]
fn platform_managed_link_identity(
    directory: &Dir,
    relative: &Path,
) -> Result<ManagedLinkIdentity, FsError> {
    super::windows::managed_link_identity(directory, relative)
}

/// Whether a strict open failed specifically because some path component is a
/// symbolic link (`ELOOP`), i.e. a managed-config link the read boundary may
/// resolve. Any other failure is reported unchanged. On Windows reparse
/// points surface through the strict open path and managed-link resolution is
/// unsupported, so this is always false there.
fn link_blocks(error: &FsError) -> bool {
    #[cfg(unix)]
    {
        matches!(
            error,
            FsError::System {
                raw_os_error: Some(code),
                ..
            } if *code == rustix::io::Errno::LOOP.raw_os_error()
        )
    }
    #[cfg(windows)]
    {
        false
    }
}

/// Public visibility for [`link_blocks`]: a strict open failed because some
/// path component is a symbolic link.
pub(crate) fn is_link_blocked(error: &FsError) -> bool {
    link_blocks(error)
}

/// Resolve `.`/`..` components lexically (no filesystem access), so the result
/// is a normalized absolute path the capability layer can open without
/// following symlinks.
fn lexically_normalize_absolute(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.file_name().is_some() {
                    normalized.pop();
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_directory_open_root_is_idempotent_and_nested() {
        let tmp = tempfile::tempdir().expect("temp root");
        let capabilities = CapabilityFs::new().expect("capabilities");
        let nested = tmp
            .path()
            .join("session")
            .join("sandbox")
            .join("1")
            .join("home");
        let absolute = AbsolutePath::parse(nested.clone()).expect("absolute");

        // Repeated CreateDirectory opens of the SAME nested path must be
        // idempotent (stat/`mkdir -p` semantics): the capability layer treats
        // an already-existing directory as success and never surfaces
        // AlreadyExists (a previous mkdirat EEXIST regression).
        for _ in 0..2 {
            let lease = capabilities
                .open_root(
                    CapabilityRootPurpose::SessionStore,
                    absolute.clone(),
                    CapabilityAccess::PrivateReadWrite,
                    RootCreation::CreateDirectory,
                )
                .expect("repeated create_directory must be idempotent");
            assert!(lease.display_root().starts_with(tmp.path()));
        }
        assert!(nested.is_dir(), "nested session root must exist");
    }

    #[cfg(unix)]
    #[test]
    fn create_directory_open_root_rejects_symlinked_entry() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("temp root");
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).expect("real dir");
        let link = tmp.path().join("session");
        symlink(&real, &link).expect("symlink");
        let absolute = AbsolutePath::parse(link).expect("absolute");

        // A pre-existing symlink at the created path must still be rejected
        // (NO_SYMLINKS) rather than silently accepted as the created
        // directory — create-dir idempotency never sacrifices link rejection.
        let capabilities = CapabilityFs::new().expect("capabilities");
        let err = capabilities
            .open_root(
                CapabilityRootPurpose::SessionStore,
                absolute,
                CapabilityAccess::PrivateReadWrite,
                RootCreation::CreateDirectory,
            )
            .expect_err("symlinked session root must be rejected");
        assert!(link_blocks(&err), "expected symlink rejection, got {err:?}");
    }

    #[test]
    fn exclusive_directory_creation_rejects_existing_leaf() {
        let root = tempfile::tempdir().expect("root");
        let published = root.path().join("published");
        std::fs::create_dir(&published).expect("published leaf");
        std::fs::write(published.join("SKILL.md"), b"winner").expect("published bytes");
        let lease = test_lease(root.path(), CapabilityAccess::PrivateReadWrite);

        let error = lease
            .create_directory_exclusive(Path::new("published"), CapabilityAccess::PrivateReadWrite)
            .expect_err("an existing leaf must never be claimed exclusively");
        assert!(
            matches!(
                error,
                FsError::System {
                    kind: std::io::ErrorKind::AlreadyExists,
                    ..
                }
            ),
            "expected AlreadyExists, got {error:?}"
        );
        assert_eq!(
            std::fs::read(published.join("SKILL.md")).expect("published bytes"),
            b"winner"
        );

        let created = lease
            .create_directory_exclusive(Path::new("fresh"), CapabilityAccess::PrivateReadWrite)
            .expect("missing leaf is created");
        assert_eq!(created.display_root(), root.path().join("fresh"));
        assert!(root.path().join("fresh").is_dir());
        assert!(matches!(
            lease
                .create_directory_exclusive(Path::new("fresh"), CapabilityAccess::PrivateReadWrite)
                .expect_err("a second exclusive create must fail"),
            FsError::System {
                kind: std::io::ErrorKind::AlreadyExists,
                ..
            }
        ));

        // The idempotent sibling keeps mkdir -p semantics: an existing leaf is
        // success and its children are left alone.
        lease
            .create_new_directory(Path::new("published"), CapabilityAccess::PrivateReadWrite)
            .expect("create_new_directory stays idempotent");
        assert_eq!(
            std::fs::read(published.join("SKILL.md")).expect("published bytes"),
            b"winner"
        );
    }

    #[cfg(unix)]
    #[test]
    fn exclusive_directory_creation_never_follows_leaf_symlink() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        let outside = tempfile::tempdir().expect("outside");
        symlink(outside.path(), root.path().join("linked")).expect("leaf symlink");
        let lease = test_lease(root.path(), CapabilityAccess::PrivateReadWrite);

        let error = lease
            .create_directory_exclusive(Path::new("linked"), CapabilityAccess::PrivateReadWrite)
            .expect_err("a symlink leaf must never be claimed exclusively");
        assert!(
            matches!(
                error,
                FsError::System {
                    kind: std::io::ErrorKind::AlreadyExists,
                    ..
                }
            ),
            "expected AlreadyExists, got {error:?}"
        );
        assert!(root.path().join("linked").is_symlink());
        assert_eq!(std::fs::read_dir(outside.path()).expect("outside entries").count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn remove_tree_unlinks_symlinks_and_sockets_without_following_targets() {
        use std::os::unix::fs::symlink;
        use std::os::unix::net::UnixListener;

        let root = tempfile::tempdir().expect("root");
        let profile = root.path().join("profile");
        let nested = profile.join("nested");
        std::fs::create_dir_all(&nested).expect("profile directories");
        std::fs::write(nested.join("Preferences"), b"profile").expect("profile file");
        let outside = root.path().join("outside");
        std::fs::write(&outside, b"preserve").expect("outside target");
        symlink(&outside, profile.join("SingletonLock")).expect("singleton symlink");
        let socket = UnixListener::bind(profile.join("SingletonSocket")).expect("singleton socket");

        let capabilities = CapabilityFs::new().expect("capabilities");
        let lease = capabilities
            .open_root(
                CapabilityRootPurpose::SessionStore,
                AbsolutePath::parse(root.path().to_path_buf()).expect("root path"),
                CapabilityAccess::PrivateReadWrite,
                RootCreation::Existing,
            )
            .expect("root lease");
        lease
            .remove_tree(Path::new("profile"))
            .expect("remove profile tree");

        drop(socket);
        assert!(!profile.exists(), "profile tree must be removed");
        assert_eq!(std::fs::read(outside).expect("outside target"), b"preserve");
    }

    /// Repeated acquisition must be idempotent for the SAME session/revision
    /// owner: a workflow child that is restored/relaunched, or a generation
    /// that is re-staged, calls `CapabilityRevisionRoots::acquire` again on
    /// revision roots this process already created. That must succeed and
    /// yield leases over the same concrete directories, never surfacing
    /// `CreateDirectory AlreadyExists`.
    #[test]
    fn revision_roots_acquire_is_idempotent_for_same_owner() {
        let tmp = tempfile::tempdir().expect("temp root");
        let session_dir = AbsolutePath::parse(tmp.path().join("session")).expect("session");
        let revision_root = session_dir.as_path().join("sandbox").join("1");
        let home = AbsolutePath::parse(revision_root.join("home")).expect("home");
        let tmp_root = AbsolutePath::parse(revision_root.join("tmp")).expect("tmp");
        let scratch_root = AbsolutePath::parse(revision_root.join("scratch")).expect("scratch");
        let capabilities = CapabilityFs::new().expect("capabilities");
        let first = CapabilityRevisionRoots::acquire(
            &capabilities,
            home.clone(),
            tmp_root.clone(),
            scratch_root.clone(),
        )
        .expect("initial revision root acquisition");
        let second = CapabilityRevisionRoots::acquire(
            &capabilities,
            home.clone(),
            tmp_root.clone(),
            scratch_root.clone(),
        )
        .expect("repeated revision root acquisition must be idempotent");
        // Both acquisitions resolve to the very same directories (same
        // display roots) created during the first acquire.
        assert_eq!(
            first.scratch_lease().display_root(),
            second.scratch_lease().display_root()
        );
        for leaf in ["home", "tmp", "scratch"] {
            assert!(
                revision_root.join(leaf).is_dir(),
                "revision root {leaf} must exist after idempotent re-acquire"
            );
        }
    }

    /// Hostile collision: a pre-existing non-directory or symlink at a
    /// revision root path must fail closed rather than be silently adopted.
    /// The `home` root is reached only through `sandbox/<revision>`, so we
    /// pre-seed the final `home` component as a symlink (and separately a
    /// regular file) to pin rejection.
    #[cfg(unix)]
    #[test]
    fn revision_roots_acquire_rejects_hostile_preexisting_root() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("temp root");
        let sessions = tmp.path().join("sessions");

        // Case 1: `home` is a symlink to a real directory outside.
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).expect("outside dir");
        let session_dir = AbsolutePath::parse(sessions.join("symlink")).expect("sess");
        std::fs::create_dir_all(session_dir.as_path()).expect("session dir");
        let root_dir = session_dir.as_path().join("sandbox").join("1");
        std::fs::create_dir_all(&root_dir).expect("revision dir");
        symlink(&outside, root_dir.join("home")).expect("hostile symlink");
        let home = AbsolutePath::parse(root_dir.join("home")).expect("home");
        let tmp_root = AbsolutePath::parse(root_dir.join("tmp")).expect("tmp");
        let scratch_root = AbsolutePath::parse(root_dir.join("scratch")).expect("scratch");
        let capabilities = CapabilityFs::new().expect("capabilities");
        let err =
            match CapabilityRevisionRoots::acquire(&capabilities, home, tmp_root, scratch_root) {
                Ok(_) => panic!("hostile symlinked root must be rejected"),
                Err(err) => err,
            };
        assert!(link_blocks(&err), "expected symlink rejection, got {err:?}");

        // Case 2: `home` is a regular file (not a directory).
        let session_dir = AbsolutePath::parse(sessions.join("file")).expect("sess");
        std::fs::create_dir_all(session_dir.as_path().join("sandbox").join("1"))
            .expect("revision dir");
        std::fs::write(
            session_dir.as_path().join("sandbox").join("1").join("home"),
            b"not a directory",
        )
        .expect("hostile file");
        let home =
            AbsolutePath::parse(session_dir.as_path().join("sandbox").join("1").join("home"))
                .expect("home");
        let tmp_root =
            AbsolutePath::parse(session_dir.as_path().join("sandbox").join("1").join("tmp"))
                .expect("tmp");
        let scratch_root = AbsolutePath::parse(
            session_dir
                .as_path()
                .join("sandbox")
                .join("1")
                .join("scratch"),
        )
        .expect("scratch");
        let err =
            match CapabilityRevisionRoots::acquire(&capabilities, home, tmp_root, scratch_root) {
                Ok(_) => panic!("hostile non-directory root must be rejected"),
                Err(err) => err,
            };
        assert!(
            matches!(err, FsError::UnsupportedEntryType | FsError::System { .. }),
            "expected non-directory rejection, got {err:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn managed_file_open_repeats_through_directory_and_file_links() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        let actual = root.path().join("actual");
        std::fs::create_dir_all(actual.join("core")).expect("actual core");
        std::fs::write(actual.join("skill-source.md"), b"managed-core").expect("skill body");
        symlink("../skill-source.md", actual.join("core/SKILL.md")).expect("file link");
        symlink(&actual, root.path().join("alias")).expect("directory link");
        let path = AbsolutePath::parse(root.path().join("alias/core/SKILL.md")).expect("path");
        let capabilities = CapabilityFs::new().expect("capabilities");

        for _ in 0..3 {
            let managed = capabilities
                .open_managed_read_file(CapabilityRootPurpose::ProjectResources, path.clone())
                .expect("repeated managed open");
            assert!(managed.followed_link());
            assert_eq!(managed.read_bounded(64).expect("read"), b"managed-core");
            assert_eq!(managed.resolved_path(), actual.join("skill-source.md"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn managed_file_open_rejects_swap_after_link_capture() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::write(&first, b"first").expect("first");
        std::fs::write(&second, b"second").expect("second");
        let link = root.path().join("SKILL.md");
        symlink(&first, &link).expect("link");
        let capabilities = CapabilityFs::new().expect("capabilities");
        let path = AbsolutePath::parse(link.clone()).expect("path");
        let mut swapped = false;
        let error = match open_managed_entry_with(
            &capabilities,
            CapabilityRootPurpose::ProjectResources,
            path.as_path(),
            &mut 0,
            &mut || {
                if swapped {
                    return;
                }
                std::fs::remove_file(&link).expect("remove link");
                symlink(&second, &link).expect("swap link");
                swapped = true;
            },
        ) {
            Ok(_) => panic!("link swap must be rejected"),
            Err(error) => error,
        };
        assert!(matches!(error, FsError::IdentityChanged));
    }

    #[cfg(unix)]
    #[test]
    fn managed_directory_open_rejects_swap_after_link_capture() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::create_dir_all(&first).expect("first directory");
        std::fs::create_dir_all(&second).expect("second directory");
        let link = root.path().join("personas");
        symlink(&first, &link).expect("link");
        let capabilities = CapabilityFs::new().expect("capabilities");
        let path = AbsolutePath::parse(link.clone()).expect("path");
        let mut swapped = false;
        let error = match open_managed_entry_with(
            &capabilities,
            CapabilityRootPurpose::AgentStore,
            path.as_path(),
            &mut 0,
            &mut || {
                if swapped {
                    return;
                }
                std::fs::remove_file(&link).expect("remove link");
                symlink(&second, &link).expect("swap link");
                swapped = true;
            },
        ) {
            Ok(_) => panic!("directory link swap must be rejected"),
            Err(error) => error,
        };
        assert!(matches!(error, FsError::IdentityChanged));
    }

    #[cfg(unix)]
    #[test]
    fn generic_replace_file_still_rejects_leaf_symlink() {
        use std::os::unix::fs::{MetadataExt, symlink};

        let root = tempfile::tempdir().expect("root");
        let target = root.path().join("target.json");
        let link = root.path().join("auth.json");
        std::fs::write(&target, b"old").expect("target");
        symlink("target.json", &link).expect("link");
        let link_before = std::fs::symlink_metadata(&link).expect("link metadata");
        let capabilities = CapabilityFs::new().expect("capabilities");
        let lease = capabilities
            .open_root(
                CapabilityRootPurpose::OperatorAuth,
                AbsolutePath::parse(root.path().to_path_buf()).expect("root path"),
                CapabilityAccess::PrivateReadWrite,
                RootCreation::Existing,
            )
            .expect("root lease");

        let error = lease
            .replace_file(Replacement {
                relative: PathBuf::from("auth.json"),
                expected: None,
                bytes: b"new".to_vec(),
                mode: FileMode::Private,
            })
            .expect_err("generic replacement must reject a leaf symlink");
        assert!(matches!(
            error,
            FsError::UnsupportedEntryType | FsError::System { .. }
        ));
        let link_after = std::fs::symlink_metadata(&link).expect("link after failure");
        assert!(link_after.file_type().is_symlink());
        assert_eq!(link_after.dev(), link_before.dev());
        assert_eq!(link_after.ino(), link_before.ino());
        assert_eq!(
            std::fs::read(&target).expect("target after failure"),
            b"old"
        );
    }

    #[cfg(unix)]
    #[test]
    fn live_leaf_link_replacement_rejects_link_swap_before_target_mutation() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        let first = root.path().join("first.json");
        let second = root.path().join("second.json");
        let link = root.path().join("auth.json");
        std::fs::write(&first, b"first").expect("first target");
        std::fs::write(&second, b"second").expect("second target");
        symlink("first.json", &link).expect("initial link");
        let capabilities = CapabilityFs::new().expect("capabilities");
        let lease = capabilities
            .open_root(
                CapabilityRootPurpose::OperatorAuth,
                AbsolutePath::parse(root.path().to_path_buf()).expect("root path"),
                CapabilityAccess::PrivateReadWrite,
                RootCreation::Existing,
            )
            .expect("root lease");

        let error = lease
            .replace_file_allowing_live_leaf_link_with(
                Replacement {
                    relative: PathBuf::from("auth.json"),
                    expected: None,
                    bytes: b"replacement".to_vec(),
                    mode: FileMode::Private,
                },
                || {
                    std::fs::remove_file(&link).expect("remove initial link");
                    symlink("second.json", &link).expect("redirect link");
                },
            )
            .expect_err("link swap must yield an identity error");
        assert!(matches!(error, FsError::IdentityChanged));
        assert_eq!(std::fs::read(&first).expect("first target"), b"first");
        assert_eq!(std::fs::read(&second).expect("second target"), b"second");
        assert_eq!(
            std::fs::read_link(&link).expect("redirected link"),
            Path::new("second.json")
        );
    }

    #[cfg(unix)]
    #[test]
    fn live_leaf_link_replacement_rejects_target_outside_retained_root() {
        use std::os::unix::fs::symlink;

        let outer = tempfile::tempdir().expect("outer");
        let root_path = outer.path().join("root");
        std::fs::create_dir(&root_path).expect("root directory");
        let outside = outer.path().join("outside.json");
        std::fs::write(&outside, b"outside").expect("outside target");
        let link = root_path.join("auth.json");
        symlink("../outside.json", &link).expect("outside link");
        let capabilities = CapabilityFs::new().expect("capabilities");
        let lease = capabilities
            .open_root(
                CapabilityRootPurpose::OperatorAuth,
                AbsolutePath::parse(root_path).expect("root path"),
                CapabilityAccess::PrivateReadWrite,
                RootCreation::Existing,
            )
            .expect("root lease");

        let error = lease
            .replace_file_allowing_live_leaf_link(Replacement {
                relative: PathBuf::from("auth.json"),
                expected: None,
                bytes: b"replacement".to_vec(),
                mode: FileMode::Private,
            })
            .expect_err("outside target must be rejected");
        assert!(matches!(error, FsError::OutsideCapability));
        assert_eq!(std::fs::read(&outside).expect("outside target"), b"outside");
        assert!(
            std::fs::symlink_metadata(&link)
                .expect("link after failure")
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn open_entry_error_includes_entry_leaf_not_unspecified() {
        let tmp = tempfile::tempdir().expect("temp root");
        let capabilities = CapabilityFs::new().expect("capabilities");
        let root = AbsolutePath::parse(tmp.path().to_path_buf()).expect("root");
        let lease = capabilities
            .open_root(
                CapabilityRootPurpose::AgentStore,
                root,
                CapabilityAccess::ReadOnly,
                RootCreation::Existing,
            )
            .expect("open root");
        let error = match lease.open_file(Path::new("missing-settings.json")) {
            Ok(_) => panic!("missing file must fail"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(
            message.contains("missing-settings.json"),
            "OpenEntry must name the entry, got {message}"
        );
        assert!(
            !message.contains("<unspecified>"),
            "OpenEntry must not omit the path, got {message}"
        );
    }

    #[test]
    fn sync_directory_accepts_root_and_retained_child() {
        let tmp = tempfile::tempdir().expect("temp root");
        std::fs::create_dir(tmp.path().join("child")).expect("child directory");
        let capabilities = CapabilityFs::new().expect("capabilities");
        let lease = capabilities
            .open_root(
                CapabilityRootPurpose::SessionStore,
                AbsolutePath::parse(tmp.path().to_path_buf()).expect("absolute root"),
                CapabilityAccess::PrivateReadWrite,
                RootCreation::Existing,
            )
            .expect("root lease");

        lease.sync_directory(Path::new("")).expect("sync root");
        lease
            .sync_directory(Path::new("child"))
            .expect("sync child");
    }

    #[cfg(unix)]
    #[test]
    fn private_agent_store_root_link_retains_target_and_rejects_other_purposes() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        let target = root.path().join("target");
        let replacement = root.path().join("replacement");
        let link = root.path().join("agent");
        std::fs::create_dir(&target).expect("target");
        std::fs::create_dir(&replacement).expect("replacement");
        std::fs::write(target.join("settings.json"), "{}").expect("settings");
        symlink(&target, &link).expect("link");
        let capabilities = CapabilityFs::new().expect("capabilities");
        let lease = capabilities
            .open_private_directory_allowing_link(
                CapabilityRootPurpose::AgentStore,
                AbsolutePath::parse(link.clone()).expect("agent link"),
            )
            .expect("private retained root");
        assert_eq!(lease.access(), CapabilityAccess::PrivateReadWrite);
        assert_eq!(
            lease
                .read_text_bounded(Path::new("settings.json"), 64)
                .expect("read"),
            "{}"
        );

        std::fs::remove_file(&link).expect("remove root link");
        symlink(&replacement, &link).expect("retarget root link");
        let error = lease
            .validate_live_root()
            .expect_err("retarget must invalidate live root");
        assert!(matches!(error, FsError::IdentityChanged));

        let error = capabilities
            .open_private_directory_allowing_link(
                CapabilityRootPurpose::ProjectResources,
                AbsolutePath::parse(target).expect("project path"),
            )
            .expect_err("project root link allowance must stay forbidden");
        assert!(matches!(error, FsError::OutsideCapability));
    }

    #[cfg(unix)]
    #[test]
    fn private_agent_store_rejects_intermediate_link_and_creates_plain_root() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        let real_parent = root.path().join("real-parent");
        let linked_parent = root.path().join("linked-parent");
        std::fs::create_dir(&real_parent).expect("real parent");
        std::fs::create_dir(real_parent.join("agent")).expect("linked agent");
        symlink(&real_parent, &linked_parent).expect("parent link");
        let capabilities = CapabilityFs::new().expect("capabilities");
        capabilities
            .open_private_directory_allowing_link(
                CapabilityRootPurpose::AgentStore,
                AbsolutePath::parse(linked_parent.join("agent")).expect("nested agent"),
            )
            .expect_err("intermediate root link must fail closed");

        let plain = root.path().join("plain").join("agent");
        let lease = capabilities
            .open_private_directory_allowing_link(
                CapabilityRootPurpose::AgentStore,
                AbsolutePath::parse(plain.clone()).expect("plain root"),
            )
            .expect("plain missing root created");
        assert_eq!(lease.display_root(), plain);
        assert!(plain.is_dir());
    }

    fn test_lease(path: &Path, access: CapabilityAccess) -> CapabilityRootLease {
        CapabilityFs::new()
            .expect("capabilities")
            .open_root(
                CapabilityRootPurpose::WorkflowWorktrees,
                AbsolutePath::parse(path.to_path_buf()).expect("absolute root"),
                access,
                RootCreation::Existing,
            )
            .expect("root lease")
    }

    #[test]
    fn streaming_replacement_reuses_fixed_buffer_and_preserves_bytes() {
        let source_root = tempfile::tempdir().expect("source root");
        let target_root = tempfile::tempdir().expect("target root");
        let mut bytes = vec![0_u8; STREAM_COPY_BUFFER_BYTES * 2 + 17];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = (index % 251) as u8;
        }
        std::fs::write(source_root.path().join("source"), &bytes).expect("source file");
        let source = test_lease(source_root.path(), CapabilityAccess::ReadOnly)
            .open_file(Path::new("source"))
            .expect("open source");
        let target = test_lease(target_root.path(), CapabilityAccess::PrivateReadWrite);
        let mut buffer = StreamCopyBuffer::new();
        let mut actual = 0;
        let installed = target
            .replace_file_streaming(
                StreamingReplacement {
                    relative: PathBuf::from("destination"),
                    expected: ReplacementTarget::Absent,
                },
                &source,
                &mut actual,
                u64::try_from(bytes.len()).expect("length"),
                &mut buffer,
            )
            .expect("stream replacement");
        assert_eq!(buffer.len(), STREAM_COPY_BUFFER_BYTES);
        assert_eq!(actual, bytes.len() as u64);
        assert_eq!(std::fs::read(target_root.path().join("destination")).expect("read destination"), bytes);
        assert_eq!(target.metadata(Path::new("destination")).expect("metadata").identity, installed);
        assert!(std::fs::read_dir(target_root.path())
            .expect("target entries")
            .all(|entry| !entry.expect("entry").file_name().to_string_lossy().contains(".rpi-replace")));
    }

    #[test]
    fn streaming_replacement_exact_identity_replaces_and_conflicts() {
        let source_root = tempfile::tempdir().expect("source root");
        let target_root = tempfile::tempdir().expect("target root");
        std::fs::write(source_root.path().join("source"), b"new").expect("source file");
        std::fs::write(target_root.path().join("destination"), b"old").expect("target file");
        let source = test_lease(source_root.path(), CapabilityAccess::ReadOnly)
            .open_file(Path::new("source"))
            .expect("open source");
        let target = test_lease(target_root.path(), CapabilityAccess::PrivateReadWrite);
        let expected = target.metadata(Path::new("destination")).expect("target metadata").identity;

        let mut buffer = StreamCopyBuffer::new();
        let mut actual = 0;
        target
            .replace_file_streaming(
                StreamingReplacement {
                    relative: PathBuf::from("destination"),
                    expected: ReplacementTarget::ExactIdentity(expected),
                },
                &source,
                &mut actual,
                3,
                &mut buffer,
            )
            .expect("exact replacement");
        assert_eq!(std::fs::read(target_root.path().join("destination")).expect("destination"), b"new");

        let error = target
            .replace_file_streaming(
                StreamingReplacement {
                    relative: PathBuf::from("destination"),
                    expected: ReplacementTarget::ExactIdentity(expected),
                },
                &source,
                &mut 0,
                3,
                &mut buffer,
            )
            .expect_err("stale identity must conflict");
        assert!(matches!(error, FsError::ReplacementConflict));
    }
    #[test]
    fn streaming_replacement_exact_identity_preserves_foreign_install() {
        let source_root = tempfile::tempdir().expect("source root");
        let target_root = tempfile::tempdir().expect("target root");
        std::fs::write(source_root.path().join("source"), b"new").expect("source file");
        std::fs::write(target_root.path().join("destination"), b"old").expect("target file");
        let source = test_lease(source_root.path(), CapabilityAccess::ReadOnly)
            .open_file(Path::new("source"))
            .expect("open source");
        let target = test_lease(target_root.path(), CapabilityAccess::PrivateReadWrite);
        let expected = target.metadata(Path::new("destination")).expect("target metadata").identity;
        let replacement = StreamingReplacement {
            relative: PathBuf::from("destination"),
            expected: ReplacementTarget::ExactIdentity(expected),
        };
        let mut prepared = target
            .prepare_streaming_replacement(&replacement)
            .expect("prepare replacement");
        prepared
            .stage_from(&source, &mut 0, 3, &mut StreamCopyBuffer::new())
            .expect("stage replacement");

        let destination = target_root.path().join("destination");
        let error = prepared
            .commit_with(replacement.expected, || {
                std::fs::write(&destination, b"foreign").expect("install foreign entry");
                Ok(())
            })
            .expect_err("foreign install must conflict");
        assert!(matches!(error, FsError::ReplacementConflict));
        let foreign_identity = target.metadata(Path::new("destination")).expect("foreign metadata").identity;
        drop(prepared);

        assert_eq!(std::fs::read(&destination).expect("foreign bytes"), b"foreign");
        assert_eq!(
            target.metadata(Path::new("destination")).expect("foreign metadata").identity,
            foreign_identity
        );
        let entries = std::fs::read_dir(target_root.path())
            .expect("target entries")
            .map(|entry| entry.expect("entry").file_name())
            .collect::<Vec<_>>();
        let backup = entries
            .iter()
            .find(|name| name.to_string_lossy().contains(".rpi-backup"))
            .expect("original backup retained");
        assert_eq!(
            std::fs::read(target_root.path().join(backup)).expect("backup bytes"),
            b"old"
        );
        assert!(entries
            .iter()
            .all(|name| !name.to_string_lossy().contains(".rpi-replace")));
    }

    #[cfg(unix)]
    #[test]
    fn strict_directory_and_streaming_permissions_fail_closed() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let source_root = tempfile::tempdir().expect("source root");
        let target_root = tempfile::tempdir().expect("target root");
        let source_path = source_root.path().join("executable");
        std::fs::write(&source_path, b"#!/bin/sh\n").expect("source file");
        std::fs::set_permissions(&source_path, std::fs::Permissions::from_mode(0o755))
            .expect("source mode");
        symlink("executable", source_root.path().join("link")).expect("source link");
        let source_lease = test_lease(source_root.path(), CapabilityAccess::ReadOnly);
        assert!(source_lease
            .read_dir_rejecting_unsupported(Path::new(""))
            .is_err());
        let source = source_lease.open_file(Path::new("executable")).expect("source");
        let target = test_lease(target_root.path(), CapabilityAccess::PrivateReadWrite);
        target
            .replace_file_streaming(
                StreamingReplacement {
                    relative: PathBuf::from("executable"),
                    expected: ReplacementTarget::Absent,
                },
                &source,
                &mut 0,
                64,
                &mut StreamCopyBuffer::new(),
            )
            .expect("stream executable");
        assert_eq!(
            std::fs::metadata(target_root.path().join("executable"))
                .expect("target metadata")
                .permissions()
                .mode()
                & 0o7777,
            0o755
        );
    }
}
