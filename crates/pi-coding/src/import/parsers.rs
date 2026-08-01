use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::{
    ImportSessionError, ImportedMessage, ImportedMessageRole, OpenedSource, OpenedSourceParts,
    SourceSessionFormat,
};

const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
const MAX_RECORDS: usize = 250_000;
const MAX_CWD_SIDECAR_BYTES: u64 = 16 * 1024;

#[derive(Debug)]
pub(super) struct ParsedSession {
    pub source_session_id: Option<String>,
    pub cwd: PathBuf,
    pub started_at: Option<String>,
    pub messages: Vec<ImportedMessage>,
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
    let contents = read_bounded_text_from(file, path, MAX_SOURCE_BYTES)?;
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
    let nodes = values[header_index + 1..]
        .iter()
        .filter_map(native_node)
        .collect::<Vec<_>>();
    let messages = active_messages(&nodes);
    Ok(ParsedSession {
        source_session_id,
        cwd,
        started_at,
        messages,
    })
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
    let text = message
        .and_then(|message| message.get("content"))
        .and_then(first_text)
        .map(str::to_owned);
    Some(TreeNode {
        id,
        parent_id,
        role,
        text,
        timestamp,
        entry_type,
        first_kept_entry_id,
    })
}

fn active_messages(nodes: &[TreeNode]) -> Vec<ImportedMessage> {
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
        .filter_map(|index| {
            let node = &nodes[*index];
            Some(ImportedMessage {
                role: node.role?,
                text: node.text.clone()?,
                timestamp: node.timestamp.clone(),
            })
        })
        .collect()
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
    Ok(ParsedSession {
        source_session_id,
        cwd,
        started_at,
        messages,
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
        nodes.push(TreeNode {
            id: id.to_owned(),
            parent_id,
            role,
            text,
            timestamp,
            entry_type: object.get("type").and_then(Value::as_str).map(str::to_owned),
            first_kept_entry_id: None,
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
    let messages = active_messages(&nodes);
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
        .collect();
    Ok(ParsedSession {
        source_session_id,
        cwd,
        started_at,
        messages,
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
    Ok(ParsedSession {
        source_session_id,
        cwd,
        started_at,
        messages,
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
