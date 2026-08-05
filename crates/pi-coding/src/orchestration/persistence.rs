//! Durable orchestration state for crash-durable child sessions.
//!
//! Stores the parent linkage, recovered agent roster (with persisted definition /
//! request / model / session path), retained job snapshots, and mailbox messages
//! in an atomic versioned sidecar beneath the child session root.
//!
//! Recovery is fail-closed and truthful: a process crash interrupts a turn, so
//! recovery parks interrupted agents and cancels interrupted jobs rather than
//! claiming exactly-once execution. Corruption, wrong-parent, path traversal,
//! and oversize inputs fail closed without mutating the in-memory registry or
//! jobs table, and the sidecar is never deleted on failure.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::runtime::{AgentSnapshot, AgentStatus, ChildSessionRequest, MailboxMessage};
use super::{JobSnapshot, JobStatus, TaskResult};

/// Sidecar schema version. Bumped on incompatible changes; recovery rejects
/// unknown future versions rather than guessing.
pub const DURABLE_STATE_VERSION: u32 = 1;
const SIDECAR_FILENAME: &str = "orchestration-state.json";
const MAX_SIDECAR_BYTES: u64 = 16 * 1024 * 1024;
const MAX_AGENTS: usize = 1024;
const MAX_JOBS: usize = 4096;
const MAX_MAILBOX_PER_AGENT: usize = 1024;
const MAX_AGENT_ID_LEN: usize = 80;
const MAX_BODY_LEN: usize = 64 * 1024;

/// Atomic versioned sidecar storing the full durable orchestration state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DurableState {
    pub version: u32,
    pub parent_session_id: String,
    pub parent_session_path: String,
    pub agents: Vec<PersistedAgent>,
    #[serde(default)]
    pub jobs: Vec<JobSnapshot>,
}

/// Persisted agent with enough material to reconstruct a `ChildSessionRequest`
/// and resume the same child JSONL transcript on revival.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PersistedAgent {
    pub snapshot: AgentSnapshot,
    pub definition: PersistedDefinition,
    pub request: PersistedRequest,
    pub session_path: Option<String>,
    #[serde(default)]
    pub mailbox: Vec<MailboxMessage>,
}

/// Persisted subset of [`AgentDefinition`] sufficient for revival validation
/// and prompt reconstruction. The live catalog is re-resolved at revival time so
/// a stale or untrusted persisted definition cannot bypass current trust rules.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PersistedDefinition {
    pub name: String,
    pub description: String,
    pub system_prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub autoload_skills: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<pi_agent::ThinkingLevel>,
    pub source: PersistedDefinitionSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    pub trusted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum PersistedDefinitionSource {
    Project,
    User,
    Bundled,
}

/// Persisted subset of [`ChildSessionRequest`] needed to reconstruct the
/// request for revival. The model is resolved fresh from the trusted catalog
/// at revival time and is stored only for diagnostics.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PersistedRequest {
    pub child_id: String,
    pub parent_id: String,
    pub depth: usize,
    pub assignment: String,
    pub system_prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_tool_names: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<pi_agent::ThinkingLevel>,
    pub max_tools_per_agent: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
}

/// Durable runtime binding: holds the canonical parent session path, canonical
/// child root, and one ordering lock spanning state capture plus atomic write.
/// Stores only paths, never the live `Session`, to avoid an `Arc` cycle.
#[derive(Clone)]
pub struct DurableRuntime {
    inner: Arc<DurableRuntimeInner>,
}

struct DurableRuntimeInner {
    parent_session_id: String,
    parent_session_path: PathBuf,
    child_root: PathBuf,
    sidecar_path: PathBuf,
    ordering_lock: Mutex<()>,
}

impl DurableRuntime {
    /// Bind to a canonical parent JSONL and its exact durable child root,
    /// `<resolved-session-root>/children/<parent-id>/`.
    pub fn new(
        parent_session_id: String,
        parent_session_path: PathBuf,
        child_root: PathBuf,
    ) -> Result<Self> {
        crate::session_store::validate_session_id(&parent_session_id)
            .context("invalid parent session id")?;
        let requested_parent = absolute_lexical(&parent_session_path)?;
        let requested_parent_dir = requested_parent
            .parent()
            .ok_or_else(|| anyhow!("parent session path has no parent"))?;
        let session_root = fs::canonicalize(requested_parent_dir).with_context(|| {
            format!(
                "canonicalizing parent session directory {}",
                requested_parent_dir.display()
            )
        })?;
        let session_root_metadata = fs::metadata(&session_root).with_context(|| {
            format!("reading parent session directory {}", session_root.display())
        })?;
        if !session_root_metadata.is_dir() {
            bail!("parent session directory is not a directory: {}", session_root.display());
        }
        let file_name = requested_parent
            .file_name()
            .ok_or_else(|| anyhow!("parent session path has no file name"))?;
        let parent_session_path = session_root.join(file_name);
        validate_parent_path_identity(&parent_session_path)?;
        let children_root = session_root.join("children");
        if children_root.exists() {
            let metadata = fs::symlink_metadata(&children_root).with_context(|| {
                format!("reading durable children root {}", children_root.display())
            })?;
            if !metadata.file_type().is_dir() {
                bail!("durable children root is not a non-symlink directory");
            }
        }
        let expected_root = children_root.join(&parent_session_id);
        let requested_root = absolute_lexical(&child_root)?;
        if requested_root != expected_root {
            bail!(
                "durable child root must be {} (got {})",
                expected_root.display(),
                requested_root.display()
            );
        }
        ensure_existing_ancestor_contained(&session_root, &requested_root)?;
        fs::create_dir_all(&requested_root).with_context(|| {
            format!("creating durable child root {}", requested_root.display())
        })?;
        let child_root = fs::canonicalize(&requested_root).with_context(|| {
            format!("canonicalizing durable child root {}", requested_root.display())
        })?;
        if child_root != requested_root {
            bail!("durable child root contains a symlink or canonical alias");
        }
        let metadata = fs::metadata(&child_root).with_context(|| {
            format!("reading durable child root {}", child_root.display())
        })?;
        if !metadata.is_dir() {
            bail!("durable child root is not a directory: {}", child_root.display());
        }
        let sidecar_path = child_root.join(SIDECAR_FILENAME);
        Ok(Self {
            inner: Arc::new(DurableRuntimeInner {
                parent_session_id,
                parent_session_path,
                child_root,
                sidecar_path,
                ordering_lock: Mutex::new(()),
            }),
        })
    }

    #[must_use]
    pub fn parent_session_id(&self) -> &str {
        &self.inner.parent_session_id
    }

    #[must_use]
    pub fn parent_session_path(&self) -> &Path {
        &self.inner.parent_session_path
    }

    #[must_use]
    pub fn child_root(&self) -> &Path {
        &self.inner.child_root
    }

    #[must_use]
    pub fn sidecar_path(&self) -> &Path {
        &self.inner.sidecar_path
    }

    /// Capture and atomically write one state while holding the same ordering
    /// primitive. This prevents an older pre-lock snapshot from overwriting a
    /// newer state written by a concurrent mutation.
    pub fn persist_with<F>(&self, capture: F) -> Result<()>
    where
        F: FnOnce() -> Result<DurableState>,
    {
        self.persist_transaction(|| capture().map(|state| ((), state)))
    }

    pub(crate) fn persist_transaction<T, F>(&self, capture: F) -> Result<T>
    where
        F: FnOnce() -> Result<(T, DurableState)>,
    {
        let _guard = self.inner.ordering_lock.lock();
        validate_parent_path_identity(&self.inner.parent_session_path)?;
        let current_root = fs::canonicalize(&self.inner.child_root).with_context(|| {
            format!("canonicalizing durable child root {}", self.inner.child_root.display())
        })?;
        if current_root != self.inner.child_root {
            bail!("durable child root changed or escaped after binding");
        }
        let (result, state) = capture()?;
        validate_state(
            &state,
            &self.inner.parent_session_id,
            &self.inner.parent_session_path,
            &self.inner.child_root,
        )?;
        write_state_atomic(&self.inner.sidecar_path, &state)?;
        Ok(result)
    }

    pub fn persist(&self, state: &DurableState) -> Result<()> {
        self.persist_with(|| Ok(state.clone()))
    }

    /// Load a sidecar when present. Missing state is represented explicitly;
    /// corruption, containment failures, and all other I/O errors fail closed.
    pub fn load_optional(&self) -> Result<Option<DurableState>> {
        let _guard = self.inner.ordering_lock.lock();
        validate_parent_path_identity(&self.inner.parent_session_path)?;
        if !self.inner.sidecar_path.try_exists().with_context(|| {
            format!("checking durable state sidecar {}", self.inner.sidecar_path.display())
        })? {
            return Ok(None);
        }
        load_and_validate_state(
            &self.inner.sidecar_path,
            &self.inner.parent_session_id,
            &self.inner.parent_session_path,
            &self.inner.child_root,
        )
        .map(Some)
    }

    pub fn load(&self) -> Result<DurableState> {
        self.load_optional()?
            .ok_or_else(|| anyhow!("durable state sidecar not found"))
    }

    pub fn canonicalize_child_session_path(&self, path: &Path) -> Result<PathBuf> {
        canonicalize_child_path(&self.inner.child_root, path)
    }

    pub fn canonicalize_artifact_path(&self, path: &Path) -> Result<PathBuf> {
        canonicalize_child_path(&self.inner.child_root, path)
    }
}

fn absolute_lexical(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

fn validate_parent_path_identity(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("parent session path has no parent"))?;
    let canonical_parent = fs::canonicalize(parent)
        .with_context(|| format!("canonicalizing parent session directory {}", parent.display()))?;
    if canonical_parent != parent {
        bail!("parent session path contains a symlink or canonical alias");
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            let canonical = fs::canonicalize(path)
                .with_context(|| format!("canonicalizing parent session path {}", path.display()))?;
            if canonical != path {
                bail!("parent session path contains a symlink or canonical alias");
            }
            Ok(())
        }
        Ok(_) => bail!("parent session path is not a regular file: {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("reading parent session path {}", path.display())),
    }
}

fn ensure_existing_ancestor_contained(root: &Path, target: &Path) -> Result<()> {
    if !target.starts_with(root) {
        bail!("durable child root escapes resolved session root");
    }
    let mut current = root.to_path_buf();
    for component in target
        .strip_prefix(root)
        .map_err(|_| anyhow!("durable child root escapes resolved session root"))?
        .components()
    {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("durable child root contains a symlink ancestor: {}", current.display());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading durable child ancestor {}", current.display()));
            }
        }
    }
    Ok(())
}

/// Canonicalize `path` and prove it is a non-symlink regular file directly
/// inside the already-canonical durable child root.
fn canonicalize_child_path(root: &Path, path: &Path) -> Result<PathBuf> {
    let resolved = if path.is_absolute() {
        absolute_lexical(path)?
    } else {
        absolute_lexical(&root.join(path))?
    };
    let metadata = fs::symlink_metadata(&resolved)
        .with_context(|| format!("reading child path {}", resolved.display()))?;
    if !metadata.file_type().is_file() {
        bail!("child path is not a non-symlink regular file: {}", resolved.display());
    }
    let canonical = fs::canonicalize(&resolved)
        .with_context(|| format!("canonicalizing child path {}", resolved.display()))?;
    if !canonical.starts_with(root) {
        bail!("child path escapes durable child root");
    }
    if canonical.parent() != Some(root) {
        bail!("child path must be directly inside the durable child root");
    }
    Ok(canonical)
}

fn validate_state(
    state: &DurableState,
    expected_parent_id: &str,
    expected_parent_path: &Path,
    child_root: &Path,
) -> Result<()> {
    if state.version != DURABLE_STATE_VERSION {
        bail!(
            "durable state version {} is unsupported (expected {})",
            state.version,
            DURABLE_STATE_VERSION
        );
    }
    if state.parent_session_id != expected_parent_id {
        bail!("durable state parent session id mismatch");
    }
    if state.parent_session_path.trim().is_empty() {
        bail!("durable state parent session path is empty");
    }
    let state_parent = absolute_lexical(Path::new(&state.parent_session_path))?;
    if state_parent != expected_parent_path {
        bail!("durable state parent session path mismatch");
    }
    validate_parent_path_identity(expected_parent_path)?;
    if state.agents.len() > MAX_AGENTS {
        bail!("durable state agents exceed maximum of {MAX_AGENTS}");
    }
    if state.jobs.len() > MAX_JOBS {
        bail!("durable state jobs exceed maximum of {MAX_JOBS}");
    }
    let mut seen_ids = BTreeSet::new();
    let mut agent_relationships = std::collections::BTreeMap::new();
    let mut seen_message_ids = BTreeSet::new();
    for agent in &state.agents {
        validate_agent_id(&agent.snapshot.id)?;
        if !seen_ids.insert(agent.snapshot.id.as_str()) {
            bail!("durable state agent id {:?} appears more than once", agent.snapshot.id);
        }
        if agent.snapshot.id == "Main" {
            bail!("durable state cannot contain the orchestration main agent");
        }
        if agent.request.child_id != agent.snapshot.id {
            bail!("durable state request child id does not match agent {:?}", agent.snapshot.id);
        }
        if agent.definition.name != agent.snapshot.agent {
            bail!("durable state definition does not match agent {:?}", agent.snapshot.id);
        }
        let parent_id = agent
            .snapshot
            .parent_id
            .as_deref()
            .ok_or_else(|| anyhow!("durable child agent {:?} has no parent", agent.snapshot.id))?;
        validate_agent_id(parent_id)
            .with_context(|| format!("invalid parent agent id {parent_id:?}"))?;
        if agent.request.parent_id != parent_id {
            bail!("durable state request parent does not match agent {:?}", agent.snapshot.id);
        }
        agent_relationships.insert(
            agent.snapshot.id.as_str(),
            (agent.snapshot.agent.as_str(), parent_id),
        );
        if agent.mailbox.len() > MAX_MAILBOX_PER_AGENT {
            bail!(
                "durable state mailbox for agent {:?} exceeds maximum of {MAX_MAILBOX_PER_AGENT}",
                agent.snapshot.id
            );
        }
        for message in &agent.mailbox {
            validate_agent_id(&message.id)?;
            validate_agent_id(&message.from)?;
            validate_agent_id(&message.to)?;
            if message.to != agent.snapshot.id {
                bail!("durable mailbox message recipient does not match agent {:?}", agent.snapshot.id);
            }
            if !seen_message_ids.insert(message.id.as_str()) {
                bail!("durable mailbox message id {:?} appears more than once", message.id);
            }
            if message.body.len() > MAX_BODY_LEN {
                bail!("durable state mailbox message body exceeds maximum length");
            }
        }
        if let Some(session_path) = agent.session_path.as_deref() {
            canonicalize_child_path(child_root, Path::new(session_path))
                .with_context(|| format!("agent {:?} session path", agent.snapshot.id))?;
        }
    }
    for (agent_id, (_, parent_id)) in &agent_relationships {
        if *parent_id != "Main" && !seen_ids.contains(parent_id) {
            bail!("durable child agent {agent_id:?} references unknown parent {parent_id:?}");
        }
    }
    let mut seen_job_ids = BTreeSet::new();
    for job in &state.jobs {
        validate_agent_id(&job.id)?;
        validate_agent_id(&job.agent_id)?;
        if !job.parent_id.is_empty() {
            validate_agent_id(&job.parent_id)?;
        }
        if !seen_job_ids.insert(job.id.as_str()) {
            bail!("durable state job id {:?} appears more than once", job.id);
        }
        if seen_ids.contains(job.id.as_str()) {
            bail!("durable state job id {:?} conflicts with an agent id", job.id);
        }
        let Some((agent_name, parent_id)) = agent_relationships.get(job.agent_id.as_str()) else {
            bail!("durable state job {:?} references unknown agent {:?}", job.id, job.agent_id);
        };
        if job.agent != *agent_name || job.parent_id != *parent_id {
            bail!("durable state job {:?} conflicts with its agent relationship", job.id);
        }
        if let Some(result) = &job.result
            && (result.id != job.agent_id || result.agent != job.agent)
        {
            bail!("durable state job {:?} result conflicts with its agent", job.id);
        }
    }
    Ok(())
}
fn load_and_validate_state(
    sidecar_path: &Path,
    expected_parent_id: &str,
    expected_parent_path: &Path,
    child_root: &Path,
) -> Result<DurableState> {
    let sidecar_path = canonicalize_child_path(child_root, sidecar_path)
        .context("validating durable state sidecar containment")?;
    let metadata = fs::metadata(&sidecar_path)
        .with_context(|| format!("reading durable state sidecar {}", sidecar_path.display()))?;
    if metadata.len() > MAX_SIDECAR_BYTES {
        bail!("durable state sidecar exceeds maximum size of {} bytes", MAX_SIDECAR_BYTES);
    }
    let bytes = fs::read(&sidecar_path)
        .with_context(|| format!("reading durable state sidecar {}", sidecar_path.display()))?;
    let state: DurableState = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing durable state sidecar {}", sidecar_path.display()))?;
    validate_state(&state, expected_parent_id, expected_parent_path, child_root)?;
    Ok(state)
}

fn write_state_atomic(path: &Path, state: &DurableState) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(state).context("serializing durable state")?;
    let serialized_len = bytes
        .len()
        .checked_add(1)
        .ok_or_else(|| anyhow!("durable state serialized size overflow"))?;
    if serialized_len as u64 > MAX_SIDECAR_BYTES {
        bail!("durable state sidecar exceeds maximum size of {} bytes", MAX_SIDECAR_BYTES);
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("durable state sidecar path has no parent"))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("creating durable state directory {}", parent.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(SIDECAR_FILENAME);
    let temporary = parent.join(format!(".{file_name}.{}.tmp", Uuid::now_v7()));
    let result = (|| -> Result<()> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("creating durable state temporary {}", temporary.display()))?;
        let mut writer = BufWriter::new(file);
        writer.write_all(&bytes).context("writing durable state")?;
        writer.write_all(b"\n").context("writing durable state newline")?;
        writer.flush().context("flushing durable state temporary")?;
        writer
            .get_ref()
            .sync_all()
            .context("syncing durable state temporary")?;
        drop(writer);
        fs::rename(&temporary, path).with_context(|| {
            format!(
                "atomically replacing durable state {} from {}",
                path.display(),
                temporary.display()
            )
        })?;
        if let Ok(directory) = File::open(parent) {
            directory
                .sync_all()
                .with_context(|| format!("syncing durable state directory {}", parent.display()))?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn validate_agent_id(id: &str) -> Result<()> {
    if id.is_empty() {
        bail!("agent id cannot be empty");
    }
    if id.len() > MAX_AGENT_ID_LEN {
        bail!("agent id must be at most {MAX_AGENT_ID_LEN} bytes");
    }
    if !id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        bail!("agent id must contain only ASCII letters, digits, '_' or '-'");
    }
    Ok(())
}

/// Transform a live agent status into its recovery truth.
///
/// Running/Queued/Idle agents recover as `Parked` (the process is gone so they
/// cannot be active). Aborted stays Aborted. Parked stays Parked.
pub fn recovery_status(status: AgentStatus) -> AgentStatus {
    match status {
        AgentStatus::Running | AgentStatus::Queued | AgentStatus::Idle => AgentStatus::Parked,
        AgentStatus::Aborted => AgentStatus::Aborted,
        AgentStatus::Parked => AgentStatus::Parked,
    }
}

/// Transform a job snapshot for recovery. Settled jobs (Completed/Failed/
/// Cancelled) are kept as-is. Unsettled jobs (Queued/Running) become Cancelled
/// with a contextual result and finished timestamp, truthfully recording that
/// the process interrupted them.
pub fn recovery_job(mut job: JobSnapshot, finished_at: u64) -> JobSnapshot {
    if job.status.is_settled() {
        return job;
    }
    job.status = JobStatus::Cancelled;
    job.finished_at = Some(finished_at);
    if job.result.is_none() {
        job.result = Some(TaskResult {
            index: 0,
            id: job.agent_id.clone(),
            agent: job.agent.clone(),
            status: AgentStatus::Aborted,
            output: String::new(),
            usage: pi_ai::Usage::default(),
            error: Some("orchestration job interrupted by process restart".to_owned()),
            artifact_ref: format!("agent://{}", job.agent_id),
            history_ref: format!("history://{}", job.agent_id),
            artifact_uri: format!("artifact://{}", job.agent_id),
        });
    }
    job
}

/// Reconstruct a `ChildSessionRequest` from persisted material and a freshly
/// resolved model. Returns `None` if the persisted definition name no longer
/// resolves to a trusted, enabled agent, or the model cannot be resolved.
pub fn reconstruct_request(
    agent: &PersistedAgent,
    resolved_model: pi_ai::Model,
    definition: &super::AgentDefinition,
    orchestration_tools: Vec<pi_agent::AgentTool>,
    max_tools_per_agent: usize,
) -> Option<ChildSessionRequest> {
    if !definition.trusted {
        return None;
    }
    if definition.name != agent.definition.name {
        return None;
    }
    Some(ChildSessionRequest {
        child_id: agent.request.child_id.clone(),
        parent_id: agent.request.parent_id.clone(),
        max_tools_per_agent,
        depth: agent.request.depth,
        definition: definition.clone(),
        assignment: agent.request.assignment.clone(),
        system_prompt: agent.request.system_prompt.clone(),
        requested_tool_names: agent.request.requested_tool_names.clone(),
        orchestration_tools,
        thinking_level: agent.request.thinking_level,
        model: resolved_model,
    })
}

/// Convert a live `AgentDefinition` into its persisted subset.
pub fn persist_definition(definition: &super::AgentDefinition) -> PersistedDefinition {
    PersistedDefinition {
        name: definition.name.clone(),
        description: definition.description.clone(),
        system_prompt: definition.system_prompt.clone(),
        tools: definition.tools.clone(),
        autoload_skills: definition.autoload_skills.clone(),
        model: definition.model.clone(),
        thinking_level: definition.thinking_level,
        source: match definition.source {
            super::AgentDefinitionSource::Project => PersistedDefinitionSource::Project,
            super::AgentDefinitionSource::User => PersistedDefinitionSource::User,
            super::AgentDefinitionSource::Bundled => PersistedDefinitionSource::Bundled,
        },
        path: definition.path.clone(),
        trusted: definition.trusted,
    }
}

/// Convert a live `ChildSessionRequest` into its persisted subset.
pub fn persist_request(request: &ChildSessionRequest) -> PersistedRequest {
    PersistedRequest {
        child_id: request.child_id.clone(),
        parent_id: request.parent_id.clone(),
        depth: request.depth,
        assignment: request.assignment.clone(),
        system_prompt: request.system_prompt.clone(),
        requested_tool_names: request.requested_tool_names.clone(),
        thinking_level: request.thinking_level,
        max_tools_per_agent: request.max_tools_per_agent,
        model_provider: Some(request.model.provider.clone()),
        model_id: Some(request.model.id.clone()),
    }
}

/// Build a `DurableState` snapshot from the live runtime state.
pub fn build_state(
    parent_session_id: &str,
    parent_session_path: &Path,
    agents: Vec<PersistedAgent>,
    jobs: Vec<JobSnapshot>,
) -> DurableState {
    DurableState {
        version: DURABLE_STATE_VERSION,
        parent_session_id: parent_session_id.to_owned(),
        parent_session_path: parent_session_path.to_string_lossy().into_owned(),
        agents,
        jobs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn parent_id() -> String {
        "parent-session-id".to_owned()
    }

    fn state_with_agents(parent_path: &Path, agents: Vec<PersistedAgent>) -> DurableState {
        DurableState {
            version: DURABLE_STATE_VERSION,
            parent_session_id: parent_id(),
            parent_session_path: parent_path.to_string_lossy().into_owned(),
            agents,
            jobs: Vec::new(),
        }
    }

    fn persisted_agent(id: &str) -> PersistedAgent {
        PersistedAgent {
            snapshot: AgentSnapshot {
                id: id.to_owned(),
                display_name: id.to_owned(),
                agent: "task".to_owned(),
                parent_id: Some("Main".to_owned()),
                status: AgentStatus::Parked,
                created_at: 1,
                last_activity: 1,
                unread: 0,
                artifact_ref: None,
                history_ref: None,
            },
            definition: PersistedDefinition {
                name: "task".to_owned(),
                description: "task".to_owned(),
                system_prompt: "prompt".to_owned(),
                tools: None,
                autoload_skills: Vec::new(),
                model: None,
                thinking_level: None,
                source: PersistedDefinitionSource::Bundled,
                path: None,
                trusted: true,
            },
            request: PersistedRequest {
                child_id: id.to_owned(),
                parent_id: "Main".to_owned(),
                depth: 1,
                assignment: "do work".to_owned(),
                system_prompt: "prompt".to_owned(),
                requested_tool_names: None,
                thinking_level: None,
                max_tools_per_agent: 16,
                model_provider: Some("test".to_owned()),
                model_id: Some("test-model".to_owned()),
            },
            session_path: None,
            mailbox: Vec::new(),
        }
    }

    fn runtime(root: &Path) -> (DurableRuntime, PathBuf) {
        let parent_path = root.join("parent.jsonl");
        fs::write(&parent_path, b"{}\n").expect("parent");
        let rt = DurableRuntime::new(
            parent_id(),
            parent_path.clone(),
            root.join("children").join(parent_id()),
        )
        .expect("durable runtime");
        (rt, parent_path)
    }

    #[test]
    fn state_round_trips_through_atomic_persist_and_load() {
        let root = tempdir().expect("root");
        let (rt, parent_path) = runtime(root.path());
        let state = state_with_agents(&parent_path, vec![persisted_agent("Alpha")]);
        rt.persist(&state).expect("persist");
        let loaded = rt.load().expect("load");
        assert_eq!(loaded, state);
    }

    #[test]
    fn wrong_parent_id_fails_without_mutating_sidecar() {
        let root = tempdir().expect("root");
        let (rt, parent_path) = runtime(root.path());
        let mut state = state_with_agents(&parent_path, vec![persisted_agent("Alpha")]);
        rt.persist(&state).expect("persist");

        state.parent_session_id = "other-parent".to_owned();
        let error = rt.persist(&state).expect_err("wrong parent rejected");
        assert!(error.to_string().contains("parent session id mismatch"));

        let loaded = rt.load().expect("original sidecar intact");
        assert_eq!(loaded.parent_session_id, parent_id());
    }

    #[test]
    fn wrong_parent_path_fails_without_mutating_sidecar() {
        let root = tempdir().expect("root");
        let (rt, parent_path) = runtime(root.path());
        let state = state_with_agents(&parent_path, vec![persisted_agent("Alpha")]);
        rt.persist(&state).expect("persist");

        let other_root = tempdir().expect("other root");
        let different_parent = other_root.path().join("different-parent.jsonl");
        fs::write(&different_parent, b"{}\n").expect("different parent");
        let rt_other = DurableRuntime::new(
            parent_id(),
            different_parent,
            other_root.path().join("children").join(parent_id()),
        )
        .expect("runtime");
        fs::copy(rt.sidecar_path(), rt_other.sidecar_path()).expect("copy sidecar");
        let error = rt_other.load().expect_err("wrong parent path rejected");
        assert!(error.to_string().contains("parent session path mismatch"));
    }

    #[test]
    fn corrupt_sidecar_fails_without_deleting() {
        let root = tempdir().expect("root");
        let (rt, parent_path) = runtime(root.path());
        let state = state_with_agents(&parent_path, vec![persisted_agent("Alpha")]);
        rt.persist(&state).expect("persist");
        fs::write(rt.sidecar_path(), b"{not valid json").expect("corrupt");
        let error = rt.load().expect_err("corrupt rejected");
        assert!(error.to_string().contains("parsing durable state"));
        assert!(rt.sidecar_path().exists(), "sidecar not deleted");
    }

    #[test]
    fn unknown_fields_rejected() {
        let root = tempdir().expect("root");
        let (rt, parent_path) = runtime(root.path());
        let state = state_with_agents(&parent_path, vec![persisted_agent("Alpha")]);
        rt.persist(&state).expect("persist");
        let mut bytes = fs::read(rt.sidecar_path()).expect("read");
        bytes.extend_from_slice(b"\n{\"unknownField\": 1}");
        fs::write(rt.sidecar_path(), &bytes).expect("inject unknown field");
        let error = rt.load().expect_err("unknown field rejected");
        assert!(error.to_string().contains("parsing durable state"));
    }

    #[test]
    fn control_char_agent_id_rejected() {
        let root = tempdir().expect("root");
        let (rt, parent_path) = runtime(root.path());
        let mut agent = persisted_agent("Alpha");
        agent.snapshot.id = "bad\u{0001}id".to_owned();
        let state = state_with_agents(&parent_path, vec![agent]);
        let error = rt.persist(&state).expect_err("control char rejected");
        assert!(error.to_string().contains("ASCII letters"));
    }

    #[test]
    fn path_traversal_session_path_rejected() {
        let root = tempdir().expect("root");
        let (rt, parent_path) = runtime(root.path());
        let mut agent = persisted_agent("Alpha");
        agent.session_path = Some("../../../etc/passwd".to_owned());
        let state = state_with_agents(&parent_path, vec![agent]);
        let error = rt.persist(&state).expect_err("traversal rejected");
        let msg = error.to_string();
        assert!(
            msg.contains("escapes durable child root")
                || msg.contains("non-symlink regular file")
                || msg.contains("agent"),
            "traversal should be rejected: {msg}"
        );
    }

    #[test]
    fn oversize_state_rejected_on_load() {
        let root = tempdir().expect("root");
        let (rt, _) = runtime(root.path());
        // Write a sidecar larger than the limit.
        let path = rt.sidecar_path();
        fs::write(&path, &vec![b' '; MAX_SIDECAR_BYTES as usize + 1]).expect("oversize");
        let error = rt.load().expect_err("oversize rejected");
        assert!(error.to_string().contains("exceeds maximum size"));
    }

    #[test]
    fn too_many_agents_rejected() {
        let root = tempdir().expect("root");
        let (rt, parent_path) = runtime(root.path());
        let agents = (0..MAX_AGENTS + 1)
            .map(|i| persisted_agent(&format!("Agent{i}")))
            .collect::<Vec<_>>();
        let state = state_with_agents(&parent_path, agents);
        let error = rt.persist(&state).expect_err("too many agents");
        assert!(error.to_string().contains("agents exceed maximum"));
    }

    #[test]
    fn recovery_status_parks_active_agents() {
        assert_eq!(recovery_status(AgentStatus::Running), AgentStatus::Parked);
        assert_eq!(recovery_status(AgentStatus::Queued), AgentStatus::Parked);
        assert_eq!(recovery_status(AgentStatus::Idle), AgentStatus::Parked);
        assert_eq!(recovery_status(AgentStatus::Aborted), AgentStatus::Aborted);
        assert_eq!(recovery_status(AgentStatus::Parked), AgentStatus::Parked);
    }

    #[test]
    fn recovery_job_cancels_unsettled_and_keeps_settled() {
        let mut running = JobSnapshot {
            id: "job-1".to_owned(),
            agent_id: "Alpha".to_owned(),
            agent: "task".to_owned(),
            parent_id: "Main".to_owned(),
            description: None,
            todo_task_id: None,
            workflow_id: None,
            workflow_generation: None,
            status: JobStatus::Running,
            created_at: 1,
            started_at: Some(2),
            finished_at: None,
            result: None,
        };
        let recovered = recovery_job(running.clone(), 100);
        assert_eq!(recovered.status, JobStatus::Cancelled);
        assert_eq!(recovered.finished_at, Some(100));
        assert!(recovered.result.as_ref().is_some_and(|r| r.error.is_some()));
        running.status = JobStatus::Completed;
        running.finished_at = Some(50);
        let settled = recovery_job(running, 100);
        assert_eq!(settled.status, JobStatus::Completed);
        assert_eq!(settled.finished_at, Some(50));
    }

    #[test]
    fn canonicalize_child_path_rejects_traversal_and_symlink() {
        let root = tempdir().expect("root");
        let (rt, _) = runtime(root.path());
        let legit = rt.child_root().join("child.jsonl");
        fs::write(&legit, b"{}").expect("write");
        let canonical = rt
            .canonicalize_child_session_path(&legit)
            .expect("legit path");
        assert_eq!(canonical, legit);

        let outside = tempdir().expect("outside");
        let escaped = outside.path().join("escaped.jsonl");
        fs::write(&escaped, b"{}").expect("write outside");
        let error = rt
            .canonicalize_child_session_path(&escaped)
            .expect_err("outside rejected");
        assert!(error.to_string().contains("escapes durable child root"));
    }

    #[test]
    fn reconstruct_request_rejects_name_mismatch() {
        let agent = persisted_agent("Alpha");
        let mut definition = super::super::AgentDefinition {
            name: "task".to_owned(),
            description: "task".to_owned(),
            system_prompt: "prompt".to_owned(),
            tools: None,
            autoload_skills: Vec::new(),
            model: None,
            thinking_level: None,
            source: super::super::AgentDefinitionSource::Bundled,
            path: None,
            trusted: true,
        };
        let model = pi_ai::Model::default();
        assert!(reconstruct_request(&agent, model.clone(), &definition, Vec::new(), 16).is_some());
        definition.name = "other".to_owned();
        assert!(reconstruct_request(&agent, model, &definition, Vec::new(), 16).is_none());
    }
}