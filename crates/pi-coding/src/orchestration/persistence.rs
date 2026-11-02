//! Durable orchestration state for crash-durable child sessions.
//!
//! Stores the parent linkage, recovered agent roster (with persisted definition /
//! request / model / session path), retained job snapshots, and mailbox messages
//! in an atomic versioned sidecar beneath the child session root.
//!
//! Recovery is fail-closed and truthful: unsettled Running/Queued jobs keep
//! their identities for revival from the durable child JSONL. Corruption,
//! wrong-parent, path traversal, and oversize inputs fail closed without
//! mutating the in-memory registry or jobs table, and the sidecar is never
//! deleted on failure.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::fs::{
    CapabilityAccess, CapabilityRootLease, CapabilityRootPurpose, EntryKind, FileMode, FsError,
    Replacement, RootCreation,
};

use super::runtime::{
    AgentSnapshot, AgentStatus, ChildSessionRequest, DELIVERED_MESSAGE_LOG_CAP,
    MAX_MESSAGE_BODY_BYTES, MailboxMessage,
};
use super::{JobSnapshot, JobStatus, TaskResult};
use crate::sandbox::DurablePolicyState;

/// Sidecar schema version. Bumped on incompatible changes; recovery rejects
/// unknown future versions rather than guessing.
pub const DURABLE_STATE_VERSION: u32 = 3;
const LEGACY_DURABLE_STATE_VERSION: u32 = 2;
const SIDECAR_FILENAME: &str = "orchestration-state.json";
const MAX_SIDECAR_BYTES: u64 = 16 * 1024 * 1024;
const MAX_AGENTS: usize = 1024;
const MAX_JOBS: usize = 4096;
const MAX_MAILBOX_PER_AGENT: usize = 1024;
const MAX_AGENT_ID_LEN: usize = 80;

/// Atomic versioned sidecar storing the full durable orchestration state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DurableState {
    pub version: u32,
    pub parent_session_id: String,
    pub parent_session_path: String,
    pub group_id: String,
    pub durable_policy: DurablePolicyState,
    pub agents: Vec<PersistedAgent>,
    /// Mailbox for the configured orchestration root agent. The root is not a
    /// child and therefore is not represented in `agents`, but child replies
    /// committed before the parent session can consume them must survive restart.
    #[serde(default)]
    pub main_mailbox: Vec<MailboxMessage>,
    /// Bounded, newest-last history of accepted deliveries, independent of mailbox consumption.
    #[serde(default)]
    pub delivered_messages: Vec<MailboxMessage>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disallowed_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_ceiling: Option<super::CapabilityCeiling>,
    pub source: PersistedDefinitionSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    pub trusted: bool,
    #[serde(default)]
    pub kind: super::AgentDefinitionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub personality: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soft_budget: Option<super::JobSoftBudget>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub knowledge_roots: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub user_proxy: bool,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_mode: Option<String>,
}

/// Durable runtime binding: retains the parent session directory and the exact
/// per-session child directory leases for the full runtime lifetime.
#[derive(Clone)]
pub struct DurableRuntime {
    inner: Arc<DurableRuntimeInner>,
}

struct DurableRuntimeInner {
    parent_session_id: String,
    main_agent_id: String,
    parent_session_path: PathBuf,
    parent_directory: CapabilityRootLease,
    parent_relative: PathBuf,
    child_root: CapabilityRootLease,
    ordering_lock: Mutex<()>,
}

impl DurableRuntime {
    pub fn new_with_main_agent_id(
        parent_session_id: String,
        parent_session_path: PathBuf,
        parent_directory: CapabilityRootLease,
        parent_relative: PathBuf,
        main_agent_id: String,
    ) -> Result<Self> {
        crate::session_store::validate_session_id(&parent_session_id)
            .context("invalid parent session id")?;
        validate_agent_id(&main_agent_id).context("invalid orchestration main agent id")?;
        if parent_directory.purpose() != CapabilityRootPurpose::SessionStore
            || parent_directory.access() != CapabilityAccess::PrivateReadWrite
        {
            bail!("durable orchestration requires a private session-store capability");
        }
        validate_direct_file_relative(&parent_relative)?;
        let expected_parent_path = parent_directory.display_root().join(&parent_relative);
        if absolute_lexical(&parent_session_path)? != expected_parent_path {
            bail!("parent session display path does not match retained authority");
        }
        validate_parent_file_authority(&parent_directory, &parent_relative)?;
        let child_relative = Path::new("children").join(&parent_session_id);
        let child_root = parent_directory
            .lease_directory(
                &child_relative,
                CapabilityAccess::PrivateReadWrite,
                RootCreation::CreateDirectory,
            )
            .context("opening durable child-session capability")?;
        Ok(Self {
            inner: Arc::new(DurableRuntimeInner {
                parent_session_id,
                main_agent_id,
                parent_session_path: expected_parent_path,
                parent_directory,
                parent_relative,
                child_root,
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
        self.inner.child_root.display_root()
    }

    #[must_use]
    pub fn sidecar_path(&self) -> PathBuf {
        self.inner.child_root.display_root().join(SIDECAR_FILENAME)
    }

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
        validate_parent_file_authority(&self.inner.parent_directory, &self.inner.parent_relative)?;
        let (result, state) = capture()?;
        validate_state(
            &state,
            &self.inner.parent_session_id,
            &self.inner.parent_session_path,
            &self.inner.child_root,
            &self.inner.main_agent_id,
        )?;
        write_state_atomic(&self.inner.child_root, &state)?;
        Ok(result)
    }

    pub fn persist(&self, state: &DurableState) -> Result<()> {
        self.persist_with(|| Ok(state.clone()))
    }

    pub fn load_optional(&self) -> Result<Option<DurableState>> {
        let _guard = self.inner.ordering_lock.lock();
        validate_parent_file_authority(&self.inner.parent_directory, &self.inner.parent_relative)?;
        match self.inner.child_root.metadata(Path::new(SIDECAR_FILENAME)) {
            Ok(metadata) if metadata.identity.kind() == EntryKind::RegularFile => {}
            Ok(_) => bail!("durable state sidecar is not a regular file"),
            Err(error) if is_missing(&error) => return Ok(None),
            Err(error) => return Err(error).context("checking durable state sidecar"),
        }
        load_and_validate_state(
            &self.inner.child_root,
            &self.inner.parent_session_id,
            &self.inner.parent_session_path,
            &self.inner.main_agent_id,
        )
    }

    pub fn load(&self) -> Result<DurableState> {
        self.load_optional()?
            .ok_or_else(|| anyhow!("durable state sidecar not found"))
    }

    pub(crate) fn parent_file(&self) -> Result<crate::fs::CapabilityFile> {
        validate_direct_file_relative(&self.inner.parent_relative)?;
        self.inner
            .parent_directory
            .open_file(&self.inner.parent_relative)
            .context("opening parent session through retained authority")
    }

    pub(crate) fn child_file(&self, path: &Path) -> Result<crate::fs::CapabilityFile> {
        let relative = child_relative(&self.inner.child_root, path)?;
        self.inner
            .child_root
            .open_file(&relative)
            .context("opening child session through retained authority")
    }

    pub(crate) fn child_file_authority(
        &self,
        path: &Path,
    ) -> Result<(CapabilityRootLease, PathBuf)> {
        let file = self.child_file(path)?;
        Ok(file.authority())
    }

    pub(crate) fn child_root_authority(&self) -> CapabilityRootLease {
        self.inner.child_root.clone()
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

fn validate_direct_file_relative(relative: &Path) -> Result<()> {
    if relative
        .parent()
        .is_some_and(|parent| !parent.as_os_str().is_empty())
        || relative.file_name().is_none()
    {
        bail!("durable session file must be directly under its retained directory");
    }
    Ok(())
}

fn validate_parent_file_authority(lease: &CapabilityRootLease, relative: &Path) -> Result<()> {
    validate_direct_file_relative(relative)?;
    match lease.metadata(relative) {
        Ok(metadata) if metadata.identity.kind() == EntryKind::RegularFile => Ok(()),
        Ok(_) => bail!("parent session path is not a regular file"),
        Err(error) if is_missing(&error) => Ok(()),
        Err(error) => Err(error).context("reading parent session through retained authority"),
    }
}

fn child_relative(root: &CapabilityRootLease, path: &Path) -> Result<PathBuf> {
    let relative = if path.is_absolute() {
        path.strip_prefix(root.display_root())
            .map_err(|_| anyhow!("child path escapes durable child root"))?
            .to_path_buf()
    } else {
        path.to_path_buf()
    };
    validate_direct_file_relative(&relative)?;
    Ok(relative)
}

fn validate_child_file(root: &CapabilityRootLease, path: &Path) -> Result<PathBuf> {
    let relative = child_relative(root, path)?;
    let metadata = root
        .metadata(&relative)
        .context("reading child path through retained authority")?;
    if metadata.identity.kind() != EntryKind::RegularFile {
        bail!("child path is not a regular file");
    }
    Ok(root.display_root().join(relative))
}

fn is_missing(error: &FsError) -> bool {
    matches!(
        error,
        FsError::System {
            kind: std::io::ErrorKind::NotFound,
            ..
        }
    )
}

fn validate_group_id(group_id: &str) -> Result<()> {
    let parsed =
        Uuid::parse_str(group_id).map_err(|_| anyhow!("durable state group id is invalid"))?;
    if parsed.is_nil() {
        bail!("durable state group id is invalid");
    }
    Ok(())
}

fn legacy_group_id(parent_session_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"pi-orchestration-group\0");
    digest.update(parent_session_id.as_bytes());
    let hash = digest.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hash[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes).to_string()
}

fn validate_state(
    state: &DurableState,
    expected_parent_id: &str,
    expected_parent_path: &Path,
    child_root: &CapabilityRootLease,
    main_agent_id: &str,
) -> Result<()> {
    if state.version != DURABLE_STATE_VERSION {
        bail!(
            "durable state version {} is unsupported (expected {})",
            state.version,
            DURABLE_STATE_VERSION
        );
    }
    validate_group_id(&state.group_id)?;
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
    if state.agents.len() > MAX_AGENTS {
        bail!("durable state agents exceed maximum of {MAX_AGENTS}");
    }
    if state.jobs.len() > MAX_JOBS {
        bail!("durable state jobs exceed maximum of {MAX_JOBS}");
    }
    if state.main_mailbox.len() > MAX_MAILBOX_PER_AGENT {
        bail!(
            "durable state mailbox for main agent {:?} exceeds maximum of {MAX_MAILBOX_PER_AGENT}",
            main_agent_id
        );
    }
    if state.delivered_messages.len() > DELIVERED_MESSAGE_LOG_CAP {
        bail!("durable delivered-message log exceeds maximum of {DELIVERED_MESSAGE_LOG_CAP}");
    }
    let mut seen_ids = BTreeSet::new();
    let mut agent_relationships = std::collections::BTreeMap::new();
    let mut seen_message_ids = BTreeSet::new();
    for message in &state.main_mailbox {
        validate_agent_id(&message.id)?;
        validate_agent_id(&message.from)?;
        validate_agent_id(&message.to)?;
        if message.to != main_agent_id {
            bail!("durable main mailbox message recipient does not match main agent");
        }
        if !seen_message_ids.insert(message.id.as_str()) {
            bail!(
                "durable mailbox message id {:?} appears more than once",
                message.id
            );
        }
        if message.body.len() > MAX_MESSAGE_BODY_BYTES {
            bail!("durable main mailbox message body exceeds maximum length");
        }
    }
    for agent in &state.agents {
        validate_agent_id(&agent.snapshot.id)?;
        if !seen_ids.insert(agent.snapshot.id.as_str()) {
            bail!(
                "durable state agent id {:?} appears more than once",
                agent.snapshot.id
            );
        }
        if agent.snapshot.id == main_agent_id {
            bail!("durable state cannot contain the orchestration main agent");
        }
        if agent.request.child_id != agent.snapshot.id {
            bail!(
                "durable state request child id does not match agent {:?}",
                agent.snapshot.id
            );
        }
        if agent.definition.name != agent.snapshot.agent {
            bail!(
                "durable state definition does not match agent {:?}",
                agent.snapshot.id
            );
        }
        let parent_id =
            agent.snapshot.parent_id.as_deref().ok_or_else(|| {
                anyhow!("durable child agent {:?} has no parent", agent.snapshot.id)
            })?;
        validate_agent_id(parent_id)
            .with_context(|| format!("invalid parent agent id {parent_id:?}"))?;
        if agent.request.parent_id != parent_id {
            bail!(
                "durable state request parent does not match agent {:?}",
                agent.snapshot.id
            );
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
                bail!(
                    "durable mailbox message recipient does not match agent {:?}",
                    agent.snapshot.id
                );
            }
            if !seen_message_ids.insert(message.id.as_str()) {
                bail!(
                    "durable mailbox message id {:?} appears more than once",
                    message.id
                );
            }
            if message.body.len() > MAX_MESSAGE_BODY_BYTES {
                bail!("durable state mailbox message body exceeds maximum length");
            }
        }
        if let Some(session_path) = agent.session_path.as_deref() {
            validate_child_file(child_root, Path::new(session_path))
                .with_context(|| format!("agent {:?} session path", agent.snapshot.id))?;
        }
    }
    let mut seen_delivered_ids = BTreeSet::new();
    for message in &state.delivered_messages {
        validate_agent_id(&message.id)?;
        validate_agent_id(&message.from)?;
        validate_agent_id(&message.to)?;
        if !seen_delivered_ids.insert(message.id.as_str()) {
            bail!(
                "durable delivered-message id {:?} appears more than once",
                message.id
            );
        }
        if message.body.len() > MAX_MESSAGE_BODY_BYTES {
            bail!("durable delivered-message body exceeds maximum length");
        }
    }
    for (agent_id, (_, parent_id)) in &agent_relationships {
        if *parent_id != main_agent_id && !seen_ids.contains(parent_id) {
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
            bail!(
                "durable state job id {:?} conflicts with an agent id",
                job.id
            );
        }
        let Some((agent_name, parent_id)) = agent_relationships.get(job.agent_id.as_str()) else {
            bail!(
                "durable state job {:?} references unknown agent {:?}",
                job.id,
                job.agent_id
            );
        };
        if job.agent != *agent_name || job.parent_id != *parent_id {
            bail!(
                "durable state job {:?} conflicts with its agent relationship",
                job.id
            );
        }
        if let Some(result) = &job.result
            && (result.id != job.agent_id || result.agent != job.agent)
        {
            bail!(
                "durable state job {:?} result conflicts with its agent",
                job.id
            );
        }
    }
    Ok(())
}
/// Minimal envelope read before strict deserialization so the version can be
/// probed from sidecars whose remaining shape is unknown (e.g. future versions).
/// Unknown fields are intentionally ignored here: the probe must tolerate fields
/// the envelope does not declare.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StateEnvelope {
    version: u32,
}

/// Exact historical v1 sidecar schema. It lacks `durablePolicy`, so it can be
/// ownership-validated but never restored as authority.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyDurableStateV1 {
    version: u32,
    parent_session_id: String,
    parent_session_path: String,
    agents: Vec<PersistedAgent>,
    #[serde(default)]
    main_mailbox: Vec<MailboxMessage>,
    #[serde(default)]
    delivered_messages: Vec<MailboxMessage>,
    #[serde(default)]
    jobs: Vec<JobSnapshot>,
}

/// Exact historical v2 sidecar schema. The execution state is authoritative,
/// but `groupId` did not exist; migration derives it once from parent identity.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyDurableStateV2 {
    version: u32,
    parent_session_id: String,
    parent_session_path: String,
    durable_policy: DurablePolicyState,
    agents: Vec<PersistedAgent>,
    #[serde(default)]
    main_mailbox: Vec<MailboxMessage>,
    #[serde(default)]
    delivered_messages: Vec<MailboxMessage>,
    #[serde(default)]
    jobs: Vec<JobSnapshot>,
}

impl LegacyDurableStateV2 {
    fn migrate(self) -> DurableState {
        DurableState {
            version: DURABLE_STATE_VERSION,
            group_id: legacy_group_id(&self.parent_session_id),
            parent_session_id: self.parent_session_id,
            parent_session_path: self.parent_session_path,
            durable_policy: self.durable_policy,
            agents: self.agents,
            main_mailbox: self.main_mailbox,
            delivered_messages: self.delivered_messages,
            jobs: self.jobs,
        }
    }
}

/// Validate a legacy v1 sidecar's ownership and structural bounds before it is
/// discarded. The v1 payload is never reconstructed (so its agents/jobs/mailbox
/// are non-authoritative), but a sidecar belonging to a different session must
/// fail closed rather than being treated as absent and overwritten, and the
/// same structural bounds as [`validate_state`] apply to anything we accept as
/// "legacy and discardable".
fn validate_legacy_v1(
    state: &LegacyDurableStateV1,
    expected_parent_id: &str,
    expected_parent_path: &Path,
    main_agent_id: &str,
) -> Result<()> {
    if state.version != 1 {
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
    if state.agents.len() > MAX_AGENTS {
        bail!("durable state agents exceed maximum of {MAX_AGENTS}");
    }
    if state.jobs.len() > MAX_JOBS {
        bail!("durable state jobs exceed maximum of {MAX_JOBS}");
    }
    if state.main_mailbox.len() > MAX_MAILBOX_PER_AGENT {
        bail!(
            "durable state mailbox for main agent {:?} exceeds maximum of {MAX_MAILBOX_PER_AGENT}",
            main_agent_id
        );
    }
    for agent in &state.agents {
        validate_agent_id(&agent.snapshot.id)?;
    }
    Ok(())
}

fn load_and_validate_state(
    child_root: &CapabilityRootLease,
    expected_parent_id: &str,
    expected_parent_path: &Path,
    main_agent_id: &str,
) -> Result<Option<DurableState>> {
    let bytes = child_root
        .read_bounded(Path::new(SIDECAR_FILENAME), MAX_SIDECAR_BYTES)
        .context("reading durable state sidecar")?;
    // Probe the version before strict deserialization so historical schemas
    // can be migrated without accepting fields outside their exact shape.
    let envelope: StateEnvelope =
        serde_json::from_slice(&bytes).context("parsing durable state sidecar")?;
    let state = match envelope.version {
        1 => {
            let legacy: LegacyDurableStateV1 =
                serde_json::from_slice(&bytes).context("parsing durable state sidecar")?;
            validate_legacy_v1(
                &legacy,
                expected_parent_id,
                expected_parent_path,
                main_agent_id,
            )?;
            return Ok(None);
        }
        LEGACY_DURABLE_STATE_VERSION => {
            let legacy: LegacyDurableStateV2 =
                serde_json::from_slice(&bytes).context("parsing durable state sidecar")?;
            legacy.migrate()
        }
        DURABLE_STATE_VERSION => {
            serde_json::from_slice(&bytes).context("parsing durable state sidecar")?
        }
        unsupported => {
            bail!(
                "durable state version {unsupported} is unsupported (expected {DURABLE_STATE_VERSION})"
            );
        }
    };
    validate_state(
        &state,
        expected_parent_id,
        expected_parent_path,
        child_root,
        main_agent_id,
    )?;
    Ok(Some(state))
}

fn write_state_atomic(root: &CapabilityRootLease, state: &DurableState) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(state).context("serializing durable state")?;
    bytes.push(b'\n');
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_SIDECAR_BYTES {
        bail!(
            "durable state sidecar exceeds maximum size of {} bytes",
            MAX_SIDECAR_BYTES
        );
    }
    let relative = PathBuf::from(SIDECAR_FILENAME);
    let expected = match root.metadata(&relative) {
        Ok(metadata) if metadata.identity.kind() == EntryKind::RegularFile => {
            Some(metadata.identity)
        }
        Ok(_) => bail!("durable state sidecar is not a regular file"),
        Err(error) if is_missing(&error) => None,
        Err(error) => return Err(error).context("reading durable state sidecar identity"),
    };
    root.replace_file(Replacement {
        relative,
        expected,
        bytes,
        mode: FileMode::Private,
    })
    .context("atomically replacing durable state sidecar")?;
    Ok(())
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
/// Running/Queued agents recover as `Queued` — the process is gone so they
/// cannot be actively running, but they are pending revival from their durable
/// child JSONL and must NOT be parked or cancelled. Idle agents recover as
/// `Parked` (they had completed their work). Aborted stays Aborted. Parked
/// stays Parked.
pub fn recovery_status(status: AgentStatus) -> AgentStatus {
    match status {
        AgentStatus::Running | AgentStatus::Queued => AgentStatus::Queued,
        AgentStatus::Idle => AgentStatus::Parked,
        AgentStatus::Aborted => AgentStatus::Aborted,
        AgentStatus::Parked => AgentStatus::Parked,
    }
}

/// Transform a job snapshot for recovery. Settled jobs (Completed/Failed/
/// Cancelled) are kept as-is. Unsettled jobs (Queued/Running) are kept in
/// their current status — they are pending revival from their durable child
/// JSONL and must NOT be cancelled. The runtime spawns revival tasks after
/// installing the recovered state.
pub fn recovery_job(job: JobSnapshot, _finished_at: u64) -> JobSnapshot {
    recovery_job_with_persona(job, _finished_at, None)
}

pub(crate) fn recovery_job_with_persona(
    mut job: JobSnapshot,
    _finished_at: u64,
    _persona: Option<&str>,
) -> JobSnapshot {
    if job.status.is_settled() {
        return job;
    }
    // Unsettled jobs keep their status (Running/Queued) for revival.
    // Do NOT cancel, do NOT set finished_at, do NOT synthesize an error result.
    // The revival task reopens the child JSONL and continues the turn.
    job.started_at = Some(job.started_at.unwrap_or(_finished_at));
    job
}

/// Reconstruct a `ChildSessionRequest` from persisted execution identity and
/// freshly resolved policy. Returns `None` if the live definition no longer
/// exactly matches the persisted definition or is not trusted.
pub fn reconstruct_request(
    agent: &PersistedAgent,
    resolved_model: pi_ai::Model,
    resolved_model_sequence: Vec<super::ResolvedAgentModelCandidate>,
    resolved_model_selected_index: usize,
    definition: &super::AgentDefinition,
    system_prompt: String,
    requested_tool_names: Option<Vec<String>>,
    orchestration_tools: Vec<pi_agent::AgentTool>,
    max_tools_per_agent: usize,
    persona_skill_store: Option<crate::PersonaSkillStore>,
    persona_skills: std::sync::Arc<Vec<crate::PersonaSkill>>,
    shared_skills: std::sync::Arc<Vec<crate::Skill>>,
) -> Option<ChildSessionRequest> {
    if !definition.trusted || persist_definition(definition) != agent.definition {
        return None;
    }
    Some(ChildSessionRequest {
        child_id: agent.request.child_id.clone(),
        parent_id: agent.request.parent_id.clone(),
        max_tools_per_agent,
        depth: agent.request.depth,
        definition: definition.clone(),
        assignment: agent.request.assignment.clone(),
        system_prompt,
        requested_tool_names,
        orchestration_tools,
        persona_skill_store,
        persona_skills,
        shared_skills,
        thinking_level: agent.request.thinking_level,
        model: resolved_model,
        resolved_model_sequence,
        resolved_model_selected_index,
        output_schema: agent.request.output_schema.clone(),
        schema_mode: agent.request.schema_mode.clone(),
        yield_state: std::sync::Arc::new(super::YieldState::default()),
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
        max_turns: definition.max_turns,
        max_tool_calls: definition.max_tool_calls,
        timeout_secs: definition.timeout_secs,
        disallowed_tools: definition.disallowed_tools.clone(),
        capability_ceiling: definition.capability_ceiling,
        source: match definition.source {
            super::AgentDefinitionSource::Project => PersistedDefinitionSource::Project,
            super::AgentDefinitionSource::User => PersistedDefinitionSource::User,
            super::AgentDefinitionSource::Bundled => PersistedDefinitionSource::Bundled,
        },
        path: definition.path.clone(),
        trusted: definition.trusted,
        kind: definition.kind,
        personality: definition.personality.clone(),
        soft_budget: definition.soft_budget,
        knowledge_roots: definition.knowledge_roots.clone(),
        user_proxy: definition.user_proxy,
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
        output_schema: request.output_schema.clone(),
        schema_mode: request.schema_mode.clone(),
    }
}

/// Build a `DurableState` snapshot from the live runtime state.
pub fn build_state(
    group_id: &str,
    parent_session_id: &str,
    parent_session_path: &Path,
    durable_policy: DurablePolicyState,
    main_mailbox: Vec<MailboxMessage>,
    delivered_messages: Vec<MailboxMessage>,
    agents: Vec<PersistedAgent>,
    jobs: Vec<JobSnapshot>,
) -> DurableState {
    DurableState {
        version: DURABLE_STATE_VERSION,
        group_id: group_id.to_owned(),
        parent_session_id: parent_session_id.to_owned(),
        parent_session_path: parent_session_path.to_string_lossy().into_owned(),
        durable_policy,
        main_mailbox,
        delivered_messages,
        agents,
        jobs,
    }
}

#[cfg(test)]
mod tests {
    fn durable_policy() -> DurablePolicyState {
        crate::sandbox::generation::test_durable_policy_state()
    }
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn parent_id() -> String {
        "parent-session-id".to_owned()
    }

    fn runtime_with_main(root: &Path, main_agent_id: &str) -> (DurableRuntime, PathBuf) {
        let parent_path = root.join("parent.jsonl");
        fs::write(&parent_path, b"{}\n").expect("parent");
        let capabilities = crate::fs::CapabilityFs::new().expect("capability filesystem");
        let lease = capabilities
            .open_root(
                CapabilityRootPurpose::SessionStore,
                pi_sandbox::AbsolutePath::parse(root.to_path_buf()).expect("absolute session root"),
                CapabilityAccess::PrivateReadWrite,
                RootCreation::Existing,
            )
            .expect("session lease");
        let runtime = DurableRuntime::new_with_main_agent_id(
            parent_id(),
            parent_path.clone(),
            lease,
            PathBuf::from("parent.jsonl"),
            main_agent_id.to_owned(),
        )
        .expect("durable runtime");
        (runtime, parent_path)
    }

    fn runtime(root: &Path) -> (DurableRuntime, PathBuf) {
        runtime_with_main(root, "Main")
    }

    fn state_with_agents(parent_path: &Path, agents: Vec<PersistedAgent>) -> DurableState {
        DurableState {
            version: DURABLE_STATE_VERSION,
            parent_session_id: parent_id(),
            group_id: Uuid::now_v7().to_string(),
            durable_policy: durable_policy(),
            parent_session_path: parent_path.to_string_lossy().into_owned(),
            main_mailbox: Vec::new(),
            delivered_messages: Vec::new(),
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
                max_turns: None,
                max_tool_calls: None,
                timeout_secs: None,
                disallowed_tools: Vec::new(),
                capability_ceiling: None,
                source: PersistedDefinitionSource::Bundled,
                path: None,
                trusted: true,
                kind: super::super::AgentDefinitionKind::Agent,
                personality: None,
                soft_budget: None,
                knowledge_roots: Vec::new(),
                user_proxy: false,
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
                output_schema: None,
                schema_mode: None,
            },
            session_path: None,
            mailbox: Vec::new(),
        }
    }

    #[test]
    fn configured_main_identity_round_trips_root_relationships_and_mailbox() {
        let root = tempdir().expect("root");
        let (runtime, parent_path) = runtime_with_main(root.path(), "Coordinator");
        let mut agent = persisted_agent("Alpha");
        agent.snapshot.parent_id = Some("Coordinator".to_owned());
        agent.request.parent_id = "Coordinator".to_owned();
        let mut state = state_with_agents(&parent_path, vec![agent]);
        state.main_mailbox.push(MailboxMessage {
            id: "message-1".to_owned(),
            from: "Alpha".to_owned(),
            to: "Coordinator".to_owned(),
            body: "durable reply".to_owned(),
            timestamp: 1,
            reply_to: None,
        });

        runtime
            .persist(&state)
            .expect("persist configured-main state");
        assert_eq!(runtime.load().expect("load configured-main state"), state);
    }

    #[test]
    fn configured_main_identity_rejects_wrong_mailbox_recipient() {
        let root = tempdir().expect("root");
        let (runtime, parent_path) = runtime_with_main(root.path(), "Coordinator");
        let mut state = state_with_agents(&parent_path, Vec::new());
        state.main_mailbox.push(MailboxMessage {
            id: "message-1".to_owned(),
            from: "Alpha".to_owned(),
            to: "Main".to_owned(),
            body: "misrouted reply".to_owned(),
            timestamp: 1,
            reply_to: None,
        });

        let error = runtime
            .persist(&state)
            .expect_err("wrong main mailbox recipient must fail closed");
        assert!(error.to_string().contains("recipient"), "{error:#}");
    }

    #[test]
    fn configured_main_identity_rejects_root_as_child() {
        let root = tempdir().expect("root");
        let (runtime, parent_path) = runtime_with_main(root.path(), "Coordinator");
        let mut root_agent = persisted_agent("Coordinator");
        root_agent.snapshot.parent_id = Some("Coordinator".to_owned());
        root_agent.request.parent_id = "Coordinator".to_owned();
        let state = state_with_agents(&parent_path, vec![root_agent]);

        let error = runtime
            .persist(&state)
            .expect_err("configured main identity cannot be persisted as a child");
        assert!(error.to_string().contains("main agent"), "{error:#}");
    }

    #[test]
    fn configured_main_identity_rejects_unknown_parent() {
        let root = tempdir().expect("root");
        let (runtime, parent_path) = runtime_with_main(root.path(), "Coordinator");
        let mut orphan = persisted_agent("Alpha");
        orphan.snapshot.parent_id = Some("Missing".to_owned());
        orphan.request.parent_id = "Missing".to_owned();
        let state = state_with_agents(&parent_path, vec![orphan]);

        let error = runtime
            .persist(&state)
            .expect_err("unknown non-root parent must fail closed");
        assert!(error.to_string().contains("unknown parent"), "{error:#}");
    }

    #[test]
    fn durable_policy_round_trips_through_atomic_persist_and_load() {
        let root = tempdir().expect("root");
        let (rt, parent_path) = runtime(root.path());
        let state = state_with_agents(&parent_path, vec![persisted_agent("Alpha")]);
        rt.persist(&state).expect("persist");
        let loaded = rt.load().expect("load");
        assert_eq!(loaded, state);
        assert_eq!(loaded.durable_policy, durable_policy());
    }

    #[test]
    fn previous_durable_state_version_is_rejected_before_recovery() {
        let root = tempdir().expect("root");
        let (rt, parent_path) = runtime(root.path());
        let state = state_with_agents(&parent_path, vec![persisted_agent("Alpha")]);
        rt.persist(&state).expect("persist current schema");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(rt.sidecar_path()).expect("read sidecar"))
                .expect("decode sidecar");
        value["version"] = serde_json::json!(DURABLE_STATE_VERSION + 1);
        fs::write(
            rt.sidecar_path(),
            serde_json::to_vec_pretty(&value).expect("encode future schema"),
        )
        .expect("write future schema");
        let error = rt
            .load()
            .expect_err("future schema rejected on recovery load");
        assert!(error.to_string().contains("unsupported"));
    }

    // Real historical v1 sidecar shapes. v1 predates `durablePolicy` and cannot
    // deserialize into `DurableState`, so recovery must probe the version field
    // and treat valid v1 as unavailable orchestration state (Ok(None)) rather
    // than failing to parse. The `__PARENT_PATH__` placeholder is replaced with
    // the actual parent path at test time; v1 never reaches `validate_state`, so
    // the path is present for realism but not validated.
    const V1_SIDECAR_WITHOUT_MAIN_MAILBOX_TEMPLATE: &str = r#"{
      "version": 1,
      "parentSessionId": "parent-session-id",
      "parentSessionPath": "__PARENT_PATH__",
      "agents": []
    }"#;
    const V1_SIDECAR_WITH_MAIN_MAILBOX_TEMPLATE: &str = r#"{
      "version": 1,
      "parentSessionId": "parent-session-id",
      "parentSessionPath": "__PARENT_PATH__",
      "mainMailbox": [
        {
          "id": "message-1",
          "from": "Alpha",
          "to": "Main",
          "body": "late durable reply",
          "timestamp": 1,
          "replyTo": null
        }
      ],
      "agents": []
    }"#;
    const MALFORMED_JSON: &str = "{not valid json";
    // A v2-versioned sidecar missing the required `durablePolicy` field: the
    // version probe succeeds, but strict v2 deserialization must fail closed
    // with the parse context preserved.
    const MALFORMED_V2_TEMPLATE: &str = r#"{
      "version": 2,
      "parentSessionId": "parent-session-id",
      "parentSessionPath": "__PARENT_PATH__",
      "agents": []
    }"#;
    const FUTURE_VERSION_SIDECAR: &str = r#"{
      "version": 4,
      "parentSessionId": "parent-session-id",
      "parentSessionPath": "__PARENT_PATH__",
      "groupId": "018f5a8c-1234-7000-8000-000000000001",
      "durablePolicy": {},
      "agents": []
    }"#;
    const ZERO_VERSION_SIDECAR: &str = r#"{
      "version": 0,
      "parentSessionId": "parent-session-id",
      "parentSessionPath": "__PARENT_PATH__",
      "agents": []
    }"#;

    fn write_fixture(rt: &DurableRuntime, template: &str, parent_path: &Path) {
        let rendered = template.replace("__PARENT_PATH__", &parent_path.to_string_lossy());
        fs::write(rt.sidecar_path(), rendered.as_bytes()).expect("write fixture sidecar");
    }

    #[test]
    fn legacy_v1_without_main_mailbox_loads_as_none_then_persists_fresh_v3() {
        let root = tempdir().expect("root");
        let (rt, parent_path) = runtime(root.path());
        write_fixture(&rt, V1_SIDECAR_WITHOUT_MAIN_MAILBOX_TEMPLATE, &parent_path);

        assert_eq!(
            rt.load_optional()
                .expect("legacy v1 is available-but-unusable state"),
            None
        );
        // Optional load must not mutate or delete the v1 sidecar.
        assert!(rt.sidecar_path().exists(), "v1 sidecar not deleted");
        // The strict load path still refuses to fabricate/migrate v1 authority.
        let error = rt
            .load()
            .expect_err("strict load of legacy v1 must not fabricate v3");
        assert!(error.to_string().contains("sidecar not found"), "{error:#}");
        // The existing `None => persist_state()` path writes fresh v3 state.
        let state = state_with_agents(&parent_path, vec![persisted_agent("Alpha")]);
        rt.persist(&state).expect("fresh v3 persist over legacy v1");
        assert_eq!(rt.load().expect("load fresh v3"), state);
    }

    #[test]
    fn legacy_v1_with_main_mailbox_loads_as_none() {
        let root = tempdir().expect("root");
        let (rt, parent_path) = runtime(root.path());
        write_fixture(&rt, V1_SIDECAR_WITH_MAIN_MAILBOX_TEMPLATE, &parent_path);

        assert_eq!(
            rt.load_optional()
                .expect("legacy v1 with main mailbox is unavailable state"),
            None
        );
        assert!(rt.sidecar_path().exists(), "v1 sidecar not deleted");
    }

    #[test]
    fn malformed_json_fails_closed_parsing_without_deleting() {
        let root = tempdir().expect("root");
        let (rt, parent_path) = runtime(root.path());
        fs::write(rt.sidecar_path(), MALFORMED_JSON).expect("write malformed json");
        let error = rt
            .load_optional()
            .expect_err("malformed json must fail closed");
        assert!(
            error.to_string().contains("parsing durable state"),
            "{error:#}"
        );
        assert!(rt.sidecar_path().exists(), "sidecar not deleted");
    }

    #[test]
    fn malformed_v2_fails_closed_parsing_without_deleting() {
        let root = tempdir().expect("root");
        let (rt, parent_path) = runtime(root.path());
        write_fixture(&rt, MALFORMED_V2_TEMPLATE, &parent_path);
        let error = rt
            .load_optional()
            .expect_err("v2 missing durablePolicy must fail closed");
        assert!(
            error.to_string().contains("parsing durable state"),
            "{error:#}"
        );
        assert!(rt.sidecar_path().exists(), "sidecar not deleted");
    }

    // Malformed v1 must fail closed: the version probe alone is not enough, the
    // legacy v1 shape must deserialize strictly (required parentSessionId /
    // parentSessionPath / agents, deny_unknown_fields) before returning None.
    const MALFORMED_V1_TRUNCATED: &str = r#"{ "version": 1 }"#;
    const MALFORMED_V1_UNKNOWN_FIELD: &str = r#"{
      "version": 1,
      "parentSessionId": "parent-session-id",
      "parentSessionPath": "__PARENT_PATH__",
      "durablePolicy": {},
      "agents": []
    }"#;
    const MALFORMED_V1_BAD_AGENT: &str = r#"{
      "version": 1,
      "parentSessionId": "parent-session-id",
      "parentSessionPath": "__PARENT_PATH__",
      "agents": [{"snapshot": {"totally": "wrong"}}]
    }"#;

    #[test]
    fn malformed_v1_that_only_says_version_fails_closed() {
        let root = tempdir().expect("root");
        let (rt, _parent_path) = runtime(root.path());
        fs::write(rt.sidecar_path(), MALFORMED_V1_TRUNCATED).expect("write truncated v1");
        let error = rt
            .load_optional()
            .expect_err("truncated v1 must fail closed");
        assert!(
            error.to_string().contains("parsing durable state"),
            "{error:#}"
        );
        assert!(rt.sidecar_path().exists(), "sidecar not deleted");
    }

    #[test]
    fn malformed_v1_with_unknown_field_fails_closed() {
        let root = tempdir().expect("root");
        let (rt, parent_path) = runtime(root.path());
        write_fixture(&rt, MALFORMED_V1_UNKNOWN_FIELD, &parent_path);
        let error = rt
            .load_optional()
            .expect_err("v1 must not accept fields from a newer schema");
        assert!(
            error.to_string().contains("parsing durable state"),
            "{error:#}"
        );
        assert!(rt.sidecar_path().exists(), "sidecar not deleted");
    }

    #[test]
    fn malformed_v1_with_bad_agent_fails_closed() {
        let root = tempdir().expect("root");
        let (rt, parent_path) = runtime(root.path());
        write_fixture(&rt, MALFORMED_V1_BAD_AGENT, &parent_path);
        let error = rt
            .load_optional()
            .expect_err("v1 with malformed agents must fail closed");
        assert!(
            error.to_string().contains("parsing durable state"),
            "{error:#}"
        );
        assert!(rt.sidecar_path().exists(), "sidecar not deleted");
    }

    // A well-formed v1 sidecar belonging to a different session must fail closed
    // rather than being treated as absent (which the caller would then
    // overwrite), matching the v2 wrong-parent guarantees.
    #[test]
    fn legacy_v1_wrong_parent_id_fails_closed() {
        let root = tempdir().expect("root");
        let (rt, parent_path) = runtime(root.path());
        let rendered = V1_SIDECAR_WITHOUT_MAIN_MAILBOX_TEMPLATE
            .replace("__PARENT_PATH__", &parent_path.to_string_lossy())
            .replace("\"parent-session-id\"", "\"foreign-parent\"");
        fs::write(rt.sidecar_path(), rendered.as_bytes()).expect("write foreign v1 sidecar");

        let error = rt
            .load_optional()
            .expect_err("foreign v1 parent id must fail closed");
        assert!(
            error.to_string().contains("parent session id mismatch"),
            "{error:#}"
        );
        assert!(rt.sidecar_path().exists(), "sidecar not deleted");
    }

    #[test]
    fn legacy_v1_wrong_parent_path_fails_closed() {
        let root = tempdir().expect("root");
        let (rt, _parent_path) = runtime(root.path());
        let other_root = tempdir().expect("other root");
        let other_parent = other_root.path().join("parent.jsonl");
        fs::write(&other_parent, b"{}\n").expect("other parent");
        write_fixture(&rt, V1_SIDECAR_WITHOUT_MAIN_MAILBOX_TEMPLATE, &other_parent);

        let error = rt
            .load_optional()
            .expect_err("foreign v1 parent path must fail closed");
        assert!(
            error.to_string().contains("parent session path mismatch"),
            "{error:#}"
        );
        assert!(rt.sidecar_path().exists(), "sidecar not deleted");
    }

    #[test]
    fn future_version_fails_closed() {
        let root = tempdir().expect("root");
        let (rt, parent_path) = runtime(root.path());
        write_fixture(&rt, FUTURE_VERSION_SIDECAR, &parent_path);
        let error = rt
            .load_optional()
            .expect_err("future version must fail closed");
        assert!(error.to_string().contains("unsupported"), "{error:#}");
        assert!(rt.sidecar_path().exists(), "sidecar not deleted");
    }

    #[test]
    fn zero_version_fails_closed() {
        let root = tempdir().expect("root");
        let (rt, parent_path) = runtime(root.path());
        write_fixture(&rt, ZERO_VERSION_SIDECAR, &parent_path);
        let error = rt
            .load_optional()
            .expect_err("zero version must fail closed");
        assert!(error.to_string().contains("unsupported"), "{error:#}");
        assert!(rt.sidecar_path().exists(), "sidecar not deleted");
    }

    #[test]
    fn current_state_without_delivered_messages_defaults_empty() {
        let root = tempdir().expect("root");
        let (rt, parent_path) = runtime(root.path());
        let state = state_with_agents(&parent_path, vec![persisted_agent("Alpha")]);
        rt.persist(&state).expect("persist current state");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(rt.sidecar_path()).expect("read current sidecar"))
                .expect("decode current sidecar");
        value
            .as_object_mut()
            .expect("state object")
            .remove("deliveredMessages");
        fs::write(
            rt.sidecar_path(),
            serde_json::to_vec_pretty(&value).expect("encode legacy current state"),
        )
        .expect("write legacy current state");

        let loaded = rt.load().expect("missing delivered history defaults empty");
        assert!(loaded.delivered_messages.is_empty());
    }

    #[test]
    fn delivered_message_log_rejects_duplicate_ids_and_oversize_state() {
        let root = tempdir().expect("root");
        let (rt, parent_path) = runtime(root.path());
        let delivered = MailboxMessage {
            id: "message-1".to_owned(),
            from: "Main".to_owned(),
            to: "Alpha".to_owned(),
            body: "accepted".to_owned(),
            timestamp: 1,
            reply_to: None,
        };
        let mut duplicate = state_with_agents(&parent_path, vec![persisted_agent("Alpha")]);
        duplicate.delivered_messages = vec![delivered.clone(), delivered];
        let error = rt
            .persist(&duplicate)
            .expect_err("duplicate delivered ids must fail closed");
        assert!(
            error.to_string().contains("appears more than once"),
            "{error:#}"
        );

        let mut oversized = state_with_agents(&parent_path, vec![persisted_agent("Alpha")]);
        oversized.delivered_messages = (0..=DELIVERED_MESSAGE_LOG_CAP)
            .map(|index| MailboxMessage {
                id: format!("message-{index}"),
                from: "Main".to_owned(),
                to: "Alpha".to_owned(),
                body: "accepted".to_owned(),
                timestamp: index as u64,
                reply_to: None,
            })
            .collect();
        let error = rt
            .persist(&oversized)
            .expect_err("oversized delivered history must fail closed");
        assert!(error.to_string().contains("exceeds maximum"), "{error:#}");
    }

    #[test]
    fn valid_strict_v3_round_trips_through_optional_load() {
        let root = tempdir().expect("root");
        let (rt, parent_path) = runtime(root.path());
        let state = state_with_agents(&parent_path, vec![persisted_agent("Alpha")]);
        rt.persist(&state).expect("persist strict v3");
        assert_eq!(
            rt.load_optional()
                .expect("load strict v3 through optional path"),
            Some(state)
        );
    }

    #[test]
    fn invalid_current_group_id_fails_closed_without_rewriting_sidecar() {
        let root = tempdir().expect("root");
        let (rt, parent_path) = runtime(root.path());
        let mut state = state_with_agents(&parent_path, Vec::new());
        state.group_id = "not-a-group-id".to_owned();
        let bytes = serde_json::to_vec_pretty(&state).expect("serialize invalid group fixture");
        fs::write(rt.sidecar_path(), &bytes).expect("write invalid group fixture");

        let error = rt
            .load()
            .expect_err("invalid group identity must fail closed");
        assert!(
            error.to_string().contains("group id is invalid"),
            "{error:#}"
        );
        assert_eq!(
            fs::read(rt.sidecar_path()).expect("read unchanged fixture"),
            bytes
        );
    }

    #[test]
    fn legacy_v2_group_migration_is_deterministic_and_persists_once() {
        let root = tempdir().expect("root");
        let (rt, parent_path) = runtime(root.path());
        let state = state_with_agents(&parent_path, vec![persisted_agent("Alpha")]);
        let mut legacy = serde_json::to_value(&state).expect("encode legacy v2 fixture");
        legacy["version"] = serde_json::json!(LEGACY_DURABLE_STATE_VERSION);
        legacy
            .as_object_mut()
            .expect("state object")
            .remove("groupId");
        let legacy_bytes = serde_json::to_vec_pretty(&legacy).expect("serialize legacy v2 fixture");
        fs::write(rt.sidecar_path(), &legacy_bytes).expect("write legacy v2 fixture");

        let first = rt.load().expect("migrate legacy v2");
        let second = rt.load().expect("repeat legacy v2 migration");
        assert_eq!(first.group_id, legacy_group_id(&parent_id()));
        assert_eq!(second.group_id, first.group_id);
        assert_eq!(
            fs::read(rt.sidecar_path()).expect("legacy source remains unchanged"),
            legacy_bytes
        );

        rt.persist(&first).expect("persist migrated v3 state");
        let persisted = rt.load().expect("load persisted migrated state");
        assert_eq!(persisted.version, DURABLE_STATE_VERSION);
        assert_eq!(persisted.group_id, first.group_id);
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

        let (rt_other, _) = runtime_with_main(other_root.path(), "Main");
        fs::write(
            rt_other.sidecar_path(),
            fs::read(rt.sidecar_path()).expect("read sidecar"),
        )
        .expect("copy sidecar");
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
        assert!(
            error.chain().any(|source| {
                source
                    .downcast_ref::<crate::fs::FsError>()
                    .is_some_and(|error| {
                        matches!(
                            error,
                            crate::fs::FsError::ReadLimitExceeded { limit }
                                if *limit == MAX_SIDECAR_BYTES
                        )
                    })
            }),
            "{error:#}"
        );
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
    fn recovery_status_queues_active_agents() {
        assert_eq!(recovery_status(AgentStatus::Running), AgentStatus::Queued);
        assert_eq!(recovery_status(AgentStatus::Queued), AgentStatus::Queued);
        assert_eq!(recovery_status(AgentStatus::Idle), AgentStatus::Parked);
        assert_eq!(recovery_status(AgentStatus::Aborted), AgentStatus::Aborted);
        assert_eq!(recovery_status(AgentStatus::Parked), AgentStatus::Parked);
    }

    #[test]
    fn recovery_job_preserves_unsettled_and_keeps_settled() {
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
            soft_budget_exhausted: false,
        };
        let recovered = recovery_job(running.clone(), 100);
        assert_eq!(
            recovered.status,
            JobStatus::Running,
            "unsettled jobs keep their status for revival"
        );
        assert_eq!(
            recovered.finished_at, None,
            "unsettled jobs are not finished"
        );
        assert!(
            recovered.result.is_none(),
            "unsettled jobs get no synthetic error result"
        );
        running.status = JobStatus::Completed;
        running.finished_at = Some(50);
        let settled = recovery_job(running, 100);
        assert_eq!(settled.status, JobStatus::Completed);
        assert_eq!(settled.finished_at, Some(50));
    }

    #[test]
    fn recovery_job_preserves_qualified_persona_history_on_settled() {
        let mut job = JobSnapshot {
            id: "job-persona".to_owned(),
            agent_id: "MentorRun".to_owned(),
            agent: "mentor".to_owned(),
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
            soft_budget_exhausted: false,
        };

        // Unsettled: status preserved, no synthetic result.
        let recovered = recovery_job_with_persona(job.clone(), 100, Some("mentor"));
        assert_eq!(recovered.status, JobStatus::Running);
        assert!(recovered.result.is_none());

        // Settled: result preserved as-is.
        job.status = JobStatus::Completed;
        job.finished_at = Some(50);
        job.result = Some(TaskResult {
            index: 0,
            id: "MentorRun".to_owned(),
            agent: "mentor".to_owned(),
            status: AgentStatus::Parked,
            output: String::new(),
            usage: pi_ai::Usage::default(),
            soft_budget_exhausted: false,
            error: None,
            structured_output: None,
            artifact_ref: "agent://MentorRun".to_owned(),
            history_ref: "history://persona/mentor/MentorRun".to_owned(),
            artifact_uri: "artifact://MentorRun".to_owned(),
        });
        let settled = recovery_job_with_persona(job, 100, Some("mentor"));
        assert_eq!(settled.status, JobStatus::Completed);
        assert_eq!(
            settled.result.expect("settled result").history_ref,
            "history://persona/mentor/MentorRun"
        );
    }

    #[test]
    fn binding_uses_retained_parent_directory_authority() {
        let root = tempdir().expect("root");
        let (rt, parent_path) = runtime(root.path());
        assert_eq!(rt.parent_session_path(), &parent_path);
        assert_eq!(
            rt.child_root(),
            root.path().join("children").join(parent_id())
        );
    }

    #[test]
    fn child_file_authority_rejects_paths_outside_retained_root() {
        let root = tempdir().expect("root");
        let (rt, _) = runtime(root.path());
        let legit = rt.child_root().join("child.jsonl");
        fs::write(&legit, b"{}").expect("write");
        let (lease, relative) = rt.child_file_authority(&legit).expect("legit authority");
        assert_eq!(lease.display_root().join(relative), legit);

        let outside = tempdir().expect("outside");
        let escaped = outside.path().join("escaped.jsonl");
        fs::write(&escaped, b"{}").expect("write outside");
        let error = rt
            .child_file_authority(&escaped)
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
            max_turns: None,
            max_tool_calls: None,
            timeout_secs: None,
            disallowed_tools: Vec::new(),
            capability_ceiling: None,
            source: super::super::AgentDefinitionSource::Bundled,
            path: None,
            trusted: true,
            kind: super::super::AgentDefinitionKind::Agent,
            personality: None,
            soft_budget: None,
            knowledge_roots: Vec::new(),
            user_proxy: false,
        };
        let model = pi_ai::Model::default();
        assert!(
            reconstruct_request(
                &agent,
                model.clone(),
                Vec::new(),
                0,
                &definition,
                "prompt".to_owned(),
                None,
                Vec::new(),
                16,
                None,
                std::sync::Arc::new(Vec::new()),
                std::sync::Arc::new(Vec::new()),
            )
            .is_some()
        );
        definition.name = "other".to_owned();
        assert!(
            reconstruct_request(
                &agent,
                model,
                Vec::new(),
                0,
                &definition,
                "prompt".to_owned(),
                None,
                Vec::new(),
                16,
                None,
                std::sync::Arc::new(Vec::new()),
                std::sync::Arc::new(Vec::new()),
            )
            .is_none()
        );
    }

    #[test]
    fn reconstruct_request_rejects_changed_definition_policy() {
        let agent = persisted_agent("Alpha");
        let mut definition = super::super::AgentDefinition {
            name: "task".to_owned(),
            description: "task".to_owned(),
            system_prompt: "prompt".to_owned(),
            tools: None,
            autoload_skills: Vec::new(),
            model: None,
            thinking_level: None,
            max_turns: None,
            max_tool_calls: None,
            timeout_secs: None,
            disallowed_tools: Vec::new(),
            capability_ceiling: None,
            source: super::super::AgentDefinitionSource::Bundled,
            path: None,
            trusted: true,
            kind: super::super::AgentDefinitionKind::Agent,
            personality: None,
            soft_budget: None,
            knowledge_roots: Vec::new(),
            user_proxy: false,
        };
        definition.tools = Some(vec!["read".to_owned()]);
        assert!(
            reconstruct_request(
                &agent,
                pi_ai::Model::default(),
                Vec::new(),
                0,
                &definition,
                "current prompt".to_owned(),
                Some(vec!["read".to_owned()]),
                Vec::new(),
                16,
                None,
                std::sync::Arc::new(Vec::new()),
                std::sync::Arc::new(Vec::new()),
            )
            .is_none()
        );
    }

    #[test]
    fn old_persisted_definition_json_defaults_new_fields() {
        let value = serde_json::json!({
            "name": "task",
            "description": "task",
            "systemPrompt": "prompt",
            "source": "bundled",
            "trusted": true
        });
        let definition: PersistedDefinition =
            serde_json::from_value(value).expect("old definition remains readable");
        assert_eq!(definition.kind, super::super::AgentDefinitionKind::Agent);
        assert_eq!(definition.personality, None);
        assert_eq!(definition.soft_budget, None);
    }
}
