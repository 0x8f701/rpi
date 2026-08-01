use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};

/// Canonical filesystem roots trusted by a coding session.
///
/// The primary working directory is always first. Additional roots are
/// canonicalized, must already exist as directories, and are reduced to a
/// deterministic minimal set without duplicate or nested additional roots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceRoots {
    roots: Arc<[PathBuf]>,
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
            if root.starts_with(&cwd) || additional.iter().any(|existing| root.starts_with(existing)) {
                continue;
            }
            additional.push(root);
        }
        let mut roots = Vec::with_capacity(additional.len() + 1);
        roots.push(cwd);
        roots.extend(additional);
        Ok(Self { roots: roots.into() })
    }

    pub(crate) fn for_tool_factory(cwd: &str) -> Self {
        let path = PathBuf::from(cwd);
        let root = path.canonicalize().unwrap_or(path);
        Self {
            roots: vec![root].into(),
        }
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
}

fn canonical_directory(path: &Path, description: &str) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("cannot resolve {description} {}", path.display()))?;
    if !canonical.is_dir() {
        bail!("{description} is not a directory: {}", canonical.display());
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_missing_and_non_directory_roots() {
        let cwd = tempfile::tempdir().expect("cwd");
        let missing = cwd.path().join("missing");
        assert!(WorkspaceRoots::new(cwd.path(), [&missing]).is_err());

        let file = cwd.path().join("file.txt");
        std::fs::write(&file, "x").expect("file");
        assert!(WorkspaceRoots::new(cwd.path(), [&file]).is_err());
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
