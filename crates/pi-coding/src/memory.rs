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
//! hostile cwd can never escape the memory root. `RPI_HOME` (or `~/.rpi`)
//! selects the agent directory, the same knob the rest of the
//! harness uses; no extra settings surface is introduced.
//!
//! When a repository moves on disk, `migrate_repo_namespace` copies the old
//! namespace's entries into the moved checkout's namespace additively and
//! idempotently; the old archive stays behind as recovery metadata until the
//! session-move core finalizes via `commit_repo_namespace_migration`.
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
use std::sync::{Arc, LazyLock};

use anyhow::{Context as _, Result, anyhow, bail};
use pi_agent::{AbortSignal, AgentTool, AgentToolResult, ToolCallContext, ToolCapability};
use pi_sandbox::AbsolutePath;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::fs::{
    CapabilityAccess, CapabilityFs, CapabilityRootLease, CapabilityRootPurpose, FileMode, FsError,
    Replacement, RootCreation,
};
use crate::redact::redact_secrets;
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
const MAX_MEMORY_STORE_BYTES: u64 = 128 * 1024 * 1024;
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
    lease: CapabilityRootLease,
}

impl MemoryStore {
    /// Builds the store for the default agent directory and the repo digest of
    /// `cwd`. The digest namespace is always valid, so this never fails.
    pub(crate) fn default_for(cwd: &Path) -> Arc<MemoryStore> {
        Self::default_for_with_capabilities(&PROCESS_CAPABILITIES, cwd)
    }

    pub(crate) fn default_for_with_capabilities(
        capabilities: &Arc<CapabilityFs>,
        cwd: &Path,
    ) -> Arc<MemoryStore> {
        Self::try_default_for_with_capabilities(capabilities, cwd)
            .expect("digest namespace is always path-safe")
    }

    fn try_default_for_with_capabilities(
        capabilities: &Arc<CapabilityFs>,
        cwd: &Path,
    ) -> Result<Arc<MemoryStore>> {
        let namespace = repo_namespace(cwd);
        Ok(Arc::new(MemoryStore::new_with_capabilities(
            capabilities,
            &crate::resources::agent_dir_path(),
            &namespace,
        )?))
    }

    /// Builds a store rooted at `agent_dir` under the given namespace.
    /// Rejects namespaces that could traverse out of the memory root.
    pub(crate) fn new(agent_dir: &Path, namespace: &str) -> Result<Self> {
        Self::new_with_capabilities(&PROCESS_CAPABILITIES, agent_dir, namespace)
    }

    pub(crate) fn new_with_capabilities(
        capabilities: &Arc<CapabilityFs>,
        agent_dir: &Path,
        namespace: &str,
    ) -> Result<Self> {
        Self::open_namespace_store(
            capabilities,
            agent_dir,
            namespace,
            RootCreation::CreateDirectory,
        )
    }

    /// Builds a namespace store under `agent_dir`, creating the namespace
    /// directory only when `creation` asks for it. Namespace-migration probes
    /// pass [`RootCreation::Existing`] so a missing old archive surfaces as
    /// an error instead of being silently created.
    fn open_namespace_store(
        capabilities: &Arc<CapabilityFs>,
        agent_dir: &Path,
        namespace: &str,
        creation: RootCreation,
    ) -> Result<Self> {
        validate_namespace(namespace)?;
        let agent_root = capabilities.open_root(
            CapabilityRootPurpose::AgentStore,
            absolute_capability_path(agent_dir)?,
            CapabilityAccess::PrivateReadWrite,
            RootCreation::CreateDirectory,
        )?;
        let lease = agent_root.lease_directory(
            &Path::new(MEMORY_DIR_NAME).join(namespace),
            CapabilityAccess::PrivateReadWrite,
            creation,
        )?;
        Ok(Self {
            entries_path: lease.display_root().join(ENTRIES_FILE_NAME),
            lease,
        })
    }

    /// Builds a persona-local store at the durable persona layout's exact
    /// `<persona-root>/memory/entries.jsonl` path.
    pub(crate) fn persona(persona_root: &Path) -> Arc<MemoryStore> {
        Arc::new(
            Self::persona_with_capabilities(&PROCESS_CAPABILITIES, persona_root)
                .expect("persona memory root must be capability-accessible"),
        )
    }
    pub(crate) fn persona_with_capabilities(
        capabilities: &Arc<CapabilityFs>,
        persona_root: &Path,
    ) -> Result<Self> {
        let persona = capabilities.open_root(
            CapabilityRootPurpose::PersonaStore,
            absolute_capability_path(persona_root)?,
            CapabilityAccess::PrivateReadWrite,
            RootCreation::Existing,
        )?;
        let lease = persona.lease_directory(
            Path::new(MEMORY_DIR_NAME),
            CapabilityAccess::PrivateReadWrite,
            RootCreation::CreateDirectory,
        )?;
        Ok(Self {
            entries_path: lease.display_root().join(ENTRIES_FILE_NAME),
            lease,
        })
    }

    pub(crate) fn persona_from_lease(persona: &CapabilityRootLease) -> Result<Self> {
        if persona.purpose() != CapabilityRootPurpose::PersonaStore
            || persona.access() != CapabilityAccess::PrivateReadWrite
        {
            bail!("persona memory requires a private persona-store lease");
        }
        let lease = persona.lease_directory(
            Path::new(MEMORY_DIR_NAME),
            CapabilityAccess::PrivateReadWrite,
            RootCreation::CreateDirectory,
        )?;
        Ok(Self {
            entries_path: lease.display_root().join(ENTRIES_FILE_NAME),
            lease,
        })
    }

    pub(crate) fn learn(
        &self,
        content: &str,
        tags: Vec<String>,
        session: &str,
    ) -> Result<(String, usize)> {
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
            tags: tags
                .into_iter()
                .filter_map(|tag| sanitize_tag(&tag))
                .collect(),
            ts,
            session: sanitize_session(session),
        };
        let _guard = MEMORY_STORE_LOCK.lock().expect("memory lock poisoned");
        let (mut entries, expected) = self.read_entries_with_identity()?;
        entries.push(entry);
        entries.sort_by(entry_order);
        entries.truncate(MAX_ENTRIES);
        let count = entries.len();
        self.write_entries(&entries, expected)?;
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
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| entry_order(&a.1, &b.1)));
        Ok(scored
            .into_iter()
            .take(limit)
            .map(|(_, entry)| entry)
            .collect())
    }

    /// Newest-first listing, optionally filtered to entries carrying `tag`
    /// (case-insensitive exact match).
    pub(crate) fn list(&self, tag: Option<&str>) -> Result<Vec<MemoryEntry>> {
        let _guard = MEMORY_STORE_LOCK.lock().expect("memory lock poisoned");
        let mut entries = self.read_entries()?;
        if let Some(tag) = tag {
            let tag = tag.trim().to_lowercase();
            if !tag.is_empty() {
                entries.retain(|entry| entry.tags.iter().any(|value| value.to_lowercase() == tag));
            }
        }
        Ok(entries)
    }

    pub(crate) fn forget(&self, id: &str) -> Result<bool> {
        let _guard = MEMORY_STORE_LOCK.lock().expect("memory lock poisoned");
        let (mut entries, expected) = self.read_entries_with_identity()?;
        let before = entries.len();
        entries.retain(|entry| entry.id != id);
        if entries.len() == before {
            return Ok(false);
        }
        self.write_entries(&entries, expected)?;
        Ok(true)
    }

    /// Loads all entries. A missing file is an empty store. Malformed lines
    /// are skipped: the file is only ever written atomically by this module,
    /// so a bad line implies external tampering, and failing the whole read
    fn read_entries(&self) -> Result<Vec<MemoryEntry>> {
        self.read_entries_with_identity()
            .map(|(entries, _)| entries)
    }

    fn read_entries_with_identity(
        &self,
    ) -> Result<(Vec<MemoryEntry>, Option<crate::fs::FileIdentity>)> {
        let (raw, identity) = match self.lease.open_file(Path::new(ENTRIES_FILE_NAME)) {
            Ok(file) => (
                file.read_bounded(MAX_MEMORY_STORE_BYTES)
                    .context("reading memory entries")?,
                Some(file.identity()),
            ),
            Err(error) if fs_error_is_missing(&error) => (Vec::new(), None),
            Err(error) => return Err(error).context("opening memory entries"),
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
        Ok((entries, identity))
    }

    fn write_entries(
        &self,
        entries: &[MemoryEntry],
        expected: Option<crate::fs::FileIdentity>,
    ) -> Result<()> {
        let mut bytes = Vec::with_capacity(entries.len() * 160);
        for entry in entries {
            serde_json::to_writer(&mut bytes, entry).context("serializing memory entry")?;
            bytes.push(b'\n');
        }
        if let Some(expected) = expected {
            self.lease
                .replace_file(Replacement {
                    relative: PathBuf::from(ENTRIES_FILE_NAME),
                    expected: Some(expected),
                    bytes,
                    mode: FileMode::Private,
                })
                .map(|_| ())
                .context("persisting memory entries")
        } else {
            let mut file = self
                .lease
                .create_new(Path::new(ENTRIES_FILE_NAME), FileMode::Private)
                .context("creating memory entries")?;
            file.write_all(&bytes).context("writing memory entries")?;
            file.finish().map(|_| ()).context("syncing memory entries")
        }
    }
}

static PROCESS_CAPABILITIES: LazyLock<Arc<CapabilityFs>> =
    LazyLock::new(|| CapabilityFs::new().expect("capability filesystem initialization"));

fn absolute_capability_path(path: &Path) -> Result<AbsolutePath> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("resolving memory store root")?
            .join(path)
    };
    AbsolutePath::parse(path).map_err(|_| anyhow!("invalid memory store root"))
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

/// Filesystem-safe repo namespace anchored at the nearest retained `.git`
/// entry. Always 32 hex chars with no separators or traversal.
pub(crate) fn repo_namespace(cwd: &Path) -> String {
    repo_digest_hex(&repo_anchor(cwd))
}

fn repo_anchor(cwd: &Path) -> PathBuf {
    let absolute = absolutized(cwd);
    git_anchor(&absolute).unwrap_or(absolute)
}

fn absolutized(cwd: &Path) -> PathBuf {
    if cwd.is_absolute() {
        cwd.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| cwd.to_path_buf(), |base| base.join(cwd))
    }
}

/// Old/new repository namespace digests for a migration. The old namespace
/// cannot be recomputed from `old_cwd` alone: by the time the session move
/// commits, the old checkout and its `.git` ancestor may already be gone,
/// and walking up from the stale path would silently hash a nested directory
/// instead of the repository root. Instead the old anchor is inferred: the
/// new cwd's repository-relative suffix is stripped from `old_cwd`,
/// mirroring a move that preserved the working directory's position inside
/// the repository. A cwd rooted at its repository (empty suffix) maps the
/// old anchor to `old_cwd` itself; a suffix that does not align lexically
/// falls back to hashing `old_cwd` directly, which the missing-store probe
/// then turns into a no-op.
fn moved_repo_namespaces(old_cwd: &Path, new_cwd: &Path) -> (String, String) {
    let new_absolute = absolutized(new_cwd);
    let new_anchor = git_anchor(&new_absolute).unwrap_or_else(|| new_absolute.clone());
    let new_namespace = repo_digest_hex(&new_anchor);
    let old_absolute = absolutized(old_cwd);
    let suffix = new_absolute
        .strip_prefix(&new_anchor)
        .unwrap_or(new_absolute.as_path());
    let mut old_anchor = old_absolute.clone();
    for _ in suffix.components() {
        if !old_anchor.pop() {
            old_anchor = old_absolute;
            break;
        }
    }
    (repo_digest_hex(&old_anchor), new_namespace)
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
    let capabilities = CapabilityFs::new().ok()?;
    let mut dir = Some(start);
    while let Some(candidate) = dir {
        let absolute = pi_sandbox::AbsolutePath::parse(candidate.to_path_buf()).ok()?;
        if let Ok(lease) = capabilities.open_root(
            CapabilityRootPurpose::ProjectResources,
            absolute,
            CapabilityAccess::ReadOnly,
            RootCreation::Existing,
        ) && lease.entry_exists(Path::new(".git")).ok()?
        {
            return Some(candidate.to_path_buf());
        }
        dir = candidate.parent();
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

/// Whether an anyhow-wrapped store open failed because the namespace does
/// not exist (the migration no-op probe path).
fn namespace_store_missing(error: &anyhow::Error) -> bool {
    error.downcast_ref::<FsError>().is_some_and(fs_error_is_missing)
}

/// Copies the memory namespace of the repository that used to live at
/// `old_cwd` into the namespace of `new_cwd` after the repository moved on
/// disk. The old namespace is inferred from the two paths (see
/// [`moved_repo_namespaces`]) because the old checkout may already be gone.
/// Additive and idempotent: entries merge into the target keyed by id (a
/// colliding target entry wins, so the live namespace after rebind is never
/// clobbered), the old archive stays on disk untouched as recovery metadata,
/// and re-running the migration copies nothing once every id is present.
/// Copied entries pass through the same write boundary as [`learn`]
/// (redaction, tag/session sanitization, per-entry size bound; oversize or
/// invalid entries are skipped), and the target is authoritative under the
/// per-namespace cap: all of its entries are retained, copied entries fill
/// the remaining slots newest-first. Reports `Ok(false)` when both paths
/// resolve to the same namespace, the old store is missing or empty, or
/// nothing new was copied; `Ok(true)` when the target namespace gained
/// entries. The session-move core drops the old archive afterwards via
/// [`commit_repo_namespace_migration`].
pub(crate) fn migrate_repo_namespace(old_cwd: &Path, new_cwd: &Path) -> Result<bool> {
    migrate_repo_namespace_with_capabilities(
        &PROCESS_CAPABILITIES,
        &crate::resources::agent_dir_path(),
        old_cwd,
        new_cwd,
    )
}

/// [`migrate_repo_namespace`] over an explicit agent store (hermetic tests).
pub(crate) fn migrate_repo_namespace_with_capabilities(
    capabilities: &Arc<CapabilityFs>,
    agent_dir: &Path,
    old_cwd: &Path,
    new_cwd: &Path,
) -> Result<bool> {
    let (old_namespace, new_namespace) = moved_repo_namespaces(old_cwd, new_cwd);
    if old_namespace == new_namespace {
        return Ok(false);
    }
    let _guard = MEMORY_STORE_LOCK.lock().expect("memory lock poisoned");
    let old_store = match MemoryStore::open_namespace_store(
        capabilities,
        agent_dir,
        &old_namespace,
        RootCreation::Existing,
    ) {
        Ok(store) => store,
        Err(error) if namespace_store_missing(&error) => return Ok(false),
        Err(error) => return Err(error).context("opening old memory namespace"),
    };
    let (old_entries, _) = old_store
        .read_entries_with_identity()
        .context("reading old memory namespace")?;
    if old_entries.is_empty() {
        return Ok(false);
    }
    let new_store = MemoryStore::open_namespace_store(
        capabilities,
        agent_dir,
        &new_namespace,
        RootCreation::CreateDirectory,
    )
    .context("opening new memory namespace")?;
    let (mut entries, expected) = new_store.read_entries_with_identity()?;
    let fresh: Vec<MemoryEntry> = {
        let known: BTreeSet<&str> = entries.iter().map(|entry| entry.id.as_str()).collect();
        let mut fresh: Vec<MemoryEntry> = old_entries
            .iter()
            .filter(|entry| !known.contains(entry.id.as_str()))
            .filter_map(entry_for_target)
            .collect();
        // Retention under the per-namespace cap: the live target namespace
        // is authoritative and always retained; copied entries fill the
        // remaining slots, newest first.
        fresh.sort_by(entry_order);
        let budget = MAX_ENTRIES.saturating_sub(entries.len());
        fresh.into_iter().take(budget).collect()
    };
    if fresh.is_empty() {
        return Ok(false);
    }
    entries.extend(fresh);
    entries.sort_by(entry_order);
    new_store.write_entries(&entries, expected)?;
    Ok(true)
}

/// Re-normalizes a copied entry through the same write-boundary rules as
/// [`MemoryStore::learn`]: redaction, tag/session sanitization, and the
/// per-entry size bound. Oversize or id-less entries are skipped, so a
/// hostile or corrupt old archive can never push the target past its read
/// bound. The id and timestamp are preserved so migration stays idempotent
/// and order-stable.
fn entry_for_target(entry: &MemoryEntry) -> Option<MemoryEntry> {
    if entry.content.len() > MAX_ENTRY_BYTES || entry.id.is_empty() {
        return None;
    }
    Some(MemoryEntry {
        id: entry.id.clone(),
        content: redact_secrets(&entry.content),
        tags: entry
            .tags
            .iter()
            .filter_map(|tag| sanitize_tag(tag))
            .collect(),
        ts: entry.ts,
        session: sanitize_session(&entry.session),
    })
}

/// Removes the old repository namespace archive once the session move has
/// committed. Refuses with an error while the new namespace does not yet
/// contain every entry from the old store, so recovery data is never dropped
/// before (or instead of) the copy. Idempotent: a missing archive (or an
/// identical repository anchor) reports `Ok(false)`; a removed archive
/// reports `Ok(true)`.
pub(crate) fn commit_repo_namespace_migration(old_cwd: &Path, new_cwd: &Path) -> Result<bool> {
    commit_repo_namespace_migration_with_capabilities(
        &PROCESS_CAPABILITIES,
        &crate::resources::agent_dir_path(),
        old_cwd,
        new_cwd,
    )
}

/// [`commit_repo_namespace_migration`] over an explicit agent store.
pub(crate) fn commit_repo_namespace_migration_with_capabilities(
    capabilities: &Arc<CapabilityFs>,
    agent_dir: &Path,
    old_cwd: &Path,
    new_cwd: &Path,
) -> Result<bool> {
    let (old_namespace, new_namespace) = moved_repo_namespaces(old_cwd, new_cwd);
    if old_namespace == new_namespace {
        return Ok(false);
    }
    let _guard = MEMORY_STORE_LOCK.lock().expect("memory lock poisoned");
    let old_store = match MemoryStore::open_namespace_store(
        capabilities,
        agent_dir,
        &old_namespace,
        RootCreation::Existing,
    ) {
        Ok(store) => store,
        Err(error) if namespace_store_missing(&error) => return Ok(false),
        Err(error) => return Err(error).context("opening old memory namespace"),
    };
    let (old_entries, _) = old_store
        .read_entries_with_identity()
        .context("reading old memory namespace")?;
    if !old_entries.is_empty() {
        let new_store = match MemoryStore::open_namespace_store(
            capabilities,
            agent_dir,
            &new_namespace,
            RootCreation::Existing,
        ) {
            Ok(store) => store,
            Err(error) if namespace_store_missing(&error) => {
                bail!("new memory namespace is missing; refusing to remove the old archive")
            }
            Err(error) => return Err(error).context("opening new memory namespace"),
        };
        let new_entries = new_store.read_entries()?;
        let known: BTreeSet<&str> = new_entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect();
        let unaccounted = old_entries
            .iter()
            .filter(|entry| !known.contains(entry.id.as_str()))
            .count();
        if unaccounted > 0 {
            bail!("{unaccounted} old memory entries are absent from the new namespace; refusing to remove the old archive");
        }
    }
    drop(old_store);
    remove_namespace_archive(capabilities, agent_dir, &old_namespace)?;
    Ok(true)
}

/// Removes `<agent_dir>/memory/<namespace>` through an identity-checked
/// subtree removal, then syncs the memory root so the deletion is durable.
fn remove_namespace_archive(
    capabilities: &Arc<CapabilityFs>,
    agent_dir: &Path,
    namespace: &str,
) -> Result<()> {
    let agent_root = capabilities.open_root(
        CapabilityRootPurpose::AgentStore,
        absolute_capability_path(agent_dir)?,
        CapabilityAccess::PrivateReadWrite,
        RootCreation::Existing,
    )?;
    let memory_root = agent_root.lease_directory(
        Path::new(MEMORY_DIR_NAME),
        CapabilityAccess::PrivateReadWrite,
        RootCreation::Existing,
    )?;
    let relative = Path::new(namespace);
    let expected = memory_root.metadata(relative)?.identity;
    memory_root.remove_tree_expected(relative, expected)?;
    memory_root
        .sync_directory(Path::new(""))
        .context("syncing memory root after archive removal")
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

pub(crate) fn memory_tool_with_capabilities(
    capabilities: &Arc<CapabilityFs>,
    cwd: &str,
    session_env: Option<SessionEnvFn>,
) -> AgentTool {
    memory_tool_with_store(
        MemoryStore::default_for_with_capabilities(capabilities, Path::new(cwd)),
        session_env,
    )
}

fn memory_tool_with_generation(
    cwd: &str,
    session_env: Option<SessionEnvFn>,
    capture: crate::tools::GenerationCaptureFn,
) -> AgentTool {
    let cwd = PathBuf::from(cwd);
    memory_tool_with_store_factory(
        Arc::new(move || {
            let generation = capture()?;
            MemoryStore::try_default_for_with_capabilities(
                generation.sandbox().capabilities(),
                &cwd,
            )
        }),
        session_env,
    )
}
/// [`memory_tool_with_session_env`] rooted at a durable persona's exact
/// `<persona-root>/memory/entries.jsonl` store.
pub(crate) fn persona_memory_tool_with_session_env(
    persona_root: &Path,
    session_env: Option<SessionEnvFn>,
) -> AgentTool {
    memory_tool_with_store(MemoryStore::persona(persona_root), session_env)
}

pub(crate) fn persona_memory_tool_with_capabilities(
    capabilities: &Arc<CapabilityFs>,
    persona_root: &Path,
    session_env: Option<SessionEnvFn>,
) -> Result<AgentTool> {
    Ok(memory_tool_with_store(
        Arc::new(MemoryStore::persona_with_capabilities(
            capabilities,
            persona_root,
        )?),
        session_env,
    ))
}

/// [`memory_tool`] over an explicitly rooted store (hermetic tests).
fn memory_tool_with_store(store: Arc<MemoryStore>, session_env: Option<SessionEnvFn>) -> AgentTool {
    memory_tool_with_store_factory(Arc::new(move || Ok(store.clone())), session_env)
}

type MemoryStoreFactory = Arc<dyn Fn() -> Result<Arc<MemoryStore>> + Send + Sync>;

fn memory_tool_with_store_factory(
    store_factory: MemoryStoreFactory,
    session_env: Option<SessionEnvFn>,
) -> AgentTool {
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
            (
                "tags",
                s_array(s_string("tag"), "Optional tags (learn, at most 20)"),
            ),
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
        let store_factory = store_factory.clone();
        let session_env = session_env.clone();
        async move {
            let store = store_factory()?;
            run_memory(store, session_env, ctx.arguments, ctx.abort).await
        }
    })
    .with_capability(ToolCapability::Write)
}

/// String schema with an enum constraint (mirrors the github tool).
fn schema_with_enum(description: &str, values: [&str; 4]) -> Schema {
    Schema {
        schema_type: Some(Value::String("string".into())),
        description: Some(description.to_string()),
        enum_values: values
            .iter()
            .map(|value| Value::String((*value).to_owned()))
            .collect(),
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
            let (id, count) = store.learn(
                &content,
                parse_tags(&args),
                &source_session(session_env.as_ref()),
            )?;
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
            .field(
                "hindsight_recall_max_tokens",
                &self.hindsight_recall_max_tokens,
            )
            .field("hindsight_recall_types", &self.hindsight_recall_types)
            .field(
                "hindsight_request_timeout_ms",
                &self.hindsight_request_timeout_ms,
            )
            .field(
                "hindsight_recall_timeout_ms",
                &self.hindsight_recall_timeout_ms,
            )
            .field(
                "hindsight_retain_timeout_ms",
                &self.hindsight_retain_timeout_ms,
            )
            .field(
                "hindsight_reflect_timeout_ms",
                &self.hindsight_reflect_timeout_ms,
            )
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
            hindsight_recall_types: DEFAULT_HINDSIGHT_RECALL_TYPES
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
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
    let base = config
        .hindsight_bank_id_prefix
        .as_deref()
        .map(|prefix| format!("{prefix}-{}", config.hindsight_bank_id))
        .unwrap_or_else(|| config.hindsight_bank_id.clone());
    match config.hindsight_scoping {
        crate::settings::HindsightScoping::Global => HindsightBankScope {
            bank_id: base,
            retain_tags: Vec::new(),
            recall_tags: Vec::new(),
            recall_tags_match: None,
        },
        crate::settings::HindsightScoping::PerProject => HindsightBankScope {
            bank_id: format!("{base}-{}", hindsight_project_label(cwd)),
            retain_tags: Vec::new(),
            recall_tags: Vec::new(),
            recall_tags_match: None,
        },
        crate::settings::HindsightScoping::PerProjectTagged => {
            let tag = format!("project:{}", hindsight_project_label(cwd));
            HindsightBankScope {
                bank_id: base,
                retain_tags: vec![tag.clone()],
                recall_tags: vec![tag],
                recall_tags_match: Some("any"),
            }
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
        out.push(
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            },
        );
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
    if label.is_empty() {
        "unknown".to_owned()
    } else {
        label
    }
}

#[derive(Clone)]
pub(crate) struct HindsightClient {
    executor: pi_ai::SharedHttpExecutor,
    base_url: url::Url,
    api_token: Option<String>,
    scope: HindsightBankScope,
    config: MemoryConfig,
}

#[derive(Debug, Deserialize)]
struct HindsightRecallResponse {
    #[serde(default)]
    results: Vec<HindsightRecallResult>,
}
#[derive(Debug, Deserialize)]
struct HindsightRecallResult {
    text: String,
    #[serde(default, rename = "type")]
    memory_type: Option<String>,
    #[serde(default)]
    mentioned_at: Option<String>,
}
#[derive(Debug, Deserialize)]
struct HindsightReflectResponse {
    #[serde(default)]
    text: String,
}

impl HindsightClient {
    pub(crate) fn new_with_executor(
        config: &MemoryConfig,
        cwd: &Path,
        executor: pi_ai::SharedHttpExecutor,
    ) -> Result<Self> {
        let raw_url = config.hindsight_api_url.as_deref().ok_or_else(|| {
            anyhow!("Hindsight is not configured: set settings.memory.hindsightApiUrl")
        })?;
        let mut base_url = url::Url::parse(raw_url)
            .map_err(|_| anyhow!("settings.memory.hindsightApiUrl is not a valid URL"))?;
        match base_url.scheme() {
            "https" => {}
            "http" if config.hindsight_allow_insecure => {}
            "http" => bail!(
                "refusing plaintext Hindsight API endpoint; use https or set settings.memory.hindsightAllowInsecure=true"
            ),
            scheme => bail!("unsupported Hindsight API URL scheme {scheme:?}; use http or https"),
        }
        base_url.set_query(None);
        base_url.set_fragment(None);
        Ok(Self {
            executor,
            base_url,
            api_token: config.hindsight_api_token.clone(),
            scope: hindsight_bank_scope(config, cwd),
            config: config.clone(),
        })
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
        let response: HindsightRecallResponse = self
            .request(
                "POST",
                "memories/recall",
                "recall",
                Some(body),
                self.config.hindsight_recall_timeout_ms,
                abort,
            )
            .await?;
        Ok(format_hindsight_memories(&response.results))
    }

    pub(crate) async fn retain(
        &self,
        content: &str,
        context: Option<&str>,
        abort: &AbortSignal,
    ) -> Result<String> {
        validate_required_hindsight_input("retain content", content)?;
        if let Some(context) = context {
            validate_hindsight_input("retain context", context)?;
        }
        self.ensure_bank(abort).await?;
        let body = serde_json::json!({ "items": [{ "content": content, "context": context, "tags": self.scope.retain_tags }], "async": false });
        let _: Value = self
            .request(
                "POST",
                "memories",
                "retain",
                Some(body),
                self.config.hindsight_retain_timeout_ms,
                abort,
            )
            .await?;
        Ok("1 memory retained.".to_owned())
    }

    pub(crate) async fn reflect(
        &self,
        query: &str,
        context: Option<&str>,
        abort: &AbortSignal,
    ) -> Result<String> {
        validate_required_hindsight_input("reflect query", query)?;
        if let Some(context) = context {
            validate_hindsight_input("reflect context", context)?;
        }
        self.ensure_bank(abort).await?;
        let body = serde_json::json!({
            "query": query,
            "context": context,
            "budget": hindsight_budget_name(self.config.hindsight_recall_budget),
            "tags": self.scope.recall_tags,
            "tags_match": self.scope.recall_tags_match,
        });
        let response: HindsightReflectResponse = self
            .request(
                "POST",
                "reflect",
                "reflect",
                Some(body),
                self.config.hindsight_reflect_timeout_ms,
                abort,
            )
            .await?;
        let text = response.text.trim();
        Ok(if text.is_empty() {
            "No relevant information found to reflect on.".to_owned()
        } else {
            redact_secrets(text)
        })
    }

    async fn ensure_bank(&self, abort: &AbortSignal) -> Result<()> {
        let body = serde_json::json!({ "reflect_mission": self.config.hindsight_bank_mission, "retain_mission": self.config.hindsight_retain_mission });
        self.request::<Value>(
            "PUT",
            "",
            "createBank",
            Some(body),
            self.config.hindsight_request_timeout_ms,
            abort,
        )
        .await?;
        Ok(())
    }

    async fn request<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        suffix: &str,
        operation: &str,
        body: Option<Value>,
        timeout_ms: u64,
        abort: &AbortSignal,
    ) -> Result<T> {
        check_aborted(abort)?;
        let mut url = self.base_url.clone();
        let mut path = url.path().trim_end_matches('/').to_owned();
        path.push_str("/v1/default/banks/");
        path.push_str(
            &url::form_urlencoded::byte_serialize(self.scope.bank_id.as_bytes())
                .collect::<String>(),
        );
        if !suffix.is_empty() {
            path.push('/');
            path.push_str(suffix);
        }
        url.set_path(&path);
        let method = match method {
            "POST" => pi_ai::HttpMethod::Post,
            "PUT" => pi_ai::HttpMethod::Put,
            _ => return Err(anyhow!("invalid Hindsight HTTP method")),
        };
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("application/json"),
        );
        if let Some(token) = self.api_token.as_deref() {
            headers.insert(
                http::header::AUTHORIZATION,
                http::HeaderValue::from_str(&format!("Bearer {token}"))?,
            );
        }
        let bytes = body
            .map(|body| serde_json::to_vec(&body))
            .transpose()?
            .unwrap_or_default();
        if !bytes.is_empty() {
            headers.insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("application/json"),
            );
        }
        let request = pi_ai::request_from_url(
            method,
            url.as_str(),
            &headers,
            bytes,
            &pi_ai::StreamOptions {
                timeout_ms: Some(timeout_ms),
                abort_signal: Some(abort.cancellation_token()),
                ..Default::default()
            },
            HINDSIGHT_RESPONSE_MAX_BYTES as u64,
        )?;
        let response = self
            .executor
            .execute(request)
            .await
            .map_err(|_| anyhow!("hindsight {operation} request failed"))?;
        let status = response.status();
        let bytes = pi_ai::collect_response_body(response).await?;
        if !status.is_success() {
            let detail = redact_secrets(&String::from_utf8_lossy(
                &bytes[..bytes.len().min(HINDSIGHT_ERROR_MAX_BYTES)],
            ));
            bail!(
                "hindsight {operation} failed (HTTP {}): {}",
                status.as_u16(),
                detail.trim()
            );
        }
        serde_json::from_slice(&bytes)
            .map_err(|_| anyhow!("hindsight {operation} returned invalid JSON"))
    }
}

fn validate_hindsight_input(label: &str, value: &str) -> Result<()> {
    if value.len() > HINDSIGHT_INPUT_MAX_BYTES {
        bail!("{label} exceeds the 1 MiB limit");
    }
    Ok(())
}
fn validate_required_hindsight_input(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{label} is required");
    }
    validate_hindsight_input(label, value)
}

const fn hindsight_budget_name(budget: crate::settings::HindsightBudget) -> &'static str {
    match budget {
        crate::settings::HindsightBudget::Low => "low",
        crate::settings::HindsightBudget::Mid => "mid",
        crate::settings::HindsightBudget::High => "high",
    }
}

fn format_hindsight_memories(results: &[HindsightRecallResult]) -> String {
    if results.is_empty() {
        return "No relevant memories found.".to_owned();
    }
    results
        .iter()
        .map(|memory| {
            let kind = memory
                .memory_type
                .as_deref()
                .map(|value| format!(" [{value}]"))
                .unwrap_or_default();
            let mentioned = memory
                .mentioned_at
                .as_deref()
                .map(|value| format!(" ({value})"))
                .unwrap_or_default();
            format!("- {}{kind}{mentioned}", redact_secrets(&memory.text))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub(crate) fn memory_tools_for(
    cwd: &str,
    session_env: Option<SessionEnvFn>,
    config: Option<MemoryConfig>,
) -> Vec<AgentTool> {
    memory_tools_for_with_generation(cwd, session_env, config, None)
}

pub(crate) fn memory_tools_for_with_generation(
    cwd: &str,
    session_env: Option<SessionEnvFn>,
    config: Option<MemoryConfig>,
    capture: Option<crate::tools::GenerationCaptureFn>,
) -> Vec<AgentTool> {
    let config = config.unwrap_or_default();
    match config.backend {
        crate::settings::MemoryBackend::Off => Vec::new(),
        crate::settings::MemoryBackend::Local => match capture {
            Some(capture) => vec![memory_tool_with_generation(cwd, session_env, capture)],
            None => vec![memory_tool_with_session_env(cwd, session_env)],
        },
        crate::settings::MemoryBackend::Hindsight => vec![
            recall_tool_with_generation(cwd, config.clone(), capture.clone()),
            retain_tool_with_generation(cwd, config.clone(), capture.clone()),
            reflect_tool_with_generation(cwd, config, capture),
        ],
    }
}

pub(crate) fn memory_tools_for_with_live_config_and_generation(
    cwd: &str,
    session_env: Option<SessionEnvFn>,
    config: Option<crate::tools::MemoryConfigFn>,
    capture: crate::tools::GenerationCaptureFn,
) -> Vec<AgentTool> {
    let resolved = config
        .as_ref()
        .and_then(|resolve| resolve())
        .unwrap_or_default();
    match resolved.backend {
        crate::settings::MemoryBackend::Off => Vec::new(),
        crate::settings::MemoryBackend::Hindsight => {
            memory_tools_for_with_generation(cwd, session_env, Some(resolved), Some(capture))
        }
        crate::settings::MemoryBackend::Local => {
            let cwd = PathBuf::from(cwd);
            vec![memory_tool_with_store_factory(
                Arc::new(move || {
                    let config = config
                        .as_ref()
                        .and_then(|resolve| resolve())
                        .unwrap_or_default();
                    if config.backend != crate::settings::MemoryBackend::Local {
                        bail!(
                            "local memory tool is disabled by the published application generation"
                        );
                    }
                    let generation = capture()?;
                    MemoryStore::try_default_for_with_capabilities(
                        generation.sandbox().capabilities(),
                        &cwd,
                    )
                }),
                session_env,
            )]
        }
    }
}

pub(crate) fn memory_tools_for_with_capabilities(
    capabilities: &Arc<CapabilityFs>,
    cwd: &str,
    session_env: Option<SessionEnvFn>,
    config: Option<MemoryConfig>,
) -> Vec<AgentTool> {
    let config = config.unwrap_or_default();
    match config.backend {
        crate::settings::MemoryBackend::Off => Vec::new(),
        crate::settings::MemoryBackend::Local => {
            vec![memory_tool_with_capabilities(
                capabilities,
                cwd,
                session_env,
            )]
        }
        crate::settings::MemoryBackend::Hindsight => vec![
            recall_tool(cwd, config.clone()),
            retain_tool(cwd, config.clone()),
            reflect_tool(cwd, config),
        ],
    }
}

pub(crate) fn memory_tools_for_with_capabilities_and_generation(
    capabilities: &Arc<CapabilityFs>,
    cwd: &str,
    session_env: Option<SessionEnvFn>,
    config: Option<MemoryConfig>,
    capture: crate::tools::GenerationCaptureFn,
) -> Vec<AgentTool> {
    let config = config.unwrap_or_default();
    match config.backend {
        crate::settings::MemoryBackend::Off => Vec::new(),
        crate::settings::MemoryBackend::Local => {
            vec![memory_tool_with_capabilities(
                capabilities,
                cwd,
                session_env,
            )]
        }
        crate::settings::MemoryBackend::Hindsight => vec![
            recall_tool_with_generation(cwd, config.clone(), Some(capture.clone())),
            retain_tool_with_generation(cwd, config.clone(), Some(capture.clone())),
            reflect_tool_with_generation(cwd, config, Some(capture)),
        ],
    }
}

pub(crate) fn memory_tools_for_persona(
    cwd: &str,
    persona_root: &Path,
    session_env: Option<SessionEnvFn>,
    config: Option<MemoryConfig>,
) -> Vec<AgentTool> {
    let config = config.unwrap_or_default();
    match config.backend {
        crate::settings::MemoryBackend::Off => Vec::new(),
        crate::settings::MemoryBackend::Local => vec![persona_memory_tool_with_session_env(
            persona_root,
            session_env,
        )],
        crate::settings::MemoryBackend::Hindsight => vec![
            recall_tool(cwd, config.clone()),
            retain_tool(cwd, config.clone()),
            reflect_tool(cwd, config),
        ],
    }
}

pub(crate) fn memory_tools_for_persona_with_capabilities(
    capabilities: &Arc<CapabilityFs>,
    cwd: &str,
    persona_root: &Path,
    session_env: Option<SessionEnvFn>,
    config: Option<MemoryConfig>,
) -> Result<Vec<AgentTool>> {
    let config = config.unwrap_or_default();
    match config.backend {
        crate::settings::MemoryBackend::Off => Ok(Vec::new()),
        crate::settings::MemoryBackend::Local => Ok(vec![persona_memory_tool_with_capabilities(
            capabilities,
            persona_root,
            session_env,
        )?]),
        crate::settings::MemoryBackend::Hindsight => Ok(vec![
            recall_tool(cwd, config.clone()),
            retain_tool(cwd, config.clone()),
            reflect_tool(cwd, config),
        ]),
    }
}

pub(crate) fn recall_tool(cwd: &str, config: MemoryConfig) -> AgentTool {
    recall_tool_with_generation(cwd, config, None)
}

fn recall_tool_with_generation(
    cwd: &str,
    config: MemoryConfig,
    capture: Option<crate::tools::GenerationCaptureFn>,
) -> AgentTool {
    let parameters = s_object(
        vec![("query", s_string("Natural-language search query"))],
        vec!["query"],
    );
    let cwd = cwd.to_owned();
    AgentTool::new(
        "recall",
        "Search the configured Hindsight memory bank for relevant prior context.",
        parameters,
        move |ctx| {
            let executor =
                capture
                    .as_ref()
                    .map(|capture| capture())
                    .transpose()
                    .map(|generation| {
                        generation.map(|generation| {
                            generation
                                .sandbox()
                                .http()
                                .executor(pi_sandbox::HostHttpClass::MemoryApi)
                        })
                    });
            let client = executor
                .and_then(|executor| {
                    executor.ok_or_else(|| anyhow!("Hindsight requires an application generation"))
                })
                .and_then(|executor| {
                    HindsightClient::new_with_executor(&config, Path::new(&cwd), executor)
                });
            async move { run_hindsight_op(client, "recall", ctx.arguments, ctx.abort).await }
        },
    )
    .with_capability(ToolCapability::Read)
}

pub(crate) fn retain_tool(cwd: &str, config: MemoryConfig) -> AgentTool {
    retain_tool_with_generation(cwd, config, None)
}

fn retain_tool_with_generation(
    cwd: &str,
    config: MemoryConfig,
    capture: Option<crate::tools::GenerationCaptureFn>,
) -> AgentTool {
    let parameters = s_object(
        vec![
            ("content", s_string("Information to remember")),
            ("context", s_string("Optional source context")),
        ],
        vec!["content"],
    );
    let cwd = cwd.to_owned();
    AgentTool::new(
        "retain",
        "Store important facts in the configured Hindsight memory bank.",
        parameters,
        move |ctx| {
            let executor =
                capture
                    .as_ref()
                    .map(|capture| capture())
                    .transpose()
                    .map(|generation| {
                        generation.map(|generation| {
                            generation
                                .sandbox()
                                .http()
                                .executor(pi_sandbox::HostHttpClass::MemoryApi)
                        })
                    });
            let client = executor
                .and_then(|executor| {
                    executor.ok_or_else(|| anyhow!("Hindsight requires an application generation"))
                })
                .and_then(|executor| {
                    HindsightClient::new_with_executor(&config, Path::new(&cwd), executor)
                });
            async move { run_hindsight_op(client, "retain", ctx.arguments, ctx.abort).await }
        },
    )
    .with_capability(ToolCapability::Write)
}

pub(crate) fn reflect_tool(cwd: &str, config: MemoryConfig) -> AgentTool {
    reflect_tool_with_generation(cwd, config, None)
}

fn reflect_tool_with_generation(
    cwd: &str,
    config: MemoryConfig,
    capture: Option<crate::tools::GenerationCaptureFn>,
) -> AgentTool {
    let parameters = s_object(
        vec![
            (
                "query",
                s_string("Question to answer from long-term memory"),
            ),
            ("context", s_string("Optional additional context")),
        ],
        vec!["query"],
    );
    let cwd = cwd.to_owned();
    AgentTool::new(
        "reflect",
        "Synthesize an answer from the configured Hindsight memory bank.",
        parameters,
        move |ctx| {
            let executor =
                capture
                    .as_ref()
                    .map(|capture| capture())
                    .transpose()
                    .map(|generation| {
                        generation.map(|generation| {
                            generation
                                .sandbox()
                                .http()
                                .executor(pi_sandbox::HostHttpClass::MemoryApi)
                        })
                    });
            let client = executor
                .and_then(|executor| {
                    executor.ok_or_else(|| anyhow!("Hindsight requires an application generation"))
                })
                .and_then(|executor| {
                    HindsightClient::new_with_executor(&config, Path::new(&cwd), executor)
                });
            async move { run_hindsight_op(client, "reflect", ctx.arguments, ctx.abort).await }
        },
    )
    .with_capability(ToolCapability::Read)
}

async fn run_hindsight_op(
    client: Result<HindsightClient>,
    op: &str,
    args: Value,
    abort: AbortSignal,
) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    let client = client?;
    let rendered = match op {
        "recall" => {
            let query = arg_str(&args, "query");
            if query.trim().is_empty() {
                return Err(anyhow!("query is required for recall"));
            }
            validate_hindsight_input("recall query", &query)?;
            client.recall(&query, &abort).await?
        }
        "retain" => {
            let content = arg_str(&args, "content");
            if content.trim().is_empty() {
                return Err(anyhow!("content is required for retain"));
            }
            let context = arg_str(&args, "context");
            client
                .retain(
                    &content,
                    (!context.trim().is_empty()).then_some(context.as_str()),
                    &abort,
                )
                .await?
        }
        "reflect" => {
            let query = arg_str(&args, "query");
            if query.trim().is_empty() {
                return Err(anyhow!("query is required for reflect"));
            }
            validate_hindsight_input("reflect query", &query)?;
            let context = arg_str(&args, "context");
            client
                .reflect(
                    &query,
                    (!context.trim().is_empty()).then_some(context.as_str()),
                    &abort,
                )
                .await?
        }
        other => return Err(anyhow!("Unknown hindsight op: {other}")),
    };
    check_aborted(&abort)?;
    Ok(text_result(
        truncate_head(&rendered, usize::MAX, OUTPUT_MAX_BYTES).content,
    ))
}

pub(crate) fn hindsight_injection_message(body: &str) -> pi_ai::Message {
    pi_ai::Message::Custom(pi_ai::CustomMessage {
        custom_type: "hindsight_memory".to_owned(),
        content: format!(
            "Related memories from the Hindsight backend for the latest user request:\n{body}"
        )
        .into(),
        display: false,
        details: None,
        timestamp: pi_ai::now_millis(),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};

    use futures_util::{FutureExt as _, stream};
    use pi_agent::{AbortController, ToolCallContext};
    use serde_json::json;

    fn test_capabilities() -> Arc<CapabilityFs> {
        CapabilityFs::new().expect("test filesystem capabilities")
    }

    fn store_in_with_capabilities(
        capabilities: &Arc<CapabilityFs>,
        dir: &Path,
        namespace: &str,
    ) -> Arc<MemoryStore> {
        Arc::new(
            MemoryStore::new_with_capabilities(capabilities, dir, namespace).expect("store builds"),
        )
    }

    fn store_in(dir: &Path, namespace: &str) -> Arc<MemoryStore> {
        store_in_with_capabilities(&test_capabilities(), dir, namespace)
    }

    fn error_rejects_symlink(error: &anyhow::Error) -> bool {
        error.chain().any(|source| {
            source.downcast_ref::<FsError>().is_some_and(|error| {
                matches!(error, FsError::UnsupportedEntryType)
                    || matches!(
                        error,
                        FsError::System {
                            raw_os_error: Some(40),
                            ..
                        }
                    )
            })
        })
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
            approval_token: None,
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
            .learn(
                "The release script lives in scripts/release.sh",
                vec!["release".into()],
                "s1",
            )
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
        assert_eq!(
            phrase.len(),
            3,
            "term matches still qualify for the phrase query"
        );
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
        store
            .learn("note one", vec!["alpha".into()], "s")
            .expect("learn");
        store
            .learn("note two", vec!["beta".into(), "alpha".into()], "s")
            .expect("learn");
        store.learn("note three", vec![], "s").expect("learn");
        assert_eq!(store.list(None).expect("list").len(), 3);
        let alpha = store.list(Some("ALPHA")).expect("list");
        assert_eq!(alpha.len(), 2, "case-insensitive tag filter");
        assert!(alpha.iter().all(|entry| {
            entry
                .tags
                .iter()
                .any(|tag| tag.eq_ignore_ascii_case("alpha"))
        }));
        assert_eq!(store.list(Some("missing")).expect("list").len(), 0);
    }

    #[test]
    fn persists_across_store_reload() {
        let dir = tempfile::tempdir().expect("dir");
        let (id, count) = {
            let store = store_in(dir.path(), "ns");
            store
                .learn_with_ts("durable note", vec!["durable".into()], "session-one", 5_000)
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

        let capabilities = test_capabilities();
        let first = persona_memory_tool_with_capabilities(&capabilities, &persona_root, None)
            .expect("first persona tool");
        (first.execute)(ctx(json!({
            "op": "learn",
            "content": "persona-only durable note"
        })))
        .await
        .expect("persona learn");

        let second = persona_memory_tool_with_capabilities(&capabilities, &persona_root, None)
            .expect("second persona tool");
        let recalled = text_of(
            &(second.execute)(ctx(json!({
                "op": "recall",
                "query": "persona-only"
            })))
            .await
            .expect("persona recall"),
        );
        assert!(recalled.contains("persona-only durable note"));
        assert_eq!(
            MemoryStore::persona_with_capabilities(&capabilities, &persona_root)
                .expect("persona store")
                .entries_path,
            persona_root.join("memory").join("entries.jsonl")
        );

        let ordinary = store_in_with_capabilities(
            &capabilities,
            &root.path().join("agent"),
            &repo_namespace(&cwd),
        );
        assert!(
            ordinary
                .recall("persona-only", 10)
                .expect("ordinary recall")
                .is_empty()
        );
        ordinary
            .learn("repo-only note", vec![], "repo")
            .expect("repo learn");
        assert!(
            MemoryStore::persona_with_capabilities(&capabilities, &persona_root)
                .expect("persona store")
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
        std::os::unix::fs::symlink(&outside, persona_root.join("memory")).expect("memory symlink");
        let capabilities = test_capabilities();
        let error = MemoryStore::persona_with_capabilities(&capabilities, &persona_root)
            .err()
            .expect("symlinked memory directory must fail");
        assert!(error_rejects_symlink(&error), "{error:#}");
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
        let store = MemoryStore::persona_with_capabilities(&capabilities, &persona_root)
            .expect("persona store with real memory directory");
        let error = store
            .list(None)
            .expect_err("symlinked entries file must fail");
        assert!(error_rejects_symlink(&error), "{error:#}");
        assert_eq!(
            std::fs::read_to_string(outside_entries).expect("outside intact"),
            "outside\n"
        );
    }

    #[test]
    fn bounds_entry_size_and_count() {
        let dir = tempfile::tempdir().expect("dir");
        let store = store_in(dir.path(), "ns");
        let oversized = "x".repeat(MAX_ENTRY_BYTES + 1);
        assert!(
            store.learn(&oversized, vec![], "s").is_err(),
            "oversized entry rejected"
        );
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
                store
                    .recall(&format!("unique-token-{i:03}"), 10)
                    .expect("recall")
                    .is_empty(),
                "oldest entry {i} must be evicted"
            );
        }
        for i in 5..105 {
            assert!(
                !store
                    .recall(&format!("unique-token-{i:03}"), 10)
                    .expect("recall")
                    .is_empty(),
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
            assert!(
                MemoryStore::new_with_capabilities(&test_capabilities(), dir.path(), hostile)
                    .is_err(),
                "{hostile:?} must be rejected"
            );
        }
        // The store path for a valid digest namespace stays under the memory root.
        let store = store_in(dir.path(), &namespace);
        let entries = store.entries_path.clone();
        assert!(entries.starts_with(dir.path().join(MEMORY_DIR_NAME)));
        assert_eq!(
            entries.file_name().and_then(|n| n.to_str()),
            Some(ENTRIES_FILE_NAME)
        );
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
        let secret_content =
            format!("token {token} and key\n{private_key}\nand github {github} and {aws}");
        store.learn(&secret_content, vec![], "s").expect("learn");
        let output = text_of(
            &(memory_tool_with_store(store.clone(), None).execute)(ctx(json!({
                "op": "recall",
                "query": "token"
            })))
            .await
            .expect("recall executes"),
        );
        assert!(output.contains("[REDACTED]"), "redaction marker present");
        let leaked = [
            ["sk", "-", "abcdefghijklmnop"].concat(),
            github,
            aws,
            "MIIEowIBAKCAQEA".to_owned(),
            ["PRIVATE ", "KEY-----"].concat(),
        ];
        for leaked in &leaked {
            assert!(
                !output.contains(leaked),
                "secret fragment {leaked:?} must not leak"
            );
        }
        // Output shape must never expose the store layout.
        let agent_dir = dir.path().to_string_lossy();
        assert!(
            !output.contains(agent_dir.as_ref()),
            "agent dir must not appear in output"
        );
        assert!(!output.contains("entries.jsonl"));
        assert!(!output.contains("memory/"));
    }

    #[tokio::test]
    async fn tool_records_source_session_provenance() {
        let dir = tempfile::tempdir().expect("dir");
        let store = store_in(dir.path(), "ns");
        // Session-backed tool records PI_SESSION_ID.
        let tool = memory_tool_with_store(store.clone(), Some(session_env_with("sess-abc")));
        let result = text_of(
            &(tool.execute)(ctx(json!({
                "op": "learn",
                "content": "provenance note",
                "tags": ["p"],
            })))
            .await
            .expect("learn executes"),
        );
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
        let output = text_of(
            &(tool.execute)(ctx(json!({ "op": "list" })))
                .await
                .expect("list executes"),
        );
        assert_eq!(output, "No memory entries.");
        // recall on an empty store renders a friendly message.
        let output = text_of(
            &(tool.execute)(ctx(json!({ "op": "recall", "query": "x" })))
                .await
                .expect("recall executes"),
        );
        assert_eq!(output, "No memory entries match \"x\".");
        // forget with a bogus id renders a miss.
        let output = text_of(
            &(tool.execute)(ctx(json!({ "op": "forget", "id": "nope" })))
                .await
                .expect("forget executes"),
        );
        assert_eq!(output, "No memory entry with id nope.");
        // limit clamps: 0 → 1, huge → 50.
        let (id, _) = store.learn("clamp me", vec![], "s").expect("learn");
        let output = text_of(
            &(tool.execute)(ctx(json!({ "op": "recall", "query": "clamp", "limit": 0 })))
                .await
                .expect("recall executes"),
        );
        assert!(
            output.contains(&id),
            "limit 0 clamped to at least one result"
        );
        let output = text_of(
            &(tool.execute)(ctx(json!({ "op": "forget", "id": &id })))
                .await
                .expect("forget executes"),
        );
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

    #[derive(Debug)]
    struct RecordedHttpRequest {
        path_and_query: String,
        body: Value,
    }

    struct TestHttpResponse {
        status: http::StatusCode,
        headers: http::HeaderMap,
        chunks: Vec<Vec<u8>>,
    }

    #[derive(Clone)]
    struct TestHttpExecutor {
        responses: Arc<Mutex<VecDeque<TestHttpResponse>>>,
        requests: Arc<Mutex<Vec<RecordedHttpRequest>>>,
    }
    impl pi_ai::HttpExecutor for TestHttpExecutor {
        fn execute<'a>(
            &'a self,
            request: pi_ai::HttpRequest,
        ) -> futures_util::future::BoxFuture<'a, Result<pi_ai::HttpResponse>> {
            async move {
                let parts = request.into_parts();
                let bound = parts.max_response_bytes.get();
                let request_body = serde_json::from_slice(&parts.body).unwrap_or(Value::Null);
                self.requests
                    .lock()
                    .expect("request lock")
                    .push(RecordedHttpRequest {
                        path_and_query: parts.path_and_query,
                        body: request_body.clone(),
                    });
                let mut response = self
                    .responses
                    .lock()
                    .expect("response lock")
                    .pop_front()
                    .ok_or_else(|| anyhow!("unexpected test HTTP request"))?;
                if response.status.is_redirection() {
                    let location = response
                        .headers
                        .get(http::header::LOCATION)
                        .and_then(|value| value.to_str().ok())
                        .ok_or_else(|| anyhow!("test redirect is missing Location"))?;
                    let target =
                        url::Url::parse(location).context("parsing test redirect target")?;
                    let mut path_and_query = target.path().to_owned();
                    if let Some(query) = target.query() {
                        path_and_query.push('?');
                        path_and_query.push_str(query);
                    }
                    self.requests
                        .lock()
                        .expect("request lock")
                        .push(RecordedHttpRequest {
                            path_and_query,
                            body: request_body,
                        });
                    response = self
                        .responses
                        .lock()
                        .expect("response lock")
                        .pop_front()
                        .ok_or_else(|| anyhow!("test redirect is missing its target response"))?;
                }
                let declared_oversize = response
                    .headers
                    .get(http::header::CONTENT_LENGTH)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
                    .is_some_and(|length| length > bound);
                let streamed_bytes = response
                    .chunks
                    .iter()
                    .try_fold(0_u64, |total, chunk| total.checked_add(chunk.len() as u64));
                let chunks: Vec<Result<Vec<u8>>> =
                    if declared_oversize || streamed_bytes.is_none_or(|total| total > bound) {
                        vec![Err(anyhow!("test HTTP response exceeds {bound} bytes"))]
                    } else {
                        response.chunks.into_iter().map(Ok).collect()
                    };
                Ok(pi_ai::HttpResponse::new(
                    response.status,
                    response.headers,
                    Box::pin(stream::iter(chunks)),
                ))
            }
            .boxed()
        }
    }

    fn http_response(status: &str, body: impl Into<Vec<u8>>) -> TestHttpResponse {
        let body = body.into();
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            http::header::CONTENT_LENGTH,
            http::HeaderValue::from_str(&body.len().to_string()).expect("content length"),
        );
        TestHttpResponse {
            status: status
                .split_whitespace()
                .next()
                .expect("HTTP status code")
                .parse()
                .expect("HTTP status"),
            headers,
            chunks: vec![body],
        }
    }

    fn redirect_response(location: &str) -> TestHttpResponse {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::LOCATION,
            http::HeaderValue::from_str(location).expect("redirect location"),
        );
        TestHttpResponse {
            status: http::StatusCode::TEMPORARY_REDIRECT,
            headers,
            chunks: Vec::new(),
        }
    }

    fn test_hindsight_client(
        config: &MemoryConfig,
        responses: Vec<TestHttpResponse>,
    ) -> (HindsightClient, Arc<Mutex<Vec<RecordedHttpRequest>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let executor = TestHttpExecutor {
            responses: Arc::new(Mutex::new(responses.into())),
            requests: requests.clone(),
        };
        let client = HindsightClient::new_with_executor(config, Path::new("."), Arc::new(executor))
            .expect("test Hindsight client");
        (client, requests)
    }

    async fn test_generation_capture(
        cwd: &Path,
    ) -> (
        tempfile::TempDir,
        crate::Application,
        crate::tools::GenerationCaptureFn,
        pi_ai::providers::FauxProviderRegistration,
    ) {
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let mut resource_options = crate::ResourceManagerOptions::new(cwd);
        let session_dir = agent_dir.path().join("sessions");
        std::fs::create_dir_all(&session_dir).expect("session dir");
        std::fs::write(
            agent_dir.path().join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )
        .expect("isolated settings");
        resource_options.agent_dir = agent_dir.path().to_path_buf();
        resource_options.project_trust_override = Some(true);
        resource_options.disable_extensions = true;
        resource_options.disable_skills = true;
        resource_options.disable_prompt_templates = true;
        resource_options.disable_themes = true;
        resource_options.disable_context_files = true;
        let resources = crate::ResourceManager::new(resource_options).expect("resources");
        let registration = pi_ai::providers::register_faux_provider(Default::default());
        let session = crate::Session::new_with_additional_tools_filtered_and_discovery(
            crate::SessionOptions {
                cwd: cwd.to_path_buf(),
                system_prompt: String::new(),
                model: registration.model(None).expect("generation faux model"),
                thinking_level: pi_agent::ThinkingLevel::Off,
                tools: Some(Vec::new()),
                compaction: None,
                stream_options: Default::default(),
                api_key: "test".to_owned(),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn: None,
                auth_resolver: None,
                auth_manager: None,
            },
            Vec::new(),
            crate::ToolSelection::default(),
            crate::ResourceDiscovery::Disabled,
        )
        .expect("session");
        session.set_session_dir(session_dir);
        session
            .attach_resources(resources)
            .await
            .expect("attach resources");
        let application = crate::Application::new(session).await;
        let capture = application.generation_capture_fn();
        (agent_dir, application, capture, registration)
    }

    #[tokio::test]
    async fn hindsight_insecure_opt_in_follows_plaintext_redirect() {
        let config = hindsight_config("http://source.test".to_owned());
        let target = "http://target.test/v1/default/banks/testbank/memories/recall";
        let (client, requests) = test_hindsight_client(
            &config,
            vec![
                redirect_response(target),
                http_response("200 OK", br#"{"results":[]}"#.to_vec()),
            ],
        );
        let output = client
            .recall("redirect opt in", &AbortSignal::none())
            .await
            .expect("plaintext redirect follows");
        assert_eq!(output, "No relevant memories found.");
        let requests = requests.lock().expect("requests");
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].path_and_query,
            "/v1/default/banks/testbank/memories/recall"
        );
        assert_eq!(
            requests[1].path_and_query,
            "/v1/default/banks/testbank/memories/recall"
        );
    }

    #[test]
    fn memory_config_debug_redacts_hindsight_token() {
        let token = ["debug", "-", "sensitive-value"].concat();
        let rendered = format!(
            "{:?}",
            MemoryConfig {
                hindsight_api_token: Some(token.clone()),
                ..Default::default()
            }
        );
        assert!(rendered.contains("[REDACTED]"), "{rendered}");
        assert!(!rendered.contains(&token), "{rendered}");
    }

    #[tokio::test]
    async fn hindsight_client_methods_enforce_input_bounds_before_network() {
        let config = hindsight_config("http://hindsight.test".to_owned());
        let (client, _) = test_hindsight_client(&config, Vec::new());
        let oversized = "x".repeat(HINDSIGHT_INPUT_MAX_BYTES + 1);
        let abort = AbortSignal::none();
        assert!(
            client
                .recall("   ", &abort)
                .await
                .expect_err("blank recall")
                .to_string()
                .contains("required")
        );
        assert!(
            client
                .recall(&oversized, &abort)
                .await
                .expect_err("large recall")
                .to_string()
                .contains("1 MiB")
        );
        assert!(
            client
                .retain("", None, &abort)
                .await
                .expect_err("blank retain")
                .to_string()
                .contains("required")
        );
        assert!(
            client
                .retain(&oversized, None, &abort)
                .await
                .expect_err("large retain content")
                .to_string()
                .contains("1 MiB")
        );
        assert!(
            client
                .retain("ok", Some(&oversized), &abort)
                .await
                .expect_err("large retain context")
                .to_string()
                .contains("1 MiB")
        );
        assert!(
            client
                .reflect(" ", None, &abort)
                .await
                .expect_err("blank reflect")
                .to_string()
                .contains("required")
        );
        assert!(
            client
                .reflect(&oversized, None, &abort)
                .await
                .expect_err("large reflect query")
                .to_string()
                .contains("1 MiB")
        );
        assert!(
            client
                .reflect("ok", Some(&oversized), &abort)
                .await
                .expect_err("large reflect context")
                .to_string()
                .contains("1 MiB")
        );
    }

    #[tokio::test]
    async fn hindsight_bank_ensure_failures_stop_retain_and_reflect() {
        for (operation, status, code) in [
            ("retain", "401 Unauthorized", 401),
            ("retain", "503 Service Unavailable", 503),
            ("reflect", "401 Unauthorized", 401),
            ("reflect", "503 Service Unavailable", 503),
        ] {
            let config = hindsight_config("http://hindsight.test".to_owned());
            let (client, requests) = test_hindsight_client(
                &config,
                vec![http_response(
                    status,
                    br#"{"detail":"bank unavailable"}"#.to_vec(),
                )],
            );
            let error = if operation == "retain" {
                client
                    .retain("bounded content", None, &AbortSignal::none())
                    .await
                    .expect_err("ensure failure")
            } else {
                client
                    .reflect("bounded query", None, &AbortSignal::none())
                    .await
                    .expect_err("ensure failure")
            };
            assert!(
                error
                    .to_string()
                    .contains(&format!("createBank failed (HTTP {code})")),
                "{error:#}"
            );
            assert_eq!(
                requests.lock().expect("requests").len(),
                1,
                "{operation} must not continue after ensure failure"
            );
        }
    }

    #[tokio::test]
    async fn hindsight_recall_matches_http_wire_contract() {
        let body = br#"{"results":[{"text":"release script lives in scripts/release.sh","type":"world","mentioned_at":"2026-08-09"}]}"#;
        let config = hindsight_config("http://hindsight.test".to_owned());
        let (client, requests) =
            test_hindsight_client(&config, vec![http_response("200 OK", body.to_vec())]);
        let output = client
            .recall("release script", &AbortSignal::none())
            .await
            .expect("recall");
        assert!(output.contains("scripts/release.sh"), "{output}");
        let requests = requests.lock().expect("requests");
        assert_eq!(
            requests[0].path_and_query,
            "/v1/default/banks/testbank/memories/recall"
        );
        assert_eq!(requests[0].body["query"], "release script");
        assert_eq!(requests[0].body["budget"], "mid");
        assert_eq!(requests[0].body["max_tokens"], 1024);
        assert_eq!(requests[0].body["types"], json!(["world", "experience"]));
    }

    #[tokio::test]
    async fn hindsight_retain_ensures_bank_and_sends_item() {
        let config = hindsight_config("http://hindsight.test".to_owned());
        let (client, requests) = test_hindsight_client(
            &config,
            vec![
                http_response("200 OK", br#"{}"#.to_vec()),
                http_response("200 OK", br#"{}"#.to_vec()),
            ],
        );
        let output = client
            .retain(
                "cache lives in target/",
                Some("build"),
                &AbortSignal::none(),
            )
            .await
            .expect("retain");
        assert_eq!(output, "1 memory retained.");
        let requests = requests.lock().expect("requests");
        assert_eq!(requests[0].path_and_query, "/v1/default/banks/testbank");
        assert_eq!(
            requests[1].path_and_query,
            "/v1/default/banks/testbank/memories"
        );
        assert_eq!(
            requests[1].body["items"][0]["content"],
            "cache lives in target/"
        );
        assert_eq!(requests[1].body["items"][0]["context"], "build");
    }

    #[tokio::test]
    async fn hindsight_http_errors_are_redacted() {
        let token = ["sk", "-", "abcdefghijklmnop1234567890"].concat();
        let body = format!(r#"{{"detail":"boom {token}"}}"#);
        let config = hindsight_config("http://hindsight.test".to_owned());
        let (client, _) = test_hindsight_client(
            &config,
            vec![http_response(
                "500 Internal Server Error",
                body.into_bytes(),
            )],
        );
        let message = client
            .recall("x", &AbortSignal::none())
            .await
            .expect_err("HTTP error")
            .to_string();
        assert!(message.contains("HTTP 500"), "{message}");
        assert!(message.contains("[REDACTED]"), "{message}");
        assert!(
            !message.contains(&["sk", "-", "abcdefghijklmnop"].concat()),
            "{message}"
        );
    }

    #[tokio::test]
    async fn hindsight_large_response_is_rejected_without_hanging() {
        let oversized = vec![b'x'; HINDSIGHT_RESPONSE_MAX_BYTES + 1];
        let config = hindsight_config("http://hindsight.test".to_owned());
        let (client, _) = test_hindsight_client(&config, vec![http_response("200 OK", oversized)]);
        let error = client
            .recall("x", &AbortSignal::none())
            .await
            .expect_err("oversized response");
        assert!(error.to_string().contains("exceeds"), "{error:#}");
    }

    #[test]
    fn hindsight_rejects_unconfigured_and_plaintext_endpoints() {
        let error = HindsightClient::new_with_executor(
            &MemoryConfig {
                backend: crate::settings::MemoryBackend::Hindsight,
                ..Default::default()
            },
            Path::new("."),
            Arc::new(TestHttpExecutor {
                responses: Arc::new(Mutex::new(VecDeque::new())),
                requests: Arc::new(Mutex::new(Vec::new())),
            }),
        )
        .err()
        .expect("missing URL errors");
        assert!(error.to_string().contains("hindsightApiUrl"));
        let error = HindsightClient::new_with_executor(
            &MemoryConfig {
                backend: crate::settings::MemoryBackend::Hindsight,
                hindsight_api_url: Some("http://127.0.0.1:8888".to_owned()),
                ..Default::default()
            },
            Path::new("."),
            Arc::new(TestHttpExecutor {
                responses: Arc::new(Mutex::new(VecDeque::new())),
                requests: Arc::new(Mutex::new(Vec::new())),
            }),
        )
        .err()
        .expect("plaintext errors");
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

        for scoping in [
            HindsightScoping::PerProject,
            HindsightScoping::PerProjectTagged,
        ] {
            let scope_at = |path: &std::path::Path| hindsight_bank_scope(&config(scoping), path);
            let a = scope_at(&repo_a);
            let b = scope_at(&repo_b);
            assert_ne!(
                a, b,
                "{scoping:?}: same-basename repos under different parents must differ"
            );
            let rendered_a = format!("{a:?}");
            let rendered_b = format!("{b:?}");
            assert!(
                rendered_a.contains("same-name") && rendered_b.contains("same-name"),
                "{scoping:?}: the human-readable basename must be retained: {rendered_a} / {rendered_b}"
            );
            // The distinguishing component is the same identity digest local
            // memory digests (its prefix), not the basename.
            let namespace_a = repo_namespace(&repo_a);
            let namespace_b = repo_namespace(&repo_b);
            assert_ne!(namespace_a, namespace_b, "sanity: digests differ");
            let prefix_a = &namespace_a[..HINDSIGHT_SCOPE_DIGEST_PREFIX];
            let prefix_b = &namespace_b[..HINDSIGHT_SCOPE_DIGEST_PREFIX];
            assert_ne!(prefix_a, prefix_b, "sanity: digest prefixes differ");
            assert!(
                rendered_a.contains(prefix_a),
                "{scoping:?}: scope must embed the local-memory digest prefix {prefix_a}: {rendered_a}"
            );
            assert!(
                rendered_b.contains(prefix_b),
                "{scoping:?}: scope must embed the local-memory digest prefix {prefix_b}: {rendered_b}"
            );
            // Nested paths inside one checkout share the repo-root scope.
            assert_eq!(
                scope_at(&nested),
                a,
                "{scoping:?}: nested paths must resolve identically to the repo root"
            );
        }

        // Per-project keys the bank id by the label; per-project-tagged keeps
        // the shared bank id and tags each item by the label.
        let per_project = hindsight_bank_scope(&config(HindsightScoping::PerProject), &repo_a);
        let label = format!(
            "same-name-{}",
            &repo_namespace(&repo_a)[..HINDSIGHT_SCOPE_DIGEST_PREFIX]
        );
        assert_eq!(
            per_project.bank_id,
            format!("{DEFAULT_HINDSIGHT_BANK_ID}-{label}")
        );
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
        assert!(
            scope
                .bank_id
                .starts_with(&format!("{DEFAULT_HINDSIGHT_BANK_ID}-weird-name-")),
            "{}",
            scope.bank_id
        );
        assert!(
            scope
                .bank_id
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')),
            "bank id must stay token-safe: {}",
            scope.bank_id
        );
        // A basename with no readable characters falls back to `unknown` but
        // stays unique via the digest prefix.
        let bare = dir.path().join("!!!");
        std::fs::create_dir_all(bare.join(".git")).expect("git");
        let bare_scope = hindsight_bank_scope(&scoped_config(), &bare);
        assert!(
            bare_scope
                .bank_id
                .starts_with(&format!("{DEFAULT_HINDSIGHT_BANK_ID}-unknown-")),
            "{}",
            bare_scope.bank_id
        );
        assert_ne!(bare_scope.bank_id, scope.bank_id);
    }

    #[test]
    fn memory_tools_for_selects_backend_tool_set() {
        let dir = tempfile::tempdir().expect("dir");
        let cwd = dir.path().to_string_lossy().into_owned();
        let names = |config: Option<MemoryConfig>| {
            memory_tools_for_with_capabilities(&test_capabilities(), &cwd, None, config)
                .into_iter()
                .map(|tool| tool.name)
                .collect::<Vec<_>>()
        };
        // off hides every memory tool.
        assert!(
            names(Some(MemoryConfig {
                backend: crate::settings::MemoryBackend::Off,
                ..Default::default()
            }))
            .is_empty()
        );
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
            vec![
                "recall".to_owned(),
                "retain".to_owned(),
                "reflect".to_owned()
            ]
        );
        // No config → default local backend (existing behavior).
        assert_eq!(names(None), vec!["memory".to_owned()]);
    }

    #[tokio::test]
    async fn hindsight_tools_validate_arguments() {
        let dir = tempfile::tempdir().expect("dir");
        let (_agent_dir, _application, capture, _registration) =
            test_generation_capture(dir.path()).await;
        let config = hindsight_config("http://hindsight.test".to_owned());
        let recall = recall_tool_with_generation(
            dir.path().to_string_lossy().as_ref(),
            config.clone(),
            Some(capture.clone()),
        );
        let error = (recall.execute)(ctx(json!({})))
            .await
            .expect_err("missing query errors");
        assert!(error.to_string().contains("query is required"));
        let retain = retain_tool_with_generation(
            dir.path().to_string_lossy().as_ref(),
            config.clone(),
            Some(capture.clone()),
        );
        let error = (retain.execute)(ctx(json!({})))
            .await
            .expect_err("missing content errors");
        assert!(error.to_string().contains("content is required"));
        let reflect = reflect_tool_with_generation(
            dir.path().to_string_lossy().as_ref(),
            config,
            Some(capture),
        );
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
        assert!(
            !custom.display,
            "injection must never auto-submit to the UI"
        );
        let text = match &custom.content {
            pi_ai::CustomMessageContent::Text(text) => text.clone(),
            pi_ai::CustomMessageContent::Blocks(blocks) => text_of_blocks(blocks),
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

    fn seed_entries(agent_dir: &Path, namespace: &str, entries: &[MemoryEntry]) {
        let dir = agent_dir.join("memory").join(namespace);
        std::fs::create_dir_all(&dir).expect("namespace dir");
        let mut body = String::new();
        for entry in entries {
            body.push_str(&serde_json::to_string(entry).expect("entry serializes"));
            body.push('\n');
        }
        std::fs::write(dir.join(ENTRIES_FILE_NAME), body).expect("entries written");
    }

    fn namespace_dir(agent_dir: &Path, namespace: &str) -> PathBuf {
        agent_dir.join("memory").join(namespace)
    }

    #[test]
    fn migrate_same_repo_anchor_is_a_noop() {
        let root = tempfile::tempdir().expect("root");
        let agent_dir = root.path().join("agent");
        let repo = root.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).expect("git");
        let nested = repo.join("docs");
        std::fs::create_dir_all(&nested).expect("nested");
        assert_eq!(repo_namespace(&repo), repo_namespace(&nested));
        let capabilities = test_capabilities();
        let store = store_in_with_capabilities(&capabilities, &agent_dir, &repo_namespace(&repo));
        store.learn("anchored note", vec![], "s1").expect("learn");

        let migrated =
            migrate_repo_namespace_with_capabilities(&capabilities, &agent_dir, &repo, &nested)
                .expect("migrate");
        assert!(!migrated, "same repository anchor must not copy");
        assert_eq!(store.list(None).expect("list").len(), 1);
        // No second namespace appeared under the memory root.
        assert_eq!(
            std::fs::read_dir(agent_dir.join("memory"))
                .expect("memory dir")
                .count(),
            1
        );
    }

    #[test]
    fn migrate_copies_entries_when_the_repo_path_changes() {
        let root = tempfile::tempdir().expect("root");
        let agent_dir = root.path().join("agent");
        let old_repo = root.path().join("old").join("repo");
        let new_repo = root.path().join("new").join("renamed");
        std::fs::create_dir_all(old_repo.join(".git")).expect("git old");
        std::fs::create_dir_all(new_repo.join(".git")).expect("git new");
        assert_ne!(repo_namespace(&old_repo), repo_namespace(&new_repo));
        let capabilities = test_capabilities();
        let old = store_in_with_capabilities(&capabilities, &agent_dir, &repo_namespace(&old_repo));
        let (carried_id, _) = old
            .learn("carried note", vec!["alpha".into()], "s1")
            .expect("learn");
        let resident = store_in_with_capabilities(&capabilities, &agent_dir, &repo_namespace(&new_repo));
        let (resident_id, _) = resident
            .learn("already there", vec![], "s2")
            .expect("learn");

        let migrated =
            migrate_repo_namespace_with_capabilities(&capabilities, &agent_dir, &old_repo, &new_repo)
                .expect("migrate");
        assert!(migrated);

        let merged = store_in_with_capabilities(&capabilities, &agent_dir, &repo_namespace(&new_repo));
        let entries = merged.list(None).expect("list");
        assert_eq!(entries.len(), 2);
        let carried = entries
            .iter()
            .find(|entry| entry.id == carried_id)
            .expect("carried entry");
        assert_eq!(carried.content, "carried note");
        assert_eq!(carried.tags, vec!["alpha".to_string()]);
        assert_eq!(carried.session, "s1");
        assert!(entries.iter().any(|entry| entry.id == resident_id));
        // The old archive is recovery metadata: untouched until core commits.
        assert_eq!(old.list(None).expect("old list").len(), 1);
    }

    #[test]
    fn migrate_colliding_equal_entry_is_a_noop() {
        let root = tempfile::tempdir().expect("root");
        let agent_dir = root.path().join("agent");
        let old_repo = root.path().join("old").join("repo");
        let new_repo = root.path().join("new").join("repo");
        std::fs::create_dir_all(old_repo.join(".git")).expect("git old");
        std::fs::create_dir_all(new_repo.join(".git")).expect("git new");
        let capabilities = test_capabilities();
        let old = store_in_with_capabilities(&capabilities, &agent_dir, &repo_namespace(&old_repo));
        old.learn("shared note", vec!["alpha".into()], "s1")
            .expect("learn");
        let entry = old.list(None).expect("list").remove(0);
        let new_namespace = repo_namespace(&new_repo);
        seed_entries(&agent_dir, &new_namespace, std::slice::from_ref(&entry));

        let migrated =
            migrate_repo_namespace_with_capabilities(&capabilities, &agent_dir, &old_repo, &new_repo)
                .expect("migrate");
        assert!(!migrated, "an identical target entry must not be copied again");
        let target = store_in_with_capabilities(&capabilities, &agent_dir, &new_namespace);
        assert_eq!(target.list(None).expect("list").len(), 1);
        assert_eq!(old.list(None).expect("old list").len(), 1);
    }

    #[test]
    fn migrate_collision_keeps_the_target_entry_on_conflict() {
        let root = tempfile::tempdir().expect("root");
        let agent_dir = root.path().join("agent");
        let old_repo = root.path().join("old").join("repo");
        let new_repo = root.path().join("new").join("repo");
        std::fs::create_dir_all(old_repo.join(".git")).expect("git old");
        std::fs::create_dir_all(new_repo.join(".git")).expect("git new");
        let capabilities = test_capabilities();
        let old = store_in_with_capabilities(&capabilities, &agent_dir, &repo_namespace(&old_repo));
        old.learn("old version", vec![], "s1").expect("learn");
        let mut conflict = old.list(None).expect("list").remove(0);
        conflict.content = "target version".to_string();
        let new_namespace = repo_namespace(&new_repo);
        seed_entries(&agent_dir, &new_namespace, std::slice::from_ref(&conflict));

        let migrated =
            migrate_repo_namespace_with_capabilities(&capabilities, &agent_dir, &old_repo, &new_repo)
                .expect("migrate");
        assert!(!migrated, "a colliding id must not be overwritten");
        let target = store_in_with_capabilities(&capabilities, &agent_dir, &new_namespace);
        let entries = target.list(None).expect("list");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "target version");
        assert_eq!(old.list(None).expect("old list")[0].content, "old version");
    }

    #[test]
    fn migrate_is_idempotent() {
        let root = tempfile::tempdir().expect("root");
        let agent_dir = root.path().join("agent");
        let old_repo = root.path().join("old").join("repo");
        let new_repo = root.path().join("new").join("repo");
        std::fs::create_dir_all(old_repo.join(".git")).expect("git old");
        std::fs::create_dir_all(new_repo.join(".git")).expect("git new");
        let capabilities = test_capabilities();
        let old = store_in_with_capabilities(&capabilities, &agent_dir, &repo_namespace(&old_repo));
        old.learn("first note", vec![], "s1").expect("learn");
        old.learn("second note", vec![], "s1").expect("learn");

        assert!(
            migrate_repo_namespace_with_capabilities(&capabilities, &agent_dir, &old_repo, &new_repo)
                .expect("first migrate")
        );
        assert!(
            !migrate_repo_namespace_with_capabilities(&capabilities, &agent_dir, &old_repo, &new_repo)
                .expect("second migrate"),
            "a completed migration must not copy again"
        );
        let merged = store_in_with_capabilities(&capabilities, &agent_dir, &repo_namespace(&new_repo));
        let entries = merged.list(None).expect("list");
        assert_eq!(entries.len(), 2);
        let ids: BTreeSet<&str> = entries.iter().map(|entry| entry.id.as_str()).collect();
        assert_eq!(ids.len(), 2, "entries must not be duplicated");
    }

    #[test]
    fn commit_removes_the_old_archive_only_after_migration() {
        let root = tempfile::tempdir().expect("root");
        let agent_dir = root.path().join("agent");
        let old_repo = root.path().join("old").join("repo");
        let new_repo = root.path().join("new").join("repo");
        std::fs::create_dir_all(old_repo.join(".git")).expect("git old");
        std::fs::create_dir_all(new_repo.join(".git")).expect("git new");
        let old_namespace = repo_namespace(&old_repo);
        let new_namespace = repo_namespace(&new_repo);
        let capabilities = test_capabilities();
        let old = store_in_with_capabilities(&capabilities, &agent_dir, &old_namespace);
        old.learn("precious note", vec![], "s1").expect("learn");

        // Refuses before the copy: recovery data must never be dropped first.
        let error = commit_repo_namespace_migration_with_capabilities(
            &capabilities,
            &agent_dir,
            &old_repo,
            &new_repo,
        )
        .expect_err("commit must refuse before migration");
        assert!(error.to_string().contains("refusing"), "{error:#}");
        assert!(namespace_dir(&agent_dir, &old_namespace).exists());

        assert!(
            migrate_repo_namespace_with_capabilities(&capabilities, &agent_dir, &old_repo, &new_repo)
                .expect("migrate")
        );
        assert!(
            commit_repo_namespace_migration_with_capabilities(
                &capabilities,
                &agent_dir,
                &old_repo,
                &new_repo,
            )
            .expect("commit")
        );
        assert!(
            !namespace_dir(&agent_dir, &old_namespace).exists(),
            "old archive removed"
        );
        let target = store_in_with_capabilities(&capabilities, &agent_dir, &new_namespace);
        assert_eq!(target.list(None).expect("list").len(), 1);
        // Idempotent: the archive is already gone.
        assert!(
            !commit_repo_namespace_migration_with_capabilities(
                &capabilities,
                &agent_dir,
                &old_repo,
                &new_repo,
            )
            .expect("second commit")
        );
    }

    #[test]
    fn migrates_after_the_old_checkout_is_deleted_with_nested_cwd() {
        let root = tempfile::tempdir().expect("root");
        let agent_dir = root.path().join("agent");
        let old_repo = root.path().join("old").join("repo");
        let old_cwd = old_repo.join("docs");
        let new_repo = root.path().join("new").join("renamed");
        let new_cwd = new_repo.join("docs");
        std::fs::create_dir_all(old_repo.join(".git")).expect("git old");
        std::fs::create_dir_all(&old_cwd).expect("old cwd");
        std::fs::create_dir_all(new_repo.join(".git")).expect("git new");
        std::fs::create_dir_all(&new_cwd).expect("new cwd");
        let capabilities = test_capabilities();
        // Pre-move store: anchored at the old repository root, not the cwd.
        let old = store_in_with_capabilities(&capabilities, &agent_dir, &repo_namespace(&old_repo));
        let (carried_id, _) = old
            .learn("carried nested note", vec![], "s1")
            .expect("learn");
        // The move already happened: the old checkout (and its .git) is gone,
        // so the old namespace must be inferred from the two cwds.
        std::fs::remove_dir_all(&old_repo).expect("old checkout removed");

        let migrated =
            migrate_repo_namespace_with_capabilities(&capabilities, &agent_dir, &old_cwd, &new_cwd)
                .expect("migrate");
        assert!(migrated);
        let merged = store_in_with_capabilities(&capabilities, &agent_dir, &repo_namespace(&new_repo));
        let entries = merged.list(None).expect("list");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, carried_id);
        assert_eq!(entries[0].content, "carried nested note");
    }

    fn planted_entries(id_prefix: &str, count: usize, ts_base: i64) -> Vec<MemoryEntry> {
        (0..count)
            .map(|i| MemoryEntry {
                id: format!("{id_prefix}-{i:032}"),
                content: format!("{id_prefix} note {i}"),
                tags: Vec::new(),
                ts: ts_base + i as i64,
                session: "s1".to_string(),
            })
            .collect()
    }

    #[test]
    fn migrate_caps_the_merged_namespace_at_max_entries() {
        // Target below the cap: target entries are authoritative, old entries
        // fill the remaining slots newest-first.
        let root = tempfile::tempdir().expect("root");
        let agent_dir = root.path().join("agent");
        let old_repo = root.path().join("old").join("repo");
        let new_repo = root.path().join("new").join("repo");
        std::fs::create_dir_all(old_repo.join(".git")).expect("git old");
        std::fs::create_dir_all(new_repo.join(".git")).expect("git new");
        let old_namespace = repo_namespace(&old_repo);
        let new_namespace = repo_namespace(&new_repo);
        let capabilities = test_capabilities();
        let target_entries = planted_entries("target", MAX_ENTRIES - 40, 10_000);
        let old_entries = planted_entries("old", MAX_ENTRIES, 0);
        seed_entries(&agent_dir, &old_namespace, &old_entries);
        seed_entries(&agent_dir, &new_namespace, &target_entries);

        assert!(
            migrate_repo_namespace_with_capabilities(&capabilities, &agent_dir, &old_repo, &new_repo)
                .expect("migrate")
        );
        let merged = store_in_with_capabilities(&capabilities, &agent_dir, &new_namespace);
        let entries = merged.list(None).expect("list");
        assert_eq!(entries.len(), MAX_ENTRIES, "merge respects the cap");
        let ids: BTreeSet<&str> = entries.iter().map(|entry| entry.id.as_str()).collect();
        for entry in &target_entries {
            assert!(ids.contains(entry.id.as_str()), "target entry lost");
        }
        // The 40 newest old entries (ts 60..=99) took the remaining slots.
        for i in 0..MAX_ENTRIES {
            let id = format!("old-{i:032}");
            assert_eq!(
                ids.contains(id.as_str()),
                i >= MAX_ENTRIES - 40,
                "retention must be newest-first within the cap"
            );
        }

        // Full target namespace: nothing is copied and the target is intact.
        let root_b = tempfile::tempdir().expect("root b");
        let agent_dir_b = root_b.path().join("agent");
        let old_repo_b = root_b.path().join("old").join("repo");
        let new_repo_b = root_b.path().join("new").join("repo");
        std::fs::create_dir_all(old_repo_b.join(".git")).expect("git old b");
        std::fs::create_dir_all(new_repo_b.join(".git")).expect("git new b");
        let target_entries_b = planted_entries("target", MAX_ENTRIES, 10_000);
        let old_entries_b = planted_entries("old", MAX_ENTRIES, 0);
        seed_entries(&agent_dir_b, &repo_namespace(&old_repo_b), &old_entries_b);
        seed_entries(&agent_dir_b, &repo_namespace(&new_repo_b), &target_entries_b);
        assert!(
            !migrate_repo_namespace_with_capabilities(&capabilities, &agent_dir_b, &old_repo_b, &new_repo_b)
                .expect("migrate b"),
            "a full target namespace cannot accept copies"
        );
        let merged_b = store_in_with_capabilities(&capabilities, &agent_dir_b, &repo_namespace(&new_repo_b));
        let entries_b = merged_b.list(None).expect("list b");
        assert_eq!(entries_b.len(), MAX_ENTRIES);
        let ids_b: BTreeSet<&str> = entries_b.iter().map(|entry| entry.id.as_str()).collect();
        for entry in &target_entries_b {
            assert!(ids_b.contains(entry.id.as_str()), "target entry lost");
        }
        assert!(
            ids_b.iter().all(|id| !id.starts_with("old-")),
            "no old entry may evict a target entry"
        );
    }

    #[test]
    fn migrate_redacts_planted_secrets_and_skips_oversize_entries() {
        let root = tempfile::tempdir().expect("root");
        let agent_dir = root.path().join("agent");
        let old_repo = root.path().join("old").join("repo");
        let new_repo = root.path().join("new").join("repo");
        std::fs::create_dir_all(old_repo.join(".git")).expect("git old");
        std::fs::create_dir_all(new_repo.join(".git")).expect("git new");
        let secret = ["sk", "-", "abcdefghijklmnop1234567890"].concat();
        let planted = vec![
            MemoryEntry {
                id: "secret-1".to_string(),
                content: format!("the token is {secret}"),
                tags: Vec::new(),
                ts: 1,
                session: "s1".to_string(),
            },
            MemoryEntry {
                id: "oversize-1".to_string(),
                content: "x".repeat(MAX_ENTRY_BYTES + 1),
                tags: Vec::new(),
                ts: 2,
                session: "s1".to_string(),
            },
            MemoryEntry {
                id: "clean-1".to_string(),
                content: "clean note".to_string(),
                tags: Vec::new(),
                ts: 3,
                session: "s1".to_string(),
            },
        ];
        let capabilities = test_capabilities();
        seed_entries(&agent_dir, &repo_namespace(&old_repo), &planted);

        assert!(
            migrate_repo_namespace_with_capabilities(&capabilities, &agent_dir, &old_repo, &new_repo)
                .expect("migrate")
        );
        let merged = store_in_with_capabilities(&capabilities, &agent_dir, &repo_namespace(&new_repo));
        let entries = merged.list(None).expect("list");
        assert_eq!(entries.len(), 2, "oversize entry is skipped");
        let copied = entries
            .iter()
            .find(|entry| entry.id == "secret-1")
            .expect("secret entry copied");
        assert!(
            !copied.content.contains(&secret),
            "planted secret must be redacted: {}",
            copied.content
        );
        assert!(copied.content.contains("the token is"), "prose preserved");
        assert!(entries.iter().any(|entry| entry.id == "clean-1"));
        // The old archive still holds the planted entries untouched.
        assert_eq!(
            store_in_with_capabilities(&capabilities, &agent_dir, &repo_namespace(&old_repo))
                .list(None)
                .expect("old list")
                .len(),
            3
        );
    }
}
