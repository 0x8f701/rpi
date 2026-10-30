use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use cap_std::fs::Dir;
use nix::libc;

use super::{EntryKind, FileIdentity, FileMode, FsError, FsOperation, RootCreation};

pub(super) fn open_root(
    path: &Path,
    creation: RootCreation,
    mode: FileMode,
) -> Result<Dir, FsError> {
    if !path.is_absolute() {
        return Err(FsError::InvalidPath);
    }
    let root = open_absolute_root()?;
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
    mkdirat(parent.as_raw_fd(), name, mode)?;
    let opened = openat(
        parent.as_raw_fd(),
        name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        0,
        FsOperation::OpenEntry,
    )?;
    if matches!(mode, FileMode::Private) {
        set_mode(&opened, file_mode(mode, true), FsOperation::CreateDirectory)?;
    }
    Ok(())
}

pub(super) fn open_read_file(directory: &Dir, relative: &Path) -> Result<File, FsError> {
    open_file(
        directory,
        relative,
        libc::O_RDONLY | libc::O_CLOEXEC,
        0,
        FsOperation::OpenEntry,
    )
}

pub(super) fn open_read_write_file(directory: &Dir, relative: &Path) -> Result<File, FsError> {
    open_file(
        directory,
        relative,
        libc::O_RDWR | libc::O_CLOEXEC,
        0,
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
        libc::O_RDWR | libc::O_CLOEXEC | libc::O_CREAT | libc::O_EXCL,
        file_mode(mode, false),
        FsOperation::CreateFile,
    )
}

pub(super) fn directory_identity(directory: &Dir) -> Result<FileIdentity, FsError> {
    identity_from_fd(directory.as_raw_fd(), Some(EntryKind::Directory))
}

pub(super) fn file_identity(file: &File) -> Result<FileIdentity, FsError> {
    identity_from_fd(file.as_raw_fd(), Some(EntryKind::RegularFile))
}

pub(super) fn entry_identity(directory: &Dir, relative: &Path) -> Result<FileIdentity, FsError> {
    let (parent, name) = open_parent(directory, relative)?;
    let name = cstring(name)?;
    let mut status = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            status.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        return Err(last_error(FsOperation::OpenEntry));
    }
    identity_from_stat(unsafe { status.assume_init() })
}

/// Stable identity of the symbolic-link entry itself, without following it.
pub(super) fn managed_link_identity(
    directory: &Dir,
    relative: &Path,
) -> Result<super::ManagedLinkIdentity, FsError> {
    let (parent, name) = open_parent(directory, relative)?;
    let name = cstring(name)?;
    let mut status = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            status.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(last_error(FsOperation::OpenEntry));
    }
    let status = unsafe { status.assume_init() };
    if status.st_mode & libc::S_IFMT != libc::S_IFLNK {
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
    let name = cstring(name)?;
    let mut status = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            status.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        return Err(last_error(FsOperation::OpenEntry));
    }
    Ok(unsafe { status.assume_init() }.st_mode & libc::S_IFMT == libc::S_IFLNK)
}

/// Read the raw target text of a symbolic link, anchored at the containing
/// directory. Fails with [`FsError::InvalidPath`] when the target does not
/// fit the bounded buffer (PATH_MAX).
pub(super) fn read_link(directory: &Dir, relative: &Path) -> Result<PathBuf, FsError> {
    let (parent, name) = open_parent(directory, relative)?;
    let name = cstring(name)?;
    let mut buffer = vec![0u8; 4096];
    let written = unsafe {
        libc::readlinkat(
            parent.as_raw_fd(),
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
        )
    };
    if written < 0 {
        return Err(last_error(FsOperation::OpenEntry));
    }
    let written = written as usize;
    if written >= buffer.len() {
        return Err(FsError::InvalidPath);
    }
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(
        &buffer[..written],
    )))
}

/// Identity of the entry the final path component resolves to (following any
/// symbolic link chain), anchored at the containing directory.
pub(super) fn followed_entry_identity(
    directory: &Dir,
    relative: &Path,
) -> Result<FileIdentity, FsError> {
    let (parent, name) = open_parent(directory, relative)?;
    let name = cstring(name)?;
    let mut status = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result =
        unsafe { libc::fstatat(parent.as_raw_fd(), name.as_ptr(), status.as_mut_ptr(), 0) };
    if result != 0 {
        return Err(last_error(FsOperation::OpenEntry));
    }
    identity_from_stat(unsafe { status.assume_init() })
}

pub(super) fn sync_directory(directory: &Dir) -> Result<(), FsError> {
    directory
        .try_clone()
        .and_then(|directory| directory.into_std_file().sync_all())
        .map_err(|error| FsError::from_io(FsOperation::Sync, &error))
}

fn open_absolute_root() -> Result<File, FsError> {
    let path = CString::new("/").expect("literal root has no NUL");
    let descriptor = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    owned_file(descriptor, FsOperation::OpenRoot)
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
        let opened = openat(
            directory.as_raw_fd(),
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0,
            FsOperation::OpenEntry,
        );
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
                match mkdirat(directory.as_raw_fd(), name, mode) {
                    Ok(()) => {}
                    // create_dir_all is idempotent: a concurrent (or prior)
                    // creation may have won the race. Treat AlreadyExists as
                    // success and let the follow-up openat (O_DIRECTORY +
                    // O_NOFOLLOW) re-verify it is a real directory, rejecting
                    // symlinks and non-directories as usual.
                    Err(FsError::System {
                        kind: io::ErrorKind::AlreadyExists,
                        ..
                    }) => {}
                    Err(error) => return Err(error),
                }
                let opened = openat(
                    directory.as_raw_fd(),
                    name,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                    0,
                    FsOperation::OpenEntry,
                )?;
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
    flags: i32,
    mode: libc::mode_t,
    operation: FsOperation,
) -> Result<File, FsError> {
    let (parent, name) = open_parent(directory, relative)?;
    let enumerated = entry_identity(&parent, Path::new(name)).ok();
    let file = openat(
        parent.as_raw_fd(),
        name,
        flags | libc::O_NOFOLLOW,
        mode,
        operation,
    )?;
    let opened = identity_from_fd(file.as_raw_fd(), None)?;
    if enumerated.is_some_and(|identity| identity != opened) {
        return Err(FsError::IdentityChanged);
    }
    Ok(file)
}

fn open_parent<'a>(directory: &Dir, relative: &'a Path) -> Result<(Dir, &'a OsStr), FsError> {
    let name = relative.file_name().ok_or(FsError::InvalidPath)?;
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    open_directory(directory, parent).map(|parent| (parent, name))
}

fn openat(
    directory: RawFd,
    name: &OsStr,
    flags: i32,
    mode: libc::mode_t,
    operation: FsOperation,
) -> Result<File, FsError> {
    let name = cstring(name)?;
    let descriptor = unsafe { libc::openat(directory, name.as_ptr(), flags, mode) };
    owned_file(descriptor, operation)
}

fn mkdirat(directory: RawFd, name: &OsStr, mode: FileMode) -> Result<(), FsError> {
    let name = cstring(name)?;
    let result = unsafe { libc::mkdirat(directory, name.as_ptr(), file_mode(mode, true)) };
    if result == 0 {
        Ok(())
    } else {
        Err(last_error(FsOperation::CreateDirectory))
    }
}

fn set_mode(file: &File, mode: libc::mode_t, operation: FsOperation) -> Result<(), FsError> {
    if unsafe { libc::fchmod(file.as_raw_fd(), mode) } == 0 {
        Ok(())
    } else {
        Err(last_error(operation))
    }
}

fn identity_from_fd(
    descriptor: RawFd,
    required_kind: Option<EntryKind>,
) -> Result<FileIdentity, FsError> {
    let mut status = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(descriptor, status.as_mut_ptr()) } != 0 {
        return Err(last_error(FsOperation::OpenEntry));
    }
    let identity = identity_from_stat(unsafe { status.assume_init() })?;
    if required_kind.is_some_and(|kind| kind != identity.kind()) {
        return Err(FsError::UnsupportedEntryType);
    }
    Ok(identity)
}

fn identity_from_stat(status: libc::stat) -> Result<FileIdentity, FsError> {
    let kind = match status.st_mode & libc::S_IFMT {
        libc::S_IFREG => EntryKind::RegularFile,
        libc::S_IFDIR => EntryKind::Directory,
        _ => return Err(FsError::UnsupportedEntryType),
    };
    Ok(FileIdentity::Unix {
        device: status.st_dev,
        inode: status.st_ino,
        kind,
    })
}

fn cstring(value: &OsStr) -> Result<CString, FsError> {
    CString::new(value.as_bytes()).map_err(|_| FsError::InvalidPath)
}

fn owned_file(descriptor: i32, operation: FsOperation) -> Result<File, FsError> {
    if descriptor < 0 {
        Err(last_error(operation))
    } else {
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }
}

fn last_error(operation: FsOperation) -> FsError {
    FsError::from_io(operation, &io::Error::last_os_error())
}

const fn file_mode(mode: FileMode, directory: bool) -> libc::mode_t {
    match (mode, directory) {
        (FileMode::Private, true) => 0o700,
        (FileMode::Private, false) => 0o600,
        (FileMode::Shared, true) => 0o755,
        (FileMode::Shared, false) => 0o644,
    }
}
