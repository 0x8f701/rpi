//! Shared path/search/metadata helpers for the session catalog.

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::env;
use std::fs::{self, Metadata};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use chrono::{Local, TimeZone};
use sha2::{Digest, Sha256};

use super::CatalogRow;
use crate::import::{ImportSessionError, MAX_SOURCE_BYTES, OpenedSource};

const SUMMARY_MAX_CHARS: usize = 100;

/// True when `cwd` sits under a Slock ephemeral agent workspace
/// (`.slock/agents/<id>`). Slock orchestrates throwaway Codex agents inside
/// these directories, so such sessions are orchestration infrastructure
/// rather than user projects. The cwd is normalized lexically first — `.` is
/// dropped and `..` pops the previous component — so a recorded cwd like
/// `<workspace>/project/../.slock/agents/<id>` classifies as its resolved
/// form. A `..` that cannot be resolved (nothing left to pop, or `..` applied
/// to a root/prefix) escapes the path and fails closed: the row is never
/// flagged. The exact `.slock` → `agents` → non-empty id component chain is
/// required, so real project roots stay real: a workspace that merely
/// CONTAINS a `.slock` directory, a project root named `Projects`, or a
/// directory literally named `agents` is never flagged. The session's
/// recorded cwd is the only signal — the foreign source row and its imported
/// native copy (whose header carries the source cwd) classify identically.
pub(super) fn is_slock_agent_cwd(cwd: &Path) -> bool {
    let mut normalized: Vec<Component<'_>> = Vec::new();
    for component in cwd.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(normalized.last(), Some(Component::Normal(_))) {
                    normalized.pop();
                } else {
                    // Nothing to pop: a leading relative `..`, or `..` applied
                    // to an absolute root/prefix — not a resolvable workspace
                    // path.
                    return false;
                }
            }
            other => normalized.push(other),
        }
    }
    normalized.windows(3).any(|window| {
        window[0].as_os_str() == OsStr::new(".slock")
            && window[1].as_os_str() == OsStr::new("agents")
            && matches!(window[2], Component::Normal(id) if !id.is_empty())
    })
}

pub(super) fn compare_rows_newest(left: &CatalogRow, right: &CatalogRow) -> Ordering {
    right
        .modified_epoch
        .total_cmp(&left.modified_epoch)
        .then_with(|| left.path.cmp(&right.path))
        .then_with(|| left.session_id.cmp(&right.session_id))
}

pub(super) fn sort_rows_newest(rows: &mut [CatalogRow]) {
    rows.sort_by(compare_rows_newest);
}

pub(super) fn display_name(row: &CatalogRow) -> String {
    row.name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(row.summary.as_str())
        .to_owned()
}

pub(super) fn normalize_summary(summary: &str) -> String {
    summary
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

pub(super) fn normalize_cwd(cwd: &Path) -> String {
    let expanded = expand_tilde(cwd.to_path_buf(), &home_dir_fallback());
    expanded
        .canonicalize()
        .unwrap_or(expanded)
        .to_string_lossy()
        .into_owned()
}

pub(super) fn home_dir_fallback() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub(super) fn truncate_summary(text: &str) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= SUMMARY_MAX_CHARS {
        return normalized;
    }
    let mut out = normalized
        .chars()
        .take(SUMMARY_MAX_CHARS.saturating_sub(1))
        .collect::<String>();
    out.push('…');
    out
}

pub(super) fn row_matches_query(row: &CatalogRow, query: &str) -> bool {
    query
        .split_whitespace()
        .all(|token| fuzzy_match(&row.search_text, token))
}

pub(super) fn fuzzy_match(candidate: &str, query: &str) -> bool {
    let mut candidate = candidate.chars().flat_map(char::to_lowercase);
    query
        .chars()
        .flat_map(char::to_lowercase)
        .all(|expected| candidate.by_ref().any(|actual| actual == expected))
}

pub(crate) fn expand_tilde(path: PathBuf, home: &Path) -> PathBuf {
    if path == Path::new("~") {
        return home.to_path_buf();
    }
    if let Ok(rest) = path.strip_prefix("~/") {
        return home.join(rest);
    }
    if let Ok(rest) = path.strip_prefix("~") {
        return home.join(rest);
    }
    path
}

pub(crate) fn make_absolute(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        env::current_dir().map_or(path.clone(), |cwd| cwd.join(path))
    }
}

pub(super) fn canonical_fingerprint(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| make_absolute(path.to_path_buf()))
        .to_string_lossy()
        .into_owned()
}

pub(super) fn content_fingerprint(metadata: &Metadata) -> String {
    crate::import::metadata_fingerprint(metadata)
}

pub(super) fn pi_content_fingerprint(opened: &OpenedSource) -> Result<String, ImportSessionError> {
    let path = opened.path().to_path_buf();
    let size = opened.metadata().len();
    if size > MAX_SOURCE_BYTES {
        return Err(ImportSessionError::ResourceLimit {
            path,
            reason: format!("file is {size} bytes; maximum is {MAX_SOURCE_BYTES}"),
        });
    }
    let mut file = opened
        .primary_ref()
        .try_clone()
        .map_err(|source| ImportSessionError::Io {
            path: path.clone(),
            source,
        })?;
    file.seek(SeekFrom::Start(0))
        .map_err(|source| ImportSessionError::Io {
            path: path.clone(),
            source,
        })?;
    let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or_default());
    file.take(MAX_SOURCE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| ImportSessionError::Io {
            path: path.clone(),
            source,
        })?;
    let mut primary = opened.primary_ref();
    primary
        .seek(SeekFrom::Start(0))
        .map_err(|source| ImportSessionError::Io {
            path: path.clone(),
            source,
        })?;
    if bytes.len() as u64 > MAX_SOURCE_BYTES {
        return Err(ImportSessionError::ResourceLimit {
            path,
            reason: format!("file exceeds maximum of {MAX_SOURCE_BYTES} bytes"),
        });
    }
    let mut digest = Sha256::new();
    digest.update(b"pi-rs-session-source-v1\0");
    digest.update(bytes);
    Ok(format!("{:x}", digest.finalize()))
}

#[cfg(unix)]
pub(super) fn metadata_epoch(metadata: &Metadata) -> f64 {
    use std::os::unix::fs::MetadataExt;
    metadata.mtime() as f64 + metadata.mtime_nsec() as f64 / 1_000_000_000.0
}

#[cfg(not(unix))]
pub(super) fn metadata_epoch(metadata: &Metadata) -> f64 {
    metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_secs_f64())
        .unwrap_or(0.0)
}

pub(super) fn format_epoch(epoch: f64) -> String {
    let seconds = epoch.floor() as i64;
    let nanos = ((epoch - epoch.floor()) * 1_000_000_000.0).round() as u32;
    Local
        .timestamp_opt(seconds, nanos)
        .single()
        .map(|value| value.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "1970-01-01 00:00:00".to_owned())
}

#[cfg(test)]
mod tests {
    use super::is_slock_agent_cwd;
    use std::path::Path;

    /// Lexical `.`/`..` normalization happens before the chain match, so a
    /// dotted or parent-qualified cwd resolves to the same classification as
    /// its canonical form, a `..` that escapes the path fails closed, and
    /// relative/dot/parent/empty/nested shapes all classify deterministically.
    #[test]
    fn slock_agent_cwd_normalizes_lexical_dots_before_classifying() {
        // Plain relative and absolute chains still flag.
        assert!(is_slock_agent_cwd(Path::new(".slock/agents/agent-7")));
        assert!(is_slock_agent_cwd(Path::new("/srv/u/.slock/agents/agent-7")));

        // A leading `.` component is dropped before matching.
        assert!(is_slock_agent_cwd(Path::new("./.slock/agents/agent-7")));
        assert!(is_slock_agent_cwd(Path::new("/srv/u/./.slock/agents/agent-7")));

        // A resolvable `..` pops the previous component, so the parent-
        // qualified form matches the canonical workspace.
        assert!(is_slock_agent_cwd(Path::new("w/project/../.slock/agents/agent-7")));
        assert!(is_slock_agent_cwd(Path::new("/srv/u/x/y/../..//.slock/agents/agent-7")));
        assert!(is_slock_agent_cwd(Path::new(".slock/agents/../agents/agent-7")));

        // A `..` that escapes the path fails closed — it can never resolve to
        // an agent workspace below a real project.
        assert!(!is_slock_agent_cwd(Path::new("../.slock/agents/agent-7")));
        assert!(!is_slock_agent_cwd(Path::new("w/../../.slock/agents/agent-7")));
        assert!(!is_slock_agent_cwd(Path::new("/../.slock/agents/agent-7")));

        // The empty cwd has no chain to match.
        assert!(!is_slock_agent_cwd(Path::new("")));

        // Nested real projects — with or without interior dots — stay real.
        assert!(!is_slock_agent_cwd(Path::new("/srv/u/Projects/pi-rs")));
        assert!(!is_slock_agent_cwd(Path::new("/srv/u/./Projects/../Projects/pi-rs")));
        // A project root that merely CONTAINS a `.slock` tree stays real.
        assert!(!is_slock_agent_cwd(Path::new("/srv/u/app/.slock")));
        // The chain requires the non-empty id component.
        assert!(!is_slock_agent_cwd(Path::new("/srv/u/.slock/agents")));
    }
}

