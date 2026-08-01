use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;
use tokio_util::sync::CancellationToken;

const MAX_CANDIDATES: usize = 4_096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AtPrefix {
    pub start: usize,
    pub end: usize,
    pub query: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMatch {
    pub value: String,
    pub label: String,
    pub is_directory: bool,
}

/// Find the current `@path` token on one editor line. The returned byte range is
/// safe to replace directly and never includes text after the cursor.
#[must_use]
pub fn current_at_prefix(line: &str, cursor: usize) -> Option<AtPrefix> {
    let cursor = cursor.min(line.len());
    if !line.is_char_boundary(cursor) {
        return None;
    }
    let before = &line[..cursor];
    let mut quoted = false;
    let start = before.char_indices().rev().find_map(|(index, character)| {
        if character == '"' {
            quoted = !quoted;
        }
        (character == '@'
            && is_token_boundary(before, index)
            && (!quoted || before[index + 1..].starts_with('"')))
        .then_some(index)
    })?;
    let raw = &before[start + 1..];
    let query = if let Some(quoted) = raw.strip_prefix('"') {
        unescape_open_quote(quoted)?
    } else {
        if raw.chars().any(char::is_whitespace) || raw.contains(['"', '\'']) {
            return None;
        }
        raw.to_owned()
    };
    validate_relative_query(&query).ok()?;
    Some(AtPrefix {
        start,
        end: cursor,
        query,
    })
}

/// Search beneath `cwd` without following symlinks. The blocking filesystem
/// walk runs off the async executor and cooperatively stops when cancelled.
pub async fn search(
    cwd: PathBuf,
    query: String,
    limit: usize,
    cancellation: CancellationToken,
) -> Result<Vec<FileMatch>> {
    validate_relative_query(&query)?;
    let limit = limit.max(1);
    tokio::task::spawn_blocking(move || search_blocking(&cwd, &query, limit, &cancellation))
        .await
        .context("file completion worker stopped unexpectedly")?
}

fn search_blocking(
    cwd: &Path,
    query: &str,
    limit: usize,
    cancellation: &CancellationToken,
) -> Result<Vec<FileMatch>> {
    let root = cwd
        .canonicalize()
        .with_context(|| format!("cannot search {}", cwd.display()))?;
    let normalized_query = normalize_query(query);
    let show_hidden = normalized_query.starts_with('.');

    let mut builder = WalkBuilder::new(&root);
    builder
        .standard_filters(true)
        .hidden(!show_hidden)
        .follow_links(false)
        .filter_entry(|entry| entry.file_name() != OsStr::new(".git"));

    let mut candidates = Vec::new();
    for entry in builder.build() {
        if cancellation.is_cancelled() || candidates.len() >= MAX_CANDIDATES {
            break;
        }
        let Ok(entry) = entry else {
            continue;
        };
        if entry.depth() == 0 || entry.file_type().is_some_and(|kind| kind.is_symlink()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(&root) else {
            continue;
        };
        let Some(relative) = relative.to_str() else {
            continue;
        };
        if relative.chars().any(char::is_control) {
            continue;
        }
        if !show_hidden
            && Path::new(relative).components().any(|component| {
                component
                    .as_os_str()
                    .to_str()
                    .is_some_and(|name| name.starts_with('.'))
            })
        {
            continue;
        }
        let is_directory = entry.file_type().is_some_and(|kind| kind.is_dir());
        let mut display = relative.replace('\\', "/");
        if is_directory {
            display.push('/');
        }
        let Some(score) = match_score(&display, &normalized_query) else {
            continue;
        };
        candidates.push((score, display, is_directory));
    }

    if cancellation.is_cancelled() {
        return Ok(Vec::new());
    }
    candidates.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.len().cmp(&right.1.len()))
            .then_with(|| left.1.to_lowercase().cmp(&right.1.to_lowercase()))
    });
    candidates.truncate(limit);
    Ok(candidates
        .into_iter()
        .map(|(_, path, is_directory)| FileMatch {
            value: quote_at_path(&path),
            label: format!("@{path}"),
            is_directory,
        })
        .collect())
}

fn validate_relative_query(query: &str) -> Result<()> {
    if query.contains('\0') {
        bail!("file completion path contains a NUL byte");
    }
    let normalized = query.replace('\\', "/");
    if normalized.starts_with('/') {
        bail!("file completion stays within the working directory");
    }
    for component in Path::new(&normalized).components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            bail!("file completion stays within the working directory");
        }
    }
    Ok(())
}

fn normalize_query(query: &str) -> String {
    let normalized = query.replace('\\', "/");
    normalized
        .strip_prefix("./")
        .unwrap_or(&normalized)
        .trim_end_matches('"')
        .to_lowercase()
}

fn is_token_boundary(text: &str, at: usize) -> bool {
    if at == 0 {
        return true;
    }
    text[..at].chars().next_back().is_some_and(|character| {
        character.is_whitespace() || matches!(character, '(' | '[' | '{' | '=' | ',' | ':')
    })
}

fn unescape_open_quote(value: &str) -> Option<String> {
    let mut result = String::with_capacity(value.len());
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            result.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '"' {
            return None;
        } else {
            result.push(character);
        }
    }
    if escaped {
        result.push('\\');
    }
    Some(result)
}

fn match_score(path: &str, query: &str) -> Option<u8> {
    if query.is_empty() {
        return (path.trim_end_matches('/').split('/').count() == 1).then_some(0);
    }
    let path = path.to_lowercase();
    let basename = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path.as_str());
    if path == query || path.trim_end_matches('/') == query.trim_end_matches('/') {
        Some(0)
    } else if path.starts_with(query) {
        Some(1)
    } else if basename.starts_with(query) {
        Some(2)
    } else if path.split('/').any(|segment| segment.starts_with(query)) {
        Some(3)
    } else if path.contains(query) {
        Some(4)
    } else if fuzzy_match(&path, query) {
        Some(5)
    } else {
        None
    }
}

fn fuzzy_match(candidate: &str, query: &str) -> bool {
    let mut candidate = candidate.chars();
    query
        .chars()
        .all(|expected| candidate.by_ref().any(|actual| actual == expected))
}

fn quote_at_path(path: &str) -> String {
    if path
        .chars()
        .all(|character| !character.is_whitespace() && !matches!(character, '"' | '\'' | '\\'))
    {
        return format!("@{path}");
    }
    let escaped = path.replace('\\', "\\\\").replace('"', "\\\"");
    format!("@\"{escaped}\"")
}
