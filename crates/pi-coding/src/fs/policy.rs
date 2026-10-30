use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use pi_sandbox::{AbsolutePath, PolicyRevision, SandboxMode, SharedPolicySnapshot};

use super::{
    CapabilityAccess, CapabilityDirectory, CapabilityEntry, CapabilityEntryAuthority,
    CapabilityFile, CapabilityFs, CapabilityReadDir, CapabilityRootLease, CapabilityRootPurpose,
    CapabilityWritableFile, EntryKind, FileIdentity, FileMode, FsError, FsMetadata, Replacement,
    RootCreation,
};

#[derive(Clone)]
pub(crate) enum FsAuthority {
    Confined(Arc<PolicyFs>),
    DangerAmbient(Arc<PolicyFs>),
}

#[derive(Clone)]
pub(crate) enum ScratchLease {
    Confined(Arc<PolicyScratchRoot>),
    DangerAmbient(Arc<PolicyScratchRoot>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScratchPurpose {
    BashOutput,
    BrowserProfile,
    Converter,
    LspTransaction,
    ImageGeneration,
}

pub(crate) struct PolicyFs {
    revision: PolicyRevision,
    snapshot: SharedPolicySnapshot,
    capabilities: Arc<CapabilityFs>,
    cwd: PathBuf,
    roots: Vec<PolicyRoot>,
    scratch: Arc<PolicyScratchRoot>,
}

struct PolicyRoot {
    configured_path: AbsolutePath,
    access: PolicyRootAccess,
    protected_descendants: Box<[PathBuf]>,
    authority: PolicyRootAuthority,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PolicyRootAccess {
    ReadOnly,
    Writable,
}

enum PolicyRootAuthority {
    Directory(CapabilityRootLease),
    File(CapabilityFile),
}

pub(crate) struct PolicyFile {
    revision: PolicyRevision,
    file: CapabilityFile,
}

pub(crate) struct PolicyDirectory {
    revision: PolicyRevision,
    directory: CapabilityDirectory,
}

pub(crate) struct PolicyWritableFile {
    revision: PolicyRevision,
    file: CapabilityWritableFile,
}

pub(crate) enum PolicyOpenedEntry {
    File(PolicyFile),
    Directory(PolicyDirectory),
}

pub(crate) struct PolicyDirEntry {
    name: OsString,
    display_path: PathBuf,
    opened: PolicyOpenedEntry,
}

pub(crate) struct PolicyReadDir {
    revision: PolicyRevision,
    policy: Option<Arc<PolicyFs>>,
    parent_display: PathBuf,
    inner: CapabilityReadDir,
}

pub(crate) struct PolicyScratchRoot {
    revision: PolicyRevision,
    lease: CapabilityRootLease,
    sandbox_mount: AbsolutePath,
}

pub(crate) struct PolicyScratch {
    root: Arc<PolicyScratchRoot>,
    purpose: ScratchPurpose,
    relative: PathBuf,
    lease: CapabilityRootLease,
}

#[derive(Clone)]
pub(crate) struct SandboxVisiblePath(PathBuf);

#[derive(Clone, Debug)]
pub(crate) struct PolicyReplacement {
    pub(crate) path: PathBuf,
    pub(crate) expected: Option<FileIdentity>,
    pub(crate) bytes: Vec<u8>,
    pub(crate) mode: FileMode,
}

#[derive(Clone, Debug)]
pub(crate) struct ScratchReplacement {
    pub(crate) relative: PathBuf,
    pub(crate) expected: Option<FileIdentity>,
    pub(crate) bytes: Vec<u8>,
    pub(crate) mode: FileMode,
}

impl FsAuthority {
    pub(crate) fn confined(&self) -> Result<&Arc<PolicyFs>, FsError> {
        match self {
            Self::Confined(fs) | Self::DangerAmbient(fs) => Ok(fs),
        }
    }
}

impl ScratchLease {
    pub(crate) fn root(&self) -> &Arc<PolicyScratchRoot> {
        match self {
            Self::Confined(root) | Self::DangerAmbient(root) => root,
        }
    }

    pub(crate) fn browser_profile_path(
        &self,
        request_id: &str,
    ) -> Result<SandboxVisiblePath, FsError> {
        let component = validate_browser_request_id(request_id)?;
        self.root()
            .sandbox_visible_relative(&PathBuf::from("browser").join(component))
    }

    pub(crate) fn create_browser_profile(
        &self,
        request_id: &str,
    ) -> Result<PolicyScratch, FsError> {
        let component = validate_browser_request_id(request_id)?;
        self.root().create_named(
            ScratchPurpose::BrowserProfile,
            Path::new("browser").join(component),
        )
    }
    pub(crate) fn create_bash_output(&self) -> Result<PolicyScratch, FsError> {
        self.root().create_random(ScratchPurpose::BashOutput)
    }
    pub(crate) fn create_converter(&self) -> Result<PolicyScratch, FsError> {
        self.root().create_random(ScratchPurpose::Converter)
    }
}

impl PolicyFs {
    pub(crate) fn new(
        snapshot: SharedPolicySnapshot,
        cwd: AbsolutePath,
        capabilities: Arc<CapabilityFs>,
        scratch: Arc<PolicyScratchRoot>,
    ) -> Result<Arc<Self>, FsError> {
        if snapshot.revision() != scratch.revision {
            return Err(FsError::IdentityChanged);
        }
        let cwd = normalize_absolute(cwd.as_path())?;
        let mut roots = Vec::new();
        if let Some(filesystem) = snapshot.policy().filesystem() {
            for root in filesystem.read_only_roots() {
                roots.push(open_policy_root(
                    &capabilities,
                    root.clone(),
                    PolicyRootAccess::ReadOnly,
                    Vec::new(),
                )?);
            }
            for writable in filesystem.writable_roots() {
                roots.push(open_policy_root(
                    &capabilities,
                    writable.root().clone(),
                    PolicyRootAccess::Writable,
                    writable
                        .protected_descendants()
                        .iter()
                        .map(|path| path.as_path().to_path_buf())
                        .collect(),
                )?);
            }
        }
        // Ensure the session cwd is a writable root so cwd-relative operations
        // resolve through it directly (no walk from the filesystem root, which
        // would cross host symlinks such as macOS `/var`). Existing behavior
        // for every non-read-only mode.
        if snapshot.policy().mode() != SandboxMode::ReadOnly
            && !roots
                .iter()
                .any(|root| cwd.starts_with(root.configured_path.as_path()))
        {
            let cwd_path = AbsolutePath::parse(cwd.clone()).map_err(|_| FsError::InvalidPath)?;
            roots.push(open_policy_root(
                &capabilities,
                cwd_path,
                PolicyRootAccess::Writable,
                Vec::new(),
            )?);
        }
        // `danger-full-access` is genuine ambient host filesystem authority
        // (not cwd-only): additionally open the filesystem root as a writable
        // fallback so absolute paths outside the cwd resolve. The cwd root and
        // any configured roots are deeper and sorted first, so they take
        // precedence; denied paths still apply. Workspace-write/read-only keep
        // cwd-scoped behavior (no ambient root).
        if snapshot.policy().mode() == SandboxMode::DangerFullAccess {
            let fs_root_path = filesystem_root(Path::new(&cwd));
            let fs_root = AbsolutePath::parse(fs_root_path).map_err(|_| FsError::InvalidPath)?;
            if !roots
                .iter()
                .any(|root| root.configured_path.as_path() == fs_root.as_path())
            {
                roots.push(open_policy_root(
                    &capabilities,
                    fs_root,
                    PolicyRootAccess::Writable,
                    Vec::new(),
                )?);
            }
        }
        roots.sort_by(|left, right| {
            right
                .configured_path
                .as_path()
                .components()
                .count()
                .cmp(&left.configured_path.as_path().components().count())
        });
        Ok(Arc::new(Self {
            revision: snapshot.revision(),
            snapshot,
            capabilities,
            cwd,
            roots,
            scratch,
        }))
    }

    pub(crate) const fn revision(&self) -> PolicyRevision {
        self.revision
    }

    pub(crate) fn mutation_coordinate(
        &self,
        path: &Path,
    ) -> Result<(FileIdentity, PathBuf), FsError> {
        let absolute = self.absolute(path)?;
        let (_, root, relative) = self.resolve_indexed(&absolute, true)?;
        let PolicyRootAuthority::Directory(lease) = &root.authority else {
            return Err(FsError::ReadOnly);
        };
        Ok((lease.identity(), relative))
    }
    pub(crate) fn metadata(&self, path: &Path) -> Result<FsMetadata, FsError> {
        let (root, relative) = self.resolve(path, false)?;
        match &root.authority {
            PolicyRootAuthority::Directory(lease) => lease.metadata(&relative),
            PolicyRootAuthority::File(file) if relative.as_os_str().is_empty() => file.metadata(),
            PolicyRootAuthority::File(_) => Err(FsError::OutsideCapability),
        }
    }

    pub(crate) fn open_file(&self, path: &Path) -> Result<PolicyFile, FsError> {
        let (root, relative) = self.resolve(path, false)?;
        match &root.authority {
            PolicyRootAuthority::Directory(lease) => {
                lease.open_file(&relative).map(|file| PolicyFile {
                    revision: self.revision,
                    file,
                })
            }
            PolicyRootAuthority::File(file) if relative.as_os_str().is_empty() => {
                clone_policy_file(file).map(|file| PolicyFile {
                    revision: self.revision,
                    file,
                })
            }
            PolicyRootAuthority::File(_) => Err(FsError::OutsideCapability),
        }
    }

    pub(crate) fn open_directory(&self, path: &Path) -> Result<PolicyDirectory, FsError> {
        let (root, relative) = self.resolve(path, false)?;
        let PolicyRootAuthority::Directory(lease) = &root.authority else {
            return Err(FsError::UnsupportedEntryType);
        };
        lease
            .open_directory(&relative)
            .map(|directory| PolicyDirectory {
                revision: self.revision,
                directory,
            })
    }

    pub(crate) fn read_bounded(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, FsError> {
        self.open_file(path)?.read_bounded(max_bytes)
    }

    pub(crate) fn read_text_bounded(&self, path: &Path, max_bytes: u64) -> Result<String, FsError> {
        String::from_utf8(self.read_bounded(path, max_bytes)?).map_err(|_| FsError::InvalidPath)
    }

    pub(crate) fn read_dir(self: &Arc<Self>, path: &Path) -> Result<PolicyReadDir, FsError> {
        let absolute = self.absolute(path)?;
        let (_, root, relative) = self.resolve_indexed(&absolute, false)?;
        let PolicyRootAuthority::Directory(lease) = &root.authority else {
            return Err(FsError::UnsupportedEntryType);
        };
        Ok(PolicyReadDir {
            revision: self.revision,
            policy: Some(self.clone()),
            parent_display: absolute,
            inner: lease.read_dir(&relative)?,
        })
    }

    pub(crate) fn create_dir_all(&self, path: &Path, mode: FileMode) -> Result<(), FsError> {
        let (root, relative) = self.resolve(path, true)?;
        policy_directory_authority(root)?.create_dir_all(&relative, mode)
    }

    pub(crate) fn create_new(
        &self,
        path: &Path,
        mode: FileMode,
    ) -> Result<PolicyWritableFile, FsError> {
        let (root, relative) = self.resolve(path, true)?;
        policy_directory_authority(root)?
            .create_new(&relative, mode)
            .map(|file| PolicyWritableFile {
                revision: self.revision,
                file,
            })
    }

    pub(crate) fn replace_file(
        &self,
        replacement: PolicyReplacement,
    ) -> Result<FileIdentity, FsError> {
        let (root, relative) = self.resolve(&replacement.path, true)?;
        policy_directory_authority(root)?.replace_file(Replacement {
            relative,
            expected: replacement.expected,
            bytes: replacement.bytes,
            mode: replacement.mode,
        })
    }

    pub(crate) fn replace_batch(
        &self,
        replacements: Vec<PolicyReplacement>,
    ) -> Result<Vec<FileIdentity>, FsError> {
        let mut root_index = None;
        let mut converted = Vec::with_capacity(replacements.len());
        for replacement in replacements {
            let absolute = self.absolute(&replacement.path)?;
            let (index, _, relative) = self.resolve_indexed(&absolute, true)?;
            if root_index
                .replace(index)
                .is_some_and(|previous| previous != index)
            {
                return Err(FsError::OutsideCapability);
            }
            converted.push(Replacement {
                relative,
                expected: replacement.expected,
                bytes: replacement.bytes,
                mode: replacement.mode,
            });
        }
        let Some(index) = root_index else {
            return Ok(Vec::new());
        };
        policy_directory_authority(&self.roots[index])?.replace_batch(converted)
    }

    pub(crate) fn remove_file(&self, path: &Path) -> Result<(), FsError> {
        let (root, relative) = self.resolve(path, true)?;
        policy_directory_authority(root)?.remove_file(&relative)
    }

    pub(crate) fn remove_directory(&self, path: &Path) -> Result<(), FsError> {
        let (root, relative) = self.resolve(path, true)?;
        policy_directory_authority(root)?.remove_directory(&relative)
    }

    pub(crate) fn remove_tree(&self, path: &Path) -> Result<(), FsError> {
        let (root, relative) = self.resolve(path, true)?;
        policy_directory_authority(root)?.remove_tree(&relative)
    }

    pub(crate) fn rename(&self, from: &Path, to: &Path) -> Result<(), FsError> {
        let from_absolute = self.absolute(from)?;
        let to_absolute = self.absolute(to)?;
        let (from_index, from_root, from_relative) = self.resolve_indexed(&from_absolute, true)?;
        let (to_index, _, to_relative) = self.resolve_indexed(&to_absolute, true)?;
        if from_index != to_index {
            return Err(FsError::OutsideCapability);
        }
        policy_directory_authority(from_root)?.rename(&from_relative, &to_relative)
    }

    pub(crate) fn create_scratch(
        self: &Arc<Self>,
        purpose: ScratchPurpose,
    ) -> Result<PolicyScratch, FsError> {
        self.scratch.create_random(purpose)
    }

    #[cfg(unix)]
    pub(crate) fn duplicate_for_spawn_working_directory(
        &self,
        path: &Path,
    ) -> Result<super::CapabilityWorkingDirectorySource, FsError> {
        let (root, relative) = self.resolve(path, false)?;
        let lease = policy_directory_authority(root)?;
        lease.duplicate_descendant_for_spawn_working_directory(&lease.display_root().join(relative))
    }

    fn absolute(&self, path: &Path) -> Result<PathBuf, FsError> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        };
        normalize_absolute(&absolute)
    }

    fn resolve(&self, path: &Path, write: bool) -> Result<(&PolicyRoot, PathBuf), FsError> {
        let absolute = self.absolute(path)?;
        let (_, root, relative) = self.resolve_indexed(&absolute, write)?;
        Ok((root, relative))
    }

    fn resolve_indexed(
        &self,
        absolute: &Path,
        write: bool,
    ) -> Result<(usize, &PolicyRoot, PathBuf), FsError> {
        if self.is_denied(absolute) || self.is_private(absolute) {
            return Err(FsError::Denied);
        }
        let (index, root) = self
            .roots
            .iter()
            .enumerate()
            .find(|(_, root)| absolute.starts_with(root.configured_path.as_path()))
            .ok_or(FsError::OutsideCapability)?;
        let relative = absolute
            .strip_prefix(root.configured_path.as_path())
            .map_err(|_| FsError::OutsideCapability)?
            .to_path_buf();
        if write {
            if root.access != PolicyRootAccess::Writable {
                return Err(FsError::ReadOnly);
            }
            if root
                .protected_descendants
                .iter()
                .any(|protected| relative.starts_with(protected))
            {
                return Err(FsError::Denied);
            }
        }
        Ok((index, root, relative))
    }

    fn is_denied(&self, absolute: &Path) -> bool {
        self.snapshot
            .policy()
            .filesystem()
            .is_some_and(|filesystem| {
                filesystem
                    .denied_paths()
                    .iter()
                    .any(|denied| absolute.starts_with(denied.as_path()))
            })
    }

    fn is_private(&self, absolute: &Path) -> bool {
        self.snapshot
            .policy()
            .filesystem()
            .is_some_and(|filesystem| {
                filesystem
                    .private_roots()
                    .iter()
                    .any(|root| absolute.starts_with(root.as_path()))
            })
    }
}

impl PolicyFile {
    pub(crate) const fn revision(&self) -> PolicyRevision {
        self.revision
    }

    pub(crate) fn display_path(&self) -> PathBuf {
        self.file.display_path()
    }

    pub(crate) const fn identity(&self) -> FileIdentity {
        self.file.identity()
    }

    pub(crate) fn metadata(&self) -> Result<FsMetadata, FsError> {
        self.file.metadata()
    }

    pub(crate) fn read_bounded(&self, max_bytes: u64) -> Result<Vec<u8>, FsError> {
        self.file.read_bounded(max_bytes)
    }

    pub(crate) fn try_clone_std(&self) -> io::Result<std::fs::File> {
        self.file.try_clone_std()
    }
}

impl PolicyDirectory {
    pub(crate) const fn revision(&self) -> PolicyRevision {
        self.revision
    }

    pub(crate) fn display_path(&self) -> PathBuf {
        self.directory.display_path()
    }

    pub(crate) const fn identity(&self) -> FileIdentity {
        self.directory.identity()
    }

    pub(crate) fn read_dir(self, fs: &Arc<PolicyFs>) -> Result<PolicyReadDir, FsError> {
        if self.revision != fs.revision {
            return Err(FsError::IdentityChanged);
        }
        let parent_display = self.display_path();
        fs.resolve_indexed(&fs.absolute(&parent_display)?, false)?;
        Ok(PolicyReadDir {
            revision: self.revision,
            policy: Some(fs.clone()),
            parent_display,
            inner: self.directory.read_dir()?,
        })
    }
}

impl PolicyWritableFile {
    pub(crate) fn write_all(&mut self, bytes: &[u8]) -> Result<(), FsError> {
        self.file.write_all(bytes)
    }

    pub(crate) fn sync_all(&mut self) -> Result<(), FsError> {
        self.file.sync_all()
    }

    pub(crate) fn finish(self) -> Result<PolicyFile, FsError> {
        self.file.finish().map(|file| PolicyFile {
            revision: self.revision,
            file,
        })
    }
}

impl PolicyOpenedEntry {
    pub(crate) const fn revision(&self) -> PolicyRevision {
        match self {
            Self::File(file) => file.revision(),
            Self::Directory(directory) => directory.revision(),
        }
    }

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

impl PolicyDirEntry {
    pub(crate) fn name(&self) -> &OsStr {
        &self.name
    }

    pub(crate) fn display_path(&self) -> &Path {
        &self.display_path
    }

    pub(crate) const fn opened(&self) -> &PolicyOpenedEntry {
        &self.opened
    }

    pub(crate) fn into_opened(self) -> PolicyOpenedEntry {
        self.opened
    }
}

impl PolicyReadDir {
    pub(crate) fn next(&mut self) -> Result<Option<PolicyDirEntry>, FsError> {
        loop {
            let Some(entry) = self.inner.next()? else {
                return Ok(None);
            };
            let name = entry.name().to_os_string();
            validate_entry_name(&name)?;
            let display_path = self.parent_display.join(&name);
            if self.policy.as_ref().is_some_and(|policy| {
                policy.is_denied(&display_path) || policy.is_private(&display_path)
            }) {
                continue;
            }
            let opened = match entry.into_entry() {
                CapabilityEntry::File(file) => PolicyOpenedEntry::File(PolicyFile {
                    revision: self.revision,
                    file,
                }),
                CapabilityEntry::Directory(directory) => {
                    PolicyOpenedEntry::Directory(PolicyDirectory {
                        revision: self.revision,
                        directory,
                    })
                }
            };
            return Ok(Some(PolicyDirEntry {
                name,
                display_path,
                opened,
            }));
        }
    }
}

impl PolicyScratchRoot {
    pub(crate) fn from_lease(
        revision: PolicyRevision,
        lease: CapabilityRootLease,
        sandbox_mount: AbsolutePath,
    ) -> Result<Arc<Self>, FsError> {
        if lease.purpose() != CapabilityRootPurpose::SessionStore
            || lease.access() != CapabilityAccess::PrivateReadWrite
        {
            return Err(FsError::OutsideCapability);
        }
        Ok(Arc::new(Self {
            revision,
            lease,
            sandbox_mount,
        }))
    }

    pub(crate) fn acquire(
        revision: PolicyRevision,
        capabilities: &Arc<CapabilityFs>,
        host_root: AbsolutePath,
        sandbox_mount: AbsolutePath,
    ) -> Result<Arc<Self>, FsError> {
        let lease = capabilities.open_root(
            CapabilityRootPurpose::SessionStore,
            host_root,
            CapabilityAccess::PrivateReadWrite,
            RootCreation::CreateDirectory,
        )?;
        Self::from_lease(revision, lease, sandbox_mount)
    }

    pub(crate) const fn revision(&self) -> PolicyRevision {
        self.revision
    }

    pub(crate) fn sandbox_mount(&self) -> &AbsolutePath {
        &self.sandbox_mount
    }

    pub(crate) fn create_random(
        self: &Arc<Self>,
        purpose: ScratchPurpose,
    ) -> Result<PolicyScratch, FsError> {
        let relative = PathBuf::from(scratch_purpose_name(purpose))
            .join(uuid::Uuid::new_v4().simple().to_string());
        self.create_named(purpose, relative)
    }

    fn create_named(
        self: &Arc<Self>,
        purpose: ScratchPurpose,
        relative: PathBuf,
    ) -> Result<PolicyScratch, FsError> {
        validate_relative(&relative)?;
        let lease = self.lease.lease_directory(
            &relative,
            CapabilityAccess::PrivateReadWrite,
            RootCreation::CreateDirectory,
        )?;
        Ok(PolicyScratch {
            root: self.clone(),
            purpose,
            relative,
            lease,
        })
    }

    fn sandbox_visible_relative(&self, relative: &Path) -> Result<SandboxVisiblePath, FsError> {
        validate_relative(relative)?;
        Ok(SandboxVisiblePath(
            self.sandbox_mount.as_path().join(relative),
        ))
    }
}

impl PolicyScratch {
    pub(crate) fn revision(&self) -> PolicyRevision {
        self.root.revision
    }

    pub(crate) const fn purpose(&self) -> ScratchPurpose {
        self.purpose
    }

    pub(crate) fn metadata(&self, relative: &Path) -> Result<FsMetadata, FsError> {
        self.lease.metadata(relative)
    }

    pub(crate) fn open_file(&self, relative: &Path) -> Result<PolicyFile, FsError> {
        self.lease.open_file(relative).map(|file| PolicyFile {
            revision: self.revision(),
            file,
        })
    }

    pub(crate) fn open_directory(&self, relative: &Path) -> Result<PolicyDirectory, FsError> {
        self.lease
            .open_directory(relative)
            .map(|directory| PolicyDirectory {
                revision: self.revision(),
                directory,
            })
    }

    pub(crate) fn read_bounded(&self, relative: &Path, max_bytes: u64) -> Result<Vec<u8>, FsError> {
        self.lease.read_bounded(relative, max_bytes)
    }

    pub(crate) fn read_text_bounded(
        &self,
        relative: &Path,
        max_bytes: u64,
    ) -> Result<String, FsError> {
        self.lease.read_text_bounded(relative, max_bytes)
    }

    pub(crate) fn read_dir(&self, relative: &Path) -> Result<PolicyReadDir, FsError> {
        Ok(PolicyReadDir {
            revision: self.revision(),
            policy: None,
            parent_display: self.lease.display_root().join(relative),
            inner: self.lease.read_dir(relative)?,
        })
    }

    pub(crate) fn create_dir_all(&self, relative: &Path, mode: FileMode) -> Result<(), FsError> {
        self.lease.create_dir_all(relative, mode)
    }

    pub(crate) fn create_new(
        &self,
        relative: &Path,
        mode: FileMode,
    ) -> Result<PolicyWritableFile, FsError> {
        self.lease
            .create_new(relative, mode)
            .map(|file| PolicyWritableFile {
                revision: self.revision(),
                file,
            })
    }
    pub(crate) fn display_path(&self, relative: &Path) -> Result<PathBuf, FsError> {
        validate_relative(relative)?;
        Ok(self.lease.display_root().join(relative))
    }

    pub(crate) fn replace_file(
        &self,
        replacement: ScratchReplacement,
    ) -> Result<FileIdentity, FsError> {
        self.lease.replace_file(Replacement {
            relative: replacement.relative,
            expected: replacement.expected,
            bytes: replacement.bytes,
            mode: replacement.mode,
        })
    }

    pub(crate) fn replace_batch(
        &self,
        replacements: Vec<ScratchReplacement>,
    ) -> Result<Vec<FileIdentity>, FsError> {
        self.lease.replace_batch(
            replacements
                .into_iter()
                .map(|replacement| Replacement {
                    relative: replacement.relative,
                    expected: replacement.expected,
                    bytes: replacement.bytes,
                    mode: replacement.mode,
                })
                .collect(),
        )
    }

    pub(crate) fn remove_file(&self, relative: &Path) -> Result<(), FsError> {
        self.lease.remove_file(relative)
    }
    pub(crate) fn remove_file_expected(
        &self,
        relative: &Path,
        expected: FileIdentity,
    ) -> Result<(), FsError> {
        self.lease.remove_file_expected(relative, expected)
    }

    pub(crate) fn remove_directory(&self, relative: &Path) -> Result<(), FsError> {
        self.lease.remove_directory(relative)
    }

    pub(crate) fn remove_tree(&self, relative: &Path) -> Result<(), FsError> {
        self.lease.remove_tree(relative)
    }

    pub(crate) fn rename(&self, from: &Path, to: &Path) -> Result<(), FsError> {
        self.lease.rename(from, to)
    }

    pub(crate) fn sandbox_visible_path(
        &self,
        relative: &Path,
    ) -> Result<SandboxVisiblePath, FsError> {
        validate_relative(relative)?;
        self.root
            .sandbox_visible_relative(&self.relative.join(relative))
    }

    pub(crate) fn destroy(self) -> Result<(), FsError> {
        self.root.lease.remove_tree(&self.relative)
    }
}

impl SandboxVisiblePath {
    pub(crate) fn as_path(&self) -> &Path {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn for_browser_test(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }
}

/// Re-contextualize a policy-root open error so the absolute configured
/// prefix never leaks past the capability boundary, while keeping the
/// contextual culprit (the offending final component) that explains why the
/// root was rejected.
///
/// Applied to `NotADirectory` — the signal that a path intermediate is a
/// regular file rather than a directory — so a misconfigured read-only root
/// reports `<absolute>/<offending-component>` instead of the host absolute
/// path the capability layer annotated. Other error kinds pass through
/// unchanged so their existing diagnostics stay intact.
fn contextualize_policy_root_error(error: FsError) -> FsError {
    match error {
        FsError::System {
            operation,
            kind: io::ErrorKind::NotADirectory,
            raw_os_error,
            purpose,
            path: Some(path),
        } => {
            let sanitized = path
                .file_name()
                .map(|name| PathBuf::from("<absolute>").join(name))
                .unwrap_or_else(|| PathBuf::from("<absolute>"));
            FsError::System {
                operation,
                kind: io::ErrorKind::NotADirectory,
                raw_os_error,
                purpose,
                path: Some(sanitized),
            }
        }
        other => other,
    }
}

fn open_policy_root(
    capabilities: &Arc<CapabilityFs>,
    path: AbsolutePath,
    access: PolicyRootAccess,
    protected_descendants: Vec<PathBuf>,
) -> Result<PolicyRoot, FsError> {
    let authority = capabilities
        .open_entry_authority(CapabilityRootPurpose::ProjectResources, path.clone())
        .map_err(contextualize_policy_root_error)?;
    let authority = match (access, authority) {
        (PolicyRootAccess::ReadOnly, CapabilityEntryAuthority::Directory(lease)) => {
            PolicyRootAuthority::Directory(lease)
        }
        (PolicyRootAccess::ReadOnly, CapabilityEntryAuthority::File(file)) => {
            PolicyRootAuthority::File(file)
        }
        (PolicyRootAccess::Writable, CapabilityEntryAuthority::Directory(_)) => {
            PolicyRootAuthority::Directory(capabilities.open_root(
                CapabilityRootPurpose::ProjectResources,
                path.clone(),
                CapabilityAccess::ReadWrite,
                RootCreation::Existing,
            )?)
        }
        (PolicyRootAccess::Writable, CapabilityEntryAuthority::File(_)) => {
            return Err(FsError::UnsupportedEntryType);
        }
    };
    Ok(PolicyRoot {
        configured_path: path,
        access,
        protected_descendants: protected_descendants.into_boxed_slice(),
        authority,
    })
}

fn policy_directory_authority(root: &PolicyRoot) -> Result<&CapabilityRootLease, FsError> {
    match &root.authority {
        PolicyRootAuthority::Directory(lease) => Ok(lease),
        PolicyRootAuthority::File(_) => Err(FsError::UnsupportedEntryType),
    }
}

fn clone_policy_file(file: &CapabilityFile) -> Result<CapabilityFile, FsError> {
    let (lease, relative) = file.authority();
    lease.open_file(&relative)
}

fn normalize_absolute(path: &Path) -> Result<PathBuf, FsError> {
    if !path.is_absolute() {
        return Err(FsError::InvalidPath);
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(FsError::InvalidPath);
                }
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    Ok(normalized)
}

/// Walk up to the topmost existing ancestor of `path` — the filesystem root
/// (`/` on unix, the drive root `C:\` on windows). Used by `danger-full-access`
/// to open a single ambient root covering the host filesystem rather than
/// only the session cwd.
fn filesystem_root(path: &Path) -> PathBuf {
    let mut root = path.to_path_buf();
    while let Some(parent) = root.parent() {
        if parent.as_os_str().is_empty() || parent == root {
            break;
        }
        root = parent.to_path_buf();
    }
    root
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

fn validate_entry_name(name: &OsStr) -> Result<(), FsError> {
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

fn validate_browser_request_id(request_id: &str) -> Result<&str, FsError> {
    let bytes = request_id.as_bytes();
    let valid_grammar = (1..=64).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        });
    let path = Path::new(request_id);
    if valid_grammar
        && path.components().count() == 1
        && matches!(path.components().next(), Some(Component::Normal(_)))
        && !matches!(request_id, "." | "..")
    {
        Ok(request_id)
    } else {
        Err(FsError::InvalidPath)
    }
}

const fn scratch_purpose_name(purpose: ScratchPurpose) -> &'static str {
    match purpose {
        ScratchPurpose::BashOutput => "bash-output",
        ScratchPurpose::BrowserProfile => "browser",
        ScratchPurpose::Converter => "converter",
        ScratchPurpose::LspTransaction => "lsp-transaction",
        ScratchPurpose::ImageGeneration => "image-generation",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_sandbox::{
        AbsolutePath, CredentialPolicy, FileSystemPolicy, HostHttpPolicy, NetworkPolicy,
        PlatformFactInputs, PolicyRevision, RequestedPolicy, ResourceLimits, SandboxMode,
        SandboxPolicy, SessionScope, derive_platform_facts, resolve_policy_snapshot,
    };
    use std::path::Path;
    use std::sync::Arc;

    #[cfg(unix)]
    #[test]
    fn filesystem_root_walks_to_unix_root() {
        assert_eq!(
            filesystem_root(Path::new("/workspace/project")),
            Path::new("/")
        );
        assert_eq!(filesystem_root(Path::new("/")), Path::new("/"));
        assert_eq!(filesystem_root(Path::new("/a/b/c")), Path::new("/"));
    }

    #[cfg(unix)]
    #[test]
    fn filesystem_root_preserves_drive_on_windows_path_shape() {
        // The helper is platform-agnostic; on unix it always reaches `/`.
        assert_eq!(filesystem_root(Path::new("/tmp/x")), Path::new("/"));
    }

    #[cfg(unix)]
    #[test]
    fn danger_policy_opens_filesystem_root_as_ambient_authority() {
        let root = tempfile::tempdir().expect("root");
        let cwd = AbsolutePath::parse(root.path().to_path_buf()).expect("cwd");
        let scratch_path = AbsolutePath::parse(root.path().join("scratch")).expect("scratch");
        std::fs::create_dir(scratch_path.as_path()).expect("scratch");
        let policy = SandboxPolicy::danger_full_access(
            CredentialPolicy::new(Vec::new()).expect("credentials"),
        )
        .expect("policy");
        let snapshot = Arc::new(
            pi_sandbox::PolicySnapshot::new(
                PolicyRevision::new(1).expect("revision"),
                #[cfg(target_os = "linux")]
                pi_sandbox::HostPlatform::Linux,
                #[cfg(target_os = "macos")]
                pi_sandbox::HostPlatform::Macos,
                policy,
            )
            .expect("snapshot"),
        );
        let capabilities = CapabilityFs::new().expect("capabilities");
        let scratch = PolicyScratchRoot::acquire(
            PolicyRevision::new(1).expect("revision"),
            &capabilities,
            scratch_path.clone(),
            scratch_path,
        )
        .expect("scratch authority");

        let fs = PolicyFs::new(snapshot, cwd, capabilities, scratch)
            .expect("danger filesystem authority");
        assert!(fs.metadata(Path::new("/")).is_ok());
    }

    #[test]
    fn read_only_regular_file_root_is_opened_without_directory_authority() {
        let root = tempfile::tempdir().expect("root");
        let cwd = root.path().join("workspace");
        let scratch = root.path().join("scratch");
        let runtime_file = root.path().join("runtime.conf");
        std::fs::create_dir(&cwd).expect("workspace");
        std::fs::create_dir(&scratch).expect("scratch");
        std::fs::write(&runtime_file, "runtime-root").expect("runtime file");
        let cwd = AbsolutePath::parse(cwd).expect("cwd");
        let scratch_path = AbsolutePath::parse(scratch.clone()).expect("scratch");
        let filesystem = FileSystemPolicy::new(
            vec![AbsolutePath::parse(runtime_file.clone()).expect("runtime file")],
            vec![
                pi_sandbox::WritableRoot::new(cwd.clone(), Vec::new()).expect("cwd root"),
                pi_sandbox::WritableRoot::new(scratch_path.clone(), Vec::new())
                    .expect("scratch root"),
            ],
            Vec::new(),
        )
        .expect("filesystem");
        let policy = SandboxPolicy::workspace_write(
            filesystem,
            NetworkPolicy::None,
            HostHttpPolicy::new(Vec::new()).expect("http"),
            CredentialPolicy::new(Vec::new()).expect("credentials"),
            ResourceLimits::confined_defaults(),
        )
        .expect("policy");
        let snapshot = Arc::new(
            pi_sandbox::PolicySnapshot::new(
                PolicyRevision::new(1).expect("revision"),
                current_host_platform(),
                policy,
            )
            .expect("snapshot"),
        );
        let capabilities = CapabilityFs::new().expect("capabilities");
        let scratch = PolicyScratchRoot::acquire(
            PolicyRevision::new(1).expect("revision"),
            &capabilities,
            scratch_path.clone(),
            scratch_path,
        )
        .expect("scratch capability");
        let fs = PolicyFs::new(snapshot, cwd, capabilities, scratch).expect("policy fs");
        assert_eq!(
            fs.read_bounded(&runtime_file, 64).expect("read file root"),
            b"runtime-root"
        );
        assert!(matches!(
            fs.open_directory(&runtime_file),
            Err(FsError::UnsupportedEntryType)
        ));
    }

    #[test]
    fn read_only_root_with_file_intermediate_is_rejected_contextually() {
        let root = tempfile::tempdir().expect("root");
        let intermediate = root.path().join("not-directory");
        std::fs::write(&intermediate, "file").expect("intermediate file");
        let requested = AbsolutePath::parse(intermediate.join("runtime.conf")).expect("path");
        let capabilities = CapabilityFs::new().expect("capabilities");
        let error = match open_policy_root(
            &capabilities,
            requested,
            PolicyRootAccess::ReadOnly,
            Vec::new(),
        ) {
            Ok(_) => panic!("file intermediate must be rejected"),
            Err(error) => error,
        };
        let display = error.to_string();
        let debug = format!("{error:?}");
        assert!(matches!(
            error,
            FsError::System {
                kind: io::ErrorKind::NotADirectory,
                ..
            }
        ));
        assert!(display.contains("ProjectResources"), "{display}");
        assert!(display.contains("<absolute>/not-directory"), "{display}");
        assert!(
            !display.contains(root.path().to_string_lossy().as_ref()),
            "{display}"
        );
        assert!(
            !debug.contains(root.path().to_string_lossy().as_ref()),
            "{debug}"
        );
    }

    #[test]
    fn unmatched_read_path_hints_add_dir_and_readonly_roots() {
        let root = tempfile::tempdir().expect("root");
        let cwd = root.path().join("workspace");
        let scratch = root.path().join("scratch");
        let outside = root.path().join("outside.txt");
        std::fs::create_dir(&cwd).expect("workspace");
        std::fs::create_dir(&scratch).expect("scratch");
        std::fs::write(&outside, "secret").expect("outside");
        let cwd_path = AbsolutePath::parse(cwd).expect("cwd");
        let scratch_path = AbsolutePath::parse(scratch.clone()).expect("scratch");
        let filesystem = FileSystemPolicy::new(
            Vec::new(),
            vec![
                pi_sandbox::WritableRoot::new(cwd_path.clone(), Vec::new()).expect("cwd root"),
                pi_sandbox::WritableRoot::new(scratch_path.clone(), Vec::new())
                    .expect("scratch root"),
            ],
            Vec::new(),
        )
        .expect("filesystem");
        let policy = SandboxPolicy::workspace_write(
            filesystem,
            NetworkPolicy::None,
            HostHttpPolicy::new(Vec::new()).expect("http"),
            CredentialPolicy::new(Vec::new()).expect("credentials"),
            ResourceLimits::confined_defaults(),
        )
        .expect("policy");
        let snapshot = Arc::new(
            pi_sandbox::PolicySnapshot::new(
                PolicyRevision::new(1).expect("revision"),
                current_host_platform(),
                policy,
            )
            .expect("snapshot"),
        );
        let capabilities = CapabilityFs::new().expect("capabilities");
        let scratch = PolicyScratchRoot::acquire(
            PolicyRevision::new(1).expect("revision"),
            &capabilities,
            scratch_path.clone(),
            scratch_path,
        )
        .expect("scratch capability");
        let fs = PolicyFs::new(snapshot, cwd_path, capabilities, scratch).expect("policy fs");
        let error = match fs.read_bounded(&outside, 64) {
            Ok(_) => panic!("outside path must be denied"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(matches!(error, FsError::OutsideCapability), "{message}");
        assert!(message.contains("--add-dir"), "{message}");
        assert!(message.contains("sandbox.readOnlyRoots"), "{message}");
    }

    #[cfg(unix)]
    #[test]
    fn home_store_sentinels_are_denied_when_workspace_is_home() {
        let root = tempfile::tempdir().expect("root");
        let home = root.path().join("home");
        let agent_dir = home.join(".rpi").join("profiles").join("active");
        let default_store = home.join(".rpi");
        let sibling_store = home.join(".rpi").join("profiles").join("other");
        let session_dir = agent_dir.join("sessions").join("session-1");
        let revision = session_dir.join("sandbox").join("revision-1");
        let scratch = revision.join("scratch");
        for path in [&home, &agent_dir, &sibling_store, &session_dir, &revision, &scratch] {
            std::fs::create_dir_all(path).expect("fixture directory");
        }
        std::fs::write(default_store.join("teams.json"), "confidential-default")
            .expect("default store sentinel");
        std::fs::write(sibling_store.join("tokens.json"), "confidential-other")
            .expect("sibling store sentinel");
        std::fs::write(agent_dir.join("teams.json"), "confidential-active")
            .expect("active store sentinel");

        let workspace = AbsolutePath::parse(home.clone()).expect("workspace");
        let agent = AbsolutePath::parse(agent_dir.clone()).expect("agent");
        let session = AbsolutePath::parse(session_dir.clone()).expect("session");
        let facts = derive_platform_facts(
            PlatformFactInputs::new(
                "home-store-sentinels".to_owned(),
                vec![workspace.clone()],
                agent.clone(),
                session.clone(),
            )
            .expect("platform fact inputs"),
        )
        .expect("platform facts");
        let scope = SessionScope::new(
            "home-store-sentinels".to_owned(),
            vec![workspace.clone()],
            agent.clone(),
            session.clone(),
        )
        .expect("session scope");
        let requested = RequestedPolicy::new(
            SandboxMode::WorkspaceWrite,
            NetworkPolicy::None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            HostHttpPolicy::new(Vec::new()).expect("empty host HTTP policy"),
            CredentialPolicy::new(Vec::new()).expect("empty credential policy"),
            ResourceLimits::confined_defaults(),
        )
        .expect("requested policy");
        let revision_path = AbsolutePath::parse(revision).expect("revision path");
        let snapshot = Arc::new(
            resolve_policy_snapshot(
                PolicyRevision::new(1).expect("revision"),
                facts.platform(),
                &scope,
                requested,
                &facts,
                AbsolutePath::parse(revision_path.as_path().join("home")).expect("revision home"),
                AbsolutePath::parse(revision_path.as_path().join("tmp")).expect("revision tmp"),
                AbsolutePath::parse(revision_path.as_path().join("scratch"))
                    .expect("revision scratch"),
                vec![AbsolutePath::parse(default_store.clone()).expect("private store")],
            )
            .expect("workspace-write policy"),
        );
        let capabilities = CapabilityFs::new().expect("capabilities");
        let scratch_root = PolicyScratchRoot::acquire(
            PolicyRevision::new(1).expect("revision"),
            &capabilities,
            AbsolutePath::parse(scratch.clone()).expect("scratch path"),
            AbsolutePath::parse(scratch).expect("scratch mount"),
        )
        .expect("scratch authority");
        let fs = PolicyFs::new(snapshot, workspace, capabilities, scratch_root).expect("policy fs");

        for (label, sentinel) in [
            ("default store", default_store.join("teams.json")),
            ("sibling profile store", sibling_store.join("tokens.json")),
            ("active profile store", agent_dir.join("teams.json")),
        ] {
            let error = match fs.read_bounded(&sentinel, 64) {
                Ok(_) => panic!("{label} sentinel must not be readable"),
                Err(error) => error,
            };
            assert!(matches!(error, FsError::Denied), "{label}: {error}");
        }
        for (label, target) in [
            ("default store", default_store.join("teams-new.json")),
            ("sibling profile store", sibling_store.join("tokens-new.json")),
        ] {
            let error = match fs.create_new(&target, FileMode::Private) {
                Ok(_) => panic!("{label} must not be writable"),
                Err(error) => error,
            };
            assert!(matches!(error, FsError::Denied), "{label}: {error}");
        }
        fs.create_new(&home.join("control.txt"), FileMode::Private)
            .expect("workspace control marker must stay writable");
    }

    const fn current_host_platform() -> pi_sandbox::HostPlatform {
        #[cfg(target_os = "linux")]
        {
            pi_sandbox::HostPlatform::Linux
        }
        #[cfg(target_os = "macos")]
        {
            pi_sandbox::HostPlatform::Macos
        }
        #[cfg(target_os = "windows")]
        {
            pi_sandbox::HostPlatform::Windows
        }
    }
}
