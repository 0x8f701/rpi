//! Unified session source catalog for native Pi sessions and foreign imports.
//!
//! This module is intentionally independent of CLI args and TUI pickers. It
//! discovers sessions across supported roots, builds list/search rows, resolves
//! path-or-id selection, and performs idempotent foreign import with durable
//! lineage metadata. Callers wire the resulting native path into resume later.

mod discovery;
mod helpers;
mod lineage;

#[cfg(test)]
mod tests;

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap};
use std::env;
use std::ffi::OsStr;
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
use std::io::ErrorKind;

use thiserror::Error;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::import::{
    emit_parsed_session_at_with_lineage, open_source_direct, open_source_under_root,
    parse_opened_source_public, parse_source_under_root_public, source_id_under_root_public,
    ImportedMessage, ImportedSession, ImportLineage, ImportSessionError, ParsedSessionPublic,
    SourceSessionFormat,
};
use crate::default_session_dir;
use crate::PreparedSessionResume;

use self::discovery::{
    contains_component, is_grok_summary_depth, is_native_tree_session, matches_source_pattern,
    path_lexically_under_root, path_under_depth, selected_sources,
};
use self::helpers::{
    canonical_fingerprint, content_fingerprint, display_name, format_epoch,
    compare_rows_newest, metadata_epoch, normalize_cwd, normalize_summary,
    row_matches_query, sort_rows_newest, truncate_summary,
};
pub(crate) use self::helpers::{expand_tilde, make_absolute};
use self::lineage::{read_native_header_lite, read_native_lineage, read_native_list_info};

pub(super) const LINEAGE_CUSTOM_TYPE: &str = "import_lineage";

/// Maximum WalkDir entries visited per source, including roots, directories, and errors.
pub const SESSION_CATALOG_WALK_ENTRY_LIMIT: usize = 4_096;
/// Maximum regular-file candidates retained from any one session source.
pub const SESSION_CATALOG_CANDIDATE_LIMIT: usize = 512;
/// Maximum rows in the shared catalog universe returned by scan/list/search.
pub const SESSION_CATALOG_ROW_LIMIT: usize = 512;

#[derive(Debug)]
struct DiscoveredCandidate {
    modified_epoch: f64,
    path: PathBuf,
}

impl PartialEq for DiscoveredCandidate {
    fn eq(&self, other: &Self) -> bool {
        self.modified_epoch.total_cmp(&other.modified_epoch) == Ordering::Equal
            && self.path == other.path
    }
}

impl Eq for DiscoveredCandidate {}

impl PartialOrd for DiscoveredCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DiscoveredCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.modified_epoch
            .total_cmp(&other.modified_epoch)
            // For equal mtimes, the lexically smaller stable path ranks newer.
            .then_with(|| other.path.cmp(&self.path))
    }
}

/// Machine identity for a session root / list row source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionSourceKind {
    NativePi,
    Omp,
    Codex,
    Claude,
    Grok,
    Droid,
}

impl SessionSourceKind {
    pub const ALL: [Self; 6] = [
        Self::NativePi,
        Self::Omp,
        Self::Codex,
        Self::Claude,
        Self::Grok,
        Self::Droid,
    ];

    pub const FOREIGN: [Self; 5] = [
        Self::Omp,
        Self::Codex,
        Self::Claude,
        Self::Grok,
        Self::Droid,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NativePi => "pi",
            Self::Omp => "omp",
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Grok => "grok",
            Self::Droid => "droid",
        }
    }

    /// Human label used by pickers. Grok storage is shared with Hyper.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::NativePi => "pi",
            Self::Omp => "omp",
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Grok => "grok/hyper",
            Self::Droid => "droid",
        }
    }

    #[must_use]
    pub const fn is_native(self) -> bool {
        matches!(self, Self::NativePi)
    }

    #[must_use]
    pub const fn to_import_format(self) -> Option<SourceSessionFormat> {
        match self {
            Self::NativePi => None,
            Self::Omp => Some(SourceSessionFormat::Omp),
            Self::Codex => Some(SourceSessionFormat::Codex),
            Self::Claude => Some(SourceSessionFormat::Claude),
            Self::Grok => Some(SourceSessionFormat::Grok),
            Self::Droid => Some(SourceSessionFormat::Droid),
        }
    }

    #[must_use]
    pub const fn from_import_format(format: SourceSessionFormat) -> Self {
        match format {
            SourceSessionFormat::Pi => Self::NativePi,
            SourceSessionFormat::Omp => Self::Omp,
            SourceSessionFormat::Codex => Self::Codex,
            SourceSessionFormat::Claude => Self::Claude,
            SourceSessionFormat::Grok => Self::Grok,
            SourceSessionFormat::Droid => Self::Droid,
        }
    }
}

impl std::fmt::Display for SessionSourceKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::str::FromStr for SessionSourceKind {
    type Err = CatalogError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "pi" | "native" | "nativepi" | "native_pi" => Ok(Self::NativePi),
            "omp" => Ok(Self::Omp),
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            "grok" | "hyper" | "grok/hyper" => Ok(Self::Grok),
            "droid" => Ok(Self::Droid),
            other => Err(CatalogError::UnsupportedSource(other.to_owned())),
        }
    }
}

/// One discovered root for a source kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSourceRoot {
    pub kind: SessionSourceKind,
    pub path: PathBuf,
    pub pattern: &'static str,
}

/// Durable foreign→native import provenance.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportLineageKey {
    pub source: SessionSourceKind,
    pub source_session_id: String,
    pub source_path_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_fingerprint: Option<String>,
}

impl ImportLineageKey {
    #[must_use]
    pub fn parent_session_value(&self) -> String {
        if self.source_session_id.is_empty() {
            format!("{}:{}", self.source.as_str(), self.source_path_fingerprint)
        } else {
            format!("{}:{}", self.source.as_str(), self.source_session_id)
        }
    }

    #[must_use]
    pub fn identity_key(&self) -> (SessionSourceKind, String) {
        if self.source_session_id.is_empty() {
            (self.source, format!("path:{}", self.source_path_fingerprint))
        } else {
            (self.source, format!("id:{}", self.source_session_id))
        }
    }
}

/// Whether a catalog row is native, foreign, or already converted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogRowStatus {
    Native,
    Foreign,
    AlreadyImported {
        native_id: String,
        native_path: PathBuf,
    },
}

impl CatalogRowStatus {
    #[must_use]
    pub const fn is_native(&self) -> bool {
        matches!(self, Self::Native)
    }

    #[must_use]
    pub const fn is_import(&self) -> bool {
        !matches!(self, Self::Native)
    }
}

/// One row in the unified resume catalog.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogRow {
    pub kind: SessionSourceKind,
    pub session_id: String,
    pub summary: String,
    pub cwd: PathBuf,
    pub modified_epoch: f64,
    pub display_time: String,
    pub path: PathBuf,
    pub size: u64,
    pub message_count: Option<usize>,
    pub name: Option<String>,
    pub status: CatalogRowStatus,
    pub import_lineage: Option<ImportLineageKey>,
    /// Concatenated corpus used by fuzzy search.
    pub search_text: String,
    /// Isolated message corpus matched by the TUI selector without crossing
    /// into cwd/path; the catalog matcher still uses `search_text`.
    pub message_blob: String,
}

/// Options for unified listing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogListOptions {
    /// Empty means every source.
    pub sources: Vec<SessionSourceKind>,
    pub include_foreign: bool,
    pub dedupe: bool,
    pub named_only: bool,
    /// When set, keep rows whose session cwd matches after normalize.
    pub cwd_scope: Option<PathBuf>,
}

/// Options for fuzzy search.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogSearchOptions {
    pub sources: Vec<SessionSourceKind>,
    pub include_foreign: bool,
    pub dedupe: bool,
    pub named_only: bool,
    pub cwd_scope: Option<PathBuf>,
}

/// Sort order for catalog rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CatalogSort {
    #[default]
    Newest,
    Name,
}

/// Result of selecting / importing a catalog entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSession {
    pub kind: SessionSourceKind,
    pub source_path: PathBuf,
    pub source_session_id: Option<String>,
    pub id: String,
    pub path: PathBuf,
    pub cwd: PathBuf,
    pub messages: Vec<ImportedMessage>,
    /// True when an existing native file was returned (native resume or idempotent hit).
    pub reused_existing: bool,
    /// True when the selection was already a native Pi session (no conversion copy).
    pub native_no_copy: bool,
}

impl From<ImportedSession> for ResolvedSession {
    fn from(value: ImportedSession) -> Self {
        Self {
            kind: SessionSourceKind::from_import_format(value.source),
            source_path: value.source_path,
            source_session_id: value.source_session_id,
            id: value.id,
            path: value.path,
            cwd: value.cwd,
            messages: value.messages,
            reused_existing: value.reused_existing,
            native_no_copy: false,
        }
    }
}

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("unsupported session source: {0}")]
    UnsupportedSource(String),
    #[error("cannot determine the user's home directory")]
    HomeDirectoryUnavailable,
    #[error("search query must not be empty")]
    EmptySearchQuery,
    #[error("session not found: {0}")]
    SessionNotFound(String),
    #[error("session id is ambiguous: {input} ({matches:?})")]
    AmbiguousSession {
        input: String,
        matches: Vec<(SessionSourceKind, PathBuf)>,
    },
    #[error(transparent)]
    Import(#[from] ImportSessionError),
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Local filesystem catalog of native and foreign coding-agent sessions.
#[derive(Debug, Clone)]
pub struct SessionCatalog {
    user_home: PathBuf,
    /// Base used for foreign roots that live under the user home layout.
    sessions_home: PathBuf,
    native_agent_dir: PathBuf,
    /// Exact native session directory selected by CLI/env/settings. When set,
    /// native files and imports live directly in this directory rather than the
    /// default agent-dir `sessions/<encoded-cwd>` tree.
    native_session_root: Option<PathBuf>,
    codex_home: PathBuf,
    claude_config_dir: PathBuf,
}

impl SessionCatalog {
    /// Hermetic catalog rooted at a single fake home (tests and callers).
    #[must_use]
    pub fn new(home: impl Into<PathBuf>) -> Self {
        let home = make_absolute(home.into());
        Self::with_homes(home.clone(), home)
    }

    /// Build a catalog with explicit sessions-home and user-home roots.
    #[must_use]
    pub fn with_homes(sessions_home: impl Into<PathBuf>, user_home: impl Into<PathBuf>) -> Self {
        let user_home = make_absolute(user_home.into());
        let sessions_home = make_absolute(expand_tilde(sessions_home.into(), &user_home));
        Self {
            native_agent_dir: sessions_home.join(".pi/agent"),
            native_session_root: None,
            codex_home: sessions_home.join(".codex"),
            claude_config_dir: sessions_home.join(".claude"),
            sessions_home,
            user_home,
        }
    }

    /// Production entry point: honors `SESSIONS_HOME`, `HOME`/`USERPROFILE`,
    /// `PI_CODING_AGENT_DIR`, `CODEX_HOME`, and `CLAUDE_CONFIG_DIR`.
    pub fn from_env() -> Result<Self, CatalogError> {
        let user_home = env::var_os("HOME")
            .or_else(|| env::var_os("USERPROFILE"))
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or(CatalogError::HomeDirectoryUnavailable)?;
        let sessions_home = env::var_os("SESSIONS_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| user_home.clone());
        let native_agent_dir = env::var_os("PI_CODING_AGENT_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let codex_home = env::var_os("CODEX_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let claude_config_dir = env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        Ok(Self::from_env_paths(
            sessions_home,
            user_home,
            native_agent_dir,
            codex_home,
            claude_config_dir,
        ))
    }

    fn from_env_paths(
        sessions_home: PathBuf,
        user_home: PathBuf,
        native_agent_dir: Option<PathBuf>,
        codex_home: Option<PathBuf>,
        claude_config_dir: Option<PathBuf>,
    ) -> Self {
        let mut catalog = Self::with_homes(sessions_home, user_home.clone());
        if let Some(path) = native_agent_dir {
            catalog.native_agent_dir = make_absolute(expand_tilde(path, &user_home));
        }
        if let Some(path) = codex_home {
            catalog.codex_home = make_absolute(expand_tilde(path, &user_home));
        }
        if let Some(path) = claude_config_dir {
            catalog.claude_config_dir = make_absolute(expand_tilde(path, &user_home));
        }
        catalog
    }

    /// Override native/agent and foreign homes after construction (tests).
    #[must_use]
    pub fn with_native_agent_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.native_agent_dir = make_absolute(path.into());
        self.native_session_root = None;
        self
    }

    /// Override the native catalog with an exact, already-resolved session root.
    #[must_use]
    pub fn with_native_session_root(mut self, path: impl Into<PathBuf>) -> Self {
        self.native_session_root = Some(make_absolute(path.into()));
        self
    }

    #[must_use]
    pub fn with_codex_home(mut self, path: impl Into<PathBuf>) -> Self {
        self.codex_home = make_absolute(path.into());
        self
    }

    #[must_use]
    pub fn with_claude_config_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.claude_config_dir = make_absolute(path.into());
        self
    }

    #[must_use]
    pub fn user_home(&self) -> &Path {
        &self.user_home
    }

    #[must_use]
    pub fn sessions_home(&self) -> &Path {
        &self.sessions_home
    }

    #[must_use]
    pub fn native_agent_dir(&self) -> &Path {
        &self.native_agent_dir
    }

    #[must_use]
    pub fn root_for(&self, kind: SessionSourceKind) -> SessionSourceRoot {
        let (path, pattern) = match kind {
            SessionSourceKind::NativePi => (
                self.native_session_root
                    .clone()
                    .unwrap_or_else(|| self.native_agent_dir.join("sessions")),
                "*.jsonl",
            ),
            SessionSourceKind::Omp => (self.sessions_home.join(".omp/agent/sessions"), "*.jsonl"),
            SessionSourceKind::Codex => (self.codex_home.join("sessions"), "rollout-*.jsonl"),
            SessionSourceKind::Claude => (self.claude_config_dir.join("projects"), "*.jsonl"),
            SessionSourceKind::Grok => (self.sessions_home.join(".grok/sessions"), "summary.json"),
            SessionSourceKind::Droid => (self.sessions_home.join(".factory/sessions"), "*.jsonl"),
        };
        SessionSourceRoot { kind, path, pattern }
    }

    #[must_use]
    pub fn roots(&self) -> Vec<SessionSourceRoot> {
        SessionSourceKind::ALL
            .into_iter()
            .map(|kind| self.root_for(kind))
            .collect()
    }

    /// Discover at most [`SESSION_CATALOG_CANDIDATE_LIMIT`] candidate files from
    /// the source's bounded traversal window, retaining newest mtime then path.
    #[must_use]
    pub fn discover(&self, kind: SessionSourceKind) -> Vec<PathBuf> {
        let root = self.root_for(kind);
        let mut newest = BinaryHeap::with_capacity(SESSION_CATALOG_CANDIDATE_LIMIT);
        if !root.path.is_dir() {
            return Vec::new();
        }
        for entry in self.walk_source_entries(kind) {
            let Ok(entry) = entry else {
                continue;
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let Some(metadata) = self.safe_session_metadata(kind, path) else {
                continue;
            };
            let candidate = DiscoveredCandidate {
                modified_epoch: metadata_epoch(&metadata),
                path: path.to_path_buf(),
            };
            if newest.len() < SESSION_CATALOG_CANDIDATE_LIMIT {
                newest.push(Reverse(candidate));
            } else if newest
                .peek()
                .is_some_and(|oldest| candidate > oldest.0)
            {
                newest.pop();
                newest.push(Reverse(candidate));
            }
        }
        // Preserve the established discover API order for small stores. Within
        // the hard traversal window, retained membership is independent of
        // filesystem visitation order.
        let mut paths = newest
            .into_iter()
            .map(|Reverse(candidate)| candidate.path)
            .collect::<Vec<_>>();
        paths.sort();
        paths
    }

    /// True when `path` is an acceptable regular session file under the source root.
    #[must_use]
    pub fn is_safe_session_path(&self, kind: SessionSourceKind, path: &Path) -> bool {
        self.safe_session_metadata(kind, path).is_some()
    }

    fn safe_session_metadata(&self, kind: SessionSourceKind, path: &Path) -> Option<Metadata> {
        let root = self.root_for(kind).path;
        if !matches_source_pattern(kind, path)
            || contains_component(path, &root, ".rsync-partial")
            || (kind == SessionSourceKind::Codex
                && contains_component(path, &root, "archived_sessions"))
            || !path_lexically_under_root(path, &root)
        {
            return None;
        }
        let metadata = fs::symlink_metadata(path).ok()?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            return None;
        }
        let valid_depth = match kind {
            SessionSourceKind::NativePi if self.native_session_root.is_some() => {
                path_under_depth(path, &root, 1, 1)
            }
            // Native Pi and OMP share the same all-project resume layout:
            // `<cwd-dir>/<session>.jsonl` only. OMP's native listAllSessions
            // globs `*/*.jsonl`; deeper paths are subagent/task child trees
            // that must never enter the resume catalog.
            SessionSourceKind::NativePi | SessionSourceKind::Omp => {
                is_native_tree_session(path, &root)
            }
            SessionSourceKind::Grok => is_grok_summary_depth(path, &root),
            _ => true,
        };
        valid_depth.then_some(metadata)
    }

    fn walk_max_depth(&self, kind: SessionSourceKind) -> usize {
        match kind {
            SessionSourceKind::NativePi if self.native_session_root.is_some() => 1,
            // OMP matches native listAllSessions (`*/*.jsonl`): walk only the
            // two-component tree and never enter parent-session child dirs.
            SessionSourceKind::NativePi | SessionSourceKind::Claude | SessionSourceKind::Omp => 2,
            SessionSourceKind::Codex => 4,
            SessionSourceKind::Grok => 3,
            SessionSourceKind::Droid => 1,
        }
    }
    fn walk_source_entries(
        &self,
        kind: SessionSourceKind,
    ) -> impl Iterator<Item = Result<walkdir::DirEntry, walkdir::Error>> {
        WalkDir::new(self.root_for(kind).path)
            .follow_links(false)
            .max_depth(self.walk_max_depth(kind))
            .into_iter()
            .take(SESSION_CATALOG_WALK_ENTRY_LIMIT)
    }

    /// Scan sources into newest-first rows. Parse failures are isolated per file.
    #[must_use]
    pub fn scan(&self, sources: &[SessionSourceKind]) -> Vec<CatalogRow> {
        let selected = selected_sources(sources, true);
        let lineage_index = self.build_lineage_index();
        let mut source_rows = Vec::with_capacity(selected.len());
        for kind in selected {
            let mut rows = Vec::with_capacity(SESSION_CATALOG_CANDIDATE_LIMIT);
            for path in self.discover(kind) {
                match self.row_from_path(kind, &path, &lineage_index) {
                    Ok(Some(row)) => rows.push(row),
                    Ok(None) | Err(_) => continue,
                }
            }
            sort_rows_newest(&mut rows);
            source_rows.push(rows);
        }
        merge_source_rows(source_rows)
    }

    /// Unified list used by future `/resume` pickers.
    #[must_use]
    pub fn list(&self, options: &CatalogListOptions) -> Vec<CatalogRow> {
        let rows = self.scan(&options.sources);
        self.finish_rows(rows, options.include_foreign, options.dedupe, options.named_only, options.cwd_scope.as_deref())
    }

    /// Fuzzy multi-token search over id/name/summary/cwd/path/source/messages.
    pub fn search(
        &self,
        query: &str,
        options: &CatalogSearchOptions,
    ) -> Result<Vec<CatalogRow>, CatalogError> {
        if query.trim().is_empty() {
            return Err(CatalogError::EmptySearchQuery);
        }
        let rows = self.scan(&options.sources);
        let filtered = rows
            .into_iter()
            .filter(|row| row_matches_query(row, query))
            .collect::<Vec<_>>();
        Ok(self.finish_rows(
            filtered,
            options.include_foreign,
            options.dedupe,
            options.named_only,
            options.cwd_scope.as_deref(),
        ))
    }

    /// Sort a row set without re-scanning.
    #[must_use]
    pub fn sort_rows(mut rows: Vec<CatalogRow>, sort: CatalogSort) -> Vec<CatalogRow> {
        match sort {
            CatalogSort::Newest => sort_rows_newest(&mut rows),
            CatalogSort::Name => rows.sort_by(|left, right| {
                display_name(left)
                    .to_ascii_lowercase()
                    .cmp(&display_name(right).to_ascii_lowercase())
                    .then_with(|| left.path.cmp(&right.path))
            }),
        }
        rows.truncate(SESSION_CATALOG_ROW_LIMIT);
        rows
    }

    /// Keep newest row per source/cwd/summary (or source/id when summary empty).
    #[must_use]
    pub fn dedupe_rows(rows: &[CatalogRow]) -> Vec<CatalogRow> {
        let mut best = Vec::<((SessionSourceKind, String, String), CatalogRow)>::with_capacity(
            SESSION_CATALOG_ROW_LIMIT,
        );
        for row in rows {
            let normalized_summary = normalize_summary(&row.summary);
            let cwd_empty = row.cwd.as_os_str().is_empty();
            let key = if cwd_empty || normalized_summary.is_empty() {
                (row.kind, row.session_id.clone(), String::new())
            } else {
                (row.kind, normalize_cwd(&row.cwd), normalized_summary)
            };
            if let Some((_, existing)) = best.iter_mut().find(|(existing, _)| *existing == key) {
                if compare_rows_newest(row, existing).is_lt() {
                    *existing = row.clone();
                }
                continue;
            }
            if best.len() < SESSION_CATALOG_ROW_LIMIT {
                best.push((key, row.clone()));
                continue;
            }
            let oldest = best
                .iter()
                .enumerate()
                .max_by(|(_, (_, left)), (_, (_, right))| compare_rows_newest(left, right))
                .map(|(index, _)| index)
                .expect("non-empty bounded catalog");
            if compare_rows_newest(row, &best[oldest].1).is_lt() {
                best[oldest] = (key, row.clone());
            }
        }
        let mut result = best.into_iter().map(|(_, row)| row).collect::<Vec<_>>();
        sort_rows_newest(&mut result);
        result
    }

    /// Resolve a path or unambiguous id/prefix to `(kind, path)`.
    pub fn resolve_any(
        &self,
        input: impl AsRef<Path>,
    ) -> Result<(SessionSourceKind, PathBuf), CatalogError> {
        let input = input.as_ref();
        let candidate = expand_tilde(input.to_path_buf(), &self.user_home);
        let candidate = make_absolute(candidate);
        if candidate.is_file() {
            for kind in SessionSourceKind::ALL {
                if self.is_safe_session_path(kind, &candidate)
                    || (kind.is_native() && candidate.extension() == Some(OsStr::new("jsonl")))
                {
                    // Prefer exact root membership; fall back to native jsonl for ad-hoc paths.
                    if self.is_safe_session_path(kind, &candidate) {
                        return Ok((kind, candidate));
                    }
                }
            }
            // Direct native Pi file outside the configured tree still resumes natively.
            if candidate.extension() == Some(OsStr::new("jsonl")) {
                return Ok((SessionSourceKind::NativePi, candidate));
            }
            return Err(CatalogError::SessionNotFound(candidate.display().to_string()));
        }

        let input_text = input
            .to_str()
            .map(str::to_owned)
            .unwrap_or_else(|| input.to_string_lossy().into_owned());
        let mut matches = Vec::with_capacity(SESSION_CATALOG_ROW_LIMIT);
        for kind in SessionSourceKind::ALL {
            for path in self.matching_paths(kind, &input_text) {
                if matches.len() == SESSION_CATALOG_ROW_LIMIT {
                    break;
                }
                matches.push((kind, path));
            }
        }
        match matches.as_slice() {
            [] => Err(CatalogError::SessionNotFound(input_text)),
            [(kind, path)] => Ok((*kind, path.clone())),
            _ => Err(CatalogError::AmbiguousSession {
                input: input_text,
                matches,
            }),
        }
    }

    /// Resolve within one forced source kind.
    pub fn resolve_for(
        &self,
        kind: SessionSourceKind,
        input: impl AsRef<Path>,
    ) -> Result<PathBuf, CatalogError> {
        let input = input.as_ref();
        let candidate = make_absolute(expand_tilde(input.to_path_buf(), &self.user_home));
        if candidate.is_file() {
            if self.is_safe_session_path(kind, &candidate)
                || (kind.is_native() && candidate.extension() == Some(OsStr::new("jsonl")))
            {
                return Ok(candidate);
            }
            return Err(CatalogError::SessionNotFound(candidate.display().to_string()));
        }
        let input_text = input
            .to_str()
            .map(str::to_owned)
            .unwrap_or_else(|| input.to_string_lossy().into_owned());
        let matches = self.matching_paths(kind, &input_text);
        match matches.as_slice() {
            [] => Err(CatalogError::SessionNotFound(input_text)),
            [path] => Ok(path.clone()),
            _ => Err(CatalogError::AmbiguousSession {
                input: input_text,
                matches: matches
                    .into_iter()
                    .map(|path| (kind, path))
                    .collect(),
            }),
        }
    }

    /// Resolve and securely prepare a native session for resume while retaining
    /// the same opened inode for parse and append.
    pub fn prepare_native_resume(
        &self,
        input: impl AsRef<Path>,
    ) -> Result<PreparedSessionResume, CatalogError> {
        let path = self.resolve_for(SessionSourceKind::NativePi, input)?;
        let root = self.root_for(SessionSourceKind::NativePi).path;
        if path_lexically_under_root(&path, &root) {
            PreparedSessionResume::prepare_under_root(&root, &path).map_err(|error| {
                CatalogError::Import(ImportSessionError::InvalidInput {
                    format: SourceSessionFormat::Pi,
                    path,
                    reason: format!("{error:#}"),
                })
            })
        } else {
            PreparedSessionResume::prepare_path(&path).map_err(|error| {
                CatalogError::Import(ImportSessionError::InvalidInput {
                    format: SourceSessionFormat::Pi,
                    path,
                    reason: format!("{error:#}"),
                })
            })
        }
    }

    /// Native resume or idempotent foreign import.
    ///
    /// - Native Pi paths return the existing file (no copy).
    /// - Foreign sources reuse a prior conversion when lineage matches.
    /// - Otherwise a new Pi v3 session is emitted with durable lineage.
    pub fn import_or_resume(
        &self,
        kind: SessionSourceKind,
        input: impl AsRef<Path>,
        preferred_cwd: Option<&Path>,
    ) -> Result<ResolvedSession, CatalogError> {
        let source_path = self.resolve_for(kind, input.as_ref())?;
        if kind.is_native() {
            return self.resume_native(&source_path);
        }
        let format = kind
            .to_import_format()
            .expect("foreign kind maps to import format");
        let root = self.root_for(kind).path;
        let opened = open_source_under_root(format, &root, &source_path)?;
        let metadata = opened.metadata().clone();
        let parsed = parse_opened_source_public(format, opened)?;
        let source_session_id = parsed
            .source_session_id
            .clone()
            .filter(|value| !value.is_empty())
            .unwrap_or_default();
        let lineage = ImportLineageKey {
            source: kind,
            source_session_id: source_session_id.clone(),
            source_path_fingerprint: make_absolute(source_path.clone())
                .to_string_lossy()
                .into_owned(),
            content_fingerprint: Some(content_fingerprint(&metadata)),
        };
        if let Some((native_id, native_path)) = self.find_existing_import(&lineage) {
            return Ok(existing_import_result(
                kind,
                source_path,
                source_session_id,
                parsed,
                native_id,
                native_path,
                preferred_cwd,
            ));
        }

        let cwd = if parsed.cwd.as_os_str().is_empty() {
            preferred_cwd
                .map(|path| path.to_path_buf())
                .or_else(|| env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from("."))
        } else {
            parsed.cwd.clone()
        };
        let output_dir = if let Some(root) = &self.native_session_root {
            root.clone()
        } else if self.native_agent_dir.as_os_str().is_empty() {
            default_session_dir(&cwd)
        } else {
            // Honor hermetic native agent dir in tests / injected catalogs.
            let encoded = {
                let absolute = make_absolute(cwd.clone());
                let mut encoded = absolute.to_string_lossy().into_owned();
                if encoded.starts_with('/') || encoded.starts_with('\\') {
                    encoded.remove(0);
                }
                encoded.replace(['/', '\\', ':'], "-")
            };
            self.native_agent_dir
                .join("sessions")
                .join(format!("--{encoded}--"))
        };
        fs::create_dir_all(&output_dir).map_err(|source| CatalogError::Io {
            path: output_dir.clone(),
            source,
        })?;
        let native_id = deterministic_import_id(&lineage);
        let output = output_dir.join(format!("import_{native_id}.jsonl"));
        let import_lineage = ImportLineage {
            source: format,
            source_session_id: lineage.source_session_id.clone(),
            source_path: source_path.clone(),
            content_fingerprint: lineage.content_fingerprint.clone(),
            imported_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        };
        match emit_parsed_session_at_with_lineage(
            format,
            source_path.clone(),
            parsed.clone(),
            make_absolute(cwd),
            native_id.clone(),
            output.clone(),
            &import_lineage,
        ) {
            Ok(imported) => Ok(ResolvedSession::from(imported)),
            Err(ImportSessionError::Io { source, .. })
                if source.kind() == ErrorKind::AlreadyExists =>
            {
                let opened = open_source_under_root(
                    SourceSessionFormat::Pi,
                    &self.root_for(SessionSourceKind::NativePi).path,
                    &output,
                )?;
                let Some((existing, existing_id)) =
                    read_native_lineage(opened.into_primary(), &output)
                else {
                    return Err(CatalogError::Import(ImportSessionError::InvalidInput {
                        format: SourceSessionFormat::Pi,
                        path: output,
                        reason: "concurrent import result has no valid lineage".to_owned(),
                    }));
                };
                if existing.identity_key() != lineage.identity_key() || existing_id != native_id {
                    return Err(CatalogError::Import(ImportSessionError::InvalidInput {
                        format: SourceSessionFormat::Pi,
                        path: output,
                        reason: "concurrent import result lineage does not match the source"
                            .to_owned(),
                    }));
                }
                Ok(existing_import_result(
                    kind,
                    source_path,
                    source_session_id,
                    parsed,
                    existing_id,
                    output,
                    preferred_cwd,
                ))
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Convenience: resolve kind automatically then import-or-resume.
    pub fn import_or_resume_any(
        &self,
        input: impl AsRef<Path>,
        preferred_cwd: Option<&Path>,
    ) -> Result<ResolvedSession, CatalogError> {
        let (kind, path) = self.resolve_any(input.as_ref())?;
        self.import_or_resume(kind, path, preferred_cwd)
    }

    /// Build lineage index from native Pi sessions already on disk.
    #[must_use]
    pub fn build_lineage_index(&self) -> HashMap<(SessionSourceKind, String), (String, PathBuf)> {
        let mut index = HashMap::new();
        let root = self.root_for(SessionSourceKind::NativePi).path;
        for path in self.discover(SessionSourceKind::NativePi) {
            let Ok(opened) = open_source_under_root(SourceSessionFormat::Pi, &root, &path) else {
                continue;
            };
            if let Some((lineage, native_id)) =
                read_native_lineage(opened.into_primary(), &path)
            {
                index.insert(lineage.identity_key(), (native_id, path));
            }
        }
        index
    }

    fn find_existing_import(&self, lineage: &ImportLineageKey) -> Option<(String, PathBuf)> {
        let index = self.build_lineage_index();
        if let Some(hit) = index.get(&lineage.identity_key()) {
            return Some(hit.clone());
        }
        // Fallback: same source path fingerprint under any id key.
        index.into_iter().find_map(|((source, _), value)| {
            if source != lineage.source {
                return None;
            }
            let path = &value.1;
            let Ok(opened) = open_source_under_root(
                SourceSessionFormat::Pi,
                &self.root_for(SessionSourceKind::NativePi).path,
                path,
            ) else {
                return None;
            };
            let Some((existing, _)) = read_native_lineage(opened.into_primary(), path) else {
                return None;
            };
            if existing.source_path_fingerprint == lineage.source_path_fingerprint {
                Some(value)
            } else {
                None
            }
        })
    }

    fn resume_native(&self, path: &Path) -> Result<ResolvedSession, CatalogError> {
        let root = self.root_for(SessionSourceKind::NativePi).path;
        let opened = if path_lexically_under_root(path, &root) {
            open_source_under_root(SourceSessionFormat::Pi, &root, path)?
        } else {
            open_source_direct(SourceSessionFormat::Pi, path)?
        };
        let header = read_native_header_lite(opened.into_primary(), path).map_err(|reason| {
            CatalogError::Import(ImportSessionError::InvalidInput {
                format: SourceSessionFormat::Pi,
                path: path.to_path_buf(),
                reason,
            })
        })?;
        Ok(ResolvedSession {
            kind: SessionSourceKind::NativePi,
            source_path: path.to_path_buf(),
            source_session_id: Some(header.id.clone()),
            id: header.id,
            path: path.to_path_buf(),
            cwd: header.cwd,
            messages: Vec::new(),
            reused_existing: true,
            native_no_copy: true,
        })
    }

    fn finish_rows(
        &self,
        rows: Vec<CatalogRow>,
        include_foreign: bool,
        dedupe: bool,
        named_only: bool,
        cwd_scope: Option<&Path>,
    ) -> Vec<CatalogRow> {
        let mut rows = rows
            .into_iter()
            .filter(|row| include_foreign || row.kind.is_native())
            .filter(|row| {
                if !named_only {
                    return true;
                }
                row.name
                    .as_ref()
                    .is_some_and(|name| !name.trim().is_empty())
            })
            .filter(|row| match cwd_scope {
                None => true,
                Some(scope) => normalize_cwd(&row.cwd) == normalize_cwd(scope),
            })
            .collect::<Vec<_>>();
        if dedupe {
            rows = Self::dedupe_rows(&rows);
        } else {
            sort_rows_newest(&mut rows);
            rows.truncate(SESSION_CATALOG_ROW_LIMIT);
        }
        rows
    }

    fn matching_paths(&self, kind: SessionSourceKind, input: &str) -> Vec<PathBuf> {
        let mut matches = Vec::with_capacity(SESSION_CATALOG_CANDIDATE_LIMIT);
        for path in self.discover(kind) {
            let file_stem = path.file_stem().and_then(OsStr::to_str).unwrap_or("");
            let stem_without_rollout = file_stem.strip_prefix("rollout-").unwrap_or(file_stem);
            let directory_id = path
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .unwrap_or("");
            let session_id = match kind {
                SessionSourceKind::NativePi => open_source_under_root(
                    SourceSessionFormat::Pi,
                    &self.root_for(kind).path,
                    &path,
                )
                .ok()
                .and_then(|opened| read_native_header_lite(opened.into_primary(), &path).ok())
                .map(|header| header.id),
                _ => kind.to_import_format().and_then(|format| {
                    source_id_under_root_public(format, &self.root_for(kind).path, &path)
                        .ok()
                        .flatten()
                }),
            }
            .unwrap_or_default();
            if session_id.is_empty()
                && file_stem != input
                && stem_without_rollout != input
                && directory_id != input
            {
                // Still allow prefix matches on names when id parse fails.
            }
            let id_matches = !session_id.is_empty()
                && (session_id == input || session_id.starts_with(input));
            let name_matches = file_stem == input
                || file_stem.starts_with(input)
                || file_stem.contains(input)
                || stem_without_rollout == input
                || stem_without_rollout.starts_with(input)
                || stem_without_rollout.contains(input)
                || stem_without_rollout.ends_with(input)
                || directory_id == input
                || directory_id.starts_with(input)
                || directory_id.contains(input);
            if id_matches || name_matches {
                matches.push(path);
            }
        }
        matches.sort();
        matches.dedup();
        matches
    }

    fn row_from_path(
        &self,
        kind: SessionSourceKind,
        path: &Path,
        lineage_index: &HashMap<(SessionSourceKind, String), (String, PathBuf)>,
    ) -> Result<Option<CatalogRow>, CatalogError> {
        let metadata = fs::symlink_metadata(path).map_err(|source| CatalogError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            return Ok(None);
        }
        let modified_epoch = metadata_epoch(&metadata);
        let size = metadata.len();

        if kind.is_native() {
            let root = self.root_for(kind).path;
            let info = match open_source_under_root(SourceSessionFormat::Pi, &root, path)
                .map(|opened| opened.into_primary())
                .map_err(CatalogError::from)
                .and_then(|file| {
                    read_native_list_info(file, path).map_err(|reason| {
                        CatalogError::Import(ImportSessionError::InvalidInput {
                            format: SourceSessionFormat::Pi,
                            path: path.to_path_buf(),
                            reason,
                        })
                    })
                })
            {
                Ok(info) => info,
                Err(_) => return Ok(None),
            };
            if info.id.is_empty() {
                return Ok(None);
            }
            let summary = info
                .name
                .clone()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| truncate_summary(&info.first_message));
            let search_text = format!(
                "{} {} {} {} {} {}",
                kind.label(),
                info.id,
                info.name.as_deref().unwrap_or_default(),
                info.first_message,
                info.cwd.display(),
                path.display()
            );
            return Ok(Some(CatalogRow {
                kind,
                session_id: info.id,
                summary,
                cwd: info.cwd,
                modified_epoch,
                display_time: format_epoch(modified_epoch),
                path: path.to_path_buf(),
                size,
                message_count: Some(info.message_count),
                name: info.name,
                status: CatalogRowStatus::Native,
                import_lineage: info.lineage,
                search_text,
                message_blob: info.first_message.clone(),
            }));
        }

        let format = kind
            .to_import_format()
            .expect("foreign kind maps to import format");
        let root = self.root_for(kind).path;
        // Open through the configured root capability once and reuse the
        // secured descriptors for both aggregate size and parsing. This avoids
        // ambient reopen and confines companion reads (e.g. Grok chat_history)
        // to the already-validated no-follow capability handles.
        let opened = match open_source_under_root(format, &root, path) {
            Ok(opened) => opened,
            Err(_) => return Ok(None),
        };
        let aggregate_size = opened.aggregate_size();
        let parsed = match parse_opened_source_public(format, opened) {
            Ok(parsed) => parsed,
            Err(_) => return Ok(None),
        };
        let session_id = parsed
            .source_session_id
            .clone()
            .filter(|value| !value.is_empty())
            .or_else(|| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(|stem| stem.strip_prefix("rollout-").unwrap_or(stem).to_owned())
            })
            .unwrap_or_default();
        if session_id.is_empty() {
            return Ok(None);
        }
        let summary = parsed
            .messages
            .iter()
            .find(|message| matches!(message.role, crate::import::ImportedMessageRole::User))
            .or_else(|| parsed.messages.first())
            .map(|message| truncate_summary(&message.text))
            .unwrap_or_else(|| "(no messages)".to_owned());
        let lineage = ImportLineageKey {
            source: kind,
            source_session_id: session_id.clone(),
            source_path_fingerprint: canonical_fingerprint(path),
            content_fingerprint: Some(content_fingerprint(&metadata)),
        };
        let status = lineage_index
            .get(&lineage.identity_key())
            .cloned()
            .map(|(native_id, native_path)| CatalogRowStatus::AlreadyImported {
                native_id,
                native_path,
            })
            .unwrap_or(CatalogRowStatus::Foreign);
        let message_blob = parsed
            .messages
            .iter()
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let search_text = format!(
            "{} {} {} {} {} {}",
            kind.label(),
            session_id,
            summary,
            message_blob,
            parsed.cwd.display(),
            path.display()
        );
        Ok(Some(CatalogRow {
            kind,
            session_id,
            summary,
            size: aggregate_size,
            message_count: Some(parsed.meaningful_count),
            cwd: parsed.cwd,
            modified_epoch,
            display_time: format_epoch(modified_epoch),
            path: path.to_path_buf(),
            name: None,
            status,
            import_lineage: Some(lineage),
            search_text,
            message_blob,
        }))
    }
}

fn merge_source_rows(sources: Vec<Vec<CatalogRow>>) -> Vec<CatalogRow> {
    let reserved_count = sources.iter().filter(|source| !source.is_empty()).count();
    let remainder_limit = SESSION_CATALOG_ROW_LIMIT.saturating_sub(reserved_count);
    let mut rows = Vec::with_capacity(SESSION_CATALOG_ROW_LIMIT);
    let mut remainder = Vec::with_capacity(remainder_limit);
    for source in sources {
        let mut source = source.into_iter();
        if let Some(row) = source.next() {
            rows.push(row);
        }
        for row in source {
            retain_newest_row(&mut remainder, row, remainder_limit);
        }
    }
    rows.extend(remainder);
    sort_rows_newest(&mut rows);
    rows
}

fn retain_newest_row(rows: &mut Vec<CatalogRow>, row: CatalogRow, limit: usize) {
    if rows.len() < limit {
        rows.push(row);
        return;
    }
    let Some(oldest) = rows
        .iter()
        .enumerate()
        .max_by(|(_, left), (_, right)| compare_rows_newest(left, right))
        .map(|(index, _)| index)
    else {
        return;
    };
    if compare_rows_newest(&row, &rows[oldest]).is_lt() {
        rows[oldest] = row;
    }
}

fn deterministic_import_id(lineage: &ImportLineageKey) -> String {
    let (source, identity) = lineage.identity_key();
    let mut digest = Sha256::new();
    digest.update(b"pi-rs-session-import-v1\0");
    digest.update(source.as_str().as_bytes());
    digest.update(b"\0");
    digest.update(identity.as_bytes());
    let bytes = digest.finalize();
    let mut uuid_bytes = [0_u8; 16];
    uuid_bytes.copy_from_slice(&bytes[..16]);
    uuid_bytes[6] = (uuid_bytes[6] & 0x0f) | 0x50;
    uuid_bytes[8] = (uuid_bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(uuid_bytes).to_string()
}

fn existing_import_result(
    kind: SessionSourceKind,
    source_path: PathBuf,
    source_session_id: String,
    parsed: ParsedSessionPublic,
    native_id: String,
    native_path: PathBuf,
    preferred_cwd: Option<&Path>,
) -> ResolvedSession {
    let cwd = if parsed.cwd.as_os_str().is_empty() {
        preferred_cwd
            .map(|path| make_absolute(path.to_path_buf()))
            .unwrap_or_else(|| native_path.parent().unwrap_or(Path::new(".")).to_path_buf())
    } else {
        make_absolute(parsed.cwd)
    };
    ResolvedSession {
        kind,
        source_path,
        source_session_id: (!source_session_id.is_empty()).then_some(source_session_id),
        id: native_id,
        path: native_path,
        cwd,
        messages: parsed.messages,
        reused_existing: true,
        native_no_copy: false,
    }
}

/// Dedupe helper exported for callers that already hold rows.
#[must_use]
pub fn dedupe_catalog_rows(rows: &[CatalogRow]) -> Vec<CatalogRow> {
    SessionCatalog::dedupe_rows(rows)
}

