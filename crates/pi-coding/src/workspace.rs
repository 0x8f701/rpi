use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use anyhow::{Result, anyhow, bail};

/// Lexically normalized filesystem roots selected for a coding session.
/// Each root retains the exact no-follow directory authority used by the
/// runtime resource and sandbox generations.
#[derive(Clone)]
pub struct WorkspaceRoots {
    roots: Arc<[PathBuf]>,
    authorities: Arc<[crate::fs::CapabilityRootLease]>,
}

impl WorkspaceRoots {
    pub fn new<I, P>(cwd: impl AsRef<Path>, additional_roots: I) -> Result<Self>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let cwd = canonical_directory(cwd.as_ref(), "working directory")?;
        let mut candidates = additional_roots
            .into_iter()
            .map(|root| canonical_directory(root.as_ref(), "additional workspace directory"))
            .collect::<Result<Vec<_>>>()?;
        candidates.sort();
        candidates.dedup();
        let mut additional = Vec::<PathBuf>::new();
        for root in candidates {
            if root.starts_with(&cwd)
                || additional.iter().any(|existing| root.starts_with(existing))
            {
                continue;
            }
            additional.push(root);
        }
        let capabilities = crate::fs::CapabilityFs::new()?;
        let mut roots = Vec::with_capacity(additional.len() + 1);
        roots.push(cwd);
        roots.extend(additional);
        let authorities = roots
            .iter()
            .map(|root| open_workspace_root(&capabilities, root))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            roots: roots.into(),
            authorities: authorities.into(),
        })
    }
    pub(crate) fn for_tool_factory(cwd: &str) -> Self {
        Self::new(cwd, Vec::<PathBuf>::new()).unwrap_or_else(|_| {
            let root = lexical_absolute(Path::new(cwd)).unwrap_or_else(|_| PathBuf::from(cwd));
            Self {
                roots: vec![root].into(),
                authorities: Vec::new().into(),
            }
        })
    }

    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.roots[0]
    }

    #[must_use]
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    #[must_use]
    pub fn additional_roots(&self) -> &[PathBuf] {
        &self.roots[1..]
    }

    pub(crate) fn capabilities(&self) -> Option<Arc<crate::fs::CapabilityFs>> {
        self.authorities.first().map(|lease| lease.capabilities())
    }

    pub(crate) fn authority(&self, index: usize) -> Option<&crate::fs::CapabilityRootLease> {
        self.authorities.get(index)
    }

    pub fn validate_authorities(&self) -> Result<()> {
        for authority in self.authorities.iter() {
            authority.validate_live_root()?;
        }
        Ok(())
    }
}

fn open_workspace_root(
    capabilities: &Arc<crate::fs::CapabilityFs>,
    path: &Path,
) -> Result<crate::fs::CapabilityRootLease> {
    let absolute = pi_sandbox::AbsolutePath::parse(path.to_path_buf())?;
    let (parent, name) = capabilities.open_exact_parent(
        crate::fs::CapabilityRootPurpose::ProjectResources,
        absolute,
        crate::fs::CapabilityAccess::ReadWrite,
        crate::fs::RootCreation::Existing,
    )?;
    parent
        .lease_directory(
            Path::new(&name),
            crate::fs::CapabilityAccess::ReadWrite,
            crate::fs::RootCreation::Existing,
        )
        .map_err(Into::into)
}
/// A no-follow inspection of an explicit operator-selected directory.
///
/// The retained capability pins the inspected directory generation while the
/// display path remains the user-facing cwd coordinate. Callers must keep this
/// value alive through preparation and validate it before publication.
pub struct PreparedOperatorDirectory {
    path: PathBuf,
    identity: crate::fs::FileIdentity,
    lease: crate::fs::CapabilityRootLease,
}

impl PreparedOperatorDirectory {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn validate(&self) -> Result<()> {
        self.lease.validate_live_root()?;
        let capabilities = crate::fs::CapabilityFs::new()?;
        let path = pi_sandbox::AbsolutePath::parse(self.path.clone())?;
        let authority = capabilities.open_entry_authority(
            crate::fs::CapabilityRootPurpose::OperatorImport,
            path,
        )?;
        let crate::fs::CapabilityEntryAuthority::Directory(current) = authority else {
            bail!(crate::FsError::IdentityChanged);
        };
        if current.identity() != self.identity {
            bail!(crate::FsError::IdentityChanged);
        }
        Ok(())
    }
}

impl PreparedOperatorDirectory {
    pub fn into_workspace<I, P>(self, additional_roots: I) -> Result<WorkspaceRoots>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut candidates = additional_roots
            .into_iter()
            .map(|root| canonical_directory(root.as_ref(), "additional workspace directory"))
            .collect::<Result<Vec<_>>>()?;
        candidates.sort();
        candidates.dedup();
        let mut roots = vec![self.path];
        let mut authorities = vec![self.lease];
        for root in candidates {
            if root.starts_with(&roots[0]) || roots[1..].iter().any(|existing| root.starts_with(existing)) {
                continue;
            }
            let authority = open_workspace_root(&authorities[0].capabilities(), &root)?;
            roots.push(root);
            authorities.push(authority);
        }
        Ok(WorkspaceRoots {
            roots: roots.into(),
            authorities: authorities.into(),
        })
    }
}
impl fmt::Debug for PreparedOperatorDirectory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedOperatorDirectory")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OperatorDirectoryError {
    #[error("operator directory must not be a symbolic link")]
    SymbolicLink,
}

/// Inspect an explicit absolute directory once and retain its no-follow
/// capability identity for the lifetime of the returned preparation.
pub fn prepare_operator_directory(path: &Path) -> Result<PreparedOperatorDirectory> {
    if !path.is_absolute() {
        bail!("operator directory must be absolute");
    }
    let absolute = lexical_absolute(path)?;
    let capabilities = crate::fs::CapabilityFs::new()?;
    let absolute_path = pi_sandbox::AbsolutePath::parse(absolute)?;
    let (parent, name) = capabilities.open_exact_parent(
        crate::fs::CapabilityRootPurpose::ProjectResources,
        absolute_path,
        crate::fs::CapabilityAccess::ReadWrite,
        crate::fs::RootCreation::Existing,
    )?;
    let lease = match parent.lease_directory(
        Path::new(&name),
        crate::fs::CapabilityAccess::ReadWrite,
        crate::fs::RootCreation::Existing,
    ) {
        Err(crate::FsError::UnsupportedEntryType)
            if parent.metadata_allowing_link(Path::new(&name)).is_ok() =>
        {
            bail!(OperatorDirectoryError::SymbolicLink);
        }
        result => result?,
    };
    let identity = lease.identity();
    let path = lease.display_root().to_path_buf();
    Ok(PreparedOperatorDirectory {
        path,
        identity,
        lease,
    })
}


fn canonical_directory(path: &Path, description: &str) -> Result<PathBuf> {
    lexical_absolute(path).map_err(|_| anyhow::anyhow!("invalid {description}: {}", path.display()))
}

fn lexical_absolute(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    bail!("path escapes filesystem root");
                }
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    Ok(normalized)
}

/// Opens an explicit operator-selected directory without following symlinks.
pub fn inspect_operator_directory(path: &Path) -> Result<PathBuf> {
    prepare_operator_directory(path).map(|prepared| prepared.path)
}

/// Create a missing leaf directory under an already-validated operator
/// directory. The parent is opened no-follow and the leaf is created through
/// the held parent lease (dirfd-relative creation), so a symlink-swap of the
/// parent between validation and creation cannot redirect the write; the
/// leaf itself is never followed. An existing leaf is idempotent. Returns
/// the leaf's no-follow display path.
pub fn create_operator_leaf_directory(parent: &Path, leaf: &std::ffi::OsStr) -> Result<PathBuf> {
    let prepared = prepare_operator_directory(parent)?;
    let child = prepared.lease.lease_directory(
        Path::new(leaf),
        crate::fs::CapabilityAccess::ReadWrite,
        crate::fs::RootCreation::CreateDirectory,
    )?;
    Ok(child.display_root().to_path_buf())
}

/// Prepare an explicit absolute operator directory, creating it when missing.
/// A missing path is located against its deepest existing ancestor, which is
/// opened with a no-follow capability lease; the remaining components are
/// created beneath that held dirfd (`lease_directory` with
/// `RootCreation::CreateDirectory` — mkdirat relative to the retained
/// descriptor, openat2 BENEATH/NO_SYMLINKS), so a symlink inserted into any
/// segment during or before creation cannot redirect the write. The final
/// lease is returned unchanged, so an existing directory behaves exactly
/// like [`prepare_operator_directory`] and every created directory is
/// validated by the same capability path.
pub fn prepare_or_create_operator_directory(path: &Path) -> Result<PreparedOperatorDirectory> {
    if !path.is_absolute() {
        bail!("operator directory must be absolute");
    }
    let absolute = lexical_absolute(path)?;
    if let Ok(prepared) = prepare_operator_directory(&absolute) {
        return Ok(prepared);
    }
    let capabilities = crate::fs::CapabilityFs::new()?;
    let mut ancestor = absolute.clone();
    loop {
        let parent = match ancestor.parent() {
            Some(parent) if parent != ancestor => parent.to_path_buf(),
            _ => bail!("operator directory path is invalid"),
        };
        let parent_absolute = pi_sandbox::AbsolutePath::parse(parent.clone())?;
        match capabilities.open_root(
            crate::fs::CapabilityRootPurpose::ProjectResources,
            parent_absolute,
            crate::fs::CapabilityAccess::ReadWrite,
            crate::fs::RootCreation::Existing,
        ) {
            Ok(lease) => {
                let suffix = absolute
                    .strip_prefix(lease.display_root())
                    .map_err(|_| anyhow!("operator directory path is invalid"))?;
                let lease = match lease.lease_directory(
                    Path::new(suffix),
                    crate::fs::CapabilityAccess::ReadWrite,
                    crate::fs::RootCreation::CreateDirectory,
                ) {
                    Ok(lease) => lease,
                    Err(error) => {
                        // Creation failure is re-classified through the
                        // capability validation so malformed paths, symlinks,
                        // and file conflicts surface their precise categories
                        // instead of an OS error.
                        return match prepare_operator_directory(&absolute) {
                            Ok(prepared) => Ok(prepared),
                            Err(classified) => {
                                Err(anyhow::Error::from(classified).context(error.to_string()))
                            }
                        };
                    }
                };
                let created = lease.display_root().to_path_buf();
                let identity = lease.identity();
                return Ok(PreparedOperatorDirectory {
                    path: created,
                    identity,
                    lease,
                });
            }
            Err(crate::fs::FsError::System {
                kind: std::io::ErrorKind::NotFound,
                ..
            }) => ancestor = parent,
            Err(error) => return Err(anyhow::Error::new(error)),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operator_directory_rejects_missing_and_non_directory_paths() {
        let cwd = tempfile::tempdir().expect("cwd");
        let missing = cwd.path().join("missing");
        assert!(inspect_operator_directory(&missing).is_err());

        let file = cwd.path().join("file.txt");
        std::fs::write(&file, "x").expect("file");
        assert!(inspect_operator_directory(&file).is_err());
    }

    #[test]
    fn prepare_or_create_creates_missing_nested_directory_idempotently() {
        let cwd = tempfile::tempdir().expect("cwd");
        let missing = cwd.path().join("projects").join("new-workspace");
        let prepared = prepare_or_create_operator_directory(&missing).expect("create");
        assert_eq!(prepared.path, missing);
        assert!(missing.is_dir());
        let again = prepare_or_create_operator_directory(&missing).expect("idempotent");
        assert_eq!(again.path, missing);
    }

    #[test]
    #[cfg(unix)]
    fn prepare_or_create_does_not_write_through_symlinked_ancestor() {
        let cwd = tempfile::tempdir().expect("cwd");
        let victim = tempfile::tempdir().expect("victim");
        let link = cwd.path().join("project");
        std::os::unix::fs::symlink(&victim, &link).expect("symlink");
        let requested = link.join("new-workspace");
        assert!(prepare_or_create_operator_directory(&requested).is_err());
        assert!(
            std::fs::read_dir(victim.path()).expect("victim").next().is_none(),
            "no directory may be created inside the symlink target"
        );
    }

    #[test]
    fn prepare_or_create_rejects_existing_file_path() {
        let cwd = tempfile::tempdir().expect("cwd");
        let file = cwd.path().join("file.txt");
        std::fs::write(&file, "x").expect("file");
        assert!(prepare_or_create_operator_directory(&file).is_err());
        assert!(!file.is_dir());
    }

    #[test]
    #[cfg(unix)]
    fn prepare_or_create_rejects_symlinked_leaf() {
        let cwd = tempfile::tempdir().expect("cwd");
        let outside = tempfile::tempdir().expect("outside");
        let leaf = cwd.path().join("leaf");
        std::os::unix::fs::symlink(&outside, &leaf).expect("symlink");
        assert!(prepare_or_create_operator_directory(&leaf).is_err());
        assert!(
            leaf.symlink_metadata().expect("leaf").file_type().is_symlink(),
            "symlink must not be replaced by a directory"
        );
    }

    #[test]
    fn prepare_or_create_requires_absolute_path() {
        assert!(prepare_or_create_operator_directory(Path::new("relative/dir")).is_err());
    }

    #[test]
    fn prepared_operator_directory_detects_replacement() {
        let parent = tempfile::tempdir().expect("parent");
        let selected = parent.path().join("selected");
        std::fs::create_dir(&selected).expect("selected");
        let prepared = prepare_operator_directory(&selected).expect("prepare directory");
        let replaced = parent.path().join("replaced");
        std::fs::rename(&selected, &replaced).expect("replace selected directory");
        std::fs::create_dir(&selected).expect("replacement directory");
        assert!(prepared.validate().is_err());
    }

    #[test]
    fn canonicalizes_and_deduplicates_nested_roots_deterministically() {
        let cwd = tempfile::tempdir().expect("cwd");
        let outside = tempfile::tempdir().expect("outside");
        let nested = outside.path().join("nested");
        std::fs::create_dir(&nested).expect("nested");
        let workspace = WorkspaceRoots::new(
            cwd.path(),
            [nested.as_path(), outside.path(), outside.path(), cwd.path()],
        )
        .expect("workspace");
        assert_eq!(
            workspace.roots(),
            [
                cwd.path().canonicalize().expect("canonical cwd"),
                outside.path().canonicalize().expect("canonical outside"),
            ]
        );
    }
}
