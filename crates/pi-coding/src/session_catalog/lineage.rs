//! Native-session lineage extraction and lightweight header parsing.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{
    ImportLineageKey, LINEAGE_CUSTOM_TYPE, SessionKind, SessionSourceKind,
};

const MAX_NATIVE_CATALOG_BYTES: u64 = 64 * 1024 * 1024;
/// Injected bootstrap envelopes that never carry genuine conversation
/// content: rpi/Codex context injection (`<environment_context>`,
/// `<user_info>`, `<system-reminder>`) plus AGENTS/project instruction
/// blocks (`<project_instructions path="...">` carrying AGENTS.md rules,
/// `<user_instructions>`). Open tags are matched as prefixes so they may
/// carry attributes; an unclosed or differently-spelled tag stays genuine.
const INJECTED_CONTEXT_ENVELOPES: &[(&str, &str)] = &[
    ("<environment_context", "</environment_context>"),
    ("<user_info", "</user_info>"),
    ("<system-reminder", "</system-reminder>"),
    ("<project_instructions", "</project_instructions>"),
    ("<user_instructions", "</user_instructions>"),
];

#[derive(Debug, Clone)]
pub(super) struct NativeHeaderLite {
    pub(crate) id: String,
    pub(crate) cwd: PathBuf,
    pub(crate) session_kind: SessionKind,
}

#[derive(Debug, Clone)]
pub(super) struct NativeListInfo {
    pub(crate) id: String,
    pub(crate) cwd: PathBuf,
    pub(crate) name: Option<String>,
    pub(crate) first_message: String,
    pub(crate) message_count: usize,
    /// Meaningful messages that carry genuine (non-bootstrap) content:
    /// image attachments or at least one text part that is not an injected
    /// context envelope. Bootstrap-only sessions have messages but zero
    /// genuine ones.
    pub(crate) genuine_message_count: usize,
    pub(crate) lineage: Option<ImportLineageKey>,
    pub(crate) has_parent_session: bool,
    pub(crate) session_kind: SessionKind,
}

fn parse_session_kind(header: &Value) -> SessionKind {
    match header.get("sessionKind") {
        Some(raw) => raw
            .as_str()
            .and_then(SessionKind::from_header_value)
            .unwrap_or(SessionKind::Unknown),
        None => SessionKind::User,
    }
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
        return Ok(NativeHeaderLite {
            id,
            cwd,
            session_kind: parse_session_kind(&value),
        });
    }
    Err("missing session header".to_owned())
}

pub(super) fn read_native_list_info(file: File, path: &Path) -> Result<NativeListInfo, String> {
    let contents = safe_read_text(file, path)?;
    let mut id = String::new();
    let mut cwd = PathBuf::new();
    let mut name = None;
    let mut first_message = None;
    let mut skipped_injected_text = false;
    let mut message_count = 0usize;
    let mut genuine_message_count = 0usize;
    let mut lineage = None;
    let mut parent_session = None;
    let mut session_kind = SessionKind::User;

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
                session_kind = parse_session_kind(&value);
            }
            Some("session_info") => {
                if let Some(value) = value.get("name").and_then(Value::as_str) {
                    if !value.trim().is_empty() {
                        name = Some(value.to_owned());
                    }
                }
            }
            Some("message") => {
                // Meaningful count and textual summary are intentionally
                // separable: an image-only user/assistant turn is meaningful
                // (it carries recoverable content) even when no portable text
                // exists, while an empty pending assistant placeholder is not.
                if message_meaningful(&value) {
                    message_count += 1;
                    if message_genuine(&value) {
                        genuine_message_count += 1;
                    }
                }
                if first_message.is_none() {
                    match message_text(&value) {
                        CatalogMessageText::Genuine(text) => first_message = Some(text),
                        CatalogMessageText::InjectedOnly => skipped_injected_text = true,
                        CatalogMessageText::Absent => {}
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
        lineage = parent_session
            .as_deref()
            .and_then(lineage_from_parent_session);
    }
    Ok(NativeListInfo {
        id,
        cwd,
        name,
        first_message: first_message.unwrap_or_else(|| {
            if skipped_injected_text {
                String::new()
            } else {
                "(no messages)".to_owned()
            }
        }),
        message_count,
        genuine_message_count,
        lineage,
        has_parent_session: parent_session.is_some(),
        session_kind,
    })
}

pub(super) fn read_native_lineage(file: File, path: &Path) -> Option<(ImportLineageKey, String)> {
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
    if !source.is_importable() {
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
    if !source.is_importable() || rest.is_empty() {
        return None;
    }
    Some(ImportLineageKey {
        source,
        source_session_id: rest.to_owned(),
        source_path_fingerprint: String::new(),
        content_fingerprint: None,
    })
}

#[derive(Debug, PartialEq, Eq)]
enum CatalogMessageText {
    Absent,
    InjectedOnly,
    Genuine(String),
}

fn message_text(value: &Value) -> CatalogMessageText {
    let Some(message) = value.get("message") else {
        return CatalogMessageText::Absent;
    };
    if !matches!(
        message.get("role").and_then(Value::as_str),
        Some("user") | Some("assistant")
    ) {
        return CatalogMessageText::Absent;
    }
    if let Some(text) = message.get("content").and_then(Value::as_str) {
        return classify_message_text(text);
    }
    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return CatalogMessageText::Absent;
    };
    let mut found_injected = false;
    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        let Some(text) = block.get("text").and_then(Value::as_str) else {
            continue;
        };
        match classify_message_text(text) {
            CatalogMessageText::Genuine(text) => return CatalogMessageText::Genuine(text),
            CatalogMessageText::InjectedOnly => found_injected = true,
            CatalogMessageText::Absent => {}
        }
    }
    if found_injected {
        CatalogMessageText::InjectedOnly
    } else {
        CatalogMessageText::Absent
    }
}

fn classify_message_text(text: &str) -> CatalogMessageText {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        CatalogMessageText::Absent
    } else if is_injected_context_envelope(trimmed) {
        CatalogMessageText::InjectedOnly
    } else {
        CatalogMessageText::Genuine(trimmed.to_owned())
    }
}

fn is_injected_context_envelope(text: &str) -> bool {
    let mut remaining = text.trim();
    let mut found_envelope = false;

    while !remaining.is_empty() {
        let Some((open, close)) = INJECTED_CONTEXT_ENVELOPES
            .iter()
            .find(|(open, _)| remaining.starts_with(open))
        else {
            return false;
        };
        // Open tags may carry attributes (`<project_instructions
        // path="/proj/AGENTS.md">`): after the matched prefix, the next
        // character must open the tag body (`>`) or start an attribute
        // region; anything else is a different, genuine tag.
        let after_open = &remaining[open.len()..];
        if !after_open.starts_with('>')
            && !after_open.starts_with(|character: char| character.is_ascii_whitespace())
        {
            return false;
        }
        let Some(open_tag_end) = after_open.find('>') else {
            return false;
        };
        let after_open_tag = &after_open[open_tag_end + 1..];
        let Some(close_offset) = after_open_tag.find(close) else {
            return false;
        };
        remaining = after_open_tag[close_offset + close.len()..].trim_start();
        found_envelope = true;
    }

    found_envelope
}

/// Whether a `type: "message"` record carries meaningful content for catalog
/// noise filtering. A user/assistant turn with non-empty text OR a supported
/// non-text content block (image attachment) is meaningful; an empty pending
/// assistant placeholder (no content blocks) is not. Tool/system/custom records
/// are excluded by role so they never count toward the meaningful total.
pub(super) fn message_meaningful(value: &Value) -> bool {
    let Some(message) = value.get("message") else {
        return false;
    };
    let role = message.get("role").and_then(Value::as_str);
    if !matches!(role, Some("user") | Some("assistant")) {
        return false;
    }
    let Some(content) = message.get("content") else {
        return false;
    };
    match content {
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(blocks) => blocks.iter().any(|block| {
            let Some(block_type) = block.get("type").and_then(Value::as_str) else {
                return false;
            };
            match block_type {
                "text" => block
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.trim().is_empty()),
                // Supported non-text content: image attachments.
                "image" => true,
                _ => false,
            }
        }),
        _ => false,
    }
}

/// Whether a meaningful user/assistant message carries genuine (non-bootstrap)
/// content: a supported non-text block (image attachment) always counts, while
/// text counts only when at least one part is not an injected-context
/// envelope (`<environment_context>`, `<user_info>`, `<system-reminder>`,
/// AGENTS/project instruction blocks). Bootstrap envelopes never make a
/// message genuine.
fn message_genuine(value: &Value) -> bool {
    if !message_meaningful(value) {
        return false;
    }
    let Some(message) = value.get("message") else {
        return false;
    };
    if let Some(blocks) = message.get("content").and_then(Value::as_array) {
        if blocks
            .iter()
            .any(|block| block.get("type").and_then(Value::as_str) == Some("image"))
        {
            return true;
        }
    }
    matches!(message_text(value), CatalogMessageText::Genuine(_))
}

/// Whether parsed message text carries genuine (non-bootstrap) content:
/// non-empty and not wholly wrapped in injected-context envelopes. Shared by
/// the foreign-row classification (whose lossy parse exposes message text) and
/// the import parser's lossless per-block genuine scan.
pub(crate) fn text_has_genuine_content(text: &str) -> bool {
    matches!(classify_message_text(text), CatalogMessageText::Genuine(_))
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lineage_message_classification_separates_genuine_injected_and_absent() {
        // Genuine user text passes through trimmed.
        assert_eq!(
            classify_message_text("  fix the login bug  "),
            CatalogMessageText::Genuine("fix the login bug".to_owned())
        );
        // Empty/whitespace-only text is absent.
        assert_eq!(classify_message_text("   "), CatalogMessageText::Absent);
        assert_eq!(classify_message_text(""), CatalogMessageText::Absent);

        // A single injected-context envelope is InjectedOnly, not genuine.
        assert_eq!(
            classify_message_text("<system-reminder>context</system-reminder>"),
            CatalogMessageText::InjectedOnly
        );
        // AGENTS/project instruction envelopes are bootstrap content too, and
        // open tags may carry attributes.
        assert_eq!(
            classify_message_text(
                "<project_instructions path=\"/proj/AGENTS.md\">\nfollow the rules\n</project_instructions>"
            ),
            CatalogMessageText::InjectedOnly
        );
        assert_eq!(
            classify_message_text("<user_instructions>rules</user_instructions>"),
            CatalogMessageText::InjectedOnly
        );
        // Consecutive envelopes (with whitespace between) are all injected.
        assert_eq!(
            classify_message_text(
                "<environment_context>env</environment_context>\n<user_info>info</user_info>"
            ),
            CatalogMessageText::InjectedOnly
        );
        // Genuine text alongside an envelope makes the message genuine
        // (the text is kept verbatim — trimming only, no stripping).
        assert_eq!(
            classify_message_text("<system-reminder>ctx</system-reminder> now fix it"),
            CatalogMessageText::Genuine(
                "<system-reminder>ctx</system-reminder> now fix it".to_owned()
            )
        );
        // An unclosed envelope is NOT an envelope — it is genuine text.
        assert_eq!(
            classify_message_text("<system-reminder>never closed"),
            CatalogMessageText::Genuine("<system-reminder>never closed".to_owned())
        );
        // An unknown tag is genuine even if it looks envelope-ish.
        assert_eq!(
            classify_message_text("<unknown-tag>x</unknown-tag>"),
            CatalogMessageText::Genuine("<unknown-tag>x</unknown-tag>".to_owned())
        );
        // A lookalike tag that merely shares a prefix is genuine — the
        // attribute check rejects it before any envelope match.
        assert_eq!(
            classify_message_text("<user_informed>x</user_informed>"),
            CatalogMessageText::Genuine("<user_informed>x</user_informed>".to_owned())
        );
        // An unclosed instruction envelope (no close tag) is genuine.
        assert_eq!(
            classify_message_text("<project_instructions path=\"x\">body"),
            CatalogMessageText::Genuine("<project_instructions path=\"x\">body".to_owned())
        );
    }

    #[test]
    fn message_genuine_counts_images_but_not_bootstrap_envelopes() {
        let genuine = |message: Value| message_genuine(&json!({ "message": message }));

        // A real prompt is genuine.
        assert!(genuine(json!({"role":"user","content":"hello"})));
        // Image-only turns are genuine content even without text.
        assert!(genuine(
            json!({"role":"user","content":[{"type":"image"}]})
        ));
        // Bootstrap envelopes alone never make a message genuine.
        assert!(!genuine(
            json!({"role":"user","content":"<environment_context>env</environment_context>"})
        ));
        assert!(!genuine(
            json!({"role":"user","content":[{"type":"text","text":"<user_info>info</user_info>"}, {"type":"text","text":"<project_instructions path=\"a\">rules</project_instructions>"}]})
        ));
        // Envelope plus real text is genuine.
        assert!(genuine(
            json!({"role":"user","content":"<system-reminder>r</system-reminder> go"})
        ));
        // Empty content is neither meaningful nor genuine.
        assert!(!genuine(json!({"role":"assistant","content":[]})));
    }

    #[test]
    fn lineage_message_meaningful_filters_roles_and_empty_content() {
        let meaningful = |message: Value| message_meaningful(&json!({ "message": message }));

        // User + non-empty string content is meaningful.
        assert!(meaningful(json!({"role":"user","content":"hello"})));
        // Whitespace-only string content is not.
        assert!(!meaningful(json!({"role":"user","content":"   "})));
        // Tool/system/custom roles never count.
        assert!(!meaningful(json!({"role":"tool","content":"result"})));
        assert!(!meaningful(json!({"role":"system","content":"sys"})));

        // Block content: non-empty text counts.
        assert!(meaningful(
            json!({"role":"assistant","content":[{"type":"text","text":"hi"}]})
        ));
        // Empty text block does not.
        assert!(!meaningful(
            json!({"role":"assistant","content":[{"type":"text","text":""}]})
        ));
        // Image attachment counts (supported non-text content).
        assert!(meaningful(
            json!({"role":"user","content":[{"type":"image"}]})
        ));
        // Unsupported block types do not.
        assert!(!meaningful(
            json!({"role":"user","content":[{"type":"tool_use"}]})
        ));
        // Empty pending assistant placeholder (no blocks) is not meaningful.
        assert!(!meaningful(json!({"role":"assistant","content":[]})));
        // Missing content or non-message records are not.
        assert!(!meaningful(json!({"role":"user"})));
        assert!(!message_meaningful(&json!({"type":"custom"})));
    }
}
