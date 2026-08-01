//! Path normalization and resolution (port of pi's `coding/tools.go` path
//! helpers: `normalizePath`, `resolveToCwd`, `resolveReadPath`).
//!
//! Faithful port of pi's path handling: unicode-space folding, `@`-prefix
//! stripping, tilde expansion, `file://` expansion, and the macOS filename
//! fallbacks (narrow no-break space before AM/PM, NFD, curly quote).

use std::path::{Path, PathBuf};
use unicode_normalization::UnicodeNormalization;

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

/// Resolves a tool path and rejects traversal outside the session working directory.
/// Existing paths are canonicalized so symlinks cannot escape the boundary; for
/// new files, the nearest existing ancestor is checked instead.
pub(crate) fn resolve_scoped_path(path: &str, cwd: &str) -> anyhow::Result<String> {
    let resolved = PathBuf::from(resolve_to_cwd(path, cwd));
    let lexical_root = PathBuf::from(clean_path(cwd));
    if !resolved.starts_with(&lexical_root) {
        anyhow::bail!("Path escapes working directory: {path}");
    }

    let canonical_root = std::fs::canonicalize(&lexical_root).unwrap_or(lexical_root);
    let mut existing = resolved.as_path();
    while !existing.exists() {
        existing = existing
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Path escapes working directory: {path}"))?;
    }
    let canonical_existing = std::fs::canonicalize(existing)
        .map_err(|error| anyhow::anyhow!("Could not resolve path {path}: {error}"))?;
    if !canonical_existing.starts_with(&canonical_root) {
        anyhow::bail!("Path escapes working directory: {path}");
    }
    Ok(resolved.to_string_lossy().into_owned())
}
pub(crate) fn resolve_read_path(path: &str, cwd: &str) -> anyhow::Result<String> {
    let resolved = resolve_scoped_path(path, cwd)?;
    if path_exists(&resolved) {
        return Ok(resolved);
    }
    let amp = mac_ampm_variant(&resolved);
    if amp != resolved && path_exists(&amp) {
        return resolve_scoped_path(&amp, cwd);
    }
    let nfd: String = resolved.nfd().collect();
    if nfd != resolved && path_exists(&nfd) {
        return resolve_scoped_path(&nfd, cwd);
    }
    let curly = resolved.replace('\'', "\u{2019}");
    if curly != resolved && path_exists(&curly) {
        return resolve_scoped_path(&curly, cwd);
    }
    let nfd_curly: String = nfd.replace('\'', "\u{2019}");
    if nfd_curly != resolved && path_exists(&nfd_curly) {
        return resolve_scoped_path(&nfd_curly, cwd);
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

/// Resolves a path and tries pi's macOS filename fallbacks
/// (path-utils.ts `resolveReadPathAsync`): narrow no-break space before AM/PM,
/// NFD, curly quote, and combined NFD+curly variants. Returns the first
/// existing variant, else the resolved path.

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
        let root = root.to_string_lossy();
        assert!(resolve_scoped_path("inside.txt", &root).is_ok());
        assert!(resolve_scoped_path("../outside.txt", &root).is_err());
        assert!(resolve_scoped_path("/system/hosts", &root).is_err());
    }

    #[test]
    fn mac_ampm_variant_inserts_narrow_nbsp() {
        let v = mac_ampm_variant("Screen Shot 2024 01 02 at 3.45.06 AM.png");
        assert!(v.contains('\u{202F}'));
        assert!(v.contains("AM."));
        assert!(!v.contains(" AM."));
    }
}