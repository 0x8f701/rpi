use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use crate::SandboxError;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NetworkMode {
    #[default]
    FullAccess,
    Restricted,
    ProxyRouted,
}

impl NetworkMode {
    #[must_use]
    pub const fn unshares_network(self) -> bool {
        !matches!(self, Self::FullAccess)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WritableRoot {
    root: PathBuf,
    protected_descendants: Vec<PathBuf>,
}

impl WritableRoot {
    pub fn new(
        root: impl Into<PathBuf>,
        protected_descendants: Vec<PathBuf>,
    ) -> Result<Self, SandboxError> {
        let root = validate_policy_path(root.into())?;
        let protected_descendants = normalized_paths(protected_descendants)?;
        for protected in &protected_descendants {
            if protected == &root || !protected.starts_with(&root) {
                return Err(SandboxError::ProtectedPathOutsideWritableRoot {
                    root,
                    protected: protected.clone(),
                });
            }
        }
        Ok(Self {
            root,
            protected_descendants,
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn protected_descendants(&self) -> &[PathBuf] {
        &self.protected_descendants
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileSystemPolicy {
    writable_roots: Vec<WritableRoot>,
    denied_paths: Vec<PathBuf>,
}

impl FileSystemPolicy {
    pub fn new(
        mut writable_roots: Vec<WritableRoot>,
        denied_paths: Vec<PathBuf>,
    ) -> Result<Self, SandboxError> {
        writable_roots.sort_by(|left, right| {
            path_depth(left.root())
                .cmp(&path_depth(right.root()))
                .then_with(|| left.root().cmp(right.root()))
        });
        let mut merged_writable_roots: Vec<WritableRoot> =
            Vec::with_capacity(writable_roots.len());
        for writable_root in writable_roots {
            match merged_writable_roots.last_mut() {
                Some(existing) if existing.root == writable_root.root => existing
                    .protected_descendants
                    .extend(writable_root.protected_descendants),
                _ => merged_writable_roots.push(writable_root),
            }
        }
        for writable_root in &mut merged_writable_roots {
            writable_root.protected_descendants.sort();
            writable_root.protected_descendants.dedup();
        }
        Ok(Self {
            writable_roots: merged_writable_roots,
            denied_paths: normalized_paths(denied_paths)?,
        })
    }

    #[must_use]
    pub fn read_only() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn writable_roots(&self) -> &[WritableRoot] {
        &self.writable_roots
    }

    #[must_use]
    pub fn denied_paths(&self) -> &[PathBuf] {
        &self.denied_paths
    }
}

pub(crate) fn path_depth(path: &Path) -> usize {
    path.components().count()
}

pub(crate) fn validate_policy_path(path: PathBuf) -> Result<PathBuf, SandboxError> {
    if !path.is_absolute() {
        return Err(SandboxError::PathNotAbsolute(path));
    }
    if path.components().any(|component| {
        matches!(component, Component::CurDir | Component::ParentDir)
    }) {
        return Err(SandboxError::PathNotNormalized(path));
    }
    reject_symlink_components(&path)?;
    Ok(path)
}

fn normalized_paths(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>, SandboxError> {
    paths
        .into_iter()
        .map(validate_policy_path)
        .collect::<Result<BTreeSet<_>, _>>()
        .map(BTreeSet::into_iter)
        .map(Iterator::collect)
}

fn reject_symlink_components(path: &Path) -> Result<(), SandboxError> {
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component.as_os_str());
        match std::fs::symlink_metadata(&prefix) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(SandboxError::SymlinkedPolicyPath(prefix));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(SandboxError::System(error)),
        }
    }
    Ok(())
}
