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
    /// User/assistant turns with meaningful content (text or supported
    /// non-text such as image), counted over the active path. Independent of
    /// the lossy text projection in `messages`.
    pub meaningful_count: usize,
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
        SourceSessionFormat::Pi | SourceSessionFormat::Omp => {
            parse_native(source, &path, primary)
        }
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
        SourceSessionFormat::Grok => read_json(primary, &path).map(|value| {
            value
                .pointer("/info/id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
        }),
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
                    .find(|value| value.get("type").and_then(Value::as_str) == Some("session_start"))
                    .and_then(|value| value.get("id"))
                    .and_then(Value::as_str),
                SourceSessionFormat::Grok => unreachable!(),
            };
            Ok(id.filter(|id| !id.is_empty()).map(str::to_owned))
        }
    }
}

fn parse_native(
    source: SourceSessionFormat,
    path: &Path,
    file: fs::File,
) -> Result<ParsedSession, ImportSessionError> {
    let native = read_native_turns(source, path, file)?;
    let (messages, meaningful_count) = project_turns(&native.turns);
    Ok(ParsedSession {
        source_session_id: native.source_session_id,
        cwd: native.cwd,
        started_at: native.started_at,
        messages,
        meaningful_count,
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
    let values = read_jsonl_text(source, path, &contents)?;
    let header_index = if source == SourceSessionFormat::Omp
        && values.first().is_some_and(valid_omp_title_slot)
    {
        1
    } else {
        0
    };
    let Some(header) = values.get(header_index).and_then(Value::as_object) else {
        return Err(ImportSessionError::InvalidNativeHeader {
            format: source,
            path: path.to_path_buf(),
        });
    };
    let valid_header = header.get("type").and_then(Value::as_str) == Some("session")
        && header
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty());
    if !valid_header {
        return Err(ImportSessionError::InvalidNativeHeader {
            format: source,
            path: path.to_path_buf(),
        });
    }
    let source_session_id = header.get("id").and_then(Value::as_str).map(str::to_owned);
    let cwd = header
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_default();
    let started_at = header
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let parent_session = header
        .get("parentSession")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let nodes = values[header_index + 1..]
        .iter()
        .filter_map(native_node)
        .collect::<Vec<_>>();
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
            let native = read_native_turns_from_contents(SourceSessionFormat::Omp, &path, contents)?;
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
    let (messages, meaningful_count) = project_turns(&turns);
    let fingerprint = chain_fingerprint(&digests);
    Ok((
        ParsedSession {
            source_session_id,
            cwd,
            started_at,
            messages,
            meaningful_count,
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
    reader.seek(SeekFrom::Start(0)).map_err(|source| ImportSessionError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut values = Vec::new();
    let mut buffer = Vec::new();
    let mut bytes_read = 0_u64;
    let mut records = 0_usize;
    while values.len() < 2 {
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
            Ok(_) => {}
            Err(source) => {
                return Err(ImportSessionError::Json {
                    path: path.to_path_buf(),
                    source,
                });
            }
        }
    }
    let header = values
        .into_iter()
        .find(|value| value.get("type").and_then(Value::as_str) == Some("session"));
    Ok(header.and_then(|value| {
        value
            .get("parentSession")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    }))
}

fn valid_omp_title_slot(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("title")
        && value.get("v").and_then(Value::as_u64) == Some(1)
        && value.get("title").and_then(Value::as_str).is_some()
        && value.get("updatedAt").and_then(Value::as_str).is_some()
        && value.get("pad").and_then(Value::as_str).is_some()
}

fn native_node(value: &Value) -> Option<TreeNode> {
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
    let entry_type = object.get("type").and_then(Value::as_str).map(str::to_owned);
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
    let meaningful = role.is_some()
        && content.is_some_and(|content| message_content_meaningful(content));
    Some(TreeNode {
        id,
        parent_id,
        role,
        text,
        timestamp,
        entry_type,
        first_kept_entry_id,
        meaningful,
    })
}

fn active_messages(nodes: &[TreeNode]) -> (Vec<ImportedMessage>, usize) {
    project_turns(&active_turns(nodes))
}

/// Project ordered active-path turns into the lossy message list and the
/// meaningful-turn count.
fn project_turns(turns: &[ActiveTurn]) -> (Vec<ImportedMessage>, usize) {
    let meaningful_count = turns.iter().filter(|turn| turn.meaningful).count();
    let messages = turns
        .iter()
        .filter_map(|turn| turn.message.clone())
        .collect::<Vec<_>>();
    (messages, meaningful_count)
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
                message,
            }
        })
        .collect()
}

/// Whether message content carries a meaningful user/assistant turn: non-empty
/// text or a supported non-text block (image attachment). Mirrors
/// `crate::session_catalog::lineage::message_meaningful` for the import parser.
fn message_content_meaningful(content: &Value) -> bool {
    match content {
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(blocks) => blocks.iter().any(|block| {
            let Some(block_type) = block.get("type").and_then(Value::as_str) else {
                return false;
            };
            match block_type {
                "text" | "input_text" | "output_text" => block
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.trim().is_empty()),
                "image" => true,
                _ => false,
            }
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
            "response_item"
                if payload.get("type").and_then(Value::as_str) == Some("message") =>
            {
                if let Some(message) = message_from_fields(
                    payload.get("role").and_then(Value::as_str),
                    payload.get("content"),
                    timestamp,
                ) {
                    messages.push(message);
                }
            }
            _ => {}
        }
    }
    let meaningful_count = messages.len();
    Ok(ParsedSession {
        source_session_id,
        cwd,
        started_at,
        messages,
        meaningful_count,
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
        let meaningful = role.is_some() && text.is_some();
        nodes.push(TreeNode {
            id: id.to_owned(),
            parent_id,
            role,
            text,
            timestamp,
            entry_type: object.get("type").and_then(Value::as_str).map(str::to_owned),
            first_kept_entry_id: None,
            meaningful,
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
    let (messages, meaningful_count) = active_messages(&nodes);
    let started_at = messages.first().and_then(|message| message.timestamp.clone());
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
        Some(file) => read_jsonl(file, &chat_path)?,
        None => Vec::new(),
    };
    let messages = values
        .iter()
        .filter_map(|value| {
            let object = value.as_object()?;
            let record_type = object.get("type").and_then(Value::as_str);
            let role = object
                .get("role")
                .and_then(Value::as_str)
                .or(record_type);
            message_from_fields(
                role,
                object.get("content"),
                ["timestamp", "created_at", "updated_at", "time"]
                    .into_iter()
                    .find_map(|key| object.get(key).and_then(Value::as_str)),
            )
        })
        .collect::<Vec<_>>();
    let meaningful_count = messages.len();
    Ok(ParsedSession {
        source_session_id,
        cwd,
        started_at,
        messages,
        meaningful_count,
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
    let messages = values
        .iter()
        .filter(|value| value.get("type").and_then(Value::as_str) == Some("message"))
        .filter_map(|value| {
            let object = value.as_object()?;
            let message = object.get("message")?.as_object()?;
            message_from_fields(
                message.get("role").and_then(Value::as_str),
                message.get("content"),
                object.get("timestamp").and_then(Value::as_str),
            )
        })
        .collect::<Vec<_>>();
    let started_at = messages.first().and_then(|message| message.timestamp.clone());
    let meaningful_count = messages.len();
    Ok(ParsedSession {
        source_session_id,
        cwd,
        started_at,
        messages,
        meaningful_count,
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
    let mut values = Vec::new();
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
        let value = serde_json::from_str::<Value>(line).map_err(|source| {
            ImportSessionError::Json {
                path: path.to_path_buf(),
                source,
            }
        })?;
        if value.is_object() {
            values.push(value);
        }
    }
    Ok(values)
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
        return Err(resource_limit(path, "source is not a regular file".to_owned()));
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

fn read_jsonl_text(
    format: SourceSessionFormat,
    path: &Path,
    contents: &str,
) -> Result<Vec<Value>, ImportSessionError> {
    let mut values = Vec::new();
    let mut records = 0_usize;
    for line in contents.lines() {
        if line.len() > MAX_LINE_BYTES {
            return Err(resource_limit(
                path,
                format!("JSONL line exceeds {MAX_LINE_BYTES} bytes"),
            ));
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if records == MAX_RECORDS {
            return Err(resource_limit(
                path,
                format!("session exceeds {MAX_RECORDS} JSON records"),
            ));
        }
        records += 1;
        match serde_json::from_str::<Value>(line) {
            Ok(value) if value.is_object() => values.push(value),
            Ok(_) => {}
            Err(source) => {
                return Err(ImportSessionError::Json {
                    path: path.to_path_buf(),
                    source,
                });
            }
        }
    }
    if values.is_empty() && !contents.trim().is_empty() {
        return Err(ImportSessionError::InvalidNativeHeader {
            format,
            path: path.to_path_buf(),
        });
    }
    Ok(values)
}
