//! `memory` tool: persistent, per-project note-style memory.
//!
//! Entries live under the agent directory as a single JSON-lines file per
//! repository namespace:
//!
//! ```text
//! <agent_dir>/memory/<repo-digest>/entries.jsonl
//! ```
//!
//! `<repo-digest>` is the hex SHA-256 (first 16 bytes, 32 chars) of the
//! canonical working directory — anchored at the first ancestor containing a
//! `.git` entry when the session runs inside a repository, mirroring the
//! workflow worktree namespace pattern. It is deterministic for a given
//! checkout (so memory persists across sessions in the same project — the
//! whole point), distinct between projects, and free of path separators, so a
//! hostile cwd can never escape the memory root. `PI_CODING_AGENT_DIR` (or
//! `~/.pi/agent`) selects the agent directory, the same knob the rest of the
//! harness uses; no extra settings surface is introduced.
//!
//! Actions:
//! - `learn <content> [tags]` — append an entry stamped with the write time
//!   and the source session id (`PI_SESSION_ID`; `standalone` outside a
//!   session).
//! - `recall <query> [limit]` — case-insensitive keyword/substring search over
//!   content and tags, ranked by match strength then newest-first, bounded.
//! - `list [tag]` — newest-first listing with an optional exact tag filter.
//! - `forget <id>` — remove one entry by the id returned by learn/recall/list.
//!
//! Bounds: entry content ≤ 1 MiB, ≤ 100 entries per namespace (oldest evicted
//! on learn), output truncated to 32 KiB. Writes are serialized in-process and
//! applied atomically (temp file + rename + dir sync, the same primitive the
//! settings store uses). Cross-process writers are not coordinated; a single
//! pi instance serves one process, so lost updates only arise from two
//! instances sharing an agent dir concurrently.
//!
//! Secrets: entries are plain text the model wrote, but obvious credential
//! shapes (private-key blocks, `sk-…`, `ghp_…`, `github_pat_…`, `AKIA…`,
//! `Bearer …`) are redacted at the store boundary before persisting. This is a
//! best-effort guard, not a secrets store — do not rely on it for real
//! credentials.
//!
//! Autolearn (automatic memory extraction from turns) is explicitly out of
//! scope for this MVP; the tool is model-invoked only.
//!
//! ## External Hindsight backend
//!
//! When `settings.memory.backend` is `hindsight`, the memory tools are
//! `recall`/`retain`/`reflect`, backed directly by the source-verified
//! Hindsight HTTP API. Requests and responses are bounded, every operation has
//! an explicit timeout, bearer credentials are never rendered, and plaintext
//! endpoints require an explicit opt-in. `off` hides every memory tool;
//! `local` (default) is the JSONL store above.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions as CapOpenOptions};
use pi_agent::{AbortSignal, AgentTool, AgentToolResult, ToolCallContext, ToolCapability};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::redact::redact_secrets;
use crate::settings::atomic_write;
use crate::tools::{
    SessionEnvFn, arg_int, arg_str, check_aborted, s_array, s_number, s_object, s_string,
    text_result,
};
use crate::truncate::truncate_head;
use pi_ai::Schema;

const MEMORY_DIR_NAME: &str = "memory";
const ENTRIES_FILE_NAME: &str = "entries.jsonl";
/// Maximum entries kept per namespace; learn evicts the oldest beyond this.
const MAX_ENTRIES: usize = 100;
const MAX_ENTRY_BYTES: usize = 1024 * 1024;
const MAX_ENTRY_BYTES_LABEL: &str = "1 MiB";
const RECALL_DEFAULT_LIMIT: usize = 10;
const RECALL_MAX_LIMIT: usize = 50;
const MAX_TAGS: usize = 20;
/// Tool output budget (matches the github tool's bounded-output convention).
const OUTPUT_MAX_BYTES: usize = 32 * 1024;
const RECALL_ENTRY_PREVIEW_BYTES: usize = 2048;
const LIST_PREVIEW_CHARS: usize = 160;

/// Serializes in-process writers; combined with atomic temp-file renames this
/// makes concurrent learns/forgets in the same process safe.
static MEMORY_STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// One stored memory entry. `tags`, `ts`, and `session` carry serde defaults so
/// a hand-edited or truncated line degrades gracefully instead of failing the
/// whole namespace read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MemoryEntry {
    pub(crate) id: String,
    pub(crate) content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) tags: Vec<String>,
    #[serde(default)]
    pub(crate) ts: i64,
    #[serde(default)]
    pub(crate) session: String,
}

pub(crate) struct MemoryStore {
    entries_path: PathBuf,
    persona_root: Option<PathBuf>,
}

impl MemoryStore {
    /// Builds the store for the default agent directory and the repo digest of
    /// `cwd`. The digest namespace is always valid, so this never fails.
    pub(crate) fn default_for(cwd: &Path) -> Arc<MemoryStore> {
        let namespace = repo_namespace(cwd);
        Arc::new(
            MemoryStore::new(&crate::resources::agent_dir_path(), &namespace)
                .expect("digest namespace is always path-safe"),
        )
    }

    /// Builds a store rooted at `agent_dir` under the given namespace.
    /// Rejects namespaces that could traverse out of the memory root.
    pub(crate) fn new(agent_dir: &Path, namespace: &str) -> Result<Self> {
        validate_namespace(namespace)?;
        Ok(Self {
            entries_path: agent_dir
                .join(MEMORY_DIR_NAME)
                .join(namespace)
                .join(ENTRIES_FILE_NAME),
            persona_root: None,
        })
    }
    /// Builds a persona-local store at the durable persona layout's exact
    /// `<persona-root>/memory/entries.jsonl` path. Every operation revalidates
    /// direct non-symlink containment before access.
    pub(crate) fn persona(persona_root: &Path) -> Arc<MemoryStore> {
        Arc::new(Self {
            entries_path: persona_root.join(MEMORY_DIR_NAME).join(ENTRIES_FILE_NAME),
            persona_root: Some(persona_root.to_path_buf()),
        })
    }

    pub(crate) fn learn(&self, content: &str, tags: Vec<String>, session: &str) -> Result<(String, usize)> {
        self.learn_with_ts(content, tags, session, pi_ai::now_millis())
    }

    /// [`Self::learn`] with an explicit timestamp (deterministic tests).
    pub(crate) fn learn_with_ts(
        &self,
        content: &str,
        tags: Vec<String>,
        session: &str,
        ts: i64,
    ) -> Result<(String, usize)> {
        if content.len() > MAX_ENTRY_BYTES {
            return Err(anyhow!(
                "memory entry too large: {} bytes exceeds the {MAX_ENTRY_BYTES_LABEL} limit",
                content.len()
            ));
        }
        let id = Uuid::new_v4().simple().to_string();
        let entry = MemoryEntry {
            id: id.clone(),
            content: redact_secrets(content),
            tags: tags.into_iter().filter_map(|tag| sanitize_tag(&tag)).collect(),
            ts,
            session: sanitize_session(session),
        };
        let _guard = MEMORY_STORE_LOCK.lock().expect("memory lock poisoned");
        let mut entries = self.read_entries()?;
        entries.push(entry);
        entries.sort_by(entry_order);
        entries.truncate(MAX_ENTRIES);
        let count = entries.len();
        self.write_entries(&entries)?;
        Ok((id, count))
    }

    /// Keyword/substring search: entries scoring above zero, ranked by match
    /// strength (full-phrase bonus + one point per matched query term) then
    /// newest-first, capped at `limit`.
    pub(crate) fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryEntry>> {
        let terms = query_terms(query);
        let full_query = query.trim().to_lowercase();
        let _guard = MEMORY_STORE_LOCK.lock().expect("memory lock poisoned");
        let mut scored: Vec<(usize, MemoryEntry)> = self
            .read_entries()?
            .into_iter()
            .filter_map(|entry| {
                let score = score_entry(&entry, &terms, &full_query);
                (score > 0).then_some((score, entry))
            })
            .collect();
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0).then_with(|| entry_order(&a.1, &b.1))
        });
        Ok(scored.into_iter().take(limit).map(|(_, entry)| entry).collect())
    }

    /// Newest-first listing, optionally filtered to entries carrying `tag`
    /// (case-insensitive exact match).
    pub(crate) fn list(&self, tag: Option<&str>) -> Result<Vec<MemoryEntry>> {
        let _guard = MEMORY_STORE_LOCK.lock().expect("memory lock poisoned");
        let mut entries = self.read_entries()?;
        if let Some(tag) = tag {
            let tag = tag.trim().to_lowercase();
            if !tag.is_empty() {
                entries.retain(|entry| entry.tags.iter().any(|t| t.to_lowercase() == tag));
            }
        }
        Ok(entries)
    }

    pub(crate) fn forget(&self, id: &str) -> Result<bool> {
        let _guard = MEMORY_STORE_LOCK.lock().expect("memory lock poisoned");
        let mut entries = self.read_entries()?;
        let before = entries.len();
        entries.retain(|entry| entry.id != id);
        if entries.len() == before {
            return Ok(false);
        }
        self.write_entries(&entries)?;
        Ok(true)
    }

    /// Loads all entries. A missing file is an empty store. Malformed lines
    /// are skipped: the file is only ever written atomically by this module,
    /// so a bad line implies external tampering, and failing the whole read
    /// would brick recall/list for the project over one stray byte.
    fn read_entries(&self) -> Result<Vec<MemoryEntry>> {
        let raw = match self.persona_root.as_deref() {
            Some(root) => read_persona_memory(root)?,
            None => match std::fs::read(&self.entries_path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(_) => return Err(anyhow!("failed to read memory entries")),
            },
        };
        let mut entries = Vec::new();
        for line in String::from_utf8_lossy(&raw).lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(entry) = serde_json::from_str::<MemoryEntry>(line) {
                entries.push(entry);
            }
        }
        Ok(entries)
    }

    fn write_entries(&self, entries: &[MemoryEntry]) -> Result<()> {
        let mut bytes = Vec::with_capacity(entries.len() * 160);
        for entry in entries {
            serde_json::to_writer(&mut bytes, entry)
                .context("serializing memory entry")?;
            bytes.push(b'\n');
        }
        match self.persona_root.as_deref() {
            Some(root) => write_persona_memory(root, &bytes),
            None => atomic_write(&self.entries_path, &bytes)
                .map_err(|_| anyhow!("failed to persist memory entries")),
        }
    }
}

fn persona_memory_directory(persona_root: &Path, create: bool) -> Result<Option<Dir>> {
    let root_metadata = std::fs::symlink_metadata(persona_root)
        .context("reading persona root metadata")?;
    if root_metadata.file_type().is_symlink() || !root_metadata.file_type().is_dir() {
        bail!("persona root must be a non-symlink directory");
    }
    let canonical_root = std::fs::canonicalize(persona_root)
        .context("resolving persona root")?;
    let root = Dir::open_ambient_dir(&canonical_root, cap_std::ambient_authority())
        .context("opening persona root")?;
    match root.open_dir(MEMORY_DIR_NAME) {
        Ok(memory) => Ok(Some(memory)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
            root.create_dir(MEMORY_DIR_NAME)
                .context("creating persona memory directory")?;
            root.open_dir(MEMORY_DIR_NAME)
                .map(Some)
                .context("opening created persona memory directory")
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => bail!("persona memory path must be a non-symlink directory"),
    }
}

fn read_persona_memory(persona_root: &Path) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let Some(memory) = persona_memory_directory(persona_root, false)? else {
        return Ok(Vec::new());
    };
    let mut options = CapOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let mut file = match memory.open_with(ENTRIES_FILE_NAME, &options) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => bail!("persona memory entries must be a regular non-symlink file"),
    };
    if !file.metadata()?.is_file() {
        bail!("persona memory entries must be a regular non-symlink file");
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .context("reading persona memory entries")?;
    Ok(bytes)
}

fn write_persona_memory(persona_root: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let memory = persona_memory_directory(persona_root, true)?
        .ok_or_else(|| anyhow!("persona memory directory was not created"))?;
    match memory.symlink_metadata(ENTRIES_FILE_NAME) {
        Ok(metadata) if metadata.is_symlink() || !metadata.is_file() => {
            bail!("persona memory entries must be a regular non-symlink file");
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("reading persona memory entries metadata"),
    }
    let temporary = format!(".{ENTRIES_FILE_NAME}.{}.tmp", Uuid::new_v4().simple());
    let result = (|| -> Result<()> {
        let mut options = CapOpenOptions::new();
        options.write(true).create_new(true).follow(FollowSymlinks::No);
        let mut file = memory
            .open_with(&temporary, &options)
            .context("creating persona memory temporary")?;
        file.write_all(bytes)
            .context("writing persona memory temporary")?;
        file.sync_all().context("syncing persona memory temporary")?;
        drop(file);
        memory
            .rename(&temporary, &memory, ENTRIES_FILE_NAME)
            .context("installing persona memory entries")?;
        let mut directory_options = CapOpenOptions::new();
        directory_options.read(true).follow(FollowSymlinks::No);
        memory
            .open_with(".", &directory_options)
            .context("opening persona memory directory for sync")?
            .into_std()
            .sync_all()
            .context("syncing persona memory directory")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = memory.remove_file(&temporary);
    }
    result
}

fn entry_order(a: &MemoryEntry, b: &MemoryEntry) -> std::cmp::Ordering {
    b.ts.cmp(&a.ts).then_with(|| a.id.cmp(&b.id))
}

fn query_terms(query: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut terms = Vec::new();
    for raw in query.split_whitespace() {
        let term = raw.to_lowercase();
        if !term.is_empty() && seen.insert(term.clone()) {
            terms.push(term);
        }
    }
    terms
}

/// Match strength of one entry: +2 when the whole query is a substring of the
/// haystack (content + tags, lowercased), +1 per query term found.
fn score_entry(entry: &MemoryEntry, terms: &[String], full_query: &str) -> usize {
    let haystack = format!("{} {}", entry.content, entry.tags.join(" ")).to_lowercase();
    let mut score = 0;
    if !full_query.is_empty() && haystack.contains(full_query) {
        score += 2;
    }
    for term in terms {
        if haystack.contains(term) {
            score += 1;
        }
    }
    score
}

/// Filesystem-safe repo namespace: hex SHA-256 of the canonical working
/// directory, anchored at the first ancestor holding a `.git` entry so nested
/// sessions in the same checkout share one store. Always 32 hex chars — no
/// separators, no traversal, stable across sessions.
pub(crate) fn repo_namespace(cwd: &Path) -> String {
    repo_digest_hex(&repo_anchor(cwd))
}

/// Canonical repository anchor: the cwd resolved to an absolute path and
/// anchored at the first ancestor holding a `.git` entry so nested paths in
/// one checkout share an identity. Falls back to the canonical cwd when no
/// repository is present, which stays deterministic for a given directory.
fn repo_anchor(cwd: &Path) -> PathBuf {
    let canonical = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    git_anchor(&canonical).unwrap_or(canonical)
}

/// Hex SHA-256 (first 16 bytes, 32 chars) of a canonical repository anchor.
/// This single digest is the repository identity shared by the local JSONL
/// namespace and the Hindsight per-project scopes, so both resolve nested
/// paths in one checkout identically and same-named repos apart.
fn repo_digest_hex(anchor: &Path) -> String {
    let digest = Sha256::digest(anchor.as_os_str().as_encoded_bytes());
    let mut hex = String::with_capacity(32);
    for byte in digest.iter().take(16) {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

fn git_anchor(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if d.join(".git").exists() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

/// Rejects namespaces that could escape the memory root or embed separators.
/// The factory only ever produces hex digests, so this is defense in depth.
fn validate_namespace(namespace: &str) -> Result<()> {
    let unsafe_segment = namespace.is_empty()
        || namespace.len() > 64
        || namespace.contains(['/', '\\', ':', '\0'])
        || matches!(namespace, "." | "..");
    if unsafe_segment {
        return Err(anyhow!("invalid memory namespace: {namespace:?}"));
    }
    Ok(())
}

/// Tags are short tokens; drop control characters and over-long/empty values.
fn sanitize_tag(tag: &str) -> Option<String> {
    let cleaned: String = tag.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim();
    (!cleaned.is_empty() && cleaned.chars().count() <= 64).then(|| cleaned.to_owned())
}

/// Session ids can come from foreign session files; neutralize control chars
/// so a hostile id cannot spoof rendered output.
fn sanitize_session(session: &str) -> String {
    let cleaned: String = session.chars().filter(|c| !c.is_control()).collect();
    cleaned
}

/// Parses and bounds the optional `tags` argument (deduped case-insensitively).
fn parse_tags(args: &Value) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut tags = Vec::new();
    if let Some(list) = args.get("tags").and_then(Value::as_array) {
        for value in list {
            if let Some(tag) = value.as_str().and_then(sanitize_tag) {
                if seen.insert(tag.to_lowercase()) {
                    tags.push(tag);
                }
                if tags.len() >= MAX_TAGS {
                    break;
                }
            }
        }
    }
    tags
}

/// The source session for provenance: `PI_SESSION_ID` from the live session
/// environment, `session` when a session exists but exposes no id, or
/// `standalone` for factory-built tools outside any session.
fn source_session(session_env: Option<&SessionEnvFn>) -> String {
    match session_env {
        Some(env) => env()
            .get("PI_SESSION_ID")
            .filter(|id| !id.is_empty())
            .cloned()
            .unwrap_or_else(|| "session".to_owned()),
        None => "standalone".to_owned(),
    }
}

fn format_ts(ts: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ts)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| "?".to_owned())
}

fn render_entry_header(entry: &MemoryEntry) -> String {
    let mut header = format!(
        "{} · {} · session {}",
        entry.id,
        format_ts(entry.ts),
        sanitize_session(&entry.session)
    );
    if !entry.tags.is_empty() {
        header.push_str(" · tags: ");
        header.push_str(&entry.tags.join(", "));
    }
    header
}

fn render_recall(entries: &[MemoryEntry], query: &str) -> String {
    if entries.is_empty() {
        return format!("No memory entries match {query:?}.");
    }
    let mut out = String::new();
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&render_entry_header(entry));
        out.push('\n');
        let preview = truncate_head(&entry.content, usize::MAX, RECALL_ENTRY_PREVIEW_BYTES).content;
        out.push_str(&preview);
    }
    out
}

fn render_list(entries: &[MemoryEntry]) -> String {
    if entries.is_empty() {
        return "No memory entries.".to_owned();
    }
    let mut out = String::new();
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&render_entry_header(entry));
        out.push('\n');
        let first_line = entry.content.lines().next().unwrap_or("");
        let preview = truncate_head(first_line, usize::MAX, LIST_PREVIEW_CHARS).content;
        out.push_str(&preview);
    }
    out
}

/// Builds the `memory` tool. The store root derives from `cwd` + agent dir at
/// construction; the source session is resolved per call.
pub(crate) fn memory_tool(cwd: &str) -> AgentTool {
    memory_tool_with_store(MemoryStore::default_for(Path::new(cwd)), None)
}

/// [`memory_tool`] with a live session-environment provider so entries record
/// the owning `PI_SESSION_ID` as provenance.
pub(crate) fn memory_tool_with_session_env(
    cwd: &str,
    session_env: Option<SessionEnvFn>,
) -> AgentTool {
    memory_tool_with_store(MemoryStore::default_for(Path::new(cwd)), session_env)
}
/// [`memory_tool_with_session_env`] rooted at a durable persona's exact
/// `<persona-root>/memory/entries.jsonl` store.
pub(crate) fn persona_memory_tool_with_session_env(
    persona_root: &Path,
    session_env: Option<SessionEnvFn>,
) -> AgentTool {
    memory_tool_with_store(MemoryStore::persona(persona_root), session_env)
}

/// [`memory_tool`] over an explicitly rooted store (hermetic tests).
fn memory_tool_with_store(store: Arc<MemoryStore>, session_env: Option<SessionEnvFn>) -> AgentTool {
    let op_schema = schema_with_enum(
        "Memory action to perform",
        ["learn", "recall", "list", "forget"],
    );
    let parameters = s_object(
        vec![
            ("op", op_schema),
            (
                "content",
                s_string("Entry content (required for learn; at most 1 MiB)"),
            ),
            ("tags", s_array(s_string("tag"), "Optional tags (learn, at most 20)")),
            ("query", s_string("Search query (required for recall)")),
            (
                "limit",
                s_number("Max results (recall; default 10, max 50)"),
            ),
            ("tag", s_string("Exact tag filter (list)")),
            ("id", s_string("Entry id to remove (required for forget)")),
        ],
        vec!["op"],
    );
    let description = format!(
        "Persistent per-project memory: learn, recall, list, and forget note-style entries \
         that survive across sessions in the same repository. learn appends a timestamped \
         entry (optionally tagged); recall searches content and tags with keyword matching, \
         newest first; list shows entries with an optional tag filter; forget removes by id. \
         Output is bounded to {} KiB.",
        OUTPUT_MAX_BYTES / 1024
    );
    AgentTool::new("memory", description, parameters, move |ctx| {
        let store = store.clone();
        let session_env = session_env.clone();
        async move { run_memory(store, session_env, ctx.arguments, ctx.abort).await }
    })
    .with_capability(ToolCapability::Write)
}

/// String schema with an enum constraint (mirrors the github tool).
fn schema_with_enum(description: &str, values: [&str; 4]) -> Schema {
    Schema {
        schema_type: Some(Value::String("string".into())),
        description: Some(description.to_string()),
        enum_values: values.iter().map(|value| Value::String((*value).to_owned())).collect(),
        ..Default::default()
    }
}

pub(crate) async fn run_memory(
    store: Arc<MemoryStore>,
    session_env: Option<SessionEnvFn>,
    args: Value,
    abort: AbortSignal,
) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    let op = arg_str(&args, "op");
    let rendered = match op.as_str() {
        "learn" => {
            let content = arg_str(&args, "content");
            if content.trim().is_empty() {
                return Err(anyhow!("content is required for memory learn"));
            }
            let (id, count) =
                store.learn(&content, parse_tags(&args), &source_session(session_env.as_ref()))?;
            format!("Learned memory entry {id} ({count} total for this project).")
        }
        "recall" => {
            let query = arg_str(&args, "query");
            if query.trim().is_empty() {
                return Err(anyhow!("query is required for memory recall"));
            }
            let limit = arg_int(&args, "limit")?
                .map(|value| usize::try_from(value).unwrap_or(RECALL_MAX_LIMIT))
                .unwrap_or(RECALL_DEFAULT_LIMIT)
                .clamp(1, RECALL_MAX_LIMIT);
            let entries = store.recall(&query, limit)?;
            render_recall(&entries, &query)
        }
        "list" => {
            let tag = arg_str(&args, "tag");
            let entries = store.list((!tag.is_empty()).then_some(tag.as_str()))?;
            render_list(&entries)
        }
        "forget" => {
            let id = arg_str(&args, "id");
            if id.trim().is_empty() {
                return Err(anyhow!("id is required for memory forget"));
            }
            if store.forget(&id)? {
                format!("Forgot memory entry {id}.")
            } else {
                format!("No memory entry with id {id}.")
            }
        }
        other => return Err(anyhow!("Unknown memory op: {other}")),
    };
    check_aborted(&abort)?;
    let truncated = truncate_head(&rendered, usize::MAX, OUTPUT_MAX_BYTES);
    Ok(text_result(truncated.content))
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// External Hindsight HTTP backend
// ---------------------------------------------------------------------------

pub const DEFAULT_HINDSIGHT_BANK_ID: &str = "rpi";
pub const DEFAULT_HINDSIGHT_RECALL_TYPES: &[&str] = &["world", "experience"];
pub const DEFAULT_HINDSIGHT_RECALL_MAX_TOKENS: u64 = 1024;
pub const DEFAULT_HINDSIGHT_REQUEST_TIMEOUT_MS: u64 = 30_000;
pub const DEFAULT_HINDSIGHT_RECALL_TIMEOUT_MS: u64 = 30_000;
pub const DEFAULT_HINDSIGHT_RETAIN_TIMEOUT_MS: u64 = 60_000;
pub const DEFAULT_HINDSIGHT_REFLECT_TIMEOUT_MS: u64 = 120_000;
const HINDSIGHT_RESPONSE_MAX_BYTES: usize = 256 * 1024;
const HINDSIGHT_ERROR_MAX_BYTES: usize = 8 * 1024;
const HINDSIGHT_INPUT_MAX_BYTES: usize = 1024 * 1024;
/// Hex chars of the repository identity digest embedded in Hindsight
/// per-project scope labels. 48 bits separates same-named checkouts with
/// negligible collision odds while keeping bank ids and tags readable.
const HINDSIGHT_SCOPE_DIGEST_PREFIX: usize = 12;

#[derive(Clone, PartialEq, Eq)]
pub struct MemoryConfig {
    pub backend: crate::settings::MemoryBackend,
    pub hindsight_api_url: Option<String>,
    pub hindsight_api_token: Option<String>,
    pub hindsight_allow_insecure: bool,
    pub hindsight_bank_id: String,
    pub hindsight_bank_id_prefix: Option<String>,
    pub hindsight_scoping: crate::settings::HindsightScoping,
    pub hindsight_bank_mission: Option<String>,
    pub hindsight_retain_mission: Option<String>,
    pub hindsight_injection: bool,
    pub hindsight_recall_budget: crate::settings::HindsightBudget,
    pub hindsight_recall_max_tokens: u64,
    pub hindsight_recall_types: Vec<String>,
    pub hindsight_request_timeout_ms: u64,
    pub hindsight_recall_timeout_ms: u64,
    pub hindsight_retain_timeout_ms: u64,
    pub hindsight_reflect_timeout_ms: u64,
}
impl std::fmt::Debug for MemoryConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MemoryConfig")
            .field("backend", &self.backend)
            .field("hindsight_api_url", &self.hindsight_api_url)
            .field(
                "hindsight_api_token",
                &self.hindsight_api_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("hindsight_allow_insecure", &self.hindsight_allow_insecure)
            .field("hindsight_bank_id", &self.hindsight_bank_id)
            .field("hindsight_bank_id_prefix", &self.hindsight_bank_id_prefix)
            .field("hindsight_scoping", &self.hindsight_scoping)
            .field("hindsight_bank_mission", &self.hindsight_bank_mission)
            .field("hindsight_retain_mission", &self.hindsight_retain_mission)
            .field("hindsight_injection", &self.hindsight_injection)
            .field("hindsight_recall_budget", &self.hindsight_recall_budget)
            .field("hindsight_recall_max_tokens", &self.hindsight_recall_max_tokens)
            .field("hindsight_recall_types", &self.hindsight_recall_types)
            .field("hindsight_request_timeout_ms", &self.hindsight_request_timeout_ms)
            .field("hindsight_recall_timeout_ms", &self.hindsight_recall_timeout_ms)
            .field("hindsight_retain_timeout_ms", &self.hindsight_retain_timeout_ms)
            .field("hindsight_reflect_timeout_ms", &self.hindsight_reflect_timeout_ms)
            .finish()
    }
}


impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            backend: crate::settings::MemoryBackend::Local,
            hindsight_api_url: None,
            hindsight_api_token: None,
            hindsight_allow_insecure: false,
            hindsight_bank_id: DEFAULT_HINDSIGHT_BANK_ID.to_owned(),
            hindsight_bank_id_prefix: None,
            hindsight_scoping: crate::settings::HindsightScoping::default(),
            hindsight_bank_mission: None,
            hindsight_retain_mission: None,
            hindsight_injection: false,
            hindsight_recall_budget: crate::settings::HindsightBudget::default(),
            hindsight_recall_max_tokens: DEFAULT_HINDSIGHT_RECALL_MAX_TOKENS,
            hindsight_recall_types: DEFAULT_HINDSIGHT_RECALL_TYPES.iter().map(|value| (*value).to_owned()).collect(),
            hindsight_request_timeout_ms: DEFAULT_HINDSIGHT_REQUEST_TIMEOUT_MS,
            hindsight_recall_timeout_ms: DEFAULT_HINDSIGHT_RECALL_TIMEOUT_MS,
            hindsight_retain_timeout_ms: DEFAULT_HINDSIGHT_RETAIN_TIMEOUT_MS,
            hindsight_reflect_timeout_ms: DEFAULT_HINDSIGHT_REFLECT_TIMEOUT_MS,
        }
    }
}

/// Which Hindsight bank (and, for tagged scoping, which tags) a session's
/// memory operations target. Per-project scopes key off the canonical
/// repository identity digest shared with the local store, so nested paths in
/// one checkout resolve identically while same-named repos under different
/// parents stay apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HindsightBankScope {
    pub bank_id: String,
    pub retain_tags: Vec<String>,
    pub recall_tags: Vec<String>,
    pub recall_tags_match: Option<&'static str>,
}

pub(crate) fn hindsight_bank_scope(config: &MemoryConfig, cwd: &Path) -> HindsightBankScope {
    let base = config.hindsight_bank_id_prefix.as_deref()
        .map(|prefix| format!("{prefix}-{}", config.hindsight_bank_id))
        .unwrap_or_else(|| config.hindsight_bank_id.clone());
    match config.hindsight_scoping {
        crate::settings::HindsightScoping::Global => HindsightBankScope { bank_id: base, retain_tags: Vec::new(), recall_tags: Vec::new(), recall_tags_match: None },
        crate::settings::HindsightScoping::PerProject => HindsightBankScope { bank_id: format!("{base}-{}", hindsight_project_label(cwd)), retain_tags: Vec::new(), recall_tags: Vec::new(), recall_tags_match: None },
        crate::settings::HindsightScoping::PerProjectTagged => {
            let tag = format!("project:{}", hindsight_project_label(cwd));
            HindsightBankScope { bank_id: base, retain_tags: vec![tag.clone()], recall_tags: vec![tag], recall_tags_match: Some("any") }
        }
    }
}

/// Sanitized, collision-resistant project scope label: the repository basename
/// sanitized to characters safe in bank ids and tags, plus a prefix of the
/// same canonical identity digest local memory digests. Nested paths inside
/// one checkout anchor at the same `.git` ancestor (so they share a label);
/// same-named repos under different parents differ in the digest prefix.
fn hindsight_project_label(cwd: &Path) -> String {
    let anchor = repo_anchor(cwd);
    let basename = anchor
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("unknown");
    let digest = repo_digest_hex(&anchor);
    format!(
        "{}-{}",
        sanitize_scope_label(basename),
        &digest[..HINDSIGHT_SCOPE_DIGEST_PREFIX]
    )
}

/// Basenames are user-controlled; keep only characters that are safe in
/// Hindsight bank ids and tags (ASCII alphanumerics plus `-`, `_`, `.`),
/// mapping everything else to `-`, then collapsing separator runs and trimming
/// edges. Falls back to `unknown` when nothing readable remains.
fn sanitize_scope_label(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        out.push(if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            ch
        } else {
            '-'
        });
    }
    let mut parts = out.split('-').filter(|part| !part.is_empty());
    let mut label = String::with_capacity(out.len());
    if let Some(first) = parts.next() {
        label.push_str(first);
        for part in parts {
            label.push('-');
            label.push_str(part);
        }
    }
    if label.is_empty() { "unknown".to_owned() } else { label }
}

#[derive(Clone)]
pub(crate) struct HindsightClient {
    client: reqwest::Client,
    base_url: url::Url,
    api_token: Option<String>,
    scope: HindsightBankScope,
    config: MemoryConfig,
}

#[derive(Debug, Deserialize)]
struct HindsightRecallResponse { #[serde(default)] results: Vec<HindsightRecallResult> }
#[derive(Debug, Deserialize)]
struct HindsightRecallResult {
    text: String,
    #[serde(default, rename = "type")] memory_type: Option<String>,
    #[serde(default)] mentioned_at: Option<String>,
}
#[derive(Debug, Deserialize)]
struct HindsightReflectResponse { #[serde(default)] text: String }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HindsightRedirectDecision {
    Follow,
    RejectPlaintext,
    RejectLimit,
}

fn hindsight_redirect_decision(
    allow_insecure: bool,
    previous_scheme: &str,
    next_scheme: &str,
    previous_hops: usize,
) -> HindsightRedirectDecision {
    const MAX_REDIRECTS: usize = 10;
    if !allow_insecure && (previous_scheme != "https" || next_scheme != "https") {
        HindsightRedirectDecision::RejectPlaintext
    } else if previous_hops > MAX_REDIRECTS {
        HindsightRedirectDecision::RejectLimit
    } else {
        HindsightRedirectDecision::Follow
    }
}

fn hindsight_http_client(allow_insecure: bool) -> Result<reqwest::Client> {
    hindsight_http_client_builder(allow_insecure)
        .build()
        .map_err(|_| anyhow!("failed to initialize Hindsight HTTP client"))
}

fn hindsight_http_client_builder(allow_insecure: bool) -> reqwest::ClientBuilder {
    let redirect = reqwest::redirect::Policy::custom(move |attempt| {
        let previous_scheme = attempt.previous().last().map(url::Url::scheme).unwrap_or("");
        match hindsight_redirect_decision(allow_insecure, previous_scheme, attempt.url().scheme(), attempt.previous().len()) {
            HindsightRedirectDecision::Follow => attempt.follow(),
            HindsightRedirectDecision::RejectPlaintext => attempt.error("refusing plaintext Hindsight redirect"),
            HindsightRedirectDecision::RejectLimit => attempt.error("too many Hindsight redirects"),
        }
    });
    reqwest::Client::builder()
        .user_agent("pi-rs")
        .https_only(!allow_insecure)
        .redirect(redirect)
}


impl HindsightClient {
    pub(crate) fn new(config: &MemoryConfig, cwd: &Path) -> Result<Self> {
        let raw_url = config.hindsight_api_url.as_deref().ok_or_else(|| anyhow!("Hindsight is not configured: set settings.memory.hindsightApiUrl"))?;
        let mut base_url = url::Url::parse(raw_url).map_err(|_| anyhow!("settings.memory.hindsightApiUrl is not a valid URL"))?;
        match base_url.scheme() {
            "https" => {}
            "http" if config.hindsight_allow_insecure => {}
            "http" => bail!("refusing plaintext Hindsight API endpoint; use https or set settings.memory.hindsightAllowInsecure=true"),
            scheme => bail!("unsupported Hindsight API URL scheme {scheme:?}; use http or https"),
        }
        base_url.set_query(None);
        base_url.set_fragment(None);
        let client = hindsight_http_client(config.hindsight_allow_insecure)?;
        Ok(Self { client, base_url, api_token: config.hindsight_api_token.clone(), scope: hindsight_bank_scope(config, cwd), config: config.clone() })
    }

    pub(crate) async fn recall(&self, query: &str, abort: &AbortSignal) -> Result<String> {
        validate_required_hindsight_input("recall query", query)?;
        let body = serde_json::json!({
            "query": query,
            "types": self.config.hindsight_recall_types,
            "max_tokens": self.config.hindsight_recall_max_tokens,
            "budget": hindsight_budget_name(self.config.hindsight_recall_budget),
            "tags": self.scope.recall_tags,
            "tags_match": self.scope.recall_tags_match,
        });
        let response: HindsightRecallResponse = self.request("POST", "memories/recall", "recall", Some(body), self.config.hindsight_recall_timeout_ms, abort).await?;
        Ok(format_hindsight_memories(&response.results))
    }

    pub(crate) async fn retain(&self, content: &str, context: Option<&str>, abort: &AbortSignal) -> Result<String> {
        validate_required_hindsight_input("retain content", content)?;
        if let Some(context) = context { validate_hindsight_input("retain context", context)?; }
        self.ensure_bank(abort).await?;
        let body = serde_json::json!({ "items": [{ "content": content, "context": context, "tags": self.scope.retain_tags }], "async": false });
        let _: Value = self.request("POST", "memories", "retain", Some(body), self.config.hindsight_retain_timeout_ms, abort).await?;
        Ok("1 memory retained.".to_owned())
    }

    pub(crate) async fn reflect(&self, query: &str, context: Option<&str>, abort: &AbortSignal) -> Result<String> {
        validate_required_hindsight_input("reflect query", query)?;
        if let Some(context) = context { validate_hindsight_input("reflect context", context)?; }
        self.ensure_bank(abort).await?;
        let body = serde_json::json!({
            "query": query,
            "context": context,
            "budget": hindsight_budget_name(self.config.hindsight_recall_budget),
            "tags": self.scope.recall_tags,
            "tags_match": self.scope.recall_tags_match,
        });
        let response: HindsightReflectResponse = self.request("POST", "reflect", "reflect", Some(body), self.config.hindsight_reflect_timeout_ms, abort).await?;
        let text = response.text.trim();
        Ok(if text.is_empty() { "No relevant information found to reflect on.".to_owned() } else { redact_secrets(text) })
    }

    async fn ensure_bank(&self, abort: &AbortSignal) -> Result<()> {
        let body = serde_json::json!({ "reflect_mission": self.config.hindsight_bank_mission, "retain_mission": self.config.hindsight_retain_mission });
        self.request::<Value>("PUT", "", "createBank", Some(body), self.config.hindsight_request_timeout_ms, abort).await?;
        Ok(())
    }

    async fn request<T: serde::de::DeserializeOwned>(&self, method: &str, suffix: &str, operation: &str, body: Option<Value>, timeout_ms: u64, abort: &AbortSignal) -> Result<T> {
        check_aborted(abort)?;
        let mut url = self.base_url.clone();
        let mut path = url.path().trim_end_matches('/').to_owned();
        path.push_str("/v1/default/banks/");
        path.push_str(&url::form_urlencoded::byte_serialize(self.scope.bank_id.as_bytes()).collect::<String>());
        if !suffix.is_empty() { path.push('/'); path.push_str(suffix); }
        url.set_path(&path);
        let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|_| anyhow!("invalid Hindsight HTTP method"))?;
        let mut request = self.client.request(method, url).timeout(std::time::Duration::from_millis(timeout_ms));
        if let Some(token) = self.api_token.as_deref() { request = request.bearer_auth(token); }
        if let Some(body) = body { request = request.json(&body); }
        let send = request.send();
        tokio::pin!(send);
        let response = tokio::select! {
            _ = abort.cancelled() => return Err(anyhow!("operation aborted")),
            response = &mut send => response.map_err(|error| if error.is_timeout() {
                anyhow!("hindsight {operation} request timed out after {}s", timeout_ms.div_ceil(1000))
            } else {
                anyhow!("hindsight {operation} request failed: {}", redact_secrets(&error.to_string()))
            })?,
        };
        let status = response.status();
        let bytes = read_bounded_response(response, HINDSIGHT_RESPONSE_MAX_BYTES).await?;
        if !status.is_success() {
            let detail = redact_secrets(&String::from_utf8_lossy(&bytes[..bytes.len().min(HINDSIGHT_ERROR_MAX_BYTES)]));
            bail!("hindsight {operation} failed (HTTP {}): {}", status.as_u16(), detail.trim());
        }
        serde_json::from_slice(&bytes).map_err(|_| anyhow!("hindsight {operation} returned invalid JSON"))
    }
}

async fn read_bounded_response(mut response: reqwest::Response, cap: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(cap.min(8192));
    while let Some(chunk) = response.chunk().await.map_err(|_| anyhow!("failed to read Hindsight response"))? {
        if bytes.len().saturating_add(chunk.len()) > cap { bail!("Hindsight response exceeds the {cap}-byte limit"); }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn validate_hindsight_input(label: &str, value: &str) -> Result<()> {
    if value.len() > HINDSIGHT_INPUT_MAX_BYTES { bail!("{label} exceeds the 1 MiB limit"); }
    Ok(())
}
fn validate_required_hindsight_input(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() { bail!("{label} is required"); }
    validate_hindsight_input(label, value)
}

const fn hindsight_budget_name(budget: crate::settings::HindsightBudget) -> &'static str {
    match budget { crate::settings::HindsightBudget::Low => "low", crate::settings::HindsightBudget::Mid => "mid", crate::settings::HindsightBudget::High => "high" }
}

fn format_hindsight_memories(results: &[HindsightRecallResult]) -> String {
    if results.is_empty() { return "No relevant memories found.".to_owned(); }
    results.iter().map(|memory| {
        let kind = memory.memory_type.as_deref().map(|value| format!(" [{value}]")).unwrap_or_default();
        let mentioned = memory.mentioned_at.as_deref().map(|value| format!(" ({value})")).unwrap_or_default();
        format!("- {}{kind}{mentioned}", redact_secrets(&memory.text))
    }).collect::<Vec<_>>().join("\n\n")
}

pub(crate) fn memory_tools_for(cwd: &str, session_env: Option<SessionEnvFn>, config: Option<MemoryConfig>) -> Vec<AgentTool> {
    let config = config.unwrap_or_default();
    match config.backend {
        crate::settings::MemoryBackend::Off => Vec::new(),
        crate::settings::MemoryBackend::Local => vec![memory_tool_with_session_env(cwd, session_env)],
        crate::settings::MemoryBackend::Hindsight => vec![recall_tool(cwd, config.clone()), retain_tool(cwd, config.clone()), reflect_tool(cwd, config)],
    }
}

pub(crate) fn memory_tools_for_persona(cwd: &str, persona_root: &Path, session_env: Option<SessionEnvFn>, config: Option<MemoryConfig>) -> Vec<AgentTool> {
    let config = config.unwrap_or_default();
    match config.backend {
        crate::settings::MemoryBackend::Off => Vec::new(),
        crate::settings::MemoryBackend::Local => vec![persona_memory_tool_with_session_env(persona_root, session_env)],
        crate::settings::MemoryBackend::Hindsight => vec![recall_tool(cwd, config.clone()), retain_tool(cwd, config.clone()), reflect_tool(cwd, config)],
    }
}

pub(crate) fn recall_tool(cwd: &str, config: MemoryConfig) -> AgentTool {
    let parameters = s_object(vec![("query", s_string("Natural-language search query"))], vec!["query"]);
    let cwd = cwd.to_owned();
    AgentTool::new("recall", "Search the configured Hindsight memory bank for relevant prior context.", parameters, move |ctx| {
        let client = HindsightClient::new(&config, Path::new(&cwd));
        async move { run_hindsight_op(client, "recall", ctx.arguments, ctx.abort).await }
    }).with_capability(ToolCapability::Read)
}

pub(crate) fn retain_tool(cwd: &str, config: MemoryConfig) -> AgentTool {
    let parameters = s_object(vec![("content", s_string("Information to remember")), ("context", s_string("Optional source context"))], vec!["content"]);
    let cwd = cwd.to_owned();
    AgentTool::new("retain", "Store important facts in the configured Hindsight memory bank.", parameters, move |ctx| {
        let client = HindsightClient::new(&config, Path::new(&cwd));
        async move { run_hindsight_op(client, "retain", ctx.arguments, ctx.abort).await }
    }).with_capability(ToolCapability::Write)
}

pub(crate) fn reflect_tool(cwd: &str, config: MemoryConfig) -> AgentTool {
    let parameters = s_object(vec![("query", s_string("Question to answer from long-term memory")), ("context", s_string("Optional additional context"))], vec!["query"]);
    let cwd = cwd.to_owned();
    AgentTool::new("reflect", "Synthesize an answer from the configured Hindsight memory bank.", parameters, move |ctx| {
        let client = HindsightClient::new(&config, Path::new(&cwd));
        async move { run_hindsight_op(client, "reflect", ctx.arguments, ctx.abort).await }
    }).with_capability(ToolCapability::Read)
}

async fn run_hindsight_op(client: Result<HindsightClient>, op: &str, args: Value, abort: AbortSignal) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    let client = client?;
    let rendered = match op {
        "recall" => { let query = arg_str(&args, "query"); if query.trim().is_empty() { return Err(anyhow!("query is required for recall")); } validate_hindsight_input("recall query", &query)?; client.recall(&query, &abort).await? }
        "retain" => { let content = arg_str(&args, "content"); if content.trim().is_empty() { return Err(anyhow!("content is required for retain")); } let context = arg_str(&args, "context"); client.retain(&content, (!context.trim().is_empty()).then_some(context.as_str()), &abort).await? }
        "reflect" => { let query = arg_str(&args, "query"); if query.trim().is_empty() { return Err(anyhow!("query is required for reflect")); } validate_hindsight_input("reflect query", &query)?; let context = arg_str(&args, "context"); client.reflect(&query, (!context.trim().is_empty()).then_some(context.as_str()), &abort).await? }
        other => return Err(anyhow!("Unknown hindsight op: {other}")),
    };
    check_aborted(&abort)?;
    Ok(text_result(truncate_head(&rendered, usize::MAX, OUTPUT_MAX_BYTES).content))
}

pub(crate) fn hindsight_injection_message(body: &str) -> pi_ai::Message {
    pi_ai::Message::Custom(pi_ai::CustomMessage {
        custom_type: "hindsight_memory".to_owned(),
        content: format!("Related memories from the Hindsight backend for the latest user request:\n{body}").into(),
        display: false,
        details: None,
        timestamp: pi_ai::now_millis(),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    use pi_agent::{AbortController, ToolCallContext};
    use serde_json::json;

    fn store_in(dir: &Path, namespace: &str) -> Arc<MemoryStore> {
        Arc::new(MemoryStore::new(dir, namespace).expect("store builds"))
    }

    fn noop_update() -> pi_agent::ToolUpdateFn {
        Arc::new(|_result: AgentToolResult| {})
    }

    fn ctx(args: Value) -> ToolCallContext {
        let (_ctrl, abort) = AbortController::new();
        std::mem::forget(_ctrl);
        ToolCallContext {
            tool_call_id: "test".to_string(),
            arguments: args,
            on_update: noop_update(),
            abort,
            model: None,
        }
    }

    fn text_of(result: &AgentToolResult) -> String {
        match result.content.first() {
            Some(pi_ai::ContentBlock::Text { text, .. }) => text.clone(),
            _ => String::new(),
        }
    }

    fn session_env_with(id: &str) -> SessionEnvFn {
        let id = id.to_owned();
        Arc::new(move || HashMap::from([("PI_SESSION_ID".to_owned(), id.clone())]))
    }

    #[test]
    fn learn_recall_round_trip() {
        let dir = tempfile::tempdir().expect("dir");
        let store = store_in(dir.path(), "ns");
        let (id, count) = store
            .learn("The release script lives in scripts/release.sh", vec!["release".into()], "s1")
            .expect("learn");
        assert_eq!(count, 1);
        let hits = store.recall("release script", 10).expect("recall");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, id);
        assert!(hits[0].content.contains("scripts/release.sh"));
        assert_eq!(hits[0].tags, vec!["release".to_string()]);
        assert_eq!(hits[0].session, "s1");
        // Unrelated query misses.
        assert!(store.recall("quantum", 10).expect("recall").is_empty());
    }

    #[test]
    fn recall_ranks_by_query_match_then_newest_first() {
        let dir = tempfile::tempdir().expect("dir");
        let store = store_in(dir.path(), "ns");
        // Old entry matches both terms; newest matches both plus the phrase;
        // middle matches one term.
        store
            .learn_with_ts("alpha beta is the old note", vec![], "s", 1_000)
            .expect("learn");
        store
            .learn_with_ts("beta only here", vec![], "s", 2_000)
            .expect("learn");
        store
            .learn_with_ts("alpha beta gamma is the newest", vec![], "s", 3_000)
            .expect("learn");
        let hits = store.recall("alpha beta", 10).expect("recall");
        let ids: Vec<&str> = hits.iter().map(|entry| entry.id.as_str()).collect();
        let newest = store.recall("gamma", 10).expect("recall");
        assert_eq!(newest.len(), 1);
        let by_content = |needle: &str| {
            hits.iter()
                .find(|entry| entry.content.contains(needle))
                .map(|entry| entry.id.as_str())
                .expect("hit present")
        };
        // Newest two-term entry first, then the older two-term entry, then the
        // one-term match — newest-first breaks the two-term tie.
        assert_eq!(ids[0], by_content("newest"));
        assert_eq!(ids[1], by_content("old note"));
        assert_eq!(ids[2], by_content("beta only"));
        // The full-phrase bonus beats same-term matches, and term matches
        // still rank below it, newest-first among ties.
        let phrase = store.recall("alpha beta gamma", 10).expect("recall");
        assert_eq!(phrase.len(), 3, "term matches still qualify for the phrase query");
        assert_eq!(phrase[0].id, by_content("newest"));
        assert_eq!(phrase[1].id, by_content("old note"));
        assert_eq!(phrase[2].id, by_content("beta only"));
    }

    #[test]
    fn forget_removes_an_entry() {
        let dir = tempfile::tempdir().expect("dir");
        let store = store_in(dir.path(), "ns");
        let (keep_id, _) = store.learn("keep me", vec![], "s").expect("learn");
        let (drop_id, _) = store.learn("drop me", vec![], "s").expect("learn");
        assert!(store.forget(&drop_id).expect("forget"));
        assert!(!store.forget(&drop_id).expect("second forget is a miss"));
        let hits = store.recall("drop", 10).expect("recall");
        assert!(hits.is_empty(), "dropped entry must not match");
        assert_eq!(store.recall("keep", 10).expect("recall")[0].id, keep_id);
    }

    #[test]
    fn list_filters_by_tag() {
        let dir = tempfile::tempdir().expect("dir");
        let store = store_in(dir.path(), "ns");
        store.learn("note one", vec!["alpha".into()], "s").expect("learn");
        store.learn("note two", vec!["beta".into(), "alpha".into()], "s").expect("learn");
        store.learn("note three", vec![], "s").expect("learn");
        assert_eq!(store.list(None).expect("list").len(), 3);
        let alpha = store.list(Some("ALPHA")).expect("list");
        assert_eq!(alpha.len(), 2, "case-insensitive tag filter");
        assert!(alpha.iter().all(|entry| entry.tags.iter().any(|tag| tag.eq_ignore_ascii_case("alpha"))));
        assert_eq!(store.list(Some("missing")).expect("list").len(), 0);
    }

    #[test]
    fn persists_across_store_reload() {
        let dir = tempfile::tempdir().expect("dir");
        let (id, count) = {
            let store = store_in(dir.path(), "ns");
            store.learn_with_ts("durable note", vec!["durable".into()], "session-one", 5_000)
                .expect("learn")
        };
        assert_eq!(count, 1);
        // A fresh store over the same root is a "new session": same namespace,
        // same agent dir, disk-backed entries must survive.
        let reloaded = store_in(dir.path(), "ns");
        let hits = reloaded.recall("durable", 10).expect("recall");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, id);
        assert_eq!(hits[0].session, "session-one");
        assert_eq!(hits[0].ts, 5_000);
        assert_eq!(reloaded.list(Some("durable")).expect("list").len(), 1);
    }

    #[tokio::test]
    async fn persona_memory_survives_tool_reconstruction_and_isolates_repo_memory() {
        let root = tempfile::tempdir().expect("root");
        let persona_root = root.path().join("personas").join("reviewer");
        let cwd = root.path().join("repo");
        std::fs::create_dir_all(&persona_root).expect("persona root");
        std::fs::create_dir_all(&cwd).expect("cwd");

        let first = persona_memory_tool_with_session_env(&persona_root, None);
        (first.execute)(ctx(json!({
            "op": "learn",
            "content": "persona-only durable note"
        })))
        .await
        .expect("persona learn");

        let second = persona_memory_tool_with_session_env(&persona_root, None);
        let recalled = text_of(&(second.execute)(ctx(json!({
            "op": "recall",
            "query": "persona-only"
        })))
        .await
        .expect("persona recall"));
        assert!(recalled.contains("persona-only durable note"));
        assert_eq!(
            MemoryStore::persona(&persona_root).entries_path,
            persona_root.join("memory").join("entries.jsonl")
        );

        let ordinary = MemoryStore::default_for(&cwd);
        assert!(ordinary.recall("persona-only", 10).expect("ordinary recall").is_empty());
        ordinary.learn("repo-only note", vec![], "repo").expect("repo learn");
        assert!(
            MemoryStore::persona(&persona_root)
                .recall("repo-only", 10)
                .expect("persona recall")
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn persona_memory_rejects_symlinked_directory_and_entries() {
        let root = tempfile::tempdir().expect("root");
        let persona_root = root.path().join("persona");
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&persona_root).expect("persona root");
        std::fs::create_dir_all(&outside).expect("outside");
        std::os::unix::fs::symlink(&outside, persona_root.join("memory"))
            .expect("memory symlink");
        let store = MemoryStore::persona(&persona_root);
        let error = store
            .learn("must not escape", vec![], "session")
            .expect_err("symlinked memory directory must fail");
        assert!(error.to_string().contains("non-symlink directory"), "{error:#}");
        assert!(!outside.join("entries.jsonl").exists());

        std::fs::remove_file(persona_root.join("memory")).expect("remove memory symlink");
        std::fs::create_dir(persona_root.join("memory")).expect("memory directory");
        let outside_entries = outside.join("entries.jsonl");
        std::fs::write(&outside_entries, "outside\n").expect("outside entries");
        std::os::unix::fs::symlink(
            &outside_entries,
            persona_root.join("memory").join("entries.jsonl"),
        )
        .expect("entries symlink");
        let error = store
            .list(None)
            .expect_err("symlinked entries file must fail");
        assert!(error.to_string().contains("regular non-symlink file"), "{error:#}");
        assert_eq!(std::fs::read_to_string(outside_entries).expect("outside intact"), "outside\n");
    }

    #[test]
    fn bounds_entry_size_and_count() {
        let dir = tempfile::tempdir().expect("dir");
        let store = store_in(dir.path(), "ns");
        let oversized = "x".repeat(MAX_ENTRY_BYTES + 1);
        assert!(store.learn(&oversized, vec![], "s").is_err(), "oversized entry rejected");
        // 105 learns → capped at 100, oldest five evicted. Each recall query is
        // a single token unique to one entry (a shared word like `entry` would
        // match every entry, and a bare digit would substring-match others).
        for i in 0..105 {
            store
                .learn_with_ts(&format!("entry {i:03} unique-token-{i:03}"), vec![], "s", i)
                .expect("learn");
        }
        assert_eq!(store.list(None).expect("list").len(), MAX_ENTRIES);
        for i in 0..5 {
            assert!(
                store.recall(&format!("unique-token-{i:03}"), 10).expect("recall").is_empty(),
                "oldest entry {i} must be evicted"
            );
        }
        for i in 5..105 {
            assert!(
                !store.recall(&format!("unique-token-{i:03}"), 10).expect("recall").is_empty(),
                "entry {i} must survive the cap"
            );
        }
    }

    #[test]
    fn namespace_is_digest_hex_and_path_safe() {
        let dir = tempfile::tempdir().expect("dir");
        let nested = dir.path().join("a").join("b");
        std::fs::create_dir_all(&nested).expect("nested");
        let namespace = repo_namespace(&nested);
        assert_eq!(namespace.len(), 32);
        assert!(namespace.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!namespace.contains('/'));
        // A `.git` ancestor anchors the namespace: nested and root agree.
        std::fs::create_dir_all(dir.path().join(".git")).expect("git");
        assert_eq!(repo_namespace(&nested), repo_namespace(dir.path()));
        // Distinct checkouts get distinct namespaces.
        let other = tempfile::tempdir().expect("other");
        assert_ne!(repo_namespace(dir.path()), repo_namespace(other.path()));
        // Hostile namespaces are rejected outright.
        for hostile in ["../../escape", "a/b", "a\\b", "a:b", "..", ".", "", "\u{0}"] {
            assert!(MemoryStore::new(dir.path(), hostile).is_err(), "{hostile:?} must be rejected");
        }
        // The store path for a valid digest namespace stays under the memory root.
        let store = store_in(dir.path(), &namespace);
        let entries = store.entries_path.clone();
        assert!(entries.starts_with(dir.path().join(MEMORY_DIR_NAME)));
        assert_eq!(entries.file_name().and_then(|n| n.to_str()), Some(ENTRIES_FILE_NAME));
        store.learn("safe", vec![], "s").expect("learn");
        assert!(entries.exists(), "entries written inside the memory root");
    }

    #[tokio::test]
    async fn redacts_obvious_secrets_and_leaks_no_paths() {
        let dir = tempfile::tempdir().expect("dir");
        let store = store_in(dir.path(), "ns");
        let token = ["sk", "-", "abcdefghijklmnop1234567890"].concat();
        let github = ["ghp", "_", "abcdefghijklmnopqrstuvwxyz123456"].concat();
        let aws = ["AK", "IA", "ABCDEFGHIJKLMNOP"].concat();
        let private_key = [
            "-----BEGIN RSA PRIVATE ",
            "KEY-----\nMIIEowIBAKCAQEA...\n-----END RSA PRIVATE ",
            "KEY-----",
        ]
        .concat();
        let secret_content = format!(
            "token {token} and key\n{private_key}\nand github {github} and {aws}"
        );
        store.learn(&secret_content, vec![], "s").expect("learn");
        let output = text_of(&(memory_tool_with_store(store.clone(), None).execute)(ctx(json!({
            "op": "recall",
            "query": "token"
        })))
        .await
        .expect("recall executes"));
        assert!(output.contains("[REDACTED]"), "redaction marker present");
        let leaked = [
            ["sk", "-", "abcdefghijklmnop"].concat(),
            github,
            aws,
            "MIIEowIBAKCAQEA".to_owned(),
            ["PRIVATE ", "KEY-----"].concat(),
        ];
        for leaked in &leaked {
            assert!(!output.contains(leaked), "secret fragment {leaked:?} must not leak");
        }
        // Output shape must never expose the store layout.
        let agent_dir = dir.path().to_string_lossy();
        assert!(!output.contains(agent_dir.as_ref()), "agent dir must not appear in output");
        assert!(!output.contains("entries.jsonl"));
        assert!(!output.contains("memory/"));
    }

    #[tokio::test]
    async fn tool_records_source_session_provenance() {
        let dir = tempfile::tempdir().expect("dir");
        let store = store_in(dir.path(), "ns");
        // Session-backed tool records PI_SESSION_ID.
        let tool = memory_tool_with_store(store.clone(), Some(session_env_with("sess-abc")));
        let result = text_of(&(tool.execute)(ctx(json!({
            "op": "learn",
            "content": "provenance note",
            "tags": ["p"],
        })))
        .await
        .expect("learn executes"));
        assert!(result.starts_with("Learned memory entry "));
        assert_eq!(store.list(None).expect("list")[0].session, "sess-abc");
        // Standalone tool records `standalone`.
        let standalone = memory_tool_with_store(store.clone(), None);
        let _ = (standalone.execute)(ctx(json!({ "op": "learn", "content": "standalone note" })))
            .await
            .expect("learn executes");
        let entries = store.list(None).expect("list");
        assert_eq!(entries[0].session, "standalone");
        assert_eq!(entries[1].session, "sess-abc");
    }

    #[tokio::test]
    async fn tool_actions_validate_and_render() {
        let dir = tempfile::tempdir().expect("dir");
        let store = store_in(dir.path(), "ns");
        let tool = memory_tool_with_store(store.clone(), None);
        // Unknown op rejected.
        let error = (tool.execute)(ctx(json!({ "op": "nope" })))
            .await
            .expect_err("unknown op errors");
        assert!(error.to_string().contains("Unknown memory op"));
        // learn without content rejected.
        let error = (tool.execute)(ctx(json!({ "op": "learn" })))
            .await
            .expect_err("missing content errors");
        assert!(error.to_string().contains("content is required"));
        // recall without query rejected.
        let error = (tool.execute)(ctx(json!({ "op": "recall" })))
            .await
            .expect_err("missing query errors");
        assert!(error.to_string().contains("query is required"));
        // Empty list renders a friendly message.
        let output = text_of(&(tool.execute)(ctx(json!({ "op": "list" })))
            .await
            .expect("list executes"));
        assert_eq!(output, "No memory entries.");
        // recall on an empty store renders a friendly message.
        let output = text_of(&(tool.execute)(ctx(json!({ "op": "recall", "query": "x" })))
            .await
            .expect("recall executes"));
        assert_eq!(output, "No memory entries match \"x\".");
        // forget with a bogus id renders a miss.
        let output = text_of(&(tool.execute)(ctx(json!({ "op": "forget", "id": "nope" })))
            .await
            .expect("forget executes"));
        assert_eq!(output, "No memory entry with id nope.");
        // limit clamps: 0 → 1, huge → 50.
        let (id, _) = store.learn("clamp me", vec![], "s").expect("learn");
        let output = text_of(&(tool.execute)(ctx(json!({ "op": "recall", "query": "clamp", "limit": 0 })))
            .await
            .expect("recall executes"));
        assert!(output.contains(&id), "limit 0 clamped to at least one result");
        let output = text_of(&(tool.execute)(ctx(json!({ "op": "forget", "id": &id })))
            .await
            .expect("forget executes"));
        assert_eq!(output, format!("Forgot memory entry {id}."));
    }

    // ------------------------------------------------------------------
    // External Hindsight backend
    // ------------------------------------------------------------------

    fn hindsight_config(base_url: String) -> MemoryConfig {
        MemoryConfig {
            backend: crate::settings::MemoryBackend::Hindsight,
            hindsight_api_url: Some(base_url),
            hindsight_allow_insecure: true,
            hindsight_bank_id: "testbank".to_owned(),
            hindsight_scoping: crate::settings::HindsightScoping::Global,
            ..Default::default()
        }
    }

    fn http_response(status: &str, body: &[u8]) -> Vec<u8> {
        let mut response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
        response.extend_from_slice(body);
        response
    }

    fn serve_responses(responses: Vec<Vec<u8>>) -> (String, Arc<std::sync::Mutex<Vec<(String, Value)>>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock Hindsight");
        let address = listener.local_addr().expect("mock address");
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = requests.clone();
        std::thread::spawn(move || {
            for response in responses {
                let (mut socket, _) = listener.accept().expect("accept mock Hindsight");
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                let header_end = loop {
                    if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n") { break position + 4; }
                    let read = socket.read(&mut buffer).expect("read request");
                    if read == 0 { break request.len(); }
                    request.extend_from_slice(&buffer[..read]);
                };
                let head = String::from_utf8_lossy(&request[..header_end]).into_owned();
                let length = head.lines().find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().expect("length"))
                }).unwrap_or(0);
                while request.len() < header_end + length {
                    let read = socket.read(&mut buffer).expect("read body");
                    if read == 0 { break; }
                    request.extend_from_slice(&buffer[..read]);
                }
                let path = head.split_whitespace().nth(1).unwrap_or("").to_owned();
                let body = serde_json::from_slice(&request[header_end..]).unwrap_or(Value::Null);
                captured.lock().expect("capture lock").push((path, body));
                socket.write_all(&response).expect("write response");
            }
        });
        (format!("http://{address}"), requests)
    }
    fn serve_redirect(location: String) -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind redirect source");
        let address = listener.local_addr().expect("redirect source address");
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept redirect source");
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let read = socket.read(&mut buffer).expect("read redirect request");
                if read == 0 { break; }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") { break; }
            }
            sent.send(String::from_utf8_lossy(&request).into_owned()).expect("capture redirect source");
            let response = format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            socket.write_all(response.as_bytes()).expect("write redirect");
        });
        (format!("http://{address}"), received)
    }

    #[test]
    fn hindsight_redirect_hop_policy_enforces_secure_and_insecure_schemes() {
        assert_eq!(
            hindsight_redirect_decision(false, "https", "http", 1),
            HindsightRedirectDecision::RejectPlaintext,
            "secure mode must reject HTTPS-to-HTTP downgrade"
        );
        assert_eq!(
            hindsight_redirect_decision(false, "https", "https", 1),
            HindsightRedirectDecision::Follow,
            "secure mode permits an HTTPS-to-HTTPS hop"
        );
        assert_eq!(
            hindsight_redirect_decision(false, "http", "https", 1),
            HindsightRedirectDecision::RejectPlaintext,
            "secure mode rejects a chain whose previous hop was plaintext"
        );
        assert_eq!(
            hindsight_redirect_decision(true, "http", "http", 1),
            HindsightRedirectDecision::Follow,
            "explicit insecure opt-in permits plaintext hops"
        );
        assert_eq!(
            hindsight_redirect_decision(false, "https", "https", 10),
            HindsightRedirectDecision::Follow,
            "secure redirect chains allow the same ten-hop maximum as reqwest"
        );
        assert_eq!(
            hindsight_redirect_decision(false, "https", "https", 11),
            HindsightRedirectDecision::RejectLimit,
            "secure redirect chains remain bounded"
        );
    }

    #[tokio::test]
    async fn hindsight_secure_redirect_hook_rejects_plaintext_before_target_request() {
        use std::io::Read;

        let target = std::net::TcpListener::bind("127.0.0.1:0").expect("bind plaintext target");
        target.set_nonblocking(true).expect("nonblocking plaintext target");
        let target_url = format!("http://{}", target.local_addr().expect("plaintext target address"));
        let (source, source_request) = serve_redirect(target_url);
        let authorization_material = ["redirect", "-", "marker"].concat();
        let request = hindsight_http_client_builder(false)
            .https_only(false)
            .build()
            .expect("redirect policy integration client")
            .get(source)
            .bearer_auth(&authorization_material)
            .send()
            .await
            .expect_err("plaintext redirect must fail");

        let source_request = source_request.recv_timeout(std::time::Duration::from_secs(1)).expect("source received request");
        assert!(source_request.contains(&authorization_material));
        assert!(request.to_string().contains("redirect"), "{request:#}");
        std::thread::sleep(std::time::Duration::from_millis(50));
        match target.accept() {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Ok((mut socket, _)) => {
                let mut leaked = String::new();
                socket.read_to_string(&mut leaked).expect("read unexpected plaintext request");
                panic!("secure redirect hook reached plaintext target: {leaked}");
            }
            Err(error) => panic!("checking plaintext target: {error}"),
        }
    }

    #[tokio::test]
    async fn hindsight_insecure_opt_in_follows_plaintext_redirect() {
        let (target, target_requests) = serve_responses(vec![http_response("200 OK", br#"{"results":[]}"#)]);
        let (source, source_request) = serve_redirect(format!("{target}/v1/default/banks/testbank/memories/recall"));
        let client = HindsightClient::new(&hindsight_config(source), Path::new(".")).expect("insecure client");
        let output = client.recall("redirect opt in", &AbortSignal::none()).await.expect("plaintext redirect follows");
        assert_eq!(output, "No relevant memories found.");
        source_request.recv_timeout(std::time::Duration::from_secs(1)).expect("redirect source reached");
        assert_eq!(target_requests.lock().expect("target requests").len(), 1);
    }

    #[test]
    fn memory_config_debug_redacts_hindsight_token() {
        let token = ["debug", "-", "sensitive-value"].concat();
        let rendered = format!("{:?}", MemoryConfig { hindsight_api_token: Some(token.clone()), ..Default::default() });
        assert!(rendered.contains("[REDACTED]"), "{rendered}");
        assert!(!rendered.contains(&token), "{rendered}");
    }

    #[tokio::test]
    async fn hindsight_client_methods_enforce_input_bounds_before_network() {
        let client = HindsightClient::new(&hindsight_config("http://127.0.0.1:9".to_owned()), Path::new(".")).expect("client");
        let oversized = "x".repeat(HINDSIGHT_INPUT_MAX_BYTES + 1);
        let abort = AbortSignal::none();
        assert!(client.recall("   ", &abort).await.expect_err("blank recall").to_string().contains("required"));
        assert!(client.recall(&oversized, &abort).await.expect_err("large recall").to_string().contains("1 MiB"));
        assert!(client.retain("", None, &abort).await.expect_err("blank retain").to_string().contains("required"));
        assert!(client.retain(&oversized, None, &abort).await.expect_err("large retain content").to_string().contains("1 MiB"));
        assert!(client.retain("ok", Some(&oversized), &abort).await.expect_err("large retain context").to_string().contains("1 MiB"));
        assert!(client.reflect(" ", None, &abort).await.expect_err("blank reflect").to_string().contains("required"));
        assert!(client.reflect(&oversized, None, &abort).await.expect_err("large reflect query").to_string().contains("1 MiB"));
        assert!(client.reflect("ok", Some(&oversized), &abort).await.expect_err("large reflect context").to_string().contains("1 MiB"));
    }

    #[tokio::test]
    async fn hindsight_bank_ensure_failures_stop_retain_and_reflect() {
        for (operation, status, code) in [
            ("retain", "401 Unauthorized", 401),
            ("retain", "503 Service Unavailable", 503),
            ("reflect", "401 Unauthorized", 401),
            ("reflect", "503 Service Unavailable", 503),
        ] {
            let (base, requests) = serve_responses(vec![http_response(status, br#"{"detail":"bank unavailable"}"#)]);
            let client = HindsightClient::new(&hindsight_config(base), Path::new(".")).expect("client");
            let error = if operation == "retain" {
                client.retain("bounded content", None, &AbortSignal::none()).await.expect_err("ensure failure")
            } else {
                client.reflect("bounded query", None, &AbortSignal::none()).await.expect_err("ensure failure")
            };
            assert!(error.to_string().contains(&format!("createBank failed (HTTP {code})")), "{error:#}");
            assert_eq!(requests.lock().expect("requests").len(), 1, "{operation} must not continue after ensure failure");
        }
    }

    #[tokio::test]
    async fn hindsight_recall_matches_http_wire_contract() {
        let body = br#"{"results":[{"text":"release script lives in scripts/release.sh","type":"world","mentioned_at":"2026-08-09"}]}"#;
        let (base, requests) = serve_responses(vec![http_response("200 OK", body)]);
        let client = HindsightClient::new(&hindsight_config(base), Path::new(".")).expect("client");
        let output = client.recall("release script", &AbortSignal::none()).await.expect("recall");
        assert!(output.contains("scripts/release.sh"), "{output}");
        let requests = requests.lock().expect("requests");
        assert_eq!(requests[0].0, "/v1/default/banks/testbank/memories/recall");
        assert_eq!(requests[0].1["query"], "release script");
        assert_eq!(requests[0].1["budget"], "mid");
        assert_eq!(requests[0].1["max_tokens"], 1024);
        assert_eq!(requests[0].1["types"], json!(["world", "experience"]));
    }

    #[tokio::test]
    async fn hindsight_retain_ensures_bank_and_sends_item() {
        let (base, requests) = serve_responses(vec![http_response("200 OK", br#"{}"#), http_response("200 OK", br#"{}"#)]);
        let client = HindsightClient::new(&hindsight_config(base), Path::new(".")).expect("client");
        let output = client.retain("cache lives in target/", Some("build"), &AbortSignal::none()).await.expect("retain");
        assert_eq!(output, "1 memory retained.");
        let requests = requests.lock().expect("requests");
        assert_eq!(requests[0].0, "/v1/default/banks/testbank");
        assert_eq!(requests[1].0, "/v1/default/banks/testbank/memories");
        assert_eq!(requests[1].1["items"][0]["content"], "cache lives in target/");
        assert_eq!(requests[1].1["items"][0]["context"], "build");
    }

    #[tokio::test]
    async fn hindsight_http_errors_are_redacted() {
        let token = ["sk", "-", "abcdefghijklmnop1234567890"].concat();
        let body = format!(r#"{{"detail":"boom {token}"}}"#);
        let (base, _) = serve_responses(vec![http_response("500 Internal Server Error", body.as_bytes())]);
        let client = HindsightClient::new(&hindsight_config(base), Path::new(".")).expect("client");
        let message = client.recall("x", &AbortSignal::none()).await.expect_err("HTTP error").to_string();
        assert!(message.contains("HTTP 500"), "{message}");
        assert!(message.contains("[REDACTED]"), "{message}");
        assert!(!message.contains(&["sk", "-", "abcdefghijklmnop"].concat()), "{message}");
    }

    #[tokio::test]
    async fn hindsight_large_response_is_rejected_without_hanging() {
        let oversized = vec![b'x'; HINDSIGHT_RESPONSE_MAX_BYTES + 1];
        let (base, _) = serve_responses(vec![http_response("200 OK", &oversized)]);
        let client = HindsightClient::new(&hindsight_config(base), Path::new(".")).expect("client");
        let error = client.recall("x", &AbortSignal::none()).await.expect_err("oversized response");
        assert!(error.to_string().contains("exceeds"), "{error:#}");
    }

    #[test]
    fn hindsight_rejects_unconfigured_and_plaintext_endpoints() {
        let error = HindsightClient::new(&MemoryConfig { backend: crate::settings::MemoryBackend::Hindsight, ..Default::default() }, Path::new("."))
            .err().expect("missing URL errors");
        assert!(error.to_string().contains("hindsightApiUrl"));
        let error = HindsightClient::new(&MemoryConfig {
            backend: crate::settings::MemoryBackend::Hindsight,
            hindsight_api_url: Some("http://127.0.0.1:8888".to_owned()),
            ..Default::default()
        }, Path::new(".")).err().expect("plaintext errors");
        assert!(error.to_string().contains("hindsightAllowInsecure"));
    }

    #[test]
    fn hindsight_project_scopes_share_repo_identity_digest() {
        use crate::settings::HindsightScoping;

        let config = |scoping: HindsightScoping| MemoryConfig {
            backend: crate::settings::MemoryBackend::Hindsight,
            hindsight_scoping: scoping,
            ..Default::default()
        };
        // Two repositories with the same basename under different parents.
        let root_a = tempfile::tempdir().expect("root a");
        let root_b = tempfile::tempdir().expect("root b");
        let repo_a = root_a.path().join("same-name");
        let repo_b = root_b.path().join("same-name");
        std::fs::create_dir_all(repo_a.join(".git")).expect("git a");
        std::fs::create_dir_all(repo_b.join(".git")).expect("git b");
        // A nested path inside repo a: must scope exactly like the repo root.
        let nested = repo_a.join("crates").join("pi-coding");
        std::fs::create_dir_all(&nested).expect("nested");

        for scoping in [HindsightScoping::PerProject, HindsightScoping::PerProjectTagged] {
            let scope_at = |path: &std::path::Path| hindsight_bank_scope(&config(scoping), path);
            let a = scope_at(&repo_a);
            let b = scope_at(&repo_b);
            assert_ne!(a, b, "{scoping:?}: same-basename repos under different parents must differ");
            let rendered_a = format!("{a:?}");
            let rendered_b = format!("{b:?}");
            assert!(rendered_a.contains("same-name") && rendered_b.contains("same-name"),
                "{scoping:?}: the human-readable basename must be retained: {rendered_a} / {rendered_b}");
            // The distinguishing component is the same identity digest local
            // memory digests (its prefix), not the basename.
            let namespace_a = repo_namespace(&repo_a);
            let namespace_b = repo_namespace(&repo_b);
            assert_ne!(namespace_a, namespace_b, "sanity: digests differ");
            let prefix_a = &namespace_a[..HINDSIGHT_SCOPE_DIGEST_PREFIX];
            let prefix_b = &namespace_b[..HINDSIGHT_SCOPE_DIGEST_PREFIX];
            assert_ne!(prefix_a, prefix_b, "sanity: digest prefixes differ");
            assert!(rendered_a.contains(prefix_a), "{scoping:?}: scope must embed the local-memory digest prefix {prefix_a}: {rendered_a}");
            assert!(rendered_b.contains(prefix_b), "{scoping:?}: scope must embed the local-memory digest prefix {prefix_b}: {rendered_b}");
            // Nested paths inside one checkout share the repo-root scope.
            assert_eq!(scope_at(&nested), a, "{scoping:?}: nested paths must resolve identically to the repo root");
        }

        // Per-project keys the bank id by the label; per-project-tagged keeps
        // the shared bank id and tags each item by the label.
        let per_project = hindsight_bank_scope(&config(HindsightScoping::PerProject), &repo_a);
        let label = format!("same-name-{}", &repo_namespace(&repo_a)[..HINDSIGHT_SCOPE_DIGEST_PREFIX]);
        assert_eq!(per_project.bank_id, format!("{DEFAULT_HINDSIGHT_BANK_ID}-{label}"));
        assert!(per_project.retain_tags.is_empty() && per_project.recall_tags.is_empty());
        assert_eq!(per_project.recall_tags_match, None);
        let tagged = hindsight_bank_scope(&config(HindsightScoping::PerProjectTagged), &repo_a);
        assert_eq!(tagged.bank_id, DEFAULT_HINDSIGHT_BANK_ID);
        assert_eq!(tagged.retain_tags, vec![format!("project:{label}")]);
        assert_eq!(tagged.recall_tags, tagged.retain_tags);
        assert_eq!(tagged.recall_tags_match, Some("any"));

        // Global scoping is untouched: plain bank id, no tags, no cwd variance.
        let global_a = hindsight_bank_scope(&config(HindsightScoping::Global), &repo_a);
        let global_b = hindsight_bank_scope(&config(HindsightScoping::Global), &repo_b);
        assert_eq!(global_a.bank_id, DEFAULT_HINDSIGHT_BANK_ID);
        assert_eq!(global_a, global_b, "Global must not vary by repository");
        assert!(global_a.retain_tags.is_empty() && global_a.recall_tags.is_empty());
        assert_eq!(global_a.recall_tags_match, None);
    }

    #[test]
    fn hindsight_project_scope_label_is_sanitized() {
        use crate::settings::HindsightScoping;

        let scoped_config = || MemoryConfig {
            backend: crate::settings::MemoryBackend::Hindsight,
            hindsight_scoping: HindsightScoping::PerProject,
            ..Default::default()
        };
        // A hostile basename must not leak separators or control characters
        // into the bank id.
        let dir = tempfile::tempdir().expect("dir");
        let hostile = dir.path().join("weird name!@#\u{7f}");
        std::fs::create_dir_all(hostile.join(".git")).expect("git");
        let scope = hindsight_bank_scope(&scoped_config(), &hostile);
        assert!(scope.bank_id.starts_with(&format!("{DEFAULT_HINDSIGHT_BANK_ID}-weird-name-")), "{}", scope.bank_id);
        assert!(scope.bank_id.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')),
            "bank id must stay token-safe: {}", scope.bank_id);
        // A basename with no readable characters falls back to `unknown` but
        // stays unique via the digest prefix.
        let bare = dir.path().join("!!!");
        std::fs::create_dir_all(bare.join(".git")).expect("git");
        let bare_scope = hindsight_bank_scope(&scoped_config(), &bare);
        assert!(bare_scope.bank_id.starts_with(&format!("{DEFAULT_HINDSIGHT_BANK_ID}-unknown-")), "{}", bare_scope.bank_id);
        assert_ne!(bare_scope.bank_id, scope.bank_id);
    }

    #[test]
    fn memory_tools_for_selects_backend_tool_set() {
        let dir = tempfile::tempdir().expect("dir");
        let cwd = dir.path().to_string_lossy().into_owned();
        let names = |config: Option<MemoryConfig>| {
            memory_tools_for(&cwd, None, config)
                .into_iter()
                .map(|tool| tool.name)
                .collect::<Vec<_>>()
        };
        // off hides every memory tool.
        assert!(names(Some(MemoryConfig {
            backend: crate::settings::MemoryBackend::Off,
            ..Default::default()
        }))
        .is_empty());
        // local keeps the built-in memory tool.
        assert_eq!(
            names(Some(MemoryConfig {
                backend: crate::settings::MemoryBackend::Local,
                ..Default::default()
            })),
            vec!["memory".to_owned()]
        );
        // hindsight swaps to the recall/retain/reflect trio.
        assert_eq!(
            names(Some(MemoryConfig {
                backend: crate::settings::MemoryBackend::Hindsight,
                ..Default::default()
            })),
            vec!["recall".to_owned(), "retain".to_owned(), "reflect".to_owned()]
        );
        // No config → default local backend (existing behavior).
        assert_eq!(names(None), vec!["memory".to_owned()]);
    }

    #[tokio::test]
    async fn hindsight_tools_validate_arguments() {
        let dir = tempfile::tempdir().expect("dir");
        let config = hindsight_config("http://127.0.0.1:9".to_owned());
        let recall = recall_tool(dir.path().to_string_lossy().as_ref(), config.clone());
        let error = (recall.execute)(ctx(json!({})))
            .await
            .expect_err("missing query errors");
        assert!(error.to_string().contains("query is required"));
        let retain = retain_tool(dir.path().to_string_lossy().as_ref(), config.clone());
        let error = (retain.execute)(ctx(json!({})))
            .await
            .expect_err("missing content errors");
        assert!(error.to_string().contains("content is required"));
        let reflect = reflect_tool(dir.path().to_string_lossy().as_ref(), config);
        let error = (reflect.execute)(ctx(json!({})))
            .await
            .expect_err("missing query errors");
        assert!(error.to_string().contains("query is required"));
    }

    #[test]
    fn hindsight_injection_message_is_hidden_context() {
        let message = hindsight_injection_message("Memory: the build cache lives in target/");
        let pi_ai::Message::Custom(custom) = message else {
            panic!("injection must be a custom message");
        };
        assert_eq!(custom.custom_type, "hindsight_memory");
        assert!(!custom.display, "injection must never auto-submit to the UI");
        let text = match &custom.content {
            pi_ai::CustomMessageContent::Text(text) => text.clone(),
            pi_ai::CustomMessageContent::Blocks(blocks) => {
                text_of_blocks(blocks)
            }
        };
        assert!(text.contains("target/"), "{text}");
    }

    fn text_of_blocks(blocks: &[pi_ai::ContentBlock]) -> String {
        let mut out = String::new();
        for block in blocks {
            if let pi_ai::ContentBlock::Text { text, .. } = block {
                out.push_str(text);
            }
        }
        out
    }
}
