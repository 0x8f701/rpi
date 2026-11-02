use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::{
    ImportSessionError, ImportedMessage, ImportedMessageRole, OpenedSource, OpenedSourceParts,
    SourceSessionFormat,
};
use crate::session_catalog::text_has_genuine_content;

/// Per-file source cap applied to every member of a parsed source, including
/// OMP rotation chain members. Exposed crate-wide so the catalog's OMP chain
/// probe rejects oversize members before scanning their headers.
pub(crate) const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
const MAX_RECORDS: usize = 250_000;
const MAX_CWD_SIDECAR_BYTES: u64 = 16 * 1024;

/// Maximum total bytes the OMP parent-session header probe scans before
/// failing closed: two full-size JSONL records (optional title slot + session
/// record) plus their trailing newlines. A file that cannot yield its session
/// record within this budget is treated as malformed instead of being read to
/// EOF.
pub(crate) const MAX_HEADER_SCAN_BYTES: u64 = 2 * MAX_LINE_BYTES as u64 + 2;
/// Maximum physical JSONL records the OMP parent-session header probe scans
/// before failing closed (same convention as the full-file record bound), so
/// files padded with empty/non-object JSON records cannot drive the probe
/// unboundedly.
pub(crate) const MAX_HEADER_SCAN_RECORDS: usize = MAX_RECORDS;

#[derive(Debug)]
pub(super) struct ParsedSession {
    pub source_session_id: Option<String>,
    pub cwd: PathBuf,
    pub started_at: Option<String>,
    pub messages: Vec<ImportedMessage>,
    /// User/assistant turns with meaningful content (text or a supported
    /// non-text image block). Independent of the lossy text projection in
    /// `messages`: every foreign format (Codex, Claude, Grok, Droid) computes
    /// it from the raw content of each turn.
    pub meaningful_count: usize,
    /// Meaningful turns carrying genuine (non-bootstrap) content: any
    /// non-envelope text block or supported non-text image block, scanned
    /// losslessly over every content block — not the first-text projection.
    /// Uniform across Codex, Claude, Grok, and Droid.
    pub genuine_count: usize,
}

#[derive(Debug)]
struct TreeNode {
    id: String,
    parent_id: Option<String>,
    role: Option<ImportedMessageRole>,
    text: Option<String>,
    timestamp: Option<String>,
    entry_type: Option<String>,
    first_kept_entry_id: Option<String>,
    meaningful: bool,
    genuine: bool,
}

pub(super) fn parse_source(
    source: SourceSessionFormat,
    path: &Path,
) -> Result<ParsedSession, ImportSessionError> {
    let opened = super::open_source_direct(source, path)?;
    parse_opened_source(source, opened)
}

pub(super) fn parse_opened_source(
    source: SourceSessionFormat,
    opened: OpenedSource,
) -> Result<ParsedSession, ImportSessionError> {
    let OpenedSourceParts {
        path,
        primary,
        grok_chat,
        grok_cwd,
    } = opened.into_parts();
    match source {
        SourceSessionFormat::Pi | SourceSessionFormat::Omp => parse_native(source, &path, primary),
        SourceSessionFormat::Codex => parse_codex(&path, primary),
        SourceSessionFormat::Claude => parse_claude(&path, primary),
        SourceSessionFormat::Grok => parse_grok(&path, primary, grok_chat, grok_cwd),
        SourceSessionFormat::Droid => parse_droid(&path, primary),
    }
}

pub(super) fn source_id(
    source: SourceSessionFormat,
    path: &Path,
) -> Result<Option<String>, ImportSessionError> {
    let opened = super::open_source_direct(source, path)?;
    source_id_opened(source, opened)
}

pub(super) fn source_id_opened(
    source: SourceSessionFormat,
    opened: OpenedSource,
) -> Result<Option<String>, ImportSessionError> {
    let OpenedSourceParts { path, primary, .. } = opened.into_parts();
    match source {
        SourceSessionFormat::Grok => {
            let value = read_json(primary, &path)?;
            let id = value
                .pointer("/info/id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned);
            drop_json_value_iterative(value);
            Ok(id)
        }
        _ => {
            let values = read_jsonl(primary, &path)?;
            let id = match source {
                SourceSessionFormat::Pi | SourceSessionFormat::Omp => values
                    .iter()
                    .find(|value| value.get("type").and_then(Value::as_str) == Some("session"))
                    .and_then(|value| value.get("id"))
                    .and_then(Value::as_str),
                SourceSessionFormat::Codex => values
                    .iter()
                    .find(|value| value.get("type").and_then(Value::as_str) == Some("session_meta"))
                    .and_then(|value| value.get("payload"))
                    .and_then(|value| value.get("id").or_else(|| value.get("session_id")))
                    .and_then(Value::as_str),
                SourceSessionFormat::Claude => values.iter().find_map(|value| {
                    value
                        .get("sessionId")
                        .or_else(|| value.get("session_id"))
                        .and_then(Value::as_str)
                }),
                SourceSessionFormat::Droid => values
                    .iter()
                    .find(|value| {
                        value.get("type").and_then(Value::as_str) == Some("session_start")
                    })
                    .and_then(|value| value.get("id"))
                    .and_then(Value::as_str),
                SourceSessionFormat::Grok => unreachable!(),
            };
            let id = id.filter(|id| !id.is_empty()).map(str::to_owned);
            drop_jsonl_values_iterative(values);
            Ok(id)
        }
    }
}

fn parse_native(
    source: SourceSessionFormat,
    path: &Path,
    file: fs::File,
) -> Result<ParsedSession, ImportSessionError> {
    let native = read_native_turns(source, path, file)?;
    let (messages, meaningful_count, genuine_count) = project_turns(&native.turns);
    Ok(ParsedSession {
        source_session_id: native.source_session_id,
        cwd: native.cwd,
        started_at: native.started_at,
        messages,
        meaningful_count,
        genuine_count,
    })
}

/// Active-path turns of one native/OMP file, retaining the node identity and
/// meaningfulness needed by rotation-chain merging.
struct NativeTurns {
    source_session_id: Option<String>,
    cwd: PathBuf,
    started_at: Option<String>,
    /// Raw `parentSession` header value (the prior rotated file reference).
    parent_session: Option<String>,
    turns: Vec<ActiveTurn>,
}

fn read_native_turns(
    source: SourceSessionFormat,
    path: &Path,
    file: fs::File,
) -> Result<NativeTurns, ImportSessionError> {
    let contents = read_bounded_text_from(file, path, MAX_SOURCE_BYTES)?;
    read_native_turns_from_contents(source, path, &contents)
}

fn read_native_turns_from_contents(
    source: SourceSessionFormat,
    path: &Path,
    contents: &str,
) -> Result<NativeTurns, ImportSessionError> {
    if contents.trim().is_empty() {
        return Err(ImportSessionError::NoConvertibleMessages {
            format: source,
            path: path.to_path_buf(),
        });
    }
    let mut records = native_jsonl_records(source, path, contents);
    let first =
        records
            .next()
            .transpose()?
            .ok_or_else(|| ImportSessionError::InvalidNativeHeader {
                format: source,
                path: path.to_path_buf(),
            })?;
    let header = if source == SourceSessionFormat::Omp && valid_omp_title_slot(&first) {
        drop_json_value_iterative(first);
        records
            .next()
            .transpose()?
            .ok_or_else(|| ImportSessionError::InvalidNativeHeader {
                format: source,
                path: path.to_path_buf(),
            })?
    } else {
        first
    };
    let header_fields = header.as_object().and_then(|header| {
        let is_valid = header.get("type").and_then(Value::as_str) == Some("session")
            && header
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty());
        is_valid.then(|| {
            (
                header.get("id").and_then(Value::as_str).map(str::to_owned),
                header
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
                    .unwrap_or_default(),
                header
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                header
                    .get("parentSession")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned),
            )
        })
    });
    drop_json_value_iterative(header);
    let Some((source_session_id, cwd, started_at, parent_session)) = header_fields else {
        return Err(ImportSessionError::InvalidNativeHeader {
            format: source,
            path: path.to_path_buf(),
        });
    };
    let mut nodes = Vec::new();
    for value in records {
        let value = value?;
        if let Some(node) = native_node(source, &value) {
            nodes.push(node);
        }
        drop_json_value_iterative(value);
    }
    Ok(NativeTurns {
        source_session_id,
        cwd,
        started_at,
        parent_session,
        turns: active_turns(&nodes),
    })
}

/// One ordered active-path turn of a native/OMP file.
#[derive(Debug, Clone)]
struct ActiveTurn {
    /// Record `id`, used to deduplicate entries copied across rotated files.
    id: String,
    meaningful: bool,
    genuine: bool,
    message: Option<ImportedMessage>,
}

/// One parsed chain member with the identity and linkage needed for
/// adjacency revalidation.
struct ParsedChainMember {
    path: PathBuf,
    parent_session: Option<String>,
    /// SHA-256 of this member's exact bytes, read from its capability-opened
    /// descriptor (domain-separated; see [`member_content_digest`]).
    digest: [u8; 32],
    native: NativeTurns,
}

/// SHA-256 of one chain member's exact bytes, domain-separated so member
/// digests are unambiguous regardless of where one file's bytes end and the
/// next begins.
fn member_content_digest(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"pi-rs-omp-chain-member-v1\0");
    hasher.update(bytes);
    hasher.finalize().into()
}

/// Chain content fingerprint: SHA-256 over the count-prefixed, root → leaf
/// ordered member digests. The explicit count plus fixed-size digests make the
/// framing unambiguous, so any accepted-member byte change (including
/// same-length rewrites with restored mtimes) or accepted-set change yields a
/// different fingerprint.
fn chain_fingerprint(digests: &[[u8; 32]]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"pi-rs-omp-chain-v1\0");
    hasher.update((digests.len() as u64).to_le_bytes());
    for digest in digests {
        hasher.update(digest);
    }
    hex_encode(&hasher.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Parse a rotation chain of OMP session files (root → leaf order) into one
/// logical session. The leaf's header identity and cwd anchor the result; the
/// root file anchors the start time. Message entries duplicated across files
/// (e.g. `createBranchedSession` copies entries with their original ids) are
/// kept once, from their earliest file.
///
/// Every candidate member is opened through the configured root capability
/// ONCE and parsed from that same descriptor — no ambient reopens. The opened
/// descriptors provide the authoritative aggregate byte budget (the newest
/// chain prefix that fits `max_bytes` is retained; the leaf always stays).
/// An ancestor that cannot be opened or parsed is excluded, and the adjacency
/// revalidation below then drops it and everything older, retaining the safe
/// newest prefix instead of failing the leaf import. Linkage is revalidated
/// against the parsed content itself: a member is kept only when the
/// next-newer member's `parentSession` equals its path, so a header swap
/// between traversal and parse fails closed instead of stitching unrelated
/// files together. The returned fingerprint is a SHA-256 over the accepted
/// members' exact bytes (read from the same capability-opened descriptors,
/// count-prefixed and root → leaf ordered), so any accepted-member byte
/// change — including a same-length rewrite with a restored mtime — yields a
/// different chain identity.
pub(super) fn parse_omp_chain(
    root: &Path,
    paths: &[PathBuf],
    max_bytes: u64,
) -> Result<(ParsedSession, String), ImportSessionError> {
    if paths.is_empty() {
        return Err(ImportSessionError::InvalidNativeHeader {
            format: SourceSessionFormat::Omp,
            path: PathBuf::from("<empty chain>"),
        });
    }
    let mut opened = Vec::with_capacity(paths.len());
    for (index, path) in paths.iter().enumerate() {
        let is_leaf = index + 1 == paths.len();
        match super::open_source_under_root(SourceSessionFormat::Omp, root, path) {
            Ok(member) => opened.push(Some(member)),
            // An unreadable ancestor cannot be trusted; exclude it and let the
            // adjacency pass retain the safe newest prefix.
            Err(_) if !is_leaf => opened.push(None),
            Err(error) => return Err(error),
        }
    }
    // Keep the newest chain prefix that fits the aggregate budget, measured
    // from the opened descriptors (authoritative revalidation; the catalog's
    // traversal-side check only bounds the walk). The leaf is unconditional.
    let mut retained = Vec::new();
    let mut total_bytes = 0_u64;
    for (index, member) in opened.into_iter().enumerate().rev() {
        let Some(member) = member else {
            continue;
        };
        let size = member.metadata().len();
        let is_leaf = index + 1 == paths.len();
        if !is_leaf && total_bytes.saturating_add(size) > max_bytes {
            break;
        }
        total_bytes = total_bytes.saturating_add(size);
        retained.push(member);
    }
    retained.reverse();

    // Parse every retained member from its own opened descriptor; an ancestor
    // parse failure excludes the member (the adjacency pass drops it and
    // everything older), while a leaf failure is fatal. The exact bytes read
    // for parsing are also hashed (no reopen), producing the per-member
    // content digest used by the chain fingerprint.
    let retained_len = retained.len();
    let mut members = Vec::with_capacity(retained_len);
    for (index, member) in retained.into_iter().enumerate() {
        let is_leaf = index + 1 == retained_len;
        let OpenedSourceParts { path, primary, .. } = member.into_parts();
        let parsed = (|| -> Result<ParsedChainMember, ImportSessionError> {
            let bytes = read_bounded_bytes_from(primary, &path, MAX_SOURCE_BYTES)?;
            let contents = std::str::from_utf8(&bytes)
                .map_err(|_| resource_limit(&path, "source is not valid UTF-8".to_owned()))?;
            let native =
                read_native_turns_from_contents(SourceSessionFormat::Omp, &path, contents)?;
            Ok(ParsedChainMember {
                path,
                parent_session: native.parent_session.clone(),
                digest: member_content_digest(&bytes),
                native,
            })
        })();
        match parsed {
            Ok(member) => members.push(member),
            Err(_) if !is_leaf => {}
            Err(error) => return Err(error),
        }
    }

    // Fail closed on linkage drift: keep the newest prefix where every
    // retained member is referenced by the next-newer member's parsed
    // `parentSession` (same descriptor/content that is being imported). The
    // leaf is unconditional; a broken link (or an excluded failed member)
    // drops that member and everything older instead of stitching unrelated
    // files together.
    let mut accepted = vec![&members[members.len() - 1]];
    let mut cursor = members.len() - 1;
    while cursor > 0 {
        let newer = &members[cursor];
        let predecessor = &members[cursor - 1];
        let linked = newer
            .parent_session
            .as_deref()
            .is_some_and(|parent| parent == predecessor.path.to_str().unwrap_or_default());
        if !linked {
            break;
        }
        accepted.push(predecessor);
        cursor -= 1;
    }
    accepted.reverse();

    let accepted_len = accepted.len();
    let mut turns = Vec::new();
    let mut seen_ids = HashSet::new();
    let mut source_session_id = None;
    let mut cwd = PathBuf::new();
    let mut started_at = None;
    let mut digests = Vec::with_capacity(accepted_len);
    for (index, member) in accepted.into_iter().enumerate() {
        if index + 1 == accepted_len {
            source_session_id = member.native.source_session_id.clone();
            cwd = member.native.cwd.clone();
        }
        if index == 0 {
            started_at = member.native.started_at.clone();
        }
        digests.push(member.digest);
        for turn in &member.native.turns {
            if seen_ids.insert(turn.id.clone()) {
                turns.push(turn.clone());
            }
        }
    }
    let (messages, meaningful_count, genuine_count) = project_turns(&turns);
    let fingerprint = chain_fingerprint(&digests);
    Ok((
        ParsedSession {
            source_session_id,
            cwd,
            started_at,
            messages,
            meaningful_count,
            genuine_count,
        },
        fingerprint,
    ))
}
/// Read the OMP `parentSession` header reference (absolute path of the prior
/// rotated session file) from an already-secured handle. Non-OMP sources and
/// files without the field return `None`; malformed headers fail closed.
pub(super) fn source_parent_session_opened(
    source: SourceSessionFormat,
    opened: &OpenedSource,
) -> Result<Option<String>, ImportSessionError> {
    if source != SourceSessionFormat::Omp {
        return Ok(None);
    }
    read_omp_parent_session(opened.primary_ref(), opened.path())
}

/// Bounded header read: the OMP session record is line 1, or line 2 behind the
/// title slot, so only the first two records are parsed. The scan is capped by
/// both a cumulative byte budget and a physical-record budget so files padded
/// with empty/non-object JSON records fail closed instead of being read to
/// EOF.
fn read_omp_parent_session(
    file: &fs::File,
    path: &Path,
) -> Result<Option<String>, ImportSessionError> {
    let mut reader = BufReader::new(file);
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|source| ImportSessionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    let mut values = IterativeDropGuard::new();
    let mut buffer = Vec::new();
    let mut bytes_read = 0_u64;
    let mut records = 0_usize;
    while values.0.len() < 2 {
        buffer.clear();
        let read = reader
            .by_ref()
            .take(MAX_LINE_BYTES.saturating_add(3) as u64)
            .read_until(b'\n', &mut buffer)
            .map_err(|source| ImportSessionError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        if read == 0 {
            break;
        }
        bytes_read = bytes_read.saturating_add(read as u64);
        if bytes_read > MAX_HEADER_SCAN_BYTES {
            return Err(resource_limit(
                path,
                format!("header scan exceeds {MAX_HEADER_SCAN_BYTES} bytes"),
            ));
        }
        let mut frame_bytes = buffer.len();
        if buffer.get(frame_bytes.wrapping_sub(1)) == Some(&b'\n') {
            frame_bytes -= 1;
            if buffer.get(frame_bytes.wrapping_sub(1)) == Some(&b'\r') {
                frame_bytes -= 1;
            }
        }
        if frame_bytes > MAX_LINE_BYTES {
            return Err(resource_limit(
                path,
                format!("JSONL line exceeds {MAX_LINE_BYTES} bytes"),
            ));
        }
        let line = std::str::from_utf8(&buffer[..frame_bytes])
            .map_err(|_| resource_limit(path, "source is not valid UTF-8".to_owned()))?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if records == MAX_HEADER_SCAN_RECORDS {
            return Err(resource_limit(
                path,
                format!("header scan exceeds {MAX_HEADER_SCAN_RECORDS} JSON records"),
            ));
        }
        records += 1;
        match serde_json::from_str::<Value>(line) {
            Ok(value) if value.is_object() => values.push(value),
            Ok(value) => drop_json_value_iterative(value),
            Err(source) => {
                return Err(ImportSessionError::Json {
                    path: path.to_path_buf(),
                    source,
                });
            }
        }
    }
    let values = values.into_inner();
    let mut parent_session = None;
    for value in values {
        if parent_session.is_none() && value.get("type").and_then(Value::as_str) == Some("session")
        {
            parent_session = value
                .get("parentSession")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
        }
        drop_json_value_iterative(value);
    }
    Ok(parent_session)
}

#[cfg(test)]
thread_local! {
    static ITERATIVE_VALUE_DROPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static ITERATIVE_GUARD_DROPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_iterative_drop_probes() {
    ITERATIVE_VALUE_DROPS.set(0);
    ITERATIVE_GUARD_DROPS.set(0);
}

#[cfg(test)]
fn iterative_drop_probes() -> (usize, usize) {
    (ITERATIVE_VALUE_DROPS.get(), ITERATIVE_GUARD_DROPS.get())
}

fn drop_json_value_iterative(value: Value) {
    #[cfg(test)]
    ITERATIVE_VALUE_DROPS.set(ITERATIVE_VALUE_DROPS.get() + 1);
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            Value::Array(values) => pending.extend(values),
            Value::Object(object) => pending.extend(object.into_iter().map(|(_, value)| value)),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }
}

fn drop_jsonl_values_iterative(values: Vec<Value>) {
    for value in values {
        drop_json_value_iterative(value);
    }
}

/// Ensures accumulated `Vec<Value>` is iteratively destructed on any exit
/// path, including early `return Err` and `?` propagation. On success,
/// call `.into_inner()` to extract the owned `Vec` (which disables the
/// iterative `Drop`).
struct IterativeDropGuard(Vec<Value>);

impl IterativeDropGuard {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn push(&mut self, value: Value) {
        self.0.push(value);
    }
    fn into_inner(mut self) -> Vec<Value> {
        std::mem::take(&mut self.0)
    }
}

impl Drop for IterativeDropGuard {
    fn drop(&mut self) {
        #[cfg(test)]
        ITERATIVE_GUARD_DROPS.set(ITERATIVE_GUARD_DROPS.get() + 1);
        while let Some(value) = self.0.pop() {
            drop_json_value_iterative(value);
        }
    }
}

fn valid_omp_title_slot(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("title")
        && value.get("v").and_then(Value::as_u64) == Some(1)
        && value.get("title").and_then(Value::as_str).is_some()
        && value.get("updatedAt").and_then(Value::as_str).is_some()
        && value.get("pad").and_then(Value::as_str).is_some()
}

fn native_node(source: SourceSessionFormat, value: &Value) -> Option<TreeNode> {
    let object = value.as_object()?;
    let id = object.get("id").and_then(Value::as_str)?.to_owned();
    if id.is_empty() {
        return None;
    }
    let parent_id = object
        .get("parentId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned);
    let timestamp = string_field(object, &["timestamp"]);
    let entry_type = object
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let first_kept_entry_id = object
        .get("firstKeptEntryId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned);
    let message = object.get("message").and_then(Value::as_object);
    let role = message
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str)
        .and_then(parse_role);
    let content = message.and_then(|message| message.get("content"));
    let text = content.and_then(first_text).map(str::to_owned);
    // Meaningful: a user/assistant turn with non-empty text OR a supported
    // non-text content block (image attachment). Empty pending assistant
    // placeholders (no content blocks) and tool/system roles are not meaningful.
    let meaningful =
        role.is_some()
            && content.is_some_and(|content| message_content_meaningful(source, content));
    // Genuine: the turn carries non-bootstrap content — a supported non-text
    // block (image) or at least one text block that is not an injected
    // envelope. Scanned over ALL blocks, not the first-text projection.
    let genuine =
        role.is_some() && content.is_some_and(|content| message_content_genuine(source, content));
    Some(TreeNode {
        id,
        parent_id,
        role,
        text,
        timestamp,
        entry_type,
        first_kept_entry_id,
        meaningful,
        genuine,
    })
}
fn active_messages(nodes: &[TreeNode]) -> (Vec<ImportedMessage>, usize, usize) {
    project_turns(&active_turns(nodes))
}

/// Project ordered active-path turns into the lossy message list, the
/// meaningful-turn count, and the genuine-turn count.
fn project_turns(turns: &[ActiveTurn]) -> (Vec<ImportedMessage>, usize, usize) {
    let meaningful_count = turns.iter().filter(|turn| turn.meaningful).count();
    let genuine_count = turns.iter().filter(|turn| turn.genuine).count();
    let messages = turns
        .iter()
        .filter_map(|turn| turn.message.clone())
        .collect::<Vec<_>>();
    (messages, meaningful_count, genuine_count)
}

/// Ordered active-path turns (root → leaf) of one native/OMP file, retaining
/// node identity and meaningfulness for rotation-chain merging.
fn active_turns(nodes: &[TreeNode]) -> Vec<ActiveTurn> {
    let by_id = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let Some(mut cursor) = nodes.len().checked_sub(1) else {
        return Vec::new();
    };
    let mut path = Vec::new();
    let mut visited = HashSet::new();
    loop {
        let node = &nodes[cursor];
        if !visited.insert(node.id.as_str()) {
            break;
        }
        path.push(cursor);
        let Some(parent) = node.parent_id.as_deref() else {
            break;
        };
        let Some(parent_index) = by_id.get(parent) else {
            break;
        };
        cursor = *parent_index;
    }
    path.reverse();
    let start = path
        .iter()
        .enumerate()
        .rev()
        .find(|(_, index)| nodes[**index].entry_type.as_deref() == Some("compaction"))
        .map_or(0, |(position, index)| {
            nodes[*index]
                .first_kept_entry_id
                .as_deref()
                .and_then(|kept_id| {
                    path[..position]
                        .iter()
                        .position(|candidate| nodes[*candidate].id == kept_id)
                })
                .unwrap_or(position)
        });
    path[start..]
        .iter()
        .map(|index| {
            let node = &nodes[*index];
            let message = match (node.role, node.text.clone()) {
                (Some(role), Some(text)) => Some(ImportedMessage {
                    role,
                    text,
                    timestamp: node.timestamp.clone(),
                }),
                _ => None,
            };
            ActiveTurn {
                id: node.id.clone(),
                meaningful: node.meaningful,
                genuine: node.genuine,
                message,
            }
        })
        .collect()
}

/// Whether a content block is a supported non-text image attachment for
/// `source`. The Codex/OpenAI Responses wire shape (`parse_codex`) carries
/// user-provided images as `input_image`; every other format (native/OMP,
/// Claude, Grok, Droid) uses `image`. Matching is scoped per format so an
/// arbitrary block type — including a foreign shape like `image` inside a
/// Codex rollout — is never misclassified as image content.
fn supported_image_block(source: SourceSessionFormat, block_type: &str) -> bool {
    match source {
        SourceSessionFormat::Codex => block_type == "input_image",
        _ => block_type == "image",
    }
}

/// Whether message content carries a meaningful user/assistant turn: non-empty
/// text or a supported non-text image block (`supported_image_block`). Mirrors
/// `crate::session_catalog::lineage::message_meaningful` for the import parser.
fn message_content_meaningful(source: SourceSessionFormat, content: &Value) -> bool {
    match content {
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(blocks) => blocks.iter().any(|block| {
            let Some(block_type) = block.get("type").and_then(Value::as_str) else {
                return false;
            };
            if supported_image_block(source, block_type) {
                return true;
            }
            matches!(block_type, "text" | "input_text" | "output_text")
                && block
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.trim().is_empty())
        }),
        _ => false,
    }
}

/// Whether message content carries genuine (non-bootstrap) content: a
/// supported non-text image block (`supported_image_block`) always counts,
/// while text counts only when at least one block is not an injected-context
/// envelope (`<environment_context>`, `<user_info>`, `<system-reminder>`,
/// AGENTS/project instruction blocks). Scanned over ALL blocks — the lossless
/// per-turn signal the first-text projection cannot express. Mirrors
/// `crate::session_catalog::lineage::message_genuine` for the import parser.
fn message_content_genuine(source: SourceSessionFormat, content: &Value) -> bool {
    match content {
        Value::String(text) => text_has_genuine_content(text),
        Value::Array(blocks) => blocks.iter().any(|block| {
            let Some(block_type) = block.get("type").and_then(Value::as_str) else {
                return false;
            };
            if supported_image_block(source, block_type) {
                return true;
            }
            matches!(block_type, "text" | "input_text" | "output_text")
                && block
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(text_has_genuine_content)
        }),
        _ => false,
    }
}

fn parse_codex(path: &Path, file: fs::File) -> Result<ParsedSession, ImportSessionError> {
    let values = read_jsonl(file, path)?;
    let mut source_session_id = None;
    let mut cwd = PathBuf::new();
    let mut started_at = None;
    let mut messages = Vec::new();
    let mut meaningful_count = 0usize;
    let mut genuine_count = 0usize;
    for value in &values {
        let timestamp = value.get("timestamp").and_then(Value::as_str);
        if started_at.is_none() {
            started_at = timestamp.map(str::to_owned);
        }
        let Some(record_type) = value.get("type").and_then(Value::as_str) else {
            continue;
        };
        let Some(payload) = value.get("payload").and_then(Value::as_object) else {
            continue;
        };
        match record_type {
            "session_meta" if source_session_id.is_none() => {
                source_session_id = string_field(payload, &["id", "session_id"]);
                cwd = string_field(payload, &["cwd"])
                    .map(PathBuf::from)
                    .unwrap_or_default();
                if let Some(timestamp) = string_field(payload, &["timestamp"]) {
                    started_at = Some(timestamp);
                }
            }
            "turn_context" if cwd.as_os_str().is_empty() => {
                cwd = string_field(payload, &["cwd"])
                    .map(PathBuf::from)
                    .unwrap_or_default();
            }
            "response_item" if payload.get("type").and_then(Value::as_str) == Some("message") => {
                let role = payload.get("role").and_then(Value::as_str);
                let content = payload.get("content");
                // Both counts are scanned losslessly over the raw content
                // blocks (all text blocks plus supported non-text images —
                // `input_image` in the Codex Responses wire shape) BEFORE the
                // first-text projection, with the same semantics as the
                // Claude/Grok/Droid parsers. Only user/assistant turns count.
                accumulate_raw_turn_counts(
                    SourceSessionFormat::Codex,
                    role,
                    content,
                    &mut meaningful_count,
                    &mut genuine_count,
                );
                if let Some(message) = message_from_fields(role, content, timestamp) {
                    messages.push(message);
                }
            }
            _ => {}
        }
    }
    drop_jsonl_values_iterative(values);
    Ok(ParsedSession {
        source_session_id,
        cwd,
        started_at,
        messages,
        meaningful_count,
        genuine_count,
    })
}

fn parse_claude(path: &Path, file: fs::File) -> Result<ParsedSession, ImportSessionError> {
    let values = read_jsonl(file, path)?;
    let mut source_session_id = None;
    let mut cwd = PathBuf::new();
    for value in &values {
        if let Some(id) = value
            .get("sessionId")
            .or_else(|| value.get("session_id"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            source_session_id = Some(id.to_owned());
        }
        if let Some(value) = value
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            cwd = PathBuf::from(value);
        }
    }

    let mut nodes = Vec::new();
    for value in &values {
        let Some(object) = value.as_object() else {
            continue;
        };
        let Some(id) = object
            .get("uuid")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        if object.get("isSidechain").and_then(Value::as_bool) == Some(true)
            || object.get("isMeta").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let parent_id = object
            .get("parentUuid")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        let message = object.get("message").and_then(Value::as_object);
        let role = message
            .and_then(|message| message.get("role"))
            .and_then(Value::as_str)
            .and_then(parse_role);
        let text = message
            .and_then(|message| message.get("content"))
            .and_then(first_direct_text)
            .map(str::to_owned);
        let timestamp = string_field(object, &["timestamp"]);
        // Meaningful: non-empty text in ANY block or a supported non-text
        // block (image), scanned losslessly over the raw content — not the
        // first-text projection (`text`), so an image-only turn counts even
        // though the projection drops it. Mirrors `native_node` and the
        // Grok/Droid `accumulate_raw_turn_counts` semantics.
        let meaningful = role.is_some()
            && message
                .and_then(|message| message.get("content"))
                .is_some_and(|content| message_content_meaningful(SourceSessionFormat::Claude, content));
        let genuine = role.is_some()
            && message
                .and_then(|message| message.get("content"))
                .is_some_and(|content| message_content_genuine(SourceSessionFormat::Claude, content));
        nodes.push(TreeNode {
            id: id.to_owned(),
            parent_id,
            role,
            text,
            timestamp,
            entry_type: object
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_owned),
            first_kept_entry_id: None,
            meaningful,
            genuine,
        });
    }

    let preferred_leaf = values.iter().rev().find_map(|value| {
        (value.get("type").and_then(Value::as_str) == Some("last-prompt"))
            .then(|| value.get("leafUuid").and_then(Value::as_str))
            .flatten()
            .filter(|id| !id.is_empty())
    });
    if let Some(leaf) = preferred_leaf {
        if let Some(index) = nodes.iter().position(|node| node.id == leaf) {
            let leaf_node = nodes.remove(index);
            nodes.push(leaf_node);
        }
    }
    let (messages, meaningful_count, genuine_count) = active_messages(&nodes);
    let started_at = messages
        .first()
        .and_then(|message| message.timestamp.clone());
    drop_jsonl_values_iterative(values);
    Ok(ParsedSession {
        source_session_id: source_session_id.or_else(|| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned)
        }),
        cwd,
        started_at,
        messages,
        meaningful_count,
        genuine_count,
    })
}

fn parse_grok(
    path: &Path,
    summary_file: fs::File,
    chat_file: Option<fs::File>,
    cwd_file: Option<fs::File>,
) -> Result<ParsedSession, ImportSessionError> {
    let summary = read_json(summary_file, path)?;
    let source_session_id = summary
        .pointer("/info/id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            path.parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .map(str::to_owned)
        });
    let cwd = summary
        .pointer("/info/cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.is_empty())
        .map(PathBuf::from)
        .or_else(|| grok_cwd_fallback(path, cwd_file))
        .unwrap_or_default();
    let started_at = ["created_at", "updated_at", "last_active_at"]
        .into_iter()
        .find_map(|key| summary.get(key).and_then(Value::as_str))
        .map(str::to_owned);
    let chat_path = path.with_file_name("chat_history.jsonl");
    let values = match chat_file {
        Some(file) => match read_jsonl(file, &chat_path) {
            Ok(values) => values,
            Err(error) => {
                drop_json_value_iterative(summary);
                return Err(error);
            }
        },
        None => Vec::new(),
    };
    let mut meaningful_count = 0usize;
    let mut genuine_count = 0usize;
    let messages = values
        .iter()
        .filter_map(|value| {
            let object = value.as_object()?;
            let record_type = object.get("type").and_then(Value::as_str);
            let role = object.get("role").and_then(Value::as_str).or(record_type);
            let content = object.get("content");
            // Both counts are scanned losslessly over the raw content blocks
            // (all text blocks plus supported non-text such as images)
            // BEFORE the first-text projection, with the same semantics as
            // the Codex/Claude parsers.
            accumulate_raw_turn_counts(
                SourceSessionFormat::Grok,
                role,
                content,
                &mut meaningful_count,
                &mut genuine_count,
            );
            message_from_fields(
                role,
                content,
                ["timestamp", "created_at", "updated_at", "time"]
                    .into_iter()
                    .find_map(|key| object.get(key).and_then(Value::as_str)),
            )
        })
        .collect::<Vec<_>>();
    drop_jsonl_values_iterative(values);
    drop_json_value_iterative(summary);
    Ok(ParsedSession {
        source_session_id,
        cwd,
        started_at,
        messages,
        meaningful_count,
        genuine_count,
    })
}

fn parse_droid(path: &Path, file: fs::File) -> Result<ParsedSession, ImportSessionError> {
    let values = read_jsonl(file, path)?;
    let start = values.iter().find_map(|value| {
        (value.get("type").and_then(Value::as_str) == Some("session_start"))
            .then(|| value.as_object())
            .flatten()
    });
    let source_session_id = start.and_then(|start| string_field(start, &["id"]));
    let cwd = start
        .and_then(|start| string_field(start, &["cwd"]))
        .map(PathBuf::from)
        .unwrap_or_default();
    let mut meaningful_count = 0usize;
    let mut genuine_count = 0usize;
    let messages = values
        .iter()
        .filter(|value| value.get("type").and_then(Value::as_str) == Some("message"))
        .filter_map(|value| {
            let object = value.as_object()?;
            let message = object.get("message")?.as_object()?;
            let role = message.get("role").and_then(Value::as_str);
            let content = message.get("content");
            // Same lossless raw-content scan as the Grok parser (see
            // `accumulate_raw_turn_counts`): both counts cover every content
            // block before the first-text projection.
            accumulate_raw_turn_counts(
                SourceSessionFormat::Droid,
                role,
                content,
                &mut meaningful_count,
                &mut genuine_count,
            );
            message_from_fields(role, content, object.get("timestamp").and_then(Value::as_str))
        })
        .collect::<Vec<_>>();
    let started_at = messages
        .first()
        .and_then(|message| message.timestamp.clone());
    drop_jsonl_values_iterative(values);
    Ok(ParsedSession {
        source_session_id,
        cwd,
        started_at,
        messages,
        meaningful_count,
        genuine_count,
    })
}

fn message_from_fields(
    role: Option<&str>,
    content: Option<&Value>,
    timestamp: Option<&str>,
) -> Option<ImportedMessage> {
    Some(ImportedMessage {
        role: parse_role(role?)?,
        text: first_text(content?)?.to_owned(),
        timestamp: timestamp.map(str::to_owned),
    })
}

/// Accumulate the lossless per-turn counts over the RAW content, before the
/// first-text projection — the uniform foreign-format count contract shared
/// by every foreign parser: Codex, Grok, and Droid call this directly and
/// Claude derives the same per-node flags from the identical helpers. A
/// user/assistant turn is meaningful when any content block carries
/// non-empty text or a supported non-text image block
/// (`supported_image_block`), and genuine when a supported image block or at
/// least one non-envelope text block exists. An image-only turn counts even
/// though the projection drops it, and genuine text in a later block still
/// counts — neither signal is expressible through the first-text projection
/// in `messages`.
fn accumulate_raw_turn_counts(
    source: SourceSessionFormat,
    role: Option<&str>,
    content: Option<&Value>,
    meaningful_count: &mut usize,
    genuine_count: &mut usize,
) {
    if !role.is_some_and(|role| parse_role(role).is_some()) {
        return;
    }
    let Some(content) = content else {
        return;
    };
    if message_content_meaningful(source, content) {
        *meaningful_count += 1;
    }
    if message_content_genuine(source, content) {
        *genuine_count += 1;
    }
}

fn parse_role(role: &str) -> Option<ImportedMessageRole> {
    match role {
        "user" => Some(ImportedMessageRole::User),
        "assistant" => Some(ImportedMessageRole::Assistant),
        _ => None,
    }
}
fn first_direct_text(content: &Value) -> Option<&str> {
    match content {
        Value::String(text) if !text.is_empty() => Some(text),
        Value::Array(items) => items.iter().find_map(|item| {
            let object = item.as_object()?;
            let block_type = object.get("type").and_then(Value::as_str);
            let text = object.get("text").and_then(Value::as_str);
            matches!(block_type, Some("text" | "input_text" | "output_text"))
                .then_some(text)
                .flatten()
                .filter(|text| !text.is_empty())
        }),
        _ => None,
    }
}

fn grok_cwd_fallback(summary_path: &Path, cwd_file: Option<fs::File>) -> Option<PathBuf> {
    let cwd_directory = summary_path.parent()?.parent()?;
    let encoded = cwd_directory.file_name()?.to_str()?;
    let decoded = percent_decode(encoded)?;
    if decoded.starts_with('/') || decoded.as_bytes().get(1) == Some(&b':') {
        return Some(PathBuf::from(decoded));
    }
    let cwd_path = cwd_directory.join(".cwd");
    let cwd = read_bounded_text_from(cwd_file?, &cwd_path, MAX_CWD_SIDECAR_BYTES).ok()?;
    let cwd = cwd.trim();
    (!cwd.is_empty()).then(|| PathBuf::from(cwd))
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex_digit(*bytes.get(index + 1)?)?;
            let low = hex_digit(*bytes.get(index + 2)?)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

const fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn first_text(content: &Value) -> Option<&str> {
    match content {
        Value::String(text) if !text.is_empty() => Some(text),
        Value::Array(items) => items.iter().find_map(|item| {
            let object = item.as_object()?;
            let block_type = object.get("type").and_then(Value::as_str);
            let text = object.get("text").and_then(Value::as_str);
            if matches!(block_type, Some("text" | "input_text" | "output_text"))
                && text.is_some_and(|text| !text.is_empty())
            {
                return text;
            }
            object.get("content").and_then(first_text)
        }),
        _ => None,
    }
}

fn string_field(object: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        object
            .get(*key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    })
}

fn read_json(file: fs::File, path: &Path) -> Result<Value, ImportSessionError> {
    let bytes = read_bounded_bytes_from(file, path, MAX_SOURCE_BYTES)?;
    serde_json::from_slice(&bytes).map_err(|source| ImportSessionError::Json {
        path: path.to_path_buf(),
        source,
    })
}

fn read_jsonl(file: fs::File, path: &Path) -> Result<Vec<Value>, ImportSessionError> {
    read_jsonl_from(file, path, MAX_SOURCE_BYTES, MAX_LINE_BYTES, MAX_RECORDS)
}

pub(super) fn read_jsonl_with_limits(
    path: &Path,
    max_bytes: u64,
    max_line_bytes: usize,
    max_records: usize,
) -> Result<Vec<Value>, ImportSessionError> {
    let file = open_bounded_file(path, max_bytes)?;
    read_jsonl_from(file, path, max_bytes, max_line_bytes, max_records)
}

fn read_jsonl_from(
    file: fs::File,
    path: &Path,
    max_bytes: u64,
    max_line_bytes: usize,
    max_records: usize,
) -> Result<Vec<Value>, ImportSessionError> {
    validate_file_size(&file, path, max_bytes)?;
    let mut reader = BufReader::new(file);
    let mut values = IterativeDropGuard::new();
    let mut records = 0_usize;
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        let read = reader
            .by_ref()
            .take(max_line_bytes.saturating_add(3) as u64)
            .read_until(b'\n', &mut buffer)
            .map_err(|source| ImportSessionError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        if read == 0 {
            break;
        }
        let mut frame_bytes = buffer.len();
        if buffer.get(frame_bytes.wrapping_sub(1)) == Some(&b'\n') {
            frame_bytes -= 1;
            if buffer.get(frame_bytes.wrapping_sub(1)) == Some(&b'\r') {
                frame_bytes -= 1;
            }
        }
        if frame_bytes > max_line_bytes {
            return Err(resource_limit(
                path,
                format!("JSONL line exceeds {max_line_bytes} bytes"),
            ));
        }
        let line = std::str::from_utf8(&buffer)
            .map_err(|_| resource_limit(path, "source is not valid UTF-8".to_owned()))?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if records == max_records {
            return Err(resource_limit(
                path,
                format!("session exceeds {max_records} JSON records"),
            ));
        }
        records += 1;
        let value =
            serde_json::from_str::<Value>(line).map_err(|source| ImportSessionError::Json {
                path: path.to_path_buf(),
                source,
            })?;
        if value.is_object() {
            values.push(value);
        } else {
            drop_json_value_iterative(value);
        }
    }
    Ok(values.into_inner())
}

fn open_bounded_file(path: &Path, max_bytes: u64) -> Result<fs::File, ImportSessionError> {
    let file = fs::File::open(path).map_err(|source| ImportSessionError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    validate_file_size(&file, path, max_bytes)?;
    Ok(file)
}

fn validate_file_size(
    file: &fs::File,
    path: &Path,
    max_bytes: u64,
) -> Result<(), ImportSessionError> {
    let metadata = file.metadata().map_err(|source| ImportSessionError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(resource_limit(
            path,
            "source is not a regular file".to_owned(),
        ));
    }
    if metadata.len() > max_bytes {
        return Err(resource_limit(
            path,
            format!("file is {} bytes; maximum is {max_bytes}", metadata.len()),
        ));
    }
    Ok(())
}

fn read_bounded_bytes_from(
    file: fs::File,
    path: &Path,
    max_bytes: u64,
) -> Result<Vec<u8>, ImportSessionError> {
    validate_file_size(&file, path, max_bytes)?;
    let capacity = file
        .metadata()
        .ok()
        .and_then(|metadata| usize::try_from(metadata.len()).ok())
        .unwrap_or_default();
    let mut bytes = Vec::with_capacity(capacity);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| ImportSessionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() as u64 > max_bytes {
        return Err(resource_limit(
            path,
            format!("file exceeds maximum of {max_bytes} bytes"),
        ));
    }
    Ok(bytes)
}

fn read_bounded_bytes(path: &Path, max_bytes: u64) -> Result<Vec<u8>, ImportSessionError> {
    let file = open_bounded_file(path, max_bytes)?;
    read_bounded_bytes_from(file, path, max_bytes)
}

fn read_bounded_text_from(
    file: fs::File,
    path: &Path,
    max_bytes: u64,
) -> Result<String, ImportSessionError> {
    String::from_utf8(read_bounded_bytes_from(file, path, max_bytes)?)
        .map_err(|_| resource_limit(path, "source is not valid UTF-8".to_owned()))
}

pub(super) fn read_bounded_text(path: &Path, max_bytes: u64) -> Result<String, ImportSessionError> {
    String::from_utf8(read_bounded_bytes(path, max_bytes)?)
        .map_err(|_| resource_limit(path, "source is not valid UTF-8".to_owned()))
}

fn resource_limit(path: &Path, reason: String) -> ImportSessionError {
    ImportSessionError::ResourceLimit {
        path: path.to_path_buf(),
        reason,
    }
}

fn native_jsonl_records<'a>(
    _format: SourceSessionFormat,
    path: &'a Path,
    contents: &'a str,
) -> impl Iterator<Item = Result<Value, ImportSessionError>> + 'a {
    let mut records = 0_usize;
    contents.lines().filter_map(move |line| {
        if line.len() > MAX_LINE_BYTES {
            return Some(Err(resource_limit(
                path,
                format!("JSONL line exceeds {MAX_LINE_BYTES} bytes"),
            )));
        }
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        if records == MAX_RECORDS {
            return Some(Err(resource_limit(
                path,
                format!("session exceeds {MAX_RECORDS} JSON records"),
            )));
        }
        records += 1;
        match serde_json::from_str::<Value>(line) {
            Ok(value) if value.is_object() => Some(Ok(value)),
            Ok(value) => {
                drop_json_value_iterative(value);
                None
            }
            Err(source) => Some(Err(ImportSessionError::Json {
                path: path.to_path_buf(),
                source,
            })),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Depth used by every constrained-stack regression below. serde_json's
    /// default recursion limit is 128, so 120 nested arrays parse cleanly
    /// (this is the repo fixture convention, e.g. the catalog fixture) while
    /// staying within one `Value::drop` recursion of the cap.
    const SERDE_LIMIT_DEPTH: usize = 120;

    /// Stack budget that comfortably fits the production parse path for a
    /// `SERDE_LIMIT_DEPTH` record (a ~122-level `Value`), measured at ~197 KiB
    /// of thread stack under the debug test profile (rustc 1.88, linux x64).
    /// 512 KiB gives ~2.5x headroom over the parse peak while still being a
    /// controlled small-stack boundary (vs the 2-8 MiB default).
    const PRODUCTION_PARSE_STACK: usize = 512 * 1024;

    /// Stack budget for the destruction-isolation proofs. Recursive
    /// destruction of a `SERDE_LIMIT_DEPTH` object needs ~27 KiB of stack
    /// (measured: `Vec<Value>` drop of one depth-120 object overflows below
    /// 27 KiB), while the iterative/guard destruction is constant-stack
    /// (~4 KiB). 16 KiB sits inside that window with ~4x margin to the
    /// iterative floor and ~1.7x below the recursive threshold, so the
    /// iterative path survives and a reverted recursive path overflows the
    /// guard page and aborts the process.
    const DESTRUCTION_STACK: usize = 16 * 1024;

    fn deep_array_text(depth: usize) -> String {
        format!("{}null{}", "[".repeat(depth), "]".repeat(depth))
    }

    /// `drop_json_value_iterative` — the free function the production
    /// success-path loop in `read_omp_parent_session` and the non-object
    /// branch in `read_jsonl_from` route destruction through — must destruct
    /// a `SERDE_LIMIT_DEPTH` nested `Value` on a `DESTRUCTION_STACK` thread.
    /// The value is parsed on the caller (test) stack, proving it fits
    /// serde_json's 128 recursion limit; only the destruction runs on the
    /// constrained stack. The iterative drop uses a heap-allocated worklist
    /// so stack depth stays constant. A regression to recursive `Value::drop`
    /// (~27 KiB at this depth) overflows the 16 KiB guard page and aborts the
    /// test process.
    #[test]
    fn iterative_drop_survives_constrained_stack_with_serde_limit_depth() {
        let nested = deep_array_text(SERDE_LIMIT_DEPTH);
        let value: Value = serde_json::from_str(&nested).expect("120-deep parses under 128 limit");

        let result = std::thread::Builder::new()
            .stack_size(DESTRUCTION_STACK)
            .spawn(move || drop_json_value_iterative(value))
            .expect("constrained-stack thread")
            .join();

        assert!(
            result.is_ok(),
            "iterative drop must not overflow 16KB stack at 120 depth"
        );
    }

    /// The real OMP parent-session probe must route both retained header
    /// values through `drop_json_value_iterative` after scanning. The
    /// thread-local production primitive probe makes that wiring directly
    /// observable: restoring the old early `break` leaves the retained guard
    /// to destruct values instead, so the exact explicit-drop count changes.
    #[test]
    fn omp_parent_session_drops_deep_remainder_iteratively() {
        let deep = deep_array_text(SERDE_LIMIT_DEPTH);
        let dir = tempfile::tempdir().expect("fixture dir");
        let prior_path = dir.path().join("prior.jsonl");
        let prior_str = prior_path.to_string_lossy().into_owned();
        let path = dir.path().join("omp-parent.jsonl");
        let mut file = fs::File::create(&path).expect("create fixture");
        writeln!(
            file,
            r#"{{"type":"session","id":"s","parentSession":"{prior}"}}"#,
            prior = prior_str,
        )
        .expect("write header");
        writeln!(
            file,
            r#"{{"type":"message","id":"m","diagnostic":{deep}}}"#,
            deep = deep,
        )
        .expect("write deep record");
        drop(file);

        let path_clone = path.clone();
        let result = std::thread::Builder::new()
            .stack_size(PRODUCTION_PARSE_STACK)
            .spawn(move || {
                reset_iterative_drop_probes();
                let file = fs::File::open(&path_clone).expect("open fixture");
                let parent = read_omp_parent_session(&file, &path_clone);
                (parent, iterative_drop_probes())
            })
            .expect("production-parse-stack thread")
            .join()
            .expect("production parent-session probe must not panic");
        assert_eq!(
            result.0.expect("parent-session probe must succeed"),
            Some(prior_str)
        );
        assert_eq!(
            result.1.0, 2,
            "both retained OMP records must use explicit iterative drop"
        );
        assert_eq!(result.1.1, 1, "the emptied guard still drops exactly once");
    }

    /// The real bounded JSONL reader must keep its accumulated values in an
    /// `IterativeDropGuard` until an early resource-limit exit. The
    /// thread-local primitive probes make that caller wiring directly
    /// observable: replacing the guard with `Vec<Value>` removes the guard
    /// drop event and the iterative value destruction event.
    #[test]
    fn read_jsonl_with_limits_accumulated_deep_object_early_exit_is_bounded() {
        let deep = deep_array_text(SERDE_LIMIT_DEPTH);
        let dir = tempfile::tempdir().expect("fixture dir");
        let path = dir.path().join("omp-accumulated.jsonl");
        let mut file = fs::File::create(&path).expect("create fixture");
        // Record 1: a deep object the guard accumulates.
        writeln!(
            file,
            r#"{{"type":"message","id":"m","diagnostic":{deep}}}"#,
            deep = deep,
        )
        .expect("write deep record");
        // Record 2: a line longer than max_line_bytes so the parser returns
        // ResourceLimit without ever parsing this record.
        writeln!(file, "{}", "x".repeat(2000)).expect("write over-long record");
        drop(file);

        let path_clone = path.clone();
        let result = std::thread::Builder::new()
            .stack_size(PRODUCTION_PARSE_STACK)
            .spawn(move || {
                reset_iterative_drop_probes();
                // max_line_bytes sits above the deep record's line (~273 bytes)
                // so it parses, and below the over-long record (2000 bytes) so
                // the early-exit guard fires with the deep object accumulated.
                let outcome = read_jsonl_with_limits(&path_clone, 64 * 1024, 1024, 1000);
                (outcome, iterative_drop_probes())
            })
            .expect("production-parse-stack thread")
            .join()
            .expect("production read_jsonl_with_limits must not panic");
        let error = result
            .0
            .expect_err("read_jsonl_with_limits must return Err on the over-long record");
        match error {
            ImportSessionError::ResourceLimit { reason, .. } => {
                assert!(
                    reason.contains("exceeds 1024 bytes"),
                    "ResourceLimit reason must cite the line-byte limit, got: {reason}"
                );
            }
            other => panic!("expected ResourceLimit, got {other:?}"),
        }
        assert_eq!(
            result.1.0, 1,
            "the accumulated deep record must be iteratively destroyed"
        );
        assert_eq!(
            result.1.1, 1,
            "the production early-exit guard must drop exactly once"
        );
    }

    /// Destruction proof for scenario (b)'s early-exit guard. The
    /// `IterativeDropGuard` is the production destruction mechanism for every
    /// early-exit path in `read_omp_parent_session` and `read_jsonl_from`: on
    /// `?`/`return Err` its `Drop` drains each accumulated `Value`
    /// iteratively via a heap worklist. This test accumulates a real
    /// `SERDE_LIMIT_DEPTH` object (parsed on the caller stack, fitting the
    /// 128 limit) into a guard and drops the guard on a `DESTRUCTION_STACK`
    /// thread. Reverting the guard to a plain `Vec<Value>` (recursive drop)
    /// overflows the 16 KB guard page and aborts the test process; the
    /// iterative `Drop` completes in constant stack.
    #[test]
    fn read_jsonl_early_exit_guard_drops_accumulated_deep_object_iteratively() {
        let deep = format!(
            r#"{{"type":"message","id":"m","diagnostic":{}}}"#,
            deep_array_text(SERDE_LIMIT_DEPTH)
        );
        let value: Value =
            serde_json::from_str(&deep).expect("120-deep object parses under 128 limit");
        let mut guard = IterativeDropGuard::new();
        guard.push(value);

        let result = std::thread::Builder::new()
            .stack_size(DESTRUCTION_STACK)
            .spawn(move || drop(guard))
            .expect("constrained-stack thread")
            .join();

        assert!(
            result.is_ok(),
            "IterativeDropGuard::drop must drain the accumulated depth-120 object on 16KB stack"
        );
    }
}
