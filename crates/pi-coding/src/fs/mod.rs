mod capability;
mod policy;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

/// Resolves a path's symlinks, returning `None` when the path does not
/// exist or cannot be canonicalized. Callers keep their own fallback.
pub(crate) fn canonicalize_existing_path(path: &std::path::Path) -> Option<std::path::PathBuf> {
    std::fs::canonicalize(path).ok()
}
pub(crate) use capability::{
    CapabilityAccess, CapabilityDirEntry, CapabilityDirectory, CapabilityEntry,
    CapabilityEntryAuthority, CapabilityExecutableSource, CapabilityFile, CapabilityFs,
    CapabilityReadDir, CapabilityRevisionRoots, CapabilityRootLease, CapabilityRootPurpose,
    CapabilitySpawnSource, CapabilityWorkingDirectorySource, CapabilityWritableFile, EntryKind,
    FileIdentity, FileMode, FsMetadata, ManagedCapabilityFile, ManagedLinkIdentity, Replacement,
    ReplacementTarget, RootCreation, StreamCopyBuffer, StreamingReplacement, is_link_blocked,
};
pub use capability::{FsError, FsOperation, SessionFileIdentity};
pub(crate) use policy::{
    FsAuthority, PolicyDirEntry, PolicyDirectory, PolicyFile, PolicyFs, PolicyOpenedEntry,
    PolicyReadDir, PolicyReplacement, PolicyScratch, PolicyScratchRoot, PolicyWritableFile,
    SandboxVisiblePath, ScratchLease, ScratchPurpose, ScratchReplacement,
};
