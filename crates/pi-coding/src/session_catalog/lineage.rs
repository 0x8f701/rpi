//! Native-session lineage extraction and lightweight header parsing.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{ImportLineageKey, LINEAGE_CUSTOM_TYPE, SessionSourceKind};

const MAX_NATIVE_CATALOG_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub(super) struct NativeHeaderLite {
    pub(crate) id: String,
    pub(crate) cwd: PathBuf,
}

#[derive(Debug, Clone)]
pub(super) struct NativeListInfo {
    pub(crate) id: String,
    pub(crate) cwd: PathBuf,
    pub(crate) name: Option<String>,
    pub(crate) first_message: String,
    pub(crate) message_count: usize,
    pub(crate) lineage: Option<ImportLineageKey>,
}

pub(super) fn read_native_header_lite(file: File, path: &Path) -> Result<NativeHeaderLite, String> {
    let contents = safe_read_text(file, path)?;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line).map_err(|error| error.to_string())?;
        if value.get("type").and_then(Value::as_str) != Some("session") {
            // OMP title slot may precede the session header.
            continue;
        }
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| "missing session id".to_owned())?
            .to_owned();
        let cwd = value
            .get("cwd")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_default();
        return Ok(NativeHeaderLite { id, cwd });
    }
    Err("missing session header".to_owned())
}

pub(super) fn read_native_list_info(file: File, path: &Path) -> Result<NativeListInfo, String> {
    let contents = safe_read_text(file, path)?;
    let mut id = String::new();
    let mut cwd = PathBuf::new();
    let mut name = None;
    let mut first_message = None;
    let mut message_count = 0usize;
    let mut lineage = None;
    let mut parent_session = None;

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("session") => {
                id = value
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                cwd = value
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
                    .unwrap_or_default();
                parent_session = value
                    .get("parentSession")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            Some("session_info") => {
                if let Some(value) = value.get("name").and_then(Value::as_str) {
                    if !value.trim().is_empty() {
                        name = Some(value.to_owned());
                    }
                }
            }
            Some("message") => {
                message_count += 1;
                if first_message.is_none() {
                    if let Some(text) = message_text(&value) {
                        first_message = Some(text);
                    }
                }
            }
            Some("custom")
                if value.get("customType").and_then(Value::as_str) == Some(LINEAGE_CUSTOM_TYPE) =>
            {
                lineage = lineage_from_custom(&value);
            }
            _ => {}
        }
    }

    if id.is_empty() {
        return Err("missing session id".to_owned());
    }
    if lineage.is_none() {
        lineage = parent_session.as_deref().and_then(lineage_from_parent_session);
    }
    Ok(NativeListInfo {
        id,
        cwd,
        name,
        first_message: first_message.unwrap_or_else(|| "(no messages)".to_owned()),
        message_count,
        lineage,
    })
}

pub(super) fn read_native_lineage(
    file: File,
    path: &Path,
) -> Option<(ImportLineageKey, String)> {
    let info = read_native_list_info(file, path).ok()?;
    let lineage = info.lineage?;
    Some((lineage, info.id))
}

pub(super) fn lineage_from_custom(value: &Value) -> Option<ImportLineageKey> {
    let data = value.get("data")?.as_object()?;
    let source = data
        .get("source")
        .and_then(Value::as_str)?
        .parse::<SessionSourceKind>()
        .ok()?;
    if source.is_native() {
        return None;
    }
    let source_session_id = data
        .get("sourceSessionId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let source_path_fingerprint = data
        .get("sourcePath")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if source_session_id.is_empty() && source_path_fingerprint.is_empty() {
        return None;
    }
    let content_fingerprint = data
        .get("contentFingerprint")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some(ImportLineageKey {
        source,
        source_session_id,
        source_path_fingerprint,
        content_fingerprint,
    })
}

pub(super) fn lineage_from_parent_session(value: &str) -> Option<ImportLineageKey> {
    let (source, rest) = value.split_once(':')?;
    let source = source.parse::<SessionSourceKind>().ok()?;
    if source.is_native() || rest.is_empty() {
        return None;
    }
    Some(ImportLineageKey {
        source,
        source_session_id: rest.to_owned(),
        source_path_fingerprint: String::new(),
        content_fingerprint: None,
    })
}

pub(super) fn message_text(value: &Value) -> Option<String> {
    let message = value.get("message")?;
    if let Some(text) = message.get("content").and_then(Value::as_str) {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_owned());
        }
    }
    let content = message.get("content")?.as_array()?;
    for block in content {
        if block.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_owned());
                }
            }
        }
    }
    None
}

fn safe_read_text(mut file: File, path: &Path) -> Result<String, String> {
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.file_type().is_file() {
        return Err("refusing non-regular session file".to_owned());
    }
    if metadata.len() > MAX_NATIVE_CATALOG_BYTES {
        return Err(format!(
            "session file exceeds catalog limit of {MAX_NATIVE_CATALOG_BYTES} bytes: {}",
            path.display()
        ));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or_default());
    file.by_ref()
        .take(MAX_NATIVE_CATALOG_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_NATIVE_CATALOG_BYTES {
        return Err(format!(
            "session file exceeds catalog limit of {MAX_NATIVE_CATALOG_BYTES} bytes: {}",
            path.display()
        ));
    }
    String::from_utf8(bytes).map_err(|_| "session file is not valid UTF-8".to_owned())
}
