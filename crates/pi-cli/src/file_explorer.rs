//! Workspace-scoped file explorer operations for the Web RPC surface.
//!
//! Paths are normally workspace-relative on the wire; an absolute request is
//! accepted only when it lexically resolves beneath the workspace root, in
//! which case all I/O runs on the converted scoped relative form. Read and
//! diff responses always display that workspace-relative path; absolute
//! request identity stays request-local on the client. Filesystem
//! reads first pass through the application's captured PolicyFs authority;
//! capability-relative no-follow handles provide bounded streaming and
//! directory listing without reopening a user-supplied absolute path.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Read};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use cap_fs_ext::{DirExt as _, FollowSymlinks, OpenOptionsFollowExt as _};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, File, OpenOptions};
use pi_coding::{Application, FsError};
use serde::{Deserialize, Serialize};
use serde_json::json;
use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::code_review::{IsolatedGitSandbox, run_git_bounded_timeout};

pub(crate) const DEFAULT_PAGE_LINES: usize = 400;
pub(crate) const MAX_PAGE_LINES: usize = 2_000;
pub(crate) const MAX_PAGE_BYTES: usize = 256 * 1024;
pub(crate) const MAX_LIST_ENTRIES: usize = 5_000;
/// Matches returned by one workspace search. Further matches set `truncated`.
pub(crate) const MAX_SEARCH_MATCHES: usize = 200;
/// Directories and files examined by one search. Hitting this stops the walk.
const MAX_SEARCH_VISITED: usize = 20_000;
pub(crate) const MAX_CHANGED_FILES: usize = 5_000;
const MAX_GIT_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const GIT_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_PAGE_CONTENT_BYTES: usize = MAX_PAGE_BYTES - 1024;
const FILE_VALIDATION_BYTES: u64 = 1;
const MAX_TEXT_LINE_BYTES: usize = MAX_PAGE_BYTES;
const MAX_GITIGNORE_BYTES: usize = MAX_PAGE_BYTES;
const MAX_SUBMODULE_EXPAND: usize = 32;
const MAX_SUBMODULE_DEPTH: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FileExplorerError {
    #[serde(rename = "file_line_too_large")]
    LineTooLarge,
    FileUnavailable,
    #[serde(rename = "file_symlink")]
    FileSymlink,
    DirectoryUnavailable,
}

impl std::fmt::Display for FileExplorerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match *self {
            Self::LineTooLarge => "line exceeds response limit",
            Self::FileUnavailable => "file is unavailable",
            Self::FileSymlink => "file is a symlink",
            Self::DirectoryUnavailable => "directory is unavailable",
        })
    }
}

impl std::error::Error for FileExplorerError {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ListEntry {
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) kind: &'static str,
    #[serde(rename = "sizeBytes", skip_serializing_if = "Option::is_none")]
    pub(crate) size_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ListResponse {
    pub(crate) entries: Vec<ListEntry>,
    pub(crate) truncated: bool,
}

/// Upper bound for one `path_complete` response. Suggestions are user-facing
/// hints, so a small cap keeps every response bounded even over huge
/// directories; `truncated` reports the overflow.
pub(crate) const MAX_COMPLETION_ENTRIES: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompletionKind {
    Directory,
    File,
    Any,
}

/// One suggestion row of the `path_complete` response. `path` is the FULL
/// candidate (`base/name`) in the request input's own form domain, so a
/// client replaces its input with `path` verbatim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct CompletionEntry {
    pub(crate) name: String,
    pub(crate) path: String,
    /// `"directory"` or `"file"` — the frozen component contract, distinct
    /// from [`ListEntry`]'s legacy `"dir"`/`"file"`.
    pub(crate) kind: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct CompletePathResponse {
    /// Normalized parent directory in the input's own form domain: an
    /// absolute request yields an absolute base, a relative request a
    /// workspace-relative base.
    pub(crate) base: String,
    pub(crate) prefix: String,
    pub(crate) entries: Vec<CompletionEntry>,
    pub(crate) truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ReadResponse {
    pub(crate) path: String,
    pub(crate) lines: Vec<String>,
    #[serde(rename = "nextCursor", skip_serializing_if = "Option::is_none")]
    pub(crate) next_cursor: Option<usize>,
    #[serde(rename = "totalLines")]
    pub(crate) total_lines: usize,
    #[serde(rename = "sizeBytes")]
    pub(crate) size_bytes: u64,
    pub(crate) truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ChangedFile {
    pub(crate) path: String,
    pub(crate) status: &'static str,
    #[serde(rename = "oldPath", skip_serializing_if = "Option::is_none")]
    pub(crate) old_path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ChangesResponse {
    pub(crate) files: Vec<ChangedFile>,
    #[serde(rename = "isRepo")]
    pub(crate) is_repo: bool,
    pub(crate) truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct DiffResponse {
    pub(crate) path: String,
    pub(crate) lines: Vec<String>,
    #[serde(rename = "nextCursor", skip_serializing_if = "Option::is_none")]
    pub(crate) next_cursor: Option<usize>,
    #[serde(rename = "totalLines")]
    pub(crate) total_lines: usize,
    pub(crate) truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct BlameLine {
    pub(crate) commit: Option<String>,
    pub(crate) author: String,
    pub(crate) time: u64,
    #[serde(rename = "originalLine")]
    pub(crate) original_line: usize,
    #[serde(rename = "currentLine")]
    pub(crate) current_line: usize,
    pub(crate) text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct BlameResponse {
    pub(crate) path: String,
    pub(crate) lines: Vec<BlameLine>,
}

#[derive(Debug)]
struct RetainedEntry {
    sort_key: (u8, String),
    entry: ListEntry,
}

impl PartialEq for RetainedEntry {
    fn eq(&self, other: &Self) -> bool {
        self.sort_key == other.sort_key
    }
}

impl Eq for RetainedEntry {}

impl PartialOrd for RetainedEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RetainedEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.sort_key.cmp(&other.sort_key)
    }
}

/// Heap retention for [`complete_path`]: bounded to the requested limit by
/// evicting the LARGEST sort key, then drained in ascending order so the
/// response is directories first and names sorted within each kind.
#[derive(Debug)]
struct CompletionCandidate {
    sort_key: (u8, String),
    entry: CompletionEntry,
}

impl PartialEq for CompletionCandidate {
    fn eq(&self, other: &Self) -> bool {
        self.sort_key == other.sort_key
    }
}

impl Eq for CompletionCandidate {}

impl PartialOrd for CompletionCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CompletionCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.sort_key.cmp(&other.sort_key)
    }
}

/// Resolve a wire path beneath the primary workspace root. Absolute inputs
/// are accepted only when they lexically resolve beneath the workspace root
/// and are converted to the scoped relative representation used everywhere
/// else. The returned absolute path is diagnostic input for PolicyFs only;
/// capability-relative helpers below retain their own no-follow authority
/// for actual I/O.
pub(crate) fn resolve_scoped_path(
    workspace_root: &Path,
    input: &str,
    allow_empty: bool,
) -> Result<(PathBuf, String)> {
    if input.contains('\0') {
        bail!("path is outside the workspace");
    }
    let input_path = Path::new(input);
    let request = scoped_components(input_path, input_path.is_absolute())?;
    let components = if input_path.is_absolute() {
        if !workspace_root.is_absolute() {
            bail!("path is outside the workspace");
        }
        let root = scoped_components(workspace_root, true)?;
        let Some(suffix) = request.strip_prefix(root.as_slice()) else {
            bail!("path is outside the workspace");
        };
        suffix.to_vec()
    } else {
        request
    };
    if components.is_empty() && !allow_empty {
        bail!("path is required");
    }
    let relative_path = components.iter().collect::<PathBuf>();
    let relative = components
        .iter()
        .map(|component| component.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    Ok((workspace_root.join(relative_path), relative))
}

/// Collapse a path into scope components: `.` vanishes, `..` pops the
/// previous component (underflow rejects), and the leading absolute
/// boundary (`/`, Windows drive or UNC prefix) is retained once so that
/// absolute requests can be compared against the workspace root.
fn scoped_components(path: &Path, retain_boundary: bool) -> Result<Vec<OsString>> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(component) => components.push(component.to_owned()),
            Component::ParentDir => {
                if components.pop().is_none() {
                    bail!("path is outside the workspace");
                }
            }
            Component::RootDir | Component::Prefix(_)
                if retain_boundary && components.is_empty() =>
            {
                components.push(component.as_os_str().to_owned());
            }
            Component::RootDir | Component::Prefix(_) => {
                bail!("path is outside the workspace");
            }
        }
    }
    Ok(components)
}

/// Wire/display path for a page response. Always the scoped relative form;
/// an absolute request keeps its identity request-local on the client and is
/// never echoed on the wire.
fn response_path(relative: &str) -> String {
    relative.to_owned()
}

pub(crate) fn list(app: &Application, path: &str) -> Result<ListResponse> {
    let workspace_root = app.session().cwd().to_path_buf();
    list_with_generation(&app.capture_generation(), &workspace_root, path)
}

/// Reuse explorer enumeration under one caller-retained policy generation.
pub(crate) fn list_with_generation(
    generation: &pi_coding::application::ApplicationGeneration,
    workspace_root: &Path,
    path: &str,
) -> Result<ListResponse> {
    let (absolute, relative) = resolve_scoped_path(workspace_root, path, true)?;
    generation
        .inspect_directory(&absolute)
        .map_err(|error| map_directory_error(&error))?;

    let root = open_workspace_root(&workspace_root)?;
    let ignore = build_ignore_stack(&root, Path::new(&relative))?;
    let directory = open_relative_directory(root, Path::new(&relative))?;
    let mut retained = BinaryHeap::with_capacity(MAX_LIST_ENTRIES + 1);
    let mut visible_count = 0usize;
    for entry in directory
        .entries()
        .map_err(|_| anyhow!("directory is unavailable"))?
    {
        let entry = entry.map_err(|_| anyhow!("directory is unavailable"))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name == ".git" {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|_| anyhow!("directory is unavailable"))?;
        if file_type.is_symlink() {
            continue;
        }
        let candidate = Path::new(&relative).join(&name);
        if ignore.is_ignored(&candidate, file_type.is_dir()) {
            continue;
        }
        let (kind, size_bytes, rank) = if file_type.is_dir() {
            ("dir", None, 0)
        } else if file_type.is_file() {
            let mut options = OpenOptions::new();
            options.read(true).follow(FollowSymlinks::No);
            let file = match directory.open_with(Path::new(&name), &options) {
                Ok(file) => file,
                Err(_) => continue,
            };
            let size = match file.metadata() {
                Ok(metadata) if metadata.is_file() => metadata.len(),
                _ => continue,
            };
            ("file", Some(size), 1)
        } else {
            continue;
        };
        visible_count = visible_count.saturating_add(1);
        let entry_path = candidate.to_string_lossy().into_owned();
        retained.push(RetainedEntry {
            sort_key: (rank, name.clone()),
            entry: ListEntry {
                name,
                path: entry_path,
                kind,
                size_bytes,
            },
        });
        if retained.len() > MAX_LIST_ENTRIES {
            retained.pop();
        }
    }
    let mut retained = retained.into_vec();
    retained.sort_by(|left, right| left.sort_key.cmp(&right.sort_key));
    Ok(ListResponse {
        entries: retained.into_iter().map(|entry| entry.entry).collect(),
        truncated: visible_count > MAX_LIST_ENTRIES,
    })
}

/// Split a completion input lexically into `(parent, prefix)` WITHOUT any
/// filesystem access: the parent keeps its trailing separator (so an input
/// ending in `/` completes the directory itself, and a bare `/` stays
/// absolute), and the text after the last separator is the typed prefix.
fn split_completion_input(input: &str) -> (&str, &str) {
    match input.rfind('/') {
        Some(index) => (&input[..=index], &input[index + 1..]),
        None => ("", input),
    }
}

/// Case-insensitive workspace search for files and directories.
///
/// The tree search box used to filter only directories the client had already
/// listed, so a nested file never appeared until its parents were expanded,
/// and a 5000-entry directory listing could drop the name before that filter
/// ran. This walk uses the same ignore and no-follow rules as [`list`] and
/// returns both files and directories. A directory the captured policy
/// rejects is omitted and not walked, so a denied or secret subtree cannot
/// leak through a match. `truncated` is set when the match cap or the visit
/// cap stops the walk before the tree is exhausted.
pub(crate) fn search(app: &Application, query: &str) -> Result<SearchResponse> {
    if query.contains('\0') {
        bail!("path is outside the workspace");
    }
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Ok(SearchResponse {
            entries: Vec::new(),
            truncated: false,
        });
    }
    let workspace_root = app.session().cwd().to_path_buf();
    app.inspect_generation_directory(&workspace_root)
        .map_err(|error| map_directory_error(&error))?;
    let root_dir = open_workspace_root(&workspace_root)?;
    let mut queue = VecDeque::from([String::new()]);
    let mut matches = Vec::new();
    let mut visited = 0usize;
    let mut truncated = false;
    'walk: while let Some(relative) = queue.pop_front() {
        let relative_path = PathBuf::from(&relative);
        let ignore = build_ignore_stack(&root_dir, &relative_path)?;
        let directory = match open_relative_directory(
            root_dir
                .try_clone()
                .map_err(|_| anyhow!("directory is unavailable"))?,
            &relative_path,
        ) {
            Ok(directory) => directory,
            Err(error) if relative.is_empty() => return Err(error),
            Err(_) => continue,
        };
        for entry in directory
            .entries()
            .map_err(|_| anyhow!("directory is unavailable"))?
        {
            if visited >= MAX_SEARCH_VISITED || matches.len() >= MAX_SEARCH_MATCHES {
                truncated = true;
                break 'walk;
            }
            visited = visited.saturating_add(1);
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if name == ".git" {
                continue;
            }
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };
            if file_type.is_symlink() {
                continue;
            }
            let is_dir = file_type.is_dir();
            if !is_dir && !file_type.is_file() {
                continue;
            }
            if ignore.is_ignored(&relative_path.join(&name), is_dir) {
                continue;
            }
            let path = if relative.is_empty() {
                name.clone()
            } else {
                format!("{relative}/{name}")
            };
            if is_dir {
                // Denied or secret directories stay out of the results and
                // are not walked. An allowed directory is inspected once;
                // files inside it follow the same listing rules as [`list`].
                let child = workspace_root.join(&path);
                if app.inspect_generation_directory(&child).is_err() {
                    continue;
                }
                queue.push_back(path.clone());
            }
            if path.to_lowercase().contains(&needle) {
                matches.push(ListEntry {
                    name,
                    path,
                    kind: if is_dir { "dir" } else { "file" },
                    size_bytes: None,
                });
            }
        }
    }
    matches.sort_by(|left, right| {
        let rank = |kind: &str| if kind == "dir" { 0 } else { 1 };
        rank(left.kind)
            .cmp(&rank(right.kind))
            .then_with(|| left.path.to_lowercase().cmp(&right.path.to_lowercase()))
    });
    Ok(SearchResponse {
        entries: matches,
        truncated,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct SearchResponse {
    pub(crate) entries: Vec<ListEntry>,
    pub(crate) truncated: bool,
}

/// Complete a user-typed path for the frontend path fields (session cwd,
/// project cwd). Exactly ONE parent directory is listed:
///
/// - a relative input resolves against the current session cwd;
/// - an absolute input is accepted only when it LEXICALLY resolves beneath
///   one of the session's authorized workspace roots (cwd or additional
///   roots) — no host canonicalization widens the scope;
/// - the exact authorized root itself typed WITHOUT a trailing separator
///   is returned as a single directory candidate — the split peels its
///   basename off as the typed prefix, so the parent-containment gate would
///   otherwise reject a fully valid request; the unauthorized parent is
///   neither opened nor listed, and with a trailing separator the root's
///   children still list;
/// - the parent directory is validated through the captured PolicyFs
///   capability (denied/secret targets fail closed) and then listed through
///   the same no-follow capability-relative descent as [`list`], with every
///   suggested child re-inspected against the same policy so denied,
///   secret, or inaccessible entries are skipped silently and their names
///   never leak through an error;
///
/// Entries are filtered by the typed prefix, hidden names surface only once
/// the prefix itself begins with a dot, symlinks and `.git` never complete,
/// and results are directories first, names sorted within each kind, capped
/// at `limit` (default and ceiling [`MAX_COMPLETION_ENTRIES`]) with
/// `truncated` reporting overflow. Errors are fixed literals that never echo
/// workspace root paths.
pub(crate) fn complete_path(
    app: &Application,
    input: &str,
    kind: CompletionKind,
    limit: Option<usize>,
) -> Result<CompletePathResponse> {
    // Lexical gate: NUL can never occur in a real filesystem path, so any
    // NUL byte anywhere in the input (parent or typed prefix) is rejected
    // before policy inspection or listing with the same fixed error as
    // every other rejected request.
    if input.contains('\0') {
        bail!("path is outside the workspace");
    }
    let limit = limit
        .unwrap_or(MAX_COMPLETION_ENTRIES)
        .min(MAX_COMPLETION_ENTRIES);
    let session = app.session();
    let roots = session.workspace_roots();
    let (parent_text, prefix) = split_completion_input(input);
    let (root, relative_components) = if Path::new(parent_text).is_absolute() {
        let request = scoped_components(Path::new(parent_text), true)?;
        // Exact authorized root typed WITHOUT a trailing separator: the
        // split peels the root's basename off as the typed prefix, so the
        // parent sits ABOVE the authorized root and the containment strip
        // below would reject a fully valid request. Return the typed root
        // itself as the single directory candidate — the parent is not
        // authorized, so it is neither opened nor listed, and nothing
        // beyond the already-authorized roots is accepted.
        if !prefix.is_empty() {
            let typed = scoped_components(Path::new(input), true)?;
            if let Some(candidate) = roots
                .roots()
                .iter()
                .find(|root| scoped_components(root, true).is_ok_and(|components| components == typed))
            {
                let candidate_text = candidate.to_string_lossy();
                let trimmed = candidate_text.trim_end_matches('/');
                let base = if trimmed.is_empty() { "/" } else { trimmed };
                return Ok(CompletePathResponse {
                    base: base.to_owned(),
                    prefix: prefix.to_owned(),
                    entries: vec![CompletionEntry {
                        name: prefix.to_owned(),
                        path: base.to_owned(),
                        kind: "directory",
                    }],
                    truncated: false,
                });
            }
        }
        let mut matched = None;
        for candidate in roots.roots() {
            let root_components = scoped_components(candidate, true)?;
            if let Some(suffix) = request.strip_prefix(root_components.as_slice()) {
                matched = Some((candidate.as_path(), suffix.to_vec()));
                break;
            }
        }
        let Some((root, suffix)) = matched else {
            bail!("path is outside the workspace");
        };
        (root.to_path_buf(), suffix)
    } else {
        (
            roots.cwd().to_path_buf(),
            scoped_components(Path::new(parent_text), false)?,
        )
    };

    let relative = relative_components.iter().collect::<PathBuf>();
    // `.git` is denied outright: any exact parent path component is
    // rejected before inspection or listing, not merely filtered as an
    // entry suggestion.
    if relative_components
        .iter()
        .any(|component| component == ".git")
    {
        bail!("path is outside the workspace");
    }
    // The base preserves the input's own form domain: an absolute request
    // gets an absolute base (root-joined, lexically normalized), a relative
    // request a workspace-relative base.
    let base = if Path::new(parent_text).is_absolute() {
        // `root.join("")` appends a trailing separator when the input lists
        // an authorized root itself; normalize it away so candidate paths
        // join with a single `/` and never contain `//`.
        let joined = root.join(&relative).to_string_lossy().into_owned();
        let trimmed = joined.trim_end_matches('/');
        if trimmed.is_empty() {
            "/".to_owned()
        } else {
            trimmed.to_owned()
        }
    } else {
        relative_components
            .iter()
            .map(|component| component.to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    };
    // Diagnostic input for the captured PolicyFs only; the listing below
    // re-opens the directory no-follow, capability relative.
    let absolute = root.join(&relative);
    app.inspect_generation_directory(&absolute)
        .map_err(|error| map_directory_error(&error))?;

    let mut retained = BinaryHeap::with_capacity(limit + 1);
    let root_dir = open_workspace_root(&root)?;
    let ignore = build_ignore_stack(&root_dir, &relative)?;
    let directory = open_relative_directory(root_dir, &relative)?;
    let mut matched = 0usize;
    for entry in directory
        .entries()
        .map_err(|_| anyhow!("directory is unavailable"))?
    {
        let entry = entry.map_err(|_| anyhow!("directory is unavailable"))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name == ".git" {
            continue;
        }
        if !name.starts_with(prefix) {
            continue;
        }
        // Hidden entries surface only once the typed prefix itself begins
        // with a dot.
        if !prefix.starts_with('.') && name.starts_with('.') {
            continue;
        }
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        // A symlinked suggestion would be an escape hatch, so no-follow
        // semantics drop them entirely.
        if file_type.is_symlink() {
            continue;
        }
        if ignore.is_ignored(&relative.join(&name), file_type.is_dir()) {
            continue;
        }
        let (entry_kind, rank) = if file_type.is_dir() {
            if matches!(kind, CompletionKind::File) {
                continue;
            }
            ("directory", 0)
        } else if file_type.is_file() {
            if matches!(kind, CompletionKind::Directory) {
                continue;
            }
            ("file", 1)
        } else {
            continue;
        };
        // Every suggestion is re-inspected against the captured policy so a
        // denied, secret, or otherwise inaccessible child inside an
        // otherwise-listable directory never surfaces and its name never
        // leaks. `UnsupportedEntryType` and a `NotADirectory` system error
        // mean the no-follow policy open resolved a non-directory child (a
        // regular file or similar), which remains a legitimate suggestion;
        // any other failure skips the entry silently.
        if let Err(error) = app.inspect_generation_directory(&absolute.join(&name)) {
            match error.downcast_ref::<FsError>() {
                Some(FsError::UnsupportedEntryType) => {}
                Some(FsError::System { kind, .. }) if *kind == ErrorKind::NotADirectory => {}
                _ => continue,
            }
        }
        matched = matched.saturating_add(1);
        let entry_path = if base.is_empty() {
            name.clone()
        } else {
            format!("{base}/{name}")
        };
        retained.push(CompletionCandidate {
            sort_key: (rank, name.clone()),
            entry: CompletionEntry {
                name,
                path: entry_path,
                kind: entry_kind,
            },
        });
        if retained.len() > limit {
            retained.pop();
        }
    }
    let mut retained = retained.into_vec();
    retained.sort_by(|left, right| left.sort_key.cmp(&right.sort_key));
    Ok(CompletePathResponse {
        base,
        prefix: prefix.to_owned(),
        entries: retained.into_iter().map(|candidate| candidate.entry).collect(),
        truncated: matched > limit,
    })
}

pub(crate) fn read(
    app: &Application,
    path: &str,
    cursor: usize,
    max_lines: Option<usize>,
) -> Result<ReadResponse> {
    let workspace_root = app.session().cwd().to_path_buf();
    match read_page(app, &workspace_root, path, cursor, max_lines) {
        Ok(response) => Ok(response),
        // A basename-only reference from transcript prose (`init.rs:495`)
        // misses at the workspace root yet names a real file deeper in the
        // tree. Resolve a UNIQUE exact-basename match and read that instead;
        // zero or several matches keep the stable missing-file error — the
        // request never guesses between homonyms.
        Err(error)
            if !path.contains('/')
                && !path.contains('\\')
                && error.downcast_ref::<FileExplorerError>()
                    == Some(&FileExplorerError::FileUnavailable) =>
        {
            match unique_basename_file(&workspace_root, Path::new(path)) {
                Some(found) => read_page(app, &workspace_root, &found, cursor, max_lines),
                None => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn read_page(
    app: &Application,
    workspace_root: &Path,
    path: &str,
    cursor: usize,
    max_lines: Option<usize>,
) -> Result<ReadResponse> {
    let (absolute, relative) = resolve_scoped_path(workspace_root, path, false)?;
    validate_regular_file(app, &absolute)?;
    let file = open_relative_file(workspace_root, Path::new(&relative))?;
    let size_bytes = file
        .metadata()
        .map_err(|_| FileExplorerError::FileUnavailable)?
        .len();
    let page = text_page(
        BufReader::new(file),
        &relative,
        cursor,
        max_lines,
        MAX_PAGE_CONTENT_BYTES,
    )?;
    Ok(ReadResponse {
        path: response_path(&relative),
        lines: page.lines,
        next_cursor: page.next_cursor,
        total_lines: page.total_lines,
        size_bytes,
        truncated: page.next_cursor.is_some(),
    })
}

/// Caps for [`unique_basename_file`]: the fallback walk must stay bounded on
/// any workspace shape, so it stops at the first ambiguity, a depth ceiling,
/// and a visited-entry budget. Every cap that stops the walk short of the
/// whole eligible tree makes that walk INCOMPLETE — an unfinished walk cannot
/// prove absence, so it never yields a verdict. `.git`, `target`, and
/// `node_modules` are the heavy subtrees that never legitimately host a cited
/// source file; they are exclusions, not bounds.
const BASENAME_SEARCH_MAX_DEPTH: usize = 32;
const BASENAME_SEARCH_MAX_VISITED: usize = 50_000;
const BASENAME_SEARCH_SKIP_DIRS: &[&str] = &[".git", "target", "node_modules"];

/// The single workspace file whose exact file name equals `leaf`, found by a
/// bounded no-follow walk from the workspace root. `Some` requires BOTH a
/// COMPLETE traversal of the eligible tree AND exactly one match; either half
/// alone is no proof. Counting one observed match says nothing about
/// homonyms the walk never reached, so the verdict is withheld whenever the
/// walk stopped early — a visited-entry budget stop, a depth-ceiling stop, or
/// any enumeration/open failure. Zero matches return `None` and keep the
/// caller's original error, exactly one match on a complete walk returns its
/// path, and a second match is homonym ambiguity and returns `None`, so the
/// fallback can only ever narrow, never widen, what a direct read would have
/// answered. Skipped heavy subtrees and symlinks are intentional exclusions,
/// not candidates (symlinks never match or recurse — the same no-follow
/// authority as every other read here).
fn unique_basename_file(root: &Path, leaf: &Path) -> Option<String> {
    let leaf = leaf.file_name()?;
    if leaf.is_empty() {
        return None;
    }
    let mut matches: Vec<String> = Vec::new();
    let mut visited = 0usize;
    let root_dir = open_workspace_root(root).ok()?;
    let complete = walk_basename(&root_dir, "", 0, leaf, &mut visited, &mut matches);
    if complete && matches.len() == 1 {
        matches.pop()
    } else {
        None
    }
}

/// Depth-first, no-follow enumeration collecting every workspace-relative
/// path whose file name equals `leaf`. Returns `true` only when the whole
/// eligible subtree was examined. Every other exit is `false` and MUST be
/// read as "unexamined entries may still hide a homonym": a
/// [`BASENAME_SEARCH_MAX_VISITED`] stop, a subtree refused at the
/// [`BASENAME_SEARCH_MAX_DEPTH`] ceiling, a directory that cannot be listed,
/// an entry whose kind cannot be read, and a child directory that cannot be
/// opened. A second match also ends the walk early with `false` (the caller
/// rejects the ambiguity regardless, so no further proof is needed).
fn walk_basename(
    directory: &Dir,
    prefix: &str,
    depth: usize,
    leaf: &OsStr,
    visited: &mut usize,
    matches: &mut Vec<String>,
) -> bool {
    if matches.len() > 1 {
        return false;
    }
    if *visited >= BASENAME_SEARCH_MAX_VISITED {
        return false;
    }
    let Ok(entries) = directory.entries() else { return false };
    for entry in entries {
        *visited += 1;
        if *visited >= BASENAME_SEARCH_MAX_VISITED {
            return false;
        }
        let Ok(entry) = entry else { return false };
        let Ok(file_type) = entry.file_type() else { return false };
        if file_type.is_symlink() {
            continue;
        }
        let name = entry.file_name();
        if file_type.is_file() && name == leaf {
            matches.push(format!("{prefix}{}", name.to_string_lossy()));
            if matches.len() > 1 {
                return false;
            }
            continue;
        }
        if file_type.is_dir() {
            if depth + 1 >= BASENAME_SEARCH_MAX_DEPTH {
                return false;
            }
            let skipped = BASENAME_SEARCH_SKIP_DIRS
                .iter()
                .any(|skipped| *skipped == name.to_string_lossy().as_ref());
            if skipped {
                continue;
            }
            let Ok(child) = directory.open_dir_nofollow(&name) else { return false };
            if !walk_basename(
                &child,
                &format!("{prefix}{}/", name.to_string_lossy()),
                depth + 1,
                leaf,
                visited,
                matches,
            ) {
                return false;
            }
        }
    }
    true
}


pub(crate) fn changes(app: &Application) -> Result<ChangesResponse> {
    let workspace_root = app.session().cwd().to_path_buf();
    app.inspect_generation_directory(&workspace_root)
        .map_err(|error| map_directory_error(&error))?;
    let Some((sandbox, prefix)) = repository_for_workspace(&workspace_root) else {
        return Ok(ChangesResponse {
            files: Vec::new(),
            is_repo: false,
            truncated: false,
        });
    };
    let mut status_args = vec![
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=all",
        "--",
    ];
    status_args.push(prefix.as_deref().unwrap_or("."));
    let output = run_git_bounded_timeout(
        sandbox.work_tree(),
        &status_args,
        MAX_GIT_OUTPUT_BYTES,
        Some(sandbox.environment()),
        GIT_TIMEOUT,
    )
    .map_err(|_| anyhow!("repository changes are unavailable"))?;
    if output.error.is_some() {
        bail!("repository changes are unavailable");
    }
    if output.truncated {
        bail!("repository change list exceeds the size limit");
    }
    let (files, truncated) = parse_status(&output.stdout, prefix.as_deref());
    let (files, truncated) = expand_gitlink_changes(
        &workspace_root,
        &sandbox,
        prefix.as_deref(),
        files,
        truncated,
        0,
    )?;
    Ok(ChangesResponse {
        files,
        is_repo: true,
        truncated,
    })
}

pub(crate) fn diff(
    app: &Application,
    path: &str,
    cursor: usize,
    max_lines: Option<usize>,
) -> Result<DiffResponse> {
    let workspace_root = app.session().cwd().to_path_buf();
    let (absolute, relative) = resolve_scoped_path(&workspace_root, path, false)?;
    let changes = changes(app)?;
    if !changes.is_repo {
        bail!("not a repository");
    }
    let change = changes.files.iter().find(|file| file.path == relative);
    validate_diff_target(app, &absolute, change)?;

    let (sandbox, repository_path) = covering_repository(&workspace_root, &relative)?;
    let old_repository_path = match change.and_then(|file| file.old_path.as_deref()) {
        Some(old_path) => Some(covering_repository(&workspace_root, old_path)?.1),
        None => None,
    };
    let untracked = change.is_some_and(|file| file.status == "added")
        && is_untracked_path(
            &sandbox,
            old_repository_path
                .as_deref()
                .unwrap_or(repository_path.as_str()),
        )?;

    let mut args = vec![
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--default-prefix",
        "--no-relative",
        "--find-renames",
        "--histogram",
    ];
    if untracked {
        args.extend(["--no-index", "--", "/dev/null", repository_path.as_str()]);
    } else {
        args.extend(["HEAD", "--"]);
        if let Some(old_path) = old_repository_path.as_deref() {
            args.push(old_path);
        }
        args.push(repository_path.as_str());
    }
    let output = run_git_bounded_timeout(
        sandbox.work_tree(),
        &args,
        MAX_GIT_OUTPUT_BYTES,
        if untracked {
            None
        } else {
            Some(sandbox.environment())
        },
        GIT_TIMEOUT,
    )
    .map_err(|_| anyhow!("file diff is unavailable"))?;
    if output.error.is_some() && !(untracked && !output.stdout.is_empty()) {
        bail!("file diff is unavailable");
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let page = page_from_lines(
        text.lines().map(str::to_owned),
        &relative,
        cursor,
        max_lines,
        MAX_PAGE_CONTENT_BYTES,
    )?;
    Ok(DiffResponse {
        path: response_path(&relative),
        lines: page.lines,
        next_cursor: page.next_cursor,
        total_lines: page.total_lines,
        truncated: page.next_cursor.is_some() || output.truncated,
    })
}

/// Attribute one inclusive 1-based source selection. The existing read page is
/// the path and text authority; git only supplies commit metadata for lines
/// that still match that page.
pub(crate) fn blame(
    app: &Application,
    path: &str,
    start_line: usize,
    end_line: usize,
) -> Result<BlameResponse> {
    if start_line == 0 || end_line < start_line {
        bail!("line range is outside the file");
    }
    let count = end_line
        .checked_sub(start_line)
        .and_then(|span| span.checked_add(1))
        .ok_or_else(|| anyhow!("line range is outside the file"))?;
    if count > MAX_PAGE_LINES {
        bail!("blame selection exceeds 2000 lines");
    }
    let workspace_root = app.session().cwd().to_path_buf();
    let page = match read_page(
        app,
        &workspace_root,
        path,
        start_line - 1,
        Some(count),
    ) {
        Ok(page) => page,
        Err(error) if error.to_string() == "cursor out of range" => {
            bail!("line range is outside the file");
        }
        Err(error) => return Err(error),
    };
    if page.lines.len() != count {
        if end_line > page.total_lines {
            bail!("line range is outside the file");
        }
        bail!("blame selection exceeds the response limit");
    }
    let (sandbox, repository_path) = covering_repository(&workspace_root, &page.path)?;
    let untracked = is_untracked_path(&sandbox, &repository_path).map_err(|error| {
        if error.downcast_ref::<FileExplorerError>().is_some() {
            error
        } else {
            anyhow!("file blame is unavailable")
        }
    })?;
    if untracked {
        bail!("file is untracked");
    }
    let range = format!("{start_line},{end_line}");
    let output = run_git_bounded_timeout(
        sandbox.work_tree(),
        &[
            "blame",
            "--line-porcelain",
            "--no-progress",
            "--encoding=none",
            "-L",
            &range,
            "--",
            &repository_path,
        ],
        MAX_GIT_OUTPUT_BYTES,
        Some(sandbox.environment()),
        GIT_TIMEOUT,
    )
    .map_err(|_| anyhow!("file blame is unavailable"))?;
    if output.error.is_some() || output.truncated {
        bail!("file blame is unavailable");
    }
    parse_line_porcelain(&output.stdout, &page.path, start_line, &page.lines)
}

fn parse_line_porcelain(
    stdout: &[u8],
    path: &str,
    start_line: usize,
    source: &[String],
) -> Result<BlameResponse> {
    let mut lines = Vec::with_capacity(source.len());
    let mut serialized = empty_blame_bytes(path)?;
    let mut records = stdout.split(|byte| *byte == b'\n');
    for (offset, expected) in source.iter().enumerate() {
        let header = records
            .next()
            .filter(|line| !line.is_empty())
            .ok_or_else(|| anyhow!("file blame is unavailable"))?;
        let mut header_fields = header.split(|byte| *byte == b' ');
        let oid = header_fields
            .next()
            .filter(|field| !field.is_empty())
            .ok_or_else(|| anyhow!("file blame is unavailable"))?;
        let original = header_fields
            .next()
            .filter(|field| !field.is_empty())
            .ok_or_else(|| anyhow!("file blame is unavailable"))?;
        let current = header_fields
            .next()
            .filter(|field| !field.is_empty())
            .ok_or_else(|| anyhow!("file blame is unavailable"))?;
        let group = header_fields.next();
        if header_fields.next().is_some() {
            bail!("file blame is unavailable");
        }
        if let Some(group) = group {
            let _ = parse_porcelain_line_number(group)?;
        }
        let current_line = parse_porcelain_line_number(current)?;
        if current_line != start_line + offset {
            bail!("file blame is unavailable");
        }
        let original_line = parse_porcelain_line_number(original)?;
        let mut author = None;
        let mut time = None;
        let mut text = None;
        loop {
            let Some(field) = records.next() else {
                bail!("file blame is unavailable");
            };
            if let Some(value) = field.strip_prefix(b"author ") {
                if author.is_some() {
                    bail!("file blame is unavailable");
                }
                author = Some(sanitize_blame_author(value)?);
            } else if let Some(value) = field.strip_prefix(b"author-time ") {
                if time.is_some() {
                    bail!("file blame is unavailable");
                }
                time = Some(parse_porcelain_time(value)?);
            } else if let Some(value) = field.strip_prefix(b"\t") {
                if text.is_some() {
                    bail!("file blame is unavailable");
                }
                text = Some(porcelain_text(value)?);
                break;
            } else if field.is_empty() || field[0].is_ascii_digit() || field[0] == b'^' {
                bail!("file blame is unavailable");
            }
        }
        let (Some(author), Some(time), Some(text)) = (author, time, text) else {
            bail!("file blame is unavailable");
        };
        if text != *expected {
            bail!("file blame is unavailable");
        }
        let line = BlameLine {
            commit: blame_commit(oid)?,
            author,
            time,
            original_line,
            current_line,
            text,
        };
        let encoded = serde_json::to_vec(&line).map_err(|_| anyhow!("file blame is unavailable"))?;
        serialized = serialized
            .checked_add(encoded.len() + 1)
            .ok_or_else(|| anyhow!("blame selection exceeds the response limit"))?;
        if serialized > MAX_PAGE_BYTES {
            bail!("blame selection exceeds the response limit");
        }
        lines.push(line);
    }
    let remainder = records.collect::<Vec<_>>();
    if remainder.iter().any(|record| !record.is_empty()) {
        bail!("file blame is unavailable");
    }
    if lines.len() != source.len() {
        bail!("file blame is unavailable");
    }
    Ok(BlameResponse {
        path: path.to_owned(),
        lines,
    })
}

fn empty_blame_bytes(path: &str) -> Result<usize> {
    Ok(serde_json::to_vec(&BlameResponse {
        path: path.to_owned(),
        lines: Vec::new(),
    })?
    .len())
}

fn parse_porcelain_line_number(value: &[u8]) -> Result<usize> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        bail!("file blame is unavailable");
    }
    let text = std::str::from_utf8(value).map_err(|_| anyhow!("file blame is unavailable"))?;
    let number = text
        .parse::<usize>()
        .map_err(|_| anyhow!("file blame is unavailable"))?;
    if number == 0 {
        bail!("file blame is unavailable");
    }
    Ok(number)
}

fn parse_porcelain_time(value: &[u8]) -> Result<u64> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        bail!("file blame is unavailable");
    }
    let text = std::str::from_utf8(value).map_err(|_| anyhow!("file blame is unavailable"))?;
    text.parse::<u64>()
        .map_err(|_| anyhow!("file blame is unavailable"))
}

fn blame_commit(oid: &[u8]) -> Result<Option<String>> {
    let oid = oid.strip_prefix(b"^").unwrap_or(oid);
    if !matches!(oid.len(), 40 | 64) || !oid.iter().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("file blame is unavailable");
    }
    if oid.iter().all(|byte| *byte == b'0') {
        return Ok(None);
    }
    let commit = std::str::from_utf8(oid).map_err(|_| anyhow!("file blame is unavailable"))?;
    Ok(Some(commit.to_ascii_lowercase()))
}

fn sanitize_blame_author(value: &[u8]) -> Result<String> {
    let author = std::str::from_utf8(value).map_err(|_| anyhow!("file blame is unavailable"))?;
    if author.chars().any(char::is_control) {
        bail!("file blame is unavailable");
    }
    Ok(author.to_owned())
}

fn porcelain_text(value: &[u8]) -> Result<String> {
    if value.len() > MAX_TEXT_LINE_BYTES || value.contains(&0) {
        bail!("file blame is unavailable");
    }
    let text = std::str::from_utf8(value).map_err(|_| anyhow!("file blame is unavailable"))?;
    Ok(text.strip_suffix('\r').unwrap_or(text).to_owned())
}

pub(crate) fn download(
    app: &Application,
    path: &str,
    max_bytes: usize,
) -> Result<(Vec<u8>, String)> {
    let workspace_root = app.session().cwd().to_path_buf();
    let (absolute, relative) = resolve_scoped_path(&workspace_root, path, false)?;
    let bytes = match app.read_generation_file_bounded(&absolute, max_bytes as u64) {
        Ok(bytes) => bytes,
        Err(error) => match error.downcast_ref::<FsError>() {
            Some(FsError::ReadLimitExceeded { .. }) => bail!("file exceeds download limit"),
            Some(FsError::UnsupportedEntryType) => bail!("not a file"),
            Some(FsError::OutsideCapability | FsError::Denied | FsError::InvalidPath) => {
                bail!("path is outside the workspace")
            }
            Some(fs_error @ FsError::System { .. }) => {
                return Err(unavailable_file_error(fs_error));
            }
            _ if app.inspect_generation_directory(&absolute).is_ok() => bail!("not a file"),
            _ => bail!("file could not be read right now"),
        },
    };
    let file_name = Path::new(&relative)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow!("file name is unavailable"))?
        .to_owned();
    Ok((bytes, file_name))
}

/// Bounded, path-free classification of an IO failure against a workspace
/// file. Missing (NotFound) is `file_unavailable`. The no-follow symlink
/// rejection (ELOOP 40 Linux / 62 macOS) is `file_symlink`. Every other IO
/// error (EACCES/ESTALE/transient) reads as 'file could not be read right
/// now' so cutover failures stay distinguishable from a gone path.
fn unavailable_file_error_from_kind(
    kind: std::io::ErrorKind,
    raw_os_error: Option<i32>,
) -> anyhow::Error {
    if kind == std::io::ErrorKind::NotFound {
        FileExplorerError::FileUnavailable.into()
    } else if raw_os_error.is_some_and(|code| code == 40 || code == 62) {
        FileExplorerError::FileSymlink.into()
    } else {
        anyhow!("file could not be read right now")
    }
}

fn unavailable_file_error(error: &FsError) -> anyhow::Error {
    match error {
        FsError::System {
            kind, raw_os_error, ..
        } => unavailable_file_error_from_kind(*kind, *raw_os_error),
        _ => anyhow!("file could not be read right now"),
    }
}

fn validate_regular_file(app: &Application, absolute: &Path) -> Result<()> {
    match app.read_generation_file_bounded(absolute, FILE_VALIDATION_BYTES) {
        Ok(_) => Ok(()),
        Err(error) => match error.downcast_ref::<FsError>() {
            Some(FsError::ReadLimitExceeded { .. }) => Ok(()),
            Some(FsError::UnsupportedEntryType) => bail!("not a file"),
            Some(FsError::OutsideCapability | FsError::Denied | FsError::InvalidPath) => {
                bail!("path is outside the workspace")
            }
            Some(fs_error @ FsError::System { .. }) => {
                return Err(unavailable_file_error(fs_error));
            }
            _ if app.inspect_generation_directory(absolute).is_ok() => bail!("not a file"),
            _ => bail!("file could not be read right now"),
        },
    }
}

fn validate_diff_target(
    app: &Application,
    absolute: &Path,
    change: Option<&ChangedFile>,
) -> Result<()> {
    match validate_regular_file(app, absolute) {
        Ok(()) => Ok(()),
        Err(error)
            if error.downcast_ref() == Some(&FileExplorerError::FileUnavailable)
                && change.is_some_and(|file| matches!(file.status, "deleted" | "renamed")) =>
        {
            let mut ancestor = absolute.parent();
            while let Some(directory) = ancestor {
                match app.inspect_generation_directory(directory) {
                    Ok(_) => return Ok(()),
                    Err(error) => match error.downcast_ref::<FsError>() {
                        Some(FsError::System {
                            kind: std::io::ErrorKind::NotFound,
                            ..
                        }) => ancestor = directory.parent(),
                        Some(FsError::OutsideCapability | FsError::Denied | FsError::InvalidPath) => {
                            bail!("path is outside the workspace")
                        }
                        _ => bail!(FileExplorerError::FileUnavailable),
                    },
                }
            }
            bail!(FileExplorerError::FileUnavailable)
        }
        Err(error) => Err(error),
    }
}

fn map_directory_error(error: &anyhow::Error) -> anyhow::Error {
    match error.downcast_ref::<FsError>() {
        Some(FsError::UnsupportedEntryType) => FileExplorerError::DirectoryUnavailable.into(),
        Some(FsError::OutsideCapability | FsError::Denied | FsError::InvalidPath) => {
            anyhow!("path is outside the workspace")
        }
        Some(FsError::System {
            kind: std::io::ErrorKind::NotFound
                | std::io::ErrorKind::PermissionDenied
                | std::io::ErrorKind::NotADirectory,
            ..
        }) => FileExplorerError::DirectoryUnavailable.into(),
        Some(FsError::System {
            raw_os_error: Some(40 | 62),
            ..
        }) => FileExplorerError::DirectoryUnavailable.into(),
        _ => anyhow!("directory is unavailable"),
    }
}

fn open_workspace_root(root: &Path) -> Result<Dir> {
    Dir::open_ambient_dir(root, ambient_authority()).map_err(|_| anyhow!("workspace is unavailable"))
}

fn open_relative_directory(mut directory: Dir, relative: &Path) -> Result<Dir> {
    for component in relative.components() {
        let Component::Normal(component) = component else {
            bail!("path is outside the workspace");
        };
        directory = directory.open_dir_nofollow(component).map_err(|error| {
            if error.kind() == ErrorKind::NotFound
                || error.kind() == ErrorKind::PermissionDenied
                || error.raw_os_error().is_some_and(|code| code == 40 || code == 62)
            {
                FileExplorerError::DirectoryUnavailable.into()
            } else {
                anyhow!("directory is unavailable")
            }
        })?;
    }
    Ok(directory)
}

fn open_relative_file(root: &Path, relative: &Path) -> Result<File> {
    let mut components = relative.components().peekable();
    let mut directory = open_workspace_root(root)?;
    while let Some(component) = components.next() {
        let Component::Normal(component) = component else {
            bail!("path is outside the workspace");
        };
        if components.peek().is_none() {
            let mut options = OpenOptions::new();
            options.read(true).follow(FollowSymlinks::No);
            return directory.open_with(component, &options).map_err(|error| {
                unavailable_file_error_from_kind(error.kind(), error.raw_os_error())
            });
        }
        directory = directory.open_dir_nofollow(component).map_err(|error| {
            unavailable_file_error_from_kind(error.kind(), error.raw_os_error())
        })?;
    }
    bail!("not a file")
}

/// Delete a file or directory (recursive) inside the workspace. The path is
/// scope-resolved against the workspace root, then the target's parent
/// directory is opened no-follow and the leaf is removed through the held
/// dirfd (dirfd-relative removal), so a symlink swap between validation and
/// removal cannot redirect the operation. `.git` and the workspace root
/// itself are refused.
pub(crate) fn delete_path(app: &Application, path: &str) -> Result<DeleteResponse> {
    let workspace_root = app.session().cwd().to_path_buf();
    let (absolute, relative) = resolve_scoped_path(&workspace_root, path, true)?;
    if relative.is_empty() {
        bail!("cannot delete the workspace root");
    }
    let root = open_workspace_root(&workspace_root)?;
    let components: Vec<_> = Path::new(&relative).components().collect();
    if components.is_empty() {
        bail!("cannot delete the workspace root");
    }
    let leaf_os = components.last().unwrap();
    let parent_relative: PathBuf = if components.len() <= 1 {
        PathBuf::new()
    } else {
        components[..components.len() - 1].iter().collect()
    };
    let parent_dir = if parent_relative.as_os_str().is_empty() {
        root
    } else {
        open_relative_directory(root, &parent_relative)?
    };
    let leaf = Path::new(leaf_os);
    // Probe the entry no-follow to determine its type before deletion.
    let metadata = match parent_dir.symlink_metadata(leaf) {
        Ok(meta) => meta,
        Err(_) => bail!("path is unavailable"),
    };
    // Refuse .git and workspace root.
    let full = absolute.to_string_lossy();
    if full.ends_with("/.git") || full.contains("/.git/") || relative == ".git" || relative.starts_with(".git/") {
        bail!("cannot delete .git");
    }
    if metadata.is_dir() {
        parent_dir.remove_dir_all(leaf).map_err(|error| anyhow!("delete failed: {error}"))?;
    } else {
        parent_dir.remove_file(leaf).map_err(|error| anyhow!("delete failed: {error}"))?;
    }
    Ok(DeleteResponse { path: relative, deleted: true })
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct DeleteResponse {
    pub path: String,
    pub deleted: bool,
}

/// Gitignore filtering for [`list`] and [`complete_path`]: one matcher per
/// `.gitignore` from the workspace root through the listed directory.
struct IgnoreStack {
    matchers: Vec<(usize, Gitignore)>,
    subtree_ignored: bool,
}

impl IgnoreStack {
    fn is_ignored(&self, candidate: &Path, is_dir: bool) -> bool {
        if self.subtree_ignored {
            return true;
        }
        let components: Vec<OsString> = candidate
            .components()
            .map(|component| component.as_os_str().to_owned())
            .collect();
        for level in 1..components.len() {
            if stack_ignores_path(&self.matchers, &components[..level], true) {
                return true;
            }
        }
        stack_ignores_path(&self.matchers, &components, is_dir)
    }
}

fn stack_ignores_path(
    matchers: &[(usize, Gitignore)],
    components: &[OsString],
    is_dir: bool,
) -> bool {
    for (depth, matcher) in matchers.iter().rev() {
        if *depth >= components.len() {
            continue;
        }
        let tail: PathBuf = components[*depth..].iter().collect();
        match matcher.matched(&tail, is_dir) {
            Match::None => {}
            Match::Ignore(_) => return true,
            Match::Whitelist(_) => break,
        }
    }
    false
}

fn build_ignore_stack(root: &Dir, relative: &Path) -> Result<IgnoreStack> {
    let mut matchers = Vec::new();
    let mut directory = PathBuf::new();
    let components: Vec<OsString> = relative
        .components()
        .map(|component| component.as_os_str().to_owned())
        .collect();
    for depth in 0..=components.len() {
        // Per git, nothing beneath an ignored directory can be re-included.
        if depth > 0 && stack_ignores_path(&matchers, &components[..depth], true) {
            return Ok(IgnoreStack {
                matchers,
                subtree_ignored: true,
            });
        }
        if let Some(matcher) = read_gitignore_matcher(root, &directory)? {
            matchers.push((depth, matcher));
        }
        if let Some(component) = components.get(depth) {
            directory.push(component);
        }
    }
    Ok(IgnoreStack {
        matchers,
        subtree_ignored: false,
    })
}

/// Missing `.gitignore` (or its no-follow symlink rejection) yields no
/// matcher; oversized, unreadable, or invalid files fail closed.
fn read_gitignore_matcher(root: &Dir, directory: &Path) -> Result<Option<Gitignore>> {
    let mut dir = root
        .try_clone()
        .map_err(|_| anyhow!("directory is unavailable"))?;
    for component in directory.components() {
        let Component::Normal(component) = component else {
            bail!("path is outside the workspace");
        };
        dir = dir.open_dir_nofollow(component).map_err(|error| {
            if error.kind() == ErrorKind::NotFound
                || error.kind() == ErrorKind::PermissionDenied
                || error.kind() == ErrorKind::NotADirectory
                || error.raw_os_error().is_some_and(|code| code == 40 || code == 62)
            {
                FileExplorerError::DirectoryUnavailable.into()
            } else {
                anyhow!("directory is unavailable")
            }
        })?;
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = match dir.open_with(".gitignore", &options) {
        Ok(file) => file,
        Err(error)
            if error.kind() == ErrorKind::NotFound
                || error.raw_os_error().is_some_and(|code| code == 40 || code == 62) =>
        {
            return Ok(None);
        }
        Err(_) => bail!("directory is unavailable"),
    };
    let mut bytes = Vec::new();
    file.take((MAX_GITIGNORE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow!("directory is unavailable"))?;
    if bytes.len() > MAX_GITIGNORE_BYTES {
        bail!("directory is unavailable");
    }
    let mut builder = GitignoreBuilder::new("");
    for line in String::from_utf8_lossy(&bytes).lines() {
        builder
            .add_line(None, line)
            .map_err(|_| anyhow!("directory is unavailable"))?;
    }
    let matcher = builder
        .build()
        .map_err(|_| anyhow!("directory is unavailable"))?;
    Ok(Some(matcher))
}

#[derive(Debug)]
struct TextPage {
    lines: Vec<String>,
    next_cursor: Option<usize>,
    total_lines: usize,
}

fn text_page<R: BufRead>(
    mut reader: R,
    path: &str,
    cursor: usize,
    max_lines: Option<usize>,
    max_bytes: usize,
) -> Result<TextPage> {
    let cap = max_lines
        .unwrap_or(DEFAULT_PAGE_LINES)
        .clamp(1, MAX_PAGE_LINES);
    let mut lines = Vec::new();
    let mut index = 0usize;
    let mut serialized_bytes = empty_page_bytes(path)?;
    loop {
        let Some(bytes) = read_line_bounded(&mut reader)? else {
            if cursor > index {
                bail!("cursor out of range");
            }
            return Ok(TextPage {
                lines,
                next_cursor: None,
                total_lines: index,
            });
        };
        if bytes.contains(&0) {
            bail!("not a text file");
        }
        let line = String::from_utf8(bytes).map_err(|_| anyhow!("not a text file"))?;
        if index < cursor {
            index = index.saturating_add(1);
            continue;
        }
        let encoded = serde_json::to_vec(&line).map_err(|_| FileExplorerError::FileUnavailable)?;
        if lines.len() >= cap || serialized_bytes + encoded.len() + 1 > max_bytes {
            if lines.is_empty() {
                bail!(FileExplorerError::LineTooLarge);
            }
            // The page is full, but the total must still be exact: stream the
            // remaining logical lines to count them without assembling any of
            // them in memory. The overflow line is one logical line too.
            let remaining = count_remaining_lines(&mut reader)?;
            let total = index.saturating_add(1).saturating_add(remaining);
            return Ok(TextPage {
                lines,
                next_cursor: Some(index),
                total_lines: total,
            });
        }
        serialized_bytes += encoded.len() + 1;
        lines.push(line);
        index = index.saturating_add(1);
    }
}

/// Counts the logical lines after a page window in one streaming pass. Line
/// content is never buffered beyond the reader's internal chunk, so totals
/// stay exact regardless of file size, and line-length limits apply to
/// rendered lines only.
fn count_remaining_lines<R: BufRead>(mut reader: R) -> Result<usize> {
    let mut newlines = 0usize;
    let mut after_newline = false;
    loop {
        let available = reader
            .fill_buf()
            .map_err(|_| FileExplorerError::FileUnavailable)?;
        if available.is_empty() {
            return Ok(newlines + usize::from(after_newline));
        }
        if available.contains(&0) {
            bail!("not a text file");
        }
        newlines += available.iter().filter(|&&byte| byte == b'\n').count();
        let length = available.len();
        after_newline = available.last().copied() != Some(b'\n');
        reader.consume(length);
    }
}

fn page_from_lines<I>(
    lines: I,
    path: &str,
    cursor: usize,
    max_lines: Option<usize>,
    max_bytes: usize,
) -> Result<TextPage>
where
    I: IntoIterator<Item = String>,
{
    let cap = max_lines
        .unwrap_or(DEFAULT_PAGE_LINES)
        .clamp(1, MAX_PAGE_LINES);
    let mut page = Vec::new();
    let mut serialized_bytes = empty_page_bytes(path)?;
    let mut total = 0usize;
    let mut iterator = lines.into_iter();
    while let Some(line) = iterator.next() {
        if total < cursor {
            total = total.saturating_add(1);
            continue;
        }
        let encoded = serde_json::to_vec(&line).map_err(|_| anyhow!("file diff is unavailable"))?;
        if page.len() >= cap || serialized_bytes + encoded.len() + 1 > max_bytes {
            if page.is_empty() {
                bail!(FileExplorerError::LineTooLarge);
            }
            // The page window is closed, but the diff stream is the source of
            // the total: drain the rest so the reported count stays exact.
            let next_cursor = total;
            total = total.saturating_add(iterator.count()).saturating_add(1);
            return Ok(TextPage {
                lines: page,
                next_cursor: Some(next_cursor),
                total_lines: total,
            });
        }
        serialized_bytes += encoded.len() + 1;
        page.push(line);
        total = total.saturating_add(1);
    }
    if cursor > total {
        bail!("cursor out of range");
    }
    Ok(TextPage {
        lines: page,
        next_cursor: None,
        total_lines: total,
    })
}

fn empty_page_bytes(path: &str) -> Result<usize> {
    serde_json::to_vec(&json!({
        "path": path,
        "lines": [],
        "nextCursor": usize::MAX,
        "totalLines": usize::MAX,
        "sizeBytes": u64::MAX,
        "truncated": true,
    }))
    .map(|value| value.len())
    .map_err(Into::into)
}

fn read_line_bounded<R: BufRead>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let available = reader
            .fill_buf()
            .map_err(|_| FileExplorerError::FileUnavailable)?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(line))
            };
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if line.len().saturating_add(take) > MAX_TEXT_LINE_BYTES {
            bail!(FileExplorerError::LineTooLarge);
        }
        line.extend_from_slice(&available[..take]);
        let ended = available[take - 1] == b'\n';
        reader.consume(take);
        if ended {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(line));
        }
    }
}

fn repository_for_workspace(workspace_root: &Path) -> Option<(IsolatedGitSandbox, Option<String>)> {
    let sandbox = IsolatedGitSandbox::discover(workspace_root).ok()?;
    let relative = workspace_root.strip_prefix(sandbox.work_tree()).ok()?;
    let prefix = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    Some((sandbox, (!prefix.is_empty()).then_some(prefix)))
}

fn parse_status(stdout: &[u8], prefix: Option<&str>) -> (Vec<ChangedFile>, bool) {
    let mut fields = stdout
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    let mut files = Vec::new();
    let mut truncated = false;
    while let Some(record) = fields.next() {
        if record.len() < 4 || record[2] != b' ' {
            continue;
        }
        let status_bytes = &record[..2];
        if status_bytes == b"!!" {
            continue;
        }
        let renamed = status_bytes
            .iter()
            .any(|status| matches!(status, b'R' | b'C'));
        let Some(path) = workspace_relative_git_path(&record[3..], prefix) else {
            if renamed {
                let _ = fields.next();
            }
            continue;
        };
        let old_path = if renamed {
            fields
                .next()
                .and_then(|old| workspace_relative_git_path(old, prefix))
        } else {
            None
        };
        let status = if renamed {
            "renamed"
        } else if status_bytes == b"??" || status_bytes.contains(&b'A') {
            "added"
        } else if status_bytes.contains(&b'D') {
            "deleted"
        } else {
            "modified"
        };
        let file = ChangedFile {
            path,
            status,
            old_path: (status == "renamed").then_some(old_path).flatten(),
        };
        if files.len() >= MAX_CHANGED_FILES {
            truncated = true;
            break;
        }
        files.push(file);
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    (files, truncated)
}

fn workspace_relative_git_path(path: &[u8], prefix: Option<&str>) -> Option<String> {
    let path = String::from_utf8(path.to_vec()).ok()?;
    match prefix {
        None => Some(path),
        Some(prefix) => path
            .strip_prefix(prefix)
            .and_then(|path| path.strip_prefix('/'))
            .map(str::to_owned),
    }
}

fn add_repository_prefix(prefix: Option<&str>, path: &str) -> String {
    match prefix {
        Some(prefix) => format!("{prefix}/{path}"),
        None => path.to_owned(),
    }
}

fn parse_gitlink_paths(stdout: &[u8], prefix: Option<&str>) -> Vec<String> {
    let mut paths = Vec::new();
    for record in stdout.split(|byte| *byte == 0).filter(|record| !record.is_empty()) {
        let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
            continue;
        };
        let Some(mode_end) = record[..tab].iter().position(|byte| *byte == b' ') else {
            continue;
        };
        if &record[..mode_end] != b"160000" {
            continue;
        }
        let Some(path) = workspace_relative_git_path(&record[tab + 1..], prefix) else {
            continue;
        };
        if !path.is_empty() {
            paths.push(path);
        }
    }
    paths
}

fn longest_gitlink_prefix<'a>(gitlinks: &'a [String], relative: &str) -> Option<&'a str> {
    gitlinks
        .iter()
        .filter(|link| {
            relative == link.as_str()
                || relative.starts_with(link.as_str())
                    && relative.as_bytes().get(link.len()) == Some(&b'/')
        })
        .max_by_key(|link| link.len())
        .map(String::as_str)
}

fn list_gitlink_paths(sandbox: &IsolatedGitSandbox, prefix: Option<&str>) -> Result<Vec<String>> {
    let mut args = vec!["ls-files", "-s", "-z", "--"];
    args.push(prefix.unwrap_or("."));
    let output = run_git_bounded_timeout(
        sandbox.work_tree(),
        &args,
        MAX_GIT_OUTPUT_BYTES,
        Some(sandbox.environment()),
        GIT_TIMEOUT,
    )
    .map_err(|_| anyhow!("repository changes are unavailable"))?;
    if output.error.is_some() {
        bail!("repository changes are unavailable");
    }
    if output.truncated {
        bail!("repository change list exceeds the size limit");
    }
    Ok(parse_gitlink_paths(&output.stdout, prefix))
}

fn prefix_changed_path(parent: &str, child: &str) -> String {
    if parent.is_empty() {
        child.to_owned()
    } else {
        format!("{parent}/{child}")
    }
}

/// Discover the submodule sandbox for a gitlink, requiring the discovered
/// repository root to BE the gitlink directory: `discover` walks ancestors,
/// so an absent/uninitialized gitlink would otherwise resolve to the
/// superproject and fabricate prefixed phantom rows.
fn submodule_sandbox_for(workspace_root: &Path, link: &str) -> Option<IsolatedGitSandbox> {
    let canonical = fs::canonicalize(workspace_root.join(link)).ok()?;
    let sub = IsolatedGitSandbox::discover(&canonical).ok()?;
    (sub.work_tree() == canonical).then_some(sub)
}

fn expand_gitlink_changes(
    workspace_root: &Path,
    sandbox: &IsolatedGitSandbox,
    prefix: Option<&str>,
    files: Vec<ChangedFile>,
    mut truncated: bool,
    depth: usize,
) -> Result<(Vec<ChangedFile>, bool)> {
    if depth >= MAX_SUBMODULE_DEPTH {
        return Ok((files, truncated));
    }
    let gitlinks = list_gitlink_paths(sandbox, prefix)?;
    if gitlinks.is_empty() {
        return Ok((files, truncated));
    }
    let mut expanded_links = HashSet::new();
    let mut inner_files = Vec::new();
    for link in &gitlinks {
        if expanded_links.len() >= MAX_SUBMODULE_EXPAND {
            truncated = true;
            break;
        }
        let Some(sub) = submodule_sandbox_for(workspace_root, link) else {
            continue;
        };
        let sub_root = workspace_root.join(link);
        let inner = match run_git_bounded_timeout(
            sub.work_tree(),
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--",
                ".",
            ],
            MAX_GIT_OUTPUT_BYTES,
            Some(sub.environment()),
            GIT_TIMEOUT,
        ) {
            Ok(output)
                if output.error.is_none()
                    && !output.truncated
                    && !output.stdout.is_empty() =>
            {
                output
            }
            // One broken submodule degrades to its parent gitlink row; it
            // must never take the whole change list down.
            Ok(_) => {
                truncated = true;
                continue;
            }
            Err(_) => {
                truncated = true;
                continue;
            }
        };
        let (inner, inner_truncated) = parse_status(&inner.stdout, None);
        truncated |= inner_truncated;
        if inner.is_empty() {
            continue;
        }
        let (inner, inner_truncated) =
            match expand_gitlink_changes(&sub_root, &sub, None, inner, inner_truncated, depth + 1)
            {
                Ok(expanded) => expanded,
                Err(_) => {
                    truncated = true;
                    continue;
                }
            };
        truncated |= inner_truncated;
        expanded_links.insert(link.clone());
        for child in inner {
            if inner_files.len() + files.len() >= MAX_CHANGED_FILES {
                truncated = true;
                break;
            }
            inner_files.push(ChangedFile {
                path: prefix_changed_path(link, &child.path),
                status: child.status,
                old_path: child
                    .old_path
                    .map(|old_path| prefix_changed_path(link, &old_path)),
            });
        }
    }
    let mut out: Vec<ChangedFile> = files
        .into_iter()
        .filter(|file| !expanded_links.contains(&file.path))
        .collect();
    out.extend(inner_files);
    out.sort_by(|left, right| left.path.cmp(&right.path));
    if out.len() > MAX_CHANGED_FILES {
        out.truncate(MAX_CHANGED_FILES);
        truncated = true;
    }
    Ok((out, truncated))
}

fn covering_repository(workspace_root: &Path, relative: &str) -> Result<(IsolatedGitSandbox, String)> {
    let (sandbox, prefix) = repository_for_workspace(workspace_root)
        .ok_or_else(|| anyhow!("not a repository"))?;
    covering_from(sandbox, prefix.as_deref(), workspace_root, relative.to_owned(), 0)
}

fn covering_from(
    sandbox: IsolatedGitSandbox,
    prefix: Option<&str>,
    workspace_root: &Path,
    relative: String,
    depth: usize,
) -> Result<(IsolatedGitSandbox, String)> {
    if depth >= MAX_SUBMODULE_DEPTH {
        return Ok((sandbox, add_repository_prefix(prefix, &relative)));
    }
    let gitlinks = match list_gitlink_paths(&sandbox, prefix) {
        Ok(paths) => paths,
        Err(_) => return Ok((sandbox, add_repository_prefix(prefix, &relative))),
    };
    let Some(link) = longest_gitlink_prefix(&gitlinks, &relative) else {
        return Ok((sandbox, add_repository_prefix(prefix, &relative)));
    };
    if link == relative {
        return Ok((sandbox, add_repository_prefix(prefix, &relative)));
    }
    let inner = relative
        .strip_prefix(link)
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or("")
        .to_owned();
    match submodule_sandbox_for(workspace_root, link) {
        Some(sub) => covering_from(sub, None, &workspace_root.join(link), inner, depth + 1),
        // Absent/uninitialized gitlink: the parent row is authoritative.
        None => Ok((sandbox, add_repository_prefix(prefix, &relative))),
    }
}

fn is_untracked_path(sandbox: &IsolatedGitSandbox, path: &str) -> Result<bool> {
    let output = run_git_bounded_timeout(
        sandbox.work_tree(),
        &["ls-files", "--others", "--exclude-standard", "-z", "--", path],
        MAX_PAGE_BYTES,
        Some(sandbox.environment()),
        GIT_TIMEOUT,
    )
    .map_err(|_| anyhow!("file status is unavailable"))?;
    if output.error.is_some() || output.truncated {
        bail!("file status is unavailable");
    }
    Ok(output.stdout.split(|byte| *byte == 0).any(|entry| {
        !entry.is_empty() && entry == path.as_bytes()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_path_is_relative_and_cannot_escape() {
        let root = Path::new("/workspace");
        assert_eq!(
            resolve_scoped_path(root, "src/../Cargo.toml", false)
                .expect("contained path")
                .1,
            "Cargo.toml"
        );
        assert!(resolve_scoped_path(root, "../secret", false).is_err());
        assert!(resolve_scoped_path(root, "/etc/passwd", false).is_err());
    }

    #[test]
    fn absolute_path_inside_workspace_resolves_to_scoped_relative() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join("notes.txt"), "body").expect("notes");
        let absolute = workspace.path().join("notes.txt");
        let input = absolute.to_str().expect("utf-8 workspace path");

        let (resolved, relative) =
            resolve_scoped_path(workspace.path(), input, false).expect("contained absolute");
        assert_eq!(relative, "notes.txt");
        assert_eq!(resolved, absolute);

        // The scoped relative representation reaches the file through the
        // same no-follow capability-relative open used for every read.
        let mut file = open_relative_file(workspace.path(), Path::new(&relative))
            .expect("no-follow read");
        let mut contents = String::new();
        file.read_to_string(&mut contents).expect("read file");
        assert_eq!(contents, "body");
    }

    #[test]
    fn absolute_outside_root_root_only_and_traversal_stay_rejected() {
        let workspace = tempfile::tempdir().expect("workspace");
        let root = workspace.path().to_str().expect("utf-8 workspace path");

        assert!(resolve_scoped_path(workspace.path(), "/etc/passwd", false).is_err());
        // A similarly named sibling directory is not containment.
        let sibling = format!("{root}-sibling/file.txt");
        assert!(resolve_scoped_path(workspace.path(), &sibling, false).is_err());
        // Root-only absolute input is an empty scoped path: rejected where a
        // path is required, accepted only where the root itself is listable.
        assert!(resolve_scoped_path(workspace.path(), root, false).is_err());
        let (resolved, relative) =
            resolve_scoped_path(workspace.path(), root, true).expect("listable root");
        assert_eq!(relative, "");
        assert_eq!(resolved, workspace.path());
        // Traversal that climbs above the root is rejected.
        let escaped = format!("{root}/../secret");
        assert!(resolve_scoped_path(workspace.path(), &escaped, false).is_err());
    }

    /// A basename-only citation (`gatherer.rs:178-260`) answers with the ONE
    /// eligible workspace file of that exact name ONLY when the bounded walk
    /// examined the whole eligible tree. The DEEP chain below stops the walk
    /// at the depth ceiling, so the lone match is no longer a verdict: an
    /// unexamined subtree may still hide a homonym, and the request stays
    /// fail-closed.
    #[test]
    fn unique_basename_file_refuses_a_lone_match_when_the_walk_does_not_complete() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::create_dir_all(workspace.path().join("src/queue")).expect("queue dir");
        std::fs::write(workspace.path().join("src/queue/gatherer.rs"), "fn handle() {}\n")
            .expect("cited file");

        let leaf = Path::new("gatherer.rs");
        assert_eq!(
            unique_basename_file(workspace.path(), leaf).as_deref(),
            Some("src/queue/gatherer.rs"),
            "no bound met: the complete walk proves the match unique"
        );

        let mut deep = workspace.path().to_path_buf();
        for _ in 0..(BASENAME_SEARCH_MAX_DEPTH + 8) {
            deep.push("d");
        }
        std::fs::create_dir_all(&deep).expect("deep chain");
        assert_eq!(
            unique_basename_file(workspace.path(), leaf),
            None,
            "the depth ceiling leaves a subtree unexamined: incomplete, no verdict"
        );
    }

    /// In-bounds trees walk to the end, so exactly one match IS the proof: the
    /// shallow eligible match resolves, while a second eligible homonym, a
    /// directory of that name, and an absent name all stay `None`.
    #[test]
    fn unique_basename_file_answers_an_in_bounds_lone_match() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::create_dir_all(workspace.path().join("src/queue")).expect("queue dir");
        std::fs::write(workspace.path().join("src/queue/gatherer.rs"), "fn handle() {}\n")
            .expect("cited file");

        let leaf = Path::new("gatherer.rs");
        assert_eq!(
            unique_basename_file(workspace.path(), leaf).as_deref(),
            Some("src/queue/gatherer.rs")
        );
        assert_eq!(
            unique_basename_file(workspace.path(), Path::new("queue")),
            None,
            "a directory of that name is not a match"
        );
        assert_eq!(
            unique_basename_file(workspace.path(), Path::new("absent.rs")),
            None
        );

        std::fs::create_dir_all(workspace.path().join("tests")).expect("tests dir");
        std::fs::write(workspace.path().join("tests/gatherer.rs"), "fn other() {}\n")
            .expect("homonym");
        assert_eq!(unique_basename_file(workspace.path(), leaf), None);
    }

    /// The counterexample the completeness rule exists for: a SECOND
    /// `gatherer.rs` below a depth-exceeding chain is never enumerated, so a
    /// lone shallow match must NOT be reported as unique while that homonym
    /// sits past the ceiling.
    #[test]
    fn unique_basename_file_refuses_a_homonym_below_the_depth_ceiling() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::create_dir_all(workspace.path().join("src/queue")).expect("queue dir");
        std::fs::write(workspace.path().join("src/queue/gatherer.rs"), "fn handle() {}\n")
            .expect("cited file");
        let mut deep = workspace.path().to_path_buf();
        for _ in 0..(BASENAME_SEARCH_MAX_DEPTH + 8) {
            deep.push("d");
        }
        std::fs::create_dir_all(&deep).expect("deep chain");
        std::fs::write(deep.join("gatherer.rs"), "fn hidden() {}\n").expect("hidden homonym");

        assert_eq!(
            unique_basename_file(workspace.path(), Path::new("gatherer.rs")),
            None,
            "a homonym below the depth ceiling is never seen, so nothing is unique"
        );
    }

    /// The heavy subtrees skipped by the walk are excluded subtrees, never
    /// homonyms: their copies of the name cannot veto the one eligible match.
    #[test]
    fn unique_basename_file_skips_heavy_subtrees() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::create_dir_all(workspace.path().join("src")).expect("src dir");
        std::fs::write(workspace.path().join("src/gatherer.rs"), "fn handle() {}\n")
            .expect("cited file");
        for skipped in ["target/debug", ".git/objects", "node_modules/pkg"] {
            let directory = workspace.path().join(skipped);
            std::fs::create_dir_all(&directory).expect("skipped dir");
            std::fs::write(directory.join("gatherer.rs"), "fn built() {}\n").expect("built copy");
        }

        assert_eq!(
            unique_basename_file(workspace.path(), Path::new("gatherer.rs")).as_deref(),
            Some("src/gatherer.rs")
        );
    }

    /// The fallback read a transcript citation actually issues: a root-relative
    /// miss on a lone basename reads the real file and echoes THAT
    /// workspace-relative path (the identity the Web probe validates against)
    /// while the whole eligible tree is walked — the deep-but-reachable chain
    /// below stays inside the bounds — and a homonym hidden below the depth
    /// ceiling keeps the stable missing-file error.
    #[tokio::test]
    async fn read_resolves_a_unique_basename_only_when_the_walk_completes() {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::create_dir_all(cwd.path().join("src/queue")).expect("queue dir");
        std::fs::write(
            cwd.path().join("src/queue/gatherer.rs"),
            "line one\nline two\n",
        )
        .expect("cited file");
        let mut reachable = cwd.path().to_path_buf();
        for _ in 0..(BASENAME_SEARCH_MAX_DEPTH / 2) {
            reachable.push("r");
        }
        std::fs::create_dir_all(&reachable).expect("reachable chain");
        std::fs::write(reachable.join("other.rs"), "fn other() {}\n").expect("deep other file");
        let app = completion_application(cwd.path(), &[]).await;

        let response = read(&app, "gatherer.rs", 0, Some(1)).expect("basename read");
        assert_eq!(response.path, "src/queue/gatherer.rs");
        assert_eq!(response.lines, ["line one"]);
        assert_eq!(response.total_lines, 2);
        assert_eq!(response.next_cursor, Some(1));
        assert_eq!(response.size_bytes, "line one\nline two\n".len() as u64);

        let mut deep = cwd.path().to_path_buf();
        for _ in 0..(BASENAME_SEARCH_MAX_DEPTH + 8) {
            deep.push("d");
        }
        std::fs::create_dir_all(&deep).expect("deep chain");
        std::fs::write(deep.join("gatherer.rs"), "fn hidden() {}\n").expect("hidden homonym");
        let error = read(&app, "gatherer.rs", 0, Some(1)).expect_err("homonym below the ceiling");
        assert_eq!(error.to_string(), "file is unavailable");
    }

    #[cfg(unix)]
    #[test]
    fn absolute_symlink_escape_stays_rejected() {
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret.txt"), "secret").expect("secret");
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            workspace.path().join("escape.txt"),
        )
        .expect("symlink");
        let absolute = workspace.path().join("escape.txt");

        // Lexical containment holds for the request...
        let (_, relative) = resolve_scoped_path(
            workspace.path(),
            absolute.to_str().expect("utf-8 workspace path"),
            false,
        )
        .expect("lexically contained");
        assert_eq!(relative, "escape.txt");

        // ...but the no-follow capability-relative open retains authority.
        let error = open_relative_file(workspace.path(), Path::new(&relative))
            .expect_err("symlink escape rejected");
        assert_eq!(error.to_string(), "file is a symlink");
        assert_eq!(
            error.downcast_ref::<FileExplorerError>(),
            Some(&FileExplorerError::FileSymlink)
        );
    }

    #[cfg(unix)]
    #[test]
    fn not_found_and_transient_io_map_to_distinct_bounded_messages() {
        // Only a MISSING entry reads as 'file is unavailable'.

        // The cap_std open path classifies the same way: a missing leaf is
        // 'unavailable'; a permission-denied directory open is transient.
        let workspace = tempfile::tempdir().expect("workspace");
        let missing = open_relative_file(workspace.path(), Path::new("absent.txt"))
            .expect_err("missing leaf rejected");
        assert_eq!(missing.to_string(), "file is unavailable");
        assert_eq!(
            missing.downcast_ref::<FileExplorerError>(),
            Some(&FileExplorerError::FileUnavailable)
        );

        let locked_dir = tempfile::tempdir().expect("workspace");
        std::fs::create_dir_all(locked_dir.path().join("dir")).expect("subdir");
        let mut permissions = std::fs::metadata(locked_dir.path().join("dir"))
            .expect("subdir metadata")
            .permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o000);
        std::fs::set_permissions(locked_dir.path().join("dir"), permissions).expect("lock dir");
        let denied = open_relative_file(locked_dir.path(), Path::new("dir/leaf.txt"))
            .expect_err("denied traversal rejected");
        assert_eq!(
            denied.to_string(),
            "file could not be read right now",
            "a permission failure must NOT read as a missing file"
        );
        assert!(
            denied.downcast_ref::<FileExplorerError>().is_none(),
            "transient IO must stay an untyped generic error"
        );
    }

    #[test]
    fn file_explorer_error_downcast_and_codes_are_independent_of_prose() {
        let typed: anyhow::Error = FileExplorerError::FileUnavailable.into();
        assert_eq!(typed.to_string(), "file is unavailable");
        assert_eq!(
            typed.downcast_ref::<FileExplorerError>(),
            Some(&FileExplorerError::FileUnavailable)
        );
        assert_eq!(
            serde_json::to_value(FileExplorerError::FileUnavailable).expect("code"),
            json!("file_unavailable")
        );

        let homonym = anyhow!("file is unavailable");
        assert!(
            homonym.downcast_ref::<FileExplorerError>().is_none(),
            "matching prose must not impersonate the typed error"
        );

        let too_large: anyhow::Error = FileExplorerError::LineTooLarge.into();
        assert_eq!(too_large.to_string(), "line exceeds response limit");
        assert_eq!(
            too_large.downcast_ref::<FileExplorerError>(),
            Some(&FileExplorerError::LineTooLarge)
        );
        assert_eq!(
            serde_json::to_value(FileExplorerError::LineTooLarge).expect("code"),
            json!("file_line_too_large")
        );

        let symlink: anyhow::Error = FileExplorerError::FileSymlink.into();
        assert_eq!(symlink.to_string(), "file is a symlink");
        assert_eq!(
            symlink.downcast_ref::<FileExplorerError>(),
            Some(&FileExplorerError::FileSymlink)
        );
        assert_eq!(
            serde_json::to_value(FileExplorerError::FileSymlink).expect("code"),
            json!("file_symlink")
        );

        let missing_dir: anyhow::Error = FileExplorerError::DirectoryUnavailable.into();
        assert_eq!(missing_dir.to_string(), "directory is unavailable");
        assert_eq!(
            missing_dir.downcast_ref::<FileExplorerError>(),
            Some(&FileExplorerError::DirectoryUnavailable)
        );
        assert_eq!(
            serde_json::to_value(FileExplorerError::DirectoryUnavailable).expect("code"),
            json!("directory_unavailable")
        );
        let homonym_dir = anyhow!("directory is unavailable");
        assert!(
            homonym_dir.downcast_ref::<FileExplorerError>().is_none(),
            "matching directory prose must not impersonate the typed error"
        );
    }
    #[test]
    fn contained_absolute_input_returns_relative_response_path() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join("notes.txt"), "body").expect("notes");
        let notes = workspace.path().join("notes.txt");
        let (_, relative) = resolve_scoped_path(
            workspace.path(),
            notes.to_str().expect("utf-8 notes path"),
            false,
        )
        .expect("contained");
        let path = response_path(&relative);
        assert_eq!(path, "notes.txt");
        assert!(!Path::new(&path).is_absolute());
        assert_eq!(response_path("src/notes.txt"), "src/notes.txt");

        let payload = serde_json::to_string(&ReadResponse {
            path,
            lines: vec!["body".into()],
            next_cursor: None,
            total_lines: 1,
            size_bytes: 4,
            truncated: false,
        })
        .expect("serialize");
        assert!(payload.contains("\"path\":\"notes.txt\""));
        assert!(payload.contains("\"totalLines\":1"), "{payload}");
        let root = workspace.path().to_str().expect("utf-8 workspace path");
        assert!(!payload.contains(root), "{payload}");

        let diff_payload = serde_json::to_string(&DiffResponse {
            path: response_path(&relative),
            lines: vec!["@@".into()],
            next_cursor: None,
            total_lines: 1,
            truncated: false,
        })
        .expect("serialize diff");
        assert!(diff_payload.contains("\"path\":\"notes.txt\""));
        assert!(diff_payload.contains("\"totalLines\":1"), "{diff_payload}");
        assert!(!diff_payload.contains(root), "{diff_payload}");
    }

    #[test]
    fn status_parser_maps_git_states_and_rename_provenance() {
        let (parsed, truncated) = parse_status(
            b" M src/lib.rs\0?? new.txt\0D  gone.txt\0R  renamed.txt\0old.txt\0",
            None,
        );
        assert!(!truncated);
        assert_eq!(
            parsed,
            vec![
                ChangedFile {
                    path: "gone.txt".into(),
                    status: "deleted",
                    old_path: None,
                },
                ChangedFile {
                    path: "new.txt".into(),
                    status: "added",
                    old_path: None,
                },
                ChangedFile {
                    path: "renamed.txt".into(),
                    status: "renamed",
                    old_path: Some("old.txt".into()),
                },
                ChangedFile {
                    path: "src/lib.rs".into(),
                    status: "modified",
                    old_path: None,
                },
            ]
        );
    }

    #[test]
    fn status_parser_bounds_over_cap_and_reports_truncated() {
        let mut stdout = Vec::new();
        for index in 0..=MAX_CHANGED_FILES {
            stdout.extend(format!(" M f{index:04}.txt\0").into_bytes());
        }
        let (parsed, truncated) = parse_status(&stdout, None);
        assert!(truncated);
        assert_eq!(parsed.len(), MAX_CHANGED_FILES);
        assert_eq!(parsed[0].path, "f0000.txt");
        assert_eq!(
            parsed[MAX_CHANGED_FILES - 1].path,
            format!("f{:04}.txt", MAX_CHANGED_FILES - 1)
        );
        assert!(parsed.iter().all(|file| file.status == "modified"));
    }

    #[test]
    fn status_parser_exact_cap_is_not_truncated() {
        let mut stdout = Vec::new();
        for index in 0..MAX_CHANGED_FILES {
            stdout.extend(format!(" M f{index:04}.txt\0").into_bytes());
        }
        let (parsed, truncated) = parse_status(&stdout, None);
        assert!(!truncated);
        assert_eq!(parsed.len(), MAX_CHANGED_FILES);
    }

    #[test]
    fn status_parser_keeps_rename_pair_on_bounded_rows() {
        let mut stdout = Vec::new();
        for index in 0..(MAX_CHANGED_FILES - 1) {
            stdout.extend(format!(" M f{index:04}.txt\0").into_bytes());
        }
        stdout.extend(b"R  renamed.txt\0old.txt\0");
        stdout.extend(b" M extra.txt\0");
        let (parsed, truncated) = parse_status(&stdout, None);
        assert!(truncated);
        assert_eq!(parsed.len(), MAX_CHANGED_FILES);
        let renamed = parsed
            .iter()
            .find(|file| file.path == "renamed.txt")
            .expect("rename retained");
        assert_eq!(renamed.status, "renamed");
        assert_eq!(renamed.old_path.as_deref(), Some("old.txt"));
        assert!(parsed.iter().all(|file| file.path != "extra.txt"));
    }

    #[test]
    fn gitlink_parser_keeps_only_mode_160000_paths() {
        let stdout = b"100644 abcdef 0\tREADME.md\x00160000 fedcba 0\text\x00100644 111 0\text/ignored.txt\x00";
        assert_eq!(parse_gitlink_paths(stdout, None), vec!["ext".to_string()]);
        assert_eq!(
            parse_gitlink_paths(stdout, Some("pkg")),
            Vec::<String>::new()
        );
        assert_eq!(
            parse_gitlink_paths(b"160000 abc 0\tpkg/ext\x00", Some("pkg")),
            vec!["ext".to_string()]
        );
        assert_eq!(
            longest_gitlink_prefix(
                &["ext".into(), "ext/nested".into()],
                "ext/nested/lib.txt"
            ),
            Some("ext/nested")
        );
        assert_eq!(
            longest_gitlink_prefix(&["ext".into()], "external/lib.txt"),
            None
        );
    }

    #[test]
    fn paging_cursor_points_to_first_unreturned_line() {
        let page = page_from_lines(
            ["a", "b", "c"].into_iter().map(str::to_owned),
            "file.txt",
            0,
            Some(2),
            MAX_PAGE_BYTES,
        )
        .expect("first page");
        assert_eq!(page.lines, ["a", "b"]);
        assert_eq!(page.next_cursor, Some(2));
        assert_eq!(page.total_lines, 3);
        let final_page = page_from_lines(
            ["a", "b", "c"].into_iter().map(str::to_owned),
            "file.txt",
            page.next_cursor.expect("cursor"),
            Some(2),
            MAX_PAGE_BYTES,
        )
        .expect("final page");
        assert_eq!(final_page.lines, ["c"]);
        assert_eq!(final_page.total_lines, 3);
        assert_eq!(final_page.next_cursor, None);
    }
    #[test]
    fn paging_clamps_lines_and_keeps_serialized_response_bounded() {
        let text = (0..2_100)
            .map(|index| format!("{index:04}-{}", "x".repeat(120)))
            .collect::<Vec<_>>()
            .join("\n");
        let page = text_page(
            BufReader::new(text.as_bytes()),
            "large.txt",
            0,
            Some(usize::MAX),
            MAX_PAGE_CONTENT_BYTES,
        )
        .expect("bounded page");
        assert!(page.lines.len() <= MAX_PAGE_LINES);
        assert_eq!(page.total_lines, 2_100);
        assert!(page.next_cursor.is_some());
        let response = ReadResponse {
            path: "large.txt".into(),
            lines: page.lines,
            next_cursor: page.next_cursor,
            total_lines: page.total_lines,
            size_bytes: text.len() as u64,
            truncated: true,
        };
        assert!(
            serde_json::to_vec(&response).expect("serialize response").len() <= MAX_PAGE_BYTES
        );
    }

    #[test]
    fn text_page_paginates_491_long_lines_with_exact_total() {
        // Mirrors the reported AGENTS.md case: 491 logical lines, most long
        // enough to wrap badly in a naive renderer.
        let text = (0..491)
            .map(|index| format!("{index:03}-{}", "x".repeat(200)))
            .collect::<Vec<_>>()
            .join("\n");
        let first = text_page(
            BufReader::new(text.as_bytes()),
            "agents.md",
            0,
            None,
            MAX_PAGE_CONTENT_BYTES,
        )
        .expect("first page");
        assert_eq!(first.lines.len(), 400);
        assert_eq!(first.next_cursor, Some(400));
        assert_eq!(first.total_lines, 491);
        let second = text_page(
            BufReader::new(text.as_bytes()),
            "agents.md",
            first.next_cursor.expect("cursor"),
            None,
            MAX_PAGE_CONTENT_BYTES,
        )
        .expect("second page");
        assert_eq!(second.lines.len(), 91);
        assert_eq!(second.next_cursor, None);
        assert_eq!(second.total_lines, 491);
    }

    #[test]
    fn text_page_full_default_page_has_no_next_cursor() {
        // nextCursor exists iff more logical lines follow, even at exactly
        // one full page.
        let text = (0..DEFAULT_PAGE_LINES)
            .map(|index| format!("{index:04}-{}", "y".repeat(120)))
            .collect::<Vec<_>>()
            .join("\n");
        let page = text_page(
            BufReader::new(text.as_bytes()),
            "exact.txt",
            0,
            None,
            MAX_PAGE_CONTENT_BYTES,
        )
        .expect("exact page");
        assert_eq!(page.lines.len(), DEFAULT_PAGE_LINES);
        assert_eq!(page.next_cursor, None);
        assert_eq!(page.total_lines, DEFAULT_PAGE_LINES);
    }

    #[test]
    fn text_page_byte_budget_paginates_with_exact_total() {
        // One-kilobyte lines exhaust the byte budget before the line cap: the
        // pager must still emit nextCursor and a truthful total across pages.
        let text = (0..491)
            .map(|index| format!("{index:03}-{}", "y".repeat(1200)))
            .collect::<Vec<_>>()
            .join("\n");
        let mut cursor = 0usize;
        let mut loaded = 0usize;
        let mut pages = 0usize;
        loop {
            let page = text_page(
                BufReader::new(text.as_bytes()),
                "big.txt",
                cursor,
                None,
                MAX_PAGE_CONTENT_BYTES,
            )
            .expect("page");
            assert_eq!(page.total_lines, 491);
            assert!(page.lines.len() < DEFAULT_PAGE_LINES);
            loaded += page.lines.len();
            pages += 1;
            match page.next_cursor {
                Some(next) => cursor = next,
                None => break,
            }
            assert!(pages < 10, "pagination loop must terminate");
        }
        assert!(pages >= 2);
        assert_eq!(loaded, 491);
    }

    #[test]
    fn text_page_line_exceeding_limit_is_rejected() {
        let text = format!("{}\nshort\n", "z".repeat(MAX_TEXT_LINE_BYTES + 1));
        let error = text_page(
            BufReader::new(text.as_bytes()),
            "long.txt",
            0,
            None,
            MAX_PAGE_CONTENT_BYTES,
        )
        .expect_err("oversized line");
        assert_eq!(error.to_string(), "line exceeds response limit");
        assert_eq!(
            error.downcast_ref::<FileExplorerError>(),
            Some(&FileExplorerError::LineTooLarge)
        );
    }

    #[test]
    fn page_from_lines_rejects_line_exceeding_limit() {
        let error = page_from_lines(
            [format!("{}", "z".repeat(MAX_PAGE_BYTES + 1))].into_iter(),
            "d.txt",
            0,
            None,
            MAX_PAGE_CONTENT_BYTES,
        )
        .expect_err("oversized diff line");
        assert_eq!(error.to_string(), "line exceeds response limit");
        assert_eq!(
            error.downcast_ref::<FileExplorerError>(),
            Some(&FileExplorerError::LineTooLarge)
        );
    }

    #[test]
    fn text_page_counts_final_line_without_trailing_newline() {
        let page = text_page(
            BufReader::new("a\nb\nc".as_bytes()),
            "tail.txt",
            0,
            Some(2),
            MAX_PAGE_CONTENT_BYTES,
        )
        .expect("page");
        assert_eq!(page.lines, ["a", "b"]);
        assert_eq!(page.next_cursor, Some(2));
        assert_eq!(page.total_lines, 3);
    }

    #[test]
    fn text_page_count_scan_rejects_binary_tail() {
        // NUL beyond the rendered window is still fatal: the total scan keeps
        // the same text-file contract as the rendered window.
        let tail = "ok\n".repeat(20);
        let text = format!("{tail}bad\u{0}content\n");
        let error = text_page(
            BufReader::new(text.as_bytes()),
            "mixed.bin",
            0,
            Some(2),
            MAX_PAGE_CONTENT_BYTES,
        )
        .expect_err("binary tail");
        assert_eq!(error.to_string(), "not a text file");
    }

    #[test]
    fn page_from_lines_reports_full_diff_total() {
        let lines = (0..600)
            .map(|index| format!("{index:04} diff line"))
            .collect::<Vec<_>>();
        let first = page_from_lines(
            lines.iter().cloned(),
            "f.txt",
            0,
            None,
            MAX_PAGE_CONTENT_BYTES,
        )
        .expect("first diff page");
        assert_eq!(first.lines.len(), 400);
        assert_eq!(first.next_cursor, Some(400));
        assert_eq!(first.total_lines, 600);
        let second = page_from_lines(
            lines.into_iter(),
            "f.txt",
            first.next_cursor.expect("cursor"),
            None,
            MAX_PAGE_CONTENT_BYTES,
        )
        .expect("second diff page");
        assert_eq!(second.lines.len(), 200);
        assert_eq!(second.next_cursor, None);
        assert_eq!(second.total_lines, 600);
    }
    // ------------------------------------------------------------------
    // path_complete
    //
    // The fixture mirrors the rpc.rs RPC fixtures: an isolated Application
    // with a disabled hermetic session and a danger-full-access sandbox so
    // the captured PolicyFs lists like production.
    // ------------------------------------------------------------------
    use pi_agent::ThinkingLevel;
    use pi_coding::{ResourceDiscovery, SessionOptions, ToolSelection, WorkspaceRoots};
    use std::process::Command;

    fn git(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args([
                "-c",
                "color.ui=false",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
                "-c",
                "core.hooksPath=.git-test-no-hooks",
            ])
            .args(args)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", cwd.join("absent-global-git-config"))
            .env("LC_ALL", "C")
            .env("GIT_AUTHOR_NAME", "Pi Test")
            .env("GIT_AUTHOR_EMAIL", "pi@example.test")
            .env("GIT_COMMITTER_NAME", "Pi Test")
            .env("GIT_COMMITTER_EMAIL", "pi@example.test")
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    async fn blame_interior_range_keeps_commit_and_dirty_null() {
        let cwd = tempfile::tempdir().expect("blame cwd");
        git(cwd.path(), &["init", "-q"]);
        let source = (1..=10)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(cwd.path().join("src.txt"), &source).expect("source");
        git(cwd.path(), &["add", "src.txt"]);
        git(cwd.path(), &["commit", "-q", "-m", "baseline"]);
        let head = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(cwd.path())
            .output()
            .expect("rev-parse");
        assert!(head.status.success(), "rev-parse failed");
        let oid = String::from_utf8(head.stdout)
            .expect("oid utf-8")
            .trim()
            .to_owned();
        assert_eq!(oid.len(), 40);
        let author = Command::new("git")
            .args(["log", "-1", "--format=%an"])
            .current_dir(cwd.path())
            .output()
            .expect("author");
        assert!(author.status.success(), "author lookup failed");
        let author = String::from_utf8(author.stdout)
            .expect("author utf-8")
            .trim()
            .to_owned();
        assert!(!author.is_empty());
        let mut dirty = source;
        dirty = dirty.replacen("line 6\n", "line six\n", 1);
        std::fs::write(cwd.path().join("src.txt"), &dirty).expect("dirty source");
        let app = completion_application(cwd.path(), &[]).await;

        let response = blame(&app, "src.txt", 4, 7).expect("interior blame");
        assert_eq!(response.path, "src.txt");
        assert_eq!(
            response
                .lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["line 4", "line 5", "line six", "line 7"]
        );
        assert_eq!(response.lines[0].author, author);
        assert_eq!(response.lines[1].author, author);
        assert_eq!(response.lines[3].author, author);
        assert!(!response.lines[2].author.is_empty());
        assert_ne!(response.lines[2].author, author);
        assert_eq!(response.lines[0].commit.as_deref(), Some(oid.as_str()));
        assert_eq!(response.lines[0].current_line, 4);
        assert_eq!(response.lines[1].commit.as_deref(), Some(oid.as_str()));
        assert_eq!(response.lines[2].commit, None);
        assert_eq!(response.lines[2].current_line, 6);
        assert_eq!(response.lines[3].commit.as_deref(), Some(oid.as_str()));
        let outside = blame(&app, "src.txt", 8, 11).expect_err("range past eof");
        assert_eq!(outside.to_string(), "line range is outside the file");
    }

    async fn completion_application(cwd: &Path, additional_roots: &[PathBuf]) -> Application {
        completion_application_with_settings(
            cwd,
            additional_roots,
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )
        .await
        .expect("completion application")
    }

    /// Variant of [`completion_application`] under caller-supplied sandbox
    /// settings (workspace-write, denied paths, ...). Confined modes
    /// authenticate the trusted launcher during bootstrap, so an environment
    /// without one fails with a `launcher`-named error; callers that must
    /// tolerate that gate match the error message, mirroring the confined
    /// fixture convention in `pi-coding`'s image tool tests.
    async fn completion_application_with_settings(
        cwd: &Path,
        additional_roots: &[PathBuf],
        settings_json: &str,
    ) -> anyhow::Result<Application> {
        pi_ai::providers::register_default_faux();
        let model = pi_ai::get_model("faux", "faux-1").expect("registered faux model");
        let workspace =
            WorkspaceRoots::new(cwd, additional_roots.iter().map(PathBuf::as_path)).expect("roots");
        let session = pi_coding::Session::new_with_additional_tools_filtered_discovery_workspace_and_uri(
            SessionOptions {
                model,
                cwd: cwd.to_path_buf(),
                system_prompt: String::new(),
                thinking_level: ThinkingLevel::Off,
                api_key: "faux".into(),
                compaction: None,
                stream_options: Default::default(),
                tools: Some(Vec::new()),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn: None,
                auth_resolver: None,
                auth_manager: None,
            },
            Vec::new(),
            ToolSelection::default(),
            ResourceDiscovery::Disabled,
            workspace,
            None,
        )
        .expect("completion session");
        let agent = tempfile::tempdir().expect("agent dir");
        std::fs::write(agent.path().join("settings.json"), settings_json)
            .expect("isolated sandbox settings");
        let session_dir = cwd.join(".pi-test-sessions");
        std::fs::create_dir_all(&session_dir).expect("session dir");
        session.set_session_dir(session_dir);
        let mut options = pi_coding::ResourceManagerOptions::new(cwd);
        options.agent_dir = agent.path().to_path_buf();
        options.project_trust_override = Some(true);
        options.disable_extensions = true;
        options.disable_skills = true;
        options.disable_prompt_templates = true;
        options.disable_themes = true;
        options.disable_context_files = true;
        let resources = pi_coding::ResourceManager::new(options).expect("resources");
        session.attach_resources(resources).await.expect("attach resources");
        Ok(Application::from_runtime_candidate(
            pi_coding::ApplicationRuntimeCandidate::new(session),
        )
        .await?)
    }

    fn names(entries: &[CompletionEntry]) -> Vec<&str> {
        entries.iter().map(|entry| entry.name.as_str()).collect()
    }

    #[tokio::test]
    async fn complete_path_lists_relative_parent_dirs_first_sorted_and_wire_typed() {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::create_dir(cwd.path().join("beta")).expect("beta dir");
        std::fs::create_dir(cwd.path().join("delta")).expect("delta dir");
        std::fs::write(cwd.path().join("gamma.txt"), "g").expect("gamma file");
        std::fs::write(cwd.path().join("alpha.txt"), "a").expect("alpha file");
        let app = completion_application(cwd.path(), &[]).await;

        let response = complete_path(&app, "", CompletionKind::Any, None).expect("completion");
        assert_eq!(response.base, "");
        assert_eq!(response.prefix, "");
        // The helper's hidden `.pi-test-sessions` directory never appears
        // with a non-dot prefix, and directories sort before files.
        assert_eq!(names(&response.entries), ["beta", "delta", "alpha.txt", "gamma.txt"]);
        assert_eq!(
            response
                .entries
                .iter()
                .map(|entry| entry.kind)
                .collect::<Vec<_>>(),
            ["directory", "directory", "file", "file"]
        );
        assert!(!response.truncated);

        let wire = serde_json::to_value(&response).expect("wire");
        assert_eq!(wire["base"], "");
        assert_eq!(wire["prefix"], "");
        assert_eq!(wire["truncated"], false);
        assert_eq!(wire["entries"][0]["name"], "beta");
        assert_eq!(wire["entries"][0]["path"], "beta");
        assert_eq!(wire["entries"][0]["kind"], "directory");
        assert_eq!(wire["entries"][3]["kind"], "file");

        // A nested relative parent keeps the relative base domain and joins
        // entry paths as `base/name`.
        std::fs::create_dir(cwd.path().join("beta/inner")).expect("inner dir");
        std::fs::write(cwd.path().join("beta/inner/deep.txt"), "d").expect("deep file");
        let nested = complete_path(&app, "beta/inner/", CompletionKind::Any, None).expect("nested");
        assert_eq!(nested.base, "beta/inner");
        assert_eq!(names(&nested.entries), ["deep.txt"]);
        assert_eq!(nested.entries[0].path, "beta/inner/deep.txt");
    }

    #[tokio::test]
    async fn complete_path_filters_prefix_and_hides_entries_without_dot_prefix() {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::write(cwd.path().join(".env"), "s").expect("hidden file");
        std::fs::create_dir(cwd.path().join(".exact")).expect("hidden dir");
        std::fs::write(cwd.path().join("visible.txt"), "v").expect("visible file");
        let app = completion_application(cwd.path(), &[]).await;

        let plain = complete_path(&app, "", CompletionKind::Any, None).expect("plain");
        assert_eq!(names(&plain.entries), ["visible.txt"]);
        let dot = complete_path(&app, ".", CompletionKind::Any, None).expect("dot");
        // The fixture also creates hidden support dirs (.pi-test-sessions,
        // .rpi); a dot prefix matches ONLY names starting with a dot.
        let dot_names = names(&dot.entries);
        assert!(dot_names.contains(&".env"));
        assert!(dot_names.contains(&".exact"));
        assert!(dot_names.iter().all(|name| name.starts_with('.')));
        assert_eq!(dot.prefix, ".");
        let hidden_prefix = complete_path(&app, ".e", CompletionKind::Any, None).expect("hidden");
        // Directories sort before files, so the directory `.exact` precedes
        // the file `.env` even though ".env" sorts first lexicographically.
        assert_eq!(names(&hidden_prefix.entries), [".exact", ".env"]);
        assert_eq!(hidden_prefix.prefix, ".e");
        let env_exact = complete_path(&app, ".env", CompletionKind::Any, None).expect("env exact");
        assert_eq!(names(&env_exact.entries), [".env"]);
        assert_eq!(env_exact.prefix, ".env");
        let miss = complete_path(&app, "zz", CompletionKind::Any, None).expect("miss");
        assert!(miss.entries.is_empty());
        assert!(!miss.truncated);
    }

    #[tokio::test]
    async fn complete_path_kind_filters_directories_and_files() {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::create_dir(cwd.path().join("dir_one")).expect("dir one");
        std::fs::create_dir(cwd.path().join("dir_two")).expect("dir two");
        std::fs::write(cwd.path().join("file_one.txt"), "1").expect("file one");
        std::fs::write(cwd.path().join("file_two.txt"), "2").expect("file two");
        let app = completion_application(cwd.path(), &[]).await;

        let dirs = complete_path(&app, "", CompletionKind::Directory, None).expect("dirs");
        assert_eq!(names(&dirs.entries), ["dir_one", "dir_two"]);
        assert!(dirs.entries.iter().all(|entry| entry.kind == "directory"));
        let files = complete_path(&app, "", CompletionKind::File, None).expect("files");
        assert_eq!(names(&files.entries), ["file_one.txt", "file_two.txt"]);
        assert!(files.entries.iter().all(|entry| entry.kind == "file"));
        // A directory request with a file-only prefix yields nothing.
        let none = complete_path(&app, "file_one", CompletionKind::Directory, None)
            .expect("dir-only miss");
        assert!(none.entries.is_empty());
    }

    #[tokio::test]
    async fn complete_path_accepts_additional_root_absolute_and_rejects_outside() {
        let cwd = tempfile::tempdir().expect("cwd");
        let additional = tempfile::tempdir().expect("additional");
        std::fs::write(additional.path().join("shared.txt"), "s").expect("shared file");
        std::fs::create_dir(additional.path().join("shared_dir")).expect("shared dir");
        std::fs::write(cwd.path().join("local.txt"), "l").expect("local file");
        let app = completion_application(cwd.path(), &[additional.path().to_path_buf()]).await;

        let root_input = additional.path().to_str().expect("utf-8 additional root");
        let listed = complete_path(
            &app,
            &format!("{root_input}/"),
            CompletionKind::Any,
            None,
        )
        .expect("additional root listing");
        assert_eq!(listed.base, root_input);
        assert_eq!(names(&listed.entries), ["shared_dir", "shared.txt"]);
        assert_eq!(listed.entries[1].path, format!("{root_input}/shared.txt"));
        assert!(listed.entries.iter().all(|entry| !entry.path.contains("//")));

        let prefixed = complete_path(
            &app,
            &format!("{root_input}/sh"),
            CompletionKind::Any,
            None,
        )
        .expect("additional root prefix");
        assert_eq!(prefixed.prefix, "sh");
        assert_eq!(names(&prefixed.entries), ["shared_dir", "shared.txt"]);

        // The cwd root still completes absolutely.
        let cwd_input = cwd.path().to_str().expect("utf-8 cwd");
        let local = complete_path(&app, &format!("{cwd_input}/loc"), CompletionKind::Any, None)
            .expect("cwd absolute");
        assert_eq!(names(&local.entries), ["local.txt"]);
        assert_eq!(local.entries[0].path, format!("{cwd_input}/local.txt"));

        // Outside every authorized root: fixed error, no root echo.
        let outside = complete_path(&app, "/etc/", CompletionKind::Any, None)
            .expect_err("outside root");
        assert_eq!(outside.to_string(), "path is outside the workspace");
        // A similarly-named sibling of a root is not containment.
        let sibling = complete_path(&app, &format!("{root_input}-sibling/"), CompletionKind::Any, None)
            .expect_err("sibling root");
        assert_eq!(sibling.to_string(), "path is outside the workspace");
    }

    #[tokio::test]
    async fn complete_path_accepts_exact_authorized_root_without_trailing_slash() {
        let cwd = tempfile::tempdir().expect("cwd");
        let additional = tempfile::tempdir().expect("additional");
        std::fs::write(additional.path().join("shared.txt"), "s").expect("shared file");
        std::fs::create_dir(additional.path().join("shared_dir")).expect("shared dir");
        let app = completion_application(cwd.path(), &[additional.path().to_path_buf()]).await;

        // The exact authorized root without a trailing separator (what the
        // Session panel pre-fills as the current cwd) succeeds as ONE
        // directory candidate — the typed root itself — and the parent
        // above the root is neither opened nor listed.
        let root_input = additional.path().to_str().expect("utf-8 additional root");
        let exact = complete_path(&app, root_input, CompletionKind::Any, None)
            .expect("exact authorized root");
        assert_eq!(exact.base, root_input);
        let root_name = additional.path().file_name().and_then(|name| name.to_str()).expect("utf-8 root name");
        assert_eq!(exact.prefix, root_name);
        assert_eq!(exact.entries.len(), 1);
        assert_eq!(exact.entries[0].name, root_name);
        assert_eq!(exact.entries[0].path, root_input);
        assert_eq!(exact.entries[0].kind, "directory");
        assert!(!exact.truncated);

        // The cwd root itself completes the same way.
        let cwd_input = cwd.path().to_str().expect("utf-8 cwd");
        let exact_cwd = complete_path(&app, cwd_input, CompletionKind::Any, None)
            .expect("exact cwd root");
        assert_eq!(exact_cwd.entries.len(), 1);
        assert_eq!(exact_cwd.entries[0].path, cwd_input);
        assert_eq!(exact_cwd.entries[0].kind, "directory");

        // Containment never widened: the root's direct ancestor and a
        // sibling typed without a trailing separator stay fail-closed with
        // the fixed literal (no path echo). A trailing `..` climb keeps the
        // authorized ROOT itself as the listing target (the split peels
        // `..` off as the prefix), so the ancestor is never opened or
        // listed — quiet empty response only.
        let parent_input = additional.path().parent().and_then(|path| path.to_str()).expect("utf-8 parent");
        let ancestor = complete_path(&app, parent_input, CompletionKind::Any, None)
            .expect_err("ancestor of root");
        assert_eq!(ancestor.to_string(), "path is outside the workspace");
        let sibling = complete_path(&app, &format!("{root_input}-sibling"), CompletionKind::Any, None)
            .expect_err("sibling of root");
        assert_eq!(sibling.to_string(), "path is outside the workspace");
        let climb = complete_path(&app, &format!("{root_input}/.."), CompletionKind::Any, None)
            .expect("climb stays inside the root");
        assert!(climb.entries.is_empty());
        assert_eq!(climb.prefix, "..");
        // A trailing-slash climb through the ancestor is rejected outright.
        let climb_out = complete_path(&app, &format!("{root_input}/../"), CompletionKind::Any, None)
            .expect_err("climb through ancestor");
        assert_eq!(climb_out.to_string(), "path is outside the workspace");

        // A trailing separator still LISTS the root's children — the exact
        // branch never swallowed the existing listing behavior.
        let listed = complete_path(&app, &format!("{root_input}/"), CompletionKind::Any, None)
            .expect("root listing");
        assert_eq!(names(&listed.entries), ["shared_dir", "shared.txt"]);
    }

    #[tokio::test]
    async fn complete_path_truncates_at_limit_and_reports_overflow() {
        let cwd = tempfile::tempdir().expect("cwd");
        for index in 0..120 {
            std::fs::write(cwd.path().join(format!("f{index:03}.txt")), "x")
                .expect("cap fixture");
        }
        let app = completion_application(cwd.path(), &[]).await;

        let bounded = complete_path(&app, "", CompletionKind::Any, Some(10)).expect("bounded");
        assert_eq!(bounded.entries.len(), 10);
        assert_eq!(bounded.entries[0].name, "f000.txt");
        assert_eq!(bounded.entries[9].name, "f009.txt");
        assert!(bounded.truncated);
        // The requested limit is clamped to the wire ceiling.
        let clamped = complete_path(&app, "", CompletionKind::Any, Some(500)).expect("clamped");
        assert_eq!(clamped.entries.len(), MAX_COMPLETION_ENTRIES);
        assert!(clamped.truncated);
    }

    #[tokio::test]
    async fn complete_path_rejects_traversal_and_escapes() {
        let cwd = tempfile::tempdir().expect("cwd");
        let app = completion_application(cwd.path(), &[]).await;
        // Inputs whose lexical climb pops above the workspace root are
        // rejected outright...
        for input in ["../outside", "../../x", "/..", "/../x", "a/../../outside"] {
            let error = complete_path(&app, input, CompletionKind::Any, None)
                .expect_err("traversal rejected");
            assert_eq!(error.to_string(), "path is outside the workspace", "{input}");
        }
        // ...while `..` chains that lexically normalize back INTO the cwd
        // stay contained: they list the cwd with the typed prefix and match
        // nothing.
        for input in ["..", "a/../.."] {
            let response = complete_path(&app, input, CompletionKind::Any, None)
                .expect("contained traversal");
            assert_eq!(response.base, "", "{input}");
            assert!(response.entries.is_empty(), "{input}");
            assert!(!response.truncated, "{input}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn complete_path_never_completes_symlinks_or_git() {
        let cwd = tempfile::tempdir().expect("cwd");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret.txt"), "secret").expect("secret file");
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            cwd.path().join("link.txt"),
        )
        .expect("file symlink");
        std::os::unix::fs::symlink(outside.path(), cwd.path().join("escape"))
            .expect("dir symlink");
        std::fs::create_dir(cwd.path().join("real_dir")).expect("real dir");
        let app = completion_application(cwd.path(), &[]).await;

        // Created after the fixture builds so the resource manager's Git
        // discovery never sees a HEAD-less .git during construction.
        std::fs::create_dir(cwd.path().join(".git")).expect("git dir");

        // Symlinked entries never surface in suggestions...
        let listed = complete_path(&app, "", CompletionKind::Any, None).expect("listing");
        assert_eq!(names(&listed.entries), ["real_dir"]);
        // ...and a symlinked directory component fails closed without
        // following to the outside target.
        let escaped = complete_path(&app, "link.txt/", CompletionKind::Any, None);
        assert!(escaped.is_err(), "symlink parent must not list");
        let escape_dir = complete_path(&app, "escape/", CompletionKind::Any, None);
        assert!(escape_dir.is_err(), "symlink dir must not list");
        let git = complete_path(&app, ".gi", CompletionKind::Any, None).expect("git prefix");
        assert!(git.entries.is_empty());
        // Any exact `.git` parent component is rejected outright, before
        // inspection or listing — not merely filtered as a suggestion.
        let git_exact = complete_path(&app, ".git/", CompletionKind::Any, None)
            .expect_err(".git never lists");
        assert_eq!(git_exact.to_string(), "path is outside the workspace");
        let git_nested = complete_path(&app, ".git/refs/", CompletionKind::Any, None)
            .expect_err(".git component never lists");
        assert_eq!(git_nested.to_string(), "path is outside the workspace");
    }

    #[tokio::test]
    async fn complete_path_rejects_nul_at_lexical_gate() {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::create_dir(cwd.path().join("beta")).expect("beta dir");
        let app = completion_application(cwd.path(), &[]).await;
        // NUL can never occur in a real filesystem path: it is rejected
        // lexically, before any policy inspection or directory open, with
        // the same fixed error as every other rejected request — including
        // when only the typed prefix carries it.
        for input in ["\0", "be\0ta", "beta/\0", "beta/x\0y"] {
            let error = complete_path(&app, input, CompletionKind::Any, None)
                .expect_err("nul rejected");
            assert_eq!(error.to_string(), "path is outside the workspace", "{input}");
        }
    }

    #[tokio::test]
    async fn complete_path_skips_policy_denied_children() -> anyhow::Result<()> {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::create_dir(cwd.path().join("blocked")).expect("blocked dir");
        std::fs::write(cwd.path().join("blocked/inner.txt"), "i").expect("inner file");
        std::fs::write(cwd.path().join("open.txt"), "o").expect("open file");
        let settings = serde_json::json!({
            "sandbox": {
                "mode": "workspace-write",
                "deniedPaths": [cwd.path().join("blocked")],
            }
        })
        .to_string();
        // Confined bootstrap authenticates the trusted launcher; an
        // environment without one fails with a `launcher`-named error and
        // the test skips rather than silently degrading (the same
        // convention as the confined fixtures in `pi-coding`).
        let app = match completion_application_with_settings(cwd.path(), &[], &settings).await {
            Ok(app) => app,
            Err(error) => {
                let message = format!("{error:#}");
                if message.contains("launcher") {
                    eprintln!(
                        "skipping confined completion test: trusted sandbox launcher unavailable ({message})"
                    );
                    return Ok(());
                }
                anyhow::bail!("confined completion fixture failed: {message}");
            }
        };
        // A denied child inside an otherwise-listable directory never
        // surfaces and its name never leaks — not even under a matching
        // prefix — while the allowed entries still complete.
        let listed = complete_path(&app, "", CompletionKind::Any, None)?;
        assert_eq!(names(&listed.entries), ["open.txt"]);
        assert!(!listed.truncated);
        let prefixed = complete_path(&app, "bl", CompletionKind::Any, None)?;
        assert!(prefixed.entries.is_empty());
        assert!(!prefixed.truncated);
        // Completing inside the denied directory still fails closed.
        let denied = complete_path(&app, "blocked/", CompletionKind::Any, None)
            .expect_err("denied dir");
        assert_eq!(denied.to_string(), "path is outside the workspace");
        Ok(())
    }

    /// P0 investigation: a File Explorer directory toggle reports 1-2s.
    /// `list` classified dir/file entries from the dirent, but acquired every
    /// FILE's size through one sandboxed `open_with` + `metadata` per entry —
    /// O(entries) sandboxed opens for a bounded (MAX_LIST_ENTRIES) response
    /// whose retention ordering never reads the size. This QA test quantifies
    /// exactly that: two same-size directories, one of FILES (one open+stat
    /// per entry on the old path) and one of DIRS (dirent classification
    /// only), so the file-entry overhead is measurable in isolation. The
    /// printed timings are the evidence; correctness assertions keep the test
    /// meaningful without turning wall-clock into a CI-stability hazard.
    #[tokio::test]
    async fn list_latency_profile_files_vs_dirs_at_scale() {
        let cwd = tempfile::tempdir().expect("cwd");
        // Build two sibling directories of the SAME cardinality: one with
        // `n` files (the old code opened + stated every one) and one with
        // `n` subdirectories (dirent classification only). The delta is the
        // per-file open+metadata overhead.
        for n in [2000usize, 10000] {
            let files_dir = cwd.path().join(format!("files-{n}"));
            let dirs_dir = cwd.path().join(format!("dirs-{n}"));
            std::fs::create_dir(&files_dir).expect("files dir");
            std::fs::create_dir(&dirs_dir).expect("dirs dir");
            for i in 0..n {
                std::fs::write(files_dir.join(format!("f{i:06}.txt")), "x").expect("file");
                std::fs::create_dir(dirs_dir.join(format!("d{i:06}"))).expect("subdir");
            }
        }
        let app = completion_application(cwd.path(), &[]).await;
        for n in [2000usize, 10000] {
            let dirs_start = std::time::Instant::now();
            let dirs_response = list(&app, &format!("dirs-{n}")).expect("dirs listing");
            let dirs_elapsed = dirs_start.elapsed();
            let files_start = std::time::Instant::now();
            let files_response = list(&app, &format!("files-{n}")).expect("files listing");
            let files_elapsed = files_start.elapsed();
            assert_eq!(dirs_response.entries.len(), n.min(MAX_LIST_ENTRIES), "dirs listing cardinality");
            assert_eq!(files_response.entries.len(), n.min(MAX_LIST_ENTRIES), "files listing cardinality");
            assert_eq!(dirs_response.truncated, n > MAX_LIST_ENTRIES);
            assert_eq!(files_response.truncated, n > MAX_LIST_ENTRIES);
            println!(
                "file_explorer list latency profile: n={n} dirs={} files={} dirs_elapsed={dirs_elapsed:?} files_elapsed={files_elapsed:?} truncated={}",
                dirs_response.entries.len(),
                files_response.entries.len(),
                files_response.truncated,
            );
        }
    }

    fn entry_names(response: &ListResponse) -> Vec<&str> {
        response.entries.iter().map(|entry| entry.name.as_str()).collect()
    }

    #[tokio::test]
    async fn list_and_changes_walk_checked_out_submodule_files() {
        let parent = tempfile::tempdir().expect("parent");
        git(parent.path(), &["init"]);
        std::fs::write(parent.path().join("README.md"), "root\n").expect("readme");
        git(parent.path(), &["add", "README.md"]);
        git(parent.path(), &["commit", "-m", "parent initial"]);
        let sub_dir = tempfile::tempdir().expect("submodule source");
        git(sub_dir.path(), &["init"]);
        std::fs::write(sub_dir.path().join("lib.txt"), "base\n").expect("lib");
        std::fs::create_dir(sub_dir.path().join("src")).expect("src");
        std::fs::write(sub_dir.path().join("src/mod.rs"), "mod\n").expect("mod");
        git(sub_dir.path(), &["add", "lib.txt", "src/mod.rs"]);
        git(sub_dir.path(), &["commit", "-m", "submodule initial"]);
        let sub_path = sub_dir.path().canonicalize().expect("canonical submodule");
        git(
            parent.path(),
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                sub_path.to_str().expect("utf8 submodule path"),
                "ext",
            ],
        );
        git(parent.path(), &["commit", "-m", "add submodule"]);
        std::fs::write(parent.path().join("ext/lib.txt"), "dirty\n").expect("dirty submodule file");

        let app = completion_application(parent.path(), &[]).await;
        let root = list(&app, "").expect("root listing");
        let ext = root
            .entries
            .iter()
            .find(|entry| entry.name == "ext")
            .expect("submodule dir");
        assert_eq!(ext.kind, "dir");
        let nested = list(&app, "ext").expect("submodule listing");
        let names = entry_names(&nested);
        assert!(names.contains(&"lib.txt"), "{names:?}");
        assert!(names.contains(&"src"), "{names:?}");
        let src = list(&app, "ext/src").expect("submodule nested listing");
        assert_eq!(entry_names(&src), ["mod.rs"]);

        let changed = changes(&app).expect("submodule changes");
        assert!(changed.is_repo);
        assert!(
            changed.files.iter().any(|file| file.path == "ext/lib.txt" && file.status == "modified"),
            "{:?}",
            changed.files
        );
        assert!(
            changed.files.iter().all(|file| file.path != "ext"),
            "expanded gitlink must not keep the submodule path as a file: {:?}",
            changed.files
        );
    }

    #[tokio::test]
    async fn changes_keeps_gitlink_row_for_missing_submodule_checkout() {
        // A committed gitlink whose checkout directory is gone must stay a
        // parent-repo row. discover() would otherwise resolve the gitlink
        // path to the superproject and fabricate prefixed phantom children.
        let parent = tempfile::tempdir().expect("parent");
        git(parent.path(), &["init"]);
        std::fs::write(parent.path().join("README.md"), "root\n").expect("readme");
        git(parent.path(), &["add", "README.md"]);
        git(parent.path(), &["commit", "-m", "parent initial"]);
        let sub_dir = tempfile::tempdir().expect("submodule source");
        git(sub_dir.path(), &["init"]);
        std::fs::write(sub_dir.path().join("lib.txt"), "base\n").expect("lib");
        git(sub_dir.path(), &["add", "lib.txt"]);
        git(sub_dir.path(), &["commit", "-m", "submodule initial"]);
        let sub_path = sub_dir.path().canonicalize().expect("canonical submodule");
        git(
            parent.path(),
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                sub_path.to_str().expect("utf8 submodule path"),
                "ext",
            ],
        );
        git(parent.path(), &["commit", "-m", "add submodule"]);
        std::fs::write(parent.path().join("ext/lib.txt"), "dirty\n").expect("dirty inner");
        std::fs::write(parent.path().join("ghost.txt"), "ghost\n").expect("ghost file");
        git(parent.path(), &["add", "ghost.txt"]);
        git(parent.path(), &["commit", "-m", "add ghost"]);
        std::fs::remove_dir_all(parent.path().join("ext")).expect("remove checkout");

        let app = completion_application(parent.path(), &[]).await;
        let changed = changes(&app).expect("changes");
        assert!(
            changed.files.iter().any(|file| file.path == "ext" && file.status == "deleted"),
            "missing-checkout gitlink keeps its parent deletion row: {:?}",
            changed.files
        );
        assert!(
            changed.files.iter().all(|file| !file.path.starts_with("ext/")),
            "no phantom children for a missing checkout: {:?}",
            changed.files
        );
        let ext_dir = list(&app, "ext");
        assert!(ext_dir.is_err(), "missing directory must fail closed: {ext_dir:?}");
    }

    #[tokio::test]
    async fn list_hides_root_ignored_files_and_dirs_but_keeps_gitignore() {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::write(cwd.path().join(".gitignore"), "ignored.txt\nlogs/\n")
            .expect("root gitignore");
        std::fs::write(cwd.path().join("ignored.txt"), "i").expect("ignored file");
        std::fs::write(cwd.path().join("kept.txt"), "k").expect("kept file");
        std::fs::create_dir(cwd.path().join("logs")).expect("ignored dir");
        std::fs::create_dir(cwd.path().join("kept")).expect("kept dir");
        let app = completion_application(cwd.path(), &[]).await;

        let response = list(&app, "").expect("root listing");
        let names = entry_names(&response);
        assert!(!names.contains(&"ignored.txt"));
        assert!(!names.contains(&"logs"));
        assert!(names.contains(&"kept"));
        assert!(names.contains(&"kept.txt"));
        assert!(names.contains(&".gitignore"));
    }

    #[tokio::test]
    async fn list_gitignore_negation_unignores_matched_files() {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::write(cwd.path().join(".gitignore"), "*.log\n!important.log\n")
            .expect("root gitignore");
        std::fs::write(cwd.path().join("a.log"), "a").expect("ignored log");
        std::fs::write(cwd.path().join("important.log"), "b").expect("negated log");
        let app = completion_application(cwd.path(), &[]).await;

        let response = list(&app, "").expect("root listing");
        let names = entry_names(&response);
        assert!(!names.contains(&"a.log"));
        assert!(names.contains(&"important.log"));
        assert!(names.contains(&".gitignore"));
    }

    #[tokio::test]
    async fn list_nested_gitignore_overrides_parent_rules() {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::write(cwd.path().join(".gitignore"), "secret.txt\n*.gen\n")
            .expect("root gitignore");
        std::fs::create_dir(cwd.path().join("sub")).expect("sub dir");
        std::fs::write(cwd.path().join("sub/.gitignore"), "!secret.txt\n")
            .expect("nested gitignore");
        std::fs::write(cwd.path().join("sub/secret.txt"), "s").expect("negated file");
        std::fs::write(cwd.path().join("sub/other.gen"), "o").expect("ignored file");
        std::fs::write(cwd.path().join("sub/keep.txt"), "k").expect("kept file");
        let app = completion_application(cwd.path(), &[]).await;

        let nested = list(&app, "sub").expect("nested listing");
        let names = entry_names(&nested);
        assert_eq!(names, [".gitignore", "keep.txt", "secret.txt"]);
    }

    #[tokio::test]
    async fn complete_path_agrees_with_list_on_gitignore_filtering() {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::write(cwd.path().join(".gitignore"), "ignored.txt\n").expect("root gitignore");
        std::fs::write(cwd.path().join("ignored.txt"), "i").expect("ignored file");
        std::fs::write(cwd.path().join("kept.txt"), "k").expect("kept file");
        std::fs::create_dir(cwd.path().join("sub")).expect("sub dir");
        std::fs::write(cwd.path().join("sub/.gitignore"), "hidden.txt\n").expect("nested gitignore");
        std::fs::write(cwd.path().join("sub/hidden.txt"), "h").expect("ignored nested file");
        std::fs::write(cwd.path().join("sub/visible.txt"), "v").expect("kept nested file");
        let app = completion_application(cwd.path(), &[]).await;

        let listed = list(&app, "").expect("root listing");
        assert!(!entry_names(&listed).contains(&"ignored.txt"));
        let completed = complete_path(&app, "", CompletionKind::Any, None).expect("root completion");
        let completed_names = names(&completed.entries);
        assert!(!completed_names.contains(&"ignored.txt"));
        assert!(completed_names.contains(&"kept.txt"));
        assert!(completed_names.contains(&"sub"));
        let prefixed = complete_path(&app, "ign", CompletionKind::Any, None).expect("prefix miss");
        assert!(prefixed.entries.is_empty());

        let nested_listed = list(&app, "sub").expect("nested listing");
        assert_eq!(entry_names(&nested_listed), [".gitignore", "visible.txt"]);
        let nested_completed =
            complete_path(&app, "sub/", CompletionKind::Any, None).expect("nested completion");
        assert_eq!(names(&nested_completed.entries), ["visible.txt"]);
    }

    #[tokio::test]
    async fn list_and_complete_inside_ignored_directory_hide_everything() {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::write(cwd.path().join(".gitignore"), "logs/\n").expect("root gitignore");
        std::fs::create_dir(cwd.path().join("logs")).expect("ignored dir");
        std::fs::write(cwd.path().join("logs/.gitignore"), "!logs.txt\n").expect("nested gitignore");
        std::fs::write(cwd.path().join("logs/logs.txt"), "l").expect("nested file");
        std::fs::write(cwd.path().join("logs/keep.txt"), "k").expect("kept nested file");
        let app = completion_application(cwd.path(), &[]).await;

        let listed = list(&app, "logs").expect("ignored dir listing");
        assert!(listed.entries.is_empty());
        assert!(!listed.truncated);
        let completed =
            complete_path(&app, "logs/", CompletionKind::Any, None).expect("ignored completion");
        assert!(completed.entries.is_empty());
        assert!(!completed.truncated);
    }

    #[tokio::test]
    async fn nested_gitignore_applies_when_parent_chain_not_ignored() {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::write(cwd.path().join(".gitignore"), "outer/*\n!outer/inner/\n")
            .expect("root gitignore");
        std::fs::create_dir(cwd.path().join("outer")).expect("outer dir");
        std::fs::create_dir(cwd.path().join("outer/inner")).expect("inner dir");
        std::fs::write(cwd.path().join("outer/inner/.gitignore"), "hidden.txt\n")
            .expect("nested gitignore");
        std::fs::write(cwd.path().join("outer/inner/hidden.txt"), "h").expect("hidden file");
        std::fs::write(cwd.path().join("outer/inner/visible.txt"), "v").expect("visible file");
        std::fs::write(cwd.path().join("outer/sibling.txt"), "s").expect("sibling file");
        let app = completion_application(cwd.path(), &[]).await;

        let outer = list(&app, "outer").expect("outer listing");
        assert_eq!(entry_names(&outer), ["inner"]);
        let inner = list(&app, "outer/inner").expect("inner listing");
        assert_eq!(entry_names(&inner), [".gitignore", "visible.txt"]);
    }

    #[tokio::test]
    async fn list_missing_directory_is_typed_unavailable() {
        let cwd = tempfile::tempdir().expect("cwd");
        let app = completion_application(cwd.path(), &[]).await;
        let missing = list(&app, "deployments/sepolia").expect_err("absent dir");
        assert_eq!(missing.to_string(), "directory is unavailable");
        assert_eq!(
            missing.downcast_ref::<FileExplorerError>(),
            Some(&FileExplorerError::DirectoryUnavailable)
        );
    }

    #[tokio::test]
    async fn list_inaccessible_directory_is_typed_unavailable() {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::create_dir(cwd.path().join("real")).expect("real dir");
        std::fs::write(cwd.path().join("real/keep.txt"), "k").expect("keep");
        let app = completion_application(cwd.path(), &[]).await;
        let locked = cwd.path().join("real");
        let mut permissions = std::fs::metadata(&locked).expect("dir metadata").permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o000);
        std::fs::set_permissions(&locked, permissions).expect("lock dir");
        let denied = list(&app, "real");
        let mut restored = std::fs::metadata(&locked).expect("restore metadata").permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut restored, 0o755);
        std::fs::set_permissions(&locked, restored).expect("unlock dir");
        match denied {
            Err(error) => {
                assert_eq!(error.to_string(), "directory is unavailable");
                assert_eq!(
                    error.downcast_ref::<FileExplorerError>(),
                    Some(&FileExplorerError::DirectoryUnavailable),
                    "an inaccessible directory is gone, not a retryable list failure"
                );
            }
            Ok(response) => {
                std::fs::read(locked.join("keep.txt"))
                    .expect("mode 000 listed only when this process can still read it");
                assert!(entry_names(&response).contains(&"keep.txt"));
            }
        }
    }

    #[tokio::test]
    async fn list_file_in_place_of_directory_is_typed_unavailable() {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::write(cwd.path().join("real"), "not a directory\n").expect("file");
        let app = completion_application(cwd.path(), &[]).await;
        let replaced = list(&app, "real").expect_err("file is not a directory");
        assert_eq!(replaced.to_string(), "directory is unavailable");
        assert_eq!(
            replaced.downcast_ref::<FileExplorerError>(),
            Some(&FileExplorerError::DirectoryUnavailable)
        );
    }

    #[tokio::test]
    async fn list_symlink_directory_is_typed_unavailable() {
        let cwd = tempfile::tempdir().expect("cwd");
        let outside = tempfile::tempdir().expect("outside");
        std::os::unix::fs::symlink(outside.path(), cwd.path().join("escape")).expect("dir symlink");
        let app = completion_application(cwd.path(), &[]).await;
        let linked = list(&app, "escape").expect_err("symlink dir");
        assert_eq!(linked.to_string(), "directory is unavailable");
        assert_eq!(
            linked.downcast_ref::<FileExplorerError>(),
            Some(&FileExplorerError::DirectoryUnavailable)
        );
    }

    fn assert_list_fail_closed_before_leak(app: &Application, sentinel: &str) {
        match list(app, "") {
            Ok(response) => panic!(
                "gitignore must fail closed before leaking {sentinel}: {:?}",
                entry_names(&response)
            ),
            Err(error) => assert_eq!(error.to_string(), "directory is unavailable"),
        }
    }

    #[tokio::test]
    async fn list_fails_closed_on_oversized_gitignore() {
        let cwd = tempfile::tempdir().expect("cwd");
        let mut bytes = vec![b'x'; MAX_GITIGNORE_BYTES + 1];
        bytes[..11].copy_from_slice(b"secret.txt\n");
        std::fs::write(cwd.path().join(".gitignore"), &bytes).expect("oversized gitignore");
        std::fs::write(cwd.path().join("secret.txt"), "s").expect("sentinel");
        let app = completion_application(cwd.path(), &[]).await;
        assert_list_fail_closed_before_leak(&app, "secret.txt");
    }

    #[tokio::test]
    async fn list_fails_closed_on_invalid_gitignore_pattern() {
        let cwd = tempfile::tempdir().expect("cwd");
        let invalid = "[z-a]";
        let mut builder = GitignoreBuilder::new("");
        assert!(
            builder.add_line(None, invalid).is_err(),
            "fixture must remain an invalid gitignore pattern"
        );
        std::fs::write(
            cwd.path().join(".gitignore"),
            format!("secret.txt\n{invalid}\n"),
        )
        .expect("invalid gitignore");
        std::fs::write(cwd.path().join("secret.txt"), "s").expect("sentinel");
        let app = completion_application(cwd.path(), &[]).await;
        assert_list_fail_closed_before_leak(&app, "secret.txt");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn list_fails_closed_on_unreadable_gitignore() {
        struct RestoreMode {
            path: PathBuf,
            permissions: std::fs::Permissions,
        }
        impl Drop for RestoreMode {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(&self.path, self.permissions.clone());
            }
        }

        let cwd = tempfile::tempdir().expect("cwd");
        let gitignore = cwd.path().join(".gitignore");
        std::fs::write(&gitignore, "secret.txt\n").expect("gitignore");
        std::fs::write(cwd.path().join("secret.txt"), "s").expect("sentinel");
        let original = std::fs::metadata(&gitignore)
            .expect("gitignore metadata")
            .permissions();
        let _restore = RestoreMode {
            path: gitignore.clone(),
            permissions: original.clone(),
        };
        let mut locked = original;
        std::os::unix::fs::PermissionsExt::set_mode(&mut locked, 0o000);
        std::fs::set_permissions(&gitignore, locked).expect("lock gitignore");
        let app = completion_application(cwd.path(), &[]).await;
        // Skip only if File::open still succeeds on mode 000 (euid 0 / CAP_DAC_OVERRIDE).
        // Any other outcome must fail closed; do not treat other IO errors as a skip.
        match std::fs::File::open(&gitignore) {
            Ok(_) => {
                eprintln!(
                    "skipping unreadable gitignore fail-closed test: effective user can still open mode-000 .gitignore"
                );
                return;
            }
            Err(_) => assert_list_fail_closed_before_leak(&app, "secret.txt"),
        }
    }

    fn blame_record(oid: &str, current: usize, author: &str, time: &str, text: &str) -> String {
        format!(
            "{oid} 4 {current} 1\nauthor {author}\nauthor-mail <hidden@example.com>\nauthor-time {time}\nauthor-tz +0000\ncommitter Committer\nsummary subject\nfilename src/lib.rs\n\t{text}\n"
        )
    }

    fn blame_record_without_group(
        oid: &str,
        current: usize,
        author: &str,
        time: &str,
        text: &str,
    ) -> String {
        format!(
            "{oid} 5 {current}\nauthor {author}\nauthor-mail <hidden@example.com>\nauthor-time {time}\nauthor-tz +0000\ncommitter Committer\nsummary subject\nfilename src/lib.rs\n\t{text}\n"
        )
    }

    #[test]
    fn blame_porcelain_accepts_exact_range_and_hides_zero_oid() {
        let porcelain = format!(
            "{}{}",
            blame_record(
                "0123456789abcdef0123456789abcdef01234567",
                2,
                "Ada Lovelace",
                "1712345678",
                "kept"
            ),
            blame_record_without_group(
                "0000000000000000000000000000000000000000",
                3,
                "Not Committed Yet",
                "1712345999",
                "dirty"
            )
        );
        let response = parse_line_porcelain(
            porcelain.as_bytes(),
            "src/lib.rs",
            2,
            &["kept".into(), "dirty".into()],
        )
        .expect("valid porcelain");
        assert_eq!(response.path, "src/lib.rs");
        assert_eq!(response.lines.len(), 2);
        assert_eq!(
            response.lines[0].commit.as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        assert_eq!(response.lines[0].author, "Ada Lovelace");
        assert_eq!(response.lines[0].time, 1_712_345_678);
        assert_eq!(response.lines[0].original_line, 4);
        assert_eq!(response.lines[0].current_line, 2);
        assert_eq!(response.lines[0].text, "kept");
        assert_eq!(response.lines[1].commit, None);
        assert_eq!(response.lines[1].current_line, 3);
        let sha256 = parse_line_porcelain(
            blame_record(&"cd".repeat(32), 8, "Ada Lovelace", "1712345678", "kept").as_bytes(),
            "src/lib.rs",
            8,
            &["kept".into()],
        )
        .expect("sha256 porcelain");
        assert_eq!(
            sha256.lines[0].commit.as_deref(),
            Some("cd".repeat(32).as_str())
        );
        let payload = serde_json::to_value(&response).expect("serialize blame");
        assert!(payload["lines"][0].get("author-mail").is_none());
        assert!(payload.to_string().contains("hidden@example.com") == false);
        assert!(payload["lines"][1]["commit"].is_null());
    }

    #[test]
    fn blame_porcelain_rejects_malformed_fields_order_and_changed_text() {
        let valid = blame_record(
            "abcdefabcdefabcdefabcdefabcdefabcdefabcd",
            1,
            "Ada",
            "10",
            "same",
        );
        let cases = [
            ("changed text", valid.replace("same", "changed")),
            ("zero group", valid.replace(" 1\n", " 0\n")),
            ("padded group", valid.replace(" 1\n", " 00\n")),
            ("nonnumeric group", valid.replace(" 1\n", " extra\n")),
            (
                "overflow group",
                valid.replace(" 1\n", " 18446744073709551616\n"),
            ),
            ("zero original", valid.replacen("1 1\n", "0 1\n", 1)),
            ("negative time", valid.replace("author-time 10", "author-time -1")),
            ("trailing time", valid.replace("author-time 10", "author-time 10extra")),
            (
                "short oid",
                valid.replace(
                    "abcdefabcdefabcdefabcdefabcdefabcdefabcd",
                    "not-an-oid",
                ),
            ),
            ("control author", valid.replace("Ada", "Ada\u{0001}")),
            ("extra record", format!("{valid}{valid}")),
            ("missing author", valid.replace("\nauthor ", "\n")),
        ];
        for (name, porcelain) in cases {
            let error = parse_line_porcelain(
                porcelain.as_bytes(),
                "src/lib.rs",
                1,
                &["same".into()],
            )
            .expect_err(name);
            assert_eq!(error.to_string(), "file blame is unavailable", "{name}");
            assert!(!error.to_string().contains('/'), "{name}");
            assert!(!error.to_string().contains('@'), "{name}");
        }
    }

    #[test]
    fn blame_response_budget_rejects_before_returning_partial_lines() {
        let long = "x".repeat(80_000);
        let mut porcelain = String::new();
        let mut source = Vec::new();
        for current in 1..=5 {
            porcelain.push_str(&blame_record(
                "abcdefabcdefabcdefabcdefabcdefabcdefabcd",
                current,
                "Ada",
                "10",
                &long,
            ));
            source.push(long.clone());
        }
        let error = parse_line_porcelain(porcelain.as_bytes(), "src/lib.rs", 1, &source)
            .expect_err("response budget");
        assert_eq!(error.to_string(), "blame selection exceeds the response limit");
    }

    #[tokio::test]
    async fn search_finds_nested_files_and_directories_and_skips_ignored() {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::create_dir_all(cwd.path().join("src/nested")).expect("nested");
        std::fs::write(cwd.path().join("src/nested/widget.rs"), "fn widget() {}\n").expect("file");
        std::fs::write(cwd.path().join("src/nested/other.txt"), "nope\n").expect("other");
        std::fs::write(cwd.path().join(".gitignore"), "secret.txt\nignored_dir/\n").expect("ignore");
        std::fs::write(cwd.path().join("secret.txt"), "hidden\n").expect("secret");
        std::fs::create_dir_all(cwd.path().join("ignored_dir")).expect("ignored dir");
        std::fs::write(cwd.path().join("ignored_dir/widget.rs"), "hidden\n").expect("ignored file");
        let app = completion_application(cwd.path(), &[]).await;
        let found = search(&app, "Widget").expect("search");
        let paths: Vec<&str> = found.entries.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(paths, vec!["src/nested/widget.rs"], "{paths:?}");
        assert!(!found.truncated);
        let dirs = search(&app, "nested").expect("directory search");
        assert!(
            dirs.entries.iter().any(|entry| entry.path == "src/nested" && entry.kind == "dir"),
            "{:?}",
            dirs.entries
        );
        assert!(
            dirs.entries.iter().any(|entry| entry.path == "src/nested/widget.rs" && entry.kind == "file"),
            "{:?}",
            dirs.entries
        );
        let empty = search(&app, "   ").expect("blank query");
        assert!(empty.entries.is_empty());
        assert!(!empty.truncated);
    }

    #[tokio::test]
    async fn search_reports_truncation_past_the_match_cap() {
        let cwd = tempfile::tempdir().expect("cwd");
        for index in 0..(MAX_SEARCH_MATCHES + 5) {
            std::fs::write(cwd.path().join(format!("hit-{index:03}.txt")), "x").expect("hit");
        }
        let app = completion_application(cwd.path(), &[]).await;
        let found = search(&app, "hit-").expect("search");
        assert_eq!(found.entries.len(), MAX_SEARCH_MATCHES);
        assert!(found.truncated);
        assert!(found.entries.iter().all(|entry| entry.kind == "file"));
    }

    #[tokio::test]
    async fn search_skips_policy_denied_directories() -> anyhow::Result<()> {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::create_dir(cwd.path().join("blocked")).expect("blocked dir");
        std::fs::write(cwd.path().join("blocked/secret.txt"), "i").expect("inner");
        std::fs::write(cwd.path().join("open.txt"), "o").expect("open");
        let settings = serde_json::json!({
            "sandbox": {
                "mode": "workspace-write",
                "deniedPaths": [cwd.path().join("blocked")],
            }
        })
        .to_string();
        let app = match completion_application_with_settings(cwd.path(), &[], &settings).await {
            Ok(app) => app,
            Err(error) => {
                let message = format!("{error:#}");
                if message.contains("launcher") {
                    eprintln!(
                        "skipping confined search test: trusted sandbox launcher unavailable ({message})"
                    );
                    return Ok(());
                }
                anyhow::bail!("confined search fixture failed: {message}");
            }
        };
        let secret = search(&app, "secret")?;
        assert!(secret.entries.is_empty(), "{:?}", secret.entries);
        let blocked = search(&app, "blocked")?;
        assert!(
            blocked.entries.iter().all(|entry| entry.path != "blocked" && !entry.path.starts_with("blocked/")),
            "{:?}",
            blocked.entries
        );
        let open = search(&app, "open.txt")?;
        assert_eq!(
            open.entries.iter().map(|entry| entry.path.as_str()).collect::<Vec<_>>(),
            vec!["open.txt"]
        );
        Ok(())
    }

}
