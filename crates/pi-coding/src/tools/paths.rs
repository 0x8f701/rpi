//! Path normalization and resolution (port of pi's `coding/tools.go` path
//! helpers: `normalizePath`, `resolveToCwd`, `resolveReadPath`).
//!
//! Faithful port of pi's path handling: unicode-space folding, `@`-prefix
//! stripping, tilde expansion, `file://` expansion, and the macOS filename
//! fallbacks (narrow no-break space before AM/PM, NFD, curly quote).

use std::path::{Path, PathBuf};
use unicode_normalization::UnicodeNormalization;

use crate::WorkspaceRoots;

/// The narrow no-break space (U+202F) pi substitutes before AM/PM in macOS
/// screenshot filenames.
const NARROW_NO_BREAK_SPACE: char = '\u{202F}';

/// Folds pi's `UNICODE_SPACES` set (U+00A0, U+2000–U+200A, U+202F, U+205F,
/// U+3000) plus the regular space to a single ASCII space (paths.ts).
fn fold_unicode_spaces(s: &str) -> String {
    s.chars().map(|c| {
        if c == ' '
            || c == '\u{00A0}'
            || (c >= '\u{2000}' && c <= '\u{200A}')
            || c == '\u{202F}'
            || c == '\u{205F}'
            || c == '\u{3000}'
        {
            ' '
        } else {
            c
        }
    })
    .collect()
}

/// Returns the user's home directory (`$HOME` on Unix), or `None` if unset.
fn home_dir() -> Option<String> {
    std::env::var("HOME").ok().filter(|h| !h.is_empty())
}

/// Lexical path cleaning, porting Go's `filepath.Clean`: collapses repeated
/// separators, eliminates `.` components, and resolves `..` against the
/// preceding component (without crossing a leading root). An empty input
/// yields `"."`.
pub(crate) fn clean_path(input: &str) -> String {
    if input.is_empty() {
        return ".".to_string();
    }
    let bytes = input.as_bytes();
    let absolute = bytes[0] == b'/';
    let mut stack: Vec<&str> = Vec::new();
    for comp in input.split('/') {
        match comp {
            "" | "." => continue,
            ".." => {
                if let Some(top) = stack.last() {
                    if *top != ".." {
                        stack.pop();
                        continue;
                    }
                }
                if !absolute {
                    stack.push("..");
                }
                // absolute: ".." at root is dropped (can't go above root).
            }
            other => stack.push(other),
        }
    }
    let joined = stack.join("/");
    match (absolute, joined.is_empty()) {
        (true, true) => "/".to_string(),
        (true, false) => format!("/{joined}"),
        (false, true) => ".".to_string(),
        (false, false) => joined,
    }
}

/// Ports pi's `normalizePath` (paths.ts): folds unicode spaces, strips a leading
/// `@`, expands `~`, and expands `file://` URLs. Does not trim.
pub(crate) fn normalize_path(input: &str) -> String {
    let mut normalized = fold_unicode_spaces(input);
    if let Some(rest) = normalized.strip_prefix('@') {
        normalized = rest.to_string();
    }
    if let Some(home) = home_dir() {
        if normalized == "~" {
            return home;
        }
        if let Some(rest) = normalized.strip_prefix("~/") {
            return clean_path(&format!("{home}/{rest}"));
        }
        // Windows `~\` form is not relevant on Unix; left as-is.
    }
    if let Some(rest) = normalized.strip_prefix("file://") {
        // Find the start of the path (first '/'); host component is ignored.
        if let Some(idx) = rest.find('/') {
            return clean_path(&rest[idx..]);
        }
        return normalized;
    }
    normalized
}

/// Resolves a possibly-relative path against `cwd`, porting pi's `resolvePath`
/// (normalize + tilde + `file://` expansion). The result is lexically cleaned.
pub(crate) fn resolve_to_cwd(path: &str, cwd: &str) -> String {
    let normalized = normalize_path(path);
    if Path::new(&normalized).is_absolute() {
        return clean_path(&normalized);
    }
    let mut base = cwd.to_string();
    if let Some(home) = home_dir() {
        if base == "~" {
            base = home;
        } else if let Some(rest) = base.strip_prefix("~/") {
            base = format!("{home}/{rest}");
        }
    }
    clean_path(&format!("{base}/{normalized}"))
}

/// Resolves a tool path and rejects traversal outside the configured workspace
/// roots. Relative paths are resolved against the primary working directory.
/// Existing paths are canonicalized so symlinks cannot escape the boundary;
/// for new files, the nearest filesystem entry (including a broken symlink) is
/// canonicalized and checked instead.
fn escape_error(workspace: &WorkspaceRoots, path: &str) -> anyhow::Error {
    if workspace.additional_roots().is_empty() {
        anyhow::anyhow!("Path escapes working directory: {path}")
    } else {
        anyhow::anyhow!("Path escapes workspace roots: {path}")
    }
}

pub(crate) fn resolve_scoped_path(
    path: &str,
    workspace: &WorkspaceRoots,
) -> anyhow::Result<String> {
    let cwd = workspace.cwd().to_string_lossy();
    let resolved = PathBuf::from(resolve_to_cwd(path, &cwd));
    if !workspace.roots().iter().any(|root| resolved.starts_with(root)) {
        return Err(escape_error(workspace, path));
    }

    let mut existing = resolved.as_path();
    while std::fs::symlink_metadata(existing).is_err() {
        existing = existing
            .parent()
            .ok_or_else(|| escape_error(workspace, path))?;
    }
    let canonical_existing = std::fs::canonicalize(existing)
        .map_err(|error| anyhow::anyhow!("Could not resolve path {path}: {error}"))?;
    if !workspace
        .roots()
        .iter()
        .any(|root| canonical_existing.starts_with(root))
    {
        return Err(escape_error(workspace, path));
    }
    Ok(resolved.to_string_lossy().into_owned())
}

/// Resolves a normal coding-tool mutation path against the primary working
/// directory without imposing workspace-root containment. Absolute paths,
/// parent-relative traversal, and ordinary symlink-following filesystem
/// semantics are intentionally allowed for write/edit.
pub(crate) fn resolve_mutation_path(
    path: &str,
    workspace: &WorkspaceRoots,
) -> anyhow::Result<String> {
    if path.is_empty() {
        return Err(anyhow::anyhow!("File path must not be empty."));
    }
    if path.contains('\0') {
        return Err(anyhow::anyhow!("File path contains a NUL byte."));
    }
    Ok(resolve_to_cwd(path, &workspace.cwd().to_string_lossy()))
}

/// Resolves a read path against the primary working directory without a
/// workspace boundary. Absolute paths, `../` traversal, and ordinary symlink
/// follow semantics are allowed (OMP-compatible default read). Tries pi's
/// macOS filename fallbacks when the resolved path does not exist: narrow
/// no-break space before AM/PM, NFD, curly quote, and combined NFD+curly.
pub(crate) fn resolve_read_path(
    path: &str,
    workspace: &WorkspaceRoots,
) -> anyhow::Result<String> {
    let cwd = workspace.cwd().to_string_lossy();
    let resolved = resolve_to_cwd(path, &cwd);
    if path_exists(&resolved) {
        return Ok(resolved);
    }
    let amp = mac_ampm_variant(&resolved);
    if amp != resolved && path_exists(&amp) {
        return Ok(amp);
    }
    let nfd: String = resolved.nfd().collect();
    if nfd != resolved && path_exists(&nfd) {
        return Ok(nfd);
    }
    let curly = resolved.replace('\'', "\u{2019}");
    if curly != resolved && path_exists(&curly) {
        return Ok(curly);
    }
    let nfd_curly: String = nfd.replace('\'', "\u{2019}");
    if nfd_curly != resolved && path_exists(&nfd_curly) {
        return Ok(nfd_curly);
    }
    Ok(resolved)
}


fn path_exists(p: &str) -> bool {
    std::fs::metadata(p).is_ok()
}

/// Builds the macOS screenshot AM/PM variant: ` AM.`/` PM.` → a narrow
/// no-break space before the suffix (port of pi's `macAMPMVariant`).
fn mac_ampm_variant(p: &str) -> String {
    // Match ` (?i)(AM|PM)\.` → narrow-nbsp + "$1."
    let lower = p.to_lowercase();
    let needle = " am.";
    let mut out = String::new();
    let mut rest = p;
    loop {
        let lrest = rest.to_lowercase();
        match lrest.find(needle) {
            None => {
                out.push_str(rest);
                break;
            }
            Some(idx) => {
                // idx is byte offset in lower == offset in rest (ASCII needle).
                out.push_str(&rest[..idx]);
                // skip the leading space, emit narrow-nbsp
                let suffix = &rest[idx + 1..idx + 4]; // "AM." or "PM."
                out.push(NARROW_NO_BREAK_SPACE);
                out.push_str(suffix);
                rest = &rest[idx + 4..];
            }
        }
    }
    out
}

/// macOS filename fallback helper used by [`resolve_read_path`]
/// (path-utils.ts `resolveReadPathAsync`): narrow no-break space before AM/PM,
/// NFD, curly quote, and combined NFD+curly variants.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_path_collapses_dots() {
        assert_eq!(clean_path(""), ".");
        assert_eq!(clean_path("./a/./b"), "a/b");
        assert_eq!(clean_path("a/../b"), "b");
        assert_eq!(clean_path("/a/../b"), "/b");
        assert_eq!(clean_path("//a"), "/a");
        assert_eq!(clean_path("a/b/.."), "a");
        assert_eq!(clean_path("/.."), "/");
    }

    #[test]
    fn normalize_path_strips_at_and_folds_spaces() {
        assert_eq!(normalize_path("@foo.txt"), "foo.txt");
        // U+00A0 folded to space.
        assert_eq!(normalize_path("a\u{00A0}b"), "a b");
    }

    #[test]
    fn resolve_to_cwd_relative_and_absolute() {
        let cwd = "/workspace/project";
        assert_eq!(resolve_to_cwd("src/main.rs", cwd), "/workspace/project/src/main.rs");
        assert_eq!(resolve_to_cwd("/system/hosts", cwd), "/system/hosts");
        assert_eq!(resolve_to_cwd("../sibling/x", cwd), "/workspace/sibling/x");
        assert_eq!(resolve_to_cwd("./a/../b", cwd), "/workspace/project/b");
    }

    #[test]
    fn scoped_paths_reject_traversal() {
        let root = std::env::temp_dir().join(format!("pi-scoped-path-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("create scoped root");
        let workspace = WorkspaceRoots::new(&root, Vec::<PathBuf>::new()).expect("workspace");
        assert!(resolve_scoped_path("inside.txt", &workspace).is_ok());
        assert!(resolve_scoped_path("../outside.txt", &workspace).is_err());
        assert!(resolve_scoped_path("/system/hosts", &workspace).is_err());
    }

    #[test]
    fn additional_root_accepts_absolute_paths_and_rejects_external_paths() {
        let cwd = tempfile::tempdir().expect("cwd");
        let additional = tempfile::tempdir().expect("additional");
        let external = tempfile::tempdir().expect("external");
        let workspace = WorkspaceRoots::new(cwd.path(), [additional.path()]).expect("workspace");
        let accepted = additional.path().join("new.txt");
        assert_eq!(
            resolve_scoped_path(&accepted.to_string_lossy(), &workspace).expect("additional path"),
            accepted.to_string_lossy()
        );
        assert!(
            resolve_scoped_path(&external.path().join("no.txt").to_string_lossy(), &workspace)
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn scoped_paths_reject_symlink_escape_for_existing_and_new_paths() {
        let cwd = tempfile::tempdir().expect("cwd");
        let external = tempfile::tempdir().expect("external");
        std::fs::write(external.path().join("secret.txt"), "secret").expect("secret");
        std::os::unix::fs::symlink(external.path(), cwd.path().join("escape"))
            .expect("directory symlink");
        std::os::unix::fs::symlink(
            external.path().join("missing.txt"),
            cwd.path().join("broken"),
        )
        .expect("broken symlink");
        let workspace = WorkspaceRoots::new(cwd.path(), Vec::<PathBuf>::new()).expect("workspace");
        assert!(resolve_scoped_path("escape/secret.txt", &workspace).is_err());
        assert!(resolve_scoped_path("escape/new.txt", &workspace).is_err());
        assert!(resolve_scoped_path("broken", &workspace).is_err());
    }

    #[test]
    fn read_path_allows_absolute_and_parent_relative_external_files() {
        let root = tempfile::tempdir().expect("root");
        let cwd = root.path().join("project");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let external = root.path().join("outside.txt");
        std::fs::write(&external, "external").expect("external file");
        let workspace = WorkspaceRoots::new(&cwd, Vec::<PathBuf>::new()).expect("workspace");

        let absolute = resolve_read_path(&external.to_string_lossy(), &workspace)
            .expect("absolute external read path");
        assert_eq!(
            std::fs::canonicalize(&absolute).expect("canonical absolute"),
            std::fs::canonicalize(&external).expect("canonical external")
        );
        assert_eq!(std::fs::read_to_string(&absolute).expect("read absolute"), "external");

        let relative = resolve_read_path("../outside.txt", &workspace)
            .expect("parent-relative external read path");
        assert_eq!(
            std::fs::canonicalize(&relative).expect("canonical relative"),
            std::fs::canonicalize(&external).expect("canonical external")
        );
        assert_eq!(std::fs::read_to_string(&relative).expect("read relative"), "external");
    }

    #[cfg(unix)]
    #[test]
    fn read_path_allows_symlink_to_external_file() {
        let cwd = tempfile::tempdir().expect("cwd");
        let external = tempfile::tempdir().expect("external");
        let target = external.path().join("secret.txt");
        std::fs::write(&target, "secret").expect("secret");
        let link = cwd.path().join("alias.txt");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        let workspace = WorkspaceRoots::new(cwd.path(), Vec::<PathBuf>::new()).expect("workspace");

        let resolved =
            resolve_read_path("alias.txt", &workspace).expect("symlink external read path");
        assert_eq!(resolved, clean_path(&link.to_string_lossy()));
        assert_eq!(std::fs::read_to_string(&resolved).expect("follow symlink"), "secret");
    }

    #[test]
    fn mutation_paths_allow_absolute_and_parent_relative_external_files() {
        let root = tempfile::tempdir().expect("root");
        let cwd = root.path().join("project");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let workspace = WorkspaceRoots::new(&cwd, Vec::<PathBuf>::new()).expect("workspace");
        let outside = root.path().join("outside.txt");

        assert_eq!(
            resolve_mutation_path(&outside.to_string_lossy(), &workspace)
                .expect("absolute external mutation path"),
            outside.to_string_lossy()
        );
        assert_eq!(
            resolve_mutation_path("../outside.txt", &workspace)
                .expect("parent-relative external mutation path"),
            outside.to_string_lossy()
        );
        assert_eq!(
            resolve_mutation_path("inside.txt", &workspace).expect("cwd-relative mutation path"),
            cwd.join("inside.txt").to_string_lossy()
        );
    }

    #[test]
    fn mutation_paths_reject_invalid_inputs() {
        let cwd = tempfile::tempdir().expect("cwd");
        let workspace = WorkspaceRoots::new(cwd.path(), Vec::<PathBuf>::new()).expect("workspace");

        assert!(resolve_mutation_path("", &workspace).is_err());
        assert!(resolve_mutation_path("bad\0path", &workspace).is_err());
    }

    #[test]
    fn mac_ampm_variant_inserts_narrow_nbsp() {
        let v = mac_ampm_variant("Screen Shot 2024 01 02 at 3.45.06 AM.png");
        assert!(v.contains('\u{202F}'));
        assert!(v.contains("AM."));
        assert!(!v.contains(" AM."));
    }
}