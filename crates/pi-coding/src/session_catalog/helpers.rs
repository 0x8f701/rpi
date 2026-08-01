//! Shared path/search/metadata helpers for the session catalog.

use std::env;
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use chrono::{Local, TimeZone};

use super::CatalogRow;

const SUMMARY_MAX_CHARS: usize = 100;

pub(super) fn sort_rows_newest(rows: &mut [CatalogRow]) {
    rows.sort_by(|left, right| {
        right
            .modified_epoch
            .partial_cmp(&left.modified_epoch)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
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

pub(super) fn expand_tilde(path: PathBuf, home: &Path) -> PathBuf {
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

pub(super) fn make_absolute(path: PathBuf) -> PathBuf {
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
    let modified = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_secs())
        .unwrap_or(0);
    format!("{}:{}", modified, metadata.len())
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
