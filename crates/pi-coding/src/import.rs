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
use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;
use walkdir::WalkDir;

use self::parsers::{ParsedSession, parse_source, source_id};
use crate::default_session_dir;

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
    /// Newly generated native Pi session id.
    pub id: String,
    pub path: PathBuf,
    pub cwd: PathBuf,
    pub messages: Vec<ImportedMessage>,
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

/// Resolve `input` as either a direct path or a source-specific session id,
/// convert it, and write a new Pi v3 session beneath the default per-cwd
/// session directory.
pub fn import_session(
    source: SourceSessionFormat,
    input: impl AsRef<Path>,
) -> Result<ImportedSession, ImportSessionError> {
    let source_path = resolve_input(source, input.as_ref())?;
    let parsed = parse_source(source, &source_path)?;
    ensure_messages(source, &source_path, &parsed)?;
    let cwd = usable_cwd(&parsed.cwd)?;
    let id = Uuid::now_v7().to_string();
    let start = session_start(&parsed);
    let output = default_session_dir(&cwd).join(session_filename(start, &id));
    emit(source, source_path, parsed, cwd, id, output)
}

/// Resolve `input`, convert it, and write a new Pi v3 session to `output`.
/// If `output` is an existing directory, the normal Pi session filename is
/// created inside it. Existing files are never overwritten.
pub fn import_session_to(
    source: SourceSessionFormat,
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
) -> Result<ImportedSession, ImportSessionError> {
    let source_path = resolve_input(source, input.as_ref())?;
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
    emit(source, source_path, parsed, cwd, id, output)
}

fn emit(
    source: SourceSessionFormat,
    source_path: PathBuf,
    parsed: ParsedSession,
    cwd: PathBuf,
    id: String,
    output: PathBuf,
) -> Result<ImportedSession, ImportSessionError> {
    let records = pi_records(source, &parsed, &cwd, &id);
    write_jsonl_new(&output, &records)?;
    Ok(ImportedSession {
        source,
        source_path,
        source_session_id: parsed.source_session_id,
        id,
        path: output,
        cwd,
        messages: parsed.messages,
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
) -> Vec<Value> {
    let start = session_start(session);
    let header_timestamp = format_timestamp(start);
    let model_entry_id = short_id();
    let thinking_entry_id = short_id();
    let provider = "pi-rs-import";
    let model = format!("converted-from-{source}");
    let mut records = vec![
        json!({
            "type": "session",
            "version": 3,
            "id": session_id,
            "timestamp": header_timestamp,
            "cwd": cwd,
        }),
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
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| ImportSessionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|source| ImportSessionError::Io {
            path: path.to_path_buf(),
            source,
        })
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
            let path = std::env::temp_dir().join(format!("pi-rs-import-{}", Uuid::new_v4()));
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
        assert_eq!(values[1]["type"], "model_change");
        assert_eq!(values[2]["type"], "thinking_level_change");
        assert_eq!(values[3]["message"]["content"][0]["text"], "question");
        assert_eq!(values[3]["timestamp"], "2026-06-01T00:00:01Z");
        assert_eq!(values[4]["message"]["content"][0]["text"], "answer");
        assert_eq!(values[4]["parentId"], values[3]["id"]);
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
