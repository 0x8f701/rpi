use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

use cap_std::fs::Dir;
use rustix::fs::{
    AtFlags, FileType, Mode, OFlags, ResolveFlags, Stat, fchmod, fstat, mkdirat, openat2, statat,
};

use super::{EntryKind, FileIdentity, FileMode, FsError, FsOperation, RootCreation};

pub(super) fn open_root(
    path: &Path,
    creation: RootCreation,
    mode: FileMode,
) -> Result<Dir, FsError> {
    if !path.is_absolute() {
        return Err(FsError::InvalidPath);
    }
    let root = rustix::fs::open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| errno(FsOperation::OpenRoot, error))?;
    let relative = path
        .strip_prefix(Path::new("/"))
        .map_err(|_| FsError::InvalidPath)?;
    if relative.as_os_str().is_empty() {
        return Ok(Dir::from_std_file(root));
    }
    open_directory_components(
        root,
        relative,
        matches!(creation, RootCreation::CreateDirectory),
        mode,
    )
    .map(Dir::from_std_file)
}

pub(super) fn open_directory(directory: &Dir, relative: &Path) -> Result<Dir, FsError> {
    if relative.as_os_str().is_empty() {
        return directory
            .try_clone()
            .map_err(|error| FsError::from_io(FsOperation::OpenEntry, &error));
    }
    let root = directory
        .try_clone()
        .map_err(|error| FsError::from_io(FsOperation::OpenEntry, &error))?
        .into_std_file();
    open_directory_components(root, relative, false, FileMode::Shared).map(Dir::from_std_file)
}

pub(super) fn create_dir_all(
    directory: &Dir,
    relative: &Path,
    mode: FileMode,
) -> Result<(), FsError> {
    if relative.as_os_str().is_empty() {
        return Ok(());
    }
    let root = directory
        .try_clone()
        .map_err(|error| FsError::from_io(FsOperation::CreateDirectory, &error))?
        .into_std_file();
    let _ = open_directory_components(root, relative, true, mode)?;
    Ok(())
}

/// Creates the final path component exclusively. An existing entry — file,
/// directory, or symlink — is an error, so the caller can never claim
/// ownership of a directory it did not create.
pub(super) fn create_directory_exclusive(
    directory: &Dir,
    relative: &Path,
    mode: FileMode,
) -> Result<(), FsError> {
    let (parent, name) = open_parent(directory, relative)?;
    mkdirat(parent.as_fd(), name, file_mode(mode, true))
        .map_err(|error| errno(FsOperation::CreateDirectory, error))?;
    let opened = openat2(
        parent.as_fd(),
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS | ResolveFlags::NO_SYMLINKS,
    )
    .map(File::from)
    .map_err(|error| errno(FsOperation::OpenEntry, error))?;
    if matches!(mode, FileMode::Private) {
        set_mode(&opened, file_mode(mode, true), FsOperation::CreateDirectory)?;
    }
    Ok(())
}

pub(super) fn open_read_file(directory: &Dir, relative: &Path) -> Result<File, FsError> {
    open_file(
        directory,
        relative,
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
        FsOperation::OpenEntry,
    )
}

pub(super) fn open_read_write_file(directory: &Dir, relative: &Path) -> Result<File, FsError> {
    open_file(
        directory,
        relative,
        OFlags::RDWR | OFlags::CLOEXEC,
        Mode::empty(),
        FsOperation::OpenEntry,
    )
}

pub(super) fn create_new_file(
    directory: &Dir,
    relative: &Path,
    mode: FileMode,
) -> Result<File, FsError> {
    open_file(
        directory,
        relative,
        OFlags::RDWR | OFlags::CLOEXEC | OFlags::CREATE | OFlags::EXCL,
        file_mode(mode, false),
        FsOperation::CreateFile,
    )
}

pub(super) fn directory_identity(directory: &Dir) -> Result<FileIdentity, FsError> {
    identity_from_stat(
        fstat(directory.as_fd()).map_err(|error| errno(FsOperation::OpenEntry, error))?,
        Some(EntryKind::Directory),
    )
}

pub(super) fn file_identity(file: &File) -> Result<FileIdentity, FsError> {
    identity_from_stat(
        fstat(file).map_err(|error| errno(FsOperation::OpenEntry, error))?,
        Some(EntryKind::RegularFile),
    )
}

pub(super) fn entry_identity(directory: &Dir, relative: &Path) -> Result<FileIdentity, FsError> {
    let (parent, name) = open_parent(directory, relative)?;
    let status = statat(parent.as_fd(), name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|error| errno(FsOperation::OpenEntry, error))?;
    identity_from_stat(status, None)
}

/// Stable identity of the symbolic-link entry itself, without following it.
pub(super) fn managed_link_identity(
    directory: &Dir,
    relative: &Path,
) -> Result<super::ManagedLinkIdentity, FsError> {
    let (parent, name) = open_parent(directory, relative)?;
    let status = statat(parent.as_fd(), name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|error| errno(FsOperation::OpenEntry, error))?;
    if FileType::from_raw_mode(status.st_mode) != FileType::Symlink {
        return Err(FsError::IdentityChanged);
    }
    Ok(super::ManagedLinkIdentity {
        device: status.st_dev,
        inode: status.st_ino,
    })
}

/// Whether the final path component is a symbolic link (not followed).
pub(super) fn entry_is_symlink(directory: &Dir, relative: &Path) -> Result<bool, FsError> {
    let (parent, name) = open_parent(directory, relative)?;
    let status = statat(parent.as_fd(), name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|error| errno(FsOperation::OpenEntry, error))?;
    Ok(FileType::from_raw_mode(status.st_mode) == FileType::Symlink)
}

/// Read the raw target text of a symbolic link, anchored at the containing
/// directory. rustix grows the read buffer as needed, so no fixed-size
/// truncation path exists.
pub(super) fn read_link(directory: &Dir, relative: &Path) -> Result<PathBuf, FsError> {
    let (parent, name) = open_parent(directory, relative)?;
    let target = rustix::fs::readlinkat(parent.as_fd(), name, Vec::with_capacity(4096))
        .map_err(|error| errno(FsOperation::OpenEntry, error))?;
    Ok(PathBuf::from(std::ffi::OsString::from_vec(
        target.into_bytes(),
    )))
}

/// Identity of the entry the final path component resolves to (following any
/// symbolic link chain), anchored at the containing directory.
pub(super) fn followed_entry_identity(
    directory: &Dir,
    relative: &Path,
) -> Result<FileIdentity, FsError> {
    let (parent, name) = open_parent(directory, relative)?;
    let status = statat(parent.as_fd(), name, AtFlags::empty())
        .map_err(|error| errno(FsOperation::OpenEntry, error))?;
    identity_from_stat(status, None)
}

pub(super) fn sync_directory(directory: &Dir) -> Result<(), FsError> {
    directory
        .try_clone()
        .and_then(|directory| directory.into_std_file().sync_all())
        .map_err(|error| FsError::from_io(FsOperation::Sync, &error))
}

fn open_directory_components(
    mut directory: File,
    relative: &Path,
    create: bool,
    mode: FileMode,
) -> Result<File, FsError> {
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            if matches!(component, Component::CurDir) {
                continue;
            }
            return Err(FsError::InvalidPath);
        };
        let is_final = components.peek().is_none();
        let opened = openat2(
            &directory,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS | ResolveFlags::NO_SYMLINKS,
        )
        .map(File::from)
        .map_err(|error| errno(FsOperation::OpenEntry, error));
        directory = match opened {
            Ok(opened) => {
                if create && is_final && matches!(mode, FileMode::Private) {
                    set_mode(&opened, file_mode(mode, true), FsOperation::CreateDirectory)?;
                }
                opened
            }
            Err(FsError::System {
                kind: io::ErrorKind::NotFound,
                ..
            }) if create => {
                if let Err(error) = mkdirat(&directory, name, file_mode(mode, true)) {
                    // create_dir_all is idempotent: a concurrent (or prior)
                    // creation may have won the race. Treat AlreadyExists as
                    // success and let the follow-up openat2 (O_DIRECTORY +
                    // NO_SYMLINKS) re-verify it is a real directory, rejecting
                    // symlinks and non-directories as usual.
                    if error != rustix::io::Errno::EXIST {
                        return Err(errno(FsOperation::CreateDirectory, error));
                    }
                }
                let opened = openat2(
                    &directory,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                    Mode::empty(),
                    ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS | ResolveFlags::NO_SYMLINKS,
                )
                .map(File::from)
                .map_err(|error| errno(FsOperation::OpenEntry, error))?;
                if matches!(mode, FileMode::Private) {
                    set_mode(&opened, file_mode(mode, true), FsOperation::CreateDirectory)?;
                }
                opened
            }
            Err(error) => return Err(error),
        };
    }
    Ok(directory)
}

fn open_file(
    directory: &Dir,
    relative: &Path,
    flags: OFlags,
    mode: Mode,
    operation: FsOperation,
) -> Result<File, FsError> {
    let (parent, name) = open_parent(directory, relative)?;
    openat2(
        parent.as_fd(),
        name,
        flags,
        mode,
        ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS | ResolveFlags::NO_SYMLINKS,
    )
    .map(File::from)
    .map_err(|error| errno(operation, error))
}

fn open_parent<'a>(
    directory: &Dir,
    relative: &'a Path,
) -> Result<(Dir, &'a std::ffi::OsStr), FsError> {
    let name = relative.file_name().ok_or(FsError::InvalidPath)?;
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    open_directory(directory, parent).map(|parent| (parent, name))
}

fn set_mode(file: &File, mode: Mode, operation: FsOperation) -> Result<(), FsError> {
    fchmod(file, mode).map_err(|error| errno(operation, error))
}

fn identity_from_stat(
    status: Stat,
    required_kind: Option<EntryKind>,
) -> Result<FileIdentity, FsError> {
    let kind = match FileType::from_raw_mode(status.st_mode) {
        FileType::RegularFile => EntryKind::RegularFile,
        FileType::Directory => EntryKind::Directory,
        _ => return Err(FsError::UnsupportedEntryType),
    };
    if required_kind.is_some_and(|required| required != kind) {
        return Err(FsError::UnsupportedEntryType);
    }
    Ok(FileIdentity::Unix {
        device: status.st_dev,
        inode: status.st_ino,
        kind,
    })
}

fn errno(operation: FsOperation, error: rustix::io::Errno) -> FsError {
    FsError::from_io(
        operation,
        &io::Error::from_raw_os_error(error.raw_os_error()),
    )
}

const fn file_mode(mode: FileMode, directory: bool) -> Mode {
    Mode::from_raw_mode(match (mode, directory) {
        (FileMode::Private, true) => 0o700,
        (FileMode::Private, false) => 0o600,
        (FileMode::Shared, true) => 0o755,
        (FileMode::Shared, false) => 0o644,
    })
}
