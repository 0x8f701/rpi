//! Import of external coding-agent sessions into native Pi v3 JSONL.
//!
//! Conversion is `agent-loader`: user/assistant/tool/thinking/image/compaction
//! blocks are preserved in the emitted tree. Catalog listing still uses the
//! local parsers for fingerprints and preview counts.

mod parsers;

use std::fmt;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use pi_sandbox::AbsolutePath;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

use crate::fs::{CapabilityFs, CapabilityRootLease, CapabilityRootPurpose, FsError};

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

#[derive(Debug)]
pub struct OperatorFileRead {
    pub bytes: Vec<u8>,
    pub len: u64,
}

pub fn read_operator_file(
    path: &Path,
    max_bytes: u64,
) -> Result<OperatorFileRead, ImportSessionError> {
    let path = absolute_path(path)?;
    let absolute = AbsolutePath::parse(path.clone()).map_err(|_| {
        invalid_source_open(
            SourceSessionFormat::Pi,
            &path,
            "operator import path must be absolute".to_owned(),
        )
    })?;
    let capabilities = CapabilityFs::new().map_err(|error| fs_import_error(&path, error))?;
    let file = capabilities
        .open_exact_file(CapabilityRootPurpose::OperatorImport, absolute)
        .map_err(|error| fs_import_error(&path, error))?;
    let metadata = file
        .metadata()
        .map_err(|error| fs_import_error(&path, error))?;
    let bytes = file
        .read_bounded(max_bytes)
        .map_err(|error| fs_import_error(&path, error))?;
    Ok(OperatorFileRead {
        bytes,
        len: metadata.len,
    })
}

fn fs_import_error(path: &Path, error: FsError) -> ImportSessionError {
    invalid_source_open(
        SourceSessionFormat::Pi,
        path,
        format!("capability filesystem access failed: {error}"),
    )
}

pub fn read_operator_file_under_roots(
    workspace: &crate::WorkspaceRoots,
    input: &Path,
    max_bytes: u64,
) -> Result<OperatorFileRead, ImportSessionError> {
    try_read_operator_file_under_roots(workspace, input, max_bytes)?.ok_or_else(|| {
        ImportSessionError::SessionNotFound {
            format: SourceSessionFormat::Pi,
            input: input.display().to_string(),
        }
    })
}

pub fn try_read_operator_file_under_roots(
    workspace: &crate::WorkspaceRoots,
    input: &Path,
    max_bytes: u64,
) -> Result<Option<OperatorFileRead>, ImportSessionError> {
    if input.as_os_str().is_empty() {
        return Err(invalid_source_open(
            SourceSessionFormat::Pi,
            input,
            "operator import path is empty".to_owned(),
        ));
    }
    let candidate = if input.is_absolute() {
        input.to_path_buf()
    } else {
        workspace.cwd().join(input)
    };
    let (root, relative) = workspace
        .roots()
        .iter()
        .find_map(|root| {
            candidate
                .strip_prefix(root)
                .ok()
                .map(|relative| (root, relative))
        })
        .filter(|(_, relative)| !relative.as_os_str().is_empty())
        .ok_or_else(|| {
            invalid_source_open(
                SourceSessionFormat::Pi,
                &candidate,
                "path escapes the configured workspace roots".to_owned(),
            )
        })?;
    let capabilities = CapabilityFs::new().map_err(|error| fs_import_error(&candidate, error))?;
    let lease = capabilities
        .open_root(
            CapabilityRootPurpose::OperatorImport,
            AbsolutePath::parse(root.clone()).map_err(|_| {
                invalid_source_open(
                    SourceSessionFormat::Pi,
                    root,
                    "workspace root is not an absolute capability path".to_owned(),
                )
            })?,
            crate::fs::CapabilityAccess::ReadOnly,
            crate::fs::RootCreation::Existing,
        )
        .map_err(|error| fs_import_error(&candidate, error))?;
    let file = match lease.open_file(relative) {
        Ok(file) => file,
        Err(error) if fs_error_is_missing(&error) => return Ok(None),
        Err(error) => return Err(fs_import_error(&candidate, error)),
    };
    let metadata = file
        .metadata()
        .map_err(|error| fs_import_error(&candidate, error))?;
    let bytes = file
        .read_bounded(max_bytes)
        .map_err(|error| fs_import_error(&candidate, error))?;
    Ok(Some(OperatorFileRead {
        bytes,
        len: metadata.len,
    }))
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
    let relative = path.strip_prefix(root).map_err(|_| {
        invalid_source_open(
            source,
            path,
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
            path,
            "path is not a normal relative source path".to_owned(),
        ));
    }
    let capabilities = CapabilityFs::new().map_err(|error| fs_import_error(path, error))?;
    let lease = capabilities
        .open_root(
            if append {
                CapabilityRootPurpose::SessionStore
            } else {
                CapabilityRootPurpose::OperatorImport
            },
            AbsolutePath::parse(root.to_path_buf()).map_err(|_| {
                invalid_source_open(source, root, "configured source root is invalid".to_owned())
            })?,
            if append {
                crate::fs::CapabilityAccess::PrivateReadWrite
            } else {
                crate::fs::CapabilityAccess::ReadOnly
            },
            crate::fs::RootCreation::Existing,
        )
        .map_err(|error| fs_import_error(path, error))?;
    let primary_capability = if append {
        lease.open_read_write_file(relative)
    } else {
        lease.open_file(relative)
    }
    .map_err(|error| fs_import_error(path, error))?;
    let metadata = primary_capability
        .try_clone_std()
        .and_then(|file| file.metadata())
        .map_err(|source_error| ImportSessionError::Io {
            path: path.to_path_buf(),
            source: source_error,
        })?;
    let primary =
        primary_capability
            .try_clone_std()
            .map_err(|source_error| ImportSessionError::Io {
                path: path.to_path_buf(),
                source: source_error,
            })?;
    let (grok_chat, grok_cwd) = if source == SourceSessionFormat::Grok {
        let chat_relative = relative.with_file_name("chat_history.jsonl");
        let chat =
            open_optional_lease_file(&lease, &chat_relative, source, &root.join(&chat_relative))?;
        let cwd_relative = relative
            .parent()
            .and_then(Path::parent)
            .map(|parent| parent.join(".cwd"));
        let cwd = match cwd_relative {
            Some(relative) => {
                open_optional_lease_file(&lease, &relative, source, &root.join(&relative))?
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
            invalid_source_open(
                source,
                &path,
                "source path has no parent directory".to_owned(),
            )
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

fn open_optional_lease_file(
    lease: &crate::fs::CapabilityRootLease,
    relative: &Path,
    source: SourceSessionFormat,
    display_path: &Path,
) -> Result<Option<fs::File>, ImportSessionError> {
    match lease.open_file(relative) {
        Ok(file) => file
            .try_clone_std()
            .map(Some)
            .map_err(|source_error| ImportSessionError::Io {
                path: display_path.to_path_buf(),
                source: source_error,
            }),
        Err(error) if fs_error_is_missing(&error) => Ok(None),
        Err(error) => Err(invalid_source_open(
            source,
            display_path,
            format!("secure no-follow companion open failed: {error}"),
        )),
    }
}

fn fs_error_is_missing(error: &FsError) -> bool {
    matches!(
        error,
        FsError::System {
            kind: std::io::ErrorKind::NotFound,
            ..
        }
    )
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
    let id = Uuid::now_v7().to_string();
    let opened = open_source_direct(source, &source_path)?;
    let (session, records, messages, cwd) =
        convert_opened_members(source, &source_path, vec![opened], &id, None)?;
    let start = session_start_from_loader(&session);
    let output = default_session_dir(&cwd).join(session_filename(start, &id));
    let lineage = lineage
        .cloned()
        .unwrap_or_else(|| default_lineage(source, &source_path, &session.session_id));
    write_converted(
        source,
        source_path,
        session.session_id,
        cwd,
        id,
        output,
        messages,
        records,
        Some(&lineage),
        start,
    )
}

/// Resolve `input`, convert it, and write a new Pi v3 session inside
/// `directory`. The directory is created through a private session-store
/// capability when missing; symbolic links in the directory path fail closed.
/// A fresh normal Pi session filename is selected and created exclusively, so
/// existing files are never overwritten.
pub fn import_session_into_directory(
    source: SourceSessionFormat,
    input: impl AsRef<Path>,
    directory: impl AsRef<Path>,
) -> Result<ImportedSession, ImportSessionError> {
    let source_path = resolve_input(source, input.as_ref())?;
    let id = Uuid::now_v7().to_string();
    let opened = open_source_direct(source, &source_path)?;
    let (session, records, messages, cwd) =
        convert_opened_members(source, &source_path, vec![opened], &id, None)?;
    let start = session_start_from_loader(&session);
    let directory = open_import_directory(directory.as_ref())?;
    let filename = session_filename(start, &id);
    let lineage = default_lineage(source, &source_path, &session.session_id);
    write_converted_under_lease(
        source,
        source_path,
        session.session_id,
        cwd,
        id,
        &directory,
        &filename,
        messages,
        records,
        Some(&lineage),
        start,
    )
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
    let source_path = resolve_input(source, input.as_ref())?;
    let id = Uuid::now_v7().to_string();
    let opened = open_source_direct(source, &source_path)?;
    let (session, records, messages, cwd) =
        convert_opened_members(source, &source_path, vec![opened], &id, None)?;
    let start = session_start_from_loader(&session);
    let requested = output.as_ref();
    let requested = absolute_path(requested)?;
    let capabilities = CapabilityFs::new().map_err(|error| fs_import_error(&requested, error))?;
    let output = match capabilities.open_root(
        CapabilityRootPurpose::SessionStore,
        AbsolutePath::parse(requested.clone()).map_err(|_| {
            invalid_source_open(source, &requested, "invalid output path".to_owned())
        })?,
        crate::fs::CapabilityAccess::PrivateReadWrite,
        crate::fs::RootCreation::Existing,
    ) {
        Ok(_) => requested.join(session_filename(start, &id)),
        Err(
            FsError::UnsupportedEntryType
            | FsError::System {
                kind: std::io::ErrorKind::NotADirectory,
                ..
            },
        ) => requested,
        Err(FsError::System {
            kind: std::io::ErrorKind::NotFound,
            ..
        }) => requested,
        Err(error) => return Err(fs_import_error(&requested, error)),
    };
    let lineage = lineage
        .cloned()
        .unwrap_or_else(|| default_lineage(source, &source_path, &session.session_id));
    write_converted(
        source,
        source_path,
        session.session_id,
        cwd,
        id,
        output,
        messages,
        records,
        Some(&lineage),
        start,
    )
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
    Ok(OmpChainMemberProbe {
        parent_session,
        size,
    })
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
    /// Every foreign format (Codex, Claude, Grok, Droid) computes both counts
    /// the same way: losslessly over the raw content blocks of each turn,
    /// before the first-text projection.
    pub meaningful_count: usize,
    /// Meaningful turns carrying genuine (non-bootstrap) content: any
    /// non-envelope text block or supported non-text block (image), scanned
    /// losslessly over every content block — not the first-text projection.
    /// Uniform across Codex, Claude, Grok, and Droid.
    pub genuine_count: usize,
}

impl From<ParsedSession> for ParsedSessionPublic {
    fn from(parsed: ParsedSession) -> Self {
        Self {
            source_session_id: parsed.source_session_id,
            cwd: parsed.cwd,
            started_at: parsed.started_at,
            messages: parsed.messages,
            meaningful_count: parsed.meaningful_count,
            genuine_count: parsed.genuine_count,
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
            genuine_count: parsed.genuine_count,
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

/// Convert `members` through agent-loader and write under `output_directory`.
/// The directory is opened or created as a private session-store capability,
/// and the file is created exclusively relative to the retained lease.
pub(crate) fn emit_source_into_directory_with_lineage(
    source: SourceSessionFormat,
    source_path: PathBuf,
    members: Vec<OpenedSource>,
    id: String,
    cwd_override: Option<&Path>,
    output_directory: &Path,
    output_name: &str,
    lineage: &ImportLineage,
) -> Result<ImportedSession, ImportSessionError> {
    let (session, records, messages, cwd) =
        convert_opened_members(source, &source_path, members, &id, cwd_override)?;
    let start = session_start_from_loader(&session);
    let directory = open_import_directory(output_directory)?;
    write_converted_under_lease(
        source,
        source_path,
        session.session_id,
        cwd,
        id,
        &directory,
        output_name,
        messages,
        records,
        Some(lineage),
        start,
    )
}

#[cfg(test)]
pub(crate) fn occupy_import_output_with_lineage(
    source: SourceSessionFormat,
    source_path: PathBuf,
    cwd: PathBuf,
    id: String,
    output_directory: &Path,
    output_name: &str,
    lineage: &ImportLineage,
) -> Result<ImportedSession, ImportSessionError> {
    let start = Utc::now();
    let records = vec![json!({
        "type": "session",
        "version": 3,
        "id": id,
        "timestamp": format_timestamp(start),
        "cwd": cwd,
    })];
    let directory = open_import_directory(output_directory)?;
    write_converted_under_lease(
        source,
        source_path,
        lineage.source_session_id.clone(),
        cwd,
        id,
        &directory,
        output_name,
        vec![ImportedMessage {
            role: ImportedMessageRole::User,
            text: "occupied".to_owned(),
            timestamp: None,
        }],
        records,
        Some(lineage),
        start,
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
    source_session_id: &str,
) -> ImportLineage {
    ImportLineage {
        source,
        source_session_id: source_session_id.to_owned(),
        source_path: source_path.to_path_buf(),
        content_fingerprint: read_operator_file(source_path, MAX_SOURCE_BYTES)
            .ok()
            .map(|read| format!("bytes:{}", read.len)),
        imported_at: format_timestamp(Utc::now()),
    }
}

fn loader_tool(source: SourceSessionFormat) -> agent_loader::domain::SourceTool {
    match source {
        SourceSessionFormat::Pi => agent_loader::domain::SourceTool::Pi,
        SourceSessionFormat::Omp => agent_loader::domain::SourceTool::Omp,
        SourceSessionFormat::Codex => agent_loader::domain::SourceTool::Codex,
        SourceSessionFormat::Claude => agent_loader::domain::SourceTool::Claude,
        SourceSessionFormat::Grok => agent_loader::domain::SourceTool::Grok,
        SourceSessionFormat::Droid => agent_loader::domain::SourceTool::Droid,
    }
}

struct LoaderStaging {
    dir: PathBuf,
    entry: PathBuf,
}

impl Drop for LoaderStaging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn stage_opened_source(
    source: SourceSessionFormat,
    opened: OpenedSource,
) -> Result<LoaderStaging, ImportSessionError> {
    if opened.metadata().len() > MAX_SOURCE_BYTES {
        return Err(ImportSessionError::ResourceLimit {
            path: opened.path().to_path_buf(),
            reason: format!(
                "file is {} bytes; maximum is {MAX_SOURCE_BYTES}",
                opened.metadata().len()
            ),
        });
    }
    let parts = opened.into_parts();
    let dir = std::env::temp_dir().join(format!("rpi-loader-{}", Uuid::new_v4()));
    fs::create_dir(&dir).map_err(|source_error| ImportSessionError::Io {
        path: dir.clone(),
        source: source_error,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).map_err(
            |source_error| ImportSessionError::Io {
                path: dir.clone(),
                source: source_error,
            },
        )?;
    }
    let entry = if source == SourceSessionFormat::Grok {
        copy_opened_handle(&parts.primary, &dir.join("summary.json"), &parts.path)?;
        if let Some(chat) = parts.grok_chat.as_ref() {
            copy_opened_handle(
                chat,
                &dir.join("chat_history.jsonl"),
                &parts.path.with_file_name("chat_history.jsonl"),
            )?;
        }
        dir.join("summary.json")
    } else {
        let name = parts
            .path
            .file_name()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("session.jsonl"));
        let dest = dir.join(name);
        copy_opened_handle(&parts.primary, &dest, &parts.path)?;
        dest
    };
    Ok(LoaderStaging { dir, entry })
}

fn copy_opened_handle(
    file: &fs::File,
    dest: &Path,
    display: &Path,
) -> Result<(), ImportSessionError> {
    let mut src = file.try_clone().map_err(|source| ImportSessionError::Io {
        path: display.to_path_buf(),
        source,
    })?;
    src.seek(SeekFrom::Start(0))
        .map_err(|source| ImportSessionError::Io {
            path: display.to_path_buf(),
            source,
        })?;
    let mut bytes = Vec::new();
    src.read_to_end(&mut bytes)
        .map_err(|source| ImportSessionError::Io {
            path: display.to_path_buf(),
            source,
        })?;
    if bytes.len() as u64 > MAX_SOURCE_BYTES {
        return Err(ImportSessionError::ResourceLimit {
            path: display.to_path_buf(),
            reason: format!(
                "file is {} bytes; maximum is {MAX_SOURCE_BYTES}",
                bytes.len()
            ),
        });
    }
    fs::write(dest, bytes).map_err(|source| ImportSessionError::Io {
        path: dest.to_path_buf(),
        source,
    })
}

fn staged_is_blank(path: &Path) -> bool {
    fs::read_to_string(path)
        .ok()
        .is_some_and(|body| body.trim().is_empty())
}

/// Cross-member identity dedup for staged OMP rotation chains (root -> leaf
/// staging order): a message record copied verbatim from an earlier member
/// (same entry id) is dropped so the library's per-file parse cannot emit it
/// twice. First occurrence wins, matching the authoritative chain merge.
fn dedup_staged_omp_chain_entries(entries: &[PathBuf]) -> Result<(), ImportSessionError> {
    let mut seen_ids = std::collections::HashSet::new();
    for entry in entries {
        let body = fs::read_to_string(entry).map_err(|source| ImportSessionError::Io {
            path: entry.clone(),
            source,
        })?;
        let mut kept: Vec<String> = Vec::new();
        for line in body.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
                if value.get("type").and_then(Value::as_str) == Some("message") {
                    let id = value.get("id").and_then(Value::as_str).map(str::to_owned);
                    if let Some(id) = id {
                        if !seen_ids.insert(id) {
                            continue;
                        }
                    }
                }
            }
            kept.push(line.to_owned());
        }
        let body = if kept.is_empty() {
            String::new()
        } else {
            kept.join("\n") + "\n"
        };
        fs::write(entry, body).map_err(|source| ImportSessionError::Io {
            path: entry.clone(),
            source,
        })?;
    }
    Ok(())
}

fn convert_opened_members(
    source: SourceSessionFormat,
    source_path: &Path,
    members: Vec<OpenedSource>,
    session_id: &str,
    cwd_override: Option<&Path>,
) -> Result<
    (
        agent_loader::domain::Session,
        Vec<Value>,
        Vec<ImportedMessage>,
        PathBuf,
    ),
    ImportSessionError,
> {
    let mut stagings = Vec::new();
    let mut entries = Vec::new();
    let is_chain = source == SourceSessionFormat::Omp;
    for opened in members {
        match stage_opened_source(source, opened) {
            Ok(staging) => {
                entries.push(staging.entry.clone());
                stagings.push(staging);
            }
            // A chain member beyond the per-file source cap is excluded from
            // conversion (catalog lineage keeps it as a boundary) instead of
            // failing the whole chain import; a lone unusable member still
            // fails so a direct oversize import keeps its contract.
            Err(ImportSessionError::ResourceLimit { .. }) if is_chain => {}
            Err(error) => return Err(error),
        }
    }
    if entries.is_empty() {
        return Err(ImportSessionError::NoConvertibleMessages {
            format: source,
            path: source_path.to_path_buf(),
        });
    }
    if entries.len() > 1 {
        dedup_staged_omp_chain_entries(&entries)?;
    }
    if entries.iter().all(|path| staged_is_blank(path)) {
        return Err(ImportSessionError::NoConvertibleMessages {
            format: source,
            path: source_path.to_path_buf(),
        });
    }
    let paths: Vec<&Path> = entries.iter().map(PathBuf::as_path).collect();
    let session = crate::agent_loader_convert::parse_staged_sources(loader_tool(source), &paths)
        .map_err(|error| map_loader_error(source, source_path, error))?;
    let messages = imported_messages(&session);
    if messages.is_empty() {
        return Err(ImportSessionError::NoConvertibleMessages {
            format: source,
            path: source_path.to_path_buf(),
        });
    }
    let cwd = usable_cwd(cwd_override.unwrap_or(session.cwd.as_path()))?;
    let records = crate::agent_loader_convert::emit_rpi_records(&session, session_id, &cwd)
        .map_err(|error| map_loader_error(source, source_path, error))?;
    drop(stagings);
    Ok((session, records, messages, cwd))
}

fn map_loader_error(
    source: SourceSessionFormat,
    path: &Path,
    error: anyhow::Error,
) -> ImportSessionError {
    if let Some(omp_error) = error.downcast_ref::<agent_loader::formats::omp::OmpParseError>() {
        return match omp_error {
            agent_loader::formats::omp::OmpParseError::Empty => {
                ImportSessionError::NoConvertibleMessages {
                    format: source,
                    path: path.to_path_buf(),
                }
            }
            agent_loader::formats::omp::OmpParseError::NoSessionHeader => {
                ImportSessionError::InvalidNativeHeader {
                    format: source,
                    path: path.to_path_buf(),
                }
            }
        };
    }
    ImportSessionError::InvalidInput {
        format: source,
        path: path.to_path_buf(),
        reason: format!("{error:#}"),
    }
}

fn imported_messages(session: &agent_loader::domain::Session) -> Vec<ImportedMessage> {
    session
        .messages
        .iter()
        .filter_map(|message| {
            let role = match message.role {
                agent_loader::domain::Role::User => ImportedMessageRole::User,
                agent_loader::domain::Role::Assistant => ImportedMessageRole::Assistant,
                agent_loader::domain::Role::Tool => return None,
            };
            Some(ImportedMessage {
                role,
                text: message.text.clone(),
                timestamp: message.timestamp.clone(),
            })
        })
        .collect()
}

fn write_converted(
    source: SourceSessionFormat,
    source_path: PathBuf,
    source_session_id: String,
    cwd: PathBuf,
    id: String,
    output: PathBuf,
    messages: Vec<ImportedMessage>,
    records: Vec<Value>,
    lineage: Option<&ImportLineage>,
    start: DateTime<Utc>,
) -> Result<ImportedSession, ImportSessionError> {
    let records = stamp_lineage(source, records, lineage, start);
    write_jsonl_new(&output, &records)?;
    Ok(ImportedSession {
        source,
        source_path,
        source_session_id: (!source_session_id.is_empty()).then_some(source_session_id),
        id,
        path: output,
        cwd,
        messages,
        reused_existing: false,
    })
}

fn write_converted_under_lease(
    source: SourceSessionFormat,
    source_path: PathBuf,
    source_session_id: String,
    cwd: PathBuf,
    id: String,
    directory: &CapabilityRootLease,
    filename: &str,
    messages: Vec<ImportedMessage>,
    records: Vec<Value>,
    lineage: Option<&ImportLineage>,
    start: DateTime<Utc>,
) -> Result<ImportedSession, ImportSessionError> {
    let output = directory.display_root().join(filename);
    let records = stamp_lineage(source, records, lineage, start);
    write_jsonl_new_under_lease(directory, Path::new(filename), &output, &records)?;
    Ok(ImportedSession {
        source,
        source_path,
        source_session_id: (!source_session_id.is_empty()).then_some(source_session_id),
        id,
        path: output,
        cwd,
        messages,
        reused_existing: false,
    })
}

fn stamp_lineage(
    source: SourceSessionFormat,
    mut records: Vec<Value>,
    lineage: Option<&ImportLineage>,
    start: DateTime<Utc>,
) -> Vec<Value> {
    let Some(lineage) = lineage else {
        return records;
    };
    let parent_session = lineage.parent_session_value();
    if let Some(header) = records.first_mut().and_then(Value::as_object_mut) {
        if header.get("type").and_then(Value::as_str) == Some("session") {
            header.insert("parentSession".to_owned(), json!(parent_session));
        }
    }
    let lineage_entry_id = short_id();
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
    data.insert("parentSession".to_owned(), json!(parent_session));
    let insert_at = usize::from(
        records
            .first()
            .and_then(|value| value.get("type"))
            .and_then(Value::as_str)
            == Some("session"),
    );
    if let Some(next) = records.get_mut(insert_at).and_then(Value::as_object_mut) {
        let parent_missing = next.get("parentId").is_none_or(Value::is_null);
        if parent_missing {
            next.insert("parentId".to_owned(), json!(lineage_entry_id));
        }
    }
    records.insert(
        insert_at,
        json!({
            "type": "custom",
            "id": lineage_entry_id,
            "parentId": Value::Null,
            "timestamp": format_timestamp(start + TimeDelta::milliseconds(1)),
            "customType": "import_lineage",
            "data": Value::Object(data),
        }),
    );
    records
}

fn open_import_directory(directory: &Path) -> Result<CapabilityRootLease, ImportSessionError> {
    let directory = absolute_path(directory)?;
    let absolute =
        AbsolutePath::parse(directory.clone()).map_err(|_| ImportSessionError::InvalidOutput {
            path: directory.clone(),
            reason: "output directory must be absolute".to_owned(),
        })?;
    CapabilityFs::new()
        .map_err(|error| ImportSessionError::InvalidOutput {
            path: directory.clone(),
            reason: format!("capability filesystem initialization failed: {error}"),
        })?
        .open_root(
            CapabilityRootPurpose::SessionStore,
            absolute,
            crate::fs::CapabilityAccess::PrivateReadWrite,
            crate::fs::RootCreation::CreateDirectory,
        )
        .map_err(|error| ImportSessionError::InvalidOutput {
            path: directory,
            reason: format!("opening output directory failed: {error}"),
        })
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

fn short_id() -> String {
    Uuid::new_v4().simple().to_string()[..8].to_owned()
}

fn session_start_from_loader(session: &agent_loader::domain::Session) -> DateTime<Utc> {
    session
        .start_timestamp
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
    let path = absolute_path(path)?;
    let absolute =
        AbsolutePath::parse(path.clone()).map_err(|_| ImportSessionError::InvalidOutput {
            path: path.clone(),
            reason: "output path must be absolute".to_owned(),
        })?;
    let capabilities = CapabilityFs::new().map_err(|error| ImportSessionError::InvalidOutput {
        path: path.clone(),
        reason: format!("capability filesystem initialization failed: {error}"),
    })?;
    let (parent, name) = capabilities
        .open_exact_parent(
            CapabilityRootPurpose::SessionStore,
            absolute,
            crate::fs::CapabilityAccess::PrivateReadWrite,
            crate::fs::RootCreation::CreateDirectory,
        )
        .map_err(|error| ImportSessionError::InvalidOutput {
            path: path.clone(),
            reason: format!("opening output parent failed: {error}"),
        })?;
    write_jsonl_new_under_lease(&parent, Path::new(&name), &path, records)
}

fn write_jsonl_new_under_lease(
    parent: &CapabilityRootLease,
    relative: &Path,
    path: &Path,
    records: &[Value],
) -> Result<(), ImportSessionError> {
    let mut bytes = Vec::new();
    for record in records {
        serde_json::to_writer(&mut bytes, record).map_err(|source| ImportSessionError::Json {
            path: path.to_path_buf(),
            source,
        })?;
        bytes.push(b'\n');
    }
    let mut file = parent
        .create_new(relative, crate::fs::FileMode::Private)
        .map_err(|error| match error {
            FsError::System {
                kind: std::io::ErrorKind::AlreadyExists,
                ..
            } => ImportSessionError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "output already exists",
                ),
            },
            error => ImportSessionError::InvalidOutput {
                path: path.to_path_buf(),
                reason: format!("creating private output failed: {error}"),
            },
        })?;
    file.write_all(&bytes)
        .map_err(|error| ImportSessionError::InvalidOutput {
            path: path.to_path_buf(),
            reason: format!("writing private output failed: {error}"),
        })?;
    file.finish()
        .map_err(|error| ImportSessionError::InvalidOutput {
            path: path.to_path_buf(),
            reason: format!("finishing private output failed: {error}"),
        })?;
    Ok(())
}

/// Maximum accepted length of a session id/prefix typed into an ID-only
/// lookup, well above any real source record id or session filename while
/// bounding abuse of the scan.
const MAX_SESSION_ID_INPUT_BYTES: usize = 256;

/// Validate a non-path input before treating it as a source session id or
/// prefix. Malformed values — empty strings, dot paths, path separators,
/// control characters, non-UTF-8 bytes, oversize values, or characters no
/// source record id or session filename can contain — fail closed with a
/// typed [`ImportSessionError::InvalidInput`] instead of being interpreted as
/// traversal or scanned against the whole source tree.
fn validate_id_lookup_input(
    source: SourceSessionFormat,
    input: &Path,
) -> Result<String, ImportSessionError> {
    let input_text = input.to_str().ok_or_else(|| {
        invalid_source_open(source, input, "session id must be valid UTF-8".to_owned())
    })?;
    let invalid = |reason: &str| invalid_source_open(source, input, reason.to_owned());
    if input_text.is_empty() {
        return Err(invalid("session id must not be empty"));
    }
    if input_text == "." || input_text == ".." {
        return Err(invalid("session id must not be a dot path"));
    }
    if input_text.contains(['/', '\\']) {
        return Err(invalid("session id must not contain path separators"));
    }
    if input_text.len() > MAX_SESSION_ID_INPUT_BYTES {
        return Err(invalid("session id is too long"));
    }
    if input_text.chars().any(char::is_control)
        || !input_text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(invalid("session id contains unsupported characters"));
    }
    Ok(input_text.to_owned())
}

fn resolve_input(source: SourceSessionFormat, input: &Path) -> Result<PathBuf, ImportSessionError> {
    let input = expand_tilde(input)?;
    if input.is_absolute() {
        // Absolute inputs are direct paths only. A path that fails
        // normalization is reported as not found, never re-interpreted as a
        // session id and re-scanned.
        return match normalize_direct_path(source, input.clone()) {
            Ok(path) => Ok(path),
            Err(ImportSessionError::InvalidInput { .. }) => {
                Err(ImportSessionError::SessionNotFound {
                    format: source,
                    input: input.display().to_string(),
                })
            }
            Err(error) => Err(error),
        };
    }
    let input_text = validate_id_lookup_input(source, &input)?;
    let root = source_root(source)?;
    resolve_id_lookup(source, &root, &input_text)
}

/// Resolve a validated session id/prefix to exactly one source record under
/// `root`, scanning through a capability lease so the lookup can never leave
/// the configured source root. Fails closed on no match, multiple matches, or
/// any symlink entry in the scanned tree — a contaminated snapshot never
/// resolves to a possibly-steered sibling match.
fn resolve_id_lookup(
    source: SourceSessionFormat,
    root: &Path,
    input: &str,
) -> Result<PathBuf, ImportSessionError> {
    let capabilities = CapabilityFs::new().map_err(|error| fs_import_error(root, error))?;
    let lease = capabilities
        .open_root(
            CapabilityRootPurpose::OperatorImport,
            AbsolutePath::parse(root.to_path_buf())
                .map_err(|_| invalid_source_open(source, root, "invalid source root".to_owned()))?,
            crate::fs::CapabilityAccess::ReadOnly,
            crate::fs::RootCreation::Existing,
        )
        .map_err(|error| fs_import_error(root, error))?;
    let mut matches = Vec::new();
    // Fail closed on symlink entries: a contaminated enumeration must not
    // resolve to a possibly-steered sibling match.
    if discover_source_matches(source, &lease, Path::new(""), input, &mut matches).is_err() {
        return Err(ImportSessionError::SessionNotFound {
            format: source,
            input: input.to_owned(),
        });
    }
    matches.sort();
    matches.dedup();
    match matches.as_slice() {
        [] => Err(ImportSessionError::SessionNotFound {
            format: source,
            input: input.to_owned(),
        }),
        [path] => Ok(path.clone()),
        _ => Err(ImportSessionError::AmbiguousSession {
            format: source,
            input: input.to_owned(),
            matches,
        }),
    }
}

fn discover_source_matches(
    source: SourceSessionFormat,
    lease: &crate::fs::CapabilityRootLease,
    relative: &Path,
    input: &str,
    matches: &mut Vec<PathBuf>,
) -> Result<(), FsError> {
    // Fail closed on symlink entries. Names come from the strict (no-symlink)
    // snapshot, then each name is re-opened with a no-follow entry open so a
    // symlink introduced between the snapshot and the open still rejects
    // instead of being followed. The lookup never follows a link, and it never
    // matches a sibling from a partially validated snapshot — a contaminated
    // directory aborts the whole scan so the caller resolves to "no session".
    let names = match lease.read_dir_names_strict(relative) {
        Ok(names) => names,
        Err(error) if fs_error_is_missing(&error) => return Ok(()),
        Err(error) => return Err(error),
    };
    for name in names {
        let child = relative.join(&name);
        match lease.open_entry_exact(&child) {
            Ok(crate::fs::CapabilityEntry::Directory(_)) => {
                discover_source_matches(source, lease, &child, input, matches)?;
            }
            Ok(crate::fs::CapabilityEntry::File(file)) => {
                let path = file.display_path();
                if !is_source_candidate(source, &path) {
                    continue;
                }
                let file_stem = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or("");
                let directory_id = path
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|name| name.to_str())
                    .unwrap_or("");
                let name_matches = file_stem == input
                    || file_stem.starts_with(input)
                    || directory_id == input
                    || directory_id.starts_with(input)
                    || file_stem
                        .strip_prefix("rollout-")
                        .is_some_and(|stem| stem == input || stem.starts_with(input));
                // Record ids cover layouts whose filename does not carry the
                // id (native `{timestamp}_{id}.jsonl`, codex `rollout-*` with
                // a divergent header id). A corrupt or oversize record yields
                // no id match, but its name can still match.
                let id_matches = source_record_id(source, lease, &child)
                    .is_some_and(|id| id == input || id.starts_with(input));
                if name_matches || id_matches {
                    matches.push(path);
                }
            }
            // A name that vanished between the snapshot and the open
            // contributes no match, exactly as a missing subdirectory would.
            Err(error) if fs_error_is_missing(&error) => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Read one discovered candidate's source record id through the held root
/// lease. Files that fail header parsing (corrupt, oversize, or foreign
/// layout) contribute no id match — name matching still applies — so a
/// malformed record can never steer lookup outside its own file.
fn source_record_id(
    source: SourceSessionFormat,
    lease: &crate::fs::CapabilityRootLease,
    relative: &Path,
) -> Option<String> {
    let file = lease.open_file(relative).ok()?;
    let primary = file.try_clone_std().ok()?;
    let metadata = primary.metadata().ok()?;
    let opened = OpenedSource {
        path: lease.display_root().join(relative),
        primary,
        metadata,
        grok_chat: None,
        grok_cwd: None,
    };
    parsers::source_id_opened(source, opened).ok().flatten()
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
    let capabilities = CapabilityFs::new().map_err(|error| fs_import_error(&input, error))?;
    let inspect = |path: &Path| {
        let absolute = AbsolutePath::parse(path.to_path_buf())
            .map_err(|_| invalid_source_open(source, path, "path is not absolute".to_owned()))?;
        capabilities
            .open_exact_file(CapabilityRootPurpose::OperatorImport, absolute)
            .map(|_| path.to_path_buf())
            .map_err(|error| fs_import_error(path, error))
    };
    if source == SourceSessionFormat::Grok {
        if input
            .file_name()
            .is_some_and(|name| name == "chat_history.jsonl")
        {
            let summary = input.with_file_name("summary.json");
            return inspect(&summary);
        }
        let summary = input.join("summary.json");
        if let Ok(path) = inspect(&summary) {
            return Ok(path);
        }
    }
    if is_source_candidate(source, &input) {
        return inspect(&input);
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
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("rollout-"))
        }
        SourceSessionFormat::Pi
        | SourceSessionFormat::Omp
        | SourceSessionFormat::Claude
        | SourceSessionFormat::Droid => path
            .extension()
            .is_some_and(|extension| extension == "jsonl"),
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
        assert_eq!(
            parsed.messages[1].timestamp.as_deref(),
            Some("2026-01-01T00:00:03Z")
        );

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
        assert_eq!(
            parsed
                .messages
                .iter()
                .map(|message| message.text.as_str())
                .collect::<Vec<_>>(),
            ["build it", "done"]
        );
    }

    /// The Codex genuine and meaningful counts are scanned losslessly over
    /// every content block: a later genuine text block or an image-only turn
    /// (a real Responses `input_image` part) counts even when the first-text
    /// projection would only see the leading bootstrap envelope, and
    /// all-envelope content never counts.
    #[test]
    fn codex_genuine_count_scans_all_blocks_losslessly() {
        let fixtures = FixtureDir::new();
        let path = fixtures.write(
            "rollout-blocks.jsonl",
            concat!(
                "{\"timestamp\":\"2026-02-01T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"blocks\",\"cwd\":\"<workspace>/codex\"}}\n",
                "{\"timestamp\":\"2026-02-01T00:00:01Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"<environment_context>\\nagent mode\\n</environment_context>\"},{\"type\":\"input_text\",\"text\":\"fix the login bug\"}]}}\n",
                "{\"timestamp\":\"2026-02-01T00:00:02Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"<user_info>agent session</user_info>\"},{\"type\":\"input_image\",\"image_url\":\"data:image/png;base64,aW1n\"}]}}\n",
                "{\"timestamp\":\"2026-02-01T00:00:03Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"<environment_context>\\nagent mode\\n</environment_context>\"},{\"type\":\"input_text\",\"text\":\"<user_instructions>follow AGENTS.md</user_instructions>\"}]}}\n",
            ),
        );
        let parsed = parse_source(SourceSessionFormat::Codex, &path).expect("parse Codex");
        // The lossy projection keeps only the first text block of each turn.
        assert_eq!(
            parsed
                .messages
                .iter()
                .map(|message| message.text.as_str())
                .collect::<Vec<_>>(),
            [
                "<environment_context>\nagent mode\n</environment_context>",
                "<user_info>agent session</user_info>",
                "<environment_context>\nagent mode\n</environment_context>",
            ]
        );
        // Genuine: turn 1 (later real text), turn 2 (input_image); turn 3
        // is all-envelope. The first-text projection alone would report zero.
        assert_eq!(parsed.meaningful_count, 3);
        assert_eq!(parsed.genuine_count, 2);
    }

    /// A truly image-only Codex turn — no text block at all — is dropped by
    /// the lossy first-text projection yet counted losslessly from the raw
    /// content: meaningful=1, genuine=1. The fixture uses the real
    /// OpenAI Responses wire shape: a user image part is `input_image` with
    /// an `image_url`, not an Anthropic-style `image` block. The old
    /// projection-derived meaningful count reported zero.
    #[test]
    fn codex_image_only_turn_counts_meaningful_and_genuine() {
        let fixtures = FixtureDir::new();
        let path = fixtures.write(
            "rollout-image-only.jsonl",
            concat!(
                "{\"timestamp\":\"2026-02-01T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"codex-image-only\",\"cwd\":\"<workspace>/codex\"}}\n",
                "{\"timestamp\":\"2026-02-01T00:00:01Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_image\",\"image_url\":\"data:image/png;base64,aW1n\"}]}}\n",
            ),
        );
        let parsed = parse_source(SourceSessionFormat::Codex, &path).expect("parse Codex");
        assert!(
            parsed.messages.is_empty(),
            "first-text projection drops image-only turn"
        );
        assert_eq!(parsed.meaningful_count, 1);
        assert_eq!(parsed.genuine_count, 1);
    }

    /// The supported image block shape is format-scoped: the Codex Responses
    /// wire never emits an Anthropic-style `image` content part, so a Codex
    /// rollout whose only content block is `{"type":"image"}` — an arbitrary
    /// foreign shape — is neither meaningful nor genuine and projects no
    /// message. Only `input_image` counts for Codex.
    #[test]
    fn codex_foreign_image_block_shape_does_not_count() {
        let fixtures = FixtureDir::new();
        let path = fixtures.write(
            "rollout-foreign-image.jsonl",
            concat!(
                "{\"timestamp\":\"2026-02-01T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"codex-foreign-image\",\"cwd\":\"<workspace>/codex\"}}\n",
                "{\"timestamp\":\"2026-02-01T00:00:01Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"image\",\"mimeType\":\"image/png\",\"data\":\"aW1n\"}]}}\n",
            ),
        );
        let parsed = parse_source(SourceSessionFormat::Codex, &path).expect("parse Codex");
        assert!(
            parsed.messages.is_empty(),
            "projection drops a foreign image-block-only turn"
        );
        assert_eq!(
            parsed.meaningful_count, 0,
            "`image` is not a supported Codex content block"
        );
        assert_eq!(
            parsed.genuine_count, 0,
            "`image` is not a supported Codex content block"
        );
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
        assert_eq!(
            parsed
                .messages
                .iter()
                .map(|message| message.text.as_str())
                .collect::<Vec<_>>(),
            ["hello", "hi"]
        );
    }

    /// Same lossless count contract for Claude: a truly image-only turn — no
    /// text block at all — counts meaningful=1, genuine=1 even though the
    /// first-text projection drops it from `messages`.
    #[test]
    fn claude_image_only_turn_counts_meaningful_and_genuine() {
        let fixtures = FixtureDir::new();
        let path = fixtures.write(
            "claude-image-only.jsonl",
            concat!(
                "{\"type\":\"user\",\"uuid\":\"u\",\"parentUuid\":null,\"isSidechain\":false,\"sessionId\":\"claude-image-only\",\"cwd\":\"<workspace>/claude\",\"timestamp\":\"2026-03-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"image\",\"source\":{\"type\":\"base64\",\"media_type\":\"image/png\",\"data\":\"aW1n\"}}]}}\n",
            ),
        );
        let parsed = parse_source(SourceSessionFormat::Claude, &path).expect("parse Claude");
        assert!(
            parsed.messages.is_empty(),
            "first-text projection drops image-only turn"
        );
        assert_eq!(parsed.meaningful_count, 1);
        assert_eq!(parsed.genuine_count, 1);
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
        assert_eq!(
            parsed
                .messages
                .iter()
                .map(|message| message.text.as_str())
                .collect::<Vec<_>>(),
            ["ask", "reply"]
        );
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
        assert_eq!(
            parsed
                .messages
                .iter()
                .map(|message| message.text.as_str())
                .collect::<Vec<_>>(),
            ["ping", "pong"]
        );
    }

    /// Grok meaningful/genuine counts must be scanned losslessly over the raw
    /// content blocks like Codex/Claude — not derived from the first-text
    /// projection: genuine text in a LATER block, an image after an envelope,
    /// and an image-only turn all count, while all-envelope and unsupported
    /// block shapes never do. The lossy projection itself is unchanged.
    #[test]
    fn grok_counts_scan_all_blocks_losslessly() {
        let fixtures = FixtureDir::new();
        let summary = fixtures.write(
            "grok/blocks/summary.json",
            "{\"info\":{\"id\":\"grok-blocks\",\"cwd\":\"<workspace>/grok\"},\"created_at\":\"2026-04-01T00:00:00Z\"}",
        );
        fixtures.write(
            "grok/blocks/chat_history.jsonl",
            concat!(
                // System records are neither user nor assistant: never counted.
                "{\"type\":\"system\",\"content\":\"ignore\"}\n",
                // Envelope first, genuine text in a LATER block.
                "{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"<environment_context>\\nagent mode\\n</environment_context>\"},{\"type\":\"text\",\"text\":\"fix the login bug\"}]}\n",
                // Envelope plus an image.
                "{\"type\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"<user_info>agent session</user_info>\"},{\"type\":\"image\"}]}\n",
                // Image-only turn: the first-text projection drops it.
                "{\"role\":\"user\",\"content\":[{\"type\":\"image\"}]}\n",
                // All-envelope content stays non-genuine.
                "{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"<user_info>bootstrap</user_info>\"},{\"type\":\"text\",\"text\":\"<system-reminder>ctx</system-reminder>\"}]}\n",
                // Unsupported block types are neither meaningful nor genuine.
                "{\"role\":\"user\",\"content\":[{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"bash\",\"input\":{\"command\":\"ls\"}}]}\n",
                // Plain genuine user prompt and assistant reply.
                "{\"role\":\"user\",\"content\":\"real prompt\"}\n",
                "{\"role\":\"assistant\",\"content\":\"reply\"}\n",
            ),
        );
        let parsed = parse_source(SourceSessionFormat::Grok, &summary).expect("parse Grok");
        // The first-text projection is unchanged: leading text per turn;
        // image-only and unsupported-only turns are dropped.
        assert_eq!(
            parsed
                .messages
                .iter()
                .map(|message| message.text.as_str())
                .collect::<Vec<_>>(),
            [
                "<environment_context>\nagent mode\n</environment_context>",
                "<user_info>agent session</user_info>",
                "<user_info>bootstrap</user_info>",
                "real prompt",
                "reply",
            ]
        );
        assert_eq!(parsed.messages.len(), 5, "projection drops image-only turn");
        // Meaningful: envelope+text, envelope+image, image-only, all-envelope,
        // plain prompt, reply = 6; the unsupported-only turn does not count.
        assert_eq!(parsed.meaningful_count, 6);
        // Genuine: envelope+text, envelope+image, image-only, plain prompt,
        // reply = 5; all-envelope and unsupported-only turns do not count.
        // The old first-text-derived count would have reported 2.
        assert_eq!(parsed.genuine_count, 5);
    }

    /// Same lossless raw-content count contract as Grok, exercised over the
    /// Droid `message` envelope shape: envelope+later text, envelope+image,
    /// image-only, all-envelope, and unsupported blocks.
    #[test]
    fn droid_counts_scan_all_blocks_losslessly() {
        let fixtures = FixtureDir::new();
        let path = fixtures.write(
            "droid-blocks.jsonl",
            concat!(
                "{\"type\":\"session_start\",\"id\":\"droid-blocks\",\"cwd\":\"<workspace>/droid\"}\n",
                "{\"type\":\"message\",\"timestamp\":\"2026-05-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"<environment_context>\\nagent mode\\n</environment_context>\"},{\"type\":\"text\",\"text\":\"fix the login bug\"}]}}\n",
                "{\"type\":\"message\",\"timestamp\":\"2026-05-01T00:00:02Z\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"<user_info>agent session</user_info>\"},{\"type\":\"image\"}]}}\n",
                "{\"type\":\"message\",\"timestamp\":\"2026-05-01T00:00:03Z\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"image\"}]}}\n",
                "{\"type\":\"message\",\"timestamp\":\"2026-05-01T00:00:04Z\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"<user_info>bootstrap</user_info>\"},{\"type\":\"text\",\"text\":\"<system-reminder>ctx</system-reminder>\"}]}}\n",
                "{\"type\":\"message\",\"timestamp\":\"2026-05-01T00:00:05Z\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"bash\",\"input\":{\"command\":\"ls\"}}]}}\n",
                "{\"type\":\"message\",\"timestamp\":\"2026-05-01T00:00:06Z\",\"message\":{\"role\":\"assistant\",\"content\":\"reply\"}}\n",
            ),
        );
        let parsed = parse_source(SourceSessionFormat::Droid, &path).expect("parse Droid");
        assert_eq!(
            parsed
                .messages
                .iter()
                .map(|message| message.text.as_str())
                .collect::<Vec<_>>(),
            [
                "<environment_context>\nagent mode\n</environment_context>",
                "<user_info>agent session</user_info>",
                "<user_info>bootstrap</user_info>",
                "reply",
            ]
        );
        assert_eq!(parsed.messages.len(), 4, "projection drops image-only turn");
        // Meaningful: envelope+text, envelope+image, image-only, all-envelope,
        // reply = 5; the unsupported-only turn does not count.
        assert_eq!(parsed.meaningful_count, 5);
        // Genuine: envelope+text, envelope+image, image-only, reply = 4;
        // all-envelope and unsupported-only turns do not count. The old
        // first-text-derived count would have reported 1.
        assert_eq!(parsed.genuine_count, 4);
    }
    #[test]
    fn directory_import_creates_missing_root_and_writes_child() {
        let fixtures = FixtureDir::new();
        let source = fixtures.write(
            "rollout-directory.jsonl",
            concat!(
                "{\"timestamp\":\"2026-06-01T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"directory-id\",\"cwd\":\"<workspace>/codex\"}}\n",
                "{\"timestamp\":\"2026-06-01T00:00:01Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"question\"}]}}\n",
                "{\"timestamp\":\"2026-06-01T00:00:02Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"answer\"}]}}\n",
            ),
        );
        let directory = fixtures.0.join("missing/nested/sessions");
        assert!(!directory.exists());

        let imported =
            import_session_into_directory(SourceSessionFormat::Codex, &source, &directory)
                .expect("import into missing directory");

        assert!(directory.is_dir());
        assert_eq!(imported.path.parent(), Some(directory.as_path()));
        assert!(imported.path.is_file());
    }

    #[cfg(unix)]
    #[test]
    fn directory_import_rejects_final_and_ancestor_symlink_roots() {
        use std::os::unix::fs::symlink;

        let fixtures = FixtureDir::new();
        let source = fixtures.write(
            "rollout-symlink-root.jsonl",
            concat!(
                "{\"timestamp\":\"2026-06-01T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"symlink-root\",\"cwd\":\"<workspace>/codex\"}}\n",
                "{\"timestamp\":\"2026-06-01T00:00:01Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"question\"}}\n",
            ),
        );
        let target = fixtures.0.join("symlink-target");
        fs::create_dir(&target).expect("symlink target");

        let final_link = fixtures.0.join("final-link");
        symlink(&target, &final_link).expect("final root symlink");
        let final_error =
            import_session_into_directory(SourceSessionFormat::Codex, &source, &final_link)
                .expect_err("final root symlink must fail closed");
        assert!(matches!(
            final_error,
            ImportSessionError::InvalidOutput { .. }
        ));

        let ancestor_link = fixtures.0.join("ancestor-link");
        symlink(&target, &ancestor_link).expect("ancestor root symlink");
        let ancestor_error = import_session_into_directory(
            SourceSessionFormat::Codex,
            &source,
            ancestor_link.join("nested"),
        )
        .expect_err("ancestor root symlink must fail closed");
        assert!(matches!(
            ancestor_error,
            ImportSessionError::InvalidOutput { .. }
        ));
        assert_eq!(fs::read_dir(&target).expect("target readable").count(), 0);
    }

    #[test]
    fn explicit_existing_output_is_not_overwritten() {
        let fixtures = FixtureDir::new();
        let source = fixtures.write(
            "rollout-no-overwrite.jsonl",
            concat!(
                "{\"timestamp\":\"2026-06-01T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"no-overwrite\",\"cwd\":\"<workspace>/codex\"}}\n",
                "{\"timestamp\":\"2026-06-01T00:00:01Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"question\"}}\n",
            ),
        );
        let output = fixtures.0.join("occupied.jsonl");
        fs::write(&output, "sentinel").expect("occupied output");

        let error = import_session_to(SourceSessionFormat::Codex, &source, &output)
            .expect_err("existing output must not be overwritten");
        assert!(matches!(
            error,
            ImportSessionError::Io { source, .. }
                if source.kind() == std::io::ErrorKind::AlreadyExists
        ));
        assert_eq!(
            fs::read_to_string(&output).expect("occupied output"),
            "sentinel"
        );
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
        let imported =
            import_session_to(SourceSessionFormat::Codex, &source, &output).expect("import Codex");
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
        assert_eq!(values[1]["type"], "custom");
        assert_eq!(values[1]["customType"], "import_lineage");
        assert_eq!(values[1]["data"]["source"], "codex");
        assert_eq!(values[1]["data"]["sourceSessionId"], "old-id");
        let texts: Vec<&str> = values
            .iter()
            .filter(|value| value["type"] == "message")
            .filter_map(|value| value["message"]["content"][0]["text"].as_str())
            .collect();
        assert_eq!(texts, ["question", "answer"]);
        let question = values
            .iter()
            .find(|value| {
                value["type"] == "message"
                    && value["message"]["content"][0]["text"] == "question"
            })
            .expect("question message");
        assert_eq!(question["timestamp"], "2026-06-01T00:00:01.000Z");
        assert!(
            values.iter().any(|value| {
                value["type"] == "message"
                    && value["message"]["content"]
                        .as_array()
                        .is_some_and(|blocks| {
                            blocks.iter().any(|block| block["type"] == "text")
                        })
            }),
            "converted Codex session must keep text blocks"
        );
    }

    #[test]
    fn grok_tool_turns_survive_import_session_to() {
        let fixtures = FixtureDir::new();
        let summary = fixtures.write(
            "sid/summary.json",
            r#"{"info":{"id":"grok-tools","cwd":"<workspace>/grok"},"created_at":"2026-04-01T00:00:00Z"}"#,
        );
        fixtures.write(
            "sid/chat_history.jsonl",
            concat!(
                r#"{"type":"user","content":[{"type":"text","text":"search it"}]}"#,
                "\n",
                r#"{"type":"backend_tool_call","kind":{"tool_type":"web_search","id":"ws_1","action":{"type":"search","query":"capybaras"}}}"#,
                "\n",
                r#"{"type":"tool_result","tool_call_id":"ws_1","content":"found"}"#,
                "\n",
                r#"{"role":"assistant","content":"done","created_at":"2026-04-01T00:00:02Z"}"#,
                "\n",
            ),
        );
        let output = fixtures.0.join("grok-converted.jsonl");
        let imported =
            import_session_to(SourceSessionFormat::Grok, &summary, &output).expect("import Grok");
        let body = fs::read_to_string(&imported.path).expect("read converted");
        assert!(
            body.contains("\"type\":\"toolCall\"")
                && body.contains("web_search")
                && body.contains("ws_1")
                && body.contains("search it"),
            "imported Grok JSONL must keep the tool turn, got {body}"
        );
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

        let output = fixtures.0.join("converted-from-corrupt-pi.jsonl");
        let imported =
            import_session_to(SourceSessionFormat::Pi, &path, &output).expect("skip malformed line");
        let body = fs::read_to_string(&imported.path).expect("read converted");
        assert!(body.contains("question") && body.contains("answer"), "{body}");
        assert!(!body.contains("this line is not JSON"), "{body}");
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

        let output = fixtures.0.join("converted-from-corrupt-codex.jsonl");
        let imported = import_session_to(SourceSessionFormat::Codex, &path, &output)
            .expect("skip malformed line");
        let body = fs::read_to_string(&imported.path).expect("read converted");
        assert!(body.contains("build it"), "{body}");
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
        let parsed =
            parse_source(SourceSessionFormat::Codex, &codex).expect("parse Codex with blanks");
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

    // -----------------------------------------------------------------------
    // Validated source record id-only lookup: exact id, prefix, ambiguity,
    // malformed input, and root containment.
    // -----------------------------------------------------------------------

    fn native_header(id: &str) -> String {
        format!(
            "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"<workspace>/project\"}}\n"
        )
    }

    fn codex_header(id: &str) -> String {
        format!(
            "{{\"timestamp\":\"2026-02-01T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"<workspace>/codex\"}}}}\n"
        )
    }

    /// Exact record id lookup must resolve a native file whose timestamped
    /// filename does not itself contain the id (`{timestamp}_{id}.jsonl`).
    #[test]
    fn id_lookup_resolves_exact_record_id_for_timestamped_native_names() {
        let root = tempfile::tempdir().expect("lookup root");
        let session = root.path().join("2026-01-01T00-00-00-000Z_abc-123.jsonl");
        fs::write(&session, native_header("abc-123")).expect("native session");
        let resolved = resolve_id_lookup(SourceSessionFormat::Pi, root.path(), "abc-123")
            .expect("exact record id resolves");
        assert_eq!(resolved, session);
    }

    /// A record id prefix resolves uniquely when no other candidate shares it.
    #[test]
    fn id_lookup_prefix_matches_unique_record_id() {
        let root = tempfile::tempdir().expect("lookup root");
        let session = root.path().join("2026-03-03T00-00-00-000Z_alpha-one.jsonl");
        fs::write(&session, native_header("alpha-one")).expect("native session");
        let other = root.path().join("2026-04-04T00-00-00-000Z_beta.jsonl");
        fs::write(&other, native_header("beta")).expect("native session");
        let resolved = resolve_id_lookup(SourceSessionFormat::Pi, root.path(), "alph")
            .expect("unique record id prefix resolves");
        assert_eq!(resolved, session);
    }

    /// A filename-stem prefix still resolves for timestamped native names.
    #[test]
    fn id_lookup_prefix_matches_native_filename_stem() {
        let root = tempfile::tempdir().expect("lookup root");
        let session = root.path().join("2026-05-05T00-00-00-000Z_abc.jsonl");
        fs::write(&session, native_header("abc")).expect("native session");
        let resolved = resolve_id_lookup(SourceSessionFormat::Pi, root.path(), "2026-05")
            .expect("timestamp stem prefix resolves");
        assert_eq!(resolved, session);
    }

    /// A shared prefix across two records is rejected as ambiguous, including
    /// when one record's exact id equals the other's prefix.
    #[test]
    fn id_lookup_rejects_ambiguous_prefix() {
        let root = tempfile::tempdir().expect("lookup root");
        let first = root.path().join("rollout-alpha-one.jsonl");
        fs::write(&first, codex_header("alpha-one")).expect("codex one");
        let second = root.path().join("rollout-alpha-two.jsonl");
        fs::write(&second, codex_header("alpha-two")).expect("codex two");
        match resolve_id_lookup(SourceSessionFormat::Codex, root.path(), "alpha")
            .expect_err("shared prefix must be ambiguous")
        {
            ImportSessionError::AmbiguousSession {
                format,
                input,
                matches,
            } => {
                assert_eq!(format, SourceSessionFormat::Codex);
                assert_eq!(input, "alpha");
                assert_eq!(matches.len(), 2, "matches={matches:?}");
                assert!(matches.contains(&first));
                assert!(matches.contains(&second));
            }
            other => panic!("expected AmbiguousSession, got {other:?}"),
        }
    }

    /// An exact record id still resolves when the shared prefix is ambiguous.
    #[test]
    fn exact_record_id_beats_shared_prefix_ambiguity() {
        let root = tempfile::tempdir().expect("lookup root");
        let shared = root.path().join("rollout-shared.jsonl");
        fs::write(&shared, codex_header("shared")).expect("shared");
        let extra = root.path().join("rollout-shared-extra.jsonl");
        fs::write(&extra, codex_header("shared-extra")).expect("extra");
        let error = resolve_id_lookup(SourceSessionFormat::Codex, root.path(), "shared")
            .expect_err("prefix sharing the exact shorter id stays ambiguous");
        assert!(
            matches!(error, ImportSessionError::AmbiguousSession { .. }),
            "{error:?}"
        );
        let resolved = resolve_id_lookup(SourceSessionFormat::Codex, root.path(), "shared-extra")
            .expect("exact id resolves");
        assert_eq!(resolved, extra);
    }

    /// Malformed id inputs fail closed with a typed input error before any
    /// root scan; the error names the requesting source format.
    #[test]
    fn malformed_id_lookup_inputs_fail_closed() {
        for (input, source) in [
            (Path::new(""), SourceSessionFormat::Pi),
            (Path::new("."), SourceSessionFormat::Pi),
            (Path::new(".."), SourceSessionFormat::Pi),
            (Path::new("a/b"), SourceSessionFormat::Pi),
            (Path::new("a\\b"), SourceSessionFormat::Pi),
            (Path::new("a b"), SourceSessionFormat::Pi),
            (Path::new("a\tb"), SourceSessionFormat::Pi),
        ] {
            let error = resolve_input(source, input).expect_err("malformed id input");
            assert!(
                matches!(error, ImportSessionError::InvalidInput { .. }),
                "{input:?} => {error:?}"
            );
        }
        let del = resolve_input(SourceSessionFormat::Codex, Path::new("x\x7f"))
            .expect_err("control character");
        assert!(matches!(del, ImportSessionError::InvalidInput { .. }));
        let message = resolve_input(SourceSessionFormat::Codex, Path::new("a/b"))
            .expect_err("separator input")
            .to_string();
        assert!(
            message.contains("codex"),
            "error names the source: {message}"
        );

        let long = "a".repeat(MAX_SESSION_ID_INPUT_BYTES + 1);
        let error = resolve_input(SourceSessionFormat::Pi, Path::new(&long))
            .expect_err("oversized id input");
        assert!(
            matches!(error, ImportSessionError::InvalidInput { .. }),
            "{error:?}"
        );
    }

    /// Non-UTF-8 id inputs are malformed and fail closed.
    #[cfg(unix)]
    #[test]
    fn malformed_non_utf8_id_input_fails_closed() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let input = PathBuf::from(OsString::from_vec(vec![0xff, 0xfe]));
        let error = resolve_input(SourceSessionFormat::Pi, &input).expect_err("non-UTF-8 id input");
        assert!(
            matches!(error, ImportSessionError::InvalidInput { .. }),
            "{error:?}"
        );
    }

    /// The id lookup is contained under the configured source root: a record
    /// with the same name and id outside the root never matches, and an
    /// absolute path input is never re-scanned as an id.
    #[test]
    fn id_lookup_stays_contained_under_source_root() {
        let root = tempfile::tempdir().expect("lookup root");
        let inside = root.path().join("rollout-target.jsonl");
        fs::write(&inside, codex_header("target")).expect("inside session");

        let outside = tempfile::tempdir().expect("outside root");
        let outside_file = outside.path().join("rollout-target.jsonl");
        fs::write(&outside_file, codex_header("target")).expect("outside session");

        let resolved = resolve_id_lookup(SourceSessionFormat::Codex, root.path(), "target")
            .expect("contained lookup");
        assert_eq!(resolved, inside);
        assert!(resolved.starts_with(root.path()));
        assert_ne!(resolved, outside_file);

        // Explicit absolute paths are authorized direct imports regardless of
        // the source root; containment governs the id scan, not user-supplied
        // absolute paths.
        let direct = resolve_input(SourceSessionFormat::Codex, &outside_file)
            .expect("absolute direct path imports");
        assert_eq!(direct, outside_file);

        // An absolute path that fails normalization is a typed not-found
        // error, never reinterpreted as an id scan of the source tree.
        let notes = outside.path().join("notes.txt");
        fs::write(&notes, "not a session").expect("notes");
        let error = resolve_input(SourceSessionFormat::Codex, &notes)
            .expect_err("unrecognized absolute path must not resolve");
        assert!(
            matches!(
                error,
                ImportSessionError::SessionNotFound {
                    format: SourceSessionFormat::Codex,
                    ..
                }
            ),
            "{error:?}"
        );
    }

    /// A symlink anywhere in the source tree fails the bounded enumeration
    /// closed, so lookup can never follow a link out of the root.
    #[cfg(unix)]
    #[test]
    fn id_lookup_fails_closed_on_symlink_entries() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().expect("lookup root");
        fs::write(
            root.path().join("rollout-target.jsonl"),
            codex_header("target"),
        )
        .expect("inside session");
        let outside = tempfile::tempdir().expect("outside root");
        let outside_file = outside.path().join("rollout-outside.jsonl");
        fs::write(&outside_file, codex_header("target")).expect("outside session");
        symlink(&outside_file, root.path().join("rollout-link.jsonl")).expect("symlink");

        let error = resolve_id_lookup(SourceSessionFormat::Codex, root.path(), "target")
            .expect_err("symlink enumeration must fail closed");
        assert!(
            matches!(error, ImportSessionError::SessionNotFound { .. }),
            "{error:?}"
        );
    }
}
