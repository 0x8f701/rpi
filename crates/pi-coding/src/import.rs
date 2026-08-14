//! Lossy import of external coding-agent sessions into native Pi v3 JSONL.
//!
//! Import intentionally preserves only the first non-empty text block from each
//! user or assistant message, plus valid RFC 3339 timestamps. Tool calls,
//! reasoning, attachments, usage, provider metadata, and inactive tree branches
//! are not portable across agents and are discarded.

mod parsers;

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions as CapOpenOptions};
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;
use walkdir::WalkDir;

use self::parsers::{ParsedSession, parse_source, source_id};
use crate::default_session_dir;

/// Per-file OMP source cap, re-exported so the catalog's chain probe rejects
/// oversize members before scanning their headers and tests can size fixtures.
pub(crate) use parsers::MAX_SOURCE_BYTES;
/// OMP header-probe budgets, re-exported for the catalog and regressions.
pub(crate) use parsers::{MAX_HEADER_SCAN_BYTES, MAX_HEADER_SCAN_RECORDS};

/// [`import_session_to`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceSessionFormat {
    Pi,
    Omp,
    Codex,
    Claude,
    Grok,
    Droid,
}

impl SourceSessionFormat {
    pub const ALL: [Self; 6] = [
        Self::Pi,
        Self::Omp,
        Self::Codex,
        Self::Claude,
        Self::Grok,
        Self::Droid,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pi => "pi",
            Self::Omp => "omp",
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Grok => "grok",
            Self::Droid => "droid",
        }
    }
}

impl fmt::Display for SourceSessionFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for SourceSessionFormat {
    type Err = ImportSessionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "pi" => Ok(Self::Pi),
            "omp" => Ok(Self::Omp),
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            "grok" => Ok(Self::Grok),
            "droid" => Ok(Self::Droid),
            _ => Err(ImportSessionError::UnsupportedFormat(value.to_owned())),
        }
    }
}

/// A portable message role. Other source roles are deliberately discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImportedMessageRole {
    User,
    Assistant,
}

impl ImportedMessageRole {
    const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// The normalized, intentionally lossy message representation used by import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedMessage {
    pub role: ImportedMessageRole,
    pub text: String,
    /// Original RFC 3339 timestamp when the source supplied one.
    pub timestamp: Option<String>,
}

/// Result of importing and emitting one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedSession {
    pub source: SourceSessionFormat,
    pub source_path: PathBuf,
    pub source_session_id: Option<String>,
    /// Newly generated native Pi session id (or reused id when idempotent).
    pub id: String,
    pub path: PathBuf,
    pub cwd: PathBuf,
    pub messages: Vec<ImportedMessage>,
    /// True when an existing native conversion was returned without rewriting.
    pub reused_existing: bool,
}

/// Durable foreign-source lineage stamped into emitted Pi sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportLineage {
    pub source: SourceSessionFormat,
    pub source_session_id: String,
    pub source_path: PathBuf,
    pub content_fingerprint: Option<String>,
    pub imported_at: String,
}

impl ImportLineage {
    #[must_use]
    pub fn parent_session_value(&self) -> String {
        if self.source_session_id.is_empty() {
            format!(
                "{}:{}",
                self.source.as_str(),
                self.source_path.to_string_lossy()
            )
        } else {
            format!("{}:{}", self.source.as_str(), self.source_session_id)
        }
    }
}

#[derive(Debug, Error)]
pub enum ImportSessionError {
    #[error("unsupported source session format: {0}")]
    UnsupportedFormat(String),
    #[error("cannot determine the user's home directory")]
    HomeDirectoryUnavailable,
    #[error("{format} session not found: {input}")]
    SessionNotFound {
        format: SourceSessionFormat,
        input: String,
    },
    #[error("{format} session id is ambiguous: {input} ({matches:?})")]
    AmbiguousSession {
        format: SourceSessionFormat,
        input: String,
        matches: Vec<PathBuf>,
    },
    #[error("invalid {format} session input {path}: {reason}")]
    InvalidInput {
        format: SourceSessionFormat,
        path: PathBuf,
        reason: String,
    },
    #[error("invalid output path {path}: {reason}")]
    InvalidOutput { path: PathBuf, reason: String },
    #[error("invalid native {format} session header in {path}")]
    InvalidNativeHeader {
        format: SourceSessionFormat,
        path: PathBuf,
    },
    #[error("{format} session contains no convertible user/assistant text messages: {path}")]
    NoConvertibleMessages {
        format: SourceSessionFormat,
        path: PathBuf,
    },
    #[error("source session exceeds import limits at {path}: {reason}")]
    ResourceLimit { path: PathBuf, reason: String },
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid JSON in {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// Open source files held by descriptor so parsers never reopen ambient paths.
#[derive(Debug)]
pub(crate) struct OpenedSource {
    path: PathBuf,
    primary: fs::File,
    metadata: fs::Metadata,
    grok_chat: Option<fs::File>,
    grok_cwd: Option<fs::File>,
}

impl OpenedSource {
    #[must_use]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub(crate) fn metadata(&self) -> &fs::Metadata {
        &self.metadata
    }

    fn into_parts(self) -> OpenedSourceParts {
        OpenedSourceParts {
            path: self.path,
            primary: self.primary,
            grok_chat: self.grok_chat,
            grok_cwd: self.grok_cwd,
        }
    }

    pub(crate) fn into_primary(self) -> fs::File {
        self.primary
    }

    /// Borrowed primary handle for bounded header reads that must not consume
    /// the descriptor (e.g. OMP `parentSession` chain resolution).
    pub(super) fn primary_ref(&self) -> &fs::File {
        &self.primary
    }

    /// Aggregate persisted size of the opened session and its companions, read
    /// from the already securely-opened descriptors (no ambient reopen, no
    /// symlink escape). For multi-file sources such as Grok this is the sum of
    /// the primary (`summary.json`) and the chat companion (`chat_history.jsonl`);
    /// for single-file sources it is the primary size. Companion regularity is
    /// verified at secure-open time, so a missing companion contributes zero.
    #[must_use]
    pub(crate) fn aggregate_size(&self) -> u64 {
        let mut total = self.metadata.len();
        if let Some(chat) = &self.grok_chat {
            if let Ok(metadata) = chat.metadata() {
                total = total.saturating_add(metadata.len());
            }
        }
        total
    }
}

#[derive(Debug)]
pub(super) struct OpenedSourceParts {
    pub(super) path: PathBuf,
    pub(super) primary: fs::File,
    pub(super) grok_chat: Option<fs::File>,
    pub(super) grok_cwd: Option<fs::File>,
}

/// Open one recognized session relative to its configured root. Ambient
/// authority establishes the root capability only; all source and companion
/// opens are relative, escape-confined, and final-component no-follow.
pub(crate) fn open_source_under_root(
    source: SourceSessionFormat,
    root: &Path,
    path: &Path,
) -> Result<OpenedSource, ImportSessionError> {
    open_source_under_root_inner(source, root, path, true, false)
}

/// Securely open a native session for retained read/append ownership under a
/// configured root. The final component is never followed.
pub(crate) fn open_native_session_for_append_under_root(
    root: &Path,
    path: &Path,
) -> Result<OpenedSource, ImportSessionError> {
    open_source_under_root_inner(SourceSessionFormat::Pi, root, path, true, true)
}

fn open_source_under_absolute_root(
    source: SourceSessionFormat,
    root: &Path,
    path: &Path,
    append: bool,
) -> Result<OpenedSource, ImportSessionError> {
    let relative = path.strip_prefix(&root).map_err(|_| {
        invalid_source_open(
            source,
            &path,
            "path is outside the configured source root".to_owned(),
        )
    })?;
    if relative.as_os_str().is_empty()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(invalid_source_open(
            source,
            &path,
            "path is not a normal relative source path".to_owned(),
        ));
    }

    let directory = Dir::open_ambient_dir(&root, cap_std::ambient_authority()).map_err(|error| {
        invalid_source_open(
            source,
            &path,
            format!("cannot open configured source root {}: {error}", root.display()),
        )
    })?;
    let primary = open_capability_file(&directory, relative, source, &path, append)?;
    let metadata = primary.metadata().map_err(|source_error| ImportSessionError::Io {
        path: path.to_path_buf(),
        source: source_error,
    })?;
    if !metadata.is_file() {
        return Err(invalid_source_open(
            source,
            &path,
            "source is not a regular file".to_owned(),
        ));
    }

    let (grok_chat, grok_cwd) = if source == SourceSessionFormat::Grok {
        let chat_relative = relative.with_file_name("chat_history.jsonl");
        let chat_path = root.join(&chat_relative);
        let chat = open_optional_capability_file(&directory, &chat_relative, source, &chat_path)?;
        let cwd_relative = relative
            .parent()
            .and_then(Path::parent)
            .map(|parent| parent.join(".cwd"));
        let cwd = match cwd_relative {
            Some(relative) => {
                let cwd_path = root.join(&relative);
                open_optional_capability_file(&directory, &relative, source, &cwd_path)?
            }
            None => None,
        };
        (chat, cwd)
    } else {
        (None, None)
    };

    Ok(OpenedSource {
        path: path.to_path_buf(),
        primary,
        metadata,
        grok_chat,
        grok_cwd,
    })
}

pub(crate) fn open_source_direct(
    source: SourceSessionFormat,
    path: &Path,
) -> Result<OpenedSource, ImportSessionError> {
    open_source_direct_inner(source, path, false)
}

/// Securely open an explicitly authorized native session for retained
/// read/append ownership relative to its parent directory capability.
pub(crate) fn open_native_session_for_append_direct(
    path: &Path,
) -> Result<OpenedSource, ImportSessionError> {
    open_source_direct_inner(SourceSessionFormat::Pi, path, true)
}

fn open_source_direct_inner(
    source: SourceSessionFormat,
    path: &Path,
    append: bool,
) -> Result<OpenedSource, ImportSessionError> {
    let path = absolute_path(path)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            invalid_source_open(source, &path, "source path has no parent directory".to_owned())
        })?;
    let root = if source == SourceSessionFormat::Grok {
        parent.parent().unwrap_or(parent)
    } else {
        parent
    };
    open_source_under_root_inner(source, root, &path, false, append)
}

fn open_source_under_root_inner(
    source: SourceSessionFormat,
    root: &Path,
    path: &Path,
    require_candidate: bool,
    append: bool,
) -> Result<OpenedSource, ImportSessionError> {
    if require_candidate && !is_source_candidate(source, path) {
        return Err(invalid_source_open(
            source,
            path,
            "path is not a recognized source session".to_owned(),
        ));
    }
    let root = absolute_path(root)?;
    let path = absolute_path(path)?;
    open_source_under_absolute_root(source, &root, &path, append)
}

fn open_capability_file(
    directory: &Dir,
    relative: &Path,
    source: SourceSessionFormat,
    display_path: &Path,
    append: bool,
) -> Result<fs::File, ImportSessionError> {
    let mut options = CapOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    if append {
        options.append(true);
    }
    directory
        .open_with(relative, &options)
        .map(cap_std::fs::File::into_std)
        .map_err(|error| {
            invalid_source_open(
                source,
                display_path,
                format!("secure no-follow open under the configured root failed: {error}"),
            )
        })
}

fn open_optional_capability_file(
    directory: &Dir,
    relative: &Path,
    source: SourceSessionFormat,
    display_path: &Path,
) -> Result<Option<fs::File>, ImportSessionError> {
    let mut options = CapOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    match directory.open_with(relative, &options) {
        Ok(file) => {
            let file = file.into_std();
            let metadata = file.metadata().map_err(|source_error| ImportSessionError::Io {
                path: display_path.to_path_buf(),
                source: source_error,
            })?;
            if !metadata.is_file() {
                return Err(invalid_source_open(
                    source,
                    display_path,
                    "companion is not a regular file".to_owned(),
                ));
            }
            Ok(Some(file))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(invalid_source_open(
            source,
            display_path,
            format!("secure no-follow companion open failed: {error}"),
        )),
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, ImportSessionError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|source| ImportSessionError::Io {
                path: path.to_path_buf(),
                source,
            })
    }
}

fn invalid_source_open(
    source: SourceSessionFormat,
    path: &Path,
    reason: String,
) -> ImportSessionError {
    ImportSessionError::InvalidInput {
        format: source,
        path: path.to_path_buf(),
        reason,
    }
}

/// Resolve `input` as either a direct path or a source-specific session id,
/// convert it, and write a new Pi v3 session beneath the default per-cwd
/// session directory.
pub fn import_session(
    source: SourceSessionFormat,
    input: impl AsRef<Path>,
) -> Result<ImportedSession, ImportSessionError> {
    import_session_with_lineage(source, input, None)
}

/// Like [`import_session`] but stamps optional durable lineage metadata.
pub fn import_session_with_lineage(
    source: SourceSessionFormat,
    input: impl AsRef<Path>,
    lineage: Option<&ImportLineage>,
) -> Result<ImportedSession, ImportSessionError> {
    let source_path = resolve_input(source, input.as_ref())?;
    let parsed = parse_source(source, &source_path)?;
    ensure_messages(source, &source_path, &parsed)?;
    let cwd = usable_cwd(&parsed.cwd)?;
    let id = Uuid::now_v7().to_string();
    let start = session_start(&parsed);
    let output = default_session_dir(&cwd).join(session_filename(start, &id));
    let lineage = lineage.cloned().unwrap_or_else(|| default_lineage(source, &source_path, &parsed));
    emit(source, source_path, parsed, cwd, id, output, Some(&lineage))
}

/// Resolve `input`, convert it, and write a new Pi v3 session to `output`.
/// If `output` is an existing directory, the normal Pi session filename is
/// created inside it. Existing files are never overwritten.
pub fn import_session_to(
    source: SourceSessionFormat,
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
) -> Result<ImportedSession, ImportSessionError> {
    import_session_to_with_lineage_inner(source, input, output, None)
}

/// Import into `output` while stamping durable lineage metadata.
pub fn import_session_to_with_lineage(
    source: SourceSessionFormat,
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    lineage: &ImportLineage,
) -> Result<ImportedSession, ImportSessionError> {
    import_session_to_with_lineage_inner(source, input, output, Some(lineage))
}

fn import_session_to_with_lineage_inner(
    source: SourceSessionFormat,
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    lineage: Option<&ImportLineage>,
) -> Result<ImportedSession, ImportSessionError> {
    let source_path = if input.as_ref().is_file() {
        normalize_direct_path(source, input.as_ref().to_path_buf())?
    } else {
        resolve_input(source, input.as_ref())?
    };
    let parsed = parse_source(source, &source_path)?;
    ensure_messages(source, &source_path, &parsed)?;
    let cwd = usable_cwd(&parsed.cwd)?;
    let id = Uuid::now_v7().to_string();
    let start = session_start(&parsed);
    let requested = output.as_ref();
    let output = if requested.is_dir() {
        requested.join(session_filename(start, &id))
    } else {
        requested.to_path_buf()
    };
    let lineage = lineage.cloned().unwrap_or_else(|| default_lineage(source, &source_path, &parsed));
    emit(source, source_path, parsed, cwd, id, output, Some(&lineage))
}

/// Public resolve helper for the unified session catalog.
pub fn resolve_input_public(
    source: SourceSessionFormat,
    input: &Path,
) -> Result<PathBuf, ImportSessionError> {
    resolve_input(source, input)
}

/// Parse a source through the secure direct-path compatibility boundary.
pub fn parse_source_public(
    source: SourceSessionFormat,
    path: &Path,
) -> Result<ParsedSessionPublic, ImportSessionError> {
    parse_source(source, path).map(ParsedSessionPublic::from)
}

/// Parse an already secured source handle without reopening its path.
pub(crate) fn parse_opened_source_public(
    source: SourceSessionFormat,
    opened: OpenedSource,
) -> Result<ParsedSessionPublic, ImportSessionError> {
    parsers::parse_opened_source(source, opened).map(ParsedSessionPublic::from)
}

/// Single-file content-fingerprint convention for non-chain sources:
/// `{mtime_secs}:{size}` from a metadata handle. OMP rotation chains do NOT
/// use this — their freshness comes from the authoritative SHA-256 content
/// digest over accepted members' exact bytes computed by
/// [`parse_omp_chain_public`], so chain identity is content-based.
pub(crate) fn metadata_fingerprint(metadata: &fs::Metadata) -> String {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|value| value.as_secs())
        .unwrap_or(0);
    format!("{}:{}", modified, metadata.len())
}

/// One chain member's linkage + size, read through the configured root
/// capability from a single securely opened descriptor.
pub(crate) struct OmpChainMemberProbe {
    /// Raw `parentSession` header value (the prior rotated file reference).
    pub parent_session: Option<String>,
    /// Size of this member from the same opened descriptor.
    pub size: u64,
}

/// Probe one OMP chain member: reject descriptors beyond the per-file source
/// cap before any header read, then read its `parentSession` header reference
/// through the configured root capability. Non-OMP sources and files without
/// the field return `None` parent; malformed references and oversize or
/// noise-padded files fail closed with the source error.
pub(crate) fn omp_chain_member_probe_under_root_public(
    source: SourceSessionFormat,
    root: &Path,
    path: &Path,
) -> Result<OmpChainMemberProbe, ImportSessionError> {
    let opened = open_source_under_root(source, root, path)?;
    let size = opened.metadata().len();
    if size > MAX_SOURCE_BYTES {
        return Err(ImportSessionError::ResourceLimit {
            path: path.to_path_buf(),
            reason: format!("file is {size} bytes; maximum is {MAX_SOURCE_BYTES}"),
        });
    }
    let parent_session = parsers::source_parent_session_opened(source, &opened)?;
    Ok(OmpChainMemberProbe { parent_session, size })
}

/// Parse a complete OMP rotation chain (root → leaf order) into one logical
/// session. Every candidate file is opened through the configured root
/// capability once; the aggregate byte budget is revalidated against those
/// descriptors (newest prefix retained, leaf unconditional) and every file
/// stays subject to per-file size/line/record limits. Entries duplicated
/// across files are kept once. Returns the parsed session plus the SHA-256
/// chain content fingerprint over the accepted members' exact bytes (count-
/// prefixed, root → leaf), used for lineage reuse — the same authoritative
/// result the catalog row status relies on.
pub(crate) fn parse_omp_chain_public(
    root: &Path,
    paths: &[PathBuf],
    max_bytes: u64,
) -> Result<(ParsedSessionPublic, String), ImportSessionError> {
    parsers::parse_omp_chain(root, paths, max_bytes)
        .map(|(parsed, fingerprint)| (ParsedSessionPublic::from(parsed), fingerprint))
}

/// Lossy parse result shared with the session catalog (no private parser types).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSessionPublic {
    pub source_session_id: Option<String>,
    pub cwd: PathBuf,
    pub started_at: Option<String>,
    pub messages: Vec<ImportedMessage>,
    /// Meaningful user/assistant turn count, independent of the lossy text
    /// projection in `messages`. Image-only turns count here even though they
    /// are dropped from `messages`; empty pending assistant placeholders do not.
    pub meaningful_count: usize,
}

impl From<ParsedSession> for ParsedSessionPublic {
    fn from(parsed: ParsedSession) -> Self {
        Self {
            source_session_id: parsed.source_session_id,
            cwd: parsed.cwd,
            started_at: parsed.started_at,
            messages: parsed.messages,
            meaningful_count: parsed.meaningful_count,
        }
    }
}

impl From<ParsedSessionPublic> for ParsedSession {
    fn from(parsed: ParsedSessionPublic) -> Self {
        Self {
            source_session_id: parsed.source_session_id,
            cwd: parsed.cwd,
            started_at: parsed.started_at,
            messages: parsed.messages,
            meaningful_count: parsed.meaningful_count,
        }
    }
}

/// Public source-id helper for catalog resolution.
pub fn source_id_public(
    source: SourceSessionFormat,
    path: &Path,
) -> Result<Option<String>, ImportSessionError> {
    source_id(source, path)
}

/// Read an id through the catalog's configured root capability.
pub(crate) fn source_id_under_root_public(
    source: SourceSessionFormat,
    root: &Path,
    path: &Path,
) -> Result<Option<String>, ImportSessionError> {
    let opened = open_source_under_root(source, root, path)?;
    parsers::source_id_opened(source, opened)
}

/// Emit a caller-parsed session at a caller-reserved deterministic path.
pub(crate) fn emit_parsed_session_at_with_lineage(
    source: SourceSessionFormat,
    source_path: PathBuf,
    parsed: ParsedSessionPublic,
    cwd: PathBuf,
    id: String,
    output: PathBuf,
    lineage: &ImportLineage,
) -> Result<ImportedSession, ImportSessionError> {
    let parsed = ParsedSession::from(parsed);
    ensure_messages(source, &source_path, &parsed)?;
    emit(
        source,
        source_path,
        parsed,
        cwd,
        id,
        output,
        Some(lineage),
    )
}

/// Resolve a source root, optionally overriding Codex/Claude homes.
pub fn source_root_for(
    source: SourceSessionFormat,
    codex_home: Option<&Path>,
    claude_config_dir: Option<&Path>,
) -> Result<PathBuf, ImportSessionError> {
    match source {
        SourceSessionFormat::Codex => Ok(codex_home
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("CODEX_HOME").map(PathBuf::from))
            .unwrap_or(home_dir()?.join(".codex"))
            .join("sessions")),
        SourceSessionFormat::Claude => Ok(claude_config_dir
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from))
            .unwrap_or(home_dir()?.join(".claude"))
            .join("projects")),
        SourceSessionFormat::Pi => Ok(home_dir()?.join(".pi/agent/sessions")),
        SourceSessionFormat::Omp => Ok(home_dir()?.join(".omp/agent/sessions")),
        SourceSessionFormat::Grok => Ok(home_dir()?.join(".grok/sessions")),
        SourceSessionFormat::Droid => Ok(home_dir()?.join(".factory/sessions")),
    }
}

fn default_lineage(
    source: SourceSessionFormat,
    source_path: &Path,
    parsed: &ParsedSession,
) -> ImportLineage {
    ImportLineage {
        source,
        source_session_id: parsed
            .source_session_id
            .clone()
            .unwrap_or_default(),
        source_path: source_path.to_path_buf(),
        content_fingerprint: fs::metadata(source_path).ok().map(|metadata| {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|value| value.as_secs())
                .unwrap_or(0);
            format!("{}:{}", modified, metadata.len())
        }),
        imported_at: format_timestamp(Utc::now()),
    }
}

fn emit(
    source: SourceSessionFormat,
    source_path: PathBuf,
    parsed: ParsedSession,
    cwd: PathBuf,
    id: String,
    output: PathBuf,
    lineage: Option<&ImportLineage>,
) -> Result<ImportedSession, ImportSessionError> {
    let records = pi_records(source, &parsed, &cwd, &id, lineage);
    write_jsonl_new(&output, &records)?;
    Ok(ImportedSession {
        source,
        source_path,
        source_session_id: parsed.source_session_id,
        id,
        path: output,
        cwd,
        messages: parsed.messages,
        reused_existing: false,
    })
}

fn ensure_messages(
    source: SourceSessionFormat,
    path: &Path,
    parsed: &ParsedSession,
) -> Result<(), ImportSessionError> {
    if parsed.messages.is_empty() {
        return Err(ImportSessionError::NoConvertibleMessages {
            format: source,
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn usable_cwd(cwd: &Path) -> Result<PathBuf, ImportSessionError> {
    if cwd.as_os_str().is_empty() {
        return std::env::current_dir().map_err(|source| ImportSessionError::Io {
            path: PathBuf::from("."),
            source,
        });
    }
    if cwd.is_absolute() {
        Ok(cwd.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|current| current.join(cwd))
            .map_err(|source| ImportSessionError::Io {
                path: cwd.to_path_buf(),
                source,
            })
    }
}

fn pi_records(
    source: SourceSessionFormat,
    session: &ParsedSession,
    cwd: &Path,
    session_id: &str,
    lineage: Option<&ImportLineage>,
) -> Vec<Value> {
    let start = session_start(session);
    let header_timestamp = format_timestamp(start);
    let model_entry_id = short_id();
    let thinking_entry_id = short_id();
    let lineage_entry_id = short_id();
    let provider = "rpi-import";
    let model = format!("converted-from-{source}");
    let parent_session = lineage.map(ImportLineage::parent_session_value);
    let mut header = serde_json::Map::new();
    header.insert("type".to_owned(), json!("session"));
    header.insert("version".to_owned(), json!(3));
    header.insert("id".to_owned(), json!(session_id));
    header.insert("timestamp".to_owned(), json!(header_timestamp));
    header.insert("cwd".to_owned(), json!(cwd));
    if let Some(parent) = parent_session.clone() {
        header.insert("parentSession".to_owned(), json!(parent));
    }
    let mut records = vec![
        Value::Object(header),
        json!({
            "type": "model_change",
            "id": model_entry_id,
            "parentId": null,
            "timestamp": format_timestamp(start + TimeDelta::milliseconds(1)),
            "provider": provider,
            "modelId": model,
        }),
        json!({
            "type": "thinking_level_change",
            "id": thinking_entry_id,
            "parentId": model_entry_id,
            "timestamp": format_timestamp(start + TimeDelta::milliseconds(2)),
            "thinkingLevel": "off",
        }),
    ];

    let mut parent_id = thinking_entry_id;
    if let Some(lineage) = lineage {
        let mut data = serde_json::Map::new();
        data.insert("source".to_owned(), json!(source.as_str()));
        data.insert(
            "sourceSessionId".to_owned(),
            json!(lineage.source_session_id),
        );
        data.insert(
            "sourcePath".to_owned(),
            json!(lineage.source_path.to_string_lossy()),
        );
        data.insert("importedAt".to_owned(), json!(lineage.imported_at));
        if let Some(fingerprint) = &lineage.content_fingerprint {
            data.insert("contentFingerprint".to_owned(), json!(fingerprint));
        }
        if let Some(parent) = parent_session {
            data.insert("parentSession".to_owned(), json!(parent));
        }
        records.push(json!({
            "type": "custom",
            "id": lineage_entry_id,
            "parentId": parent_id,
            "timestamp": format_timestamp(start + TimeDelta::milliseconds(3)),
            "customType": "import_lineage",
            "data": Value::Object(data),
        }));
        parent_id = lineage_entry_id;
    }

    for (index, message) in session.messages.iter().enumerate() {
        let timestamp = message
            .timestamp
            .as_deref()
            .and_then(parse_timestamp)
            .unwrap_or_else(|| start + TimeDelta::seconds(index as i64 + 1));
        let timestamp_text = message
            .timestamp
            .as_deref()
            .filter(|value| parse_timestamp(value).is_some())
            .map_or_else(|| format_timestamp(timestamp), str::to_owned);
        let entry_id = short_id();
        let mut payload = serde_json::Map::new();
        payload.insert("role".to_owned(), json!(message.role.as_str()));
        payload.insert(
            "content".to_owned(),
            json!([{ "type": "text", "text": message.text }]),
        );
        payload.insert("timestamp".to_owned(), json!(timestamp.timestamp_millis()));
        payload.insert("usage".to_owned(), zero_usage());
        if message.role == ImportedMessageRole::Assistant {
            payload.insert("api".to_owned(), json!("openai-completions"));
            payload.insert("provider".to_owned(), json!(provider));
            payload.insert("model".to_owned(), json!(model));
            payload.insert("stopReason".to_owned(), json!("stop"));
            payload.insert("responseId".to_owned(), json!(format!("imported-{}", short_id())));
        }
        records.push(json!({
            "type": "message",
            "id": entry_id,
            "parentId": parent_id,
            "timestamp": timestamp_text,
            "message": Value::Object(payload),
        }));
        parent_id = entry_id;
    }
    records
}

fn zero_usage() -> Value {
    json!({
        "input": 0,
        "output": 0,
        "cacheRead": 0,
        "cacheWrite": 0,
        "totalTokens": 0,
        "cost": {
            "input": 0,
            "output": 0,
            "cacheRead": 0,
            "cacheWrite": 0,
            "total": 0,
        },
    })
}

fn short_id() -> String {
    Uuid::new_v4().simple().to_string()[..8].to_owned()
}

fn session_start(session: &ParsedSession) -> DateTime<Utc> {
    session
        .started_at
        .as_deref()
        .and_then(parse_timestamp)
        .or_else(|| {
            session
                .messages
                .iter()
                .find_map(|message| message.timestamp.as_deref().and_then(parse_timestamp))
        })
        .unwrap_or_else(Utc::now)
}

fn parse_timestamp(timestamp: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn format_timestamp(timestamp: DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn session_filename(timestamp: DateTime<Utc>, id: &str) -> String {
    format!(
        "{}_{}.jsonl",
        format_timestamp(timestamp).replace([':', '.'], "-"),
        id
    )
}

fn write_jsonl_new(path: &Path, records: &[Value]) -> Result<(), ImportSessionError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| ImportSessionError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let mut bytes = Vec::new();
    for record in records {
        serde_json::to_writer(&mut bytes, record).map_err(|source| ImportSessionError::Json {
            path: path.to_path_buf(),
            source,
        })?;
        bytes.push(b'\n');
    }

    let temporary = parent.join(format!(".pi-import-{}.tmp", Uuid::new_v4().simple()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|source| ImportSessionError::Io {
                path: temporary.clone(),
                source,
            })?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|source| ImportSessionError::Io {
                path: temporary.clone(),
                source,
            })?;
        drop(file);
        fs::hard_link(&temporary, path).map_err(|source| ImportSessionError::Io {
            path: path.to_path_buf(),
            source,
        })
    })();
    let _ = fs::remove_file(&temporary);
    result
}

fn resolve_input(
    source: SourceSessionFormat,
    input: &Path,
) -> Result<PathBuf, ImportSessionError> {
    let input = expand_tilde(input)?;
    if input.exists() {
        return normalize_direct_path(source, input);
    }

    let input_text = input
        .to_str()
        .map(str::to_owned)
        .unwrap_or_else(|| input.to_string_lossy().into_owned());
    let root = source_root(source)?;
    let mut matches = Vec::new();
    if root.is_dir() {
        for entry in WalkDir::new(&root).follow_links(false) {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_file() || !is_source_candidate(source, entry.path()) {
                continue;
            }
            let path = entry.path();
            let file_stem = path.file_stem().and_then(|stem| stem.to_str()).unwrap_or("");
            let directory_id = path
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .unwrap_or("");
            let parsed_id = source_id(source, path).ok().flatten();
            let id_matches = parsed_id.as_deref().is_some_and(|id| {
                id == input_text || id.starts_with(&input_text)
            });
            let name_matches = file_stem == input_text
                || file_stem.starts_with(&input_text)
                || directory_id == input_text
                || directory_id.starts_with(&input_text)
                || file_stem
                    .strip_prefix("rollout-")
                    .is_some_and(|stem| stem == input_text || stem.starts_with(&input_text));
            if id_matches || name_matches {
                matches.push(path.to_path_buf());
            }
        }
    }
    matches.sort();
    matches.dedup();
    match matches.as_slice() {
        [] => Err(ImportSessionError::SessionNotFound {
            format: source,
            input: input_text,
        }),
        [path] => Ok(path.clone()),
        _ => Err(ImportSessionError::AmbiguousSession {
            format: source,
            input: input_text,
            matches,
        }),
    }
}

fn expand_tilde(input: &Path) -> Result<PathBuf, ImportSessionError> {
    if input == Path::new("~") {
        return home_dir();
    }
    if let Ok(rest) = input.strip_prefix("~/") {
        return home_dir().map(|home| home.join(rest));
    }
    Ok(input.to_path_buf())
}

fn normalize_direct_path(
    source: SourceSessionFormat,
    input: PathBuf,
) -> Result<PathBuf, ImportSessionError> {
    if source == SourceSessionFormat::Grok {
        if input.is_dir() {
            let summary = input.join("summary.json");
            if summary.is_file() {
                return Ok(summary);
            }
        }
        if input.file_name().is_some_and(|name| name == "chat_history.jsonl") {
            let summary = input.with_file_name("summary.json");
            if summary.is_file() {
                return Ok(summary);
            }
        }
    }
    if input.is_file() && is_source_candidate(source, &input) {
        return Ok(input);
    }
    Err(ImportSessionError::InvalidInput {
        format: source,
        path: input,
        reason: "path is not a recognized source session".to_owned(),
    })
}

fn source_root(source: SourceSessionFormat) -> Result<PathBuf, ImportSessionError> {
    match source {
        SourceSessionFormat::Codex => Ok(std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or(home_dir()?.join(".codex"))
            .join("sessions")),
        SourceSessionFormat::Claude => Ok(std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or(home_dir()?.join(".claude"))
            .join("projects")),
        SourceSessionFormat::Pi => Ok(home_dir()?.join(".pi/agent/sessions")),
        SourceSessionFormat::Omp => Ok(home_dir()?.join(".omp/agent/sessions")),
        SourceSessionFormat::Grok => Ok(home_dir()?.join(".grok/sessions")),
        SourceSessionFormat::Droid => Ok(home_dir()?.join(".factory/sessions")),
    }
}

fn home_dir() -> Result<PathBuf, ImportSessionError> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or(ImportSessionError::HomeDirectoryUnavailable)
}

fn is_source_candidate(source: SourceSessionFormat, path: &Path) -> bool {
    match source {
        SourceSessionFormat::Grok => path.file_name().is_some_and(|name| name == "summary.json"),
        SourceSessionFormat::Codex => {
            path.extension().is_some_and(|extension| extension == "jsonl")
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("rollout-"))
        }
        SourceSessionFormat::Pi
        | SourceSessionFormat::Omp
        | SourceSessionFormat::Claude
        | SourceSessionFormat::Droid => {
            path.extension().is_some_and(|extension| extension == "jsonl")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    struct FixtureDir(PathBuf);

    impl FixtureDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("rpi-import-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).expect("create fixture directory");
            Self(path)
        }

        fn write(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.0.join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create fixture parent");
            }
            fs::write(&path, contents).expect("write fixture");
            path
        }
    }

    impl Drop for FixtureDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn parses_pi_and_omp_active_tree_fixtures() {
        let fixtures = FixtureDir::new();
        let body = concat!(
            "{\"type\":\"session\",\"version\":3,\"id\":\"native\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"<workspace>/project\"}\n",
            "{\"type\":\"message\",\"id\":\"u\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"question\"}]}}\n",
            "{\"type\":\"message\",\"id\":\"dead\",\"parentId\":\"u\",\"timestamp\":\"2026-01-01T00:00:02Z\",\"message\":{\"role\":\"assistant\",\"content\":\"dead branch\"}}\n",
            "{\"type\":\"message\",\"id\":\"live\",\"parentId\":\"u\",\"timestamp\":\"2026-01-01T00:00:03Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"answer\"}]}}\n",
        );
        let pi = fixtures.write("pi.jsonl", body);
        let parsed = parse_source(SourceSessionFormat::Pi, &pi).expect("parse Pi");
        assert_eq!(parsed.messages.len(), 2);
        assert_eq!(parsed.messages[0].text, "question");

        assert_eq!(parsed.messages[1].text, "answer");
        assert_eq!(parsed.messages[1].timestamp.as_deref(), Some("2026-01-01T00:00:03Z"));

        let omp = fixtures.write(
            "omp.jsonl",
            &format!(
                "{{\"type\":\"title\",\"v\":1,\"title\":\"Fixture\",\"updatedAt\":\"2026-01-01T00:00:00Z\",\"pad\":\" \"}}\n{body}"
            ),
        );
        let parsed = parse_source(SourceSessionFormat::Omp, &omp).expect("parse OMP");
        assert_eq!(parsed.messages.len(), 2);
        assert_eq!(parsed.messages[1].text, "answer");
    }

    #[test]
    fn native_compaction_drops_pre_compaction_messages() {
        let fixtures = FixtureDir::new();
        let path = fixtures.write(
            "compacted.jsonl",
            concat!(
                "{\"type\":\"session\",\"version\":3,\"id\":\"compact\",\"cwd\":\"<workspace>\"}\n",
                "{\"type\":\"message\",\"id\":\"old\",\"parentId\":null,\"message\":{\"role\":\"user\",\"content\":\"old context\"}}\n",
                "{\"type\":\"message\",\"id\":\"kept\",\"parentId\":\"old\",\"message\":{\"role\":\"assistant\",\"content\":\"kept context\"}}\n",
                "{\"type\":\"compaction\",\"id\":\"compact-entry\",\"parentId\":\"kept\",\"firstKeptEntryId\":\"kept\"}\n",
                "{\"type\":\"message\",\"id\":\"new\",\"parentId\":\"compact-entry\",\"message\":{\"role\":\"user\",\"content\":\"new context\"}}\n",
            ),
        );
        let parsed = parse_source(SourceSessionFormat::Pi, &path).expect("parse compacted Pi");
        assert_eq!(
            parsed
                .messages
                .iter()
                .map(|message| message.text.as_str())
                .collect::<Vec<_>>(),
            ["kept context", "new context"]
        );
    }

    #[test]
    fn parses_codex_rollout_fixture() {
        let fixtures = FixtureDir::new();
        let path = fixtures.write(
            "rollout-fixture.jsonl",
            concat!(
                "{\"timestamp\":\"2026-02-01T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"codex-id\",\"cwd\":\"<workspace>/codex\",\"timestamp\":\"2026-02-01T00:00:00Z\"}}\n",
                "{\"timestamp\":\"2026-02-01T00:00:01Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"build it\"}]}}\n",
                "{\"timestamp\":\"2026-02-01T00:00:02Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"name\":\"shell\"}}\n",
                "{\"timestamp\":\"2026-02-01T00:00:03Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}]}}\n",
            ),
        );
        let parsed = parse_source(SourceSessionFormat::Codex, &path).expect("parse Codex");
        assert_eq!(parsed.source_session_id.as_deref(), Some("codex-id"));
        assert_eq!(parsed.cwd, PathBuf::from("<workspace>/codex"));
        assert_eq!(parsed.messages.iter().map(|message| message.text.as_str()).collect::<Vec<_>>(), ["build it", "done"]);
    }

    #[test]
    fn parses_claude_project_fixture() {
        let fixtures = FixtureDir::new();
        let path = fixtures.write(
            "claude.jsonl",
            concat!(
                "{\"type\":\"user\",\"uuid\":\"u\",\"parentUuid\":null,\"isSidechain\":false,\"sessionId\":\"claude-id\",\"cwd\":\"<workspace>/claude\",\"timestamp\":\"2026-03-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n",
                "{\"type\":\"assistant\",\"uuid\":\"dead\",\"parentUuid\":\"u\",\"isSidechain\":true,\"timestamp\":\"2026-03-01T00:00:02Z\",\"message\":{\"role\":\"assistant\",\"content\":\"ignore\"}}\n",
                "{\"type\":\"assistant\",\"uuid\":\"a\",\"parentUuid\":\"u\",\"isSidechain\":false,\"timestamp\":\"2026-03-01T00:00:03Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n",
                "{\"type\":\"last-prompt\",\"leafUuid\":\"a\",\"sessionId\":\"claude-id\"}\n",
            ),
        );
        let parsed = parse_source(SourceSessionFormat::Claude, &path).expect("parse Claude");
        assert_eq!(parsed.source_session_id.as_deref(), Some("claude-id"));
        assert_eq!(parsed.messages.iter().map(|message| message.text.as_str()).collect::<Vec<_>>(), ["hello", "hi"]);
    }

    #[test]
    fn parses_grok_directory_fixture() {
        let fixtures = FixtureDir::new();
        let summary = fixtures.write(
            "grok/session/summary.json",
            "{\"info\":{\"id\":\"grok-id\",\"cwd\":\"<workspace>/grok\"},\"created_at\":\"2026-04-01T00:00:00Z\"}",
        );
        fixtures.write(
            "grok/session/chat_history.jsonl",
            concat!(
                "{\"type\":\"system\",\"content\":\"ignore\"}\n",
                "{\"type\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"ask\"}],\"timestamp\":\"2026-04-01T00:00:01Z\"}\n",
                "{\"role\":\"assistant\",\"content\":\"reply\",\"created_at\":\"2026-04-01T00:00:02Z\"}\n",
            ),
        );
        let parsed = parse_source(SourceSessionFormat::Grok, &summary).expect("parse Grok");
        assert_eq!(parsed.source_session_id.as_deref(), Some("grok-id"));
        assert_eq!(parsed.messages.iter().map(|message| message.text.as_str()).collect::<Vec<_>>(), ["ask", "reply"]);
    }

    #[test]
    fn parses_droid_fixture() {
        let fixtures = FixtureDir::new();
        let path = fixtures.write(
            "droid.jsonl",
            concat!(
                "{\"type\":\"session_start\",\"id\":\"droid-id\",\"cwd\":\"<workspace>/droid\"}\n",
                "{\"type\":\"message\",\"timestamp\":\"2026-05-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"ping\"}]}}\n",
                "{\"type\":\"message\",\"timestamp\":\"2026-05-01T00:00:02Z\",\"message\":{\"role\":\"assistant\",\"content\":\"pong\"}}\n",
            ),
        );
        let parsed = parse_source(SourceSessionFormat::Droid, &path).expect("parse Droid");
        assert_eq!(parsed.source_session_id.as_deref(), Some("droid-id"));
        assert_eq!(parsed.messages.iter().map(|message| message.text.as_str()).collect::<Vec<_>>(), ["ping", "pong"]);
    }

    #[test]
    fn codex_to_pi_emits_v3_tree_with_new_id() {
        let fixtures = FixtureDir::new();
        let source = fixtures.write(
            "rollout-source.jsonl",
            concat!(
                "{\"timestamp\":\"2026-06-01T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"old-id\",\"cwd\":\"<workspace>/codex\"}}\n",
                "{\"timestamp\":\"2026-06-01T00:00:01Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"question\"}]}}\n",
                "{\"timestamp\":\"2026-06-01T00:00:02Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"answer\"}]}}\n",
            ),
        );
        let output = fixtures.0.join("emitted.jsonl");
        let imported = import_session_to(SourceSessionFormat::Codex, &source, &output)
            .expect("import Codex");
        assert_ne!(imported.id, "old-id");
        assert_eq!(imported.path, output);
        let values = fs::read_to_string(&imported.path)
            .expect("read emitted")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("valid JSONL"))
            .collect::<Vec<_>>();
        assert_eq!(values[0]["type"], "session");
        assert_eq!(values[0]["version"], 3);
        assert_eq!(values[0]["id"], imported.id);
        assert_eq!(values[0]["parentSession"], "codex:old-id");
        assert_eq!(values[1]["type"], "model_change");
        assert_eq!(values[2]["type"], "thinking_level_change");
        assert_eq!(values[3]["type"], "custom");
        assert_eq!(values[3]["customType"], "import_lineage");
        assert_eq!(values[3]["data"]["source"], "codex");
        assert_eq!(values[3]["data"]["sourceSessionId"], "old-id");
        assert_eq!(values[4]["message"]["content"][0]["text"], "question");
        assert_eq!(values[4]["timestamp"], "2026-06-01T00:00:01Z");
        assert_eq!(values[4]["parentId"], values[3]["id"]);
        assert_eq!(values[5]["message"]["content"][0]["text"], "answer");
        assert_eq!(values[5]["parentId"], values[4]["id"]);
    }

    #[test]
    fn rejects_invalid_native_header_and_no_messages() {
        let fixtures = FixtureDir::new();
        let invalid = fixtures.write(
            "invalid.jsonl",
            "{\"type\":\"message\",\"id\":\"m\",\"message\":{\"role\":\"user\",\"content\":\"text\"}}\n",
        );
        assert!(matches!(
            parse_source(SourceSessionFormat::Pi, &invalid),
            Err(ImportSessionError::InvalidNativeHeader { .. })
        ));

        let empty = fixtures.write(
            "rollout-empty.jsonl",
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"empty\",\"cwd\":\"<workspace>\"}}\n",
        );
        let output = fixtures.0.join("must-not-exist.jsonl");
        assert!(matches!(
            import_session_to(SourceSessionFormat::Codex, &empty, &output),
            Err(ImportSessionError::NoConvertibleMessages { .. })
        ));
        assert!(!output.exists());
    }

    #[test]
    fn corrupt_pi_jsonl_fails_with_json_error_and_emits_nothing() {
        let fixtures = FixtureDir::new();
        let path = fixtures.write(
            "corrupt-pi.jsonl",
            concat!(
                "{\"type\":\"session\",\"version\":3,\"id\":\"native\",\"cwd\":\"<workspace>/project\"}\n",
                "{\"type\":\"message\",\"id\":\"u\",\"parentId\":null,\"message\":{\"role\":\"user\",\"content\":\"question\"}}\n",
                "{\"type\":\"message\",\"id\":\"a\",\"parentId\":\"u\",\"message\":{\"role\":\"assistant\",\"content\":\"answer\"}}\n",
                "this line is not JSON\n",
            ),
        );
        assert!(matches!(
            parse_source(SourceSessionFormat::Pi, &path),
            Err(ImportSessionError::Json { .. })
        ));

        let output = fixtures.0.join("must-not-exist.jsonl");
        assert!(matches!(
            import_session_to(SourceSessionFormat::Pi, &path, &output),
            Err(ImportSessionError::Json { .. })
        ));
        assert!(!output.exists());
    }

    #[test]
    fn corrupt_omp_jsonl_fails_with_json_error() {
        let fixtures = FixtureDir::new();
        let path = fixtures.write(
            "corrupt-omp.jsonl",
            concat!(
                "{\"type\":\"title\",\"v\":1,\"title\":\"Fixture\",\"updatedAt\":\"2026-01-01T00:00:00Z\",\"pad\":\" \"}\n",
                "{\"type\":\"session\",\"version\":3,\"id\":\"native\",\"cwd\":\"<workspace>/project\"}\n",
                "still not JSON\n",
            ),
        );
        assert!(matches!(
            parse_source(SourceSessionFormat::Omp, &path),
            Err(ImportSessionError::Json { .. })
        ));
    }

    #[test]
    fn corrupt_source_id_fails_with_json_error() {
        let fixtures = FixtureDir::new();
        let path = fixtures.write(
            "corrupt-source-id.jsonl",
            concat!("not JSON\n", "{\"type\":\"session\",\"id\":\"native\",\n"),
        );
        assert!(matches!(
            source_id(SourceSessionFormat::Pi, &path),
            Err(ImportSessionError::Json { .. })
        ));
        assert!(matches!(
            source_id(SourceSessionFormat::Codex, &path),
            Err(ImportSessionError::Json { .. })
        ));
    }

    #[test]
    fn corrupt_codex_jsonl_fails_with_json_error_and_emits_nothing() {
        let fixtures = FixtureDir::new();
        let path = fixtures.write(
            "rollout-corrupt.jsonl",
            concat!(
                "{\"timestamp\":\"2026-02-01T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"codex-id\",\"cwd\":\"<workspace>/codex\"}}\n",
                "{\"timestamp\":\"2026-02-01T00:00:01Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"build it\"}]}}\n",
                "{\"truncated json\n",
            ),
        );
        assert!(matches!(
            parse_source(SourceSessionFormat::Codex, &path),
            Err(ImportSessionError::Json { .. })
        ));

        let output = fixtures.0.join("must-not-exist.jsonl");
        assert!(matches!(
            import_session_to(SourceSessionFormat::Codex, &path, &output),
            Err(ImportSessionError::Json { .. })
        ));
        assert!(!output.exists());
    }

    #[test]
    fn blank_lines_and_non_object_records_are_still_tolerated() {
        let fixtures = FixtureDir::new();
        let body = concat!(
            "\n",
            "  \n",
            "{\"type\":\"session\",\"version\":3,\"id\":\"native\",\"cwd\":\"<workspace>/project\"}\n",
            "123\n",
            "\"scalar\"\n",
            "{\"type\":\"message\",\"id\":\"u\",\"parentId\":null,\"message\":{\"role\":\"user\",\"content\":\"question\"}}\n",
            "\n",
            "true\n",
            "{\"type\":\"message\",\"id\":\"a\",\"parentId\":\"u\",\"message\":{\"role\":\"assistant\",\"content\":\"answer\"}}\n",
            "   \n",
        );
        let pi = fixtures.write("blank-tolerance-pi.jsonl", body);
        let parsed = parse_source(SourceSessionFormat::Pi, &pi).expect("parse Pi with blanks");
        assert_eq!(
            parsed
                .messages
                .iter()
                .map(|message| message.text.as_str())
                .collect::<Vec<_>>(),
            ["question", "answer"]
        );

        let codex = fixtures.write(
            "rollout-blank-tolerance.jsonl",
            concat!(
                "\n",
                "{\"timestamp\":\"2026-02-01T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"codex-id\",\"cwd\":\"<workspace>/codex\"}}\n",
                "42\n",
                "{\"timestamp\":\"2026-02-01T00:00:01Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"build it\"}]}}\n",
                "   \n",
            ),
        );
        let parsed = parse_source(SourceSessionFormat::Codex, &codex).expect("parse Codex with blanks");
        assert_eq!(
            parsed
                .messages
                .iter()
                .map(|message| message.text.as_str())
                .collect::<Vec<_>>(),
            ["build it"]
        );
    }

    #[test]
    fn blank_only_files_still_report_no_convertible_messages() {
        let fixtures = FixtureDir::new();
        let blank = fixtures.write("blank-only.jsonl", "\n  \n\n");
        assert!(matches!(
            parse_source(SourceSessionFormat::Pi, &blank),
            Err(ImportSessionError::NoConvertibleMessages { .. })
        ));

        let rollout_blank = fixtures.write("rollout-blank-only.jsonl", "\n  \n\n");
        let output = fixtures.0.join("must-not-exist.jsonl");
        assert!(matches!(
            import_session_to(SourceSessionFormat::Codex, &rollout_blank, &output),
            Err(ImportSessionError::NoConvertibleMessages { .. })
        ));
        assert!(!output.exists());
    }
    #[test]
    fn bounded_jsonl_accepts_exact_limits() {
        let fixtures = FixtureDir::new();
        let path = fixtures.write("bounded.jsonl", "{}\n{}\n");
        let values = parsers::read_jsonl_with_limits(&path, 6, 2, 2)
            .expect("exact file, line, and record limits are accepted");
        assert_eq!(values.len(), 2);
    }

    #[test]
    fn bounded_jsonl_rejects_oversize_file_line_and_record_count() {
        let fixtures = FixtureDir::new();

        let file = fixtures.write("oversize-file.jsonl", "{}\n{}\n");
        assert!(matches!(
            parsers::read_jsonl_with_limits(&file, 5, 2, 2),
            Err(ImportSessionError::ResourceLimit { .. })
        ));

        let line = fixtures.write("oversize-line.jsonl", "{ }\n");
        assert!(matches!(
            parsers::read_jsonl_with_limits(&line, 4, 2, 1),
            Err(ImportSessionError::ResourceLimit { .. })
        ));

        let records = fixtures.write("oversize-records.jsonl", "{}\n{}\n");
        assert!(matches!(
            parsers::read_jsonl_with_limits(&records, 6, 2, 1),
            Err(ImportSessionError::ResourceLimit { .. })
        ));
    }

    #[test]
    fn bounded_text_rejects_oversize_and_invalid_utf8() {
        let fixtures = FixtureDir::new();
        let oversize = fixtures.write("oversize.json", "12345");
        assert!(matches!(
            parsers::read_bounded_text(&oversize, 4),
            Err(ImportSessionError::ResourceLimit { .. })
        ));

        let invalid = fixtures.0.join("invalid-utf8.json");
        fs::write(&invalid, [0xff]).expect("write invalid UTF-8 fixture");
        assert!(matches!(
            parsers::read_bounded_text(&invalid, 1),
            Err(ImportSessionError::ResourceLimit { .. })
        ));
    }
}
