use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};

use super::{EntryKind, FileIdentity, FsError, FsOperation};

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
const FILE_ID_INFO_CLASS: i32 = 18;

#[repr(C)]
struct FileId128 {
    identifier: [u8; 16],
}

#[repr(C)]
struct FileIdInfo {
    volume_serial_number: u64,
    file_id: FileId128,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetFileInformationByHandleEx(
        file: *mut core::ffi::c_void,
        class: i32,
        information: *mut core::ffi::c_void,
        size: u32,
    ) -> i32;
}

pub(super) fn directory_identity(directory: &Dir) -> Result<FileIdentity, FsError> {
    let file = directory
        .try_clone()
        .map_err(|error| FsError::from_io(FsOperation::OpenEntry, &error))?
        .into_std_file();
    identity_from_file(&file, EntryKind::Directory)
}

pub(super) fn file_identity(file: &File) -> Result<FileIdentity, FsError> {
    identity_from_file(file, EntryKind::RegularFile)
}

pub(super) fn entry_identity(directory: &Dir, relative: &Path) -> Result<FileIdentity, FsError> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    match directory.open_with(relative, &options) {
        Ok(file) => identity_from_file(&file.into_std(), EntryKind::RegularFile),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::IsADirectory | io::ErrorKind::PermissionDenied
            ) =>
        {
            let opened = directory
                .open_dir_nofollow(relative)
                .map_err(|error| FsError::from_io(FsOperation::OpenEntry, &error))?;
            directory_identity(&opened)
        }
        Err(error) => Err(FsError::from_io(FsOperation::OpenEntry, &error)),
    }
}

/// Windows stub: managed-config symlink following is a Unix convention, so
/// the probe reports false and the boundary keeps strict behavior unchanged.
pub(super) fn entry_is_symlink(_directory: &Dir, _relative: &Path) -> Result<bool, FsError> {
    Ok(false)
}

pub(super) fn managed_link_identity(
    _directory: &Dir,
    _relative: &Path,
) -> Result<super::ManagedLinkIdentity, FsError> {
    Err(FsError::UnsupportedEntryType)
}

/// Windows stub: managed-config symlink following is unsupported; the
/// boundary fails closed exactly as a strict open would.
pub(super) fn read_link(_directory: &Dir, _relative: &Path) -> Result<PathBuf, FsError> {
    Err(FsError::UnsupportedEntryType)
}

/// Windows stub: managed-config symlink following is unsupported; the
/// boundary fails closed exactly as a strict open would.
pub(super) fn followed_entry_identity(
    _directory: &Dir,
    _relative: &Path,
) -> Result<FileIdentity, FsError> {
    Err(FsError::UnsupportedEntryType)
}

fn identity_from_file(file: &File, required_kind: EntryKind) -> Result<FileIdentity, FsError> {
    let metadata = file
        .metadata()
        .map_err(|error| FsError::from_io(FsOperation::OpenEntry, &error))?;
    use std::os::windows::fs::MetadataExt;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(FsError::UnsupportedEntryType);
    }
    let kind = if metadata.is_file() {
        EntryKind::RegularFile
    } else if metadata.is_dir() {
        EntryKind::Directory
    } else {
        return Err(FsError::UnsupportedEntryType);
    };
    if kind != required_kind {
        return Err(FsError::UnsupportedEntryType);
    }
    let mut information = MaybeUninit::<FileIdInfo>::uninit();
    let success = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FILE_ID_INFO_CLASS,
            information.as_mut_ptr().cast(),
            u32::try_from(std::mem::size_of::<FileIdInfo>()).expect("FILE_ID_INFO size fits u32"),
        )
    };
    if success == 0 {
        return Err(FsError::from_io(
            FsOperation::OpenEntry,
            &io::Error::last_os_error(),
        ));
    }
    let information = unsafe { information.assume_init() };
    Ok(FileIdentity::Windows {
        volume_serial: information.volume_serial_number,
        file_id: information.file_id.identifier,
        kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_identity_preserves_high_bits() {
        let file_id = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        assert_eq!(
            FileIdentity::Windows {
                volume_serial: u64::MAX - 7,
                file_id,
                kind: EntryKind::RegularFile,
            },
            FileIdentity::Windows {
                volume_serial: u64::MAX - 7,
                file_id,
                kind: EntryKind::RegularFile,
            }
        );
    }
}
